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
//!   (keeps registration).
//! - macOS otherwise → our own LaunchAgent plist; enabled == plist installed.
//! - Linux → `systemctl --user` (unit from the .deb, or a generated user
//!   unit for tarball installs).

use anyhow::{bail, Context, Result};
#[cfg(target_os = "macos")]
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

trait Backend {
    fn name(&self) -> &'static str;
    fn status(&self) -> Result<(bool, bool)>;
    fn start(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    fn on(&self) -> Result<()>;
    fn off(&self) -> Result<()>;
}

fn backend() -> Result<Box<dyn Backend>> {
    #[cfg(target_os = "macos")]
    {
        if let Some(brew) = brew_backend()? {
            return Ok(Box::new(brew));
        }
        Ok(Box::new(LaunchAgent::locate()?))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(SystemdUser))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        bail!("service management is only supported on macOS and Linux")
    }
}

fn run_out(cmd: &mut Command) -> Result<String> {
    let what = format!("{cmd:?}");
    let out = cmd.output().with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------- macOS: Homebrew ----------

#[cfg(target_os = "macos")]
struct BrewServices;

/// Homebrew-installed remux is managed through `brew services` — it owns the
/// LaunchAgent lifecycle and survives brew upgrades (stable opt path). We
/// detect it by our own binary living under the Homebrew prefix.
#[cfg(target_os = "macos")]
fn brew_backend() -> Result<Option<BrewServices>> {
    let exe = std::env::current_exe().context("locating own binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let under_brew = exe.components().any(|c| c.as_os_str() == "Cellar");
    if under_brew && which("brew").is_some() {
        Ok(Some(BrewServices))
    } else {
        Ok(None)
    }
}

#[cfg(target_os = "macos")]
impl Backend for BrewServices {
    fn name(&self) -> &'static str {
        "brew services"
    }
    fn status(&self) -> Result<(bool, bool)> {
        let out = run_out(Command::new("brew").args(["services", "info", "remux", "--json"]))?;
        let v: serde_json::Value = serde_json::from_str(&out).context("brew services info json")?;
        let info = v.get(0).cloned().unwrap_or_default();
        let running = info["running"].as_bool().unwrap_or(false);
        let enabled = info["register_at_login"]
            .as_bool()
            .or_else(|| info["loaded"].as_bool())
            .unwrap_or(false);
        Ok((running, enabled))
    }
    fn start(&self) -> Result<()> {
        run_out(Command::new("brew").args(["services", "run", "remux"])).map(drop)
    }
    fn stop(&self) -> Result<()> {
        run_out(Command::new("brew").args(["services", "kill", "remux"])).map(drop)
    }
    fn on(&self) -> Result<()> {
        run_out(Command::new("brew").args(["services", "start", "remux"])).map(drop)
    }
    fn off(&self) -> Result<()> {
        run_out(Command::new("brew").args(["services", "stop", "remux"])).map(drop)
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

    fn install_plist(&self) -> Result<()> {
        let bin = std::env::current_exe().context("locating own binary")?;
        let home = dirs::home_dir().context("no home directory")?;
        let logs = home.join("Library/Logs/remux");
        std::fs::create_dir_all(&logs)?;
        let log = logs.join("daemon.log");
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
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
  </dict>
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
    // std has no getuid; users::get_current_uid would add a dep for one call.
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
            bail!("service not installed — run `remux service on` (or `remux setup`) first");
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
        // bootout first so a stale copy of the job can't shadow the new plist.
        let _ = Command::new("launchctl")
            .args(["bootout", &self.domain_target()])
            .output();
        run_out(
            Command::new("launchctl")
                .args(["bootstrap", &format!("gui/{}", self.uid)])
                .arg(&self.plist),
        )
        .map(drop)
    }
    fn off(&self) -> Result<()> {
        let _ = Command::new("launchctl")
            .args(["bootout", &self.domain_target()])
            .output();
        if self.plist.exists() {
            std::fs::remove_file(&self.plist)
                .with_context(|| format!("removing {}", self.plist.display()))?;
        }
        Ok(())
    }
}

// ---------- Linux: systemd user unit ----------

#[cfg(target_os = "linux")]
struct SystemdUser;

#[cfg(target_os = "linux")]
impl SystemdUser {
    /// The .deb ships a user unit; a tarball install has none. Generate one
    /// in ~/.config/systemd/user only when systemd doesn't already know the
    /// name — a generated copy must never shadow a packaged unit.
    fn ensure_unit(&self) -> Result<()> {
        let known = Command::new("systemctl")
            .args(["--user", "cat", "remux.service"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if known {
            return Ok(());
        }
        let bin = std::env::current_exe().context("locating own binary")?;
        let dir = dirs::home_dir()
            .context("no home directory")?
            .join(".config/systemd/user");
        std::fs::create_dir_all(&dir)?;
        let unit = format!(
            "[Unit]\nDescription=remux — your persistent tmux session, on your phone\n\
             After=network-online.target\n\n\
             [Service]\nExecStart={} serve\nRestart=on-failure\nRestartSec=2\n\n\
             [Install]\nWantedBy=default.target\n",
            bin.display()
        );
        std::fs::write(dir.join("remux.service"), unit)?;
        run_out(Command::new("systemctl").args(["--user", "daemon-reload"])).map(drop)
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
            .map(drop)
    }
}

#[cfg(target_os = "macos")]
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}
