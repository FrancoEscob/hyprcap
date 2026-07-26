//! GTK4 + libadwaita GUI view (client only).
//!
//! Opened solely via `hyprcap` / `hyprcap gui`. Closing the window
//! disconnects this view — it does **not** stop an active recording.
//!
//! Long IPC (start_region/slurp) runs on dedicated threads so Stop/status
//! remain available on separate connections. Status polls are coalesced.
//! Timer display prefers `started_at_unix` + wall clock for smooth ticks.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, CheckButton, DropDown, Label, Orientation, StringList,
    ToggleButton,
};
use libadwaita as adw;
use libadwaita::prelude::*;
use hyprcap::audio::{AudioInventory, AudioPlan, SystemAudioMode};
use hyprcap::client::{self, ClientError};
use hyprcap::config::Config;
use hyprcap::ipc::{IpcRequest, IpcResponse, IpcStatus};
use hyprcap::server::{self, RuntimePaths};
use hyprcap::sys::{self, EnvPaths, OutputInfo};

/// Primary entry: ensure session server, open small Adwaita window.
///
/// Returns a process exit code (0 on clean window close).
pub fn run_gui() -> i32 {
    let paths = match RuntimePaths::from_env() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("hyprcap: {e}");
            return 1;
        }
    };

    if let Err(e) = server::ensure_server(&paths) {
        eprintln!("hyprcap: failed to start session server: {e}");
        return 1;
    }

    // Init Adwaita/GTK only on this path — never from CLI subcommands.
    if let Err(e) = adw::init() {
        eprintln!("hyprcap: failed to initialize libadwaita: {e}");
        return 1;
    }

    let app = adw::Application::builder()
        .application_id("dev.hyprcap.app")
        .build();

    let socket_path = paths.socket_path.clone();
    // Single window: re-activate raises existing rather than building another.
    let window_slot: Rc<RefCell<Option<adw::ApplicationWindow>>> = Rc::new(RefCell::new(None));
    let slot_for_activate = Rc::clone(&window_slot);
    app.connect_activate(move |app| {
        if let Some(ref win) = *slot_for_activate.borrow() {
            win.present();
            return;
        }
        let win = build_window(app, socket_path.clone(), Rc::clone(&slot_for_activate));
        slot_for_activate.borrow_mut().replace(win);
    });

    // No argv forwarding: CLI already selected Gui.
    let code = app.run_with_args::<&str>(&[]);
    code.value()
}

// ---------------------------------------------------------------------------
// Worker IPC
// ---------------------------------------------------------------------------

enum WorkerCmd {
    PollStatus,
    ListAudio,
    StartRegion {
        plan: hyprcap::audio::AudioPlan,
        epoch: u64,
    },
    StartFullscreen {
        plan: hyprcap::audio::AudioPlan,
        epoch: u64,
        /// Wayland output name for `-o` (required when multi-head).
        output: Option<String>,
        /// Explicit FPS for this start. `Some(0)` forces Auto (no `-r`).
        fps: Option<u32>,
    },
    /// Dual-monitor Both session (exactly 2 heads + ffmpeg; layout on server).
    StartBoth {
        plan: hyprcap::audio::AudioPlan,
        epoch: u64,
    },
    Stop,
    ShutdownWorker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorSource {
    Start,
    Stop,
    Status,
}

enum UiMsg {
    Status(IpcStatus),
    OpDone {
        kind: OpKind,
        resp: IpcResponse,
        /// Start epoch captured at enqueue; ignored for Stop.
        epoch: u64,
    },
    Error {
        source: ErrorSource,
        message: String,
        epoch: u64,
    },
    /// Non-error notice (e.g. reconnected to session server).
    Info(String),
    Subscribed(IpcStatus),
    /// Subscribe could not be established — close the view.
    SubscribeFailed(String),
    /// Pulse/PipeWire inventory for audio pickers.
    AudioList(IpcResponse),
}

#[derive(Clone, Copy)]
enum OpKind {
    Start,
    Stop,
}

struct WorkerHandle {
    tx: Sender<WorkerCmd>,
    /// Coalesce PollStatus: UI only enqueues when false; worker clears after poll.
    poll_pending: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

fn spawn_worker(socket: PathBuf, ui_tx: Sender<UiMsg>) -> WorkerHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
    let poll_pending = Arc::new(AtomicBool::new(false));
    let poll_flag = Arc::clone(&poll_pending);

    let join = thread::spawn(move || {
        worker_main(socket, ui_tx, cmd_rx, poll_flag);
    });

    WorkerHandle {
        tx: cmd_tx,
        poll_pending,
        join: Some(join),
    }
}

fn worker_main(
    socket: PathBuf,
    ui_tx: Sender<UiMsg>,
    cmd_rx: Receiver<WorkerCmd>,
    poll_pending: Arc<AtomicBool>,
) {
    // Hard requirement: establish subscribe hold (retry + ensure_server).
    let mut subscribe_hold = match establish_subscribe(&socket) {
        Ok((stream, status)) => {
            let _ = ui_tx.send(UiMsg::Subscribed(status));
            Some(stream)
        }
        Err(err) => {
            let _ = ui_tx.send(UiMsg::SubscribeFailed(format!(
                "could not attach GUI view: {err}"
            )));
            // Drain until Shutdown so the UI can close cleanly.
            while let Ok(cmd) = cmd_rx.recv() {
                if matches!(cmd, WorkerCmd::ShutdownWorker) {
                    break;
                }
            }
            return;
        }
    };

    let mut connect_fail_streak: u32 = 0;
    const CONNECT_FAIL_LIMIT: u32 = 5;
    let start_handles: Arc<Mutex<Vec<thread::JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
    let shutting_down = Arc::new(AtomicBool::new(false));

    while let Ok(first) = cmd_rx.recv() {
        // Drain pending commands; coalesce polls; prioritize Shutdown/Stop.
        let mut cmds = vec![first];
        while let Ok(c) = cmd_rx.try_recv() {
            cmds.push(c);
        }

        let mut shutdown = false;
        let mut want_stop = false;
        let mut want_list_audio = false;
        let mut starts: Vec<WorkerCmd> = Vec::new();
        let mut want_poll = false;

        for c in cmds {
            match c {
                WorkerCmd::ShutdownWorker => shutdown = true,
                WorkerCmd::Stop => want_stop = true,
                WorkerCmd::PollStatus => want_poll = true,
                WorkerCmd::ListAudio => want_list_audio = true,
                s @ WorkerCmd::StartRegion { .. }
                | s @ WorkerCmd::StartFullscreen { .. }
                | s @ WorkerCmd::StartBoth { .. } => {
                    starts.push(s);
                }
            }
        }

        if shutdown {
            shutting_down.store(true, Ordering::SeqCst);
            // Unblock any UI poll CAS waiting for worker clear.
            poll_pending.store(false, Ordering::SeqCst);
            break;
        }

        // Same batch: Stop supersedes Start* — do not spawn starts after cancel.
        if want_stop {
            starts.clear();
        }

        // Stop on this thread (separate connection from any in-flight start).
        if want_stop {
            match client::request(&socket, &IpcRequest::stop()) {
                Ok(resp) => {
                    connect_fail_streak = 0;
                    let _ = ui_tx.send(UiMsg::OpDone {
                        kind: OpKind::Stop,
                        resp,
                        epoch: 0,
                    });
                }
                Err(e) => {
                    let _ = ui_tx.send(UiMsg::Error {
                        source: ErrorSource::Stop,
                        message: format!("stop: {e}"),
                        epoch: 0,
                    });
                }
            }
        }

        if want_list_audio {
            match client::request(&socket, &IpcRequest::list_audio()) {
                Ok(resp) => {
                    connect_fail_streak = 0;
                    let _ = ui_tx.send(UiMsg::AudioList(resp));
                }
                Err(e) => {
                    let _ = ui_tx.send(UiMsg::Error {
                        source: ErrorSource::Status,
                        message: format!("list audio: {e}"),
                        epoch: 0,
                    });
                }
            }
        }

        // Long starts on dedicated threads so Stop/status stay responsive.
        for start in starts {
            let sock = socket.clone();
            let tx = ui_tx.clone();
            let sd = Arc::clone(&shutting_down);
            let handle = thread::spawn(move || {
                let (kind_label, req, epoch) = match start {
                    WorkerCmd::StartRegion { plan, epoch } => (
                        "start region",
                        IpcRequest::start_region_plan(plan),
                        epoch,
                    ),
                    WorkerCmd::StartFullscreen {
                        plan,
                        epoch,
                        output,
                        fps,
                    } => (
                        "start one monitor",
                        IpcRequest::start_fullscreen_plan(plan, output, fps),
                        epoch,
                    ),
                    WorkerCmd::StartBoth { plan, epoch } => {
                        ("start both", IpcRequest::start_both_plan(plan), epoch)
                    }
                    _ => return,
                };
                match client::request(&sock, &req) {
                    Ok(resp) => {
                        if sd.load(Ordering::SeqCst) {
                            return;
                        }
                        let _ = tx.send(UiMsg::OpDone {
                            kind: OpKind::Start,
                            resp,
                            epoch,
                        });
                    }
                    Err(e) => {
                        if sd.load(Ordering::SeqCst) {
                            return;
                        }
                        let _ = tx.send(UiMsg::Error {
                            source: ErrorSource::Start,
                            message: format!("{kind_label}: {e}"),
                            epoch,
                        });
                    }
                }
            });
            if let Ok(mut g) = start_handles.lock() {
                // Drop finished handles opportunistically (join completed ones).
                g.retain(|h| !h.is_finished());
                g.push(handle);
            }
        }

        // At most one status RPC per drain (coalesced).
        if want_poll {
            match client::request(&socket, &IpcRequest::status()) {
                Ok(resp) => {
                    connect_fail_streak = 0;
                    let _ = ui_tx.send(UiMsg::Status(resp.status));
                }
                Err(e) => {
                    if is_connect_like(&e) {
                        connect_fail_streak = connect_fail_streak.saturating_add(1);
                        if connect_fail_streak >= CONNECT_FAIL_LIMIT {
                            connect_fail_streak = 0;
                            // Revive daemon and re-establish GUI hold (gui_clients policy).
                            match try_ensure_runtime(&socket) {
                                Ok(()) => match establish_subscribe(&socket) {
                                    Ok((stream, status)) => {
                                        // Replace hold (drop old first so prior server can exit).
                                        drop(subscribe_hold.take());
                                        subscribe_hold = Some(stream);
                                        let _ = ui_tx.send(UiMsg::Subscribed(status));
                                        let _ = ui_tx.send(UiMsg::Info(
                                            "reconnected to session server".into(),
                                        ));
                                    }
                                    Err(err) => {
                                        drop(subscribe_hold.take());
                                        let _ = ui_tx.send(UiMsg::SubscribeFailed(format!(
                                            "lost session and could not re-attach: {err}"
                                        )));
                                        break;
                                    }
                                },
                                Err(err) => {
                                    let _ = ui_tx.send(UiMsg::Error {
                                        source: ErrorSource::Status,
                                        message: format!(
                                            "lost connection to session server ({e}); revive failed: {err}"
                                        ),
                                        epoch: 0,
                                    });
                                }
                            }
                        }
                    } else if !is_transient(&e) {
                        let _ = ui_tx.send(UiMsg::Error {
                            source: ErrorSource::Status,
                            message: format!("status: {e}"),
                            epoch: 0,
                        });
                    }
                }
            }
            poll_pending.store(false, Ordering::SeqCst);
        }
    }

    // Drop subscribe stream → server decrements gui_clients.
    drop(subscribe_hold);
    poll_pending.store(false, Ordering::SeqCst);

    // Best-effort reap of start threads (worker is off the GTK thread).
    let kids: Vec<_> = start_handles
        .lock()
        .map(|mut g| g.drain(..).collect())
        .unwrap_or_default();
    for h in kids {
        if h.is_finished() {
            let _ = h.join();
        } else {
            // Detached reaper so ShutdownWorker returns promptly.
            thread::spawn(move || {
                let _ = h.join();
            });
        }
    }
}

/// Retry subscribe with ensure_server.
fn establish_subscribe(
    socket: &Path,
) -> Result<(std::os::unix::net::UnixStream, IpcStatus), String> {
    const ATTEMPTS: u32 = 8;
    let mut last_err = String::new();
    for attempt in 0..ATTEMPTS {
        let _ = try_ensure_runtime(socket);
        match client::subscribe(socket) {
            Ok((stream, resp)) if resp.ok => {
                return Ok((stream, resp.status));
            }
            Ok((_, resp)) => {
                last_err = format!("subscribe rejected: {} ({})", resp.message, resp.code);
            }
            Err(e) => {
                last_err = e.to_string();
            }
        }
        thread::sleep(Duration::from_millis(40 * (attempt + 1) as u64));
    }
    Err(last_err)
}

fn try_ensure_runtime(socket: &Path) -> Result<(), String> {
    let runtime_dir = socket
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let paths = RuntimePaths::from_runtime_dir(runtime_dir);
    server::ensure_server(&paths)
}

fn is_connect_like(e: &ClientError) -> bool {
    matches!(e, ClientError::Connect(_))
        || matches!(
            e,
            ClientError::Io(io) if matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::NotConnected
            )
        )
}

