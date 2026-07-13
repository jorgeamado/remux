pub mod admin;
pub mod attention;
pub mod auth;
pub mod ingest;
pub mod push;
pub mod server;
pub mod tmux;
pub mod topology;
pub mod ws;

use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;

/// remux — your persistent tmux session, on your phone.
#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(clap::Subcommand, Debug)]
pub enum Cmd {
    /// Run the daemon.
    Serve(Args),
    /// Print a fresh pairing link + QR from the running daemon.
    Pair,
    /// Manage paired devices (list, revoke, rename).
    Devices {
        #[command(subcommand)]
        cmd: DevicesCmd,
    },
    /// Report a semantic event to the running daemon (for hook scripts).
    Emit {
        #[command(subcommand)]
        cmd: EmitCmd,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum EmitCmd {
    /// An agent (e.g. Claude Code) is waiting for input in a pane.
    NeedsInput {
        /// tmux pane id (%N). Defaults to $TMUX_PANE.
        #[arg(long)]
        pane: Option<String>,
        /// Producer label, e.g. claude-code.
        #[arg(long, default_value = "unknown")]
        source: String,
        /// Short human-readable detail (sanitized and capped by the daemon).
        #[arg(long)]
        message: Option<String>,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum DevicesCmd {
    /// List paired devices.
    List,
    /// Revoke a device: its token stops working, live connections drop,
    /// push subscriptions are deleted, pending pairing links are cancelled.
    Revoke { id: String },
    /// Rename a device.
    Rename { id: String, name: String },
}

/// Options for `remux serve`.
#[derive(Parser, Debug)]
pub struct Args {
    /// Address to listen on. Bind to your Tailscale IP in production.
    #[arg(long, default_value = "127.0.0.1:7777")]
    pub listen: SocketAddr,

    /// tmux session to attach clients to (created if missing).
    #[arg(long, default_value = "main")]
    pub session: String,

    /// TLS certificate path (PEM). Use `tailscale cert` to obtain one.
    #[arg(long, requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,

    /// TLS private key path (PEM).
    #[arg(long, requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,

    /// Extra hostnames accepted in Host/Origin checks (e.g. your MagicDNS name).
    /// localhost and the listen address are always allowed.
    #[arg(long = "allowed-host")]
    pub allowed_hosts: Vec<String>,

    /// Do not print a pairing token at startup.
    #[arg(long)]
    pub no_pair: bool,

    /// Public URL clients should use (printed in the pairing QR).
    /// Defaults to http(s)://<listen-addr>.
    #[arg(long)]
    pub url: Option<String>,
}

/// An attention-worthy moment in a session. `kind`/`reason`/`source` ride
/// the in-band ws frame and `/api/attention` — never the push payload,
/// which stays empty by design.
#[derive(Clone, Debug, serde::Serialize, PartialEq)]
pub struct Attention {
    pub session: String,
    /// `agent_needs_input` (hook-fed, precise) or `quiet_after_busy`
    /// (the heuristic fallback detector).
    pub kind: String,
    /// Producer-supplied detail ("permission prompt"); sanitized at ingest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Producer label ("claude-code"); hook-fed events only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl Attention {
    /// The heuristic detector's event: no detail beyond the session.
    pub fn quiet(session: String) -> Self {
        Self {
            session,
            kind: "quiet_after_busy".into(),
            reason: None,
            source: None,
        }
    }
}

pub struct App {
    pub args: Args,
    pub auth: auth::Auth,
    pub allowed_hosts: Vec<String>,
    /// Attention events, fanned out to websockets attached to the session
    /// and to the push dispatcher.
    pub attention: tokio::sync::broadcast::Sender<Attention>,
    /// URL clients use to reach the daemon (goes into pairing links).
    pub public_url: String,
    /// Web Push state (VAPID key + subscriptions).
    pub push: push::Push,
    /// Live websocket connections: (device id, session) -> count. Push
    /// delivery skips devices that already get the in-band frame.
    pub connections: std::sync::Mutex<std::collections::HashMap<(String, String), usize>>,
    /// Sessions with recent attention (session -> when + what), for
    /// /api/attention.
    pub pending_attention:
        std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Attention)>>,
    /// Revocations (payload = device id): live sockets of that device close.
    pub revoked: tokio::sync::broadcast::Sender<String>,
    /// Latest tmux topology (sessions → windows), streamed to every client.
    pub topology: tokio::sync::watch::Sender<topology::Snapshot>,
}

/// Select the process-wide rustls crypto provider. Both axum-server and
/// reqwest pull rustls, and with two providers in the dependency graph
/// rustls refuses to guess — the TLS listener would panic at startup.
/// Call before any TLS use; safe to call repeatedly.
pub fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

pub fn host_of_url(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let host_port = rest.split('/').next()?;
    Some(host_port.split(':').next()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_url_variants() {
        assert_eq!(
            host_of_url("https://a.ts.net:7777/x"),
            Some("a.ts.net".into())
        );
        assert_eq!(host_of_url("http://10.0.0.1"), Some("10.0.0.1".into()));
        assert_eq!(host_of_url("no-scheme"), None);
    }
}
