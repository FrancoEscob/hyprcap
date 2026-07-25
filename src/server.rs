//! Session server: Unix socket + pid file, sole `Recorder` owner.
//!
//! Socket: `$XDG_RUNTIME_DIR/record-ui.sock` mode `0600`.
//! Pid: `$XDG_RUNTIME_DIR/record-ui.pid`.
//! Framing: newline-delimited JSON.
//!
//! # Trust model
//!
//! The control plane is **same-UID only** (socket mode `0600`). Do not place the
//! socket in a shared or world-writable directory. Local processes running as the
//! same user can issue any IPC command (start/stop/shutdown).
//!
//! # Region selection
//!
//! `start_region` / idle `toggle_region` use non-blocking `begin_region` + `poll`
//! so other clients can `stop`/`toggle`/`status` while slurp is open. The original
//! client connection stays open until selection completes or is cancelled.
//!
//! # Stop
//!
//! Cooperative stop remains synchronous (SIGINT→TERM waits up to config timeouts,
//! ~7s). Status may be unavailable during `Stopping` in v1.
//!
//! # IPC commands
//!
//! Includes `toggle_region` (CLI efficiency; not listed in SPEC request table but
//! required for the normative CLI surface).

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;
use crate::ipc::{decode_request, encode_response, IpcCommand, IpcRequest, IpcResponse, IpcStatus};
use crate::ports::{Clipboard, Clock, CommandSpawner, Notifier, Paths};
use crate::recorder::{CommandResult, MachineCode, Recorder, State};
use crate::sys::{
    is_our_server_pid, pid_alive, read_pid_file, write_pid_file, EnvPaths, NotifySendNotifier,
    ProcessSpawner, SystemClock, WlCopyClipboard,
};

pub const SOCKET_NAME: &str = "record-ui.sock";
pub const PID_NAME: &str = "record-ui.pid";
pub const LOCK_NAME: &str = "record-ui.lock";

/// Cap for a single IPC request/response line (DoS guard).
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Runtime paths for socket + pid under an XDG runtime directory.
#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
    pub lock_path: PathBuf,
}

impl RuntimePaths {
    pub fn from_runtime_dir(runtime_dir: impl Into<PathBuf>) -> Self {
        let runtime_dir = runtime_dir.into();
        Self {
            socket_path: runtime_dir.join(SOCKET_NAME),
            pid_path: runtime_dir.join(PID_NAME),
            lock_path: runtime_dir.join(LOCK_NAME),
            runtime_dir,
        }
    }

    /// Resolve from environment; errors if no private runtime dir is available.
    pub fn from_env() -> Result<Self, String> {
        let paths = EnvPaths::from_env();
        let dir = paths.runtime_dir();
        validate_private_runtime_dir(&dir).map_err(|e| e.to_string())?;
        Ok(Self::from_runtime_dir(dir))
    }
}

/// Errors acquiring the server bind.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    #[error("server already running")]
    AlreadyRunning,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Listener + exclusive acquisition lock (held for server lifetime).
pub struct BoundServer {
    pub listener: UnixListener,
    /// `flock(LOCK_EX)` held until drop — serializes stale recovery / dual bind.
    _lock: File,
}

impl std::fmt::Debug for BoundServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundServer").finish_non_exhaustive()
    }
}

/// Session state for the accept loop (single-threaded ownership).
struct SessionState<S, C, N, Cl>
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    recorder: Recorder<S, C, N, Cl>,
    /// Number of connected GUI views; CLI does not count.
    gui_clients: u32,
    /// Long-lived GUI subscribe sockets (held until peer disconnect).
    gui_views: Vec<UnixStream>,
    /// Set by shutdown / quit.
    shutdown: bool,
    /// At least one real IPC request was handled (idle-exit gate).
    ever_served: bool,
    /// Client waiting for region selection to finish.
    pending_region: Option<UnixStream>,
}

/// Result of handling one request (whether the server should exit after response).
#[derive(Debug)]
pub struct HandleOutcome {
    pub response: IpcResponse,
    /// Request asked for shutdown.
    pub want_shutdown: bool,
}

/// How the accept loop should treat a connection after dispatch.
enum ConnAction {
    /// Response already written (or nothing to write); connection done.
    Done { want_shutdown: bool },
    /// Region selection in progress; stream parked in `pending_region`.
    Deferred,
    /// GUI subscribe: stream parked in `gui_views` until peer disconnects.
    HoldGui,
}

// ---------------------------------------------------------------------------
// Bind / stale recovery
// ---------------------------------------------------------------------------

/// Bind the Unix socket with mode `0600`, write pid file.
///
/// Serializes acquisition with exclusive `flock` on `record-ui.lock`.
/// If the address is in use: try connect; if connect fails and pid is not a live
/// record-ui server → remove stale socket/pid and rebind.
pub fn acquire_listener(paths: &RuntimePaths) -> Result<BoundServer, AcquireError> {
    ensure_runtime_dir(&paths.runtime_dir)?;

    let lock = exclusive_lock(&paths.lock_path)?;

    match try_bind(paths) {
        Ok(listener) => {
            write_pid_file(&paths.pid_path, std::process::id())
                .map_err(|e| AcquireError::Other(e.to_string()))?;
            Ok(BoundServer {
                listener,
                _lock: lock,
            })
        }
        Err(e) if is_addr_in_use(&e) => {
            let listener = recover_or_busy(paths)?;
            Ok(BoundServer {
                listener,
                _lock: lock,
            })
        }
        Err(e) => Err(AcquireError::Io(e)),
    }
}

/// Ensure `dir` exists, is owned by euid, and is not group/other-writable.
///
/// Fail closed: reject attacker-precreated dirs we do not own; chmod failures are
/// errors. Production never resolves to bare `/tmp` or predictable
/// `/tmp/record-ui-$UID` ([`crate::ports::default_runtime_dir`]); unique temp
/// subdirs (tests) pass if owned by euid and private.
pub fn validate_private_runtime_dir(dir: &Path) -> Result<(), AcquireError> {
    use std::os::unix::fs::MetadataExt;

    // Never use the shared sticky directory itself as the runtime home.
    if dir == Path::new("/tmp") {
        return Err(AcquireError::Other(
            "refusing control socket in /tmp; set XDG_RUNTIME_DIR to a private directory".into(),
        ));
    }

    if !dir.exists() {
        fs::create_dir_all(dir).map_err(AcquireError::Io)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| {
            AcquireError::Other(format!(
                "cannot set runtime dir mode 0700 on {}: {e}",
                dir.display()
            ))
        })?;
    }

    let meta = fs::metadata(dir).map_err(AcquireError::Io)?;
    if !meta.is_dir() {
        return Err(AcquireError::Other(format!(
            "runtime path is not a directory: {}",
            dir.display()
        )));
    }

    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return Err(AcquireError::Other(format!(
            "runtime dir {} not owned by current user (uid {} != euid {})",
            dir.display(),
            meta.uid(),
            euid
        )));
    }

    let mode = meta.mode() & 0o777;
    // Must not be group- or other-writable.
    if mode & 0o022 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| {
            AcquireError::Other(format!(
                "runtime dir {} is group/other-writable and chmod 0700 failed: {e}",
                dir.display()
            ))
        })?;
        let meta2 = fs::metadata(dir).map_err(AcquireError::Io)?;
        if meta2.mode() & 0o022 != 0 {
            return Err(AcquireError::Other(format!(
                "runtime dir {} remains group/other-writable after chmod",
                dir.display()
            )));
        }
    }

    Ok(())
}

