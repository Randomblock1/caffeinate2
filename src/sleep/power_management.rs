#![allow(non_upper_case_globals)]
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, CFType,
    kCFBooleanFalse, kCFBooleanTrue,
};
use objc2_io_kit::{
    IOPMAssertionCreateWithName, IOPMAssertionDeclareUserActivity, IOPMAssertionRelease,
    IOPMCopyAssertionsByProcess, IOPMUserActiveType, kIOPMAssertionLevelOn, kIOReturnBadArgument,
    kIOReturnNotFound,
};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::{fmt, mem::MaybeUninit};

// `IOPMSetSystemPowerSetting` is not exposed by objc2-io-kit, so it is declared
// here by hand. It is a stable public IOKit C entry point (it is what
// `pmset disablesleep` ultimately drives); the signature matches the SDK
// header (`CFStringRef`, `CFBooleanRef`, returning `IOReturn`/`i32`). Toggling
// it mutates a system-wide power setting and needs root, so it has no unit
// coverage; `EntirelyCoordinator` owns its lifecycle in production.
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMSetSystemPowerSetting(key: &CFString, value: &CFBoolean) -> i32;
}

#[derive(Copy, Clone)]
pub enum AssertionType {
    PreventUserIdleDisplaySleep,
    PreventDiskIdle,
    PreventUserIdleSystemSleep,
    PreventSystemSleep,
}

impl AssertionType {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::PreventUserIdleDisplaySleep => "PreventUserIdleDisplaySleep",
            Self::PreventDiskIdle => "PreventDiskIdle",
            Self::PreventUserIdleSystemSleep => "PreventUserIdleSystemSleep",
            Self::PreventSystemSleep => "PreventSystemSleep",
        }
    }
}

impl fmt::Display for AssertionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub struct PowerAssertion {
    id: u32,
    verbose: bool,
}

impl Drop for PowerAssertion {
    fn drop(&mut self) {
        release_assertion(self.id, self.verbose);
    }
}

///
/// # Errors
///
/// Returns an `IOKit` error code if the assertion cannot be created.
pub fn create_assertion(
    assertion_type: AssertionType,
    verbose: bool,
) -> Result<PowerAssertion, u32> {
    let assertion_name = CFString::from_str("caffeinate2");
    let type_ = CFString::from_str(assertion_type.as_str());
    let level = kIOPMAssertionLevelOn;
    let mut id = MaybeUninit::uninit();

    let status = unsafe {
        IOPMAssertionCreateWithName(Some(&type_), level, Some(&assertion_name), id.as_mut_ptr())
    };

    if status == 0 {
        let id = unsafe { id.assume_init() };
        if verbose {
            tracing::debug!("Successfully created power management assertion with ID: {id}");
        }
        Ok(PowerAssertion { id, verbose })
    } else {
        Err(status.cast_unsigned())
    }
}

fn release_assertion(assertion_id: u32, verbose: bool) {
    if verbose {
        tracing::debug!("Releasing power management assertion with ID: {assertion_id}");
    }

    let status = IOPMAssertionRelease(assertion_id).cast_unsigned();

    match status {
        0 => {
            if verbose {
                tracing::debug!(
                    "Successfully released power management assertion with ID: {assertion_id}"
                );
            }
        }
        kIOReturnNotFound => {
            if verbose {
                tracing::debug!("Assertion {assertion_id} already released");
            }
        }
        kIOReturnBadArgument => {
            if verbose {
                tracing::debug!("Assertion {assertion_id} was invalid");
            }
        }
        _ => {
            tracing::warn!("Failed to release power management assertion with code: {status:X}");
        }
    }
}

/// How often to re-assert user activity for the lifetime of a `--user-active`
/// hold. `IOPMAssertionDeclareUserActivity` marks the user active "now" and the
/// effect lapses after the system idle timer elapses, so a long-lived hold must
/// periodically refresh it (the classic `caffeinate -u` behavior) or sleep
/// prevention silently stops. A 30s cadence is comfortably below any idle-sleep
/// timeout.
const USER_ACTIVITY_REFRESH: Duration = Duration::from_secs(30);

