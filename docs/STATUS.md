# remux — working status snapshot

> Uncommitted working note (per request). Point-in-time picture of where the
> project is and what's planned. The durable plan is `docs/PLAN.md`.

Repo state: `main` clean, everything pushed to `github.com/jorgeamado/remux`
(public), CI green. HEAD = `5251fdd` (M3b pane tabs). The whole M3
control-mode arc is done and deployed.

## Deployed

- `remux-mobile` Docker container serving the latest build over HTTPS/HTTP2 at
  `https://georges-macbook-air.shrew-fort.ts.net:7777` (Tailscale cert).
- It runs the devcontainer image + the debug binary from the bind-mounted repo;
  `docker restart remux-mobile` after a `cargo build` picks up new code.
- ⚠️ **Deploy hazard (bitten 2026-07-13):** the container execs
  `target/debug/remux` from the bind mount, and a *host* (macOS) `cargo
  build`/`test` overwrites it with a Mach-O binary → the container
  crash-loops with exit 126. Rebuild the Linux binary with
  `docker run --rm -v $PWD:/workspaces/remux <devcontainer-image> bash -c
  'cd /workspaces/remux && CARGO_TARGET_DIR=target-linux cargo build && cp
  target-linux/debug/remux target/debug/remux'` and the restart policy
  self-heals. Durable fix worth doing: recreate the container to exec from
  `target-linux/debug/remux` so host and container builds never collide.
- Mint a pairing link: `docker exec remux-mobile /workspaces/remux/target/debug/remux pair`.

## Done (this arc)

- **M0** — release pipeline (CI + tagged releases building 4 platforms:
  tarballs, .debs, SHA256SUMS, filled Homebrew formula, sigstore attestations);
  v0.1.0 draft release built & verified on clean Debian; onboarding fixes
  (reusable pairing tokens + iOS install flow), `remux pair` CLI over a 0600
  admin socket, cert-renewal timer.
- **M1 Web Push** — VAPID payload-less push, SSRF-guarded subscribe API,
  keyboard-aware + live-socket-skip dispatcher, `/api/attention` deep-link.
  **On-device iPhone spike PASSED** (lock-screen notifications confirmed).
- **M1.5** — (superseded) observer fit; **M2** — device management (revocation
  cascade, `remux devices` CLI, read-only PWA Devices sheet).
- **Security hardening** — gitleaks/cargo-audit/npm-audit in CI, clippy `-D
  warnings`, Dependabot, SECURITY.md; adversarial audit fixes (push SSRF via
  userinfo URL, synchronous revocation, connection caps, constant-time token
  compare, admin peer-uid check).
- **M3.0 sizing stabilization** — removed the status-clip/fit-width hacks that
  garbled full-screen apps; one debounced grid == what's reported to tmux;
  WebGL renderer (fixes small-font glyph overlap); font-as-resolution (default
  10px, floor 7px); debug overlay (aA → Debug) showing grid/box/cell/dpr/vv.
- **M3a control-mode topology adapter** — read-only watcher client, dirty-bit
  full re-list, `topology` frames over a watch channel; attached-count excludes
  the internal watcher; `ensure_session` made idempotent (race fix).
- **M3b DONE** — live **window tabs** + **pane tabs** from topology (tap to
  switch; no polling), session picker fed from topology, `+` menu create-only,
  **status-bar hiding restored** (controller-only, render-extra-row-and-clip,
  not the old pixel hack), **phone splits auto-zoom** (small screens never
  render split geometry; pane tabs navigate a split pane-by-pane, each
  full-screen). WebGL renderer + debounced sizing underneath (font = tmux
  resolution). **M3 control-mode arc complete.**

## Waiting on the user (both one-click, public-facing — I can't do these)

1. Publish the draft **v0.1.0** GitHub release (built & verified).
2. Create the `jorgeamado/homebrew-remux` tap repo and drop the release's
   generated `remux.rb` into `Formula/` → makes `brew install` real.

## Active: M4 — semantic layer (started 2026-07-13)

**M4a VERIFIED ON DEVICE (2026-07-14):** locked iPhone shows the named
notification ("main — test: test notification — it works!") from a
`remux test-attention` run. Full path proven: ingest socket → typed
attention event → payload-less push → SW wakes, reads the device token
from IndexedDB, fetches /api/attention (8s deadline, generic fallback)
→ named lock-screen text. Field gotchas learned: the PWA must be
force-quit + relaunched to pick up a new service worker, and the first
generic-text round was exactly that; /api/attention hits are now logged
at info to make this diagnosable.

**PWA input backlog DONE (2026-07-14):** Tab completion, Ctrl+letter,
composer ▴ history (editable recall, wraps, hints when empty), and
composer-first routing for key-row punctuation / cursor keys /
single-line paste. All verified on-device by the user; shell-history
mirroring into the composer is recorded as an M4c deliverable (needs
OSC 133 prompt/command boundaries). Field notes: "direct typing" is a
deliberate off-by-default phone setting (aA menu), and a hard reload
resets it + wipes composer history — both read as bugs until explained.

