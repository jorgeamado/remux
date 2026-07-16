use anyhow::{Context, Result};
use clap::Parser;
use remux::{
    admin, attention, auth, host_of_url, ingest, push, server, shell, tmux, topology, App, Cli,
    Cmd, DevicesCmd, EmitCmd, SetupCmd,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    remux::init_crypto();
    // Logs go to stderr, unconditionally. `remux emit permission` prints
    // Claude Code's decision JSON to stdout and the hook parses it, so a stray
    // tracing line on stdout would corrupt the contract.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "remux=info,warn".into()),
        )
        .init();

    // Honor $XDG_DATA_HOME on every platform, not just Linux: the tests (and
    // any multi-daemon setup) rely on it for state isolation, and on macOS
    // `dirs::data_dir()` ignores it — which made an isolated test daemon trip
    // over the real daemon's single-instance guard.
    let state_dir = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(dirs::data_dir)
        .context("no data dir")?
        .join("remux");

    let args = match Cli::parse().cmd {
        Cmd::Serve(args) => args,
        Cmd::Pair => {
            let url = admin::request_pairing(&state_dir)?;
            print_pairing(&url);
            return Ok(());
        }
        Cmd::Devices { cmd } => {
            match cmd {
                DevicesCmd::List => {
                    let v = admin::request(&state_dir, serde_json::json!({"cmd": "devices"}))?;
                    let empty = vec![];
                    let devices = v["devices"].as_array().unwrap_or(&empty);
                    if devices.is_empty() {
                        println!("no paired devices");
                    }
                    for d in devices {
                        let approve = if d["approve"].as_bool().unwrap_or(false) {
                            "  [approve]"
                        } else {
                            ""
                        };
                        println!(
                            "{}  {:<24} paired {}  last seen {}{}",
                            d["id"].as_str().unwrap_or("?"),
                            d["name"].as_str().unwrap_or("?"),
                            fmt_unix(d["created_unix"].as_u64()),
                            fmt_unix(d["last_seen_unix"].as_u64()),
                            approve,
                        );
                    }
                }
                DevicesCmd::Revoke { id } => {
                    let v =
                        admin::request(&state_dir, serde_json::json!({"cmd": "revoke", "id": id}))?;
                    println!("revoked");
                    // The token is dead regardless, but a push-prune persistence
                    // failure must not be hidden from the operator.
                    if let Some(w) = v["warning"].as_str() {
                        eprintln!("warning: {w}");
                    }
                }
                DevicesCmd::Rename { id, name } => {
                    admin::request(
                        &state_dir,
                        serde_json::json!({"cmd": "rename", "id": id, "name": name}),
                    )?;
                    println!("renamed");
                }
                DevicesCmd::GrantApprove { id } => {
                    let v = admin::request(
                        &state_dir,
                        serde_json::json!({"cmd": "set_approve", "id": id, "grant": true}),
                    )?;
                    if v["changed"] == serde_json::json!(true) {
                        println!("granted approve to {id}");
                    } else {
                        println!("{id} already had approve");
                    }
                }
                DevicesCmd::RevokeApprove { id } => {
                    let v = admin::request(
                        &state_dir,
                        serde_json::json!({"cmd": "set_approve", "id": id, "grant": false}),
                    )?;
                    if v["changed"] == serde_json::json!(true) {
                        println!("revoked approve from {id}");
                    } else {
                        println!("{id} did not have approve");
                    }
                }
            }
            return Ok(());
        }
        Cmd::TestAttention { delay, message } => {
            let pane = std::env::var("TMUX_PANE")
                .context("run this inside the remux-served tmux session")?;
            println!("Lock your phone now — firing a test notification in {delay}s…");
            let mut left = delay;
            while left > 0 {
                let step = left.min(5);
                tokio::time::sleep(std::time::Duration::from_secs(step)).await;
                left -= step;
                if left > 0 {
                    println!("  {left}s…");
                }
            }
            let v = ingest::request(
                &state_dir,
                serde_json::json!({
                    "v": 1, "kind": "agent_needs_input",
                    "pane": pane, "source": "test", "message": message,
                }),
            )?;
            println!(
                "fired: session {} — check the lock screen (arrives within ~30s; \
                 no notification means a client was still active on the session)",
                v["session"].as_str().unwrap_or("?")
            );
            return Ok(());
        }
        Cmd::TestPermission { tool, summary } => {
            let pane = std::env::var("TMUX_PANE")
                .context("run this inside the remux-served tmux session")?;
            println!(
                "Opening a permission card for this session — Approve or Deny it \
                 on your phone (expires in ~100s)…"
            );
            let v = ingest::request_wait(
                &state_dir,
                serde_json::json!({
                    "v": 1, "kind": "agent_permission", "pane": pane,
                    "source": "test", "tool": tool, "summary": summary,
                }),
            )?;
            match v["decision"]
                .as_str()
                .filter(|_| v["ok"] == serde_json::json!(true))
            {
                Some(d) => {
                    println!("decision: {d}");
                    return Ok(());
                }
                // Report failure through the error path (stderr, non-zero) so a
                // smoke test doesn't read "no decision" as success.
                _ => anyhow::bail!(
                    "no decision ({}) — did you approve in time, and is this device \
                     granted approve? (remux devices grant-approve <id>)",
                    v["error"].as_str().unwrap_or("unknown")
                ),
            }
        }
        Cmd::Setup { cmd } => return setup_shell(cmd),
        Cmd::Emit { cmd } => match cmd {
            EmitCmd::NeedsInput {
                pane,
                source,
                message,
            } => {
                let pane = pane
                    .or_else(|| std::env::var("TMUX_PANE").ok())
                    .context("no --pane and no $TMUX_PANE in the environment")?;
                let v = ingest::request(
                    &state_dir,
                    serde_json::json!({
                        "v": 1, "kind": "agent_needs_input",
                        "pane": pane, "source": source, "message": message,
                    }),
                )?;
                println!(
                    "ok: event accepted for session {}",
                    v["session"].as_str().unwrap_or("?")
                );
                return Ok(());
            }
            EmitCmd::Permission {
                pane,
                source,
                wait: _,
            } => return emit_permission(&state_dir, pane, source),
            EmitCmd::CommandStart {
                pane,
                shell_id,
                command_id,
                command,
                cwd,
            } => {
                // Best-effort, non-blocking: resolve the pane, fire, exit. A
                // missing pane just means "not in a remux session" → no-op.
                if let Some(pane) = pane.or_else(|| std::env::var("TMUX_PANE").ok()) {
                    // Cap here, before serializing: the datagram must stay well
                    // under the receive buffer or an over-long command's line
                    // would be truncated on the wire and silently dropped.
                    shell::emit(
                        &state_dir,
                        &serde_json::json!({
                            "v": 1, "kind": "command_started", "pane": pane,
                            "source": "shell", "shell_id": shell_id,
                            "command_id": command_id,
                            "command": truncate_bytes(&command, 512),
                            "cwd": truncate_bytes(&cwd, 256),
                        }),
                    );
                }
                return Ok(());
            }
            EmitCmd::CommandEnd {
                shell_id,
                command_id,
                exit,
            } => {
                shell::emit(
                    &state_dir,
                    &serde_json::json!({
                        "v": 1, "kind": "command_finished", "source": "shell",
                        "shell_id": shell_id, "command_id": command_id, "exit": exit,
                    }),
                );
                return Ok(());
            }
        },
    };

    std::fs::create_dir_all(&state_dir)?;
    let auth = auth::Auth::load(state_dir.join("devices.json"))?;

    let mut allowed_hosts = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        args.listen.ip().to_string(),
    ];
    allowed_hosts.extend(args.allowed_hosts.iter().cloned());
    if let Some(url) = &args.url {
        if let Some(host) = host_of_url(url) {
            allowed_hosts.push(host);
        }
    }
    allowed_hosts.sort();
    allowed_hosts.dedup();

    // Fail fast on a malformed --allowed-client-origin: a typo silently
    // ignored would read as "multi-machine is broken" much later, on the phone.
    let mut allowed_client_origins = Vec::new();
    for raw in &args.allowed_client_origins {
        let origin = remux::normalize_origin(raw).with_context(|| {
            format!("--allowed-client-origin {raw:?} is not a valid http(s) origin")
        })?;
        allowed_client_origins.push(origin);
    }
    allowed_client_origins.sort();
    allowed_client_origins.dedup();

    let machine_id = remux::machine_id(&state_dir)?;
    let machine_name = args
        .machine_name
        .clone()
        .unwrap_or_else(remux::default_machine_name);

    tmux::ensure_session(&args.session)?;
    tracing::info!(session = %args.session, "tmux session ready");

    let scheme = if args.tls_cert.is_some() {
        "https"
    } else {
        "http"
    };
    let public_url = args
        .url
        .clone()
        .unwrap_or_else(|| format!("{scheme}://{}", args.listen));

    if args.tls_cert.is_none() && !args.listen.ip().is_loopback() {
        tracing::warn!(
            "listening on a non-loopback address WITHOUT TLS — use `tailscale cert` \
             and pass --tls-cert/--tls-key (see README)"
        );
    }
    warn_if_cert_stale(args.tls_cert.as_deref());

    let app = Arc::new(App {
        allowed_hosts,
        allowed_client_origins,
        machine_id,
        machine_name,
        auth,
        args,
        attention: tokio::sync::broadcast::channel(16).0,
        public_url,
        push: push::Push::load(&state_dir)?,
        connections: Default::default(),
        pending_attention: Default::default(),
        revoked: tokio::sync::broadcast::channel(16).0,
        topology: tokio::sync::watch::channel(std::sync::Arc::new(Vec::new())).0,
        perms: Default::default(),
        feed: Default::default(),
        detector_reset: tokio::sync::broadcast::channel(16).0,
    });

    if !app.args.no_pair {
        let token = app.auth.new_pairing_token();
        print_pairing(&format!("{}/#pair={token}", app.public_url));
    }

    admin::spawn(app.clone(), &state_dir)?;
    attention::spawn(app.clone());
    push::spawn_dispatcher(app.clone());
    topology::spawn(app.clone());
    // Ingest last: its acks promise the attention pipeline saw the event, so
    // the dispatcher must be subscribed (and topology publishing) first.
    ingest::spawn(app.clone(), &state_dir)?;
    // Shell command feed (M4c): its own datagram socket + a sweeper task.
    shell::spawn(app.clone(), &state_dir)?;
    server::run(app).await
}