/// Declare user activity once, returning the assertion id. Re-declaring with
/// the same id (in/out parameter) refreshes that assertion instead of leaking a
/// new one.
fn declare_user_activity_once(id: &mut u32, verbose: bool) -> Result<(), u32> {
    let assertion_name = CFString::from_str("caffeinate2");
    // Declaring activity is inherently "active now"; the only choice is the
    // activity type, and Local means a user is physically at this machine.
    let status = unsafe {
        IOPMAssertionDeclareUserActivity(
            Some(&assertion_name),
            IOPMUserActiveType::Local,
            std::ptr::from_mut(id),
        )
    };
    if status == 0 {
        if verbose {
            tracing::debug!("Successfully declared user activity with ID: {id}");
        }
        Ok(())
    } else {
        Err(status.cast_unsigned())
    }
}

/// A `--user-active` hold: an `IOPMAssertionDeclareUserActivity` assertion plus
/// a background thread that re-declares it on a timer (see
/// [`USER_ACTIVITY_REFRESH`]) so it does not lapse on long runs. Dropping the
/// hold stops the refresher and releases the assertion.
pub struct UserActivityHold {
    /// Shared with the refresher thread, which rewrites it after each
    /// re-declare. `IOPMAssertionDeclareUserActivity` is documented to reuse the
    /// same id, but it takes the id as an in/out parameter and could in
    /// principle return a different one; reading the live value here means
    /// `Drop` always releases the assertion the refresher last touched rather
    /// than a stale initial id (which would leak the live assertion).
    id: Arc<AtomicU32>,
    verbose: bool,
    stop_tx: Option<mpsc::Sender<()>>,
    refresher: Option<JoinHandle<()>>,
}

impl Drop for UserActivityHold {
    fn drop(&mut self) {
        // Stop the refresher first so it can never re-declare after release.
        // Dropping the sender wakes the timer's blocking recv immediately.
        self.stop_tx.take();
        if let Some(handle) = self.refresher.take() {
            let _ = handle.join();
        }
        release_assertion(self.id.load(Ordering::SeqCst), self.verbose);
    }
}

///
/// # Errors
///
/// Returns an `IOKit` error code if user activity cannot be declared.
pub fn declare_user_activity(verbose: bool) -> Result<UserActivityHold, u32> {
    let mut initial_id = 0u32;
    declare_user_activity_once(&mut initial_id, verbose)?;
    let id = Arc::new(AtomicU32::new(initial_id));

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let thread_id = Arc::clone(&id);
    let refresher = thread::spawn(move || {
        let mut current = thread_id.load(Ordering::SeqCst);
        loop {
            match stop_rx.recv_timeout(USER_ACTIVITY_REFRESH) {
                // The hold was dropped (sender gone) or explicitly signalled.
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(code) = declare_user_activity_once(&mut current, verbose) {
                        tracing::warn!("Failed to refresh user activity assertion: {code:X}");
                        // A failed declare may have overwritten `current` (IOKit
                        // takes the id as an in/out parameter); restore the
                        // last-known-good id so the next refresh reuses it rather
                        // than seeding a fresh, never-released assertion.
                        current = thread_id.load(Ordering::SeqCst);
                    } else {
                        // Publish the (possibly updated) id so Drop releases it.
                        thread_id.store(current, Ordering::SeqCst);
                    }
                }
            }
        }
    });

    Ok(UserActivityHold {
        id,
        verbose,
        stop_tx: Some(stop_tx),
        refresher: Some(refresher),
    })
}

///
/// # Errors
///
/// Returns an `IOKit` error code if the sleep setting cannot be changed, or
/// [`kIOReturnBadArgument`] if the Core Foundation boolean constants are
/// unavailable (should never happen on a healthy system).
pub fn set_sleep_disabled(sleep_disabled: bool, verbose: bool) -> Result<(), u32> {
    let sleep_disabled_bool = if sleep_disabled {
        unsafe { kCFBooleanTrue }
    } else {
        unsafe { kCFBooleanFalse }
    };
    let Some(sleep_disabled_bool) = sleep_disabled_bool else {
        return Err(kIOReturnBadArgument);
    };

    let key = CFString::from_str("SleepDisabled");

    let result = unsafe { IOPMSetSystemPowerSetting(&key, sleep_disabled_bool) };

    let code = result.cast_unsigned();
    if verbose {
        tracing::debug!(
            "Got result {:X} when {} sleep",
            code,
            if sleep_disabled {
                "disabling"
            } else {
                "enabling"
            }
        );
    }

    if result == 0 { Ok(()) } else { Err(code) }
}

