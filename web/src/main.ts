import "./style.css";
import { createTerminal } from "./term";
import { setupKeyRow, applyCtrl } from "./keys";
import { setupTouchScroll } from "./scroll";

const TOKEN_KEY = "remux.device_token";
const FONT_KEY = "remux.font";
const NOTIFY_KEY = "remux.notify";
const SESSION_KEY = "remux.session";
const STATUS_KEY = "remux.statusbar";
const HISTORY_KEY = "remux.history";
const TERMKB_KEY = "remux.termkb";
const FIT_KEY = "remux.fitwidth";
const FONT_MIN = 6; // small enough to view a desktop-sized grid while observing
const FONT_MAX = 28;

const $ = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T;

const connDot = $("conn-dot");
const connLabel = $("conn-label");
const sessionName = $<HTMLButtonElement>("session-name");
const sessionMenu = $("session-menu");
const connInfo = $("conn-info");
const controlBanner = $("control-banner");
const controlText = $("control-text");
const controlBtn = $<HTMLButtonElement>("control-btn");
const menuBtn = $<HTMLButtonElement>("menu-btn");
const menu = $("menu");
const hint = $<HTMLButtonElement>("hint");
const setup = $("setup");
const setupError = $("setup-error");
const composer = $("composer");
const composerInput = $<HTMLInputElement>("composer-input");

const encoder = new TextEncoder();

let ws: WebSocket | null = null;
let isController = false;
let sessionTitle = "";
let reconnectDelay = 500;
let reconnectTimer: number | undefined;
let intentionalClose = false;

// Input typed while still an observer is buffered and flushed once the
// server grants control ("type to take control").
let pendingInput = "";
let controlRequested = false;

let fontSize = parseInt(localStorage.getItem(FONT_KEY) ?? "14", 10) || 14;
let hideStatusBar = localStorage.getItem(STATUS_KEY) !== "show";
// Touch devices: the composer is the input surface; tapping the terminal
// must not open the on-screen keyboard. Desktop keeps direct typing.
let directInput =
  (localStorage.getItem(TERMKB_KEY) ??
    (matchMedia("(pointer: coarse)").matches ? "off" : "on")) === "on";
const handle = createTerminal($("terminal"), fontSize, hideStatusBar);
handle.setDirectInput(directInput);

// ---------- pairing ----------

function extractPairToken(text: string): string | null {
  const m = text.match(/(?:#pair=)?([0-9a-f]{64})\s*$/i);
  return m ? m[1] : null;
}

async function pairWith(token: string): Promise<void> {
  const resp = await fetch("/api/pair", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      token,
      device_name: navigator.userAgent.includes("iPhone")
        ? "iPhone"
        : navigator.userAgent.includes("Android")
          ? "Android"
          : "browser",
    }),
  });
  if (!resp.ok) {
    throw new Error(await resp.text());
  }
  const body = (await resp.json()) as { device_token: string };
  localStorage.setItem(TOKEN_KEY, body.device_token);
}

function showSetup(message?: string): void {
  setup.hidden = false;
  if (message) {
    setupError.textContent = message;
    setupError.hidden = false;
  }
}

// ---------- status & hints ----------

/// Session identity lives on the left (breadcrumb button); connection state
/// lives on the right ("Connected · 12 ms").
function setStatus(text: string, state: "connected" | "connecting" | "offline"): void {
  if (state === "connected") {
    sessionName.textContent = text || "remux";
    connLabel.textContent = "Connected";
  } else {
    connLabel.textContent = text;
  }
  connLabel.classList.toggle("ok", state === "connected");
  connDot.classList.toggle("connected", state === "connected");
  connDot.classList.toggle("connecting", state === "connecting");
}

let hintTimer: number | undefined;
let hintAction: (() => void) | null = null;
function showHint(text: string, action: (() => void) | null = null): void {
  hintAction = action;
  hint.textContent = text;
  hint.hidden = false;
  clearTimeout(hintTimer);
  hintTimer = window.setTimeout(() => (hint.hidden = true), action ? 6000 : 2500);
}

