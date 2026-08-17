pub const ICON_OFF: &[u8] = include_bytes!("../../resources/icons/icon_off.png");

pub const ICON_ON: &[u8] = include_bytes!("../../resources/icons/icon_on.png");

use crate::tray::error::TrayError;

/// Decode an embedded PNG to raw RGBA plus its actual dimensions.
///
/// Swapped assets can't silently mismatch a hardcoded size. macOS scales the
/// tray image down to menu-bar height, so a large source stays crisp on retina
/// displays.
///
/// # Errors
///
/// Returns an error if the PNG bytes cannot be decoded.
pub fn decode_icon_rgba(png_bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), TrayError> {
    let image = image::load_from_memory(png_bytes)?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok((rgba.into_raw(), width, height))
}
