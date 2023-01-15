#![allow(dead_code)]

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::string::{CFString, CFStringRef};
use libloading::{Library, Symbol};
use std::mem::MaybeUninit;

type IOPMAssertionID = u32;
type IOPMAssertionLevel = u32;
const IOPMASSERTION_LEVEL_ON: u32 = 255;
const IOPMASSERTION_LEVEL_OFF: u32 = 0;

fn load_iokit() -> Library {
    unsafe { Library::new("/System/Library/Frameworks/IOKit.framework/IOKit").unwrap() }
}

pub fn create_assertion(assertion_type: &str, state: bool) -> u32 {
    let iokit = load_iokit();
    let iopmassertion_create_with_name: Symbol<
        unsafe extern "C" fn(
            CFStringRef,
            IOPMAssertionLevel,
            CFStringRef,
            *mut IOPMAssertionID,
        ) -> i32,
    > = unsafe { iokit.get(b"IOPMAssertionCreateWithName") }.unwrap();
    let name = CFString::new("caffeinate2");
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
                name.as_concrete_TypeRef(),
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

pub fn release_assertion(assertion_id: u32) -> i32 {
    let iokit = load_iokit();
    let iopmassertion_release: Symbol<unsafe extern "C" fn(IOPMAssertionID) -> i32> =
        unsafe { iokit.get(b"IOPMAssertionRelease") }.unwrap();

    let status = unsafe { iopmassertion_release(assertion_id) };
    if status == 0 {
        #[cfg(debug_assertions)]
        println!(
            "Successfully released power management assertion with ID: {}",
            assertion_id
        );
    } else {
        panic!(
            "Failed to release power management assertion with code: {:X}",
            status
        );
    }
    status
}

pub fn declare_user_activity(state: bool) -> u32 {
    let iokit = load_iokit();
    let iopmassertion_declare_user_activity: Symbol<
        unsafe extern "C" fn(CFStringRef, IOPMAssertionLevel, *mut IOPMAssertionID) -> i32,
    > = unsafe { iokit.get(b"IOPMAssertionDeclareUserActivity") }.unwrap();

    let name = CFString::new("caffeinate2");
    let level = if state {
        IOPMASSERTION_LEVEL_ON
    } else {
        IOPMASSERTION_LEVEL_OFF
    };

    let mut id = MaybeUninit::uninit();
    let status = unsafe {
        iopmassertion_declare_user_activity(name.as_concrete_TypeRef(), level, id.as_mut_ptr())
    };
    if status != 0 {
        panic!("Failed to declare user activity with code: {:X}", status);
    }

    let id = unsafe { id.assume_init() };

    #[cfg(debug_assertions)]
    println!("Successfully declared user activity with ID: {}", id);

    id
}

pub fn check_assertion_status(assertion_id: u32) -> i32 {
    let iokit = load_iokit();
    let iopmassertion_getvalue: Symbol<
        unsafe extern "C" fn(IOPMAssertionID, CFStringRef, *mut i32) -> i32,
    > = unsafe { iokit.get(b"IOPMAssertionGetValue") }.unwrap();

    let name = CFString::new("caffeinate2");
    let mut status = MaybeUninit::uninit();
    let status = unsafe {
        iopmassertion_getvalue(
            assertion_id,
            name.as_concrete_TypeRef(),
            status.as_mut_ptr(),
        )
    };
    if status == 0 {
        #[cfg(debug_assertions)]
        println!(
            "Successfully checked assertion status with ID: {}",
            assertion_id
        );
    } else {
        panic!("Failed to check assertion status with code: {:X}", status);
    }
    status
}

pub fn set_sleep_disabled(sleep_disabled: bool) -> u32 {
    let iokit = load_iokit();
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

    result
}

pub fn get_sleep_disabled() -> bool {
    let path = "/Library/Preferences/com.apple.PowerManagement.plist";

    // Open the file
    let value: plist::Value = match plist::from_file(path) {
        Ok(v) => v,
        Err(e) => {
            panic!("Failed to open {}: {}", path, e);
        }
    };

    // Get the "SystemPowerSettings" dictionary from the root dictionary
    let system_power_settings = value
        .as_dictionary()
        .and_then(|dict| dict.get("SystemPowerSettings"))
        .and_then(|dict| dict.as_dictionary())
        .unwrap_or_else(|| {
            panic!("Failed to get SystemPowerSettings dictionary from {}", path);
        });

    // Get the "SleepDisabled" key from the "SystemPowerSettings" dictionary
    let sleep_disabled = system_power_settings
        .get("SleepDisabled")
        .and_then(|val| val.as_boolean())
        .unwrap_or_else(|| {
            panic!("Failed to get SleepDisabled value from {}", path);
        });

    #[cfg(debug_assertions)]
    println!(
        "Sleep is currently {}",
        if sleep_disabled {
            "disabled"
        } else {
            "enabled"
        }
    );
    sleep_disabled
}