function setRole(controller: boolean): void {
  isController = controller;
  renderBanner();
  menuBtn.hidden = false;
  renderFitBtn();
}

/// The control row: a role chip on the left, the takeover button on the right.
function renderBanner(): void {
  controlBanner.hidden = false;
  controlText.classList.toggle("controller", isController);
  if (isController) {
    const { cols, rows } = handle.size();
    controlText.textContent = `Controller · ${cols}×${rows}`;
    controlBtn.textContent = "Release";
    controlBtn.classList.remove("primary");
  } else {
    controlText.textContent = "Observer";
    controlBtn.textContent = "Take control";
    controlBtn.classList.add("primary");
  }
}

// ---------- connection ----------

function sendJson(obj: unknown): void {
  if (ws?.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify(obj));
  }
}

function requestControl(): void {
  if (!controlRequested) {
    controlRequested = true;
    sendJson({ type: "take_control" });
    showHint("Taking control…");
  }
}

/// Send terminal input. Typing as an observer implicitly requests control
/// and buffers the keystrokes; wheel reports are sent even as an observer
/// (the daemon whitelists them — scrollback without taking over); automatic
/// terminal protocol replies never take control and never hint.
function sendInput(
  data: string,
  opts: { takeControl?: boolean; silent?: boolean; allowObserver?: boolean } = {}
): void {
  if (!isController && !opts.allowObserver) {
    if (opts.takeControl === false) {
      if (!opts.silent) {
        showHint("Observing — tap here to take control");
      }
      return;
    }
    pendingInput = (pendingInput + data).slice(-1024);
    requestControl();
    return;
  }
  if (ws?.readyState === WebSocket.OPEN) {
    ws.send(encoder.encode(data));
  }
}

