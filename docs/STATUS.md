# remux — working status snapshot

> Uncommitted working note (per request). Point-in-time picture of where the
> project is and what's planned. The durable plan is `docs/PLAN.md`.

## M5 pane views / custom dashboards — VERIFIED ON DEVICE (2026-07-15)

remux can render a **custom, phone-adapted PWA interface for a pane** — a
"semantic lens" over the pane, the terminal always one tap away. On branch
`feat/pane-view` (NOT merged), ~2.7k lines, multiple Codex review rounds.
Architecture (Codex-guided): compiled-in renderers over a versioned schema (NO
third-party JS), terminal stays canonical, one pane-view registry that any
*source* feeds.

**The pipe** — `source → remux (pane_views registry) → session-filtered WS frame
→ compiled-in PWA renderer`, with a Terminal/Dashboard toggle:
- `remux stream --view <id>` + a dedicated persistent `pane-view.sock`: latest-
  state-per-pane registry, one view/pane, EOF + topology-GC cleanup, size/rate/
  shape caps.

**Two source kinds proven:**
- **`taskscope`** (`examples/taskscope`) — our demo monitor; run it plain and it
  self-streams to remux (bg process-sub) so a Dashboard appears with no piping.
- **htop capture adapter** — the daemon auto-detects a pane running `htop`
  (topology `pane_current_command`) and reads its RENDERED screen via
  `tmux capture-pane` on a timer, parses the visible process table + meters into
  `htop.v1` (tolerant, visible-slice-only, low-confidence fallback). No /proc,
  no reimplementation — uses htop's own output.

**Dashboard "capture resolution"**: entering the dashboard forces that pane's
window to 210×60 (window-size manual) so a full-screen tool exposes all
columns/rows for the capture; restored to `latest` on leave/disconnect. Terminal
hidden then, so the oversized render is invisible.

**Interactions** — the phone sends a *whitelisted semantic action*, the daemon
maps it to the tool's real keys and `tmux send-keys` (never raw keystrokes),
only for an htop pane in the client's session:
- sort CPU/MEM/TIME (P/M/T) + invert (I), tree (F5), filter (F4+clear+text+Enter,
  control-stripped/capped), kill:<pid> (SIGTERM, gated to a pid in the pane's
  captured view, behind a confirmation sheet).

**htop UI redesign** (Codex-reviewed): instrument-panel — compact CPU/MEM bars,
sticky filter/tree/sort toolbar, two-line rows with a 3px CPU rail, restrained
color thresholds, stateful renderer that keeps the filter input focused across
live updates.

Also fixed a latent bug: static assets had no cache headers, so a stale
`index.html` pinned the old JS bundle — now the shell/sw revalidate and hashed
assets are immutable.

On device (iPhone, `remux-mobile` container): taskscope dashboard, htop dashboard
with command names (via capture-resolution), and sort/filter/kill all verified.
Tree renders but is cosmetically rough at phone width (kept as-is).

