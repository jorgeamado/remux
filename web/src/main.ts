import "./style.css";
import { createTerminal } from "./term";
import { setupKeyRow, applyCtrl, disarmCtrl } from "./keys";
import { setupTouchScroll } from "./scroll";

const TOKEN_KEY = "remux.device_token";

// ---------- token mirror for the service worker ----------
// The SW cannot read localStorage; it needs the device token to ask
// /api/attention for notification detail after a (payload-less) push.
// IndexedDB is the only storage both contexts share.
function idbTokenWrite(token: string | null): void {
  try {
    const open = indexedDB.open("remux", 1);
    open.onupgradeneeded = () => open.result.createObjectStore("kv");
    open.onsuccess = () => {
      const tx = open.result.transaction("kv", "readwrite");
      if (token === null) {
        tx.objectStore("kv").delete("device_token");
      } else {
        tx.objectStore("kv").put(token, "device_token");
      }
      tx.oncomplete = () => open.result.close();
    };
  } catch {
    /* private mode etc. — SW falls back to the generic notification */
  }
}

function setDeviceToken(token: string | null): void {
  if (token === null) {
    localStorage.removeItem(TOKEN_KEY);
  } else {
    localStorage.setItem(TOKEN_KEY, token);
  }
  idbTokenWrite(token);
}

// Devices paired before the mirror existed: sync on every startup.
idbTokenWrite(localStorage.getItem(TOKEN_KEY));
const FONT_KEY = "remux.font";
const NOTIFY_KEY = "remux.notify";
const SESSION_KEY = "remux.session";
const TERMKB_KEY = "remux.termkb";
// Purge any typed-command history persisted by older builds — command lines can
// contain secrets and must never live on disk. History is memory-only now.
for (const k of Object.keys(localStorage)) {
  if (k.startsWith("remux.history")) localStorage.removeItem(k);
}
// Font size is effectively the tmux resolution: smaller font = more cols/rows.
// Default to a compact grid; A-/A+ tune it, down to a still-legible floor.
const FONT_MIN = 7;
const FONT_MAX = 28;
const FONT_DEFAULT = 10;

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
const composerPlaceholder = composerInput.placeholder;
let placeholderTimer: number | undefined;
/// Briefly show a hint in the composer placeholder, then restore it. A single
/// managed timer, so rapid repeats can't leave the hint stuck.
function flashPlaceholder(msg: string): void {
  composerInput.placeholder = msg;
  if (placeholderTimer !== undefined) window.clearTimeout(placeholderTimer);
  placeholderTimer = window.setTimeout(() => {
    composerInput.placeholder = composerPlaceholder;
    placeholderTimer = undefined;
  }, 2500);
}

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

// Clamp the stored font to the readable range. Earlier builds (auto-fit-width,
// a too-low FONT_MIN) could persist a tiny value like 6px; treat anything
// below the floor as stale and reset to the default.
let fontSize = (() => {
  const stored = parseInt(localStorage.getItem(FONT_KEY) ?? "", 10);
  if (!stored || stored < FONT_MIN || stored > FONT_MAX) return FONT_DEFAULT;
  return stored;
})();
localStorage.setItem(FONT_KEY, String(fontSize));
// Touch devices: the composer is the input surface; tapping the terminal
// must not open the on-screen keyboard. Desktop keeps direct typing.
let directInput =
  (localStorage.getItem(TERMKB_KEY) ??
    (matchMedia("(pointer: coarse)").matches ? "off" : "on")) === "on";
const handle = createTerminal($("terminal"), fontSize);
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
  setDeviceToken(body.device_token);
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
  // Hide the tmux status line only while we drive the size (controller); as an
  // observer tmux's status isn't on our bottom row, so clipping would misfire.
  // The window tabs make the status line redundant anyway.
  handle.setHideStatusRow(controller);
  maybeAutoZoom(); // a split active window auto-zooms once we can drive it
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
    windowTabs.hidden = true;
    paneTabs.hidden = true;
    clearPermissionCards();
    clearFeed();
    clearPaneViews();
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

interface PaneTopo {
  /** tmux pane id (`%N`) — stable identity, unlike index. */
  id: string;
  index: number;
  active: boolean;
  command: string;
}
interface WindowTopo {
  index: number;
  active: boolean;
  zoomed: boolean;
  name: string;
  panes: PaneTopo[];
}
interface SessionTopo {
  name: string;
  attached: boolean;
  windows: WindowTopo[];
}
interface ControlMsg {
  type: string;
  state?: string;
  session?: string;
  code?: string;
  message?: string;
  sessions?: SessionTopo[];
  /** attention frames: event kind + optional hook-fed detail */
  kind?: string;
  reason?: string;
  source?: string;
  /** permission_cards frames (M4b) */
  cards?: PermissionCard[];
  /** command_feed frames (M4c) */
  commands?: FeedCommand[];
  /** pane_views frames: structured per-pane state for custom renderers */
  views?: PaneView[];
}

/** A pane's structured view state, rendered as a custom interface. */
interface PaneView {
  pane: string;
  view: string;
  rev: number;
  // Shape depends on `view`; the renderer for that view id validates it.
  state: Record<string, unknown>;
}

/** A shell command in the session feed (M4c). Mirrors the daemon's Cmd view. */
interface FeedCommand {
  id: number;
  session: string;
  pane: string;
  command: string;
  cwd: string;
  state: "running" | "done" | "aborted";
  exit: number | null;
  elapsed_ms: number | null;
  started_unix: number;
  age_ms: number;
}

/** An open agent permission request (M4b). Mirrors the daemon's Card::view. */
interface PermissionCard {
  id: string;
  session: string;
  pane: string;
  source: string;
  tool: string;
  summary: string;
  // The summary is only a prefix of the real input — the phone must not offer a
  // remote Allow it can't fully see. Deny stays available.
  truncated?: boolean;
  prompt_id?: string;
  remaining_secs: number;
}

// Latest tmux topology (M3a). M3b renders it as tabs/breadcrumb; for now it's
// stored and exposed for tests.
let topology: SessionTopo[] = [];

