import "./style.css";
import { createTerminal } from "./term";
import { setupKeyRow, applyCtrl } from "./keys";
import { setupTouchScroll } from "./scroll";

const TOKEN_KEY = "remux.device_token";
const FONT_KEY = "remux.font";
const FONT_MIN = 10;
const FONT_MAX = 22;

const $ = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T;

const connDot = $("conn-dot");
const sessionName = $("session-name");
const rolePill = $("role-pill");
const controlBtn = $<HTMLButtonElement>("control-btn");
const menuBtn = $<HTMLButtonElement>("menu-btn");
const menu = $("menu");
const hint = $<HTMLButtonElement>("hint");
const setup = $("setup");
const setupError = $("setup-error");

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
const handle = createTerminal($("terminal"), fontSize);

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

function setStatus(text: string, state: "connected" | "connecting" | "offline"): void {
  sessionName.textContent = text;
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
  rolePill.hidden = false;
  rolePill.textContent = controller ? "controller" : "observer";
  rolePill.classList.toggle("controller", controller);
  controlBtn.hidden = false;
  controlBtn.textContent = controller ? "Release" : "Take control";
  menuBtn.hidden = false;
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
    sendJson({ type: "auth", token, cols, rows });
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
    setRole(false);
    if (!intentionalClose) {
      setStatus("offline — reconnecting…", "offline");
      scheduleReconnect();
    }
    intentionalClose = false;
  };
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
    case "error":
      if (msg.code === "auth_failed") {
        localStorage.removeItem(TOKEN_KEY);
        intentionalClose = true;
        ws?.close();
        showSetup("This device is no longer paired. Pair it again.");
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
  menu.hidden = !menu.hidden;
});
document.addEventListener("click", (ev) => {
  if (!menu.hidden && !menu.contains(ev.target as Node)) {
    menu.hidden = true;
  }
});
$("font-dec").addEventListener("click", () => applyFont(fontSize - 1));
$("font-inc").addEventListener("click", () => applyFont(fontSize + 1));
$("paste-btn").addEventListener("click", () => void pasteFromClipboard());

// ---------- wire up ----------

handle.term.onData((data) => {
  if (RESPONSE_RE.test(data)) {
    sendInput(data, { takeControl: false, silent: true });
  } else {
    sendInput(applyCtrl(data));
  }
});
handle.onResize((cols, rows) => sendJson({ type: "resize", cols, rows }));

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
setupTouchScroll($("terminal"), handle.term, (data) =>
  sendInput(data, { takeControl: false })
);

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
