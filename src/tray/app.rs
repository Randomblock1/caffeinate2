use crate::sleep::sleep_mode::SleepMode;
use crate::tray::menu::{MenuAction, build_menu, handle_menu_event, install_menu};
use crate::tray::state::AppState;
use crate::tray::tray_icons;
use crate::tray::wait_window::{self, WaitWindow, WaitWindowMsg};
use objc2_foundation::MainThreadMarker;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

fn poll_stop_conditions(state: &mut AppState) -> bool {
    // `|` (not `||`): every poll must run each tick. `check_app_watch` and
    // `poll_upgrade` both maintain per-tick state (app-seen, clear debounce),
    // so short-circuiting would skip the watcher whenever the timer fires.
    let timeout = state.check_timeout();
    let app_watch = state.check_app_watch();
    let upgrade = state.poll_upgrade();
    timeout | app_watch | upgrade
}

///
/// # Errors
///
/// Returns an error if tray setup or icon decoding fails.
pub fn run() -> Result<(), String> {
    crate::tray::macos_activation::init_tray_app();
    let (tray_events, menu_events) = crate::tray::macos_activation::install_tray_event_handlers();
    let (workspace_dirty, _workspace_guard) =
        crate::tray::macos_activation::install_workspace_observers();
    // tray-icon requires a running main-thread event loop before creating the icon.
    crate::tray::macos_activation::pump_event_loop(Some(Duration::from_millis(16)));

    let (icon_off_rgba, icon_width, icon_height) =
        tray_icons::decode_icon_rgba(tray_icons::ICON_OFF)?;
    let icon_off =
        Icon::from_rgba(icon_off_rgba, icon_width, icon_height).map_err(|e| e.to_string())?;

    // Everything below runs on the main thread only; muda menu items are not
    // Send, so no locking or sharing is involved.
    let mtm = MainThreadMarker::new().ok_or("tray must run on the main thread")?;
    let mut state = AppState::new();
    let initial = state.menu_snapshot();

    // The picker window reports its result back over this channel, drained at
    // the top of the loop so the selection is applied outside the button
    // action (which runs re-entrantly inside the shared event pump).
    let (wait_tx, wait_rx) = std::sync::mpsc::channel::<WaitWindowMsg>();
    let mut wait_window: Option<WaitWindow> = None;

    let (menu, initial_handles) = build_menu(&initial);
    let tray = TrayIconBuilder::new()
        .with_icon(icon_off)
        .with_icon_as_template(true)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("caffeinate2")
        .build()
        .map_err(|e| e.to_string())?;
    crate::tray::macos_activation::wake_event_loop();

    let mut handles = initial_handles;

    'main: loop {
        if poll_stop_conditions(&mut state) {
            state.set_icon(&tray);
        } else if state.is_on() {
            state.update_tooltip(&tray);
        }

        while let Ok(event) = menu_events.try_recv() {
            if event.id == handles.wait_for_apps_id {
                // Modeless: open it (if not already up) and let the existing
                // pump drive it. Don't act on the selection here.
                if wait_window.is_none() {
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
            if let WaitWindowMsg::Apply(targets) = msg {
                if let Err(e) = state.set_wait_for_apps(targets) {
                    eprintln!("{e}");
                } else if state.is_on() {
                    state.invalidate_tooltip();
                    state.update_tooltip(&tray);
                }
                // Rebuild so the "(N selected)" label updates.
                handles = install_menu(&tray, &state.menu_snapshot());
            }
            // The window has closed itself; drop our handle to release it.
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
                // Optimistically show the target state and pump one run-loop
                // pass so the new image is committed to the menu bar *before*
                // the potentially slow hold acquisition/release (helper RPC +
                // IOPMSetSystemPowerSetting in Entirely mode) blocks this
                // thread. `set_icon` below re-syncs icon and tooltip to the
                // real state, reverting the flip if the toggle failed.
                AppState::show_icon_state(&tray, !state.is_on());
                // Turning Entirely on can additionally block on the helper
                // install (admin prompt + up to ~5s of socket retries);
                // surface that in the tooltip while it runs.
                if !state.is_on() && state.menu_snapshot().mode == SleepMode::Entirely {
                    let _ = tray.set_tooltip(Some("caffeinate2 (enabling Entirely mode…)"));
                    state.invalidate_tooltip();
                }
                crate::tray::macos_activation::pump_event_loop(Some(Duration::from_millis(1)));
                match state.toggle() {
                    Ok(()) => state.set_icon(&tray),
                    Err(e) => {
                        eprintln!("{e}");
                        state.set_icon(&tray);
                        // Leave the reason visible on hover (e.g. an
                        // authorization denial with grant instructions).
                        state.show_error_tooltip(&tray, &e.to_string());
                    }
                }
            }
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

        crate::tray::macos_activation::pump_event_loop(state.pump_timeout());
    }

    state.stop_session();
    Ok(())
}
