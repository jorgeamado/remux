//! Login-service control: `remux service status|start|stop|on|off`.
//!
//! Service state has two independent axes — *running* (right now) and
//! *enabled* (starts at login) — and the verbs keep them apart: `start`/
//! `stop` are transient, `on`/`off` are persistent (enable+start /
//! disable+stop). State persists in the platform's service manager, never in
//! a remux-owned file.
//!
//! Backends:
//! - macOS + Homebrew install → `brew services` (it owns the LaunchAgent):
//!   on=start, off=stop, start=run (no login registration), stop=kill
//!   (keeps registration). If the binary is Homebrew's but `brew` cannot be
//!   found, that's an error — falling back to our own LaunchAgent would run
//!   a SECOND daemon next to the brew-managed one.
//! - macOS otherwise → our own LaunchAgent plist; enabled == plist installed.
//! - Linux → `systemctl --user` (unit from the .deb, or a generated user
//!   unit for tarball installs, marked so we only ever delete our own).

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

#[derive(clap::Subcommand, Debug)]
pub enum ServiceCmd {
    /// Show whether the daemon service is running and enabled at login.
    Status {
        /// Machine-readable one-liner: "<running|stopped> <enabled|disabled>".
        #[arg(long)]
        short: bool,
    },
    /// Start now, without changing the at-login setting.
    Start,
    /// Stop now, without changing the at-login setting.
    Stop,
    /// Enable at login and start now.
    On,
    /// Disable at login and stop now.
    Off,
}

pub fn run(cmd: ServiceCmd) -> Result<()> {
    let b = backend()?;
    match cmd {
        ServiceCmd::Status { short } => {
            let (running, enabled) = b.status()?;
            if short {
                println!(
                    "{} {}",
                    if running { "running" } else { "stopped" },
                    if enabled { "enabled" } else { "disabled" }
                );
            } else {
                println!(
                    "remux daemon: {}, {} at login ({})",
                    if running { "running" } else { "stopped" },
                    if enabled { "starts" } else { "does not start" },
                    b.name()
                );
            }
        }
        ServiceCmd::Start => b.start()?,
        ServiceCmd::Stop => b.stop()?,
        ServiceCmd::On => b.on()?,
        ServiceCmd::Off => b.off()?,
    }
    Ok(())
}

/// Enable at login and make sure the RUNNING daemon reflects the current
/// config: a plain `on` leaves an already-running service untouched
/// (`systemctl enable --now`, `brew services start`), which would let setup
/// pair against a daemon still using the old settings.
pub fn enroll_fresh() -> Result<()> {
    let b = backend()?;
    b.on()?;
    b.restart()
}

trait Backend {
    fn name(&self) -> &'static str;
    fn status(&self) -> Result<(bool, bool)>;
    fn start(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    fn on(&self) -> Result<()>;
    fn off(&self) -> Result<()>;
    /// Restart the running instance (used after config changes).
    fn restart(&self) -> Result<()>;
}

#[cfg(target_os = "macos")]
fn backend() -> Result<Box<dyn Backend>> {
    if let Some(brew) = brew_backend()? {
        return Ok(Box::new(brew));
    }
    Ok(Box::new(LaunchAgent::locate()?))
}

#[cfg(target_os = "linux")]
fn backend() -> Result<Box<dyn Backend>> {
    Ok(Box::new(SystemdUser))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn backend() -> Result<Box<dyn Backend>> {
    anyhow::bail!("service management is only supported on macOS and Linux")
}

fn run_out(cmd: &mut Command) -> Result<String> {
    let what = format!("{cmd:?}");
    let out = cmd.output().with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The path this binary should be referenced by from service units and
/// launchers: Homebrew's versioned Cellar path is rewritten to the stable
/// `opt` symlink so a `brew upgrade` doesn't strand them.
pub fn stable_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating own binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let s = exe.to_string_lossy();
    if let Some(idx) = s.find("/Cellar/remux/") {
        let prefix = &s[..idx];
        let opt = PathBuf::from(format!("{prefix}/opt/remux/bin/remux"));
        if opt.exists() {
            return Ok(opt);
        }
    }
    Ok(exe)
}

/// XDG overrides in *our* environment must reach the service too, or setup
/// under a custom XDG_CONFIG_HOME writes a config the service never reads
/// (and setup then waits on an admin socket in the wrong state dir).
fn xdg_env() -> Vec<(&'static str, String)> {
    ["XDG_CONFIG_HOME", "XDG_DATA_HOME"]
        .into_iter()
        .filter_map(|k| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| (k, v))
        })
        .collect()
}