const SHELL_HOOK_BEGIN: &str = "# >>> remux command feed >>>";
const SHELL_HOOK_END: &str = "# <<< remux command feed <<<";

/// The zsh block. Fire-and-forget (backgrounded and disowned), guarded to only
/// run inside a remux tmux, and self-documenting so a reader of their rc file
/// knows what it is and how to remove it.
fn zsh_hook_block() -> String {
    format!(
        "{SHELL_HOOK_BEGIN}
# Reports each command's start/finish to remux for a phone command feed +
# precise failure notifications (M4c). Informational only; command lines go to
# your paired devices over the authed connection (never the lock screen / push)
# and are kept in daemon memory only. Remove with: remux setup shell --uninstall
export REMUX_CAPTURE=1
if [[ -n $TMUX_PANE && -n $REMUX_CAPTURE ]] && command -v remux >/dev/null 2>&1; then
  typeset -g  _REMUX_SHELL_ID=\"$$-${{RANDOM}}${{RANDOM}}\"
  typeset -gi _REMUX_CMD_ID=0
  _remux_preexec() {{
    [[ -n $_REMUX_SHELL_ID ]] || return
    _REMUX_CMD_ID=$(( _REMUX_CMD_ID + 1 ))
    remux emit command-start --shell-id \"$_REMUX_SHELL_ID\" --command-id \"$_REMUX_CMD_ID\" --command \"$1\" --cwd \"$PWD\" &>/dev/null &!
  }}
  _remux_precmd() {{
    local ec=$?
    [[ -n $_REMUX_SHELL_ID ]] || return
    (( _REMUX_CMD_ID > 0 )) || return
    remux emit command-end --shell-id \"$_REMUX_SHELL_ID\" --command-id \"$_REMUX_CMD_ID\" --exit \"$ec\" &>/dev/null &!
  }}
  autoload -Uz add-zsh-hook
  add-zsh-hook preexec _remux_preexec
  add-zsh-hook precmd  _remux_precmd
fi
{SHELL_HOOK_END}"
    )
}

