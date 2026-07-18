# Design note: pressing TUI elements without taking control

Status: draft — brainstorm synthesis (Claude + Codex), not scheduled.

## The itch

On a phone (or as a casual observer) you often want to **press exactly one
thing** in a TUI: pick "Yes" in a claude-code permission prompt, tap a
lazygit menu row, dismiss a dialog, select an fzf match. Today that costs:

1. **Take control** — which flips `refresh-client -f '!ignore-size'`, making
   the phone a size participant: the whole session reflows to phone
   dimensions and the desktop user's layout is trashed (`src/tmux.rs`
   `promote_client`). Banner churn on every viewer, soft ownership stolen.
2. Possibly **enable direct typing** so the tap even reaches xterm.

That is a heavyweight, disruptive ritual for a single tap. Meanwhile mouse
*clicks* are not forwarded at all — the observer binary-input whitelist
(`wheel_reports_only`, `src/ws.rs`) admits only SGR wheel codes 64/65 — even
though the managed session runs `mouse on` and most modern TUIs (htop,
lazygit, fzf `--mouse`, claude-code) respond to a real SGR click.

## Key insight

remux's "control" bundles two things that don't have to travel together:

- **write permission** (daemon-side gate on binary frames), and
- **size ownership** (tmux `ignore-size` flag).

A tap needs the first for ~50ms and never needs the second. Observers are
not tmux `read-only` clients — they are `ignore-size` clients whose input is
gated purely by the daemon. So the daemon can deliver a click it constructs
itself through the observer's *existing* attached PTY, without touching
client flags: no resize, no banner churn, no ownership change.

tmux then does the coordinate mapping for us: with `ignore-size` each client
is still *rendered* at its own PTY size, so client-relative (col,row) from
the observer's xterm grid is exactly what tmux expects from that client, and
tmux routes the click to the pane under that cell in *that client's* view.
What you see is what you can tap, by construction. (Corollary: do **not**
try to compute pane-relative coords daemon-side from capture geometry — the
attached client already owns that mapping.)

Both brainstorm tracks (Claude, Codex) independently converged on this
mechanism and on keeping the binary path frozen. The distilled principle:

> **controller** = arbitrary byte-stream ownership + sizing participation;
> **press** = one explicit, bounded, server-vetted mutation;
> **semantic action** (`pane_action`) = a currently advertised,
> capability-checked intent.
>
> Three separate authorities. Never quietly redefine observer as controller.

## Recommendation: one-shot **Press mode** (`terminal_press`)