function handleControl(msg: ControlMsg): void {
  switch (msg.type) {
    case "status": {
      reconnectDelay = 500;
      sessionTitle = msg.session ?? "";
      setStatus(sessionTitle, "connected");
      if (pingTimer === undefined) startPing();
      const nowController = msg.state === "controller";
      setRole(nowController);
      renderTabs(); // session may have changed
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
    case "topology":
      topology = msg.sessions ?? [];
      renderTabs();
      maybeAutoZoom();
      refreshPaneView(); // the active pane may have changed → re-pick its view
      if (!sessionMenu.hidden) openSessionMenu(); // refresh open picker live
      break;
    case "attention":
      onAttention();
      break;
    case "permission_cards":
      renderPermissionCards(msg.cards ?? []);
      break;
    case "command_feed":
      onCommandFeed(msg.commands ?? []);
      break;
    case "pane_views":
      onPaneViews(msg.views ?? []);
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
        setDeviceToken(null);
        intentionalClose = true;
        ws?.close();
        showSetup("This device is no longer paired. Pair it again.");
      } else if (msg.code === "revoked") {
        setDeviceToken(null);
        intentionalClose = true;
        ws?.close();
        showSetup("This device was revoked. Pair it again if that was a mistake.");
      } else if (msg.code === "invalid_session") {
        // Fall back to the server default; onclose will reconnect.
        localStorage.removeItem(SESSION_KEY);
        showHint("Session unavailable — using default");
      } else if (msg.code === "release_failed") {
        // We tried to open the dashboard, which releases terminal control — but
        // tmux didn't demote us, so we're still driving size. Don't leave the
        // dashboard covering a terminal we still control (the hidden xterm would
        // keep shrinking the desktop); revert to the terminal.
        if (dashboardMode) setDashboard(false);
        showHint("Couldn't switch to Dashboard — still controlling the terminal");
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

function menuItem(label: string, onClick: () => void): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.className = "btn";
  btn.textContent = label;
  btn.addEventListener("click", onClick);
  return btn;
}

// Fed by the live topology stream (M3b) — no polling, always current.
function openSessionMenu(): void {
  sessionMenu.textContent = "";
  for (const s of topology) {
    const marker = s.name === sessionTitle ? "● " : "";
    const attached = s.attached ? " · attached" : "";
    sessionMenu.appendChild(
      menuItem(`${marker}${s.name} — ${s.windows.length}w${attached}`, () =>
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
  // The feed is per-session; clear it now. The old socket's close is ignored
  // (superseded), so onclose won't, and the new session may have an empty feed
  // the daemon stays silent about — leaving stale cards without this.
  clearFeed();
  clearPaneViews(); // per-pane views belong to the old session — drop them
  handle.term.reset(); // fresh grid; the new attach repaints everything
  connect();
}

sessionName.addEventListener("click", (ev) => {
  ev.stopPropagation();
  menu.hidden = true;
  tmuxMenu.hidden = true;
  if (sessionMenu.hidden) {
    openSessionMenu();
  } else {
    sessionMenu.hidden = true;
  }
});

// ---------- windows & panes (tmux "tabs") ----------

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

// Window switching now lives in the always-visible tab strip (renderTabs);
// the + menu is create-only.
function openTmuxMenu(): void {
  tmuxMenu.textContent = "";
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
    openTmuxMenu();
  } else {
    tmuxMenu.hidden = true;
  }
});

// ---------- window tabs (live from topology) ----------

const windowTabs = $("window-tabs");

// Small screens shouldn't render tmux split geometry — it's unusable at phone
// size. Instead we auto-zoom the active pane so a split window shows as a
// single full pane; "Next pane" cycles between them (each stays zoomed). Only
// on coarse-pointer (touch) devices, and only while we drive the window.
const smallScreen = matchMedia("(pointer: coarse)").matches;

function activeWindow(): WindowTopo | undefined {
  return topology.find((s) => s.name === sessionTitle)?.windows.find((w) => w.active);
}

function maybeAutoZoom(): void {
  if (!smallScreen || !isController) return;
  const active = activeWindow();
  if (active && active.panes.length > 1 && !active.zoomed) {
    sendJson({ type: "window_action", action: "zoom_pane" });
  }
}

const paneTabs = $("pane-tabs");

/// Render the current session's windows as tappable tabs, active highlighted.
/// Driven purely by the topology stream — no polling.
function renderTabs(): void {
  const sess = topology.find((s) => s.name === sessionTitle);
  const windows = sess?.windows ?? [];
  windowTabs.textContent = "";
  if (windows.length >= 2) {
    for (const w of windows) {
      const tab = document.createElement("button");
      tab.className = `wtab${w.active ? " active" : ""}`;
      const panes = w.panes.length > 1 ? ` ·${w.panes.length}` : "";
      tab.textContent = `${w.index}: ${w.name}${panes}`;
      tab.addEventListener("click", () => {
        if (!w.active) windowAction("select_window", w.index);
      });
      windowTabs.appendChild(tab);
    }
    windowTabs.hidden = false;
  } else {
    windowTabs.hidden = true;
  }
  renderPaneTabs();
}

/// When the active window is split, its panes become tabs — so a split can be
/// navigated pane-by-pane (each shown zoomed full-screen) instead of rendered
/// as split geometry on a small screen.
function renderPaneTabs(): void {
  const active = activeWindow();
  paneTabs.textContent = "";
  if (!active || active.panes.length < 2) {
    paneTabs.hidden = true;
    return;
  }
  const label = document.createElement("span");
  label.className = "ptab-label";
  label.textContent = "panes";
  paneTabs.appendChild(label);
  for (const p of active.panes) {
    const tab = document.createElement("button");
    tab.className = `wtab${p.active ? " active" : ""}`;
    tab.textContent = `${p.index}: ${p.command || "sh"}`;
    tab.addEventListener("click", () => {
      if (!p.active) windowAction("select_pane", p.index);
    });
    paneTabs.appendChild(tab);
  }
  paneTabs.hidden = false;
}

// ---------- M4b permission cards ----------

const permissionCards = $("permission-cards");
// Latest cards from the WS reconcile, plus the wall-clock we received them, so
// the countdown keeps ticking between frames without trusting a stale value.
let permCards: PermissionCard[] = [];
let permReceivedAt = 0;
// Ids currently being decided — disables the buttons so a double-tap can't
// fire two POSTs (the second would 404, but the flicker is confusing).
const permDeciding = new Set<string>();
let permTicker: number | undefined;

function renderPermissionCards(cards: PermissionCard[]): void {
  // Each WS frame is a full reconcile: replace the model and re-baseline the
  // countdown to now (the frame carries fresh remaining_secs).
  permCards = cards;
  permReceivedAt = performance.now();
  if (cards.length > 0 && permTicker === undefined) {
    permTicker = window.setInterval(paintPermissionCards, 1000);
  }
  paintPermissionCards(); // may stop the ticker if nothing is live
}

function clearPermissionCards(): void {
  renderPermissionCards([]);
}

/// (Re)paint from the current model. Called on each frame and each tick.
/// Prunes cards that have run out locally (the daemon's empty frame usually
/// arrives moments later, but the UI must not wait on it) and stops the ticker
/// once nothing is live.
function paintPermissionCards(): void {
  const elapsed = Math.floor((performance.now() - permReceivedAt) / 1000);
  const live = permCards.filter((c) => c.remaining_secs - elapsed > 0);
  permCards = live;
  permissionCards.textContent = "";
  for (const card of live) {
    permissionCards.appendChild(permCardEl(card, card.remaining_secs - elapsed));
  }
  permissionCards.hidden = live.length === 0;
  if (live.length === 0 && permTicker !== undefined) {
    window.clearInterval(permTicker);
    permTicker = undefined;
  }
}

function permCardEl(card: PermissionCard, left: number): HTMLElement {
  const el = document.createElement("div");
  el.className = "perm-card";

  const head = document.createElement("div");
  head.className = "perm-head";
  const title = document.createElement("span");
  title.className = "perm-title";
  // source is the agent, tool is what it wants to run.
  title.textContent = `${card.source} · ${card.tool}`;
  const countdown = document.createElement("span");
  countdown.className = "perm-countdown";
  countdown.textContent = `${left}s`;
  head.append(title, countdown);

  const summary = document.createElement("div");
  summary.className = "perm-summary";
  // A truncated summary is only a prefix — mark it visibly with an ellipsis.
  summary.textContent = (card.summary || "(no detail)") + (card.truncated ? " …" : "");

  const actions = document.createElement("div");
  actions.className = "perm-actions";
  const deciding = permDeciding.has(card.id);
  const approve = document.createElement("button");
  approve.className = "btn perm-approve";
  approve.textContent = "Approve";
  // When the input was too long to show in full, the phone must not Allow it —
  // a hidden suffix could be destructive. Disable Approve only; Deny is always
  // safe, and the host (which sees the whole command) can still approve.
  approve.disabled = deciding || !!card.truncated;
  if (card.truncated) approve.title = "Full command not shown — approve on the host";
  approve.addEventListener("click", () => void decidePermission(card, "allow"));
  const deny = document.createElement("button");
  deny.className = "btn perm-deny";
  deny.textContent = "Deny";
  deny.disabled = deciding;
  deny.addEventListener("click", () => void decidePermission(card, "deny"));
  actions.append(approve, deny);

  el.append(head, summary);
  if (card.truncated) {
    const warn = document.createElement("div");
    warn.className = "perm-warn";
    warn.textContent = "⚠ too long to show in full — approve on the host";
    el.append(warn);
  }
  el.append(actions);

  if (card.session !== sessionTitle) {
    const note = document.createElement("div");
    note.className = "perm-note";
    note.textContent = `in session ${card.session}`;
    el.append(note);
  }
  return el;
}

async function decidePermission(card: PermissionCard, decision: "allow" | "deny"): Promise<void> {
  if (permDeciding.has(card.id)) return;
  permDeciding.add(card.id);
  paintPermissionCards();
  try {
    const resp = await fetch(`/api/permissions/${encodeURIComponent(card.id)}/decide`, {
      method: "POST",
      headers: { ...authHeader(), "content-type": "application/json" },
      body: JSON.stringify({ decision }),
    });
    // Drop the card locally only on a terminal outcome (decided, or the daemon
    // says it's gone). On an unexpected failure keep it: the WS reconcile is
    // the source of truth and will repair, and the buttons re-enable.
    const terminal = resp.ok || [409, 410, 404, 403].includes(resp.status);
    if (terminal) {
      permCards = permCards.filter((c) => c.id !== card.id);
      paintPermissionCards();
    }
    if (resp.ok) {
      showHint(decision === "allow" ? "Approved" : "Denied");
    } else if (resp.status === 409) {
      showHint("Too late — the request was already answered");
    } else if (resp.status === 410) {
      showHint("That request expired");
    } else if (resp.status === 404) {
      showHint("That request is no longer pending");
    } else if (resp.status === 403) {
      showHint("This device can't approve — grant it on the host");
    } else if (resp.status === 422) {
      // Truncated card: server refused a remote Allow. Card stays open (Deny
      // still works), so this is deliberately not terminal.
      showHint("Full command not shown — approve on the host");
    } else {
      showHint("Could not send the decision — try again");
    }
  } catch {
    showHint("Could not reach the daemon");
  } finally {
    permDeciding.delete(card.id);
    // Repaint so buttons re-enable on a non-terminal outcome (e.g. a 422 that
    // leaves the card open); terminal outcomes already dropped the card.
    paintPermissionCards();
  }
}

// ---------- M4c command feed ----------

const feedPanel = $("command-feed-panel");
const feedBtn = $<HTMLButtonElement>("feed-btn");
let feedCommands: FeedCommand[] = [];
let feedOpen = false;
// When the current snapshot arrived, so relative ages keep advancing between
// frames (the daemon's age_ms is a point-in-time value).
let feedReceivedAt = 0;
let feedTicker: number | undefined;

function onCommandFeed(commands: FeedCommand[]): void {
  feedCommands = commands;
  feedReceivedAt = performance.now();
  if (feedOpen) paintFeed();
}

function toggleFeed(): void {
  feedOpen = !feedOpen;
  menu.hidden = true;
  feedPanel.hidden = !feedOpen;
  if (feedOpen) {
    paintFeed();
    if (feedTicker === undefined) feedTicker = window.setInterval(paintFeed, 15000);
  } else if (feedTicker !== undefined) {
    window.clearInterval(feedTicker);
    feedTicker = undefined;
  }
}

function clearFeed(): void {
  feedCommands = [];
  endRecall(); // recall list is per-session; reset on switch
  if (feedOpen) paintFeed();
}

// ---------- Pane views (custom dashboards) ----------

const dashboardPanel = $("dashboard-panel");
const viewToggleBtn = $<HTMLButtonElement>("view-toggle-btn");
let paneViews: PaneView[] = [];
let dashboardMode = false;

/** The view for the pane we're actually looking at. Only falls back to the
 *  first available while topology is still unknown — otherwise a split could
 *  show an unrelated pane's dashboard. */
function currentView(): PaneView | undefined {
  if (paneViews.length === 0) return undefined;
  const active = activePaneId();
  if (active === undefined) return paneViews[0]; // topology not loaded yet
  return paneViews.find((v) => v.pane === active);
}

/** Re-evaluate the toggle + dashboard against the current view. Called on a new
 *  pane_views frame AND on topology changes (the active pane may have moved). */
function refreshPaneView(): void {
  const v = currentView();
  // The toggle only exists while a source is streaming a view for THIS pane.
  viewToggleBtn.hidden = v === undefined;
  if (v === undefined && dashboardMode) {
    setDashboard(false); // no view for this pane → fall back to the terminal
  } else if (dashboardMode) {
    renderDashboard();
  }
}

function onPaneViews(views: PaneView[]): void {
  paneViews = views;
  refreshPaneView();
}

/** Drop all pane views and leave dashboard mode — on reconnect / session switch. */
function clearPaneViews(): void {
  paneViews = [];
  viewToggleBtn.hidden = true;
  if (dashboardMode) setDashboard(false);
}

function setDashboard(on: boolean): void {
  dashboardMode = on;
  dashboardPanel.hidden = !on;
  viewToggleBtn.textContent = on ? "Terminal" : "Dashboard";
  viewToggleBtn.classList.toggle("primary", on);
  if (on) {
    // A dashboard is not a terminal view: stop driving tmux size. If we're the
    // controller, hand control back so the now-hidden xterm can't keep shrinking
    // the desktop layout (window-size latest).
    if (isController) sendJson({ type: "release_control" });
    // Ask the daemon to render this pane at a big "capture resolution" so a
    // full-screen tool (htop) exposes all its info to the dashboard. The
    // terminal is hidden now, so the oversized render isn't seen.
    sendJson({ type: "view_mode", pane: currentView()?.pane ?? "", dashboard: true });
    renderDashboard();
  } else {
    sendJson({ type: "view_mode", pane: "", dashboard: false }); // restore size
    handle.fit(); // terminal is visible again — remeasure the grid
  }
}

function toggleDashboard(): void {
  menu.hidden = true;
  setDashboard(!dashboardMode);
}

function renderDashboard(): void {
  const v = currentView();
  if (!v) {
    dashboardPanel.textContent = "";
    htChrome = null;
    return;
  }
  if (v.view === "htop.v1") {
    // Stateful: keep the toolbar (filter input focus!) across 1.5s updates.
    renderHtopInto(v.state, v.pane);
    return;
  }
  htChrome = null;
  dashboardPanel.textContent = "";
  if (v.view === "taskscope.v1") {
    dashboardPanel.appendChild(renderTaskscope(v.state));
  } else {
    const unknown = document.createElement("div");
    unknown.className = "dash-empty";
    unknown.textContent = `No renderer for “${v.view}”.`;
    dashboardPanel.appendChild(unknown);
  }
}

// --- htop.v1 renderer: a live "instrument panel" over the real htop ---

interface HtChrome {
  pane: string;
  root: HTMLElement;
  sys: HTMLElement;
  list: HTMLElement;
  sorts: Record<string, HTMLButtonElement>;
}
let htChrome: HtChrome | null = null;
let htSortKey = "cpu"; // active sort, remembered for the toolbar highlight

function htAction(pane: string, action: string): void {
  sendJson({ type: "pane_action", pane, action });
}

/** "" (calm) | "warn" (violet) | "crit" (red) by threshold — restrained color. */
function loadClass(v: number, warn: number, crit: number): string {
  return v >= crit ? "crit" : v >= warn ? "warn" : "";
}

function updateHtSorts(sorts: Record<string, HTMLButtonElement>): void {
  for (const [key, b] of Object.entries(sorts)) b.classList.toggle("active", key === htSortKey);
}

function buildHtChrome(pane: string): HtChrome {
  const root = document.createElement("div");
  root.className = "ht";

  const sys = document.createElement("div");
  sys.className = "ht-sys";

  // Sticky toolbar: filter + tree on top, sort row below.
  const bar = document.createElement("div");
  bar.className = "ht-toolbar";

  const top = document.createElement("div");
  top.className = "ht-tbar-top";
  const filter = document.createElement("input");
  filter.className = "ht-filter";
  filter.type = "text";
  filter.placeholder = "Filter processes…";
  filter.autocapitalize = "off";
  filter.autocomplete = "off";
  filter.spellcheck = false;
  filter.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      htAction(pane, `filter:${filter.value}`);
      filter.blur();
    }
  });
  const tree = document.createElement("button");
  tree.className = "ht-tool";
  tree.textContent = "Tree";
  tree.addEventListener("click", () => {
    tree.classList.toggle("active");
    htAction(pane, "tree");
  });
  top.append(filter, tree);

  const sortRow = document.createElement("div");
  sortRow.className = "ht-sorts";
  const sl = document.createElement("span");
  sl.className = "ht-sorts-l";
  sl.textContent = "sort";
  sortRow.appendChild(sl);
  const sorts: Record<string, HTMLButtonElement> = {};
  for (const [key, label] of [
    ["cpu", "CPU"],
    ["mem", "MEM"],
    ["time", "TIME"],
  ] as const) {
    const b = document.createElement("button");
    b.className = "ht-sort";
    b.textContent = label;
    b.addEventListener("click", () => {
      if (htSortKey === key) htAction(pane, "invert"); // tap active → reverse
      else {
        htSortKey = key;
        htAction(pane, `sort:${key}`);
      }
      updateHtSorts(sorts);
    });
    sorts[key] = b;
    sortRow.appendChild(b);
  }
  bar.append(top, sortRow);

  const list = document.createElement("div");
  list.className = "ht-list";

  root.append(sys, bar, list);
  return { pane, root, sys, list, sorts };
}

