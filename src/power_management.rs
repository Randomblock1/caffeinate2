use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::string::{CFString, CFStringRef};
use libloading::{Library, Symbol};
use std::mem::MaybeUninit;

#[allow(non_camel_case_types)]
type IOPMAssertionID = u32;
type IOPMAssertionLevel = u32;
const IOPMASSERTION_LEVEL_ON: u32 = 255;
const IOPMASSERTION_LEVEL_OFF: u32 = 0;

pub fn create_assertion(assertion_type: &str, state: bool) -> u32 {
    let io_kit: Library =
        unsafe { Library::new("/System/Library/Frameworks/IOKit.framework/IOKit") }.unwrap();

    let iopmassertion_create_with_name: Symbol<
        unsafe extern "C" fn(
            CFStringRef,
            IOPMAssertionLevel,
            CFStringRef,
            *mut IOPMAssertionID,
        ) -> i32,
    > = unsafe { io_kit.get(b"IOPMAssertionCreateWithName") }.unwrap();
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
        "Successfully created power management assertion with ID: {:?}",
        id
    );

    id
}

pub fn release_assertion(assertion_id: u32) -> i32 {
    let io_kit: Library =
        unsafe { Library::new("/System/Library/Frameworks/IOKit.framework/IOKit") }.unwrap();

    let iopmassertion_release: Symbol<unsafe extern "C" fn(IOPMAssertionID) -> i32> =
        unsafe { io_kit.get(b"IOPMAssertionRelease") }.unwrap();

    let status = unsafe { iopmassertion_release(assertion_id) };
    if status == 0 {
        #[cfg(debug_assertions)]
        println!(
            "Successfully released power management assertion with ID: {:?}",
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

pub fn declare_user_activity() -> u32 {
    let io_kit: Library =
        unsafe { Library::new("/System/Library/Frameworks/IOKit.framework/IOKit") }.unwrap();

    let iopmassertion_declare_user_activity: Symbol<
        unsafe extern "C" fn(CFStringRef, IOPMAssertionLevel, *mut IOPMAssertionID) -> i32,
    > = unsafe { io_kit.get(b"IOPMAssertionDeclareUserActivity") }.unwrap();

    let name = CFString::new("caffeinate2");
    let level = IOPMASSERTION_LEVEL_ON;
    let id = {
        let mut id = MaybeUninit::uninit();
        let status = unsafe {
            iopmassertion_declare_user_activity(name.as_concrete_TypeRef(), level, id.as_mut_ptr())
        };
        if status == 0 {
            unsafe { id.assume_init() }
        } else {
            panic!("Failed to declare user activity with code: {:X}", status);
        }
    };

    #[cfg(debug_assertions)]
    println!("Successfully declared user activity with ID: {:?}", id);

    id
}

pub fn check_assertion_status(assertion_id: u32) -> i32 {
    let io_kit: Library =
        unsafe { Library::new("/System/Library/Frameworks/IOKit.framework/IOKit") }.unwrap();

    let iopmassertion_getvalue: Symbol<
        unsafe extern "C" fn(IOPMAssertionID, CFStringRef, *mut i32) -> i32,
    > = unsafe { io_kit.get(b"IOPMAssertionGetValue") }.unwrap();

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
            "Successfully checked assertion status with ID: {:?}",
            assertion_id
        );
    } else {
        panic!("Failed to check assertion status with code: {:X}", status);
    }
    status
}

pub fn disable_sleep(sleep_disabled: bool) -> u32 {
    let lib = unsafe { Library::new("/System/Library/Frameworks/IOKit.framework/IOKit") }.unwrap();
    let iopm_set_system_power_setting: libloading::Symbol<
        unsafe extern "C" fn(CFString, CFBoolean) -> u32,
    > = unsafe { lib.get(b"IOPMSetSystemPowerSetting").unwrap() };

    let sleep_disabled_bool = if sleep_disabled {
        CFBoolean::true_value()
    } else {
        CFBoolean::false_value()
    };
    unsafe {
        iopm_set_system_power_setting(
            CFString::from_static_string("SleepDisabled"),
            sleep_disabled_bool,
        )
    }
}
