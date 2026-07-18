use crate::entirely::error::InstallError;
use crate::entirely::helper_ipc::HelperClient;
use crate::entirely::install;
use crate::sleep::power_management::{self, AssertionType, ExternalAssertion};
use crate::sleep::sleep_mode::{ActiveSleepHold, EnableError, SleepMode};
use crate::tray::app_target::WatchTarget;
use crate::tray::error::TrayError;
use crate::tray::process_enum;
use crate::tray::tray_icons;
use crate::tray::tray_mode::{self, TrayConfig};
use std::sync::mpsc;
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
const UPGRADE_IGNORE_PROCESSES: &[&str] = &["powerd", "runningboardd"];

/// Ignored processes so ubiquitous that surfacing them as "Ignoring…" lines is
/// pure noise: they hold an assertion during ordinary active use essentially
/// always, so listing them tells the user nothing. `powerd` (the macOS power
/// daemon) and `runningboardd` (the process/assertion lifecycle daemon) are
/// effectively always active, so they are dropped from the menu entirely rather
/// than shown as ignored.
const SILENTLY_IGNORED_PROCESSES: &[&str] = &["powerd", "runningboardd"];

/// Whether a failed upgrade enable should latch `upgrade_failed` (blocking
/// retries until the external trigger clears). Transient errors (helper socket
/// not up yet, IOKit flakes) are retried on the next poll; permanent denials
/// latch so the watcher does not spam install prompts or admin dialogs.
fn should_latch_upgrade_failure(error: &EnableError) -> bool {
    // Only policy denials are permanent. Transient helper/IOKit/IPC failures
    // (connect errors, internal errors, RPC timeouts, capacity limits) retry on
    // the next poll so a momentary flake does not disable upgrades for the
    // whole external-trigger episode.
    matches!(error, EnableError::NotAuthorized(_))
}

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
        // A nameless holder can't be shown meaningfully; never list a process
        // under "ignored" when another of its assertions is upgraded; and drop
        // the always-active system processes that would only add noise.
        .filter(|a| {
            !a.process_name.is_empty()
                && !trigger_names.contains(a.process_name.as_str())
                && !SILENTLY_IGNORED_PROCESSES.contains(&a.process_name.as_str())
        })
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

/// A sleep-prevention enable running on a background thread. Acquiring an
/// Entirely hold can block for several seconds (the helper install prompt plus
/// the post-install socket wait), so it must never run on the UI thread; the
/// worker delivers the acquired hold (or error) back over `rx` and wakes the
/// event loop, which commits it via [`AppState::poll_pending_enable`].
struct PendingEnable {
    mode: SleepMode,
    started_by_upgrade: bool,
    /// External process names to attach to the committed upgrade session.
    /// Refreshed by `poll_upgrade` while the enable is still in flight.
    upgrade_apps: Vec<String>,
    /// When set, revert `config.mode` (and persist) if this enable fails.
    /// Used by [`AppState::set_mode`] so a failed mode switch rolls back
    /// without leaving the tray off or on the wrong mode.
    rollback_mode: Option<SleepMode>,
    /// Snapshot of the previous session's "seen a watched app running" latch,
    /// captured when the enable started. Carried onto the committed session so a
    /// two-phase mode switch doesn't reset the latch (which would make the
    /// "seen running then all quit" auto-stop unreachable if the app quits
    /// during the acquire window). OR'd with a fresh scan at commit time.
    app_saw_seed: bool,
    rx: mpsc::Receiver<Result<ActiveSleepHold, EnableError>>,
}

/// Result of polling the in-flight enable, consumed by the run loop.
pub enum PendingEnableOutcome {
    /// No enable in flight.
    Idle,
    /// Still acquiring the hold on the background thread.
    Pending,
    /// The hold was acquired and a session committed.
    Started,
    /// Acquisition failed; carries the error for the UI. `started_by_upgrade`
    /// marks a background (watcher) enable, whose failures are logged
    /// (rate-limited) in [`AppState::poll_pending_enable`] and must not raise a
    /// user-facing error tooltip.
    Failed {
        error: EnableError,
        started_by_upgrade: bool,
    },
}

