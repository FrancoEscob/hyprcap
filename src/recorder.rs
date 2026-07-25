//! Recorder state machine: region/fullscreen spawn, cooperative stop, success hooks.
//!
//! States: `Idle | SelectingRegion | Starting | Recording | Stopping` (SPEC v1).
//!
//! `Drop` is a last-resort reap path; normal shutdown is [`Recorder::stop`].

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::ports::{
    absolutize_path, ChildHandle, Clipboard, Clock, CommandSpawner, ExitStatus, Notifier,
    PortError, Signal, SpawnOpts,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Normative session states (SPEC v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    SelectingRegion,
    Starting,
    Recording,
    Stopping,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "Idle",
            State::SelectingRegion => "SelectingRegion",
            State::Starting => "Starting",
            State::Recording => "Recording",
            State::Stopping => "Stopping",
        }
    }

    pub fn is_busy(self) -> bool {
        !matches!(self, State::Idle)
    }
}

/// Machine-readable result codes (IPC / CLI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineCode {
    Ok,
    Busy,
    NotRecording,
    DepMissing,
    SlurpCancel,
    SpawnFailed,
    StopTimeout,
    IoError,
    Invalid,
}

impl MachineCode {
    pub fn as_str(self) -> &'static str {
        match self {
            MachineCode::Ok => "ok",
            MachineCode::Busy => "busy",
            MachineCode::NotRecording => "not_recording",
            MachineCode::DepMissing => "dep_missing",
            MachineCode::SlurpCancel => "slurp_cancel",
            MachineCode::SpawnFailed => "spawn_failed",
            MachineCode::StopTimeout => "stop_timeout",
            MachineCode::IoError => "io_error",
            MachineCode::Invalid => "invalid",
        }
    }

    /// Suggested CLI exit code (SPEC table).
    pub fn exit_code(self) -> i32 {
        match self {
            MachineCode::Ok | MachineCode::SlurpCancel => 0,
            MachineCode::Busy => 2,
            MachineCode::DepMissing => 4,
            MachineCode::NotRecording => 3,
            _ => 1,
        }
    }
}

/// Outcome of a recorder command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub ok: bool,
    pub code: MachineCode,
    pub message: String,
    pub warnings: Vec<String>,
}

impl CommandResult {
    pub fn ok_msg(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            code: MachineCode::Ok,
            message: message.into(),
            warnings: Vec::new(),
        }
    }

    pub fn with_warnings(message: impl Into<String>, warnings: Vec<String>) -> Self {
        Self {
            ok: true,
            code: MachineCode::Ok,
            message: message.into(),
            warnings,
        }
    }

    pub fn err(code: MachineCode, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            code,
            message: message.into(),
            warnings: Vec::new(),
        }
    }

    pub fn is_success_with_warnings(&self) -> bool {
        self.ok && !self.warnings.is_empty()
    }
}

/// Snapshot for `status` / GUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub output_path: Option<PathBuf>,
    pub pid: Option<u32>,
    pub started_at_unix: Option<u64>,
    pub audio: bool,
    pub last_error: Option<String>,
    pub last_success_path: Option<PathBuf>,
    pub elapsed_ms: Option<u64>,
    /// Resolved Wayland output for one-monitor fullscreen (`-o`), if any.
    pub capture_output: Option<String>,
}

// ---------------------------------------------------------------------------
// Recorder
// ---------------------------------------------------------------------------

/// Owns at most one `wf-recorder` child and drives the SPEC state machine.
pub struct Recorder<S, C, N, Cl>
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    state: State,
    spawner: S,
    clock: C,
    notifier: N,
    clipboard: Cl,
    config: Config,

    /// Effective audio for the current / last session.
    audio: bool,
    /// Pending audio override while selecting a region.
    pending_audio: bool,
    /// Whether to fire start notify for the in-flight start (CLI vs GUI).
    notify_start: bool,

    slurp_child: Option<S::Child>,
    recorder_child: Option<S::Child>,

    output_path: Option<PathBuf>,
    started_at: Option<SystemTime>,
    last_error: Option<String>,
    last_success_path: Option<PathBuf>,
    /// Last reaped child exit (diagnostics; non-zero logged on cooperative success).
    last_child_exit: Option<ExitStatus>,

    /// Signals delivered during the current stop (debug / tests).
    stop_sent_signals: Vec<Signal>,
    /// Pid retained for `status` while Stopping (child taken out of Option for wait).
    stopping_pid: Option<u32>,
    /// Active fullscreen / one-monitor output name (fixed at start).
    capture_output: Option<String>,
    /// When set, used instead of probing hyprctl/wf-recorder (tests / inject).
    forced_output_inventory: Option<Vec<String>>,
}

