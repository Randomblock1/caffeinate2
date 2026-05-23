use crate::app_target::AppTarget;
use crate::install;
use crate::macos_apps;
use crate::sleep_mode::{ActiveSleepHold, EnableError, SleepMode, TRAY_ENTIRELY_POLICY};
use crate::tray_mode::{self, TrayConfig};
use crate::tray_icons;
use std::time::{Duration, Instant};
use tray_icon::{Icon, TrayIcon};

/// Persisted tray preferences (saved to disk).
#[derive(Debug, Clone)]
struct TraySettings {
    mode: SleepMode,
    time_limit_secs: Option<u64>,
    wait_for_app: Option<AppTarget>,
}

/// Runtime state while sleep prevention is active.
struct ActiveTraySession {
    #[allow(dead_code)]
    hold: ActiveSleepHold,
    until: Option<Instant>,
    app_saw_running: bool,
}

/// Menu bar settings used to build the tray menu (no runtime session state).
#[derive(Debug, Clone)]
pub struct MenuSnapshot {
    pub mode: SleepMode,
    pub time_limit_secs: Option<u64>,
    pub wait_for_app: Option<AppTarget>,
    pub start_at_login: bool,
}

pub struct AppState {
    settings: TraySettings,
    session: Option<ActiveTraySession>,
    last_tooltip: Option<String>,
    start_at_login: bool,
}

impl AppState {
    pub fn new() -> Self {
        let config = tray_mode::load_config();
        Self {
            settings: TraySettings {
                mode: config.mode,
                time_limit_secs: config.time_limit_secs,
                wait_for_app: config.wait_for_app,
            },
            session: None,
            last_tooltip: None,
            start_at_login: install::tray_launch_agent_installed(),
        }
    }

    pub fn menu_snapshot(&self) -> MenuSnapshot {
        MenuSnapshot {
            mode: self.settings.mode,
            time_limit_secs: self.settings.time_limit_secs,
            wait_for_app: self.settings.wait_for_app.clone(),
            start_at_login: self.start_at_login,
        }
    }

    fn config(&self) -> TrayConfig {
        TrayConfig {
            version: tray_mode::CONFIG_VERSION,
            mode: self.settings.mode,
            time_limit_secs: self.settings.time_limit_secs,
            wait_for_app: self.settings.wait_for_app.clone(),
        }
    }

    fn save_config(&self) -> Result<(), String> {
        tray_mode::save_config(&self.config())
    }

    pub fn is_on(&self) -> bool {
        self.session.is_some()
    }

    pub fn waiting_for_app_launch(&self) -> bool {
        self.settings.wait_for_app.as_ref().is_some_and(|app| {
            self.is_on()
                && !self
                    .session
                    .as_ref()
                    .is_some_and(|s| s.app_saw_running)
                && !macos_apps::is_bundle_running(&app.bundle_id)
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
            let remaining = self.session.as_ref().and_then(|session| {
                session.until.map(|until| {
                    until
                        .saturating_duration_since(Instant::now())
                        .as_secs()
                })
            });
            tray_mode::format_active_tooltip(
                remaining,
                self.settings.wait_for_app.as_ref(),
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

    pub fn invalidate_tooltip(&mut self) {
        self.last_tooltip = None;
    }

    pub fn stop_session(&mut self) {
        self.session = None;
        self.last_tooltip = None;
    }

    pub fn check_timeout(&mut self) -> bool {
        if self.session.as_ref().is_some_and(|session| {
            session
                .until
                .is_some_and(|until| Instant::now() >= until)
        }) {
            self.stop_session();
            return true;
        }
        false
    }

    pub fn check_app_watch(&mut self) -> bool {
        let Some(app) = self.settings.wait_for_app.as_ref() else {
            return false;
        };
        let Some(session) = self.session.as_mut() else {
            return false;
        };

        if macos_apps::is_bundle_running(&app.bundle_id) {
            session.app_saw_running = true;
            return false;
        }

        if session.app_saw_running {
            self.stop_session();
            return true;
        }

        false
    }

    pub fn toggle(&mut self) -> Result<(), EnableError> {
        if self.session.is_some() {
            self.stop_session();
            return Ok(());
        }
        self.start_session()
    }

    fn acquire_hold(mode: SleepMode) -> Result<ActiveSleepHold, EnableError> {
        match mode.enable(false, TRAY_ENTIRELY_POLICY) {
            Err(EnableError::HelperUnavailable) if mode == SleepMode::Entirely => {
                install::install_helper_privileged().map_err(EnableError::Ipc)?;
                mode.enable(false, TRAY_ENTIRELY_POLICY).map_err(|error| match error {
                    EnableError::HelperUnavailable => EnableError::Ipc(
                        "helper is not running after install; try: sudo caffeinate2 install-helper"
                            .to_string(),
                    ),
                    other => other,
                })
            }
            other => other,
        }
    }

    pub fn start_session(&mut self) -> Result<(), EnableError> {
        let hold = Self::acquire_hold(self.settings.mode)?;
        self.session = Some(ActiveTraySession {
            hold,
            until: self
                .settings
                .time_limit_secs
                .map(|secs| Instant::now() + Duration::from_secs(secs)),
            app_saw_running: self
                .settings
                .wait_for_app
                .as_ref()
                .is_some_and(|app| macos_apps::is_bundle_running(&app.bundle_id)),
        });
        self.last_tooltip = None;
        Ok(())
    }

    pub fn set_mode(&mut self, mode: SleepMode) -> Result<(), EnableError> {
        if mode == self.settings.mode {
            return Ok(());
        }
        let was_on = self.is_on();
        let previous_mode = self.settings.mode;
        if was_on {
            self.stop_session();
            self.settings.mode = mode;
            match self.start_session() {
                Ok(()) => {
                    self.save_config().map_err(EnableError::Ipc)?;
                    Ok(())
                }
                Err(error) => {
                    self.settings.mode = previous_mode;
                    let _ = self.start_session();
                    Err(error)
                }
            }
        } else {
            self.settings.mode = mode;
            self.save_config().map_err(EnableError::Ipc)?;
            Ok(())
        }
    }

    pub fn set_time_limit(&mut self, time_limit_secs: Option<u64>) -> Result<(), String> {
        self.settings.time_limit_secs = time_limit_secs;
        if let Some(session) = self.session.as_mut() {
            session.until =
                time_limit_secs.map(|secs| Instant::now() + Duration::from_secs(secs));
            self.last_tooltip = None;
        }
        self.save_config()
    }

    pub fn set_wait_for_app(&mut self, wait_for_app: Option<AppTarget>) -> Result<(), String> {
        self.settings.wait_for_app = wait_for_app;
        if let Some(session) = self.session.as_mut() {
            session.app_saw_running = self
                .settings
                .wait_for_app
                .as_ref()
                .is_some_and(|app| macos_apps::is_bundle_running(&app.bundle_id));
            self.last_tooltip = None;
        }
        self.save_config()
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
