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

/// One Wayland / Hyprland output from inventory.
///
/// Geometry and refresh come from `hyprctl monitors -j` when available.
/// Names-only fallback (`wf-recorder -L`) leaves layout fields as `None` —
/// **never invent (0,0)** so layout-true Both cannot false-enable later.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    pub name: String,
    /// Compositor layout origin X. `None` when unknown (names-only source).
    pub x: Option<i32>,
    /// Compositor layout origin Y. `None` when unknown.
    pub y: Option<i32>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    /// Refresh rate in Hz (`hyprctl` field `refreshRate`). `None` when unknown.
    pub refresh: Option<f64>,
}

impl OutputInfo {
    /// Names-only entry: no invented layout positions.
    pub fn names_only(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            x: None,
            y: None,
            width: None,
            height: None,
            refresh: None,
        }
    }

    /// True when layout geometry is known and usable for layout-true compose.
    ///
    /// Requires x/y present (either may be 0 — primary origin is valid) and
    /// positive width/height. Zero/negative dimensions do not count as geometry.
    pub fn has_geometry(&self) -> bool {
        matches!(
            (self.x, self.y, self.width, self.height),
            (Some(_), Some(_), Some(w), Some(h)) if w > 0 && h > 0
        )
    }

    /// Format one CLI / script line for `list-outputs`.
    ///
    /// With geometry: `name\tx\ty\twidth\theight\trefresh` (refresh blank if unknown).
    /// Without: name only (first column still script-friendly).
    pub fn display_line(&self) -> String {
        match (self.x, self.y, self.width, self.height) {
            (Some(x), Some(y), Some(w), Some(h)) if w > 0 && h > 0 => {
                let refresh = self.refresh.map(format_refresh_hz).unwrap_or_default();
                format!("{}\t{}\t{}\t{}\t{}\t{}", self.name, x, y, w, h, refresh)
            }
            _ => self.name.clone(),
        }
    }
}