// ---------- macOS: Homebrew ----------

#[cfg(target_os = "macos")]
struct BrewServices {
    brew: PathBuf,
}

/// Homebrew-installed remux is managed through `brew services`. Detection
/// must not depend on the caller's PATH (the app launcher and SwiftBar run
/// with a minimal GUI environment): check the standard prefixes too.
#[cfg(target_os = "macos")]
fn brew_backend() -> Result<Option<BrewServices>> {
    let exe = std::env::current_exe().context("locating own binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let under_brew = exe.components().any(|c| c.as_os_str() == "Cellar");
    if !under_brew {
        return Ok(None);
    }
    let brew = which("brew")
        .or_else(|| {
            ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
                .into_iter()
                .map(PathBuf::from)
                .find(|p| p.is_file())
        })
        .context(
            "this remux was installed by Homebrew but `brew` was not found — \
             refusing to set up a second, conflicting service",
        )?;
    Ok(Some(BrewServices { brew }))
}

#[cfg(target_os = "macos")]
impl Backend for BrewServices {
    fn name(&self) -> &'static str {
        "brew services"
    }
    fn status(&self) -> Result<(bool, bool)> {
        let out = run_out(Command::new(&self.brew).args(["services", "info", "remux", "--json"]))?;
        let v: serde_json::Value = serde_json::from_str(&out).context("brew services info json")?;
        let info = v.get(0).cloned().unwrap_or_default();
        let running = info["running"].as_bool().unwrap_or(false);
        // `registered` is the at-login axis; a transient `brew services run`
        // job is loaded and running but NOT registered.
        let enabled = info["registered"].as_bool().unwrap_or(false);
        Ok((running, enabled))
    }
    fn start(&self) -> Result<()> {
        run_out(Command::new(&self.brew).args(["services", "run", "remux"])).map(drop)
    }
    fn stop(&self) -> Result<()> {
        run_out(Command::new(&self.brew).args(["services", "kill", "remux"])).map(drop)
    }
    fn on(&self) -> Result<()> {
        run_out(Command::new(&self.brew).args(["services", "start", "remux"])).map(drop)
    }
    fn off(&self) -> Result<()> {
        run_out(Command::new(&self.brew).args(["services", "stop", "remux"])).map(drop)
    }
    fn restart(&self) -> Result<()> {
        run_out(Command::new(&self.brew).args(["services", "restart", "remux"])).map(drop)
    }
}

// ---------- macOS: our own LaunchAgent ----------

#[cfg(target_os = "macos")]
struct LaunchAgent {
    plist: PathBuf,
    uid: u32,
}

#[cfg(target_os = "macos")]
const LABEL: &str = "io.github.jorgeamado.remux";

#[cfg(target_os = "macos")]
impl LaunchAgent {
    fn locate() -> Result<Self> {
        let home = dirs::home_dir().context("no home directory")?;
        Ok(Self {
            plist: home.join(format!("Library/LaunchAgents/{LABEL}.plist")),
            uid: unsafe { libc_getuid() },
        })
    }

    fn domain_target(&self) -> String {
        format!("gui/{}/{}", self.uid, LABEL)
    }

