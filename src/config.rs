//! Startup configuration for `remux serve`.
//!
//! A TOML file whose keys mirror the serve flags one-to-one, so users run
//! `remux serve` with no arguments day to day. Precedence is strict:
//! CLI flag > config file > built-in default — a flag typed today always
//! beats a value saved yesterday. Only `serve` reads this file; the other
//! subcommands find the daemon through the state dir / admin socket.
//!
//! The file holds no secrets (device tokens live hashed in the state dir;
//! the TLS entries are paths), but it is written 0600 in a 0700 dir anyway:
//! its contents shape the daemon's security posture (origins, hosts).

use crate::Args;
use anyhow::Context;
use std::path::{Path, PathBuf};

/// Serve settings that may come from the config file. Every field optional:
/// absent means "use the CLI value or the built-in default". Unknown keys are
/// a hard error — a typo silently ignored would surface much later as
/// "remux is broken", far from its cause.
#[derive(serde::Serialize, serde::Deserialize, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ServeConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<std::net::SocketAddr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_key: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_hosts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_client_origins: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_pair: Option<bool>,
}

/// `$XDG_CONFIG_HOME/remux/config.toml`, falling back to
/// `~/.config/remux/config.toml` on every platform — the same directory the
/// shipped packaging already uses (`~/.config/remux/env`), and honoring the
/// env var everywhere is what keeps tests and multi-daemon setups isolated
/// from the developer's real config (the XDG_DATA_HOME lesson).
pub fn default_path() -> anyhow::Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .context("no home directory")?;
    Ok(base.join("remux").join("config.toml"))
}

/// Load the file; missing file is an empty config, a malformed one is fatal.
pub fn load(path: &Path) -> anyhow::Result<ServeConfig> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ServeConfig::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut cfg: ServeConfig =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    // TLS paths are relative to the file that names them, never to whatever
    // working directory a service manager happened to start us in.
    if let Some(dir) = path.parent() {
        for p in [&mut cfg.tls_cert, &mut cfg.tls_key].into_iter().flatten() {
            if p.is_relative() {
                *p = dir.join(&p);
            }
        }
    }
    Ok(cfg)
}

/// Fill non-explicit `args` fields from the config. `explicit` answers "was
/// this flag typed on the command line?" (clap ValueSource) — an explicit
/// flag always wins, including list flags, which REPLACE the config list
/// rather than merging with it (merging cannot be reasoned about).
pub fn apply(args: &mut Args, cfg: ServeConfig, explicit: &dyn Fn(&str) -> bool) {
    if !explicit("listen") {
        if let Some(v) = cfg.listen {
            args.listen = v;
        }
    }
    if !explicit("session") {
        if let Some(v) = cfg.session {
            args.session = v;
        }
    }
    if args.url.is_none() {
        args.url = cfg.url;
    }
    if args.machine_name.is_none() {
        args.machine_name = cfg.machine_name;
    }
    if args.tls_cert.is_none() && args.tls_key.is_none() {
        args.tls_cert = cfg.tls_cert;
        args.tls_key = cfg.tls_key;
    }
    if !explicit("allowed_hosts") {
        if let Some(v) = cfg.allowed_hosts {
            args.allowed_hosts = v;
        }
    }
    if !explicit("allowed_client_origins") {
        if let Some(v) = cfg.allowed_client_origins {
            args.allowed_client_origins = v;
        }
    }
    // Two flags steer one bool: --no-pair forces true, --pair-on-start forces
    // false (it exists precisely to override a saved `no-pair = true` for one
    // run — e.g. pairing a new phone against a service install).
    if args.pair_on_start {
        args.no_pair = false;
    } else if !args.no_pair {
        args.no_pair = cfg.no_pair.unwrap_or(false);
    }
}

/// Post-merge validation: clap's `requires` can only see the command line, so
/// a config supplying half a TLS pair (or a cert the daemon can't read at
/// startup) must be caught here, not at first TLS handshake.
pub fn validate(args: &Args) -> anyhow::Result<()> {
    match (&args.tls_cert, &args.tls_key) {
        (Some(_), None) => anyhow::bail!("tls-cert is set but tls-key is not"),
        (None, Some(_)) => anyhow::bail!("tls-key is set but tls-cert is not"),
        _ => {}
    }
    for (name, p) in [("tls-cert", &args.tls_cert), ("tls-key", &args.tls_key)] {
        if let Some(p) = p {
            std::fs::metadata(p)
                .with_context(|| format!("{name} {} is not readable", p.display()))?;
        }
    }
    if let Some(url) = &args.url {
        if crate::host_of_url(url).is_none() {
            anyhow::bail!("url {url:?} is not a valid http(s) URL");
        }
    }
    Ok(())
}

/// What `--save-config` writes: the existing file's declarative intent with
/// the explicitly typed flags overlaid. Built-in defaults and derived values
/// are NOT frozen in — a config that never mentions `machine-name` keeps
/// tracking the hostname; one that never mentions `url` keeps deriving it.
pub fn merged_for_save(
    mut existing: ServeConfig,
    args: &Args,
    explicit: &dyn Fn(&str) -> bool,
) -> ServeConfig {
    if explicit("listen") {
        existing.listen = Some(args.listen);
    }
    if explicit("session") {
        existing.session = Some(args.session.clone());
    }
    if explicit("url") {
        existing.url = args.url.clone();
    }
    if explicit("machine_name") {
        existing.machine_name = args.machine_name.clone();
    }
    if explicit("tls_cert") {
        existing.tls_cert = args.tls_cert.clone();
        existing.tls_key = args.tls_key.clone();
    }
    if explicit("allowed_hosts") {
        existing.allowed_hosts = Some(args.allowed_hosts.clone());
    }
    if explicit("allowed_client_origins") {
        existing.allowed_client_origins = Some(args.allowed_client_origins.clone());
    }
    if args.no_pair {
        existing.no_pair = Some(true);
    } else if args.pair_on_start {
        // The explicit inverse: drop the key, restoring the default.
        existing.no_pair = None;
    }
    existing
}

