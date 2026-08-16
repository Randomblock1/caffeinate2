use crate::sleep::sleep_mode::SleepMode;
use crate::tray::error::TrayError;
use crate::tray::menu::{
    MenuAction, MenuHandles, build_menu, handle_menu_event, install_menu, sync_menu_to_snapshot,
};
use crate::tray::single_instance;
use crate::tray::state::{AppState, PendingEnableOutcome, PendingInstallOutcome};
use crate::tray::tray_icons;
use crate::tray::wait_window::{self, WaitWindow, WaitWindowMsg};
use objc2_foundation::MainThreadMarker;
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::Signals,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

/// Longest the main event pump will block when otherwise idle, so an
/// off-main-thread shutdown signal is noticed promptly (see the pump call site).
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Set by the signal thread when SIGINT/SIGTERM arrives. A flag rather than a
/// channel because more than the run loop has to see it: a modal dialog runs
/// its own nested event loop, and must tear itself down instead of holding the
/// process (and its sleep assertions) open until somebody clicks a button.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Whether the process has been asked to quit. Long-running main-thread work
/// must poll this and bail out, so `run` reaches the teardown that releases the
/// holds.
pub(crate) fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Relaxed)
}

fn poll_stop_conditions(state: &mut AppState) -> bool {
    // `|` (not `||`): every poll must run each tick. `check_app_watch` and
    // `poll_upgrade` both maintain per-tick state (app-seen, clear debounce),
    // so short-circuiting would skip the watcher whenever the timer fires.
    let timeout = state.check_timeout();
    let app_watch = state.check_app_watch();
    let upgrade = state.poll_upgrade();
    timeout | app_watch | upgrade
}

fn poll_pending_enable(state: &mut AppState, tray: &tray_icon::TrayIcon, handles: &MenuHandles) {
    match state.poll_pending_enable() {
        PendingEnableOutcome::Idle | PendingEnableOutcome::Pending => {}
        PendingEnableOutcome::Started => {
            state.set_icon(tray);
            // An async mode switch can commit a different mode than the menu
            // currently shows; re-sync the checkboxes to the committed state.
            sync_menu_to_snapshot(handles, &state.menu_snapshot());
        }
        PendingEnableOutcome::Failed {
            error,
            started_by_upgrade,
        } => {
            state.set_icon(tray);
            // Background (upgrade-watcher) failures are already logged,
            // rate-limited, in poll_pending_enable and must not raise the
            // user-facing error tooltip, which is reserved for explicit actions.
            if !started_by_upgrade {
                eprintln!("{error}");
                state.show_error_tooltip(tray, &error.to_string());
            }
            // A failed mode switch rolled `config.mode` back; the menu checkbox
            // was optimistically moved to the failed target when the command ran
            // (muda auto-toggles, then dispatch_command re-synced to the target),
            // so re-sync it to the rolled-back state here.
            sync_menu_to_snapshot(handles, &state.menu_snapshot());
        }
    }
}

fn poll_pending_install(state: &mut AppState, tray: &tray_icon::TrayIcon, handles: &MenuHandles) {
    match state.poll_pending_install() {
        PendingInstallOutcome::Idle | PendingInstallOutcome::Pending => {}
        PendingInstallOutcome::Installed => {
            // Upgrade any already-present external assertion right away rather
            // than waiting for the next poll interval.
            state.poll_upgrade();
            state.set_icon(tray);
            // The watcher just committed `upgrade_external`; re-sync the checkbox
            // (a menu rebuild from `menu_dirty`, if any, happens later in the
            // loop and also reflects it).
            sync_menu_to_snapshot(handles, &state.menu_snapshot());
        }
        PendingInstallOutcome::Failed(error) => {
            eprintln!("{error}");
            state.set_icon(tray);
            state.show_error_tooltip(tray, &error.to_string());
            // The install did not commit; re-sync so the optimistically-checked
            // box (muda auto-toggle) returns to unchecked.
            sync_menu_to_snapshot(handles, &state.menu_snapshot());
        }
    }
}

