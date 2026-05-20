#[cfg(all(target_os = "macos", feature = "tray"))]
use caffeinate2::install;
#[cfg(all(target_os = "macos", feature = "tray"))]
use caffeinate2::macos_apps::{self, RunningAppChoice};
#[cfg(all(target_os = "macos", feature = "tray"))]
use caffeinate2::tray_mode::{self, ActiveMode, TimeLimitPreset, TrayConfig, TrayMode, WaitForApp};
#[cfg(all(target_os = "macos", feature = "tray"))]
use caffeinate2::tray_icons;
#[cfg(all(target_os = "macos", feature = "tray"))]
use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
#[cfg(all(target_os = "macos", feature = "tray"))]
use std::sync::{Arc, Mutex};
#[cfg(all(target_os = "macos", feature = "tray"))]
use std::time::{Duration, Instant};
#[cfg(all(target_os = "macos", feature = "tray"))]
use tray_icon::menu::MenuId;
#[cfg(all(target_os = "macos", feature = "tray"))]
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[cfg(all(target_os = "macos", feature = "tray"))]
struct AppState {
    mode: TrayMode,
    time_limit_secs: Option<u64>,
    wait_for_app: Option<WaitForApp>,
    active: Option<ActiveMode>,
    active_until: Option<Instant>,
    app_saw_running: bool,
    last_tooltip: Option<String>,
    start_at_login: bool,
}

#[cfg(all(target_os = "macos", feature = "tray"))]
impl AppState {
    fn new() -> Self {
        let config = tray_mode::load_config();
        Self {
            mode: config.mode,
            time_limit_secs: config.time_limit_secs,
            wait_for_app: config.wait_for_app,
            active: None,
            active_until: None,
            app_saw_running: false,
            last_tooltip: None,
            start_at_login: install::tray_launch_agent_installed(),
        }
    }

    fn config(&self) -> TrayConfig {
        TrayConfig {
            mode: self.mode,
            time_limit_secs: self.time_limit_secs,
            wait_for_app: self.wait_for_app.clone(),
        }
    }

    fn save_config(&self) -> Result<(), String> {
        tray_mode::save_config(&self.config())
    }

    fn is_on(&self) -> bool {
        self.active.is_some()
    }

    fn waiting_for_app_launch(&self) -> bool {
        self.wait_for_app.as_ref().is_some_and(|app| {
            self.is_on() && !self.app_saw_running && !macos_apps::is_bundle_running(&app.bundle_id)
        })
    }

    fn set_icon(&mut self, tray: &TrayIcon) {
        let bytes = if self.is_on() {
            tray_icons::ICON_ON
        } else {
            tray_icons::ICON_OFF
        };
        if let Ok(icon) = Icon::from_rgba(image_load_rgba(bytes), 22, 22) {
            let _ = tray.set_icon(Some(icon));
        }
        self.update_tooltip(tray);
    }

    fn update_tooltip(&mut self, tray: &TrayIcon) {
        let tooltip = if self.is_on() {
            let remaining = self.active_until.map(|until| {
                until
                    .saturating_duration_since(Instant::now())
                    .as_secs()
            });
            tray_mode::format_active_tooltip(
                remaining,
                self.wait_for_app.as_ref(),
                self.waiting_for_app_launch(),
            )
        } else {
            "caffeinate2".to_string()
        };
        if self.last_tooltip.as_ref() != Some(&tooltip) {
            self.last_tooltip = Some(tooltip.clone());
            let _ = tray.set_tooltip(Some(tooltip));
        }
    }

    fn clear_active(&mut self) {
        self.active = None;
        self.active_until = None;
        self.app_saw_running = false;
        self.last_tooltip = None;
    }

    fn check_timeout(&mut self) -> bool {
        if self.active.is_some()
            && self
                .active_until
                .is_some_and(|until| Instant::now() >= until)
        {
            self.clear_active();
            return true;
        }
        false
    }

    fn check_app_watch(&mut self) -> bool {
        let Some(app) = self.wait_for_app.as_ref() else {
            return false;
        };
        if self.active.is_none() {
            return false;
        }

        if macos_apps::is_bundle_running(&app.bundle_id) {
            self.app_saw_running = true;
            return false;
        }

        if self.app_saw_running {
            self.clear_active();
            return true;
        }

        false
    }

    fn toggle(&mut self) -> Result<(), String> {
        if self.active.is_some() {
            self.clear_active();
            return Ok(());
        }
        self.enable()?;
        Ok(())
    }

    fn enable(&mut self) -> Result<(), String> {
        if self.mode == TrayMode::Entirely
            && !caffeinate2::helper_ipc::HelperClient::new().is_available()
        {
            install::install_helper_privileged()?;
            if !caffeinate2::helper_ipc::HelperClient::new().is_available() {
                return Err(
                    "helper is not running after install; try: sudo caffeinate2 install-helper"
                        .to_string(),
                );
            }
        }
        self.active = Some(self.mode.enable().map_err(|_| {
            "failed to enable sleep prevention (entirely mode requires the helper)".to_string()
        })?);
        self.active_until = self
            .time_limit_secs
            .map(|secs| Instant::now() + Duration::from_secs(secs));
        self.app_saw_running = self
            .wait_for_app
            .as_ref()
            .is_some_and(|app| macos_apps::is_bundle_running(&app.bundle_id));
        self.last_tooltip = None;
        Ok(())
    }

