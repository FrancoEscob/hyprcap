//! CLI parsing and subcommand dispatch (no GTK/adw init).

use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use record_ui::ipc::{IpcRequest, IpcResponse};
use record_ui::server::{self, RuntimePaths};

/// record-ui — native frontend for wf-recorder (Hyprland / wlroots).
#[derive(Debug, Parser)]
#[command(name = "record-ui", version, about)]
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
        /// Enable system audio (`wf-recorder -a`).
        #[arg(long)]
        audio: bool,
    },
    /// Start fullscreen recording (no region).
    Fullscreen {
        #[arg(long)]
        audio: bool,
    },
    /// Toggle region: Idle→start, SelectingRegion→cancel, Recording→stop.
    ToggleRegion {
        #[arg(long)]
        audio: bool,
    },
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
            eprintln!("record-ui: {msg}");
            1
        }
    }
}

fn run_server() -> i32 {
    match server::run_production_server() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("record-ui server: {e}");
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

fn dispatch(cmd: Command) -> Result<i32, String> {
    match cmd {
        Command::Gui => {
            // GTK/Adwaita init happens only inside `ui::run_gui` — never here for other cmds.
            Ok(crate::ui::run_gui())
        }
        Command::Region { audio } => {
            let audio = if audio { Some(true) } else { None };
            let resp = ensure_and_request(&IpcRequest::start_region(audio))?;
            print_message(&resp);
            Ok(exit_from(&resp))
        }
        Command::Fullscreen { audio } => {
            let audio = if audio { Some(true) } else { None };
            let resp = ensure_and_request(&IpcRequest::start_fullscreen(audio))?;
            print_message(&resp);
            Ok(exit_from(&resp))
        }
        Command::ToggleRegion { audio } => {
            let audio = if audio { Some(true) } else { None };
            let resp = ensure_and_request(&IpcRequest::toggle_region(audio))?;
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
        .unwrap_or_else(|_| PathBuf::from("/tmp/record-ui-fallback"))
}

#[cfg(test)]
mod tests {
    use super::is_server_mode;

    #[test]
    fn server_mode_only_argv1() {
        let args = vec!["record-ui".into(), "--server".into()];
        assert!(is_server_mode(&args));
        let args = vec!["record-ui".into(), "status".into(), "--server".into()];
        assert!(!is_server_mode(&args));
        let args = vec!["record-ui".into(), "status".into()];
        assert!(!is_server_mode(&args));
    }
}
