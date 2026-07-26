//! CLI parsing and subcommand dispatch (no GTK/adw init).

use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use hyprcap::ipc::{IpcRequest, IpcResponse};
use hyprcap::server::{self, RuntimePaths};

/// hyprcap — native frontend for wf-recorder (Hyprland / wlroots).
#[derive(Debug, Parser)]
#[command(name = "hyprcap", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open / raise the GUI view (does not stop recording on close).
    Gui,
    /// Start region recording.
    Region {
        /// Enable system audio (default sink monitor). Legacy alias for `--system all`.
        #[arg(long)]
        audio: bool,
        /// System sound: off | all | app (default: config / off).
        #[arg(long, value_name = "MODE")]
        system: Option<String>,
        /// Sink name when `--system all` (default: Pulse default sink).
        #[arg(long, value_name = "SINK")]
        audio_sink: Option<String>,
        /// Application name when `--system app` (e.g. Spotify).
        #[arg(long, value_name = "NAME")]
        audio_app: Option<String>,
        /// Record microphone (mixed into one track with system sound).
        #[arg(long)]
        mic: bool,
        /// Mic source name (`pactl list short sources`).
        #[arg(long, value_name = "SOURCE")]
        mic_device: Option<String>,
    },
    /// Start one-monitor fullscreen recording (no region; always `wf-recorder -o`).
    Fullscreen {
        /// Enable system audio (default sink monitor).
        #[arg(long)]
        audio: bool,
        #[arg(long, value_name = "MODE")]
        system: Option<String>,
        #[arg(long, value_name = "SINK")]
        audio_sink: Option<String>,
        #[arg(long, value_name = "NAME")]
        audio_app: Option<String>,
        #[arg(long)]
        mic: bool,
        #[arg(long, value_name = "SOURCE")]
        mic_device: Option<String>,
        /// Wayland output name (`wf-recorder -o`). Required when multi-monitor
        /// and `fullscreen_output` is unset in config.
        #[arg(long)]
        output: Option<String>,
        /// Capture frame rate (`wf-recorder -r`). Omitted = use config `one_fps`
        /// if set, else Auto (no `-r`). Explicit `--fps N` overrides config for
        /// this start (`N > 0`; `0` is treated as Auto).
        #[arg(long)]
        fps: Option<u32>,
    },
    /// Start both-monitors recording (exactly 2 heads; layout-true compose after stop).
    Both {
        /// Enable system audio on the primary head only.
        #[arg(long)]
        audio: bool,
        #[arg(long, value_name = "MODE")]
        system: Option<String>,
        #[arg(long, value_name = "SINK")]
        audio_sink: Option<String>,
        #[arg(long, value_name = "NAME")]
        audio_app: Option<String>,
        #[arg(long)]
        mic: bool,
        #[arg(long, value_name = "SOURCE")]
        mic_device: Option<String>,
    },
    /// Toggle region: Idle→start, SelectingRegion→cancel, Recording→stop.
    ToggleRegion {
        /// Enable system audio when starting.
        #[arg(long)]
        audio: bool,
        #[arg(long, value_name = "MODE")]
        system: Option<String>,
        #[arg(long, value_name = "SINK")]
        audio_sink: Option<String>,
        #[arg(long, value_name = "NAME")]
        audio_app: Option<String>,
        #[arg(long)]
        mic: bool,
        #[arg(long, value_name = "SOURCE")]
        mic_device: Option<String>,
    },
    /// List known Wayland outputs (name + geometry/refresh when known; no daemon).
    ListOutputs,
    /// List audio sinks, mics, and playing apps (JSON).
    ListAudio,
    /// Stop recording / cancel selection (no-op success if idle).
    Stop,
    /// Print one JSON status object on stdout.
    Status,
    /// Stop if needed and shut down the session server.
    Quit,
}

/// Internal daemon mode: only `argv[1] == "--server"` (not anywhere in argv).
pub fn is_server_mode(args: &[String]) -> bool {
    args.get(1).map(|a| a.as_str()) == Some("--server")
}

