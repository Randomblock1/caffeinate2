//! The upgrade watcher's classification rules: which external sleep assertions
//! are worth upgrading, which are deliberately ignored (and why), and which
//! belong to the operating system and are dropped silently.
//!
//! Pure decision logic — no `IOKit`, no live PIDs, no session state. The rules
//! take their environment (ignore lists, the system-process check) as
//! parameters so they can be unit-tested standalone; the session lifecycle
//! that acts on these verdicts lives in [`crate::tray::state`].

use crate::sleep::power_management::{AssertionType, ExternalAssertion};
use std::collections::HashSet;

/// Assertion type the watcher upgrades: the system-idle hold (what agents and
/// `caffeinate -i` use). Display-only assertions (video players) are surfaced as
/// "ignored" rather than upgraded.
const UPGRADE_TRIGGER_TYPE: AssertionType = AssertionType::PreventUserIdleSystemSleep;

/// Assertion types the watcher scans for: the one it upgrades plus the
/// display-only one, so the latter can be reported as ignored instead of
/// silently dropped. Other types (disk idle) are intentionally left out to keep
/// the "ignored" list focused on sleep-relevant holds and free of system noise.
pub const OBSERVED_TYPES: &[AssertionType] = &[
    AssertionType::PreventUserIdleSystemSleep,
    AssertionType::PreventUserIdleDisplaySleep,
];

/// Reason text for the menu's "Ignoring…" lines, one per non-upgraded verdict.
pub const REASON_USER_IGNORED: &str = "you ignore this app";
pub const REASON_IGNORED_ONCE: &str = "ignored this time";
pub const REASON_DISPLAY_ONLY: &str = "display only";

/// A sleep-relevant external assertion the watcher saw but did not upgrade,
/// paired with the reason, for the informational "Ignoring…" menu entries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct IgnoredAssertion {
    pub process_name: String,
    pub reason: String,
}

/// The watcher's verdict on the external assertions found in one poll.
pub struct ExternalClassification {
    /// System-idle holders worth upgrading. Nameless holders are kept so
    /// presence is detected even without a process name.
    pub upgradeable: Vec<ExternalAssertion>,
    /// Holders deliberately not upgraded, with a reason, for display.
    pub ignored: Vec<IgnoredAssertion>,
}

