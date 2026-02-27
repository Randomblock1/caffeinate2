#![allow(non_upper_case_globals)]
#[cfg(target_os = "macos")]
use objc2_core_foundation::{CFBoolean, CFString, kCFBooleanFalse, kCFBooleanTrue};
#[cfg(target_os = "macos")]
use objc2_io_kit::{
    IOPMAssertionCreateWithName, IOPMAssertionDeclareUserActivity, IOPMAssertionRelease,
    IOPMUserActiveType, kIOPMAssertionLevelOff, kIOPMAssertionLevelOn, kIOReturnBadArgument,
    kIOReturnNotFound, kIOReturnNotPrivileged,
};
#[cfg(target_os = "macos")]
use std::mem::MaybeUninit;
use std::fmt;

// Missing functions from objc2-io-kit
#[cfg(target_os = "macos")]
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMSetSystemPowerSetting(key: &CFString, value: &CFBoolean) -> u32;
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

#[cfg(target_os = "macos")]
pub fn create_assertion(
    assertion_type: AssertionType,
    state: bool,
    verbose: bool,
) -> Result<PowerAssertion, u32> {
    let assertion_name = CFString::from_str("caffeinate2");
    let type_ = CFString::from_str(assertion_type.as_str());
    let level = if state {
        kIOPMAssertionLevelOn
    } else {
        kIOPMAssertionLevelOff
    };
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

#[cfg(not(target_os = "macos"))]
pub fn create_assertion(
    _assertion_type: AssertionType,
    _state: bool,
    verbose: bool,
) -> Result<PowerAssertion, u32> {
    // Mock implementation for non-macOS/testing
    let id = 12345; // Dummy ID
    if verbose {
        println!(
            "Successfully created (MOCKED) power management assertion with ID: {}",
            id
        );
    }
    Ok(PowerAssertion { id, verbose })
}

#[cfg(target_os = "macos")]
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

    #[cfg(test)]
    test_spy::record_release(assertion_id);
}

#[cfg(not(target_os = "macos"))]
fn release_assertion(assertion_id: u32, verbose: bool) {
    if verbose {
        println!(
            "Releasing (MOCKED) power management assertion with ID: {}",
            assertion_id
        );
    }
    #[cfg(test)]
    test_spy::record_release(assertion_id);
}

