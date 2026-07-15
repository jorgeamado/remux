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
    /// May this device resolve agent permission cards (M4b)? Off by default;
    /// granted host-side via `remux devices grant-approve`. Remotely
    /// authorizing an agent's tool-use is more than "view and type", so it is
    /// a separate opt-in capability. `#[serde(default)]` keeps older
    /// devices.json files (no field) loading as non-approvers.
    #[serde(default)]
    pub approve: bool,
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

    /// Exchange a pairing token for a device token. A *valid* token always
    /// pairs; only failed attempts consume the rate-limit bucket, so an
    /// attacker spraying bad tokens cannot starve the owner's real pairing.
    pub fn pair(&self, pairing_token: &str, device_name: &str) -> Result<String, PairError> {
        let mut inner = self.inner.lock().unwrap();

        let now = Instant::now();
        // Tokens stay valid until their TTL (not single-use): the iOS flow
        // needs to pair twice — once in the Safari tab and once inside the
        // installed PWA, whose storage is partitioned from the tab.
        inner.pairing.retain(|_, expiry| *expiry > now);
        if !inner.pairing.contains_key(pairing_token) {
            inner
                .attempts
                .retain(|t| now.duration_since(*t).as_secs() < 60);
            if inner.attempts.len() as u32 >= PAIR_ATTEMPTS_PER_MIN {
                return Err(PairError::RateLimited);
            }
            inner.attempts.push(now);
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
            approve: false,
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

    /// Validate a device token; returns the device record. The hash compare
    /// is constant-time (defense in depth — the value compared is already a
    /// SHA-256, so a timing leak would only reveal hash prefixes, but token
    /// checks should not leak timing regardless).
    pub fn authenticate(&self, device_token: &str) -> Option<Device> {
        use subtle::ConstantTimeEq;
        let hash = sha256_hex(device_token);
        let inner = self.inner.lock().unwrap();
        inner
            .devices
            .iter()
            .find(|d| d.token_sha256.as_bytes().ct_eq(hash.as_bytes()).into())
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

    /// Rename a device. Like `revoke`, a rename that cannot be persisted is
    /// rolled back and reported as an error — never claim success while the old
    /// name would silently return on restart.
    pub fn rename(&self, device_id: &str, name: &str) -> Result<()> {
        let inner = &mut *self.inner.lock().unwrap();
        let Some(pos) = inner.devices.iter().position(|d| d.id == device_id) else {
            anyhow::bail!("no such device");
        };
        let new_name: String = name.trim().chars().take(64).collect();
        let old = std::mem::replace(&mut inner.devices[pos].name, new_name);
        if let Err(e) = self.persist(&inner.devices) {
            inner.devices[pos].name = old;
            tracing::error!("failed to persist rename: {e:#}");
            anyhow::bail!("could not persist the rename; name NOT changed");
        }
        Ok(())
    }

    /// Is this device id still paired? Checked synchronously on each input
    /// frame so a revocation takes effect immediately, without waiting for
    /// the async revocation broadcast to close the socket.
    pub fn is_active(&self, device_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .devices
            .iter()
            .any(|d| d.id == device_id)
    }

    /// May this device resolve permission cards *right now*? Checked by id at
    /// decision time — never off a `Device` clone captured at WS auth — so a
    /// `grant-approve`/`revoke-approve` (or a full revoke) takes effect on
    /// already-connected sockets, not just new ones (M4b).
    pub fn can_approve(&self, device_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .devices
            .iter()
            .any(|d| d.id == device_id && d.approve)
    }

    /// Grant or revoke the `approve` capability. Persisted before returning;
    /// a write failure rolls the change back and errors, so the capability
    /// can never silently flip after a restart (mirrors `revoke`). Returns
    /// whether the value actually changed.
    pub fn set_approve(&self, device_id: &str, approve: bool) -> Result<bool> {
        let inner = &mut *self.inner.lock().unwrap();
        let Some(d) = inner.devices.iter_mut().find(|d| d.id == device_id) else {
            anyhow::bail!("no such device");
        };
        if d.approve == approve {
            return Ok(false);
        }
        d.approve = approve;
        if let Err(e) = self.persist(&inner.devices) {
            // Roll back the in-memory change so state matches disk.
            if let Some(d) = inner.devices.iter_mut().find(|d| d.id == device_id) {
                d.approve = !approve;
            }
            tracing::error!("failed to persist approve change: {e:#}");
            anyhow::bail!("could not persist the capability change; NOT applied");
        }
        Ok(true)
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
    fn approve_capability_off_by_default_and_persists() {
        let auth = temp_auth();
        let path = auth.path.clone();
        let pairing = auth.new_pairing_token();
        let _ = auth.pair(&pairing, "phone").unwrap();
        let id = auth.devices()[0].id.clone();
        // Off by default.
        assert!(!auth.can_approve(&id));
        // Grant is idempotent-aware: first grant changes, second is a no-op.
        assert!(auth.set_approve(&id, true).unwrap());
        assert!(!auth.set_approve(&id, true).unwrap());
        assert!(auth.can_approve(&id));
        // Unknown device errors, does not panic.
        assert!(auth.set_approve("nope", true).is_err());
        // Survives reload.
        drop(auth);
        let reloaded = Auth::load(path).unwrap();
        assert!(reloaded.can_approve(&id));
        // Revoking the whole device also drops the capability.
        assert!(reloaded.revoke(&id).is_ok());
        assert!(!reloaded.can_approve(&id));
    }

    #[test]
    fn rename_persists_rolls_back_on_failure_and_reports_errors() {
        use std::os::unix::fs::PermissionsExt;
        let auth = temp_auth();
        let path = auth.path.clone();
        let pairing = auth.new_pairing_token();
        let _ = auth.pair(&pairing, "orig").unwrap();
        let id = auth.devices()[0].id.clone();

        // Happy path renames.
        auth.rename(&id, "renamed").unwrap();
        assert_eq!(auth.devices()[0].name, "renamed");
        // Unknown device is an error, not a silent success.
        assert!(auth.rename("nope", "x").is_err());

        // A persist failure must roll back the in-memory name (the bug was
        // returning success and leaving the doomed name to revert on restart).
        let dir = path.parent().unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        // Skip the failure assertion where the process can write anyway (root).
        let can_still_write = std::fs::write(dir.join(".probe"), b"x").is_ok();
        let _ = std::fs::remove_file(dir.join(".probe"));
        if !can_still_write {
            assert!(auth.rename(&id, "should-fail").is_err());
            assert_eq!(
                auth.devices()[0].name,
                "renamed",
                "rolled back, not applied"
            );
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        // The persisted name is the last successful one.
        drop(auth);
        let reloaded = Auth::load(path).unwrap();
        assert_eq!(reloaded.devices()[0].name, "renamed");
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
