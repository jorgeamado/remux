use anyhow::{Context, Result};
use clap::Parser;
use remux::{auth, host_of_url, server, tmux, App, Args};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "remux=info,warn".into()),
        )
        .init();

    let args = Args::parse();

    let state_dir = dirs::data_dir().context("no data dir")?.join("remux");
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

    if !args.no_pair {
        let token = auth.new_pairing_token();
        print_pairing(&public_url, &token);
    }

    if args.tls_cert.is_none() && !args.listen.ip().is_loopback() {
        tracing::warn!(
            "listening on a non-loopback address WITHOUT TLS — use `tailscale cert` \
             and pass --tls-cert/--tls-key (see README)"
        );
    }

    let app = Arc::new(App {
        allowed_hosts,
        auth,
        args,
        attention: tokio::sync::broadcast::channel(16).0,
    });

    remux::attention::spawn(app.clone());
    server::run(app).await
}

fn print_pairing(public_url: &str, token: &str) {
    let pair_url = format!("{public_url}/#pair={token}");
    println!("\nPair a device (token valid 10 minutes, single use):\n");
    println!("  {pair_url}\n");
    if let Ok(code) = qrcode::QrCode::new(pair_url.as_bytes()) {
        let s = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build();
        println!("{s}\n");
    }
}
