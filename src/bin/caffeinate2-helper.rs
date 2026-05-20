#[cfg(target_os = "macos")]
use caffeinate2::entirely::EntirelyCoordinator;
#[cfg(target_os = "macos")]
use caffeinate2::helper_ipc::{self, serve_connection};
#[cfg(target_os = "macos")]
use std::os::unix::net::UnixListener;
#[cfg(target_os = "macos")]
use std::sync::Arc;

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
    let coordinator = Arc::new(EntirelyCoordinator::helper_daemon(verbose));

    eprintln!("caffeinate2-helper listening on {socket_path}");

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept failed: {e}");
                continue;
            }
        };
        if let Err(e) = serve_connection(stream, &coordinator) {
            eprintln!("connection error: {e}");
        }
    }

    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("caffeinate2-helper only supports macOS.");
    std::process::exit(1);
}
