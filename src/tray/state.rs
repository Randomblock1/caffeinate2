use crate::entirely::helper_ipc::HelperClient;
use crate::entirely::install;
use crate::sleep::power_management::{self, AssertionType, ExternalAssertion};
use crate::sleep::sleep_mode::{ActiveSleepHold, EnableError, SleepMode};
use crate::tray::app_target::WatchTarget;
use crate::tray::process_enum;
use crate::tray::tray_icons;
use crate::tray::tray_mode::{self, TrayConfig};
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::{Icon, TrayIcon};

/// How often to re-check external assertions while the upgrade watcher is on.
/// External assertion changes post no `NSWorkspace` notification, so the only
/// option is polling — the same cadence `pmset` uses.
const UPGRADE_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Keep an upgrade hold this long after the external assertion disappears.
/// Agents (Claude Code, Codex, …) routinely drop their assertion for a moment
/// between turns; the grace period avoids flapping the hold off and on.
const UPGRADE_CLEAR_GRACE: Duration = Duration::from_secs(20);

/// Assertion type the watcher upgrades: the system-idle hold (what agents and
/// `caffeinate -i` use). Display-only assertions (video players) are surfaced as
/// "ignored" rather than upgraded.
const UPGRADE_TRIGGER_TYPE: AssertionType = AssertionType::PreventUserIdleSystemSleep;

/// Assertion types the watcher scans for: the one it upgrades plus the
/// display-only one, so the latter can be reported as ignored instead of
/// silently dropped. Other types (disk idle) are intentionally left out to keep
/// the "ignored" list focused on sleep-relevant holds and free of system noise.
const OBSERVED_TYPES: &[AssertionType] = &[
    AssertionType::PreventUserIdleSystemSleep,
    AssertionType::PreventUserIdleDisplaySleep,
];

/// Processes whose system-idle assertions never warrant an upgrade. `powerd` is
/// the macOS power daemon that holds such an assertion during ordinary active
/// use and releases it on its own; taking a stronger Entirely hold over it does
/// nothing but flap the watcher, so it is treated as ignored rather than a
/// trigger.
const UPGRADE_IGNORE_PROCESSES: &[&str] = &["powerd"];

/// A sleep-relevant external assertion the watcher saw but did not upgrade,
/// paired with the reason, for the informational "Ignoring…" menu entries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct IgnoredAssertion {
    pub process_name: String,
    pub reason: String,
}

/// The watcher's verdict on the external assertions found in one poll.
struct ExternalClassification {
    /// System-idle holders worth upgrading (not in the ignore list). Nameless
    /// holders are kept so presence is detected even without a process name.
    upgradeable: Vec<ExternalAssertion>,
    /// Holders deliberately not upgraded, with a reason, for display.
    ignored: Vec<IgnoredAssertion>,
}

/// Why an observed assertion is not upgraded. Drives the menu's reason text.
fn ignore_reason(assertion: &ExternalAssertion) -> &'static str {
    if UPGRADE_IGNORE_PROCESSES.contains(&assertion.process_name.as_str()) {
        "system process"
    } else if assertion.assertion_type == AssertionType::PreventUserIdleDisplaySleep.as_str() {
        "display only"
    } else {
        "not upgradeable"
    }
}

