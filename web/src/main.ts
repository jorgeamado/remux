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
const FONT_MIN = 10;
const FONT_MAX = 22;

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
const handle = createTerminal($("terminal"), fontSize, hideStatusBar);

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
function showHint(text: string): void {
  hint.textContent = text;
  hint.hidden = false;
  clearTimeout(hintTimer);
  hintTimer = window.setTimeout(() => (hint.hidden = true), 2500);
}

function setRole(controller: boolean): void {
  isController = controller;
  renderBanner();
  menuBtn.hidden = false;
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
/// and buffers the keystrokes; scrolling as an observer only hints (a glance
/// shouldn't resize the session under the desktop user); automatic terminal
/// protocol replies never take control and never hint.
function sendInput(
  data: string,
  opts: { takeControl?: boolean; silent?: boolean } = {}
): void {
  if (!isController) {
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
  setup.hidden = true;
  setStatus("connecting…", "connecting");

  const scheme = location.protocol === "https:" ? "wss" : "ws";
  ws = new WebSocket(`${scheme}://${location.host}/ws`);
  ws.binaryType = "arraybuffer";

  ws.onopen = () => {
    const { cols, rows } = handle.size();
    sendJson({
      type: "auth",
      token,
      cols,
      rows,
      session: localStorage.getItem(SESSION_KEY) || undefined,
    });
  };

  ws.onmessage = (ev) => {
    if (typeof ev.data === "string") {
      handleControl(JSON.parse(ev.data));
    } else {
      handle.term.write(new Uint8Array(ev.data as ArrayBuffer));
    }
  };

  ws.onclose = () => {
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
}

function handleControl(msg: ControlMsg): void {
  switch (msg.type) {
    case "status": {
      reconnectDelay = 500;
      sessionTitle = msg.session ?? "";
      setStatus(sessionTitle, "connected");
      if (pingTimer === undefined) startPing();
      const nowController = msg.state === "controller";
      setRole(nowController);
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
  intentionalClose = true;
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
  } else {
    if (!("Notification" in window)) {
      showHint("Notifications aren't supported here");
      return;
    }
    // Must happen inside this user gesture.
    if ((await Notification.requestPermission()) !== "granted") {
      showHint("Notifications are blocked by the browser");
      return;
    }
    notifyPref = true;
  }
  localStorage.setItem(NOTIFY_KEY, notifyPref ? "on" : "off");
  renderNotifyBtn();
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

handle.term.onData((data) => {
  if (RESPONSE_RE.test(data)) {
    sendInput(data, { takeControl: false, silent: true });
  } else {
    sendInput(applyCtrl(data));
  }
});
handle.onResize((cols, rows) => {
  sendJson({ type: "resize", cols, rows });
  if (isController) renderBanner();
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
  requestControl();
});

setupKeyRow(sendInput);
composer.hidden = false;
setupTouchScroll($("terminal"), handle.term, (data) =>
  sendInput(data, { takeControl: false })
);

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

(async () => {
  const hashToken = extractPairToken(location.hash);
  if (hashToken) {
    history.replaceState(null, "", location.pathname);
    try {
      await pairWith(hashToken);
    } catch (e) {
      showSetup(`Pairing failed: ${e instanceof Error ? e.message : e}`);
      return;
    }
  }
  requestWakeLock();
  connect();
})();
