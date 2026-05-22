use crate::app_target::AppTarget;
use crate::macos_apps;
use crate::sleep_mode::SleepMode;
use crate::tray::state::AppState;
use crate::tray_mode::TimeLimitPreset;
use muda::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::menu::MenuId;

pub struct MenuHandles {
    pub mode_items: Vec<(MenuId, SleepMode, CheckMenuItem)>,
    pub time_limit_items: Vec<(MenuId, Option<u64>, CheckMenuItem)>,
    pub until_app_off: CheckMenuItem,
    pub until_app_off_id: MenuId,
    pub until_app_items: Vec<(MenuId, AppTarget, CheckMenuItem)>,
    pub choose_app_id: MenuId,
    pub start_at_login: CheckMenuItem,
    pub start_at_login_id: MenuId,
    pub quit_id: MenuId,
}

pub fn running_apps_menu_key(apps: &[AppTarget]) -> Vec<(String, String)> {
    apps.iter()
        .map(|a| (a.bundle_id.clone(), a.name.clone()))
        .collect()
}

pub fn build_menu(state: &AppState, running_apps: &[AppTarget]) -> (Menu, MenuHandles) {
    let menu = Menu::new();
    let mut mode_items = Vec::new();

    for mode in SleepMode::all() {
        let item = CheckMenuItem::new(mode.label(), true, state.mode == mode, None);
        let id = item.id().clone();
        mode_items.push((id, mode, item.clone()));
        menu.append(&item).expect("append mode item");
    }

    menu.append(&PredefinedMenuItem::separator()).expect("separator");

    let time_limit_submenu = Submenu::new("Time limit", true);
    let mut time_limit_items = Vec::new();
    for preset in TimeLimitPreset::ALL {
        let item = CheckMenuItem::new(
            preset.label,
            true,
            state.time_limit_secs == preset.seconds,
            None,
        );
        let id = item.id().clone();
        time_limit_items.push((id, preset.seconds, item.clone()));
        time_limit_submenu
            .append(&item)
            .expect("append time limit item");
    }
    menu.append(&time_limit_submenu)
        .expect("append time limit submenu");

    let until_app_submenu = Submenu::new("Until app quits", true);
    let off_item = CheckMenuItem::new("Off", true, state.wait_for_app.is_none(), None);
    let until_app_off_id = off_item.id().clone();
    until_app_submenu
        .append(&off_item)
        .expect("append until app off");

    let mut until_app_items = Vec::new();
    if !running_apps.is_empty() {
        until_app_submenu
            .append(&PredefinedMenuItem::separator())
            .expect("separator");
        for app in running_apps {
            let checked = state
                .wait_for_app
                .as_ref()
                .is_some_and(|w| w.bundle_id == app.bundle_id);
            let item = CheckMenuItem::new(&app.name, true, checked, None);
            let id = item.id().clone();
            until_app_items.push((id, app.clone(), item.clone()));
            until_app_submenu
                .append(&item)
                .expect("append running app");
        }
    }

    until_app_submenu
        .append(&PredefinedMenuItem::separator())
        .expect("separator");
    let choose_app = MenuItem::new("Choose application…", true, None);
    let choose_app_id = choose_app.id().clone();
    until_app_submenu
        .append(&choose_app)
        .expect("append choose app");

    menu.append(&until_app_submenu)
        .expect("append until app submenu");

    menu.append(&PredefinedMenuItem::separator()).expect("separator");

    let start_at_login = CheckMenuItem::new("Start at login", true, state.start_at_login, None);
    let start_at_login_id = start_at_login.id().clone();
    menu.append(&start_at_login).expect("append login item");

    menu.append(&PredefinedMenuItem::separator()).expect("separator");

    let quit = MenuItem::new("Quit", true, None);
    let quit_id = quit.id().clone();
    menu.append(&quit).expect("append quit");

    (
        menu,
        MenuHandles {
            mode_items,
            time_limit_items,
            until_app_off: off_item,
            until_app_off_id,
            until_app_items,
            choose_app_id,
            start_at_login,
            start_at_login_id,
            quit_id,
        },
    )
}

