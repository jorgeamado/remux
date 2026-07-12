# remux — development plan (v2, 2026-07-12)

> Progress: **M0 done** — CI green from the first run; v0.1.0 draft release
> built natively for 4 platforms (tarballs, .debs, SHA256SUMS, filled-in
> brew formula, attestations); clean-Debian `apt install` verified (tmux
> dependency, service units, serve smoke); onboarding fixes + `remux pair`
> + cert renewal shipped; repo public. Remaining M0 user actions: publish
> the draft release, create the `homebrew-remux` tap with the generated
> `remux.rb`. **M1 code complete** (push stack + dispatcher + deep-link,
> reviewed) — awaiting the real-device spike on an iPhone. **M1.5 done**
> (observer Fit toggle; window dims ride the status frame). **M2 done**
> (revocation cascade incl. persist-rollback and lagged-broadcast fail-safe,
> `remux devices` CLI over the admin socket, read-only PWA Devices sheet).
> Next: M3a, starting with the control-mode compatibility spike. Shakedown
> findings fixed along the way: attestations are public-repo-only, the
> macos-13 runner pool is starved (cross-target from macos-14), and a
> second rustls crypto provider panicked the TLS listener (now a smoke-
> tested class).

Where we are: V1 + V1.x are built and deployed (daemon + PWA, pairing/TLS,
observer/controller handoff, session picker, windows/panes menu, attention
notifications, observer scrollback, release pipeline). Guiding principle
unchanged: **tmux owns terminal state, the daemon owns product state**;
every milestone leaves the tool usable and shippable.

> Reviewed 2026-07-12 by three perspective reviews (security, engineering
> feasibility, product) plus Codex. This v2 incorporates their findings;
> the notable ones are marked ⚑ inline.

## M0 — release + onboarding shakedown (small-medium)

Goal: the release machinery works end to end, and a stranger with a tailnet
can install and pair in under 10 minutes — including on iOS.

- Push the branch; the workflows have never run — the first CI/release runs
  are their real test. Fix what they reveal (arm runners, Playwright on CI).
