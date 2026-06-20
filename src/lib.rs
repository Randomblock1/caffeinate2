#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod tray;
#[cfg(target_os = "macos")]
pub mod entirely;
#[cfg(target_os = "macos")]
pub mod sleep;
pub mod util;