/// xterm.js auto-answers terminal queries arriving in the output stream
/// (Device Attributes, cursor position reports, DECRPM, OSC/DCS replies).
/// These surface through onData exactly like keystrokes, but they are
/// protocol, not typing — they must never trigger take-control.
const RESPONSE_RE =
  // eslint-disable-next-line no-control-regex
  /^(?:\x1b\[\?[\d;]*c|\x1b\[>[\d;]*c|\x1b\[\d+;\d+R|\x1b\[\??\d+n|\x1b\[\?[\d;]*\$y|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1bP[^\x1b]*\x1b\\)+$/;

function connect(): void {
  const token = localStorage.getItem(TOKEN_KEY);
  if (!token) {
    showSetup();
    return;
  }
  clearTimeout(reconnectTimer); // a pending retry must not race this connect
  setup.hidden = true;
  setStatus("connecting…", "connecting");

  const scheme = location.protocol === "https:" ? "wss" : "ws";
  const sock = new WebSocket(`${scheme}://${location.host}/ws`);
  sock.binaryType = "arraybuffer";
  ws = sock;

  sock.onopen = () => {
    const { cols, rows } = handle.size();
    sendJson({
      type: "auth",
      token,
      cols,
      rows,
      session: localStorage.getItem(SESSION_KEY) || undefined,
    });
  };

  // Events from a socket that has been superseded by a newer connect() (e.g.
  // a session switch) are ignored: they must not write stale output or
  // suppress/schedule reconnects for the current socket.
  sock.onmessage = (ev) => {
    if (ws !== sock) return;
    if (typeof ev.data === "string") {
      handleControl(JSON.parse(ev.data));
    } else {
      handle.term.write(new Uint8Array(ev.data as ArrayBuffer));
    }
  };

  sock.onclose = () => {
    if (ws !== sock) return;
    controlRequested = false;
    pendingInput = "";
    isController = false;
    controlBanner.hidden = true;
    stopPing();
    if (!intentionalClose) {
      setStatus("offline — reconnecting…", "offline");
      scheduleReconnect();
    }
    intentionalClose = false;
  };
}

// ---------- latency ----------

let pingTimer: number | undefined;
let pingSentAt = 0;

function startPing(): void {
  stopPing();
  pingTimer = window.setInterval(() => {
    if (ws?.readyState === WebSocket.OPEN && document.visibilityState === "visible") {
      pingSentAt = performance.now();
      sendJson({ type: "ping" });
    }
  }, 20_000);
  pingSentAt = performance.now();
  sendJson({ type: "ping" });
}

function stopPing(): void {
  clearInterval(pingTimer);
  pingTimer = undefined;
  connInfo.hidden = true;
}

interface ControlMsg {
  type: string;
  state?: string;
  session?: string;
  code?: string;
  message?: string;
  window_cols?: number;
  window_rows?: number;
}

function handleControl(msg: ControlMsg): void {
  switch (msg.type) {
    case "status": {
      reconnectDelay = 500;
      sessionTitle = msg.session ?? "";
      windowCols = msg.window_cols ?? 0;
      setStatus(sessionTitle, "connected");
      if (pingTimer === undefined) startPing();
      const nowController = msg.state === "controller";
      setRole(nowController);
      applyFitWidth();
      if (nowController) {
        hint.hidden = true;
        controlRequested = false;
        if (pendingInput) {
          const buffered = pendingInput;
          pendingInput = "";
          sendInput(buffered);
        }
      } else {
        controlRequested = false;
        pendingInput = "";
      }
      break;
    }
    case "attention":
      onAttention();
      break;
    case "pong": {
      const rtt = Math.max(1, Math.round(performance.now() - pingSentAt));
      if (rtt < 10_000) {
        connInfo.textContent = `${rtt} ms`;
        connInfo.hidden = false;
      }
      break;
    }
    case "error":
      if (msg.code === "auth_failed") {
        localStorage.removeItem(TOKEN_KEY);
        intentionalClose = true;
        ws?.close();
        showSetup("This device is no longer paired. Pair it again.");
      } else if (msg.code === "revoked") {
        localStorage.removeItem(TOKEN_KEY);
        intentionalClose = true;
        ws?.close();
        showSetup("This device was revoked. Pair it again if that was a mistake.");
      } else if (msg.code === "invalid_session") {
        // Fall back to the server default; onclose will reconnect.
        localStorage.removeItem(SESSION_KEY);
        showHint("Session unavailable — using default");
      }
      break;
  }
}

function scheduleReconnect(): void {
  clearTimeout(reconnectTimer);
  reconnectTimer = window.setTimeout(() => {
    reconnectDelay = Math.min(reconnectDelay * 2, 8000);
    connect();
  }, reconnectDelay);
}

// ---------- session picker ----------

interface SessionInfo {
  name: string;
  windows: number;
  attached: number;
}

function menuItem(label: string, onClick: () => void): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.className = "btn";
  btn.textContent = label;
  btn.addEventListener("click", onClick);
  return btn;
}

async function openSessionMenu(): Promise<void> {
  const token = localStorage.getItem(TOKEN_KEY);
  if (!token) return;
  let sessions: SessionInfo[];
  try {
    const resp = await fetch("/api/sessions", {
      headers: { authorization: `Bearer ${token}` },
    });
    if (!resp.ok) throw new Error(String(resp.status));
    sessions = (await resp.json()) as SessionInfo[];
  } catch {
    showHint("Couldn't list sessions");
    return;
  }
  sessionMenu.textContent = "";
  for (const s of sessions) {
    const marker = s.name === sessionTitle ? "● " : "";
    const attached = s.attached > 0 ? " · attached" : "";
    sessionMenu.appendChild(
      menuItem(`${marker}${s.name} — ${s.windows}w${attached}`, () =>
        switchSession(s.name)
      )
    );
  }
  sessionMenu.appendChild(
    menuItem("New session…", () => {
      const name = window.prompt("New session name:")?.trim();
      if (!name) return;
      if (!/^[A-Za-z0-9_-]{1,64}$/.test(name)) {
        sessionMenu.hidden = true;
        showHint("Names: letters, digits, - and _ only");
        return;
      }
      switchSession(name);
    })
  );
  sessionMenu.hidden = false;
}

function switchSession(name: string): void {
  sessionMenu.hidden = true;
  if (name === sessionTitle) return;
  localStorage.setItem(SESSION_KEY, name);
  // connect() supersedes the old socket; its close event is then ignored,
  // so no intentionalClose flag is needed (which could leak and suppress
  // the reconnect after an invalid_session rejection).
  ws?.close();
  handle.term.reset(); // fresh grid; the new attach repaints everything
  connect();
}

sessionName.addEventListener("click", (ev) => {
  ev.stopPropagation();
  menu.hidden = true;
  if (sessionMenu.hidden) {
    void openSessionMenu();
  } else {
    sessionMenu.hidden = true;
  }
});

// ---------- observer fit-width ----------

/// The observer's terminal is a viewport onto a (usually wider) desktop-sized
/// window. "Fit" shrinks the font just enough that the full window width fits
/// on screen — pure font-size math on this client; tmux is never resized.
let windowCols = 0;
let fitWidth = localStorage.getItem(FIT_KEY) === "on";
const fitBtn = $<HTMLButtonElement>("fit-btn");

function renderFitBtn(): void {
  fitBtn.hidden = isController;
  fitBtn.classList.toggle("on", fitWidth);
}

function applyFitWidth(): void {
  if (isController || !fitWidth || windowCols <= 0) {
    handle.setFontSize(fontSize); // back to the user's preference
    return;
  }
  const screen = document.querySelector(".xterm-screen");
  if (!screen) return;
  const { cols } = handle.size();
  const cellW = screen.getBoundingClientRect().width / cols;
  if (!isFinite(cellW) || cellW <= 0) return;
  const currentFont = handle.term.options.fontSize ?? fontSize;
  const target = Math.min(
    FONT_MAX,
    Math.max(FONT_MIN, Math.floor((currentFont * cols) / windowCols))
  );
  if (target !== currentFont) {
    handle.setFontSize(target); // triggers a refit; converges (cols → windowCols)
  }
}

fitBtn.addEventListener("click", () => {
  fitWidth = !fitWidth;
  localStorage.setItem(FIT_KEY, fitWidth ? "on" : "off");
  renderFitBtn();
  applyFitWidth();
  if (fitWidth && windowCols > 0) {
    showHint(`Fitting ${windowCols} columns`);
  }
});

// ---------- windows & panes (tmux "tabs") ----------

interface WindowInfo {
  index: number;
  active: boolean;
  panes: number;
  name: string;
}

const tmuxBtn = $<HTMLButtonElement>("tmux-btn");
const tmuxMenu = $("tmux-menu");

function windowAction(action: string, index?: number): void {
  tmuxMenu.hidden = true;
  if (!isController) {
    showHint("Take control first");
    return;
  }
  sendJson({ type: "window_action", action, index });
}

async function openTmuxMenu(): Promise<void> {
  const token = localStorage.getItem(TOKEN_KEY);
  if (!token || !sessionTitle) return;
  let windows: WindowInfo[] = [];
  try {
    const resp = await fetch(
      `/api/windows?session=${encodeURIComponent(sessionTitle)}`,
      { headers: { authorization: `Bearer ${token}` } }
    );
    if (resp.ok) windows = (await resp.json()) as WindowInfo[];
  } catch {
    /* menu still offers the actions */
  }
  tmuxMenu.textContent = "";
  if (windows.length > 0) {
    const label = document.createElement("div");
    label.className = "menu-label";
    label.textContent = "Windows";
    tmuxMenu.appendChild(label);
    for (const w of windows) {
      const marker = w.active ? "● " : "";
      const panes = w.panes > 1 ? ` · ${w.panes} panes` : "";
      tmuxMenu.appendChild(
        menuItem(`${marker}${w.index}: ${w.name}${panes}`, () =>
          windowAction("select_window", w.index)
        )
      );
    }
  }
  const label = document.createElement("div");
  label.className = "menu-label";
  label.textContent = "Create";
  tmuxMenu.appendChild(label);
  tmuxMenu.appendChild(menuItem("New window", () => windowAction("new_window")));
  tmuxMenu.appendChild(
    menuItem("Split │ side by side", () => windowAction("split_h"))
  );
  tmuxMenu.appendChild(menuItem("Split ─ stacked", () => windowAction("split_v")));
  tmuxMenu.appendChild(menuItem("Next pane", () => windowAction("next_pane")));
  tmuxMenu.hidden = false;
}

tmuxBtn.addEventListener("click", (ev) => {
  ev.stopPropagation();
  menu.hidden = true;
  sessionMenu.hidden = true;
  if (tmuxMenu.hidden) {
    void openTmuxMenu();
  } else {
    tmuxMenu.hidden = true;
  }
});

// ---------- attention notifications ----------

/// The daemon says the session was busy and went quiet (job finished, or a
/// program is waiting for input). Only relevant when the user isn't looking;
/// while the terminal is on screen the event is self-evident.
let notifyPref = localStorage.getItem(NOTIFY_KEY) === "on";
const notifyBtn = $<HTMLButtonElement>("notify-btn");

function renderNotifyBtn(): void {
  notifyBtn.textContent = `Notifications: ${notifyPref ? "on" : "off"}`;
}

async function toggleNotify(): Promise<void> {
  if (notifyPref) {
    notifyPref = false;
    void unsubscribePush();
  } else {
    if (!("Notification" in window)) {
      showHint("On iPhone: Add to Home Screen first, then enable here");
      return;
    }
    // Must happen inside this user gesture.
    if ((await Notification.requestPermission()) !== "granted") {
      showHint("Notifications are blocked by the browser");
      return;
    }
    notifyPref = true;
    void subscribePush();
  }
  localStorage.setItem(NOTIFY_KEY, notifyPref ? "on" : "off");
  renderNotifyBtn();
}

// ---------- Web Push (lock-screen delivery while the socket is dead) ----------

function b64urlToBytes(value: string): Uint8Array {
  const b64 = value.replace(/-/g, "+").replace(/_/g, "/");
  const raw = atob(b64.padEnd(b64.length + ((4 - (b64.length % 4)) % 4), "="));
  return Uint8Array.from(raw, (c) => c.charCodeAt(0));
}

function authHeader(): Record<string, string> {
  return { authorization: `Bearer ${localStorage.getItem(TOKEN_KEY) ?? ""}` };
}

async function subscribePush(): Promise<void> {
  if (!("serviceWorker" in navigator)) return;
  const reg = await navigator.serviceWorker.getRegistration();
  if (!reg?.pushManager) {
    showHint("Lock-screen alerts need the installed app; in-app alerts active");
    return;
  }
  try {
    const resp = await fetch("/api/push/key", { headers: authHeader() });
    if (!resp.ok) throw new Error(String(resp.status));
    const { key } = (await resp.json()) as { key: string };
    let sub: PushSubscription;
    try {
      sub = await reg.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: b64urlToBytes(key).buffer as ArrayBuffer,
      });
    } catch {
      // A stale subscription under a rotated VAPID key: drop and retry once.
      await (await reg.pushManager.getSubscription())?.unsubscribe();
      sub = await reg.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: b64urlToBytes(key).buffer as ArrayBuffer,
      });
    }
    const json = sub.toJSON();
    const resp2 = await fetch("/api/push/subscribe", {
      method: "POST",
      headers: { ...authHeader(), "content-type": "application/json" },
      body: JSON.stringify({ endpoint: sub.endpoint, keys: json.keys ?? {} }),
    });
    if (!resp2.ok) throw new Error(await resp2.text());
    showHint("Lock-screen notifications on");
  } catch (e) {
    showHint("Push setup failed — in-app alerts only");
    console.warn("push subscribe failed:", e);
  }
}