fn is_transient(e: &ClientError) -> bool {
    match e {
        ClientError::Connect(_) => true,
        ClientError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
        ),
        ClientError::Protocol(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------

/// Minimum gap between idle-path Both gate revalidates (avoids hyprctl every status poll).
const BOTH_REVALIDATE_IDLE_MIN: Duration = Duration::from_secs(10);

/// Whether idle chrome should run a live Both gate check (pure; unit-tested).
///
/// - First attach (`prev_state` None) → true
/// - Busy → Idle edge (`prev_state` not Idle) → true
/// - Steady Idle polls → true only when `elapsed_since_last` is None or ≥ `min_interval`
fn should_revalidate_both_on_idle(
    prev_state: Option<&str>,
    elapsed_since_last: Option<Duration>,
    min_interval: Duration,
) -> bool {
    let entering_idle = match prev_state {
        None => true, // first status / Subscribed
        Some("Idle") => false,
        Some(_) => true, // was busy (Recording/Stopping/…)
    };
    let stale = match elapsed_since_last {
        None => true, // never stamped
        Some(e) => e >= min_interval,
    };
    entering_idle || stale
}

struct ViewState {
    last_status: Option<IpcStatus>,
    last_path: Option<String>,
    /// Start (region/fullscreen/both) RPC in flight.
    start_in_flight: bool,
    /// Stop RPC in flight.
    stop_in_flight: bool,
    /// Stop may block on Both layout-true stitch — keep Composing UI copy.
    /// Set when stop is requested for a Both session (incl. start_in_flight + Both mode).
    stopping_both: bool,
    /// Epoch for the active start request; Stop/new start bumps it.
    start_epoch: u64,
    /// Whether we already applied status.audio to the checkbox this session attach.
    audio_from_server: bool,
    /// Last time we ran live inventory/ffmpeg gate check (idle chrome throttle).
    last_both_revalidate: Option<Instant>,
}

fn build_window(
    app: &adw::Application,
    socket: PathBuf,
    window_slot: Rc<RefCell<Option<adw::ApplicationWindow>>>,
) -> adw::ApplicationWindow {
    let (ui_tx, ui_rx) = mpsc::channel::<UiMsg>();
    let worker = spawn_worker(socket, ui_tx);
    let worker = Rc::new(RefCell::new(Some(worker)));
    let closed = Rc::new(Cell::new(false));
    // Shared with click handlers for epoch assignment (also in ViewState).
    let start_epoch_counter = Rc::new(AtomicU64::new(1));

    // Compact control surface — Hyprland also floats+sizes via window rule.
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Hyprcap")
        .default_width(340)
        .default_height(420)
        .resizable(true)
        .build();

    let root = GtkBox::new(Orientation::Vertical, 6);
    root.set_margin_top(10);
    root.set_margin_bottom(10);
    root.set_margin_start(12);
    root.set_margin_end(12);

    let mode_row = GtkBox::new(Orientation::Horizontal, 6);
    let region_btn = ToggleButton::with_label("Region");
    let full_btn = ToggleButton::with_label("One monitor");
    // Label says "2 monitors" (not "Both") — only exactly two heads are supported.
    let both_btn = ToggleButton::with_label("2 monitors");
    region_btn.set_active(true);
    full_btn.set_group(Some(&region_btn));
    both_btn.set_group(Some(&region_btn));
    region_btn.set_hexpand(true);
    full_btn.set_hexpand(true);
    both_btn.set_hexpand(true);
    region_btn.set_tooltip_text(Some("Select a region with slurp (one head only)."));
    full_btn.set_tooltip_text(Some(
        "Capture one full monitor. List is live from hyprctl / wf-recorder — not hardcoded.",
    ));
    mode_row.append(&region_btn);
    mode_row.append(&full_btn);
    mode_row.append(&both_btn);
    root.append(&mode_row);

    // One-monitor pickers (sensitive only when mode = One monitor).
    let env_paths = EnvPaths::from_env();
    let loaded_cfg = Config::load(&env_paths).unwrap_or_else(|_| Config::with_defaults(&env_paths));
    let sticky_cfg = Rc::new(RefCell::new(loaded_cfg.clone()));
    let inventory = sys::list_output_inventory();
    // Both enablement: exactly 2 heads + ffmpeg on PATH (DUAL-MONITOR §5.2).
    // Revalidated on idle status and on Record when Both is selected (hotplug).
    let both_gate = both_enablement(inventory.len(), sys::which("ffmpeg").is_some());
    let both_eligible = Rc::new(Cell::new(both_gate.enabled()));
    both_btn.set_sensitive(both_gate.enabled());
    both_btn.set_tooltip_text(Some(both_gate.tooltip()));
    let picker = Rc::new(RefCell::new(PickerState::from_inventory_and_config(
        inventory,
        &loaded_cfg,
    )));

    let monitor_labels: Vec<String> = picker
        .borrow()
        .inventory
        .iter()
        .map(monitor_combo_label)
        .collect();
    let monitor_label_refs: Vec<&str> = monitor_labels.iter().map(String::as_str).collect();
    let monitor_dd = if monitor_label_refs.is_empty() {
        DropDown::from_strings(&["(no outputs)"])
    } else {
        DropDown::from_strings(&monitor_label_refs)
    };
    monitor_dd.set_selected(picker.borrow().selected_monitor as u32);
    monitor_dd.set_sensitive(false); // Region is default mode
    root.append(&monitor_dd);

    let fps_labels: Vec<String> = picker
        .borrow()
        .fps_entries
        .iter()
        .map(|e| e.label.clone())
        .collect();
    // build_fps_entries always yields at least Auto.
    debug_assert!(!fps_labels.is_empty());
    let fps_label_refs: Vec<&str> = fps_labels.iter().map(String::as_str).collect();
    let fps_dd = DropDown::from_strings(&fps_label_refs);
    fps_dd.set_selected(picker.borrow().selected_fps as u32);
    fps_dd.set_sensitive(false);
    root.append(&fps_dd);

    // --- Audio matrix: system (off/all/app) + optional mic ---
    let sys_audio_label = Label::new(Some("System sound"));
    sys_audio_label.set_halign(Align::Start);
    sys_audio_label.add_css_class("dim-label");
    root.append(&sys_audio_label);

    let system_mode_dd = DropDown::from_strings(&["Off", "All PC sound", "One app"]);
    let cfg_plan = sticky_cfg.borrow().audio_plan();
    system_mode_dd.set_selected(match cfg_plan.system {
        SystemAudioMode::Off => 0,
        SystemAudioMode::All => 1,
        SystemAudioMode::App => 2,
    });
    system_mode_dd.set_tooltip_text(Some(
        "All PC = everything on the selected output. App = one playing application. Mixed with mic into one track.",
    ));
    root.append(&system_mode_dd);

    let system_detail_dd = DropDown::from_strings(&["Default output"]);
    system_detail_dd.set_tooltip_text(Some("Output device (All) or playing app (App mode)."));
    root.append(&system_detail_dd);

    let mic_check = CheckButton::with_label("Microphone");
    mic_check.set_active(cfg_plan.mic);
    mic_check.set_tooltip_text(Some("Mix mic into the same audio track as system sound."));
    root.append(&mic_check);

    let mic_dd = DropDown::from_strings(&["Default mic"]);
    mic_dd.set_sensitive(cfg_plan.mic);
    root.append(&mic_dd);

    let audio_inv: Rc<RefCell<AudioInventory>> = Rc::new(RefCell::new(AudioInventory::default()));
    // Seed inventory once (best-effort); refresh via ListAudio after attach.
    if let Ok(inv) = hyprcap::audio::list_audio_inventory() {
        *audio_inv.borrow_mut() = inv;
    }
    populate_audio_dropdowns(
        &system_mode_dd,
        &system_detail_dd,
        &mic_dd,
        &audio_inv.borrow(),
        &sticky_cfg.borrow().audio_plan(),
    );

    // Rebuild detail list when system mode changes; toggle mic device sensitivity.
    {
        let system_detail_dd2 = system_detail_dd.clone();
        let mic_dd2 = mic_dd.clone();
        let mic_check2 = mic_check.clone();
        let audio_inv2 = Rc::clone(&audio_inv);
        let sticky_cfg2 = Rc::clone(&sticky_cfg);
        system_mode_dd.connect_selected_notify(move |dd| {
            let plan = sticky_cfg2.borrow().audio_plan();
            populate_audio_dropdowns(dd, &system_detail_dd2, &mic_dd2, &audio_inv2.borrow(), &plan);
            let mode = dd.selected();
            system_detail_dd2.set_sensitive(mode == 1 || mode == 2);
            mic_dd2.set_sensitive(mic_check2.is_active());
        });
        let mic_dd3 = mic_dd.clone();
        mic_check.connect_toggled(move |c| {
            mic_dd3.set_sensitive(c.is_active());
        });
    }

    let primary = Button::with_label("Record");
    primary.add_css_class("suggested-action");
    primary.add_css_class("pill");
    root.append(&primary);

    let state_label = Label::new(Some("State: Idle"));
    state_label.set_halign(Align::Start);
    state_label.add_css_class("title-4");
    root.append(&state_label);

    let timer_label = Label::new(Some("00:00"));
    timer_label.set_halign(Align::Start);
    timer_label.add_css_class("monospace");
    root.append(&timer_label);

    let path_label = Label::new(Some("Last file: —"));
    path_label.set_halign(Align::Start);
    path_label.set_wrap(true);
    path_label.set_xalign(0.0);
    path_label.set_selectable(true);
    root.append(&path_label);

    let msg_label = Label::new(None);
    msg_label.set_halign(Align::Start);
    msg_label.set_wrap(true);
    msg_label.set_xalign(0.0);
    msg_label.add_css_class("dim-label");
    root.append(&msg_label);

    let open_row = GtkBox::new(Orientation::Horizontal, 8);
    let open_file_btn = Button::with_label("Open file");
    let open_folder_btn = Button::with_label("Open folder");
    open_file_btn.set_hexpand(true);
    open_folder_btn.set_hexpand(true);
    open_file_btn.set_sensitive(false);
    open_folder_btn.set_sensitive(false);
    open_row.append(&open_file_btn);
    open_row.append(&open_folder_btn);
    root.append(&open_row);

    window.set_content(Some(&root));

    let view = Rc::new(RefCell::new(ViewState {
        last_status: None,
        last_path: None,
        start_in_flight: false,
        stop_in_flight: false,
        stopping_both: false,
        start_epoch: 0,
        audio_from_server: false,
        // Initial build already ran list_output_inventory + which(ffmpeg).
        last_both_revalidate: Some(Instant::now()),
    }));

    // Track timeout SourceIds so close can cancel them.
    let timeout_ids: Rc<RefCell<Vec<glib::SourceId>>> = Rc::new(RefCell::new(Vec::new()));

    // --- Mode toggles: pickers sensitive only in One monitor ---
    {
        let monitor_dd_a = monitor_dd.clone();
        let fps_dd_a = fps_dd.clone();
        let closed_a = Rc::clone(&closed);
        region_btn.connect_toggled(move |btn| {
            if closed_a.get() || !btn.is_active() {
                return;
            }
            // Region active → pickers off.
            monitor_dd_a.set_sensitive(false);
            fps_dd_a.set_sensitive(false);
        });
        let monitor_dd_b = monitor_dd.clone();
        let fps_dd_b = fps_dd.clone();
        let view_b = Rc::clone(&view);
        let closed_b = Rc::clone(&closed);
        full_btn.connect_toggled(move |btn| {
            if closed_b.get() || !btn.is_active() {
                return;
            }
            let idle = {
                let v = view_b.borrow();
                v.last_status
                    .as_ref()
                    .map(|s| s.state.as_str() == "Idle")
                    .unwrap_or(true)
                    && !v.start_in_flight
                    && !v.stop_in_flight
            };
            monitor_dd_b.set_sensitive(idle);
            fps_dd_b.set_sensitive(idle);
        });
        // Both: pickers always inactive (nothing to choose).
        // Revalidate gate when user activates Both (hotplug-friendly; not on status poll).
        let monitor_dd_c = monitor_dd.clone();
        let fps_dd_c = fps_dd.clone();
        let both_eligible_c = Rc::clone(&both_eligible);
        let view_c = Rc::clone(&view);
        let closed_c = Rc::clone(&closed);
        both_btn.connect_toggled(move |btn| {
            if closed_c.get() || !btn.is_active() {
                return;
            }
            monitor_dd_c.set_sensitive(false);
            fps_dd_c.set_sensitive(false);
            let gate = revalidate_both_gate(btn, &both_eligible_c);
            view_c.borrow_mut().last_both_revalidate = Some(Instant::now());
            let idle = {
                let v = view_c.borrow();
                v.last_status
                    .as_ref()
                    .map(|s| s.state.as_str() == "Idle")
                    .unwrap_or(true)
                    && !v.start_in_flight
                    && !v.stop_in_flight
            };
            btn.set_sensitive(idle && gate.enabled());
        });
    }

    // --- Monitor change: rebuild FPS list, persist config ---
    {
        let picker = Rc::clone(&picker);
        let fps_dd = fps_dd.clone();
        let msg_label = msg_label.clone();
        let closed = Rc::clone(&closed);
        monitor_dd.connect_selected_notify(move |dd| {
            if closed.get() {
                return;
            }
            let idx = dd.selected() as usize;
            let mut p = picker.borrow_mut();
            if p.suppress_notify {
                return;
            }
            if p.inventory.is_empty() || idx >= p.inventory.len() {
                return;
            }
            if p.selected_monitor == idx {
                return;
            }
            p.selected_monitor = idx;
            // Rebuild FPS for new monitor native; keep Auto or prior rate if still offered.
            let keep_auto = p
                .fps_entries
                .get(p.selected_fps)
                .map(|e| e.fps.is_none())
                .unwrap_or(false);
            let prev_fps = p.selected_fps_value();
            p.rebuild_fps_entries(prev_fps, keep_auto);
            let labels: Vec<String> = p.fps_entries.iter().map(|e| e.label.clone()).collect();
            let sel = p.selected_fps as u32;
            let out_name = p.selected_output_name().map(|s| s.to_string());
            let fps_val = p.selected_fps_value();
            drop(p);

            // Update FPS dropdown model without re-entrant persist storms.
            {
                let mut p = picker.borrow_mut();
                p.suppress_notify = true;
            }
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let model = StringList::new(&refs);
            fps_dd.set_model(Some(&model));
            fps_dd.set_selected(sel);
            picker.borrow_mut().suppress_notify = false;

            if let Err(e) = persist_one_pickers(out_name.as_deref(), fps_val) {
                msg_label.set_text(&e);
            }
        });
    }

    // --- FPS change: persist config ---
    {
        let picker = Rc::clone(&picker);
        let msg_label = msg_label.clone();
        let closed = Rc::clone(&closed);
        fps_dd.connect_selected_notify(move |dd| {
            if closed.get() {
                return;
            }
            let idx = dd.selected() as usize;
            let mut p = picker.borrow_mut();
            if p.suppress_notify {
                return;
            }
            if p.fps_entries.is_empty() || idx >= p.fps_entries.len() {
                return;
            }
            if p.selected_fps == idx {
                return;
            }
            p.selected_fps = idx;
            // Only pass a real output name; empty inventory must not clear the pin.
            let out_name = p.selected_output_name().map(|s| s.to_string());
            let fps_val = p.selected_fps_value();
            drop(p);
            if let Err(e) = persist_one_pickers(out_name.as_deref(), fps_val) {
                msg_label.set_text(&e);
            }
        });
    }

    // --- Primary Record / Stop ---
    {
        let worker = Rc::clone(&worker);
        let region_btn = region_btn.clone();
        let full_btn = full_btn.clone();
        let both_btn = both_btn.clone();
        let monitor_dd = monitor_dd.clone();
        let fps_dd = fps_dd.clone();
        let system_mode_dd = system_mode_dd.clone();
        let system_detail_dd = system_detail_dd.clone();
        let mic_check = mic_check.clone();
        let mic_dd = mic_dd.clone();
        let audio_inv = Rc::clone(&audio_inv);
        let sticky_cfg = Rc::clone(&sticky_cfg);
        let view = Rc::clone(&view);
        let picker = Rc::clone(&picker);
        let msg_label = msg_label.clone();
        let primary_btn = primary.clone();
        let window_weak = window.downgrade();
        let closed = Rc::clone(&closed);
        let start_epoch_counter = Rc::clone(&start_epoch_counter);
        let both_eligible = Rc::clone(&both_eligible);
        primary.connect_clicked(move |_| {
            if closed.get() {
                return;
            }
            let worker_ref = worker.borrow();
            let Some(w) = worker_ref.as_ref() else {
                msg_label.set_text("Session worker unavailable");
                return;
            };

            let both_mode = both_btn.is_active();
            let (session_busy, start_in_flight, stop_in_flight, was_both) = {
                let v = view.borrow();
                let busy = v
                    .last_status
                    .as_ref()
                    .map(|s| s.state.as_str() != "Idle")
                    .unwrap_or(false)
                    || v.start_in_flight;
                // capture_mode may still be unset while Both start RPC is in flight.
                let was_both = v
                    .last_status
                    .as_ref()
                    .and_then(|s| s.capture_mode.as_deref())
                    == Some("both")
                    || (v.start_in_flight && both_mode);
                (busy, v.start_in_flight, v.stop_in_flight, was_both)
            };

            // Stop path: allowed while SelectingRegion/Recording/start in flight.
            if session_busy {
                if stop_in_flight {
                    return;
                }
                match w.tx.send(WorkerCmd::Stop) {
                    Ok(()) => {
                        let mut v = view.borrow_mut();
                        v.stop_in_flight = true;
                        v.stopping_both = was_both;
                        // Only invalidate start when a start RPC is actually in flight
                        // (avoid killing a future Start OpDone on idle no-op stop races).
                        if v.start_in_flight {
                            let next = start_epoch_counter.fetch_add(1, Ordering::SeqCst) + 1;
                            v.start_epoch = next;
                        }
                        // Keep start_in_flight until Stop OpDone/Error so label stays Stop;
                        // late Start is ignored via epoch when we bumped it.
                        drop(v);
                        // Both stop blocks on layout-true ffmpeg stitch — be honest.
                        msg_label.set_text(if was_both {
                            "Stopping… Composing…"
                        } else {
                            "Stopping…"
                        });
                        primary_btn.set_sensitive(false);
                    }
                    Err(_) => {
                        msg_label.set_text("Worker unavailable");
                    }
                }
                return;
            }

            // Start path: only from Idle, not already starting.
            if start_in_flight {
                return;
            }
            let plan = plan_from_audio_widgets(
                &system_mode_dd,
                &system_detail_dd,
                &mic_check,
                &mic_dd,
                &audio_inv.borrow(),
            );
            if plan.system == SystemAudioMode::App && plan.app.is_none() {
                msg_label.set_text("No app selected — start playback or pick All PC sound");
                return;
            }
            // Sticky config for next session.
            {
                let mut cfg = sticky_cfg.borrow_mut();
                cfg.apply_audio_plan(&plan);
                let paths = EnvPaths::from_env();
                let _ = cfg.save(&paths);
            }
            let region = region_btn.is_active();
            let one = full_btn.is_active();
            let both = both_mode;
            if !region && !one && !both {
                msg_label.set_text("Select Region, One monitor, or 2 monitors");
                return;
            }
            if both {
                // Live revalidate (hotplug / PATH): required false-positive guard before StartBoth.
                let gate = revalidate_both_gate(&both_btn, &both_eligible);
                view.borrow_mut().last_both_revalidate = Some(Instant::now());
                if !gate.enabled() {
                    both_btn.set_sensitive(false);
                    msg_label.set_text(gate.tooltip());
                    return;
                }
            }
            if one && picker.borrow().inventory.is_empty() {
                msg_label.set_text("No outputs discovered (hyprctl monitors / wf-recorder -L)");
                return;
            }
            let epoch = start_epoch_counter.fetch_add(1, Ordering::SeqCst) + 1;
            let cmd = if region {
                WorkerCmd::StartRegion {
                    plan: plan.clone(),
                    epoch,
                }
            } else if both {
                WorkerCmd::StartBoth {
                    plan: plan.clone(),
                    epoch,
                }
            } else {
                let p = picker.borrow();
                let output = p.selected_output_name().map(|s| s.to_string());
                // Explicit fps for this start: Some(0) = Auto so config cannot override.
                let fps = ipc_fps_for_start(p.selected_fps_value());
                WorkerCmd::StartFullscreen {
                    plan: plan.clone(),
                    epoch,
                    output,
                    fps,
                }
            };
            match w.tx.send(cmd) {
                Ok(()) => {
                    {
                        let mut v = view.borrow_mut();
                        v.start_in_flight = true;
                        v.start_epoch = epoch;
                    }
                    // Immediately freeze mode/pickers (don't wait for status poll).
                    region_btn.set_sensitive(false);
                    full_btn.set_sensitive(false);
                    both_btn.set_sensitive(false);
                    monitor_dd.set_sensitive(false);
                    fps_dd.set_sensitive(false);
                    system_mode_dd.set_sensitive(false);
                    system_detail_dd.set_sensitive(false);
                    mic_check.set_sensitive(false);
                    mic_dd.set_sensitive(false);
                    if region {
                        msg_label.set_text("Select a region…");
                        // Best-effort: get out of the way of slurp focus.
                        if let Some(win) = window_weak.upgrade() {
                            win.minimize();
                        }
                    } else if both {
                        // Neutral copy: inventory list order ≠ server primary (min x,y).
                        msg_label.set_text("Starting… 2 monitors");
                    } else {
                        let name = picker
                            .borrow()
                            .selected_output_name()
                            .unwrap_or("?")
                            .to_string();
                        msg_label.set_text(&format!("Starting… Output: {name}"));
                    }
                    // Keep Stop available during SelectingRegion (start_in_flight).
                    refresh_primary_from_view(&view, &primary_btn);
                }
                Err(_) => {
                    msg_label.set_text("Worker unavailable");
                }
            }
        });
    }

    // --- Open file / folder ---
    {
        let view = Rc::clone(&view);
        let msg_label = msg_label.clone();
        let closed = Rc::clone(&closed);
        open_file_btn.connect_clicked(move |_| {
            if closed.get() {
                return;
            }
            if let Some(p) = view.borrow().last_path.clone() {
                if let Err(e) = xdg_open(Path::new(&p)) {
                    msg_label.set_text(&format!("Could not open file: {e}"));
                }
            }
        });
    }
    {
        let view = Rc::clone(&view);
        let msg_label = msg_label.clone();
        let closed = Rc::clone(&closed);
        open_folder_btn.connect_clicked(move |_| {
            if closed.get() {
                return;
            }
            if let Some(p) = view.borrow().last_path.clone() {
                let parent = Path::new(&p)
                    .parent()
                    .map(|d| d.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                if let Err(e) = xdg_open(&parent) {
                    msg_label.set_text(&format!("Could not open folder: {e}"));
                }
            }
        });
    }

    // --- Status poll (coalesced) ---
    {
        let worker = Rc::clone(&worker);
        let closed = Rc::clone(&closed);
        let id = glib::timeout_add_local(Duration::from_millis(350), move || {
            if closed.get() {
                return glib::ControlFlow::Break;
            }
            if let Some(w) = worker.borrow().as_ref() {
                // Only enqueue if no poll already waiting in the worker.
                if w.poll_pending
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                    && w.tx.send(WorkerCmd::PollStatus).is_err()
                {
                    w.poll_pending.store(false, Ordering::SeqCst);
                }
            }
            glib::ControlFlow::Continue
        });
        timeout_ids.borrow_mut().push(id);
    }

    // --- Smooth timer: started_at_unix + wall clock ---
    // Freeze while stop_in_flight / Stopping (Both compose can take seconds while
    // last_status is still "Recording" because stop blocks the accept loop).
    {
        let view = Rc::clone(&view);
        let timer_label = timer_label.clone();
        let closed = Rc::clone(&closed);
        let id = glib::timeout_add_local(Duration::from_millis(200), move || {
            if closed.get() {
                return glib::ControlFlow::Break;
            }
            let v = view.borrow();
            if let Some(ref st) = v.last_status {
                if timer_should_tick(&st.state, v.stop_in_flight) {
                    timer_label.set_text(&format_elapsed(st));
                }
            }
            glib::ControlFlow::Continue
        });
        timeout_ids.borrow_mut().push(id);
    }

    // --- Drain worker → UI ---
    {
        let view = Rc::clone(&view);
        let closed = Rc::clone(&closed);
        let ui_rx = Rc::new(RefCell::new(ui_rx));
        let window_weak = window.downgrade();
        let widgets = UiWidgets {
            state_label,
            timer_label,
            path_label,
            msg_label: msg_label.clone(),
            primary: primary.clone(),
            open_file_btn,
            open_folder_btn,
            region_btn,
            full_btn,
            both_btn,
            both_eligible: Rc::clone(&both_eligible),
            monitor_dd,
            fps_dd,
            system_mode_dd: system_mode_dd.clone(),
            system_detail_dd: system_detail_dd.clone(),
            mic_check: mic_check.clone(),
            mic_dd: mic_dd.clone(),
            audio_inv: Rc::clone(&audio_inv),
            sticky_cfg: Rc::clone(&sticky_cfg),
            window: window_weak,
        };
        // Refresh sinks/apps/mics from session server.
        if let Some(ref wh) = *worker.borrow() {
            let _ = wh.tx.send(WorkerCmd::ListAudio);
        }
        let id = glib::timeout_add_local(Duration::from_millis(50), move || {
            if closed.get() {
                return glib::ControlFlow::Break;
            }
            let mut msgs = Vec::new();
            {
                let rx = ui_rx.borrow();
                loop {
                    match rx.try_recv() {
                        Ok(m) => msgs.push(m),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            return glib::ControlFlow::Break;
                        }
                    }
                }
            }
            for msg in msgs {
                if closed.get() {
                    break;
                }
                apply_msg(msg, &view, &widgets, &closed);
            }
            glib::ControlFlow::Continue
        });
        timeout_ids.borrow_mut().push(id);
    }

    // --- Close: disconnect view only ---
    {
        let worker = Rc::clone(&worker);
        let closed = Rc::clone(&closed);
        let timeout_ids = Rc::clone(&timeout_ids);
        let window_slot = Rc::clone(&window_slot);
        window.connect_close_request(move |_win| {
            closed.set(true);
            // Clear slot so re-activate builds a fresh window with a live worker.
            window_slot.borrow_mut().take();
            // Cancel glib timeouts so we never touch disposed widgets.
            for id in timeout_ids.borrow_mut().drain(..) {
                id.remove();
            }
            if let Some(mut w) = worker.borrow_mut().take() {
                let _ = w.tx.send(WorkerCmd::ShutdownWorker);
                if let Some(j) = w.join.take() {
                    thread::spawn(move || {
                        let _ = j.join();
                    });
                }
            }
            glib::Propagation::Proceed
        });
    }

    // First status kick (coalesced path).
    if let Some(w) = worker.borrow().as_ref() {
        if w.poll_pending
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
            && w.tx.send(WorkerCmd::PollStatus).is_err()
        {
            w.poll_pending.store(false, Ordering::SeqCst);
        }
    }

    window.present();
    window
}

struct UiWidgets {
    state_label: Label,
    timer_label: Label,
    path_label: Label,
    msg_label: Label,
    primary: Button,
    open_file_btn: Button,
    open_folder_btn: Button,
    region_btn: ToggleButton,
    full_btn: ToggleButton,
    both_btn: ToggleButton,
    /// Live gate: inventory_len==2 && ffmpeg on PATH (revalidated idle / Both Record).
    both_eligible: Rc<Cell<bool>>,
    monitor_dd: DropDown,
    fps_dd: DropDown,
    system_mode_dd: DropDown,
    system_detail_dd: DropDown,
    mic_check: CheckButton,
    mic_dd: DropDown,
    audio_inv: Rc<RefCell<AudioInventory>>,
    sticky_cfg: Rc<RefCell<Config>>,
    window: glib::object::WeakRef<adw::ApplicationWindow>,
}

fn apply_msg(msg: UiMsg, view: &Rc<RefCell<ViewState>>, w: &UiWidgets, closed: &Rc<Cell<bool>>) {
    if closed.get() {
        return;
    }
    match msg {
        UiMsg::Subscribed(status) => {
            // Fresh attach / reconnect: re-sync audio from session status.
            view.borrow_mut().audio_from_server = false;
            // Surface last failure from a previous session (if any).
            let note = status.last_error.clone();
            apply_status(status, view, w, note.as_deref());
        }
        UiMsg::AudioList(resp) => {
            if let Some(inv) = resp.audio_list {
                *w.audio_inv.borrow_mut() = inv;
            } else if !resp.ok {
                w.msg_label
                    .set_text(&format!("Audio devices: {}", resp.message));
            }
            let plan = w.sticky_cfg.borrow().audio_plan();
            populate_audio_dropdowns(
                &w.system_mode_dd,
                &w.system_detail_dd,
                &w.mic_dd,
                &w.audio_inv.borrow(),
                &plan,
            );
            let idle = {
                let v = view.borrow();
                v.last_status
                    .as_ref()
                    .map(|s| s.state == "Idle")
                    .unwrap_or(true)
                    && !v.start_in_flight
                    && !v.stop_in_flight
            };
            set_audio_controls_sensitive(w, idle);
        }
        UiMsg::Status(status) => {
            // Unexpected wf-recorder death is only reaped server-side; status polls
            // must surface last_error so the label is not stuck on "Recording…".
            let prev_busy = view
                .borrow()
                .last_status
                .as_ref()
                .map(|s| s.state != "Idle")
                .unwrap_or(false);
            let note = if status.state == "Idle" && prev_busy {
                status.last_error.clone()
            } else {
                None
            };
            apply_status(status, view, w, note.as_deref());
        }
        UiMsg::Info(message) => {
            w.msg_label.set_text(&message);
        }
        UiMsg::OpDone { kind, resp, epoch } => {
            match kind {
                OpKind::Start => {
                    let mut v = view.borrow_mut();
                    // Ignore superseded start completions (stop / newer start).
                    if epoch != v.start_epoch {
                        return;
                    }
                    v.start_in_flight = false;
                    drop(v);
                    // Restore window after region selection / start completes.
                    if let Some(win) = w.window.upgrade() {
                        win.present();
                    }
                    let note = format_op_message(kind, &resp);
                    apply_status(resp.status.clone(), view, w, Some(note.as_str()));
                    append_warnings(w, &resp);
                }
                OpKind::Stop => {
                    {
                        let mut v = view.borrow_mut();
                        v.stop_in_flight = false;
                        v.stopping_both = false;
                        // Stop supersedes any in-flight start UI flag.
                        v.start_in_flight = false;
                    }
                    if let Some(win) = w.window.upgrade() {
                        win.present();
                    }
                    let note = format_op_message(kind, &resp);
                    apply_status(resp.status.clone(), view, w, Some(note.as_str()));
                    append_warnings(w, &resp);
                }
            }
        }
        UiMsg::Error {
            source,
            message,
            epoch,
        } => {
            {
                let mut v = view.borrow_mut();
                match source {
                    ErrorSource::Start => {
                        if epoch == v.start_epoch {
                            v.start_in_flight = false;
                        }
                    }
                    ErrorSource::Stop => {
                        // Recoverable: stop failed — clear both flags so Record/Stop
                        // is not stuck (start may have been invalidated by epoch bump).
                        v.stop_in_flight = false;
                        v.stopping_both = false;
                        v.start_in_flight = false;
                    }
                    ErrorSource::Status => {
                        // Do not clear start/stop flags on poll errors.
                    }
                }
            }
            if source != ErrorSource::Status {
                if let Some(win) = w.window.upgrade() {
                    win.present();
                }
            }
            w.msg_label.set_text(&message);
            refresh_primary_from_view(view, &w.primary);
        }
        UiMsg::SubscribeFailed(e) => {
            w.msg_label.set_text(&e);
            eprintln!("hyprcap: {e}");
            // Hard-fail the view: close without stopping recording.
            if let Some(win) = w.window.upgrade() {
                win.close();
            }
        }
    }
}

fn append_warnings(w: &UiWidgets, resp: &IpcResponse) {
    if resp.warnings.is_empty() {
        return;
    }
    let warn = resp.warnings.join("; ");
    let cur = w.msg_label.text();
    if cur.is_empty() {
        w.msg_label.set_text(&format!("warning: {warn}"));
    } else {
        w.msg_label.set_text(&format!("{cur}  (warning: {warn})"));
    }
}

fn format_op_message(kind: OpKind, resp: &IpcResponse) -> String {
    if resp.ok {
        match kind {
            OpKind::Start => {
                if resp.code == "slurp_cancel" {
                    "Region selection canceled".into()
                } else if resp.status.state == "Recording" {
                    if let Some(msg) = format_capture_message(&resp.status) {
                        msg
                    } else if !resp.message.is_empty()
                        && resp.message.to_lowercase().contains("recording")
                    {
                        resp.message.clone()
                    } else {
                        "Recording…".into()
                    }
                } else {
                    resp.message.clone()
                }
            }
            OpKind::Stop => {
                // Path-oriented microcopy. Server skips clipboard silently when
                // copy_path=false (no warning), so GUI must not claim "path copied"
                // without a positive signal. Prefer "path ready" always.
                if let Some(ref p) = resp.status.last_success_path {
                    if resp.warnings.is_empty() {
                        format!("Saved — path ready: {p}")
                    } else {
                        format!("Saved: {p}")
                    }
                } else if !resp.message.is_empty() {
                    resp.message.clone()
                } else {
                    "Stopped".into()
                }
            }
        }
    } else if !resp.message.is_empty() {
        resp.message.clone()
    } else {
        format!("Error ({})", resp.code)
    }
}

/// Status line for active capture: `Output: NAME` (One) or `2 monitors: A + B`.
///
/// `capture_output` for dual mode is stored as `A+B` (server); pretty-print with spaces.
/// Mode branch is the product contract (`both` vs other); `+` is cosmetic only.
fn format_capture_message(status: &IpcStatus) -> Option<String> {
    let o = status.capture_output.as_ref()?;
    if status.capture_mode.as_deref() == Some("both") {
        let pretty = o.replace('+', " + ");
        Some(format!("2 monitors: {pretty}"))
    } else {
        // Region normally has no capture_output; if present, still show Output: …
        Some(format!("Output: {o}"))
    }
}

/// Stopping / compose honesty while stop RPC or server Stopping is active.
///
/// `stopping_both` covers start_in_flight Both stops before `capture_mode` is known.
fn format_stopping_message(status: &IpcStatus, stopping_both: bool) -> String {
    if stopping_both || status.capture_mode.as_deref() == Some("both") {
        "Stopping… Composing…".into()
    } else {
        "Stopping…".into()
    }
}

/// Pure message-line policy for [`apply_status`] (unit-tested).
///
/// Branch order:
/// 1. Explicit `message` wins (op notes / errors).
/// 2. Stopping or stop_in_flight → stopping / compose honesty (never Both capture label).
/// 3. Recording / Starting → capture label when present.
/// 4. Else leave unchanged (`None`).
fn status_message_line(
    message: Option<&str>,
    status: &IpcStatus,
    stop_in_flight: bool,
    stopping_both: bool,
) -> Option<String> {
    if let Some(m) = message {
        return Some(m.to_string());
    }
    if status.state == "Stopping" || stop_in_flight {
        return Some(format_stopping_message(status, stopping_both));
    }
    if matches!(status.state.as_str(), "Recording" | "Starting") {
        return format_capture_message(status);
    }
    None
}

fn apply_status(
    status: IpcStatus,
    view: &Rc<RefCell<ViewState>>,
    w: &UiWidgets,
    message: Option<&str>,
) {
    // Capture previous server state before overwrite (edge-trigger into Idle).
    let prev_state = view.borrow().last_status.as_ref().map(|s| s.state.clone());

    {
        let mut v = view.borrow_mut();
        if let Some(ref p) = status.last_success_path {
            v.last_path = Some(p.clone());
        }
        // First attach: keep GUI sticky config; do not force-toggle from bool status.audio.
        if !v.audio_from_server {
            v.audio_from_server = true;
        }
        v.last_status = Some(status.clone());
    }

    w.state_label.set_text(&format!("State: {}", status.state));

    let stop_in_flight = view.borrow().stop_in_flight;
    match status.state.as_str() {
        "Recording" if timer_should_tick("Recording", stop_in_flight) => {
            w.timer_label.set_text(&format_elapsed(&status));
        }
        "Idle" | "SelectingRegion" => {
            w.timer_label.set_text("00:00");
        }
        // Freeze display during stop / compose (and while stop RPC is in flight).
        "Starting" | "Stopping" | "Recording" => {}
        _ => {}
    }

    let last = view.borrow().last_path.clone();
    if let Some(ref p) = last {
        w.path_label.set_text(&format!("Last file: {p}"));
        w.open_file_btn.set_sensitive(true);
        w.open_folder_btn.set_sensitive(true);
    }

    let (stop_in_flight, stopping_both) = {
        let v = view.borrow();
        (v.stop_in_flight, v.stopping_both)
    };
    if let Some(msg) = status_message_line(message, &status, stop_in_flight, stopping_both) {
        w.msg_label.set_text(&msg);
    }

    let idle = {
        let v = view.borrow();
        status.state == "Idle" && !v.start_in_flight && !v.stop_in_flight
    };
    w.region_btn.set_sensitive(idle);
    w.full_btn.set_sensitive(idle);
    // Both chrome: never spawn hyprctl/ffmpeg on the 350ms poll path.
    // Live revalidate only via should_revalidate_both_on_idle (edge + throttle).
    // Record path always revalidates before StartBoth; Both toggle also revalidates.
    if idle {
        let elapsed = view.borrow().last_both_revalidate.map(|t| t.elapsed());
        if should_revalidate_both_on_idle(prev_state.as_deref(), elapsed, BOTH_REVALIDATE_IDLE_MIN)
        {
            revalidate_both_gate(&w.both_btn, &w.both_eligible);
            view.borrow_mut().last_both_revalidate = Some(Instant::now());
        }
        w.both_btn.set_sensitive(w.both_eligible.get());
    } else {
        w.both_btn.set_sensitive(false);
    }
    set_audio_controls_sensitive(w, idle);
    // Monitor / FPS lists only when mode = One monitor (not Region / Both).
    let one = w.full_btn.is_active();
    w.monitor_dd.set_sensitive(idle && one);
    w.fps_dd.set_sensitive(idle && one);

    refresh_primary_from_view(view, &w.primary);
}

fn set_audio_controls_sensitive(w: &UiWidgets, idle: bool) {
    w.system_mode_dd.set_sensitive(idle);
    let mode = w.system_mode_dd.selected();
    w.system_detail_dd
        .set_sensitive(idle && (mode == 1 || mode == 2));
    w.mic_check.set_sensitive(idle);
    w.mic_dd.set_sensitive(idle && w.mic_check.is_active());
}

/// Rebuild detail / mic dropdown models from inventory + preferred plan.
fn populate_audio_dropdowns(
    system_mode_dd: &DropDown,
    system_detail_dd: &DropDown,
    mic_dd: &DropDown,
    inv: &AudioInventory,
    plan: &AudioPlan,
) {
    let mode = system_mode_dd.selected();
    // Detail list: sinks (All) or apps (App) or placeholder (Off).
    let detail_labels: Vec<String> = match mode {
        1 => {
            let mut v = vec!["Default output".to_string()];
            for s in &inv.sinks {
                let mark = if s.is_default { " ★" } else { "" };
                v.push(format!("{}{mark}", s.description));
            }
            if inv.sinks.is_empty() {
                v = vec!["Default output".into()];
            }
            v
        }
        2 => {
            if inv.apps.is_empty() {
                vec!["(no apps playing)".into()]
            } else {
                inv.apps
                    .iter()
                    .map(|a| {
                        if let Some(ref m) = a.media_name {
                            format!("{} — {m}", a.name)
                        } else {
                            a.name.clone()
                        }
                    })
                    .collect()
            }
        }
        _ => vec!["—".into()],
    };
    let detail_list = StringList::new(&detail_labels.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    system_detail_dd.set_model(Some(&detail_list));
    // Restore selection preference.
    let sel = match mode {
        1 => {
            if let Some(ref want) = plan.sink {
                inv.sinks
                    .iter()
                    .position(|s| &s.name == want)
                    .map(|i| (i + 1) as u32) // +1 for Default row
                    .unwrap_or(0)
            } else {
                0
            }
        }
        2 => {
            if let Some(ref want) = plan.app {
                inv.apps
                    .iter()
                    .position(|a| a.name.eq_ignore_ascii_case(want))
                    .unwrap_or(0) as u32
            } else {
                0
            }
        }
        _ => 0,
    };
    if !detail_labels.is_empty() {
        system_detail_dd.set_selected(sel.min((detail_labels.len() - 1) as u32));
    }
    system_detail_dd.set_sensitive(mode == 1 || mode == 2);

    let mic_labels: Vec<String> = {
        let mics: Vec<_> = inv.mics().collect();
        if mics.is_empty() {
            vec!["Default mic".into()]
        } else {
            let mut v = vec!["Default mic".to_string()];
            for m in mics {
                let mark = if m.is_default { " ★" } else { "" };
                v.push(format!("{}{mark}", m.description));
            }
            v
        }
    };
    let mic_list = StringList::new(&mic_labels.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    mic_dd.set_model(Some(&mic_list));
    let mic_sel = if let Some(ref want) = plan.mic_device {
        inv.mics()
            .position(|m| &m.name == want)
            .map(|i| (i + 1) as u32)
            .unwrap_or(0)
    } else {
        0
    };
    if !mic_labels.is_empty() {
        mic_dd.set_selected(mic_sel.min((mic_labels.len() - 1) as u32));
    }
}

fn plan_from_audio_widgets(
    system_mode_dd: &DropDown,
    system_detail_dd: &DropDown,
    mic_check: &CheckButton,
    mic_dd: &DropDown,
    inv: &AudioInventory,
) -> AudioPlan {
    let system = match system_mode_dd.selected() {
        1 => SystemAudioMode::All,
        2 => SystemAudioMode::App,
        _ => SystemAudioMode::Off,
    };
    let mut sink = None;
    let mut app = None;
    match system {
        SystemAudioMode::All => {
            let idx = system_detail_dd.selected() as usize;
            if idx >= 1 {
                if let Some(s) = inv.sinks.get(idx - 1) {
                    sink = Some(s.name.clone());
                }
            }
        }
        SystemAudioMode::App => {
            let idx = system_detail_dd.selected() as usize;
            if let Some(a) = inv.apps.get(idx) {
                app = Some(a.name.clone());
            }
        }
        SystemAudioMode::Off => {}
    }
    let mic = mic_check.is_active();
    let mic_device = if mic {
        let idx = mic_dd.selected() as usize;
        if idx >= 1 {
            inv.mics().nth(idx - 1).map(|m| m.name.clone())
        } else {
            None
        }
    } else {
        None
    };
    AudioPlan {
        system,
        sink,
        app,
        mic,
        mic_device,
    }
    .normalized()
}

fn refresh_primary_from_view(view: &Rc<RefCell<ViewState>>, primary: &Button) {
    let (state, start_in_flight, stop_in_flight) = {
        let v = view.borrow();
        let state = v
            .last_status
            .as_ref()
            .map(|s| s.state.clone())
            .unwrap_or_else(|| "Idle".to_string());
        (state, v.start_in_flight, v.stop_in_flight)
    };

    // During SelectingRegion / start_in_flight, Stop stays available.
    let show_stop = start_in_flight
        || matches!(
            state.as_str(),
            "Recording" | "Starting" | "SelectingRegion" | "Stopping"
        );

    if show_stop {
        primary.set_label("Stop");
        primary.remove_css_class("suggested-action");
        primary.add_css_class("destructive-action");
        primary.set_sensitive(!stop_in_flight && state != "Stopping");
    } else {
        primary.set_label("Record");
        primary.remove_css_class("destructive-action");
        primary.add_css_class("suggested-action");
        primary.set_sensitive(!start_in_flight && !stop_in_flight);
    }
}

/// Whether the smooth timer may advance. Frozen during stop/compose so Both
/// stitch time is not counted as recording.
fn timer_should_tick(state: &str, stop_in_flight: bool) -> bool {
    state == "Recording" && !stop_in_flight
}

/// Smooth elapsed: prefer `started_at_unix` + wall clock so ticks advance between polls.
fn format_elapsed(st: &IpcStatus) -> String {
    let ms = if let Some(start) = st.started_at_unix {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        now_ms.saturating_sub(start.saturating_mul(1000))
    } else {
        st.elapsed_ms.unwrap_or_default()
    };
    let total_secs = ms / 1000;
    let m = total_secs / 60;
    let s = total_secs % 60;
    format!("{m:02}:{s:02}")
}

/// Soft-fail `xdg-open`. Reaps the child so we never leave zombies.
fn xdg_open(path: &Path) -> Result<(), String> {
    match std::process::Command::new("xdg-open")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(())
        }
        Err(e) => Err(format!("{e}")),
    }
}

// ---------------------------------------------------------------------------
// One-monitor pickers (pure helpers + state)
// ---------------------------------------------------------------------------

/// One FPS combo entry: `fps` is `None` for Auto, else the integer for `-r`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FpsEntry {
    label: String,
    fps: Option<u32>,
    is_native: bool,
}

/// GUI session state for monitor + FPS combos.
struct PickerState {
    inventory: Vec<OutputInfo>,
    fps_entries: Vec<FpsEntry>,
    selected_monitor: usize,
    selected_fps: usize,
    /// Suppress DropDown notify while programmatically rebuilding models.
    suppress_notify: bool,
}

impl PickerState {
    fn from_inventory_and_config(inventory: Vec<OutputInfo>, cfg: &Config) -> Self {
        let selected_monitor = select_monitor_index(&inventory, cfg.fullscreen_output.as_deref());
        let native = inventory
            .get(selected_monitor)
            .and_then(|o| native_fps_hz(o.refresh));
        // one_fps: Some(0) = sticky Auto; Some(n>0) = rate (+ include in list);
        // None = first-run GUI default → native.
        let force_auto = matches!(cfg.one_fps, Some(0));
        let extra = cfg.one_fps.filter(|&n| n > 0);
        let fps_entries = build_fps_entries(native, extra);
        let selected_fps = if force_auto {
            select_fps_index(&fps_entries, None, false)
        } else {
            select_fps_index(&fps_entries, extra, true)
        };
        Self {
            inventory,
            fps_entries,
            selected_monitor,
            selected_fps,
            suppress_notify: false,
        }
    }

    fn selected_output_name(&self) -> Option<&str> {
        self.inventory
            .get(self.selected_monitor)
            .map(|o| o.name.as_str())
    }

    /// `None` = Auto (no `-r`).
    fn selected_fps_value(&self) -> Option<u32> {
        self.fps_entries.get(self.selected_fps).and_then(|e| e.fps)
    }

    fn rebuild_fps_entries(&mut self, prefer: Option<u32>, keep_auto: bool) {
        let native = self
            .inventory
            .get(self.selected_monitor)
            .and_then(|o| native_fps_hz(o.refresh));
        // Keep a non-standard rate (e.g. 120) visible after monitor switch.
        self.fps_entries = build_fps_entries(native, prefer);
        // Keep Auto if user had Auto; else prior rate if offered; else native.
        self.selected_fps = if keep_auto {
            select_fps_index(&self.fps_entries, None, false)
        } else {
            select_fps_index(&self.fps_entries, prefer, true)
        };
    }
}

/// Combo label: `HDMI-A-1 · 2560×1440` when geometry known, else name only.
fn monitor_combo_label(o: &OutputInfo) -> String {
    match (o.width, o.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => format!("{} · {}×{}", o.name, w, h),
        _ => o.name.clone(),
    }
}

/// Integer Hz for native FPS picker (rounded). `None` when refresh unknown.
fn native_fps_hz(refresh: Option<f64>) -> Option<u32> {
    refresh
        .filter(|r| r.is_finite() && *r > 0.0)
        .map(|r| r.round() as u32)
        .filter(|&n| n > 0)
}

/// Largest-area monitor index; tie-break by name ascending. Empty → 0.
fn default_monitor_index(inventory: &[OutputInfo]) -> usize {
    if inventory.is_empty() {
        return 0;
    }
    let area = |o: &OutputInfo| -> i64 {
        let w = o.width.unwrap_or(0).max(0) as i64;
        let h = o.height.unwrap_or(0).max(0) as i64;
        w * h
    };
    let mut order: Vec<usize> = (0..inventory.len()).collect();
    order.sort_by(|&i, &j| {
        area(&inventory[j])
            .cmp(&area(&inventory[i]))
            .then_with(|| inventory[i].name.cmp(&inventory[j].name))
    });
    order[0]
}

/// Prefer config pin when still in inventory; else largest-area default.
fn select_monitor_index(inventory: &[OutputInfo], config_pin: Option<&str>) -> usize {
    if inventory.is_empty() {
        return 0;
    }
    if let Some(pin) = config_pin.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(i) = inventory.iter().position(|o| o.name == pin) {
            return i;
        }
    }
    default_monitor_index(inventory)
}