function renderHtopInto(state: Record<string, unknown>, pane: string): void {
  if (!htChrome || htChrome.pane !== pane) {
    dashboardPanel.textContent = "";
    htChrome = buildHtChrome(pane);
    dashboardPanel.appendChild(htChrome.root);
    updateHtSorts(htChrome.sorts);
  }
  const summary = (state.summary ?? {}) as Record<string, unknown>;
  const procs = Array.isArray(state.processes) ? (state.processes as Record<string, unknown>[]) : [];
  renderHtSys(htChrome.sys, summary, procs.length);
  renderHtList(htChrome.list, procs, pane, state.confidence === "low");
}

function htMeter(pct: number, cls: string): HTMLElement {
  const m = document.createElement("div");
  m.className = "ht-meter";
  const f = document.createElement("div");
  f.className = "ht-meter-f " + cls;
  f.style.width = `${Math.max(0, Math.min(100, pct))}%`;
  m.appendChild(f);
  return m;
}

/** "2.27G/3.83G" → percent used, for the memory bar. */
function memPercent(mem: string): number {
  const m = mem.match(/([\d.]+)\s*([KMGT]?)\s*\/\s*([\d.]+)\s*([KMGT]?)/);
  if (!m) return 0;
  const u: Record<string, number> = { K: 1, M: 1024, G: 1024 ** 2, T: 1024 ** 3, "": 1 };
  const used = parseFloat(m[1]) * (u[m[2]] ?? 1);
  const total = parseFloat(m[3]) * (u[m[4]] ?? 1);
  return total > 0 ? (used / total) * 100 : 0;
}

