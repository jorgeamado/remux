//! Guided host setup: bare `remux setup`.
//!
//! Goal: after `brew install remux` / the .deb, ONE command gets a working,
//! login-persistent daemon — probe how this machine is reachable, sort out
//! TLS, write the config file, optionally enroll the login service, then
//! hand back a pairing link minted over the admin socket (never left in
//! service logs).
//!
//! Interactive by design, but every question has a sane default so `--yes`
//! (or a non-TTY stdin) runs straight through. Everything it creates is
//! undone by `remux setup --uninstall` — package managers must not be asked
//! to delete files they didn't install.

use anyhow::{Context, Result};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Options {
    pub yes: bool,
}

pub enum Outcome {
    /// Setup finished; if the service was enrolled a pairing link is ready.
    Configured {
        pair_url: Option<String>,
    },
    Uninstalled,
}

/// How this machine can be reached from the phone, best first.
#[derive(Clone)]
struct Candidate {
    label: String,
    ip: String,
    /// DNS name usable in URLs and certificates (Tailscale MagicDNS).
    dns: Option<String>,
    /// Tailscale gives us real, phone-trusted TLS; nothing else here does.
    tailscale: bool,
}

pub fn run(opts: &Options, state_dir: &Path) -> Result<Outcome> {
    let cfg_path = crate::config::default_path()?;
    println!("remux setup — this will:");
    println!("  1. detect how your phone can reach this machine");
    println!("  2. set up TLS when possible (needed for the iOS app install)");
    println!("  3. write {}", cfg_path.display());
    println!("  4. optionally start remux at login\n");

    // ---- 1. connectivity ----
    let candidates = probe();
    if candidates.is_empty() {
        anyhow::bail!(
            "no usable network address found — configure manually: \
             remux serve --listen <ip>:7777 ... --save-config"
        );
    }
    println!("Reachable via:");
    for (i, c) in candidates.iter().enumerate() {
        println!("  {}) {}", i + 1, c.label);
    }
    // Default to the best candidate that is USABLE unattended — WireGuard
    // has no probeable address, so a --yes/non-TTY run must not land on it.
    let default_pick = candidates
        .iter()
        .position(|c| !c.ip.is_empty())
        .unwrap_or(0)
        + 1;
    let pick = ask_line(
        &format!("Use which? [1-{}]", candidates.len()),
        &default_pick.to_string(),
        opts.yes,
    );
    let mut chosen = pick
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|n| candidates.get(n.saturating_sub(1)))
        .context("not a listed option")?
        .clone();
    if chosen.ip.is_empty() {
        // WireGuard: the address can't be probed, only asked for.
        let ip = ask_line("This machine's WireGuard IP", "", opts.yes);
        chosen.ip = ip.trim().to_string();
        if chosen.ip.is_empty() {
            anyhow::bail!(
                "a WireGuard address is required — rerun setup interactively, \
                 or configure manually with --save-config"
            );
        }
    }
    let chosen = &chosen;

    let port = ask_line("Port", "7777", opts.yes);
    let port: u16 = port.trim().parse().context("not a port number")?;

    // ---- 2. TLS ----
    let mut tls: Option<(PathBuf, PathBuf)> = None;
    let cfg_dir = cfg_path.parent().unwrap().to_path_buf();
    if chosen.tailscale {
        let dns = chosen.dns.as_deref().unwrap_or_default().to_string();
        if !dns.is_empty()
            && ask_yn(
                &format!("Get a TLS certificate for {dns} via `tailscale cert`?"),
                true,
                opts.yes,
            )
        {
            match tailscale_cert(&dns, &cfg_dir) {
                Ok(pair) => tls = Some(pair),
                Err(e) => println!("  tailscale cert failed ({e:#}); continuing without TLS"),
            }
        }
    } else if which("mkcert").is_some() {
        // Advanced path only: a mkcert certificate is signed by a LOCAL CA
        // that every phone must install and explicitly trust first.
        println!(
            "  No trusted-TLS path here. mkcert is installed — you can create a \
             locally-trusted cert (each phone must trust the mkcert CA):\n    \
             mkcert -cert-file {d}/cert.crt -key-file {d}/cert.key {ip}\n  \
             then add tls-cert/tls-key to {p}",
            d = cfg_dir.display(),
            ip = chosen.ip,
            p = cfg_path.display()
        );
    }
    // ---- 3. config ----
    let url_host = chosen.dns.clone().unwrap_or_else(|| chosen.ip.clone());
    let service = ask_yn("Start remux at login?", true, opts.yes);
    // Setup owns the NETWORK story only: start from the existing file so a
    // re-run never wipes session, machine-name, client origins, or other
    // settings the user configured themselves.
    let mut cfg = crate::config::load(&cfg_path)?;
    cfg.listen = Some(format!("{}:{port}", chosen.ip).parse()?);
    if let Some(dns) = &chosen.dns {
        let hosts = cfg.allowed_hosts.get_or_insert_with(Vec::new);
        if !hosts.contains(dns) {
            hosts.push(dns.clone());
        }
    }
    if let Some((cert, key)) = &tls {
        cfg.tls_cert = Some(cert.clone());
        cfg.tls_key = Some(key.clone());
    }
    // The URL scheme must match the FINAL TLS state — a re-run that keeps an
    // existing cert must not write an http:// QR for a TLS listener.
    let scheme = if cfg.tls_cert.is_some() {
        "https"
    } else {
        "http"
    };
    if cfg.tls_cert.is_none() {
        println!(
            "  WARNING: without TLS the iOS home-screen app can't be installed; \
             the browser still works (see README → TLS)."
        );
    }
    if tls.is_none() && cfg.tls_cert.is_some() && chosen.dns.is_none() {
        println!(
            "  NOTE: keeping the existing TLS certificate — make sure it covers \
             {url_host}, or re-run setup on a network with a certificate path"
        );
    }
    cfg.url = Some(format!("{scheme}://{url_host}:{port}"));
    if service {
        // A service's pairing QR would rot unread in a log file — pair via
        // `remux pair` (we mint one right below). Foreground runs keep it.
        cfg.no_pair = Some(true);
    }
    crate::config::save(&cfg_path, &cfg)?;
    println!("  wrote {}", cfg_path.display());
    warn_if_remux_args_overrides();
    if service && !crate::service::propagates_xdg() {
        println!(
            "  WARNING: XDG_CONFIG_HOME/XDG_DATA_HOME are set, but the service \
             manager here (brew services / a packaged unit) won't pass them to \
             the daemon — it will use the DEFAULT config and state paths"
        );
    }

    // ---- 4. app entry, service, pairing ----
    #[cfg(target_os = "macos")]
    if ask_yn(
        "Add remux to ~/Applications (start it from Launchpad/Spotlight)?",
        true,
        opts.yes,
    ) {
        match install_app_bundle() {
            Ok(p) => println!("  created {} (opening it turns remux on)", p.display()),
            Err(e) => println!("  app bundle failed ({e:#}); skipping"),
        }
    }
    if !service {
        println!("\nDone. Start with: remux serve");
        return Ok(Outcome::Configured { pair_url: None });
    }
    // enroll_fresh restarts a service that was already running — a plain
    // enable would leave the OLD daemon up and pair against stale settings.
    crate::service::enroll_fresh()?;
    println!("  service enabled (control it with `remux service on|off|status`)");
    // The daemon needs a moment before its admin socket answers.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let pair_url = loop {
        match crate::admin::request_pairing(state_dir) {
            Ok(url) => break Some(url),
            Err(e) if std::time::Instant::now() >= deadline => {
                println!(
                    "  service started but pairing didn't answer ({e:#}) — \
                     run `remux pair` once it's up"
                );
                break None;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(500)),
        }
    };
    Ok(Outcome::Configured { pair_url })
}