/// FPS list: Auto | native | optional extra pin | 60 | 30 with equal-value dedupe.
///
/// Order: Auto, `{N} (native)`, extra (e.g. CLI 120), 60, 30 — skip duplicates.
fn build_fps_entries(native: Option<u32>, extra: Option<u32>) -> Vec<FpsEntry> {
    let mut out = Vec::with_capacity(5);
    out.push(FpsEntry {
        label: "Auto".into(),
        fps: None,
        is_native: false,
    });
    let mut seen = std::collections::HashSet::new();
    if let Some(n) = native {
        out.push(FpsEntry {
            label: format!("{n} (native)"),
            fps: Some(n),
            is_native: true,
        });
        seen.insert(n);
    }
    if let Some(n) = extra.filter(|&n| n > 0) {
        if seen.insert(n) {
            out.push(FpsEntry {
                label: n.to_string(),
                fps: Some(n),
                is_native: false,
            });
        }
    }
    for n in [60u32, 30] {
        if seen.insert(n) {
            out.push(FpsEntry {
                label: n.to_string(),
                fps: Some(n),
                is_native: false,
            });
        }
    }
    out
}

/// Index of preferred FPS. `prefer` is the rate to select when present.
/// When `prefer` is None and `prefer_native_if_unset`, select native entry if any,
/// else Auto (index 0).
fn select_fps_index(
    entries: &[FpsEntry],
    prefer: Option<u32>,
    prefer_native_if_unset: bool,
) -> usize {
    if entries.is_empty() {
        return 0;
    }
    if let Some(n) = prefer {
        if let Some(i) = entries.iter().position(|e| e.fps == Some(n)) {
            return i;
        }
    }
    if prefer_native_if_unset {
        if let Some(i) = entries.iter().position(|e| e.is_native) {
            return i;
        }
    }
    // Auto
    entries.iter().position(|e| e.fps.is_none()).unwrap_or(0)
}