**M4b day-1 gate PASSED (2026-07-14).** Ran the blocking
PermissionRequest-hook test against live Claude Code v2.1.197 (isolated
tmux, scratch project, hook sleeps 90s then returns allow). Confirmed:
the hook blocks and its decision is honored; hook death → the Mac dialog
stays live and never auto-decides; and — the load-bearing one — when the
user answers the Mac dialog *while the hook blocks*, Claude **SIGTERMs the
hook** and honors the Mac, never accepting hook output afterward. See
`docs/spikes/M4.0-protocol.md` "Empirical check" + "Mac-vs-hook race".
Consequence baked into the plan: the held ingest connection must watch for
socket EOF (= "Mac already answered") alongside the decision + expiry.

**M4b detailed design reviewed by Codex (2026-07-14): 6 blockers, 7 major,
minors — all folded into PLAN.md §M4b before any code.** Key corrections:
same-uid is already trusted (the phone is remote auth, not a local-attacker
boundary); capability checked by current device id at decision time (not a
captured clone); permission cards need their own broadcast+registry
(App.connections holds counts, not senders) with reconcile-on-subscribe;
held waits get their own small cap (never the shared 16-slot ingest pool);
generic lock-screen text only (command fetched post-auth); HTTP POST is the
canonical decision op.

**M4b increment 1 DONE + Codex-reviewed + CI (2026-07-14).** The daemon
side of approvals, minus the phone-facing delivery:
- `permit::Registry` — single-winner `resolve` with the `approve` check
  under the registry lock, prompt_id dedup, global/per-pane caps, 128-bit
  ids;
- ingest lifecycle refactored into admission/parse → dispatch → held-wait;
  the `agent_permission` wait releases its admission slot and does a biased
  select of socket-EOF / decision / expiry (EOF wins ties, honoring "a
  broken wait is never overridden by a late decision"), with RAII cleanup;
- `remux emit permission --wait` (stdin parse, decision-JSON-only stdout,
  logs forced to stderr), `Device.approve` + grant/revoke CLI.
Code review found 1 blocker + 4 major, all fixed (resolve/EOF tie via
`biased`; capability coupled into resolve; prompt_id dedup + strict reject;
stdout hygiene). One residual is recorded for increment 2: the phone's
decision response must reflect *actual delivery* to the hook, not just that
resolve consumed the card. Next: **increment 2** — broadcast channel + WS
card frame + approve-gated HTTP decide/list + generic permission attention.

Note on where Claude Code runs: the day-1 test needed no container decision
(ran on the host). For real use the hook's `remux emit` must reach the
daemon's ingest socket, so Claude Code runs wherever the daemon's tmux is —
today that's the container. Deferred to integration (increment 4); doesn't
block increments 1–3.

Housekeeping owed: 6 stale iPhone device rows to revoke (user go-ahead
needed); local e2e assumes a "$" prompt (fails under the user's zsh
theme, fine in CI); v0.1.0 publish + brew tap still user-only.


Plan drafted, Codex-reviewed (20 findings, 4 blockers), and rewritten
before any code — see PLAN.md §M4. The review killed in-band OSC 133
parsing (per-connection PTY = no byte source while the phone is locked;
attached clients see rendered output, not per-pane bytes; tmux <3.4 lacks
native OSC 133). New mechanism: out-of-band hooks → a local ingest socket
(`remux emit`), agent hooks actionable / shell hooks informational-only,
approvals returned to the blocked Claude Code hook as decision JSON (never
send-keys), gated by a new per-device `approve` capability.

Next step: **M4.0 protocol spike** — verify the Claude Code permission
hook (name, context, can it block awaiting a remote decision), and the
ingest-socket + `$TMUX_PANE`↔topology mapping.

## Parked (see PLAN.md "Parked")

- **Multi-machine client** (new idea, 2026-07-13): one app talks to several
  remux daemons (laptop / home server / cloud VM) — machine picker, per-host
  device tokens. Blocker to design around: cross-origin (a PWA from host A
  → WebSocket to host B is rejected by B's Host/Origin guard) — needs a
  native shell, mutual origin allowlisting, or a hub/relay. Plus host
  discovery. Independent of M3.
- Custom renderer + snapshot/delta protocol (only if xterm.js/bandwidth hurt).
- Per-device permission tiers (observer/controller/admin) — unlocks
  invite-from-device and shared use (M4 pulls just the `approve` flag out).
- Hosted apt repo (GPG-signed); built-in ACME (VPN-agnostic TLS — no code
  dependency on Tailscale today, it's only the zero-config cert path).
- M4d streamed-output feed — gated on M4c metadata cards proving
  insufficient.

Optional small polish still open: a live pane-command **breadcrumb** in the
header (the session button + tabs already form a breadcrumb, so low value).

## Working notes / gotchas

- tmux 3.3a quirks (in memory + code comments): `-F` sanitizes control chars
  (incl. tabs) to `_`; `split-window`/`display-message -t '=sess'` need the
  pane-shaped `'=sess:'`; `session_activity` = client input (use
  `window_activity` for output); `read-only` can't be cleared via
  `refresh-client`.
- Loop discipline this arc: each increment → full tests + e2e → Codex review →
  fix findings → commit → push → CI. Codex caught real bugs *in the fixes*
  repeatedly (cap ordering, revocation gaps, FitAddon measuring the padded
  card, topology attached-count). Keep doing this.
- Debug overlay (aA → Debug) is the fastest way to diagnose device-specific
  rendering — it surfaced the font-stuck-at-6px in one screenshot.