    /// bootout that tolerates exactly one failure mode: the job not being
    /// loaded. Anything else is reported — deleting the plist after a real
    /// bootout failure would leave a running job nobody owns.
    fn bootout_if_loaded(&self) -> Result<()> {
        let out = Command::new("launchctl")
            .args(["bootout", &self.domain_target()])
            .output()
            .context("running launchctl bootout")?;
        if out.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        // launchctl exits 3 / prints this when the service isn't loaded.
        if err.contains("No such process")
            || err.contains("Could not find")
            || out.status.code() == Some(3)
        {
            return Ok(());
        }
        anyhow::bail!("launchctl bootout failed: {}", err.trim());
    }

    fn install_plist(&self) -> Result<()> {
        let bin = crate::service::stable_exe()?;
        let home = dirs::home_dir().context("no home directory")?;
        let logs = home.join("Library/Logs/remux");
        std::fs::create_dir_all(&logs)?;
        let log = logs.join("daemon.log");
        let mut env_entries = String::from(
            "    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>\n",
        );
        for (k, v) in xdg_env() {
            env_entries.push_str(&format!(
                "    <key>{}</key><string>{}</string>\n",
                k,
                xml_escape(&v)
            ));
        }
        // ProgramArguments is argv — no shell, no expansion. PATH is set
        // explicitly because the daemon invokes `tmux` by name, and a
        // launchd job inherits almost nothing.
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin}</string>
    <string>serve</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>WorkingDirectory</key><string>{home}</string>
  <key>EnvironmentVariables</key>
  <dict>
{env_entries}  </dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            bin = xml_escape(&bin.display().to_string()),
            home = xml_escape(&home.display().to_string()),
            log = xml_escape(&log.display().to_string()),
        );
        std::fs::create_dir_all(self.plist.parent().unwrap())?;
        std::fs::write(&self.plist, plist)
            .with_context(|| format!("writing {}", self.plist.display()))?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
unsafe fn libc_getuid() -> u32 {
    // std has no getuid; a crate dependency for one call isn't worth it.
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}

#[cfg(target_os = "macos")]
impl Backend for LaunchAgent {
    fn name(&self) -> &'static str {
        "launchd"
    }
    fn status(&self) -> Result<(bool, bool)> {
        let enabled = self.plist.exists();
        let running = Command::new("launchctl")
            .args(["print", &self.domain_target()])
            .output()
            .map(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).contains("state = running")
            })
            .unwrap_or(false);
        Ok((running, enabled))
    }
    fn start(&self) -> Result<()> {
        if !self.plist.exists() {
            anyhow::bail!(
                "service not installed — run `remux service on` (or `remux setup`) first"
            );
        }
        // Idempotent: bootstrap if needed, then kick.
        let _ = Command::new("launchctl")
            .args(["bootstrap", &format!("gui/{}", self.uid)])
            .arg(&self.plist)
            .output();
        run_out(Command::new("launchctl").args(["kickstart", &self.domain_target()])).map(drop)
    }
    fn stop(&self) -> Result<()> {
        run_out(Command::new("launchctl").args(["kill", "SIGTERM", &self.domain_target()]))
            .map(drop)
    }
    fn on(&self) -> Result<()> {
        self.install_plist()?;
        let _ = Command::new("launchctl")
            .args(["enable", &self.domain_target()])
            .output();
        // Re-bootstrap so the job definitely runs from the fresh plist.
        self.bootout_if_loaded()?;
        run_out(
            Command::new("launchctl")
                .args(["bootstrap", &format!("gui/{}", self.uid)])
                .arg(&self.plist),
        )
        .map(drop)
    }
    fn off(&self) -> Result<()> {
        self.bootout_if_loaded()?;
        if self.plist.exists() {
            std::fs::remove_file(&self.plist)
                .with_context(|| format!("removing {}", self.plist.display()))?;
        }
        Ok(())
    }
    fn restart(&self) -> Result<()> {
        run_out(Command::new("launchctl").args(["kickstart", "-k", &self.domain_target()]))
            .map(drop)
    }
}

