#[cfg(all(target_os = "macos", feature = "tray"))]
use caffeinate2::install;
#[cfg(all(target_os = "macos", feature = "tray"))]
use caffeinate2::tray_mode::{self, ActiveMode, TrayMode};
#[cfg(all(target_os = "macos", feature = "tray"))]
use caffeinate2::tray_icons;
#[cfg(all(target_os = "macos", feature = "tray"))]
use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
#[cfg(all(target_os = "macos", feature = "tray"))]
use std::sync::{Arc, Mutex};
#[cfg(all(target_os = "macos", feature = "tray"))]
use tray_icon::menu::MenuId;
#[cfg(all(target_os = "macos", feature = "tray"))]
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

#[cfg(all(target_os = "macos", feature = "tray"))]
struct AppState {
    mode: TrayMode,
    active: Option<ActiveMode>,
    start_at_login: bool,
}

#[cfg(all(target_os = "macos", feature = "tray"))]
impl AppState {
    fn new() -> Self {
        Self {
            mode: tray_mode::load_config(),
            active: None,
            start_at_login: install::tray_launch_agent_installed(),
        }
    }

    fn is_on(&self) -> bool {
        self.active.is_some()
    }

    fn set_icon(&self, tray: &tray_icon::TrayIcon) {
        let bytes = if self.is_on() {
            tray_icons::ICON_ON
        } else {
            tray_icons::ICON_OFF
        };
        if let Ok(icon) = Icon::from_rgba(
            image_load_rgba(bytes),
            22,
            22,
        ) {
            let _ = tray.set_icon(Some(icon));
        }
    }

    fn toggle(&mut self) -> Result<(), String> {
        if self.active.is_some() {
            self.active = None;
            return Ok(());
        }
        self.enable()?;
        Ok(())
    }

    fn enable(&mut self) -> Result<(), String> {
        if self.mode == TrayMode::Entirely && !caffeinate2::helper_ipc::HelperClient::new().is_available()
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
        Ok(())
    }

    fn set_mode(&mut self, mode: TrayMode) -> Result<(), String> {
        let was_on = self.active.is_some();
        self.active = None;
        self.mode = mode;
        let _ = tray_mode::save_config(mode);
        if was_on {
            self.enable()?;
        }
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
}

#[cfg(all(target_os = "macos", feature = "tray"))]
fn image_load_rgba(png_bytes: &[u8]) -> Vec<u8> {
    // Minimal decode: our icons are 22x22 RGB PNGs from resources.
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
    // Icons are fixed 22x22 RGB - use off/on solid fills as fallback
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
    start_at_login: CheckMenuItem,
    start_at_login_id: MenuId,
    quit_id: MenuId,
}

#[cfg(all(target_os = "macos", feature = "tray"))]
fn build_menu(state: &AppState) -> (Menu, MenuHandles) {
    let menu = Menu::new();
    let mut mode_items = Vec::new();

    for mode in TrayMode::all() {
        let item = CheckMenuItem::new(mode.label(), true, state.mode == mode, None);
        let id = item.id().clone();
        mode_items.push((id, mode, item.clone()));
        menu.append(&item).expect("append mode item");
    }

    menu.append(&PredefinedMenuItem::separator()).expect("separator");

    let start_at_login =
        CheckMenuItem::new("Start at login", true, state.start_at_login, None);
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
    let initial = state.lock().expect("state lock").clone_for_menu();
    let (menu, handles) = build_menu(&initial);

    let tray = TrayIconBuilder::new()
        .with_icon(icon_off)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("caffeinate2")
        .build()
        .map_err(|e| e.to_string())?;

    loop {
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == handles.quit_id {
                break;
            }
            if event.id == handles.start_at_login_id {
                let mut s = state.lock().expect("state lock");
                let new_val = !s.start_at_login;
                if let Err(e) = s.set_start_at_login(new_val) {
                    eprintln!("{e}");
                } else {
                    let _ = handles.start_at_login.set_checked(new_val);
                }
                continue;
            }
            for (id, mode, _) in &handles.mode_items {
                if event.id == *id {
                    let mut s = state.lock().expect("state lock");
                    if let Err(e) = s.set_mode(*mode) {
                        eprintln!("{e}");
                    } else {
                        refresh_mode_checks(&handles, *mode);
                    }
                    s.set_icon(&tray);
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

        std::thread::sleep(std::time::Duration::from_millis(16));
    }

    let mut s = state.lock().expect("state lock");
    s.active = None;
    Ok(())
}

#[cfg(all(target_os = "macos", feature = "tray"))]
impl AppState {
    fn clone_for_menu(&self) -> Self {
        Self {
            mode: self.mode,
            active: None,
            start_at_login: self.start_at_login,
        }
    }
}

#[cfg(not(all(target_os = "macos", feature = "tray")))]
fn main() {
    eprintln!("caffeinate2-tray requires macOS and the tray feature.");
    std::process::exit(1);
}