/// `remux setup --uninstall`: remove what setup created on this machine.
/// The config file is kept (it's the user's declared intent; deleting it
/// would surprise) — say so instead.
pub fn uninstall() -> Result<Outcome> {
    if let Err(e) = crate::service::run(crate::service::ServiceCmd::Off) {
        println!("service removal: {e:#} (continuing)");
    } else {
        println!("login service removed");
    }
    #[cfg(target_os = "macos")]
    {
        let app = dirs::home_dir()
            .map(|h| h.join("Applications/remux.app"))
            .filter(|p| p.exists());
        if let Some(app) = app {
            if owns_app_bundle(&app) {
                match std::fs::remove_dir_all(&app) {
                    Ok(()) => println!("removed {}", app.display()),
                    Err(e) => println!("could not remove {}: {e}", app.display()),
                }
            } else {
                println!("left {} alone — not created by remux", app.display());
            }
        }
    }
    match crate::config::default_path() {
        Ok(p) if p.exists() => println!("kept {} — delete it if unwanted", p.display()),
        _ => {}
    }
    Ok(Outcome::Uninstalled)
}

/// The packaged systemd unit lets `$REMUX_ARGS` (from ~/.config/remux/env)
/// override everything — flags outrank the config file by design. A user
/// migrating to the config file needs to know their old env file still wins.
fn warn_if_remux_args_overrides() {
    let Some(home) = dirs::home_dir() else { return };
    // The packaged unit hardcodes %h/.config/remux/env, so check that path
    // regardless of any XDG_CONFIG_HOME override (and the override too).
    let mut env_files = vec![home.join(".config/remux/env")];
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(xdg).join("remux/env");
        if !env_files.contains(&p) {
            env_files.push(p);
        }
    }
    for env_file in env_files {
        if let Ok(body) = std::fs::read_to_string(&env_file) {
            if body.contains("REMUX_ARGS") {
                println!(
                    "  NOTE: {} sets REMUX_ARGS, which OVERRIDES the config file — \
                     empty it out to let the config take effect",
                    env_file.display()
                );
            }
        }
    }
}