/// A privileged-helper install running on a background thread. Installing the
/// helper prompts for an admin password (a blocking `osascript` dialog), so it
/// must never run on the UI thread; the worker delivers the result back over
/// `rx` and wakes the event loop, which commits it via
/// [`AppState::poll_pending_install`].
struct PendingInstall {
    rx: mpsc::Receiver<Result<(), InstallError>>,
    /// Set when the watcher was toggled off while the install was still
    /// running. The admin dialog can't be revoked, so the worker keeps going
    /// and its result is discarded; the entry stays tracked so re-enabling
    /// re-attaches to it instead of stacking a second password dialog.
    cancelled: bool,
}

/// Result of polling the in-flight helper install, consumed by the run loop.
pub enum PendingInstallOutcome {
    /// No install in flight.
    Idle,
    /// Still installing on the background thread.
    Pending,
    /// The helper installed and `upgrade_external` was committed.
    Installed,
    /// The install failed; carries the error for the UI.
    Failed(TrayError),
}

/// Runtime state while sleep prevention is active.
struct ActiveTraySession {
    /// Held only for its `Drop`, which releases the underlying assertion/lock;
    /// the field is never read directly, hence the lint allowance.
    #[allow(dead_code)]
    hold: ActiveSleepHold,
    /// The mode this hold actually enforces. Tracked so a failed mode switch
    /// rolls `config.mode` back to what is *really* still active, rather than to
    /// an optimistic `config.mode` a prior in-flight switch already advanced.
    mode: SleepMode,
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
    /// An enable acquiring its hold off the UI thread; `None` when idle.
    pending_enable: Option<PendingEnable>,
    /// A privileged-helper install running off the UI thread; `None` when idle.
    /// Set only while enabling the upgrade watcher with the helper missing.
    pending_install: Option<PendingInstall>,
    /// Decoded on/off tray icons (RGBA + dimensions), cached at startup so an
    /// icon flip never re-decodes the embedded PNG. `None` only if decoding
    /// failed, in which case `show_icon_state` falls back to decoding on use.
    icon_on_rgba: Option<(Vec<u8>, u32, u32)>,
    icon_off_rgba: Option<(Vec<u8>, u32, u32)>,
    last_tooltip: Option<String>,
    /// Last menu bar title text pushed to the tray (the live countdown while a
    /// timed session runs, `None` when the icon should stand alone). Mirrors
    /// `last_tooltip`: cached so a per-second refresh only calls `set_title`
    /// when the displayed value actually changes.
    last_title: Option<String>,
    start_at_login: bool,
    /// When the external trigger assertion first went away while an upgrade
    /// session was active, used to debounce against agents briefly dropping it.
    upgrade_clear_since: Option<Instant>,
    /// True once an upgrade auto-start failed (e.g. helper denied the hold), so
    /// the watcher logs once and stops retrying until the trigger clears.
    upgrade_failed: bool,
    /// Last background-enable failure message logged, latched so a helper that
    /// keeps failing every poll logs once per distinct error rather than on
    /// every retry. Cleared when an enable succeeds; a changed message re-logs.
    last_upgrade_error: Option<String>,
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
            pending_enable: None,
            pending_install: None,
            icon_on_rgba: tray_icons::decode_icon_rgba(tray_icons::ICON_ON).ok(),
            icon_off_rgba: tray_icons::decode_icon_rgba(tray_icons::ICON_OFF).ok(),
            last_tooltip: None,
            last_title: None,
            start_at_login: install::tray_launch_agent_installed(),
            upgrade_clear_since: None,
            upgrade_failed: false,
            last_upgrade_error: None,
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
            // Show the checkbox checked while a helper install is still pending
            // (the commit is deferred until it confirms), so the toggle reads as
            // taken; a failed or cancelled install unchecks it.
            upgrade_external: self.config.upgrade_external || self.is_installing(),
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

    fn save_config(&self) -> Result<(), TrayError> {
        tray_mode::save_config(&self.config)
    }

    pub const fn is_on(&self) -> bool {
        self.session.is_some()
    }

    /// True while an enable is being acquired on a background thread. The icon
    /// shows the target on-state and the tooltip reads "enabling…" meanwhile.
    pub const fn is_enabling(&self) -> bool {
        self.pending_enable.is_some()
    }

    /// True while the privileged helper is being installed on a background
    /// thread (enabling the upgrade watcher with the helper missing). The
    /// tooltip reads "installing helper…" meanwhile. A cancelled install still
    /// in flight doesn't count: the user toggled it off, so the UI reads idle.
    pub const fn is_installing(&self) -> bool {
        matches!(&self.pending_install, Some(pending) if !pending.cancelled)
    }