impl<S, C, N, Cl> Recorder<S, C, N, Cl>
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    pub fn new(spawner: S, clock: C, notifier: N, clipboard: Cl, mut config: Config) -> Self {
        config.normalize_paths(None);
        let audio = config.audio_default;
        Self {
            state: State::Idle,
            spawner,
            clock,
            notifier,
            clipboard,
            config,
            audio,
            pending_audio: audio,
            notify_start: true,
            slurp_child: None,
            recorder_child: None,
            output_path: None,
            started_at: None,
            last_error: None,
            last_success_path: None,
            last_child_exit: None,
            stop_sent_signals: Vec::new(),
            stopping_pid: None,
            capture_output: None,
            forced_output_inventory: None,
        }
    }

    /// Inject output inventory for tests (avoids calling hyprctl).
    pub fn set_forced_output_inventory(&mut self, inventory: Option<Vec<String>>) {
        self.forced_output_inventory = inventory;
    }

    fn output_inventory(&self) -> Vec<String> {
        self.forced_output_inventory
            .clone()
            .unwrap_or_else(crate::sys::list_output_names)
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn set_config(&mut self, mut config: Config) {
        config.normalize_paths(None);
        self.config = config;
    }

    pub fn spawner(&self) -> &S {
        &self.spawner
    }

    pub fn spawner_mut(&mut self) -> &mut S {
        &mut self.spawner
    }

    pub fn notifier(&self) -> &N {
        &self.notifier
    }

    pub fn clipboard(&self) -> &Cl {
        &self.clipboard
    }

    pub fn stop_sent_signals(&self) -> &[Signal] {
        &self.stop_sent_signals
    }

    pub fn last_success_path(&self) -> Option<&Path> {
        self.last_success_path.as_deref()
    }

    pub fn last_child_exit(&self) -> Option<ExitStatus> {
        self.last_child_exit
    }

    pub fn status(&self) -> Status {
        let started_at_unix = self
            .started_at
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        let elapsed_ms = self.started_at.and_then(|start| {
            self.clock
                .now()
                .duration_since(start)
                .ok()
                .map(|d| d.as_millis() as u64)
        });
        let pid = self
            .recorder_child
            .as_ref()
            .map(|c| c.id())
            .or(self.stopping_pid);
        Status {
            state: self.state,
            output_path: self.output_path.clone(),
            pid,
            started_at_unix,
            audio: self.audio,
            last_error: self.last_error.clone(),
            last_success_path: self.last_success_path.clone(),
            elapsed_ms: if matches!(self.state, State::Recording | State::Stopping) {
                elapsed_ms
            } else {
                None
            },
            capture_output: if matches!(
                self.state,
                State::Starting | State::Recording | State::Stopping
            ) {
                self.capture_output.clone()
            } else {
                None
            },
        }
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    /// Full region start: slurp → geometry → wf-recorder → Recording.
    ///
    /// `notify_start`: when true and config allows, emit “Recording started”
    /// (CLI/keybind path). Server should pass false when a GUI client is attached.
    pub fn start_region(&mut self, audio: Option<bool>, notify_start: bool) -> CommandResult {
        if let Err(r) = self.begin_region(audio, notify_start) {
            return r;
        }
        self.complete_region_selection()
    }

    /// Idle → SelectingRegion and spawn `slurp` (does not wait).
    pub fn begin_region(
        &mut self,
        audio: Option<bool>,
        notify_start: bool,
    ) -> Result<(), CommandResult> {
        if self.state != State::Idle {
            return Err(CommandResult::err(
                MachineCode::Busy,
                format!("busy: state is {}", self.state.as_str()),
            ));
        }
        if !self.spawner.command_exists("slurp") {
            let msg = "missing hard dependency: slurp".to_string();
            self.last_error = Some(msg.clone());
            return Err(CommandResult::err(MachineCode::DepMissing, msg));
        }
        if !self.spawner.command_exists("wf-recorder") {
            let msg = "missing hard dependency: wf-recorder".to_string();
            self.last_error = Some(msg.clone());
            return Err(CommandResult::err(MachineCode::DepMissing, msg));
        }

        self.pending_audio = audio.unwrap_or(self.config.audio_default);
        self.notify_start = notify_start;
        self.last_error = None;

        let argv = vec!["slurp".to_string()];
        match self.spawner.spawn(&argv, SpawnOpts::default()) {
            Ok(child) => {
                self.slurp_child = Some(child);
                self.state = State::SelectingRegion;
                Ok(())
            }
            Err(e) => {
                let msg = format!("failed to spawn slurp: {e}");
                self.last_error = Some(msg.clone());
                self.state = State::Idle;
                Err(CommandResult::err(MachineCode::SpawnFailed, msg))
            }
        }
    }

    /// Wait for slurp; on geometry spawn wf-recorder → Recording.
    pub fn complete_region_selection(&mut self) -> CommandResult {
        if self.state != State::SelectingRegion {
            return CommandResult::err(
                MachineCode::Invalid,
                format!(
                    "complete_region_selection requires SelectingRegion, got {}",
                    self.state.as_str()
                ),
            );
        }

        let mut child = match self.slurp_child.take() {
            Some(c) => c,
            None => {
                self.state = State::Idle;
                return CommandResult::err(MachineCode::IoError, "slurp child missing");
            }
        };

        let status = match child.wait() {
            Ok(s) => s,
            Err(e) => {
                // Always reap on wait error — never orphan.
                force_reap_slurp(&mut child);
                self.state = State::Idle;
                let msg = format!("slurp wait failed: {e}");
                self.last_error = Some(msg.clone());
                return CommandResult::err(MachineCode::IoError, msg);
            }
        };
        self.finish_slurp(status, &mut child)
    }

    /// Non-blocking region progress for a daemon accept loop.
    ///
    /// Returns `None` while slurp is still running.
    pub fn poll_region_selection(&mut self) -> Option<CommandResult> {
        if self.state != State::SelectingRegion {
            return None;
        }
        let status = {
            let child = self.slurp_child.as_mut()?;
            match child.try_wait() {
                Ok(Some(s)) => s,
                Ok(None) => return None,
                Err(e) => {
                    // Take + force-reap so we never drop a live handle on poll error.
                    if let Some(mut child) = self.slurp_child.take() {
                        force_reap_slurp(&mut child);
                    }
                    self.state = State::Idle;
                    let msg = format!("slurp poll failed: {e}");
                    self.last_error = Some(msg.clone());
                    return Some(CommandResult::err(MachineCode::IoError, msg));
                }
            }
        };
        let mut child = match self.slurp_child.take() {
            Some(c) => c,
            None => {
                self.state = State::Idle;
                let msg = "slurp child missing after try_wait".to_string();
                self.last_error = Some(msg.clone());
                return Some(CommandResult::err(MachineCode::IoError, msg));
            }
        };
        Some(self.finish_slurp(status, &mut child))
    }

    /// Fullscreen / one-monitor start: always `wf-recorder -o NAME` (no `-g`).
    ///
    /// `output_override`: CLI/IPC `--output` / `output` (wins over config pin).
    /// `fps_override`: CLI/IPC `--fps` / `fps` (wins over config `one_fps`; `None` = Auto).
    pub fn start_fullscreen(
        &mut self,
        audio: Option<bool>,
        notify_start: bool,
        output_override: Option<&str>,
        fps_override: Option<u32>,
    ) -> CommandResult {
        if self.state != State::Idle {
            return CommandResult::err(
                MachineCode::Busy,
                format!("busy: state is {}", self.state.as_str()),
            );
        }
        if !self.spawner.command_exists("wf-recorder") {
            let msg = "missing hard dependency: wf-recorder".to_string();
            self.last_error = Some(msg.clone());
            return CommandResult::err(MachineCode::DepMissing, msg);
        }
        let inventory = self.output_inventory();
        let name = match resolve_fullscreen_output(
            output_override,
            self.config.fullscreen_output_override(),
            &inventory,
        ) {
            Ok(n) => n,
            Err(e) => {
                self.last_error = Some(e.clone());
                return CommandResult::err(MachineCode::Invalid, e);
            }
        };
        let fps = resolve_one_fps(fps_override, self.config.one_fps_override());
        let audio = audio.unwrap_or(self.config.audio_default);
        self.notify_start = notify_start;
        self.last_error = None;
        self.capture_output = Some(name.clone());
        self.spawn_recorder(None, audio, Some(name), fps)
    }

    /// Stop if selecting or recording; idle no-op success; Stopping is idempotent.
    pub fn stop(&mut self) -> CommandResult {
        match self.state {
            State::Idle => CommandResult::ok_msg("already idle"),
            State::SelectingRegion => self.cancel_slurp(),
            State::Starting => self.abort_starting(),
            State::Recording => self.stop_recording(),
            State::Stopping => {
                if self.recorder_child.is_some() {
                    self.stop_recording()
                } else {
                    CommandResult::ok_msg("already stopping/idle")
                }
            }
        }
    }

    /// Toggle: Idle→region; SelectingRegion→cancel; Recording→stop; Stopping→idempotent.
    ///
    /// `notify_start` applies only when starting from Idle.
    pub fn toggle_region(&mut self, audio: Option<bool>, notify_start: bool) -> CommandResult {
        match self.state {
            State::Idle => self.start_region(audio, notify_start),
            State::SelectingRegion => self.cancel_slurp(),
            State::Recording => self.stop_recording(),
            State::Stopping => self.stop(),
            State::Starting => self.abort_starting(),
        }
    }

    /// Poll non-blocking progress: region selection and unexpected recorder exit.
    pub fn poll(&mut self) -> Option<CommandResult> {
        match self.state {
            State::SelectingRegion => self.poll_region_selection(),
            State::Recording => self.poll_recorder_exited(),
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    fn finish_slurp(&mut self, status: ExitStatus, child: &mut S::Child) -> CommandResult {
        let geom = child.take_stdout().trim().to_string();
        let cancelled = !status.success() || geom.is_empty();
        if cancelled {
            self.state = State::Idle;
            return CommandResult {
                ok: true,
                code: MachineCode::SlurpCancel,
                message: "region selection cancelled".into(),
                warnings: Vec::new(),
            };
        }
        self.capture_output = None;
        self.spawn_recorder(Some(geom), self.pending_audio, None, None)
    }

    fn spawn_recorder(
        &mut self,
        geometry: Option<String>,
        audio: bool,
        fullscreen_output: Option<String>,
        fps: Option<u32>,
    ) -> CommandResult {
        self.state = State::Starting;
        self.audio = audio;

        if let Err(e) = ensure_output_dir(&self.config.output_dir) {
            self.state = State::Idle;
            self.capture_output = None;
            let msg = format!("cannot create output_dir: {e}");
            self.last_error = Some(msg.clone());
            return CommandResult::err(MachineCode::IoError, msg);
        }

        let path = match unique_output_path(&self.config.output_dir, &self.clock) {
            Ok(p) => absolutize_path(
                &p,
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            ),
            Err(e) => {
                self.state = State::Idle;
                self.capture_output = None;
                let msg = format!("cannot allocate output path: {e}");
                self.last_error = Some(msg.clone());
                return CommandResult::err(MachineCode::IoError, msg);
            }
        };

        // Fullscreen must always pass `-o` (never bare argv). Region uses `-g` only.
        if geometry.is_none() {
            let name = fullscreen_output
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if name.is_none() {
                self.state = State::Idle;
                self.capture_output = None;
                let msg = "internal: fullscreen spawn without resolved output".to_string();
                self.last_error = Some(msg.clone());
                return CommandResult::err(MachineCode::Invalid, msg);
            }
        }

        let argv = build_wf_recorder_argv(
            geometry.as_deref(),
            audio,
            &path,
            fullscreen_output.as_deref(),
            fps,
        );
        let opts = SpawnOpts {
            new_process_group: true,
        };

        match self.spawner.spawn(&argv, opts) {
            Ok(child) => {
                self.recorder_child = Some(child);
                self.output_path = Some(path);
                self.started_at = Some(self.clock.now());
                self.state = State::Recording;
                // Immediate fail (interactive output prompt, bad `-o`, etc.): surface
                // as start failure instead of a brief "Recording…" then toast.
                self.clock.sleep(Duration::from_millis(200));
                if let Some(early) = self.poll_recorder_exited() {
                    return early;
                }
                self.maybe_notify_start();
                let msg = if let Some(ref o) = self.capture_output {
                    format!("Recording {o}")
                } else {
                    "recording started".to_string()
                };
                CommandResult::ok_msg(msg)
            }
            Err(e) => {
                self.state = State::Idle;
                self.recorder_child = None;
                self.output_path = None;
                self.started_at = None;
                self.capture_output = None;
                let msg = format!("failed to spawn wf-recorder: {e}");
                self.last_error = Some(msg.clone());
                CommandResult::err(MachineCode::SpawnFailed, msg)
            }
        }
    }

    fn maybe_notify_start(&mut self) {
        if self.notify_start && self.config.notify && self.config.notify_on_start_cli {
            let body = if let Some(ref o) = self.capture_output {
                format!("Recording {o}")
            } else {
                "Recording started".to_string()
            };
            let _ = self.notifier.notify("record-ui", &body);
        }
    }

    /// TERM → short wait → KILL → blocking reap (SPEC: always reap).
    fn cancel_slurp(&mut self) -> CommandResult {
        if let Some(mut child) = self.slurp_child.take() {
            force_reap_slurp(&mut child);
        }
        self.state = State::Idle;
        CommandResult {
            ok: true,
            code: MachineCode::SlurpCancel,
            message: "region selection cancelled".into(),
            warnings: Vec::new(),
        }
    }

    fn abort_starting(&mut self) -> CommandResult {
        if self.recorder_child.is_some() {
            // Defensive: child present while Starting → cooperative stop path.
            return self.stop_recording();
        }
        self.state = State::Idle;
        self.output_path = None;
        self.started_at = None;
        self.capture_output = None;
        CommandResult::ok_msg("aborted start (no child)")
    }

    fn stop_recording(&mut self) -> CommandResult {
        self.state = State::Stopping;
        self.stop_sent_signals.clear();

        let mut child = match self.recorder_child.take() {
            Some(c) => c,
            None => {
                self.state = State::Idle;
                self.stopping_pid = None;
                return CommandResult::err(MachineCode::IoError, "recorder child missing");
            }
        };
        self.stopping_pid = Some(child.id());

        // 1) SIGINT to process group (cooperative).
        if let Err(e) = child.signal_group(Signal::Interrupt) {
            self.last_error = Some(format!("SIGINT to process group failed: {e}"));
        }
        self.stop_sent_signals.push(Signal::Interrupt);

        let int_timeout = self.config.stop_timeout();
        match child.wait_timeout(int_timeout) {
            Ok(Some(status)) => {
                let stderr = child.take_stderr_tail();
                return self.finish_after_reap(status, true, stderr);
            }
            Ok(None) => { /* escalate */ }
            Err(e) => {
                self.last_error = Some(format!("wait after SIGINT failed: {e}"));
            }
        }

        // 2) SIGTERM to process group.
        let _ = child.signal_group(Signal::Terminate);
        self.stop_sent_signals.push(Signal::Terminate);

        let term_timeout = self.config.stop_term_timeout();
        match child.wait_timeout(term_timeout) {
            Ok(Some(status)) => {
                let stderr = child.take_stderr_tail();
                self.finish_after_reap(status, true, stderr)
            }
            Ok(None) | Err(_) => {
                // 3) Nuclear SIGKILL; always blocking reap.
                let _ = child.signal_group(Signal::Kill);
                self.stop_sent_signals.push(Signal::Kill);
                let stderr = self.reap_nuclear(&mut child);
                self.finish_stop_timeout(stderr)
            }
        }
    }

    /// Guaranteed reap after nuclear SIGKILL.
    fn reap_nuclear(&mut self, child: &mut S::Child) -> String {
        force_reap_after_kill(child, &self.clock);
        child.take_stderr_tail()
    }

    fn finish_stop_timeout(&mut self, stderr: String) -> CommandResult {
        let path = self.output_path.take();
        self.started_at = None;
        self.recorder_child = None;
        self.stopping_pid = None;
        self.capture_output = None;
        self.state = State::Idle;
        let mut msg = format!(
            "stop timed out after SIGINT/SIGTERM; nuclear SIGKILL used to reap; file may be corrupt{}",
            path.as_ref()
                .map(|p| format!(" ({})", p.display()))
                .unwrap_or_default()
        );
        if !stderr.is_empty() {
            msg.push_str("; stderr: ");
            msg.push_str(&stderr);
        }
        self.last_error = Some(msg.clone());
        if self.config.notify {
            let _ = self.notifier.notify("record-ui", &msg);
        }
        CommandResult::err(MachineCode::StopTimeout, msg)
    }

    /// After child reaped: evaluate success predicate.
    ///
    /// `cooperative` = SIGINT/SIGTERM stop path (or clean exit after our stop).
    /// Unexpected death via [`poll`] uses `cooperative = false`.
    fn finish_after_reap(
        &mut self,
        status: ExitStatus,
        cooperative: bool,
        stderr: String,
    ) -> CommandResult {
        self.last_child_exit = Some(status);
        let path = self.output_path.clone();
        self.started_at = None;
        self.recorder_child = None;
        self.stopping_pid = None;
        self.capture_output = None;
        self.state = State::Idle;

        let path = match path {
            Some(p) => p,
            None => {
                let msg = "output path missing after stop".to_string();
                self.last_error = Some(msg.clone());
                return CommandResult::err(MachineCode::IoError, msg);
            }
        };

        let file_ok = match file_nonempty(&path) {
            Ok(v) => v,
            Err(e) => {
                let mut msg = format!("cannot stat output file: {e}");
                append_stderr(&mut msg, &stderr);
                self.last_error = Some(msg.clone());
                self.output_path = None;
                if self.config.notify {
                    let _ = self.notifier.notify("record-ui", &msg);
                }
                return CommandResult::err(MachineCode::IoError, msg);
            }
        };

        if !file_ok {
            let mut msg = format!(
                "recording failed: output missing or empty ({})",
                path.display()
            );
            append_stderr(&mut msg, &stderr);
            self.last_error = Some(msg.clone());
            self.output_path = None;
            if self.config.notify {
                let _ = self.notifier.notify("record-ui", &msg);
            }
            return CommandResult::err(MachineCode::IoError, msg);
        }

        if !cooperative {
            let mut msg = format!(
                "recording failed: unexpected child exit ({status}), path={}",
                path.display()
            );
            append_stderr(&mut msg, &stderr);
            self.last_error = Some(msg.clone());
            self.output_path = None;
            if self.config.notify {
                let _ = self.notifier.notify("record-ui", &msg);
            }
            return CommandResult::err(MachineCode::IoError, msg);
        }

        // Success path (exit may be non-zero).
        self.last_success_path = Some(path.clone());
        self.output_path = None;
        self.last_error = None;

        let mut warnings = Vec::new();
        let path_str = path.display().to_string();

        if self.config.copy_path {
            if let Err(e) = self.clipboard.copy_text(&path_str) {
                warnings.push(format!("clipboard failed: {e}"));
            }
        }

        if self.config.notify {
            let body = if self.config.copy_path && warnings.is_empty() {
                format!("Saved {path_str}\nPath copied to clipboard")
            } else if self.config.copy_path {
                format!("Saved {path_str}\n(clipboard warning)")
            } else {
                format!("Saved {path_str}")
            };
            if let Err(e) = self.notifier.notify("record-ui", &body) {
                warnings.push(format!("notify failed: {e}"));
            }
        }

        let mut message = format!("saved {path_str}");
        if !status.success() {
            // SPEC: capture non-zero exit in logs / message on cooperative success.
            message.push_str(&format!(" (child {status})"));
        }

        if warnings.is_empty() {
            CommandResult::ok_msg(message)
        } else {
            CommandResult::with_warnings(message, warnings)
        }
    }

    fn poll_recorder_exited(&mut self) -> Option<CommandResult> {
        let status = {
            let child = self.recorder_child.as_mut()?;
            match child.try_wait() {
                Ok(Some(s)) => s,
                Ok(None) => return None,
                Err(e) => {
                    // Force group reap so we never drop a live wf-recorder handle.
                    if let Some(mut child) = self.recorder_child.take() {
                        force_reap_recorder(&mut child, &self.clock);
                    }
                    let msg = format!("recorder poll failed: {e}");
                    self.last_error = Some(msg.clone());
                    self.output_path = None;
                    self.started_at = None;
                    self.stopping_pid = None;
                    self.capture_output = None;
                    self.state = State::Idle;
                    return Some(CommandResult::err(MachineCode::IoError, msg));
                }
            }
        };
        let mut child = match self.recorder_child.take() {
            Some(c) => c,
            None => {
                self.state = State::Idle;
                self.output_path = None;
                self.started_at = None;
                self.stopping_pid = None;
                self.capture_output = None;
                let msg = "recorder child missing after try_wait".to_string();
                self.last_error = Some(msg.clone());
                return Some(CommandResult::err(MachineCode::IoError, msg));
            }
        };
        let stderr = child.take_stderr_tail();
        // Unexpected death is not a cooperative stop context.
        Some(self.finish_after_reap(status, false, stderr))
    }
}

// ---------------------------------------------------------------------------
// Shared force-reap helpers (cancel / poll-error / Drop / nuclear)
// ---------------------------------------------------------------------------

/// Slurp: TERM → short wait → KILL → blocking wait. Always best-effort reap.
fn force_reap_slurp<H: ChildHandle>(child: &mut H) {
    let _ = child.signal(Signal::Terminate);
    match child.wait_timeout(Duration::from_millis(500)) {
        Ok(Some(_)) => {}
        _ => {
            let _ = child.signal(Signal::Kill);
            force_reap_after_kill(child, &NullClock);
        }
    }
}

/// wf-recorder process group: INT → TERM → KILL with short waits, then blocking reap.
fn force_reap_recorder<H: ChildHandle>(child: &mut H, clock: &dyn Clock) {
    let _ = child.signal_group(Signal::Interrupt);
    if child
        .wait_timeout(Duration::from_millis(100))
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    let _ = child.signal_group(Signal::Terminate);
    if child
        .wait_timeout(Duration::from_millis(100))
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    let _ = child.signal_group(Signal::Kill);
    force_reap_after_kill(child, clock);
}

/// After SIGKILL (or when wait is unreliable): blocking wait + try_wait fallback.
fn force_reap_after_kill<H: ChildHandle>(child: &mut H, clock: &dyn Clock) {
    match child.wait() {
        Ok(_) => {}
        Err(_) => {
            for _ in 0..50 {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                clock.sleep(Duration::from_millis(10));
            }
            let _ = child.try_wait();
        }
    }
}

/// Clock used only for Drop / slurp force-reap when no real clock is available.
struct NullClock;

impl Clock for NullClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn sleep(&self, _duration: Duration) {}
}

/// Last-resort cleanup: never intentionally orphan children on drop/panic paths.
///
/// Normal stop is [`Recorder::stop`]. Drop does not run success hooks.
impl<S, C, N, Cl> Drop for Recorder<S, C, N, Cl>
where
    S: CommandSpawner,
    C: Clock,
    N: Notifier,
    Cl: Clipboard,
{
    fn drop(&mut self) {
        if let Some(mut child) = self.slurp_child.take() {
            force_reap_slurp(&mut child);
        }
        if let Some(mut child) = self.recorder_child.take() {
            force_reap_recorder(&mut child, &self.clock);
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Pure fullscreen output resolution (inject inventory; no hyprctl in tests).
///
/// Chain: request override → config pin → sole inventory name → Err.
/// Never uses focused-output-after-click (DUAL-MONITOR §6.1).
pub fn resolve_fullscreen_output(
    override_name: Option<&str>,
    config_pin: Option<&str>,
    inventory: &[String],
) -> Result<String, String> {
    let override_name = override_name.map(str::trim).filter(|s| !s.is_empty());
    let config_pin = config_pin.map(str::trim).filter(|s| !s.is_empty());
    let chosen = override_name.or(config_pin);

    if let Some(name) = chosen {
        if inventory.is_empty() {
            // Still allow? SPEC: empty inventory → Err. Pin not in inventory also Err.
            return Err(format!(
                "cannot resolve Wayland output {name:?}: no outputs discovered \
                 (is hyprctl/wf-recorder available?). Set fullscreen_output only when \
                 list-outputs shows names."
            ));
        }
        if inventory.iter().any(|n| n == name) {
            return Ok(name.to_string());
        }
        return Err(format!(
            "unknown Wayland output {name:?}; known: {}. \
             Use record-ui list-outputs or set fullscreen_output in \
             ~/.config/record-ui/config.toml",
            format_known_outputs(inventory)
        ));
    }

    match inventory.len() {
        0 => Err("cannot resolve Wayland output: no outputs discovered \
             (hyprctl monitors / wf-recorder -L). Multi-monitor setups need \
             fullscreen_output in ~/.config/record-ui/config.toml or --output NAME \
             (record-ui list-outputs)."
            .to_string()),
        1 => Ok(inventory[0].clone()),
        _ => Err(format!(
            "multi-monitor: set fullscreen_output in ~/.config/record-ui/config.toml \
             or pass --output NAME (known: {}). See also: record-ui list-outputs",
            format_known_outputs(inventory)
        )),
    }
}

/// One-monitor FPS: CLI/IPC override → config `one_fps` → Auto (`None`, no `-r`).
///
/// Values of `0` are treated as Auto (never emit `wf-recorder -r 0`).
pub fn resolve_one_fps(override_fps: Option<u32>, config_fps: Option<u32>) -> Option<u32> {
    override_fps.or(config_fps).filter(|&n| n > 0)
}

fn format_known_outputs(inventory: &[String]) -> String {
    if inventory.is_empty() {
        "(none)".to_string()
    } else {
        inventory.join(", ")
    }
}

/// Build argv for wf-recorder (never shell).
///
/// `output`: Wayland output name for fullscreen (`-o`). Ignored when `geometry`
/// is set (region already pins the capture area).
/// `fps`: One-monitor only — when `geometry` is `None` and `fps` is `Some(n)` with
/// `n > 0`, emit `-r n`. Region (`geometry` set) always omits `-r` even if `fps`
/// is `Some` (defensive; production region path passes `None`).
///
/// Fullscreen production paths must pass a non-empty `output` so `-o` is always present.
/// Order (DUAL-MONITOR §6.3): `wf-recorder -o NAME [-r FPS] [-a] -f path`.
pub fn build_wf_recorder_argv(
    geometry: Option<&str>,
    audio: bool,
    path: &Path,
    output: Option<&str>,
    fps: Option<u32>,
) -> Vec<String> {
    let mut argv = vec!["wf-recorder".to_string()];
    let region = if let Some(g) = geometry {
        argv.push("-g".into());
        argv.push(g.to_string());
        true
    } else {
        if let Some(o) = output.map(str::trim).filter(|s| !s.is_empty()) {
            argv.push("-o".into());
            argv.push(o.to_string());
        }
        false
    };
    // FPS is One-monitor only; never combine `-g` and `-r`.
    if !region {
        if let Some(r) = fps.filter(|&n| n > 0) {
            argv.push("-r".into());
            argv.push(r.to_string());
        }
    }
    if audio {
        argv.push("-a".into());
    }
    argv.push("-f".into());
    argv.push(path.display().to_string());
    argv
}

/// `rec-YYYYMMDD-HHMMSS.mp4` with `-N` on collision.
pub fn unique_output_path(output_dir: &Path, clock: &dyn Clock) -> Result<PathBuf, PortError> {
    let stamp = format_timestamp(clock.now())?;
    let candidate = output_dir.join(format!("rec-{stamp}.mp4"));
    if !candidate.exists() {
        return Ok(candidate);
    }
    for n in 1..10_000 {
        let p = output_dir.join(format!("rec-{stamp}-{n}.mp4"));
        if !p.exists() {
            return Ok(p);
        }
    }
    Err(PortError::Io("too many filename collisions".into()))
}

fn format_timestamp(now: SystemTime) -> Result<String, PortError> {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_err(|e| PortError::Other(format!("clock before epoch: {e}")))?
        .as_secs();
    let (y, m, d, hh, mm, ss) = civil_utc(secs);
    Ok(format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}"))
}

/// Unix epoch seconds → UTC civil date/time (Howard Hinnant algorithm).
fn civil_utc(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let ss = (secs % 60) as u32;
    let mins = secs / 60;
    let mm = (mins % 60) as u32;
    let hours = mins / 60;
    let hh = (hours % 24) as u32;
    let days = (hours / 24) as i64;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (y, m, d, hh, mm, ss)
}

fn ensure_output_dir(dir: &Path) -> Result<(), PortError> {
    fs::create_dir_all(dir).map_err(|e| PortError::Io(format!("{}: {e}", dir.display())))
}

fn file_nonempty(path: &Path) -> Result<bool, PortError> {
    match fs::metadata(path) {
        Ok(meta) => Ok(meta.len() > 0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(PortError::Io(e.to_string())),
    }
}

fn append_stderr(msg: &mut String, stderr: &str) {
    if !stderr.is_empty() {
        msg.push_str("; stderr: ");
        msg.push_str(stderr);
    }
}

// ---------------------------------------------------------------------------
// Unit tests U1–U17 (mock spawner, fake child, fake clock, temp dirs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{Paths, PortError};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // ----- fakes -----

    #[derive(Debug, Clone)]
    struct SignalEvent {
        #[allow(dead_code)]
        pid: u32,
        signal: Signal,
        group: bool,
    }

    #[derive(Debug, Clone)]
    struct SpawnRecord {
        argv: Vec<String>,
        opts: SpawnOpts,
    }

    /// Scripted child behaviour for tests.
    #[derive(Debug, Clone)]
    struct FakeChildScript {
        pid: u32,
        exit: ExitStatus,
        stdout: String,
        stderr: String,
        ignore_sigint: bool,
        ignore_sigterm: bool,
        /// Bytes to write on SIGINT/TERM when path known. `None` = do not write.
        write_bytes_on_signal: Option<Vec<u8>>,
        write_file_on_signal: Option<PathBuf>,
        wait_for_signal: bool,
        /// If set, child is already dead at spawn (for poll unexpected exit).
        already_dead: bool,
        /// Survive this many `try_wait` calls, then auto-exit (spawn settle uses one).
        auto_exit_after_try_waits: Option<u32>,
    }

    impl Default for FakeChildScript {
        fn default() -> Self {
            Self {
                pid: 1000,
                exit: ExitStatus::Code(0),
                stdout: String::new(),
                stderr: String::new(),
                ignore_sigint: false,
                ignore_sigterm: false,
                write_bytes_on_signal: Some(b"fake-video-bytes".to_vec()),
                write_file_on_signal: None,
                wait_for_signal: false,
                already_dead: false,
                auto_exit_after_try_waits: None,
            }
        }
    }

    struct FakeChild {
        pid: u32,
        exit: ExitStatus,
        stdout: String,
        stderr: String,
        ignore_sigint: bool,
        ignore_sigterm: bool,
        write_bytes_on_signal: Option<Vec<u8>>,
        write_file_on_signal: Option<PathBuf>,
        wait_for_signal: bool,
        alive: bool,
        saw_sigint: bool,
        saw_sigterm: bool,
        saw_kill: bool,
        log: Arc<Mutex<Vec<SignalEvent>>>,
        auto_exit_after_try_waits: Option<u32>,
        try_wait_calls: u32,
    }

    impl FakeChild {
        fn from_script(script: FakeChildScript, log: Arc<Mutex<Vec<SignalEvent>>>) -> Self {
            Self {
                pid: script.pid,
                exit: script.exit,
                stdout: script.stdout,
                stderr: script.stderr,
                ignore_sigint: script.ignore_sigint,
                ignore_sigterm: script.ignore_sigterm,
                write_bytes_on_signal: script.write_bytes_on_signal,
                write_file_on_signal: script.write_file_on_signal,
                wait_for_signal: script.wait_for_signal,
                alive: !script.already_dead,
                saw_sigint: false,
                saw_sigterm: false,
                saw_kill: false,
                log,
                auto_exit_after_try_waits: script.auto_exit_after_try_waits,
                try_wait_calls: 0,
            }
        }

        fn maybe_write_file(&self) {
            if let (Some(ref p), Some(ref bytes)) =
                (&self.write_file_on_signal, &self.write_bytes_on_signal)
            {
                let _ = fs::write(p, bytes);
            }
        }

        fn apply_signal(&mut self, signal: Signal, group: bool) {
            self.log.lock().unwrap().push(SignalEvent {
                pid: self.pid,
                signal,
                group,
            });
            match signal {
                Signal::Interrupt => {
                    self.saw_sigint = true;
                    self.maybe_write_file();
                    if !self.ignore_sigint {
                        self.alive = false;
                    }
                }
                Signal::Terminate => {
                    self.saw_sigterm = true;
                    self.maybe_write_file();
                    if !self.ignore_sigterm {
                        self.alive = false;
                    }
                }
                Signal::Kill => {
                    self.saw_kill = true;
                    self.alive = false;
                }
            }
        }
    }

    impl ChildHandle for FakeChild {
        fn id(&self) -> u32 {
            self.pid
        }

        fn signal(&mut self, signal: Signal) -> Result<(), PortError> {
            self.apply_signal(signal, false);
            Ok(())
        }

        fn signal_group(&mut self, signal: Signal) -> Result<(), PortError> {
            self.apply_signal(signal, true);
            Ok(())
        }

        fn try_wait(&mut self) -> Result<Option<ExitStatus>, PortError> {
            self.try_wait_calls = self.try_wait_calls.saturating_add(1);
            if self.alive {
                if let Some(n) = self.auto_exit_after_try_waits {
                    if self.try_wait_calls > n {
                        self.alive = false;
                        return Ok(Some(self.exit));
                    }
                }
                Ok(None)
            } else {
                Ok(Some(self.exit))
            }
        }

        fn wait(&mut self) -> Result<ExitStatus, PortError> {
            if self.wait_for_signal && self.alive {
                if self.saw_sigint || self.saw_sigterm || self.saw_kill {
                    self.alive = false;
                    return Ok(self.exit);
                }
                // Sticky child forced to blocking wait: treat as exit after signal path
                // only. For nuclear reap after Kill, saw_kill is set.
                self.alive = false;
                return Ok(self.exit);
            }
            self.alive = false;
            Ok(self.exit)
        }

        fn wait_timeout(&mut self, _timeout: Duration) -> Result<Option<ExitStatus>, PortError> {
            if !self.alive {
                return Ok(Some(self.exit));
            }
            if self.wait_for_signal {
                if self.saw_kill {
                    self.alive = false;
                    return Ok(Some(self.exit));
                }
                if self.saw_sigterm && !self.ignore_sigterm {
                    self.alive = false;
                    return Ok(Some(self.exit));
                }
                if self.saw_sigint && !self.ignore_sigint {
                    self.alive = false;
                    return Ok(Some(self.exit));
                }
                return Ok(None);
            }
            self.alive = false;
            Ok(Some(self.exit))
        }

        fn take_stdout(&mut self) -> String {
            std::mem::take(&mut self.stdout)
        }

        fn take_stderr_tail(&mut self) -> String {
            std::mem::take(&mut self.stderr)
        }
    }

    struct FakeSpawner {
        next_pid: u32,
        scripts: VecDeque<FakeChildScript>,
        fail_binary: Option<String>,
        missing: Vec<String>,
        spawns: Vec<SpawnRecord>,
        signal_log: Arc<Mutex<Vec<SignalEvent>>>,
        /// When spawning wf-recorder, auto-attach write path from `-f`.
        auto_write_on_signal: bool,
        /// Bytes written on signal when auto_write is on (`None` = missing file).
        auto_write_bytes: Option<Vec<u8>>,
        recorder_ignore_sigint: bool,
        recorder_ignore_sigterm: bool,
        recorder_exit: ExitStatus,
        recorder_stderr: String,
    }

    impl FakeSpawner {
        fn new() -> Self {
            Self {
                next_pid: 2000,
                scripts: VecDeque::new(),
                fail_binary: None,
                missing: Vec::new(),
                spawns: Vec::new(),
                signal_log: Arc::new(Mutex::new(Vec::new())),
                auto_write_on_signal: true,
                auto_write_bytes: Some(b"fake-video-bytes".to_vec()),
                recorder_ignore_sigint: false,
                recorder_ignore_sigterm: false,
                recorder_exit: ExitStatus::Code(0),
                recorder_stderr: String::new(),
            }
        }

        fn push_script(&mut self, script: FakeChildScript) {
            self.scripts.push_back(script);
        }

        fn signal_log(&self) -> Vec<SignalEvent> {
            self.signal_log.lock().unwrap().clone()
        }

        fn wf_spawns(&self) -> Vec<&SpawnRecord> {
            self.spawns
                .iter()
                .filter(|s| s.argv.first().map(|a| a == "wf-recorder").unwrap_or(false))
                .collect()
        }

        fn slurp_spawns(&self) -> usize {
            self.spawns
                .iter()
                .filter(|s| s.argv.first().map(|a| a == "slurp").unwrap_or(false))
                .count()
        }
    }

    impl CommandSpawner for FakeSpawner {
        type Child = FakeChild;

        fn spawn(&mut self, argv: &[String], opts: SpawnOpts) -> Result<Self::Child, PortError> {
            let bin = argv.first().cloned().unwrap_or_default();
            self.spawns.push(SpawnRecord {
                argv: argv.to_vec(),
                opts: opts.clone(),
            });

            if let Some(ref fail) = self.fail_binary {
                if fail == &bin {
                    return Err(PortError::Spawn(format!("simulated spawn fail for {bin}")));
                }
            }

            let pid = self.next_pid;
            self.next_pid += 1;

            let mut script = self.scripts.pop_front().unwrap_or(FakeChildScript {
                pid,
                exit: self.recorder_exit,
                stdout: String::new(),
                stderr: self.recorder_stderr.clone(),
                ignore_sigint: self.recorder_ignore_sigint,
                ignore_sigterm: self.recorder_ignore_sigterm,
                write_bytes_on_signal: self.auto_write_bytes.clone(),
                write_file_on_signal: None,
                wait_for_signal: bin == "wf-recorder",
                already_dead: false,
                auto_exit_after_try_waits: None,
            });
            script.pid = pid;

            if bin == "wf-recorder" {
                if self.auto_write_on_signal {
                    if let Some(i) = argv.iter().position(|a| a == "-f") {
                        if let Some(p) = argv.get(i + 1) {
                            script.write_file_on_signal = Some(PathBuf::from(p));
                        }
                    }
                    script.write_bytes_on_signal = self.auto_write_bytes.clone();
                } else {
                    script.write_file_on_signal = None;
                    script.write_bytes_on_signal = None;
                }
                script.wait_for_signal = true;
                script.ignore_sigint = self.recorder_ignore_sigint;
                script.ignore_sigterm = self.recorder_ignore_sigterm;
                script.exit = self.recorder_exit;
                if script.stderr.is_empty() {
                    script.stderr = self.recorder_stderr.clone();
                }
            }

            Ok(FakeChild::from_script(script, Arc::clone(&self.signal_log)))
        }

        fn command_exists(&self, binary: &str) -> bool {
            !self.missing.iter().any(|m| m == binary)
        }
    }

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
        fn sleep(&self, _duration: Duration) {}
    }

    #[derive(Default)]
    struct FakeNotifier {
        calls: Vec<(String, String)>,
        fail: bool,
    }

    impl Notifier for FakeNotifier {
        fn notify(&mut self, title: &str, body: &str) -> Result<(), PortError> {
            if self.fail {
                return Err(PortError::Other("notify failed".into()));
            }
            self.calls.push((title.to_string(), body.to_string()));
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeClipboard {
        texts: Vec<String>,
        fail: bool,
    }

    impl Clipboard for FakeClipboard {
        fn copy_text(&mut self, text: &str) -> Result<(), PortError> {
            if self.fail {
                return Err(PortError::Other("clipboard failed".into()));
            }
            self.texts.push(text.to_string());
            Ok(())
        }
    }

    struct TempPaths {
        root: PathBuf,
    }

    impl TempPaths {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "record-ui-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(root.join("Videos")).unwrap();
            fs::create_dir_all(root.join("config")).unwrap();
            fs::create_dir_all(root.join("runtime")).unwrap();
            Self { root }
        }

        fn config(&self) -> Config {
            Config {
                output_dir: self.root.join("Videos"),
                audio_default: false,
                copy_path: true,
                notify: true,
                notify_on_start_cli: true,
                stop_timeout_ms: 50,
                stop_term_timeout_ms: 50,
                // Avoid calling real hyprctl from unit tests.
                fullscreen_output: Some("TEST-OUT".into()),
                one_fps: None,
            }
        }
    }

    impl Paths for TempPaths {
        fn config_path(&self) -> PathBuf {
            self.root.join("config").join("config.toml")
        }
        fn output_dir(&self) -> PathBuf {
            self.root.join("Videos")
        }
        fn runtime_dir(&self) -> PathBuf {
            self.root.join("runtime")
        }
        fn home_dir(&self) -> PathBuf {
            self.root.clone()
        }
    }

    impl Drop for TempPaths {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Fixed clock: 2024-01-15 12:37:25 UTC (unix 1_705_322_245).
    const TEST_UNIX: u64 = 1_705_322_245;

    fn make_recorder(
        paths: &TempPaths,
        spawner: FakeSpawner,
    ) -> Recorder<FakeSpawner, FakeClock, FakeNotifier, FakeClipboard> {
        let clock = FakeClock::at_secs(TEST_UNIX);
        let mut rec = Recorder::new(
            spawner,
            clock,
            FakeNotifier::default(),
            FakeClipboard::default(),
            paths.config(),
        );
        // Inventory must include config pin TEST-OUT; avoid real hyprctl.
        rec.set_forced_output_inventory(Some(vec!["TEST-OUT".into()]));
        rec
    }

    fn slurp_ok(geom: &str) -> FakeChildScript {
        FakeChildScript {
            pid: 0,
            exit: ExitStatus::Code(0),
            stdout: geom.to_string(),
            wait_for_signal: false,
            write_bytes_on_signal: None,
            ..Default::default()
        }
    }

    fn slurp_cancel_empty() -> FakeChildScript {
        FakeChildScript {
            exit: ExitStatus::Code(0),
            stdout: String::new(),
            write_bytes_on_signal: None,
            ..Default::default()
        }
    }

    fn slurp_cancel_nonzero() -> FakeChildScript {
        FakeChildScript {
            exit: ExitStatus::Code(1),
            stdout: String::new(),
            write_bytes_on_signal: None,
            ..Default::default()
        }
    }

    // ----- U1–U17 -----

    /// U1: region start, slurp returns geom → argv has -g, geom, -f; state Recording
    #[test]
    fn u1_region_start_argv_and_recording() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("10,20 300x200"));
        let mut rec = make_recorder(&paths, spawner);

        let r = rec.start_region(None, true);
        assert!(r.ok, "{r:?}");
        assert_eq!(rec.state(), State::Recording);

        let wf = rec.spawner().wf_spawns();
        assert_eq!(wf.len(), 1);
        let argv = &wf[0].argv;
        assert_eq!(argv[0], "wf-recorder");
        assert!(argv.contains(&"-g".into()));
        let gpos = argv.iter().position(|a| a == "-g").unwrap();
        assert_eq!(argv[gpos + 1], "10,20 300x200");
        assert!(argv.contains(&"-f".into()));
        assert!(wf[0].opts.new_process_group);
        assert!(!argv.iter().any(|a| a == "-a"));
        let fpos = argv.iter().position(|a| a == "-f").unwrap();
        assert!(
            Path::new(&argv[fpos + 1]).is_absolute(),
            "output path must be absolute"
        );
    }

    /// U2: slurp empty/cancel → no wf-recorder; Idle; slurp_cancel
    #[test]
    fn u2_slurp_empty_cancel() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_cancel_empty());
        let mut rec = make_recorder(&paths, spawner);

        let r = rec.start_region(None, true);
        assert!(r.ok);
        assert_eq!(r.code, MachineCode::SlurpCancel);
        assert_eq!(rec.state(), State::Idle);
        assert!(rec.spawner().wf_spawns().is_empty());
    }

    #[test]
    fn u2_slurp_nonzero_cancel() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_cancel_nonzero());
        let mut rec = make_recorder(&paths, spawner);

        let r = rec.start_region(None, true);
        assert_eq!(r.code, MachineCode::SlurpCancel);
        assert_eq!(rec.state(), State::Idle);
        assert!(rec.spawner().wf_spawns().is_empty());
    }

    /// U3: audio on/off → -a present/absent
    #[test]
    fn u3_audio_on_off() {
        let paths = TempPaths::new();

        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("0,0 1x1"));
        let mut rec = make_recorder(&paths, spawner);
        rec.start_region(Some(true), true);
        let argv = &rec.spawner().wf_spawns()[0].argv;
        assert!(argv.iter().any(|a| a == "-a"));

        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("0,0 1x1"));
        let mut rec = make_recorder(&paths, spawner);
        rec.start_region(Some(false), true);
        let argv = &rec.spawner().wf_spawns()[0].argv;
        assert!(!argv.iter().any(|a| a == "-a"));
    }

    /// U4: fullscreen start → no -g; always has -o with non-empty name
    #[test]
    fn u4_fullscreen_no_geometry() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let mut rec = make_recorder(&paths, spawner);

        let r = rec.start_fullscreen(None, true, None, None);
        assert!(r.ok, "{r:?}");
        assert_eq!(rec.state(), State::Recording);
        assert!(r.message.contains("TEST-OUT"), "{r:?}");
        let argv = &rec.spawner().wf_spawns()[0].argv;
        assert!(!argv.iter().any(|a| a == "-g"));
        assert!(argv.contains(&"-f".into()));
        let opos = argv.iter().position(|a| a == "-o").expect("-o required");
        assert!(!argv[opos + 1].is_empty());
        assert_eq!(argv[opos + 1], "TEST-OUT");
        assert_eq!(rec.status().capture_output.as_deref(), Some("TEST-OUT"));
        assert_eq!(rec.spawner().slurp_spawns(), 0);
    }

    /// U5: double start → second busy; one child; exit 2
    #[test]
    fn u5_double_start_busy() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("1,1 2x2"));
        let mut rec = make_recorder(&paths, spawner);

        assert!(rec.start_region(None, true).ok);
        let r2 = rec.start_region(None, true);
        assert!(!r2.ok);
        assert_eq!(r2.code, MachineCode::Busy);
        assert_eq!(r2.code.exit_code(), 2);
        assert_eq!(rec.spawner().wf_spawns().len(), 1);
        assert_eq!(rec.state(), State::Recording);
    }

    /// U6: stop from Recording → SIGINT to process group only; Idle
    #[test]
    fn u6_stop_sigint_process_group() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("1,1 2x2"));
        let mut rec = make_recorder(&paths, spawner);

        assert!(rec.start_region(None, true).ok);
        assert_eq!(rec.state(), State::Recording);

        let r = rec.stop();
        assert!(r.ok, "{r:?}");
        assert_eq!(rec.state(), State::Idle);

        let log = rec.spawner().signal_log();
        assert_eq!(log.len(), 1, "cooperative success must only send SIGINT");
        assert_eq!(log[0].signal, Signal::Interrupt);
        assert!(log[0].group, "SIGINT must target process group");
        assert!(!log.iter().any(|e| e.signal == Signal::Kill));
        assert!(!log.iter().any(|e| e.signal == Signal::Terminate));
    }

    /// U7: double stop → idempotent
    #[test]
    fn u7_double_stop_idempotent() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("1,1 2x2"));
        let mut rec = make_recorder(&paths, spawner);

        assert!(rec.start_region(None, true).ok);
        let r1 = rec.stop();
        assert!(r1.ok, "{r1:?}");
        let r2 = rec.stop();
        assert!(r2.ok, "{r2:?}");
        assert_eq!(rec.state(), State::Idle);
    }

    /// U8: stop/toggle during SelectingRegion → slurp killed; Idle
    #[test]
    fn u8_stop_during_selecting_region() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(FakeChildScript {
            wait_for_signal: true,
            exit: ExitStatus::Code(1),
            write_bytes_on_signal: None,
            ..Default::default()
        });
        let mut rec = make_recorder(&paths, spawner);

        rec.begin_region(None, true).unwrap();
        assert_eq!(rec.state(), State::SelectingRegion);

        let r = rec.stop();
        assert!(r.ok);
        assert_eq!(r.code, MachineCode::SlurpCancel);
        assert_eq!(rec.state(), State::Idle);
        assert!(rec.spawner().wf_spawns().is_empty());

        let log = rec.spawner().signal_log();
        assert!(
            log.iter()
                .any(|e| e.signal == Signal::Terminate || e.signal == Signal::Kill),
            "slurp must be signalled: {log:?}"
        );
    }

    #[test]
    fn u8_toggle_during_selecting_region() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(FakeChildScript {
            wait_for_signal: true,
            exit: ExitStatus::Code(1),
            write_bytes_on_signal: None,
            ..Default::default()
        });
        let mut rec = make_recorder(&paths, spawner);

        rec.begin_region(None, true).unwrap();
        let r = rec.toggle_region(None, true);
        assert_eq!(r.code, MachineCode::SlurpCancel);
        assert_eq!(rec.state(), State::Idle);
        assert!(rec.spawner().wf_spawns().is_empty());
        let log = rec.spawner().signal_log();
        assert!(
            log.iter()
                .any(|e| e.signal == Signal::Terminate || e.signal == Signal::Kill),
            "toggle cancel must signal slurp: {log:?}"
        );
    }

    /// U9: toggle while Recording → stop, no slurp
    #[test]
    fn u9_toggle_while_recording_stops_no_slurp() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("1,1 2x2"));
        let mut rec = make_recorder(&paths, spawner);

        rec.start_region(None, true);
        let slurp_before = rec.spawner().slurp_spawns();
        assert_eq!(rec.state(), State::Recording);

        let r = rec.toggle_region(None, true);
        assert!(r.ok, "{r:?}");
        assert_eq!(rec.state(), State::Idle);
        assert_eq!(rec.spawner().slurp_spawns(), slurp_before);
    }

    /// U10: spawn fail → Idle + error; no success hooks
    #[test]
    fn u10_spawn_fail() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("1,1 2x2"));
        spawner.fail_binary = Some("wf-recorder".into());
        let mut rec = make_recorder(&paths, spawner);

        let r = rec.start_region(None, true);
        assert!(!r.ok);
        assert_eq!(r.code, MachineCode::SpawnFailed);
        assert_eq!(rec.state(), State::Idle);
        assert!(rec.clipboard().texts.is_empty());
        assert!(rec.last_success_path().is_none());
    }

    /// U11: cooperative stop, non-zero exit, file size OK → Success; hooks run
    #[test]
    fn u11_cooperative_stop_nonzero_exit_file_ok() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("1,1 2x2"));
        spawner.recorder_exit = ExitStatus::Code(255);
        let mut rec = make_recorder(&paths, spawner);

        assert!(rec.start_region(None, true).ok);
        let out = rec.status().output_path.clone().unwrap();
        let r = rec.stop();
        assert!(r.ok, "{r:?}");
        assert_eq!(r.code, MachineCode::Ok);
        assert!(r.message.contains("exit 255") || r.message.contains("child exit 255"));
        let success = rec.last_success_path().unwrap().to_path_buf();
        assert_eq!(success, out);
        assert_eq!(rec.clipboard().texts, vec![success.display().to_string()]);
        assert!(
            rec.notifier()
                .calls
                .iter()
                .any(|(_, body)| body.contains(&success.display().to_string())),
            "notify body must contain path"
        );
        assert_eq!(rec.last_child_exit(), Some(ExitStatus::Code(255)));
    }

    /// U12: file missing after stop → Failure; no success clipboard
    #[test]
    fn u12_file_missing_failure() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("1,1 2x2"));
        spawner.auto_write_on_signal = false;
        let mut rec = make_recorder(&paths, spawner);

        assert!(rec.start_region(None, true).ok);
        let r = rec.stop();
        assert!(!r.ok, "{r:?}");
        assert_eq!(rec.state(), State::Idle);
        assert!(rec.clipboard().texts.is_empty());
        assert!(rec.last_success_path().is_none());
    }

    /// U12: zero-byte file after stop → Failure; no success clipboard
    #[test]
    fn u12_file_empty_failure() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("1,1 2x2"));
        spawner.auto_write_on_signal = true;
        spawner.auto_write_bytes = Some(Vec::new()); // zero-byte
        let mut rec = make_recorder(&paths, spawner);

        assert!(rec.start_region(None, true).ok);
        let r = rec.stop();
        assert!(!r.ok, "{r:?}");
        assert!(r.message.contains("empty") || r.message.contains("missing"));
        assert!(rec.clipboard().texts.is_empty());
        assert!(rec.last_success_path().is_none());
    }

    /// U13: stop timeout → ordered INT→TERM→KILL; Failure; can start again
    #[test]
    fn u13_stop_timeout_escalation() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("1,1 2x2"));
        spawner.recorder_ignore_sigint = true;
        spawner.recorder_ignore_sigterm = true;
        let mut rec = make_recorder(&paths, spawner);

        assert!(rec.start_region(None, true).ok);
        let r = rec.stop();
        assert!(!r.ok);
        assert_eq!(r.code, MachineCode::StopTimeout);
        assert!(r.message.contains("nuclear SIGKILL"));
        assert_eq!(rec.state(), State::Idle);

        let log = rec.spawner().signal_log();
        let signals: Vec<_> = log.iter().map(|e| e.signal).collect();
        assert_eq!(
            signals,
            vec![Signal::Interrupt, Signal::Terminate, Signal::Kill]
        );
        assert!(log.iter().all(|e| e.group));

        rec.spawner_mut().push_script(slurp_ok("3,3 4x4"));
        rec.spawner_mut().recorder_ignore_sigint = false;
        rec.spawner_mut().recorder_ignore_sigterm = false;
        let r2 = rec.start_region(None, true);
        assert!(r2.ok, "{r2:?}");
        assert_eq!(rec.state(), State::Recording);
    }

    /// Cooperative success after SIGINT ignored + SIGTERM reaps with file OK.
    #[test]
    fn term_cooperative_success_after_sigint_ignored() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("1,1 2x2"));
        spawner.recorder_ignore_sigint = true;
        spawner.recorder_ignore_sigterm = false;
        let mut rec = make_recorder(&paths, spawner);

        assert!(rec.start_region(None, true).ok);
        let r = rec.stop();
        assert!(r.ok, "{r:?}");
        assert_eq!(r.code, MachineCode::Ok);
        let signals: Vec<_> = rec
            .spawner()
            .signal_log()
            .iter()
            .map(|e| e.signal)
            .collect();
        assert_eq!(signals, vec![Signal::Interrupt, Signal::Terminate]);
        assert!(!signals.contains(&Signal::Kill));
        assert!(rec.last_success_path().is_some());
    }

    /// U14: path collision same second → unique filename
    #[test]
    fn u14_path_collision_unique_filename() {
        let paths = TempPaths::new();
        let clock = FakeClock::at_secs(TEST_UNIX);
        let stamp = format_timestamp(clock.now()).unwrap();
        let dir = paths.output_dir();
        fs::write(dir.join(format!("rec-{stamp}.mp4")), b"x").unwrap();
        fs::write(dir.join(format!("rec-{stamp}-1.mp4")), b"x").unwrap();

        let path = unique_output_path(&dir, &clock).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("rec-{stamp}-2.mp4")
        );
    }

    /// U15: notify/wl-copy fail → SuccessWithWarnings
    #[test]
    fn u15_notify_clipboard_fail_warnings() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("1,1 2x2"));
        let clock = FakeClock::at_secs(TEST_UNIX);
        let mut rec = Recorder::new(
            spawner,
            clock,
            FakeNotifier {
                fail: true,
                ..Default::default()
            },
            FakeClipboard {
                fail: true,
                ..Default::default()
            },
            paths.config(),
        );

        assert!(rec.start_region(None, true).ok);
        let r = rec.stop();
        assert!(r.ok, "{r:?}");
        assert!(r.is_success_with_warnings());
        assert!(r.warnings.iter().any(|w| w.contains("clipboard")));
        assert!(r.warnings.iter().any(|w| w.contains("notify")));
        assert!(rec.last_success_path().is_some());
    }

    /// U16: missing hard dep → clear error; exit semantics
    #[test]
    fn u16_missing_hard_dep_slurp() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.missing.push("slurp".into());
        let mut rec = make_recorder(&paths, spawner);

        let r = rec.start_region(None, true);
        assert!(!r.ok);
        assert_eq!(r.code, MachineCode::DepMissing);
        assert_eq!(r.code.exit_code(), 4);
        assert!(r.message.contains("slurp"));
        assert_eq!(rec.state(), State::Idle);
    }

    #[test]
    fn u16_missing_hard_dep_wf_recorder_fullscreen() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.missing.push("wf-recorder".into());
        let mut rec = make_recorder(&paths, spawner);

        let r = rec.start_fullscreen(None, true, None, None);
        assert!(!r.ok);
        assert_eq!(r.code, MachineCode::DepMissing);
        assert_eq!(r.code.exit_code(), 4);
        assert!(r.message.contains("wf-recorder"));
    }

    #[test]
    fn u16_missing_hard_dep_wf_recorder_region() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.missing.push("wf-recorder".into());
        let mut rec = make_recorder(&paths, spawner);

        let r = rec.start_region(None, true);
        assert!(!r.ok);
        assert_eq!(r.code, MachineCode::DepMissing);
        assert_eq!(r.code.exit_code(), 4);
        assert!(r.message.contains("wf-recorder"));
        assert_eq!(rec.spawner().slurp_spawns(), 0);
        assert_eq!(rec.state(), State::Idle);
    }

    /// U17: config defaults + overrides applied to paths/audio + stop hooks
    #[test]
    fn u17_config_defaults_and_overrides() {
        let paths = TempPaths::new();
        let mut cfg = paths.config();
        assert!(!cfg.audio_default);
        assert_eq!(cfg.stop_timeout_ms, 50);

        cfg.audio_default = true;
        cfg.output_dir = paths.root.join("CustomClips");
        cfg.copy_path = false;
        cfg.notify = false;
        cfg.notify_on_start_cli = false;

        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("1,1 2x2"));
        let clock = FakeClock::at_secs(TEST_UNIX);
        let mut rec = Recorder::new(
            spawner,
            clock,
            FakeNotifier::default(),
            FakeClipboard::default(),
            cfg,
        );

        let r = rec.start_region(None, true);
        assert!(r.ok, "{r:?}");
        // notify=false → no start notify even if notify_start true
        assert!(rec.notifier().calls.is_empty());
        let argv = &rec.spawner().wf_spawns()[0].argv;
        assert!(argv.iter().any(|a| a == "-a"));
        let fpos = argv.iter().position(|a| a == "-f").unwrap();
        assert!(argv[fpos + 1].contains("CustomClips"));
        assert!(Path::new(&argv[fpos + 1]).is_absolute());

        let r = rec.stop();
        assert!(r.ok, "{r:?}");
        assert!(
            rec.clipboard().texts.is_empty(),
            "copy_path=false must not clipboard"
        );
        // notify=false: only possible calls would be start (none) or stop success (none)
        assert!(
            rec.notifier().calls.is_empty(),
            "notify=false must not notify on success"
        );

        let home = PathBuf::from("/home/tester");
        let d = Config::with_home(&home, None);
        assert_eq!(d.output_dir, PathBuf::from("/home/tester/Videos"));
        assert!(!d.audio_default);
        assert!(d.copy_path);
        assert!(d.notify);
        assert!(d.notify_on_start_cli);
        assert_eq!(d.stop_timeout_ms, 5000);
        assert_eq!(d.stop_term_timeout_ms, 2000);

        let parsed = Config::parse_toml(
            "audio_default = true\noutput_dir = \"/tmp/out\"\n",
            Config::with_home(&home, None),
        )
        .unwrap();
        assert!(parsed.audio_default);
        assert_eq!(parsed.output_dir, PathBuf::from("/tmp/out"));
        assert_eq!(parsed.stop_timeout_ms, 5000);
    }

    #[test]
    fn notify_start_false_suppresses_start_toast() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let mut rec = make_recorder(&paths, spawner);
        rec.start_fullscreen(None, false, None, None);
        assert!(!rec
            .notifier()
            .calls
            .iter()
            .any(|(_, b)| b.contains("Recording started")));
    }

    #[test]
    fn poll_region_selection_with_ready_slurp() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(FakeChildScript {
            exit: ExitStatus::Code(0),
            stdout: "5,5 10x10".into(),
            already_dead: true,
            wait_for_signal: false,
            write_bytes_on_signal: None,
            ..Default::default()
        });
        let mut rec = make_recorder(&paths, spawner);
        rec.begin_region(None, true).unwrap();
        assert_eq!(rec.state(), State::SelectingRegion);
        let r = rec.poll_region_selection().expect("slurp ready");
        assert!(r.ok, "{r:?}");
        assert_eq!(rec.state(), State::Recording);
    }

    #[test]
    fn poll_unexpected_recorder_death_fails() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        // Survive spawn settle (one try_wait), then die on the next poll — no file.
        spawner.auto_write_on_signal = false;
        spawner.push_script(FakeChildScript {
            already_dead: false,
            wait_for_signal: true,
            exit: ExitStatus::Code(1),
            write_bytes_on_signal: None,
            auto_exit_after_try_waits: Some(1),
            stderr: "encoder exploded".into(),
            ..Default::default()
        });
        let mut rec = make_recorder(&paths, spawner);
        let r = rec.start_fullscreen(None, true, None, None);
        assert!(r.ok, "{r:?}");
        assert_eq!(rec.state(), State::Recording);
        let r = rec.poll().expect("dead child");
        assert!(!r.ok, "{r:?}");
        assert!(
            r.message.contains("missing or empty")
                || r.message.contains("unexpected")
                || r.message.contains("encoder"),
            "{r:?}"
        );
        assert_eq!(rec.state(), State::Idle);
        assert!(rec.clipboard().texts.is_empty());
    }

    #[test]
    fn stopping_pid_visible_in_status() {
        // Soft check: while Recording, pid is Some.
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("1,1 2x2"));
        let mut rec = make_recorder(&paths, spawner);
        rec.start_region(None, true);
        assert!(rec.status().pid.is_some());
        rec.stop();
        assert!(rec.status().pid.is_none());
    }

    #[test]
    fn build_argv_helpers() {
        let p = Path::new("/tmp/rec.mp4");
        let a = build_wf_recorder_argv(Some("0,0 10x10"), true, p, Some("HDMI-A-1"), None);
        assert_eq!(
            a,
            vec!["wf-recorder", "-g", "0,0 10x10", "-a", "-f", "/tmp/rec.mp4"]
        );
        let b = build_wf_recorder_argv(None, false, p, None, None);
        assert_eq!(b, vec!["wf-recorder", "-f", "/tmp/rec.mp4"]);
        let c = build_wf_recorder_argv(None, true, p, Some("DP-1"), None);
        assert_eq!(
            c,
            vec!["wf-recorder", "-o", "DP-1", "-a", "-f", "/tmp/rec.mp4"]
        );
        // Fullscreen with FPS: `-o NAME -r N [-a] -f path` (no `-g`).
        let d = build_wf_recorder_argv(None, true, p, Some("HDMI-A-1"), Some(144));
        assert_eq!(
            d,
            vec![
                "wf-recorder",
                "-o",
                "HDMI-A-1",
                "-r",
                "144",
                "-a",
                "-f",
                "/tmp/rec.mp4"
            ]
        );
        // Auto FPS: omit `-r`.
        let e = build_wf_recorder_argv(None, false, p, Some("eDP-1"), None);
        assert_eq!(e, vec!["wf-recorder", "-o", "eDP-1", "-f", "/tmp/rec.mp4"]);
        // Region ignores fps even if Some (no `-g`+`-r`).
        let f = build_wf_recorder_argv(Some("0,0 10x10"), false, p, None, Some(60));
        assert_eq!(
            f,
            vec!["wf-recorder", "-g", "0,0 10x10", "-f", "/tmp/rec.mp4"]
        );
        assert!(!f.iter().any(|a| a == "-r"));
        // Zero FPS never emits `-r`.
        let g = build_wf_recorder_argv(None, false, p, Some("eDP-1"), Some(0));
        assert!(!g.iter().any(|a| a == "-r"), "argv={g:?}");
    }

    #[test]
    fn fullscreen_respects_config_output_override() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let mut rec = make_recorder(&paths, spawner);
        rec.set_forced_output_inventory(Some(vec!["OTHER-OUT".into(), "TEST-OUT".into()]));
        let mut cfg = rec.config().clone();
        cfg.fullscreen_output = Some("OTHER-OUT".into());
        rec.set_config(cfg);
        assert!(rec.start_fullscreen(None, false, None, None).ok);
        let argv = rec.spawner().wf_spawns()[0].argv.clone();
        assert!(
            argv.windows(2).any(|w| w[0] == "-o" && w[1] == "OTHER-OUT"),
            "argv={argv:?}"
        );
        let _ = rec.stop();
    }

    #[test]
    fn fullscreen_cli_override_beats_config() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let mut rec = make_recorder(&paths, spawner);
        rec.set_forced_output_inventory(Some(vec!["DP-1".into(), "HDMI-A-1".into()]));
        let mut cfg = rec.config().clone();
        cfg.fullscreen_output = Some("DP-1".into());
        rec.set_config(cfg);
        assert!(rec.start_fullscreen(None, false, Some("HDMI-A-1"), None).ok);
        let argv = &rec.spawner().wf_spawns()[0].argv;
        assert!(
            argv.windows(2).any(|w| w[0] == "-o" && w[1] == "HDMI-A-1"),
            "argv={argv:?}"
        );
    }

    #[test]
    fn fullscreen_multi_head_without_pin_fails_no_spawn() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let mut rec = make_recorder(&paths, spawner);
        rec.set_forced_output_inventory(Some(vec!["DP-1".into(), "HDMI-A-1".into()]));
        let mut cfg = rec.config().clone();
        cfg.fullscreen_output = None;
        rec.set_config(cfg);
        let r = rec.start_fullscreen(None, false, None, None);
        assert!(!r.ok, "{r:?}");
        assert_eq!(r.code, MachineCode::Invalid);
        assert!(
            r.message.contains("DP-1") && r.message.contains("HDMI-A-1"),
            "{r:?}"
        );
        assert!(rec.spawner().wf_spawns().is_empty());
    }

    #[test]
    fn fullscreen_empty_inventory_fails_no_spawn() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let mut rec = make_recorder(&paths, spawner);
        rec.set_forced_output_inventory(Some(vec![]));
        let mut cfg = rec.config().clone();
        cfg.fullscreen_output = None;
        rec.set_config(cfg);
        let r = rec.start_fullscreen(None, false, None, None);
        assert!(!r.ok, "{r:?}");
        assert_eq!(r.code, MachineCode::Invalid);
        assert!(rec.spawner().wf_spawns().is_empty());
    }

    #[test]
    fn fullscreen_invalid_name_fails_lists_known() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let mut rec = make_recorder(&paths, spawner);
        rec.set_forced_output_inventory(Some(vec!["DP-1".into()]));
        let r = rec.start_fullscreen(None, false, Some("NOPE"), None);
        assert!(!r.ok, "{r:?}");
        assert_eq!(r.code, MachineCode::Invalid);
        assert!(r.message.contains("DP-1"), "{r:?}");
        assert!(rec.spawner().wf_spawns().is_empty());
    }

    #[test]
    fn fullscreen_single_head_no_pin_ok() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let mut rec = make_recorder(&paths, spawner);
        rec.set_forced_output_inventory(Some(vec!["eDP-1".into()]));
        let mut cfg = rec.config().clone();
        cfg.fullscreen_output = None;
        rec.set_config(cfg);
        let r = rec.start_fullscreen(None, true, None, None);
        assert!(r.ok, "{r:?}");
        assert!(r.message.contains("eDP-1"), "{r:?}");
        assert!(
            rec.notifier().calls.iter().any(|t| t.1.contains("eDP-1")),
            "notify should mention output: {:?}",
            rec.notifier().calls
        );
        let argv = &rec.spawner().wf_spawns()[0].argv;
        assert!(
            argv.windows(2).any(|w| w[0] == "-o" && w[1] == "eDP-1"),
            "argv={argv:?}"
        );
    }

    #[test]
    fn resolve_fullscreen_output_chain() {
        let inv = vec!["A".into(), "B".into()];
        assert_eq!(
            resolve_fullscreen_output(Some("B"), Some("A"), &inv).unwrap(),
            "B"
        );
        assert_eq!(
            resolve_fullscreen_output(None, Some("A"), &inv).unwrap(),
            "A"
        );
        // Multi-head without pin/CLI → Err listing known names.
        let multi_err = resolve_fullscreen_output(None, None, &inv).unwrap_err();
        assert!(
            multi_err.contains("A") && multi_err.contains("B"),
            "{multi_err}"
        );
        assert_eq!(
            resolve_fullscreen_output(None, None, &["Solo".into()]).unwrap(),
            "Solo"
        );
        assert!(resolve_fullscreen_output(None, None, &[]).is_err());
        // Explicit name not in inventory → Err (no silent fallback).
        let unknown = resolve_fullscreen_output(Some("Z"), None, &inv).unwrap_err();
        assert!(unknown.contains("Z") && unknown.contains("A"), "{unknown}");
        // Config pin not in inventory → Err.
        assert!(resolve_fullscreen_output(None, Some("Z"), &inv).is_err());
    }

    #[test]
    fn resolve_one_fps_priority() {
        assert_eq!(resolve_one_fps(Some(30), Some(144)), Some(30));
        assert_eq!(resolve_one_fps(None, Some(144)), Some(144));
        assert_eq!(resolve_one_fps(None, None), None);
        assert_eq!(resolve_one_fps(Some(60), None), Some(60));
        // 0 is Auto (never -r 0).
        assert_eq!(resolve_one_fps(Some(0), Some(144)), None);
        assert_eq!(resolve_one_fps(None, Some(0)), None);
    }

    #[test]
    fn region_spawn_ignores_config_one_fps() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(slurp_ok("10,20 300x200"));
        let mut rec = make_recorder(&paths, spawner);
        let mut cfg = rec.config().clone();
        cfg.one_fps = Some(144);
        rec.set_config(cfg);
        let r = rec.start_region(None, false);
        assert!(r.ok, "{r:?}");
        assert_eq!(rec.state(), State::Recording);
        let argv = &rec.spawner().wf_spawns()[0].argv;
        assert!(argv.contains(&"-g".into()), "argv={argv:?}");
        assert!(
            !argv.iter().any(|a| a == "-r"),
            "region must not inherit one_fps: argv={argv:?}"
        );
        let _ = rec.stop();
    }

    #[test]
    fn fullscreen_argv_uses_config_one_fps() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let mut rec = make_recorder(&paths, spawner);
        rec.set_forced_output_inventory(Some(vec!["eDP-1".into()]));
        let mut cfg = rec.config().clone();
        cfg.fullscreen_output = None;
        cfg.one_fps = Some(144);
        rec.set_config(cfg);
        assert!(rec.start_fullscreen(None, false, None, None).ok);
        let argv = &rec.spawner().wf_spawns()[0].argv;
        assert!(
            argv.windows(2).any(|w| w[0] == "-o" && w[1] == "eDP-1"),
            "argv={argv:?}"
        );
        assert!(
            argv.windows(2).any(|w| w[0] == "-r" && w[1] == "144"),
            "argv={argv:?}"
        );
        let _ = rec.stop();
    }

    #[test]
    fn fullscreen_cli_fps_beats_config_one_fps() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let mut rec = make_recorder(&paths, spawner);
        rec.set_forced_output_inventory(Some(vec!["eDP-1".into()]));
        let mut cfg = rec.config().clone();
        cfg.fullscreen_output = None;
        cfg.one_fps = Some(144);
        rec.set_config(cfg);
        assert!(rec.start_fullscreen(None, false, None, Some(60)).ok);
        let argv = &rec.spawner().wf_spawns()[0].argv;
        assert!(
            argv.windows(2).any(|w| w[0] == "-r" && w[1] == "60"),
            "argv={argv:?}"
        );
        assert!(
            !argv.windows(2).any(|w| w[0] == "-r" && w[1] == "144"),
            "argv={argv:?}"
        );
        let _ = rec.stop();
    }

    #[test]
    fn fullscreen_auto_fps_omits_r() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let mut rec = make_recorder(&paths, spawner);
        rec.set_forced_output_inventory(Some(vec!["eDP-1".into()]));
        let mut cfg = rec.config().clone();
        cfg.fullscreen_output = None;
        cfg.one_fps = None;
        rec.set_config(cfg);
        assert!(rec.start_fullscreen(None, false, None, None).ok);
        let argv = &rec.spawner().wf_spawns()[0].argv;
        assert!(!argv.iter().any(|a| a == "-r"), "argv={argv:?}");
        let _ = rec.stop();
    }

    #[test]
    fn spawn_early_exit_returns_start_failure() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(FakeChildScript {
            already_dead: true,
            wait_for_signal: false,
            exit: ExitStatus::Code(1),
            stderr: "Failed to select output, exiting".into(),
            write_bytes_on_signal: None,
            ..Default::default()
        });
        // Do not auto-override already_dead for wf-recorder — keep script as-is.
        spawner.auto_write_on_signal = false;
        let mut rec = make_recorder(&paths, spawner);
        let r = rec.start_fullscreen(None, false, None, None);
        assert!(!r.ok, "{r:?}");
        assert!(
            r.message.contains("Failed to select output") || r.message.contains("missing or empty"),
            "{r:?}"
        );
        assert_eq!(rec.state(), State::Idle);
    }

    #[test]
    fn civil_utc_matches_test_constant() {
        let (y, m, d, hh, mm, ss) = civil_utc(TEST_UNIX);
        assert_eq!((y, m, d, hh, mm, ss), (2024, 1, 15, 12, 37, 25));
    }
}
