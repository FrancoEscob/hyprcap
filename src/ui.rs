//! GTK4 + libadwaita GUI view (client only).
//!
//! Opened solely via `record-ui` / `record-ui gui`. Closing the window
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, CheckButton, Label, Orientation, ToggleButton};
use libadwaita as adw;
use libadwaita::prelude::*;
use record_ui::client::{self, ClientError};
use record_ui::ipc::{IpcCommand, IpcRequest, IpcResponse, IpcStatus};
use record_ui::server::{self, RuntimePaths};

/// Primary entry: ensure session server, open small Adwaita window.
///
/// Returns a process exit code (0 on clean window close).
pub fn run_gui() -> i32 {
    let paths = match RuntimePaths::from_env() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("record-ui: {e}");
            return 1;
        }
    };

    if let Err(e) = server::ensure_server(&paths) {
        eprintln!("record-ui: failed to start session server: {e}");
        return 1;
    }

    // Init Adwaita/GTK only on this path — never from CLI subcommands.
    if let Err(e) = adw::init() {
        eprintln!("record-ui: failed to initialize libadwaita: {e}");
        return 1;
    }

    let app = adw::Application::builder()
        .application_id("dev.recordui.app")
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
    StartRegion { audio: bool, epoch: u64 },
    StartFullscreen { audio: bool, epoch: u64 },
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
        let mut starts: Vec<WorkerCmd> = Vec::new();
        let mut want_poll = false;

        for c in cmds {
            match c {
                WorkerCmd::ShutdownWorker => shutdown = true,
                WorkerCmd::Stop => want_stop = true,
                WorkerCmd::PollStatus => want_poll = true,
                s @ WorkerCmd::StartRegion { .. } | s @ WorkerCmd::StartFullscreen { .. } => {
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

        // Long starts on dedicated threads so Stop/status stay responsive.
        for start in starts {
            let sock = socket.clone();
            let tx = ui_tx.clone();
            let sd = Arc::clone(&shutting_down);
            let handle = thread::spawn(move || {
                let (kind_label, req, epoch) = match start {
                    WorkerCmd::StartRegion { audio, epoch } => (
                        "start region",
                        IpcRequest {
                            cmd: IpcCommand::StartRegion,
                            audio: Some(audio),
                            gui: None,
                            output: None,
                        },
                        epoch,
                    ),
                    WorkerCmd::StartFullscreen { audio, epoch } => (
                        "start one monitor",
                        IpcRequest {
                            cmd: IpcCommand::StartFullscreen,
                            audio: Some(audio),
                            gui: None,
                            output: None,
                        },
                        epoch,
                    ),
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

struct ViewState {
    last_status: Option<IpcStatus>,
    last_path: Option<String>,
    /// Start (region/fullscreen) RPC in flight.
    start_in_flight: bool,
    /// Stop RPC in flight.
    stop_in_flight: bool,
    /// Epoch for the active start request; Stop/new start bumps it.
    start_epoch: u64,
    /// Whether we already applied status.audio to the checkbox this session attach.
    audio_from_server: bool,
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
        .title("record-ui")
        .default_width(300)
        .default_height(250)
        .resizable(true)
        .build();

    let root = GtkBox::new(Orientation::Vertical, 6);
    root.set_margin_top(10);
    root.set_margin_bottom(10);
    root.set_margin_start(12);
    root.set_margin_end(12);

    let mode_row = GtkBox::new(Orientation::Horizontal, 8);
    let region_btn = ToggleButton::with_label("Region");
    let full_btn = ToggleButton::with_label("One monitor");
    region_btn.set_active(true);
    full_btn.set_group(Some(&region_btn));
    region_btn.set_hexpand(true);
    full_btn.set_hexpand(true);
    mode_row.append(&region_btn);
    mode_row.append(&full_btn);
    root.append(&mode_row);

    let audio_check = CheckButton::with_label("System audio");
    audio_check.set_active(false);
    root.append(&audio_check);

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
        start_epoch: 0,
        audio_from_server: false,
    }));

    // Track timeout SourceIds so close can cancel them.
    let timeout_ids: Rc<RefCell<Vec<glib::SourceId>>> = Rc::new(RefCell::new(Vec::new()));

    // --- Primary Record / Stop ---
    {
        let worker = Rc::clone(&worker);
        let region_btn = region_btn.clone();
        let audio_check = audio_check.clone();
        let view = Rc::clone(&view);
        let msg_label = msg_label.clone();
        let primary_btn = primary.clone();
        let window_weak = window.downgrade();
        let closed = Rc::clone(&closed);
        let start_epoch_counter = Rc::clone(&start_epoch_counter);
        primary.connect_clicked(move |_| {
            if closed.get() {
                return;
            }
            let worker_ref = worker.borrow();
            let Some(w) = worker_ref.as_ref() else {
                msg_label.set_text("Session worker unavailable");
                return;
            };

            let (session_busy, start_in_flight, stop_in_flight) = {
                let v = view.borrow();
                let busy = v
                    .last_status
                    .as_ref()
                    .map(|s| s.state.as_str() != "Idle")
                    .unwrap_or(false)
                    || v.start_in_flight;
                (busy, v.start_in_flight, v.stop_in_flight)
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
                        // Only invalidate start when a start RPC is actually in flight
                        // (avoid killing a future Start OpDone on idle no-op stop races).
                        if v.start_in_flight {
                            let next = start_epoch_counter.fetch_add(1, Ordering::SeqCst) + 1;
                            v.start_epoch = next;
                        }
                        // Keep start_in_flight until Stop OpDone/Error so label stays Stop;
                        // late Start is ignored via epoch when we bumped it.
                        drop(v);
                        msg_label.set_text("Stopping…");
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
            let audio = audio_check.is_active();
            let region = region_btn.is_active();
            let epoch = start_epoch_counter.fetch_add(1, Ordering::SeqCst) + 1;
            let cmd = if region {
                WorkerCmd::StartRegion { audio, epoch }
            } else {
                WorkerCmd::StartFullscreen { audio, epoch }
            };
            match w.tx.send(cmd) {
                Ok(()) => {
                    {
                        let mut v = view.borrow_mut();
                        v.start_in_flight = true;
                        v.start_epoch = epoch;
                    }
                    if region {
                        msg_label.set_text("Select a region…");
                        // Best-effort: get out of the way of slurp focus.
                        if let Some(win) = window_weak.upgrade() {
                            win.minimize();
                        }
                    } else {
                        msg_label.set_text("Starting…");
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
                if st.state == "Recording" {
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
            audio_check,
            window: window_weak,
        };
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
    audio_check: CheckButton,
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
            eprintln!("record-ui: {e}");
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
                    if let Some(ref o) = resp.status.capture_output {
                        format!("Output: {o}")
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

fn apply_status(
    status: IpcStatus,
    view: &Rc<RefCell<ViewState>>,
    w: &UiWidgets,
    message: Option<&str>,
) {
    {
        let mut v = view.borrow_mut();
        if let Some(ref p) = status.last_success_path {
            v.last_path = Some(p.clone());
        }
        // Sync audio from server on first status / mid-recording attach.
        if !v.audio_from_server {
            w.audio_check.set_active(status.audio);
            v.audio_from_server = true;
        }
        v.last_status = Some(status.clone());
    }

    w.state_label.set_text(&format!("State: {}", status.state));

    match status.state.as_str() {
        "Recording" => {
            w.timer_label.set_text(&format_elapsed(&status));
        }
        "Idle" | "SelectingRegion" => {
            w.timer_label.set_text("00:00");
        }
        "Starting" | "Stopping" => {}
        _ => {}
    }

    let last = view.borrow().last_path.clone();
    if let Some(ref p) = last {
        w.path_label.set_text(&format!("Last file: {p}"));
        w.open_file_btn.set_sensitive(true);
        w.open_folder_btn.set_sensitive(true);
    }

    // Mandatory Output: NAME after one-monitor resolve (status or start message).
    if let Some(m) = message {
        w.msg_label.set_text(m);
    } else if let Some(ref o) = status.capture_output {
        if matches!(status.state.as_str(), "Recording" | "Starting" | "Stopping") {
            w.msg_label.set_text(&format!("Output: {o}"));
        }
    }

    let idle = {
        let v = view.borrow();
        status.state == "Idle" && !v.start_in_flight && !v.stop_in_flight
    };
    w.region_btn.set_sensitive(idle);
    w.full_btn.set_sensitive(idle);
    w.audio_check.set_sensitive(idle);

    refresh_primary_from_view(view, &w.primary);
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
