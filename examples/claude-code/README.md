# Claude Code → remux: phone approvals + attention

Approve or deny Claude Code's tool use **from your phone**, and get a status
badge on the pane when Claude is waiting or blocked — all over your existing
remux/Tailscale link.

Three integrations:

| Hook(s) | Fires when | Effect on the phone |
| --- | --- | --- |
| `PreToolUse` → **`remux-approve`** | Claude is about to use a matched tool | An **Approve / Deny** card (M4b); Claude blocks until you decide |
| `Notification` → `remux emit needs-input` | Claude shows a prompt or goes idle | A **"waiting for input"** status chip/badge |
| lifecycle hooks → **`remux-agent-state`** | session/prompt/tool/idle transitions | A live **`claude.v1` dashboard** (Working / Idle / Awaiting approval) — toggle **Dashboard** on a Claude pane |

`remux-agent-state` sends only coarse status (tool NAMES, never inputs); the
command/summary for an approval stays in the approve-only permission card, which
the dashboard joins by id.

Requires remux running and Claude Code inside a remux-served tmux pane (the hooks
inherit `$TMUX_PANE`).

## Install

1. Put `remux-approve` on your `PATH` (or reference it by absolute path below).
   It only needs `remux` on `PATH` too (or set `REMUX_BIN`).

2. Add to `~/.claude/settings.json` (or a project's `.claude/settings.json`):

Both scripts (`remux-approve`, `remux-agent-state`) must be on `PATH` (or use
absolute paths). `remux-agent-state` needs `jq`.

```json
{
  "hooks": {
    "SessionStart":     [{ "hooks": [{ "type": "command", "command": "remux-agent-state session-start" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "remux-agent-state prompt-submitted" }] }],
    "PreToolUse": [
      { "hooks": [{ "type": "command", "command": "remux-agent-state operation-started" }] },
      { "matcher": "Bash|Edit|Write|MultiEdit|NotebookEdit",
        "hooks": [{ "type": "command", "command": "remux-approve" }] }
    ],
    "PostToolUse":        [{ "hooks": [{ "type": "command", "command": "remux-agent-state operation-ended" }] }],
    "PostToolUseFailure": [{ "hooks": [{ "type": "command", "command": "remux-agent-state operation-ended" }] }],
    "Stop":         [{ "hooks": [{ "type": "command", "command": "remux-agent-state idle" }] }],
    "StopFailure":  [{ "hooks": [{ "type": "command", "command": "remux-agent-state idle" }] }],
    "SessionEnd":   [{ "hooks": [{ "type": "command", "command": "remux-agent-state session-ended" }] }],
    "PreCompact":   [{ "hooks": [{ "type": "command", "command": "remux-agent-state touch" }] }],
    "Notification": [{ "hooks": [{ "type": "command", "command": "remux emit needs-input --source claude-code --message 'Claude needs you'" }] }]
  }
}
```

- The matcherless `PreToolUse` `operation-started` fires for **every** tool (so
  the dashboard shows what Claude is doing, incl. reads); it runs before
  `remux-approve` so the pending approval correlates to the active operation.
- Scope the `remux-approve` `matcher` to the tools you want phone approval for.
  Routing read-only tools (`Read`/`Grep`/`Glob`) through the phone would be
  tedious; use `"*"` to route everything.

## How approval works

- `PreToolUse` runs `remux-approve`, which pipes Claude Code's payload into
  `remux emit permission --source claude-code`. `remux emit` reads `tool_name`
  and `tool_input` directly (the field names match), opens an Approve/Deny card
  on approve-capable devices, and **blocks** until one decides.
- The card shows a one-line summary of the tool input (command / file / URL).
  If the input is too long to show in full it is marked truncated, and the phone
  can only **Deny** — a hidden suffix could be destructive. The host, which sees
  the whole command, can still approve there.
- `remux-approve` maps the neutral `allow` / `deny` into Claude Code's
  `PreToolUse` `permissionDecision` (`allow` bypasses the normal prompt, `deny`
  blocks the tool).

## Fail-safe (never silently allow or block)

On **any** failure — daemon down, no paired/approve-capable device, the request
expired, or Claude isn't in a remux pane — `remux-approve` returns
`permissionDecision: "ask"`, so Claude Code falls back to its own prompt on the
host. remux never fabricates a decision.

## Try it without Claude Code

The whole approval loop is `remux emit permission`, so you can exercise it from
any pane in your remux session (it needs `$TMUX_PANE`):

```sh
echo '{"tool_name":"Bash","tool_input":{"command":"rm -rf /tmp/demo"}}' \
  | remux emit permission --source claude-code
```

It blocks; your phone shows the card (and the pane tab gets the ⌘ badge). Tap
Approve/Deny and the command prints `allow` / `deny`.