/// The bash block. bash has no native preexec/precmd, so this uses a DEBUG trap
/// (before each command) + PROMPT_COMMAND (after). Crucially the trap action
/// captures `$?` and `$BASH_COMMAND` *at fire time* and passes them as args —
/// reading `$BASH_COMMAND` inside the trap body is unreliable (the trap's own
/// commands overwrite it), and reading `$?` in PROMPT_COMMAND would see the
/// trap's exit, not the user's. The exit is stashed in a global for
/// PROMPT_COMMAND, avoiding the `extdebug` pitfall of restoring `$?` via the
/// trap's return value. Emits run in a backgrounded subshell so no job-control
/// noise reaches the prompt.
fn bash_hook_block() -> String {
    format!(
        "{SHELL_HOOK_BEGIN}
# Reports each command's start/finish to remux for a phone command feed +
# precise failure notifications (M4c). Informational only; command lines go to
# your paired devices over the authed connection (never the lock screen / push)
# and are kept in daemon memory only. Remove with: remux setup shell --uninstall
export REMUX_CAPTURE=1
if [[ -n $TMUX_PANE && -n $REMUX_CAPTURE && -n $BASH_VERSION ]] && command -v remux >/dev/null 2>&1; then
  _REMUX_SHELL_ID=\"$$-${{RANDOM}}${{RANDOM}}\"
  _REMUX_CMD_ID=0
  _remux_active=\"\"
  _remux_got=\"\"
  _remux_ready=\"\"
  _remux_ret=0
  _remux_pre() {{
    # $1=$? and $2=$BASH_COMMAND, both captured in the trap action at FIRE time
    # (reading them in the body is unreliable). Record the finished command's
    # exit exactly once — the first DEBUG after it — so PROMPT_COMMAND's own
    # commands can't overwrite it.
    if [[ -n $_remux_active && -z $_remux_got ]]; then _remux_ret=$1; _remux_got=1; fi
    # Only the first command after a prompt counts. `_remux_ready` is armed by
    # precmd, so startup and PROMPT_COMMAND's own commands are ignored.
    [[ -z $_remux_ready ]] && return
    [[ -n ${{COMP_LINE-}} ]] && return          # tab completion, not a command
    case \"$2\" in _remux_precmd*) return ;; esac
    _remux_ready=\"\"
    _remux_active=1
    _remux_got=\"\"
    _REMUX_CMD_ID=$(( _REMUX_CMD_ID + 1 ))
    ( remux emit command-start --shell-id \"$_REMUX_SHELL_ID\" --command-id \"$_REMUX_CMD_ID\" --command \"$2\" --cwd \"$PWD\" >/dev/null 2>&1 & )
  }}
  _remux_precmd() {{
    if [[ -n $_remux_active ]]; then
      ( remux emit command-end --shell-id \"$_REMUX_SHELL_ID\" --command-id \"$_REMUX_CMD_ID\" --exit \"$_remux_ret\" >/dev/null 2>&1 & )
      _remux_active=\"\"
    fi
    _remux_ready=1   # arm for the next user command
  }}
  # Append: any existing PROMPT_COMMAND runs first (sees the real $?, and its
  # commands are ignored by the once-per-prompt guard); we finalize last.
  PROMPT_COMMAND=\"${{PROMPT_COMMAND:+$PROMPT_COMMAND;}}_remux_precmd\"
  # Never clobber an existing DEBUG trap (bash-preexec, debuggers, editor
  # integrations); if one is set, leave the feed off rather than break it.
  if [[ -z $(trap -p DEBUG) ]]; then
    trap '_remux_pre \"$?\" \"$BASH_COMMAND\"' DEBUG
  else
    echo \"remux: existing DEBUG trap detected — command feed left off (unset it to enable)\" >&2
  fi
fi
{SHELL_HOOK_END}"
    )
}

fn zsh_hook_why() -> &'static str {
    "\
remux command feed — what this installs and why

A shell hook that tells remux when each command starts and finishes, giving you:
  • precise lock-screen notifications — \"cargo test failed (1) after 6m\"
    instead of the vague \"a session went quiet\";
  • a command feed on your phone (aA -> Command feed): what ran, exit, duration;
  • up-arrow recall in the composer of the session's real commands (including
    ones you ran at your Mac), editable.

Informational and opt-in. Command lines can hold secrets, so they go only to
your paired devices over the authenticated connection — never the lock screen,
never Web Push — and live in daemon memory only, never on disk."
}

