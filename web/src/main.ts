import "./style.css";
import { createTerminal } from "./term";
import { setupKeyRow, applyCtrl, disarmCtrl } from "./keys";
import { setupTouchScroll } from "./scroll";
import {
  Machine,
  activeMachine,
  allMachines,
  apiUrl,
  homeMachine,
  loadMachines,
  normalizeMachineUrl,
  setActiveMachine,
  setMachineIdentity,
  setMachineSession,
  setMachineToken,
  upsertMachine,
  wsUrl,
} from "./machines";

// Machine records (per-machine device token + last session), migrated from
// the pre-multi-machine single-token keys and mirrored to IndexedDB for the
// service worker. Must run before anything touches tokens.
loadMachines();
const FONT_KEY = "remux.font";
const NOTIFY_KEY = "remux.notify";
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

function deviceName(): string {
  return navigator.userAgent.includes("iPhone")
    ? "iPhone"
    : navigator.userAgent.includes("Android")
      ? "Android"
      : "browser";
}

/// Pair with a daemon. `baseUrl` "" = the machine that served this PWA
/// (the "home" machine); anything else is a foreign machine being added.
async function pairMachine(baseUrl: string, token: string): Promise<Machine> {
  const resp = await fetch(`${baseUrl}/api/pair`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ token, device_name: deviceName() }),
  });
  if (!resp.ok) {
    throw new Error(await resp.text());
  }
  const body = (await resp.json()) as { device_token: string };
  const fallbackName = baseUrl ? new URL(baseUrl).host : location.host;
  const machine = upsertMachine({
    id: baseUrl || "home",
    name: fallbackName,
    url: baseUrl,
    token: body.device_token,
  });
  // May merge into an existing record for the same daemon — hand the caller
  // the survivor, not a record that no longer exists.
  return refreshMachineMeta(machine);
}