function renderHtSys(sys: HTMLElement, s: Record<string, unknown>, tasks: number): void {
  sys.textContent = "";
  const cpu = Number(s.cpu_pct ?? 0);

  const cpuRow = document.createElement("div");
  cpuRow.className = "ht-sys-row";
  cpuRow.append(htSysL("CPU"), htSysV(`${cpu.toFixed(1)}%`), htMeter(cpu, loadClass(cpu, 60, 90)));
  const tk = document.createElement("span");
  tk.className = "ht-sys-x";
  tk.textContent = `${tasks} tasks`;
  cpuRow.append(tk);
  sys.appendChild(cpuRow);

  if (s.mem) {
    const memPct = memPercent(String(s.mem));
    const memRow = document.createElement("div");
    memRow.className = "ht-sys-row";
    memRow.append(htSysL("MEM"), htSysV(String(s.mem)), htMeter(memPct, loadClass(memPct, 75, 90)));
    sys.appendChild(memRow);
  }

  const meta: string[] = [];
  if (s.load) meta.push(`load ${s.load}`);
  if (s.uptime) meta.push(`up ${s.uptime}`);
  if (meta.length) {
    const m = document.createElement("div");
    m.className = "ht-sys-meta";
    m.textContent = meta.join("   ·   ");
    sys.appendChild(m);
  }
}

