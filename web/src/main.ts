import "./style.css";
import { createTerminal } from "./term";
import { setupKeyRow, applyCtrl } from "./keys";
import { setupTouchScroll } from "./scroll";

const TOKEN_KEY = "remux.device_token";

const $ = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T;

const connDot = $("conn-dot");
const sessionName = $("session-name");
const rolePill = $("role-pill");
const controlBtn = $<HTMLButtonElement>("control-btn");
const setup = $("setup");
const setupError = $("setup-error");

const encoder = new TextEncoder();

let ws: WebSocket | null = null;
let isController = false;
let reconnectDelay = 500;
let reconnectTimer: number | undefined;
let intentionalClose = false;

const handle = createTerminal($("terminal"));

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

// ---------- connection ----------

function sendJson(obj: unknown): void {
  if (ws?.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify(obj));
  }
}

function sendInput(data: string): void {
  if (!isController) {
    // Hint that input (typing, scrolling) needs control.
    controlBtn.classList.remove("pulse");
    void controlBtn.offsetWidth; // restart the animation
    controlBtn.classList.add("pulse");
    return;
  }
  if (ws?.readyState === WebSocket.OPEN) {
    ws.send(encoder.encode(data));
  }
}

function setRole(controller: boolean): void {
  isController = controller;
  rolePill.hidden = false;
  rolePill.textContent = controller ? "controller" : "observer";
  rolePill.classList.toggle("controller", controller);
  controlBtn.hidden = false;
  controlBtn.textContent = controller ? "Release" : "Take control";
}

function connect(): void {
  const token = localStorage.getItem(TOKEN_KEY);
  if (!token) {
    showSetup();
    return;
  }
  setup.hidden = true;

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
    connDot.classList.remove("connected");
    setRole(false);
    if (!intentionalClose) {
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
    case "status":
      connDot.classList.add("connected");
      reconnectDelay = 500;
      sessionName.textContent = msg.session ?? "";
      setRole(msg.state === "controller");
      break;
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

// ---------- wire up ----------

handle.term.onData((data) => sendInput(applyCtrl(data)));
handle.onResize((cols, rows) => sendJson({ type: "resize", cols, rows }));

controlBtn.addEventListener("click", () => {
  sendJson({ type: isController ? "release_control" : "take_control" });
});

setupKeyRow(sendInput);
setupTouchScroll($("terminal"), handle.term, sendInput);

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
