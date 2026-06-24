use crate::sleep::sleep_mode::SleepMode;
use crate::tray::state::{AppState, IgnoredAssertion, MenuSnapshot};
use crate::tray::tray_mode::TimeLimitPreset;
use muda::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::menu::MenuId;

pub struct MenuHandles {
    pub mode_items: Vec<(MenuId, SleepMode, CheckMenuItem)>,
    pub time_limit_items: Vec<(MenuId, Option<u64>, CheckMenuItem)>,
    pub wait_for_apps_id: MenuId,
    pub upgrade_external: CheckMenuItem,
    pub upgrade_external_id: MenuId,
    pub start_at_login: CheckMenuItem,
    pub start_at_login_id: MenuId,
    pub quit_id: MenuId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCommand {
    Quit,
    ToggleStartAtLogin,
    ToggleUpgradeExternal,
    OpenWaitForAppsWindow,
    SetMode(SleepMode),
    SetTimeLimit(Option<u64>),
}

impl MenuHandles {
    pub fn resolve(&self, event_id: &MenuId) -> Option<MenuCommand> {
        if event_id == &self.quit_id {
            return Some(MenuCommand::Quit);
        }
        if event_id == &self.start_at_login_id {
            return Some(MenuCommand::ToggleStartAtLogin);
        }
        if event_id == &self.upgrade_external_id {
            return Some(MenuCommand::ToggleUpgradeExternal);
        }
        if event_id == &self.wait_for_apps_id {
            return Some(MenuCommand::OpenWaitForAppsWindow);
        }
        for (id, mode, _) in &self.mode_items {
            if event_id == id {
                return Some(MenuCommand::SetMode(*mode));
            }
        }
        for (id, secs, _) in &self.time_limit_items {
            if event_id == id {
                return Some(MenuCommand::SetTimeLimit(*secs));
            }
        }
        None
    }
}

pub fn build_menu(snapshot: &MenuSnapshot) -> (Menu, MenuHandles) {
    let menu = Menu::new();
    let mut mode_items = Vec::new();

    for mode in SleepMode::all() {
        // `muda` menu items are not `Copy`; clone is required to append the same
        // item to the menu after building the handle map.
        let item = CheckMenuItem::new(mode.label(), true, snapshot.mode == mode, None);
        let id = item.id().clone();
        mode_items.push((id, mode, item.clone()));
        menu.append(&item).expect("append mode item");
    }

    menu.append(&PredefinedMenuItem::separator())
        .expect("separator");

    let time_limit_submenu = Submenu::new("Time limit", true);
    let mut time_limit_items = Vec::new();
    for preset in TimeLimitPreset::ALL {
        let item = CheckMenuItem::new(
            preset.label,
            true,
            snapshot.time_limit_secs == preset.seconds,
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

    // A single entry opens the multi-select picker window; the count keeps the
    // current selection visible at a glance.
    let wait_label = match snapshot.wait_for_apps.len() {
        0 => "Wait for apps…".to_string(),
        n => format!("Wait for apps… ({n} selected)"),
    };
    let wait_for_apps_item = MenuItem::new(wait_label, true, None);
    let wait_for_apps_id = wait_for_apps_item.id().clone();
    menu.append(&wait_for_apps_item)
        .expect("append wait for apps item");

    menu.append(&PredefinedMenuItem::separator())
        .expect("separator");

    // Watches for low-level sleep assertions from other apps and upgrades them
    // to Entirely mode. muda items have no tooltip, so the label must stand on
    // its own.
    let upgrade_external = CheckMenuItem::new(
        "Upgrade other apps' sleep prevention",
        true,
        snapshot.upgrade_external,
        None,
    );
    let upgrade_external_id = upgrade_external.id().clone();
    menu.append(&upgrade_external)
        .expect("append upgrade external item");

    // While the watcher is actively upgrading, show which external process(es)
    // it is upgrading. One app is a single disabled line; several collapse into
    // an expandable submenu so the top-level menu stays compact.
    if let Some(apps) = snapshot.upgrading_apps.as_deref() {
        append_upgrading_status(&menu, apps);
    }

    // Sleep assertions the watcher saw but is not upgrading (e.g. powerd, or
    // display-only holds from video players), so a blank menu isn't mistaken for
    // "nothing is keeping the Mac awake."
    if !snapshot.ignored_assertions.is_empty() {
        append_ignored_status(&menu, &snapshot.ignored_assertions);
    }

    let start_at_login = CheckMenuItem::new("Start at login", true, snapshot.start_at_login, None);
    let start_at_login_id = start_at_login.id().clone();
    menu.append(&start_at_login).expect("append login item");

    menu.append(&PredefinedMenuItem::separator())
        .expect("separator");

    let quit = MenuItem::new("Quit", true, None);
    let quit_id = quit.id().clone();
    menu.append(&quit).expect("append quit");

    (
        menu,
        MenuHandles {
            mode_items,
            time_limit_items,
            wait_for_apps_id,
            upgrade_external,
            upgrade_external_id,
            start_at_login,
            start_at_login_id,
            quit_id,
        },
    )
}

/// Append the watcher's "Upgrading…" status to the menu. These items are purely
/// informational (disabled, no command), so they carry no `MenuHandles` entry;
/// the structure is rebuilt via `install_menu` whenever the set changes.
fn append_upgrading_status(menu: &Menu, apps: &[String]) {
    match apps {
        // Upgrading, but no holder exposed a name.
        [] => {
            menu.append(&MenuItem::new("Upgrading external app", false, None))
                .expect("append upgrading status");
        }
        [one] => {
            menu.append(&MenuItem::new(format!("Upgrading {one}"), false, None))
                .expect("append upgrading status");
        }
        many => {
            let submenu = Submenu::new(format!("Upgrading {} apps", many.len()), true);
            for name in many {
                submenu
                    .append(&MenuItem::new(name, false, None))
                    .expect("append upgrading app");
            }
            menu.append(&submenu).expect("append upgrading submenu");
        }
    }
}

/// Append the watcher's "Ignoring…" status: sleep assertions it saw but chose
/// not to upgrade, each with a short reason. Like the upgrading status, these
/// are disabled and carry no `MenuHandles` entry; the structure is rebuilt via
/// `install_menu` whenever the set changes. One holder is a single line; several
/// collapse into an expandable submenu so the top-level menu stays compact.
fn append_ignored_status(menu: &Menu, ignored: &[IgnoredAssertion]) {
    let line = |a: &IgnoredAssertion| format!("Ignoring {} ({})", a.process_name, a.reason);
    match ignored {
        [] => {}
        [one] => {
            menu.append(&MenuItem::new(line(one), false, None))
                .expect("append ignored status");
        }
        many => {
            let submenu = Submenu::new(format!("Ignoring {} assertions", many.len()), true);
            for assertion in many {
                submenu
                    .append(&MenuItem::new(line(assertion), false, None))
                    .expect("append ignored assertion");
            }
            menu.append(&submenu).expect("append ignored submenu");
        }
    }
}

/// Sync all checkbox items to the current menu snapshot.
pub fn sync_menu_to_snapshot(handles: &MenuHandles, snapshot: &MenuSnapshot) {
    for (_, mode, item) in &handles.mode_items {
        item.set_checked(snapshot.mode == *mode);
    }
    for (_, secs, item) in &handles.time_limit_items {
        item.set_checked(*secs == snapshot.time_limit_secs);
    }
    handles
        .upgrade_external
        .set_checked(snapshot.upgrade_external);
    handles.start_at_login.set_checked(snapshot.start_at_login);
}

pub fn install_menu(tray: &tray_icon::TrayIcon, snapshot: &MenuSnapshot) -> MenuHandles {
    let (menu, handles) = build_menu(snapshot);
    tray.set_menu(Some(Box::new(menu)));
    handles
}

pub enum MenuAction {
    Quit,
    Handled,
    Unhandled,
}

pub fn dispatch_command(
    command: &MenuCommand,
    handles: &MenuHandles,
    state: &mut AppState,
    tray: &tray_icon::TrayIcon,
) -> MenuAction {
    match *command {
        MenuCommand::Quit => return MenuAction::Quit,
        // The run loop owns the AppKit window code, so it handles this one.
        MenuCommand::OpenWaitForAppsWindow => return MenuAction::Unhandled,
        MenuCommand::ToggleStartAtLogin => {
            let new_val = !state.menu_snapshot().start_at_login;
            if let Err(e) = state.set_start_at_login(new_val) {
                eprintln!("{e}");
            }
        }
        MenuCommand::ToggleUpgradeExternal => {
            let enable = !state.menu_snapshot().upgrade_external;
            // Enabling installs the helper if needed (admin prompt + socket
            // wait); surface that in the tooltip before the blocking work.
            if enable {
                let _ = tray.set_tooltip(Some("caffeinate2 (enabling sleep upgrade…)"));
                state.invalidate_tooltip();
                crate::tray::macos_activation::pump_event_loop(Some(
                    std::time::Duration::from_millis(1),
                ));
            }
            match state.set_upgrade_external(enable) {
                Ok(()) => {
                    // Poll now so an already-present external assertion is
                    // upgraded immediately rather than after the next interval.
                    state.poll_upgrade();
                    state.set_icon(tray);
                }
                Err(e) => {
                    eprintln!("{e}");
                    state.set_icon(tray);
                    state.show_error_tooltip(tray, &e.to_string());
                }
            }
        }
        MenuCommand::SetMode(mode) => {
            // Switching an active session to Entirely can block on the helper
            // install prompt and socket wait; surface progress in the tooltip
            // before the blocking work starts. set_icon below restores it.
            if state.is_on() && mode == SleepMode::Entirely {
                let _ = tray.set_tooltip(Some("caffeinate2 (enabling Entirely mode…)"));
                state.invalidate_tooltip();
                crate::tray::macos_activation::pump_event_loop(Some(
                    std::time::Duration::from_millis(1),
                ));
            }
            match state.set_mode(mode) {
                Ok(()) => state.set_icon(tray),
                Err(e) => {
                    eprintln!("{e}");
                    state.set_icon(tray);
                    state.show_error_tooltip(tray, &e.to_string());
                }
            }
        }
        MenuCommand::SetTimeLimit(secs) => {
            if let Err(e) = state.set_time_limit(secs) {
                eprintln!("{e}");
            } else if state.is_on() {
                state.update_tooltip(tray);
            }
        }
    }

    // Always re-sync the checkboxes: muda auto-toggles the clicked item before
    // this handler runs, so a failed state change (e.g. a cancelled admin
    // prompt when enabling Entirely mode) would otherwise leave the menu
    // showing a state that was never applied.
    sync_menu_to_snapshot(handles, &state.menu_snapshot());
    MenuAction::Handled
}

pub fn handle_menu_event(
    event_id: &MenuId,
    handles: &MenuHandles,
    state: &mut AppState,
    tray: &tray_icon::TrayIcon,
) -> MenuAction {
    handles
        .resolve(event_id)
        .map_or(MenuAction::Unhandled, |command| {
            dispatch_command(&command, handles, state, tray)
        })
}
