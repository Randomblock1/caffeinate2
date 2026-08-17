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

/// What a [`HelperIpcError`] means, for callers that dispatch on it.
///
/// The wire protocol carries only a message string, so errors received from
/// the daemon are classified by their message prefix ("connect failed:",
/// "not authorized", "internal error:" — the prefixes are wire contract, see
/// [`crate::entirely::authz::denial_message`]); locally constructed errors
/// carry their kind directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperIpcErrorKind {
    /// Could not connect to the helper socket: the daemon is not running.
    Connect,
    /// The helper denied the hold (a policy decision, not a failure).
    NotAuthorized,
    /// Helper-side internal failure (e.g. a failed peer-credential read) —
    /// deliberately distinct from `NotAuthorized` so it is never surfaced as
    /// a denial.
    Internal,
    /// The peer closed the connection without sending a byte. The daemon
    /// treats this as a liveness probe, not a failure.
    EmptyRequest,
    /// A newline-framed line exceeded the size limit.
    RequestTooLarge,
    /// The frame never terminated with a newline.
    MissingNewline,
    /// The absolute per-connection deadline elapsed.
    DeadlineExceeded,
    /// Anything else: transport failures, malformed JSON, unexpected responses.
    Other,
}

/// IPC / RPC error from the privileged helper client or daemon.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct HelperIpcError {
    message: String,
    kind: HelperIpcErrorKind,
}

impl HelperIpcError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        let kind = Self::classify(&message);
        Self { message, kind }
    }

    #[must_use]
    pub fn with_kind(kind: HelperIpcErrorKind, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind,
        }
    }

    /// Derive the kind from the message prefixes the wire protocol uses. Only
    /// meaningful semantics travel as prefixes; everything else is `Other`.
    fn classify(message: &str) -> HelperIpcErrorKind {
        if message.starts_with("connect failed:") {
            HelperIpcErrorKind::Connect
        } else if message.starts_with("not authorized") {
            HelperIpcErrorKind::NotAuthorized
        } else if message.starts_with("internal error:") {
            HelperIpcErrorKind::Internal
        } else {
            HelperIpcErrorKind::Other
        }
    }

    #[must_use]
    pub const fn kind(&self) -> HelperIpcErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn connect(error: io::Error) -> Self {
        Self::with_kind(
            HelperIpcErrorKind::Connect,
            format!("connect failed: {error}"),
        )
    }

    #[must_use]
    pub fn internal(error: io::Error) -> Self {
        Self::with_kind(
            HelperIpcErrorKind::Internal,
            format!("internal error: {error}"),
        )
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