/// Map GUI FPS selection to IPC `fps` for start_fullscreen.
///
/// Auto (`None`) → `Some(0)` so server config `one_fps` cannot override this start.
/// Rate `n` → `Some(n)`.
fn ipc_fps_for_start(selected: Option<u32>) -> Option<u32> {
    Some(selected.unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Both enablement (pure; DUAL-MONITOR §5.2)
// ---------------------------------------------------------------------------

/// GUI Both-mode gate result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BothEnablement {
    Enabled,
    /// Tooltip / message when Both must stay disabled.
    Disabled {
        reason: &'static str,
    },
}

impl BothEnablement {
    fn enabled(self) -> bool {
        matches!(self, BothEnablement::Enabled)
    }

    fn tooltip(self) -> &'static str {
        match self {
            BothEnablement::Enabled => {
                "Both screens → one video (Hyprland layout, black voids). Exactly 2 monitors only for now."
            }
            BothEnablement::Disabled { reason } => reason,
        }
    }
}

/// Dual-monitor mode enabled iff exactly **2** live outputs **and** `ffmpeg` on PATH.
///
/// Inventory is **live** (`hyprctl monitors -j` / `wf-recorder -L`) — never hardcoded names.
/// Check order (when disabled): ≠2 monitors first, then missing ffmpeg.
fn both_enablement(inventory_len: usize, ffmpeg_present: bool) -> BothEnablement {
    if inventory_len < 2 {
        BothEnablement::Disabled {
            reason: "2-monitor mode needs exactly two monitors (fewer detected).",
        }
    } else if inventory_len > 2 {
        BothEnablement::Disabled {
            reason: "Only exactly two monitors supported for now (3+ not supported yet).",
        }
    } else if !ffmpeg_present {
        BothEnablement::Disabled {
            reason: "2-monitor mode needs ffmpeg on PATH (post-stop stitch).",
        }
    } else {
        BothEnablement::Enabled
    }
}

