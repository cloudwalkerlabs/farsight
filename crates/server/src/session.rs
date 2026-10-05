//! The session's environment (docs/design.md §6): a private runtime
//! directory, a private D-Bus session bus, PipeWire with no hardware, and
//! the desktop, supervised.
//!
//! Children get an environment built here, not the one the server was
//! started with: a few variables from the user's login (home, locale,
//! search paths) and the session's own sockets. Each child leads its own
//! process group, dies with the server (`PR_SET_PDEATHSIG`) and is sent
//! `SIGTERM` when the session ends.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

/// `sun_path` holds 107 bytes and a NUL.
const SUN_PATH_MAX: usize = 107;

/// Variables kept from the server's own environment: who the user is and
/// how they like things, but no sockets or display.
const KEEP: &[&str] = &[
    "HOME", "USER", "LOGNAME", "SHELL", "PATH", "LANG", "LANGUAGE", "TZ", "XDG_CONFIG_HOME", "XDG_DATA_HOME",
    "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_CONFIG_DIRS", "XDG_DATA_DIRS", "XKB_DEFAULT_RULES",
    "XKB_DEFAULT_MODEL", "XKB_DEFAULT_LAYOUT", "XKB_DEFAULT_VARIANT", "XKB_DEFAULT_OPTIONS",
];

/// One graph cycle is one 5 ms Opus frame (§8).
const PIPEWIRE_CONF: &str = "\
context.properties = {
    default.clock.rate          = 48000
    default.clock.allowed-rates = [ 48000 ]
    default.clock.quantum       = 240
    default.clock.min-quantum   = 240
    default.clock.max-quantum   = 240
}
";

/// No hardware at all: the session's only devices are the server's own
/// streams (§8).
const WIREPLUMBER_CONF: &str = "\
wireplumber.profiles = {
  farsight = {
    inherits = [ main, mixin.systemwide-session ]
    hardware.audio = disabled
    hardware.bluetooth = disabled
    hardware.video-capture = disabled
  }
}
";

