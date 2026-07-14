// Minimal service worker: enough for PWA installability. The app is useless
// offline (it is a live terminal), so we only cache the shell as a fallback
// and always prefer the network.
const CACHE = "remux-shell-v1";

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

function idbToken() {
  return new Promise((resolve) => {
    try {
      const open = indexedDB.open("remux", 1);
      open.onupgradeneeded = () => open.result.createObjectStore("kv");
      open.onerror = () => resolve(null);
      open.onsuccess = () => {
        const get = open.result.transaction("kv").objectStore("kv").get("device_token");
        get.onerror = () => resolve(null);
        get.onsuccess = () => {
          open.result.close();
          resolve(typeof get.result === "string" ? get.result : null);
        };
      };
    } catch {
      resolve(null);
    }
  });
}

function idbTokenClear() {
  try {
    const open = indexedDB.open("remux", 1);
    open.onsuccess = () => {
      open.result.transaction("kv", "readwrite").objectStore("kv").delete("device_token");
    };
  } catch {
    /* best effort */
  }
}

async function attentionBody() {
  const token = await idbToken();
  if (!token) return null;
  const resp = await fetch("/api/attention", {
    headers: { authorization: `Bearer ${token}` },
  });
  if (resp.status === 401) {
    idbTokenClear(); // revoked/re-paired elsewhere: stop sending it
    return null;
  }
  if (!resp.ok) return null;
  const { details } = await resp.json();
  if (!Array.isArray(details) || details.length === 0) return null;
  const d = details[0]; // freshest first, per the API
  const what = d.source
    ? `${d.source}: ${d.reason || "needs input"}`
    : "may need your attention";
  const more = details.length > 1 ? ` (+${details.length - 1} more)` : "";
  return `${d.session} — ${what}${more}`.slice(0, 180);
}

self.addEventListener("push", (event) => {
  // A stalled fetch/IndexedDB must never starve showNotification — iOS
  // revokes push permission for pushes that surface nothing. Hard deadline,
  // generic text on any failure or timeout. 8s: a locked phone's tailnet
  // needs a few seconds to wake before /api/attention is reachable.
  const deadline = new Promise((resolve) => setTimeout(() => resolve(null), 8000));
  event.waitUntil(
    Promise.race([attentionBody().catch(() => null), deadline]).then((body) =>
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
  event.respondWith(
    fetch(event.request)
      .then((resp) => {
        const copy = resp.clone();
        caches.open(CACHE).then((c) => c.put(event.request, copy)).catch(() => {});
        return resp;
      })
      .catch(() => caches.match(event.request).then((m) => m || caches.match("/")))
  );
});
