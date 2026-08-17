#[cfg(target_os = "macos")]
pub mod entirely;
#[cfg(target_os = "macos")]
pub mod sleep;
#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod tray;
pub mod util;