///
/// # Errors
///
/// Returns an error if tray setup or icon decoding fails.
pub fn run() -> Result<(), TrayError> {
    single_instance::acquire_or_exit();
    // Validate we're on the main thread before any AppKit-backed setup runs:
    // init_tray_app, the event-handler/observer installers, and the event-loop
    // pump all assume the main thread. Bail early rather than touch AppKit off it.
    let mtm = MainThreadMarker::new().ok_or(TrayError::NotMainThread)?;
    crate::tray::macos_activation::init_tray_app();
    let (tray_events, menu_events) = crate::tray::macos_activation::install_tray_event_handlers();
    let (workspace_dirty, _workspace_guard) =
        crate::tray::macos_activation::install_workspace_observers();

    thread::spawn(move || {
        let mut signals =
            Signals::new([SIGINT, SIGTERM]).expect("failed to create signal iterator");
        if signals.forever().next().is_some() {
            crate::tray::macos_activation::wake_event_loop();
        }
    });

    // Detect a version-skewed installed helper. The helper is a copied
    // snapshot that launchd keeps serving across package upgrades, so a skew
    // silently misses fixes until the older side (helper or this binary) is
    // updated. Off the main thread: it is a socket RPC.
    thread::spawn(|| {
        use crate::entirely::helper_ipc::{HelperClient, HelperStatus};
        let client = HelperClient::new();
        if client.is_available()
            && let Ok(status) = client.status()
            && status.is_stale()
        {
            if status.helper_is_newer() {
                // Reinstalling from this binary would downgrade the helper;
                // the fix is updating this binary instead.
                tracing::warn!(
                    "installed caffeinate2 helper is version {}, this binary is only {}; update this caffeinate2 binary",
                    status.version.as_deref().unwrap_or("pre-0.8.0"),
                    HelperStatus::CLIENT_VERSION,
                );
            } else {
                tracing::warn!(
                    "installed caffeinate2 helper is version {}, this binary is {}; update it with: sudo caffeinate2 --install-helper",
                    status.version.as_deref().unwrap_or("pre-0.8.0"),
                    HelperStatus::CLIENT_VERSION,
                );
            }
        }
    });

    // tray-icon requires a running main-thread event loop before creating the icon.
    crate::tray::macos_activation::pump_event_loop(Some(Duration::from_millis(16)));

    let (icon_off_rgba, icon_width, icon_height) =
        tray_icons::decode_icon_rgba(tray_icons::ICON_OFF)?;
    let icon_off = Icon::from_rgba(icon_off_rgba, icon_width, icon_height)
        .map_err(|e| TrayError::BuildIcon(e.to_string()))?;

    // Everything below runs on the main thread only (validated above via `mtm`);
    // muda menu items are not Send, so no locking or sharing is involved.
    let mut state = AppState::new();
    let initial = state.menu_snapshot();

    // The picker window reports its result back over this channel, drained at
    // the top of the loop so the selection is applied outside the button
    // action (which runs re-entrantly inside the shared event pump).
    let (wait_tx, wait_rx) = mpsc::channel::<WaitWindowMsg>();
    let mut wait_window: Option<WaitWindow> = None;

    let (menu, initial_handles) = build_menu(&initial);
    let tray = TrayIconBuilder::new()
        .with_icon(icon_off)
        .with_icon_as_template(true)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("caffeinate2")
        .build()
        .map_err(|e| TrayError::TraySetup(e.to_string()))?;
    crate::tray::macos_activation::wake_event_loop();

    let mut handles = initial_handles;

    'main: loop {
        if shutdown_requested() {
            break 'main;
        }

        poll_pending_enable(&mut state, &tray, &handles);
        poll_pending_install(&mut state, &tray, &handles);

        if poll_stop_conditions(&mut state) {
            state.set_icon(&tray);
        } else if state.is_on() || state.is_enabling() || state.is_installing() {
            state.update_tooltip(&tray);
        }

        while let Ok(event) = menu_events.try_recv() {
            if event.id == handles.wait_for_apps_id {
                if let Some(window) = wait_window.as_ref() {
                    window.bring_to_front();
                } else {
                    let selected = state.menu_snapshot().wait_for_apps;
                    wait_window = Some(wait_window::open(mtm, &selected, wait_tx.clone()));
                }
                continue;
            }

            match handle_menu_event(&event.id, &handles, &mut state, &tray) {
                MenuAction::Quit => break 'main,
                MenuAction::Handled | MenuAction::Unhandled => {}
            }
        }

        // Apply the picker's result once it closes (see `wait_tx` above).
        while let Ok(msg) = wait_rx.try_recv() {
            match msg {
                WaitWindowMsg::Apply(targets) => {
                    if let Err(e) = state.set_wait_for_apps(targets) {
                        eprintln!("{e}");
                    } else if state.is_on() {
                        state.invalidate_tooltip();
                        state.update_tooltip(&tray);
                    }
                    // Rebuild so the "(N selected)" label updates.
                    handles = install_menu(&tray, &state.menu_snapshot());
                }
                // Cancel: discard without touching config; the run loop only
                // needs to drop its handle so the window can be reopened.
                WaitWindowMsg::Cancel => {}
            }
            wait_window = None;
        }

        while let Ok(event) = tray_events.try_recv() {
            // Toggle on mouse-down: native menu bar items respond at press
            // time, so waiting for mouse-up reads as lag.
            if let TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Down,
                ..
            } = event
            {
                let turning_on = !state.is_on() && !state.is_enabling();
                state.show_icon_state(&tray, !state.is_on() || state.is_enabling());
                if turning_on && state.menu_snapshot().mode == SleepMode::Entirely {
                    let _ = tray.set_tooltip(Some("caffeinate2 (enabling Entirely mode…)"));
                    state.invalidate_tooltip();
                }
                crate::tray::macos_activation::pump_event_loop(Some(Duration::from_millis(1)));
                state.toggle();
                state.set_icon(&tray);
            }
        }

        if let Some(window) = wait_window.as_ref() {
            window.poll_debounce();
        }

        // App launch/quit wakes the loop (so `check_app_watch` runs promptly).
        // If the picker is open, re-scan its list so newly launched or quit
        // programs appear/disappear live.
        if workspace_dirty.swap(false, Ordering::Relaxed)
            && let Some(window) = wait_window.as_ref()
        {
            window.refresh();
        }

        // The watcher changes the menu's structure (the "Upgrading…" entries)
        // outside of any user action, so rebuild when it flags a change.
        if state.take_menu_dirty() {
            handles = install_menu(&tray, &state.menu_snapshot());
        }

        // Pending picker work must wake the loop within its own debounce window
        // so `poll_debounce` can fire; otherwise an idle `pump_timeout()` (None)
        // would block until some unrelated event arrives and the list would
        // never update after the user stops typing (or after a launch burst).
        let mut pump_timeout = state.pump_timeout();
        if let Some(pending) = wait_window.as_ref().and_then(WaitWindow::pending_timeout) {
            pump_timeout = Some(pump_timeout.map_or(pending, |timeout| timeout.min(pending)));
        }
        // Never block the pump indefinitely. The SIGINT/SIGTERM handler runs off
        // the main thread, where `wake_event_loop` can only nudge the run loop
        // (it cannot post an NSEvent without a main-thread marker), so a fully
        // blocked `nextEventMatchingMask` would miss shutdown until some
        // unrelated event arrived. Capping the idle wait bounds shutdown latency
        // without busy-looping; non-idle ticks (countdown, polling) already use
        // shorter timeouts, so this only affects the otherwise-infinite case.
        let pump_timeout = pump_timeout.unwrap_or(SHUTDOWN_POLL_INTERVAL);
        crate::tray::macos_activation::pump_event_loop(Some(pump_timeout));
    }

    state.shutdown();
    Ok(())
}
