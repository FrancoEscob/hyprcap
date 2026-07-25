//! IPC client over `$XDG_RUNTIME_DIR/hyprcap.sock`.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::ipc::{decode_response, encode_request, IpcRequest, IpcResponse};
use crate::server::MAX_LINE_BYTES;

/// Client-side errors.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("connect failed: {0}")]
    Connect(std::io::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// Send one request and read one response line.
///
/// Read timeout is long (10 minutes) so region selection can wait on slurp while
/// other clients cancel via separate connections. No write/read size above
/// [`MAX_LINE_BYTES`].
pub fn request(socket_path: &Path, req: &IpcRequest) -> Result<IpcResponse, ClientError> {
    let mut stream = UnixStream::connect(socket_path).map_err(ClientError::Connect)?;
    // Long timeout: start_region parks until selection completes or is cancelled.
    stream.set_read_timeout(Some(Duration::from_secs(600)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    let line = encode_request(req).map_err(|e| ClientError::Protocol(e.to_string()))?;
    if line.len() > MAX_LINE_BYTES {
        return Err(ClientError::Protocol("request too large".into()));
    }
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    let mut take = reader.by_ref().take(MAX_LINE_BYTES as u64 + 1);
    let n = take.read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Err(ClientError::Protocol("empty response from server".into()));
    }
    if buf.len() > MAX_LINE_BYTES {
        return Err(ClientError::Protocol("response line too large".into()));
    }
    let resp_line = String::from_utf8_lossy(&buf);
    decode_response(&resp_line).map_err(|e| ClientError::Protocol(e.to_string()))
}

/// Convenience: connect using runtime dir (socket name fixed).
pub fn request_in_runtime(
    runtime_dir: &Path,
    req: &IpcRequest,
) -> Result<IpcResponse, ClientError> {
    request(&runtime_dir.join(crate::server::SOCKET_NAME), req)
}

/// Open a long-lived GUI subscribe connection.
///
/// The returned stream **must be kept open** for the lifetime of the GUI view so
/// the server keeps `gui_clients` elevated (idle-exit / notify-on-start policy).
/// Dropping the stream disconnects the view only — it does **not** stop recording.
pub fn subscribe(socket_path: &Path) -> Result<(UnixStream, IpcResponse), ClientError> {
    let mut stream = UnixStream::connect(socket_path).map_err(ClientError::Connect)?;
    // No short read timeout: we hold the connection open; server does not push.
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    let req = IpcRequest::subscribe();
    let line = encode_request(&req).map_err(|e| ClientError::Protocol(e.to_string()))?;
    if line.len() > MAX_LINE_BYTES {
        return Err(ClientError::Protocol("request too large".into()));
    }
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    // Read one response line, leave the socket open for the server hold.
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut buf = Vec::new();
    let mut take = reader.by_ref().take(MAX_LINE_BYTES as u64 + 1);
    let n = take.read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Err(ClientError::Protocol("empty response from server".into()));
    }
    if buf.len() > MAX_LINE_BYTES {
        return Err(ClientError::Protocol("response line too large".into()));
    }
    let resp_line = String::from_utf8_lossy(&buf);
    let resp = decode_response(&resp_line).map_err(|e| ClientError::Protocol(e.to_string()))?;
    Ok((stream, resp))
}
