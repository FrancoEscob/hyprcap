//! Newline-delimited JSON IPC protocol (SPEC v1).
//!
//! One request object per line; one response object per line.
//!
//! Commands include `toggle_region` for CLI efficiency (normative CLI surface);
//! the SPEC request table lists the core set and is not re-edited here.

use serde::{Deserialize, Serialize};

use crate::audio::{AudioInventory, AudioPlan};
use crate::recorder::{CommandResult, MachineCode, Status};

/// IPC request command names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcCommand {
    Ping,
    Status,
    StartRegion,
    StartFullscreen,
    /// Dual-monitor Both session (layout from live inventory; optional audio only).
    StartBoth,
    Stop,
    ToggleRegion,
    Shutdown,
    Subscribe,
    /// Enumerate sinks / mics / playing apps (Pulse/PipeWire via pactl).
    ListAudio,
}

/// Client → server request (one JSON object per line).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcRequest {
    pub cmd: IpcCommand,
    /// Optional audio override for start / toggle-region (legacy boolean).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<bool>,
    /// Full audio matrix (system / app / mic). Wins over `audio` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_plan: Option<AudioPlan>,
    /// When true (typically with `Subscribe`), the server counts this connection
    /// as a GUI view until disconnect (idle-exit / notify-on-start policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gui: Option<bool>,
    /// Optional Wayland output for `StartFullscreen` (`wf-recorder -o`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Optional FPS for `StartFullscreen` (`wf-recorder -r`).
    /// Omit / null = use config `one_fps` if set, else Auto (no `-r`).
    /// Explicit number overrides config for this start (`0` treated as Auto).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
}

impl IpcRequest {
    fn bare(cmd: IpcCommand) -> Self {
        Self {
            cmd,
            audio: None,
            audio_plan: None,
            gui: None,
            output: None,
            fps: None,
        }
    }

    pub fn ping() -> Self {
        Self::bare(IpcCommand::Ping)
    }

    pub fn status() -> Self {
        Self::bare(IpcCommand::Status)
    }

    pub fn list_audio() -> Self {
        Self::bare(IpcCommand::ListAudio)
    }

    pub fn start_region(audio: Option<bool>) -> Self {
        Self {
            cmd: IpcCommand::StartRegion,
            audio,
            audio_plan: None,
            gui: None,
            output: None,
            fps: None,
        }
    }

    pub fn start_region_plan(plan: AudioPlan) -> Self {
        Self {
            cmd: IpcCommand::StartRegion,
            audio: None,
            audio_plan: Some(plan),
            gui: None,
            output: None,
            fps: None,
        }
    }

    pub fn start_fullscreen(audio: Option<bool>, output: Option<String>, fps: Option<u32>) -> Self {
        Self {
            cmd: IpcCommand::StartFullscreen,
            audio,
            audio_plan: None,
            gui: None,
            output,
            fps,
        }
    }

    pub fn start_fullscreen_plan(
        plan: AudioPlan,
        output: Option<String>,
        fps: Option<u32>,
    ) -> Self {
        Self {
            cmd: IpcCommand::StartFullscreen,
            audio: None,
            audio_plan: Some(plan),
            gui: None,
            output,
            fps,
        }
    }

    /// Start Both: optional `audio` only (layout from live inventory).
    pub fn start_both(audio: Option<bool>) -> Self {
        Self {
            cmd: IpcCommand::StartBoth,
            audio,
            audio_plan: None,
            gui: None,
            output: None,
            fps: None,
        }
    }

    pub fn start_both_plan(plan: AudioPlan) -> Self {
        Self {
            cmd: IpcCommand::StartBoth,
            audio: None,
            audio_plan: Some(plan),
            gui: None,
            output: None,
            fps: None,
        }
    }

    pub fn stop() -> Self {
        Self::bare(IpcCommand::Stop)
    }

    pub fn toggle_region(audio: Option<bool>) -> Self {
        Self {
            cmd: IpcCommand::ToggleRegion,
            audio,
            audio_plan: None,
            gui: None,
            output: None,
            fps: None,
        }
    }

    pub fn shutdown() -> Self {
        Self::bare(IpcCommand::Shutdown)
    }