function htSysL(t: string): HTMLElement {
  const e = document.createElement("span");
  e.className = "ht-sys-l";
  e.textContent = t;
  return e;
}
function htSysV(t: string): HTMLElement {
  const e = document.createElement("span");
  e.className = "ht-sys-v";
  e.textContent = t;
  return e;
}

function renderHtList(
  list: HTMLElement,
  procs: Record<string, unknown>[],
  pane: string,
  low: boolean
): void {
  list.textContent = "";
  if (low || procs.length === 0) {
    const note = document.createElement("div");
    note.className = "dash-empty";
    note.textContent = "Couldn't read htop's screen — tap Terminal for the live view.";
    list.appendChild(note);
    return;
  }
  for (const p of procs) list.appendChild(htRow(p, pane));
}

function htRow(p: Record<string, unknown>, pane: string): HTMLElement {
  const cpu = Number(p.cpu ?? 0);
  const mem = Number(p.mem ?? 0);
  const cpuCls = loadClass(cpu, 60, 90);

  const row = document.createElement("div");
  row.className = "ht-r";

  const rail = document.createElement("div");
  rail.className = "ht-rail";
  const rf = document.createElement("div");
  rf.className = "ht-rail-f " + cpuCls;
  rf.style.height = `${Math.max(2, Math.min(100, cpu))}%`;
  rail.appendChild(rf);

  const body = document.createElement("div");
  body.className = "ht-r-body";

  const l1 = document.createElement("div");
  l1.className = "ht-r-line";
  const cmd = document.createElement("span");
  cmd.className = "ht-cmd";
  cmd.textContent = String(p.command || `pid ${p.pid ?? "?"}`);
  const cpuEl = document.createElement("span");
  cpuEl.className = "ht-cpu " + cpuCls;
  cpuEl.textContent = cpu.toFixed(1);
  l1.append(cmd, cpuEl);

  const l2 = document.createElement("div");
  l2.className = "ht-r-line ht-r-sub";
  const who = document.createElement("span");
  who.className = "ht-who";
  who.textContent = `${p.pid ?? "?"} ${String(p.user ?? "")}`;
  const nums = document.createElement("span");
  nums.className = "ht-nums";
  nums.textContent = `${String(p.res ?? "")}   M ${mem.toFixed(1)}   ${String(p.time ?? "")}`;
  l2.append(who, nums);

  body.append(l1, l2);
  row.append(rail, body);
  row.addEventListener("click", () => openKillSheet(p, pane));
  return row;
}

/** Deliberate, confirmed kill — never a one-tap action. */
function openKillSheet(p: Record<string, unknown>, pane: string): void {
  const pid = Number(p.pid ?? 0);
  if (!pid) return;
  const bg = document.createElement("div");
  bg.className = "ht-sheet-bg";
  const sheet = document.createElement("div");
  sheet.className = "ht-sheet";
  const cmd = document.createElement("div");
  cmd.className = "ht-sheet-cmd";
  cmd.textContent = String(p.command || `pid ${pid}`);
  const meta = document.createElement("div");
  meta.className = "ht-sheet-meta";
  meta.textContent = `pid ${pid} · ${String(p.user ?? "")} · cpu ${Number(p.cpu ?? 0).toFixed(
    1
  )}% · mem ${Number(p.mem ?? 0).toFixed(1)}%`;
  const btns = document.createElement("div");
  btns.className = "ht-sheet-btns";
  const cancel = document.createElement("button");
  cancel.className = "btn";
  cancel.textContent = "Cancel";
  const kill = document.createElement("button");
  kill.className = "btn ht-kill";
  kill.textContent = `Kill ${pid}`;
  const close = (): void => bg.remove();
  cancel.addEventListener("click", close);
  bg.addEventListener("click", (e) => {
    if (e.target === bg) close();
  });
  kill.addEventListener("click", () => {
    htAction(pane, `kill:${pid}`);
    close();
  });
  btns.append(cancel, kill);
  sheet.append(cmd, meta, btns);
  bg.appendChild(sheet);
  $("app").appendChild(bg);
}

