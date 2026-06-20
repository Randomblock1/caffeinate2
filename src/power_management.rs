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
use std::{fmt, mem::MaybeUninit};

// Missing functions from objc2-io-kit
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
    pub fn as_str(&self) -> &'static str {
        match self {
            AssertionType::PreventUserIdleDisplaySleep => "PreventUserIdleDisplaySleep",
            AssertionType::PreventDiskIdle => "PreventDiskIdle",
            AssertionType::PreventUserIdleSystemSleep => "PreventUserIdleSystemSleep",
            AssertionType::PreventSystemSleep => "PreventSystemSleep",
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
            println!(
                "Successfully created power management assertion with ID: {}",
                id
            );
        }
        Ok(PowerAssertion { id, verbose })
    } else {
        Err(status as u32)
    }
}

fn release_assertion(assertion_id: u32, verbose: bool) {
    if verbose {
        println!(
            "Releasing power management assertion with ID: {}",
            assertion_id
        );
    }

    let status = IOPMAssertionRelease(assertion_id) as u32;

    match status {
        0 => {
            if verbose {
                println!(
                    "Successfully released power management assertion with ID: {}",
                    assertion_id
                );
            }
        }
        kIOReturnNotFound => {
            if verbose {
                println!("Assertion {} already released", assertion_id);
            }
        }
        kIOReturnBadArgument => {
            if verbose {
                println!("Assertion {} was invalid", assertion_id);
            }
        }
        _ => {
            eprintln!(
                "Failed to release power management assertion with code: {:X}",
                status
            );
        }
    }
}

pub fn declare_user_activity(verbose: bool) -> Result<PowerAssertion, u32> {
    let assertion_name = CFString::from_str("caffeinate2");
    let mut id = MaybeUninit::uninit();

    // Declaring activity is inherently "active now"; the only choice is the
    // activity type, and Local means a user is physically at this machine.
    let status = unsafe {
        IOPMAssertionDeclareUserActivity(
            Some(&assertion_name),
            IOPMUserActiveType::Local,
            id.as_mut_ptr(),
        )
    };
    if status != 0 {
        return Err(status as u32);
    }

    let id = unsafe { id.assume_init() };

    if verbose {
        println!("Successfully declared user activity with ID: {}", id);
    }

    Ok(PowerAssertion { id, verbose })
}

pub struct SleepDisabledGuard {
    verbose: bool,
}

impl Drop for SleepDisabledGuard {
    fn drop(&mut self) {
        let _ = set_sleep_disabled(false, self.verbose);
    }
}

pub fn disable_sleep(verbose: bool) -> Result<SleepDisabledGuard, u32> {
    set_sleep_disabled(true, verbose)?;
    Ok(SleepDisabledGuard { verbose })
}

pub fn set_sleep_disabled(sleep_disabled: bool, verbose: bool) -> Result<(), u32> {
    let sleep_disabled_bool = if sleep_disabled {
        unsafe { kCFBooleanTrue.unwrap() }
    } else {
        unsafe { kCFBooleanFalse.unwrap() }
    };

    let key = CFString::from_str("SleepDisabled");

    let result = unsafe { IOPMSetSystemPowerSetting(&key, sleep_disabled_bool) };

    let code = result as u32;
    if verbose {
        println!(
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
pub fn external_assertions(types: &[AssertionType]) -> Result<Vec<ExternalAssertion>, u32> {
    let self_pid = std::process::id() as i32;

    let mut raw: *const CFDictionary = std::ptr::null();
    let status = unsafe { IOPMCopyAssertionsByProcess(&mut raw) };
    if status != 0 {
        return Err(status as u32);
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
    let count = by_pid.count().max(0) as usize;
    let mut keys: Vec<*const c_void> = vec![std::ptr::null(); count];
    let mut values: Vec<*const c_void> = vec![std::ptr::null(); count];
    unsafe {
        by_pid.keys_and_values(keys.as_mut_ptr(), values.as_mut_ptr());
    }

    let mut found = Vec::new();
    for (key_ptr, value_ptr) in keys.into_iter().zip(values) {
        let (Some(key), Some(value)) = (cf_ref(key_ptr), cf_ref(value_ptr)) else {
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
            let Some(item) = cf_ref(unsafe { list.value_at_index(i) }) else {
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
/// valid only while the owning collection is still alive.
fn cf_ref<'a>(ptr: *const c_void) -> Option<&'a CFType> {
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
    let value = cf_ref(unsafe { dict.value(std::ptr::from_ref(key).cast()) })?;
    value.downcast_ref::<CFString>().map(|s| s.to_string())
}

fn dict_i32(dict: &CFDictionary, key: &CFString) -> Option<i32> {
    let value = cf_ref(unsafe { dict.value(std::ptr::from_ref(key).cast()) })?;
    value.downcast_ref::<CFNumber>().and_then(cf_number_i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_io_kit::kIOReturnNotPrivileged;

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
        println!("Declared user activity with ID: {}", assertion.id);
    }

    #[test]
    #[ignore = "changes the system SleepDisabled power setting"]
    fn smoke_disable_sleep() {
        match disable_sleep(true) {
            Ok(guard) => {
                println!("Successfully disabled sleep");
                drop(guard);
            }
            Err(code) => {
                if code == kIOReturnNotPrivileged {
                    println!(
                        "Insufficient privileges to disable sleep (expected in non-root tests)"
                    );
                } else {
                    panic!("Failed to disable sleep with unexpected code: {:X}", code);
                }
            }
        }
    }

    #[test]
    #[ignore = "calls IOKit with an invalid assertion id"]
    fn smoke_release_assertion_invalid_id() {
        release_assertion(u32::MAX, true);
    }

    #[test]
    #[ignore = "enumerates live IOKit assertions from other processes"]
    fn smoke_external_assertions() {
        // Hold an assertion ourselves and confirm it is filtered out (same PID).
        let _held = create_assertion(AssertionType::PreventUserIdleSystemSleep, false).unwrap();
        let externals = external_assertions(&[AssertionType::PreventUserIdleSystemSleep]).unwrap();
        let self_pid = std::process::id() as i32;
        for assertion in &externals {
            assert_ne!(assertion.pid, self_pid, "self PID must be excluded");
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
