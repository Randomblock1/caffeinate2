use crate::entirely::EntirelyCoordinator;
use crate::process_util;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

pub const HELPER_SOCKET_PATH: &str = "/var/run/caffeinate2.sock";

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum HelperRequest {
    Hold,
    Status,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HelperResponse {
    HoldOk,
    Status {
        holders: usize,
        sleep_disabled: bool,
    },
    Error {
        message: String,
    },
}

impl HelperResponse {
    pub fn into_result(self) -> Result<Self, String> {
        match self {
            HelperResponse::Error { message } => Err(message),
            other => Ok(other),
        }
    }

    pub fn into_hold_ok(self) -> Result<(), String> {
        match self.into_result()? {
            HelperResponse::HoldOk => Ok(()),
            HelperResponse::Status { .. } => Err("unexpected status response for hold".to_string()),
            HelperResponse::Error { message } => Err(message),
        }
    }

    pub fn into_status(self) -> Result<(usize, bool), String> {
        match self.into_result()? {
            HelperResponse::Status {
                holders,
                sleep_disabled,
            } => Ok((holders, sleep_disabled)),
            HelperResponse::HoldOk => Err("unexpected hold_ok response for status".to_string()),
            HelperResponse::Error { message } => Err(message),
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

fn configure_rpc_timeouts(stream: &UnixStream) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("set timeout: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("set timeout: {e}"))
}

fn write_request(stream: &mut UnixStream, request: &HelperRequest) -> Result<(), String> {
    stream
        .write_all(encode_request(request).as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    stream.flush().map_err(|e| format!("flush failed: {e}"))
}

fn read_line(stream: &mut UnixStream) -> Result<String, String> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read failed: {e}"))?;
    Ok(line)
}

fn read_response_line(stream: &mut UnixStream) -> Result<HelperResponse, String> {
    let line = read_line(stream)?;
    decode_response(&line).map_err(|e| format!("invalid response: {e}"))
}

fn read_request_line(stream: &mut UnixStream) -> Result<HelperRequest, String> {
    let line = read_line(stream)?;
    decode_request(&line).map_err(|e| format!("invalid request: {e}"))
}

fn rpc(
    socket_path: &str,
    request: HelperRequest,
    with_timeouts: bool,
) -> Result<HelperResponse, String> {
    let mut stream =
        UnixStream::connect(socket_path).map_err(|e| format!("connect failed: {e}"))?;
    if with_timeouts {
        configure_rpc_timeouts(&stream)?;
    }
    write_request(&mut stream, &request)?;
    read_response_line(&mut stream)
}

#[derive(Clone)]
pub struct HelperClient {
    socket_path: String,
}

impl HelperClient {
    pub fn new() -> Self {
        Self {
            socket_path: HELPER_SOCKET_PATH.to_string(),
        }
    }

    pub fn try_connect(&self) -> Result<UnixStream, String> {
        UnixStream::connect(&self.socket_path).map_err(|e| format!("connect failed: {e}"))
    }

    pub fn is_available(&self) -> bool {
        self.try_connect().is_ok()
    }

    pub fn status(&self) -> Result<(usize, bool), String> {
        rpc(&self.socket_path, HelperRequest::Status, true)?.into_status()
    }
}

pub fn is_connect_error(message: &str) -> bool {
    message.starts_with("connect failed:")
}

/// Keeps a helper hold alive for as long as this guard owns the connection.
pub struct HelperHoldGuard {
    #[allow(dead_code)]
    stream: UnixStream,
}

impl HelperHoldGuard {
    pub fn try_acquire(client: &HelperClient) -> Result<Self, String> {
        let mut stream = client.try_connect()?;
        write_request(&mut stream, &HelperRequest::Hold)?;
        read_response_line(&mut stream)?.into_hold_ok()?;
        Ok(Self { stream })
    }
}

#[cfg(target_os = "macos")]
pub fn peer_process_id(stream: &UnixStream) -> Result<crate::lockfile::ProcessId, String> {
    use nix::sys::socket::getsockopt;
    use nix::sys::socket::sockopt::LocalPeerPid;

    let pid = getsockopt(stream, LocalPeerPid).map_err(|e| e.to_string())?;
    process_util::process_id_from_pid(pid).map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
pub fn serve_connection(
    mut stream: UnixStream,
    coordinator: &Arc<EntirelyCoordinator>,
) -> Result<(), String> {
    let peer = match peer_process_id(&stream) {
        Ok(id) => id,
        Err(e) => {
            return write_response_on_stream(
                &mut stream,
                HelperResponse::Error { message: e },
            );
        }
    };

    let request = match read_request_line(&mut stream) {
        Ok(req) => req,
        Err(e) => {
            return write_response_on_stream(
                &mut stream,
                HelperResponse::Error { message: e },
            );
        }
    };

    match request {
        HelperRequest::Hold => serve_hold_connection(stream, coordinator, peer),
        HelperRequest::Status => {
            let response = match coordinator.status() {
                Ok(status) => HelperResponse::Status {
                    holders: status.holders,
                    sleep_disabled: status.sleep_disabled,
                },
                Err(e) => HelperResponse::Error {
                    message: e.to_string(),
                },
            };
            write_response_on_stream(&mut stream, response)
        }
    }
}

#[cfg(target_os = "macos")]
fn serve_hold_connection(
    mut stream: UnixStream,
    coordinator: &Arc<EntirelyCoordinator>,
    peer: crate::lockfile::ProcessId,
) -> Result<(), String> {
    if let Err(e) = coordinator.hold(peer) {
        return write_response_on_stream(
            &mut stream,
            HelperResponse::Error {
                message: e.to_string(),
            },
        );
    }

    if let Err(e) = write_response_on_stream(&mut stream, HelperResponse::HoldOk) {
        let _ = coordinator.release(peer);
        return Err(e);
    }

    let mut reader = BufReader::new(&stream);
    let mut buf = [0u8; 256];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    if let Err(e) = coordinator.release(peer) {
        eprintln!("Error releasing helper hold for peer {peer}: {e}");
    }

    Ok(())
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
        let resp = HelperResponse::Status {
            holders: 2,
            sleep_disabled: true,
        };
        let decoded = decode_response(&encode_response(&resp)).unwrap();
        assert_eq!(decoded, resp);
    }

    #[test]
    fn connect_error_detection() {
        assert!(is_connect_error("connect failed: No such file"));
        assert!(!is_connect_error("hold failed"));
    }

    #[test]
    fn error_response_into_result() {
        let resp = HelperResponse::Error {
            message: "nope".to_string(),
        };
        assert_eq!(resp.into_result().unwrap_err(), "nope");
    }

    #[test]
    fn status_response_into_status() {
        let resp = HelperResponse::Status {
            holders: 3,
            sleep_disabled: false,
        };
        assert_eq!(resp.into_status().unwrap(), (3, false));
    }

    #[test]
    fn hold_ok_response_into_hold_ok() {
        assert!(HelperResponse::HoldOk.into_hold_ok().is_ok());
    }
}

#[cfg(target_os = "macos")]
fn write_response_on_stream(
    stream: &mut UnixStream,
    response: HelperResponse,
) -> Result<(), String> {
    stream
        .write_all(encode_response(&response).as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())
}
