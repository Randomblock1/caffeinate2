//! The dialog shown when the tray icon is clicked while the watcher is
//! upgrading another program's sleep assertion.
//!
//! A plain left-click normally toggles caffeinate2's own hold, but the hold in
//! this state belongs to someone else's assertion, so "off" is ambiguous: stop
//! upgrading *this* assertion, stop upgrading *this program* for good, or keep
//! the Mac awake on caffeinate2's own terms instead. The dialog asks, and
//! explains what is currently holding the Mac awake before it does.
//!
//! Unlike the "Wait for apps…" picker, this is a real modal `NSAlert`: it runs
//! its own event loop, so the tray's countdown and the upgrade watcher pause
//! while it is up. That is acceptable for a short, user-initiated question —
//! deadlines are recomputed from the clock afterwards — and it keeps the choice
//! atomic, with no chance of the watcher tearing the session down underneath
//! it. Shutdown is the one thing that must not wait on an answer: a modal
//! run loop would otherwise keep the process, and its sleep assertions, alive
//! until somebody clicked a button, so [`modal_ticker`] ends the dialog as soon
//! as a signal arrives.

use crate::sleep::sleep_mode::SleepMode;
use crate::tray::app;
use crate::util::duration_parser::format_countdown_minutes;
use block2::RcBlock;
use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationOptions,
    NSApplicationActivationPolicy, NSModalPanelRunLoopMode, NSModalResponse, NSModalResponseAbort,
    NSPopUpButton, NSRunningApplication, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSPoint, NSRect, NSRunLoop, NSSize, NSString, NSTimer,
};
use std::cell::Cell;
use std::ptr::NonNull;
use std::time::Duration;

/// What the user chose. Every variant except [`UpgradeChoice::KeepUpgrading`]
/// changes what the watcher does next; the app-scoped ones carry the program the
/// choice applies to (the one selected in the dialog when several are upgraded).
pub enum UpgradeChoice {
    /// Dismissed: leave the upgrade exactly as it is.
    KeepUpgrading,
    /// Stop upgrading this program's current assertion.
    IgnoreAssertion(String),
    /// Never upgrade this program again (until it is removed from the list).
    IgnoreApp(String),
    /// Replace the upgrade with caffeinate2's own session (configured mode and
    /// time limit).
    TakeOver,
}

const POPUP_WIDTH: f64 = 260.0;
const POPUP_HEIGHT: f64 = 25.0;

/// How often the modal checks for a shutdown request. Fast enough that `pkill`
/// on a tray with an open dialog feels immediate, slow enough to be free.
const MODAL_TICK: Duration = Duration::from_millis(150);

/// Response code [`modal_ticker`] ends the modal with when the process is asked
/// to quit. It is not `NSAlertFirstButtonReturn + n` for any button, so the
/// choice mapping in [`ask`] already reads it as "changed nothing".
const SHUTDOWN_RESPONSE: NSModalResponse = NSModalResponseAbort;

/// Ask what to do about the assertion(s) being upgraded. `apps` must be
/// non-empty; with several, the dialog carries a picker for which one the
/// ignore choices apply to. `mode` and `time_limit_secs` are the configured
/// session settings a take-over would run under, named in the dialog so the
/// consequence is visible before clicking.
pub fn ask(
    mtm: MainThreadMarker,
    apps: &[String],
    mode: SleepMode,
    time_limit_secs: Option<u64>,
) -> UpgradeChoice {
    let Some(first) = apps.first() else {
        return UpgradeChoice::KeepUpgrading;
    };
    // Never put a modal up on the way out; the run loop is about to tear the
    // session down.
    if app::shutdown_requested() {
        return UpgradeChoice::KeepUpgrading;
    }

    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    alert.setMessageText(&NSString::from_str(&message_text(apps)));
    alert.setInformativeText(&NSString::from_str(&informative_text(
        apps,
        mode,
        time_limit_secs,
    )));

    // The buttons read right-to-left on screen in the order they are added, so
    // the first one is both rightmost and the Return default. "Cancel" is
    // recognized by AppKit and picks up Escape on its own.
    alert.addButtonWithTitle(&NSString::from_str(&take_over_title(time_limit_secs)));
    alert.addButtonWithTitle(&NSString::from_str(IGNORE_ONCE_TITLE));
    alert.addButtonWithTitle(&NSString::from_str(never_upgrade_title(apps)));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));

    // With one upgraded program there is nothing to choose between; with
    // several, the ignore choices need to know which one they apply to.
    let picker = (apps.len() > 1).then(|| {
        let popup = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(POPUP_WIDTH, POPUP_HEIGHT),
            ),
            false,
        );
        let titles: Vec<Retained<NSString>> =
            apps.iter().map(|app| NSString::from_str(app)).collect();
        popup.addItemsWithTitles(&NSArray::from_retained_slice(&titles));
        alert.setAccessoryView(Some(&popup));
        popup
    });

    let response = run_modal(mtm, &alert);

    let selected = picker.map_or_else(
        || first.clone(),
        |popup| {
            let index = usize::try_from(popup.indexOfSelectedItem()).unwrap_or(0);
            apps.get(index).unwrap_or(first).clone()
        },
    );

    match response - NSAlertFirstButtonReturn {
        0 => UpgradeChoice::TakeOver,
        1 => UpgradeChoice::IgnoreAssertion(selected),
        2 => UpgradeChoice::IgnoreApp(selected),
        // Cancel, a closed window, or the shutdown teardown: change nothing.
        _ => UpgradeChoice::KeepUpgrading,
    }
}

