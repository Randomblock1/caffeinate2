use std::io;

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("Failed to disable sleep (IOKit error: {code:X})")]
    DisableSleepFailed { code: u32 },
    #[error("Failed to re-enable sleep (IOKit error: {code:X})")]
    EnableSleepFailed { code: u32 },
    #[error("Failed to reconcile sleep state (IOKit error: {code:X})")]
    ReconcileSleepFailed { code: u32 },
}

/// IPC / RPC error from the privileged helper client or daemon.
///
/// Message prefixes are stable API for [`crate::entirely::helper_ipc::is_connect_error`]
/// and [`crate::entirely::helper_ipc::is_authorization_error`]; helper-side
/// failures that are neither use the `internal error:` prefix so they can never
/// match the authorization check.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct HelperIpcError {
    message: String,
}

impl HelperIpcError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn connect(error: io::Error) -> Self {
        Self::new(format!("connect failed: {error}"))
    }

    #[must_use]
    pub fn internal(error: io::Error) -> Self {
        Self::new(format!("internal error: {error}"))
    }
}

impl From<serde_json::Error> for HelperIpcError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(format!("invalid response: {error}"))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("{0}")]
    Message(String),
}

impl InstallError {
    #[must_use]
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}