// ---------- probes ----------

fn probe() -> Vec<Candidate> {
    let mut out = Vec::new();
    if let Some(c) = tailscale() {
        out.push(c);
    }
    if let Some(c) = zerotier() {
        out.push(c);
    }
    if let Some(c) = wireguard() {
        out.push(c);
    }
    if let Some(ip) = lan_ip() {
        out.push(Candidate {
            label: format!("LAN — {ip} (same wifi only; address may change)"),
            ip,
            dns: None,
            tailscale: false,
        });
    }
    out
}

fn tailscale() -> Option<Candidate> {
    let bin = which("tailscale").or_else(|| {
        let app = PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        app.is_file().then_some(app)
    })?;
    let out = Command::new(bin).args(["status", "--json"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let ip = v["Self"]["TailscaleIPs"]
        .as_array()?
        .iter()
        .filter_map(|x| x.as_str())
        .find(|x| x.contains('.'))? // first IPv4
        .to_string();
    let dns = v["Self"]["DNSName"]
        .as_str()
        .map(|d| d.trim_end_matches('.').to_string())
        .filter(|d| !d.is_empty());
    Some(Candidate {
        label: match &dns {
            Some(d) => format!("Tailscale — {d} ({ip}); trusted TLS available"),
            None => format!("Tailscale — {ip}"),
        },
        ip,
        dns,
        tailscale: true,
    })
}

fn zerotier() -> Option<Candidate> {
    let bin = which("zerotier-cli")?;
    let out = Command::new(bin)
        .args(["-j", "listnetworks"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // often requires root; treat as "not usable"
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let ip = v
        .as_array()?
        .iter()
        .flat_map(|n| n["assignedAddresses"].as_array().into_iter().flatten())
        .filter_map(|a| a.as_str())
        .filter_map(|a| a.split('/').next())
        .find(|a| a.contains('.'))?
        .to_string();
    Some(Candidate {
        label: format!("ZeroTier — {ip}"),
        ip,
        dns: None,
        tailscale: false,
    })
}

/// WireGuard has no query for "my address" that works everywhere; probing
/// only detects that an interface is up. The address is asked for LATER and
/// only if the user actually picks this candidate — probing must never
/// prompt, and `--yes`/non-TTY runs must stay prompt-free.
fn wireguard() -> Option<Candidate> {
    let bin = which("wg")?;
    let out = Command::new(bin)
        .args(["show", "interfaces"])
        .output()
        .ok()?;
    let ifs = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || ifs.is_empty() {
        return None;
    }
    Some(Candidate {
        label: format!("WireGuard ({ifs}) — asks for this machine's WireGuard IP"),
        ip: String::new(), // filled in after selection
        dns: None,
        tailscale: false,
    })
}

/// Local IP on the default route. A UDP connect() sends no packet — it just
/// asks the kernel which source address it would pick.
fn lan_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:80").ok()?; // TEST-NET; never actually sent to
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then(|| ip.to_string())
}

fn tailscale_cert(dns: &str, dir: &Path) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let cert = dir.join("cert.crt");
    let key = dir.join("cert.key");
    let bin = which("tailscale")
        .or_else(|| {
            let app = PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
            app.is_file().then_some(app)
        })
        .context("tailscale binary not found")?;
    let out = Command::new(bin)
        .arg("cert")
        .arg("--cert-file")
        .arg(&cert)
        .arg("--key-file")
        .arg(&key)
        .arg(dns)
        .output()
        .context("running tailscale cert")?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok((cert, key))
}

/// A minimal, locally-generated app bundle whose sole job is `remux service
/// on` — a Launchpad/Spotlight way to turn remux on when it isn't enrolled
/// at login. Local generation avoids Gatekeeper quarantine (nothing was
/// downloaded); LSUIElement keeps it out of the Dock. It hard-codes the
/// binary path from generation time — re-run `remux setup` after moving the
/// binary.
#[cfg(target_os = "macos")]
const APP_BUNDLE_ID: &str = "io.github.jorgeamado.remux.launcher";

/// True when the bundle at `app` is one we generated (identified by our
/// CFBundleIdentifier). Setup must neither overwrite nor delete a foreign
/// bundle that happens to be called remux.app.
#[cfg(target_os = "macos")]
fn owns_app_bundle(app: &Path) -> bool {
    // Match the exact key/value pair, not a substring anywhere in the file:
    // only a bundle whose CFBundleIdentifier IS ours counts as ours.
    let needle = format!("<key>CFBundleIdentifier</key><string>{APP_BUNDLE_ID}</string>");
    std::fs::read_to_string(app.join("Contents/Info.plist"))
        .map(|p| {
            p.split_whitespace()
                .collect::<String>()
                .contains(&needle.split_whitespace().collect::<String>())
        })
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn install_app_bundle() -> Result<PathBuf> {
    // Stable path (Homebrew opt symlink) so a brew upgrade doesn't strand
    // the launcher; single-quote shell escaping handles every legal path.
    let bin = crate::service::stable_exe()?;
    let quoted = format!("'{}'", bin.display().to_string().replace('\'', r"'\''"));
    let apps = dirs::home_dir()
        .context("no home directory")?
        .join("Applications");
    let app = apps.join("remux.app");
    if app.exists() && !owns_app_bundle(&app) {
        anyhow::bail!(
            "{} exists but wasn't created by remux — refusing to overwrite it",
            app.display()
        );
    }
    let macos = app.join("Contents/MacOS");
    std::fs::create_dir_all(&macos)?;
    std::fs::write(
        app.join("Contents/Info.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>remux</string>
  <key>CFBundleIdentifier</key><string>{APP_BUNDLE_ID}</string>
  <key>CFBundleExecutable</key><string>launcher</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSUIElement</key><true/>
</dict>
</plist>
"#
        ),
    )?;
    let launcher = macos.join("launcher");
    std::fs::write(&launcher, format!("#!/bin/sh\nexec {quoted} service on\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(app)
}

// ---------- small IO helpers ----------

fn interactive() -> bool {
    std::io::stdin().is_terminal()
}

fn ask_line(prompt: &str, default: &str, assume_yes: bool) -> String {
    if assume_yes || !interactive() {
        return default.to_string();
    }
    print!(
        "{prompt}{}: ",
        if default.is_empty() {
            String::new()
        } else {
            format!(" [{default}]")
        }
    );
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() || line.trim().is_empty() {
        return default.to_string();
    }
    line.trim().to_string()
}

fn ask_yn(prompt: &str, default: bool, assume_yes: bool) -> bool {
    let d = if default { "Y/n" } else { "y/N" };
    let ans = ask_line(&format!("{prompt} [{d}]"), "", assume_yes);
    match ans.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}