// ---------- Linux: systemd user unit ----------

#[cfg(target_os = "linux")]
struct SystemdUser;

#[cfg(target_os = "linux")]
const UNIT_MARKER: &str = "# generated by `remux service` — safe to delete";

#[cfg(target_os = "linux")]
impl SystemdUser {
    fn user_unit_path() -> Result<PathBuf> {
        Ok(dirs::home_dir()
            .context("no home directory")?
            .join(".config/systemd/user/remux.service"))
    }

    /// The .deb ships a user unit; a tarball install has none. Generate one
    /// only when systemd doesn't already know the name — a generated copy
    /// must never shadow a packaged unit.
    fn ensure_unit(&self) -> Result<()> {
        let known = Command::new("systemctl")
            .args(["--user", "cat", "remux.service"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if known {
            return Ok(());
        }
        let bin = crate::service::stable_exe()?;
        let bin = bin.to_str().context("binary path is not UTF-8")?;
        // systemd unit syntax gives % and \ and quotes special meaning; a
        // path using them is rare enough that refusing beats mis-quoting.
        if bin
            .chars()
            .any(|c| matches!(c, '%' | '\\' | '"' | '\'' | '\n'))
        {
            anyhow::bail!(
                "binary path {bin:?} contains characters unsafe in a systemd unit — \
                 install the unit manually (see packaging/remux.service)"
            );
        }
        let path = Self::user_unit_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let unit = format!(
            "{UNIT_MARKER}\n[Unit]\nDescription=remux — your persistent tmux session, on your phone\n\
             After=network-online.target\n\n\
             [Service]\nExecStart=\"{bin}\" serve\nRestart=on-failure\nRestartSec=2\n{env}\n\
             [Install]\nWantedBy=default.target\n",
            env = xdg_env()
                .into_iter()
                .map(|(k, v)| format!("Environment=\"{k}={v}\"\n"))
                .collect::<String>(),
        );
        std::fs::write(&path, unit)?;
        run_out(Command::new("systemctl").args(["--user", "daemon-reload"])).map(drop)
    }

    /// Remove the unit file only if WE generated it (marker present).
    fn remove_generated_unit(&self) -> Result<()> {
        let path = Self::user_unit_path()?;
        match std::fs::read_to_string(&path) {
            Ok(body) if body.starts_with(UNIT_MARKER) => {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                run_out(Command::new("systemctl").args(["--user", "daemon-reload"])).map(drop)
            }
            _ => Ok(()),
        }
    }
}

#[cfg(target_os = "linux")]
impl Backend for SystemdUser {
    fn name(&self) -> &'static str {
        "systemd --user"
    }
    fn status(&self) -> Result<(bool, bool)> {
        let active = Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", "remux.service"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let enabled = Command::new("systemctl")
            .args(["--user", "is-enabled", "--quiet", "remux.service"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        Ok((active, enabled))
    }
    fn start(&self) -> Result<()> {
        self.ensure_unit()?;
        run_out(Command::new("systemctl").args(["--user", "start", "remux.service"])).map(drop)
    }
    fn stop(&self) -> Result<()> {
        run_out(Command::new("systemctl").args(["--user", "stop", "remux.service"])).map(drop)
    }
    fn on(&self) -> Result<()> {
        self.ensure_unit()?;
        run_out(Command::new("systemctl").args(["--user", "enable", "--now", "remux.service"]))
            .map(drop)
    }
    fn off(&self) -> Result<()> {
        run_out(Command::new("systemctl").args(["--user", "disable", "--now", "remux.service"]))
            .map(drop)?;
        self.remove_generated_unit()
    }
    fn restart(&self) -> Result<()> {
        run_out(Command::new("systemctl").args(["--user", "restart", "remux.service"])).map(drop)
    }
}

#[cfg(target_os = "macos")]
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}
