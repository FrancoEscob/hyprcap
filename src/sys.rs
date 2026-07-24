//! Production port adapters (real processes, XDG paths, notify-send, wl-copy).
//!
//! Used by the server daemon and binary. Unit tests use fakes in `recorder` tests.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;

use crate::ports::{
    default_config_path, default_runtime_dir, default_videos_dir, truncate_stderr_tail,
    ChildHandle, Clipboard, Clock, CommandSpawner, ExitStatus, Notifier, Paths, PortError, Signal,
    SpawnOpts, STDERR_TAIL_MAX, STDOUT_CAPTURE_MAX,
};

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// XDG-aware paths from the environment.
#[derive(Debug, Clone)]
pub struct EnvPaths {
    home: PathBuf,
    config: PathBuf,
    output_dir: PathBuf,
    runtime: PathBuf,
}

impl EnvPaths {
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let xdg_config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
        let xdg_runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let xdg_videos = std::env::var_os("XDG_VIDEOS_DIR")
            .map(PathBuf::from)
            .or_else(query_xdg_user_dir_videos);
        Self {
            config: default_config_path(xdg_config.as_deref(), &home),
            output_dir: default_videos_dir(&home, xdg_videos.as_deref()),
            runtime: default_runtime_dir(xdg_runtime.as_deref()),
            home,
        }
    }

    /// Override runtime dir (tests / custom XDG_RUNTIME_DIR already applied via env).
    pub fn with_runtime_dir(mut self, runtime: PathBuf) -> Self {
        self.runtime = runtime;
        self
    }
}

impl Paths for EnvPaths {
    fn config_path(&self) -> PathBuf {
        self.config.clone()
    }
    fn output_dir(&self) -> PathBuf {
        self.output_dir.clone()
    }
    fn runtime_dir(&self) -> PathBuf {
        self.runtime.clone()
    }
    fn home_dir(&self) -> PathBuf {
        self.home.clone()
    }
}