/// What the watcher does with one observed assertion.
enum Verdict {
    /// Take a stronger hold while this assertion lasts.
    Upgrade,
    /// Neither upgrade nor mention it. Reserved for the operating system's own
    /// holders: macOS daemons (`powerd`, `runningboardd`, `coreaudiod`, …) hold
    /// sleep assertions during ordinary use essentially always and release them
    /// on their own, so upgrading them only flaps the watcher and listing them is
    /// pure noise. Only the user's programs are ever upgraded.
    DropSilently,
    /// Don't upgrade, but say so in the menu, with this reason.
    Ignore(&'static str),
}

/// The watcher's decision for a single assertion. `is_system` answers "does this
/// PID belong to the operating system rather than to the user?" — injected so
/// the rules can be unit-tested without live PIDs.
fn verdict_for(
    assertion: &ExternalAssertion,
    ignores_app: &impl Fn(&str) -> bool,
    ignored_once: &HashSet<&str>,
    is_system: &impl Fn(i32) -> bool,
) -> Verdict {
    if is_system(assertion.pid) {
        return Verdict::DropSilently;
    }
    // Type before the ignore lists: a display-only hold was never upgradeable in
    // the first place, so blaming an ignore rule for it would misattribute the
    // reason shown in the menu (VLC on the ignore list still reads
    // "display only", because that is why it isn't upgraded).
    if assertion.assertion_type != UPGRADE_TRIGGER_TYPE.as_str() {
        // The only other observed type (see `OBSERVED_TYPES`).
        return Verdict::Ignore(REASON_DISPLAY_ONLY);
    }
    if ignores_app(&assertion.process_name) {
        return Verdict::Ignore(REASON_USER_IGNORED);
    }
    if ignored_once.contains(assertion.process_name.as_str()) {
        return Verdict::Ignore(REASON_IGNORED_ONCE);
    }
    Verdict::Upgrade
}

/// Split the observed external assertions into the ones worth upgrading and the
/// ones to report as ignored.
pub fn classify_external_assertions(
    all: &[ExternalAssertion],
    ignores_app: &impl Fn(&str) -> bool,
    ignored_once: &HashSet<&str>,
    is_system: &impl Fn(i32) -> bool,
) -> ExternalClassification {
    let mut upgradeable = Vec::new();
    let mut listed = Vec::new();
    for assertion in all {
        match verdict_for(assertion, ignores_app, ignored_once, is_system) {
            Verdict::Upgrade => upgradeable.push(assertion.clone()),
            Verdict::DropSilently => {}
            Verdict::Ignore(reason) => listed.push((assertion, reason)),
        }
    }

    let trigger_names: HashSet<&str> = upgradeable
        .iter()
        .map(|a| a.process_name.as_str())
        .collect();
    let mut ignored: Vec<IgnoredAssertion> = listed
        .into_iter()
        // A nameless holder can't be shown meaningfully, and a process is never
        // listed as ignored when another of its assertions is upgraded.
        .filter(|(a, _)| {
            !a.process_name.is_empty() && !trigger_names.contains(a.process_name.as_str())
        })
        .map(|(a, reason)| IgnoredAssertion {
            process_name: a.process_name.clone(),
            reason: reason.to_string(),
        })
        .collect();
    ignored.sort();
    ignored.dedup();

    ExternalClassification {
        upgradeable,
        ignored,
    }
}

/// Distinct, sorted holder names, so the menu and tooltip stay stable across
/// polls (and don't show the same app twice when it holds several assertions).
/// Nameless holders are dropped: they still count as present, but there is
/// nothing to display.
pub fn holder_names(assertions: &[ExternalAssertion]) -> Vec<String> {
    let mut names: Vec<String> = assertions
        .iter()
        .map(|assertion| assertion.process_name.clone())
        .filter(|name| !name.is_empty())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Assertion constructors shared between this module's tests and the session
/// tests in [`crate::tray::state`].
#[cfg(test)]
pub mod fixtures {
    use super::{AssertionType, ExternalAssertion, IgnoredAssertion};

    /// PID of a holder the injected classifier calls a system program.
    pub const SYSTEM_PID: i32 = 1;
    /// PID of a holder that belongs to the user.
    pub const USER_PID: i32 = 501;

    pub fn assertion(name: &str, type_: AssertionType) -> ExternalAssertion {
        ExternalAssertion {
            pid: USER_PID,
            process_name: name.to_string(),
            assertion_type: type_.as_str().to_string(),
        }
    }

    pub fn system_assertion(name: &str, type_: AssertionType) -> ExternalAssertion {
        ExternalAssertion {
            pid: SYSTEM_PID,
            ..assertion(name, type_)
        }
    }

    pub fn ignored(name: &str, reason: &str) -> IgnoredAssertion {
        IgnoredAssertion {
            process_name: name.to_string(),
            reason: reason.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{SYSTEM_PID, assertion, ignored, system_assertion};
    use super::*;

    /// Classify with no ignore rules beyond the system check, which the tests
    /// drive through the PID rather than live processes.
    fn classify(all: &[ExternalAssertion]) -> ExternalClassification {
        classify_with(all, &|_| false, &HashSet::new())
    }

    fn classify_with(
        all: &[ExternalAssertion],
        ignores_app: &impl Fn(&str) -> bool,
        ignored_once: &HashSet<&str>,
    ) -> ExternalClassification {
        classify_external_assertions(all, ignores_app, ignored_once, &|pid| pid == SYSTEM_PID)
    }

    #[test]
    fn system_idle_holder_is_upgradeable_not_ignored() {
        let all = [assertion(
            "Claude Code",
            AssertionType::PreventUserIdleSystemSleep,
        )];
        let result = classify(&all);
        assert_eq!(result.upgradeable.len(), 1);
        assert_eq!(result.upgradeable[0].process_name, "Claude Code");
        assert!(result.ignored.is_empty());
    }

    /// Every system program is dropped — not just a hardcoded few — whatever it
    /// is called and whichever sleep-relevant assertion it holds.
    #[test]
    fn system_programs_are_neither_triggers_nor_listed() {
        let all = [
            system_assertion("powerd", AssertionType::PreventUserIdleSystemSleep),
            system_assertion("runningboardd", AssertionType::PreventUserIdleSystemSleep),
            system_assertion("coreaudiod", AssertionType::PreventUserIdleSystemSleep),
            system_assertion("Music", AssertionType::PreventUserIdleDisplaySleep),
        ];
        let result = classify(&all);
        // These never trigger an upgrade ...
        assert!(result.upgradeable.is_empty());
        // ... and, being the OS's own business, are dropped from the menu.
        assert!(result.ignored.is_empty());
    }

    #[test]
    fn display_only_holder_is_ignored() {
        let all = [assertion(
            "Safari",
            AssertionType::PreventUserIdleDisplaySleep,
        )];
        let result = classify(&all);
        assert!(result.upgradeable.is_empty());
        assert_eq!(result.ignored, vec![ignored("Safari", REASON_DISPLAY_ONLY)]);
    }

    #[test]
    fn user_ignored_app_is_listed_with_its_own_reason() {
        let all = [assertion("Zoom", AssertionType::PreventUserIdleSystemSleep)];
        let result = classify_with(&all, &|name| name == "Zoom", &HashSet::new());
        assert!(result.upgradeable.is_empty());
        assert_eq!(result.ignored, vec![ignored("Zoom", REASON_USER_IGNORED)]);
    }

    /// A display-only hold was never upgradeable, so an ignore rule is not the
    /// reason it isn't upgraded and must not be shown as one.
    #[test]
    fn display_only_reason_wins_over_the_ignore_lists() {
        let all = [assertion("VLC", AssertionType::PreventUserIdleDisplaySleep)];
        let once: HashSet<&str> = ["VLC"].into_iter().collect();
        assert_eq!(
            classify_with(&all, &|name| name == "VLC", &once).ignored,
            vec![ignored("VLC", REASON_DISPLAY_ONLY)]
        );
    }

    #[test]
    fn ignored_once_holder_is_listed_and_not_upgraded() {
        let all = [assertion(
            "Codex",
            AssertionType::PreventUserIdleSystemSleep,
        )];
        let once: HashSet<&str> = ["Codex"].into_iter().collect();
        let result = classify_with(&all, &|_| false, &once);
        assert!(result.upgradeable.is_empty());
        assert_eq!(result.ignored, vec![ignored("Codex", REASON_IGNORED_ONCE)]);
        // Without the ignore-once entry the same assertion is upgraded again.
        assert_eq!(classify(&all).upgradeable.len(), 1);
    }

    #[test]
    fn nameless_holder_is_dropped_from_ignored_but_still_a_trigger() {
        let all = [
            assertion("", AssertionType::PreventUserIdleSystemSleep),
            assertion("", AssertionType::PreventUserIdleDisplaySleep),
        ];
        let result = classify(&all);
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
            assertion("Zoom", AssertionType::PreventUserIdleSystemSleep),
            assertion("Zoom", AssertionType::PreventUserIdleDisplaySleep),
        ];
        let result = classify(&all);
        assert_eq!(result.upgradeable.len(), 1);
        assert!(result.ignored.is_empty());
    }

    #[test]
    fn ignored_list_is_sorted_and_deduped() {
        let all = [
            assertion("VLC", AssertionType::PreventUserIdleDisplaySleep),
            // A system holder is silently dropped, so it never reaches the list.
            system_assertion("powerd", AssertionType::PreventUserIdleSystemSleep),
            assertion("IINA", AssertionType::PreventUserIdleDisplaySleep),
            // Duplicate display hold from the same app collapses to one entry.
            assertion("VLC", AssertionType::PreventUserIdleDisplaySleep),
        ];
        let result = classify(&all);
        assert_eq!(
            result.ignored,
            vec![
                ignored("IINA", REASON_DISPLAY_ONLY),
                ignored("VLC", REASON_DISPLAY_ONLY),
            ]
        );
    }
}