/// Atomic write: temp file in the target dir, then rename. Dir 0700,
/// file 0600 (the file steers the daemon's security posture).
pub fn save(path: &Path, cfg: &ServeConfig) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .context("config path has no parent directory")?;
    create_private_dir(dir)?;
    let body = toml::to_string_pretty(cfg).context("serializing config")?;
    let tmp = dir.join(".config.toml.tmp");
    write_private_file(&tmp, &body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("moving config into place at {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true).mode(0o700);
    b.create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

#[cfg(unix)]
fn write_private_file(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, body: &str) -> std::io::Result<()> {
    std::fs::write(path, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn args(argv: &[&str]) -> Args {
        let full: Vec<&str> = std::iter::once("remux")
            .chain(argv.iter().copied())
            .collect();
        Args::parse_from(full)
    }

    /// Test double for clap ValueSource: the set of explicitly typed ids.
    fn explicit(ids: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |id| ids.contains(&id)
    }

    #[test]
    fn cli_beats_config_beats_default() {
        let mut a = args(&["--listen", "127.0.0.1:9999"]);
        let cfg = ServeConfig {
            listen: Some("127.0.0.1:1111".parse().unwrap()),
            session: Some("cfg".into()),
            ..Default::default()
        };
        apply(&mut a, cfg, &explicit(&["listen"]));
        assert_eq!(a.listen.port(), 9999, "explicit flag wins");
        assert_eq!(a.session, "cfg", "config beats the built-in default");
    }

    #[test]
    fn cli_list_replaces_config_list() {
        let mut a = args(&["--allowed-host", "cli.example"]);
        let cfg = ServeConfig {
            allowed_hosts: Some(vec!["cfg1.example".into(), "cfg2.example".into()]),
            ..Default::default()
        };
        apply(&mut a, cfg, &explicit(&["allowed_hosts"]));
        assert_eq!(a.allowed_hosts, vec!["cli.example".to_string()]);
    }

    #[test]
    fn pair_on_start_overrides_saved_no_pair() {
        let mut a = args(&["--pair-on-start"]);
        let cfg = ServeConfig {
            no_pair: Some(true),
            ..Default::default()
        };
        apply(&mut a, cfg, &explicit(&[]));
        assert!(!a.no_pair);
    }

    #[test]
    fn config_no_pair_applies_when_not_overridden() {
        let mut a = args(&[]);
        let cfg = ServeConfig {
            no_pair: Some(true),
            ..Default::default()
        };
        apply(&mut a, cfg, &explicit(&[]));
        assert!(a.no_pair);
    }

    #[test]
    fn unknown_key_is_fatal_and_missing_file_is_empty() {
        let dir = tempdir();
        let p = dir.join("config.toml");
        assert_eq!(load(&p).unwrap(), ServeConfig::default());
        std::fs::write(&p, "lisen = \"127.0.0.1:1\"\n").unwrap();
        assert!(load(&p).is_err(), "typo'd key must not be ignored");
    }

    #[test]
    fn relative_tls_paths_resolve_against_config_dir() {
        let dir = tempdir();
        let p = dir.join("config.toml");
        std::fs::write(&p, "tls-cert = \"c.crt\"\ntls-key = \"c.key\"\n").unwrap();
        let cfg = load(&p).unwrap();
        assert_eq!(cfg.tls_cert.unwrap(), dir.join("c.crt"));
        assert_eq!(cfg.tls_key.unwrap(), dir.join("c.key"));
    }

    #[test]
    fn validate_rejects_half_a_tls_pair_from_config() {
        let mut a = args(&[]);
        let cfg = ServeConfig {
            tls_cert: Some("/nonexistent/c.crt".into()),
            ..Default::default()
        };
        apply(&mut a, cfg, &explicit(&[]));
        assert!(validate(&a).unwrap_err().to_string().contains("tls-key"));
    }

    #[test]
    fn save_merges_intent_and_roundtrips() {
        let existing = ServeConfig {
            session: Some("kept".into()),
            no_pair: Some(true),
            ..Default::default()
        };
        let a = args(&["--listen", "100.1.2.3:7777", "--pair-on-start"]);
        let merged = merged_for_save(existing, &a, &explicit(&["listen"]));
        assert_eq!(
            merged.session.as_deref(),
            Some("kept"),
            "untouched field survives"
        );
        assert_eq!(merged.listen.unwrap().port(), 7777, "explicit flag lands");
        assert_eq!(merged.no_pair, None, "--pair-on-start clears the saved key");
        assert_eq!(merged.machine_name, None, "defaults are not frozen in");

        let dir = tempdir();
        let p = dir.join("config.toml");
        save(&p, &merged).unwrap();
        assert_eq!(load(&p).unwrap(), merged);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "remux-config-test-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
