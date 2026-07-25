//! Newline-delimited JSON IPC protocol (SPEC v1).
//!
//! One request object per line; one response object per line.
//!
//! Commands include `toggle_region` for CLI efficiency (normative CLI surface);
//! the SPEC request table lists the core set and is not re-edited here.

use serde::{Deserialize, Serialize};

use crate::recorder::{CommandResult, MachineCode, Status};

/// IPC request command names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcCommand {
    Ping,
    Status,
    StartRegion,
    StartFullscreen,
    Stop,
    ToggleRegion,
    Shutdown,
    Subscribe,
}

/// Client → server request (one JSON object per line).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcRequest {
    pub cmd: IpcCommand,
    /// Optional audio override for start / toggle-region.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<bool>,
    /// When true (typically with `Subscribe`), the server counts this connection
    /// as a GUI view until disconnect (idle-exit / notify-on-start policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gui: Option<bool>,
    /// Optional Wayland output for `StartFullscreen` (`wf-recorder -o`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

impl IpcRequest {
    pub fn ping() -> Self {
        Self {
            cmd: IpcCommand::Ping,
            audio: None,
            gui: None,
            output: None,
        }
    }

    pub fn status() -> Self {
        Self {
            cmd: IpcCommand::Status,
            audio: None,
            gui: None,
            output: None,
        }
    }

    pub fn start_region(audio: Option<bool>) -> Self {
        Self {
            cmd: IpcCommand::StartRegion,
            audio,
            gui: None,
            output: None,
        }
    }

    pub fn start_fullscreen(audio: Option<bool>, output: Option<String>) -> Self {
        Self {
            cmd: IpcCommand::StartFullscreen,
            audio,
            gui: None,
            output,
        }
    }

    pub fn stop() -> Self {
        Self {
            cmd: IpcCommand::Stop,
            audio: None,
            gui: None,
            output: None,
        }
    }

    pub fn toggle_region(audio: Option<bool>) -> Self {
        Self {
            cmd: IpcCommand::ToggleRegion,
            audio,
            gui: None,
            output: None,
        }
    }

    pub fn shutdown() -> Self {
        Self {
            cmd: IpcCommand::Shutdown,
            audio: None,
            gui: None,
            output: None,
        }
    }

    /// GUI attach: server counts this connection until disconnect.
    pub fn subscribe() -> Self {
        Self {
            cmd: IpcCommand::Subscribe,
            audio: None,
            gui: Some(true),
            output: None,
        }
    }
}

/// Status snapshot embedded in every response / printed by `record-ui status`.
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
    /// Resolved one-monitor output name while Starting/Recording/Stopping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_output: Option<String>,
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
        }
    }
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
}

impl IpcResponse {
    pub fn from_command_result(result: &CommandResult, status: &Status) -> Self {
        Self {
            ok: result.ok,
            code: result.code.as_str().to_string(),
            message: result.message.clone(),
            status: IpcStatus::from(status),
            warnings: result.warnings.clone(),
        }
    }

    pub fn ok_status(message: impl Into<String>, status: &Status) -> Self {
        Self {
            ok: true,
            code: MachineCode::Ok.as_str().to_string(),
            message: message.into(),
            status: IpcStatus::from(status),
            warnings: Vec::new(),
        }
    }

    pub fn err(code: MachineCode, message: impl Into<String>, status: &Status) -> Self {
        Self {
            ok: false,
            code: code.as_str().to_string(),
            message: message.into(),
            status: IpcStatus::from(status),
            warnings: Vec::new(),
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
