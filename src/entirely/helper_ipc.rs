use crate::entirely::coordinator::EntirelyCoordinator;
use crate::entirely::error::HelperIpcError;
use crate::entirely::process_util;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const HELPER_SOCKET_PATH: &str = "/var/run/caffeinate2.sock";
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Absolute wall-clock budget for reading one request on the helper side. The
/// 5s RPC timeouts are per-recv (`SO_RCVTIMEO`), so a client trickling one byte
/// per timeout window would otherwise hold a connection slot indefinitely; this
/// deadline bounds the whole read no matter how the bytes arrive.
const CONNECTION_DEADLINE: Duration = Duration::from_secs(10);

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
        /// The daemon's crate version. `default` keeps responses from
        /// pre-versioning helpers parseable (they decode as `None`, which
        /// [`HelperStatus::is_stale`] reports as stale); older clients in turn
        /// ignore the unknown field, so the handshake is compatible both ways.
        #[serde(default)]
        version: Option<String>,
    },
    Error {
        message: String,
    },
}

/// Decoded `Status` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperStatus {
    pub holders: usize,
    pub sleep_disabled: bool,
    /// The helper daemon's crate version; `None` means a pre-versioning
    /// helper binary is still installed.
    pub version: Option<String>,
}

impl HelperStatus {
    /// This binary's crate version, for comparison against the daemon's.
    pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

    /// True when the installed helper daemon reports a different version than
    /// this binary. The helper is a copied snapshot that launchd keeps
    /// serving across package upgrades, so helper-side fixes only take effect
    /// after a reinstall (`sudo caffeinate2 --install-helper`) — callers
    /// should surface that.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.version.as_deref() != Some(Self::CLIENT_VERSION)
    }
}

impl HelperResponse {
    ///
    /// # Errors
    ///
    /// Returns an error if the response is an RPC error.
    pub fn into_result(self) -> Result<Self, HelperIpcError> {
        match self {
            Self::Error { message } => Err(HelperIpcError::new(message)),
            other => Ok(other),
        }
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the response is not `HoldOk`.
    pub fn into_hold_ok(self) -> Result<(), HelperIpcError> {
        match self.into_result()? {
            Self::HoldOk => Ok(()),
            other => Err(HelperIpcError::new(format!(
                "unexpected response for hold: {other:?}"
            ))),
        }
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the response is not `ReleaseOk`.
    pub fn into_release_ok(self) -> Result<(), HelperIpcError> {
        match self.into_result()? {
            Self::ReleaseOk => Ok(()),
            other => Err(HelperIpcError::new(format!(
                "unexpected response for release: {other:?}"
            ))),
        }
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the response is not `Status`.
    pub fn into_status(self) -> Result<HelperStatus, HelperIpcError> {
        match self.into_result()? {
            Self::Status {
                holders,
                sleep_disabled,
                version,
            } => Ok(HelperStatus {
                holders,
                sleep_disabled,
                version,
            }),
            other => Err(HelperIpcError::new(format!(
                "unexpected response for status: {other:?}"
            ))),
        }
    }
}

fn try_encode_request(request: &HelperRequest) -> Result<String, serde_json::Error> {
    Ok(serde_json::to_string(request)? + "\n")
}

///
/// # Errors
///
/// Returns an error if the line is not valid JSON for a helper request.
pub fn decode_request(line: &str) -> Result<HelperRequest, serde_json::Error> {
    serde_json::from_str(line.trim())
}

fn try_encode_response(response: &HelperResponse) -> Result<String, serde_json::Error> {
    Ok(serde_json::to_string(response)? + "\n")
}

///
/// # Errors
///
/// Returns an error if the line is not valid JSON for a helper response.
pub fn decode_response(line: &str) -> Result<HelperResponse, serde_json::Error> {
    serde_json::from_str(line.trim())
}

fn configure_rpc_timeouts(stream: &UnixStream) -> Result<(), HelperIpcError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| HelperIpcError::new(format!("set timeout: {e}")))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| HelperIpcError::new(format!("set timeout: {e}")))
}

