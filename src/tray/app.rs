use crate::macos_apps;
use crate::tray::menu::{
    build_menu, handle_choose_app, handle_menu_event, install_menu, running_apps_menu_key,
    MenuAction,
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

    let icon_off_rgba = tray_icons::decode_icon_rgba(tray_icons::ICON_OFF)?;
    let icon_off = Icon::from_rgba(icon_off_rgba, tray_icons::ICON_SIZE, tray_icons::ICON_SIZE)
        .map_err(|e| e.to_string())?;

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
            if let TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            } = event
            {
                if let Err(e) = state.toggle() {
                    eprintln!("{e}");
                }
                state.set_icon(&tray);
            }
        }

        crate::macos_activation::pump_event_loop(state.pump_timeout());
    }

    state.stop_session();
    Ok(())
}