/// Resolve which shell to set up: an explicit `--shell`, else `$SHELL`.
fn resolve_shell(flag: Option<String>) -> Result<&'static str> {
    if let Some(s) = flag {
        // clap already restricts to bash|zsh.
        return Ok(if s == "zsh" { "zsh" } else { "bash" });
    }
    let sh = std::env::var("SHELL").unwrap_or_default();
    match sh.rsplit('/').next().unwrap_or("") {
        "zsh" => Ok("zsh"),
        "bash" => Ok("bash"),
        other => anyhow::bail!(
            "couldn't detect your shell from $SHELL ({other:?}) — pass --shell bash|zsh"
        ),
    }
}

fn rc_file_for(shell: &str) -> &'static str {
    if shell == "zsh" {
        ".zshrc"
    } else {
        ".bashrc"
    }
}

fn hook_block_for(shell: &str) -> String {
    if shell == "zsh" {
        zsh_hook_block()
    } else {
        bash_hook_block()
    }
}

/// `remux setup shell` — describe, confirm, and install/remove the command-feed
/// hook for the user's shell (bash or zsh). Only edits the rc file; does not
/// need the daemon running.
fn setup_shell(cmd: SetupCmd) -> Result<()> {
    use std::io::Write;
    let SetupCmd::Shell {
        shell,
        yes,
        uninstall,
        print,
    } = cmd;
    let shell = resolve_shell(shell)?;
    let block = hook_block_for(shell);
    if print {
        println!("{block}");
        return Ok(());
    }
    let rc = dirs::home_dir()
        .context("no home directory")?
        .join(rc_file_for(shell));
    // A read error other than "missing" must NEVER lead to overwriting the
    // file (a non-UTF-8 or transiently-unreadable rc would otherwise be
    // clobbered with just our block).
    let existing = match std::fs::read(&rc) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|_| {
            anyhow::anyhow!(
                "{} is not UTF-8 — refusing to edit it. Use `remux setup shell --print` \
                 and add the block by hand.",
                rc.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", rc.display()));
        }
    };
    let installed = existing.contains(SHELL_HOOK_BEGIN);

    if uninstall {
        // Attempt removal if EITHER marker is present — an orphan END (no BEGIN)
        // is malformed and must be reported as such, not as "nothing to do".
        if !installed && !existing.contains(SHELL_HOOK_END) {
            println!("No remux hook found in {}.", rc.display());
            return Ok(());
        }
        let stripped = strip_all_hook_blocks(&existing).with_context(|| {
            format!(
                "the remux hook block in {} is malformed (unpaired markers) — \
                 refusing to edit it; remove it by hand",
                rc.display()
            )
        })?;
        atomic_write_rc(&rc, &stripped)?;
        println!(
            "Removed the remux command-feed hook from {}. Open a new shell to apply.",
            rc.display()
        );
        return Ok(());
    }

    if installed {
        println!(
            "The remux command-feed hook is already installed in {}.",
            rc.display()
        );
        return Ok(());
    }

    println!("{}", zsh_hook_why());
    if !yes {
        print!(
            "\nAdd it to {} (detected shell: {shell})? [y/N] ",
            rc.display()
        );
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Skipped — nothing was changed. Run this again anytime, or see docs/shell-hooks.md.");
            return Ok(());
        }
    }

    let content = rc_with_block_appended(&existing, &block);
    atomic_write_rc(&rc, &content)?;
    println!(
        "Installed into {}. Open a new shell (a new tmux window/pane, or `exec {shell}`) \
         inside your remux tmux, then check aA -> Command feed on your phone.",
        rc.display()
    );
    Ok(())
}