// --- taskscope.v1 renderer (hard-coded; one built-in view) ---

function tsStatusClass(status: string): string {
  switch (status) {
    case "running":
      return "run";
    case "done":
      return "done";
    case "error":
      return "err";
    default:
      return "idle";
  }
}

function renderTaskscope(state: Record<string, unknown>): HTMLElement {
  const root = document.createElement("div");
  root.className = "ts";
  const workers = Array.isArray(state.workers) ? (state.workers as Record<string, unknown>[]) : [];

  const head = document.createElement("div");
  head.className = "ts-head";
  head.textContent = `taskscope · ${workers.length} worker${workers.length === 1 ? "" : "s"}`;
  root.appendChild(head);

  if (workers.length === 0) {
    const empty = document.createElement("div");
    empty.className = "dash-empty";
    empty.textContent = "No workers.";
    root.appendChild(empty);
    return root;
  }
  for (const w of workers) root.appendChild(taskscopeCard(w));
  return root;
}

function taskscopeCard(w: Record<string, unknown>): HTMLElement {
  const status = String(w.status ?? "");
  const cls = tsStatusClass(status);
  const cpu = Number(w.cpu ?? 0);
  const mem = Number(w.mem ?? 0);
  const progress = Math.max(0, Math.min(100, Number(w.progress ?? 0)));

  const card = document.createElement("div");
  card.className = "ts-card";

  const row = document.createElement("div");
  row.className = "ts-row";
  const name = document.createElement("span");
  name.className = "ts-name";
  name.textContent = String(w.name ?? "?");
  const badge = document.createElement("span");
  badge.className = `ts-badge ${cls}`;
  badge.textContent = status;
  row.append(name, badge);

  const meta = document.createElement("div");
  meta.className = "ts-meta";
  meta.textContent = `${cpu}% CPU · ${mem} MB`;

  const bar = document.createElement("div");
  bar.className = "ts-bar";
  const fill = document.createElement("div");
  fill.className = `ts-fill ${cls}`;
  fill.style.width = `${progress}%`;
  bar.appendChild(fill);

  card.append(row, meta, bar);
  return card;
}

function paintFeed(): void {
  feedPanel.textContent = "";
  if (feedCommands.length === 0) {
    const empty = document.createElement("div");
    empty.className = "feed-empty";
    empty.textContent = "No commands yet. Install the zsh hook to see this session's command history.";
    feedPanel.appendChild(empty);
    return;
  }
  // Snapshot is oldest→newest; show newest first.
  for (let i = feedCommands.length - 1; i >= 0; i--) {
    feedPanel.appendChild(feedCardEl(feedCommands[i]));
  }
}

function feedCardEl(c: FeedCommand): HTMLElement {
  const el = document.createElement("div");
  el.className = "feed-card";

  const badge = document.createElement("span");
  if (c.state === "running") {
    badge.className = "feed-badge running";
    badge.textContent = "···";
  } else if (c.state === "aborted") {
    badge.className = "feed-badge aborted";
    badge.textContent = "—";
  } else if (c.exit === 0) {
    badge.className = "feed-badge ok";
    badge.textContent = "✓";
  } else {
    badge.className = "feed-badge fail";
    badge.textContent = String(c.exit ?? "?");
  }

  const body = document.createElement("div");
  body.className = "feed-body";
  const cmd = document.createElement("div");
  cmd.className = "feed-cmd";
  cmd.textContent = c.command || "(no command)"; // textContent — never HTML
  const meta = document.createElement("div");
  meta.className = "feed-meta";
  meta.textContent = feedMeta(c);
  body.append(cmd, meta);

  el.append(badge, body);

  // The feed is this session's; tapping a card just returns to the terminal.
  el.style.cursor = "pointer";
  el.addEventListener("click", toggleFeed);
  return el;
}

function feedMeta(c: FeedCommand): string {
  const parts: string[] = [];
  if (c.state === "running") parts.push("running");
  else if (c.state === "aborted") parts.push("aborted");
  else if (c.elapsed_ms != null) parts.push(humanMs(c.elapsed_ms));
  parts.push(agoFrom(c.age_ms + (performance.now() - feedReceivedAt)));
  const dir = shortCwd(c.cwd);
  if (dir) parts.push(dir);
  return parts.join(" · ");
}

/// Last two path segments of a cwd (~/… kept), for compact display.
function shortCwd(cwd: string): string {
  if (!cwd) return "";
  const parts = cwd.split("/").filter(Boolean);
  return parts.length <= 2 ? cwd : "…/" + parts.slice(-2).join("/");
}

function humanMs(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  return `${Math.floor(s / 3600)}h${Math.floor((s % 3600) / 60)}m`;
}

