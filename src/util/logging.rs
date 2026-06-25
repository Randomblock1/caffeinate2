use tracing_subscriber::EnvFilter;

/// Initialize tracing for CLI binaries (`caffeinate2`, `sleepdetect`, `caffeinate2-tray`).
///
/// Respects `RUST_LOG`. Default filter: `warn,caffeinate2=info`.
pub fn init_cli_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,caffeinate2=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
}

/// Initialize tracing for the privileged helper daemon.
///
/// On macOS, logs go to Console.app via os_log and to stderr (captured by
/// launchd in `/var/log/caffeinate2-helper.log`). Respects `RUST_LOG`.
/// Default filter: `warn,caffeinate2=info`.
#[cfg(target_os = "macos")]
pub fn init_helper_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,caffeinate2=info"));
    let oslog = tracing_oslog::OsLogger::new("com.randomblock1.caffeinate2.helper", "default");
    let stderr = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr);
    tracing_subscriber::registry()
        .with(filter)
        .with(oslog)
        .with(stderr)
        .try_init()
        .ok();
}

#[cfg(not(target_os = "macos"))]
pub fn init_helper_tracing() {
    init_cli_tracing();
}