/// Upgrade a record's placeholder identity to the daemon's persistent
/// machine_id + display name; may merge with an existing record for the same
/// daemon (returns the survivor). Best effort: a pre-/api/meta daemon keeps
/// the placeholder (URL-keyed) identity and everything still works.
async function refreshMachineMeta(m: Machine): Promise<Machine> {
  try {
    const resp = await fetch(apiUrl(m, "/api/meta"), {
      headers: { authorization: `Bearer ${m.token}` },
    });
    if (!resp.ok) return m;
    const meta = (await resp.json()) as { machine_id?: string; name?: string };
    if (meta.machine_id) {
      return setMachineIdentity(m, meta.machine_id, meta.name || m.name);
    }
  } catch {
    /* offline / older daemon */
  }
  return m;
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
  const machine = activeMachine();
  if (!machine?.token) {
    showSetup();
    return;
  }
  clearTimeout(reconnectTimer); // a pending retry must not race this connect
  setup.hidden = true;
  setStatus("connecting…", "connecting");

  const sock = new WebSocket(wsUrl(machine));
  sock.binaryType = "arraybuffer";
  ws = sock;

  sock.onopen = () => {
    const { cols, rows } = handle.size();
    sendJson({
      type: "auth",
      token: machine.token,
      cols,
      rows,
      session: machine.session || undefined,
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
        onMachineAuthLost("This device is no longer paired. Pair it again.");
      } else if (msg.code === "revoked") {
        onMachineAuthLost(
          "This device was revoked. Pair it again if that was a mistake."
        );
      } else if (msg.code === "invalid_session") {
        // Fall back to the server default; onclose will reconnect.
        const m = activeMachine();
        if (m) setMachineSession(m, undefined);
        showHint("Session unavailable — using default");
      }
      break;
  }
}

/// The active machine rejected our token (unpaired/revoked). Home machine:
/// clear the token and return to the pairing screen. Foreign machine: forget
/// it and land back on home — its PWA can only re-add it by pairing anyway.
function onMachineAuthLost(message: string): void {
  const m = activeMachine();
  if (!m || m.url === "") {
    intentionalClose = true;
    ws?.close();
    if (m) setMachineToken(m, null);
    showSetup(message);
    return;
  }
  const name = m.name;
  setMachineToken(m, null); // removes the foreign machine, active falls home
  // No intentionalClose here: connect() supersedes this socket, so its close
  // event is ignored — a set flag would leak onto the NEW socket and swallow
  // its first real disconnect (the bug the 2026-07-12 review found).
  ws?.close();
  resetMachineScopedState();
  connect();
  showHint(`${name}: ${message}`);
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
  // Machines: sessions above are the ACTIVE machine's; other paired daemons
  // are one tap away. Single live connection by design — switching machines
  // closes this socket (a lingering one would suppress that machine's push
  // and linger as a phantom tmux client).
  if (allMachines().length > 1 || activeMachine()?.url !== "") {
    const label = document.createElement("div");
    label.className = "menu-label";
    label.textContent = "Machines";
    sessionMenu.appendChild(label);
    for (const m of allMachines()) {
      const marker = m.id === activeMachine()?.id ? "● " : "";
      sessionMenu.appendChild(menuItem(`${marker}${m.name}`, () => switchMachine(m)));
    }
  }
  sessionMenu.appendChild(menuItem("Add machine…", () => void addMachineFlow()));
  if (typeof navigator.mediaDevices?.getUserMedia === "function") {
    sessionMenu.appendChild(menuItem("Scan QR to add…", () => void scanMachineFlow()));
  }
  sessionMenu.hidden = false;
}

function switchSession(name: string): void {
  sessionMenu.hidden = true;
  if (name === sessionTitle) return;
  const m = activeMachine();
  if (m) setMachineSession(m, name);
  // connect() supersedes the old socket; its close event is then ignored,
  // so no intentionalClose flag is needed (which could leak and suppress
  // the reconnect after an invalid_session rejection).
  ws?.close();
  // The feed is per-session; clear it now. The old socket's close is ignored
  // (superseded), so onclose won't, and the new session may have an empty feed
  // the daemon stays silent about — leaving stale cards without this.
  clearFeed();
  handle.term.reset(); // fresh grid; the new attach repaints everything
  connect();
}

// ---------- machines (multi-host) ----------

/// Close the connection to the current machine and attach to another. The old
/// socket MUST die (not idle in the background): a live socket registers this
/// device as "watching" on that daemon, which suppresses its Web Push — and
/// its PTY is a real tmux client on that machine.
function switchMachine(m: Machine): void {
  sessionMenu.hidden = true;
  if (m.id === activeMachine()?.id) return;
  setActiveMachine(m.id);
  ws?.close(); // superseded by the connect() below; close event ignored
  resetMachineScopedState();
  setStatus(`connecting to ${m.name}…`, "connecting");
  connect();
}

/// Everything that belongs to the machine we are leaving. A composer draft or
/// buffered keystrokes typed for machine A must never be sendable to machine
/// B — a half-typed command can carry secrets and is wrong on B anyway.
function resetMachineScopedState(): void {
  clearFeed();
  clearPermissionCards();
  topology = []; // the new machine streams its own
  sessionTitle = "";
  composerInput.value = "";
  shellLinePending = false;
  endRecall(); // an in-progress recall indexes the OLD machine's snapshot
  pendingInput = "";
  controlRequested = false;
  handle.term.reset();
}

/// Split pairing input into address + optional token. A full pairing link
/// ("https://host:port/#pair=<64hex>") answers both at once; a bare URL
/// leaves the token null; a bare token with no address is not enough.
/// Returns an error hint string when the input can't name a machine.
function parsePairingInput(
  raw: string
): { baseUrl: string; token: string | null } | string {
  const token = extractPairToken(raw);
  const withoutToken = raw.replace(/(?:#pair=)?[0-9a-f]{64}\s*$/i, "").trim();
  if (token && !withoutToken) {
    return "Paste the full pairing link — it includes the machine's address";
  }
  const baseUrl = normalizeMachineUrl(withoutToken.replace(/#$/, ""));
  if (!baseUrl) return "Machine URLs must be https://host[:port]";
  if (baseUrl === location.origin) return "That's this machine";
  return { baseUrl, token };
}

async function completePairing(baseUrl: string, token: string): Promise<void> {
  try {
    const machine = await pairMachine(baseUrl, token);
    showHint(`Paired with ${machine.name}`);
    switchMachine(machine);
  } catch (e) {
    // A CORS/network failure surfaces as a bare TypeError — the usual cause
    // is the foreign daemon not allowlisting this PWA's origin.
    if (e instanceof TypeError) {
      showHint(
        `Can't reach it — run its daemon with --allowed-client-origin ${location.origin}`
      );
    } else {
      showHint(`Pairing failed: ${e instanceof Error ? e.message : e}`);
    }
  }
}

/// In-app "Add machine": paste the other machine's pairing link and it pairs
/// in one step (the link carries both the URL and the token). A bare URL
/// still works — the token is asked for separately.
async function addMachineFlow(): Promise<void> {
  sessionMenu.hidden = true;
  const rawUrl = window.prompt(
    "Pairing link from that machine (or just its URL):",
    "https://"
  );
  if (rawUrl === null) return;
  const parsed = parsePairingInput(rawUrl);
  if (typeof parsed === "string") {
    showHint(parsed);
    return;
  }
  let token = parsed.token;
  if (!token) {
    const link = window.prompt("Pairing link or token (run `remux pair` there):");
    if (link === null) return;
    token = extractPairToken(link);
  }
  if (!token) {
    showHint("That doesn't look like a pairing link or token");
    return;
  }
  await completePairing(parsed.baseUrl, token);
}

/// In-app QR scan for "Add machine". The daemon's pairing QR encodes the full
/// pairing link; scanning it with the OS camera would open that machine's own
/// PWA in the browser, outside this installed app — so we scan it HERE, with
/// our own camera view, and feed the decoded link into the same pairing path
/// as paste.
async function scanMachineFlow(): Promise<void> {
  sessionMenu.hidden = true;
  const text = await scanQrCode();
  if (text === null) return; // cancelled or camera unavailable (already hinted)
  const parsed = parsePairingInput(text);
  if (typeof parsed === "string") {
    showHint(parsed);
    return;
  }
  if (!parsed.token) {
    showHint("That QR has no pairing token — run `remux pair` on that machine");
    return;
  }
  await completePairing(parsed.baseUrl, parsed.token);
}

/// Minimal surface of the Shape Detection API (Chrome/Android). iOS Safari
/// doesn't have it — there we lazy-load the pure-JS jsQR decoder instead.
interface QrDetector {
  detect(source: CanvasImageSource): Promise<Array<{ rawValue: string }>>;
}

/// Full-screen camera view; resolves with the first decoded QR payload, or
/// null on cancel / no camera. Frames are sampled a few times a second — a
/// tight loop would peg the phone's CPU for no faster lock-on.
async function scanQrCode(): Promise<string | null> {
  if (typeof navigator.mediaDevices?.getUserMedia !== "function") {
    showHint("No camera here — paste the pairing link instead");
    return null;
  }
  let stream: MediaStream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({
      video: { facingMode: "environment" },
    });
  } catch {
    showHint("Camera unavailable — paste the pairing link instead");
    return null;
  }
  const detectorCtor = (
    window as unknown as { BarcodeDetector?: new (opts: { formats: string[] }) => QrDetector }
  ).BarcodeDetector;
  const detector = detectorCtor ? new detectorCtor({ formats: ["qr_code"] }) : null;
  return await new Promise<string | null>((resolve) => {
    const overlay = document.createElement("div");
    overlay.id = "qr-scan";
    const video = document.createElement("video");
    video.playsInline = true; // iOS: without this, playback goes fullscreen
    video.muted = true;
    video.srcObject = stream;
    const label = document.createElement("p");
    label.textContent = "Point at the pairing QR";
    const cancel = document.createElement("button");
    cancel.className = "btn";
    cancel.textContent = "Cancel";
    overlay.append(video, label, cancel);
    document.body.append(overlay);
    let done = false;
    const finish = (text: string | null): void => {
      if (done) return;
      done = true;
      stream.getTracks().forEach((t) => t.stop());
      overlay.remove();
      resolve(text);
    };
    cancel.addEventListener("click", () => finish(null));
    void video.play();
    let jsQr: typeof import("jsqr").default | null = null;
    if (!detector) {
      import("jsqr").then(
        (m) => (jsQr = m.default),
        () => {
          showHint("QR decoder failed to load — paste the link instead");
          finish(null);
        }
      );
    }
    const canvas = document.createElement("canvas");
    const ctx = canvas.getContext("2d", { willReadFrequently: true });
    const tick = async (): Promise<void> => {
      if (done) return;
      if (video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA) {
        try {
          if (detector) {
            const codes = await detector.detect(video);
            const hit = codes.find((c) => c.rawValue);
            if (hit) return finish(hit.rawValue);
          } else if (jsQr && ctx) {
            canvas.width = video.videoWidth;
            canvas.height = video.videoHeight;
            ctx.drawImage(video, 0, 0);
            const img = ctx.getImageData(0, 0, canvas.width, canvas.height);
            const code = jsQr(img.data, img.width, img.height);
            if (code?.data) return finish(code.data);
          }
        } catch {
          /* transient decode error — keep scanning */
        }
      }
      window.setTimeout(() => void tick(), 200);
    };
    void tick();
  });
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
    const resp = await fetch(activeApi(`/api/permissions/${encodeURIComponent(card.id)}/decide`), {
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

/// Auth for the ACTIVE machine's API.
function authHeader(): Record<string, string> {
  return { authorization: `Bearer ${activeMachine()?.token ?? ""}` };
}

/// API path on the active machine (absolute for a foreign machine).
function activeApi(path: string): string {
  const m = activeMachine();
  return m ? apiUrl(m, path) : path;
}

/// Web Push is HOME-machine only: the service worker's single push
/// subscription is bound to the home daemon's VAPID key, so other daemons
/// can't use it. Their notifications arrive in-band while active; true push
/// from every machine is the roadmap's push-coordinator step.
function homeAuthHeader(): Record<string, string> | null {
  const home = homeMachine();
  return home?.token ? { authorization: `Bearer ${home.token}` } : null;
}

async function subscribePush(): Promise<void> {
  if (!("serviceWorker" in navigator)) return;
  const auth = homeAuthHeader();
  if (!auth) {
    // Never fail silently here — the user just toggled notifications on.
    showHint("Push setup failed — pair this app's own machine first");
    return;
  }
  const reg = await navigator.serviceWorker.getRegistration();
  if (!reg?.pushManager) {
    showHint("Lock-screen alerts need the installed app; in-app alerts active");
    return;
  }
  try {
    const resp = await fetch("/api/push/key", { headers: auth });
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
      headers: { ...auth, "content-type": "application/json" },
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
    const auth = homeAuthHeader();
    if (auth) {
      await fetch("/api/push/unsubscribe", {
        method: "POST",
        headers: { ...auth, "content-type": "application/json" },
        body: JSON.stringify({ endpoint: sub.endpoint }),
      });
    }
    await sub.unsubscribe();
  } catch {
    /* best effort */
  }
}

/// After a notification tap (or any return to the app), land on the session
/// that actually wants attention — the push payload deliberately can't say.
/// Fans out across every paired machine (there is no live socket to the
/// inactive ones — this poll is their only in-app signal).
async function checkPendingAttention(): Promise<void> {
  const active = activeMachine();
  if (!active?.token) return;
  const results = await Promise.all(
    allMachines().map(async (m) => {
      try {
        // Bounded: an unreachable machine must not stall the deep-link past
        // the moment the user is still looking at the hint.
        const resp = await fetch(apiUrl(m, "/api/attention"), {
          headers: { authorization: `Bearer ${m.token}` },
          signal: AbortSignal.timeout(5000),
        });
        if (!resp.ok) return null;
        const { sessions } = (await resp.json()) as { sessions: string[] };
        return { m, sessions };
      } catch {
        return null; // that machine is offline — not this one's problem
      }
    })
  );
  // The active machine's current session already shows its own state; prefer
  // its other sessions, then other machines.
  const here = results.find((r) => r?.m.id === active.id);
  if (here && here.sessions.includes(sessionTitle)) return;
  const localOther = here?.sessions.find((s) => s !== sessionTitle);
  if (localOther) {
    showHint(`Attention in ${localOther} — tap to open`, () =>
      switchSession(localOther)
    );
    return;
  }
  for (const r of results) {
    if (!r || r.m.id === active.id || r.sessions.length === 0) continue;
    const { m, sessions } = r;
    showHint(`Attention on ${m.name}: ${sessions[0]} — tap to open`, () => {
      setMachineSession(m, sessions[0]);
      switchMachine(m);
    });
    return;
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

// Per-machine, per-session typed-command history, MEMORY ONLY (see above).
// Keyed by machine id + session name: "main" on machine A and "main" on
// machine B are different shells — recall must never offer A's commands
// (possibly secrets) for sending to B.
const typedHistoryMem = new Map<string, string[]>();

function typedHistoryKey(): string {
  return `${activeMachine()?.id ?? ""}:${sessionTitle}`;
}

function typedHistory(): string[] {
  if (!sessionTitle) return [];
  return typedHistoryMem.get(typedHistoryKey()) ?? [];
}

/// Record a *typed* command for this session, in memory only. Skips
/// feed-derived text (already recallable via the feed) and the no-session case.
function recordTyped(cmd: string): void {
  if (!sessionTitle || composerFromFeed) return;
  const h = typedHistory();
  if (h[h.length - 1] === cmd) return;
  typedHistoryMem.set(typedHistoryKey(), [...h, cmd].slice(-HISTORY_MAX));
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
  const m = activeMachine();
  debugOverlay.textContent = [
    `remux debug · ${standaloneMode()} · ${role}`,
    `machine ${m?.name ?? "?"} ${m?.url || "(home)"}`,
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
    const resp = await fetch(activeApi("/api/devices"), { headers: authHeader() });
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
    await pairMachine("", token);
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
      await pairMachine("", hashToken);
      offerInstallTip(pairUrl);
    } catch (e) {
      showSetup(`Pairing failed: ${e instanceof Error ? e.message : e}`);
      return;
    }
  }
  requestWakeLock();
  connect();
  // Devices paired before /api/meta existed carry the placeholder identity;
  // upgrade to the daemon's persistent machine_id + name when reachable.
  const home = homeMachine();
  if (home?.token && home.id === "home") void refreshMachineMeta(home);
  // A notification tap may cold-start the app (no visibilitychange fires):
  // land on the session that wants attention.
  void checkPendingAttention();
})();
