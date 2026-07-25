//! Recorder state machine: region/fullscreen/both spawn, cooperative stop, success hooks.
//!
//! States: `Idle | SelectingRegion | Starting | Recording | Stopping` (SPEC v1).
//! Both uses dual ownership under Recording; Stopping reaps both then layout-true stitch.
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
use crate::sys::OutputInfo;

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

/// Capture mode for status / dual ownership (DUAL-MONITOR §7.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    Region,
    One,
    Both,
}

impl CaptureMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureMode::Region => "region",
            CaptureMode::One => "one",
            CaptureMode::Both => "both",
        }
    }
}

/// One compositor head used for layout-true Both compose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadGeom {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Both layout after primary sort (primary = min `(x,y)` lexicographic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BothLayout {
    pub primary: HeadGeom,
    pub secondary: HeadGeom,
}

/// Axis-aligned canvas + head offsets relative to canvas origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutCanvas {
    pub width: i32,
    pub height: i32,
    pub primary_ox: i32,
    pub primary_oy: i32,
    pub secondary_ox: i32,
    pub secondary_oy: i32,
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
    /// Resolved Wayland output for one-monitor fullscreen (`-o`), or Both label.
    pub capture_output: Option<String>,
    /// `"region" | "one" | "both"` while Starting/Recording/Stopping.
    pub capture_mode: Option<String>,
}

// ---------------------------------------------------------------------------
// Recorder
// ---------------------------------------------------------------------------