/// Run the alert with the app temporarily promoted to a regular (foreground)
/// app, so a menu-bar-only process actually gets a focused, frontmost dialog,
/// then hand activation back the way the picker window does — flipping Regular
/// back to Accessory while frontmost otherwise strands keyboard focus.
fn run_modal(mtm: MainThreadMarker, alert: &NSAlert) -> NSModalResponse {
    let app = NSApplication::sharedApplication(mtm);
    let previous: Option<Retained<NSRunningApplication>> =
        NSWorkspace::sharedWorkspace().frontmostApplication();
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    let ticker = modal_ticker();
    let response = alert.runModal();
    ticker.invalidate();

    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    if let Some(previous) = previous
        && !previous.isTerminated()
    {
        previous.activateWithOptions(NSApplicationActivationOptions::empty());
    }
    crate::tray::macos_activation::wake_event_loop();
    response
}

/// A repeating timer registered in the *modal* run loop mode — the only mode
/// running while the alert is up, so a `scheduledTimer` (default mode) would
/// never fire. Caller must `invalidate` it once the modal returns. It does two
/// jobs, both of which have to happen from inside the modal loop:
///
/// * Puts the panel in front on its first tick. `runModal` orders the window
///   front within *this app's* window layer, which leaves it behind whatever is
///   active when macOS 14+ declines the activation request in [`run_modal`] —
///   activation is cooperative there, the same hazard
///   `wait_window::activate_and_order_front` works around. Only
///   `orderFrontRegardless` ignores app order, and it has to run after
///   `runModal` has done its own ordering to stick.
/// * Ends the modal when the process is asked to quit, so a SIGINT/SIGTERM that
///   arrives while the dialog is open still reaches the run loop's teardown
///   (which releases the sleep holds) instead of waiting on an answer.
fn modal_ticker() -> Retained<NSTimer> {
    let raised = Cell::new(false);
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        if !raised.replace(true)
            && let Some(window) = app.modalWindow()
        {
            window.orderFrontRegardless();
        }
        if app::shutdown_requested() {
            app.stopModalWithCode(SHUTDOWN_RESPONSE);
            // `stopModal` only takes effect when the modal loop next comes
            // round; give it an event to come round for.
            crate::tray::macos_activation::wake_event_loop();
        }
    });
    // SAFETY: the block captures only a `Cell<bool>` and re-derives everything
    // else from `MainThreadMarker::new()`, so it is sound to invoke from any
    // thread — and the timer is installed on the main thread's run loop, which
    // is the only place it ever runs.
    let timer = unsafe {
        NSTimer::timerWithTimeInterval_repeats_block(MODAL_TICK.as_secs_f64(), true, &block)
    };
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSModalPanelRunLoopMode) };
    timer
}

const IGNORE_ONCE_TITLE: &str = "Ignore This Time";

/// Label for the "add to the ignore list" button. With several programs
/// upgraded it acts on the pop-up selection, and has to say so.
fn never_upgrade_title(apps: &[String]) -> &'static str {
    if apps.len() == 1 {
        "Never Upgrade This App"
    } else {
        "Never Upgrade Selected App"
    }
}

/// Label for the take-over button, naming the limit it will run under so the
/// consequence is visible before clicking.
fn take_over_title(time_limit_secs: Option<u64>) -> String {
    time_limit_secs.map_or_else(
        || "Hold Until I Stop It".to_string(),
        |secs| format!("Hold For {}", format_countdown_minutes(secs)),
    )
}

fn message_text(apps: &[String]) -> String {
    match apps {
        [one] => format!("caffeinate2 is upgrading sleep prevention for {one}"),
        many => format!(
            "caffeinate2 is upgrading sleep prevention for {} apps",
            many.len()
        ),
    }
}