    /// GUI attach: server counts this connection until disconnect.
    pub fn subscribe() -> Self {
        Self {
            cmd: IpcCommand::Subscribe,
            audio: None,
            audio_plan: None,
            gui: Some(true),
            output: None,
            fps: None,
        }
    }

    /// Resolve effective audio plan for a start command.
    pub fn resolved_audio_plan(&self, config: &crate::config::Config) -> AudioPlan {
        AudioPlan::resolve(self.audio_plan.clone(), self.audio, config)
    }
}

/// Status snapshot embedded in every response / printed by `hyprcap status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcStatus {
    pub state: String,
    pub output_path: Option<String>,
    pub pid: Option<u32>,
    pub started_at_unix: Option<u64>,
    pub audio: bool,
    pub last_error: Option<String>,
    pub last_success_path: Option<String>,
    pub elapsed_ms: Option<u64>,
    /// Resolved one-monitor / Both output label while Starting/Recording/Stopping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_output: Option<String>,
    /// `"region" | "one" | "both"` while Starting/Recording/Stopping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_mode: Option<String>,
}

impl From<&Status> for IpcStatus {
    fn from(s: &Status) -> Self {
        Self {
            state: s.state.as_str().to_string(),
            output_path: s.output_path.as_ref().map(|p| p.display().to_string()),
            pid: s.pid,
            started_at_unix: s.started_at_unix,
            audio: s.audio,
            last_error: s.last_error.clone(),
            last_success_path: s
                .last_success_path
                .as_ref()
                .map(|p| p.display().to_string()),
            elapsed_ms: s.elapsed_ms,
            capture_output: s.capture_output.clone(),
            capture_mode: s.capture_mode.clone(),
        }
    }
}

/// Response payload for `list_audio`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcAudioList {
    #[serde(flatten)]
    pub inventory: AudioInventory,
}

/// Server → client response (one JSON object per line).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    pub code: String,
    pub message: String,
    pub status: IpcStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Present for `list_audio` responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_list: Option<AudioInventory>,
}

impl IpcResponse {
    pub fn from_command_result(result: &CommandResult, status: &Status) -> Self {
        Self {
            ok: result.ok,
            code: result.code.as_str().to_string(),
            message: result.message.clone(),
            status: IpcStatus::from(status),
            warnings: result.warnings.clone(),
            audio_list: None,
        }
    }

    pub fn ok_status(message: impl Into<String>, status: &Status) -> Self {
        Self {
            ok: true,
            code: MachineCode::Ok.as_str().to_string(),
            message: message.into(),
            status: IpcStatus::from(status),
            warnings: Vec::new(),
            audio_list: None,
        }
    }

    pub fn err(code: MachineCode, message: impl Into<String>, status: &Status) -> Self {
        Self {
            ok: false,
            code: code.as_str().to_string(),
            message: message.into(),
            status: IpcStatus::from(status),
            warnings: Vec::new(),
            audio_list: None,
        }
    }

    pub fn audio_list(inv: AudioInventory, status: &Status) -> Self {
        Self {
            ok: true,
            code: MachineCode::Ok.as_str().to_string(),
            message: "audio inventory".into(),
            status: IpcStatus::from(status),
            warnings: Vec::new(),
            audio_list: Some(inv),
        }
    }

    pub fn machine_code(&self) -> MachineCode {
        match self.code.as_str() {
            "ok" => MachineCode::Ok,
            "busy" => MachineCode::Busy,
            "not_recording" => MachineCode::NotRecording,
            "dep_missing" => MachineCode::DepMissing,
            "slurp_cancel" => MachineCode::SlurpCancel,
            "spawn_failed" => MachineCode::SpawnFailed,
            "stop_timeout" => MachineCode::StopTimeout,
            "io_error" => MachineCode::IoError,
            _ => MachineCode::Invalid,
        }
    }

    pub fn cli_exit_code(&self) -> i32 {
        self.machine_code().exit_code()
    }
}

/// Encode a request as a single newline-terminated JSON line.
pub fn encode_request(req: &IpcRequest) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(req)?;
    s.push('\n');
    Ok(s)
}

/// Encode a response as a single newline-terminated JSON line.
pub fn encode_response(resp: &IpcResponse) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(resp)?;
    s.push('\n');
    Ok(s)
}