fn ensure_runtime_dir(dir: &Path) -> Result<(), AcquireError> {
    validate_private_runtime_dir(dir)
}

fn exclusive_lock(path: &Path) -> Result<File, AcquireError> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    // Non-blocking: another holder means a server is acquiring/running.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) || err.raw_os_error() == Some(libc::EAGAIN)
        {
            return Err(AcquireError::AlreadyRunning);
        }
        return Err(AcquireError::Io(err));
    }
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    Ok(file)
}

fn try_bind(paths: &RuntimePaths) -> std::io::Result<UnixListener> {
    // Tighten creation mask around bind; still chmod 0600 after.
    let prev = unsafe { libc::umask(0o077) };
    let result = UnixListener::bind(&paths.socket_path);
    unsafe {
        libc::umask(prev);
    }
    let listener = result?;
    set_mode_0600(&paths.socket_path)?;
    Ok(listener)
}

fn recover_or_busy(paths: &RuntimePaths) -> Result<UnixListener, AcquireError> {
    // Live server? connect succeeds.
    if UnixStream::connect(&paths.socket_path).is_ok() {
        return Err(AcquireError::AlreadyRunning);
    }

    // Connect failed — check whether pid is a live *record-ui* server.
    let ours_alive = match read_pid_file(&paths.pid_path) {
        Some(pid) => pid_alive(pid) && is_our_server_pid(pid),
        None => false,
    };

    if ours_alive {
        // Pid looks like our server but connect failed — race/permission; do not clobber.
        return Err(AcquireError::AlreadyRunning);
    }

    // Stale socket + dead/foreign/missing pid → clean and become server.
    let _ = fs::remove_file(&paths.socket_path);
    let _ = fs::remove_file(&paths.pid_path);

    let listener = try_bind(paths)?;
    write_pid_file(&paths.pid_path, std::process::id())
        .map_err(|e| AcquireError::Other(e.to_string()))?;
    Ok(listener)
}

fn is_addr_in_use(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::AlreadyExists
    ) || e.raw_os_error() == Some(libc::EADDRINUSE)
}

fn set_mode_0600(path: &Path) -> std::io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

/// Remove socket + pid after clean server exit (lock file may remain empty).
pub fn cleanup_runtime_files(paths: &RuntimePaths) {
    let _ = fs::remove_file(&paths.socket_path);
    let _ = fs::remove_file(&paths.pid_path);
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

/// Apply one IPC request to the recorder (blocking full start_region for tests).
///
/// Production accept loop uses [`dispatch_request`] for non-blocking region start.
pub fn handle_request<S, C, N, Cl>(
    recorder: &mut Recorder<S, C, N, Cl>,
    req: &IpcRequest,
    gui_clients: u32,
) -> HandleOutcome
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    let notify_start = gui_clients == 0;
    let (result, want_shutdown) = match req.cmd {
        IpcCommand::Ping => (CommandResult::ok_msg("pong"), false),
        IpcCommand::Status => (CommandResult::ok_msg("status"), false),
        IpcCommand::StartRegion => (recorder.start_region(req.audio, notify_start), false),
        IpcCommand::StartFullscreen => (
            recorder.start_fullscreen(req.audio, notify_start, req.output.as_deref()),
            false,
        ),
        IpcCommand::Stop => (recorder.stop(), false),
        IpcCommand::ToggleRegion => (recorder.toggle_region(req.audio, notify_start), false),
        IpcCommand::Shutdown => shutdown_result(recorder),
        IpcCommand::Subscribe => (CommandResult::ok_msg("subscribed"), false),
    };

    let status = recorder.status();
    let mut response = IpcResponse::from_command_result(&result, &status);
    if req.cmd == IpcCommand::Ping {
        response.message = "pong".into();
    }
    HandleOutcome {
        response,
        want_shutdown,
    }
}

fn shutdown_result<S, C, N, Cl>(recorder: &mut Recorder<S, C, N, Cl>) -> (CommandResult, bool)
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    let stop_result = recorder.stop();
    // Preserve stop failure (e.g. stop_timeout) on the response while still exiting.
    if stop_result.code == MachineCode::Ok || stop_result.code == MachineCode::SlurpCancel {
        if stop_result.code == MachineCode::SlurpCancel {
            (
                CommandResult {
                    ok: true,
                    code: MachineCode::Ok,
                    message: format!("shutting down ({})", stop_result.message),
                    warnings: stop_result.warnings,
                },
                true,
            )
        } else {
            (CommandResult::ok_msg("shutting down"), true)
        }
    } else {
        (
            CommandResult {
                ok: stop_result.ok,
                code: stop_result.code,
                message: format!("shutting down: {}", stop_result.message),
                warnings: stop_result.warnings,
            },
            true,
        )
    }
}

fn outcome_from_result<S, C, N, Cl>(
    result: CommandResult,
    recorder: &Recorder<S, C, N, Cl>,
) -> HandleOutcome
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    let status = recorder.status();
    HandleOutcome {
        response: IpcResponse::from_command_result(&result, &status),
        want_shutdown: false,
    }
}

// ---------------------------------------------------------------------------
// Accept loop
// ---------------------------------------------------------------------------

