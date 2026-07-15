//! Web Push for attention notifications. Payload-less by design: the
//! notification text is always generic, so no terminal content or session
//! names ever transit Apple's/Google's push services. The daemon signs each
//! send with a VAPID (ES256) key generated once and kept in the state dir.

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::Generate;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Push service hosts the daemon will POST to. The endpoint URL is
/// client-supplied — without this allowlist a paired device could aim the
/// daemon's outbound requests at the tailnet (blind SSRF).
const ALLOWED_SUFFIXES: &[&str] = &[
    "push.apple.com",
    "fcm.googleapis.com",
    "push.services.mozilla.com",
    "notify.windows.com",
];

/// Global cap on stored subscriptions (per-device cap is separate). Bounds
/// disk/memory even if many devices are paired.
const MAX_SUBSCRIPTIONS: usize = 1024;

const SUBS_PER_DEVICE: usize = 3;
const PUSH_TTL_SECS: u32 = 300;
const THROTTLE: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Subscription {
    pub device_id: String,
    pub endpoint: String,
    /// Client keys, kept for a future encrypted-payload upgrade.
    #[serde(default)]
    pub p256dh: String,
    #[serde(default)]
    pub auth: String,
}

pub struct Push {
    subs_path: PathBuf,
    key: SigningKey,
    /// base64url uncompressed public point — the browser's applicationServerKey.
    public_key_b64: String,
    subs: Mutex<Vec<Subscription>>,
    /// (endpoint, session) -> last push, for throttling.
    recent: Mutex<HashMap<(String, String), Instant>>,
    http: reqwest::Client,
}

impl Push {
    pub fn load(state_dir: &Path) -> Result<Self> {
        let key_path = state_dir.join("vapid.json");
        let key = match std::fs::read(&key_path) {
            Ok(bytes) => {
                let v: serde_json::Value = serde_json::from_slice(&bytes)?;
                let raw = URL_SAFE_NO_PAD
                    .decode(v["private"].as_str().context("vapid.json missing key")?)?;
                SigningKey::from_slice(&raw).context("bad VAPID key")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = SigningKey::generate();
                let encoded = URL_SAFE_NO_PAD.encode(key.to_bytes());
                let tmp = key_path.with_extension("tmp");
                std::fs::write(&tmp, serde_json::json!({ "private": encoded }).to_string())?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
                }
                std::fs::rename(&tmp, &key_path)?;
                key
            }
            Err(e) => return Err(e.into()),
        };
        let public_key_b64 = URL_SAFE_NO_PAD.encode(
            key.verifying_key()
                .to_sec1_point(false /* uncompressed */)
                .as_bytes(),
        );

        let subs_path = state_dir.join("push.json");
        let subs: Vec<Subscription> = match std::fs::read(&subs_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };

        Ok(Self {
            subs_path,
            key,
            public_key_b64,
            subs: Mutex::new(subs),
            recent: Mutex::new(HashMap::new()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
        })
    }

    pub fn public_key(&self) -> &str {
        &self.public_key_b64
    }

