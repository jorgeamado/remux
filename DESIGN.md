# remux — Design

> **Your persistent terminal session, redesigned as a mobile conversation.**

remux gives you your tmux session on your phone. Start work on the Mac (mosh → tmux),
glance at Claude Code from the phone, type a reply, put the phone down, keep typing on
the Mac — nothing restarts, nothing re-attaches.

This document distills the original architecture discussion (see the ChatGPT design
conversation) plus a four-perspective design review (Rust/systems architect, tmux
internals expert, security engineer, frontend/product engineer) into the V1 design
actually being built.

## Core use case

```
Mac    → mosh → tmux client        ┐
                                   ├── same tmux server, session `main`
Phone  → PWA  → remux daemon → tmux client (PTY)
```

One live tmux session, one active controller, fast handoff between devices.

## Architecture principle

> **tmux is the source of truth for terminal state; the daemon is the source of truth
> for product state (devices, auth, control).**

The daemon never emulates a terminal. tmux renders; xterm.js in the browser parses.

## V1 architecture (post-review)

```
┌──────────── Phone / any browser ────────────┐
│ PWA: xterm.js + fit addon                    │
│ key row (Esc/Ctrl/Tab/arrows), take-control  │
└──────────────┬───────────────────────────────┘
               │ WSS (binary frames = terminal bytes,
               │      JSON text frames = control)
┌──────────────▼───────────────────────────────┐
│ remux daemon (Rust, single static binary)    │
│  axum HTTP+WS server, serves embedded PWA    │
│  auth: pairing token → device token          │
│  per-connection: PTY running `tmux attach`   │
└──────────────┬───────────────────────────────┘
               │ PTY (raw bytes, TIOCSWINSZ)
        tmux attach-session -t main
               │
          tmux server ── panes ── shell / Claude Code
```

**Per WebSocket connection, the daemon spawns a real tmux client in a PTY:**

- Observer (default): `tmux attach-session -t main -f read-only,ignore-size`.
  tmux refuses its input and ignores its size. Attach performs a full repaint of the
  current screen (including alternate-screen apps) from tmux's own screen model — this
  *is* the "snapshot on attach", for free, with perfect fidelity.
- Take control: `tmux refresh-client -t <client> -f '!read-only,!ignore-size'` +
  set PTY winsize. With `window-size latest` the pane resizes to the phone.
- Release / phone locks / network dies: the WebSocket dies → daemon kills the attach
  client → the Mac is the remaining sizing client → its dimensions restore instantly.
  Release logic is zero lines of code.
- Typing on the Mac makes the Mac the "latest" client → tmux resizes back
  automatically. Soft ownership, exactly as designed, with no daemon-side lease logic.

**Why raw byte stream, not the snapshot/delta protocol from the original design:**
all four reviewers flagged the row-delta protocol as unimplementable as specified —
it requires the server-side VT parser the design explicitly deleted, and xterm.js's
buffer API is read-only so deltas would have to be re-serialized into synthetic ANSI
anyway (losing modes, breaking scrollback). The raw stream into xterm.js is the proven
ttyd/GoTTY architecture; over Tailscale (LAN-class link) the bandwidth optimization the
delta engine existed for is not a V1 problem. The delta/revision protocol remains the
documented V2 direction if a custom (non-xterm.js) renderer or measured bandwidth ever
demands it.

### Protocol (the whole thing)

- **Binary frames**: terminal bytes, both directions. Server→client always; client→
  server accepted only from the controller.
- **Text frames (JSON)**:
  - client→server: `{"type":"auth","token"}`, `{"type":"resize","cols","rows"}`,
    `{"type":"take_control"}`, `{"type":"release_control"}`, `{"type":"ping"}`
  - server→client: `{"type":"status",...}` (auth result, observer/controller state,
    session name), `{"type":"error","code","message"}`, `{"type":"pong"}`
- No message is processed before `auth` succeeds.

### Security model (per security review)

Threat calibration: this is remote shell access to your machine, self-hosted,
single user, tailnet-only. Realistic threats: other tailnet devices, malicious
websites in your browser (cross-site WebSocket hijacking / DNS rebinding), XSS,
stolen phone, hostile terminal output.

- **Network**: bind only to localhost and/or the Tailscale interface. Never public.
- **TLS**: self-signed certs are a dead end on iOS (no PWA install, WSS refused).
  Production setup uses `tailscale cert` — a real Let's Encrypt certificate for the
  machine's `*.ts.net` MagicDNS name — passed via `--tls-cert/--tls-key`.
  Plain HTTP is allowed for localhost development only.
- **Auth**: `remux serve` prints a single-use, time-limited pairing token as URL + QR.
  The PWA exchanges it (`POST /api/pair`, rate-limited) for a long-lived random device
  token (stored hashed server-side; revocable per device). Every WS connection must
  authenticate before any tmux action. Ed25519 per-request signing from the original
  design was dropped as over-engineered (bespoke crypto risk > threat mitigated);
  connect-time auth over pinned TLS is the sound simple design.
- **Origin/Host validation (mandatory, was missing from the original design)**:
  WS upgrades and API calls are rejected unless the `Host` header and (when present)
  the `Origin` host are in the allowlist. This kills cross-site WebSocket hijacking
  and DNS rebinding from arbitrary websites.