async function unsubscribePush(): Promise<void> {
  try {
    const reg = await navigator.serviceWorker?.getRegistration();
    const sub = await reg?.pushManager?.getSubscription();
    if (!sub) return;
    await fetch("/api/push/unsubscribe", {
      method: "POST",
      headers: { ...authHeader(), "content-type": "application/json" },
      body: JSON.stringify({ endpoint: sub.endpoint }),
    });
    await sub.unsubscribe();
  } catch {
    /* best effort */
  }
}

/// After a notification tap (or any return to the app), land on the session
/// that actually wants attention — the push payload deliberately can't say.
async function checkPendingAttention(): Promise<void> {
  if (!localStorage.getItem(TOKEN_KEY)) return;
  try {
    const resp = await fetch("/api/attention", { headers: authHeader() });
    if (!resp.ok) return;
    const { sessions } = (await resp.json()) as { sessions: string[] };
    const others = sessions.filter((s) => s !== sessionTitle);
    if (others.length > 0 && !sessions.includes(sessionTitle)) {
      showHint(`Attention in ${others[0]} — tap to open`, () =>
        switchSession(others[0])
      );
    }
  } catch {
    /* offline */
  }
}

function onAttention(): void {
  if (document.visibilityState === "visible") {
    return;
  }
  document.title = "● remux";
  if (!notifyPref || !("Notification" in window) || Notification.permission !== "granted") {
    return;
  }
  const opts: NotificationOptions = {
    body: `${sessionTitle || "session"} may need your attention`,
    tag: "remux-attention", // replaces, never stacks
    icon: "/icon-512.png",
  };
  void navigator.serviceWorker
    ?.getRegistration()
    .then((reg) => {
      if (reg) return reg.showNotification("remux", opts);
      new Notification("remux", opts);
    })
    .catch(() => {
      try {
        new Notification("remux", opts);
      } catch {
        /* platform without page-created notifications */
      }
    });
}