/// Run the server accept loop until shutdown or idle-exit policy.
///
/// Single-threaded: owns the recorder. Region selection is non-blocking so other
/// clients can cancel/status while slurp runs. Stop is still synchronous (v1).
pub fn run_loop<S, C, N, Cl>(
    bound: BoundServer,
    paths: RuntimePaths,
    recorder: Recorder<S, C, N, Cl>,
    idle_exit: bool,
) -> std::io::Result<()>
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    let BoundServer { listener, _lock } = bound;

    let mut state = SessionState {
        recorder,
        gui_clients: 0,
        gui_views: Vec::new(),
        shutdown: false,
        ever_served: false,
        pending_region: None,
    };

    listener.set_nonblocking(true)?;

    loop {
        // Drive async region progress + unexpected child exit.
        poll_and_complete_pending(&mut state);
        // Drop GUI views whose clients disconnected (keeps gui_clients accurate).
        reap_disconnected_gui_views(&mut state);

        if state.shutdown {
            break;
        }

        // Idle-exit after poll-to-idle (e.g. unexpected child death) once we have served.
        if should_idle_exit(idle_exit, &state) {
            break;
        }

        match listener.accept() {
            Ok((stream, _)) => {
                // Connection-local errors must not kill the daemon.
                if let Err(e) = handle_connection(stream, &mut state) {
                    eprintln!("record-ui server: connection error: {e}");
                }
                reap_disconnected_gui_views(&mut state);
                if state.shutdown {
                    break;
                }
                if should_idle_exit(idle_exit, &state) {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // Listener-level fatal.
                eprintln!("record-ui server: accept error: {e}");
                return Err(e);
            }
        }
    }

    // Best-effort stop on exit.
    if state.recorder.state().is_busy() {
        let _ = state.recorder.stop();
    }
    // Drop pending / GUI clients without response if we are shutting down hard.
    state.pending_region = None;
    state.gui_views.clear();
    state.gui_clients = 0;

    cleanup_runtime_files(&paths);
    drop(_lock);
    Ok(())
}

/// Reap GUI subscribe sockets that disconnected; decrement `gui_clients`.
fn reap_disconnected_gui_views<S, C, N, Cl>(state: &mut SessionState<S, C, N, Cl>)
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    let before = state.gui_views.len();
    state.gui_views.retain(|s| !peer_closed(s));
    let dropped = before.saturating_sub(state.gui_views.len());
    if dropped > 0 {
        state.gui_clients = state.gui_clients.saturating_sub(dropped as u32);
    }
}

fn should_idle_exit<S, C, N, Cl>(idle_exit: bool, state: &SessionState<S, C, N, Cl>) -> bool
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    idle_exit
        && state.ever_served
        && state.gui_clients == 0
        && !state.recorder.state().is_busy()
        && state.pending_region.is_none()
        && !state.shutdown
}

fn poll_and_complete_pending<S, C, N, Cl>(state: &mut SessionState<S, C, N, Cl>)
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    // Parked client disconnected → cancel region selection (do not start recording).
    cancel_pending_if_peer_closed(state);

    if let Some(result) = state.recorder.poll() {
        if let Some(mut stream) = state.pending_region.take() {
            let outcome = outcome_from_result(result, &state.recorder);
            let _ = write_response(&mut stream, &outcome.response);
        }
        // Unexpected death while Recording with no pending: state already Idle.
    }
}

/// If the parked region client hung up, cancel slurp and drop the pending stream.
fn cancel_pending_if_peer_closed<S, C, N, Cl>(state: &mut SessionState<S, C, N, Cl>)
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    let Some(stream) = state.pending_region.as_mut() else {
        return;
    };
    if !peer_closed(stream) {
        return;
    }
    let _ = state.pending_region.take();
    // Cancel SelectingRegion (or stop if somehow advanced).
    if state.recorder.state().is_busy() {
        let _ = state.recorder.stop();
    }
}

