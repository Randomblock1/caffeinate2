#![allow(dead_code)]

#[cfg(feature = "tray")]
pub const ICON_OFF: &[u8] = include_bytes!("../../resources/icons/icon_off.png");

#[cfg(feature = "tray")]
pub const ICON_ON: &[u8] = include_bytes!("../../resources/icons/icon_on.png");

/// Decode an embedded PNG to raw RGBA plus its actual dimensions.
///
/// Swapped assets can't silently mismatch a hardcoded size. macOS scales the
/// tray image down to menu-bar height, so a large source stays crisp on retina
/// displays.
///
/// # Errors
///
/// Returns an error if the PNG bytes cannot be decoded.
#[cfg(feature = "tray")]
pub fn decode_icon_rgba(png_bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    let image = image::load_from_memory(png_bytes).map_err(|e| e.to_string())?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok((rgba.into_raw(), width, height))
}