pub fn refresh_mode_checks(handles: &MenuHandles, selected: SleepMode) {
    for (_, mode, item) in &handles.mode_items {
        let _ = item.set_checked(*mode == selected);
    }
}

pub fn refresh_time_limit_checks(handles: &MenuHandles, selected: Option<u64>) {
    for (_, secs, item) in &handles.time_limit_items {
        let _ = item.set_checked(*secs == selected);
    }
}

pub fn set_until_app_checks(handles: &MenuHandles, selected: Option<&AppTarget>) {
    let _ = handles.until_app_off.set_checked(selected.is_none());
    for (_, app, item) in &handles.until_app_items {
        let _ = item.set_checked(selected.is_some_and(|target| target.bundle_id == app.bundle_id));
    }
}

pub fn install_menu(
    tray: &tray_icon::TrayIcon,
    state: &AppState,
    running_apps: &[AppTarget],
) -> MenuHandles {
    let (menu, handles) = build_menu(state, running_apps);
    let _ = tray.set_menu(Some(Box::new(menu)));
    handles
}

pub enum MenuAction {
    Quit,
    Handled,
    Unhandled,
}

pub fn handle_menu_event(
    event_id: &MenuId,
    handles: &MenuHandles,
    state: &mut AppState,
    tray: &tray_icon::TrayIcon,
) -> MenuAction {
    if event_id == &handles.quit_id {
        return MenuAction::Quit;
    }
    if event_id == &handles.start_at_login_id {
        let new_val = !state.start_at_login;
        if let Err(e) = state.set_start_at_login(new_val) {
            eprintln!("{e}");
        } else {
            let _ = handles.start_at_login.set_checked(new_val);
        }
        return MenuAction::Handled;
    }
    if event_id == &handles.until_app_off_id {
        if let Err(e) = state.set_wait_for_app(None) {
            eprintln!("{e}");
        } else {
            set_until_app_checks(handles, None);
        }
        return MenuAction::Handled;
    }
    if event_id == &handles.choose_app_id {
        return MenuAction::Unhandled;
    }
    for (id, app, _) in &handles.until_app_items {
        if event_id == id {
            if let Err(e) = state.set_wait_for_app(Some(app.clone())) {
                eprintln!("{e}");
            } else {
                set_until_app_checks(handles, Some(app));
                if state.is_on() {
                    state.last_tooltip = None;
                    state.update_tooltip(tray);
                }
            }
            return MenuAction::Handled;
        }
    }
    for (id, mode, _) in &handles.mode_items {
        if event_id == id {
            if let Err(e) = state.set_mode(*mode) {
                eprintln!("{e}");
            } else {
                refresh_mode_checks(handles, *mode);
            }
            state.set_icon(tray);
            return MenuAction::Handled;
        }
    }
    for (id, secs, _) in &handles.time_limit_items {
        if event_id == id {
            if let Err(e) = state.set_time_limit(*secs) {
                eprintln!("{e}");
            } else {
                refresh_time_limit_checks(handles, *secs);
                if state.is_on() {
                    state.update_tooltip(tray);
                }
            }
            return MenuAction::Handled;
        }
    }
    MenuAction::Unhandled
}

pub fn handle_choose_app(
    state: &mut AppState,
    tray: &tray_icon::TrayIcon,
) -> Option<Vec<AppTarget>> {
    let choice = macos_apps::choose_app_bundle()?;
    if let Err(e) = state.set_wait_for_app(Some(choice)) {
        eprintln!("{e}");
    } else if state.is_on() {
        state.last_tooltip = None;
        state.update_tooltip(tray);
    }
    Some(macos_apps::running_app_choices())
}
