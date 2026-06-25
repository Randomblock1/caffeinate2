#[cfg(all(target_os = "macos", feature = "tray"))]
mod app;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod app_target;
#[cfg(all(target_os = "macos", feature = "tray"))]
mod error;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod macos_activation;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod macos_apps;
#[cfg(all(target_os = "macos", feature = "tray"))]
mod menu;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod process_enum;
#[cfg(all(target_os = "macos", feature = "tray"))]
mod single_instance;
#[cfg(all(target_os = "macos", feature = "tray"))]
mod state;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod tray_icons;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod tray_mode;
#[cfg(all(target_os = "macos", feature = "tray"))]
mod wait_window;

#[cfg(all(target_os = "macos", feature = "tray"))]
pub use app::run;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub use error::TrayError;
