use anyhow::Result;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PAIRING_TTL: Duration = Duration::from_secs(600);
const PAIR_ATTEMPTS_PER_MIN: u32 = 5;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub token_sha256: String,
    pub created_unix: u64,
    /// Unix seconds of the last successful websocket auth.
    #[serde(default)]
    pub last_seen_unix: u64,
}

struct Inner {
    devices: Vec<Device>,
    /// pairing token -> expiry
    pairing: HashMap<String, Instant>,
    attempts: Vec<Instant>,
}

pub struct Auth {
    path: PathBuf,
    inner: Mutex<Inner>,
}

fn random_token() -> String {
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

impl Auth {
    pub fn load(path: PathBuf) -> Result<Self> {
        let devices = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path,
            inner: Mutex::new(Inner {
                devices,
                pairing: HashMap::new(),
                attempts: Vec::new(),
            }),
        })
    }

    fn persist(&self, devices: &[Device]) -> Result<()> {
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(devices)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Generate a single-use pairing token valid for PAIRING_TTL.
    pub fn new_pairing_token(&self) -> String {
        let token = random_token();
        let mut inner = self.inner.lock().unwrap();
        inner
            .pairing
            .insert(token.clone(), Instant::now() + PAIRING_TTL);
        token
    }

    /// Exchange a pairing token for a device token. Rate limited.
    pub fn pair(&self, pairing_token: &str, device_name: &str) -> Result<String, PairError> {
        let mut inner = self.inner.lock().unwrap();

        let now = Instant::now();
        inner
            .attempts
            .retain(|t| now.duration_since(*t).as_secs() < 60);
        if inner.attempts.len() as u32 >= PAIR_ATTEMPTS_PER_MIN {
            return Err(PairError::RateLimited);
        }
        inner.attempts.push(now);

        // Tokens stay valid until their TTL (not single-use): the iOS flow
        // needs to pair twice — once in the Safari tab and once inside the
        // installed PWA, whose storage is partitioned from the tab.
        inner.pairing.retain(|_, expiry| *expiry > now);
        if !inner.pairing.contains_key(pairing_token) {
            return Err(PairError::InvalidToken);
        }

        let device_token = random_token();
        let device = Device {
            id: random_token()[..12].to_string(),
            name: device_name.trim().chars().take(64).collect(),
            token_sha256: sha256_hex(&device_token),
            created_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            last_seen_unix: 0,
        };
        inner.devices.push(device);
        // Persist under the lock: concurrent mutators (pair/touch) must not
        // race each other's snapshots onto disk, or a device can be lost.
        if let Err(e) = self.persist(&inner.devices) {
            tracing::error!("failed to persist devices: {e:#}");
            inner.devices.pop();
            return Err(PairError::Internal);
        }
        Ok(device_token)
    }

    /// Validate a device token; returns the device record.
    pub fn authenticate(&self, device_token: &str) -> Option<Device> {
        let hash = sha256_hex(device_token);
        let inner = self.inner.lock().unwrap();
        inner
            .devices
            .iter()
            .find(|d| d.token_sha256 == hash)
            .cloned()
    }

    /// All paired devices (for `remux devices` and the read-only PWA sheet).
    pub fn devices(&self) -> Vec<Device> {
        self.inner.lock().unwrap().devices.clone()
    }

    /// Remove a device. Also cancels every outstanding pairing token: a
    /// revocation mid-incident must not leave a live enrollment window open.
    /// A revocation that cannot be persisted is rolled back and reported as
    /// an error — the token must not silently come back after a restart.
    pub fn revoke(&self, device_id: &str) -> Result<()> {
        let inner = &mut *self.inner.lock().unwrap();
        let Some(pos) = inner.devices.iter().position(|d| d.id == device_id) else {
            anyhow::bail!("no such device");
        };
        let removed = inner.devices.remove(pos);
        if let Err(e) = self.persist(&inner.devices) {
            inner.devices.insert(pos, removed);
            tracing::error!("failed to persist revocation: {e:#}");
            anyhow::bail!("could not persist the revocation; device NOT revoked");
        }
        inner.pairing.clear();
        Ok(())
    }

    pub fn rename(&self, device_id: &str, name: &str) -> bool {
        let inner = &mut *self.inner.lock().unwrap();
        let Some(d) = inner.devices.iter_mut().find(|d| d.id == device_id) else {
            return false;
        };
        d.name = name.trim().chars().take(64).collect();
        if let Err(e) = self.persist(&inner.devices) {
            tracing::error!("failed to persist rename: {e:#}");
        }
        true
    }

    /// Record websocket auth time for a device (best effort).
    pub fn touch(&self, device_id: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let inner = &mut *self.inner.lock().unwrap();
        match inner.devices.iter_mut().find(|d| d.id == device_id) {
            Some(d) => d.last_seen_unix = now,
            None => return,
        }
        // Persist under the lock — see pair().
        if let Err(e) = self.persist(&inner.devices) {
            tracing::debug!("failed to persist last-seen: {e:#}");
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum PairError {
    #[error("invalid or expired pairing token")]
    InvalidToken,
    #[error("too many pairing attempts, try again later")]
    RateLimited,
    #[error("internal error")]
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_auth() -> Auth {
        let dir = std::env::temp_dir().join(format!("remux-test-{}", random_token()));
        std::fs::create_dir_all(&dir).unwrap();
        Auth::load(dir.join("devices.json")).unwrap()
    }

    #[test]
    fn pair_and_authenticate() {
        let auth = temp_auth();
        let pairing = auth.new_pairing_token();
        let device_token = auth.pair(&pairing, "phone").unwrap();
        assert_eq!(
            auth.authenticate(&device_token).map(|d| d.name),
            Some("phone".into())
        );
        assert!(auth.authenticate("bogus").is_none());
    }

    #[test]
    fn pairing_token_reusable_within_ttl() {
        // Both the Safari tab and the installed PWA must be able to pair
        // with the same token (iOS partitions their storage).
        let auth = temp_auth();
        let pairing = auth.new_pairing_token();
        let t1 = auth.pair(&pairing, "safari tab").unwrap();
        let t2 = auth.pair(&pairing, "installed pwa").unwrap();
        assert_ne!(t1, t2);
        assert!(auth.authenticate(&t1).is_some());
        assert!(auth.authenticate(&t2).is_some());
    }

    #[test]
    fn expired_pairing_token_rejected() {
        let auth = temp_auth();
        let pairing = auth.new_pairing_token();
        auth.inner
            .lock()
            .unwrap()
            .pairing
            .insert(pairing.clone(), Instant::now() - Duration::from_secs(1));
        assert!(matches!(
            auth.pair(&pairing, "late"),
            Err(PairError::InvalidToken)
        ));
    }

    #[test]
    fn invalid_pairing_token_rejected() {
        let auth = temp_auth();
        assert!(matches!(
            auth.pair("nope", "x"),
            Err(PairError::InvalidToken)
        ));
    }

    #[test]
    fn pairing_rate_limited() {
        let auth = temp_auth();
        for _ in 0..PAIR_ATTEMPTS_PER_MIN {
            let _ = auth.pair("wrong", "x");
        }
        assert!(matches!(
            auth.pair("wrong", "x"),
            Err(PairError::RateLimited)
        ));
    }

    #[test]
    fn devices_persist_across_load() {
        let auth = temp_auth();
        let path = auth.path.clone();
        let pairing = auth.new_pairing_token();
        let device_token = auth.pair(&pairing, "phone").unwrap();
        drop(auth);
        let reloaded = Auth::load(path).unwrap();
        assert_eq!(
            reloaded.authenticate(&device_token).map(|d| d.name),
            Some("phone".into())
        );
    }
}
