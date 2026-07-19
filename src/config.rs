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
#[derive(serde::Serialize, serde::Deserialize, Debug, Default, PartialEq, Clone)]
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
    // Cert and key merge independently: a config may hold the cert while the
    // command line supplies a rotated key (post-merge validation still
    // requires the pair to be complete).
    if args.tls_cert.is_none() {
        args.tls_cert = cfg.tls_cert;
    }
    if args.tls_key.is_none() {
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
        validate_public_url(url)?;
    }
    Ok(())
}

/// Strict shape check for the public URL: it ends up in pairing QRs and in
/// menu-bar plugin actions, so "roughly URL-shaped" is not enough — no
/// credentials, no whitespace/control characters, http(s) only, real host.
pub fn validate_public_url(url: &str) -> anyhow::Result<()> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .with_context(|| format!("url {url:?} must start with http:// or https://"))?;
    let host_port = rest.split('/').next().unwrap_or("");
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        anyhow::bail!("url {url:?} contains whitespace or control characters");
    }
    if host_port.contains('@') {
        anyhow::bail!("url {url:?} must not contain credentials");
    }
    // Split host from port ([v6]:port / host:port / bare), then check each
    // part properly — "roughly host-shaped" let `http://:7777` through.
    let port = if let Some(rest6) = host_port.strip_prefix('[') {
        let (h, after) = rest6
            .split_once(']')
            .with_context(|| format!("url {url:?} has an unterminated [ipv6] host"))?;
        // The brackets must hold an actual IPv6 address, and NOTHING may
        // follow them except an optional :port.
        h.parse::<std::net::Ipv6Addr>()
            .ok()
            .with_context(|| format!("url {url:?}: {h:?} is not an IPv6 address"))?;
        match after {
            "" => None,
            p => Some(
                p.strip_prefix(':')
                    .with_context(|| format!("url {url:?} has junk after the [ipv6] host"))?,
            ),
        }
    } else {
        let (host, port) = match host_port.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_port, None),
        };
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            anyhow::bail!("url {url:?} has an invalid host");
        }
        port
    };
    if let Some(p) = port {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            anyhow::bail!("url {url:?} has an invalid port");
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
    }
    if explicit("tls_key") {
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

/// Atomic write: unique temp file in the target dir (create_new — a
/// pre-placed symlink or a concurrent writer cannot make us truncate some
/// other file), 0600, fsync, rename. Dir forced to 0700 even if it already
/// existed looser.
pub fn save(path: &Path, cfg: &ServeConfig) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .context("config path has no parent directory")?;
    // Only OUR default directory gets its permissions forced: --config may
    // point anywhere, and chmod'ing an arbitrary shared parent dir to 0700
    // is not ours to do.
    let owned_dir = default_path()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    if owned_dir.as_deref() == Some(dir) {
        create_private_dir(dir)?;
    } else {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let body = toml::to_string_pretty(cfg).context("serializing config")?;
    let tmp = dir.join(format!(
        ".config.toml.tmp.{}.{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let write =
        write_private_file(&tmp, &body).with_context(|| format!("writing {}", tmp.display()));
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) =
        std::fs::rename(&tmp, path).with_context(|| format!("moving into {}", path.display()))
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::PermissionsExt;
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true).mode(0o700);
    b.create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    // create() is a no-op for a pre-existing dir — tighten it regardless.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("securing {}", dir.display()))
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
        .create_new(true)
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
    fn public_url_validation_rejects_junk_and_accepts_real_shapes() {
        for good in [
            "https://host.ts.net:7777",
            "http://127.0.0.1:7801",
            "https://[fd7a::1]:7777",
            "http://localhost",
            "https://host.ts.net:7777/",
        ] {
            assert!(validate_public_url(good).is_ok(), "{good} should pass");
        }
        for bad in [
            "ftp://host:7777",
            "http://:7777",
            "http://[]",
            "http://host:abc",
            "http://user@host:7777",
            "http://host :7777",
            "http://[fd7a::1:7777",
            "host.ts.net:7777",
        ] {
            assert!(validate_public_url(bad).is_err(), "{bad} should fail");
        }
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
