//! System / app / mic audio plan and Pulse/PipeWire capture session.
//!
//! `wf-recorder -a` accepts a single source. We resolve the user intent into one
//! capture source name: either an existing monitor/input, or a temporary null-sink
//! mix built with `pactl` modules (cleaned up on stop).

use std::process::Command;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Domain
// ---------------------------------------------------------------------------

/// How system (playback) audio is captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SystemAudioMode {
    #[default]
    Off,
    /// Everything playing on a sink (default or named).
    All,
    /// A single application's sink-input (matched by name).
    App,
}

impl SystemAudioMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SystemAudioMode::Off => "off",
            SystemAudioMode::All => "all",
            SystemAudioMode::App => "app",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" | "0" => Some(Self::Off),
            "all" | "system" | "pc" | "true" | "1" => Some(Self::All),
            "app" | "application" => Some(Self::App),
            _ => None,
        }
    }
}

/// User-facing audio intent for one recording session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AudioPlan {
    #[serde(default)]
    pub system: SystemAudioMode,
    /// Sink name for [`SystemAudioMode::All`]. Empty / None → default sink.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sink: Option<String>,
    /// Application name match for [`SystemAudioMode::App`] (`application.name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Record a microphone input.
    #[serde(default)]
    pub mic: bool,
    /// Mic source name. Empty / None → default input source (not a `.monitor`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mic_device: Option<String>,
}

impl AudioPlan {
    pub fn off() -> Self {
        Self::default()
    }

    /// Legacy checkbox / `--audio`: system all on default sink, no mic.
    pub fn system_all() -> Self {
        Self {
            system: SystemAudioMode::All,
            ..Self::default()
        }
    }

    pub fn enabled(&self) -> bool {
        self.system != SystemAudioMode::Off || self.mic
    }

    /// Resolve from optional IPC `audio_plan`, legacy `audio` bool, and config.
    pub fn resolve(
        plan: Option<AudioPlan>,
        legacy_audio: Option<bool>,
        config: &crate::config::Config,
    ) -> Self {
        if let Some(p) = plan {
            return p.normalized();
        }
        if let Some(on) = legacy_audio {
            return if on {
                Self::system_all()
            } else {
                Self::off()
            };
        }
        config.audio_plan()
    }

    pub fn normalized(mut self) -> Self {
        if let Some(s) = self.sink.take() {
            let t = s.trim().to_string();
            self.sink = if t.is_empty() { None } else { Some(t) };
        }
        if let Some(s) = self.app.take() {
            let t = s.trim().to_string();
            self.app = if t.is_empty() { None } else { Some(t) };
        }
        if let Some(s) = self.mic_device.take() {
            let t = s.trim().to_string();
            self.mic_device = if t.is_empty() { None } else { Some(t) };
        }
        if self.system == SystemAudioMode::App && self.app.is_none() {
            // Leave as App; setup will error with a clear message.
        }
        self
    }
}

// ---------------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioSinkInfo {
    pub name: String,
    pub description: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioSourceInfo {
    pub name: String,
    pub description: String,
    pub is_default: bool,
    /// True for `*.monitor` (playback monitors), false for real inputs (mics).
    pub is_monitor: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioAppInfo {
    /// Pulse sink-input index / object.serial.
    pub index: u32,
    pub name: String,
    pub media_name: Option<String>,
    pub sink: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AudioInventory {
    pub sinks: Vec<AudioSinkInfo>,
    pub sources: Vec<AudioSourceInfo>,
    pub apps: Vec<AudioAppInfo>,
}

impl AudioInventory {
    pub fn mics(&self) -> impl Iterator<Item = &AudioSourceInfo> {
        self.sources.iter().filter(|s| !s.is_monitor)
    }
}

// ---------------------------------------------------------------------------
// Session (capture source + teardown)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedApp {
    pub index: u32,
    pub original_sink: String,
}

/// Live Pulse routing created for a recording; must be torn down on stop.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioSession {
    /// Source name for `wf-recorder -aDEVICE`.
    pub capture_source: String,
    /// `pactl load-module` ids to unload (reverse order).
    pub module_ids: Vec<u32>,
    pub moved_apps: Vec<MovedApp>,
}

