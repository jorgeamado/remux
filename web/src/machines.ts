/// Multi-machine store. The PWA is served by ONE daemon (the "home" machine,
/// url === "") but may be paired with several; each machine keeps its own
/// device token and last session. Exactly one machine is active at a time —
/// the app holds a single live connection by design: an idle socket to a
/// background machine would register as "watching" and suppress that
/// machine's push notifications (and linger as a phantom tmux client).
///
/// Storage: localStorage for the app, mirrored to IndexedDB for the service
/// worker (which fans out /api/attention & /api/permissions queries across
/// machines after a payload-less push — it cannot read localStorage).

const MACHINES_KEY = "remux.machines";
const ACTIVE_KEY = "remux.active_machine";
// Pre-multi-machine keys, migrated on first load and kept write-through so a
// rollback to an older build (or a not-yet-updated service worker) still works.
const LEGACY_TOKEN_KEY = "remux.device_token";
const LEGACY_SESSION_KEY = "remux.session";

export interface Machine {
  /** Daemon's machine_id (/api/meta), or "home" until first fetched. */
  id: string;
  name: string;
  /** Base URL ("https://host:port"); "" = the origin that served this PWA. */
  url: string;
  token: string;
  /** Last session attached on this machine. */
  session?: string;
}

let machines: Machine[] = [];
let activeId = "";

function persist(): void {
  localStorage.setItem(MACHINES_KEY, JSON.stringify(machines));
  localStorage.setItem(ACTIVE_KEY, activeId);
  const home = homeMachine();
  // Write-through the legacy keys for the home machine.
  if (home?.token) {
    localStorage.setItem(LEGACY_TOKEN_KEY, home.token);
  } else {
    localStorage.removeItem(LEGACY_TOKEN_KEY);
  }
  if (home?.session) {
    localStorage.setItem(LEGACY_SESSION_KEY, home.session);
  } else {
    localStorage.removeItem(LEGACY_SESSION_KEY);
  }
  idbMirror();
}

/// Mirror what the service worker needs into IndexedDB: the machine list
/// (fan-out queries) and the legacy single token (stale SW builds).
function idbMirror(): void {
  try {
    const open = indexedDB.open("remux", 1);
    open.onupgradeneeded = () => open.result.createObjectStore("kv");
    open.onsuccess = () => {
      const tx = open.result.transaction("kv", "readwrite");
      const kv = tx.objectStore("kv");
      kv.put(
        machines.map((m) => ({ id: m.id, name: m.name, url: m.url, token: m.token })),
        "machines"
      );
      const home = homeMachine();
      if (home?.token) {
        kv.put(home.token, "device_token");
      } else {
        kv.delete("device_token");
      }
      tx.oncomplete = () => open.result.close();
    };
  } catch {
    /* private mode etc. — SW falls back to the generic notification */
  }
}

export function loadMachines(): void {
  try {
    machines = JSON.parse(localStorage.getItem(MACHINES_KEY) ?? "[]") as Machine[];
  } catch {
    machines = [];
  }
  if (!Array.isArray(machines)) machines = [];
  machines = machines.filter(
    (m) => m && typeof m.id === "string" && typeof m.url === "string"
  );
  // Migration: a device paired before multi-machine has only the legacy keys.
  if (machines.length === 0) {
    const token = localStorage.getItem(LEGACY_TOKEN_KEY);
    if (token) {
      machines = [
        {
          id: "home",
          name: location.host,
          url: "",
          token,
          session: localStorage.getItem(LEGACY_SESSION_KEY) ?? undefined,
        },
      ];
    }
  }
  activeId = localStorage.getItem(ACTIVE_KEY) ?? "";
  if (!machines.some((m) => m.id === activeId)) {
    activeId = homeMachine()?.id ?? machines[0]?.id ?? "";
  }
  persist(); // normalize + mirror (covers pre-mirror installs)
}

export function allMachines(): Machine[] {
  return machines;
}

export function homeMachine(): Machine | undefined {
  return machines.find((m) => m.url === "");
}