/// Non-blocking probe: true if peer closed or stream is in unrecoverable error.
fn peer_closed(stream: &UnixStream) -> bool {
    // MSG_PEEK | MSG_DONTWAIT: detect HUP without consuming bytes or needing
    // unstable `UnixStream::peek`.
    let mut buf = [0u8; 1];
    let fd = stream.as_raw_fd();
    let n = unsafe {
        libc::recv(
            fd,
            buf.as_mut_ptr() as *mut libc::c_void,
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if n == 0 {
        return true; // orderly shutdown
    }
    if n > 0 {
        return false; // data available (unexpected on parked client)
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        // On Linux EAGAIN and EWOULDBLOCK are the same value.
        Some(e) if e == libc::EAGAIN || e == libc::EINTR => false,
        _ => true, // ECONNRESET, EPIPE, etc.
    }
}

/// Handle one client connection. Errors are connection-local.
fn handle_connection<S, C, N, Cl>(
    stream: UnixStream,
    state: &mut SessionState<S, C, N, Cl>,
) -> std::io::Result<()>
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    // GUI count must always be released, even if dispatch I/O fails mid-request.
    let mut gui_counted = false;
    let result = handle_connection_inner(stream, state, &mut gui_counted);
    if gui_counted {
        state.gui_clients = state.gui_clients.saturating_sub(1);
    }
    result
}

fn handle_connection_inner<S, C, N, Cl>(
    stream: UnixStream,
    state: &mut SessionState<S, C, N, Cl>,
    gui_counted: &mut bool,
) -> std::io::Result<()>
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    loop {
        let line = match read_capped_line(&mut reader)? {
            None => break, // EOF
            Some(Err(resp_msg)) => {
                state.ever_served = true;
                let status = state.recorder.status();
                let resp = IpcResponse::err(MachineCode::Invalid, resp_msg, &status);
                let _ = write_response(&mut writer, &resp);
                break; // close after error response
            }
            Some(Ok(line)) => line,
        };

        if line.trim().is_empty() {
            continue;
        }

        state.ever_served = true;

        let req = match decode_request(&line) {
            Ok(r) => r,
            Err(e) => {
                let status = state.recorder.status();
                let resp = IpcResponse::err(
                    MachineCode::Invalid,
                    format!("invalid request: {e}"),
                    &status,
                );
                let _ = write_response(&mut writer, &resp);
                break;
            }
        };

        // GUI subscribe: count at most once per connection.
        if !*gui_counted && (req.cmd == IpcCommand::Subscribe || req.gui == Some(true)) {
            state.gui_clients = state.gui_clients.saturating_add(1);
            *gui_counted = true;
        }

        match dispatch_connection(state, &req, &mut writer)? {
            ConnAction::Done { want_shutdown } => {
                if want_shutdown {
                    state.shutdown = true;
                }
                // One request per connection (CLI model).
                break;
            }
            ConnAction::Deferred => {
                // Park this connection until poll completes region selection.
                if let Some(mut old) = state.pending_region.take() {
                    let st = state.recorder.status();
                    let resp =
                        IpcResponse::err(MachineCode::Busy, "superseded region selection", &st);
                    let _ = write_response(&mut old, &resp);
                }
                // Pending region client is not a long-lived GUI view.
                state.pending_region = Some(writer);
                poll_and_complete_pending(state);
                return Ok(());
            }
            ConnAction::HoldGui => {
                // Transfer count ownership to parked list (do not decrement on return).
                state.gui_views.push(writer);
                *gui_counted = false;
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Dispatch a request. May park `writer` into `pending_region` (Deferred).
fn dispatch_connection<S, C, N, Cl>(
    state: &mut SessionState<S, C, N, Cl>,
    req: &IpcRequest,
    writer: &mut UnixStream,
) -> std::io::Result<ConnAction>
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    let notify_start = state.gui_clients == 0;

    match req.cmd {
        IpcCommand::StartRegion => {
            // Non-blocking: begin slurp, park client until poll completes.
            match state.recorder.begin_region(req.audio, notify_start) {
                Err(r) => {
                    let o = outcome_from_result(r, &state.recorder);
                    write_response(writer, &o.response)?;
                    Ok(ConnAction::Done {
                        want_shutdown: false,
                    })
                }
                Ok(()) => Ok(ConnAction::Deferred),
            }
        }
        IpcCommand::ToggleRegion => match state.recorder.state() {
            State::Idle => match state.recorder.begin_region(req.audio, notify_start) {
                Err(r) => {
                    let o = outcome_from_result(r, &state.recorder);
                    write_response(writer, &o.response)?;
                    Ok(ConnAction::Done {
                        want_shutdown: false,
                    })
                }
                Ok(()) => Ok(ConnAction::Deferred),
            },
            _ => {
                // Selecting→cancel, Recording→stop, etc. May complete pending.
                let result = state.recorder.toggle_region(req.audio, notify_start);
                complete_pending_with(&mut state.pending_region, &result, &state.recorder);
                let o = outcome_from_result(result, &state.recorder);
                write_response(writer, &o.response)?;
                Ok(ConnAction::Done {
                    want_shutdown: false,
                })
            }
        },
        IpcCommand::Stop => {
            let result = state.recorder.stop();
            complete_pending_with(&mut state.pending_region, &result, &state.recorder);
            let o = outcome_from_result(result, &state.recorder);
            write_response(writer, &o.response)?;
            Ok(ConnAction::Done {
                want_shutdown: false,
            })
        }
        IpcCommand::StartFullscreen => {
            let result =
                state
                    .recorder
                    .start_fullscreen(req.audio, notify_start, req.output.as_deref());
            let o = outcome_from_result(result, &state.recorder);
            write_response(writer, &o.response)?;
            Ok(ConnAction::Done {
                want_shutdown: false,
            })
        }
        IpcCommand::Ping => {
            let o = handle_request(&mut state.recorder, req, state.gui_clients);
            write_response(writer, &o.response)?;
            Ok(ConnAction::Done {
                want_shutdown: o.want_shutdown,
            })
        }
        IpcCommand::Status => {
            let o = handle_request(&mut state.recorder, req, state.gui_clients);
            write_response(writer, &o.response)?;
            Ok(ConnAction::Done {
                want_shutdown: o.want_shutdown,
            })
        }
        IpcCommand::Subscribe => {
            let o = handle_request(&mut state.recorder, req, state.gui_clients);
            write_response(writer, &o.response)?;
            // Keep the connection open so gui_clients stays elevated until disconnect.
            Ok(ConnAction::HoldGui)
        }
        IpcCommand::Shutdown => {
            let (result, want) = shutdown_result(&mut state.recorder);
            complete_pending_with(&mut state.pending_region, &result, &state.recorder);
            let status = state.recorder.status();
            let response = IpcResponse::from_command_result(&result, &status);
            write_response(writer, &response)?;
            Ok(ConnAction::Done {
                want_shutdown: want,
            })
        }
    }
}

fn complete_pending_with<S, C, N, Cl>(
    pending: &mut Option<UnixStream>,
    result: &CommandResult,
    recorder: &Recorder<S, C, N, Cl>,
) where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    if let Some(mut stream) = pending.take() {
        let status = recorder.status();
        let resp = IpcResponse::from_command_result(result, &status);
        let _ = write_response(&mut stream, &resp);
    }
}

/// Read one line capped at [`MAX_LINE_BYTES`].
///
/// `Ok(None)` = EOF. `Ok(Some(Err(msg)))` = line too long / invalid. `Ok(Some(Ok(line)))` = data.
/// Oversize lines do **not** allocate unbounded drain buffers; remainder is discarded
/// with a fixed scratch buffer, or the connection is closed by the caller after Err.
fn read_capped_line(reader: &mut impl BufRead) -> std::io::Result<Option<Result<String, String>>> {
    let mut buf = Vec::new();
    let mut take = reader.take(MAX_LINE_BYTES as u64 + 1);
    let n = take.read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_LINE_BYTES {
        // Discard rest of line with a fixed-size scratch (no unbounded Vec).
        let mut scratch = [0u8; 4096];
        loop {
            let n = reader.read(&mut scratch)?;
            if n == 0 {
                break;
            }
            if scratch[..n].contains(&b'\n') {
                break;
            }
        }
        return Ok(Some(Err(format!(
            "request line exceeds {MAX_LINE_BYTES} bytes"
        ))));
    }
    let s = String::from_utf8_lossy(&buf).into_owned();
    Ok(Some(Ok(s)))
}

fn write_response(writer: &mut UnixStream, resp: &IpcResponse) -> std::io::Result<()> {
    let line = encode_response(resp)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    if line.len() > MAX_LINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "response too large",
        ));
    }
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Production entry
// ---------------------------------------------------------------------------

/// Production server entry used by `record-ui --server`.
pub fn run_production_server() -> Result<(), Box<dyn std::error::Error>> {
    let env_paths = EnvPaths::from_env();
    let runtime = RuntimePaths::from_runtime_dir(env_paths.runtime_dir());
    // Fail closed on unsafe/unowned runtime dirs (also applied in acquire_listener).
    validate_private_runtime_dir(&runtime.runtime_dir)?;

    let bound = match acquire_listener(&runtime) {
        Ok(b) => b,
        Err(AcquireError::AlreadyRunning) => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let config = Config::load(&env_paths).unwrap_or_else(|_| Config::with_defaults(&env_paths));
    let recorder = Recorder::new(
        ProcessSpawner,
        SystemClock,
        NotifySendNotifier::default(),
        WlCopyClipboard::default(),
        config,
    );

    run_loop(bound, runtime, recorder, true)?;
    Ok(())
}

/// Spawn a detached `--server` child of the current executable.
pub fn spawn_daemon(runtime_dir: &Path) -> Result<std::process::Child, std::io::Error> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--server")
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.spawn()
}

/// Wait until the socket accepts connections (or timeout).
pub fn wait_for_socket(socket_path: &Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if UnixStream::connect(socket_path).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Ensure a server is running for `paths`. Spawns daemon if needed.
pub fn ensure_server(paths: &RuntimePaths) -> Result<(), String> {
    if UnixStream::connect(&paths.socket_path).is_ok() {
        return Ok(());
    }

    if paths.socket_path.exists() {
        let recover = match read_pid_file(&paths.pid_path) {
            Some(pid) => !(pid_alive(pid) && is_our_server_pid(pid)),
            None => true,
        };
        if recover {
            let _ = fs::remove_file(&paths.socket_path);
            let _ = fs::remove_file(&paths.pid_path);
        } else if wait_for_socket(&paths.socket_path, Duration::from_secs(2)) {
            return Ok(());
        } else {
            return Err("server pid alive but socket not accepting".into());
        }
    }

    let mut child = spawn_daemon(&paths.runtime_dir).map_err(|e| format!("spawn server: {e}"))?;

    if wait_for_socket(&paths.socket_path, Duration::from_secs(5)) {
        // Detach: do not leave a zombie if child exits later — we are the parent of a daemon.
        // After readiness, forget waiting; if spawn failed immediately, try_wait surfaces it.
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => {
                return Err(format!("server exited immediately: {status}"));
            }
            _ => {}
        }
        Ok(())
    } else {
        let _ = child.try_wait();
        Err("timeout waiting for server socket".into())
    }
}