// Per-assertion dictionary keys used by `IOPMCopyAssertionsByProcess`. These
// are stable IOKit strings that objc2-io-kit does not export as constants.
// Note these differ from the public `kIOPMAssertionTypeKey`/`...LevelKey`
// names ("AssertionType"/"AssertionLevel") — the by-process dictionary uses
// the shorter internal keys, confirmed against live `pmset -g assertions`.
//
// `AssertionTrueType` is the normalized type: a process can register under a
// legacy alias (e.g. Electron uses "NoIdleSleepAssertion") whose true type is
// "PreventUserIdleSystemSleep". Matching the true type catches every form.
const ASSERTION_TRUE_TYPE_KEY: &str = "AssertionTrueType";
const ASSERTION_TYPE_KEY: &str = "AssertType";
const ASSERTION_LEVEL_KEY: &str = "AssertLevel";
const ASSERTION_PROCESS_NAME_KEY: &str = "Process Name";

/// A sleep-preventing power assertion held by another process.
#[derive(Debug, Clone)]
pub struct ExternalAssertion {
    /// PID of the holder. The tray resolves it to an executable path to tell a
    /// system daemon from a program the user launched; process names alone are
    /// ambiguous (and absent for some holders).
    pub pid: i32,
    pub process_name: String,
    pub assertion_type: String,
}

/// Active assertions of the given types held by processes *other than this one*.
///
/// The tray's "upgrade external wakefulness" watcher uses this to notice when
/// something else (a coding agent, `caffeinate -i`, …) is holding a low-level
/// assertion that still allows lid-close sleep, so it can take a stronger hold.
/// Our own PID is filtered out so the watcher can never react to itself.
///
/// # Errors
///
/// Returns an `IOKit` error code if assertions cannot be queried.
pub fn external_assertions(types: &[AssertionType]) -> Result<Vec<ExternalAssertion>, u32> {
    let self_pid = std::process::id().cast_signed();

    let mut raw: *const CFDictionary = std::ptr::null();
    let status = unsafe { IOPMCopyAssertionsByProcess(&raw mut raw) };
    if status != 0 {
        return Err(status.cast_unsigned());
    }
    let Some(raw) = NonNull::new(raw.cast_mut()) else {
        // No process holds any assertion.
        return Ok(Vec::new());
    };
    // IOPMCopyAssertionsByProcess returns a +1 reference; CFRetained releases
    // it on drop, as the API requires.
    let by_pid = unsafe { CFRetained::from_raw(raw) };

    let true_type_key = CFString::from_str(ASSERTION_TRUE_TYPE_KEY);
    let type_key = CFString::from_str(ASSERTION_TYPE_KEY);
    let level_key = CFString::from_str(ASSERTION_LEVEL_KEY);
    let name_key = CFString::from_str(ASSERTION_PROCESS_NAME_KEY);

    // Top level: keys are pid CFNumbers, values are CFArrays of assertion dicts.
    let count = by_pid.count().max(0).cast_unsigned();
    let mut keys: Vec<*const c_void> = vec![std::ptr::null(); count];
    let mut values: Vec<*const c_void> = vec![std::ptr::null(); count];
    unsafe {
        by_pid.keys_and_values(keys.as_mut_ptr(), values.as_mut_ptr());
    }

    let mut found = Vec::new();
    for (key_ptr, value_ptr) in keys.into_iter().zip(values) {
        let (Some(key), Some(value)) = (cf_ref(&by_pid, key_ptr), cf_ref(&by_pid, value_ptr))
        else {
            continue;
        };
        let Some(pid) = key.downcast_ref::<CFNumber>().and_then(cf_number_i32) else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Some(list) = value.downcast_ref::<CFArray>() else {
            continue;
        };
        for i in 0..list.count() {
            let Some(item) = cf_ref(list, unsafe { list.value_at_index(i) }) else {
                continue;
            };
            let Some(dict) = item.downcast_ref::<CFDictionary>() else {
                continue;
            };
            // Prefer the normalized true type; fall back to the raw type.
            let Some(assertion_type) =
                dict_string(dict, &true_type_key).or_else(|| dict_string(dict, &type_key))
            else {
                continue;
            };
            if !types.iter().any(|t| t.as_str() == assertion_type) {
                continue;
            }
            // Skip assertions toggled off (level 0); only count active ones.
            if dict_i32(dict, &level_key) == Some(0) {
                continue;
            }
            found.push(ExternalAssertion {
                pid,
                process_name: dict_string(dict, &name_key).unwrap_or_default(),
                assertion_type,
            });
        }
    }

    Ok(found)
}