- **Terminal hardening**: OSC 52 clipboard write disabled in xterm.js; no clipboard
  read ever; strict CSP; no inline scripts; terminal content never touches innerHTML.
- **Daemon hardening**: per-user, no root; state files 0600; never logs terminal
  input/output; devices auditable and revocable.

### What tmux owns vs what the daemon owns

- tmux: sessions, panes, PTYs, process lifetime, screen state, scrollback,
  multi-client sizing (`window-size latest`), repaint-on-attach.
- daemon: network exposure, TLS, pairing/auth/devices, observer↔controller flag
  toggling, PTY lifecycle per connection.
- The daemon touches tmux only via: `has-session` / `new-session` / `set-option`
  (managed session only — never global config), `attach-session` (in the PTY),
  `refresh-client`, `list-clients`. All tmux-aware code lives in one module.

### Scrollback

V1 uses tmux copy-mode with `mouse on` (set on the managed session only): xterm.js
forwards wheel/touch scroll as mouse events, tmux scrolls its own history. Server-paged
scrollback returns in V2 alongside a custom renderer.

### Client (PWA)

Vanilla TypeScript + Vite + `@xterm/xterm` + `@xterm/addon-fit`. No framework — two
screens (pair, terminal) and a key row. DOM renderer first. Embedded into the Rust
binary (`rust-embed`), so the daemon is one file.

iOS specifics the review flagged as non-negotiable even for a "minimal" client:
`visualViewport` handling (keyboard overlays the page; shrink terminal + refit +
resize), `autocorrect/autocapitalize/spellcheck` off, sticky Ctrl modifier in the key
row sending raw bytes, `100dvh`, reconnect-on-foreground (Safari kills background
sockets — the server treats socket death as release; no clean-close assumption),
wake lock while visible.

## Decisions log

| Decision | Choice | Origin |
|---|---|---|
| Host daemon language | Rust | original design |
| tmux | mandatory; the only session engine | original design |
| Daemon lifetime | persistent service, survives with no clients | original design |
| Client | PWA-first, xterm.js | original design |
| Ownership | soft (observer/controller, auto-handoff via tmux `latest`) | original design |
| tmux integration | **per-connection PTY attach** (control mode deferred to V2) | review |
| Wire format | **raw terminal bytes** + small JSON control messages | review |
| Snapshot/delta/revision sync engine | **deferred to V2** (custom renderer prerequisite) | review |
| Event bus + state store components | **cut**; tokio ownership + channels | review |
| TLS | `tailscale cert` (ts.net Let's Encrypt), never self-signed | review |
| Auth | pairing QR → bearer device token, auth-before-anything | review (simplified) |
| Origin/Host allowlist | mandatory | review (gap found) |
| OSC 52 clipboard write | disabled | review (gap found) |
| Styled runs / style tables / zstd / CBOR | cut from V1 | review |
| Scrollback | tmux copy-mode + mouse | review |
| Per-device permission tiers (observer/controller/admin) | deferred; V1 = paired device may observe and take control | review |
| Multi-session UI, workspaces, notifications, chat feed, shell integration | deferred (V2+) | original design + review |

## Review findings archive (summary)

1. **Rust architect**: event bus/state store = ceremony, use channels; `%output`
   head-of-line blocking and `send-keys` escaping are control-mode minefields; watch
   channels give "no replay, snapshot on fall-behind" semantics for free; drop
   portable-pty/zstd *(portable-pty later reinstated by the PTY-attach decision)*.
2. **tmux expert**: verified control-mode claims (with 3.2+ version pins) but found
   `window-size latest` may not respond to `send-keys` from a control client (input
   injected by command doesn't count as client activity) and that capture-pane cannot
   recover scroll regions/wrap-pending state → splice glitches. Verdict: PTY-attach
   eliminates both top risks; hybrid with a `read-only,no-output` control client for
   metadata is the V2 path.
3. **Security**: missing Origin/Host validation was the biggest real gap; per-request
   signing over-engineered; `tailscale cert` is the only workable iOS TLS story;
   disable OSC 52 writes; pairing needs rate limiting + single-use TTL tokens.
4. **Frontend/product**: xterm.js cannot be fed cell deltas (read-only buffer);
   raw stream is the architecture xterm.js actually speaks; iOS keyboard/viewport
   work is mandatory V1 scope; everything else (delta sync, control mode, permission
   tiers, style tables) cuttable without breaking the client contract
   ("bytes in, bytes out, resize").

## Roadmap

- **V1 (this build)**: daemon + PWA, pair, attach, live terminal, input, resize,
  take/release control, reconnect, TLS via tailscale cert.
- **V1.x**: attention notification (pane quiet + waiting heuristic), session picker,
  launchd/systemd unit files, device management UI.
- **V2**: control-mode metadata client (panes as cards/tabs), snapshot/delta sync +
  custom renderer, server-paged scrollback, paste confirmation UX.
- **V3**: shell integration (OSC 133 / hooks), semantic feed ("chat mode"),
  Claude Code-aware approval cards, push notifications.