// ---------- menu (font size, paste) ----------

function applyFont(px: number): void {
  fontSize = Math.min(FONT_MAX, Math.max(FONT_MIN, px));
  localStorage.setItem(FONT_KEY, String(fontSize));
  handle.setFontSize(fontSize);
}

async function pasteFromClipboard(): Promise<void> {
  let text = "";
  try {
    text = await navigator.clipboard.readText();
  } catch {
    /* insecure context or permission denied */
  }
  if (!text) {
    text = window.prompt("Paste text to send:") ?? "";
  }
  if (text) {
    // Goes through xterm so bracketed paste is applied when the app wants it.
    handle.term.paste(text);
  }
  menu.hidden = true;
}

menuBtn.addEventListener("click", (ev) => {
  ev.stopPropagation();
  sessionMenu.hidden = true;
  menu.hidden = !menu.hidden;
});
document.addEventListener("click", (ev) => {
  if (!menu.hidden && !menu.contains(ev.target as Node)) {
    menu.hidden = true;
  }
  if (!sessionMenu.hidden && !sessionMenu.contains(ev.target as Node)) {
    sessionMenu.hidden = true;
  }
  if (!tmuxMenu.hidden && !tmuxMenu.contains(ev.target as Node)) {
    tmuxMenu.hidden = true;
  }
  if (!devicesMenu.hidden && !devicesMenu.contains(ev.target as Node)) {
    devicesMenu.hidden = true;
  }
});
$("font-dec").addEventListener("click", () => applyFont(fontSize - 1));
$("font-inc").addEventListener("click", () => applyFont(fontSize + 1));
$("paste-btn").addEventListener("click", () => void pasteFromClipboard());
notifyBtn.addEventListener("click", () => void toggleNotify());
renderNotifyBtn();

