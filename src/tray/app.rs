use crate::macos_apps;
use crate::sleep_mode::SleepMode;
use crate::tray::menu::{
    MenuAction, build_menu, handle_choose_app, handle_menu_event, install_menu,
    running_apps_menu_key,
};
use crate::tray::state::AppState;
use crate::tray_icons;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

fn poll_stop_conditions(state: &mut AppState) -> bool {
    state.check_timeout() || state.check_app_watch()
}

pub fn run() -> Result<(), String> {
    crate::macos_activation::init_tray_app();
    let (tray_events, menu_events) = crate::macos_activation::install_tray_event_handlers();
    let (workspace_dirty, _workspace_guard) =
        crate::macos_activation::install_workspace_observers();
    // tray-icon requires a running main-thread event loop before creating the icon.
    crate::macos_activation::pump_event_loop(Some(Duration::from_millis(16)));

    let (icon_off_rgba, icon_width, icon_height) =
        tray_icons::decode_icon_rgba(tray_icons::ICON_OFF)?;
    let icon_off =
        Icon::from_rgba(icon_off_rgba, icon_width, icon_height).map_err(|e| e.to_string())?;

    // Everything below runs on the main thread only; muda menu items are not
    // Send, so no locking or sharing is involved.
    let mut state = AppState::new();
    let running_apps = macos_apps::running_app_choices();
    let initial = state.menu_snapshot();
    let mut menu_apps_key = running_apps_menu_key(&running_apps);

    let (menu, initial_handles) = build_menu(&initial, &running_apps);
    let tray = TrayIconBuilder::new()
        .with_icon(icon_off)
        .with_icon_as_template(true)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("caffeinate2")
        .build()
        .map_err(|e| e.to_string())?;
    crate::macos_activation::wake_event_loop();

    let mut handles = initial_handles;

    'main: loop {
        if poll_stop_conditions(&mut state) {
            state.set_icon(&tray);
        } else if state.is_on() {
            state.update_tooltip(&tray);
        }

        if workspace_dirty.swap(false, Ordering::Relaxed) {
            let apps = macos_apps::running_app_choices();
            let key = running_apps_menu_key(&apps);
            if key != menu_apps_key {
                menu_apps_key = key;
                handles = install_menu(&tray, &state.menu_snapshot(), &apps);
            }
        }

        while let Ok(event) = menu_events.try_recv() {
            if event.id == handles.choose_app_id {
                if let Some(apps) = handle_choose_app(&mut state, &tray) {
                    menu_apps_key = running_apps_menu_key(&apps);
                    handles = install_menu(&tray, &state.menu_snapshot(), &apps);
                }
                continue;
            }

            match handle_menu_event(&event.id, &handles, &mut state, &tray) {
                MenuAction::Quit => break 'main,
                MenuAction::Handled | MenuAction::Unhandled => {}
            }
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
                crate::macos_activation::pump_event_loop(Some(Duration::from_millis(1)));
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

        crate::macos_activation::pump_event_loop(state.pump_timeout());
    }

    state.stop_session();
    Ok(())
}
