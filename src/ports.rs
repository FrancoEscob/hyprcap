//! Injectable ports for process spawning, time, notifications, clipboard, and paths.
//!
//! Production adapters (real process groups, notify-send, wl-copy) sit behind these
//! traits. Unit tests use fakes with no Wayland / GTK / real socket.
//!
//! Argv is `String` (UTF-8) for the v1 controller: all external tools we spawn
//! (`wf-recorder`, `slurp`, …) take ASCII/UTF-8 flags and paths we control.
//! Non-UTF8 path support can move to `OsString` with the production adapter later
//! without changing state-machine semantics.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Soft cap for retained child stderr (SPEC ~4KiB).
pub const STDERR_TAIL_MAX: usize = 4096;

/// Soft cap for child stdout capture (slurp geometry); oversize is discarded.
pub const STDOUT_CAPTURE_MAX: usize = 4096;

/// Unix-style signals used for cooperative stop of `wf-recorder` process groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    /// SIGINT — first stop signal (cooperative finalize).
    Interrupt,
    /// SIGTERM — escalation after SIGINT timeout.
    Terminate,
    /// SIGKILL — nuclear reap only after TERM timeout (must be logged).
    Kill,
}

impl Signal {
    pub fn as_str(self) -> &'static str {
        match self {
            Signal::Interrupt => "SIGINT",
            Signal::Terminate => "SIGTERM",
            Signal::Kill => "SIGKILL",
        }
    }
}

/// How a child process exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    Code(i32),
    Signal(i32),
}

impl ExitStatus {
    pub fn success(self) -> bool {
        matches!(self, ExitStatus::Code(0))
    }

    pub fn code(self) -> Option<i32> {
        match self {
            ExitStatus::Code(c) => Some(c),
            ExitStatus::Signal(_) => None,
        }
    }
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExitStatus::Code(c) => write!(f, "exit {c}"),
            ExitStatus::Signal(s) => write!(f, "signal {s}"),
        }
    }
}

/// Options for spawning an external command (argv-only; never shell).
#[derive(Debug, Clone, Default)]
pub struct SpawnOpts {
    /// Put the child in a new process group (required for `wf-recorder` stop-by-group).
    pub new_process_group: bool,
}

/// Handle to a spawned child process.
pub trait ChildHandle {
    /// OS process id.
    fn id(&self) -> u32;

    /// Deliver `signal` to the process only.
    fn signal(&mut self, signal: Signal) -> Result<(), PortError>;

    /// Deliver `signal` to the child's process group (negative pid / killpg semantics).
    fn signal_group(&mut self, signal: Signal) -> Result<(), PortError>;

    /// Non-blocking poll for exit.
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, PortError>;

    /// Block until the child exits.
    fn wait(&mut self) -> Result<ExitStatus, PortError>;

    /// Bounded wait. Returns `None` if still running when `timeout` elapses.
    fn wait_timeout(&mut self, timeout: Duration) -> Result<Option<ExitStatus>, PortError>;

    /// Stdout captured from the child (e.g. slurp geometry). Default empty.
    fn take_stdout(&mut self) -> String {
        String::new()
    }

    /// Bounded stderr tail (~4KiB) for error surfaces. Default empty.
    fn take_stderr_tail(&mut self) -> String {
        String::new()
    }
}

/// Spawns argv-only commands (no `sh -c`).
pub trait CommandSpawner {
    type Child: ChildHandle;

    /// Spawn `argv[0]` with arguments `argv[1..]`.
    fn spawn(&mut self, argv: &[String], opts: SpawnOpts) -> Result<Self::Child, PortError>;

    /// Whether `binary` is available on PATH (or otherwise runnable).
    fn command_exists(&self, binary: &str) -> bool;
}

/// Wall-clock and monotonic time for filenames, timeouts, and `started_at`.
pub trait Clock {
    fn now(&self) -> SystemTime;

    /// Sleep (real or fake) for timeout simulation in tests.
    fn sleep(&self, duration: Duration);
}

/// Desktop notifications (`notify-send` in production; no-op/fake in tests).
pub trait Notifier {
    fn notify(&mut self, title: &str, body: &str) -> Result<(), PortError>;
}

/// Clipboard path-as-text (`wl-copy` in production).
pub trait Clipboard {
    fn copy_text(&mut self, text: &str) -> Result<(), PortError>;
}

/// Resolves config, output, and runtime paths (XDG-aware in production).
pub trait Paths {
    fn config_path(&self) -> PathBuf;
    fn output_dir(&self) -> PathBuf;
    fn runtime_dir(&self) -> PathBuf;
    fn home_dir(&self) -> PathBuf;
}

/// Errors originating from port adapters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PortError {
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("signal failed: {0}")]
    Signal(String),
    #[error("wait failed: {0}")]
    Wait(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// Path helpers (pure)
// ---------------------------------------------------------------------------

/// Default Videos directory: `xdg-user-dir` result if provided, else `~/Videos`.
pub fn default_videos_dir(home: &Path, xdg_user_dir_videos: Option<&Path>) -> PathBuf {
    if let Some(p) = xdg_user_dir_videos {
        if !p.as_os_str().is_empty() {
            return p.to_path_buf();
        }
    }
    home.join("Videos")
}

/// Config file path: `$XDG_CONFIG_HOME/hyprcap/config.toml` or `~/.config/...`.
pub fn default_config_path(xdg_config_home: Option<&Path>, home: &Path) -> PathBuf {
    let base = xdg_config_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"));
    base.join("hyprcap").join("config.toml")
}

/// Runtime dir for socket/pid.
///
/// Prefers `$XDG_RUNTIME_DIR`, then `/run/user/$UID`.
/// **Never** falls back to `/tmp` or predictable `/tmp/hyprcap-$UID` (multi-user risk).
/// Callers must validate ownership/mode via [`crate::server`] acquire path before binding.
pub fn default_runtime_dir(xdg_runtime_dir: Option<&Path>) -> PathBuf {
    if let Some(p) = xdg_runtime_dir {
        if !p.as_os_str().is_empty() {
            return p.to_path_buf();
        }
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/run/user/{uid}"))
}

/// Make `path` absolute using `cwd` when relative (SPEC: absolute paths in hooks).
pub fn absolutize_path(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Truncate to last `STDERR_TAIL_MAX` bytes (char-safe for UTF-8 tails).
pub fn truncate_stderr_tail(s: &str) -> String {
    if s.len() <= STDERR_TAIL_MAX {
        return s.to_string();
    }
    // Keep the tail end of the buffer.
    let start = s.len() - STDERR_TAIL_MAX;
    // Avoid splitting a char.
    let start = s
        .char_indices()
        .find(|(i, _)| *i >= start)
        .map(|(i, _)| i)
        .unwrap_or(start);
    s[start..].to_string()
}