/// Split the observed external assertions into the ones worth upgrading and the
/// ones to report as ignored. Pure (no `IOKit`) so it can be unit-tested.
fn classify_external_assertions(all: &[ExternalAssertion]) -> ExternalClassification {
    let trigger_type = UPGRADE_TRIGGER_TYPE.as_str();
    let is_upgradeable = |a: &ExternalAssertion| {
        a.assertion_type == trigger_type
            && !UPGRADE_IGNORE_PROCESSES.contains(&a.process_name.as_str())
    };

    let upgradeable: Vec<ExternalAssertion> =
        all.iter().filter(|a| is_upgradeable(a)).cloned().collect();
    let trigger_names: std::collections::HashSet<&str> = upgradeable
        .iter()
        .map(|a| a.process_name.as_str())
        .collect();

    let mut ignored: Vec<IgnoredAssertion> = all
        .iter()
        .filter(|a| !is_upgradeable(a))
        // A nameless holder can't be shown meaningfully; and never list a
        // process under "ignored" when another of its assertions is upgraded.
        .filter(|a| !a.process_name.is_empty() && !trigger_names.contains(a.process_name.as_str()))
        .map(|a| IgnoredAssertion {
            process_name: a.process_name.clone(),
            reason: ignore_reason(a).to_string(),
        })
        .collect();
    ignored.sort();
    ignored.dedup();

    ExternalClassification {
        upgradeable,
        ignored,
    }
}

/// Runtime state while sleep prevention is active.
struct ActiveTraySession {
    #[allow(dead_code)]
    hold: ActiveSleepHold,
    until: Option<Instant>,
    app_saw_running: bool,
    /// True when the upgrade watcher started this session (vs. a manual
    /// left-click or menu action). The watcher only ever auto-stops sessions
    /// it started, and other stop conditions ignore them.
    started_by_upgrade: bool,
    /// Names of the external processes being upgraded, for the tooltip and menu
    /// (deduped and sorted; empty when none are known). Only meaningful when
    /// `started_by_upgrade` is true.
    upgrade_apps: Vec<String>,
}

/// Menu bar settings used to build the tray menu (no runtime session state).
#[derive(Debug, Clone)]
pub struct MenuSnapshot {
    pub mode: SleepMode,
    pub time_limit_secs: Option<u64>,
    pub wait_for_apps: Vec<WatchTarget>,
    pub start_at_login: bool,
    pub upgrade_external: bool,
    /// `Some` while the watcher is actively upgrading, carrying the external
    /// process names (empty when none are known). `None` when the active
    /// session was not started by the watcher. Drives the "Upgrading…" entries.
    pub upgrading_apps: Option<Vec<String>>,
    /// Sleep-relevant external assertions the watcher saw but did not upgrade,
    /// with reasons. Drives the informational "Ignoring…" entries. Empty when
    /// the watcher is off. Independent of any session, so it explains a blank
    /// menu even when nothing is being upgraded.
    pub ignored_assertions: Vec<IgnoredAssertion>,
}