    fn set_mode(&mut self, mode: TrayMode) -> Result<(), String> {
        let was_on = self.active.is_some();
        self.clear_active();
        self.mode = mode;
        self.save_config()?;
        if was_on {
            self.enable()?;
        }
        Ok(())
    }

    fn set_time_limit(&mut self, time_limit_secs: Option<u64>) -> Result<(), String> {
        self.time_limit_secs = time_limit_secs;
        if self.is_on() {
            self.active_until = time_limit_secs.map(|secs| Instant::now() + Duration::from_secs(secs));
            self.last_tooltip = None;
        }
        self.save_config()?;
        Ok(())
    }

    fn set_wait_for_app(&mut self, wait_for_app: Option<WaitForApp>) -> Result<(), String> {
        self.wait_for_app = wait_for_app;
        if self.is_on() {
            self.app_saw_running = self.wait_for_app.as_ref().is_some_and(|app| {
                macos_apps::is_bundle_running(&app.bundle_id)
            });
            self.last_tooltip = None;
        }
        self.save_config()?;
        Ok(())
    }

    fn set_start_at_login(&mut self, enabled: bool) -> Result<(), String> {
        let tray_path = std::env::current_exe().map_err(|e| e.to_string())?;
        if enabled {
            install::install_tray_launch_agent(&tray_path)?;
        } else {
            install::uninstall_tray_launch_agent()?;
        }
        self.start_at_login = enabled;
        Ok(())
    }

    fn snapshot_for_menu(&self) -> Self {
        Self {
            mode: self.mode,
            time_limit_secs: self.time_limit_secs,
            wait_for_app: self.wait_for_app.clone(),
            active: None,
            active_until: None,
            app_saw_running: false,
            last_tooltip: None,
            start_at_login: self.start_at_login,
        }
    }
}

#[cfg(all(target_os = "macos", feature = "tray"))]
fn image_load_rgba(png_bytes: &[u8]) -> Vec<u8> {
    let (rgba, w, h) = decode_png_rgba(png_bytes).unwrap_or((vec![0; 22 * 22 * 4], 22, 22));
    let _ = (w, h);
    rgba
}

#[cfg(all(target_os = "macos", feature = "tray"))]
fn decode_png_rgba(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    if bytes.len() < 24 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if w != 22 || h != 22 {
        return None;
    }
    let fill = if bytes.contains(&220) { 220u8 } else { 80u8 };
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        rgba.extend([fill, fill, fill, 255]);
    }
    Some((rgba, w, h))
}

#[cfg(all(target_os = "macos", feature = "tray"))]
struct MenuHandles {
    mode_items: Vec<(MenuId, TrayMode, CheckMenuItem)>,
    time_limit_items: Vec<(MenuId, Option<u64>, CheckMenuItem)>,
    until_app_off: CheckMenuItem,
    until_app_off_id: MenuId,
    until_app_items: Vec<(MenuId, String, String, CheckMenuItem)>,
    choose_app_id: MenuId,
    start_at_login: CheckMenuItem,
    start_at_login_id: MenuId,
    quit_id: MenuId,
}

#[cfg(all(target_os = "macos", feature = "tray"))]
fn running_apps_menu_key(apps: &[RunningAppChoice]) -> Vec<(String, String)> {
    apps.iter()
        .map(|a| (a.bundle_id.clone(), a.name.clone()))
        .collect()
}