/// Owns region/one `wf-recorder` or dual Both children + optional stitch, and drives the SPEC state machine.
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
    /// Primary (or sole) `wf-recorder` child.
    recorder_child: Option<S::Child>,
    /// Secondary `wf-recorder` when [`CaptureMode::Both`].
    recorder_child_b: Option<S::Child>,
    /// In-flight `ffmpeg` stitch (Both stop); reaped on Drop without success hooks.
    ffmpeg_child: Option<S::Child>,

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
    /// Active fullscreen / one-monitor / Both output label (fixed at start).
    capture_output: Option<String>,
    /// Active capture mode while Starting/Recording/Stopping.
    capture_mode: Option<CaptureMode>,
    /// Both primary temp (`.record-ui-both-*-A.mkv`).
    both_temp_a: Option<PathBuf>,
    /// Both secondary temp (`.record-ui-both-*-B.mkv`).
    both_temp_b: Option<PathBuf>,
    /// Layout fixed at Both start for layout-true stitch.
    both_layout: Option<BothLayout>,
    /// When set, used instead of probing hyprctl/wf-recorder (tests / inject) — names only.
    forced_output_inventory: Option<Vec<String>>,
    /// When set, used for Both layout (tests / inject) — rich geometry.
    forced_layout_inventory: Option<Vec<OutputInfo>>,
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
            recorder_child_b: None,
            ffmpeg_child: None,
            output_path: None,
            started_at: None,
            last_error: None,
            last_success_path: None,
            last_child_exit: None,
            stop_sent_signals: Vec::new(),
            stopping_pid: None,
            capture_output: None,
            capture_mode: None,
            both_temp_a: None,
            both_temp_b: None,
            both_layout: None,
            forced_output_inventory: None,
            forced_layout_inventory: None,
        }
    }

    /// Inject output inventory for tests (avoids calling hyprctl).
    pub fn set_forced_output_inventory(&mut self, inventory: Option<Vec<String>>) {
        self.forced_output_inventory = inventory;
    }

    /// Inject rich layout inventory for Both tests (avoids calling hyprctl).
    pub fn set_forced_layout_inventory(&mut self, inventory: Option<Vec<OutputInfo>>) {
        self.forced_layout_inventory = inventory;
    }

    fn output_inventory(&self) -> Vec<String> {
        if let Some(ref forced) = self.forced_output_inventory {
            return forced.clone();
        }
        if let Some(ref rich) = self.forced_layout_inventory {
            return rich.iter().map(|o| o.name.clone()).collect();
        }
        crate::sys::list_output_names()
    }

    fn layout_inventory(&self) -> Vec<OutputInfo> {
        if let Some(ref rich) = self.forced_layout_inventory {
            return rich.clone();
        }
        // Names-only forced inventory has no geometry — Both start must fail positions.
        if let Some(ref names) = self.forced_output_inventory {
            return names.iter().map(OutputInfo::names_only).collect();
        }
        crate::sys::list_output_inventory()
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
        let active = matches!(
            self.state,
            State::Starting | State::Recording | State::Stopping
        );
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
            capture_output: if active {
                self.capture_output.clone()
            } else {
                None
            },
            capture_mode: if active {
                self.capture_mode.map(|m| m.as_str().to_string())
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
        self.capture_mode = Some(CaptureMode::One);
        self.capture_output = Some(name.clone());
        self.clear_both_session_fields();
        self.spawn_recorder(None, audio, Some(name), fps)
    }

    /// Both-monitors start: dual `wf-recorder` @ 60 fps + `-D`, layout-true stitch on stop.
    ///
    /// Preconditions (DUAL-MONITOR §7.1–7.2): inventory length exactly 2 with hyprctl
    /// positions, `ffmpeg` + `wf-recorder` present. Audio (`-a`) only on primary.
    pub fn start_both(&mut self, audio: Option<bool>, notify_start: bool) -> CommandResult {
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
        if !self.spawner.command_exists("ffmpeg") {
            let msg = "missing hard dependency: ffmpeg (required for Both compose)".to_string();
            self.last_error = Some(msg.clone());
            return CommandResult::err(MachineCode::DepMissing, msg);
        }

        let inventory = self.layout_inventory();
        let layout = match both_layout_from_inventory(&inventory) {
            Ok(l) => l,
            Err(e) => {
                self.last_error = Some(e.clone());
                return CommandResult::err(MachineCode::Invalid, e);
            }
        };

        let audio = audio.unwrap_or(self.config.audio_default);
        self.notify_start = notify_start;
        self.last_error = None;
        self.audio = audio;
        self.capture_mode = Some(CaptureMode::Both);
        self.capture_output = Some(format!("{}+{}", layout.primary.name, layout.secondary.name));
        self.both_layout = Some(layout.clone());
        self.spawn_both_recorders(layout, audio)
    }

    /// Stop if selecting or recording; idle no-op success; Stopping is idempotent.
    pub fn stop(&mut self) -> CommandResult {
        match self.state {
            State::Idle => CommandResult::ok_msg("already idle"),
            State::SelectingRegion => self.cancel_slurp(),
            State::Starting => self.abort_starting(),
            State::Recording => self.stop_recording(),
            State::Stopping => {
                if self.recorder_child.is_some() || self.recorder_child_b.is_some() {
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
        self.capture_mode = Some(CaptureMode::Region);
        self.capture_output = None;
        self.clear_both_session_fields();
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
            self.reset_to_idle_clear_session();
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
                self.reset_to_idle_clear_session();
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
                self.reset_to_idle_clear_session();
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
                self.reset_to_idle_clear_session();
                let msg = format!("failed to spawn wf-recorder: {e}");
                self.last_error = Some(msg.clone());
                CommandResult::err(MachineCode::SpawnFailed, msg)
            }
        }
    }

    /// Dual capture: primary first (optional `-a`), then secondary; both before Recording.
    fn spawn_both_recorders(&mut self, layout: BothLayout, audio: bool) -> CommandResult {
        self.state = State::Starting;

        if let Err(e) = ensure_output_dir(&self.config.output_dir) {
            self.reset_to_idle_clear_session();
            let msg = format!("cannot create output_dir: {e}");
            self.last_error = Some(msg.clone());
            return CommandResult::err(MachineCode::IoError, msg);
        }

        let final_path = match unique_output_path(&self.config.output_dir, &self.clock) {
            Ok(p) => absolutize_path(
                &p,
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            ),
            Err(e) => {
                self.reset_to_idle_clear_session();
                let msg = format!("cannot allocate output path: {e}");
                self.last_error = Some(msg.clone());
                return CommandResult::err(MachineCode::IoError, msg);
            }
        };

        let session_id = both_session_id(&self.clock);
        let (temp_a, temp_b) = both_temp_paths(&self.config.output_dir, &session_id);
        let temp_a = absolutize_path(
            &temp_a,
            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        );
        let temp_b = absolutize_path(
            &temp_b,
            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        );

        let argv_a = build_both_wf_recorder_argv(&layout.primary.name, audio, &temp_a);
        let argv_b = build_both_wf_recorder_argv(&layout.secondary.name, false, &temp_b);
        let opts = SpawnOpts {
            new_process_group: true,
        };

        let child_a = match self.spawner.spawn(&argv_a, opts.clone()) {
            Ok(c) => c,
            Err(e) => {
                self.reset_to_idle_clear_session();
                let msg = format!("failed to spawn primary wf-recorder: {e}");
                self.last_error = Some(msg.clone());
                return CommandResult::err(MachineCode::SpawnFailed, msg);
            }
        };

        let child_b = match self.spawner.spawn(&argv_b, opts) {
            Ok(c) => c,
            Err(e) => {
                // Partial spawn: reap first → best-effort temp cleanup → Idle (never Recording).
                let mut orphan = child_a;
                force_reap_recorder(&mut orphan, &self.clock);
                let _ = fs::remove_file(&temp_a);
                let _ = fs::remove_file(&temp_b);
                self.reset_to_idle_clear_session();
                let msg = format!(
                    "failed to spawn secondary wf-recorder: {e}; reaped primary; \
                     cleaned temp {}",
                    temp_a.display()
                );
                self.last_error = Some(msg.clone());
                return CommandResult::err(MachineCode::SpawnFailed, msg);
            }
        };

        self.recorder_child = Some(child_a);
        self.recorder_child_b = Some(child_b);
        self.both_temp_a = Some(temp_a);
        self.both_temp_b = Some(temp_b);
        self.both_layout = Some(layout);
        self.output_path = Some(final_path);
        self.started_at = Some(self.clock.now());
        self.state = State::Recording;

        // Early settle (~200ms): both still alive or fail start.
        // Death here uses the same peer-death / IoError path as mid-record (One parity:
        // unexpected exit is not reclassified as spawn_failed after children were spawned).
        self.clock.sleep(Duration::from_millis(200));
        if let Some(early) = self.poll_recorder_exited() {
            return early;
        }

        self.maybe_notify_start();
        let msg = if let Some(ref o) = self.capture_output {
            format!("Recording both ({o})")
        } else {
            "recording both started".to_string()
        };
        CommandResult::ok_msg(msg)
    }

    fn clear_both_session_fields(&mut self) {
        self.both_temp_a = None;
        self.both_temp_b = None;
        self.both_layout = None;
        self.recorder_child_b = None;
        // ffmpeg_child handled separately on Drop / stitch
    }

    fn reset_to_idle_clear_session(&mut self) {
        self.state = State::Idle;
        self.recorder_child = None;
        self.recorder_child_b = None;
        self.output_path = None;
        self.started_at = None;
        self.stopping_pid = None;
        self.capture_output = None;
        self.capture_mode = None;
        self.both_temp_a = None;
        self.both_temp_b = None;
        self.both_layout = None;
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
        if self.recorder_child.is_some() || self.recorder_child_b.is_some() {
            // Defensive: child present while Starting → cooperative stop path.
            return self.stop_recording();
        }
        self.reset_to_idle_clear_session();
        CommandResult::ok_msg("aborted start (no child)")
    }

    fn stop_recording(&mut self) -> CommandResult {
        if self.capture_mode == Some(CaptureMode::Both) || self.recorder_child_b.is_some() {
            return self.stop_both_recording();
        }
        self.stop_single_recording()
    }

    fn stop_single_recording(&mut self) -> CommandResult {
        self.state = State::Stopping;
        self.stop_sent_signals.clear();

        let mut child = match self.recorder_child.take() {
            Some(c) => c,
            None => {
                self.reset_to_idle_clear_session();
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

    /// Dual stop: SIGINT both PGs → joint wait → escalate both → stitch layout-true.
    ///
    /// Stop does not return until stitch completes or fails (product choice A).
    /// IPC `quit`/`stop` is cooperative finalize (may stitch); only `Drop` unclean skips stitch.
    fn stop_both_recording(&mut self) -> CommandResult {
        self.state = State::Stopping;
        self.stop_sent_signals.clear();

        let mut child_a = match self.recorder_child.take() {
            Some(c) => c,
            None => {
                // Defensive: still try to reap B if present; retain temps in last_error.
                if let Some(mut b) = self.recorder_child_b.take() {
                    force_reap_recorder(&mut b, &self.clock);
                }
                let msg = format!(
                    "primary recorder child missing; no stitch; temps={} , {}",
                    self.both_temp_a
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(none)".into()),
                    self.both_temp_b
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(none)".into()),
                );
                self.fail_both_retain_temps(msg.clone());
                return CommandResult::err(MachineCode::IoError, msg);
            }
        };
        let mut child_b = match self.recorder_child_b.take() {
            Some(c) => c,
            None => {
                force_reap_recorder(&mut child_a, &self.clock);
                let msg = format!(
                    "secondary recorder child missing; no stitch; temps={} , {}",
                    self.both_temp_a
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(none)".into()),
                    self.both_temp_b
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(none)".into()),
                );
                self.fail_both_retain_temps(msg.clone());
                return CommandResult::err(MachineCode::IoError, msg);
            }
        };
        self.stopping_pid = Some(child_a.id());

        // Peer death race: if either already exited before cooperative stop, no stitch (§7.6).
        let a_dead = matches!(child_a.try_wait(), Ok(Some(_)));
        let b_dead = matches!(child_b.try_wait(), Ok(Some(_)));
        if a_dead || b_dead {
            if !a_dead {
                force_reap_recorder(&mut child_a, &self.clock);
            }
            if !b_dead {
                force_reap_recorder(&mut child_b, &self.clock);
            }
            let msg = self
                .format_both_peer_death_error(&self.both_temp_a.clone(), &self.both_temp_b.clone());
            self.fail_both_no_stitch(msg.clone());
            return CommandResult::err(MachineCode::IoError, msg);
        }

        // 1) SIGINT both process groups (parallel signal).
        let _ = child_a.signal_group(Signal::Interrupt);
        let _ = child_b.signal_group(Signal::Interrupt);
        self.stop_sent_signals.push(Signal::Interrupt);

        let int_timeout = self.config.stop_timeout();
        let (mut a_done, mut b_done) =
            joint_wait_both(&mut child_a, &mut child_b, int_timeout, &self.clock);
        if a_done && b_done {
            let _sa = child_a.take_stderr_tail();
            let _sb = child_b.take_stderr_tail();
            return self.finish_both_after_reap();
        }

        // 2) SIGTERM only groups still alive.
        if !a_done {
            let _ = child_a.signal_group(Signal::Terminate);
        }
        if !b_done {
            let _ = child_b.signal_group(Signal::Terminate);
        }
        self.stop_sent_signals.push(Signal::Terminate);

        let term_timeout = self.config.stop_term_timeout();
        let (a2, b2) = joint_wait_both(&mut child_a, &mut child_b, term_timeout, &self.clock);
        a_done = a_done || a2;
        b_done = b_done || b2;
        if a_done && b_done {
            let _sa = child_a.take_stderr_tail();
            let _sb = child_b.take_stderr_tail();
            return self.finish_both_after_reap();
        }

        // 3) Nuclear only survivors; always reap; retain temps; no stitch claim.
        if !a_done {
            let _ = child_a.signal_group(Signal::Kill);
        }
        if !b_done {
            let _ = child_b.signal_group(Signal::Kill);
        }
        self.stop_sent_signals.push(Signal::Kill);
        force_reap_after_kill(&mut child_a, &self.clock);
        force_reap_after_kill(&mut child_b, &self.clock);
        let stderr = format!(
            "{}{}",
            child_a.take_stderr_tail(),
            child_b.take_stderr_tail()
        );
        self.finish_both_stop_timeout(stderr)
    }

    /// After both children reaped cooperatively: check temps → ffmpeg layout-true → final.
    ///
    /// Keeps `stopping_pid` / `started_at` until Idle so status stays useful mid-stitch.
    fn finish_both_after_reap(&mut self) -> CommandResult {
        let temp_a = self.both_temp_a.clone();
        let temp_b = self.both_temp_b.clone();
        let layout = self.both_layout.clone();
        let final_path = self.output_path.clone();

        self.recorder_child = None;
        self.recorder_child_b = None;
        // Keep started_at + stopping_pid through stitch (status while Stopping).

        let (Some(temp_a), Some(temp_b), Some(layout), Some(final_path)) =
            (temp_a, temp_b, layout, final_path)
        else {
            let msg = "both session missing temps/layout/path after stop".to_string();
            self.fail_both_no_stitch(msg.clone());
            return CommandResult::err(MachineCode::IoError, msg);
        };

        let a_ok = file_nonempty(&temp_a).unwrap_or(false);
        let b_ok = file_nonempty(&temp_b).unwrap_or(false);
        if !a_ok || !b_ok {
            let msg = format!(
                "both recording failed: temp missing or empty (A={} ok={a_ok}, B={} ok={b_ok})",
                temp_a.display(),
                temp_b.display()
            );
            // Retain temps for debug when present-but-empty is not the case; keep whatever exists.
            self.fail_both_retain_temps(msg.clone());
            return CommandResult::err(MachineCode::IoError, msg);
        }

        match self.run_layout_true_stitch(&temp_a, &temp_b, &layout, &final_path) {
            Ok(()) => {
                // Success only if final size > 0 and ffmpeg exit 0 (checked inside).
                let _ = fs::remove_file(&temp_a);
                let _ = fs::remove_file(&temp_b);
                self.both_temp_a = None;
                self.both_temp_b = None;
                self.both_layout = None;
                self.capture_output = None;
                self.capture_mode = None;
                self.started_at = None;
                self.stopping_pid = None;
                self.state = State::Idle;

                // Success hooks on FINAL path only.
                self.last_success_path = Some(final_path.clone());
                self.output_path = None;
                self.last_error = None;

                let mut warnings = Vec::new();
                let path_str = final_path.display().to_string();

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

                let message = format!("saved {path_str}");
                if warnings.is_empty() {
                    CommandResult::ok_msg(message)
                } else {
                    CommandResult::with_warnings(message, warnings)
                }
            }
            Err(msg) => {
                // Stitch fail: last_error includes temp paths; RETAIN temps; no success hooks.
                let full = format!(
                    "{msg}; temps retained: {} , {}",
                    temp_a.display(),
                    temp_b.display()
                );
                self.fail_both_retain_temps(full.clone());
                CommandResult::err(MachineCode::IoError, full)
            }
        }
    }

    fn run_layout_true_stitch(
        &mut self,
        temp_a: &Path,
        temp_b: &Path,
        layout: &BothLayout,
        final_path: &Path,
    ) -> Result<(), String> {
        if !self.spawner.command_exists("ffmpeg") {
            return Err("ffmpeg missing at stitch time".into());
        }
        // Blocking compose: stop/quit RPC waits here until ffmpeg exits (documented product A).
        let argv = build_layout_true_ffmpeg_argv(temp_a, temp_b, layout, final_path, self.audio);
        let opts = SpawnOpts {
            new_process_group: true,
        };
        let child = self
            .spawner
            .spawn(&argv, opts)
            .map_err(|e| format!("failed to spawn ffmpeg: {e}"))?;
        // Hold in field so Drop reaps if wait is interrupted.
        self.ffmpeg_child = Some(child);

        let status = match self.ffmpeg_child.as_mut() {
            Some(child) => child.wait(),
            None => return Err("ffmpeg child lost before wait".into()),
        };
        let status = match status {
            Ok(s) => s,
            Err(e) => {
                if let Some(mut child) = self.ffmpeg_child.take() {
                    force_reap_recorder(&mut child, &self.clock);
                    let stderr = child.take_stderr_tail();
                    let mut msg = format!("ffmpeg wait failed: {e}");
                    append_stderr(&mut msg, &stderr);
                    return Err(msg);
                }
                return Err(format!("ffmpeg wait failed: {e}"));
            }
        };
        let stderr = self
            .ffmpeg_child
            .as_mut()
            .map(|c| c.take_stderr_tail())
            .unwrap_or_default();
        self.ffmpeg_child = None;
        self.last_child_exit = Some(status);

        if !status.success() {
            let mut msg = format!("ffmpeg stitch failed ({status})");
            append_stderr(&mut msg, &stderr);
            return Err(msg);
        }

        match file_nonempty(final_path) {
            Ok(true) => Ok(()),
            Ok(false) => Err(format!(
                "ffmpeg exit 0 but final missing or empty ({})",
                final_path.display()
            )),
            Err(e) => Err(format!("cannot stat final file: {e}")),
        }
    }

    fn fail_both_no_stitch(&mut self, msg: String) {
        // Prefer retain temps on unclean path; do not delete.
        self.output_path = None;
        self.started_at = None;
        self.recorder_child = None;
        self.recorder_child_b = None;
        self.stopping_pid = None;
        self.capture_output = None;
        self.capture_mode = None;
        // both_temp_* retained intentionally for debug.
        self.both_layout = None;
        self.state = State::Idle;
        self.last_error = Some(msg.clone());
        if self.config.notify {
            let _ = self.notifier.notify("record-ui", &msg);
        }
    }

    fn fail_both_retain_temps(&mut self, msg: String) {
        self.fail_both_no_stitch(msg);
    }

    fn format_both_peer_death_error(
        &self,
        temp_a: &Option<PathBuf>,
        temp_b: &Option<PathBuf>,
    ) -> String {
        format!(
            "both recording failed: peer death while recording; no stitch; temps={} , {}",
            temp_a
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(none)".into()),
            temp_b
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(none)".into()),
        )
    }

    fn finish_both_stop_timeout(&mut self, stderr: String) -> CommandResult {
        let temps = format!(
            "temps={} , {}",
            self.both_temp_a
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(none)".into()),
            self.both_temp_b
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(none)".into()),
        );
        let mut msg = format!(
            "both stop timed out after SIGINT/SIGTERM; nuclear SIGKILL used; no stitch; {temps}"
        );
        append_stderr(&mut msg, &stderr);
        self.fail_both_retain_temps(msg.clone());
        CommandResult::err(MachineCode::StopTimeout, msg)
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
        self.recorder_child_b = None;
        self.stopping_pid = None;
        self.capture_output = None;
        self.capture_mode = None;
        self.both_temp_a = None;
        self.both_temp_b = None;
        self.both_layout = None;
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
        self.recorder_child_b = None;
        self.stopping_pid = None;
        self.capture_output = None;
        self.capture_mode = None;
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
        if self.capture_mode == Some(CaptureMode::Both) || self.recorder_child_b.is_some() {
            return self.poll_both_exited();
        }
        self.poll_single_exited()
    }

    fn poll_single_exited(&mut self) -> Option<CommandResult> {
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
                    self.capture_mode = None;
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
                self.capture_mode = None;
                let msg = "recorder child missing after try_wait".to_string();
                self.last_error = Some(msg.clone());
                return Some(CommandResult::err(MachineCode::IoError, msg));
            }
        };
        let stderr = child.take_stderr_tail();
        // Unexpected death is not a cooperative stop context.
        Some(self.finish_after_reap(status, false, stderr))
    }

    /// Peer death while Recording Both: force-reap peer; NO stitch; NO success hooks.
    fn poll_both_exited(&mut self) -> Option<CommandResult> {
        let a_dead = match self.recorder_child.as_mut() {
            Some(c) => match c.try_wait() {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => {
                    return Some(
                        self.force_fail_both_poll(format!("primary recorder poll failed: {e}")),
                    );
                }
            },
            None => true,
        };
        let b_dead = match self.recorder_child_b.as_mut() {
            Some(c) => match c.try_wait() {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => {
                    return Some(
                        self.force_fail_both_poll(format!("secondary recorder poll failed: {e}")),
                    );
                }
            },
            None => true,
        };

        if !a_dead && !b_dead {
            return None;
        }

        // One or both dead unexpectedly: force-reap survivors; no stitch.
        if let Some(mut a) = self.recorder_child.take() {
            if a_dead {
                let _ = a.try_wait();
            } else {
                force_reap_recorder(&mut a, &self.clock);
            }
        }
        if let Some(mut b) = self.recorder_child_b.take() {
            if b_dead {
                let _ = b.try_wait();
            } else {
                force_reap_recorder(&mut b, &self.clock);
            }
        }

        let msg =
            self.format_both_peer_death_error(&self.both_temp_a.clone(), &self.both_temp_b.clone());
        self.fail_both_no_stitch(msg.clone());
        Some(CommandResult::err(MachineCode::IoError, msg))
    }

    fn force_fail_both_poll(&mut self, msg: String) -> CommandResult {
        if let Some(mut a) = self.recorder_child.take() {
            force_reap_recorder(&mut a, &self.clock);
        }
        if let Some(mut b) = self.recorder_child_b.take() {
            force_reap_recorder(&mut b, &self.clock);
        }
        self.fail_both_no_stitch(msg.clone());
        CommandResult::err(MachineCode::IoError, msg)
    }
}

/// Shared wall-clock joint wait after signals already delivered (SPEC §7.4).
///
/// Returns `(a_exited, b_exited)`. Deadline is wall-clock (`Instant`), so two hanging
/// children escalate after ~one `timeout`, not serial 2×. Only polls / short-slices
/// until both exit or deadline; does not re-signal.
fn joint_wait_both<H: ChildHandle>(
    a: &mut H,
    b: &mut H,
    timeout: Duration,
    clock: &dyn Clock,
) -> (bool, bool) {
    let deadline = std::time::Instant::now() + timeout;
    let slice = Duration::from_millis(20).min(timeout.max(Duration::from_millis(1)));
    let mut a_done = matches!(a.try_wait(), Ok(Some(_)));
    let mut b_done = matches!(b.try_wait(), Ok(Some(_)));

    while !(a_done && b_done) {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(now);
        let wait = slice.min(remaining);

        // Prefer bounded wait on a still-live child so production does not busy-spin.
        if !a_done {
            if matches!(a.wait_timeout(wait), Ok(Some(_))) {
                a_done = true;
            }
        } else if !b_done {
            if matches!(b.wait_timeout(wait), Ok(Some(_))) {
                b_done = true;
            }
        } else {
            clock.sleep(wait);
        }

        if !a_done {
            a_done = matches!(a.try_wait(), Ok(Some(_)));
        }
        if !b_done {
            b_done = matches!(b.try_wait(), Ok(Some(_)));
        }
    }

    if !a_done {
        a_done = matches!(a.try_wait(), Ok(Some(_)));
    }
    if !b_done {
        b_done = matches!(b.try_wait(), Ok(Some(_)));
    }
    (a_done, b_done)
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
        if let Some(mut child) = self.recorder_child_b.take() {
            force_reap_recorder(&mut child, &self.clock);
        }
        // Skip stitch on unclean Drop; kill ffmpeg if mid-stitch; prefer retain temps.
        if let Some(mut child) = self.ffmpeg_child.take() {
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

// ---------------------------------------------------------------------------
// Both helpers (layout-true; pure + unit-tested)
// ---------------------------------------------------------------------------

/// Both FPS is fixed at 60 (DUAL-MONITOR §7.3); not configurable.
pub const BOTH_FPS: u32 = 60;

/// Build layout from inventory: exactly 2 heads with geometry; primary = min (x,y).
pub fn both_layout_from_inventory(inventory: &[OutputInfo]) -> Result<BothLayout, String> {
    if inventory.len() != 2 {
        let names: Vec<&str> = inventory.iter().map(|o| o.name.as_str()).collect();
        return Err(format!(
            "Both requires exactly 2 monitors, found {}: [{}]",
            inventory.len(),
            names.join(", ")
        ));
    }
    let missing: Vec<&str> = inventory
        .iter()
        .filter(|o| !o.has_geometry())
        .map(|o| o.name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "Both requires hyprctl layout positions (x,y,w,h); names-only inventory cannot start Both \
             (missing geometry: {}). Use Hyprland / hyprctl monitors -j.",
            missing.join(", ")
        ));
    }
    let h0 = head_from_output(&inventory[0])?;
    let h1 = head_from_output(&inventory[1])?;
    Ok(order_primary_secondary(h0, h1))
}

fn head_from_output(o: &OutputInfo) -> Result<HeadGeom, String> {
    match (o.x, o.y, o.width, o.height) {
        (Some(x), Some(y), Some(w), Some(h)) if w > 0 && h > 0 => Ok(HeadGeom {
            name: o.name.clone(),
            x,
            y,
            width: w,
            height: h,
        }),
        _ => Err(format!(
            "output {} missing positive geometry for Both",
            o.name
        )),
    }
}

/// Primary = minimum `(x, y)` lexicographic (left/top-most).
pub fn order_primary_secondary(a: HeadGeom, b: HeadGeom) -> BothLayout {
    if (a.x, a.y) <= (b.x, b.y) {
        BothLayout {
            primary: a,
            secondary: b,
        }
    } else {
        BothLayout {
            primary: b,
            secondary: a,
        }
    }
}

/// Canvas = AABB of both heads; offsets relative to canvas origin (black voids).
///
/// Width/height are forced **even** (ceil) so libx264/yuv420p accepts the canvas;
/// extra pixel of black padding is on the right/bottom edge only (overlays unchanged).
pub fn layout_canvas(layout: &BothLayout) -> LayoutCanvas {
    let p = &layout.primary;
    let s = &layout.secondary;
    let min_x = p.x.min(s.x);
    let min_y = p.y.min(s.y);
    let max_x = (p.x + p.width).max(s.x + s.width);
    let max_y = (p.y + p.height).max(s.y + s.height);
    let mut width = max_x - min_x;
    let mut height = max_y - min_y;
    if width % 2 != 0 {
        width += 1;
    }
    if height % 2 != 0 {
        height += 1;
    }
    LayoutCanvas {
        width,
        height,
        primary_ox: p.x - min_x,
        primary_oy: p.y - min_y,
        secondary_ox: s.x - min_x,
        secondary_oy: s.y - min_y,
    }
}

/// Layout-true filter_complex graph (overlay on black canvas). **Never** hstack.
///
/// Duration policy: `-shortest` on the encode argv (see [`build_layout_true_ffmpeg_argv`]).
/// Prefer pad-shorter-with-black when probe is available later; this slice uses `-shortest`
/// for deterministic behavior without ffprobe.
///
/// Color source is pinned to [`BOTH_FPS`] so the black canvas timeline matches dual capture.
pub fn build_layout_true_filter(layout: &BothLayout) -> String {
    let c = layout_canvas(layout);
    let p = &layout.primary;
    let s = &layout.secondary;
    format!(
        "[0:v]setpts=PTS-STARTPTS,scale={pw}:{ph}:force_original_aspect_ratio=decrease,pad={pw}:{ph}:(ow-iw)/2:(oh-ih)/2,setsar=1[v0];\
[1:v]setpts=PTS-STARTPTS,scale={sw}:{sh}:force_original_aspect_ratio=decrease,pad={sw}:{sh}:(ow-iw)/2:(oh-ih)/2,setsar=1[v1];\
color=c=black:s={cw}x{ch}:r={fps}:d=86400,setsar=1[base];\
[base][v0]overlay={pox}:{poy}:eof_action=pass:repeatlast=0[tmp];\
[tmp][v1]overlay={sox}:{soy}:eof_action=pass:repeatlast=0,format=yuv420p[vout]",
        pw = p.width,
        ph = p.height,
        sw = s.width,
        sh = s.height,
        cw = c.width,
        ch = c.height,
        fps = BOTH_FPS,
        pox = c.primary_ox,
        poy = c.primary_oy,
        sox = c.secondary_ox,
        soy = c.secondary_oy,
    )
}

/// `ffmpeg` argv for layout-true Both stitch.
///
/// When `map_audio` is true, maps primary audio only (`0:a?`) and encodes AAC.
/// When false, omits audio map/codec (no-audio sessions).
///
/// Duration policy: **`-shortest`** (ends when the shorter demuxed input ends).
/// Forbidden: same-height `hstack` path.
pub fn build_layout_true_ffmpeg_argv(
    temp_a: &Path,
    temp_b: &Path,
    layout: &BothLayout,
    final_path: &Path,
    map_audio: bool,
) -> Vec<String> {
    let filter = build_layout_true_filter(layout);
    let mut argv = vec![
        "ffmpeg".into(),
        "-y".into(),
        "-i".into(),
        temp_a.display().to_string(),
        "-i".into(),
        temp_b.display().to_string(),
        "-filter_complex".into(),
        filter,
        "-map".into(),
        "[vout]".into(),
    ];
    if map_audio {
        argv.push("-map".into());
        argv.push("0:a?".into());
    }
    argv.push("-c:v".into());
    argv.push("libx264".into());
    argv.push("-preset".into());
    argv.push("veryfast".into());
    argv.push("-crf".into());
    argv.push("18".into());
    if map_audio {
        argv.push("-c:a".into());
        argv.push("aac".into());
    }
    argv.push("-r".into());
    argv.push(BOTH_FPS.to_string());
    argv.push("-shortest".into());
    argv.push(final_path.display().to_string());
    argv
}

/// Both child argv: `wf-recorder -o NAME -r 60 -D [-a] -f temp` (never shell).
pub fn build_both_wf_recorder_argv(output: &str, audio: bool, path: &Path) -> Vec<String> {
    let mut argv = vec![
        "wf-recorder".into(),
        "-o".into(),
        output.to_string(),
        "-r".into(),
        BOTH_FPS.to_string(),
        "-D".into(),
    ];
    if audio {
        argv.push("-a".into());
    }
    argv.push("-f".into());
    argv.push(path.display().to_string());
    argv
}

/// Unique Both session id for temp basenames (timestamp-based).
fn both_session_id(clock: &dyn Clock) -> String {
    match format_timestamp(clock.now()) {
        Ok(stamp) => stamp,
        Err(_) => format!(
            "{}",
            clock
                .now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ),
    }
}

/// Temp paths: `.record-ui-both-<id>-A.mkv` / `-B.mkv` under `output_dir`.
pub fn both_temp_paths(output_dir: &Path, session_id: &str) -> (PathBuf, PathBuf) {
    (
        output_dir.join(format!(".record-ui-both-{session_id}-A.mkv")),
        output_dir.join(format!(".record-ui-both-{session_id}-B.mkv")),
    )
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
        /// Fail spawn at this 0-based index into `spawns` (after record push).
        fail_at_spawn: Option<usize>,
        missing: Vec<String>,
        spawns: Vec<SpawnRecord>,
        signal_log: Arc<Mutex<Vec<SignalEvent>>>,
        /// When spawning wf-recorder, auto-attach write path from `-f`.
        auto_write_on_signal: bool,
        /// Bytes written on signal when auto_write is on (`None` = missing file).
        auto_write_bytes: Option<Vec<u8>>,
        /// When spawning ffmpeg, write final path (last argv) immediately if true.
        auto_write_ffmpeg: bool,
        recorder_ignore_sigint: bool,
        recorder_ignore_sigterm: bool,
        recorder_exit: ExitStatus,
        recorder_stderr: String,
        ffmpeg_exit: ExitStatus,
        ffmpeg_stderr: String,
    }

    impl FakeSpawner {
        fn new() -> Self {
            Self {
                next_pid: 2000,
                scripts: VecDeque::new(),
                fail_binary: None,
                fail_at_spawn: None,
                missing: Vec::new(),
                spawns: Vec::new(),
                signal_log: Arc::new(Mutex::new(Vec::new())),
                auto_write_on_signal: true,
                auto_write_bytes: Some(b"fake-video-bytes".to_vec()),
                auto_write_ffmpeg: true,
                recorder_ignore_sigint: false,
                recorder_ignore_sigterm: false,
                recorder_exit: ExitStatus::Code(0),
                recorder_stderr: String::new(),
                ffmpeg_exit: ExitStatus::Code(0),
                ffmpeg_stderr: String::new(),
            }
        }

        fn push_script(&mut self, script: FakeChildScript) {
            self.scripts.push_back(script);
        }

        fn signal_log(&self) -> Vec<SignalEvent> {
            self.signal_log.lock().unwrap().clone()
        }

        fn signal_log_arc(&self) -> Arc<Mutex<Vec<SignalEvent>>> {
            Arc::clone(&self.signal_log)
        }

        fn wf_spawns(&self) -> Vec<&SpawnRecord> {
            self.spawns
                .iter()
                .filter(|s| s.argv.first().map(|a| a == "wf-recorder").unwrap_or(false))
                .collect()
        }

        fn ffmpeg_spawns(&self) -> Vec<&SpawnRecord> {
            self.spawns
                .iter()
                .filter(|s| s.argv.first().map(|a| a == "ffmpeg").unwrap_or(false))
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
            let spawn_idx = self.spawns.len();
            self.spawns.push(SpawnRecord {
                argv: argv.to_vec(),
                opts: opts.clone(),
            });

            if self.fail_at_spawn == Some(spawn_idx) {
                return Err(PortError::Spawn(format!(
                    "simulated spawn fail at index {spawn_idx} for {bin}"
                )));
            }

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
            } else if bin == "ffmpeg" {
                script.wait_for_signal = false;
                script.ignore_sigint = false;
                script.ignore_sigterm = false;
                script.exit = self.ffmpeg_exit;
                script.stderr = self.ffmpeg_stderr.clone();
                script.write_file_on_signal = None;
                script.write_bytes_on_signal = None;
                // Write final output (last non-flag-ish argv) on successful fake stitch.
                if self.auto_write_ffmpeg && self.ffmpeg_exit.success() {
                    if let Some(out) = argv.last() {
                        if !out.starts_with('-') {
                            let bytes = self
                                .auto_write_bytes
                                .clone()
                                .unwrap_or_else(|| b"fake-stitched-mp4".to_vec());
                            let _ = fs::write(out, bytes);
                        }
                    }
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

    // ----- Both dual session (slice 04) -----

    /// SPEC example: HDMI 2560×1440 @0,0 + DP 1920×1080 @2560,180 → canvas 4480×1440.
    fn y180_layout_inventory() -> Vec<OutputInfo> {
        vec![
            OutputInfo {
                name: "HDMI-A-1".into(),
                x: Some(0),
                y: Some(0),
                width: Some(2560),
                height: Some(1440),
                refresh: Some(144.0),
            },
            OutputInfo {
                name: "DP-1".into(),
                x: Some(2560),
                y: Some(180),
                width: Some(1920),
                height: Some(1080),
                refresh: Some(60.0),
            },
        ]
    }

    fn make_both_recorder(
        paths: &TempPaths,
        spawner: FakeSpawner,
    ) -> Recorder<FakeSpawner, FakeClock, FakeNotifier, FakeClipboard> {
        let mut rec = make_recorder(paths, spawner);
        rec.set_forced_layout_inventory(Some(y180_layout_inventory()));
        rec
    }

    #[test]
    fn layout_true_y180_canvas_and_filter() {
        let inv = y180_layout_inventory();
        let layout = both_layout_from_inventory(&inv).expect("layout");
        assert_eq!(layout.primary.name, "HDMI-A-1");
        assert_eq!(layout.secondary.name, "DP-1");
        let canvas = layout_canvas(&layout);
        assert_eq!(canvas.width, 4480);
        assert_eq!(canvas.height, 1440);
        assert_eq!((canvas.primary_ox, canvas.primary_oy), (0, 0));
        assert_eq!((canvas.secondary_ox, canvas.secondary_oy), (2560, 180));

        let filter = build_layout_true_filter(&layout);
        assert!(filter.contains("2560:1440"), "primary scale: {filter}");
        assert!(filter.contains("1920:1080"), "secondary scale: {filter}");
        assert!(filter.contains("4480x1440"), "canvas: {filter}");
        assert!(filter.contains("overlay=2560:180"), "DP offset: {filter}");
        assert!(
            filter.contains(&format!("r={BOTH_FPS}")),
            "color source fps: {filter}"
        );
        assert!(filter.contains("format=yuv420p"), "yuv420p: {filter}");
        assert!(
            !filter.contains("hstack"),
            "FORBIDDEN hstack default: {filter}"
        );

        let argv = build_layout_true_ffmpeg_argv(
            Path::new("/tmp/a.mkv"),
            Path::new("/tmp/b.mkv"),
            &layout,
            Path::new("/tmp/out.mp4"),
            true,
        );
        assert_eq!(argv[0], "ffmpeg");
        assert!(argv.contains(&"-shortest".into()), "duration policy");
        assert!(argv.contains(&"0:a?".into()), "audio from primary");
        assert!(argv.windows(2).any(|w| w[0] == "-r" && w[1] == "60"));
        assert!(!argv.iter().any(|a| a.contains("hstack")));

        let no_audio = build_layout_true_ffmpeg_argv(
            Path::new("/tmp/a.mkv"),
            Path::new("/tmp/b.mkv"),
            &layout,
            Path::new("/tmp/out.mp4"),
            false,
        );
        assert!(!no_audio.iter().any(|a| a == "0:a?"));
        assert!(!no_audio.iter().any(|a| a == "aac"));
    }

    #[test]
    fn layout_canvas_odd_dimensions_forced_even() {
        // Odd secondary height/offset → raw AABB height odd; canvas must be even for yuv420p.
        let layout = BothLayout {
            primary: HeadGeom {
                name: "A".into(),
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            secondary: HeadGeom {
                name: "B".into(),
                x: 100,
                y: 1,
                width: 101,
                height: 99,
            },
        };
        let c = layout_canvas(&layout);
        assert_eq!(c.width % 2, 0, "even width {}", c.width);
        assert_eq!(c.height % 2, 0, "even height {}", c.height);
        assert_eq!((c.secondary_ox, c.secondary_oy), (100, 1));
    }

    #[test]
    fn both_layout_primary_independent_of_inventory_order() {
        let mut inv = y180_layout_inventory();
        inv.reverse(); // DP first, HDMI second
        let layout = both_layout_from_inventory(&inv).unwrap();
        assert_eq!(layout.primary.name, "HDMI-A-1");
        assert_eq!(layout.secondary.name, "DP-1");
    }

    #[test]
    fn both_start_argv_dual_60_no_damage_audio_primary_only() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let mut rec = make_both_recorder(&paths, spawner);
        let r = rec.start_both(Some(true), true);
        assert!(r.ok, "{r:?}");
        assert_eq!(rec.state(), State::Recording);
        let st = rec.status();
        assert_eq!(st.capture_mode.as_deref(), Some("both"));
        assert_eq!(st.capture_output.as_deref(), Some("HDMI-A-1+DP-1"));
        assert!(st.pid.is_some());

        let wf = rec.spawner().wf_spawns();
        assert_eq!(wf.len(), 2, "two children");
        for s in &wf {
            assert!(s.opts.new_process_group);
            let a = &s.argv;
            assert!(a.windows(2).any(|w| w[0] == "-r" && w[1] == "60"), "{a:?}");
            assert!(a.iter().any(|x| x == "-D"), "need -D: {a:?}");
            assert!(a.windows(2).any(|w| w[0] == "-o"), "{a:?}");
        }
        // Primary HDMI gets -a; secondary does not.
        let a0 = &wf[0].argv;
        let a1 = &wf[1].argv;
        assert!(
            a0.windows(2).any(|w| w[0] == "-o" && w[1] == "HDMI-A-1"),
            "primary first: {a0:?}"
        );
        assert!(a0.iter().any(|x| x == "-a"), "audio on primary: {a0:?}");
        assert!(
            a1.windows(2).any(|w| w[0] == "-o" && w[1] == "DP-1"),
            "secondary: {a1:?}"
        );
        assert!(
            !a1.iter().any(|x| x == "-a"),
            "no audio on secondary: {a1:?}"
        );
        let _ = rec.stop();
    }

    #[test]
    fn both_dual_stop_signals_both_and_stitches() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let mut rec = make_both_recorder(&paths, spawner);
        assert!(rec.start_both(None, false).ok);
        assert_eq!(rec.state(), State::Recording);
        let primary_pid = rec.status().pid.expect("primary pid");

        let r = rec.stop();
        assert!(r.ok, "{r:?}");
        assert_eq!(rec.state(), State::Idle);
        assert!(rec.last_success_path().is_some());

        let log = rec.spawner().signal_log();
        let int_groups: Vec<_> = log
            .iter()
            .filter(|e| e.signal == Signal::Interrupt && e.group)
            .collect();
        assert_eq!(int_groups.len(), 2, "exactly two SIGINT+group, got {log:?}");
        let int_pids: std::collections::HashSet<u32> = int_groups.iter().map(|e| e.pid).collect();
        assert_eq!(int_pids.len(), 2, "distinct pids: {int_groups:?}");
        assert!(int_pids.contains(&primary_pid));
        assert!(
            !log.iter()
                .any(|e| e.signal == Signal::Terminate || e.signal == Signal::Kill),
            "no escalation on cooperative dual stop: {log:?}"
        );

        let ff = rec.spawner().ffmpeg_spawns();
        assert_eq!(ff.len(), 1, "one stitch");
        let ff_argv = &ff[0].argv;
        assert!(
            !ff_argv.iter().any(|a| a.contains("hstack")),
            "no hstack: {ff_argv:?}"
        );
        assert!(ff_argv.iter().any(|a| a.contains("overlay=")));
        assert!(ff_argv.contains(&"-shortest".into()), "{ff_argv:?}");
        // No-audio session: omit audio map.
        assert!(!ff_argv.iter().any(|a| a == "0:a?"), "{ff_argv:?}");
        // Ordered -i A then -i B (temps).
        let i_pos: Vec<_> = ff_argv
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-i")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(i_pos.len(), 2);
        assert!(
            ff_argv[i_pos[0] + 1].contains("-A.mkv") && ff_argv[i_pos[1] + 1].contains("-B.mkv"),
            "primary then secondary temps: {ff_argv:?}"
        );

        // Temps deleted on success.
        let vids = paths.output_dir();
        let leftover: Vec<_> = fs::read_dir(&vids)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("record-ui-both"))
            .collect();
        assert!(leftover.is_empty(), "temps should be deleted: {leftover:?}");

        // Success hooks on FINAL path only (not temps).
        let final_path = rec.last_success_path().unwrap();
        let final_str = final_path.display().to_string();
        assert_eq!(rec.clipboard().texts, vec![final_str.clone()]);
        assert!(!final_str.contains("record-ui-both"));
        for (_, body) in &rec.notifier().calls {
            assert!(!body.contains("record-ui-both"), "notify body: {body}");
        }
        assert!(final_path.extension().and_then(|e| e.to_str()) == Some("mp4"));
        assert!(file_nonempty(final_path).unwrap());
    }

    #[test]
    fn both_peer_death_reaps_peer_no_stitch() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        // Primary dies after settle try_wait; secondary stays until force-reaped.
        spawner.push_script(FakeChildScript {
            wait_for_signal: true,
            auto_exit_after_try_waits: Some(1),
            exit: ExitStatus::Code(1),
            write_bytes_on_signal: Some(b"partial".to_vec()),
            ..Default::default()
        });
        spawner.push_script(FakeChildScript {
            wait_for_signal: true,
            write_bytes_on_signal: Some(b"still-running".to_vec()),
            ..Default::default()
        });
        let mut rec = make_both_recorder(&paths, spawner);
        let r = rec.start_both(None, false);
        assert!(
            r.ok,
            "start ok after settle if primary still alive once: {r:?}"
        );
        let primary_pid = rec.status().pid.expect("pid");
        // Secondary is next pid after primary (FakeSpawner increments).
        let secondary_pid = primary_pid + 1;

        let r = rec.poll().expect("peer death");
        assert!(!r.ok, "{r:?}");
        assert!(r.message.contains("peer death"), "{r:?}");
        assert!(r.message.contains("no stitch"), "{r:?}");
        assert!(
            r.message.contains("record-ui-both") || r.message.contains("temps="),
            "temp paths in error: {r:?}"
        );
        assert_eq!(rec.state(), State::Idle);
        assert!(
            rec.spawner().ffmpeg_spawns().is_empty(),
            "no stitch on peer death"
        );
        assert!(rec.clipboard().texts.is_empty(), "no success clipboard");
        assert!(rec.last_success_path().is_none());
        let err = rec.status().last_error.expect("last_error");
        assert!(err.contains("peer death"), "{err}");
        assert!(
            err.contains("temps=") || err.contains("record-ui-both"),
            "{err}"
        );

        // Survivor (secondary) force-reaped via process group.
        let log = rec.spawner().signal_log();
        assert!(
            log.iter().any(|e| e.pid == secondary_pid && e.group),
            "secondary {secondary_pid} force-reaped: {log:?}"
        );
    }

    #[test]
    fn both_peer_death_stop_without_poll_no_stitch() {
        // Race: child dies mid-record; stop() wins before poll → still no stitch (§7.6).
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.push_script(FakeChildScript {
            wait_for_signal: true,
            auto_exit_after_try_waits: Some(1),
            exit: ExitStatus::Code(1),
            ..Default::default()
        });
        spawner.push_script(FakeChildScript {
            wait_for_signal: true,
            ..Default::default()
        });
        let mut rec = make_both_recorder(&paths, spawner);
        assert!(rec.start_both(None, false).ok);
        let r = rec.stop();
        assert!(!r.ok, "{r:?}");
        assert!(r.message.contains("peer death"), "{r:?}");
        assert!(rec.spawner().ffmpeg_spawns().is_empty());
        assert!(rec.clipboard().texts.is_empty());
        assert_eq!(rec.state(), State::Idle);
    }

    #[test]
    fn both_partial_spawn_reaps_first() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        // Spawn index 0 = primary ok; index 1 = secondary fails.
        spawner.fail_at_spawn = Some(1);
        let mut rec = make_both_recorder(&paths, spawner);
        let r = rec.start_both(None, false);
        assert!(!r.ok, "{r:?}");
        assert_eq!(r.code, MachineCode::SpawnFailed);
        assert_eq!(rec.state(), State::Idle);
        assert_eq!(rec.spawner().wf_spawns().len(), 2);
        // Primary pid is first FakeSpawner pid (2000).
        let primary_pid = 2000u32;
        let log = rec.spawner().signal_log();
        assert!(
            log.iter().any(|e| e.pid == primary_pid && e.group),
            "primary {primary_pid} force-reaped: {log:?}"
        );
        assert!(
            r.message.contains("reaped primary") || r.message.contains("cleaned temp"),
            "{r:?}"
        );
        assert!(rec.spawner().ffmpeg_spawns().is_empty());
        assert!(rec.status().capture_mode.is_none());
        assert!(rec.status().pid.is_none());
    }

    #[test]
    fn both_empty_temps_no_stitch() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.auto_write_on_signal = false;
        let mut rec = make_both_recorder(&paths, spawner);
        assert!(rec.start_both(None, false).ok);
        let r = rec.stop();
        assert!(!r.ok, "{r:?}");
        assert_eq!(r.code, MachineCode::IoError);
        assert!(r.message.contains("temp missing or empty"), "{r:?}");
        assert!(rec.spawner().ffmpeg_spawns().is_empty());
        assert!(rec.clipboard().texts.is_empty());
        assert_eq!(rec.state(), State::Idle);
        assert!(rec.last_success_path().is_none());
    }

    #[test]
    fn both_stop_timeout_nuclear_both_no_stitch() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.recorder_ignore_sigint = true;
        spawner.recorder_ignore_sigterm = true;
        let mut rec = make_both_recorder(&paths, spawner);
        assert!(rec.start_both(None, false).ok);
        let primary_pid = rec.status().pid.expect("pid");
        let secondary_pid = primary_pid + 1;

        let r = rec.stop();
        assert!(!r.ok, "{r:?}");
        assert_eq!(r.code, MachineCode::StopTimeout);
        assert!(
            r.message.contains("nuclear") || r.message.contains("timed out"),
            "{r:?}"
        );
        assert!(rec.spawner().ffmpeg_spawns().is_empty());
        assert!(rec.clipboard().texts.is_empty());
        assert_eq!(rec.state(), State::Idle);

        let log = rec.spawner().signal_log();
        // Both PIDs should see INT → TERM → KILL (group).
        for pid in [primary_pid, secondary_pid] {
            let seq: Vec<_> = log
                .iter()
                .filter(|e| e.pid == pid && e.group)
                .map(|e| e.signal)
                .collect();
            assert!(
                seq.contains(&Signal::Interrupt)
                    && seq.contains(&Signal::Terminate)
                    && seq.contains(&Signal::Kill),
                "pid {pid} escalation {seq:?} log={log:?}"
            );
        }
        // Temps retained (written on signal before ignore... actually ignore_sigint still
        // maybe_write_file on Interrupt). auto_write runs on signal even when ignore.
        let leftover: Vec<_> = fs::read_dir(paths.output_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("record-ui-both"))
            .collect();
        assert_eq!(leftover.len(), 2, "retain temps: {leftover:?}");
    }

    #[test]
    fn both_drop_mid_session_reaps_no_stitch() {
        let paths = TempPaths::new();
        let spawner = FakeSpawner::new();
        let (log_arc, primary_pid, secondary_pid) = {
            let mut rec = make_both_recorder(&paths, spawner);
            assert!(rec.start_both(None, false).ok);
            let primary_pid = rec.status().pid.expect("primary");
            let secondary_pid = primary_pid + 1;
            let log_arc = rec.spawner().signal_log_arc();
            drop(rec); // unclean Drop: reap both, skip stitch
            (log_arc, primary_pid, secondary_pid)
        };
        let log = log_arc.lock().unwrap().clone();
        for pid in [primary_pid, secondary_pid] {
            assert!(
                log.iter().any(|e| e.pid == pid && e.group),
                "drop reaped {pid}: {log:?}"
            );
        }
        // No success stitch on Drop: ffmpeg never spawned (log has only recorder signals).
        assert!(!log.is_empty(), "expected force-reap signals on drop");
    }

    #[test]
    fn both_requires_exactly_two_lists_names() {
        let paths = TempPaths::new();
        let mut rec = make_recorder(&paths, FakeSpawner::new());
        rec.set_forced_layout_inventory(Some(vec![
            OutputInfo {
                name: "A".into(),
                x: Some(0),
                y: Some(0),
                width: Some(100),
                height: Some(100),
                refresh: None,
            },
            OutputInfo {
                name: "B".into(),
                x: Some(100),
                y: Some(0),
                width: Some(100),
                height: Some(100),
                refresh: None,
            },
            OutputInfo {
                name: "C".into(),
                x: Some(200),
                y: Some(0),
                width: Some(100),
                height: Some(100),
                refresh: None,
            },
        ]));
        let r = rec.start_both(None, false);
        assert!(!r.ok);
        assert_eq!(r.code, MachineCode::Invalid);
        assert!(
            r.message.contains('A') && r.message.contains('B') && r.message.contains('C'),
            "{r:?}"
        );
        assert!(rec.spawner().wf_spawns().is_empty());
    }

    #[test]
    fn both_requires_exactly_two_zero_or_one_head() {
        let paths = TempPaths::new();
        let mut rec = make_recorder(&paths, FakeSpawner::new());
        rec.set_forced_layout_inventory(Some(vec![]));
        let r = rec.start_both(None, false);
        assert!(!r.ok);
        assert_eq!(r.code, MachineCode::Invalid);
        assert!(r.message.contains("exactly 2"), "{r:?}");

        rec.set_forced_layout_inventory(Some(vec![OutputInfo {
            name: "Solo".into(),
            x: Some(0),
            y: Some(0),
            width: Some(1920),
            height: Some(1080),
            refresh: None,
        }]));
        let r = rec.start_both(None, false);
        assert!(!r.ok);
        assert_eq!(r.code, MachineCode::Invalid);
        assert!(r.message.contains("Solo"), "{r:?}");
    }

    #[test]
    fn both_names_only_inventory_fails() {
        let paths = TempPaths::new();
        let mut rec = make_recorder(&paths, FakeSpawner::new());
        rec.set_forced_output_inventory(Some(vec!["HDMI-A-1".into(), "DP-1".into()]));
        let r = rec.start_both(None, false);
        assert!(!r.ok, "{r:?}");
        assert_eq!(r.code, MachineCode::Invalid);
        assert!(
            r.message.contains("hyprctl")
                || r.message.contains("geometry")
                || r.message.contains("positions"),
            "{r:?}"
        );
        assert!(rec.spawner().wf_spawns().is_empty());
    }

    #[test]
    fn both_missing_ffmpeg_dep() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.missing.push("ffmpeg".into());
        let mut rec = make_both_recorder(&paths, spawner);
        let r = rec.start_both(None, false);
        assert!(!r.ok);
        assert_eq!(r.code, MachineCode::DepMissing);
        assert!(r.message.contains("ffmpeg"), "{r:?}");
    }

    #[test]
    fn both_missing_wf_recorder_dep() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.missing.push("wf-recorder".into());
        let mut rec = make_both_recorder(&paths, spawner);
        let r = rec.start_both(None, false);
        assert!(!r.ok);
        assert_eq!(r.code, MachineCode::DepMissing);
        assert!(r.message.contains("wf-recorder"), "{r:?}");
    }

    #[test]
    fn both_stitch_fail_retains_temps_no_clipboard() {
        let paths = TempPaths::new();
        let mut spawner = FakeSpawner::new();
        spawner.ffmpeg_exit = ExitStatus::Code(1);
        spawner.ffmpeg_stderr = "filter graph broken".into();
        spawner.auto_write_ffmpeg = false;
        let mut rec = make_both_recorder(&paths, spawner);
        assert!(rec.start_both(None, false).ok);
        let r = rec.stop();
        assert!(!r.ok, "{r:?}");
        assert!(r.message.contains("temps retained"), "{r:?}");
        assert!(
            r.message.contains("ffmpeg") || r.message.contains("stitch"),
            "{r:?}"
        );
        assert!(
            r.message.contains("-A.mkv") && r.message.contains("-B.mkv"),
            "both temp basenames: {r:?}"
        );
        let err = rec.status().last_error.expect("last_error");
        assert!(err.contains("temps retained"), "{err}");
        assert!(err.contains("-A.mkv") && err.contains("-B.mkv"), "{err}");
        assert!(rec.clipboard().texts.is_empty());
        assert!(rec.last_success_path().is_none());
        assert_eq!(rec.state(), State::Idle);
        let leftover: Vec<_> = fs::read_dir(paths.output_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("record-ui-both"))
            .collect();
        assert_eq!(leftover.len(), 2, "retain both temps: {leftover:?}");
    }

    #[test]
    fn both_busy_while_recording() {
        let paths = TempPaths::new();
        let mut rec = make_both_recorder(&paths, FakeSpawner::new());
        assert!(rec.start_both(None, false).ok);
        let r = rec.start_both(None, false);
        assert!(!r.ok);
        assert_eq!(r.code, MachineCode::Busy);
        let _ = rec.stop();
    }

    #[test]
    fn build_both_wf_recorder_argv_shape() {
        let p = Path::new("/tmp/t.mkv");
        let a = build_both_wf_recorder_argv("HDMI-A-1", true, p);
        assert_eq!(
            a,
            vec![
                "wf-recorder",
                "-o",
                "HDMI-A-1",
                "-r",
                "60",
                "-D",
                "-a",
                "-f",
                "/tmp/t.mkv"
            ]
        );
        let b = build_both_wf_recorder_argv("DP-1", false, p);
        assert!(!b.iter().any(|x| x == "-a"));
        assert!(b.iter().any(|x| x == "-D"));
    }
}