Do **not** widen `wheel_reports_only` (binary from observers = wheel only,
forever — parsing attacker-shaped SGR invites drag/motion/release smuggling
and can't carry intent, acks, or capability checks). Instead add a
control-plane message, sibling of `pane_action`. The server *constructs*
the only permitted byte sequence rather than accepting one.

```jsonc
// client → server
{ "type": "terminal_press", "request_id": "p12",
  "cols": 74, "rows": 28,        // echoed grid — stale-layout guard
  "col": 19, "row": 12 }         // 1-based client cells, left button implied

// server → client
{ "type": "terminal_press_result", "request_id": "p12",
  "status": "delivered" }
// other statuses: forbidden | stale | copy_mode | mouse_off |
//                 outside_pane | rate_limited
```

`delivered` means "written to the tmux client", not "the TUI acted" — the
UI copy should say **"Tap sent"**, never "Activated".

### Server gating (all must hold)

- Paired device with the `press` capability (see Capabilities below).
- Echoed `(cols, rows)` match the connection's settled PTY grid — any
  resize/rotation/reconnect between render and tap → `stale`.
- Coordinates in-bounds and inside a **pane body** — never the status line
  or a border. tmux chrome is programmable (`MouseDown1Pane` can be bound
  to `run-shell`; status-line clicks switch windows for everyone), so
  chrome stays controller/`window_action` territory. Pane rectangles come
  from `list-panes -F '#{pane_left},#{pane_top},...'`; cheapest first cut:
  active pane only, or single-pane/zoomed windows only.
- Target pane not in copy-mode (`#{pane_in_mode}`) — a click there would
  move/exit the selection, not reach the TUI. Client shows "scroll to live
  output before pressing".
- Foreground app has mouse reporting on (`#{mouse_any_flag}`); otherwise
  reply `mouse_off` and the client says "this app doesn't accept clicks —
  take control or use an advertised action". No silent degrade to
  pane-focus, and no automatic fallback to synthesized keys.
- Exactly one unmodified left click; press+release
  (`ESC[<0;c;rM` + `ESC[<0;c;rm`) enqueued adjacently in one buffer to the
  existing `in_tx`. No motion, drag, modifiers, right/double click.
- Per-connection rate cap (reuse the `PANE_ACTION_MIN_INTERVAL` pattern;
  ~burst 2, sustained 4/s) and `request_id` replay rejection.
- Re-check the cheap gates (mouse mode, copy-mode) immediately before
  enqueueing; keep validation-to-write synchronous and short (a TUI can
  exit between validation and delivery).

Never touches `controller`, never calls `promote_client`. The desktop
layout never reflows.

### Client UX (mobile)

Arm-first, one-shot (Codex's shape — safer than always-on tap-to-press):

1. User is `Observer`. A **Press** button sits near "Take control"
   (shown only when the server grants the capability).
2. Tap Press → banner: `Observer · tap one control`; terminal gets a subtle
   outline. The mobile keyboard never appears (xterm textarea stays inert).
3. Next clean tap sends one `terminal_press`. Drag still means scroll and
   cancels the arm. Esc / tapping chrome / layout change / ~5s timeout
   disarm.
4. Brief cell highlight + toast "Tap sent"; auto-disarm after one gesture.

Touch mechanics: reuse the `cellAt` math in `web/src/scroll.ts` (refactor to
a shared helper); while armed, record the pointer-down cell, cancel if
movement exceeds ~half a cell / 8px, send on clean pointer-up, suspend wheel
synthesis for that gesture. `touch-action: manipulation` + `preventDefault`
on the consumed gesture kills iOS double-tap zoom; server-side `request_id`
covers duplicate delivery.

A sticky "multi-press" mode (long-press the Press button) is plausible later
for htop-style poking sessions; defer it — arming is two taps total, already
far cheaper than the take-control ritual.

### Capabilities

Any paired device can already `take_control` and type anything, so press
adds **no new ultimate authority** — but it removes friction *and* the
visible takeover signal the desktop user gets today. That is a real semantic
change, so model it explicitly now:

- `observe` — see output, scroll, capture.
- `press` — one-shot vetted mouse gestures + low-risk semantic actions.
- `control` — arbitrary terminal input + sizing participation.
- `approve` — destructive / command-execution semantic actions (exists).

Existing paired devices inherit `control + press`; future observe-only
shares must not inherit `press`. Surface capabilities in the `status` frame
so the client knows whether to render the Press button. Optional later:
attention/command-feed entry "iPhone pressed pane %3" for auditability —
structured taps can ride that plumbing in a way raw bytes never could.

## Phase 2: semantic targets (extend `pane_action`, keep it preferred)

`pane_action` remains the ceiling — it already has the right properties
(session membership, provenance routing, advertised-token membership,
`approve` gating, foreground re-verification). Two hardening/extension
ideas from the Codex track:

- Views may advertise **grid-relative targets**
  (`{label, region:{col,row,width,height}, action, requires, style}`) in
  their state. Dashboards render them as real DOM buttons; terminal mode
  may highlight the region **only** when the advertised grid exactly
  matches the current pane grid. Advertisement rides the existing trusted
  Unix-socket/`remux stream` provenance — never OSC output, which SSH'd or
  malicious programs could forge into a convincing "Allow" button. (An OSC
  mechanism could someday mark explicitly *untrusted* coordinate links,
  never approve-capable actions.)
- Add `(instance, rev)` echo to `pane_action` and require exact match
  before dispatch — token membership alone lets an action reused across a
  source restart hit newer state.

## Rejected / deferred

- **Raw click passthrough (extend `wheel_reports_only`)** — attacker-shaped
  SGR parsing, chrome targeting, no intent/ack/capability attachment.
  Structured wins on every axis; keep the binary parser untouched.
- **Flash control (promote→click→demote)** — `refresh-client` round-trip
  per tap: resize nudges, desktop reflow, "latest client" churn, banner
  flicker, partial-demotion failure modes, races with a real controller.
  Wrong abstraction: the tmux flag is only about *size*, which a tap never
  wants. `terminal_press` *is* the one-vetted-gesture primitive.
- **Generic one-shot keys (`pane_key`: Enter/y/n/…)** — appeared in the
  Claude draft; Codex's rebuttal kills it: a synthesized Enter can execute
  a command already staged at a shell prompt, and `y`/`n` can land in
  whatever app wins a transition race. A coordinate click is meaningfully
  safer (at a plain shell it hits tmux mouse handling, not buffered text).
  Keys stay controller input or foreground-revalidated adapter actions.
- **Heuristic tap-targets from capture** (detect `[Yes]`, `❯` rows,
  `(q)uit` hints in captured text) — fragile TUI-OCR, stale between capture
  and tap, and unsafe as an *authority* for synthesizing input. Acceptable
  much later as advisory highlighting only; the mouse-off-app gap is better
  served by Phase 2 semantic targets.

## Security summary

A malicious press-capable observer can repeatedly activate visible
controls, dismiss warnings, race the screen, and poke custom tmux mouse
bindings inside pane bodies. Mitigations: capability gate, one-button
canonicalization, geometry echo (`stale`), chrome/copy-mode/mouse-mode
guards, rate cap + replay rejection, no attacker-controlled literals ever
reaching the PTY (coords are reformatted integers), optional interaction
notifications. Because tmux bindings are programmable, treat `press` as
**mutating authority narrower than control** — never as "safe observation".

## Open questions

- MVP pane scope: active pane only vs any pane body vs single-pane/zoomed
  windows only (border-adjacent off-by-ones need tmux-version tests).
- Is a `press_context` opaque token (pushed on every layout/geometry
  change) worth it over the simpler `(cols,rows)` echo? Start with the
  echo; add the token if stale-tap reports show up.
- Should delivery emit an attention/command-feed event by default, or only
  when another device holds control?
