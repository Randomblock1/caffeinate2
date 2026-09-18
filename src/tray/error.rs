use std::io;

use crate::entirely::error::InstallError;

#[derive(Debug, thiserror::Error)]
pub enum TrayError {
    #[error("HOME is not set")]
    HomeNotSet,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("failed to serialize config: {0}")]
    SerializeConfig(#[from] toml::ser::Error),
    #[error("failed to decode tray icon: {0}")]
    DecodeIcon(#[from] image::ImageError),
    #[error("failed to build tray icon: {0}")]
    BuildIcon(String),
    #[error(transparent)]
    Install(#[from] InstallError),
    #[error("tray must run on the main thread")]
    NotMainThread,
    #[error("failed to create tray icon: {0}")]
    TraySetup(String),
    #[error("helper install still in progress; retry once it finishes")]
    HelperInstallPending,
}