impl AudioSession {
    pub fn is_empty_setup(&self) -> bool {
        self.module_ids.is_empty() && self.moved_apps.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioError {
    #[error("pactl not available")]
    PactlMissing,
    #[error("audio: {0}")]
    Message(String),
}

// ---------------------------------------------------------------------------
// PulseCtl abstraction
// ---------------------------------------------------------------------------

/// Minimal Pulse/PipeWire control surface (production: `pactl`).
pub trait PulseCtl {
    fn command_exists(&self, binary: &str) -> bool;
    fn default_sink(&mut self) -> Result<String, AudioError>;
    fn default_source(&mut self) -> Result<String, AudioError>;
    fn list_inventory(&mut self) -> Result<AudioInventory, AudioError>;
    fn load_module(&mut self, name: &str, args: &str) -> Result<u32, AudioError>;
    fn unload_module(&mut self, id: u32) -> Result<(), AudioError>;
    fn move_sink_input(&mut self, index: u32, sink: &str) -> Result<(), AudioError>;
}

/// Production adapter using `pactl`.
#[derive(Debug, Default)]
pub struct PactlPulse;

impl PulseCtl for PactlPulse {
    fn command_exists(&self, binary: &str) -> bool {
        Command::new("which")
            .arg(binary)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn default_sink(&mut self) -> Result<String, AudioError> {
        let out = run_pactl(&["get-default-sink"])?;
        let s = out.trim().to_string();
        if s.is_empty() {
            Err(AudioError::Message("no default sink".into()))
        } else {
            Ok(s)
        }
    }

    fn default_source(&mut self) -> Result<String, AudioError> {
        let out = run_pactl(&["get-default-source"])?;
        let s = out.trim().to_string();
        if s.is_empty() {
            Err(AudioError::Message("no default source".into()))
        } else {
            Ok(s)
        }
    }

    fn list_inventory(&mut self) -> Result<AudioInventory, AudioError> {
        list_audio_inventory_pactl()
    }

    fn load_module(&mut self, name: &str, args: &str) -> Result<u32, AudioError> {
        let mut cmd = Command::new("pactl");
        cmd.arg("load-module").arg(name);
        if !args.is_empty() {
            for tok in args.split_whitespace() {
                cmd.arg(tok);
            }
        }
        let out = cmd
            .output()
            .map_err(|e| AudioError::Message(format!("pactl load-module: {e}")))?;
        if !out.status.success() {
            return Err(AudioError::Message(format!(
                "pactl load-module {name} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        s.parse::<u32>()
            .map_err(|_| AudioError::Message(format!("bad module id: {s}")))
    }

    fn unload_module(&mut self, id: u32) -> Result<(), AudioError> {
        let out = Command::new("pactl")
            .args(["unload-module", &id.to_string()])
            .output()
            .map_err(|e| AudioError::Message(format!("pactl unload-module: {e}")))?;
        if !out.status.success() {
            // Best-effort: module may already be gone.
            return Ok(());
        }
        Ok(())
    }

    fn move_sink_input(&mut self, index: u32, sink: &str) -> Result<(), AudioError> {
        let out = Command::new("pactl")
            .args(["move-sink-input", &index.to_string(), sink])
            .output()
            .map_err(|e| AudioError::Message(format!("pactl move-sink-input: {e}")))?;
        if !out.status.success() {
            return Err(AudioError::Message(format!(
                "move-sink-input {index} → {sink}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }
}

fn run_pactl(args: &[&str]) -> Result<String, AudioError> {
    let out = Command::new("pactl")
        .args(args)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AudioError::PactlMissing
            } else {
                AudioError::Message(format!("pactl: {e}"))
            }
        })?;
    if !out.status.success() {
        return Err(AudioError::Message(format!(
            "pactl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

// ---------------------------------------------------------------------------
// Parse helpers (pure)
// ---------------------------------------------------------------------------

/// Monitor source name for a sink (`{sink}.monitor`).
pub fn sink_monitor_name(sink: &str) -> String {
    format!("{sink}.monitor")
}

/// Parse `pactl list short sinks` lines.
pub fn parse_short_sinks(text: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(idx) = parts.next().and_then(|s| s.parse().ok()) else {
            continue;
        };
        let Some(name) = parts.next() else { continue };
        out.push((idx, name.to_string()));
    }
    out
}

/// Parse `pactl list short sources` → (index, name).
pub fn parse_short_sources(text: &str) -> Vec<(u32, String)> {
    parse_short_sinks(text) // same format
}

/// Parse `pactl list sink-inputs` for app capture.
pub fn parse_sink_inputs(text: &str) -> Vec<AudioAppInfo> {
    let mut apps = Vec::new();
    let mut cur_index: Option<u32> = None;
    let mut cur_sink = String::new();
    let mut cur_app_name: Option<String> = None;
    let mut cur_media: Option<String> = None;

    let flush = |apps: &mut Vec<AudioAppInfo>,
                 idx: &mut Option<u32>,
                 sink: &mut String,
                 app: &mut Option<String>,
                 media: &mut Option<String>| {
        if let Some(i) = idx.take() {
            let name = app
                .take()
                .filter(|s| !s.is_empty())
                .or_else(|| media.clone())
                .unwrap_or_else(|| format!("sink-input-{i}"));
            apps.push(AudioAppInfo {
                index: i,
                name,
                media_name: media.take(),
                sink: std::mem::take(sink),
            });
        } else {
            app.take();
            media.take();
            sink.clear();
        }
    };

    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Sink Input #") {
            flush(
                &mut apps,
                &mut cur_index,
                &mut cur_sink,
                &mut cur_app_name,
                &mut cur_media,
            );
            cur_index = rest.trim().parse().ok();
            continue;
        }
        if cur_index.is_none() {
            continue;
        }
        if let Some(rest) = t.strip_prefix("Sink:") {
            cur_sink = rest.trim().to_string();
            // Sink may be index; keep raw — we resolve name later if needed.
            continue;
        }
        if let Some(rest) = t.strip_prefix("application.name = ") {
            cur_app_name = Some(unquote_prop(rest));
            continue;
        }
        if let Some(rest) = t.strip_prefix("media.name = ") {
            cur_media = Some(unquote_prop(rest));
            continue;
        }
    }
    flush(
        &mut apps,
        &mut cur_index,
        &mut cur_sink,
        &mut cur_app_name,
        &mut cur_media,
    );
    apps
}

fn unquote_prop(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Build inventory from raw pactl outputs (pure; testable).
pub fn inventory_from_pactl_text(
    sinks_short: &str,
    sources_short: &str,
    sink_inputs: &str,
    default_sink: &str,
    default_source: &str,
    sink_descs: &[(String, String)],
    source_descs: &[(String, String)],
) -> AudioInventory {
    let def_sink = default_sink.trim();
    let def_src = default_source.trim();
    let sinks: Vec<AudioSinkInfo> = parse_short_sinks(sinks_short)
        .into_iter()
        .map(|(_, name)| {
            let description = sink_descs
                .iter()
                .find(|(n, _)| n == &name)
                .map(|(_, d)| d.clone())
                .unwrap_or_else(|| name.clone());
            AudioSinkInfo {
                is_default: name == def_sink,
                name,
                description,
            }
        })
        .collect();

    // Map sink index → name for app.sink field when pactl prints numeric Sink:
    let sink_by_idx: std::collections::HashMap<u32, String> = parse_short_sinks(sinks_short)
        .into_iter()
        .collect();

    let sources: Vec<AudioSourceInfo> = parse_short_sources(sources_short)
        .into_iter()
        .map(|(_, name)| {
            let is_monitor = name.ends_with(".monitor");
            let description = source_descs
                .iter()
                .find(|(n, _)| n == &name)
                .map(|(_, d)| d.clone())
                .unwrap_or_else(|| name.clone());
            AudioSourceInfo {
                is_default: name == def_src,
                is_monitor,
                name,
                description,
            }
        })
        .collect();

    let mut apps = parse_sink_inputs(sink_inputs);
    for app in &mut apps {
        if let Ok(idx) = app.sink.parse::<u32>() {
            if let Some(n) = sink_by_idx.get(&idx) {
                app.sink = n.clone();
            }
        }
    }

    AudioInventory {
        sinks,
        sources,
        apps,
    }
}

fn list_audio_inventory_pactl() -> Result<AudioInventory, AudioError> {
    let sinks_short = run_pactl(&["list", "short", "sinks"])?;
    let sources_short = run_pactl(&["list", "short", "sources"])?;
    let sink_inputs = run_pactl(&["list", "sink-inputs"]).unwrap_or_default();
    let default_sink = run_pactl(&["get-default-sink"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let default_source = run_pactl(&["get-default-source"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // Descriptions: best-effort from `pactl list sinks` / sources (optional).
    let sink_descs = parse_device_descriptions(&run_pactl(&["list", "sinks"]).unwrap_or_default());
    let source_descs =
        parse_device_descriptions(&run_pactl(&["list", "sources"]).unwrap_or_default());

    Ok(inventory_from_pactl_text(
        &sinks_short,
        &sources_short,
        &sink_inputs,
        &default_sink,
        &default_source,
        &sink_descs,
        &source_descs,
    ))
}

/// Parse Name + Description pairs from `pactl list sinks|sources`.
pub fn parse_device_descriptions(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut name: Option<String> = None;
    let mut desc: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("Sink #") || t.starts_with("Source #") {
            if let (Some(n), d) = (name.take(), desc.take()) {
                out.push((n, d.unwrap_or_default()));
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("Name:") {
            name = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = t.strip_prefix("Description:") {
            desc = Some(rest.trim().to_string());
        }
    }
    if let (Some(n), d) = (name, desc) {
        out.push((n, d.unwrap_or_default()));
    }
    out
}

// ---------------------------------------------------------------------------
// Setup / teardown
// ---------------------------------------------------------------------------

/// Classify how many independent sources we need (for mix vs direct).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureRecipe {
    None,
    /// Single existing source name (monitor or mic).
    Direct(String),
    /// Build a null-sink mix from these source names (+ optional moved apps).
    Mix {
        feed_sources: Vec<String>,
        /// Apps to move onto the null sink (then loopback null → original).
        apps: Vec<MovedApp>,
        /// When capturing app-only without mic, we still need hear-loopback to speakers.
        hear_sink: Option<String>,
    },
}

/// Pure planning step: turn intent + inventory into a recipe (no side effects).
pub fn plan_capture(
    plan: &AudioPlan,
    inv: &AudioInventory,
    default_sink: &str,
    default_source: &str,
) -> Result<CaptureRecipe, AudioError> {
    let plan = plan.clone().normalized();
    if !plan.enabled() {
        return Ok(CaptureRecipe::None);
    }

    let mut feeds: Vec<String> = Vec::new();
    let mut apps: Vec<MovedApp> = Vec::new();
    let mut hear_sink: Option<String> = None;

    match plan.system {
        SystemAudioMode::Off => {}
        SystemAudioMode::All => {
            let sink = plan
                .sink
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(default_sink);
            if sink.is_empty() {
                return Err(AudioError::Message("no default sink for system audio".into()));
            }
            // Prefer known sink from inventory when possible.
            let sink = inv
                .sinks
                .iter()
                .find(|s| s.name == sink)
                .map(|s| s.name.as_str())
                .unwrap_or(sink);
            feeds.push(sink_monitor_name(sink));
        }
        SystemAudioMode::App => {
            let want = plan
                .app
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AudioError::Message("app audio selected but no app name".into()))?;
            let app = find_app(inv, want).ok_or_else(|| {
                AudioError::Message(format!(
                    "no playing app matching \"{want}\" (start playback and retry)"
                ))
            })?;
            let original = if app.sink.is_empty() {
                default_sink.to_string()
            } else {
                app.sink.clone()
            };
            apps.push(MovedApp {
                index: app.index,
                original_sink: original.clone(),
            });
            hear_sink = Some(original);
            // Capture comes from null.monitor after move — not a pre-existing feed.
        }
    }

    if plan.mic {
        let src = plan
            .mic_device
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                // Prefer default if it is not a monitor; else first non-monitor.
                if !default_source.is_empty() && !default_source.ends_with(".monitor") {
                    default_source.to_string()
                } else {
                    inv.mics()
                        .next()
                        .map(|s| s.name.clone())
                        .unwrap_or_default()
                }
            });
        if src.is_empty() {
            return Err(AudioError::Message("no microphone source found".into()));
        }
        feeds.push(src);
    }

    // Decide Direct vs Mix.
    let app_only = !apps.is_empty() && feeds.is_empty();
    let multi = feeds.len() > 1 || (!apps.is_empty() && !feeds.is_empty()) || app_only;

    if !multi {
        if let Some(one) = feeds.into_iter().next() {
            return Ok(CaptureRecipe::Direct(one));
        }
        return Ok(CaptureRecipe::None);
    }

    Ok(CaptureRecipe::Mix {
        feed_sources: feeds,
        apps,
        hear_sink,
    })
}

fn find_app<'a>(inv: &'a AudioInventory, want: &str) -> Option<&'a AudioAppInfo> {
    let want_l = want.to_ascii_lowercase();
    inv.apps.iter().find(|a| a.name.eq_ignore_ascii_case(want))
        .or_else(|| {
            inv.apps.iter().find(|a| {
                a.name.to_ascii_lowercase().contains(&want_l)
                    || a.media_name
                        .as_ref()
                        .map(|m| m.to_ascii_lowercase().contains(&want_l))
                        .unwrap_or(false)
            })
        })
}

/// Execute a recipe against Pulse; returns session with capture source.
pub fn setup_session_with<P: PulseCtl>(
    plan: &AudioPlan,
    pulse: &mut P,
) -> Result<Option<AudioSession>, AudioError> {
    if !plan.enabled() {
        return Ok(None);
    }
    if !pulse.command_exists("pactl") {
        return Err(AudioError::PactlMissing);
    }

    let inv = pulse.list_inventory()?;
    let default_sink = pulse.default_sink().unwrap_or_default();
    let default_source = pulse.default_source().unwrap_or_default();
    let recipe = plan_capture(plan, &inv, &default_sink, &default_source)?;

    match recipe {
        CaptureRecipe::None => Ok(None),
        CaptureRecipe::Direct(src) => Ok(Some(AudioSession {
            capture_source: src,
            module_ids: Vec::new(),
            moved_apps: Vec::new(),
        })),
        CaptureRecipe::Mix {
            feed_sources,
            apps,
            hear_sink,
        } => setup_mix(pulse, feed_sources, apps, hear_sink, &default_sink),
    }
}

fn setup_mix<P: PulseCtl>(
    pulse: &mut P,
    feed_sources: Vec<String>,
    apps: Vec<MovedApp>,
    hear_sink: Option<String>,
    default_sink: &str,
) -> Result<Option<AudioSession>, AudioError> {
    let tag = std::process::id();
    let sink_name = format!("hyprcap_mix_{tag}");
    let null_id = pulse.load_module(
        "module-null-sink",
        &format!("sink_name={sink_name} sink_properties=device.description=HyprcapMix"),
    )?;
    let mut module_ids = vec![null_id];
    let mut moved = Vec::new();

    let rollback = |pulse: &mut P, module_ids: &[u32], moved: &[MovedApp]| {
        for m in moved.iter().rev() {
            let _ = pulse.move_sink_input(m.index, &m.original_sink);
        }
        for id in module_ids.iter().rev() {
            let _ = pulse.unload_module(*id);
        }
    };

    // Move apps onto null sink so we capture only them (and loopback to hear).
    for app in &apps {
        if let Err(e) = pulse.move_sink_input(app.index, &sink_name) {
            rollback(pulse, &module_ids, &moved);
            return Err(e);
        }
        moved.push(app.clone());
    }

    // Loopback each feed source → mix sink.
    for src in &feed_sources {
        match pulse.load_module(
            "module-loopback",
            &format!("source={src} sink={sink_name} latency_msec=20"),
        ) {
            Ok(id) => module_ids.push(id),
            Err(e) => {
                rollback(pulse, &module_ids, &moved);
                return Err(e);
            }
        }
    }

    // If we moved apps onto null, loopback null.monitor → real speakers so user still hears.
    if !apps.is_empty() {
        let hear = hear_sink
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(default_sink);
        if !hear.is_empty() {
            match pulse.load_module(
                "module-loopback",
                &format!(
                    "source={}.monitor sink={hear} latency_msec=20",
                    sink_name
                ),
            ) {
                Ok(id) => module_ids.push(id),
                Err(e) => {
                    rollback(pulse, &module_ids, &moved);
                    return Err(e);
                }
            }
        }
    }

    Ok(Some(AudioSession {
        capture_source: sink_monitor_name(&sink_name),
        module_ids,
        moved_apps: moved,
    }))
}

/// Tear down a session (best-effort). Returns warnings.
pub fn teardown_session_with<P: PulseCtl>(
    session: &AudioSession,
    pulse: &mut P,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for m in session.moved_apps.iter().rev() {
        if let Err(e) = pulse.move_sink_input(m.index, &m.original_sink) {
            warnings.push(format!("restore app #{}: {e}", m.index));
        }
    }
    for id in session.module_ids.iter().rev() {
        if let Err(e) = pulse.unload_module(*id) {
            warnings.push(format!("unload module {id}: {e}"));
        }
    }
    warnings
}

/// Production helpers.
pub fn list_audio_inventory() -> Result<AudioInventory, AudioError> {
    let mut p = PactlPulse;
    if !p.command_exists("pactl") {
        return Err(AudioError::PactlMissing);
    }
    p.list_inventory()
}

pub fn setup_session(plan: &AudioPlan) -> Result<Option<AudioSession>, AudioError> {
    let mut p = PactlPulse;
    setup_session_with(plan, &mut p)
}

pub fn teardown_session(session: &AudioSession) -> Vec<String> {
    let mut p = PactlPulse;
    teardown_session_with(session, &mut p)
}

/// `wf-recorder` argv audio flag: `-aDEVICE` (no space) or omit.
pub fn audio_argv_flag(device: Option<&str>) -> Option<String> {
    device
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("-a{s}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn sink_monitor_suffix() {
        assert_eq!(
            sink_monitor_name("alsa_output.usb.analog-stereo"),
            "alsa_output.usb.analog-stereo.monitor"
        );
    }

    #[test]
    fn parse_short_sinks_basic() {
        let t = "54\talsa_output.pci.hdmi-stereo\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n";
        let v = parse_short_sinks(t);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].1, "alsa_output.pci.hdmi-stereo");
    }

    #[test]
    fn parse_sink_inputs_spotify() {
        let t = r#"
Sink Input #2862
        Sink: 2585
        Properties:
                media.name = "Spotify"
                application.name = "Spotify"
"#;
        let apps = parse_sink_inputs(t);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].index, 2862);
        assert_eq!(apps[0].name, "Spotify");
    }

    #[test]
    fn plan_all_direct() {
        let plan = AudioPlan::system_all();
        let inv = AudioInventory {
            sinks: vec![AudioSinkInfo {
                name: "spk".into(),
                description: "Speakers".into(),
                is_default: true,
            }],
            ..Default::default()
        };
        let r = plan_capture(&plan, &inv, "spk", "mic").unwrap();
        assert_eq!(r, CaptureRecipe::Direct("spk.monitor".into()));
    }

    #[test]
    fn plan_mic_only_direct() {
        let plan = AudioPlan {
            mic: true,
            mic_device: Some("mic1".into()),
            ..Default::default()
        };
        let inv = AudioInventory::default();
        let r = plan_capture(&plan, &inv, "spk", "mic1").unwrap();
        assert_eq!(r, CaptureRecipe::Direct("mic1".into()));
    }

    #[test]
    fn plan_all_plus_mic_mix() {
        let plan = AudioPlan {
            system: SystemAudioMode::All,
            mic: true,
            mic_device: Some("mic1".into()),
            ..Default::default()
        };
        let inv = AudioInventory {
            sinks: vec![AudioSinkInfo {
                name: "spk".into(),
                description: "S".into(),
                is_default: true,
            }],
            ..Default::default()
        };
        match plan_capture(&plan, &inv, "spk", "mic1").unwrap() {
            CaptureRecipe::Mix { feed_sources, apps, .. } => {
                assert_eq!(
                    feed_sources,
                    vec!["spk.monitor".to_string(), "mic1".to_string()]
                );
                assert!(apps.is_empty());
            }
            other => panic!("expected mix, got {other:?}"),
        }
    }

    #[test]
    fn plan_app_mix() {
        let plan = AudioPlan {
            system: SystemAudioMode::App,
            app: Some("Spotify".into()),
            ..Default::default()
        };
        let inv = AudioInventory {
            apps: vec![AudioAppInfo {
                index: 9,
                name: "Spotify".into(),
                media_name: None,
                sink: "spk".into(),
            }],
            ..Default::default()
        };
        match plan_capture(&plan, &inv, "spk", "mic").unwrap() {
            CaptureRecipe::Mix {
                feed_sources,
                apps,
                hear_sink,
            } => {
                assert!(feed_sources.is_empty());
                assert_eq!(apps.len(), 1);
                assert_eq!(apps[0].index, 9);
                assert_eq!(hear_sink.as_deref(), Some("spk"));
            }
            other => panic!("expected mix, got {other:?}"),
        }
    }

    #[test]
    fn audio_argv_flag_format() {
        assert_eq!(
            audio_argv_flag(Some("spk.monitor")).as_deref(),
            Some("-aspk.monitor")
        );
        assert_eq!(audio_argv_flag(Some("")), None);
        assert_eq!(audio_argv_flag(None), None);
    }

    #[test]
    fn resolve_legacy_and_plan() {
        let cfg = crate::config::Config {
            audio_default: false,
            ..crate::config::Config::default()
        };
        assert!(!AudioPlan::resolve(None, None, &cfg).enabled());
        assert_eq!(
            AudioPlan::resolve(None, Some(true), &cfg).system,
            SystemAudioMode::All
        );
        let p = AudioPlan {
            system: SystemAudioMode::Off,
            mic: true,
            ..Default::default()
        };
        assert!(AudioPlan::resolve(Some(p), Some(true), &cfg).mic);
    }

    #[derive(Default)]
    struct FakePulse {
        default_sink: String,
        default_source: String,
        inv: AudioInventory,
        modules: HashMap<u32, String>,
        next_id: u32,
        moves: Vec<(u32, String)>,
        fail_load: bool,
    }

    impl PulseCtl for FakePulse {
        fn command_exists(&self, binary: &str) -> bool {
            binary == "pactl"
        }
        fn default_sink(&mut self) -> Result<String, AudioError> {
            Ok(self.default_sink.clone())
        }
        fn default_source(&mut self) -> Result<String, AudioError> {
            Ok(self.default_source.clone())
        }
        fn list_inventory(&mut self) -> Result<AudioInventory, AudioError> {
            Ok(self.inv.clone())
        }
        fn load_module(&mut self, name: &str, args: &str) -> Result<u32, AudioError> {
            if self.fail_load {
                return Err(AudioError::Message("fail".into()));
            }
            let id = self.next_id;
            self.next_id += 1;
            self.modules.insert(id, format!("{name} {args}"));
            Ok(id)
        }
        fn unload_module(&mut self, id: u32) -> Result<(), AudioError> {
            self.modules.remove(&id);
            Ok(())
        }
        fn move_sink_input(&mut self, index: u32, sink: &str) -> Result<(), AudioError> {
            self.moves.push((index, sink.to_string()));
            Ok(())
        }
    }

    #[test]
    fn setup_direct_no_modules() {
        let mut pulse = FakePulse {
            default_sink: "spk".into(),
            default_source: "mic".into(),
            inv: AudioInventory {
                sinks: vec![AudioSinkInfo {
                    name: "spk".into(),
                    description: "S".into(),
                    is_default: true,
                }],
                ..Default::default()
            },
            next_id: 1,
            ..Default::default()
        };
        let sess = setup_session_with(&AudioPlan::system_all(), &mut pulse)
            .unwrap()
            .unwrap();
        assert_eq!(sess.capture_source, "spk.monitor");
        assert!(sess.module_ids.is_empty());
    }

    #[test]
    fn setup_mix_loads_null_and_loopbacks() {
        let mut pulse = FakePulse {
            default_sink: "spk".into(),
            default_source: "mic".into(),
            inv: AudioInventory {
                sinks: vec![AudioSinkInfo {
                    name: "spk".into(),
                    description: "S".into(),
                    is_default: true,
                }],
                sources: vec![AudioSourceInfo {
                    name: "mic".into(),
                    description: "M".into(),
                    is_default: true,
                    is_monitor: false,
                }],
                ..Default::default()
            },
            next_id: 10,
            ..Default::default()
        };
        let plan = AudioPlan {
            system: SystemAudioMode::All,
            mic: true,
            ..Default::default()
        };
        let sess = setup_session_with(&plan, &mut pulse).unwrap().unwrap();
        assert!(sess.capture_source.ends_with(".monitor"));
        assert!(sess.module_ids.len() >= 3); // null + 2 loopbacks
        teardown_session_with(&sess, &mut pulse);
        assert!(pulse.modules.is_empty());
    }

    #[test]
    fn setup_app_moves_and_restores() {
        let mut pulse = FakePulse {
            default_sink: "spk".into(),
            default_source: "mic".into(),
            inv: AudioInventory {
                apps: vec![AudioAppInfo {
                    index: 42,
                    name: "Spotify".into(),
                    media_name: None,
                    sink: "spk".into(),
                }],
                sinks: vec![AudioSinkInfo {
                    name: "spk".into(),
                    description: "S".into(),
                    is_default: true,
                }],
                ..Default::default()
            },
            next_id: 5,
            ..Default::default()
        };
        let plan = AudioPlan {
            system: SystemAudioMode::App,
            app: Some("Spotify".into()),
            ..Default::default()
        };
        let sess = setup_session_with(&plan, &mut pulse).unwrap().unwrap();
        assert!(!sess.moved_apps.is_empty());
        assert!(pulse.moves.iter().any(|(i, s)| *i == 42 && s.starts_with("hyprcap_mix_")));
        teardown_session_with(&sess, &mut pulse);
        assert!(pulse
            .moves
            .iter()
            .any(|(i, s)| *i == 42 && s == "spk"));
    }
}