/// Live inventory + PATH check; updates eligibility cell and Both tooltip.
///
/// Call only from Record (Both), Both-toggle activate, idle **edge**/throttle —
/// never on every status poll (hyprctl is synchronous on the GTK thread).
fn revalidate_both_gate(both_btn: &ToggleButton, both_eligible: &Cell<bool>) -> BothEnablement {
    let inv_len = sys::list_output_inventory().len();
    let gate = both_enablement(inv_len, sys::which("ffmpeg").is_some());
    both_eligible.set(gate.enabled());
    both_btn.set_tooltip_text(Some(gate.tooltip()));
    gate
}

/// Persist One-monitor pickers to XDG config (snappy on change).
///
/// - `output`: when `Some(name)`, set `fullscreen_output`. When `None` (empty
///   inventory / no selection), **leave** the existing pin alone — never clear.
/// - `fps`: `None` = Auto → stored as `one_fps = 0` (sticky). `Some(n)` → `one_fps = n`.
/// - Load errors: **do not save** (avoids wiping a corrupt/unreadable config with defaults).
fn persist_one_pickers(output: Option<&str>, fps: Option<u32>) -> Result<(), String> {
    let paths = EnvPaths::from_env();
    let mut cfg = Config::load(&paths).map_err(|e| {
        let msg = format!("Could not save settings: {e}");
        eprintln!("hyprcap: {msg}");
        msg
    })?;
    if let Some(name) = output.map(str::trim).filter(|s| !s.is_empty()) {
        cfg.fullscreen_output = Some(name.to_string());
    }
    // Sticky Auto sentinel; resolve_one_fps treats 0 as Auto.
    cfg.one_fps = Some(fps.unwrap_or(0));
    cfg.save(&paths).map_err(|e| {
        let msg = format!("Could not save settings: {e}");
        eprintln!("hyprcap: {msg}");
        msg
    })
}