fn write_request(stream: &mut UnixStream, request: &HelperRequest) -> Result<(), HelperIpcError> {
    let encoded = try_encode_request(request)
        .map_err(|e| HelperIpcError::new(format!("encode failed: {e}")))?;
    stream
        .write_all(encoded.as_bytes())
        .map_err(|e| HelperIpcError::new(format!("write failed: {e}")))?;
    stream
        .flush()
        .map_err(|e| HelperIpcError::new(format!("flush failed: {e}")))
}

fn read_line(stream: &mut UnixStream) -> Result<String, HelperIpcError> {
    let mut reader = BufReader::new(stream).take(MAX_REQUEST_BYTES as u64 + 1);
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .map_err(|e| HelperIpcError::new(format!("read failed: {e}")))?;
    if bytes == 0 {
        return Err(HelperIpcError::new("missing request"));
    }
    if !line.ends_with('\n') {
        return Err(HelperIpcError::new("request missing newline"));
    }
    if line.len() > MAX_REQUEST_BYTES {
        return Err(HelperIpcError::new("request too large"));
    }
    Ok(line)
}

fn read_response_line(stream: &mut UnixStream) -> Result<HelperResponse, HelperIpcError> {
    let line = read_line(stream)?;
    decode_response(&line).map_err(|e| HelperIpcError::new(format!("invalid response: {e}")))
}

/// Read one newline-framed line, enforcing `deadline` across the whole read:
/// the read timeout is shrunk to the time remaining before every recv, so a
/// writer feeding one byte per timeout window hits the deadline instead of
/// resetting a fresh `SO_RCVTIMEO` window with each byte. Server-side only —
/// clients talk to a helper that answers in one write, so [`read_line`]'s
/// per-recv timeout suffices there.
fn read_line_with_deadline(
    stream: &mut UnixStream,
    deadline: Instant,
) -> Result<String, HelperIpcError> {
    const PER_RECV_TIMEOUT: Duration = Duration::from_secs(5);

    let mut buf: Vec<u8> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(HelperIpcError::new("connection deadline exceeded"));
        }
        let timeout = remaining.min(PER_RECV_TIMEOUT);
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| HelperIpcError::new(format!("set timeout: {e}")))?;
        let mut chunk = [0u8; 1024];
        let bytes = match stream.read(&mut chunk) {
            Ok(bytes) => bytes,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // A recv that timed out on a window shrunk below the per-recv
                // timeout ran out of *deadline*, not of client patience.
                if timeout < PER_RECV_TIMEOUT {
                    return Err(HelperIpcError::new("connection deadline exceeded"));
                }
                return Err(HelperIpcError::new(format!("read failed: {e}")));
            }
            Err(e) => return Err(HelperIpcError::new(format!("read failed: {e}"))),
        };
        if bytes == 0 {
            if buf.is_empty() {
                return Err(HelperIpcError::new("missing request"));
            }
            return Err(HelperIpcError::new("request missing newline"));
        }
        buf.extend_from_slice(&chunk[..bytes]);
        if let Some(newline) = buf.iter().position(|&b| b == b'\n') {
            if newline + 1 > MAX_REQUEST_BYTES {
                return Err(HelperIpcError::new("request too large"));
            }
            return String::from_utf8(buf[..=newline].to_vec())
                .map_err(|e| HelperIpcError::new(format!("read failed: {e}")));
        }
        if buf.len() > MAX_REQUEST_BYTES {
            return Err(HelperIpcError::new("request missing newline"));
        }
    }
}

fn read_request_line(
    stream: &mut UnixStream,
    deadline: Instant,
) -> Result<HelperRequest, HelperIpcError> {
    let line = read_line_with_deadline(stream, deadline)?;
    decode_request(&line).map_err(|e| HelperIpcError::new(format!("invalid request: {e}")))
}