/// Ensure server + request with connect retries (idle-exit TOCTOU).
pub fn ensure_and_request(paths: &RuntimePaths, req: &IpcRequest) -> Result<IpcResponse, String> {
    const ATTEMPTS: u32 = 3;
    let mut last_err = String::new();
    for attempt in 0..ATTEMPTS {
        ensure_server(paths)?;
        match crate::client::request(&paths.socket_path, req) {
            Ok(resp) => return Ok(resp),
            Err(crate::client::ClientError::Connect(e)) => {
                last_err = format!("connect: {e}");
                if attempt + 1 < ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(50 * (attempt + 1) as u64));
                    continue;
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Err(format!("failed after {ATTEMPTS} attempts: {last_err}"))
}

pub fn status_json_from_response(resp: &IpcResponse) -> &IpcStatus {
    &resp.status
}

// ---------------------------------------------------------------------------
// Integration tests I1–I5 + review coverage
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client;
    use crate::ports::{
        ChildHandle, Clipboard, Clock, CommandSpawner, ExitStatus, Notifier, PortError, Signal,
        SpawnOpts,
    };
    use crate::recorder::Recorder;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[derive(Clone)]
    struct FakeClock {
        now: Arc<Mutex<SystemTime>>,
    }
    impl FakeClock {
        fn at_secs(secs: u64) -> Self {
            Self {
                now: Arc::new(Mutex::new(UNIX_EPOCH + Duration::from_secs(secs))),
            }
        }
    }
    impl Clock for FakeClock {
        fn now(&self) -> SystemTime {
            *self.now.lock().unwrap()
        }
        fn sleep(&self, _d: Duration) {}
    }

    #[derive(Default)]
    struct FakeNotifier;
    impl Notifier for FakeNotifier {
        fn notify(&mut self, _: &str, _: &str) -> Result<(), PortError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeClipboard;
    impl Clipboard for FakeClipboard {
        fn copy_text(&mut self, _: &str) -> Result<(), PortError> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct Script {
        exit: ExitStatus,
        stdout: String,
        /// If true, slurp stays running until signalled (cancel tests).
        block_until_signal: bool,
    }

    struct FakeChild {
        pid: u32,
        exit: ExitStatus,
        stdout: String,
        wait_for_signal: bool,
        write_on_signal: Option<PathBuf>,
        alive: bool,
    }

    impl ChildHandle for FakeChild {
        fn id(&self) -> u32 {
            self.pid
        }
        fn signal(&mut self, _s: Signal) -> Result<(), PortError> {
            if self.wait_for_signal || self.alive {
                if let Some(ref p) = self.write_on_signal {
                    let _ = fs::write(p, b"fake-video");
                }
                self.alive = false;
            }
            Ok(())
        }
        fn signal_group(&mut self, s: Signal) -> Result<(), PortError> {
            self.signal(s)
        }
        fn try_wait(&mut self) -> Result<Option<ExitStatus>, PortError> {
            if self.alive {
                Ok(None)
            } else {
                Ok(Some(self.exit))
            }
        }
        fn wait(&mut self) -> Result<ExitStatus, PortError> {
            self.alive = false;
            Ok(self.exit)
        }
        fn wait_timeout(&mut self, _t: Duration) -> Result<Option<ExitStatus>, PortError> {
            if self.wait_for_signal && self.alive {
                Ok(None)
            } else if self.alive {
                self.alive = false;
                Ok(Some(self.exit))
            } else {
                Ok(Some(self.exit))
            }
        }
        fn take_stdout(&mut self) -> String {
            std::mem::take(&mut self.stdout)
        }
    }

    struct FakeSpawner {
        next_pid: u32,
        scripts: VecDeque<Script>,
        spawns: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl FakeSpawner {
        fn new() -> Self {
            Self {
                next_pid: 3000,
                scripts: VecDeque::new(),
                spawns: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn push(&mut self, s: Script) {
            self.scripts.push_back(s);
        }
    }

    impl CommandSpawner for FakeSpawner {
        type Child = FakeChild;
        fn spawn(&mut self, argv: &[String], _opts: SpawnOpts) -> Result<Self::Child, PortError> {
            self.spawns.lock().unwrap().push(argv.to_vec());
            let bin = argv.first().map(|s| s.as_str()).unwrap_or("");
            let pid = self.next_pid;
            self.next_pid += 1;

            if bin == "slurp" {
                let script = self.scripts.pop_front().unwrap_or(Script {
                    exit: ExitStatus::Code(0),
                    stdout: "10,20 100x200".into(),
                    block_until_signal: false,
                });
                return Ok(FakeChild {
                    pid,
                    exit: script.exit,
                    stdout: script.stdout,
                    wait_for_signal: script.block_until_signal,
                    write_on_signal: None,
                    alive: script.block_until_signal,
                });
            }

            let mut write_path = None;
            if let Some(i) = argv.iter().position(|a| a == "-f") {
                write_path = argv.get(i + 1).map(PathBuf::from);
            }
            Ok(FakeChild {
                pid,
                exit: ExitStatus::Code(0),
                stdout: String::new(),
                wait_for_signal: true,
                write_on_signal: write_path,
                alive: true,
            })
        }
        fn command_exists(&self, _: &str) -> bool {
            true
        }
    }

    fn temp_runtime() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "record-ui-ipc-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_config(videos: &Path) -> Config {
        Config {
            output_dir: videos.to_path_buf(),
            audio_default: false,
            copy_path: false,
            notify: false,
            notify_on_start_cli: false,
            stop_timeout_ms: 50,
            stop_term_timeout_ms: 50,
            // Fixed name so unit tests never call hyprctl / real outputs.
            fullscreen_output: Some("TEST-OUT".into()),
        }
    }

    type TestRec = Recorder<FakeSpawner, FakeClock, FakeNotifier, FakeClipboard>;

    fn make_recorder(videos: &Path, spawner: FakeSpawner) -> TestRec {
        Recorder::new(
            spawner,
            FakeClock::at_secs(1_705_322_245),
            FakeNotifier,
            FakeClipboard,
            test_config(videos),
        )
    }

    fn start_test_server(
        paths: RuntimePaths,
        recorder: TestRec,
        idle_exit: bool,
    ) -> std::thread::JoinHandle<()> {
        let bound = acquire_listener(&paths).expect("acquire listener");
        std::thread::spawn(move || {
            let _ = run_loop(bound, paths, recorder, idle_exit);
        })
    }

    fn wait_sock(paths: &RuntimePaths) {
        assert!(
            wait_for_socket(&paths.socket_path, Duration::from_secs(3)),
            "socket not ready"
        );
    }

    fn slurp_ok(geom: &str) -> Script {
        Script {
            exit: ExitStatus::Code(0),
            stdout: geom.into(),
            block_until_signal: false,
        }
    }

    fn slurp_blocking() -> Script {
        Script {
            exit: ExitStatus::Code(0),
            stdout: String::new(),
            block_until_signal: true,
        }
    }

    /// I1: start then status from second client.
    #[test]
    fn i1_start_then_status_shows_recording_and_path() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();

        let mut spawner = FakeSpawner::new();
        spawner.push(slurp_ok("0,0 800x600"));
        let rec = make_recorder(&videos, spawner);
        let h = start_test_server(paths.clone(), rec, false);
        wait_sock(&paths);

        let start = client::request(&paths.socket_path, &IpcRequest::start_region(None))
            .expect("start_region");
        assert!(start.ok, "{start:?}");
        assert_eq!(start.status.state, "Recording");
        assert!(start.status.output_path.is_some(), "{start:?}");

        let status = client::request(&paths.socket_path, &IpcRequest::status()).expect("status");
        assert_eq!(status.status.state, "Recording");
        assert_eq!(status.status.output_path, start.status.output_path);
        assert!(status.status.pid.is_some());

        let _ = client::request(&paths.socket_path, &IpcRequest::shutdown());
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// I2: toggle-region twice → start then stop.
    #[test]
    fn i2_toggle_region_twice_start_then_stop() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();

        let mut spawner = FakeSpawner::new();
        spawner.push(slurp_ok("1,2 3x4"));
        let rec = make_recorder(&videos, spawner);
        let h = start_test_server(paths.clone(), rec, false);
        wait_sock(&paths);

        let t1 =
            client::request(&paths.socket_path, &IpcRequest::toggle_region(None)).expect("toggle1");
        assert!(t1.ok, "{t1:?}");
        assert_eq!(t1.status.state, "Recording");

        let t2 =
            client::request(&paths.socket_path, &IpcRequest::toggle_region(None)).expect("toggle2");
        assert!(t2.ok, "{t2:?}");
        assert_eq!(t2.status.state, "Idle");

        let _ = client::request(&paths.socket_path, &IpcRequest::shutdown());
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// I3: concurrent start_region → exactly one Recording + busy exit 2.
    #[test]
    fn i3_concurrent_start_region_one_recording() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();

        let mut spawner = FakeSpawner::new();
        spawner.push(slurp_ok("5,5 10x10"));
        let rec = make_recorder(&videos, spawner);
        let h = start_test_server(paths.clone(), rec, false);
        wait_sock(&paths);

        let sock = paths.socket_path.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let b1 = Arc::clone(&barrier);
        let b2 = Arc::clone(&barrier);
        let s1 = sock.clone();
        let s2 = sock.clone();

        let j1 = std::thread::spawn(move || {
            b1.wait();
            client::request(&s1, &IpcRequest::start_region(None))
        });
        let j2 = std::thread::spawn(move || {
            b2.wait();
            client::request(&s2, &IpcRequest::start_region(None))
        });

        let r1 = j1.join().unwrap().expect("client1");
        let r2 = j2.join().unwrap().expect("client2");

        let results = [&r1, &r2];
        let ok_count = results.iter().filter(|r| r.ok).count();
        let busy_count = results.iter().filter(|r| r.code == "busy").count();
        assert_eq!(ok_count, 1, "exactly one ok start: {r1:?} {r2:?}");
        assert_eq!(busy_count, 1, "exactly one busy: {r1:?} {r2:?}");
        let winner = results.iter().find(|r| r.ok).unwrap();
        assert_eq!(winner.status.state, "Recording");
        let busy = results.iter().find(|r| r.code == "busy").unwrap();
        assert_eq!(busy.cli_exit_code(), 2);

        let st = client::request(&paths.socket_path, &IpcRequest::status()).unwrap();
        assert_eq!(st.status.state, "Recording");

        let _ = client::request(&paths.socket_path, &IpcRequest::shutdown());
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// Mid-selection cancel from second client (Issue 1).
    #[test]
    fn i_mid_selection_cancel_from_second_client() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();

        let mut spawner = FakeSpawner::new();
        spawner.push(slurp_blocking());
        let rec = make_recorder(&videos, spawner);
        let h = start_test_server(paths.clone(), rec, false);
        wait_sock(&paths);

        let sock = paths.socket_path.clone();
        let starter =
            std::thread::spawn(move || client::request(&sock, &IpcRequest::start_region(None)));

        // Wait until SelectingRegion visible.
        let mut saw_selecting = false;
        for _ in 0..100 {
            if let Ok(st) = client::request(&paths.socket_path, &IpcRequest::status()) {
                if st.status.state == "SelectingRegion" {
                    saw_selecting = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            saw_selecting,
            "expected SelectingRegion while slurp blocked"
        );

        let stop = client::request(&paths.socket_path, &IpcRequest::stop()).expect("stop");
        assert!(stop.ok, "{stop:?}");
        assert!(
            stop.code == "slurp_cancel" || stop.status.state == "Idle",
            "{stop:?}"
        );

        let start_resp = starter.join().unwrap().expect("starter response");
        assert_eq!(start_resp.code, "slurp_cancel", "{start_resp:?}");
        assert_eq!(start_resp.cli_exit_code(), 0);

        let _ = client::request(&paths.socket_path, &IpcRequest::shutdown());
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// I4: stale socket + dead pid → recover and serve.
    #[test]
    fn i4_stale_socket_dead_pid_recovers() {
        let root = temp_runtime();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();

        fs::write(&paths.socket_path, b"").unwrap();
        fs::write(&paths.pid_path, b"16777215\n").unwrap();

        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let rec = make_recorder(&videos, FakeSpawner::new());

        let bound = acquire_listener(&paths).expect("recover bind");
        assert!(paths.socket_path.exists());
        let pid_text = fs::read_to_string(&paths.pid_path).unwrap();
        assert_eq!(pid_text.trim(), std::process::id().to_string());

        let h = std::thread::spawn({
            let paths = paths.clone();
            move || {
                let _ = run_loop(bound, paths, rec, false);
            }
        });
        wait_sock(&paths);

        let pong = client::request(&paths.socket_path, &IpcRequest::ping()).unwrap();
        assert!(pong.ok);
        assert_eq!(pong.message, "pong");

        let _ = client::request(&paths.socket_path, &IpcRequest::shutdown());
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// Second acquire while first listening → AlreadyRunning (Issue 18).
    #[test]
    fn acquire_while_running_is_already_running() {
        let root = temp_runtime();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let bound = acquire_listener(&paths).unwrap();
        let err = acquire_listener(&paths).unwrap_err();
        assert!(matches!(err, AcquireError::AlreadyRunning), "{err:?}");
        drop(bound);
        let _ = fs::remove_dir_all(&root);
    }

    /// I5: stop with server idle → clean no-op.
    #[test]
    fn i5_stop_when_idle_is_noop_success() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();

        let rec = make_recorder(&videos, FakeSpawner::new());
        let h = start_test_server(paths.clone(), rec, false);
        wait_sock(&paths);

        let stop = client::request(&paths.socket_path, &IpcRequest::stop()).unwrap();
        assert!(stop.ok, "{stop:?}");
        assert_eq!(stop.code, "ok");
        assert_eq!(stop.status.state, "Idle");
        assert_eq!(stop.cli_exit_code(), 0);

        let _ = client::request(&paths.socket_path, &IpcRequest::shutdown());
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// Idle-exit: probe connect keeps server; status then exits (Issue 17).
    #[test]
    fn idle_exit_ignores_probe_exits_after_status() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();

        let rec = make_recorder(&videos, FakeSpawner::new());
        let h = start_test_server(paths.clone(), rec, true);
        wait_sock(&paths);

        // Readiness-style probe.
        drop(UnixStream::connect(&paths.socket_path).unwrap());
        std::thread::sleep(Duration::from_millis(80));
        // Server still up.
        assert!(
            UnixStream::connect(&paths.socket_path).is_ok(),
            "server should survive empty probe"
        );

        let st = client::request(&paths.socket_path, &IpcRequest::status()).unwrap();
        assert_eq!(st.status.state, "Idle");

        // Server should idle-exit.
        for _ in 0..50 {
            if h.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(h.is_finished(), "server should exit after idle status");
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// GUI subscribe hold: idle server stays up while view connected; drops on disconnect.
    #[test]
    fn subscribe_hold_blocks_idle_exit_until_disconnect() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();

        let rec = make_recorder(&videos, FakeSpawner::new());
        let h = start_test_server(paths.clone(), rec, true);
        wait_sock(&paths);

        // Round-trip IpcRequest::subscribe encode/decode + live hold.
        let sub_req = IpcRequest::subscribe();
        assert_eq!(sub_req.cmd, IpcCommand::Subscribe);
        assert_eq!(sub_req.gui, Some(true));
        let line = crate::ipc::encode_request(&sub_req).unwrap();
        let back = crate::ipc::decode_request(&line).unwrap();
        assert_eq!(back.cmd, IpcCommand::Subscribe);
        assert_eq!(back.gui, Some(true));

        let (hold, sub) = client::subscribe(&paths.socket_path).expect("subscribe");
        assert!(sub.ok, "{sub:?}");
        assert_eq!(sub.message, "subscribed");
        assert_eq!(sub.code, "ok");

        // Status while GUI held — server must not idle-exit.
        let st = client::request(&paths.socket_path, &IpcRequest::status()).unwrap();
        assert_eq!(st.status.state, "Idle");
        std::thread::sleep(Duration::from_millis(120));
        assert!(
            !h.is_finished(),
            "server must stay up while GUI subscribe is held"
        );
        assert!(UnixStream::connect(&paths.socket_path).is_ok());

        // Disconnect view → idle-exit allowed.
        drop(hold);
        for _ in 0..80 {
            if h.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            h.is_finished(),
            "server should idle-exit after GUI disconnect"
        );
        let _ = h.join();
        // Socket gone after clean idle-exit.
        assert!(
            UnixStream::connect(&paths.socket_path).is_err(),
            "connect should fail after server idle-exit"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// GUI disconnect mid-recording: recording continues; stop still works.
    #[test]
    fn gui_disconnect_mid_recording_keeps_session() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();

        let mut spawner = FakeSpawner::new();
        spawner.push(slurp_ok("0,0 100x100"));
        let rec = make_recorder(&videos, spawner);
        let h = start_test_server(paths.clone(), rec, false);
        wait_sock(&paths);

        let (hold, sub) = client::subscribe(&paths.socket_path).expect("subscribe");
        assert!(sub.ok, "{sub:?}");

        let start = client::request(&paths.socket_path, &IpcRequest::start_region(None)).unwrap();
        assert!(start.ok, "{start:?}");
        assert_eq!(start.status.state, "Recording");

        // Close GUI view only.
        drop(hold);
        std::thread::sleep(Duration::from_millis(80));

        let st = client::request(&paths.socket_path, &IpcRequest::status()).unwrap();
        assert_eq!(
            st.status.state, "Recording",
            "recording must continue after GUI close"
        );

        let stop = client::request(&paths.socket_path, &IpcRequest::stop()).unwrap();
        assert!(stop.ok, "{stop:?}");
        assert_eq!(stop.status.state, "Idle");

        let _ = client::request(&paths.socket_path, &IpcRequest::shutdown());
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// Dual subscribe: both count; both must disconnect before idle-exit.
    #[test]
    fn dual_subscribe_reap_both_before_idle_exit() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();

        let rec = make_recorder(&videos, FakeSpawner::new());
        let h = start_test_server(paths.clone(), rec, true);
        wait_sock(&paths);

        let (hold1, s1) = client::subscribe(&paths.socket_path).expect("sub1");
        let (hold2, s2) = client::subscribe(&paths.socket_path).expect("sub2");
        assert!(s1.ok && s2.ok);

        let _ = client::request(&paths.socket_path, &IpcRequest::status()).unwrap();
        drop(hold1);
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !h.is_finished(),
            "one remaining GUI view must block idle-exit"
        );

        drop(hold2);
        for _ in 0..80 {
            if h.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(h.is_finished(), "idle-exit after both GUI views disconnect");
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// CLI start/stop while GUI subscribe is held.
    #[test]
    fn cli_start_stop_under_gui_hold() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();

        let mut spawner = FakeSpawner::new();
        spawner.push(slurp_ok("1,1 2x2"));
        let rec = make_recorder(&videos, spawner);
        let h = start_test_server(paths.clone(), rec, false);
        wait_sock(&paths);

        let (hold, _) = client::subscribe(&paths.socket_path).expect("subscribe");

        let start = client::request(&paths.socket_path, &IpcRequest::start_region(None)).unwrap();
        assert!(start.ok, "{start:?}");
        assert_eq!(start.status.state, "Recording");

        let stop = client::request(&paths.socket_path, &IpcRequest::stop()).unwrap();
        assert!(stop.ok, "{stop:?}");
        assert_eq!(stop.status.state, "Idle");

        drop(hold);
        let _ = client::request(&paths.socket_path, &IpcRequest::shutdown());
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// notify_start suppressed when gui_clients > 0 (via handle_request path).
    #[test]
    fn notify_start_suppressed_when_gui_clients_nonzero() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();

        #[derive(Default)]
        struct CountingNotifier {
            calls: Vec<(String, String)>,
        }
        impl Notifier for CountingNotifier {
            fn notify(&mut self, title: &str, body: &str) -> Result<(), PortError> {
                self.calls.push((title.into(), body.into()));
                Ok(())
            }
        }

        let mut spawner = FakeSpawner::new();
        spawner.push(slurp_ok("0,0 10x10"));
        let mut cfg = test_config(&videos);
        cfg.notify = true;
        cfg.notify_on_start_cli = true;
        let mut rec = Recorder::new(
            spawner,
            FakeClock::at_secs(1_705_322_245),
            CountingNotifier::default(),
            FakeClipboard,
            cfg,
        );

        // gui_clients = 1 → notify_start false inside handle_request.
        let out = handle_request(&mut rec, &IpcRequest::start_region(None), 1);
        assert!(out.response.ok, "{:?}", out.response);
        assert_eq!(out.response.status.state, "Recording");
        assert!(
            !rec.notifier()
                .calls
                .iter()
                .any(|(_, b)| b.contains("Recording started")),
            "start toast must be suppressed when GUI clients attached: {:?}",
            rec.notifier().calls
        );

        let _ = rec.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn handle_request_status_shape() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let mut rec = make_recorder(&videos, FakeSpawner::new());
        let out = handle_request(&mut rec, &IpcRequest::status(), 0);
        assert!(out.response.ok);
        assert_eq!(out.response.status.state, "Idle");
        assert!(!out.want_shutdown);

        let json = serde_json::to_value(&out.response.status).unwrap();
        for key in [
            "state",
            "output_path",
            "pid",
            "started_at_unix",
            "audio",
            "last_error",
            "last_success_path",
            "elapsed_ms",
        ] {
            assert!(json.get(key).is_some(), "missing status key {key}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn status_keys_while_recording() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let mut spawner = FakeSpawner::new();
        spawner.push(slurp_ok("0,0 1x1"));
        let rec = make_recorder(&videos, spawner);
        let h = start_test_server(paths.clone(), rec, false);
        wait_sock(&paths);
        let start = client::request(&paths.socket_path, &IpcRequest::start_region(None)).unwrap();
        let json = serde_json::to_value(&start.status).unwrap();
        for key in [
            "state",
            "output_path",
            "pid",
            "started_at_unix",
            "audio",
            "last_error",
            "last_success_path",
            "elapsed_ms",
        ] {
            assert!(json.get(key).is_some(), "missing {key} in {json}");
        }
        assert_eq!(json["state"], "Recording");
        assert!(json["output_path"].is_string());
        let _ = client::request(&paths.socket_path, &IpcRequest::shutdown());
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn malformed_ipc_returns_invalid_then_ping_works() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let rec = make_recorder(&videos, FakeSpawner::new());
        let h = start_test_server(paths.clone(), rec, false);
        wait_sock(&paths);

        let mut stream = UnixStream::connect(&paths.socket_path).unwrap();
        stream.write_all(b"{not json\n").unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let resp: IpcResponse = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(resp.code, "invalid");
        assert!(!resp.ok);

        let pong = client::request(&paths.socket_path, &IpcRequest::ping()).unwrap();
        assert!(pong.ok);

        let _ = client::request(&paths.socket_path, &IpcRequest::shutdown());
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn connection_io_error_does_not_kill_server() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let rec = make_recorder(&videos, FakeSpawner::new());
        let h = start_test_server(paths.clone(), rec, false);
        wait_sock(&paths);

        // Connect and drop without proper protocol — should not kill server.
        drop(UnixStream::connect(&paths.socket_path).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        let pong = client::request(&paths.socket_path, &IpcRequest::ping()).unwrap();
        assert!(pong.ok);

        let _ = client::request(&paths.socket_path, &IpcRequest::shutdown());
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn socket_mode_is_0600() {
        let root = temp_runtime();
        let paths = RuntimePaths::from_runtime_dir(&root);
        let bound = acquire_listener(&paths).unwrap();
        let meta = fs::metadata(&paths.socket_path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket mode {mode:o}");
        drop(bound);
        cleanup_runtime_files(&paths);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn pid_file_mode_is_0600() {
        let root = temp_runtime();
        let paths = RuntimePaths::from_runtime_dir(&root);
        let bound = acquire_listener(&paths).unwrap();
        let meta = fs::metadata(&paths.pid_path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "pid mode {mode:o}");
        drop(bound);
        cleanup_runtime_files(&paths);
        let _ = fs::remove_dir_all(&root);
    }

    /// Parked region client disconnect cancels selection (no Recording).
    #[test]
    fn pending_client_hangup_cancels_selection() {
        let root = temp_runtime();
        let videos = root.join("Videos");
        fs::create_dir_all(&videos).unwrap();
        let paths = RuntimePaths::from_runtime_dir(root.join("run"));
        fs::create_dir_all(&paths.runtime_dir).unwrap();

        let mut spawner = FakeSpawner::new();
        spawner.push(slurp_blocking());
        let rec = make_recorder(&videos, spawner);
        let h = start_test_server(paths.clone(), rec, false);
        wait_sock(&paths);

        let sock = paths.socket_path.clone();
        let starter = std::thread::spawn(move || {
            // Connect and start region, then drop without reading (simulate kill).
            let mut stream = UnixStream::connect(&sock).unwrap();
            let line = crate::ipc::encode_request(&IpcRequest::start_region(None)).unwrap();
            stream.write_all(line.as_bytes()).unwrap();
            stream.flush().unwrap();
            // Brief pause so server parks us, then drop = HUP.
            std::thread::sleep(Duration::from_millis(50));
            drop(stream);
        });
        let _ = starter.join();

        // Wait until server notices HUP and returns to Idle.
        let mut idle = false;
        for _ in 0..100 {
            if let Ok(st) = client::request(&paths.socket_path, &IpcRequest::status()) {
                if st.status.state == "Idle" {
                    idle = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(idle, "expected Idle after parked client hangup");

        let _ = client::request(&paths.socket_path, &IpcRequest::shutdown());
        let _ = h.join();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_runtime_dir_rejects_bare_tmp() {
        let err = validate_private_runtime_dir(Path::new("/tmp")).unwrap_err();
        assert!(err.to_string().contains("/tmp"), "{err}");
    }

    #[test]
    fn validate_runtime_dir_accepts_owned_private() {
        let root = temp_runtime();
        validate_private_runtime_dir(&root).expect("owned temp subdir ok");
        let _ = fs::remove_dir_all(&root);
    }
}