    /// The mode of the in-flight enable, if any.
    const fn pending_mode(&self) -> Option<SleepMode> {
        match &self.pending_enable {
            Some(pending) => Some(pending.mode),
            None => None,
        }
    }

    /// How long to wait before the next loop iteration, or `None` to block
    /// indefinitely until an event arrives.
    pub fn pump_timeout(&self) -> Option<Duration> {
        // While an enable or a helper install is in flight, keep cycling so the
        // run loop polls the worker channel promptly even if its wake-up is
        // missed.
        if self.pending_enable.is_some() || self.pending_install.is_some() {
            return Some(Duration::from_millis(200));
        }
        // A timed session needs ~1s ticks to update the countdown tooltip and
        // the menu bar title (which flips only once a minute, so sub-second
        // wake phase doesn't matter).
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
                .map(|until| until.saturating_duration_since(Instant::now()).as_secs())
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

        let tooltip = if self.is_installing() {
            "caffeinate2 (installing helper…)".to_string()
        } else if self.pending_enable.is_some() {
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
        // Upgrade sessions also ignore the wait-for-apps list in tooltips
        // (see `format_active_tooltip` / `menu_snapshot`).
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

    pub fn toggle(&mut self) {
        if self.session.is_some() || self.pending_enable.is_some() {
            // A manual stop is an explicit override: the watcher must not undo it
            // by re-taking a hold while the same trigger persists. This also
            // cancels an in-flight enable the user changed their mind about.
            self.upgrade_overridden = true;
            self.cancel_pending_enable();
            self.stop_session();
            return;
        }
        self.start_session();
    }

    pub fn start_session(&mut self) {
        self.start_session_with(self.config.mode, false, None);
    }

    /// Begin acquiring a session in `mode` on a background thread (the helper
    /// install + socket wait must never block the UI thread). The hold is
    /// committed to a session by [`AppState::poll_pending_enable`] once the
    /// worker reports back. Upgrade-watcher sessions (`started_by_upgrade`)
    /// ignore the configured time limit — they end when the external assertion
    /// goes away, not on a clock.
    pub fn start_session_with(
        &mut self,
        mode: SleepMode,
        started_by_upgrade: bool,
        rollback_mode: Option<SleepMode>,
    ) {
        // Never stack enables; one in-flight request already targets a session.
        if self.pending_enable.is_some() {
            return;
        }
        // Capture the current session's "seen a watched app" latch now, so a
        // two-phase mode switch (which keeps the old session alive while the new
        // hold is acquired) doesn't lose it when the new session commits.
        let app_saw_seed = self
            .session
            .as_ref()
            .is_some_and(|session| session.app_saw_running);
        let (tx, rx) = mpsc::channel();
        let install_helper_if_missing = !started_by_upgrade;
        thread::spawn(move || {
            let result = mode.enable_for_tray(install_helper_if_missing);
            // If the receiver was dropped (the user cancelled), the hold inside
            // `result` is dropped here, releasing it.
            let _ = tx.send(result);
            crate::tray::macos_activation::wake_event_loop();
        });
        self.pending_enable = Some(PendingEnable {
            mode,
            started_by_upgrade,
            upgrade_apps: Vec::new(),
            rollback_mode,
            app_saw_seed,
            rx,
        });
        self.last_tooltip = None;
    }

    /// Cancel any in-flight enable. The worker thread keeps running but its
    /// result is discarded; if it already acquired a hold, dropping the
    /// receiver causes the worker to drop (and release) it.
    fn cancel_pending_enable(&mut self) {
        self.pending_enable = None;
    }

    /// Restore `config.mode` to the value captured before an enable that has
    /// now failed, so the persisted mode doesn't advance past the hold that is
    /// actually (still) enforced. No-op when the enable carried no rollback.
    fn rollback_pending_mode(&mut self, pending: &PendingEnable) {
        if let Some(rollback_mode) = pending.rollback_mode {
            self.config.mode = rollback_mode;
            if let Err(save_err) = self.save_config() {
                eprintln!("failed to roll back mode after enable failure: {save_err}");
            }
        }
    }

    /// Poll the in-flight enable, committing the session or surfacing the error.
    /// Called once per run-loop iteration.
    pub fn poll_pending_enable(&mut self) -> PendingEnableOutcome {
        let Some(pending) = self.pending_enable.as_ref() else {
            return PendingEnableOutcome::Idle;
        };
        let result = match pending.rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return PendingEnableOutcome::Pending,
            Err(mpsc::TryRecvError::Disconnected) => {
                // The worker vanished without sending (should not happen); treat
                // it as a failure (via the common Err path below, which rolls a
                // failed mode switch back) so the UI doesn't hang "enabling…".
                Err(EnableError::Ipc(
                    "enable worker terminated unexpectedly".to_string(),
                ))
            }
        };
        let pending = self.pending_enable.take().expect("pending enable present");
        match result {
            Ok(hold) => {
                let started_by_upgrade = pending.started_by_upgrade;
                let previous_started_by_upgrade = self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.started_by_upgrade);
                self.session = Some(ActiveTraySession {
                    hold,
                    mode: pending.mode,
                    until: if started_by_upgrade {
                        None
                    } else {
                        self.config
                            .time_limit_secs
                            .map(|secs| Instant::now() + Duration::from_secs(secs))
                    },
                    // Preserve the prior session's "seen running" latch across a
                    // two-phase switch (OR'd with a fresh scan) so a watched app
                    // that quit during the acquire window can still auto-stop.
                    // Invalidate the scan cache first: this seed is the session's
                    // first "seen running" decision and must reflect processes
                    // launched during the (possibly slow) acquire, not a scan
                    // cached before the enable began.
                    app_saw_running: pending.app_saw_seed || {
                        process_enum::invalidate_exec_path_cache();
                        process_enum::any_target_running(&self.config.wait_for_apps)
                    },
                    started_by_upgrade,
                    upgrade_apps: pending.upgrade_apps,
                });
                self.last_tooltip = None;
                // A successful enable clears the background-failure log latch so
                // a later distinct failure logs again.
                self.last_upgrade_error = None;
                if started_by_upgrade || previous_started_by_upgrade {
                    // Rebuild when entering or leaving an upgrade-started session.
                    self.menu_dirty = true;
                }
                PendingEnableOutcome::Started
            }
            Err(error) => {
                self.rollback_pending_mode(&pending);
                let started_by_upgrade = pending.started_by_upgrade;
                if started_by_upgrade {
                    if should_latch_upgrade_failure(&error) {
                        self.upgrade_failed = true;
                    }
                    // Rate-limit: a broken helper re-arms the enable every poll,
                    // so log once per distinct error message (until an enable
                    // succeeds or the message changes) rather than every retry.
                    let message = error.to_string();
                    if self.last_upgrade_error.as_deref() != Some(message.as_str()) {
                        eprintln!("upgrade external wakefulness failed: {error}");
                        self.last_upgrade_error = Some(message);
                    }
                }
                PendingEnableOutcome::Failed {
                    error,
                    started_by_upgrade,
                }
            }
        }
    }