export function activeMachine(): Machine | undefined {
  return machines.find((m) => m.id === activeId);
}

export function setActiveMachine(id: string): void {
  if (machines.some((m) => m.id === id)) {
    activeId = id;
    persist();
  }
}

/// Add or update (same id, or same url) a machine. Returns the stored record.
/// The url match must include "" — re-pairing the home machine arrives with
/// the placeholder id "home" while the stored record already carries its real
/// machine_id, and matching only by id would duplicate it.
export function upsertMachine(m: Machine): Machine {
  const existing = machines.find((x) => x.id === m.id || x.url === m.url);
  if (existing) {
    // The id may change (placeholder ↔ real machine_id) — keep the active
    // pointer on this record through the rename.
    const wasActive = activeId === existing.id;
    existing.id = m.id;
    existing.name = m.name;
    existing.url = m.url;
    existing.token = m.token;
    if (wasActive) activeId = m.id;
    persist();
    return existing;
  }
  machines.push(m);
  // First machine ever (fresh install pairing its home daemon): make it
  // active — nothing else will, and connect() needs an active machine.
  if (!machines.some((x) => x.id === activeId)) {
    activeId = m.id;
  }
  persist();
  return m;
}

export function removeMachine(id: string): void {
  machines = machines.filter((m) => m.id !== id);
  if (activeId === id) activeId = homeMachine()?.id ?? machines[0]?.id ?? "";
  persist();
}

export function setMachineToken(m: Machine, token: string | null): void {
  if (token === null) {
    // A home machine with a dead token returns to the pairing screen; a
    // foreign machine without a token is useless — forget it entirely.
    if (m.url === "") {
      m.token = "";
      persist();
    } else {
      removeMachine(m.id);
    }
  } else {
    m.token = token;
    persist();
  }
}

export function setMachineSession(m: Machine, session: string | undefined): void {
  m.session = session;
  persist();
}

/// Upgrade a record to the daemon's persistent identity. If that identity is
/// already known under ANOTHER record (the user added the same daemon by an
/// alternate URL, or re-paired), the two merge into one — and the home record
/// (url === "") always survives the merge: deleting it would orphan push and
/// the legacy-key write-through. Returns the surviving record.
export function setMachineIdentity(m: Machine, id: string, name: string): Machine {
  const dup = machines.find((x) => x !== m && x.id === id) ?? null;
  const keep = dup && m.url !== "" ? dup : m;
  const drop = dup ? (keep === m ? dup : m) : null;
  const activeInvolved = activeId === m.id || (dup !== null && activeId === dup.id);
  if (drop) {
    keep.token = m.token; // the just-completed pairing minted the fresh token
    machines = machines.filter((x) => x !== drop);
  }
  keep.id = id;
  keep.name = name;
  if (activeInvolved) activeId = id;
  persist();
  return keep;
}

/// Absolute API URL on a machine ("" base = same-origin relative path).
export function apiUrl(m: Machine, path: string): string {
  return m.url ? `${m.url}${path}` : path;
}

export function wsUrl(m: Machine): string {
  if (m.url) {
    return `${m.url.replace(/^http/, "ws")}/ws`;
  }
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws`;
}

/// Normalize user input into a machine base URL. TLS-only for foreign
/// machines (matches the daemon CSP; pairing over plaintext leaks the token),
/// except localhost for development.
export function normalizeMachineUrl(raw: string): string | null {
  let s = raw.trim().replace(/\/+$/, "");
  if (!s) return null;
  if (!/^https?:\/\//i.test(s)) s = `https://${s}`;
  let u: URL;
  try {
    u = new URL(s);
  } catch {
    return null;
  }
  if (u.pathname !== "/" && u.pathname !== "") return null;
  if (u.username || u.password || u.search || u.hash) return null;
  const local = u.hostname === "localhost" || u.hostname === "127.0.0.1";
  if (u.protocol !== "https:" && !(u.protocol === "http:" && local)) return null;
  return u.origin;
}