#[cfg(test)]
mod picker_tests {
    use super::*;
    use hyprcap::sys::OutputInfo;

    fn out(name: &str, w: Option<i32>, h: Option<i32>, refresh: Option<f64>) -> OutputInfo {
        OutputInfo {
            name: name.into(),
            x: Some(0),
            y: Some(0),
            width: w,
            height: h,
            refresh,
        }
    }

    #[test]
    fn monitor_label_includes_resolution() {
        let o = out("HDMI-A-1", Some(2560), Some(1440), Some(144.0));
        assert_eq!(monitor_combo_label(&o), "HDMI-A-1 · 2560×1440");
        let bare = OutputInfo::names_only("DP-1");
        assert_eq!(monitor_combo_label(&bare), "DP-1");
        // Partial / zero geometry → name only.
        let partial = OutputInfo {
            name: "eDP-1".into(),
            x: Some(0),
            y: None,
            width: Some(1920),
            height: None,
            refresh: None,
        };
        assert_eq!(monitor_combo_label(&partial), "eDP-1");
        let zero = out("Z", Some(0), Some(1080), None);
        assert_eq!(monitor_combo_label(&zero), "Z");
    }

    #[test]
    fn default_monitor_is_largest_area_name_tiebreak() {
        let inv = vec![
            out("B-small", Some(1920), Some(1080), Some(60.0)),
            out("A-large", Some(2560), Some(1440), Some(144.0)),
        ];
        assert_eq!(default_monitor_index(&inv), 1);
        // Equal area → name ascending (A before B).
        let tied = vec![
            out("B", Some(1920), Some(1080), Some(60.0)),
            out("A", Some(1920), Some(1080), Some(60.0)),
        ];
        assert_eq!(default_monitor_index(&tied), 1); // "A"
        assert_eq!(default_monitor_index(&[]), 0);
    }

