use crate::app_target::AppTarget;
use crate::helper_ipc::HelperClient;
use crate::install;
use crate::macos_apps;
use crate::sleep_mode::{ActiveSleepHold, EntirelyPolicy, SleepMode};
use crate::tray_mode::{self, TrayConfig};
use crate::tray_icons;
use std::time::{Duration, Instant};
use tray_icon::{Icon, TrayIcon};

/// Menu bar settings used to build the tray menu (no runtime session state).
#[derive(Debug, Clone)]
pub struct MenuSnapshot {
    pub mode: SleepMode,
    pub time_limit_secs: Option<u64>,
    pub wait_for_app: Option<AppTarget>,
    pub start_at_login: bool,
}

pub struct AppState {
    pub mode: SleepMode,
    pub time_limit_secs: Option<u64>,
    pub wait_for_app: Option<AppTarget>,
    pub active: Option<ActiveSleepHold>,
    pub active_until: Option<Instant>,
    pub app_saw_running: bool,
    pub last_tooltip: Option<String>,
    pub start_at_login: bool,
}

impl AppState {
    pub fn new() -> Self {
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

    pub fn menu_snapshot(&self) -> MenuSnapshot {
        MenuSnapshot {
            mode: self.mode,
            time_limit_secs: self.time_limit_secs,
            wait_for_app: self.wait_for_app.clone(),
            start_at_login: self.start_at_login,
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

    pub fn is_on(&self) -> bool {
        self.active.is_some()
    }

    pub fn waiting_for_app_launch(&self) -> bool {
        self.wait_for_app.as_ref().is_some_and(|app| {
            self.is_on() && !self.app_saw_running && !macos_apps::is_bundle_running(&app.bundle_id)
        })
    }

    pub fn set_icon(&mut self, tray: &TrayIcon) {
        let bytes = if self.is_on() {
            tray_icons::ICON_ON
        } else {
            tray_icons::ICON_OFF
        };
        if let Ok(rgba) = tray_icons::decode_icon_rgba(bytes) {
            if let Ok(icon) = Icon::from_rgba(rgba, tray_icons::ICON_SIZE, tray_icons::ICON_SIZE) {
                let _ = tray.set_icon(Some(icon));
            }
        }
        self.update_tooltip(tray);
    }

    pub fn update_tooltip(&mut self, tray: &TrayIcon) {
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

    pub fn clear_active(&mut self) {
        self.active = None;
        self.active_until = None;
        self.app_saw_running = false;
        self.last_tooltip = None;
    }

    pub fn check_timeout(&mut self) -> bool {
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

    pub fn check_app_watch(&mut self) -> bool {
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

    pub fn toggle(&mut self) -> Result<(), String> {
        if self.active.is_some() {
            self.clear_active();
            return Ok(());
        }
        self.enable()?;
        Ok(())
    }

    fn ensure_helper_for_entirely(&self) -> Result<(), String> {
        if self.mode != SleepMode::Entirely {
            return Ok(());
        }
        let client = HelperClient::new();
        if client.is_available() {
            return Ok(());
        }
        install::install_helper_privileged()?;
        if !client.is_available() {
            return Err(
                "helper is not running after install; try: sudo caffeinate2 install-helper"
                    .to_string(),
            );
        }
        Ok(())
    }

    pub fn enable(&mut self) -> Result<(), String> {
        self.ensure_helper_for_entirely()?;
        self.active = Some(
            self.mode
                .enable(false, EntirelyPolicy::HelperRequired)
                .map_err(|e| e.to_string())?,
        );
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

    pub fn set_mode(&mut self, mode: SleepMode) -> Result<(), String> {
        let was_on = self.active.is_some();
        self.clear_active();
        self.mode = mode;
        self.save_config()?;
        if was_on {
            self.enable()?;
        }
        Ok(())
    }

    pub fn set_time_limit(&mut self, time_limit_secs: Option<u64>) -> Result<(), String> {
        self.time_limit_secs = time_limit_secs;
        if self.is_on() {
            self.active_until =
                time_limit_secs.map(|secs| Instant::now() + Duration::from_secs(secs));
            self.last_tooltip = None;
        }
        self.save_config()?;
        Ok(())
    }

    pub fn set_wait_for_app(&mut self, wait_for_app: Option<AppTarget>) -> Result<(), String> {
        self.wait_for_app = wait_for_app;
        if self.is_on() {
            self.app_saw_running = self
                .wait_for_app
                .as_ref()
                .is_some_and(|app| macos_apps::is_bundle_running(&app.bundle_id));
            self.last_tooltip = None;
        }
        self.save_config()?;
        Ok(())
    }

    pub fn set_start_at_login(&mut self, enabled: bool) -> Result<(), String> {
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