    pub fn subscribe(&self, sub: Subscription) -> Result<()> {
        validate_endpoint(&sub.endpoint)?;
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| s.endpoint != sub.endpoint);
        let per_device = subs.iter().filter(|s| s.device_id == sub.device_id).count();
        if per_device >= SUBS_PER_DEVICE {
            bail!("too many push subscriptions for this device");
        }
        if subs.len() >= MAX_SUBSCRIPTIONS {
            bail!("subscription store is full");
        }
        subs.push(sub);
        self.persist(&subs)
    }

    pub fn unsubscribe(&self, device_id: &str, endpoint: &str) {
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| !(s.device_id == device_id && s.endpoint == endpoint));
        let _ = self.persist(&subs);
    }

    /// Drop everything belonging to a device (revocation cascade, M2).
    pub fn remove_device(&self, device_id: &str) {
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| s.device_id != device_id);
        let _ = self.persist(&subs);
    }

    fn persist(&self, subs: &[Subscription]) -> Result<()> {
        let tmp = self.subs_path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(subs)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.subs_path)?;
        Ok(())
    }

    /// Push "attention" for `session` to every subscription except devices in
    /// `skip` (devices with a live socket on that session get the in-band
    /// frame instead). Throttled per (endpoint, session).
    pub async fn notify(&self, session: &str, skip: &[String]) {
        let targets: Vec<Subscription> = {
            let subs = self.subs.lock().unwrap();
            let mut recent = self.recent.lock().unwrap();
            let now = Instant::now();
            recent.retain(|_, t| now.duration_since(*t) < THROTTLE);
            subs.iter()
                .filter(|s| !skip.contains(&s.device_id))
                .filter(
                    |s| match recent.entry((s.endpoint.clone(), session.to_string())) {
                        std::collections::hash_map::Entry::Occupied(_) => false,
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(now);
                            true
                        }
                    },
                )
                .cloned()
                .collect()
        };
        for sub in targets {
            if let Err(e) = self.send(&sub).await {
                tracing::debug!(endpoint = %sub.endpoint, "push failed: {e:#}");
            }
        }
    }

    async fn send(&self, sub: &Subscription) -> Result<()> {
        // Re-validate before every outbound request: the store is trusted,
        // but this is the last line before the daemon makes a network call.
        validate_endpoint(&sub.endpoint)?;
        let jwt = self.vapid_jwt(&sub.endpoint)?;
        let resp = self
            .http
            .post(&sub.endpoint)
            .header("TTL", PUSH_TTL_SECS)
            .header("Urgency", "high")
            .header(
                "Authorization",
                format!("vapid t={jwt}, k={}", self.public_key_b64),
            )
            .body(Vec::new())
            .send()
            .await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
            tracing::info!(endpoint = %sub.endpoint, "push subscription expired; pruning");
            self.unsubscribe(&sub.device_id, &sub.endpoint);
        } else if !status.is_success() {
            bail!("push service answered {status}");
        }
        Ok(())
    }

    /// RFC 8292 ES256 JWT for the endpoint's origin.
    fn vapid_jwt(&self, endpoint: &str) -> Result<String> {
        let origin = origin_of(endpoint).context("bad endpoint url")?;
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 12 * 3600;
        let claims = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "aud": origin,
                "exp": exp,
                "sub": "https://github.com/jorgeamado/remux",
            })
            .to_string(),
        );
        let signing_input = format!("{header}.{claims}");
        let sig: Signature = self.key.sign(signing_input.as_bytes());
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(sig.to_bytes())
        ))
    }
}