    #[test]
    fn select_monitor_prefers_config_pin() {
        let inv = vec![
            out("HDMI-A-1", Some(2560), Some(1440), Some(144.0)),
            out("DP-1", Some(1920), Some(1080), Some(60.0)),
        ];
        assert_eq!(select_monitor_index(&inv, Some("DP-1")), 1);
        assert_eq!(select_monitor_index(&inv, Some("GONE")), 0); // largest
        assert_eq!(select_monitor_index(&inv, None), 0);
        // Whitespace-only pin ignored.
        assert_eq!(select_monitor_index(&inv, Some("  ")), 0);
        assert_eq!(select_monitor_index(&inv, Some("  DP-1  ")), 1);
    }

    #[test]
    fn fps_entries_dedupe_native_60() {
        let e = build_fps_entries(Some(60), None);
        let labels: Vec<_> = e.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, vec!["Auto", "60 (native)", "30"]);
        assert_eq!(e[1].fps, Some(60));
        assert!(e[1].is_native);
        assert!(e.iter().filter(|x| x.fps == Some(60)).count() == 1);
    }

    #[test]
    fn fps_entries_dedupe_native_30() {
        let e = build_fps_entries(Some(30), None);
        let labels: Vec<_> = e.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, vec!["Auto", "30 (native)", "60"]);
        assert!(e[1].is_native);
        assert_eq!(e[1].fps, Some(30));
    }

    #[test]
    fn fps_entries_144_native_offers_60_30() {
        let e = build_fps_entries(Some(144), None);
        let labels: Vec<_> = e.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, vec!["Auto", "144 (native)", "60", "30"]);
    }

    #[test]
    fn fps_entries_includes_extra_pin() {
        let e = build_fps_entries(Some(144), Some(120));
        let labels: Vec<_> = e.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, vec!["Auto", "144 (native)", "120", "60", "30"]);
        assert_eq!(select_fps_index(&e, Some(120), true), 2);
    }

    #[test]
    fn fps_entries_unknown_native() {
        let e = build_fps_entries(None, None);
        let labels: Vec<_> = e.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, vec!["Auto", "60", "30"]);
    }

    #[test]
    fn select_fps_prefers_config_then_native() {
        let e = build_fps_entries(Some(144), None);
        assert_eq!(select_fps_index(&e, Some(30), true), 3);
        assert_eq!(select_fps_index(&e, Some(999), true), 1); // fall back native
        assert_eq!(select_fps_index(&e, None, true), 1); // native default
        assert_eq!(select_fps_index(&e, None, false), 0); // Auto
    }

    #[test]
    fn native_fps_rounds() {
        assert_eq!(native_fps_hz(Some(59.951)), Some(60));
        assert_eq!(native_fps_hz(Some(144.0)), Some(144));
        assert_eq!(native_fps_hz(None), None);
        assert_eq!(native_fps_hz(Some(0.0)), None);
        assert_eq!(native_fps_hz(Some(-1.0)), None);
        assert_eq!(native_fps_hz(Some(f64::NAN)), None);
        assert_eq!(native_fps_hz(Some(f64::INFINITY)), None);
    }

    #[test]
    fn ipc_fps_for_start_auto_and_rate() {
        assert_eq!(ipc_fps_for_start(None), Some(0));
        assert_eq!(ipc_fps_for_start(Some(144)), Some(144));
        assert_eq!(ipc_fps_for_start(Some(30)), Some(30));
    }

    #[test]
    fn picker_state_defaults_largest_and_native() {
        let inv = vec![
            out("DP-1", Some(1920), Some(1080), Some(60.0)),
            out("HDMI-A-1", Some(2560), Some(1440), Some(144.0)),
        ];
        let cfg = Config::with_home(Path::new("/home/test"), None);
        let p = PickerState::from_inventory_and_config(inv, &cfg);
        assert_eq!(p.selected_output_name(), Some("HDMI-A-1"));
        assert_eq!(p.selected_fps_value(), Some(144));
    }

    #[test]
    fn picker_state_uses_config_pins() {
        let inv = vec![
            out("HDMI-A-1", Some(2560), Some(1440), Some(144.0)),
            out("DP-1", Some(1920), Some(1080), Some(60.0)),
        ];
        let mut cfg = Config::with_home(Path::new("/home/test"), None);
        cfg.fullscreen_output = Some("DP-1".into());
        cfg.one_fps = Some(30);
        let p = PickerState::from_inventory_and_config(inv, &cfg);
        assert_eq!(p.selected_output_name(), Some("DP-1"));
        assert_eq!(p.selected_fps_value(), Some(30));
    }

    #[test]
    fn picker_state_auto_sentinel_and_first_run() {
        let inv = vec![out("HDMI-A-1", Some(2560), Some(1440), Some(144.0))];
        // Sticky Auto: one_fps = 0.
        let mut cfg = Config::with_home(Path::new("/home/test"), None);
        cfg.fullscreen_output = Some("HDMI-A-1".into());
        cfg.one_fps = Some(0);
        let p = PickerState::from_inventory_and_config(inv.clone(), &cfg);
        assert_eq!(p.selected_output_name(), Some("HDMI-A-1"));
        assert_eq!(p.selected_fps_value(), None); // Auto
                                                  // First-run: one_fps absent → native.
        cfg.one_fps = None;
        let p2 = PickerState::from_inventory_and_config(inv, &cfg);
        assert_eq!(p2.selected_fps_value(), Some(144));
    }

    #[test]
    fn picker_state_empty_inventory() {
        let cfg = Config::with_home(Path::new("/home/test"), None);
        let p = PickerState::from_inventory_and_config(vec![], &cfg);
        assert!(p.inventory.is_empty());
        assert!(p.selected_output_name().is_none());
        // Still has Auto | 60 | 30 (no native).
        assert!(!p.fps_entries.is_empty());
        assert_eq!(p.fps_entries[0].fps, None);
    }

    #[test]
    fn rebuild_fps_keeps_auto_and_rate() {
        let inv = vec![
            out("HDMI-A-1", Some(2560), Some(1440), Some(144.0)),
            out("DP-1", Some(1920), Some(1080), Some(60.0)),
        ];
        let cfg = Config::with_home(Path::new("/home/test"), None);
        let mut p = PickerState::from_inventory_and_config(inv, &cfg);
        // Select Auto then switch monitor — keep Auto.
        p.selected_fps = 0;
        p.selected_monitor = 1;
        p.rebuild_fps_entries(None, true);
        assert_eq!(p.selected_fps_value(), None);
        assert!(p
            .fps_entries
            .iter()
            .any(|e| e.is_native && e.fps == Some(60)));

        // Select 30 on HDMI, switch to DP — keep 30 if offered.
        p.selected_monitor = 0;
        p.rebuild_fps_entries(Some(30), false);
        assert_eq!(p.selected_fps_value(), Some(30));

        // Prefer 120 (extra) survives rebuild onto other head.
        p.selected_monitor = 1;
        p.rebuild_fps_entries(Some(120), false);
        assert_eq!(p.selected_fps_value(), Some(120));
        assert!(p.fps_entries.iter().any(|e| e.fps == Some(120)));

        // Out-of-list rate is included as extra and kept (not silently dropped).
        p.rebuild_fps_entries(Some(999), false);
        assert_eq!(p.selected_fps_value(), Some(999));
        assert!(p.fps_entries.iter().any(|e| e.fps == Some(999)));

        // select_fps_index falls back to native when prefer is absent from list.
        let e = build_fps_entries(Some(60), None);
        assert_eq!(select_fps_index(&e, Some(999), true), 1);
    }

    #[test]
    fn both_enablement_matrix_all_cells() {
        const FEW: &str = "2-monitor mode needs exactly two monitors (fewer detected).";
        const MANY: &str = "Only exactly two monitors supported for now (3+ not supported yet).";
        const FF: &str = "2-monitor mode needs ffmpeg on PATH (post-stop stitch).";
        const OK: &str =
            "Both screens → one video (Hyprland layout, black voids). Exactly 2 monitors only for now.";
        // Every cell of {0,1,2,3} × {ffmpeg T/F}.
        let cases: &[(usize, bool, bool, &str)] = &[
            (0, false, false, FEW),
            (0, true, false, FEW),
            (1, false, false, FEW),
            (1, true, false, FEW),
            (2, false, false, FF),
            (2, true, true, OK),
            (3, false, false, MANY),
            (3, true, false, MANY),
        ];
        for &(n, ff, want_en, want_tip) in cases {
            let g = both_enablement(n, ff);
            assert_eq!(g.enabled(), want_en, "enabled n={n} ff={ff}: got {:?}", g);
            assert_eq!(g.tooltip(), want_tip, "tooltip n={n} ff={ff}");
        }
    }

    #[test]
    fn timer_freezes_while_stop_in_flight_or_not_recording() {
        assert!(timer_should_tick("Recording", false));
        assert!(!timer_should_tick("Recording", true));
        assert!(!timer_should_tick("Stopping", false));
        assert!(!timer_should_tick("Stopping", true));
        assert!(!timer_should_tick("Idle", false));
        assert!(!timer_should_tick("Starting", false));
    }

    #[test]
    fn should_revalidate_both_on_idle_table() {
        let min = BOTH_REVALIDATE_IDLE_MIN; // 10s
        let s = Duration::from_secs;
        // (prev_state, elapsed_since_last, want)
        let cases: &[(Option<&str>, Option<Duration>, bool)] = &[
            // First attach
            (None, Some(s(0)), true),
            (None, None, true),
            // Steady Idle polls — anti-regression (must not fire every 350ms)
            (Some("Idle"), Some(s(0)), false),
            (Some("Idle"), Some(s(9)), false),
            // Throttle fire at/after min_interval
            (Some("Idle"), Some(s(10)), true),
            (Some("Idle"), Some(s(11)), true),
            // Never stamped
            (Some("Idle"), None, true),
            // Busy → idle edge even with fresh stamp
            (Some("Recording"), Some(s(0)), true),
            (Some("Stopping"), Some(s(0)), true),
            (Some("Starting"), Some(s(0)), true),
            (Some("SelectingRegion"), Some(s(0)), true),
        ];
        for &(prev, elapsed, want) in cases {
            assert_eq!(
                should_revalidate_both_on_idle(prev, elapsed, min),
                want,
                "prev={prev:?} elapsed={elapsed:?}"
            );
        }
    }

    fn ipc_status(
        state: &str,
        capture_mode: Option<&str>,
        capture_output: Option<&str>,
    ) -> IpcStatus {
        IpcStatus {
            state: state.into(),
            output_path: None,
            pid: None,
            started_at_unix: None,
            audio: false,
            last_error: None,
            last_success_path: None,
            elapsed_ms: None,
            capture_output: capture_output.map(str::to_string),
            capture_mode: capture_mode.map(str::to_string),
        }
    }

    #[test]
    fn format_capture_and_stopping_all_modes() {
        // One
        let one = ipc_status("Recording", Some("one"), Some("HDMI-A-1"));
        assert_eq!(
            format_capture_message(&one).as_deref(),
            Some("Output: HDMI-A-1")
        );
        assert_eq!(format_stopping_message(&one, false), "Stopping…");

        // Dual happy A+B pretty-print
        let both = ipc_status("Recording", Some("both"), Some("HDMI-A-1+DP-1"));
        assert_eq!(
            format_capture_message(&both).as_deref(),
            Some("2 monitors: HDMI-A-1 + DP-1")
        );
        assert_eq!(
            format_stopping_message(&both, false),
            "Stopping… Composing…"
        );
        // Mode branch is the contract — no '+' still dual:
        let both_one_name = ipc_status("Recording", Some("both"), Some("HDMI-A-1"));
        assert_eq!(
            format_capture_message(&both_one_name).as_deref(),
            Some("2 monitors: HDMI-A-1")
        );

        // Region: no capture_output → no capture label; stop is plain Stopping…
        let region = ipc_status("Recording", Some("region"), None);
        assert!(format_capture_message(&region).is_none());
        assert_eq!(format_stopping_message(&region, false), "Stopping…");

        // stopping_both flag wins even when capture_mode unset (start_in_flight stop).
        let idle = ipc_status("Idle", None, None);
        assert_eq!(format_stopping_message(&idle, true), "Stopping… Composing…");
        assert_eq!(format_stopping_message(&idle, false), "Stopping…");
    }

    #[test]
    fn status_message_line_branch_order() {
        let both_rec = ipc_status("Recording", Some("both"), Some("HDMI-A-1+DP-1"));
        // (c) Recording + both → 2 monitors: A + B
        assert_eq!(
            status_message_line(None, &both_rec, false, false).as_deref(),
            Some("2 monitors: HDMI-A-1 + DP-1")
        );

        // (a) stop_in_flight + both + capture_output → Composing, not dual label
        assert_eq!(
            status_message_line(None, &both_rec, true, true).as_deref(),
            Some("Stopping… Composing…")
        );

        // (b) Stopping + one → Stopping…
        let one_stop = ipc_status("Stopping", Some("one"), Some("DP-1"));
        assert_eq!(
            status_message_line(None, &one_stop, false, false).as_deref(),
            Some("Stopping…")
        );

        // (d) explicit message wins over Stopping
        assert_eq!(
            status_message_line(Some("Saved — path ready: /x.mp4"), &one_stop, true, false)
                .as_deref(),
            Some("Saved — path ready: /x.mp4")
        );

        // Idle without stop / message → None (leave label alone)
        let idle = ipc_status("Idle", None, None);
        assert!(status_message_line(None, &idle, false, false).is_none());
    }
}
