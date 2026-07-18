// Minimal service worker: enough for PWA installability. The app is useless
// offline (it is a live terminal), so we only cache the shell as a fallback
// and always prefer the network.
const CACHE = "remux-shell-v2";

self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(CACHE).then((c) => c.addAll(["/"])));
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
  );
  self.clients.claim();
});

// Payload-less by design: the daemon never sends terminal content or session
// names through the push service. Detail is fetched from the daemon itself,
// post-auth, after the push wakes us. iOS requires every push to surface a
// notification — every path below ends in showNotification, always.

function idbGet(key) {
  return new Promise((resolve) => {
    try {
      const open = indexedDB.open("remux", 1);
      open.onupgradeneeded = () => open.result.createObjectStore("kv");
      open.onerror = () => resolve(null);
      open.onsuccess = () => {
        const get = open.result.transaction("kv").objectStore("kv").get(key);
        get.onerror = () => resolve(null);
        get.onsuccess = () => {
          open.result.close();
          resolve(get.result === undefined ? null : get.result);
        };
      };
    } catch {
      resolve(null);
    }
  });
}

// All paired machines: the app mirrors [{id, name, url, token}] ("" url =
// this origin). Only the ACTIVE machine has a live socket, so a push about
// any other machine can only be resolved by asking that machine directly —
// fan out. Legacy fallback: the single home token from pre-multi-machine
// app builds.
async function machineList() {
  const machines = await idbGet("machines");
  if (Array.isArray(machines) && machines.length > 0) {
    return machines
      .filter((m) => m && typeof m.url === "string" && m.token)
      .sort((a, b) => (a.url ? 1 : 0) - (b.url ? 1 : 0)); // home first
  }
  const token = await idbGet("device_token");
  return typeof token === "string" ? [{ name: "", url: "", token }] : [];
}

async function authedJson(machine, path) {
  // Per-request bound well under the push handler's 8s deadline: one
  // sleeping machine must not cost the answers the others already have.
  const resp = await fetch(`${machine.url}${path}`, {
    headers: { authorization: `Bearer ${machine.token}` },
    signal: AbortSignal.timeout(4000),
  });
  // 401 (revoked/re-paired elsewhere): the app repairs the store on next
  // open; the SW must not mutate records it doesn't own.
  return resp.ok ? resp.json() : null;
}

// A pending permission request outranks a busy→quiet attention: it's blocking
// an agent. Lock-screen text is built ONLY from daemon/user-controlled values:
// a fixed template plus the session name (the user's own tmux label). Producer/
// hook-supplied strings (source, reason/message) are deliberately NOT shown —
// a hook could put a secret in them, and the lock screen is visible unlocked.
// Fuller detail (agent, tool) is shown in-app after auth.
// Machine label prefix: only useful once several machines exist, and only
// for machines other than the one that served this SW.
function withMachine(machine, many, text) {
  return many && machine.url ? `${machine.name}: ${text}` : text;
}

async function permissionBody(machine, many) {
  const body = await authedJson(machine, "/api/permissions");
  const cards = body && body.cards;
  if (!Array.isArray(cards) || cards.length === 0) return null;
  const c = cards[0];
  const more = cards.length > 1 ? ` (+${cards.length - 1} more)` : "";
  return withMachine(machine, many, `An agent needs permission in ${c.session}${more}`).slice(
    0,
    180
  );
}

async function attentionBody(machine, many) {
  const body = await authedJson(machine, "/api/attention");
  const details = body && body.details;
  if (!Array.isArray(details) || details.length === 0) return null;
  const d = details[0]; // freshest first, per the API
  // Session name only — never d.source / d.reason (producer-supplied).
  const more = details.length > 1 ? ` (+${details.length - 1} more)` : "";
  return withMachine(machine, many, `${d.session} — needs your attention${more}`).slice(0, 180);
}

async function notificationBody() {
  const machines = await machineList();
  if (machines.length === 0) return null;
  const many = machines.length > 1;
  // All machines × both endpoints, concurrent: a slow or offline machine must
  // not eat the 8s budget the others need on a cold tailnet wake. A pending
  // permission (blocking an agent) outranks attention on any machine; the
  // home machine sorts first within each rank (it sent the push today).
  const [perms, atts] = await Promise.all([
    Promise.all(machines.map((m) => permissionBody(m, many).catch(() => null))),
    Promise.all(machines.map((m) => attentionBody(m, many).catch(() => null))),
  ]);
  return perms.find(Boolean) || atts.find(Boolean) || null;
}

self.addEventListener("push", (event) => {
  // A stalled fetch/IndexedDB must never starve showNotification — iOS
  // revokes push permission for pushes that surface nothing. Hard deadline,
  // generic text on any failure or timeout. 8s: a locked phone's tailnet
  // needs a few seconds to wake before /api/attention is reachable.
  const deadline = new Promise((resolve) => setTimeout(() => resolve(null), 8000));
  event.waitUntil(
    Promise.race([notificationBody().catch(() => null), deadline]).then((body) =>
      self.registration.showNotification("remux", {
        body: body || "A session may need your attention",
        tag: "remux-attention",
        icon: "/icon-512.png",
      })
    )
  );
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  event.waitUntil(
    self.clients
      .matchAll({ type: "window", includeUncontrolled: true })
      .then((list) => {
        for (const client of list) {
          if ("focus" in client) return client.focus();
        }
        return self.clients.openWindow("/");
      })
  );
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (event.request.method !== "GET" || url.pathname.startsWith("/api")) return;
  // The app shell (a navigation to "/") must come from the network bypassing the
  // browser's HTTP cache, so a new deploy is picked up immediately. Hashed
  // assets are immutable, so a normal (cache-respecting) fetch is fine for them.
  const req =
    event.request.mode === "navigate"
      ? new Request(event.request, { cache: "reload" })
      : event.request;
  event.respondWith(
    fetch(req)
      .then((resp) => {
        const copy = resp.clone();
        caches.open(CACHE).then((c) => c.put(event.request, copy)).catch(() => {});
        return resp;
      })
      .catch(() => caches.match(event.request).then((m) => m || caches.match("/")))
  );
});
