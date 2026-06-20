use crate::entirely::coordinator::EntirelyCoordinator;
use crate::entirely::process_util;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

pub const HELPER_SOCKET_PATH: &str = "/var/run/caffeinate2.sock";
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Holds are keyed by the requesting process (pid + start time, taken from the
/// socket peer), not by the connection.
///
/// Each request is a short RPC on its own connection, so holds survive helper
/// restarts: the lockfile is the source of truth and the helper reaps holders
/// whose processes have died.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum HelperRequest {
    Hold,
    Release,
    Status,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HelperResponse {
    HoldOk,
    ReleaseOk,
    Status {
        holders: usize,
        sleep_disabled: bool,
    },
    Error {
        message: String,
    },
}

impl HelperResponse {
    ///
    /// # Errors
    ///
    /// Returns an error if the response is an RPC error.
    pub fn into_result(self) -> Result<Self, String> {
        match self {
            Self::Error { message } => Err(message),
            other => Ok(other),
        }
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the response is not `HoldOk`.
    pub fn into_hold_ok(self) -> Result<(), String> {
        match self.into_result()? {
            Self::HoldOk => Ok(()),
            other => Err(format!("unexpected response for hold: {other:?}")),
        }
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the response is not `ReleaseOk`.
    pub fn into_release_ok(self) -> Result<(), String> {
        match self.into_result()? {
            Self::ReleaseOk => Ok(()),
            other => Err(format!("unexpected response for release: {other:?}")),
        }
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the response is not `Status`.
    pub fn into_status(self) -> Result<(usize, bool), String> {
        match self.into_result()? {
            Self::Status {
                holders,
                sleep_disabled,
            } => Ok((holders, sleep_disabled)),
            other => Err(format!("unexpected response for status: {other:?}")),
        }
    }
}

///
/// # Panics
///
/// Panics if the request cannot be serialized to JSON.
#[must_use]
pub fn encode_request(request: &HelperRequest) -> String {
    serde_json::to_string(request).expect("request should serialize") + "\n"
}

///
/// # Errors
///
/// Returns an error if the line is not valid JSON for a helper request.
pub fn decode_request(line: &str) -> Result<HelperRequest, serde_json::Error> {
    serde_json::from_str(line.trim())
}

///
/// # Panics
///
/// Panics if the response cannot be serialized to JSON.
#[must_use]
pub fn encode_response(response: &HelperResponse) -> String {
    serde_json::to_string(response).expect("response should serialize") + "\n"
}

///
/// # Errors
///
/// Returns an error if the line is not valid JSON for a helper response.
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
    let mut reader = BufReader::new(stream).take(MAX_REQUEST_BYTES as u64 + 1);
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .map_err(|e| format!("read failed: {e}"))?;
    if bytes == 0 {
        return Err("missing request".to_string());
    }
    if !line.ends_with('\n') {
        return Err("request missing newline".to_string());
    }
    if line.len() > MAX_REQUEST_BYTES {
        return Err("request too large".to_string());
    }
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
    request: &HelperRequest,
    with_timeouts: bool,
) -> Result<HelperResponse, String> {
    let mut stream =
        UnixStream::connect(socket_path).map_err(|e| format!("connect failed: {e}"))?;
    if with_timeouts {
        configure_rpc_timeouts(&stream)?;
    }
    write_request(&mut stream, request)?;
    read_response_line(&mut stream)
}

#[derive(Clone)]
pub struct HelperClient {
    socket_path: String,
}

impl Default for HelperClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HelperClient {
    #[must_use]
    pub fn new() -> Self {
        Self {
            socket_path: HELPER_SOCKET_PATH.to_string(),
        }
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the helper socket cannot be connected to.
    pub fn try_connect(&self) -> Result<UnixStream, String> {
        UnixStream::connect(&self.socket_path).map_err(|e| format!("connect failed: {e}"))
    }

    #[must_use]
    pub fn is_available(&self) -> bool {
        self.try_connect().is_ok()
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the status RPC fails.
    pub fn status(&self) -> Result<(usize, bool), String> {
        rpc(&self.socket_path, &HelperRequest::Status, true)?.into_status()
    }
}

#[must_use]
pub fn is_connect_error(message: &str) -> bool {
    message.starts_with("connect failed:")
}

/// True for helper errors that mean the peer is not allowed to take
/// entirely-mode holds.
///
/// Distinct from the helper being unreachable or failing. Matches the prefix
/// used by [`crate::entirely::authz::denial_message`].
#[must_use]
pub fn is_authorization_error(message: &str) -> bool {
    message.starts_with("not authorized")
}

/// RAII guard for a helper-managed hold.
///
/// The hold is registered against this process in the helper's lockfile and
/// released by an explicit RPC on drop. If this process dies without
/// releasing (or the helper is unreachable at drop time), the helper's
/// periodic reconcile reaps the entry once the process is gone.
pub struct HelperHoldGuard {
    client: HelperClient,
    released: bool,
}

impl HelperHoldGuard {
    ///
    /// # Errors
    ///
    /// Returns an error if the hold RPC fails.
    pub fn try_acquire(client: &HelperClient) -> Result<Self, String> {
        rpc(&client.socket_path, &HelperRequest::Hold, true)?.into_hold_ok()?;
        Ok(Self {
            client: client.clone(),
            released: false,
        })
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the release RPC fails.
    pub fn release(&mut self) -> Result<(), String> {
        if self.released {
            return Ok(());
        }
        // Only mark released on success so Drop retries a failed explicit
        // release; otherwise a transient helper outage strands the hold for
        // as long as this process lives (the reaper only prunes dead pids).
        // Release is idempotent on the helper side, so a duplicate after a
        // lost response is harmless.
        rpc(&self.client.socket_path, &HelperRequest::Release, true)?.into_release_ok()?;
        self.released = true;
        Ok(())
    }
}

impl Drop for HelperHoldGuard {
    fn drop(&mut self) {
        if !self.released
            && let Err(e) = self.release()
        {
            eprintln!("Error releasing helper hold: {e}");
        }
    }
}

#[cfg(target_os = "macos")]
fn write_response_on_stream(
    stream: &mut UnixStream,
    response: &HelperResponse,
) -> Result<(), String> {
    stream
        .write_all(encode_response(response).as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
///
/// # Errors
///
/// Returns an error if the peer PID or process start time cannot be read.
pub fn peer_process_id(
    stream: &UnixStream,
) -> Result<crate::entirely::lockfile::ProcessId, String> {
    use nix::sys::socket::getsockopt;
    use nix::sys::socket::sockopt::LocalPeerPid;

    let pid = getsockopt(stream, LocalPeerPid).map_err(|e| e.to_string())?;
    process_util::process_id_from_pid(pid).map_err(|e| e.to_string())
}

/// Authorize a Hold from this peer, failing closed: unreadable credentials
/// deny. Uses only the kernel-supplied effective uid (`LOCAL_PEERCRED`);
/// group membership is resolved by [`crate::entirely::authz`].
#[cfg(target_os = "macos")]
fn authorize_hold(stream: &UnixStream) -> Result<(), String> {
    use nix::sys::socket::getsockopt;
    use nix::sys::socket::sockopt::LocalPeerCred;

    let uid = match getsockopt(stream, LocalPeerCred) {
        Ok(cred) => cred.uid(),
        Err(e) => {
            return Err(format!(
                "not authorized: could not verify peer credentials: {e}"
            ));
        }
    };
    if crate::entirely::authz::uid_may_hold(uid) {
        Ok(())
    } else {
        Err(crate::entirely::authz::denial_message(uid))
    }
}

#[cfg(target_os = "macos")]
///
/// # Errors
///
/// Returns an error if the connection cannot be served.
pub fn serve_connection(
    stream: UnixStream,
    coordinator: &Arc<EntirelyCoordinator>,
) -> Result<(), String> {
    serve_connection_inner(stream, coordinator, &authorize_hold, &peer_process_id)
}

#[cfg(target_os = "macos")]
fn serve_connection_inner(
    mut stream: UnixStream,
    coordinator: &Arc<EntirelyCoordinator>,
    authorize_hold: &dyn Fn(&UnixStream) -> Result<(), String>,
    peer_process_id: &dyn Fn(&UnixStream) -> Result<crate::entirely::lockfile::ProcessId, String>,
) -> Result<(), String> {
    // Bound the whole RPC so a client that connects and sends nothing can't
    // pin a helper thread forever.
    configure_rpc_timeouts(&stream)?;

    let request = match read_request_line(&mut stream) {
        Ok(req) => req,
        Err(e) => {
            return write_response_on_stream(&mut stream, &HelperResponse::Error { message: e });
        }
    };

    // Hold is the privileged operation (it can set the system-wide
    // SleepDisabled setting) and is gated on the peer's identity. Release
    // stays open: it only acts on the peer's own process identity, so a
    // client can never release another process's hold, and gating it would
    // strand the holds of users whose grant was later revoked. Status is
    // read-only and open to everyone.
    let response = match request {
        HelperRequest::Hold => match authorize_hold(&stream) {
            Err(denial) => HelperResponse::Error { message: denial },
            Ok(()) => match peer_process_id(&stream).and_then(|peer| {
                // Once the hold is committed, a later response-write failure is
                // at-least-once from the client's point of view: the client may
                // treat acquisition as failed while the helper keeps the hold.
                // The hold is keyed to the client process, so Release remains
                // idempotent and the reaper clears it if the client exits.
                coordinator.hold(peer).map_err(|e| e.to_string())
            }) {
                Ok(()) => HelperResponse::HoldOk,
                Err(e) => HelperResponse::Error { message: e },
            },
        },
        HelperRequest::Release => match peer_process_id(&stream)
            .and_then(|peer| coordinator.release(peer).map_err(|e| e.to_string()))
        {
            Ok(()) => HelperResponse::ReleaseOk,
            Err(e) => HelperResponse::Error { message: e },
        },
        HelperRequest::Status => match coordinator.status() {
            Ok(status) => HelperResponse::Status {
                holders: status.holders,
                sleep_disabled: status.sleep_disabled,
            },
            Err(e) => HelperResponse::Error {
                message: e.to_string(),
            },
        },
    };
    write_response_on_stream(&mut stream, &response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entirely::coordinator::SleepDisabler;
    use crate::entirely::process_util;
    use std::os::unix::net::UnixListener;
    use std::sync::Mutex;

    #[cfg(target_os = "macos")]
    #[test]
    fn hold_status_release_round_trip_over_socket() {
        let dir = std::env::temp_dir();
        let sock_path = dir.join(format!("caffeinate2_ipc_{}.sock", std::process::id()));
        let lock_path = dir.join(format!("caffeinate2_ipc_{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_file(&lock_path);

        let listener = UnixListener::bind(&sock_path).unwrap();
        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_server = sleep_calls.clone();
        let disabler: SleepDisabler = Arc::new(move |state, _verbose| {
            sleep_calls_server.lock().unwrap().push(state);
            Ok(())
        });
        let coordinator = Arc::new(EntirelyCoordinator::with_options(
            false,
            lock_path.clone(),
            disabler,
            Arc::new(process_util::default_process_checker),
        ));
        let server = std::thread::spawn(move || {
            // One connection each for hold, status, and release. Use a
            // permissive authorizer so the test doesn't depend on the
            // developer's group memberships.
            for _ in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                serve_connection_inner(stream, &coordinator, &|_| Ok(()), &peer_process_id)
                    .unwrap();
            }
        });

        let client = HelperClient {
            socket_path: sock_path.display().to_string(),
        };
        let mut guard = HelperHoldGuard::try_acquire(&client).unwrap();
        assert_eq!(client.status().unwrap(), (1, true));
        guard.release().unwrap();
        server.join().unwrap();

        assert_eq!(*sleep_calls.lock().unwrap(), vec![true, false]);

        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_file(&lock_path);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unauthorized_hold_is_denied_and_status_stays_open() {
        let dir = std::env::temp_dir();
        let sock_path = dir.join(format!("caffeinate2_authz_{}.sock", std::process::id()));
        let lock_path = dir.join(format!("caffeinate2_authz_{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_file(&lock_path);

        let listener = UnixListener::bind(&sock_path).unwrap();
        let disabler: SleepDisabler = Arc::new(|_state, _verbose| Ok(()));
        let coordinator = Arc::new(EntirelyCoordinator::with_options(
            false,
            lock_path.clone(),
            disabler,
            Arc::new(process_util::default_process_checker),
        ));
        let server = std::thread::spawn(move || {
            // One denied hold, then one open status request.
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                serve_connection_inner(
                    stream,
                    &coordinator,
                    &|_| Err("not authorized: test denial".to_string()),
                    &peer_process_id,
                )
                .unwrap();
            }
        });

        let client = HelperClient {
            socket_path: sock_path.display().to_string(),
        };
        let Err(error) = HelperHoldGuard::try_acquire(&client) else {
            panic!("hold should have been denied");
        };
        assert!(is_authorization_error(&error), "{error}");
        // The denied hold must not have registered anything.
        assert_eq!(client.status().unwrap(), (0, false));
        server.join().unwrap();

        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_file(&lock_path);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn status_succeeds_when_peer_lookup_fails() {
        let dir = std::env::temp_dir();
        let sock_path = dir.join(format!("caffeinate2_status_{}.sock", std::process::id()));
        let lock_path = dir.join(format!("caffeinate2_status_{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_file(&lock_path);

        let listener = UnixListener::bind(&sock_path).unwrap();
        let disabler: SleepDisabler = Arc::new(|_state, _verbose| Ok(()));
        let coordinator = Arc::new(EntirelyCoordinator::with_options(
            false,
            lock_path.clone(),
            disabler,
            Arc::new(process_util::default_process_checker),
        ));
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_connection_inner(stream, &coordinator, &|_| Ok(()), &|_| {
                Err("peer lookup failed".to_string())
            })
            .unwrap();
        });

        let client = HelperClient {
            socket_path: sock_path.display().to_string(),
        };
        assert_eq!(client.status().unwrap(), (0, false));
        server.join().unwrap();

        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_file(&lock_path);
    }

    #[test]
    fn rejects_oversized_requests() {
        let (mut left, mut right) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            left.write_all(&vec![b'a'; MAX_REQUEST_BYTES + 1]).unwrap();
            left.write_all(b"\n").unwrap();
        });

        assert_eq!(
            read_line(&mut right).unwrap_err(),
            "request missing newline"
        );
        writer.join().unwrap();
    }

    #[test]
    fn authorization_error_detection() {
        assert!(is_authorization_error("not authorized: nope"));
        assert!(!is_authorization_error("connect failed: nope"));
    }

    #[test]
    fn round_trip_request() {
        for req in [
            HelperRequest::Hold,
            HelperRequest::Release,
            HelperRequest::Status,
        ] {
            let decoded = decode_request(&encode_request(&req)).unwrap();
            assert_eq!(decoded, req);
        }
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
        assert!(HelperResponse::ReleaseOk.into_hold_ok().is_err());
    }

    #[test]
    fn release_ok_response_into_release_ok() {
        assert!(HelperResponse::ReleaseOk.into_release_ok().is_ok());
        assert!(HelperResponse::HoldOk.into_release_ok().is_err());
    }
}