/// The body of the dialog: what is holding the Mac awake, what caffeinate2 did
/// about it, and what each button does about that. Long on purpose — this is
/// the only place the upgrade mechanism is explained inside the app, and it is
/// shown precisely when the user is asking "why won't this turn off?".
fn informative_text(apps: &[String], mode: SleepMode, time_limit_secs: Option<u64>) -> String {
    let mut text = match apps {
        [one] => format!(
            "{one} is holding an idle-sleep assertion: it stops the Mac going to sleep on its \
             own, but still lets it sleep the moment the lid closes.\n\n"
        ),
        many => format!(
            "These {} programs are holding idle-sleep assertions, which stop the Mac going to \
             sleep on its own but still let it sleep the moment the lid closes:\n\n{}\n\n",
            many.len(),
            many.iter()
                .map(|app| format!("    {app}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };
    let subject = match apps {
        [one] => one.clone(),
        _ => "the last of them".to_string(),
    };
    text.push_str(&format!(
        "caffeinate2 upgraded that to Entirely mode so the Mac stays awake with the lid closed \
         too, and releases it about 20 seconds after {subject} stops asking.\n\n"
    ));
    // The take-over bullet has to describe the end condition it actually gets:
    // a configured limit ends the hold on a clock, no limit ends it only when
    // the user says so. Both differ from the upgrade, which ends on whatever
    // the other program decides.
    let ends = if time_limit_secs.is_some() {
        "ending on that schedule instead of when the other program stops asking"
    } else {
        "staying on until you switch it off instead of ending when the other program stops asking"
    };
    text.push_str(&format!(
        "• {} — hold on caffeinate2's own terms in {} mode, {ends}.\n\
         • {IGNORE_ONCE_TITLE} — stop upgrading the hold it has right now. It keeps its own \
         weaker assertion, so closing the lid puts the Mac to sleep again.\n\
         • {} — the same, and never upgrade it again; undo that under Ignored apps in the menu.\n\
         • Cancel — leave the upgrade running.",
        take_over_title(time_limit_secs),
        mode.label(),
        never_upgrade_title(apps),
    ));
    if apps.len() > 1 {
        text.push_str("\n\nThe two ignore choices apply to the app selected below.");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn take_over_title_names_the_configured_limit() {
        assert_eq!(take_over_title(Some(1800)), "Hold For 30m");
        assert_eq!(take_over_title(Some(2 * 60 * 60)), "Hold For 2h");
        assert_eq!(take_over_title(None), "Hold Until I Stop It");
    }

    #[test]
    fn message_text_names_one_app_and_counts_several() {
        assert_eq!(
            message_text(&apps(&["Claude"])),
            "caffeinate2 is upgrading sleep prevention for Claude"
        );
        assert_eq!(
            message_text(&apps(&["Claude", "Codex"])),
            "caffeinate2 is upgrading sleep prevention for 2 apps"
        );
    }

    /// The body has to explain the situation, not just label the buttons: what
    /// is holding the Mac awake, what caffeinate2 did, and what each button
    /// does — with button names matching the buttons exactly.
    #[test]
    fn informative_text_explains_the_situation_and_every_button() {
        let text = informative_text(&apps(&["Claude"]), SleepMode::System, Some(1800));
        assert!(text.starts_with("Claude is holding an idle-sleep assertion"));
        assert!(text.contains("lid closes"));
        assert!(text.contains("upgraded that to Entirely mode"));
        assert!(text.contains("about 20 seconds after Claude stops asking"));
        assert!(text.contains("Hold For 30m"));
        assert!(text.contains("in System mode, ending on that schedule"));
        assert!(text.contains(IGNORE_ONCE_TITLE));
        assert!(text.contains("Never Upgrade This App"));
        assert!(text.contains("Cancel"));
        // Nothing to choose between with one app, so no pop-up to explain.
        assert!(!text.contains("selected below"));
    }

    /// With several holders the body names each one (the pop-up alone would
    /// make the user click through to find out who is involved) and says what
    /// the pop-up is for.
    #[test]
    fn informative_text_lists_every_holder_and_explains_the_popup() {
        let text = informative_text(
            &apps(&["Claude", "Codex", "node"]),
            SleepMode::Entirely,
            None,
        );
        assert!(text.starts_with("These 3 programs are holding idle-sleep assertions"));
        assert!(text.contains("    Claude\n    Codex\n    node"));
        assert!(text.contains("about 20 seconds after the last of them stops asking"));
        assert!(text.contains("Hold Until I Stop It"));
        assert!(text.contains("staying on until you switch it off"));
        assert!(text.contains("Never Upgrade Selected App"));
        assert!(text.ends_with("The two ignore choices apply to the app selected below."));
    }

    #[test]
    fn never_upgrade_title_scopes_itself_to_the_selection_only_when_there_is_one() {
        assert_eq!(
            never_upgrade_title(&apps(&["Claude"])),
            "Never Upgrade This App"
        );
        assert_eq!(
            never_upgrade_title(&apps(&["Claude", "Codex"])),
            "Never Upgrade Selected App"
        );
    }
}