pub fn run() -> i32 {
    let raw: Vec<String> = std::env::args().collect();
    if is_server_mode(&raw) {
        return run_server();
    }

    let cli = Cli::parse();
    let cmd = cli.command.unwrap_or(Command::Gui);

    match dispatch(cmd) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("hyprcap: {msg}");
            1
        }
    }
}

fn run_server() -> i32 {
    match server::run_production_server() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("hyprcap server: {e}");
            1
        }
    }
}

fn runtime_paths() -> Result<RuntimePaths, String> {
    RuntimePaths::from_env()
}

fn ensure_and_request(req: &IpcRequest) -> Result<IpcResponse, String> {
    let paths = runtime_paths()?;
    server::ensure_and_request(&paths, req)
}

fn cli_audio_plan(
    audio: bool,
    system: Option<String>,
    audio_sink: Option<String>,
    audio_app: Option<String>,
    mic: bool,
    mic_device: Option<String>,
) -> Option<hyprcap::audio::AudioPlan> {
    use hyprcap::audio::{AudioPlan, SystemAudioMode};
    let has_matrix = system.is_some()
        || audio_sink.is_some()
        || audio_app.is_some()
        || mic
        || mic_device.is_some();
    if !has_matrix && !audio {
        return None; // pure config defaults on server
    }
    let system_mode = if let Some(s) = system.as_deref() {
        SystemAudioMode::parse(s).unwrap_or(SystemAudioMode::Off)
    } else if audio {
        SystemAudioMode::All
    } else {
        SystemAudioMode::Off
    };
    Some(
        AudioPlan {
            system: system_mode,
            sink: audio_sink,
            app: audio_app,
            mic: mic || mic_device.is_some(),
            mic_device,
        }
        .normalized(),
    )
}

fn start_req_region(
    audio: bool,
    system: Option<String>,
    audio_sink: Option<String>,
    audio_app: Option<String>,
    mic: bool,
    mic_device: Option<String>,
) -> IpcRequest {
    if let Some(plan) = cli_audio_plan(audio, system, audio_sink, audio_app, mic, mic_device) {
        IpcRequest::start_region_plan(plan)
    } else {
        IpcRequest::start_region(None)
    }
}

