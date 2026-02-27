#![allow(non_upper_case_globals)]
use objc2_core_foundation::{CFBoolean, CFString, kCFBooleanFalse, kCFBooleanTrue};
use objc2_io_kit::{
    IOPMAssertionCreateWithName, IOPMAssertionDeclareUserActivity, IOPMAssertionRelease,
    IOPMUserActiveType, kIOPMAssertionLevelOff, kIOPMAssertionLevelOn, kIOReturnBadArgument,
    kIOReturnNotFound, kIOReturnNotPrivileged,
};
use std::{fmt, mem::MaybeUninit};

// Missing functions from objc2-io-kit
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
    test_mode: bool,
}

impl PowerAssertion {
    pub fn new_test(id: u32, verbose: bool) -> Self {
        Self {
            id,
            verbose,
            test_mode: true,
        }
    }
}

impl Drop for PowerAssertion {
    fn drop(&mut self) {
        if !self.test_mode {
            release_assertion(self.id, self.verbose);
        }
    }
}

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
        Ok(PowerAssertion {
            id,
            verbose,
            test_mode: false,
        })
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

    Ok(PowerAssertion {
        id,
        verbose,
        test_mode: false,
    })
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
}