/// Parse one request line (trailing newline optional).
pub fn decode_request(line: &str) -> Result<IpcRequest, serde_json::Error> {
    serde_json::from_str(line.trim())
}

/// Parse one response line (trailing newline optional).
pub fn decode_response(line: &str) -> Result<IpcResponse, serde_json::Error> {
    serde_json::from_str(line.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_start_region() {
        let req = IpcRequest::start_region(Some(true));
        let line = encode_request(&req).unwrap();
        assert!(line.ends_with('\n'));
        let back = decode_request(&line).unwrap();
        assert_eq!(back.cmd, IpcCommand::StartRegion);
        assert_eq!(back.audio, Some(true));
    }

    #[test]
    fn roundtrip_start_fullscreen_output_fps() {
        let req = IpcRequest::start_fullscreen(Some(true), Some("DP-1".into()), Some(60));
        let line = encode_request(&req).unwrap();
        let back = decode_request(&line).unwrap();
        assert_eq!(back.cmd, IpcCommand::StartFullscreen);
        assert_eq!(back.audio, Some(true));
        assert_eq!(back.output.as_deref(), Some("DP-1"));
        assert_eq!(back.fps, Some(60));
        // Omitted fps deserializes as None (config one_fps or Auto).
        let bare = decode_request(r#"{"cmd":"start_fullscreen","output":"eDP-1"}"#).unwrap();
        assert_eq!(bare.fps, None);
        assert_eq!(bare.output.as_deref(), Some("eDP-1"));
        // Explicit JSON null also means unset (SPEC §8.2 number or null).
        let null_fps =
            decode_request(r#"{"cmd":"start_fullscreen","fps":null,"output":"DP-1"}"#).unwrap();
        assert_eq!(null_fps.fps, None);
        assert_eq!(null_fps.output.as_deref(), Some("DP-1"));
    }

    #[test]
    fn roundtrip_start_both() {
        let req = IpcRequest::start_both(Some(true));
        let line = encode_request(&req).unwrap();
        let back = decode_request(&line).unwrap();
        assert_eq!(back.cmd, IpcCommand::StartBoth);
        assert_eq!(back.audio, Some(true));
        assert!(back.output.is_none());
        assert!(back.fps.is_none());
        let bare = decode_request(r#"{"cmd":"start_both"}"#).unwrap();
        assert_eq!(bare.cmd, IpcCommand::StartBoth);
        assert_eq!(bare.audio, None);
    }

    #[test]
    fn response_exit_codes() {
        let st = Status {
            state: crate::recorder::State::Idle,
            output_path: None,
            pid: None,
            started_at_unix: None,
            audio: false,
            last_error: None,
            last_success_path: None,
            elapsed_ms: None,
            capture_output: None,
            capture_mode: None,
        };
        let cases = [
            (MachineCode::Ok, 0),
            (MachineCode::SlurpCancel, 0),
            (MachineCode::Busy, 2),
            (MachineCode::NotRecording, 3),
            (MachineCode::DepMissing, 4),
            (MachineCode::SpawnFailed, 1),
            (MachineCode::StopTimeout, 1),
            (MachineCode::IoError, 1),
            (MachineCode::Invalid, 1),
        ];
        for (code, exit) in cases {
            let resp = IpcResponse::err(code, "x", &st);
            // Ok/SlurpCancel may be ok:true in real use; exit_code is what matters.
            assert_eq!(code.exit_code(), exit, "MachineCode::{code:?} exit");
            assert_eq!(resp.cli_exit_code(), exit, "IpcResponse {code:?}");
        }
        let cancel = IpcResponse {
            ok: true,
            code: "slurp_cancel".into(),
            message: "cancel".into(),
            status: IpcStatus::from(&st),
            warnings: vec![],
            audio_list: None,
        };
        assert_eq!(cancel.cli_exit_code(), 0);
    }

    #[test]
    fn malformed_json_fails_decode() {
        assert!(decode_request("{not json").is_err());
        assert!(decode_request("{}").is_err()); // missing cmd
        assert!(decode_request(r#"{"cmd":"nope"}"#).is_err());
    }
}