- Tag `v0.1.0`; create the `homebrew-remux` tap (branch protection + 2FA —
  the formula executes on users' machines); verify `brew install` on a clean
  Mac and `apt install ./remux_*.deb` + systemd unit on a clean Debian VM.
- Signing posture (decided): GitHub artifact attestations (sigstore, no
  secrets) + SHA256SUMS. README documents the `gh attestation verify`
  one-liner; SHA256SUMS is a corruption check, not a trust anchor. No Apple
  notarization (brew avoids Gatekeeper); GPG arrives with a hosted apt repo.
- ⚑ **iOS pairing flow is currently broken for installed PWAs**: Safari and
  the Home-Screen app have partitioned storage, so "pair in Safari → Add to
  Home Screen" opens an unpaired app with the single-use token already
  burned. Fix: pairing tokens become reusable within their TTL, and the pair
  screen detects non-standalone iOS and says "install first, then open the
  pairing link inside the app".
- ⚑ `remux pair` CLI ships now, not in M2: under systemd/brew-services the
  startup QR is buried in logs, and pairing a second device must not require
  a daemon restart. Minimal admin channel: Unix domain socket in the state
  dir (0600), one endpoint (mint pairing token); CLI prints URL + QR.
- ⚑ Cert renewal is load-bearing (TLS ⇒ PWA install ⇒ push origin): ship the
  systemd/launchd renewal timer + docs in M0 and verify the installed PWA
  and its origin survive a renewal. Startup gains doctor-style checks
  (cert/key readable, name matches, non-public bind) with actionable errors.

Acceptance: clean-machine installs via brew and deb; pair from the installed
iOS PWA; `remux pair` mints a token while the daemon runs as a service.

## M1 — Web Push: attention on a locked iPhone (medium-large)

The attention heuristic already fires; the phone can't hear it while locked.
Daily use hits this immediately.

- ⚑ **Day-1 gate: real-device spike.** Installed iOS PWA, locked phone,
  payload-less push (VAPID, TTL/urgency headers), tap behavior. If Apple
  mishandles empty payloads, switch to RFC 8291 `aes128gcm` with a *generic*
  payload — either way, no terminal content or session names ever transit
  Apple/Google.
- ⚑ Device-lifecycle groundwork lands here (one schema migration, not two):
  device *id* plumbed through WS auth (today only the name is), `last_seen`,
  per-device push subscriptions, and the revoke cascade primitives (delete
  subscriptions, close sockets by device id).
- Subscribe API: `POST /api/push/subscribe`/`unsubscribe` (Bearer). ⚑ The
  endpoint URL is attacker-supplied — the daemon gains outbound HTTP where
  it had none: require https, resolve and reject loopback/private/CGNAT
  (tailnet!) targets or allowlist known push origins; cap subscriptions per
  device (3); prune on 404/410.
- Delivery: a dedicated dispatcher task consumes the attention broadcast;
  ⚑ skips devices with a live socket on that session (they get the in-band
  frame; no double notification); throttles per (subscription, session)
  ≥60s. ⚑ The service worker calls `showNotification` unconditionally on
  every push — iOS revokes push permission otherwise; all suppression logic
  lives in the daemon.
- ⚑ Deep link without payload data: `GET /api/attention` (Bearer) returns
  sessions with pending attention; on notification tap / app open the client
  queries it and switches (picker if several).
- ⚑ Attention quality is in-scope, not "later": suppress pushes while any
  tmux client is recently active (`#{client_activity}`) — no lock-screen
  spam while the owner is sitting at the Mac.
- VAPID: ES256 keypair, generated once, 0600. Rotation story: delete key +
  restart drops all subscriptions; clients detect the changed
  `applicationServerKey` and transparently re-subscribe.
- Reliability: daemon restart preserves keys/subscriptions; stale
  subscriptions pruned; the device token must be reachable from the service
  worker (IndexedDB, not localStorage) for the attention query.

Acceptance: phone locked, Claude Code asks on the Mac session → lock-screen
notification ≤30s; tapping opens the PWA on that session; no pushes while
actively typing at the Mac; revoking a device stops its pushes.

## M1.5 — observer fit-width, cheap version (small)

⚑ The core "glance at Claude Code" readability fix must not wait for the
control-mode milestone. The daemon includes the current window's cols×rows
in the `status` frame (cheap query; refreshed on reconnect and control
changes); the client offers **Fit width** for observers — pure font-size
math, clamped at 6px, never a tmux resize. M3b later upgrades the data
source to live topology.

## M2 — device management (medium)

Security hygiene before more surface is added.

- Admin API grows on the M0 Unix socket (0600, filesystem-authenticated;
  never on the network listener): list devices (id, name, created,
  last-seen), revoke, rename. CLI: `remux devices list|revoke|rename`.
- ⚑ Privilege decision recorded: device tokens stay flat, so **management
  is host-CLI only** in this milestone. The PWA gets a *read-only* Devices
  sheet. Invite-from-device (and any device revoking others) waits for
  per-device capabilities — a stolen phone must not be able to enroll
  attacker devices or lock the owner out.
- ⚑ Revocation is a cascade, specified: token invalid + live sockets closed
  (per-device connection registry from M1) + push subscriptions deleted +
  any unexpired pairing tokens cancelled.
- Reliability: devices.json migrations are versioned; daemon restart loses
  no device state.

Acceptance: `remux devices revoke <id>` → that phone's socket drops within
a second and its pushes stop; the PWA sheet shows live last-seen times.

## M3a — control-mode metadata client (large)

The V2 foundation: topology (sessions/windows/panes, names, active flags)
streamed to clients. **Additive and metadata-only** — the per-connection PTY
attach remains the byte path, and losing the control client only degrades
(tabs disappear, terminal unaffected).

- ⚑ Design subtask first, then code: one control client per tmux server,
  attached `read-only,no-output,`**`ignore-size`** (without `ignore-size` a
  default-sized control client would fight the `window-size latest`
  Mac↔phone handoff — the exact bug class V1 was built to avoid).
- ⚑ Notifications are **dirty-bits only**: on any `%…` event, re-run the
  existing `list-sessions`/`list-windows -a` parsers for a fresh snapshot.
  No incremental state from notification parsing (windows in non-attached
  sessions don't get full events; the `%begin/%end` minefield stays closed).
- Supervision: respawn with backoff; tmux server restart (last session
  killed) recovers; a dead control client never affects the byte path.
- ⚑ Compatibility spike up front: tmux 3.2→3.5, which events arrive under
  these flags, verified on the versions we claim to support.
- ⚑ Topology strings (window/pane titles) are terminal-controlled input:
  textContent-only rendering, length caps, control chars stripped; frames
  post-auth only. Recorded decision: every paired device sees every
  session's topology, consistent with the V1 "any device, any session"
  access model.

Acceptance: window created on the Mac → `topology` frame ≤1s; kill the
control client → terminal stream unaffected; tmux server restart → topology
recovers without daemon restart.

## M3b — topology UI (medium)

Persistent window tabs (replacing the + menu's window list), live header
breadcrumb `session / window / pane-command`, session picker fed by topology
instead of polling, fit-width upgraded to live window dims. Ships separately
from M3a so the risky plumbing and the UI each land shippable.

## Cross-cutting

- e2e suite stays fast (<5s) and deterministic; every milestone adds its
  reliability cases (daemon restart, tmux restart, revoked device).
- Attention detector tuning as real-world usage data arrives.

## Parked (decide after M3)

- Custom renderer + snapshot/delta protocol — only if xterm.js or bandwidth
  measurably hurts; the protocol boundary already allows it.
- Semantic layer (V3): OSC 133 shell integration, command feed, Claude Code
  approval cards. Note: OSC 133 does *not* depend on control mode — if M3
  slips, it can be re-evaluated independently.
- Per-device permission tiers (observer/controller/admin) — prerequisite
  for invite-from-device and shared use.
- Hosted apt repo (GPG-signed), cloud relay, collaboration.

## Sequencing rationale

M0 de-risks distribution and fixes onboarding while everything is fresh.
M1 changes daily usefulness the most; its top risk (Apple push behavior) is
a day-1 spike, and it carries the device-schema groundwork so M2 stays
small. M1.5 front-loads the glance experience for one status-frame field.
M2 pays down security debt before M3 adds surface. M3 is split so the risky
plumbing (M3a) and the UI payoff (M3b) each ship independently.