    pub fn set_mode(&mut self, mode: SleepMode) -> Result<(), TrayError> {
        if mode == self.config.mode {
            return Ok(());
        }
        // Roll back to the mode actually being enforced, not to `config.mode`:
        // a prior in-flight switch may have already advanced `config.mode`
        // optimistically, so using it would leave the menu/config disagreeing
        // with the live hold if this switch fails. The live session's mode is
        // the source of truth; with no live session, the current config is the
        // best available target.
        let previous_mode = self
            .session
            .as_ref()
            .map_or(self.config.mode, |session| session.mode);
        // Persist first: build and save the new config, then commit it in RAM
        // only on success, so a failed write never leaves disk and memory
        // disagreeing.
        let mut new_config = self.config.clone();
        new_config.mode = mode;
        tray_mode::save_config(&new_config)?;
        self.config = new_config;

        if self.is_on() || self.pending_enable.is_some() {
            let was_upgrade = self
                .session
                .as_ref()
                .is_some_and(|session| session.started_by_upgrade)
                || self
                    .pending_enable
                    .as_ref()
                    .is_some_and(|pending| pending.started_by_upgrade);
            self.cancel_pending_enable();
            if was_upgrade {
                // Explicit mode change takes manual control away from the watcher.
                self.upgrade_overridden = true;
            }
            // Two-phase mode switch: keep the current session until the new hold
            // is acquired. On failure, poll_pending_enable rolls config back and
            // the old session (if any) keeps preventing sleep.
            if !self.is_on() {
                self.stop_session();
            }
            self.start_session_with(mode, false, Some(previous_mode));
        }
        Ok(())
    }

