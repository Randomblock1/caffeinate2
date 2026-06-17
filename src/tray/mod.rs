#[cfg(all(target_os = "macos", feature = "tray"))]
mod app;
#[cfg(all(target_os = "macos", feature = "tray"))]
mod menu;
#[cfg(all(target_os = "macos", feature = "tray"))]
mod state;

#[cfg(all(target_os = "macos", feature = "tray"))]
pub use app::run;
