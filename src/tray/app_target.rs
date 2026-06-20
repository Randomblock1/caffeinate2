use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppTarget {
    pub bundle_id: String,
    pub name: String,
}

/// A program the tray watches. A "wait for apps" session stays awake while *any*
/// of its targets is running and stops only once *all* of them have quit (after
/// at least one was seen running).
///
/// Targets are keyed by something stable, never a PID (PIDs are recycled): an
/// `.app` is watched by its bundle id; a non-bundle program is watched by its
/// executable path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WatchTarget {
    /// An `.app`, identified by bundle id (matched via `NSRunningApplication`).
    Bundle { bundle_id: String, name: String },
    /// A non-bundle process, identified by its executable path (matched by
    /// scanning the process tree).
    Executable { path: String, name: String },
}

impl WatchTarget {
    /// Human-readable label for tooltips and the picker.
    #[must_use] 
    pub fn name(&self) -> &str {
        match self {
            Self::Bundle { name, .. } | Self::Executable { name, .. } => name,
        }
    }

    /// Stable identity used to dedup targets and to match checkbox rows across
    /// rebuilds: the bundle id for an app, the executable path otherwise.
    #[must_use] 
    pub fn key(&self) -> &str {
        match self {
            Self::Bundle { bundle_id, .. } => bundle_id,
            Self::Executable { path, .. } => path,
        }
    }

    /// Lift a legacy single-app target into the multi-select model.
    #[must_use] 
    pub fn from_app_target(app: AppTarget) -> Self {
        Self::Bundle {
            bundle_id: app.bundle_id,
            name: app.name,
        }
    }
}