// ---------- command composer ----------

/// Mobile-friendly alternative to typing straight into the terminal: a text
/// field that sends a full line. Submitting as an observer requests control
/// (the line is buffered and flushed by the existing take-control path).
const HISTORY_MAX = 50;
let cmdHistory: string[] = JSON.parse(localStorage.getItem(HISTORY_KEY) ?? "[]");
let historyIdx: number | null = null;

function composerSubmit(): void {
  const text = composerInput.value;
  if (!text) return;
  sendInput(text + "\r");
  if (cmdHistory[cmdHistory.length - 1] !== text) {
    cmdHistory = [...cmdHistory, text].slice(-HISTORY_MAX);
    localStorage.setItem(HISTORY_KEY, JSON.stringify(cmdHistory));
  }
  historyIdx = null;
  composerInput.value = "";
}

composerInput.addEventListener("keydown", (ev) => {
  if (ev.key === "Enter") {
    ev.preventDefault();
    composerSubmit();
  } else if (ev.key === "ArrowUp" && cmdHistory.length > 0) {
    ev.preventDefault();
    historyIdx = historyIdx === null ? cmdHistory.length - 1 : Math.max(0, historyIdx - 1);
    composerInput.value = cmdHistory[historyIdx];
  } else if (ev.key === "ArrowDown" && historyIdx !== null) {
    ev.preventDefault();
    historyIdx = historyIdx >= cmdHistory.length - 1 ? null : historyIdx + 1;
    composerInput.value = historyIdx === null ? "" : cmdHistory[historyIdx];
  }
});

