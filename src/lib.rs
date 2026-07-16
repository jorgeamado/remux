pub mod admin;
pub mod attention;
pub mod auth;
pub mod feed;
pub mod ingest;
pub mod permit;
pub mod push;
pub mod server;
pub mod shell;
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
    /// One-time, opt-in setup for features that need host config.
    Setup {
        #[command(subcommand)]
        cmd: SetupCmd,
    },
    /// Test the notification pipeline end to end: counts down (so you can
    /// lock the phone — pushes are suppressed while you're typing), then
    /// raises an agent_needs_input event for this pane's session.
    TestAttention {
        /// Seconds to wait before firing. Must outlast the dispatcher's
        /// 30s keyboard grace or the event is (correctly) suppressed.
        #[arg(long, default_value_t = 45)]
        delay: u64,
        /// Notification detail text.
        #[arg(long, default_value = "test notification — it works!")]
        message: String,
    },
    /// Test the M4b approval flow end to end: open a real permission card for
    /// this pane's session and block until you Approve/Deny it from the phone
    /// (or it expires ~100s). Needs this device granted `approve`
    /// (`remux devices grant-approve <id>`) and the PWA open.
    TestPermission {
        /// Tool name shown on the card.
        #[arg(long, default_value = "Bash")]
        tool: String,
        /// One-line summary shown on the card (stand-in for a command).
        #[arg(long, default_value = "echo hello from remux   # test approval")]
        summary: String,
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
    /// An agent permission prompt: block until a device approves/denies, then
    /// print Claude Code's decision JSON on stdout. Reads the hook's
    /// PermissionRequest payload (tool_name, tool_input, prompt_id) from stdin
    /// — the install snippet pipes it straight through, so no fragile shell
    /// extraction. On any failure (no decision, expiry, daemon down) prints a
    /// diagnostic to stderr and exits non-zero, which makes Claude Code fall
    /// back to its own dialog on the Mac.
    Permission {
        /// tmux pane id (%N). Defaults to $TMUX_PANE (read here, not from the
        /// hook payload, which doesn't carry it).
        #[arg(long)]
        pane: Option<String>,
        /// Producer label.
        #[arg(long, default_value = "claude-code")]
        source: String,
        /// Present for symmetry with the docs; the wait is implied for this
        /// subcommand (a permission prompt with no blocking makes no sense).
        #[arg(long, default_value_t = true)]
        wait: bool,
    },
    /// A shell command is starting (zsh preexec). Fire-and-forget, non-blocking
    /// (M4c) — for the shell-hook one-liners. Informational only.
    CommandStart {
        /// tmux pane id (%N). Defaults to $TMUX_PANE.
        #[arg(long)]
        pane: Option<String>,
        /// Random id, stable for this interactive shell's lifetime.
        #[arg(long)]
        shell_id: String,
        /// Per-shell monotonic counter for this command.
        #[arg(long)]
        command_id: u64,
        /// The command line (capped/sanitized by the daemon).
        #[arg(long)]
        command: String,
        /// Working directory when the command started.
        #[arg(long, default_value = "")]
        cwd: String,
    },
    /// A shell command finished (zsh precmd). Fire-and-forget, non-blocking.
    CommandEnd {
        /// Random id, stable for this interactive shell's lifetime.
        #[arg(long)]
        shell_id: String,
        /// Must match the `command_id` of the paired start.
        #[arg(long)]
        command_id: u64,
        /// The command's exit status.
        #[arg(long)]
        exit: i32,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum SetupCmd {
    /// Install (or remove) the command-feed hook (M4c) in your shell's rc file
    /// (bash → ~/.bashrc, zsh → ~/.zshrc; auto-detected). Prints what it does
    /// and why, and asks before writing anything (unless --yes).
    Shell {
        /// Which shell to install for. Auto-detected from $SHELL if omitted.
        #[arg(long, value_parser = ["bash", "zsh"])]
        shell: Option<String>,
        /// Install without the interactive confirmation prompt.
        #[arg(long, conflicts_with_all = ["uninstall", "print"])]
        yes: bool,
        /// Remove the remux hook block from the shell's rc file.
        #[arg(long, conflicts_with = "print")]
        uninstall: bool,
        /// Print the snippet instead of touching the rc file (manual install).
        #[arg(long)]
        print: bool,
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
    /// Grant a device the `approve` capability: it may resolve agent
    /// permission cards (M4b). Off by default; host-only.
    GrantApprove { id: String },
    /// Revoke a device's `approve` capability. Takes effect on live
    /// connections immediately (checked by id at decision time).
    RevokeApprove { id: String },
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

    /// Exact origin (scheme://host[:port]) of a remux PWA served by ANOTHER
    /// machine that may use this daemon cross-origin (multi-machine setup:
    /// the phone installs the PWA from one machine and adds the others).
    /// Grants that origin CORS on /api/* and websocket access. Repeatable.
    /// Unlike --allowed-host this matches the whole origin, not the hostname.
    #[arg(long = "allowed-client-origin")]
    pub allowed_client_origins: Vec<String>,

    /// Human-readable name for this machine, shown by multi-machine clients.
    /// Defaults to the OS hostname.
    #[arg(long)]
    pub machine_name: Option<String>,

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
    /// Normalized exact origins of foreign remux PWAs (see
    /// `--allowed-client-origin`); ascii-serialized for byte comparison.
    pub allowed_client_origins: Vec<String>,
    /// Persistent random id for this daemon's machine (state dir), so clients
    /// can key their per-machine records on something stabler than a URL.
    pub machine_id: String,
    /// Display name for this machine (`--machine-name`, default: hostname).
    pub machine_name: String,
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
    /// Open agent permission cards (M4b) awaiting a decision from a device.
    pub perms: permit::Registry,
    /// Per-session shell command feed (M4c), fed by the shell datagram socket.
    pub feed: feed::Feed,
    /// Session names whose busy→quiet detector should reset — sent when a
    /// precise `command_finished` arrives (M4c), so the heuristic doesn't
    /// *also* fire "went quiet" for the same command. Consumed by the
    /// attention monitor.
    pub detector_reset: tokio::sync::broadcast::Sender<String>,
}

/// Select the process-wide rustls crypto provider. Both axum-server and
/// reqwest pull rustls, and with two providers in the dependency graph
/// rustls refuses to guess — the TLS listener would panic at startup.
/// Call before any TLS use; safe to call repeatedly.
pub fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Canonical form of a web origin for exact matching: lowercased scheme+host,
/// default port elided, no userinfo/path/query. `None` for anything that isn't
/// a clean http(s) origin — including opaque origins and URLs with credentials.
pub fn normalize_origin(s: &str) -> Option<String> {
    let u = url::Url::parse(s.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    if !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    // An origin has no path/query/fragment. Fail rather than strip: a config
    // value like `https://host/pwa` would otherwise silently grant the whole
    // host, which is more than the operator wrote down.
    if !matches!(u.path(), "" | "/") || u.query().is_some() || u.fragment().is_some() {
        return None;
    }
    match u.origin() {
        url::Origin::Tuple(..) => Some(u.origin().ascii_serialization()),
        url::Origin::Opaque(_) => None,
    }
}

/// Load (or create, first run) this machine's persistent random id.
/// A corrupt/foreign-format file is regenerated rather than trusted: the id
/// is only a client-side correlation key, never a credential.
pub fn machine_id(state_dir: &std::path::Path) -> anyhow::Result<String> {
    use rand::Rng;
    let path = state_dir.join("machine.id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let id = existing.trim();
        if id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(id.to_string());
        }
    }
    let mut buf = [0u8; 16];
    rand::rng().fill_bytes(&mut buf);
    let id = hex::encode(buf);
    std::fs::write(&path, &id)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(id)
}

/// Default machine display name: the OS hostname (short form), or "remux".
pub fn default_machine_name() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "remux".into())
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
    fn normalize_origin_variants() {
        // Canonicalization: case, default ports, trailing slash.
        assert_eq!(
            normalize_origin("HTTPS://Mac.TS.net:7777"),
            Some("https://mac.ts.net:7777".into())
        );
        assert_eq!(
            normalize_origin("https://mac.ts.net:443/"),
            Some("https://mac.ts.net".into())
        );
        assert_eq!(
            normalize_origin("http://localhost:5173"),
            Some("http://localhost:5173".into())
        );
        // Rejected: credentials, non-http schemes, garbage, and anything
        // beyond a bare origin (fail fast instead of granting more than
        // the operator wrote).
        assert_eq!(normalize_origin("https://a@evil.example"), None);
        assert_eq!(normalize_origin("file:///etc/passwd"), None);
        assert_eq!(normalize_origin("not an origin"), None);
        assert_eq!(normalize_origin("https://mac.ts.net:7777/pwa"), None);
        assert_eq!(normalize_origin("https://mac.ts.net:7777/?x=1"), None);
        assert_eq!(normalize_origin("https://mac.ts.net:7777/#f"), None);
    }

    #[test]
    fn machine_id_persists_and_survives_corruption() {
        let dir = std::env::temp_dir().join(format!("remux-mid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = machine_id(&dir).unwrap();
        let b = machine_id(&dir).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        std::fs::write(dir.join("machine.id"), "not-hex!").unwrap();
        let c = machine_id(&dir).unwrap();
        assert_ne!(c, "not-hex!");
        assert_eq!(c.len(), 32);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
