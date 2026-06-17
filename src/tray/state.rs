use crate::app_target::AppTarget;
use crate::install;
use crate::macos_apps;
use crate::sleep_mode::{ActiveSleepHold, EnableError, SleepMode};
use crate::tray_icons;
use crate::tray_mode::{self, TrayConfig};
use std::time::{Duration, Instant};
use tray_icon::{Icon, TrayIcon};

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
    config: TrayConfig,
    session: Option<ActiveTraySession>,
    last_tooltip: Option<String>,
    start_at_login: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            config: tray_mode::load_config(),
            session: None,
            last_tooltip: None,
            start_at_login: install::tray_launch_agent_installed(),
        }
    }

    pub fn menu_snapshot(&self) -> MenuSnapshot {
        MenuSnapshot {
            mode: self.config.mode,
            time_limit_secs: self.config.time_limit_secs,
            wait_for_app: self.config.wait_for_app.clone(),
            start_at_login: self.start_at_login,
        }
    }

    fn save_config(&self) -> Result<(), String> {
        tray_mode::save_config(&self.config)
    }

    pub fn is_on(&self) -> bool {
        self.session.is_some()
    }

    /// True while a timed session is active (tooltip countdown needs 1s ticks).
    pub fn needs_tick(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.until.is_some())
    }

    /// How long to wait before the next loop iteration, or `None` to block
    /// indefinitely until an event arrives.
    pub fn pump_timeout(&self) -> Option<Duration> {
        if !self.needs_tick() {
            return None;
        }
        let until = self.session.as_ref()?.until?;
        let remaining = until.saturating_duration_since(Instant::now());
        Some(remaining.min(Duration::from_secs(1)))
    }

    pub fn waiting_for_app_launch(&self) -> bool {
        self.config.wait_for_app.as_ref().is_some_and(|app| {
            self.is_on()
                && !self.session.as_ref().is_some_and(|s| s.app_saw_running)
                && !macos_apps::is_bundle_running(&app.bundle_id)
        })
    }

    /// Set the tray image for the given on/off state without touching the
    /// session or tooltip. Used to flip the icon optimistically before a
    /// potentially slow toggle (e.g. the helper RPC in Entirely mode).
    pub fn show_icon_state(tray: &TrayIcon, on: bool) {
        let bytes = if on {
            tray_icons::ICON_ON
        } else {
            tray_icons::ICON_OFF
        };
        let icon = tray_icons::decode_icon_rgba(bytes).and_then(|(rgba, width, height)| {
            Icon::from_rgba(rgba, width, height).map_err(|e| e.to_string())
        });
        match icon {
            Ok(icon) => {
                let _ = tray.set_icon_with_as_template(Some(icon), true);
            }
            Err(e) => eprintln!("failed to decode tray icon: {e}"),
        }
    }

    pub fn set_icon(&mut self, tray: &TrayIcon) {
        Self::show_icon_state(tray, self.is_on());
        self.update_tooltip(tray);
    }

    pub fn update_tooltip(&mut self, tray: &TrayIcon) {
        let tooltip = if self.is_on() {
            let remaining = self.session.as_ref().and_then(|session| {
                session
                    .until
                    .map(|until| until.saturating_duration_since(Instant::now()).as_secs())
            });
            tray_mode::format_active_tooltip(
                remaining,
                self.config.wait_for_app.as_ref(),
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

    /// Show an error in the tooltip (e.g. a denied entirely-mode hold).
    /// Call after `set_icon` so the icon reflects the real state; the text
    /// persists while idle and is replaced on the next tooltip update.
    pub fn show_error_tooltip(&mut self, tray: &TrayIcon, message: &str) {
        let _ = tray.set_tooltip(Some(format!("caffeinate2 — {message}")));
        self.last_tooltip = None;
    }

    pub fn stop_session(&mut self) {
        self.session = None;
        self.last_tooltip = None;
    }

    pub fn check_timeout(&mut self) -> bool {
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.until.is_some_and(|until| Instant::now() >= until))
        {
            self.stop_session();
            return true;
        }
        false
    }

    pub fn check_app_watch(&mut self) -> bool {
        let Some(app) = self.config.wait_for_app.as_ref() else {
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

    pub fn start_session(&mut self) -> Result<(), EnableError> {
        let hold = self.config.mode.enable_for_tray()?;
        self.session = Some(ActiveTraySession {
            hold,
            until: self
                .config
                .time_limit_secs
                .map(|secs| Instant::now() + Duration::from_secs(secs)),
            app_saw_running: self
                .config
                .wait_for_app
                .as_ref()
                .is_some_and(|app| macos_apps::is_bundle_running(&app.bundle_id)),
        });
        self.last_tooltip = None;
        Ok(())
    }

    pub fn set_mode(&mut self, mode: SleepMode) -> Result<(), EnableError> {
        if mode == self.config.mode {
            return Ok(());
        }
        let previous_mode = self.config.mode;
        if self.is_on() {
            self.stop_session();
            self.config.mode = mode;
            if let Err(error) = self.start_session() {
                self.config.mode = previous_mode;
                let _ = self.start_session();
                return Err(error);
            }
        } else {
            self.config.mode = mode;
        }
        self.save_config().map_err(EnableError::Ipc)
    }

    /// Set (or clear) the time limit. Deliberately restarts the countdown
    /// from now when a session is active, rather than rebasing on the
    /// session start (documented in the README).
    pub fn set_time_limit(&mut self, time_limit_secs: Option<u64>) -> Result<(), String> {
        self.config.time_limit_secs = time_limit_secs;
        if let Some(session) = self.session.as_mut() {
            session.until = time_limit_secs.map(|secs| Instant::now() + Duration::from_secs(secs));
            self.last_tooltip = None;
        }
        self.save_config()
    }

    pub fn set_wait_for_app(&mut self, wait_for_app: Option<AppTarget>) -> Result<(), String> {
        self.config.wait_for_app = wait_for_app;
        if let Some(session) = self.session.as_mut() {
            session.app_saw_running = self
                .config
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
