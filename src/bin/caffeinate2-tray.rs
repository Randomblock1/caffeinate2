#[cfg(all(target_os = "macos", feature = "tray"))]
fn main() {
    if let Err(e) = caffeinate2::tray::run() {
        eprintln!("caffeinate2-tray error: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(all(target_os = "macos", feature = "tray")))]
fn main() {
    eprintln!("caffeinate2-tray requires macOS and the tray feature.");
    std::process::exit(1);
}
