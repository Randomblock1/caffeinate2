#[cfg(target_os = "macos")]
use caffeinate2::entirely::coordinator::EntirelyCoordinator;
#[cfg(target_os = "macos")]
use caffeinate2::entirely::helper_ipc::{self, serve_connection};
#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "macos")]
use std::os::unix::net::UnixListener;
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(target_os = "macos")]
const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_CONCURRENT_CONNECTIONS: usize = 32;

#[cfg(target_os = "macos")]
fn main() {
    if let Err(e) = run() {
        eprintln!("caffeinate2-helper error: {e}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn run() -> Result<(), String> {
    let verbose = std::env::args().any(|a| a == "--verbose" || a == "-v");
    let socket_path = helper_ipc::HELPER_SOCKET_PATH;
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path).map_err(|e| e.to_string())?;
    // Allow non-root clients (tray, CLI) to connect; peers are identified via
    // LocalPeerPid, not socket permissions.
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o666))
        .map_err(|e| format!("failed to set socket permissions: {e}"))?;
    let coordinator = Arc::new(EntirelyCoordinator::helper_daemon(verbose));

    // Re-sync the global SleepDisabled setting with whatever the lockfile says
    // is still alive. An empty lockfile does not force re-enable sleep, since
    // that could undo an unrelated manual `pmset disablesleep`.
    if let Err(e) = coordinator.reconcile_startup() {
        eprintln!("startup reconcile failed: {e}");
    }

    // Reap holders that died without sending Release (crashed or killed
    // clients) and converge the sleep setting.
    let reaper = Arc::clone(&coordinator);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(RECONCILE_INTERVAL);
            if let Err(e) = reaper.reconcile() {
                eprintln!("periodic reconcile failed: {e}");
            }
        }
    });

    eprintln!("caffeinate2-helper listening on {socket_path}");

    let active_connections = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept failed: {e}");
                continue;
            }
        };
        let active = active_connections.load(Ordering::Relaxed);
        if active >= MAX_CONCURRENT_CONNECTIONS {
            eprintln!("connection rejected: too many concurrent clients");
            continue;
        }
        active_connections.fetch_add(1, Ordering::Relaxed);
        let coordinator = Arc::clone(&coordinator);
        let active_connections = Arc::clone(&active_connections);
        std::thread::spawn(move || {
            let _guard = ConnectionGuard(active_connections);
            if let Err(e) = serve_connection(stream, &coordinator) {
                eprintln!("connection error: {e}");
            }
        });
    }

    Ok(())
}

struct ConnectionGuard(Arc<AtomicUsize>);

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