#[cfg(target_os = "macos")]
pub fn declare_user_activity(state: bool, verbose: bool) -> Result<PowerAssertion, u32> {
    let assertion_name = CFString::from_str("caffeinate2");
    let level = if state {
        kIOPMAssertionLevelOn
    } else {
        kIOPMAssertionLevelOff
    };

    let mut id = MaybeUninit::uninit();

    let level_typed: IOPMUserActiveType = unsafe { std::mem::transmute(level) };

    let status = unsafe {
        IOPMAssertionDeclareUserActivity(Some(&assertion_name), level_typed, id.as_mut_ptr())
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

#[cfg(not(target_os = "macos"))]
pub fn declare_user_activity(_state: bool, verbose: bool) -> Result<PowerAssertion, u32> {
    let id = 67890; // Dummy ID
    if verbose {
        println!("Successfully declared (MOCKED) user activity with ID: {}", id);
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

#[cfg(target_os = "macos")]
pub fn set_sleep_disabled(sleep_disabled: bool, verbose: bool) -> Result<(), u32> {
    let sleep_disabled_bool = if sleep_disabled {
        unsafe { kCFBooleanTrue.unwrap() }
    } else {
        unsafe { kCFBooleanFalse.unwrap() }
    };

    let key = CFString::from_str("SleepDisabled");

    let result = unsafe { IOPMSetSystemPowerSetting(&key, sleep_disabled_bool) };

    if verbose {
        println!(
            "Got result {:X} when {} sleep",
            result,
            if sleep_disabled {
                "disabling"
            } else {
                "enabling"
            }
        );
    }

    if result == 0 {
        Ok(())
    } else if result == kIOReturnNotPrivileged {
        Err(result as u32)
    } else {
        Err(result as u32)
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_sleep_disabled(sleep_disabled: bool, verbose: bool) -> Result<(), u32> {
    if verbose {
        println!(
            "(MOCKED) {} sleep",
            if sleep_disabled {
                "Disabling"
            } else {
                "Enabling"
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_assertion() {
        // Test creating a valid assertion
        let assertion =
            create_assertion(AssertionType::PreventUserIdleSystemSleep, true, true).unwrap();
        // The ID is a u32, usually non-zero if successful, but the function panics on failure.
        // So if we get here, it worked.
        println!("Created assertion with ID: {}", assertion.id);
    }

    #[test]
    fn test_declare_user_activity() {
        let assertion = declare_user_activity(true, true).unwrap();
        println!("Declared user activity with ID: {}", assertion.id);
    }

    #[test]
    fn test_disable_sleep() {
        // This requires root privileges usually, so we expect it to either succeed or fail with kIOReturnNotPrivileged
        match disable_sleep(true) {
            Ok(guard) => {
                println!("Successfully disabled sleep");
                drop(guard); // Should re-enable sleep
            }
            Err(code) => {
                // In our mock, it always succeeds.
                // On macOS it might fail with kIOReturnNotPrivileged if not root.
                // We handle both.
                #[cfg(target_os = "macos")]
                {
                    if code == kIOReturnNotPrivileged {
                        println!(
                            "Insufficient privileges to disable sleep (expected in non-root tests)"
                        );
                    } else {
                        panic!("Failed to disable sleep with unexpected code: {:X}", code);
                    }
                }
                #[cfg(not(target_os = "macos"))]
                {
                    panic!("Failed to disable sleep with unexpected code: {:X}", code);
                }
            }
        }
    }

    #[test]
    fn test_release_assertion_invalid_id() {
        // Releasing an invalid ID should not panic, but print a message if verbose is true
        release_assertion(u32::MAX, true);
    }

    #[test]
    fn test_assertion_lifecycle() {
        let assertion =
            create_assertion(AssertionType::PreventUserIdleSystemSleep, true, false).unwrap();
        let id = assertion.id;
        // Explicitly drop the assertion to trigger release
        drop(assertion);

        // Try to release it again manually. This should not panic and should handle the "already released" case.
        // This verifies that the drop implementation correctly released it, or at least that release_assertion is robust.
        release_assertion(id, true);
    }

    #[test]
    fn test_create_all_known_assertion_types() {
        let types = [
            AssertionType::PreventUserIdleDisplaySleep,
            AssertionType::PreventDiskIdle,
            AssertionType::PreventUserIdleSystemSleep,
            AssertionType::PreventSystemSleep,
        ];

        for assertion_type in types {
            let assertion = create_assertion(assertion_type, true, false).unwrap();
            println!(
                "Successfully created assertion type: {} with ID: {}",
                assertion_type, assertion.id
            );
        }
    }

    #[test]
    fn test_drop_releases_assertion() {
        // Clear any previous state
        crate::power_management::test_spy::reset_spy();

        // Create an assertion
        let assertion =
            create_assertion(AssertionType::PreventUserIdleSystemSleep, true, true).unwrap();
        let id = assertion.id;

        // Verify it hasn't been released yet (sanity check)
        crate::power_management::test_spy::RELEASED_ASSERTIONS.with(|assertions| {
            assert!(!assertions.borrow().contains(&id));
        });

        // Drop the assertion
        drop(assertion);

        // Verify it was released
        crate::power_management::test_spy::assert_released(id);
    }
}

#[cfg(test)]
pub mod test_spy {
    use std::cell::RefCell;

    thread_local! {
        pub static RELEASED_ASSERTIONS: RefCell<Vec<u32>> = RefCell::new(Vec::new());
    }

    pub fn record_release(id: u32) {
        RELEASED_ASSERTIONS.with(|assertions| {
            assertions.borrow_mut().push(id);
        });
    }

    pub fn reset_spy() {
        RELEASED_ASSERTIONS.with(|assertions| {
            assertions.borrow_mut().clear();
        });
    }

    pub fn assert_released(id: u32) {
        RELEASED_ASSERTIONS.with(|assertions| {
            let released = assertions.borrow();
            assert!(
                released.contains(&id),
                "Assertion ID {} was not released. Released IDs: {:?}",
                id,
                *released
            );
        });
    }
}