fn query_xdg_user_dir_videos() -> Option<PathBuf> {
    let out = Command::new("xdg-user-dir").arg("VIDEOS").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

// ---------------------------------------------------------------------------
// Notifier / Clipboard
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct NotifySendNotifier {
    warned_missing: bool,
}

impl Notifier for NotifySendNotifier {
    fn notify(&mut self, title: &str, body: &str) -> Result<(), PortError> {
        match Command::new("notify-send").arg(title).arg(body).status() {
            Ok(st) if st.success() => Ok(()),
            Ok(st) => Err(PortError::Other(format!("notify-send exit {st}"))),
            Err(e) => {
                if !self.warned_missing {
                    self.warned_missing = true;
                    eprintln!("record-ui: notify-send unavailable ({e}); notifications disabled");
                }
                Err(PortError::Other(format!("notify-send: {e}")))
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct WlCopyClipboard {
    warned_missing: bool,
}

impl Clipboard for WlCopyClipboard {
    fn copy_text(&mut self, text: &str) -> Result<(), PortError> {
        let mut child = match Command::new("wl-copy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                if !self.warned_missing {
                    self.warned_missing = true;
                    eprintln!("record-ui: wl-copy unavailable ({e}); path copy disabled");
                }
                return Err(PortError::Other(format!("wl-copy: {e}")));
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(text.as_bytes()) {
                return Err(PortError::Other(format!("wl-copy write: {e}")));
            }
        }
        match child.wait() {
            Ok(st) if st.success() => Ok(()),
            Ok(st) => Err(PortError::Other(format!("wl-copy exit {st}"))),
            Err(e) => Err(PortError::Other(format!("wl-copy wait: {e}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Process spawner
// ---------------------------------------------------------------------------

/// Real argv-only process spawner with process-group support.
#[derive(Debug, Default)]
pub struct ProcessSpawner;

/// Captured child process.
pub struct ProcessChild {
    child: Child,
    /// Process group id (same as pid when `new_process_group` was set).
    pgid: Option<i32>,
    stdout_buf: Arc<Mutex<String>>,
    stderr_buf: Arc<Mutex<String>>,
    stdout_reader: Option<std::thread::JoinHandle<()>>,
    stderr_reader: Option<std::thread::JoinHandle<()>>,
    taken_stdout: bool,
    taken_stderr: bool,
}

impl ProcessChild {
    fn from_spawned(mut child: Child, new_group: bool) -> Self {
        let pgid = if new_group {
            Some(child.id() as i32)
        } else {
            None
        };

        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));

        let stdout_reader = child.stdout.take().map(|mut out| {
            let buf = Arc::clone(&stdout_buf);
            std::thread::spawn(move || {
                // Hard cap during read — never hold more than STDOUT_CAPTURE_MAX+chunk.
                let mut raw = Vec::with_capacity(STDOUT_CAPTURE_MAX.min(512));
                let mut chunk = [0u8; 512];
                let mut oversize = false;
                loop {
                    match out.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            if raw.len() + n > STDOUT_CAPTURE_MAX {
                                oversize = true;
                                // Drain remainder without storing.
                                let mut discard = [0u8; 4096];
                                while matches!(out.read(&mut discard), Ok(m) if m > 0) {}
                                break;
                            }
                            raw.extend_from_slice(&chunk[..n]);
                        }
                        Err(_) => break,
                    }
                }
                let s = if oversize {
                    // Empty → slurp_cancel / empty geometry upstream.
                    String::new()
                } else {
                    String::from_utf8_lossy(&raw).into_owned()
                };
                if let Ok(mut g) = buf.lock() {
                    *g = s;
                }
            })
        });

        let stderr_reader = child.stderr.take().map(|mut err| {
            let buf = Arc::clone(&stderr_buf);
            std::thread::spawn(move || {
                // Rolling tail: never retain more than STDERR_TAIL_MAX bytes.
                let mut ring: Vec<u8> = Vec::with_capacity(STDERR_TAIL_MAX);
                let mut chunk = [0u8; 512];
                loop {
                    match err.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            ring.extend_from_slice(&chunk[..n]);
                            if ring.len() > STDERR_TAIL_MAX {
                                let excess = ring.len() - STDERR_TAIL_MAX;
                                ring.drain(..excess);
                            }
                        }
                        Err(_) => break,
                    }
                }
                let s = String::from_utf8_lossy(&ring).into_owned();
                if let Ok(mut g) = buf.lock() {
                    *g = s;
                }
            })
        });

        Self {
            child,
            pgid,
            stdout_buf,
            stderr_buf,
            stdout_reader,
            stderr_reader,
            taken_stdout: false,
            taken_stderr: false,
        }
    }

    fn join_stdout(&mut self) {
        if let Some(h) = self.stdout_reader.take() {
            let _ = h.join();
        }
    }

    fn join_stderr(&mut self) {
        if let Some(h) = self.stderr_reader.take() {
            let _ = h.join();
        }
    }

    fn join_readers(&mut self) {
        self.join_stdout();
        self.join_stderr();
    }

    fn map_status(status: std::process::ExitStatus) -> ExitStatus {
        if let Some(code) = status.code() {
            ExitStatus::Code(code)
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                ExitStatus::Signal(status.signal().unwrap_or(0))
            }
            #[cfg(not(unix))]
            {
                ExitStatus::Code(1)
            }
        }
    }

    fn send_signal(pid: i32, sig: i32) -> Result<(), PortError> {
        let rc = unsafe { libc::kill(pid, sig) };
        if rc == 0 {
            Ok(())
        } else {
            let err = std::io::Error::last_os_error();
            // ESRCH = already dead — treat as ok for stop races.
            if err.raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(PortError::Signal(err.to_string()))
            }
        }
    }

    fn signal_num(signal: Signal) -> i32 {
        match signal {
            Signal::Interrupt => libc::SIGINT,
            Signal::Terminate => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        }
    }
}

impl ChildHandle for ProcessChild {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn signal(&mut self, signal: Signal) -> Result<(), PortError> {
        Self::send_signal(self.child.id() as i32, Self::signal_num(signal))
    }

    fn signal_group(&mut self, signal: Signal) -> Result<(), PortError> {
        let pgid = self.pgid.unwrap_or(self.child.id() as i32);
        // kill(-pgid, sig) delivers to the process group.
        Self::send_signal(-pgid, Self::signal_num(signal))
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, PortError> {
        match self.child.try_wait() {
            Ok(Some(st)) => {
                self.join_readers();
                Ok(Some(Self::map_status(st)))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(PortError::Wait(e.to_string())),
        }
    }

    fn wait(&mut self) -> Result<ExitStatus, PortError> {
        match self.child.wait() {
            Ok(st) => {
                self.join_readers();
                Ok(Self::map_status(st))
            }
            Err(e) => Err(PortError::Wait(e.to_string())),
        }
    }

    fn wait_timeout(&mut self, timeout: Duration) -> Result<Option<ExitStatus>, PortError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.try_wait()? {
                Some(st) => return Ok(Some(st)),
                None => {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    fn take_stdout(&mut self) -> String {
        if self.taken_stdout {
            return String::new();
        }
        self.taken_stdout = true;
        // Join only stdout — do not block on stderr while child may still run.
        self.join_stdout();
        self.stdout_buf
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    fn take_stderr_tail(&mut self) -> String {
        if self.taken_stderr {
            return String::new();
        }
        self.taken_stderr = true;
        self.join_stderr();
        let s = self
            .stderr_buf
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        truncate_stderr_tail(&s)
    }
}

impl CommandSpawner for ProcessSpawner {
    type Child = ProcessChild;

    fn spawn(&mut self, argv: &[String], opts: SpawnOpts) -> Result<Self::Child, PortError> {
        if argv.is_empty() {
            return Err(PortError::Spawn("empty argv".into()));
        }
        let program = &argv[0];
        let mut cmd = Command::new(program);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if opts.new_process_group {
            // Put child in its own process group (pgid = child pid).
            cmd.process_group(0);
        }

        match cmd.spawn() {
            Ok(child) => Ok(ProcessChild::from_spawned(child, opts.new_process_group)),
            Err(e) => Err(PortError::Spawn(format!("{program}: {e}"))),
        }
    }

    fn command_exists(&self, binary: &str) -> bool {
        which(binary).is_some()
    }
}

/// Simple PATH lookup (no shell).
pub fn which(binary: &str) -> Option<PathBuf> {
    if binary.contains('/') {
        let p = PathBuf::from(binary);
        return if p.is_file() { Some(p) } else { None };
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            // Executable bit not strictly required for existence checks on all FS.
            return Some(candidate);
        }
    }
    None
}

/// True if `pid` is still running (or we lack permission to signal it).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    // EPERM: process exists but we cannot signal it.
    err.raw_os_error() == Some(libc::EPERM)
}

/// True if `pid` looks like a record-ui process (`/proc/pid/comm`).
///
/// Used to avoid treating PID reuse of an unrelated process as a live server.
pub fn is_our_server_pid(pid: u32) -> bool {
    let path = PathBuf::from(format!("/proc/{pid}/comm"));
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let name = text.trim();
    // comm is truncated to 15 chars on Linux ("record-ui" fits).
    name == "record-ui" || name.starts_with("record-ui")
}

/// Read first line of a pid file as u32.
pub fn read_pid_file(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.trim().parse().ok()
}

/// Write pid file (single line, mode 0600).
pub fn write_pid_file(path: &Path, pid: u32) -> Result<(), PortError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| PortError::Io(format!("create pid dir: {e}")))?;
    }
    std::fs::write(path, format!("{pid}\n"))
        .map_err(|e| PortError::Io(format!("write pid file: {e}")))?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(())
}