fn rpc(
    socket_path: &str,
    request: &HelperRequest,
    with_timeouts: bool,
) -> Result<HelperResponse, HelperIpcError> {
    let mut stream = UnixStream::connect(socket_path).map_err(HelperIpcError::connect)?;
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

    fn try_connect(&self) -> Result<UnixStream, HelperIpcError> {
        UnixStream::connect(&self.socket_path).map_err(HelperIpcError::connect)
    }

    #[must_use]
    pub fn is_available(&self) -> bool {
        self.try_connect().is_ok()
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the status RPC fails.
    pub fn status(&self) -> Result<HelperStatus, HelperIpcError> {
        rpc(&self.socket_path, &HelperRequest::Status, true)?.into_status()
    }
}

#[must_use]
pub fn is_connect_error(message: &str) -> bool {
    message.starts_with("connect failed:")
}

/// True for helper errors that mean the peer is not allowed to take
/// entirely-mode holds (an actual policy denial).
///
/// Distinct from the helper being unreachable or failing internally (e.g. a
/// failed peer-credential read, which uses the `internal error:` prefix).
/// Matches the prefix used by [`crate::entirely::authz::denial_message`].
#[must_use]
pub fn is_authorization_error(message: &str) -> bool {
    message.starts_with("not authorized")
}

/// Process-wide count of live helper holds. The helper keys holds by process
/// (pid + start time), so every `HelperHoldGuard` in this process maps to the
/// *same* lockfile entry: a Release from any one of them drops that shared
/// entry and re-enables sleep, even if another guard is still alive. Counting
/// guards here means the Release RPC is sent only when the last in-process hold
/// is dropped — so a stale/cancelled guard can't release a hold a newer session
/// still relies on.
///
/// This is a `Mutex`, not an atomic, because the count change and its RPC must
/// be one critical section. The lock is held across both the count mutation and
/// the Hold/Release RPC so acquire and release can't interleave: otherwise a
/// concurrent `try_acquire` could re-establish a hold (count back to 1) in the
/// window between the last guard's decrement-to-0 and its Release RPC, leaving
/// the in-process count claiming a hold the helper has already dropped. Holds
/// are infrequent, so serializing them behind one lock (briefly blocking across
/// a bounded-timeout RPC) is fine.
static HELPER_HOLD_COUNT: Mutex<usize> = Mutex::new(0);

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
    pub fn try_acquire(client: &HelperClient) -> Result<Self, HelperIpcError> {
        // Hold the count lock across the Hold RPC so a concurrent release can't
        // slip its decrement-and-Release between this RPC and the increment.
        let mut count = HELPER_HOLD_COUNT.lock().unwrap_or_else(|e| e.into_inner());
        match rpc(&client.socket_path, &HelperRequest::Hold, true) {
            // The helper answered. An explicit `Error` response means it did not
            // commit a hold, so there is nothing to release; just surface it.
            Ok(response) => response.into_hold_ok()?,
            // Transport/read failure: the request may have reached the helper and
            // been accepted even though we never saw the reply, leaving a hold
            // committed against this process that no guard will ever release. If
            // we are the first in-process hold (count == 0), send a best-effort
            // Release so a lost response can't strand sleep disabled for the life
            // of this process. Release is idempotent helper-side, so it is
            // harmless if the Hold never actually landed. Skip it when other
            // guards are live (count > 0): they share the per-process lockfile
            // entry and releasing would yank it out from under them.
            //
            // This Release can even be processed *before* a still-queued Hold
            // commits (two connections, two worker threads, unfair ops mutex);
            // that inversion is closed server-side: `serve_connection_inner`
            // rolls back a newly-created hold whose response write fails, so
            // the abandoned Hold cannot strand a live-pid entry either way.
            Err(e) => {
                if *count == 0 {
                    let _ = rpc(&client.socket_path, &HelperRequest::Release, true);
                }
                return Err(e);
            }
        }
        *count += 1;
        Ok(Self {
            client: client.clone(),
            released: false,
        })
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the release RPC fails.
    pub fn release(&mut self) -> Result<(), HelperIpcError> {
        self.release_inner(false)
    }

    /// `discarding` is true only for the Drop-time attempt, when the guard is
    /// about to cease existing: the count must then stop reflecting it even if
    /// the Release RPC fails. A count stranded at 1 for a discarded guard would
    /// send every later release in this process down the `count > 1` fast path,
    /// so the Release RPC would never be sent again for the process lifetime
    /// and the helper's reconcile would keep re-applying the disable for this
    /// live pid. The stale helper-side entry self-heals instead: the next
    /// session's true 1 -> 0 transition sends the idempotent Release RPC.
    fn release_inner(&mut self, discarding: bool) -> Result<(), HelperIpcError> {
        if self.released {
            return Ok(());
        }
        // Take the count lock for the whole critical section. Exactly one guard
        // observes the 1 -> 0 transition and is responsible for the actual
        // Release RPC. While other in-process guards remain (count > 1), just
        // drop our reference: releasing now would remove the shared per-process
        // lockfile entry out from under them.
        let mut count = HELPER_HOLD_COUNT.lock().unwrap_or_else(|e| e.into_inner());
        if *count > 1 {
            *count -= 1;
            self.released = true;
            return Ok(());
        }
        // We are the last hold: send the Release RPC while still holding the lock
        // so a concurrent acquire can't re-establish a hold (and the count) in
        // the gap between our decrement and the RPC. Only decrement and mark
        // released on success so Drop retries a failed explicit release;
        // otherwise a transient helper outage strands the hold for as long as
        // this process lives (the reaper only prunes dead pids). Release is
        // idempotent on the helper side, so a duplicate after a lost response is
        // harmless. On failure the count stays at 1 so a later explicit retry
        // still owns the 1 -> 0 transition — unless this is the final Drop-time
        // attempt (`discarding`), after which no guard remains to retry.
        match rpc(&self.client.socket_path, &HelperRequest::Release, true)
            .and_then(HelperResponse::into_release_ok)
        {
            Ok(()) => {
                *count -= 1;
                self.released = true;
                Ok(())
            }
            Err(e) => {
                if discarding {
                    *count -= 1;
                    self.released = true;
                }
                Err(e)
            }
        }
    }
}

impl Drop for HelperHoldGuard {
    fn drop(&mut self) {
        if !self.released
            && let Err(e) = self.release_inner(true)
        {
            tracing::warn!("Error releasing helper hold: {e}");
        }
    }
}

/// Write a single newline-framed response to a connection. Exposed so the
/// daemon can reply (e.g. with an over-capacity error) before closing a
/// connection it will not fully serve.
///
/// # Errors
///
/// Returns an error if the response cannot be encoded or written.
#[cfg(target_os = "macos")]
pub fn write_response(
    stream: &mut UnixStream,
    response: &HelperResponse,
) -> Result<(), HelperIpcError> {
    let encoded = try_encode_response(response).map_err(HelperIpcError::from)?;
    stream
        .write_all(encoded.as_bytes())
        .map_err(|e| HelperIpcError::new(e.to_string()))?;
    stream
        .flush()
        .map_err(|e| HelperIpcError::new(e.to_string()))
}

#[cfg(target_os = "macos")]
///
/// # Errors
///
/// Returns an error if the peer PID or process start time cannot be read.
pub fn peer_process_id(
    stream: &UnixStream,
) -> Result<crate::entirely::lockfile::ProcessId, HelperIpcError> {
    use nix::sys::socket::getsockopt;
    use nix::sys::socket::sockopt::LocalPeerPid;

    let pid = getsockopt(stream, LocalPeerPid).map_err(|e| HelperIpcError::new(e.to_string()))?;
    process_util::process_id_from_pid(pid).map_err(HelperIpcError::internal)
}

/// Authorize a Hold from this peer, failing closed: unreadable credentials
/// deny. Uses only the kernel-supplied effective uid (`LOCAL_PEERCRED`);
/// group membership is resolved by [`crate::entirely::authz`].
#[cfg(target_os = "macos")]
fn authorize_hold(stream: &UnixStream) -> Result<(), HelperIpcError> {
    use nix::sys::socket::getsockopt;
    use nix::sys::socket::sockopt::LocalPeerCred;

    let uid = match getsockopt(stream, LocalPeerCred) {
        Ok(cred) => cred.uid(),
        Err(e) => {
            // Reading the peer credential failed: this is an internal/helper
            // failure, NOT a policy decision. Use the "internal error:" prefix
            // so callers don't surface it as "you're not authorized" (which
            // would be misleading) and don't treat it as a definitive denial.
            return Err(HelperIpcError::new(format!(
                "internal error: could not verify peer credentials: {e}"
            )));
        }
    };
    if crate::entirely::authz::uid_may_hold(uid) {
        Ok(())
    } else {
        Err(HelperIpcError::new(crate::entirely::authz::denial_message(
            uid,
        )))
    }
}

/// Best-effort peer uid for audit logging only (never used for authorization
/// decisions — those go through [`authorize_hold`]).
#[cfg(target_os = "macos")]
fn peer_uid_for_audit(stream: &UnixStream) -> Option<u32> {
    use nix::sys::socket::getsockopt;
    use nix::sys::socket::sockopt::LocalPeerCred;

    getsockopt(stream, LocalPeerCred)
        .ok()
        .map(|cred| cred.uid())
}

/// Lightweight always-on audit trail for the privileged Hold/Release ops. Goes
/// to stderr, which the LaunchDaemon captures in the helper log. Status is
/// read-only and frequently polled, so it is intentionally not audited here.
#[cfg(target_os = "macos")]
fn audit_log(op: &str, uid: Option<u32>, pid: Option<i32>, result: Result<(), &str>) {
    let uid = uid.map_or_else(|| "?".to_string(), |u| u.to_string());
    let pid = pid.map_or_else(|| "?".to_string(), |p| p.to_string());
    match result {
        Ok(()) => tracing::info!(
            target: "caffeinate2::audit",
            op,
            uid,
            pid,
            result = "ok",
            "helper audit"
        ),
        Err(msg) => tracing::warn!(
            target: "caffeinate2::audit",
            op,
            uid,
            pid,
            result = "error",
            msg,
            "helper audit"
        ),
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
) -> Result<(), HelperIpcError> {
    serve_connection_inner(stream, coordinator, &authorize_hold, &peer_process_id)
}

#[cfg(target_os = "macos")]
fn serve_connection_inner(
    mut stream: UnixStream,
    coordinator: &Arc<EntirelyCoordinator>,
    authorize_hold: &dyn Fn(&UnixStream) -> Result<(), HelperIpcError>,
    peer_process_id: &dyn Fn(
        &UnixStream,
    ) -> Result<crate::entirely::lockfile::ProcessId, HelperIpcError>,
) -> Result<(), HelperIpcError> {
    // Bound the whole RPC so a client that connects and sends nothing — or
    // trickles bytes to keep resetting the per-recv timeout — can't pin a
    // helper connection slot forever.
    let deadline = Instant::now() + CONNECTION_DEADLINE;
    configure_rpc_timeouts(&stream)?;

    let request = match read_request_line(&mut stream, deadline) {
        Ok(req) => req,
        Err(e) => {
            return write_response(
                &mut stream,
                &HelperResponse::Error {
                    message: e.to_string(),
                },
            );
        }
    };

    // Hold is the privileged operation (it can set the system-wide
    // SleepDisabled setting) and is gated on the peer's identity. Release
    // stays open: it only acts on the peer's own process identity, so a
    // client can never release another process's hold, and gating it would
    // strand the holds of users whose grant was later revoked. Status is
    // read-only and open to everyone.
    // Set when a Hold commits a *newly created* holder entry. If the final
    // response write then fails, the client never learned the hold exists —
    // it timed out and may even have fired a compensating Release that a
    // second worker processed before this commit — so the entry is rolled
    // back below. Only the creator may undo: releasing a pre-existing entry
    // would yank it out from under the peer's other live guards, which
    // learned of it through earlier, answered RPCs (and whose in-process
    // count also means the peer sends no compensating Release).
    let mut undo_hold_on_failed_write = None;
    let response = match request {
        HelperRequest::Hold => {
            let uid = peer_uid_for_audit(&stream);
            match authorize_hold(&stream) {
                Err(denial) => {
                    audit_log("hold", uid, None, Err(denial.message()));
                    HelperResponse::Error {
                        message: denial.to_string(),
                    }
                }
                Ok(()) => match peer_process_id(&stream) {
                    Err(e) => {
                        audit_log("hold", uid, None, Err(e.message()));
                        HelperResponse::Error {
                            message: e.to_string(),
                        }
                    }
                    Ok(peer) => match coordinator.hold(peer).map_err(|e| e.to_string()) {
                        Ok(outcome) => {
                            audit_log("hold", uid, Some(peer.pid), Ok(()));
                            if outcome.newly_inserted {
                                undo_hold_on_failed_write = Some((peer, outcome.hold_epoch));
                            }
                            HelperResponse::HoldOk
                        }
                        Err(e) => {
                            audit_log("hold", uid, Some(peer.pid), Err(&e));
                            HelperResponse::Error { message: e }
                        }
                    },
                },
            }
        }
        HelperRequest::Release => {
            let uid = peer_uid_for_audit(&stream);
            match peer_process_id(&stream) {
                Err(e) => {
                    audit_log("release", uid, None, Err(e.message()));
                    HelperResponse::Error {
                        message: e.to_string(),
                    }
                }
                Ok(peer) => match coordinator.release(peer).map_err(|e| e.to_string()) {
                    Ok(()) => {
                        audit_log("release", uid, Some(peer.pid), Ok(()));
                        HelperResponse::ReleaseOk
                    }
                    Err(e) => {
                        audit_log("release", uid, Some(peer.pid), Err(&e));
                        HelperResponse::Error { message: e }
                    }
                },
            }
        }
        HelperRequest::Status => match coordinator.status() {
            Ok(status) => HelperResponse::Status {
                holders: status.holders,
                sleep_disabled: status.sleep_disabled,
                version: Some(HelperStatus::CLIENT_VERSION.to_string()),
            },
            Err(e) => HelperResponse::Error {
                message: e.to_string(),
            },
        },
    };
    let write_result = write_response(&mut stream, &response);
    if write_result.is_err()
        && let Some((peer, hold_epoch)) = undo_hold_on_failed_write
    {
        // The client provably never saw HoldOk (AF_UNIX write to a closed
        // peer fails), so no guard exists for this entry. Undo it rather
        // than strand a live-pid hold that the reaper never prunes. The
        // epoch check keeps a delayed rollback exact: if the client has
        // meanwhile re-held successfully (new epoch), the stale rollback
        // must not release that newer, guarded hold.
        match coordinator.release_if_hold_epoch(peer, hold_epoch) {
            Ok(true) => tracing::warn!(
                "rolled back hold for pid {}: response write failed",
                peer.pid
            ),
            Ok(false) => tracing::warn!(
                "skipped hold rollback for pid {}: a newer hold superseded it",
                peer.pid
            ),
            Err(e) => tracing::warn!(
                "response write failed and hold rollback for pid {} also failed: {e}",
                peer.pid
            ),
        }
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entirely::coordinator::SleepDisabler;
    use crate::entirely::process_util;
    use std::os::unix::net::UnixListener;
    use std::sync::Mutex;

    /// Serializes the tests that take real guards: `HELPER_HOLD_COUNT` is
    /// process-wide, so concurrent guards from different tests would observe
    /// each other's counts and take the wrong release path.
    static HOLD_COUNT_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(target_os = "macos")]
    #[test]
    fn hold_status_release_round_trip_over_socket() {
        let _serial = HOLD_COUNT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let status = client.status().unwrap();
        assert_eq!((status.holders, status.sleep_disabled), (1, true));
        guard.release().unwrap();
        server.join().unwrap();

        assert_eq!(*sleep_calls.lock().unwrap(), vec![true, false]);

        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_file(&lock_path);
    }

    /// A Hold whose response write fails must be rolled back by the server.
    /// The client that abandoned the RPC (timed out and closed the socket)
    /// treats acquisition as failed and holds no guard; worse, its
    /// compensating Release can be processed by a second worker *before* the
    /// delayed Hold commits, so without the rollback a live-pid entry with no
    /// owner would strand sleep disabled until this process exits.
    #[cfg(target_os = "macos")]
    #[test]
    fn hold_with_lost_response_is_rolled_back() {
        let dir = std::env::temp_dir();
        let sock_path = dir.join(format!("caffeinate2_lostresp_{}.sock", std::process::id()));
        let lock_path = dir.join(format!("caffeinate2_lostresp_{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_file(&lock_path);

        let listener = UnixListener::bind(&sock_path).unwrap();

        // Synchronize the client's abandonment with the server's commit: the
        // disabler runs while the hold is being committed, and parks there
        // until the test has closed the client socket. The HoldOk write is
        // then guaranteed to hit a closed peer — the deterministic version of
        // "the client timed out (and maybe fired its compensating Release)
        // while the Hold was still queued".
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let go_rx = Mutex::new(go_rx);
        let disabler: SleepDisabler = Arc::new(move |disable, _verbose| {
            if disable {
                let _ = reached_tx.send(());
                let _ = go_rx.lock().unwrap().recv();
            }
            Ok(())
        });
        let coordinator = Arc::new(EntirelyCoordinator::with_options(
            false,
            lock_path.clone(),
            disabler,
            Arc::new(process_util::default_process_checker),
        ));

        let server_coordinator = Arc::clone(&coordinator);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_connection_inner(stream, &server_coordinator, &|_| Ok(()), &peer_process_id)
        });

        let mut stream = UnixStream::connect(&sock_path).unwrap();
        write_request(&mut stream, &HelperRequest::Hold).unwrap();
        // Wait until the hold is mid-commit, then abandon the RPC like a
        // timed-out client, and let the commit finish.
        reached_rx.recv().unwrap();
        drop(stream);
        go_tx.send(()).unwrap();

        let serve_result = server.join().unwrap();
        assert!(
            serve_result.is_err(),
            "the HoldOk write must fail against the closed peer"
        );

        let status = coordinator.status().unwrap();
        assert_eq!(
            status.holders, 0,
            "a hold whose response was never delivered must not survive"
        );

        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_file(&lock_path);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dropped_guard_with_failed_release_frees_the_hold_count() {
        let _serial = HOLD_COUNT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir();
        let sock_path = dir.join(format!("caffeinate2_dropfail_{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);

        let listener = UnixListener::bind(&sock_path).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_server = requests.clone();
        let server = std::thread::spawn(move || {
            // Scripted helper: Hold ok; the explicit Release and the Drop retry
            // both fail; then a fresh Hold/Release session succeeds.
            let script = [
                HelperResponse::HoldOk,
                HelperResponse::Error {
                    message: "injected release failure".to_string(),
                },
                HelperResponse::Error {
                    message: "injected release failure".to_string(),
                },
                HelperResponse::HoldOk,
                HelperResponse::ReleaseOk,
            ];
            for response in script {
                let (mut stream, _) = listener.accept().unwrap();
                let line = read_line(&mut stream).unwrap();
                requests_server
                    .lock()
                    .unwrap()
                    .push(decode_request(&line).unwrap());
                write_response(&mut stream, &response).unwrap();
            }
        });

        let client = HelperClient {
            socket_path: sock_path.display().to_string(),
        };
        let mut guard = HelperHoldGuard::try_acquire(&client).unwrap();
        assert!(guard.release().is_err());
        // Drop is the final retry: it fails again, but the count must stop
        // reflecting the discarded guard.
        drop(guard);
        assert_eq!(
            *HELPER_HOLD_COUNT.lock().unwrap(),
            0,
            "discarded guard must not strand the process-wide hold count"
        );

        // The next session owns the 1 -> 0 transition again, so its release
        // sends the (idempotent) Release RPC that heals the stale helper-side
        // entry. A stranded count would take the `count > 1` fast path here and
        // never send it.
        let mut guard = HelperHoldGuard::try_acquire(&client).unwrap();
        guard.release().unwrap();
        server.join().unwrap();

        assert_eq!(
            *requests.lock().unwrap(),
            vec![
                HelperRequest::Hold,
                HelperRequest::Release,
                HelperRequest::Release,
                HelperRequest::Hold,
                HelperRequest::Release,
            ]
        );

        let _ = std::fs::remove_file(&sock_path);
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
                    &|_| Err(HelperIpcError::new("not authorized: test denial")),
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
        assert!(is_authorization_error(&error.to_string()), "{error}");
        // The denied hold must not have registered anything.
        let status = client.status().unwrap();
        assert_eq!((status.holders, status.sleep_disabled), (0, false));
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
                Err(HelperIpcError::new("peer lookup failed"))
            })
            .unwrap();
        });

        let client = HelperClient {
            socket_path: sock_path.display().to_string(),
        };
        let status = client.status().unwrap();
        assert_eq!((status.holders, status.sleep_disabled), (0, false));
        server.join().unwrap();

        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_file(&lock_path);
    }

    #[test]
    fn rejects_request_exceeding_limit_without_newline() {
        // The take() cap is MAX_REQUEST_BYTES + 1, so a payload of that many
        // non-newline bytes is read in full without ever seeing a newline: the
        // failure is the missing newline, detected before the size check.
        let (mut left, mut right) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            let _ = left.write_all(&vec![b'a'; MAX_REQUEST_BYTES + 1]);
            let _ = left.write_all(b"\n");
        });

        assert_eq!(
            read_line(&mut right).unwrap_err().message(),
            "request missing newline"
        );
        writer.join().unwrap();
    }

    #[test]
    fn rejects_oversized_request_with_newline() {
        // A line that ends in a newline but exceeds the byte limit must be
        // rejected as "request too large". MAX_REQUEST_BYTES bytes plus the
        // newline is MAX_REQUEST_BYTES + 1 total, which fits within the take()
        // cap and ends in '\n', so the size check (not the newline check) fires.
        let (mut left, mut right) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            let mut payload = vec![b'a'; MAX_REQUEST_BYTES];
            payload.push(b'\n');
            let _ = left.write_all(&payload);
        });

        assert_eq!(
            read_line(&mut right).unwrap_err().message(),
            "request too large"
        );
        writer.join().unwrap();
    }

    #[test]
    fn trickling_writer_hits_the_connection_deadline() {
        // One byte per interval keeps every individual recv well under its
        // timeout; only the absolute deadline can cut the connection off.
        let (mut left, mut right) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            // Bounded in case the reader never closes; exits early once the
            // reader's end is gone and the write fails.
            for _ in 0..100 {
                if left.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });

        let deadline = Instant::now() + Duration::from_millis(200);
        assert_eq!(
            read_line_with_deadline(&mut right, deadline)
                .unwrap_err()
                .message(),
            "connection deadline exceeded"
        );
        drop(right);
        writer.join().unwrap();
    }

    #[test]
    fn deadline_read_accepts_a_prompt_request() {
        let (mut left, mut right) = UnixStream::pair().unwrap();
        left.write_all(b"{\"op\":\"status\"}\n").unwrap();

        let line =
            read_line_with_deadline(&mut right, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(decode_request(&line).unwrap(), HelperRequest::Status);
    }

    #[test]
    fn authorization_error_detection() {
        assert!(is_authorization_error("not authorized: nope"));
        assert!(!is_authorization_error("connect failed: nope"));
        // A credential-read failure is an internal error, not a policy denial.
        assert!(!is_authorization_error(
            "internal error: could not verify peer credentials: x"
        ));
    }

    #[test]
    fn round_trip_request() {
        for req in [
            HelperRequest::Hold,
            HelperRequest::Release,
            HelperRequest::Status,
        ] {
            let decoded = decode_request(&try_encode_request(&req).unwrap()).unwrap();
            assert_eq!(decoded, req);
        }
    }

    #[test]
    fn round_trip_status_response() {
        let resp = HelperResponse::Status {
            holders: 2,
            sleep_disabled: true,
            version: Some(HelperStatus::CLIENT_VERSION.to_string()),
        };
        let decoded = decode_response(&try_encode_response(&resp).unwrap()).unwrap();
        assert_eq!(decoded, resp);
    }

    /// A pre-versioning helper's Status response (no `version` field) must
    /// still parse — and read as stale, so upgraded clients surface the
    /// reinstall hint instead of silently talking to the old daemon.
    #[test]
    fn legacy_status_without_version_parses_as_stale() {
        let legacy = r#"{"kind":"status","holders":1,"sleep_disabled":true}"#;
        let status = decode_response(legacy).unwrap().into_status().unwrap();
        assert_eq!(status.version, None);
        assert!(status.is_stale());

        let current = HelperResponse::Status {
            holders: 1,
            sleep_disabled: true,
            version: Some(HelperStatus::CLIENT_VERSION.to_string()),
        };
        assert!(!current.into_status().unwrap().is_stale());
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
        assert_eq!(resp.into_result().unwrap_err().message(), "nope");
    }

    #[test]
    fn status_response_into_status() {
        let resp = HelperResponse::Status {
            holders: 3,
            sleep_disabled: false,
            version: None,
        };
        assert_eq!(
            resp.into_status().unwrap(),
            HelperStatus {
                holders: 3,
                sleep_disabled: false,
                version: None,
            }
        );
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
