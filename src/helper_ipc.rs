use crate::entirely::EntirelyCoordinator;
use crate::process_util;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const HELPER_SOCKET_PATH: &str = "/var/run/caffeinate2.sock";

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum HelperRequest {
    Hold,
    Release,
    Status,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelperResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holders: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_disabled: Option<bool>,
}

impl HelperResponse {
    pub fn success() -> Self {
        Self {
            ok: true,
            error: None,
            holders: None,
            sleep_disabled: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            holders: None,
            sleep_disabled: None,
        }
    }
}

pub fn encode_request(request: &HelperRequest) -> String {
    serde_json::to_string(request).expect("request should serialize") + "\n"
}

pub fn decode_request(line: &str) -> Result<HelperRequest, serde_json::Error> {
    serde_json::from_str(line.trim())
}

pub fn encode_response(response: &HelperResponse) -> String {
    serde_json::to_string(response).expect("response should serialize") + "\n"
}

pub fn decode_response(line: &str) -> Result<HelperResponse, serde_json::Error> {
    serde_json::from_str(line.trim())
}

pub struct HelperClient {
    socket_path: String,
}

impl HelperClient {
    pub fn new() -> Self {
        Self {
            socket_path: HELPER_SOCKET_PATH.to_string(),
        }
    }

    pub fn is_available(&self) -> bool {
        Path::new(&self.socket_path).exists()
            && UnixStream::connect(&self.socket_path).is_ok()
    }

    fn request(&self, request: HelperRequest) -> Result<HelperResponse, String> {
        let mut stream =
            UnixStream::connect(&self.socket_path).map_err(|e| format!("connect failed: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| format!("set timeout: {e}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| format!("set timeout: {e}"))?;

        stream
            .write_all(encode_request(&request).as_bytes())
            .map_err(|e| format!("write failed: {e}"))?;
        stream.flush().map_err(|e| format!("flush failed: {e}"))?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("read failed: {e}"))?;
        decode_response(&line).map_err(|e| format!("invalid response: {e}"))
    }

    pub fn hold(&self) -> Result<(), String> {
        let response = self.request(HelperRequest::Hold)?;
        if response.ok {
            Ok(())
        } else {
            Err(response.error.unwrap_or_else(|| "hold failed".to_string()))
        }
    }

    pub fn release(&self) -> Result<(), String> {
        let response = self.request(HelperRequest::Release)?;
        if response.ok {
            Ok(())
        } else {
            Err(response
                .error
                .unwrap_or_else(|| "release failed".to_string()))
        }
    }

    pub fn status(&self) -> Result<(usize, bool), String> {
        let response = self.request(HelperRequest::Status)?;
        if !response.ok {
            return Err(response
                .error
                .unwrap_or_else(|| "status failed".to_string()));
        }
        Ok((
            response.holders.unwrap_or(0),
            response.sleep_disabled.unwrap_or(false),
        ))
    }
}

pub struct HelperHoldGuard {
    client: HelperClient,
    held: bool,
}

impl HelperHoldGuard {
    pub fn acquire() -> Result<Self, String> {
        let client = HelperClient::new();
        client.hold()?;
        Ok(Self {
            client,
            held: true,
        })
    }
}

impl Drop for HelperHoldGuard {
    fn drop(&mut self) {
        if self.held {
            if let Err(e) = self.client.release() {
                eprintln!("Error releasing helper hold: {e}");
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub fn peer_process_id(stream: &UnixStream) -> Result<crate::lockfile::ProcessId, String> {
    use std::os::unix::io::AsRawFd;

    let mut pid: libc::pid_t = 0;
    let fd = stream.as_raw_fd();
    let ret = unsafe {
        libc::getpeereid(
            fd,
            &mut pid as *mut libc::pid_t,
            std::ptr::null_mut(),
        )
    };
    if ret != 0 {
        return Err(format!(
            "getpeereid failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    process_util::process_id_from_pid(pid as i32).map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
pub fn serve_connection(
    stream: UnixStream,
    coordinator: &Arc<EntirelyCoordinator>,
) -> Result<(), String> {
    use std::io::BufRead;

    let peer = match peer_process_id(&stream) {
        Ok(id) => id,
        Err(e) => return write_response_on_stream(stream, HelperResponse::err(e)),
    };

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return write_response_on_stream(stream, HelperResponse::err("failed to read request"));
    }

    let request = match decode_request(&line) {
        Ok(req) => req,
        Err(e) => {
            return write_response_on_stream(
                stream,
                HelperResponse::err(format!("invalid request: {e}")),
            );
        }
    };

    let response = match request {
        HelperRequest::Hold => match coordinator.hold(peer) {
            Ok(()) => HelperResponse::success(),
            Err(e) => HelperResponse::err(e.to_string()),
        },
        HelperRequest::Release => match coordinator.release(peer) {
            Ok(()) => HelperResponse::success(),
            Err(e) => HelperResponse::err(e.to_string()),
        },
        HelperRequest::Status => {
            let holders = coordinator.holder_count().unwrap_or(0);
            HelperResponse {
                ok: true,
                error: None,
                holders: Some(holders),
                sleep_disabled: Some(holders > 0),
            }
        }
    };

    write_response_on_stream(stream, response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_request() {
        let req = HelperRequest::Hold;
        let decoded = decode_request(&encode_request(&req)).unwrap();
        assert_eq!(decoded, req);
    }

    #[test]
    fn round_trip_status_response() {
        let resp = HelperResponse {
            ok: true,
            error: None,
            holders: Some(2),
            sleep_disabled: Some(true),
        };
        let decoded = decode_response(&encode_response(&resp)).unwrap();
        assert_eq!(decoded, resp);
    }
}

#[cfg(target_os = "macos")]
fn write_response_on_stream(
    mut stream: UnixStream,
    response: HelperResponse,
) -> Result<(), String> {
    stream
        .write_all(encode_response(&response).as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())
}