function agoFrom(ageMs: number): string {
  const s = Math.round(ageMs / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

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
  // This fires only while backgrounded, so it lands on the lock screen — build
  // the body from a fixed template + our own session label ONLY, never the
  // producer-supplied source/reason (they can carry secrets). Full detail is
  // shown in-app after the user opens it. Matches sw.js.
  const opts: NotificationOptions = {
    body: `${sessionTitle || "session"} — needs your attention`,
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
    // Single-line paste while composing lands in the field — you see what
    // you pasted before it can run. "Composing" = field focused or holding
    // a draft (tapping the menu's paste button blurs the field first).
    // Multi-line paste keeps going through xterm so bracketed paste
    // applies; a text input would silently flatten the newlines.
    const composing = composerFocused() || composerInput.value !== "";
    if (composing && !text.includes("\n") && !text.includes("\r")) {
      insertIntoComposer(text);
      // Routed here on a stale draft alone? Focus the field so it's
      // visible where the paste went instead of a silent surprise.
      composerInput.focus();
    } else {
      handle.term.paste(text);
    }
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
feedBtn.addEventListener("click", toggleFeed);
viewToggleBtn.addEventListener("click", toggleDashboard);
renderNotifyBtn();

// ---------- command composer ----------

/// Mobile-friendly alternative to typing straight into the terminal: a text
/// field that sends a full line. Submitting as an observer requests control
/// (the line is buffered and flushed by the existing take-control path).
const HISTORY_MAX = 50;
// Composer recall (↑ / ▴) draws from this session's real command history: the
// feed (every command run in the session, from the Mac or the phone) plus
// commands typed from the composer. ALL of it is memory-only, per session:
// command lines can contain secrets, so — like the feed — typed history is
// never written to localStorage/disk. It is intentionally cleared on reload.
// Recall is anchored to a snapshot frozen when it starts, so a feed frame
// arriving mid-recall can't shift the positional index under the user.
let recallIdx: number | null = null; // index into recallSnapshot; 0 = newest
let recallSnapshot: string[] = [];
// Kept only to avoid double-storing feed commands (already in `feedCommands`)
// into the typed-history buffer; persistence provenance no longer matters now
// that nothing is persisted.
let composerFromFeed = false;

// Per-session typed-command history, MEMORY ONLY (see above). session -> lines.
const typedHistoryMem = new Map<string, string[]>();

function typedHistory(): string[] {
  if (!sessionTitle) return [];
  return typedHistoryMem.get(sessionTitle) ?? [];
}

/// Record a *typed* command for this session, in memory only. Skips
/// feed-derived text (already recallable via the feed) and the no-session case.
function recordTyped(cmd: string): void {
  if (!sessionTitle || composerFromFeed) return;
  const h = typedHistory();
  if (h[h.length - 1] === cmd) return;
  typedHistoryMem.set(sessionTitle, [...h, cmd].slice(-HISTORY_MAX));
}

function feedCommandSet(): Set<string> {
  return new Set(feedCommands.map((c) => c.command).filter(Boolean));
}

/// Newest-first recall list: the session's feed commands first (the actual
/// shell history, Mac or phone), then typed commands not already present.
function recallList(): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  for (let i = feedCommands.length - 1; i >= 0; i--) {
    const c = feedCommands[i].command;
    if (c && !seen.has(c)) {
      seen.add(c);
      out.push(c);
    }
  }
  const typed = typedHistory();
  for (let i = typed.length - 1; i >= 0; i--) {
    if (!seen.has(typed[i])) {
      seen.add(typed[i]);
      out.push(typed[i]);
    }
  }
  return out;
}

/// True after a Tab flushed a draft to the shell: the real command line
/// lives in the terminal now, partially completed there.
let shellLinePending = false;

function composerSubmit(): void {
  const text = composerInput.value;
  if (!text && !shellLinePending) return;
  if (!text) {
    // Empty submit finishes the tab-completed line already in the shell.
    sendInput("\r");
    shellLinePending = false;
    return;
  }
  sendInput(text + "\r");
  // After a tab-flush the field only holds the suffix — recording it would
  // pollute history with an invalid partial command.
  if (!shellLinePending) recordTyped(text);
  shellLinePending = false;
  endRecall();
  composerInput.value = "";
}

/// End an in-progress recall and clear its provenance.
function endRecall(): void {
  recallIdx = null;
  recallSnapshot = [];
  composerFromFeed = false;
}

/// Shell completion from the composer: the draft must live in the shell's
/// input buffer for Tab to mean anything — flush it un-submitted, then Tab.
/// The completed line continues in the terminal; the composer clears so a
/// following submit appends to that same shell line.
function composerTabComplete(): void {
  sendInput(composerInput.value + "\t");
  shellLinePending = true;
  composerInput.value = "";
  endRecall();
}

composerInput.addEventListener("keydown", (ev) => {
  // Armed ⌃ (key row) or a hardware Ctrl: the next letter is a control
  // code for the terminal, not text for the field.
  if (ev.key.length === 1 && /[a-z]/i.test(ev.key)) {
    const viaHardware = ev.ctrlKey && !ev.metaKey && !ev.altKey;
    const transformed = viaHardware
      ? String.fromCharCode(ev.key.toLowerCase().charCodeAt(0) & 0x1f)
      : applyCtrl(ev.key);
    if (transformed !== ev.key) {
      ev.preventDefault();
      if (viaHardware) disarmCtrl(); // don't leave a stale sticky ⌃ armed
      sendInput(transformed);
      return;
    }
  }
  if (ev.key === "Tab") {
    ev.preventDefault();
    composerTabComplete();
  } else if (ev.key === "Enter") {
    ev.preventDefault();
    composerSubmit();
  } else if (ev.key === "ArrowUp") {
    ev.preventDefault();
    composerHistoryPrev(false);
  } else if (ev.key === "ArrowDown" && recallIdx !== null) {
    ev.preventDefault();
    composerHistoryNext();
  }
});

/// Step forward (toward newer) through the frozen recall snapshot; reaching the
/// newest end clears the field. No-op when not currently recalling.
function composerHistoryNext(): void {
  if (recallIdx === null) return;
  recallIdx = recallIdx <= 0 ? null : recallIdx - 1;
  if (recallIdx === null) {
    composerInput.value = "";
    composerFromFeed = false;
  } else {
    const val = recallSnapshot[recallIdx] ?? "";
    composerFromFeed = feedCommandSet().has(val);
    composerInput.value = val;
  }
}

// Manually clearing the field drops feed provenance, so a fresh command typed
// from scratch persists normally.
composerInput.addEventListener("input", () => {
  if (composerInput.value === "") composerFromFeed = false;
});

/// Step back through composer history into the (editable) field. The ▴
/// button wraps from oldest to newest — one button must never dead-end.
function composerHistoryPrev(wrap: boolean): void {
  if (recallIdx === null) {
    // Freeze the list for this recall so an incoming feed frame can't shift
    // the index mid-recall.
    recallSnapshot = recallList();
    if (recallSnapshot.length === 0) {
      // Silence reads as "broken" — say why there's nothing to recall.
      flashPlaceholder("no command history yet for this session");
      return;
    }
    recallIdx = 0; // newest
  } else if (recallIdx >= recallSnapshot.length - 1) {
    if (wrap) recallIdx = 0; // oldest → wrap to newest (▴ never dead-ends)
  } else {
    recallIdx += 1; // older
  }
  const val = recallSnapshot[recallIdx];
  composerFromFeed = feedCommandSet().has(val);
  composerInput.value = val;
}

// pointerdown + preventDefault keeps focus (and the keyboard) in the input.
$("composer-send").addEventListener("pointerdown", (ev) => {
  ev.preventDefault();
  composerSubmit();
});
$("composer-hist").addEventListener("pointerdown", (ev) => {
  ev.preventDefault();
  composerHistoryPrev(true);
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
  // Debounced by term.ts: the settled grid we render is exactly what we
  // report to tmux.
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
  if (hintAction) {
    const action = hintAction;
    hintAction = null;
    action();
  } else {
    requestControl();
  }
});

// Tap the connection status to see terminal sizing diagnostics (helps debug
// device-specific grid problems). Shown as a long-lived hint.
// ---------- debug overlay ----------

const DEBUG_KEY = "remux.debug";
const debugBtn = $<HTMLButtonElement>("debug-btn");
const debugOverlay = $("debug-overlay");
let debugOn = localStorage.getItem(DEBUG_KEY) === "on";
let debugTimer: number | undefined;

function standaloneMode(): string {
  const s =
    matchMedia("(display-mode: standalone)").matches ||
    (navigator as { standalone?: boolean }).standalone === true;
  return s ? "PWA" : "browser-tab";
}

function updateDebug(): void {
  if (!debugOn) return;
  const role = isController ? "controller" : "observer";
  debugOverlay.textContent = [
    `remux debug · ${standaloneMode()} · ${role}`,
    `session ${sessionTitle || "?"}`,
    handle.debug(),
    `ua ${navigator.userAgent.slice(0, 60)}`,
  ].join("\n");
}

function renderDebugBtn(): void {
  debugBtn.textContent = `Debug: ${debugOn ? "on" : "off"}`;
}

function applyDebug(): void {
  debugOverlay.hidden = !debugOn;
  clearInterval(debugTimer);
  if (debugOn) {
    updateDebug();
    debugTimer = window.setInterval(updateDebug, 500);
  }
  renderDebugBtn();
}

debugBtn.addEventListener("click", () => {
  debugOn = !debugOn;
  localStorage.setItem(DEBUG_KEY, debugOn ? "on" : "off");
  applyDebug();
});
applyDebug();

// Renderer-independent view of the terminal buffer (the WebGL renderer draws
// to a canvas, leaving the DOM rows empty). Used by e2e; harmless in prod.
(window as unknown as { __termText?: () => string }).__termText = () => {
  const b = handle.term.buffer.active;
  const start = Math.max(0, b.length - 400);
  let out = "";
  for (let i = start; i < b.length; i++) {
    out += (b.getLine(i)?.translateToString(true) ?? "") + "\n";
  }
  return out;
};
(window as unknown as { __termCols?: () => number }).__termCols = () =>
  handle.size().cols;
(window as unknown as { __topology?: () => SessionTopo[] }).__topology = () =>
  topology;

function composerFocused(): boolean {
  return document.activeElement === composerInput;
}

/// Insert at the field's cursor, preserving selection semantics.
function insertIntoComposer(text: string): void {
  const value = composerInput.value;
  const s = composerInput.selectionStart ?? value.length;
  const e = composerInput.selectionEnd ?? value.length;
  composerInput.value = value.slice(0, s) + text + value.slice(e);
  const pos = s + text.length;
  composerInput.setSelectionRange(pos, pos);
}

function moveComposerCursor(target: "left" | "right" | "home" | "end"): void {
  const len = composerInput.value.length;
  const s = composerInput.selectionStart ?? 0;
  const e = composerInput.selectionEnd ?? s;
  let next: number;
  if (target === "home") {
    next = 0;
  } else if (target === "end") {
    next = len;
  } else if (s !== e) {
    // A selected range collapses to its edge, like native cursor keys.
    next = target === "left" ? s : e;
  } else {
    next = Math.max(0, Math.min(len, s + (target === "left" ? -1 : 1)));
  }
  composerInput.setSelectionRange(next, next);
}

/// Key-row keys that act on the composer while it's focused: punctuation
/// inserts into the draft (that's why those keys exist — iOS buries them),
/// and cursor keys edit the draft when there is one. Arrows on an empty
/// field still reach the terminal — TUIs need them.
const COMPOSER_INSERT = new Set(["-", "|", "/", "~"]);
const COMPOSER_CURSOR: Record<string, "left" | "right" | "home" | "end"> = {
  "\x1b[D": "left",
  "\x1b[C": "right",
  "\x1b[H": "home",
  "\x1b[F": "end",
};

/// The tmux pane the terminal is currently showing (active pane of the active
/// window in the current session), or undefined if topology hasn't said yet.
function activePaneId(): string | undefined {
  return activeWindow()?.panes.find((p) => p.active)?.id;
}

/// Should the key-row ↑/↓ drive the composer's history recall, or pass through
/// to the terminal? Recall only when the M4c feed says the ACTIVE PANE is idle
/// at a prompt (its newest command finished). Crucially this is scoped to the
/// active pane, not the session's newest command: with a split where pane A runs
/// vim and pane B just finished, the session-newest entry would be "done" and
/// would wrongly steal vim's arrows. While a command/tool is running in this
/// pane — or the pane has no feed history, or the active pane is unknown — the
/// arrows pass straight through, so vim/htop/less always receive them. Uses the
/// feed (shell-hook updated in ~real time), never tmux's pane_current_command,
/// which isn't refreshed on a foreground-process change (stale → could hijack).
function keyRowArrowsRecall(): boolean {
  const pane = activePaneId();
  if (!pane) return false; // unknown active pane → safe: pass through
  for (let i = feedCommands.length - 1; i >= 0; i--) {
    if (feedCommands[i].pane === pane) {
      return feedCommands[i].state !== "running";
    }
  }
  return false; // no feed history for this pane → safe: pass through
}

setupKeyRow((data) => {
  if (data === "\t" && composerInput.value) {
    composerTabComplete();
  } else if ((data === "\x1b[A" || data === "\x1b[B") && keyRowArrowsRecall()) {
    // At a shell prompt, ↑/↓ recall the session's commands into the editable
    // composer (what "up" is expected to do on the phone). A running command or
    // tool makes this false, so the arrows pass through to the terminal.
    if (data === "\x1b[A") composerHistoryPrev(false);
    else composerHistoryNext();
  } else if (composerFocused() && COMPOSER_INSERT.has(data)) {
    insertIntoComposer(data);
  } else if (composerFocused() && composerInput.value && COMPOSER_CURSOR[data]) {
    moveComposerCursor(COMPOSER_CURSOR[data]);
  } else {
    sendInput(data);
  }
});
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

// When the on-screen keyboard opens, make sure the focused pairing field is
// scrolled into the (now smaller) visual viewport rather than hidden behind it.
$<HTMLInputElement>("pair-input").addEventListener("focus", () => {
  setTimeout(() => {
    $("pair-input").scrollIntoView({ block: "center", behavior: "smooth" });
  }, 300);
});

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