/// Borrow a CoreFoundation object from a raw pointer returned by a CF getter
/// (null → `None`). CF getters return non-owning references, so the borrow is
/// only valid while the owning collection is alive. `owner` carries that
/// collection's lifetime into the returned reference so the borrow checker
/// enforces the invariant instead of leaving it to convention. `owner` is the
/// only reference input, so lifetime elision ties the returned borrow to it.
fn cf_ref<T: ?Sized>(owner: &T, ptr: *const c_void) -> Option<&CFType> {
    let _ = owner;
    NonNull::new(ptr.cast_mut()).map(|p| unsafe { p.cast::<CFType>().as_ref() })
}

fn cf_number_i32(number: &CFNumber) -> Option<i32> {
    let mut value: i32 = 0;
    let ok = unsafe {
        number.value(
            CFNumberType::SInt32Type,
            std::ptr::from_mut(&mut value).cast::<c_void>(),
        )
    };
    ok.then_some(value)
}

fn dict_string(dict: &CFDictionary, key: &CFString) -> Option<String> {
    let value = cf_ref(dict, unsafe { dict.value(std::ptr::from_ref(key).cast()) })?;
    value
        .downcast_ref::<CFString>()
        .map(std::string::ToString::to_string)
}

fn dict_i32(dict: &CFDictionary, key: &CFString) -> Option<i32> {
    let value = cf_ref(dict, unsafe { dict.value(std::ptr::from_ref(key).cast()) })?;
    value.downcast_ref::<CFNumber>().and_then(cf_number_i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_assertion_type_names_match_iokit_names() {
        let cases = [
            (
                AssertionType::PreventUserIdleDisplaySleep,
                "PreventUserIdleDisplaySleep",
            ),
            (AssertionType::PreventDiskIdle, "PreventDiskIdle"),
            (
                AssertionType::PreventUserIdleSystemSleep,
                "PreventUserIdleSystemSleep",
            ),
            (AssertionType::PreventSystemSleep, "PreventSystemSleep"),
        ];

        for (assertion_type, expected) in cases {
            assert_eq!(assertion_type.as_str(), expected);
            assert_eq!(assertion_type.to_string(), expected);
        }
    }

    #[test]
    #[ignore = "creates real IOKit power assertions"]
    fn smoke_create_all_known_assertion_types() {
        let types = [
            AssertionType::PreventUserIdleDisplaySleep,
            AssertionType::PreventDiskIdle,
            AssertionType::PreventUserIdleSystemSleep,
            AssertionType::PreventSystemSleep,
        ];

        for assertion_type in types {
            let assertion = create_assertion(assertion_type, false).unwrap();
            // IOKit assertion ids are non-zero; zero would mean the create
            // "succeeded" without actually registering anything.
            assert_ne!(assertion.id, 0, "{assertion_type}");
            println!(
                "Successfully created assertion type: {} with ID: {}",
                assertion_type, assertion.id
            );
        }
    }

    #[test]
    #[ignore = "declares real user activity through IOKit"]
    fn smoke_declare_user_activity() {
        let assertion = declare_user_activity(true).unwrap();
        let id = assertion.id.load(Ordering::SeqCst);
        assert_ne!(id, 0, "a declared user-activity assertion has a real id");
        println!("Declared user activity with ID: {id}");
    }

    #[test]
    #[ignore = "enumerates live IOKit assertions from other processes"]
    fn smoke_external_assertions() {
        // Hold an assertion ourselves; our own PID is excluded inside
        // `external_assertions`, so only other processes' holds come back —
        // which makes the self-exclusion assertable even when nothing else on
        // the machine is holding.
        let _held = create_assertion(AssertionType::PreventUserIdleSystemSleep, false).unwrap();
        let externals = external_assertions(&[AssertionType::PreventUserIdleSystemSleep]).unwrap();
        let self_pid = std::process::id().cast_signed();
        assert!(
            externals.iter().all(|assertion| assertion.pid != self_pid),
            "our own assertion must be filtered out"
        );
        for assertion in &externals {
            assert_eq!(
                assertion.assertion_type,
                AssertionType::PreventUserIdleSystemSleep.as_str()
            );
            println!(
                "external assertion: pid={} name={} type={}",
                assertion.pid, assertion.process_name, assertion.assertion_type
            );
        }
    }
}
