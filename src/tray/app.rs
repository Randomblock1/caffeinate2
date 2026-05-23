use crate::macos_apps;
use crate::tray::menu::{
    build_menu, handle_choose_app, handle_menu_event, install_menu, running_apps_menu_key,
    MenuAction,
};
use crate::tray::state::AppState;
use crate::tray_icons;
use muda::MenuEvent;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

fn poll_stop_conditions(state: &mut AppState) -> bool {
    state.check_timeout() || state.check_app_watch()
}

pub fn run() -> Result<(), String> {
    crate::macos_activation::hide_dock_icon();

    let icon_off_rgba = tray_icons::decode_icon_rgba(tray_icons::ICON_OFF)?;
    let icon_off = Icon::from_rgba(icon_off_rgba, tray_icons::ICON_SIZE, tray_icons::ICON_SIZE)
        .map_err(|e| e.to_string())?;

    let state = Arc::new(Mutex::new(AppState::new()));
    let running_apps = macos_apps::running_app_choices();
    let initial = state.lock().expect("state lock").menu_snapshot();
    let mut menu_apps_key = running_apps_menu_key(&running_apps);

    let (menu, initial_handles) = build_menu(&initial, &running_apps);
    let tray = TrayIconBuilder::new()
        .with_icon(icon_off)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("caffeinate2")
        .build()
        .map_err(|e| e.to_string())?;

    let handles = Arc::new(Mutex::new(initial_handles));
    let mut last_menu_refresh = Instant::now();

    loop {
        let mut state_changed = false;

        {
            let mut s = state.lock().expect("state lock");
            if poll_stop_conditions(&mut s) {
                state_changed = true;
            }
            if state_changed {
                s.set_icon(&tray);
            } else if s.is_on() {
                s.update_tooltip(&tray);
            }
        }

        if last_menu_refresh.elapsed() >= Duration::from_secs(2) {
            last_menu_refresh = Instant::now();
            let apps = macos_apps::running_app_choices();
            let key = running_apps_menu_key(&apps);
            if key != menu_apps_key {
                menu_apps_key = key;
                let snapshot = state.lock().expect("state lock").menu_snapshot();
                let new_handles = install_menu(&tray, &snapshot, &apps);
                *handles.lock().expect("handles lock") = new_handles;
            }
        }

        if let Ok(event) = MenuEvent::receiver().try_recv() {
            let h = handles.lock().expect("handles lock");
            if event.id == h.choose_app_id {
                drop(h);
                if let Some(apps) = {
                    let mut s = state.lock().expect("state lock");
                    handle_choose_app(&mut s, &tray)
                } {
                    menu_apps_key = running_apps_menu_key(&apps);
                    let snapshot = state.lock().expect("state lock").menu_snapshot();
                    *handles.lock().expect("handles lock") = install_menu(&tray, &snapshot, &apps);
                    let selected = state.lock().expect("state lock").wait_for_app.clone();
                    let h = handles.lock().expect("handles lock");
                    crate::tray::menu::set_until_app_checks(&h, selected.as_ref());
                }
                continue;
            }

            let action = {
                let mut s = state.lock().expect("state lock");
                handle_menu_event(&event.id, &h, &mut s, &tray)
            };
            match action {
                MenuAction::Quit => break,
                MenuAction::Handled | MenuAction::Unhandled => {}
            }
        }

        if let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            } = event
            {
                let mut s = state.lock().expect("state lock");
                if let Err(e) = s.toggle() {
                    eprintln!("{e}");
                }
                s.set_icon(&tray);
            }
        }

        std::thread::sleep(Duration::from_millis(16));
    }

    let mut s = state.lock().expect("state lock");
    s.clear_active();
    Ok(())
}