/// Compact refresh for CLI (drop trailing `.0` when whole Hz).
fn format_refresh_hz(r: f64) -> String {
    if r.fract() == 0.0 && r.is_finite() {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

/// Rich output inventory: name + optional geometry/refresh.
///
/// Prefers `hyprctl monitors -j` (layout-true). Falls back to names-only from
/// `wf-recorder -L` with all geometry/refresh fields `None`.
pub fn list_output_inventory() -> Vec<OutputInfo> {
    list_inventory_from_hyprctl().unwrap_or_else(list_inventory_from_wf_recorder)
}

/// Ordered Wayland output names for fullscreen resolve / callers that only need names.
///
/// Derived from [`list_output_inventory`] so resolve paths stay compatible.
pub fn list_output_names() -> Vec<String> {
    list_output_inventory()
        .into_iter()
        .map(|o| o.name)
        .collect()
}

/// Parse `hyprctl monitors -j` → rich inventory (testable pure helper).
///
/// Returns `None` on invalid JSON or non-array root. Entries missing `name` are
/// skipped. Missing geometry fields become `None` (not invented zeros).
pub fn parse_hyprctl_monitors(json: &str) -> Option<Vec<OutputInfo>> {
    let monitors: serde_json::Value = serde_json::from_str(json).ok()?;
    let arr = monitors.as_array()?;
    let mut out = Vec::new();
    for m in arr {
        let Some(name) = m.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let refresh = m
            .get("refreshRate")
            .or_else(|| m.get("refresh"))
            .and_then(json_as_f64);
        out.push(OutputInfo {
            name: name.to_string(),
            x: m.get("x").and_then(json_as_i32),
            y: m.get("y").and_then(json_as_i32),
            width: m.get("width").and_then(json_as_i32),
            height: m.get("height").and_then(json_as_i32),
            refresh,
        });
    }
    Some(out)
}

/// Parse `hyprctl monitors -j` array → output names only (compat wrapper).
pub fn parse_hyprctl_monitor_names(json: &str) -> Option<Vec<String>> {
    Some(
        parse_hyprctl_monitors(json)?
            .into_iter()
            .map(|o| o.name)
            .collect(),
    )
}

fn json_as_i32(v: &serde_json::Value) -> Option<i32> {
    if let Some(n) = v.as_i64() {
        return i32::try_from(n).ok();
    }
    if let Some(n) = v.as_u64() {
        return i32::try_from(n).ok();
    }
    // Hyprland usually emits integers; tolerate float dimensions.
    v.as_f64().and_then(|f| {
        if f.is_finite() && f >= i32::MIN as f64 && f <= i32::MAX as f64 {
            Some(f as i32)
        } else {
            None
        }
    })
}

fn json_as_f64(v: &serde_json::Value) -> Option<f64> {
    if let Some(f) = v.as_f64() {
        return f.is_finite().then_some(f);
    }
    if let Some(n) = v.as_i64() {
        return Some(n as f64);
    }
    if let Some(n) = v.as_u64() {
        return Some(n as f64);
    }
    None
}

fn list_inventory_from_hyprctl() -> Option<Vec<OutputInfo>> {
    let out = Command::new("hyprctl")
        .args(["monitors", "-j"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let inv = parse_hyprctl_monitors(&text)?;
    if inv.is_empty() {
        None
    } else {
        Some(inv)
    }
}

fn list_inventory_from_wf_recorder() -> Vec<OutputInfo> {
    let out = Command::new("wf-recorder")
        .arg("-L")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok();
    let Some(out) = out else {
        return Vec::new();
    };
    // Prefer stdout when non-empty (accept even if exit non-zero — list may still
    // be useful). Fall back to stderr only on success (some builds list there).
    // Never scrape stderr after a failed exit with empty stdout (error text can
    // contain the substring "Name:" and would invent fake outputs).
    let stdout = String::from_utf8_lossy(&out.stdout);
    let text = if !stdout.trim().is_empty() {
        stdout.into_owned()
    } else if out.status.success() {
        String::from_utf8_lossy(&out.stderr).into_owned()
    } else {
        return Vec::new();
    };
    parse_wf_recorder_list_inventory(&text)
}

/// Extract all output names from `wf-recorder -L` text (testable).
pub fn parse_wf_recorder_list_names(text: &str) -> Vec<String> {
    parse_wf_recorder_list_inventory(text)
        .into_iter()
        .map(|o| o.name)
        .collect()
}

/// Names-only inventory from `wf-recorder -L` (geometry/refresh always `None`).
pub fn parse_wf_recorder_list_inventory(text: &str) -> Vec<OutputInfo> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        // "1. Name: HDMI-A-1 Description: ..."
        if let Some(rest) = line
            .split_once("Name:")
            .map(|(_, r)| r.trim())
            .filter(|r| !r.is_empty())
        {
            let name = rest
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !name.is_empty() {
                out.push(OutputInfo::names_only(name));
            }
        }
    }
    out
}

/// First name from `wf-recorder -L` text (compat helper).
pub fn parse_first_wf_recorder_list_name(text: &str) -> Option<String> {
    parse_wf_recorder_list_names(text).into_iter().next()
}

#[cfg(test)]
mod output_inventory_tests {
    use super::*;

    /// Fixture shaped like real `hyprctl monitors -j` (geometry + refreshRate).
    const HYPRCTL_TWO_HEADS: &str = r#"[
      {
        "id": 0,
        "name": "HDMI-A-1",
        "width": 2560,
        "height": 1440,
        "refreshRate": 144.0,
        "x": 0,
        "y": 0,
        "focused": true
      },
      {
        "id": 1,
        "name": "DP-1",
        "width": 1920,
        "height": 1080,
        "refreshRate": 59.951,
        "x": 2560,
        "y": 180,
        "focused": false
      }
    ]"#;

    #[test]
    fn parse_wf_recorder_list_all_names() {
        let sample = "\
1. Name: HDMI-A-1 Description: Foo (HDMI-A-1)
2. Name: DP-1 Description: Bar (DP-1)
";
        assert_eq!(
            parse_wf_recorder_list_names(sample),
            vec!["HDMI-A-1".to_string(), "DP-1".to_string()]
        );
        assert_eq!(
            parse_first_wf_recorder_list_name(sample).as_deref(),
            Some("HDMI-A-1")
        );
    }

    #[test]
    fn parse_wf_recorder_list_empty() {
        assert!(parse_wf_recorder_list_names("").is_empty());
        assert!(parse_wf_recorder_list_names("no names here").is_empty());
        assert_eq!(parse_first_wf_recorder_list_name(""), None);
    }

    #[test]
    fn parse_wf_recorder_inventory_has_no_geometry() {
        let sample = "\
1. Name: HDMI-A-1 Description: Foo (HDMI-A-1)
2. Name: DP-1 Description: Bar (DP-1)
";
        let inv = parse_wf_recorder_list_inventory(sample);
        assert_eq!(inv.len(), 2);
        for o in &inv {
            assert!(!o.has_geometry(), "must not invent positions: {o:?}");
            assert!(o.x.is_none());
            assert!(o.y.is_none());
            assert!(o.width.is_none());
            assert!(o.height.is_none());
            assert!(o.refresh.is_none());
            // display_line is name-only so scripts using first column still work
            assert_eq!(o.display_line(), o.name);
        }
        assert_eq!(inv[0].name, "HDMI-A-1");
        assert_eq!(inv[1].name, "DP-1");
    }

    #[test]
    fn parse_hyprctl_names() {
        let sample = r#"[
          {"name":"DP-1","focused":false},
          {"name":"HDMI-A-1","focused":true}
        ]"#;
        assert_eq!(
            parse_hyprctl_monitor_names(sample).unwrap(),
            vec!["DP-1".to_string(), "HDMI-A-1".to_string()]
        );
    }

    #[test]
    fn parse_hyprctl_monitors_geometry_and_refresh() {
        let inv = parse_hyprctl_monitors(HYPRCTL_TWO_HEADS).expect("parse ok");
        assert_eq!(inv.len(), 2);

        let a = &inv[0];
        assert_eq!(a.name, "HDMI-A-1");
        assert_eq!(a.x, Some(0));
        assert_eq!(a.y, Some(0));
        assert_eq!(a.width, Some(2560));
        assert_eq!(a.height, Some(1440));
        assert_eq!(a.refresh, Some(144.0));
        assert!(a.has_geometry());
        assert_eq!(a.display_line(), "HDMI-A-1\t0\t0\t2560\t1440\t144");

        let b = &inv[1];
        assert_eq!(b.name, "DP-1");
        assert_eq!(b.x, Some(2560));
        assert_eq!(b.y, Some(180));
        assert_eq!(b.width, Some(1920));
        assert_eq!(b.height, Some(1080));
        assert!((b.refresh.unwrap() - 59.951).abs() < 1e-9);
        assert_eq!(b.display_line(), "DP-1\t2560\t180\t1920\t1080\t59.951");
    }

    #[test]
    fn parse_hyprctl_accepts_refresh_alias() {
        let sample = r#"[{"name":"eDP-1","x":0,"y":0,"width":1920,"height":1080,"refresh":60.0}]"#;
        let inv = parse_hyprctl_monitors(sample).unwrap();
        assert_eq!(inv[0].refresh, Some(60.0));
    }

    #[test]
    fn parse_hyprctl_invalid_json_and_empty() {
        assert!(parse_hyprctl_monitors("not-json").is_none());
        assert!(parse_hyprctl_monitors("{}").is_none()); // object, not array
        assert_eq!(parse_hyprctl_monitors("[]").unwrap().len(), 0);
        assert!(parse_hyprctl_monitor_names("not-json").is_none());
    }

    #[test]
    fn parse_hyprctl_skips_missing_or_empty_name() {
        let sample = r#"[
          {"x":0,"y":0,"width":1,"height":1},
          {"name":"","width":1},
          {"name":"  "},
          {"name":"OK","width":800,"height":600,"x":10,"y":20,"refreshRate":60}
        ]"#;
        let inv = parse_hyprctl_monitors(sample).unwrap();
        assert_eq!(inv.len(), 1);
        assert_eq!(inv[0].name, "OK");
        assert_eq!(inv[0].x, Some(10));
        assert_eq!(inv[0].width, Some(800));
    }

    #[test]
    fn parse_hyprctl_partial_geometry_not_has_geometry() {
        // Missing y — do not invent; has_geometry false for layout-true gates later.
        let sample = r#"[{"name":"A","x":0,"width":100,"height":100,"refreshRate":60}]"#;
        let inv = parse_hyprctl_monitors(sample).unwrap();
        assert_eq!(inv[0].x, Some(0));
        assert!(inv[0].y.is_none());
        assert!(!inv[0].has_geometry());
        // Partial → name-only CLI line (no fake zeros for missing fields).
        assert_eq!(inv[0].display_line(), "A");
    }

    #[test]
    fn display_line_geometry_without_refresh_blank_sixth_field() {
        // Full geometry present, no refreshRate/refresh → still TSV with blank refresh.
        let sample = r#"[{"name":"eDP-1","x":0,"y":0,"width":1920,"height":1080}]"#;
        let inv = parse_hyprctl_monitors(sample).unwrap();
        assert_eq!(inv.len(), 1);
        assert!(inv[0].has_geometry());
        assert!(inv[0].refresh.is_none());
        assert_eq!(inv[0].display_line(), "eDP-1\t0\t0\t1920\t1080\t");
    }

    #[test]
    fn has_geometry_requires_positive_dimensions() {
        // x/y may be 0 (primary origin). width/height must be > 0.
        let ok = OutputInfo {
            name: "A".into(),
            x: Some(0),
            y: Some(0),
            width: Some(1920),
            height: Some(1080),
            refresh: None,
        };
        assert!(ok.has_geometry());

        let zero_w = OutputInfo {
            width: Some(0),
            ..ok.clone()
        };
        assert!(!zero_w.has_geometry());
        assert_eq!(zero_w.display_line(), "A");

        let zero_h = OutputInfo {
            height: Some(0),
            ..ok.clone()
        };
        assert!(!zero_h.has_geometry());

        let neg = OutputInfo {
            width: Some(-1),
            height: Some(100),
            ..ok
        };
        assert!(!neg.has_geometry());
    }

    #[test]
    fn names_only_constructor_never_invents_layout() {
        let o = OutputInfo::names_only("HDMI-A-1");
        assert!(!o.has_geometry());
        assert_eq!(
            o,
            OutputInfo {
                name: "HDMI-A-1".into(),
                x: None,
                y: None,
                width: None,
                height: None,
                refresh: None,
            }
        );
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
