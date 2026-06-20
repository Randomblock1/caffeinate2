pub mod app_target;
#[cfg(target_os = "macos")]
pub mod authz;
pub mod duration_parser;
#[cfg(target_os = "macos")]
pub mod entirely;
pub mod fs_util;
#[cfg(target_os = "macos")]
pub mod helper_ipc;
#[cfg(target_os = "macos")]
pub mod install;
#[cfg(any(all(test, unix), target_os = "macos"))]
pub mod lockfile;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod macos_activation;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod macos_apps;
#[cfg(target_os = "macos")]
pub mod power_management;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod process_enum;
#[cfg(target_os = "macos")]
pub mod process_util;
#[cfg(target_os = "macos")]
pub mod sleep_mode;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod tray;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod tray_icons;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod tray_mode;
