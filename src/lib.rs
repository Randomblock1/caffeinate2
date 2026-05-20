pub mod duration_parser;
#[cfg(any(all(test, unix), target_os = "macos"))]
pub mod lockfile;
#[cfg(target_os = "macos")]
pub mod power_management;
#[cfg(target_os = "macos")]
pub mod process_lock;
#[cfg(target_os = "macos")]
pub mod process_util;
#[cfg(target_os = "macos")]
pub mod entirely;
#[cfg(target_os = "macos")]
pub mod helper_ipc;
#[cfg(target_os = "macos")]
pub mod install;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod macos_apps;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod tray_mode;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod macos_activation;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod tray_icons;
