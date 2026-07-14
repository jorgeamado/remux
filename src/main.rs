use anyhow::{Context, Result};
use clap::Parser;
use remux::{
    admin, attention, auth, host_of_url, ingest, push, server, shell, tmux, topology, App, Cli,
    Cmd, DevicesCmd, EmitCmd,
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

    let state_dir = dirs::data_dir().context("no data dir")?.join("remux");

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
                    admin::request(&state_dir, serde_json::json!({"cmd": "revoke", "id": id}))?;
                    println!("revoked");
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
    let summary = summarize_tool(&payload["tool_input"]);
    let mut body = serde_json::json!({
        "v": 1, "kind": "agent_permission",
        "pane": pane, "source": source, "tool": tool, "summary": summary,
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

/// A one-line human summary of a tool_input for the card. Never shown on the
/// lock screen (fetched post-auth); capped here, sanitized again daemon-side.
fn summarize_tool(input: &serde_json::Value) -> String {
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
                return s.chars().take(200).collect();
            }
        }
    }
    input.to_string().chars().take(200).collect()
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
