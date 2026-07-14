# Shell command feed (M4c) — zsh hooks

Installing these hooks gives your phone a live **command feed** for a session
(what ran, exit code, duration, running state) and precise **failure
notifications** ("a command failed (101) in main") — for plain shell work, not
just agents.

They are **informational only**. Like every shell-hook event in remux, any
process running as you could forge them, so they can never trigger an action —
at worst they add a bogus line to the feed. No command *output* is ever sent;
only metadata. Command lines can contain secrets, so they are sent over the
authenticated connection (never through the push service) and shown only to
your paired devices; they are held in daemon memory only and never written to
disk. The lock-screen notification never contains the command — only the exit
code, duration, and session.

## Opt-in scope

Capture is **off by default** and turns on only where you export
`REMUX_CAPTURE=1`. This is deliberate: the daemon can see every tmux session on
your machine, so capture must be a choice, not automatic. Export it wherever
you want the feed — for every shell (put `export REMUX_CAPTURE=1` at the top of
`~/.zshrc`), or only in specific sessions. Any tmux pane where it's set and
remux is running will report commands.

## Install (zsh)

The easy way — describes what it does, asks first, and writes an idempotent,
clearly-marked block to `~/.zshrc` (it also sets `REMUX_CAPTURE=1` for you):

```
remux setup shell
```

`remux setup shell --uninstall` removes it; `--print` just prints the snippet
(for another shell or a manual install); `--yes` skips the prompt.

Or add it by hand. Every call is fire-and-forget over a local datagram
socket — a stopped daemon is a no-op, and the emits are backgrounded and
disowned so nothing is ever on your prompt's critical path.

```zsh
# remux command feed (M4c) — active only when REMUX_CAPTURE is set
if [[ -n $TMUX_PANE && -n $REMUX_CAPTURE ]] && command -v remux >/dev/null 2>&1; then
  typeset -g  _REMUX_SHELL_ID="$$-${RANDOM}${RANDOM}"   # per interactive shell
  typeset -gi _REMUX_CMD_ID=0

  _remux_preexec() {
    [[ -n $_REMUX_SHELL_ID ]] || return       # unset to disable this shell
    _REMUX_CMD_ID=$(( _REMUX_CMD_ID + 1 ))
    remux emit command-start \
      --shell-id "$_REMUX_SHELL_ID" --command-id "$_REMUX_CMD_ID" \
      --command "$1" --cwd "$PWD" &>/dev/null &!
  }
  _remux_precmd() {
    local ec=$?                                # capture BEFORE anything else
    [[ -n $_REMUX_SHELL_ID ]] || return
    (( _REMUX_CMD_ID > 0 )) || return          # first prompt: nothing ran yet
    remux emit command-end \
      --shell-id "$_REMUX_SHELL_ID" --command-id "$_REMUX_CMD_ID" \
      --exit "$ec" &>/dev/null &!
  }

  autoload -Uz add-zsh-hook
  add-zsh-hook preexec _remux_preexec
  add-zsh-hook precmd  _remux_precmd
fi
```

`remux` must be on your `PATH` inside the session. Re-source `~/.zshrc` or open
a new shell to activate.

## What's supported, and what degrades

The model is **interactive-shell submission**, not process lifecycle:

- A command "finishes" when the shell returns to a prompt. For `long-job &`,
  that's the *submission* returning — not the background job's exit.
- Each interactive shell gets its own `shell_id`, so **nested shells** are
  tracked independently; the inner shell needs its own hooks to report.
- The two events (start, finish) are sent as separate fire-and-forget
  datagrams, so they can arrive out of order under load — the daemon tolerates
  that (it buffers an early finish and won't let a delayed start clobber a
  newer command).
- **ssh to another host**, **fish**, and **bash** are not covered (bash 3.2 on
  stock macOS lacks the timing/hook primitives — a separate, tested bash shim
  is planned). Unhooked shells fall back to remux's built-in busy→quiet
  heuristic; nothing breaks.
- Empty prompts (Enter with no command) are harmless — the daemon
  de-duplicates the repeated finish.

## Disabling capture

- One shell: `unset _REMUX_SHELL_ID` (the hooks then return immediately, no
  `remux` fork).
- Everywhere: `unset REMUX_CAPTURE` (or remove the export) and open new shells.

The feed holds only what was captured while capture was on, in memory, until it
ages out — nothing is persisted.