/// Write rc contents durably: through a temp file in the same directory + an
/// atomic rename, so a crash or full disk can't leave a half-written rc. A
/// symlinked rc (dotfile managers) is followed so we edit the real file rather
/// than replacing the link; the original mode is preserved.
fn atomic_write_rc(path: &std::path::Path, contents: &str) -> Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().context("rc path has no parent directory")?;
    let fname = target.file_name().context("rc path has no file name")?;
    let tmp = dir.join(format!(".{}.remux-tmp", fname.to_string_lossy()));
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(&target) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &tmp,
            std::fs::Permissions::from_mode(meta.permissions().mode()),
        );
    }
    std::fs::rename(&tmp, &target).with_context(|| format!("replacing {}", target.display()))?;
    Ok(())
}

/// Remove the marked hook block, plus the single blank separator line we insert
/// before it, preserving the rest of the file **byte for byte** (line endings
/// included — no CRLF→LF normalization). Returns `None` if the markers are
/// missing or unpaired, so the caller can refuse rather than delete too much.
/// Compose rc contents with the hook block appended: ensure a trailing newline,
/// a blank separator, then the block + a final newline. Single source of truth
/// for how install writes — `setup_shell` and the tests both call it, so they
/// can't drift.
fn rc_with_block_appended(existing: &str, block: &str) -> String {
    let mut content = existing.to_string();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push('\n');
    content.push_str(block);
    content.push('\n');
    content
}

/// Strip EVERY marked hook block, not just the first. Normally there's exactly
/// one (the install guard refuses to add a second), but a hand-edited rc could
/// hold more — uninstall must not leave a still-active block behind. Returns
/// `None` if ANY marker is unpaired, so we refuse rather than partially edit.
fn strip_all_hook_blocks(s: &str) -> Option<String> {
    let mut out = s.to_string();
    while out.contains(SHELL_HOOK_BEGIN) || out.contains(SHELL_HOOK_END) {
        out = strip_hook_block(&out)?;
    }
    Some(out)
}

fn strip_hook_block(s: &str) -> Option<String> {
    let begin = s.find(SHELL_HOOK_BEGIN)?;
    let end = s[begin..].find(SHELL_HOOK_END)? + begin;
    // Between the two markers there must be no other marker: a second BEGIN or
    // END inside means malformed/nested blocks. Refuse rather than over-strip
    // the region and silently swallow whatever the user put between them.
    let inner = s.get(begin + SHELL_HOOK_BEGIN.len()..end)?;
    if inner.contains(SHELL_HOOK_BEGIN) || inner.contains(SHELL_HOOK_END) {
        return None;
    }
    // Expand to whole lines: start of BEGIN's line … through END's newline.
    let mut start = s[..begin].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let stop = s[end..].find('\n').map(|i| end + i + 1).unwrap_or(s.len());
    // Consume one blank separator line immediately before the block, if present.
    if start >= 1 {
        let prev_start = s[..start - 1].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if s[prev_start..start].trim().is_empty() {
            start = prev_start;
        }
    }
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..start]);
    out.push_str(&s[stop..]);
    Some(out)
}

/// `remux emit permission` — the blocking Claude Code PermissionRequest hook.
/// Reads the hook's JSON payload from stdin (so there's no fragile shell-arg
/// extraction in the install snippet), blocks on the ingest socket until a
/// device decides or the card expires, then prints *only* Claude Code's exact
/// decision JSON on stdout. Any failure returns an error (→ stderr, non-zero
/// exit, no stdout), which makes Claude Code fall back to its own Mac dialog —
/// never a fabricated allow/deny.
fn emit_permission(
    state_dir: &std::path::Path,
    pane: Option<String>,
    source: String,
) -> Result<()> {
    use std::io::Read;
    let pane = pane
        .or_else(|| std::env::var("TMUX_PANE").ok())
        .context("no --pane and no $TMUX_PANE in the environment")?;
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("reading the hook payload from stdin")?;
    let payload: serde_json::Value =
        serde_json::from_str(input.trim()).context("hook stdin was not valid JSON")?;
    let tool = payload["tool_name"].as_str().unwrap_or_default();
    if tool.is_empty() {
        anyhow::bail!("hook payload missing tool_name");
    }
    let (summary, truncated) = summarize_tool(&payload["tool_input"]);
    let mut body = serde_json::json!({
        "v": 1, "kind": "agent_permission",
        "pane": pane, "source": source, "tool": tool,
        "summary": summary, "truncated": truncated,
    });
    if let Some(pid) = payload["prompt_id"].as_str() {
        body["prompt_id"] = pid.into();
    }

    let v = ingest::request_wait(state_dir, body)?;
    // Require an explicit success: an inconsistent ack (ok:false with a stray
    // decision field) must never be treated as a decision.
    match v["decision"]
        .as_str()
        .filter(|_| v["ok"] == serde_json::json!(true))
    {
        Some(b @ ("allow" | "deny")) => {
            // The exact shape Claude Code expects (verified in the M4.0 spike).
            let out = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": { "behavior": b }
                }
            });
            println!("{out}");
            Ok(())
        }
        // ok:false / "expired" / anything unexpected → no decision → fall back.
        _ => anyhow::bail!(
            "no remote decision ({}); falling back to the Mac dialog",
            v["error"].as_str().unwrap_or("unknown")
        ),
    }
}