#[allow(clippy::struct_excessive_bools)] // session/watch flags are independent toggles
pub struct AppState {
    config: TrayConfig,
    session: Option<ActiveTraySession>,
    last_tooltip: Option<String>,
    start_at_login: bool,
    /// When the external trigger assertion first went away while an upgrade
    /// session was active, used to debounce against agents briefly dropping it.
    upgrade_clear_since: Option<Instant>,
    /// True once an upgrade auto-start failed (e.g. helper denied the hold), so
    /// the watcher logs once and stops retrying until the trigger clears.
    upgrade_failed: bool,
    /// Set when the user manually clicks the tray icon: a manual click overrides
    /// the watcher for the rest of the current external-trigger episode, so the
    /// watcher won't immediately re-take a hold the user just dismissed. Reset
    /// once the trigger goes away (the next episode upgrades normally).
    upgrade_overridden: bool,
    /// Latest set of ignored external assertions from `poll_upgrade`, surfaced
    /// in the menu. Updated only while the watcher is on; cleared when it is off.
    upgrade_ignored: Vec<IgnoredAssertion>,
    /// Set when the tray menu's structure (not just checkbox state) needs to be
    /// rebuilt — currently when the set of upgraded processes changes. The event
    /// loop reinstalls the menu and clears this via `take_menu_dirty`.
    menu_dirty: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            config: tray_mode::load_config(),
            session: None,
            last_tooltip: None,
            start_at_login: install::tray_launch_agent_installed(),
            upgrade_clear_since: None,
            upgrade_failed: false,
            upgrade_overridden: false,
            upgrade_ignored: Vec::new(),
            menu_dirty: false,
        }
    }

    pub fn menu_snapshot(&self) -> MenuSnapshot {
        MenuSnapshot {
            mode: self.config.mode,
            time_limit_secs: self.config.time_limit_secs,
            wait_for_apps: self.config.wait_for_apps.clone(),
            start_at_login: self.start_at_login,
            upgrade_external: self.config.upgrade_external,
            upgrading_apps: self
                .session
                .as_ref()
                .filter(|session| session.started_by_upgrade)
                .map(|session| session.upgrade_apps.clone()),
            ignored_assertions: if self.config.upgrade_external {
                self.upgrade_ignored.clone()
            } else {
                Vec::new()
            },
        }
    }

    /// Whether the menu structure changed since the last call (and clear the
    /// flag). The event loop uses this to rebuild the menu when the set of
    /// upgraded processes changes.
    pub fn take_menu_dirty(&mut self) -> bool {
        std::mem::take(&mut self.menu_dirty)
    }

    fn save_config(&self) -> Result<(), String> {
        tray_mode::save_config(&self.config)
    }

    pub const fn is_on(&self) -> bool {
        self.session.is_some()
    }

    /// How long to wait before the next loop iteration, or `None` to block
    /// indefinitely until an event arrives.
    pub fn pump_timeout(&self) -> Option<Duration> {
        // A timed session needs ~1s ticks to update the countdown tooltip.
        let countdown = self.session.as_ref().and_then(|s| s.until).map(|until| {
            until
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(1))
        });

        // Daemon / non-.app targets post no NSWorkspace notification, so a
        // session watching any Executable target must poll like the upgrade
        // watcher does.
        let needs_proc_poll = self.is_on()
            && self
                .config
                .wait_for_apps
                .iter()
                .any(|target| matches!(target, WatchTarget::Executable { .. }));

        // The upgrade watcher needs to keep polling even while idle, since
        // external assertion changes arrive via no event.
        if self.config.upgrade_external || needs_proc_poll {
            return countdown.map_or(Some(UPGRADE_POLL_INTERVAL), |remaining| {
                Some(remaining.min(UPGRADE_POLL_INTERVAL))
            });
        }
        countdown
    }

    pub fn waiting_for_app_launch(&self) -> bool {
        !self.config.wait_for_apps.is_empty()
            && self.is_on()
            && !self.session.as_ref().is_some_and(|s| s.app_saw_running)
            && !process_enum::any_target_running(&self.config.wait_for_apps)
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
    }

    /// Show an error in the tooltip (e.g. a denied entirely-mode hold).
    /// Call after `set_icon` so the icon reflects the real state; the text
    /// persists while idle and is replaced on the next tooltip update.
    pub fn show_error_tooltip(&mut self, tray: &TrayIcon, message: &str) {
        let _ = tray.set_tooltip(Some(format!("caffeinate2 — {message}")));
        self.last_tooltip = None;
    }

    pub fn stop_session(&mut self) {
        // Removing an upgrade session changes the menu's structure (its
        // "Upgrading…" entries), which the checkbox-only sync can't undo — so
        // flag a rebuild here. Centralizing it covers every teardown path
        // (toggle, set_mode, grace-stop, disabling the watcher) at once.
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.started_by_upgrade)
        {
            self.menu_dirty = true;
        }
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
        // The upgrade watcher owns its session's start/stop lifecycle; a
        // configured app watch must never cut an upgrade session short.
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.started_by_upgrade)
        {
            return false;
        }
        if self.config.wait_for_apps.is_empty() {
            return false;
        }
        // Resolve before the mutable session borrow — the scan can be a
        // process-tree walk.
        let any_running = process_enum::any_target_running(&self.config.wait_for_apps);
        let Some(session) = self.session.as_mut() else {
            return false;
        };

        if any_running {
            session.app_saw_running = true;
            return false;
        }

        // Stop only once at least one target was seen running and now all have
        // quit ("until all selected quit").
        if session.app_saw_running {
            self.stop_session();
            return true;
        }

        false
    }

    pub fn toggle(&mut self) -> Result<(), EnableError> {
        if self.session.is_some() {
            // A manual stop is an explicit override: the watcher must not undo it
            // by re-taking a hold while the same trigger persists.
            self.upgrade_overridden = true;
            self.stop_session();
            return Ok(());
        }
        self.start_session()
    }

    pub fn start_session(&mut self) -> Result<(), EnableError> {
        self.start_session_with(self.config.mode, false)
    }

    /// Start a session in `mode`. Upgrade-watcher sessions (`started_by_upgrade`)
    /// ignore the configured time limit — they end when the external assertion
    /// goes away, not on a clock.
    pub fn start_session_with(
        &mut self,
        mode: SleepMode,
        started_by_upgrade: bool,
    ) -> Result<(), EnableError> {
        let hold = mode.enable_for_tray()?;
        self.session = Some(ActiveTraySession {
            hold,
            until: if started_by_upgrade {
                None
            } else {
                self.config
                    .time_limit_secs
                    .map(|secs| Instant::now() + Duration::from_secs(secs))
            },
            app_saw_running: process_enum::any_target_running(&self.config.wait_for_apps),
            started_by_upgrade,
            upgrade_apps: Vec::new(),
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
            let was_upgrade = self
                .session
                .as_ref()
                .is_some_and(|session| session.started_by_upgrade);
            self.stop_session();
            self.config.mode = mode;
            if was_upgrade {
                // Explicit mode change takes manual control away from the watcher.
                self.upgrade_overridden = true;
            }
            if let Err(error) = self.start_session_with(mode, false) {
                self.config.mode = previous_mode;
                let _ = self.start_session_with(previous_mode, false);
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
            if !session.started_by_upgrade {
                session.until =
                    time_limit_secs.map(|secs| Instant::now() + Duration::from_secs(secs));
            }
            self.last_tooltip = None;
        }
        self.save_config()
    }

    pub fn set_wait_for_apps(&mut self, targets: Vec<WatchTarget>) -> Result<(), String> {
        self.config.wait_for_apps = targets;
        // Re-seed the "seen running" guard from the new set so a freshly added,
        // already-running target keeps the session alive and an emptied list
        // does not auto-stop on the next tick.
        let seen = process_enum::any_target_running(&self.config.wait_for_apps);
        if let Some(session) = self.session.as_mut() {
            session.app_saw_running = seen;
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

    /// Toggle the upgrade watcher. Enabling installs the privileged helper now
    /// (so the admin prompt happens on the click, not mid-watch); disabling
    /// stops any session the watcher started.
    pub fn set_upgrade_external(&mut self, enabled: bool) -> Result<(), String> {
        let previous = self.config.upgrade_external;
        if enabled == previous {
            return Ok(());
        }
        self.config.upgrade_external = enabled;
        self.save_config()?;

        self.upgrade_clear_since = None;
        self.upgrade_failed = false;
        self.upgrade_overridden = false;
        if enabled {
            // Upgrades use Entirely mode, which needs the helper. Surface the
            // admin prompt here rather than on the first external assertion.
            let client = HelperClient::new();
            if !client.is_available() {
                if let Err(error) = install::install_helper_privileged() {
                    self.config.upgrade_external = previous;
                    let _ = self.save_config();
                    return Err(error);
                }
                // launchd starts the helper asynchronously. Let the socket check
                // finish off the UI thread; the watcher will retry on its poll.
                thread::spawn(move || {
                    for _ in 0..25 {
                        if client.is_available() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(200));
                    }
                });
            }
        } else {
            // Drop any "Ignoring…" entries; the menu rebuild removes them.
            if !self.upgrade_ignored.is_empty() {
                self.upgrade_ignored.clear();
                self.menu_dirty = true;
            }
            if self
                .session
                .as_ref()
                .is_some_and(|session| session.started_by_upgrade)
            {
                // stop_session flags the menu dirty so the entries disappear.
                self.stop_session();
            }
        }
        Ok(())
    }

    /// Poll external assertions and start/stop an upgrade session as needed.
    /// Returns true when the icon/tooltip should be refreshed.
    ///
    /// Also refreshes the informational "ignored" list (assertions seen but not
    /// upgraded), flagging a menu rebuild when it changes.
    ///
    /// State machine (only while `upgrade_external` is on):
    /// - external trigger present, no session, not overridden → take an Entirely hold;
    /// - present, upgrade session → refresh the displayed process name;
    /// - present, manual session (or manual override) → leave it;
    /// - absent, upgrade session → release after `UPGRADE_CLEAR_GRACE`;
    /// - absent otherwise → clear the manual override and do nothing.
    pub fn poll_upgrade(&mut self) -> bool {
        if !self.config.upgrade_external {
            self.upgrade_clear_since = None;
            // Drop any stale "Ignoring…" entries; the menu rebuild removes them.
            if !self.upgrade_ignored.is_empty() {
                self.upgrade_ignored.clear();
                self.menu_dirty = true;
            }
            return false;
        }

        let Ok(external) = power_management::external_assertions(OBSERVED_TYPES) else {
            // Transient IOKit failure; leave the current state and retry.
            return false;
        };
        let ExternalClassification {
            upgradeable,
            ignored,
        } = classify_external_assertions(&external);

        // Refresh the informational ignored list. Rebuild the menu only when the
        // set actually changes so an open menu isn't churned every poll.
        if self.upgrade_ignored != ignored {
            self.upgrade_ignored = ignored;
            self.menu_dirty = true;
        }

        let present = !upgradeable.is_empty();
        // Distinct, sorted process names so the menu/tooltip stay stable across
        // polls (and don't show the same app twice when it holds several
        // assertions). The list can be empty even when `present` is true if no
        // holder exposed a name.
        let mut app_names: Vec<String> = upgradeable
            .into_iter()
            .map(|assertion| assertion.process_name)
            .filter(|name| !name.is_empty())
            .collect();
        app_names.sort();
        app_names.dedup();

        if !present {
            // Trigger episode is over: clear the manual override and failure
            // latch so the next episode upgrades normally.
            self.upgrade_failed = false;
            self.upgrade_overridden = false;
            if !self
                .session
                .as_ref()
                .is_some_and(|session| session.started_by_upgrade)
            {
                self.upgrade_clear_since = None;
                return false;
            }
            // Debounce: tolerate the agent dropping its assertion momentarily.
            let now = Instant::now();
            let since = *self.upgrade_clear_since.get_or_insert(now);
            if now.duration_since(since) >= UPGRADE_CLEAR_GRACE {
                // stop_session flags the menu dirty so the entries disappear.
                self.stop_session();
                self.upgrade_clear_since = None;
                return true;
            }
            return false;
        }

        // Something external is holding a system-idle assertion.
        self.upgrade_clear_since = None;
        match self.session.as_mut() {
            None => {
                if self.upgrade_failed || self.upgrade_overridden {
                    // Either a prior attempt failed (e.g. helper denied the
                    // hold) or the user manually dismissed it; wait for the
                    // trigger to clear before considering an upgrade again.
                    return false;
                }
                match self.start_session_with(SleepMode::Entirely, true) {
                    Ok(()) => {
                        if let Some(session) = self.session.as_mut() {
                            session.upgrade_apps = app_names;
                        }
                        self.last_tooltip = None;
                        // A new "Upgrading…" entry needs a menu rebuild.
                        self.menu_dirty = true;
                        true
                    }
                    Err(error) => {
                        if matches!(error, EnableError::NotAuthorized(_)) {
                            self.upgrade_failed = true;
                        }
                        eprintln!("upgrade external wakefulness failed: {error}");
                        false
                    }
                }
            }
            Some(session) if session.started_by_upgrade => {
                if session.upgrade_apps == app_names {
                    false
                } else {
                    session.upgrade_apps = app_names;
                    self.last_tooltip = None;
                    // The displayed process list changed; rebuild the menu.
                    self.menu_dirty = true;
                    true
                }
            }
            // A manual session is already preventing sleep; leave it untouched.
            Some(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assertion(pid: i32, name: &str, type_: AssertionType) -> ExternalAssertion {
        ExternalAssertion {
            pid,
            process_name: name.to_string(),
            assertion_type: type_.as_str().to_string(),
        }
    }

    fn ignored(name: &str, reason: &str) -> IgnoredAssertion {
        IgnoredAssertion {
            process_name: name.to_string(),
            reason: reason.to_string(),
        }
    }

    #[test]
    fn system_idle_holder_is_upgradeable_not_ignored() {
        let all = [assertion(
            10,
            "Claude Code",
            AssertionType::PreventUserIdleSystemSleep,
        )];
        let result = classify_external_assertions(&all);
        assert_eq!(result.upgradeable.len(), 1);
        assert_eq!(result.upgradeable[0].process_name, "Claude Code");
        assert!(result.ignored.is_empty());
    }

    #[test]
    fn ignore_listed_system_process_is_ignored_not_a_trigger() {
        let all = [assertion(
            1,
            "powerd",
            AssertionType::PreventUserIdleSystemSleep,
        )];
        let result = classify_external_assertions(&all);
        assert!(result.upgradeable.is_empty());
        assert_eq!(result.ignored, vec![ignored("powerd", "system process")]);
    }

    #[test]
    fn display_only_holder_is_ignored() {
        let all = [assertion(
            20,
            "Safari",
            AssertionType::PreventUserIdleDisplaySleep,
        )];
        let result = classify_external_assertions(&all);
        assert!(result.upgradeable.is_empty());
        assert_eq!(result.ignored, vec![ignored("Safari", "display only")]);
    }

    #[test]
    fn nameless_holder_is_dropped_from_ignored_but_still_a_trigger() {
        let all = [
            assertion(30, "", AssertionType::PreventUserIdleSystemSleep),
            assertion(31, "", AssertionType::PreventUserIdleDisplaySleep),
        ];
        let result = classify_external_assertions(&all);
        // The nameless system-idle hold still counts as upgradeable (presence is
        // detected without a name) ...
        assert_eq!(result.upgradeable.len(), 1);
        // ... but no nameless entry is shown in the ignored list.
        assert!(result.ignored.is_empty());
    }

    #[test]
    fn upgraded_process_is_not_double_listed_as_ignored() {
        // One process holds both a system-idle (upgraded) and a display hold;
        // it must not also appear under "ignored".
        let all = [
            assertion(40, "Zoom", AssertionType::PreventUserIdleSystemSleep),
            assertion(40, "Zoom", AssertionType::PreventUserIdleDisplaySleep),
        ];
        let result = classify_external_assertions(&all);
        assert_eq!(result.upgradeable.len(), 1);
        assert!(result.ignored.is_empty());
    }

    #[test]
    fn ignored_list_is_sorted_and_deduped() {
        let all = [
            assertion(50, "VLC", AssertionType::PreventUserIdleDisplaySleep),
            assertion(51, "powerd", AssertionType::PreventUserIdleSystemSleep),
            // Duplicate display hold from the same app collapses to one entry.
            assertion(52, "VLC", AssertionType::PreventUserIdleDisplaySleep),
        ];
        let result = classify_external_assertions(&all);
        assert_eq!(
            result.ignored,
            vec![
                ignored("VLC", "display only"),
                ignored("powerd", "system process"),
            ]
        );
    }
}