// pointerdown + preventDefault keeps focus (and the keyboard) in the input.
$("composer-send").addEventListener("pointerdown", (ev) => {
  ev.preventDefault();
  composerSubmit();
});

const keysToggle = $<HTMLButtonElement>("keys-toggle");
const keypanel = $("keypanel");
keysToggle.addEventListener("pointerdown", (ev) => {
  ev.preventDefault();
  keypanel.hidden = !keypanel.hidden;
  keysToggle.textContent = keypanel.hidden ? "⌃" : "⌄";
});

// ---------- wire up ----------

/// Wheel reports (desktop trackpad/mouse over the terminal) scroll history —
/// they must never trigger take-control and they work while observing.
const WHEEL_RE = /^(?:\x1b\[<6[45];\d+;\d+M)+$/;

handle.term.onData((data) => {
  if (RESPONSE_RE.test(data)) {
    sendInput(data, { takeControl: false, silent: true });
  } else if (WHEEL_RE.test(data)) {
    sendInput(data, { takeControl: false, silent: true, allowObserver: true });
  } else {
    sendInput(applyCtrl(data));
  }
});
handle.onResize((cols, rows) => {
  sendJson({ type: "resize", cols, rows });
  if (isController) renderBanner();
  // Rotation/keyboard changes refit xterm at the old font; recompute the
  // fitted size against the tmux window (no-ops once converged).
  applyFitWidth();
});

controlBtn.addEventListener("click", () => {
  if (isController) {
    sendJson({ type: "release_control" });
  } else {
    requestControl();
  }
});

hint.addEventListener("click", () => {
  hint.hidden = true;
  if (hintAction) {
    const action = hintAction;
    hintAction = null;
    action();
  } else {
    requestControl();
  }
});

setupKeyRow(sendInput);
composer.hidden = false;
setupTouchScroll($("terminal"), handle.term, (data) =>
  sendInput(data, { takeControl: false, silent: true, allowObserver: true })
);

// ---------- devices sheet (read-only; manage via the host CLI) ----------

const devicesMenu = $("devices-menu");

function agoText(ts: number): string {
  if (!ts) return "never";
  const s = Math.max(0, Math.floor(Date.now() / 1000 - ts));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

$("devices-btn").addEventListener("click", async (ev) => {
  ev.stopPropagation();
  menu.hidden = true;
  let list: { name: string; last_seen_unix: number; this_device: boolean }[];
  try {
    const resp = await fetch("/api/devices", { headers: authHeader() });
    if (!resp.ok) throw new Error(String(resp.status));
    list = await resp.json();
  } catch {
    showHint("Couldn't list devices");
    return;
  }
  devicesMenu.textContent = "";
  const label = document.createElement("div");
  label.className = "menu-label";
  label.textContent = "Paired devices";
  devicesMenu.appendChild(label);
  for (const d of list) {
    const row = document.createElement("div");
    row.className = "menu-row";
    row.textContent = `${d.this_device ? "● " : ""}${d.name} · ${agoText(d.last_seen_unix)}`;
    devicesMenu.appendChild(row);
  }
  const foot = document.createElement("div");
  foot.className = "menu-label";
  foot.textContent = "revoke/rename: remux devices (host CLI)";
  devicesMenu.appendChild(foot);
  devicesMenu.hidden = false;
});

// ---------- tmux status bar toggle ----------

const statusBtn = $<HTMLButtonElement>("status-btn");
function renderStatusBtn(): void {
  statusBtn.textContent = `tmux bar: ${hideStatusBar ? "hidden" : "shown"}`;
}
statusBtn.addEventListener("click", () => {
  hideStatusBar = !hideStatusBar;
  localStorage.setItem(STATUS_KEY, hideStatusBar ? "hide" : "show");
  handle.setHideStatusRow(hideStatusBar);
  renderStatusBtn();
});
renderStatusBtn();

// ---------- direct terminal typing toggle ----------

const termkbBtn = $<HTMLButtonElement>("termkb-btn");
function renderTermkbBtn(): void {
  termkbBtn.textContent = `Direct typing: ${directInput ? "on" : "off"}`;
}
termkbBtn.addEventListener("click", () => {
  directInput = !directInput;
  localStorage.setItem(TERMKB_KEY, directInput ? "on" : "off");
  handle.setDirectInput(directInput);
  renderTermkbBtn();
});
renderTermkbBtn();

$("pair-btn").addEventListener("click", async () => {
  const input = $<HTMLInputElement>("pair-input").value.trim();
  const token = extractPairToken(input);
  if (!token) {
    showSetup("That doesn't look like a pairing link or token.");
    return;
  }
  try {
    await pairWith(token);
    setup.hidden = true;
    connect();
  } catch (e) {
    showSetup(`Pairing failed: ${e instanceof Error ? e.message : e}`);
  }
});

// Reconnect promptly when the app returns to the foreground (iOS kills
// background sockets; the daemon treats the dead socket as release).
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") {
    document.title = "remux"; // clear the attention badge
    if (!ws || ws.readyState === WebSocket.CLOSED) {
      clearTimeout(reconnectTimer);
      connect();
    }
    requestWakeLock();
    void checkPendingAttention();
  }
});