/// Truncate to a byte budget at a char boundary (keeps the shell-event
/// datagram small enough to never be truncated on the wire).
fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Max chars of tool input shown on an approval card. Generous on purpose: the
/// phone user must see the *whole* command they approve — a benign prefix can
/// hide a destructive suffix — so we only clip pathological inputs. Kept under
/// the ingest line/summary caps so a compliant value is never re-cut.
const SUMMARY_MAX_CHARS: usize = 1500;

/// A human summary of a `tool_input` for the card, plus whether it was
/// truncated. The primary key is the security-relevant identity (the Bash
/// `command`, the edited `file_path`, the fetched `url`, …); when none matches
/// we fall back to the whole input object so no field is silently hidden.
/// `truncated` means the phone did NOT see the full input and must refuse a
/// remote Allow. Never shown on the lock screen (fetched post-auth); sanitized
/// again daemon-side.
fn summarize_tool(input: &serde_json::Value) -> (String, bool) {
    let clip = |s: &str| -> (String, bool) {
        let out: String = s.chars().take(SUMMARY_MAX_CHARS).collect();
        // Truncated iff `take` actually dropped chars.
        let truncated = out.chars().count() < s.chars().count();
        (out, truncated)
    };
    for key in [
        "command",
        "file_path",
        "path",
        "url",
        "pattern",
        "description",
    ] {
        if let Some(s) = input[key].as_str() {
            if !s.is_empty() {
                return clip(s);
            }
        }
    }
    clip(&input.to_string())
}

/// Let's Encrypt certificates live ~90 days; nudge before it bites. (The
/// file mtime is a proxy — good enough for a warning.)
fn warn_if_cert_stale(cert: Option<&std::path::Path>) {
    let Some(cert) = cert else { return };
    let Ok(meta) = std::fs::metadata(cert) else {
        tracing::warn!(cert = %cert.display(), "TLS certificate file is not readable");
        return;
    };
    if let Ok(modified) = meta.modified() {
        if let Ok(age) = modified.elapsed() {
            let days = age.as_secs() / 86_400;
            if days > 75 {
                tracing::warn!(
                    days,
                    "TLS certificate file is {days} days old — Let's Encrypt certs \
                     expire after ~90; renew with `tailscale cert` (see README)"
                );
            }
        }
    }
}

fn fmt_unix(ts: Option<u64>) -> String {
    match ts {
        None | Some(0) => "never".into(),
        Some(ts) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let ago = now.saturating_sub(ts);
            match ago {
                0..=59 => format!("{ago}s ago"),
                60..=3599 => format!("{}m ago", ago / 60),
                3600..=86399 => format!("{}h ago", ago / 3600),
                _ => format!("{}d ago", ago / 86400),
            }
        }
    }
}

