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
> **M1 iPhone push spike PASSED** (lock-screen notification confirmed on
> the installed PWA). **M3a done** (control-mode topology adapter). **M3b
> done** — live window tabs + pane tabs (fed by topology, no polling),
> topology-fed session picker, status-bar hiding restored (controller-only,
> clean mechanism not the old pixel hack), and phone splits auto-zoom
> (small screens never render split geometry; pane tabs navigate them).
> **The whole M3 control-mode arc is complete and deployed.**
>
> Shakedown findings fixed along the way: attestations are public-repo-only,
> the macos-13 runner pool is starved (cross-target from macos-14), a second
> rustls crypto provider panicked the TLS listener (now smoke-tested).
> M3.0 sizing rework replaced the fragile pixel hacks with WebGL rendering +
> debounced grid == what's reported to tmux (fixed garbled full-screen apps
> and small-font glyph overlap); font size = tmux resolution (default 10px).
>
> Remaining user actions: publish the draft v0.1.0 release; create the
> `homebrew-remux` tap with the generated `remux.rb`. Remaining engineering:
> the Parked list below.

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

## M3.0 — terminal sizing stabilization (do first) ⚑

On-device testing exposed the real problem behind the "buggy" mobile
terminal (garbled full-screen apps, status-bar artifacts, the observer
"letterbox"): the phone and tmux disagree on the grid, and several
client-side hacks churn the size. Full-screen apps (htop, vim, Claude Code)
redraw with cursor-addressed output for size A while xterm has resized to
size B — over 2s tailnet latency the redraws interleave and corrupt.

Root causes to remove before building topology on top:
- The status-bar clip renders xterm at `rows+1` and reports `rows+1` to tmux
  (off-by-one vs the visible grid), with a two-pass fit that emits an extra
  resize each time.
- The M1.5 fit-width auto-font loop runs inside `onResize` and re-sets the
  font on every resize → a resize/font feedback loop.
- Resizes are sent to the daemon un-debounced, so a full-screen app is
  hammered with size changes during any layout settle.

Fix: one stable, debounced terminal size == the visible grid (no phantom
row); drop the status-clip and the fit-width-in-resize loop; coalesce
resizes. The status-line hiding and observer fit return properly in M3b via
the topology model, not pixel hacks. This is the concrete first step of the
M3 work and should make the terminal render correctly on the phone.

## M3a — control-mode metadata client (large)

> **Spike done (2026-07-12, tmux 3.3a in the devcontainer)** — the design
> holds: with `read-only,no-output,ignore-size` the window size is untouched
> (120×40 stays 120×40 through create/split/kill), zero `%output` lines leak
> even with pane output flowing, every structural change emits at least one
> notification (non-attached sessions only via `%unlinked-window-*` /
> `%sessions-changed` — confirming dirty-bits + re-list as the model, not
> incremental parsing), and server death delivers a clean `%exit` for the
> supervision loop. Implementation detail: control mode exits on stdin EOF —
> the adapter must hold the client's stdin open.

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

## M4 — semantic layer (hooks → events → approval cards → command feed)

The V3 thesis from DESIGN.md, now unblocked: the daemon stops guessing what
a session is doing and starts knowing. Today attention is a busy→quiet timer
(`src/attention.rs`).

> Reviewed 2026-07-13 by Codex (20 findings, 4 blockers) before any code.
> The review invalidated the first draft's core mechanism — parsing OSC 133
> in-band on the PTY path — for architectural reasons, not detail ones:
> ⚑ the PTY exists only per live WebSocket (`ws.rs`), so there is *no byte
> source at all* when no phone is connected, which is exactly when
> notifications matter; ⚑ what a connected client receives is tmux's
> *rendered* active window (redraws, status line, one window at a time),
> not raw per-pane output, so "one parse per session" doesn't exist on
> that path; ⚑ tmux only handles OSC 133 natively from 3.4 (3.3 needs
> wrapped DCS passthrough, off by default, and `allow-passthrough` opens
> far more than OSC 133). Conclusion: **semantics arrive out-of-band via
> hooks, not in-band via escape-sequence parsing.** OSC 133 is demoted to
> a possible future client-side nicety.

Mechanism: one narrow local **ingest socket** (Unix, 0600, strict JSON
schema, rate/size caps — a *separate* socket from admin: it accepts data,
never commands). A tiny `remux emit` helper posts events to it. Two
producers, sharply distinguished by trust:

- **Agent hooks** (Claude Code, later Codex/OpenCode): the tool itself
  reports "I need permission for X" with structured context, and — for
  approvals — *blocks awaiting the decision*. Authenticated by the
  filesystem boundary; these events may be **actionable**.
- **Shell hooks** (zsh/bash precmd/preexec): report command start/end,
  exit, duration, cwd, keyed by `$TMUX_PANE`. Any process in the session
  could forge these, so they are **informational only, provenance-labeled,
  never actionable** (⚑ forged markers must not be able to create an
  Approve button).

Guiding constraints, recorded up front:
- **No screen scraping, no keystroke approvals.** TUIs are never parsed or
  re-rendered; approve/deny is returned to the *waiting hook process* as
  its documented decision JSON — ⚑ never `send-keys` into a pane, where
  focus/prompt races make "y" a confused-deputy write.
- **Cards are bound and one-shot**: unguessable request id + pane/session
  identity (from M3 topology — ⚑ a real dependency, not "independent") +
  expiry + atomic consumption; stale or duplicate decisions are rejected.
- ⚑ **Approval is a new privilege.** Flat device tokens were acceptable
  for "view and type"; remotely authorizing agent tool-use is more. Pull
  the minimal capability from the parked tiers: approvals require a
  per-device `approve` flag, host-CLI-granted (`remux devices`), off by
  default.
- ⚑ **Secrets posture**: command lines carry tokens and passwords. Command
  text is capped, redacted (common secret shapes), bounded-retention,
  in-memory only; lock-screen/push text stays generic (push is already
  payload-less — detail is fetched post-auth on tap, as with attention).
- **Progressive enhancement.** No hooks installed → today's behavior,
  busy→quiet heuristic stays as the fallback detector.

### M4.0 — protocol spike (small, do first)

Same day-1-gate discipline as M3a/M1. (a) Claude Code side: which hook
carries permission requests and can return an allow/deny decision
(PermissionRequest vs PreToolUse `permissionDecision` — verify against
current docs on the versions we support), what context it provides
(tool, input, session, cwd), and confirm a hook can block for tens of
seconds awaiting a remote decision without Claude Code timing out or
misbehaving. (b) Daemon side: ingest socket shape + `remux emit` helper +
pane identity (`$TMUX_PANE` ↔ topology pane id, including pane death while
a request is pending). Deliverable: a written protocol note; if hooks
can't block, M4b re-plans around Notification-hook + polling before any
daemon code.

### M4a — ingest + Claude Code attention (medium)

The ingest socket, event model with **classes** (`agent_needs_input`,
`command_finished`, …) and provenance, wired into the existing attention
broadcast → push dispatcher → `/api/attention` deep link. ⚑ Suppression
rules become class-aware: approval-class events bypass the live-socket
skip and per-session throttle (a backgrounded PWA counts as a live socket
today — that must not swallow an approval), with dedup + expiry + ack
instead. Ships alone as: precise, named lock-screen notifications for
Claude Code ("Claude Code needs permission in remux:main") instead of
"went quiet".

Acceptance: phone locked, Claude Code asks on the Mac → notification ≤30s
naming the session; opening the app deep-links to it; marker-less panes
still get busy→quiet.

### M4b — approval decisions (medium)

The headline: approve/deny Claude Code's permission prompts from the
locked phone. `remux emit permission --wait` (the hook) blocks on the
ingest socket; the daemon holds a pending card until a phone decision or
expiry; the decision is returned to the hook as Claude Code's documented
decision JSON. Empirically de-risked (spike + Codex plan review,
2026-07-14) — the design below reflects both.

**Threat model (corrected — Codex blocker 1).** Same-uid is *already
fully trusted*: the admin socket lets any same-uid process pair a device
and (M4b) grant it `approve`, and it can run commands directly anyway. So
M4b is **not** a defense against local same-uid code — the phone is
remote authorization + a second human in the loop, not a privilege
boundary against the host. What M4b *does* guarantee: a card carries no
authority (opening one only makes a prompt appear); only a paired,
`approve`-capable device resolves it; and no failure path ever fabricates
a decision. Do not claim more than that in code or docs.

**The Mac-vs-hook race is settled (spike Q6).** Claude shows its own
dialog concurrently while the hook blocks; answering the Mac dialog
**SIGTERMs the hook** and honors the Mac — Claude never accepts hook
output afterward. So there's no conflicting-decision hazard, but it makes
one thing a hard requirement, not cleanup: the held ingest connection
**must monitor its socket for EOF/reset concurrently** with the decision
channel and the expiry timer. A broken wait = "the Mac already answered"
→ drop the card; no phone decision may resolve it after. (Codex blocker
5; timeout-only cleanup is insufficient.)

