use tracing_subscriber::EnvFilter;

/// Initialize tracing for CLI binaries (`caffeinate2`, `sleepdetect`, `caffeinate2-tray`).
///
/// An explicit `RUST_LOG` always wins. Otherwise the default filter is
/// `warn,caffeinate2=info`, promoted to `warn,caffeinate2=debug` when `verbose`
/// is set, so the assertion-lifecycle `debug!` output is actually emitted rather
/// than being filtered out. Callers pass their own clap-parsed `-v`/`--verbose`
/// value: this correctly handles bundled short flags (`-vt`, `-vi`) and does not
/// misread a `-v` belonging to a wrapped command after `--`.
pub fn init_cli_tracing(verbose: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(if verbose {
            "warn,caffeinate2=debug"
        } else {
            "warn,caffeinate2=info"
        })
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
}

/// Initialize tracing for the privileged helper daemon.
///
/// On macOS, logs go to the unified system log via os_log (subsystem
/// `com.randomblock1.caffeinate2.helper` — view with Console.app or
/// `log show`) and to stderr, which launchd discards for the daemon; the
/// stderr layer serves a helper run by hand. Respects `RUST_LOG`.
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

    // Panics bypass tracing entirely and go to the raw stderr fd, which
    // launchd points at /dev/null for the daemon — without this hook a panic
    // in a connection worker (or a KeepAlive restart loop's cause) would
    // leave no trace anywhere. Route the text through tracing, and thus
    // os_log, before the default hook runs.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default_hook(info);
    }));
}

#[cfg(not(target_os = "macos"))]
pub fn init_helper_tracing() {
    init_cli_tracing(false);
}
