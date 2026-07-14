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

The headline. Pending-request registry (one-shot ids, expiry, atomic
consume), card payload over the WS + fetched on deep link, Approve/Deny
in the PWA, decision returned to the blocked hook as Claude Code's
documented decision JSON. Requires the `approve` device capability (off by
default), and revocation cancels pending cards (extends the M2 cascade).

Acceptance: locked phone → notification → card shows tool + command →
Approve → Claude proceeds; Deny → Claude declines; expired/duplicate
decisions rejected; a device without `approve` sees the card read-only;
revoking a device kills its pending cards.

### M4c — shell command events + metadata cards (medium)

zsh/bash hook snippets (installed via a documented one-liner, idempotent,
narrow supported matrix — nested shells/ssh/fish explicitly degrade to
busy→quiet) emitting `command_started`/`command_finished {exit, duration,
cwd}` through `remux emit`. Attention v2 for plain shells ("`cargo build`
failed (101) after 4m"). PWA gets the first feed: **metadata-only command
cards** (command, exit badge, duration, running-state) with tap-through to
the terminal — ⚑ no output streaming: OSC boundaries don't make arbitrary
terminal bytes safely re-renderable outside xterm, so output stays in the
real terminal.

Acceptance: long command fails while phone is locked → named notification;
feed shows the day's command history for a session; forged events can at
worst pollute the informational feed, never trigger actions.

### M4d — streamed-output feed (large, gated)

The full chat-timeline with output in the blocks. Explicitly gated on M4c
proving insufficient — rendering command output outside the terminal means
real VT handling per block (progress bars, cursor rewrites, color) and a
capture path that doesn't exist today. Decide with usage data, like the
custom renderer.

Sequencing: M4.0 → M4a → M4b is the shortest path to the differentiating
feature (remote approvals) on the most trustworthy signal (agent hooks);
M4c adds breadth for every plain shell; M4d only if metadata cards leave
a real gap.

## PWA input backlog (reported from daily use)

- **Tab key** (2026-07-14): no way to send Tab from the phone — needed for
  shell completion. Make the on-screen key panel's Tab actionable for the
  terminal input field.
- **Ctrl+letter broken** (2026-07-14): the ⌃ modifier + letter combos do
  not reach the terminal. Diagnose whether the keypanel modifier state or
  the iOS keyboard event path drops them.

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

## Sequencing rationale

M0 de-risks distribution and fixes onboarding while everything is fresh.
M1 changes daily usefulness the most; its top risk (Apple push behavior) is
a day-1 spike, and it carries the device-schema groundwork so M2 stays
small. M1.5 front-loads the glance experience for one status-frame field.
M2 pays down security debt before M3 adds surface. M3 is split so the risky
plumbing (M3a) and the UI payoff (M3b) each ship independently.
