#![allow(dead_code)]

#[cfg(feature = "tray")]
pub const ICON_OFF: &[u8] = include_bytes!("../resources/icons/icon_off.png");

#[cfg(feature = "tray")]
pub const ICON_ON: &[u8] = include_bytes!("../resources/icons/icon_on.png");

#[cfg(feature = "tray")]
pub fn decode_icon_rgba(png_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory(png_bytes).map_err(|e| e.to_string())?;
    let rgba = image.to_rgba8();
    Ok(rgba.into_raw())
}

/// Pixel dimensions of the embedded PNGs. macOS scales the tray image down
/// to menu-bar height, so a large source stays crisp on retina displays.
#[cfg(feature = "tray")]
pub const ICON_SIZE: u32 = 256;