/// The session's environment and its long-lived children.
pub struct Session {
    pub runtime_dir: PathBuf,
    /// Remove the directory at the end: we made it.
    owned_dir: bool,
    env: Vec<(OsString, OsString)>,
    services: Vec<(&'static str, Child)>,
    pub audio: bool,
}

impl Session {
    /// Creates the runtime directory and starts the bus and the audio
    /// daemons. `port` names the directory; `audio` false skips PipeWire.
    pub fn start(port: u16, audio: bool) -> anyhow::Result<Self> {
        let (runtime_dir, owned_dir) = runtime_dir(port)?;
        let longest = runtime_dir.join("pulse/native");
        if longest.as_os_str().len() > SUN_PATH_MAX {
            bail!(
                "{} is too long for a socket path ({} bytes, at most {SUN_PATH_MAX}); use a shorter runtime directory",
                longest.display(),
                longest.as_os_str().len()
            );
        }
        tracing::info!(dir = %runtime_dir.display(), "session runtime directory");

        let mut env: Vec<(OsString, OsString)> =
            std::env::vars_os().filter(|(k, _)| KEEP.iter().any(|keep| OsStr::new(keep) == k)).collect();
        if !env.iter().any(|(k, _)| k == "PATH") {
            env.push(("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()));
        }
        let mut set = |k: &str, v: OsString| {
            env.retain(|(key, _)| key != k);
            env.push((k.into(), v));
        };
        set("XDG_RUNTIME_DIR", runtime_dir.clone().into());
        set("XDG_SESSION_TYPE", "wayland".into());
        set("JACK_NO_START_SERVER", "1".into());
        set("JACK_DEFAULT_SERVER", format!("farsight-{port}").into());
        let mut pulse = OsString::from("unix:");
        pulse.push(runtime_dir.join("pulse/native"));
        set("PULSE_SERVER", pulse);
        let mut session = Session { runtime_dir, owned_dir, env, services: Vec::new(), audio: false };

        let bus = session.start_bus()?;
        session.set("DBUS_SESSION_BUS_ADDRESS", bus.into());
        if audio {
            match session.start_audio(port) {
                Ok(()) => session.audio = true,
                Err(err) => tracing::warn!("{err:#}; the session has no audio"),
            }
        }
        Ok(session)
    }

    fn set(&mut self, key: &str, value: OsString) {
        self.env.retain(|(k, _)| k != key);
        self.env.push((key.into(), value));
    }

    /// A command with the session's environment, in its own process group,
    /// that dies with the server.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut c = Command::new(program);
        c.env_clear().envs(self.env.iter().map(|(k, v)| (k, v))).current_dir(self.home()).process_group(0);
        // SAFETY: prctl and sigprocmask are async-signal-safe.
        unsafe {
            c.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                // The server blocks the signals it waits for; children must
                // not inherit that.
                let mut none: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut none);
                libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
                Ok(())
            });
        }
        c
    }

    fn home(&self) -> PathBuf {
        self.env.iter().find(|(k, _)| k == "HOME").map_or_else(|| PathBuf::from("/"), |(_, v)| PathBuf::from(v))
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.runtime_dir.join(name)
    }

    /// `dbus-daemon` on `<rundir>/bus`, once it is listening.
    fn start_bus(&mut self) -> anyhow::Result<String> {
        let address = format!("unix:path={}", self.path("bus").display());
        let mut child = self
            .command("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile", "--print-address"])
            .arg(format!("--address={address}"))
            .stdout(Stdio::piped())
            .spawn()
            .context("starting dbus-daemon")?;
        // It prints its address once it listens.
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line)?;
        if line.trim().is_empty() {
            bail!("dbus-daemon exited: {:?}", child.wait()?);
        }
        tracing::info!(address = line.trim(), "session bus");
        self.services.push(("dbus-daemon", child));
        Ok(line.trim().to_string())
    }

    /// PipeWire, WirePlumber and pipewire-pulse, with their own
    /// configuration and state (§8).
    fn start_audio(&mut self, port: u16) -> anyhow::Result<()> {
        let config = self.path("config");
        write_file(&config.join("pipewire/pipewire.conf.d/farsight.conf"), PIPEWIRE_CONF)?;
        write_file(&config.join("pipewire/pipewire-pulse.conf.d/farsight.conf"), "")?;
        write_file(&config.join("wireplumber/wireplumber.conf.d/farsight.conf"), WIREPLUMBER_CONF)?;
        let state = state_home(&self.home(), &self.env).join(format!("farsight/{port}"));
        std::fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;
        let daemon = |s: &Self, program: &str| {
            let mut c = s.command(program);
            c.env("XDG_CONFIG_HOME", &config).env("XDG_STATE_HOME", &state).stdin(Stdio::null());
            c
        };

        let pipewire = daemon(self, "pipewire").spawn().context("starting pipewire")?;
        self.services.push(("pipewire", pipewire));
        wait_for(&self.path("pipewire-0"), Duration::from_secs(5)).context("pipewire did not start")?;
        let wireplumber =
            daemon(self, "wireplumber").args(["--profile", "farsight"]).spawn().context("starting wireplumber")?;
        self.services.push(("wireplumber", wireplumber));
        let pulse = daemon(self, "pipewire-pulse").spawn().context("starting pipewire-pulse")?;
        self.services.push(("pipewire-pulse", pulse));
        wait_for(&self.path("pulse/native"), Duration::from_secs(5)).context("pipewire-pulse did not start")?;
        tracing::info!(state = %state.display(), "session audio started");
        Ok(())
    }

    /// Reports services that died; they aren't restarted.
    pub fn check_services(&mut self) {
        self.services.retain_mut(|(name, child)| match child.try_wait() {
            Ok(Some(status)) => {
                tracing::warn!(service = *name, %status, "session service exited");
                false
            }
            _ => true,
        });
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for (_, child) in self.services.iter().rev() {
            terminate(child.id());
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        for (name, child) in &mut self.services {
            while child.try_wait().ok().flatten().is_none() {
                if Instant::now() >= deadline {
                    tracing::warn!(service = *name, "killing");
                    // SAFETY: plain syscall.
                    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if self.owned_dir {
            let _ = std::fs::remove_dir_all(&self.runtime_dir);
        }
    }
}

/// `SIGTERM` to a child's process group.
fn terminate(pid: u32) {
    // SAFETY: plain syscall; each child leads its own process group.
    unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
}

/// The first of `$RUNTIME_DIRECTORY` (systemd), `$XDG_RUNTIME_DIR/farsight-<port>`
/// and a fresh directory under `/tmp`; and whether we created it.
fn runtime_dir(port: u16) -> anyhow::Result<(PathBuf, bool)> {
    if let Some(dir) = std::env::var_os("RUNTIME_DIRECTORY").filter(|d| !d.is_empty()) {
        // systemd separates several with ':'; the first is ours.
        let first = dir.as_encoded_bytes().split(|&b| b == b':').next().unwrap_or_default().to_vec();
        // SAFETY: a prefix of an OsStr's bytes, cut at an ASCII byte.
        let dir = PathBuf::from(unsafe { OsString::from_encoded_bytes_unchecked(first) });
        clear_dir(&dir)?;
        return Ok((dir, false));
    }
    if let Some(base) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        let dir = PathBuf::from(base).join(format!("farsight-{port}"));
        // The UDP port is already ours, so anything here is stale.
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing the stale {}", dir.display()))?;
        }
        std::fs::DirBuilder::new().mode(0o700).create(&dir).with_context(|| format!("creating {}", dir.display()))?;
        return Ok((dir, true));
    }
    let mut template = format!("/tmp/farsight-{port}-XXXXXX\0").into_bytes();
    // SAFETY: a NUL-terminated, writable template.
    let p = unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) };
    if p.is_null() {
        return Err(std::io::Error::last_os_error()).context("creating a runtime directory under /tmp");
    }
    template.pop();
    Ok((PathBuf::from(String::from_utf8(template)?), true))
}

