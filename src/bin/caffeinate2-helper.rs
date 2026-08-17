#[cfg(target_os = "macos")]
use anyhow::Context;
#[cfg(target_os = "macos")]
use caffeinate2::entirely::coordinator::EntirelyCoordinator;
#[cfg(target_os = "macos")]
use caffeinate2::entirely::helper_ipc::{self, HelperResponse, serve_connection};
#[cfg(target_os = "macos")]
use caffeinate2::util::logging;
#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "macos")]
use std::os::unix::net::UnixListener;
#[cfg(target_os = "macos")]
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(target_os = "macos")]
use std::time::Duration;

#[cfg(target_os = "macos")]
const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(target_os = "macos")]
const MAX_CONCURRENT_CONNECTIONS: usize = 32;

#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = run() {
        tracing::error!("caffeinate2-helper error: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn run() -> anyhow::Result<()> {
    logging::init_helper_tracing();
    let verbose = std::env::args().any(|a| a == "--verbose" || a == "-v");
    let socket_path = helper_ipc::HELPER_SOCKET_PATH;
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind helper socket at {socket_path}"))?;
    // Allow non-root clients (tray, CLI) to connect; peers are authenticated by
    // their kernel-supplied credentials (LocalPeerPid / LocalPeerCred), not by
    // socket permissions, and the privileged Hold is gated on the peer uid.
    //
    // 0666 (world read/write) is intentional: entirely mode is meant to work
    // for any local user the admin has granted, and macOS has no portable way
    // to restrict a socket to a supplementary group here. The tradeoff is a
    // local DoS surface — any local user can open connections — which is capped
    // by MAX_CONCURRENT_CONNECTIONS and the absolute per-connection deadline
    // that serve_connection enforces (per-recv timeouts alone would let a
    // byte-trickling client hold a slot indefinitely). This grants no privilege
    // by itself: Hold still requires passing the uid-based authorization check.
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o666))
        .context("failed to set socket permissions")?;
    let coordinator = Arc::new(EntirelyCoordinator::helper_daemon(verbose));

    // Re-sync the global SleepDisabled setting with whatever the lockfile says
    // is still alive. An empty lockfile does not force re-enable sleep, since
    // that could undo an unrelated manual `pmset disablesleep`.
    if let Err(e) = coordinator.reconcile_startup() {
        tracing::warn!("startup reconcile failed: {e}");
    }

    // Reap holders that died without sending Release (crashed or killed
    // clients) and converge the sleep setting.
    let reaper = Arc::clone(&coordinator);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(RECONCILE_INTERVAL);
            if let Err(e) = reaper.reconcile() {
                tracing::warn!("periodic reconcile failed: {e}");
            }
        }
    });

    tracing::info!("caffeinate2-helper listening on {socket_path}");

    let active_connections = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                continue;
            }
        };
        // Reserve a slot atomically so concurrent accepts can't both observe a
        // sub-limit count and push us over MAX_CONCURRENT_CONNECTIONS (the old
        // load-then-fetch_add was a TOCTOU). The matching release happens in
        // ConnectionGuard::drop on the worker thread.
        if active_connections
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |active| {
                (active < MAX_CONCURRENT_CONNECTIONS).then_some(active + 1)
            })
            .is_err()
        {
            tracing::warn!("connection rejected: too many concurrent clients");
            // Tell the client explicitly so it fails fast instead of blocking
            // until its read timeout. Write synchronously with a short timeout
            // rather than on a detached thread: spawning a thread per rejection
            // is unbounded, so a connect-flood while saturated could exhaust
            // threads/memory. A short timeout caps how long a stuck client that
            // never reads can pin the accept loop (the rejection is best-effort
            // — errors are ignored since we're closing the connection anyway).
            let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
            let _ = helper_ipc::write_response(
                &mut stream,
                &HelperResponse::Error {
                    message: "too many concurrent clients".to_string(),
                },
            );
            continue;
        }
        let coordinator = Arc::clone(&coordinator);
        let active_connections = Arc::clone(&active_connections);
        std::thread::spawn(move || {
            let _guard = ConnectionGuard(active_connections);
            if let Err(e) = serve_connection(stream, &coordinator) {
                tracing::warn!("connection error: {e}");
            }
        });
    }

    Ok(())
}

#[cfg(target_os = "macos")]
struct ConnectionGuard(Arc<AtomicUsize>);

#[cfg(target_os = "macos")]
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("caffeinate2-helper only supports macOS.");
    std::process::exit(1);
}