**Two Codex security rounds addressed** (fix → re-verify → fix). Final posture:
- Command-execution path closed: `filter` types attacker-controlled literal text,
  and check-then-send to a foreground tool is fundamentally non-atomic (if htop
  exits mid-sequence the text can reach the shell for the user's next Enter). So
  `filter` and `kill` now require the host-granted `approve` capability — a device
  already trusted to approve arbitrary command execution — plus per-phase htop
  re-verification. Fixed keys (sort/invert/tree) carry no attacker content and
  stay open to any in-session device.
- Capture lifecycle: poll-driven (independent 2 s `pane_current_command` poll,
  not the structural topology feed), each task re-verifies ownership per 1.5 s
  tick and drops its view the instant htop exits, and a task whose claim is pruned
  out from under it (`guard.update` → `None`) now exits so the poller re-claims
  (previously it could wedge capture permanently).
- Kill: `approve`-gated AND restricted to a pid the pane's view is showing.
- ViewMode sizing: gated to the client's own in-session pane with a live view.
- PWA re-sends `view_mode` and closes the kill sheet on any pane switch.

Accepted residuals (documented, within the same-uid / trusted-approver model):
capture verification has a ≤1.5 s tick (a lingering htop.v1 view can only drive
harmless fixed keys — filter/kill are approve-gated); kill-by-pid has an inherent
PID-reuse window; a daemon crash mid-dashboard can leave a window at `manual`
sizing (self-inflicted, same-uid, cleared by re-entering the dashboard — an
earlier startup-sweep fix was reverted because it clobbered a user's deliberate
`window-size manual`). Branch is merge-ready.
**Generic popup primitive (slice 1 done).** A pane-view renderer (and, later, a
plugin) declares a title + options; each option maps to an *already-whitelisted*
pane action. The popup is pure presentation over the action whitelist — it never
sends raw input, and the daemon re-validates every action on receipt, so the
security model is untouched. Core PWA component `openPopup(spec)`/`closePopup()`
(bottom sheet, `default`/`danger`/`cancel` styles, backdrop-dismiss, lifecycle
tied to pane switch / dashboard exit / view clear). First consumer: the htop
process-signal sheet, refactored onto it and now offering **SIGTERM / SIGKILL**
(action whitelist extended to `kill:<pid>:TERM|KILL`, signal a closed whitelist,
still `approve`-gated + `pane_has_pid`).
**Generic popup primitive (slice 2 done — the plugin interaction loop).** A
streaming *source* can now declare an interactive `menu` in its view state, and
the user's choice is routed back to that source over a duplex back-channel — so a
plugin can offer actions without shipping UI or reaching tmux/shell. Design and
implementation went through **three Codex security rounds**. Shape:
- Schema: a snapshot may carry a generic `menu {title, detail?, options:[{label,
  action, style?, requires?}]}`, validated for any view. Action tokens use an
  ASCII grammar `[A-Za-z0-9._:-]{1,64}` (no newline/separator → safe to relay as
  one JSON line), deduped, bounded.
- Provenance boundary: `htop.v1` is internal-only (daemon executes its actions
  via tmux/kill); external `remux stream` sources get socket views only and can
  never claim `htop.v1` or drive the tmux path. Actions route by provenance, not
  view id. The htop kill gate checks `InternalHtop` + pid under one lock.
- Capability: each option carries `requires: approve|session` (default `approve`;
  `danger` forces `approve`); the daemon enforces it and only forwards a token
  that is in the pane's *current* menu. Sources use nonced/target-bound tokens;
  the PWA also drops an open popup when the underlying menu changes.
- Back-channel: bounded (16) channel, write-timeout, fair (non-biased) select,
  prompt prune-release; `remux stream --actions <fifo>` relays choices to the
  source (O_RDWR FIFO, detached relay, EOF-clean). WS hardened with a 64 KiB
  message cap + a pre-parse text-message token bucket (CPU-flood bound).
- Demo: taskscope advertises Pause/Resume and reacts via a FIFO; PWA renders a
  generic Actions button + the shared popup.

Other next options: `docker stats` (native-JSON command adapter — Codex's robust
pick), or an agent-window source.

Repo: `github.com/jorgeamado/remux` (public); `main` builds green on CI. The M3
control-mode arc and the whole M4 semantic layer (M4a attention → M4b approvals
→ M4c command feed) are done and verified on-device. On top of that, three
multi-angle Codex-reviewed hardening tiers (security/correctness →
robustness/honesty → release-readiness/tests/docs): Tiers 1–2 are committed and
CI-green; Tier 3 lands with this change. Each bug fix carries a regression test,
release is gated on the full CI, and the load-bearing paths (bash hook, rc-file
editing, M4b decision/EOF race) now have real tests.

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
- **Recreate recipe (2026-07-15).** The container is launched by a custom command,
  NOT stock `devcontainer up`. State that survives a recreate lives on external
  mounts: pairing/devices on the `remux-state` named volume (`/root/.local/share`)
  and the TLS cert on the `~/.remux-certs` bind (`/certs`, ro). The repo bind
  carries the binary + PWA + persisted Claude config. To recreate:
  ```sh
  docker rm -f remux-mobile
  docker run -d --name remux-mobile --restart unless-stopped -p 7777:7777 \
    -e CLAUDE_CONFIG_DIR=/workspaces/remux/.devcontainer/data/claude \
    -v remux-state:/root/.local/share \
    -v $PWD:/workspaces/remux \
    -v ~/.remux-certs:/certs:ro \
    vsc-remux-f67888504cc9283453a2e1f4b49fb4445a96be485d44774ed0d6a30d70bf5fa4 \
    bash -c 'exec /workspaces/remux/target/debug/remux serve --listen 0.0.0.0:7777 \
      --url https://georges-macbook-air.shrew-fort.ts.net:7777 \
      --allowed-host georges-macbook-air.shrew-fort.ts.net \
      --tls-cert /certs/georges-macbook-air.shrew-fort.ts.net.crt \
      --tls-key /certs/georges-macbook-air.shrew-fort.ts.net.key'
  docker exec remux-mobile npm install -g @anthropic-ai/claude-code   # ephemeral fs → reinstall
  docker exec remux-mobile ln -sf /workspaces/remux/target/debug/remux /usr/local/bin/remux  # PATH symlink (hooks call bare `remux`)
  ```
  `CLAUDE_CONFIG_DIR` points at the bind-mounted repo so Claude Code's login
  persists across recreates; `.devcontainer/data/` is gitignored. Claude Code
  itself lives on the ephemeral fs (npm -g), so reinstall after a recreate — the
  `devcontainer.json` `postCreateCommand` does this on a real `devcontainer up`.

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

## Publishing v0.2.0 (2026-07-15)

v0.1.0's draft was 63 commits stale (predated the whole M4 layer + the three
hardening tiers), so we cut a fresh **v0.2.0** from HEAD. Prep done and verified:

- version bumped (Cargo.toml/lock), README filled (What-you-get list, badges,
  demo placeholder `docs/media/demo-placeholder.svg`, FAQ, Contributing/License),
  approval docs reconciled to the plugin path;
- CI actions flagged for Node-20 bumped to node24 majors (gitleaks v2→v3,
  attest-build-provenance v2→v4, upload-artifact v4→v7, download-artifact
  v4→v8) — re-cut release run is warning-free;
- stale v0.1.0 draft binned;
- **draft v0.2.0 built green** (4 platforms + full CI gate) with the polished
  README bundled; shipped macOS-arm64 artifact smoke-tested (boots, serves 200,
  `/api` 401);
- `jorgeamado/homebrew-remux` tap created + populated (README + `Formula/
  remux.rb`, checksums synced to the re-cut build);
- `main` branch-protected: PR + `test`/`security` checks required, force-push &
  deletion blocked, admins may bypass.

One public flip left, held for the user's explicit go:

1. **Publish the draft v0.2.0 release** → tarballs/.debs become downloadable and
   the tap's formula URLs resolve, making `brew install remux` real.

## Active: M4 — semantic layer (started 2026-07-13)

**M4c VERIFIED ON DEVICE (2026-07-14) — command feed works end to end.**
Shell commands (zsh/bash hooks, opt-in) flow over a separate non-blocking
datagram socket into a bounded, order-tolerant per-session feed; a notable
finish (failure or ≥30s) raises a secrets-safe notification
(exit+duration+session, never the command); the PWA shows a command-feed
panel (aA → Command feed) with exit badges/durations/running-state; the
composer ↑ (and key-row ↑/↓, at a prompt only) recalls the session's real
commands. Shipped and deployed:
- inc1: datagram socket + order-tolerant feed + timer sweeper + zsh hook;
- inc2: precise secrets-safe attention + detector-reset coordination;
- inc3: WS command_feed frame + PWA feed panel;
- inc4: shell-history mirroring + session-scoped composer history;
- `remux setup shell`: guided, shell-aware install (bash + zsh), safe
  ~/.bashrc/~/.zshrc editing (atomic, marker-guarded);