**Ingest lifecycle refactor (Codex blocker 6).** The current 5s timeout
wraps all of `handle()` before the line is even read. Split into: bounded
admission/read/parse (keeps the 5s budget + a short pre-parse cap) →
kind dispatch → either the short ack path (today's behavior) or a
separately-bounded held-wait path (up to CARD_TTL). Held permission waits
get their **own small cap** (~4–8 global, 1–2 per pane), never the shared
16-slot ingest pool — else 16 prompts starve all shell events (Codex
major 1, Q1). Excess → immediate reject → hook falls back to the Mac.

**Registry + resolver.** `App.pending_permissions` (its own type; keyed by
a daemon-minted ≥128-bit id, *not* 48-bit — Codex minor; correlate with
Claude's `prompt_id` for dedup so a retry can't double-card — Q5). One
`resolve(id, decision, device_id)` operation does everything under the
registry mutex **without awaiting**: check not-expired, check the device's
*current* `approve` capability by id (not a captured `Device` clone — a
socket authed before a grant/revoke must see the change; Codex blocker 2),
remove exactly one entry, then send on its `oneshot` (sync send, so no
mutex held across `.await` — Codex major 2). A failed send = "waiter gone"
→ report "request gone", never success. Aborting the ingest future must
not orphan the map entry (guard/RAII on the registry slot).

**Delivery (Codex blocker 3).** Add a dedicated permission broadcast
channel (App.connections holds counts, not senders — can't fan frames
today). But broadcast is a **hint only**: every eligible socket reconciles
against a registry snapshot on (a) connect/subscribe and (b) `Lagged`.
Subscribe *before* the connection is marked push-suppression-eligible, or
the setup-race drops the card on both surfaces (Codex blocker 3 / major 7).
Decision transport: an authenticated **HTTP POST is the canonical decision
op** (Q3 — approving shouldn't require a full PTY WebSocket); the WS frame
invokes the same `resolve`. Card *details* (tool, command) are
`approve`-only on **both** WS and HTTP (Codex major 6 — visibility is
privileged since commands leak secrets); ids never ride push payloads or
URLs (fetched post-auth, as attention does).

**Secrets / notification hygiene (Codex major 3).** Permission attention
must **not** reuse the 600s `/api/attention` retention (cards expire at
CARD_TTL≈100s — a tap would point at a dead card) and must **not** put the
command/path in `Attention.reason` (the service worker prints it on the
lock screen). Lock-screen text stays generic ("Claude Code needs
permission in remux:main"); the command is fetched only after the PWA
opens and authenticates. Resolve/expire correlates the attention away.

**Capability plumbing.** `Device.approve: bool` (`#[serde(default)]`, off;
compatible with existing devices.json). `remux devices grant-approve <id>`
/ `revoke-approve <id>`, host-CLI-only, persisted-then-return with
rollback on write failure (like `revoke`). Decision-time check is always
by current id state. Revoking a device or its capability drops *its* right
to resolve, but does **not** cancel cards it doesn't own (Q4) — other
approvers may still act; if none remain, cards expire to the Mac.

**Install snippet is part of the boundary (Codex major 5).** The hook is a
one-liner that pipes Claude's stdin JSON straight into `remux emit
permission --wait` (structured parse inside the CLI — no fragile shell
arg extraction); the CLI reads `$TMUX_PANE` from its own env, prints
*only* Claude's exact decision JSON on stdout (diagnostics to stderr),
exits non-zero on any failure → Mac dialog. `timeout: 120` in the hook
config, CARD_TTL≈100s comfortably under it. Document that
PermissionRequest doesn't fire in `-p` mode and that swapping in PreToolUse
would change the fallback from "ask on Mac" to "hard block".

**Build increments** (each: tests + Codex review + commit):
1. **DONE (2026-07-14).** Ingest lifecycle refactor + `permit::Registry`
   (single-winner `resolve` with the capability check evaluated *under the
   registry lock*, prompt_id dedup, global/per-pane caps, 128-bit ids) +
   held-wait path (biased select: socket-EOF wins ties, so a broken wait is
   never overridden by a late decision) + RAII `CardGuard` cleanup on abort
   + `emit permission --wait` (stdin parse, stdout is decision-JSON-only,
   logs forced to stderr) + `Device.approve` + grant/revoke CLI. Resolved
   via a test-only resolver (no shipped approve-bypass). Codex-reviewed
   (1 blocker, 4 major — all fixed).
2. **DONE (2026-07-14).** Registry change-hint broadcast (payload-less;
   receivers reconcile against `snapshot()`) fired on open/resolve/remove/
   sweep and on capability change (`notify_watchers`); WS `permission_cards`
   frame (reconcile-on-subscribe + on-lag, approve-gated by *current* id, only
   sends when non-empty or clearing); approve-gated `GET /api/permissions` +
   `POST /api/permissions/{id}/decide` (the canonical decision op — the WS
   only delivers). **Write confirmation** implemented: `resolve` returns
   `(Card, Receiver<()>)`; the held-wait fires it only after a successful
   socket write, and the decide handler awaits it (8s > the 5s write timeout)
   → `written:true` or 409 (this proves the decision was written to the live
   hook socket, not a guaranteed end-to-end ACK). Generic wake (kind only, no
   command/source) pushed but kept out of the 600s attention retention;
   permission pushes bypass the busy→quiet throttle. Codex-reviewed (0
   blockers, 4 major, 1 minor — all fixed). Tests: registry hints, http
   auth/visibility/validation/write-confirmation.
3. **DONE (2026-07-14).** PWA `permission_cards` handler → Approve/Deny card
   with a live countdown (prunes locally on expiry; retains on unexpected
   POST failure so the WS reconcile repairs; double-tap-guarded; textContent
   only, no HTML sink). SW prefers a "needs permission in <session>"
   notification (agent+session, never the command), fetched concurrently with
   attention under the 8s deadline. `remux test-permission` opens a real card
   and blocks so the whole path is exercisable without a Claude Code hook.
   Codex-reviewed (0 blockers, 2 major, 2 minor — all fixed).
4. **DONE (2026-07-14).** On-device integration test PASSED — approved a
   real permission card from the iPhone via `remux test-permission`; the
   blocked command received `decision: allow`. Full path proven end to end.

Acceptance: locked phone → generic notification → open → card shows tool +
command (approve-capable only) with a live countdown → Approve → Claude
proceeds; Deny → Claude declines; answering on the Mac instead kills the
card cleanly; expired/duplicate/already-resolved decisions rejected
distinctly; a non-approve device sees neither details nor buttons;
granting/revoking `approve` takes effect on already-connected sockets.

### M4c — shell command events + metadata cards (medium)

Plan Codex-reviewed 2026-07-14 (6 blockers + majors) before code — design
below reflects it. **zsh first** (bash deferred: macOS bash 3.2 lacks
`EPOCHREALTIME` and `trap DEBUG` fires too broadly — needs a proven preexec
shim + a recursion-guarded state machine, its own increment). Hooks emit
correlated command events; the daemon keeps a bounded per-session metadata
feed and raises precise failure notifications; the PWA shows metadata-only
command cards. ⚑ **No output streaming** (that's the gated M4d).

**Delivery must be non-blocking + isolated (Codex blockers 1, 3).** The
current `remux emit` blocks on an ack (5s, no connect deadline) and shares
the 60/min global ingest limiter — calling it from `preexec`/`precmd` would
stall every prompt, and shell volume would starve *actionable*
`agent_permission` events. So shell events get a **separate surface**: a
dedicated non-blocking, lossy delivery (a `SOCK_DGRAM` ingest, or a
write-and-close with no ack) on its own socket/queue with its own budget
(~120/pane/min, ~600/min global, overflow drops silently). The agent socket
and its 16 slots stay untouched. The hook must never hang the shell: dead
daemon → the emit is a no-op, prompt returns instantly.

**Correlated events (Codex blocker 2).** The shell mints a `shell_id` (once
per interactive shell) and a per-shell monotonic `command_id`; both ride
start and finish. `command_started {shell_id, command_id, pane, command,
cwd}` on preexec; `command_finished {shell_id, command_id, pane, exit}` on
precmd (daemon-observed elapsed from the matched start is canonical duration;
the shell's own timing is diagnostic only). Semantics: duplicate start/finish
= no-op; finish-without-start = ignored + counted; a new start superseding an
unfinished one on the same shell marks the predecessor aborted; nested shells
stay independent (distinct `shell_id`). Model name is "interactive shell
submission", not process lifecycle — `job &` reports prompt-return, not job
exit. Cap `command`/`cwd` in bytes (line stays < 4096) and clamp exit/elapsed.

**Feed store + sweeper (Codex blocker 5).** `feed::Feed` mirrors the
`permit::Registry` hint+snapshot pattern, but a reconcile-on-hint model has
no wake for stale/expired entries — so a **timer sweeper** marks running
commands stale after a cap (~6h), evicts by age (~6–12h) and by count, and
**fires the change hint** so views update. Bounds are global, not just
per-session (abandoned sessions must not leak): separate running-command map
so completed pressure can't evict an active command; global cap (~5000);
dead-session bucket cleanup. Memory-only (no disk); restart loss is
acceptable and documented. Snapshot serializes `started_age_ms`/wall-clock,
never a Rust `Instant`.

**Precise attention without double-firing (Codex blocker 6).** The busy→quiet
`Detector` map is private to the attention monitor task; ingest can't reach
it. Add a coordination channel: a matched `command_finished` sends
`(session)` to the monitor, which **resets that session's busy epoch** (all
matched finishes consume it, including quick successes), then the finish
handler raises attention only if policy says so (`exit != 0` OR elapsed ≥
~30s). Keep the anti-spam per-(endpoint,session) throttle, but let a real
failure supersede a prior heuristic event from the same busy cycle.

**Secrets (Codex Q5, hard rule).** Command lines carry tokens. Never copy
`command`/`cwd` into `Attention` (the SW renders `reason` on the lock
screen). Shell attention text is built only from validated **exit + elapsed +
session** ("a command failed (101) in <session>"), never the command — this
overrides the old "`cargo build` failed" example. The full command lives only
in the post-auth feed (memory-only, over the authenticated WS, never push).
Add a test asserting no command reaches `/api/attention`/the SW. Never log raw
commands. Feed history must **not** be persisted to `localStorage`.

**PWA feed (Codex majors).** A `command_feed` frame, but — unlike the tiny
permission frame — a ≤200-entry snapshot shares the bounded PTY-byte outgoing
queue, so repeated snapshots could delay terminal output: **filter to the
connection's session**, coalesce/debounce the hint (~100–250ms, trailing),
clone the snapshot outside the mutex, always send one initial snapshot (even
empty) to clear stale client state. Feed panel shows metadata cards (command
via `textContent`, exit badge ✓/✗code, elapsed, relative time, running
spinner) newest-first. Tap-through: define it — a tap navigates to the card's
session and to the current topology location of its stable pane id (handle
moved/deleted panes); switching sessions alone isn't enough, and selecting a
pane needs controller status. No new device capability (paired already
observes the session), but document that the feed adds *retrospective* command
history; offer a **host-level "disable command capture"** switch rather than
per-device gating.

**Shell-history mirroring (Codex major).** Composer ↑ recalls the session's
actual recent commands from the feed — but keep that recall list in
**per-session memory only**; never merge captured desktop commands into the
`localStorage` composer history (that would persist them indefinitely, across
sessions, beyond feed retention). Make the existing global composer history
session-scoped as part of this.

**Build increments** (each: tests + Codex review + commit):
1. Non-blocking shell ingest surface + correlated `command_started/finished`
   + `feed::Feed` (pairing, global+per-session bounds, running map) + timer
   sweeper + zsh hook snippet + non-blocking `remux emit command`. Tests:
   pairing/dupe/out-of-order, bounds+sweep, ambiguous pane, forgery-inert,
   budget isolation.
2. Precise attention (matched finish → epoch reset + secrets-safe
   notification) + the no-command-leak test.
3. `command_feed` WS frame (session-filtered, debounced, initial snapshot) +
   PWA feed panel + tap-through.
4. Shell-history mirroring + session-scoped composer history.
5. On-device test.

Acceptance: a long/failed command on the Mac tmux → a secrets-safe named
notification on the locked phone; the feed shows the session's recent
commands with exit/duration; ↑ in the composer recalls actual session
commands; forged events can at worst pollute the informational feed, never
trigger an action, and never leak a command to the lock screen; a busy build
session never starves permission events.

### M4d — streamed-output feed (large, gated — do NOT build on spec)

The full chat-timeline with output in the blocks. Explicitly gated on M4c
proving insufficient — rendering command output outside the terminal means
real VT handling per block (progress bars, cursor rewrites, color, alternate
screen) and a per-block capture path that doesn't exist today, plus new
background-process, retention, and secret-exposure problems M4c's shell
tokens do NOT solve. Codex-reviewed gate (Q8): M4c metadata is enough for
awareness, navigation, exit/duration diagnosis, and history recall. **Build
M4d only after repeated real evidence of needing a command's *output* once
it's no longer recoverable from tmux scrollback** — e.g. "which tests
failed?", "what URL/artifact did it print?", "what was the final error after
scrollback scrolled off?", "a command ran in another pane while the phone was
disconnected and exit metadata can't explain the outcome". One-off curiosity
doesn't count; a recurring workflow gap does. Decide with usage data, like
the custom renderer.

Sequencing: M4.0 → M4a → M4b is the shortest path to the differentiating
feature (remote approvals) on the most trustworthy signal (agent hooks);
M4c adds breadth for every plain shell; M4d only if metadata cards leave
a real gap.

## PWA input backlog (reported from daily use)

- ~~**Tab key**~~ / ~~**Ctrl+letter**~~ — DONE 2026-07-14 (`66db6ca`),
  plus composer-first routing for punctuation/cursor/paste and the ▴
  composer-history button (`a0f5819`).
- **Shell-history mirroring** (2026-07-14, wants M4c): key-row ↑ recalls
  the shell's previous command in the terminal, but the phone can't edit
  it there and the client can't lift it into the composer — it only sees
  pixels, with no prompt/command boundary. Once OSC 133 marks command
  spans (M4c), mirror the recalled shell line into the editable composer.
  This was the user's instinctive expectation of ↑.

## Cross-cutting

- e2e suite stays fast (<5s) and deterministic; every milestone adds its
  reliability cases (daemon restart, tmux restart, revoked device).
- Attention detector tuning as real-world usage data arrives.

## Parked (decide after M3)

- Custom renderer + snapshot/delta protocol — only if xterm.js or bandwidth
  measurably hurts; the protocol boundary already allows it.
- Per-device permission tiers (observer/controller/admin) — prerequisite
  for invite-from-device and shared use.
- Hosted apt repo (GPG-signed), cloud relay, collaboration.
- **Multi-machine client** (the "Machines" screen from the original design):
  one PWA/app talks to several remux daemons — laptop, home server, cloud VM
  — with a machine picker and per-host device tokens. Key challenge: a PWA
  served from host A opening a WebSocket to host B is cross-origin, which each
  daemon's Host/Origin guard currently rejects; needs one of (a) a native
  shell not bound to a single origin, (b) each daemon allowlisting the others'
  origins, or (c) a small hub/relay the app talks to. Also: host discovery
  (MagicDNS/manual/QR) and switching sessions across hosts. Independent of
  M3; a real V2/V3 direction.
- Built-in ACME (`rustls-acme`, e.g. `--acme-domain` + DNS-01 credentials):
  the daemon issues and renews its own publicly-trusted certificate, making
  remux fully VPN-agnostic (today Tailscale is only "special" because
  `tailscale cert` is the zero-config way to satisfy iOS's trusted-TLS
  requirement — there is no code dependency on it).
- **Voice input for the composer** (idea, decide later) — dictate commands to the
  phone composer by speech, but with a *domain-adapted* recognizer rather than a
  generic dictation engine: bias/condition the speech-to-text on the CLI
  vocabulary actually in play — installed tools' names, their subcommands/flags
  (from `--help`/completion specs/man pages), symbols and paths visible in the
  pane, and recent shell history — so it transcribes `git rebase -i HEAD~3`, not
  "git rebase eye head three". Open questions to resolve when we pick this up:
  (a) contextual biasing / hotword lists over a general STT vs. a small
  fine-tuned model; (b) where the dictionary comes from and how it's kept current
  per session/host; (c) on-device vs server inference under the secrets posture —
  spoken commands and their transcripts can carry secrets, so audio/text should
  stay memory-only and most likely on-device. Fits the composer, not the raw
  terminal (the composer is already the edit-before-send surface).

## Sequencing rationale

M0 de-risks distribution and fixes onboarding while everything is fresh.
M1 changes daily usefulness the most; its top risk (Apple push behavior) is
a day-1 spike, and it carries the device-schema groundwork so M2 stays
small. M1.5 front-loads the glance experience for one status-frame field.
M2 pays down security debt before M3 adds surface. M3 is split so the risky
plumbing (M3a) and the UI payoff (M3b) each ship independently.
