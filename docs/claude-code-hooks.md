# Claude Code → remux notifications

Get a lock-screen notification the moment Claude Code needs your input in
a remux-managed tmux session — instead of waiting for the generic
busy→quiet heuristic to notice the pane went silent.

Requires remux ≥ the M4a ingest socket (`remux emit` exists) and Claude
Code running inside a tmux pane that remux serves.

## Install

Add to `~/.claude/settings.json` (or a project's `.claude/settings.json`):

```json
{
  "hooks": {
    "Notification": [
      {
        "matcher": "permission_prompt",
        "hooks": [
          {
            "type": "command",
            "command": "remux emit needs-input --source claude-code --message 'permission prompt'"
          }
        ]
      },
      {
        "matcher": "idle_prompt",
        "hooks": [
          {
            "type": "command",
            "command": "remux emit needs-input --source claude-code --message 'waiting for input'"
          }
        ]
      }
    ]
  }
}
```

## How it works

- Claude Code's `Notification` hook fires when it shows a permission
  prompt or goes idle waiting for you. It is display-only — it cannot
  affect what Claude does (approvals from the phone are M4b, via the
  `PermissionRequest` hook).
- The hook command inherits `$TMUX_PANE` from the pane's environment;
  `remux emit` sends one JSON line to the daemon's local ingest socket
  (filesystem-authenticated, data-only — an event can raise a
  notification, never act).
- The daemon maps the pane to its session via live topology and raises
  attention through the normal pipeline: push (with the usual
  suppression — no ping if you're actively using that session), in-app
  banner, `/api/attention` deep link.

## Behavior notes

- Outside tmux (no `$TMUX_PANE`), `remux emit` exits non-zero and Claude
  Code just logs the failed hook; nothing breaks.
- If the daemon is down, the hook fails fast (5s deadline) — Claude Code
  is unaffected.
- Panes in windows linked into several sessions are rejected (the daemon
  won't guess which session to notify).
- The busy→quiet heuristic stays active for everything without hooks.