    /// Cancel any in-flight enable or helper install and tear down the active
    /// session. Used on shutdown so holds are released before the process exits.
    pub fn shutdown(&mut self) {
        self.cancel_pending_enable();
        self.cancel_pending_install();
        self.stop_session();
    }

    /// Set (or clear) the time limit. Deliberately restarts the countdown
    /// from now when a session is active, rather than rebasing on the
    /// session start (documented in the README).
    ///
    /// Persists the new value before mutating in-memory state so a failed save
    /// never leaves disk and RAM disagreeing.
    pub fn set_time_limit(&mut self, time_limit_secs: Option<u64>) -> Result<(), TrayError> {
        let mut new_config = self.config.clone();
        new_config.time_limit_secs = time_limit_secs;
        tray_mode::save_config(&new_config)?;
        self.config = new_config;

        if let Some(session) = self.session.as_mut() {
            if !session.started_by_upgrade {
                session.until =
                    time_limit_secs.map(|secs| Instant::now() + Duration::from_secs(secs));
            }
            self.last_tooltip = None;
        }
        Ok(())
    }

    /// Persists the new selection before mutating in-memory state so a failed
    /// save never leaves disk and RAM disagreeing.
    pub fn set_wait_for_apps(&mut self, targets: Vec<WatchTarget>) -> Result<(), TrayError> {
        let mut new_config = self.config.clone();
        new_config.wait_for_apps = targets;
        tray_mode::save_config(&new_config)?;
        self.config = new_config;

        // Re-seed the "seen running" guard from the new set so a freshly added,
        // already-running target keeps the session alive and an emptied list
        // does not auto-stop on the next tick. Only scan when a session is
        // actually active — otherwise the (potentially expensive) process walk
        // is computed and discarded on the idle tray.
        if self.session.is_some() {
            // The selection just changed, so this re-seed is a fresh "seen
            // running" decision for the new set. Drop the cached scan first so a
            // newly added, already-running target isn't missed by a scan cached
            // before the change.
            process_enum::invalidate_exec_path_cache();
            let seen = process_enum::any_target_running(&self.config.wait_for_apps);
            if let Some(session) = self.session.as_mut() {
                session.app_saw_running = seen;
                self.last_tooltip = None;
            }
        }
        Ok(())
    }

    pub fn set_start_at_login(&mut self, enabled: bool) -> Result<(), TrayError> {
        let tray_path = std::env::current_exe()?;
        if enabled {
            install::install_tray_launch_agent(&tray_path)?;
        } else {
            install::uninstall_tray_launch_agent()?;
        }
        self.start_at_login = enabled;
        Ok(())
    }

    /// Toggle the upgrade watcher. Enabling needs the privileged helper (upgrades
    /// use Entirely mode): if it is already installed the watcher turns on now,
    /// otherwise the install runs on a background thread (the admin-password
    /// dialog must never block the UI thread) and the watcher commits once the
    /// install confirms via [`AppState::poll_pending_install`]. Disabling stops
    /// any session the watcher started and cancels an install still in flight.
    pub fn set_upgrade_external(&mut self, enabled: bool) -> Result<(), TrayError> {
        if !enabled {
            // Turning off also cancels an in-flight install (the user dismissed
            // the toggle mid-prompt); its result is discarded, nothing committed.
            self.cancel_pending_install();
            if !self.config.upgrade_external {
                return Ok(());
            }
            // Persist first, commit in RAM on success (see set_mode).
            let mut new_config = self.config.clone();
            new_config.upgrade_external = false;
            tray_mode::save_config(&new_config)?;
            self.config = new_config;

            self.upgrade_clear_since = None;
            self.upgrade_failed = false;
            self.upgrade_overridden = false;
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
            return Ok(());
        }

        // Enabling: a no-op if already on or an install is already in flight.
        // An install cancelled mid-flight re-attaches instead: its admin dialog
        // is still up (it can't be revoked), so spawning another would stack a
        // second password prompt.
        if self.config.upgrade_external {
            return Ok(());
        }
        if let Some(pending) = self.pending_install.as_mut() {
            pending.cancelled = false;
            self.last_tooltip = None;
            return Ok(());
        }
        // Upgrades use Entirely mode, which needs the helper. If it is already
        // installed, turn the watcher on now; otherwise install it off the UI
        // thread and defer the commit to `poll_pending_install`. The watcher
        // stays off until the install confirms, so the persisted config never
        // claims the watcher is on without the helper present. launchd then
        // starts the helper asynchronously, so the socket may not be up
        // immediately — the watcher's `poll_upgrade` retries the hold once it
        // appears.
        if HelperClient::new().is_available() {
            self.commit_upgrade_external_on()?;
        } else {
            self.spawn_pending_install();
        }
        Ok(())
    }