#[cfg(all(target_os = "macos", feature = "tray"))]
fn build_menu(state: &AppState, running_apps: &[RunningAppChoice]) -> (Menu, MenuHandles) {
    let menu = Menu::new();
    let mut mode_items = Vec::new();

    for mode in TrayMode::all() {
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
            until_app_items.push((id, app.bundle_id.clone(), app.name.clone(), item.clone()));
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

#[cfg(all(target_os = "macos", feature = "tray"))]
fn refresh_mode_checks(handles: &MenuHandles, selected: TrayMode) {
    for (_, mode, item) in &handles.mode_items {
        let _ = item.set_checked(*mode == selected);
    }
}

#[cfg(all(target_os = "macos", feature = "tray"))]
fn refresh_time_limit_checks(handles: &MenuHandles, selected: Option<u64>) {
    for (_, secs, item) in &handles.time_limit_items {
        let _ = item.set_checked(*secs == selected);
    }
}

#[cfg(all(target_os = "macos", feature = "tray"))]
fn set_until_app_checks(handles: &MenuHandles, selected: Option<&WaitForApp>) {
    let _ = handles.until_app_off.set_checked(selected.is_none());
    for (_, bundle_id, _, item) in &handles.until_app_items {
        let _ = item.set_checked(selected.is_some_and(|app| app.bundle_id == *bundle_id));
    }
}

#[cfg(all(target_os = "macos", feature = "tray"))]
fn install_menu(
    tray: &TrayIcon,
    state: &AppState,
    running_apps: &[RunningAppChoice],
) -> MenuHandles {
    let (menu, handles) = build_menu(state, running_apps);
    let _ = tray.set_menu(Some(Box::new(menu)));
    handles
}

#[cfg(all(target_os = "macos", feature = "tray"))]
fn main() {
    if let Err(e) = run() {
        eprintln!("caffeinate2-tray error: {e}");
        std::process::exit(1);
    }
}

#[cfg(all(target_os = "macos", feature = "tray"))]
fn run() -> Result<(), String> {
    caffeinate2::macos_activation::hide_dock_icon();

    let icon_off = Icon::from_rgba(image_load_rgba(tray_icons::ICON_OFF), 22, 22)
        .map_err(|e| e.to_string())?;

    let state = Arc::new(Mutex::new(AppState::new()));
    let running_apps = macos_apps::running_app_choices();
    let initial = state.lock().expect("state lock").snapshot_for_menu();
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
            if s.check_timeout() || s.check_app_watch() {
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
                let snapshot = state.lock().expect("state lock").snapshot_for_menu();
                let new_handles = install_menu(&tray, &snapshot, &apps);
                *handles.lock().expect("handles lock") = new_handles;
            }
        }

        if let Ok(event) = MenuEvent::receiver().try_recv() {
            let h = handles.lock().expect("handles lock");

            if event.id == h.quit_id {
                break;
            }
            if event.id == h.start_at_login_id {
                let mut s = state.lock().expect("state lock");
                let new_val = !s.start_at_login;
                if let Err(e) = s.set_start_at_login(new_val) {
                    eprintln!("{e}");
                } else {
                    let _ = h.start_at_login.set_checked(new_val);
                }
                continue;
            }
            if event.id == h.until_app_off_id {
                let mut s = state.lock().expect("state lock");
                if let Err(e) = s.set_wait_for_app(None) {
                    eprintln!("{e}");
                } else {
                    set_until_app_checks(&h, None);
                }
                continue;
            }
            if event.id == h.choose_app_id {
                drop(h);
                if let Some(choice) = macos_apps::choose_app_bundle() {
                    let app = WaitForApp {
                        bundle_id: choice.bundle_id,
                        name: choice.name,
                    };
                    let mut s = state.lock().expect("state lock");
                    if let Err(e) = s.set_wait_for_app(Some(app.clone())) {
                        eprintln!("{e}");
                    } else if s.is_on() {
                        s.last_tooltip = None;
                        s.update_tooltip(&tray);
                    }
                }
                let apps = macos_apps::running_app_choices();
                menu_apps_key = running_apps_menu_key(&apps);
                let snapshot = state.lock().expect("state lock").snapshot_for_menu();
                *handles.lock().expect("handles lock") = install_menu(&tray, &snapshot, &apps);
                let selected = state.lock().expect("state lock").wait_for_app.clone();
                let h = handles.lock().expect("handles lock");
                set_until_app_checks(&h, selected.as_ref());
                continue;
            }
            let mut handled = false;
            for (id, bundle_id, name, _) in &h.until_app_items {
                if event.id == *id {
                    let app = WaitForApp {
                        bundle_id: bundle_id.clone(),
                        name: name.clone(),
                    };
                    let mut s = state.lock().expect("state lock");
                    if let Err(e) = s.set_wait_for_app(Some(app.clone())) {
                        eprintln!("{e}");
                    } else {
                        set_until_app_checks(&h, Some(&app));
                        if s.is_on() {
                            s.last_tooltip = None;
                            s.update_tooltip(&tray);
                        }
                    }
                    handled = true;
                    break;
                }
            }
            if handled {
                continue;
            }
            for (id, mode, _) in &h.mode_items {
                if event.id == *id {
                    let mut s = state.lock().expect("state lock");
                    if let Err(e) = s.set_mode(*mode) {
                        eprintln!("{e}");
                    } else {
                        refresh_mode_checks(&h, *mode);
                    }
                    s.set_icon(&tray);
                    break;
                }
            }
            for (id, secs, _) in &h.time_limit_items {
                if event.id == *id {
                    let mut s = state.lock().expect("state lock");
                    if let Err(e) = s.set_time_limit(*secs) {
                        eprintln!("{e}");
                    } else {
                        refresh_time_limit_checks(&h, *secs);
                        if s.is_on() {
                            s.update_tooltip(&tray);
                        }
                    }
                    break;
                }
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

#[cfg(not(all(target_os = "macos", feature = "tray")))]
fn main() {
    eprintln!("caffeinate2-tray requires macOS and the tray feature.");
    std::process::exit(1);
}