- bash hook (DEBUG trap + PROMPT_COMMAND, verified in bash 5.2 via PTY);
- key-row ↑/↓ → composer recall at a prompt, passthrough for TUIs (keyed off
  the feed's running-state, not the stale pane command).
Every increment Codex-reviewed (real bugs caught each round — event
reordering, feed secrets, stale-topology arrow-hijack, bash $?/BASH_COMMAND
capture) and CI-green. On-device: feed shows commands, key-row ↑ recalls.

**M4d stays gated** (see PLAN §M4d) — build output-streaming only on repeated
real evidence that metadata isn't enough.


**M4b VERIFIED ON DEVICE (2026-07-14): remote agent approvals work.**
Approved a permission card from the iPhone (`remux test-permission` → tap
Approve → the blocked command got `decision: allow`). All four increments
done, Codex-reviewed, CI-green, deployed: (1) registry + ingest held-wait +
`Device.approve`; (2) WS card frames + approve-gated HTTP decide with
write confirmation; (3) PWA Approve/Deny card + SW notification +
`test-permission`; (4) on-device test. Field notes: the phone caches the
PWA JS hard — a full force-quit (not background) is required to load new
code; and grant `approve` to the device whose `last seen` is freshest (the
one the PWA is actually using). Remaining to make it real-world: ship a small
**Claude Code plugin** whose payload is a `PermissionRequest` command hook
calling `remux emit permission`, so an actual Claude Code — not just
`test-permission` — opens cards. Codex-reviewed the packaging (2026-07-15):
a plugin bundling a `hooks/hooks.json` `PermissionRequest` command hook is the
supported, current mechanism (not a workaround); matcher `*`, `timeout: 120`,
card TTL kept under that; on daemon-down/no-approver/expiry the hook must exit
non-zero → Claude falls back to its native prompt, never auto-allow. Anthropic's
own Remote Control is the native alternative for Claude-subscription users;
remux covers self-hosted / API-key setups it doesn't reach. Deferred by
choice — not a near-term TODO.

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
deliberate off-by-default phone setting (aA menu) that a hard reload resets;
composer history is memory-only by design (command lines can hold secrets, so
they never touch localStorage — Tier 1), so a reload intentionally clears it.

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
stdout hygiene).

**M4b increment 2 DONE + Codex-reviewed (2026-07-14).** The phone-facing
daemon surface (PWA UI is increment 3):
- registry change-hint broadcast (reconcile against snapshot) fired on
  open/resolve/remove/sweep and on capability change;
- WS `permission_cards` frame — reconcile-on-subscribe/lag, approve-gated by
  current id, quiet until a card exists;
- approve-gated `GET /api/permissions` (list) + `POST
  /api/permissions/{id}/decide` (canonical decision op);
- **write confirmation**: `resolve` → `(Card, Receiver)`, the held-wait
  fires it only after a successful socket write, the decide handler awaits it
  (8s) → `written:true` or 409 — so the phone is never told "approved" when
  the decision wasn't written to the live hook (not a guaranteed end-to-end ACK);
- generic wake (kind only, no command/source) pushed but not filed in the
  600s attention retention; permission pushes bypass the busy→quiet throttle.
Codex review: 0 blockers, 4 major, 1 minor — all fixed (capability-change
wake, sweep-fires-hint, throttle bypass, confirm-timeout margin, source leak).
Not yet deployed (deploy with increment 3, since web assets are compile-time
embedded). Next: **increment 3** — the PWA Approve/Deny card UI + SW.

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