fn print_pairing(pair_url: &str) {
    println!("\nPair a device (link valid 10 minutes, reusable within that window):\n");
    println!("  {pair_url}\n");
    if let Ok(code) = qrcode::QrCode::new(pair_url.as_bytes()) {
        let s = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build();
        println!("{s}\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Install exactly as setup_shell does — via the shared composition helper,
    /// so the test can't drift from production.
    fn install_into(original: &str) -> String {
        rc_with_block_appended(original, &zsh_hook_block())
    }

    #[test]
    fn hook_block_install_uninstall_roundtrips_exactly() {
        // Files that already end in a newline (the normal case) — and empty —
        // round-trip byte-for-byte.
        for original in ["# my rc\nexport A=1\n", "", "\n"] {
            let installed = install_into(original);
            assert!(installed.contains(SHELL_HOOK_BEGIN));
            assert!(installed.contains(SHELL_HOOK_END));
            assert_eq!(
                strip_hook_block(&installed).as_deref(),
                Some(original),
                "round trip failed for {original:?}"
            );
        }
    }

    #[test]
    fn install_normalizes_a_missing_final_newline() {
        // A file lacking a trailing newline gains one (standard for text files,
        // harmless); everything else is preserved. Documented, not exact.
        let installed = install_into("no-final-newline");
        assert_eq!(
            strip_hook_block(&installed).as_deref(),
            Some("no-final-newline\n")
        );
    }

    #[test]
    fn strip_preserves_crlf_and_surrounding_bytes() {
        // A CRLF file with content before and after the block.
        let original = "line1\r\nline2\r\n";
        let installed = install_into(original);
        // The rest of the file must not be normalized to LF.
        assert_eq!(strip_hook_block(&installed).as_deref(), Some(original));
    }

    #[test]
    fn strip_returns_none_when_markers_missing_or_unpaired() {
        assert_eq!(strip_hook_block("export A=1\n"), None); // no markers
                                                            // BEGIN but no END — must refuse (would otherwise delete to EOF).
        let malformed = format!("keep me\n{SHELL_HOOK_BEGIN}\nrogue\n");
        assert_eq!(strip_hook_block(&malformed), None);
    }

    #[test]
    fn both_hooks_carry_the_opt_in_guard_and_emits() {
        for b in [zsh_hook_block(), bash_hook_block()] {
            assert!(b.contains("export REMUX_CAPTURE=1"));
            assert!(b.contains("$TMUX_PANE"));
            assert!(b.contains("$REMUX_CAPTURE"));
            assert!(b.contains("emit command-start"));
            assert!(b.contains("emit command-end"));
            assert!(b.contains(SHELL_HOOK_BEGIN) && b.contains(SHELL_HOOK_END));
        }
    }

    #[test]
    fn bash_hook_composes_and_captures_safely() {
        let b = bash_hook_block();
        // $? and $BASH_COMMAND captured in the trap ACTION (fire time).
        assert!(b.contains(r#"trap '_remux_pre "$?" "$BASH_COMMAND"' DEBUG"#));
        // Exit recorded once, guarded — PROMPT_COMMAND commands can't overwrite.
        assert!(b.contains("-z $_remux_got ]]; then _remux_ret=$1"));
        assert!(b.contains("--exit \"$_remux_ret\""));
        // set -u safe.
        assert!(b.contains("${COMP_LINE-}"));
        // Appends to PROMPT_COMMAND (existing hooks run first, see real $?).
        assert!(b.contains(r#"PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND;}_remux_precmd""#));
        // Does not clobber an existing DEBUG trap.
        assert!(b.contains("if [[ -z $(trap -p DEBUG) ]]; then"));
    }

    #[test]
    fn bash_block_round_trips_through_strip() {
        let original = "# bashrc\nexport A=1\n";
        let installed = format!("{original}\n{}\n", bash_hook_block());
        assert_eq!(strip_hook_block(&installed).as_deref(), Some(original));
    }

    #[test]
    fn resolve_shell_honors_flag() {
        assert_eq!(resolve_shell(Some("bash".into())).unwrap(), "bash");
        assert_eq!(resolve_shell(Some("zsh".into())).unwrap(), "zsh");
    }

    #[test]
    fn strip_all_removes_multiple_blocks_and_refuses_malformed() {
        // Two well-formed blocks (a hand-edited rc) → both removed, exact rest.
        let original = "# rc\nexport A=1\n";
        let two = format!(
            "{original}\n{b}\n\n{b}\n",
            b = zsh_hook_block(),
            original = original
        );
        assert!(two.matches(SHELL_HOOK_BEGIN).count() == 2);
        let stripped = strip_all_hook_blocks(&two).unwrap();
        assert!(!stripped.contains(SHELL_HOOK_BEGIN));
        assert!(!stripped.contains(SHELL_HOOK_END));
        assert_eq!(stripped, original);
        // A second, unpaired BEGIN after a good block → refuse (don't half-edit).
        let malformed = format!("{}\n\nkeep\n{SHELL_HOOK_BEGIN}\nrogue\n", zsh_hook_block());
        assert_eq!(strip_all_hook_blocks(&malformed), None);
        // Nested markers (BEGIN…BEGIN…END) must be refused, never over-stripped.
        let nested =
            format!("a\n{SHELL_HOOK_BEGIN}\nx\n{SHELL_HOOK_BEGIN}\ny\n{SHELL_HOOK_END}\nb\n");
        assert_eq!(strip_hook_block(&nested), None);
        assert_eq!(strip_all_hook_blocks(&nested), None);
        // An orphan END with no BEGIN is malformed, not "nothing here".
        assert_eq!(
            strip_all_hook_blocks(&format!("keep\n{SHELL_HOOK_END}\n")),
            None
        );
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("remux-rc-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn atomic_write_preserves_mode_follows_symlink_and_leaves_no_temp() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("atomic");
        let real = dir.join("zshrc");
        std::fs::write(&real, "export A=1\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        // The dotfile-manager pattern: ~/.zshrc is a symlink to the real file.
        let link = dir.join(".zshrc");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        atomic_write_rc(&link, "export A=1\n# added\n").unwrap();

        // The symlink is intact (we edited the real file, not replaced the link).
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "export A=1\n# added\n"
        );
        // Mode preserved on the real file.
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // No temp file left behind.
        let leftover = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains("remux-tmp"));
        assert!(!leftover, "a .remux-tmp file was left behind");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bash_hook_emits_start_end_with_captured_exit() {
        use std::os::unix::fs::PermissionsExt;
        // Exercise the REAL DEBUG-trap capture path: the trap fires before each
        // top-level command in a script too, so with a fake `remux` on PATH we
        // can drive `true`/`false` and assert the start/end emits and the
        // captured exit codes. We invoke `_remux_precmd` at the prompt points
        // (PROMPT_COMMAND is interactive-only); the append composition itself is
        // covered separately by bash_hook_appends_to_existing_prompt_command.
        // Uses only bash-3.2 constructs, so it runs on macOS bash and CI's 5.x.
        let dir = scratch_dir("bashhook");
        let log = dir.join("emits.log");
        let fake = dir.join("remux");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log:?}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rc = dir.join("bashrc");
        std::fs::write(&rc, bash_hook_block()).unwrap();

        // Two prompt cycles: run `true` (exit 0) then `false` (exit 1). No
        // command after the last precmd (it would fire the trap and pollute).
        let driver = format!(
            "export TMUX_PANE=%1\n\
             export PATH={dir:?}:$PATH\n\
             source {rc:?}\n\
             _remux_precmd\n\
             true\n\
             _remux_precmd\n\
             false\n\
             _remux_precmd\n"
        );
        let status = std::process::Command::new("bash")
            .arg("-c")
            .arg(&driver)
            .status();
        let status = match status {
            Ok(s) => s,
            Err(_) => {
                std::fs::remove_dir_all(&dir).ok();
                return; // no bash available — skip
            }
        };
        assert!(status.success(), "driver bash exited non-zero");

        // The `( remux & )` emits are backgrounded, so poll (best-effort) until
        // all four REQUIRED emits have landed. We assert their presence and
        // correlation by command-id — not an exact line count, since a detached
        // process can't guarantee the absence of a late stray write. cmd 1 =
        // `true` → exit 0; cmd 2 = `false` → exit 1.
        let required: [&[&str]; 4] = [
            &["emit command-start", "--command true", "--command-id 1"],
            &["emit command-end", "--command-id 1", "--exit 0"],
            &["emit command-start", "--command false", "--command-id 2"],
            &["emit command-end", "--command-id 2", "--exit 1"],
        ];
        let present = |lines: &[String], needles: &[&str]| {
            lines.iter().any(|l| needles.iter().all(|n| l.contains(n)))
        };
        let mut lines: Vec<String> = Vec::new();
        for _ in 0..60 {
            if let Ok(s) = std::fs::read_to_string(&log) {
                lines = s.lines().map(str::to_string).collect();
                if required.iter().all(|r| present(&lines, r)) {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::fs::remove_dir_all(&dir).ok();
        for r in required {
            assert!(present(&lines, r), "missing {r:?} in {lines:?}");
        }
    }

    #[test]
    fn bash_hook_appends_to_existing_prompt_command() {
        use std::os::unix::fs::PermissionsExt;
        // The append (not prepend) is load-bearing: an existing PROMPT_COMMAND
        // must run FIRST so it still sees the real $?. Verify the composed value.
        let dir = scratch_dir("bashpc");
        let fake = dir.join("remux");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rc = dir.join("bashrc");
        std::fs::write(&rc, bash_hook_block()).unwrap();

        let driver = format!(
            "export TMUX_PANE=%1\n\
             export PATH={dir:?}:$PATH\n\
             PROMPT_COMMAND='__existing'\n\
             source {rc:?}\n\
             printf '%s' \"$PROMPT_COMMAND\"\n"
        );
        let out = match std::process::Command::new("bash")
            .arg("-c")
            .arg(&driver)
            .output()
        {
            Ok(o) => o,
            Err(_) => {
                std::fs::remove_dir_all(&dir).ok();
                return; // no bash — skip
            }
        };
        std::fs::remove_dir_all(&dir).ok();
        let pc = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            pc.trim(),
            "__existing;_remux_precmd",
            "existing hook must run first, ours appended: {pc:?}"
        );
    }

    #[test]
    fn install_then_uninstall_through_the_filesystem_restores_exactly() {
        let dir = scratch_dir("roundtrip");
        let rc = dir.join("bashrc");
        let original = "# bashrc\nexport EDITOR=vim\n";
        std::fs::write(&rc, original).unwrap();

        // Install via the SAME composition helper setup_shell uses + the real
        // atomic write (so this can't drift from production).
        let existing = std::fs::read_to_string(&rc).unwrap();
        assert!(!existing.contains(SHELL_HOOK_BEGIN), "not installed yet");
        atomic_write_rc(&rc, &rc_with_block_appended(&existing, &bash_hook_block())).unwrap();

        // Idempotency guard now trips (already installed).
        let after = std::fs::read_to_string(&rc).unwrap();
        assert!(after.contains(SHELL_HOOK_BEGIN));

        // Uninstall via strip → the original file is restored byte-for-byte.
        let stripped = strip_all_hook_blocks(&after).unwrap();
        atomic_write_rc(&rc, &stripped).unwrap();
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), original);
        std::fs::remove_dir_all(&dir).ok();
    }
}
