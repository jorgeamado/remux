#![allow(dead_code)] // shared across test binaries; not all use every helper
use clap::Parser;
use remux::{auth::Auth, server, App, Args};
use std::net::SocketAddr;
use std::sync::Arc;

/// Start the real router on an ephemeral port with a fresh state dir.
/// Returns the bound address and the app (for minting pairing tokens).
pub async fn start_server(session: &str) -> (SocketAddr, Arc<App>) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("remux=trace")
        .try_init();
    let dir =
        std::env::temp_dir().join(format!("remux-it-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let auth = Auth::load(dir.join("devices.json")).unwrap();

    let args = Args::parse_from(["remux", "--session", session, "--no-pair"]);
    let app = Arc::new(App {
        allowed_hosts: vec!["localhost".into(), "127.0.0.1".into()],
        auth,
        args,
        attention: tokio::sync::broadcast::channel(16).0,
        public_url: "http://127.0.0.1:0".into(),
        push: remux::push::Push::load(&dir).unwrap(),
        connections: Default::default(),
        pending_attention: Default::default(),
        revoked: tokio::sync::broadcast::channel(16).0,
        topology: tokio::sync::watch::channel(std::sync::Arc::new(Vec::new())).0,
        perms: Default::default(),
        agents: Default::default(),
        chat: Default::default(),
        pane_views: Default::default(),
        dash_windows: Default::default(),
        feed: Default::default(),
        detector_reset: tokio::sync::broadcast::channel(16).0,
    });
    remux::attention::spawn(app.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server::router(app.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (addr, app)
}

pub fn rand_suffix() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    // Timestamp alone can collide across parallel tests; disambiguate.
    static SEQ: AtomicU32 = AtomicU32::new(0);
    format!(
        "{:x}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn tmux_available() -> bool {
    std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