/// Bridges attention events to pushes: records pending attention, skips
/// delivery when someone is at a keyboard (tmux client activity) or when the
/// device already receives the in-band frame.
pub fn spawn_dispatcher(app: std::sync::Arc<crate::App>) {
    const KEYBOARD_GRACE_SECS: u64 = 30;
    tokio::spawn(async move {
        let mut rx = app.attention.subscribe();
        loop {
            match rx.recv().await {
                Ok(session) => {
                    app.pending_attention
                        .lock()
                        .unwrap()
                        .insert(session.clone(), Instant::now());
                    let at_keyboard = tokio::task::spawn_blocking(|| {
                        crate::tmux::any_client_active_within(KEYBOARD_GRACE_SECS)
                    })
                    .await
                    .map(|r| r.unwrap_or(false))
                    .unwrap_or(false);
                    if at_keyboard {
                        continue;
                    }
                    let skip: Vec<String> = app
                        .connections
                        .lock()
                        .unwrap()
                        .keys()
                        .filter(|(_, s)| s == &session)
                        .map(|(d, _)| d.clone())
                        .collect();
                    app.push.notify(&session, &skip).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn origin_of(url: &str) -> Option<String> {
    // http only ever appears via the REMUX_PUSH_ALLOW_HOST test hook.
    let (scheme, rest) = if let Some(r) = url.strip_prefix("https://") {
        ("https", r)
    } else {
        ("http", url.strip_prefix("http://")?)
    };
    let host_port = rest.split('/').next()?;
    Some(format!("{scheme}://{host_port}"))
}

/// Parse and validate a push endpoint URL, returning the authoritative host.
/// Uses a real URL parser (not string splitting): the naive `split(':')`
/// approach treated `https://web.push.apple.com:443@127.0.0.1/x` as host
/// `web.push.apple.com`, while the HTTP client connects to `127.0.0.1` —
/// an SSRF into the tailnet. url::Url resolves userinfo/host correctly.
fn validate_endpoint(endpoint: &str) -> Result<()> {
    if endpoint.len() > 2048 {
        bail!("endpoint too long");
    }
    let url = url::Url::parse(endpoint).context("unparseable endpoint URL")?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("push endpoint must not contain userinfo");
    }
    let host = url
        .host_str()
        .context("endpoint has no host")?
        .to_ascii_lowercase();

    // Test hook: REMUX_PUSH_ALLOW_HOST permits one extra host (any scheme)
    // so integration tests can run a local fake push service.
    if let Ok(allowed) = std::env::var("REMUX_PUSH_ALLOW_HOST") {
        if !allowed.is_empty() && host == allowed.to_ascii_lowercase() {
            return Ok(());
        }
    }
    if url.scheme() != "https" {
        bail!("push endpoints must be https");
    }
    // Exact host, or a subdomain of a provider domain we anchor on a leading
    // dot (so "evilfcm.googleapis.com" cannot match "fcm.googleapis.com").
    let ok = ALLOWED_SUFFIXES.iter().any(|s| {
        let base = s.trim_start_matches('.');
        host == base || host.ends_with(&format!(".{base}"))
    });
    if ok {
        Ok(())
    } else {
        bail!("push endpoint host {host:?} is not a known push service");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_push() -> (Push, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "remux-push-{}-{:x}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (Push::load(&dir).unwrap(), dir)
    }

    #[test]
    fn vapid_key_persists_across_load() {
        let (push, dir) = temp_push();
        let key1 = push.public_key().to_string();
        drop(push);
        let push2 = Push::load(&dir).unwrap();
        assert_eq!(push2.public_key(), key1);
        // applicationServerKey: uncompressed P-256 point = 65 bytes.
        assert_eq!(URL_SAFE_NO_PAD.decode(key1).unwrap().len(), 65);
    }

    #[test]
    fn jwt_shape_and_audience() {
        let (push, _) = temp_push();
        let jwt = push
            .vapid_jwt("https://web.push.apple.com/QOX99y8Z")
            .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://web.push.apple.com");
        assert_eq!(URL_SAFE_NO_PAD.decode(parts[2]).unwrap().len(), 64);
    }

    #[test]
    fn endpoint_allowlist() {
        std::env::remove_var("REMUX_PUSH_ALLOW_HOST");
        assert!(validate_endpoint("https://web.push.apple.com/QOX").is_ok());
        assert!(validate_endpoint("https://fcm.googleapis.com/fcm/send/x").is_ok());
        assert!(validate_endpoint("https://updates.push.services.mozilla.com/wpush/v2/x").is_ok());
        assert!(validate_endpoint("https://WEB.PUSH.APPLE.COM/x").is_ok()); // case-folded
        assert!(validate_endpoint("http://web.push.apple.com/x").is_err()); // not https
        assert!(validate_endpoint("https://evil.example.com/x").is_err());
        assert!(validate_endpoint("https://127.0.0.1:9/x").is_err());
        assert!(validate_endpoint("https://internal.push.apple.com.evil.io/x").is_err());
        // SSRF via userinfo: real host is 127.0.0.1, not the push service.
        assert!(validate_endpoint("https://web.push.apple.com:443@127.0.0.1:8443/x").is_err());
        assert!(validate_endpoint("https://user@web.push.apple.com/x").is_err());
        // Suffix confusion: not a subdomain of the anchored base.
        assert!(validate_endpoint("https://evilfcm.googleapis.com/x").is_err());
        assert!(validate_endpoint("https://notapple.com/x").is_err());
    }

    #[test]
    fn subscribe_limits_and_persistence() {
        let (push, dir) = temp_push();
        let sub = |i: usize| Subscription {
            device_id: "dev1".into(),
            endpoint: format!("https://web.push.apple.com/s{i}"),
            p256dh: String::new(),
            auth: String::new(),
        };
        for i in 0..SUBS_PER_DEVICE {
            push.subscribe(sub(i)).unwrap();
        }
        assert!(push.subscribe(sub(99)).is_err()); // cap
        push.subscribe(sub(0)).unwrap(); // same endpoint replaces, no growth
        push.unsubscribe("dev1", "https://web.push.apple.com/s1");
        drop(push);
        let push2 = Push::load(&dir).unwrap();
        assert_eq!(push2.subs.lock().unwrap().len(), SUBS_PER_DEVICE - 1);
        push2.remove_device("dev1");
        assert!(push2.subs.lock().unwrap().is_empty());
    }
}
