use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::string::{CFString, CFStringRef};
use libloading::{Library, Symbol};
use std::mem::MaybeUninit;

// constants
type IOPMAssertionID = u32;
type IOPMAssertionLevel = u32;
const IOPMASSERTION_LEVEL_ON: u32 = 255;
const IOPMASSERTION_LEVEL_OFF: u32 = 0;

// global variables
pub struct IOKit {
    library: Library,
    assertion_name: CFString,
}

// functions
impl IOKit {
    pub fn new() -> IOKit {
        let library =
            unsafe { Library::new("/System/Library/Frameworks/IOKit.framework/IOKit").unwrap() };
        let assertion_name = CFString::new("caffeinate2");
        IOKit {
            library,
            assertion_name,
        }
    }

    pub fn create_assertion(&self, assertion_type: &str, state: bool) -> u32 {
        let iokit = &self.library;
        let iopmassertion_create_with_name: Symbol<
            unsafe extern "C" fn(
                CFStringRef,
                IOPMAssertionLevel,
                CFStringRef,
                *mut IOPMAssertionID,
            ) -> i32,
        > = unsafe { iokit.get(b"IOPMAssertionCreateWithName") }.unwrap();
        let type_ = CFString::new(assertion_type);
        let level = if state {
            IOPMASSERTION_LEVEL_ON
        } else {
            IOPMASSERTION_LEVEL_OFF
        };
        let id = {
            let mut id = MaybeUninit::uninit();
            let status = unsafe {
                iopmassertion_create_with_name(
                    type_.as_concrete_TypeRef(),
                    level,
                    self.assertion_name.as_concrete_TypeRef(),
                    id.as_mut_ptr(),
                )
            };
            if status == 0 {
                unsafe { id.assume_init() }
            } else {
                panic!(
                    "Failed to create power management assertion with code: {:X}",
                    status
                );
            }
        };

        #[cfg(debug_assertions)]
        println!(
            "Successfully created power management assertion with ID: {}",
            id
        );

        id
    }

    pub fn release_assertion(&self, assertion_id: u32) {
        let iokit = &self.library;
        let iopmassertion_release: Symbol<unsafe extern "C" fn(IOPMAssertionID) -> u32> =
            unsafe { iokit.get(b"IOPMAssertionRelease") }.unwrap();

        #[cfg(debug_assertions)]
        println!(
            "Releasing power management assertion with ID: {}",
            assertion_id
        );

        let status = unsafe { iopmassertion_release(assertion_id) };

        match status {
            0 => {
                #[cfg(debug_assertions)]
                println!(
                    "Successfully released power management assertion with ID: {}",
                    assertion_id
                );
            }
            0xE00002C2 => {
                #[cfg(debug_assertions)]
                println!("Assertion {} already released", assertion_id);
            }
            _ => panic!(
                "Failed to release power management assertion with code: {:X}",
                status
            ),
        }
    }

    pub fn declare_user_activity(&self, state: bool) -> u32 {
        let iokit = &self.library;
        let iopmassertion_declare_user_activity: Symbol<
            unsafe extern "C" fn(CFStringRef, IOPMAssertionLevel, *mut IOPMAssertionID) -> i32,
        > = unsafe { iokit.get(b"IOPMAssertionDeclareUserActivity") }.unwrap();

        let level = if state {
            IOPMASSERTION_LEVEL_ON
        } else {
            IOPMASSERTION_LEVEL_OFF
        };

        let mut id = MaybeUninit::uninit();
        let status = unsafe {
            iopmassertion_declare_user_activity(
                self.assertion_name.as_concrete_TypeRef(),
                level,
                id.as_mut_ptr(),
            )
        };
        if status != 0 {
            panic!("Failed to declare user activity with code: {:X}", status);
        }

        let id = unsafe { id.assume_init() };

        #[cfg(debug_assertions)]
        println!("Successfully declared user activity with ID: {}", id);

        id
    }

    pub fn set_sleep_disabled(&self, sleep_disabled: bool) -> Result<(), u32> {
        let iokit = &self.library;
        let iopm_set_system_power_setting: libloading::Symbol<
            unsafe extern "C" fn(CFString, CFBoolean) -> u32,
        > = unsafe { iokit.get(b"IOPMSetSystemPowerSetting").unwrap() };

        let sleep_disabled_bool = if sleep_disabled {
            CFBoolean::true_value()
        } else {
            CFBoolean::false_value()
        };

        let result = unsafe {
            iopm_set_system_power_setting(
                CFString::from_static_string("SleepDisabled"),
                sleep_disabled_bool,
            )
        };

        #[cfg(debug_assertions)]
        println!(
            "Got result {:X} when {} sleep",
            result,
            if sleep_disabled {
                "disabling"
            } else {
                "enabling"
            }
        );

        // See IOKit/IOReturn.h for error codes.
        if result == 0 {
            // Success
            Ok(())
        } else if result == 0xE00002C1 {
            // Insufficient privileges
            Err(result)
        } else {
            panic!(
                "Error: Failed to modify system sleep with code: {:X}",
                result
            );
        }
    }

    pub fn get_sleep_disabled() -> bool {
        const PATH: &str = "/Library/Preferences/com.apple.PowerManagement.plist";

        // Open the file
        let value: plist::Value = match plist::from_file(PATH) {
            Ok(v) => v,
            Err(e) => {
                panic!("Failed to open {}: {}", PATH, e);
            }
        };

        // Get the "SystemPowerSettings" dictionary from the root dictionary
        let system_power_settings = value
            .as_dictionary()
            .and_then(|dict| dict.get("SystemPowerSettings"))
            .and_then(|dict| dict.as_dictionary())
            .unwrap_or_else(|| {
                panic!("Failed to get SystemPowerSettings dictionary from {}", PATH);
            });

        // Get the "SleepDisabled" key from the "SystemPowerSettings" dictionary
        let sleep_disabled = system_power_settings
            .get("SleepDisabled")
            .and_then(|val| val.as_boolean())
            .unwrap_or_else(|| {
                panic!("Failed to get SleepDisabled value from {}", PATH);
            });

        #[cfg(debug_assertions)]
        println!(
            "System sleep is currently {}",
            if sleep_disabled {
                "disabled"
            } else {
                "enabled"
            }
        );
        sleep_disabled
    }
}
