use anyhow::{Context, Result};
use clap::Parser;
use remux::{admin, attention, auth, host_of_url, push, server, tmux, App, Cli, Cmd, DevicesCmd};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    remux::init_crypto();
    tracing_subscriber::fmt()
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
                        println!(
                            "{}  {:<24} paired {}  last seen {}",
                            d["id"].as_str().unwrap_or("?"),
                            d["name"].as_str().unwrap_or("?"),
                            fmt_unix(d["created_unix"].as_u64()),
                            fmt_unix(d["last_seen_unix"].as_u64()),
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
            }
            return Ok(());
        }
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
    });

    if !app.args.no_pair {
        let token = app.auth.new_pairing_token();
        print_pairing(&format!("{}/#pair={token}", app.public_url));
    }

    admin::spawn(app.clone(), &state_dir)?;
    attention::spawn(app.clone());
    push::spawn_dispatcher(app.clone());
    server::run(app).await
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