/// Empties a directory we were given, and makes sure only we can use it.
fn clear_dir(dir: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn state_home(home: &Path, env: &[(OsString, OsString)]) -> PathBuf {
    env.iter()
        .find(|(k, v)| k == "XDG_STATE_HOME" && !v.is_empty())
        .map_or_else(|| home.join(".local/state"), |(_, v)| PathBuf::from(v))
}

fn write_file(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn wait_for(path: &Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() >= deadline {
            bail!("{} never appeared", path.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// When the desktop exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Restart {
    /// Restart it after a crash; a clean exit (the user logged out) ends
    /// the session.
    OnFailure,
    /// Always restart it.
    Always,
    /// End the session.
    Never,
}

/// Crashes this close together are a crash loop; the session ends.
const CRASH_WINDOW: Duration = Duration::from_secs(60);
const MAX_CRASHES: usize = 5;

/// The desktop (or the kiosk app), restarted as `Restart` says.
pub struct Desktop {
    cmd: Vec<String>,
    /// The host's Wayland socket, relative to the runtime directory.
    socket: OsString,
    restart: Restart,
    child: Option<Child>,
    /// Recent unclean exits.
    crashes: Vec<Instant>,
    /// When to start it again, after a backoff.
    restart_at: Option<Instant>,
}

/// What the supervisor wants from the server.
#[derive(Debug, PartialEq, Eq)]
pub enum Supervision {
    Running,
    /// The desktop is gone for good: end the session.
    End,
}

impl Desktop {
    pub fn new(cmd: Vec<String>, socket: OsString, restart: Restart) -> Self {
        Self { cmd, socket, restart, child: None, crashes: Vec::new(), restart_at: None }
    }

    pub fn start(&mut self, session: &Session) -> anyhow::Result<()> {
        tracing::info!(cmd = ?self.cmd, socket = ?self.socket, "starting the desktop");
        let child = session
            .command(&self.cmd[0])
            .args(&self.cmd[1..])
            .env("WAYLAND_DISPLAY", &self.socket)
            .stdin(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", self.cmd[0]))?;
        self.child = Some(child);
        Ok(())
    }

    /// Called periodically: notices an exit, and restarts after a backoff.
    pub fn supervise(&mut self, session: &Session) -> Supervision {
        if let Some(at) = self.restart_at {
            if Instant::now() < at {
                return Supervision::Running;
            }
            self.restart_at = None;
            if let Err(err) = self.start(session) {
                tracing::error!("{err:#}");
                return Supervision::End;
            }
        }
        let Some(child) = self.child.as_mut() else { return Supervision::Running };
        let status = match child.try_wait() {
            Ok(Some(status)) => status,
            _ => return Supervision::Running,
        };
        let pid = child.id();
        self.child = None;
        // Whatever it left behind in its group goes too.
        terminate(pid);
        let clean = status.success();
        tracing::info!(%status, "the desktop exited");
        match (self.restart, clean) {
            (Restart::Never, _) | (Restart::OnFailure, true) => return Supervision::End,
            _ => {}
        }
        let now = Instant::now();
        if !clean {
            self.crashes.retain(|t| now.duration_since(*t) < CRASH_WINDOW);
            self.crashes.push(now);
            if self.crashes.len() >= MAX_CRASHES {
                tracing::error!(crashes = self.crashes.len(), "the desktop keeps crashing; ending the session");
                return Supervision::End;
            }
        }
        let backoff = backoff(self.crashes.len());
        tracing::info!(after_ms = backoff.as_millis() as u64, "restarting the desktop");
        self.restart_at = Some(now + backoff);
        Supervision::Running
    }
}

/// 0.5 s after the first crash, doubling.
fn backoff(crashes: usize) -> Duration {
    Duration::from_millis(250 << crashes.min(6))
}

impl Drop for Desktop {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            terminate(child.id());
            let deadline = Instant::now() + Duration::from_secs(2);
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(1), Duration::from_millis(500));
        assert_eq!(backoff(2), Duration::from_millis(1000));
        assert_eq!(backoff(10), backoff(6));
    }
}