fn dispatch(cmd: Command) -> Result<i32, String> {
    match cmd {
        Command::Gui => {
            // GTK/Adwaita init happens only inside `ui::run_gui` — never here for other cmds.
            Ok(crate::ui::run_gui())
        }
        Command::Region {
            audio,
            system,
            audio_sink,
            audio_app,
            mic,
            mic_device,
        } => {
            let resp = ensure_and_request(&start_req_region(
                audio, system, audio_sink, audio_app, mic, mic_device,
            ))?;
            print_message(&resp);
            Ok(exit_from(&resp))
        }
        Command::Fullscreen {
            audio,
            system,
            audio_sink,
            audio_app,
            mic,
            mic_device,
            output,
            fps,
        } => {
            let req = if let Some(plan) =
                cli_audio_plan(audio, system, audio_sink, audio_app, mic, mic_device)
            {
                IpcRequest::start_fullscreen_plan(plan, output, fps)
            } else {
                IpcRequest::start_fullscreen(None, output, fps)
            };
            let resp = ensure_and_request(&req)?;
            print_message(&resp);
            Ok(exit_from(&resp))
        }
        Command::Both {
            audio,
            system,
            audio_sink,
            audio_app,
            mic,
            mic_device,
        } => {
            let req = if let Some(plan) =
                cli_audio_plan(audio, system, audio_sink, audio_app, mic, mic_device)
            {
                IpcRequest::start_both_plan(plan)
            } else {
                IpcRequest::start_both(None)
            };
            let resp = ensure_and_request(&req)?;
            print_message(&resp);
            Ok(exit_from(&resp))
        }
        Command::ListOutputs => {
            // Prefer hyprctl rich inventory; names-only fallback prints name alone.
            let inv = hyprcap::sys::list_output_inventory();
            let mut out = std::io::stdout();
            for o in &inv {
                writeln!(out, "{}", o.display_line()).map_err(|e| e.to_string())?;
            }
            if inv.is_empty() {
                eprintln!("hyprcap: no outputs discovered (hyprctl monitors / wf-recorder -L)");
                Ok(1)
            } else {
                Ok(0)
            }
        }
        Command::ListAudio => {
            let inv = hyprcap::audio::list_audio_inventory().map_err(|e| e.to_string())?;
            let json = serde_json::to_string_pretty(&inv).map_err(|e| e.to_string())?;
            let mut out = std::io::stdout();
            writeln!(out, "{json}").map_err(|e| e.to_string())?;
            Ok(0)
        }
        Command::ToggleRegion {
            audio,
            system,
            audio_sink,
            audio_app,
            mic,
            mic_device,
        } => {
            // Toggle uses legacy audio bool; matrix flags map to --audio when system all.
            let plan = cli_audio_plan(audio, system, audio_sink, audio_app, mic, mic_device);
            let audio_flag = plan
                .as_ref()
                .map(|p| p.enabled())
                .unwrap_or(false);
            // Prefer full plan via start when idle would need matrix — server toggle only
            // takes legacy bool; if plan is rich, use start_region_plan when idle via
            // a start request is better. Keep toggle simple: --audio / plan enabled.
            let resp = if let Some(p) = plan.filter(|p| {
                p.mic
                    || p.system == hyprcap::audio::SystemAudioMode::App
                    || p.sink.is_some()
                    || p.mic_device.is_some()
            }) {
                // Complex plan: emulate toggle with status then start/stop.
                let st = ensure_and_request(&IpcRequest::status())?;
                match st.status.state.as_str() {
                    "Idle" => ensure_and_request(&IpcRequest::start_region_plan(p))?,
                    _ => ensure_and_request(&IpcRequest::stop())?,
                }
            } else {
                ensure_and_request(&IpcRequest::toggle_region(if audio_flag {
                    Some(true)
                } else {
                    None
                }))?
            };
            print_message(&resp);
            Ok(exit_from(&resp))
        }
        Command::Stop => {
            let resp = ensure_and_request(&IpcRequest::stop())?;
            print_message(&resp);
            Ok(exit_from(&resp))
        }
        Command::Status => {
            let resp = ensure_and_request(&IpcRequest::status())?;
            let json = serde_json::to_string(&resp.status).map_err(|e| e.to_string())?;
            let mut out = std::io::stdout();
            writeln!(out, "{json}").map_err(|e| e.to_string())?;
            Ok(exit_from(&resp))
        }
        Command::Quit => {
            let paths = match runtime_paths() {
                Ok(p) => p,
                Err(_) => return Ok(0),
            };
            if std::os::unix::net::UnixStream::connect(&paths.socket_path).is_err() {
                return Ok(0);
            }
            match server::ensure_and_request(&paths, &IpcRequest::shutdown()) {
                Ok(resp) => {
                    print_message(&resp);
                    Ok(exit_from(&resp))
                }
                // Server went away — already quit.
                Err(_) => Ok(0),
            }
        }
    }
}

fn print_message(resp: &IpcResponse) {
    if !resp.message.is_empty() {
        // Human-friendly line on stderr; status keeps stdout for JSON only.
        eprintln!("{}", resp.message);
    }
    for w in &resp.warnings {
        eprintln!("warning: {w}");
    }
}

fn exit_from(resp: &IpcResponse) -> i32 {
    resp.cli_exit_code()
}

/// Resolve runtime dir for tests / tooling.
#[allow(dead_code)]
pub fn runtime_dir_from_env() -> PathBuf {
    runtime_paths()
        .map(|p| p.runtime_dir)
        .unwrap_or_else(|_| PathBuf::from("/tmp/hyprcap-fallback"))
}

#[cfg(test)]
mod tests {
    use super::is_server_mode;

    #[test]
    fn server_mode_only_argv1() {
        let args = vec!["hyprcap".into(), "--server".into()];
        assert!(is_server_mode(&args));
        let args = vec!["hyprcap".into(), "status".into(), "--server".into()];
        assert!(!is_server_mode(&args));
        let args = vec!["hyprcap".into(), "status".into()];
        assert!(!is_server_mode(&args));
    }
}