    /// Persist and commit `upgrade_external = true`, clearing the per-episode
    /// watcher latches. Shared by the synchronous enable (helper already present)
    /// and the deferred commit once a background install confirms.
    fn commit_upgrade_external_on(&mut self) -> Result<(), TrayError> {
        // Persist first, commit in RAM on success (see set_mode).
        let mut new_config = self.config.clone();
        new_config.upgrade_external = true;
        tray_mode::save_config(&new_config)?;
        self.config = new_config;

        self.upgrade_clear_since = None;
        self.upgrade_failed = false;
        self.upgrade_overridden = false;
        Ok(())
    }

    /// Install the privileged helper on a background thread. The worker reports
    /// back over `rx` and wakes the event loop; [`AppState::poll_pending_install`]
    /// commits `upgrade_external` on success or surfaces the error.
    fn spawn_pending_install(&mut self) {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = install::install_helper_privileged();
            let _ = tx.send(result);
            crate::tray::macos_activation::wake_event_loop();
        });
        self.pending_install = Some(PendingInstall {
            rx,
            cancelled: false,
        });
        self.last_tooltip = None;
    }

    /// Cancel any in-flight helper install; its result is discarded. The worker
    /// thread keeps running (the admin dialog can't be revoked) but nothing is
    /// committed when it finishes, and it never blocks shutdown. The entry
    /// stays tracked (marked cancelled) so a re-enable before the worker
    /// finishes re-attaches to it rather than opening a second dialog.
    fn cancel_pending_install(&mut self) {
        if let Some(pending) = self.pending_install.as_mut() {
            pending.cancelled = true;
        }
    }

    /// Poll the in-flight helper install, committing `upgrade_external` or
    /// surfacing the error. Called once per run-loop iteration. A cancelled
    /// install is drained the same way but its result is discarded (no commit,
    /// no error surfaced — the user already turned the watcher off).
    pub fn poll_pending_install(&mut self) -> PendingInstallOutcome {
        let Some(pending) = self.pending_install.as_ref() else {
            return PendingInstallOutcome::Idle;
        };
        let cancelled = pending.cancelled;
        let result = match pending.rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return PendingInstallOutcome::Pending,
            Err(mpsc::TryRecvError::Disconnected) => Err(InstallError::msg(
                "helper install worker terminated unexpectedly",
            )),
        };
        self.pending_install = None;
        if cancelled {
            return PendingInstallOutcome::Idle;
        }
        match result {
            Ok(()) => match self.commit_upgrade_external_on() {
                Ok(()) => PendingInstallOutcome::Installed,
                Err(error) => PendingInstallOutcome::Failed(error),
            },
            Err(error) => PendingInstallOutcome::Failed(error.into()),
        }
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
    /// - absent, manual override or failure latch → clear those after the same
    ///   grace (agents can drop their assertion briefly between turns);
    /// - absent otherwise → do nothing.
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
            let needs_clear_grace = self
                .session
                .as_ref()
                .is_some_and(|session| session.started_by_upgrade)
                || self.upgrade_overridden
                || self.upgrade_failed;
            if !needs_clear_grace {
                self.upgrade_clear_since = None;
                return false;
            }
            // Debounce: tolerate the agent dropping its assertion momentarily.
            let now = Instant::now();
            let since = *self.upgrade_clear_since.get_or_insert(now);
            if now.duration_since(since) >= UPGRADE_CLEAR_GRACE {
                // Trigger episode is over: clear the manual override and failure
                // latch so the next episode upgrades normally.
                self.upgrade_failed = false;
                self.upgrade_overridden = false;
                let had_upgrade_session = self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.started_by_upgrade);
                if had_upgrade_session {
                    // stop_session flags the menu dirty so the entries disappear.
                    self.stop_session();
                }
                self.upgrade_clear_since = None;
                return had_upgrade_session;
            }
            return false;
        }

        // Something external is holding a system-idle assertion.
        self.upgrade_clear_since = None;
        match self.session.as_mut() {
            None => {
                if self.upgrade_failed || self.upgrade_overridden {
                    return false;
                }
                if self.pending_enable.is_some() {
                    if let Some(pending) = self.pending_enable.as_mut()
                        && pending.started_by_upgrade
                        && pending.upgrade_apps != app_names
                    {
                        pending.upgrade_apps = app_names;
                        self.last_tooltip = None;
                        return true;
                    }
                    return false;
                }
                self.start_session_with(SleepMode::Entirely, true, None);
                if let Some(pending) = self.pending_enable.as_mut() {
                    pending.upgrade_apps = app_names;
                }
                self.last_tooltip = None;
                self.menu_dirty = true;
                true
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
    fn silently_ignored_system_processes_are_neither_triggers_nor_listed() {
        let all = [
            assertion(1, "powerd", AssertionType::PreventUserIdleSystemSleep),
            assertion(
                2,
                "runningboardd",
                AssertionType::PreventUserIdleSystemSleep,
            ),
        ];
        let result = classify_external_assertions(&all);
        // These never trigger an upgrade ...
        assert!(result.upgradeable.is_empty());
        // ... and, being always active, are dropped from the menu entirely.
        assert!(result.ignored.is_empty());
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
            // powerd is silently ignored, so it never reaches the list.
            assertion(51, "powerd", AssertionType::PreventUserIdleSystemSleep),
            assertion(53, "IINA", AssertionType::PreventUserIdleDisplaySleep),
            // Duplicate display hold from the same app collapses to one entry.
            assertion(52, "VLC", AssertionType::PreventUserIdleDisplaySleep),
        ];
        let result = classify_external_assertions(&all);
        assert_eq!(
            result.ignored,
            vec![
                ignored("IINA", "display only"),
                ignored("VLC", "display only"),
            ]
        );
    }

    #[test]
    fn upgrade_failure_latching_rules() {
        assert!(super::should_latch_upgrade_failure(
            &EnableError::NotAuthorized("denied".into())
        ));
        assert!(!super::should_latch_upgrade_failure(
            &EnableError::HelperUnavailable
        ));
        assert!(!super::should_latch_upgrade_failure(&EnableError::Iokit(0)));
        assert!(!super::should_latch_upgrade_failure(&EnableError::Ipc(
            "connect failed: connection refused".into()
        )));
        assert!(!super::should_latch_upgrade_failure(&EnableError::Ipc(
            "too many concurrent clients".into()
        )));
        assert!(!super::should_latch_upgrade_failure(&EnableError::Ipc(
            "internal error: unreadable peer credentials".into()
        )));
        assert!(!super::should_latch_upgrade_failure(&EnableError::Ipc(
            "read failed: timeout".into()
        )));
        assert!(!super::should_latch_upgrade_failure(&EnableError::Ipc(
            "mock enable failure".into()
        )));
    }

    #[test]
    fn mode_switch_enable_failure_restores_previous_mode() {
        // Point HOME at a throwaway dir so AppState::new() (load_config) and the
        // rollback's save_config() operate on an isolated config, never the
        // user's real ~/Library/Application Support/caffeinate2/tray.toml. No
        // other test reads or writes HOME-derived config, so this can't race.
        let temp_home = std::env::temp_dir().join(format!(
            "caffeinate2_state_test_{}_{}",
            std::process::id(),
            line!()
        ));
        let prev_home = std::env::var_os("HOME");
        // SAFETY: single-threaded with respect to HOME — no concurrent test
        // reads or mutates it (see comment above).
        unsafe { std::env::set_var("HOME", &temp_home) };

        let mut state = AppState::new();
        let original_mode = state.config.mode;
        let target_mode = if original_mode == SleepMode::Display {
            SleepMode::System
        } else {
            SleepMode::Display
        };

        state.config.mode = target_mode;
        let (tx, rx) = mpsc::channel();
        tx.send(Err(EnableError::Ipc("mock enable failure".into())))
            .unwrap();
        state.pending_enable = Some(PendingEnable {
            mode: target_mode,
            started_by_upgrade: false,
            upgrade_apps: Vec::new(),
            rollback_mode: Some(original_mode),
            app_saw_seed: false,
            rx,
        });

        let outcome = state.poll_pending_enable();
        assert!(matches!(outcome, PendingEnableOutcome::Failed { .. }));
        assert_eq!(state.config.mode, original_mode);

        // SAFETY: see the set_var note above.
        unsafe {
            match prev_home {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&temp_home);
    }
}