// Keep the screen awake while watching a session (iOS 16.4+).
async function requestWakeLock(): Promise<void> {
  try {
    await (navigator as any).wakeLock?.request("screen");
  } catch {
    /* not critical */
  }
}

// ---------- boot ----------

if ("serviceWorker" in navigator && location.protocol === "https:") {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

// iOS partitions storage between the Safari tab and the installed PWA, so a
// tab that just paired offers the (TTL-reusable) link for pasting inside the
// installed app.
function offerInstallTip(pairUrl: string): void {
  const isIOS = /iPhone|iPad|iPod/.test(navigator.userAgent);
  const standalone =
    matchMedia("(display-mode: standalone)").matches ||
    (navigator as { standalone?: boolean }).standalone === true;
  if (!isIOS || standalone) return;
  const tip = $("install-tip");
  tip.hidden = false;
  $("copy-pair").addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(pairUrl);
      showHint("Link copied — paste it in the installed app");
    } catch {
      window.prompt("Copy this pairing link:", pairUrl);
    }
  });
  $("tip-close").addEventListener("click", () => (tip.hidden = true));
}

(async () => {
  const hashToken = extractPairToken(location.hash);
  if (hashToken) {
    const pairUrl = `${location.origin}/#pair=${hashToken}`;
    history.replaceState(null, "", location.pathname);
    try {
      await pairWith(hashToken);
      offerInstallTip(pairUrl);
    } catch (e) {
      showSetup(`Pairing failed: ${e instanceof Error ? e.message : e}`);
      return;
    }
  }
  requestWakeLock();
  connect();
  // A notification tap may cold-start the app (no visibilitychange fires):
  // land on the session that wants attention.
  void checkPendingAttention();
})();
