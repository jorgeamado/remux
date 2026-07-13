# remux — working status snapshot

> Uncommitted working note (per request). Point-in-time picture of where the
> project is and what's planned. The durable plan is `docs/PLAN.md`.

Repo state: `main` clean, everything pushed to `github.com/jorgeamado/remux`
(public), CI green. HEAD = `a1a9d5a` (M3b window tabs).

## Deployed

- `remux-mobile` Docker container serving the latest build over HTTPS/HTTP2 at
  `https://georges-macbook-air.shrew-fort.ts.net:7777` (Tailscale cert).
- It runs the devcontainer image + the debug binary from the bind-mounted repo;
  `docker restart remux-mobile` after a `cargo build` picks up new code.
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
- **M3b (part 1)** — live **window tabs** from topology (tap to switch),
  session picker fed from topology, `+` menu create-only.

## Waiting on the user (both one-click, public-facing — I can't do these)

1. Publish the draft **v0.1.0** GitHub release (built & verified).
2. Create the `jorgeamado/homebrew-remux` tap repo and drop the release's
   generated `remux.rb` into `Formula/` → makes `brew install` real.

## Planned next

- **M3b (part 2) — polish** (next up):
  - Restore proper **status-bar hiding** (tabs now show window info, so the
    tmux status row is redundant) — done correctly on the clean sizing base,
    not the old pixel hack.
  - Live **breadcrumb**: active window's running command in the header.
- **V2/V3 parked** (see PLAN.md "Parked"): custom renderer + snapshot/delta
  protocol; semantic layer (OSC 133 shell integration, command feed, Claude
  Code approval cards); per-device permission tiers (unlocks invite-from-
  device); hosted apt repo (GPG); built-in ACME (VPN-agnostic TLS — no code
  dependency on Tailscale today, it's only the zero-config cert path).

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
