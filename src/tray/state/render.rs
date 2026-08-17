//! Everything that paints the tray: icon flips, the tooltip, and the menu-bar
//! countdown title. This is the only part of [`AppState`] that touches the
//! live [`tray_icon::TrayIcon`]; the rest of it is pure state that the run
//! loop renders through these methods.

use super::{AppState, SleepMode, sleep_aware_now};
use crate::tray::error::TrayError;
use crate::tray::tray_icons;
use crate::tray::tray_mode;
use std::time::{Duration, Instant};
use tray_icon::{Icon, TrayIcon};

impl AppState {
    /// The off-state icon for the tray builder, from the startup cache
    /// (falling back to a fresh decode when caching failed), so startup
    /// decodes each embedded PNG exactly once.
    pub fn initial_icon(&self) -> Result<Icon, TrayError> {
        let (rgba, width, height) = match self.icon_off_rgba.clone() {
            Some(cached) => cached,
            None => tray_icons::decode_icon_rgba(tray_icons::ICON_OFF)?,
        };
        Icon::from_rgba(rgba, width, height).map_err(|e| TrayError::BuildIcon(e.to_string()))
    }

    /// Set the tray image for the given on/off state without touching the
    /// session or tooltip. Used to flip the icon optimistically before a
    /// potentially slow toggle (e.g. the helper RPC in Entirely mode). Reuses
    /// the icons decoded at startup so a flip never re-decodes the PNG.
    pub fn show_icon_state(&self, tray: &TrayIcon, on: bool) {
        let cached = if on {
            self.icon_on_rgba.as_ref()
        } else {
            self.icon_off_rgba.as_ref()
        };
        let icon = match cached {
            Some((rgba, width, height)) => {
                Icon::from_rgba(rgba.clone(), *width, *height).map_err(|e| e.to_string())
            }
            None => {
                // Startup decode failed; fall back to decoding on demand.
                let bytes = if on {
                    tray_icons::ICON_ON
                } else {
                    tray_icons::ICON_OFF
                };
                tray_icons::decode_icon_rgba(bytes)
                    .map_err(|e| e.to_string())
                    .and_then(|(rgba, width, height)| {
                        Icon::from_rgba(rgba, width, height).map_err(|e| e.to_string())
                    })
            }
        };
        match icon {
            Ok(icon) => {
                let _ = tray.set_icon_with_as_template(Some(icon), true);
            }
            Err(e) => eprintln!("failed to decode tray icon: {e}"),
        }
    }

    pub fn set_icon(&mut self, tray: &TrayIcon) {
        // An in-flight enable shows the target on-state while it acquires.
        self.show_icon_state(tray, self.is_on() || self.is_enabling());
        self.update_tooltip(tray);
    }

    /// Whole seconds left on the active timed session, or `None` when there is
    /// no session or the session carries no time limit (upgrade sessions and
    /// untimed manual holds). Recomputed from `until` on demand — there is no
    /// stored countdown to drift.
    fn remaining_secs(&self) -> Option<u64> {
        self.session.as_ref().and_then(|session| {
            session
                .until
                .map(|until| until.saturating_sub(sleep_aware_now()).as_secs())
        })
    }

    /// The menu bar title shown next to the icon: the minutes remaining while a
    /// timed session runs (see `format_countdown_minutes` for why not seconds),
    /// `None` otherwise (idle, enabling, or a session with no time limit) so
    /// the icon stands alone. Zero is treated as "no title": the session is
    /// torn down by `check_timeout` on the same tick it hits 0, so there is
    /// nothing left to count down to.
    fn menu_bar_title(&self) -> Option<String> {
        self.remaining_secs()
            .filter(|&secs| secs > 0)
            .map(crate::util::duration_parser::format_countdown_minutes)
    }

    pub fn update_tooltip(&mut self, tray: &TrayIcon) {
        // Keep the menu bar countdown text in step with the tooltip; this runs
        // on the same ~1s cadence while a timed session is active.
        let title = self.menu_bar_title();
        if self.last_title != title {
            self.last_title = title.clone();
            // Clear with `Some("")`, never `None`: tray-icon's macOS backend
            // silently ignores `set_title(None)` (its `set_title_inner` only
            // acts on `Some`), which would leave the final countdown value
            // stuck in the menu bar after the session ends.
            tray.set_title(Some(title.as_deref().unwrap_or("")));
        }

        // Hold a fresh error tooltip in place: while a session is active this
        // refresh runs every tick, and repainting immediately would replace
        // the error within ~a second of the failure — before the user could
        // possibly hover to read it. The title above keeps updating; only the
        // tooltip text is held.
        if self
            .error_tooltip_until
            .is_some_and(|until| Instant::now() < until)
        {
            return;
        }

        let tooltip = if self.is_installing() {
            "caffeinate2 (installing helper…)".to_string()
        } else if self.is_enabling() {
            if self.pending_mode() == Some(SleepMode::Entirely) {
                "caffeinate2 (enabling Entirely mode…)".to_string()
            } else {
                "caffeinate2 (enabling…)".to_string()
            }
        } else if self.is_on() {
            let remaining = self.remaining_secs();
            let waiting = self.waiting_for_app_launch();
            let upgrading = self
                .session
                .as_ref()
                .filter(|session| session.started_by_upgrade)
                .map(|session| session.upgrade_apps.as_slice());
            tray_mode::format_active_tooltip(
                remaining,
                &self.config.wait_for_apps,
                waiting,
                upgrading,
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
        self.last_title = None;
    }

    /// Show an error in the tooltip (e.g. a denied entirely-mode hold).
    /// Call after `set_icon` so the icon reflects the real state. The text is
    /// held for [`ERROR_TOOLTIP_HOLD`] even while an active session's
    /// per-second refresh runs, then replaced by the next tooltip update.
    pub fn show_error_tooltip(&mut self, tray: &TrayIcon, message: &str) {
        let _ = tray.set_tooltip(Some(format!("caffeinate2 — {message}")));
        self.last_tooltip = None;
        self.error_tooltip_until = Some(Instant::now() + ERROR_TOOLTIP_HOLD);
    }
}

/// How long [`AppState::show_error_tooltip`]'s text survives the per-second
/// tooltip refresh. Long enough to hover after noticing something went wrong;
/// short enough that a stale error doesn't shadow live session state.
const ERROR_TOOLTIP_HOLD: Duration = Duration::from_secs(10);
