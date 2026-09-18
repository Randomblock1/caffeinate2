use crate::sleep::sleep_mode::SleepMode;
use crate::tray::app_target::WatchTarget;
use crate::tray::error::TrayError;
use crate::util::duration_parser::format_remaining_secs;
use crate::util::fs_util;
use serde::{Deserialize, Serialize};

/// Bumped to 2 when the single `wait_for_app` target became the multi-select
/// `wait_for_apps` list, and to 3 when the single `mode` became the
/// multi-select `modes` list. Recorded for future format changes; the loader
/// never gates parsing on it.
pub const CONFIG_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeLimitPreset {
    pub label: &'static str,
    pub seconds: Option<u64>,
}

impl TimeLimitPreset {
    pub const ALL: [Self; 7] = [
        Self {
            label: "Off",
            seconds: None,
        },
        Self {
            label: "15 minutes",
            seconds: Some(15 * 60),
        },
        Self {
            label: "30 minutes",
            seconds: Some(30 * 60),
        },
        Self {
            label: "1 hour",
            seconds: Some(60 * 60),
        },
        Self {
            label: "2 hours",
            seconds: Some(2 * 60 * 60),
        },
        Self {
            label: "4 hours",
            seconds: Some(4 * 60 * 60),
        },
        Self {
            label: "8 hours",
            seconds: Some(8 * 60 * 60),
        },
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrayConfig {
    #[serde(default = "default_config_version")]
    pub version: u32,
    /// The sleep modes a session runs in. Several at once is the point: holding
    /// Entirely (so the Mac survives the lid closing) alongside User active (so
    /// the screen stays unlocked) is one selection, not two sessions.
    /// Normalized by [`TrayConfig::normalize_modes`].
    #[serde(default)]
    pub modes: Vec<SleepMode>,
    /// The pre-v3 single-`mode` key, read so an existing `tray.toml` keeps its
    /// selection. Never written back: [`TrayConfig::normalize_modes`] folds it
    /// into `modes`.
    #[serde(default, rename = "mode", skip_serializing)]
    pub legacy_mode: Option<SleepMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_limit_secs: Option<u64>,
    /// Multi-select list of programs the session waits on; empty means no app
    /// watch. `serde(default)` yields an empty vec when the key is absent — it
    /// must NOT fall back to the whole-struct `Default`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wait_for_apps: Vec<WatchTarget>,
    /// Watch for low-level sleep assertions from other processes and upgrade
    /// them to Entirely mode while they last. `serde(default)` keeps old
    /// configs (without this key) loadable.
    #[serde(default)]
    pub upgrade_external: bool,
    /// Programs the watcher must never upgrade, by the process name IOKit
    /// reports for the assertion. Built from the menu bar (the upgrade dialog's
    /// "Never Upgrade This App", removable under **Ignored apps**) and editable
    /// by hand in `tray.toml`. Compared case-insensitively; kept sorted and
    /// deduped by [`TrayConfig::normalize_ignored_apps`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_apps: Vec<String>,
}

const fn default_config_version() -> u32 {
    CONFIG_VERSION
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            version: default_config_version(),
            modes: vec![SleepMode::default()],
            legacy_mode: None,
            time_limit_secs: None,
            wait_for_apps: Vec::new(),
            upgrade_external: false,
            ignored_apps: Vec::new(),
        }
    }
}

impl TrayConfig {
    /// Fold the pre-v3 `mode` key into `modes`, then put the result in
    /// [`SleepMode::all`] order and dedupe it, so a hand-edited config behaves
    /// like a menu-edited one and the menu's checkmarks match the file.
    ///
    /// An empty selection falls back to the default mode: "on" has to prevent
    /// something, and a session holding nothing is a state the menu bar cannot
    /// explain (the icon would read on while the Mac slept anyway).
    pub fn normalize_modes(&mut self) {
        if let Some(legacy) = self.legacy_mode.take()
            && self.modes.is_empty()
        {
            self.modes.push(legacy);
        }
        self.modes = SleepMode::all()
            .into_iter()
            .filter(|mode| self.modes.contains(mode))
            .collect();
        if self.modes.is_empty() {
            self.modes.push(SleepMode::default());
        }
    }

    /// Sort, dedupe (case-insensitively) and drop blank entries from
    /// `ignored_apps`, so a hand-edited config behaves like a menu-edited one
    /// and the **Ignored apps** submenu lists each program exactly once.
    pub fn normalize_ignored_apps(&mut self) {
        self.ignored_apps.retain(|name| !name.trim().is_empty());
        self.ignored_apps.sort_by_key(|name| name.to_lowercase());
        self.ignored_apps.dedup_by_key(|name| name.to_lowercase());
    }

    /// Whether `name` is on the user's ignore list. Case-insensitive: IOKit's
    /// process names are whatever the holder registered, and a hand-edited
    /// config should not have to match their capitalization exactly.
    #[must_use]
    pub fn ignores_app(&self, name: &str) -> bool {
        self.ignored_apps
            .iter()
            .any(|ignored| same_app_name(ignored, name))
    }
}

/// Case-insensitive app-name equality, using the same Unicode `to_lowercase`
/// that [`TrayConfig::normalize_ignored_apps`] sorts and dedups with. Every
/// comparison against `ignored_apps` must go through this: an ASCII-only
/// comparison treats two names differing in non-ASCII case ("CAFÉ" vs "Café")
/// as different apps, so such an ignore entry silently never matches.
#[must_use]
pub fn same_app_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

///
/// # Errors
///
/// Returns an error if `HOME` is not set.
pub fn config_path() -> Result<std::path::PathBuf, TrayError> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or(TrayError::HomeNotSet)?;
    Ok(home
        .join("Library/Application Support/caffeinate2")
        .join("tray.toml"))
}

#[must_use]
pub fn load_config() -> TrayConfig {
    let Ok(path) = config_path() else {
        return TrayConfig::default();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return TrayConfig::default();
    };
    let parsed = toml::from_str::<TrayConfig>(&content);
    let mut config = match parsed {
        Ok(config) => config,
        Err(error) => {
            let backup = path.with_extension("toml.bak");
            if let Err(copy_error) = std::fs::copy(&path, &backup) {
                eprintln!(
                    "Warning: could not parse {} ({error}); failed to back up to {} ({copy_error}); using defaults",
                    path.display(),
                    backup.display()
                );
            } else {
                eprintln!(
                    "Warning: could not parse {} ({error}); backed up to {}; using defaults",
                    path.display(),
                    backup.display()
                );
            }
            TrayConfig::default()
        }
    };
    config.normalize_modes();
    config.normalize_ignored_apps();
    config
}

///
/// # Errors
///
/// Returns an error if the config directory or file cannot be written.
pub fn save_config(config: &TrayConfig) -> Result<(), TrayError> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut to_save = config.clone();
    to_save.version = CONFIG_VERSION;
    let content = toml::to_string_pretty(&to_save)?;
    fs_util::atomic_write(&path, content.as_bytes())?;
    Ok(())
}

/// Maximum number of app names shown inline in a tooltip before the rest are
/// collapsed into a "+N more" suffix; the full set lives in the menu/picker.
const MAX_INLINE_APPS: usize = 3;

/// Join `names` with ", ", capping the inline list at `MAX_INLINE_APPS` and
/// summarizing the overflow as " +N more".
fn truncate_join<S: AsRef<str>>(names: &[S]) -> String {
    let join = |slice: &[S]| {
        slice
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(", ")
    };
    if names.len() <= MAX_INLINE_APPS {
        join(names)
    } else {
        format!(
            "{} +{} more",
            join(&names[..MAX_INLINE_APPS]),
            names.len() - MAX_INLINE_APPS
        )
    }
}

/// What the upgrade watcher is doing right now, for the tooltip.
pub struct UpgradeStatus<'a> {
    /// The external processes whose assertions the hold covers (empty when none
    /// of them exposed a name).
    pub apps: &'a [String],
    /// Seconds until the hold is released because those processes stopped
    /// asserting; `None` while their assertions are still live.
    pub releasing_in: Option<u64>,
}

/// Build menu bar tooltip text while sleep prevention is active.
///
/// When the session was started by the upgrade watcher, `upgrading` is `Some`
/// and the other parameters are irrelevant (upgrade sessions ignore the time
/// limit and app watch). A long list of names is truncated so the tooltip stays
/// readable; the full set is shown in the menu.
pub fn format_active_tooltip(
    remaining_secs: Option<u64>,
    wait_for_apps: &[WatchTarget],
    waiting_for_app_launch: bool,
    upgrading: Option<UpgradeStatus<'_>>,
) -> String {
    if let Some(status) = upgrading {
        let who = match status.apps {
            [] => "external app".to_string(),
            apps => truncate_join(apps),
        };
        return match status.releasing_in {
            // The assertions are already gone; the hold outlives them by the
            // release grace period, and saying "upgrading" through that window
            // reads as though the programs were still holding the Mac awake.
            Some(secs) => format!("caffeinate2 ({who} stopped asserting · releasing in {secs}s)"),
            None => format!("caffeinate2 (upgrading {who})"),
        };
    }

    let mut parts = Vec::new();
    if let Some(secs) = remaining_secs.filter(|&s| s > 0) {
        parts.push(format_remaining_secs(secs));
    }
    if !wait_for_apps.is_empty() {
        let names: Vec<&str> = wait_for_apps.iter().map(WatchTarget::name).collect();
        let shown = truncate_join(&names);
        if waiting_for_app_launch {
            parts.push(format!("waiting for {shown}"));
        } else {
            // Singular reads naturally for the common one-app case.
            let verb = if wait_for_apps.len() == 1 {
                "quits"
            } else {
                "quit"
            };
            parts.push(format!("until {shown} {verb}"));
        }
    }
    if parts.is_empty() {
        "caffeinate2".to_string()
    } else {
        format!("caffeinate2 ({})", parts.join(" · "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_current_version() {
        assert_eq!(TrayConfig::default().version, CONFIG_VERSION);
    }

    #[test]
    fn parse_config_defaults_time_limit_to_off() {
        let config: TrayConfig = toml::from_str("modes = [\"system\"]\n").unwrap();
        assert_eq!(config.modes, vec![SleepMode::System]);
        assert_eq!(config.time_limit_secs, None);
    }

    #[test]
    fn parse_config_reads_time_limit() {
        let config: TrayConfig =
            toml::from_str("modes = [\"display\"]\ntime_limit_secs = 1800\n").unwrap();
        assert_eq!(config.modes, vec![SleepMode::Display]);
        assert_eq!(config.time_limit_secs, Some(1800));
    }

    /// A config written before the multi-select menu keeps its mode.
    #[test]
    fn pre_v3_single_mode_key_migrates_into_the_list() {
        let mut config: TrayConfig = toml::from_str("version = 2\nmode = \"entirely\"\n").unwrap();
        config.normalize_modes();
        assert_eq!(config.modes, vec![SleepMode::Entirely]);
        assert_eq!(config.legacy_mode, None);
        // And it is not written back, so the next load reads only `modes`.
        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(!serialized.contains("\nmode ="));
        assert!(serialized.contains("modes = [\"entirely\"]"));
    }

    #[test]
    fn normalize_modes_orders_dedupes_and_never_leaves_an_empty_selection() {
        let mut config: TrayConfig =
            toml::from_str("modes = [\"entirely\", \"display\", \"entirely\"]\n").unwrap();
        config.normalize_modes();
        assert_eq!(config.modes, vec![SleepMode::Display, SleepMode::Entirely]);

        let mut empty: TrayConfig = toml::from_str("modes = []\n").unwrap();
        empty.normalize_modes();
        assert_eq!(empty.modes, vec![SleepMode::default()]);
    }

    /// `modes` wins over a leftover `mode` key rather than being merged with
    /// it: the v3 list is the one the menu wrote.
    #[test]
    fn an_explicit_mode_list_ignores_the_legacy_key() {
        let mut config: TrayConfig =
            toml::from_str("mode = \"disk\"\nmodes = [\"user_active\"]\n").unwrap();
        config.normalize_modes();
        assert_eq!(config.modes, vec![SleepMode::UserActive]);
    }

    #[test]
    fn parse_config_reads_wait_for_apps_array() {
        let config: TrayConfig = toml::from_str(
            r#"
modes = ["system"]

[[wait_for_apps]]
kind = "bundle"
bundle_id = "com.example.app"
name = "Example"

[[wait_for_apps]]
kind = "executable"
path = "/usr/local/bin/foo"
name = "foo"
"#,
        )
        .unwrap();
        assert_eq!(
            config.wait_for_apps,
            vec![
                WatchTarget::Bundle {
                    bundle_id: "com.example.app".into(),
                    name: "Example".into(),
                },
                WatchTarget::Executable {
                    path: "/usr/local/bin/foo".into(),
                    name: "foo".into(),
                },
            ]
        );
    }

    #[test]
    fn parse_config_defaults_upgrade_external_to_false() {
        let config: TrayConfig = toml::from_str("modes = [\"system\"]\n").unwrap();
        assert!(!config.upgrade_external);
    }

    #[test]
    fn parse_config_reads_upgrade_external() {
        let config: TrayConfig =
            toml::from_str("modes = [\"system\"]\nupgrade_external = true\n").unwrap();
        assert!(config.upgrade_external);
    }

    #[test]
    fn parse_config_defaults_ignored_apps_to_empty() {
        let config: TrayConfig = toml::from_str("modes = [\"system\"]\n").unwrap();
        assert!(config.ignored_apps.is_empty());
        assert!(!config.ignores_app("Zoom"));
    }

    #[test]
    fn hand_edited_ignore_list_is_normalized_and_matched_case_insensitively() {
        let mut config: TrayConfig =
            toml::from_str("ignored_apps = [\"zoom\", \"  \", \"Amphetamine\", \"ZOOM\"]\n")
                .unwrap();
        config.normalize_ignored_apps();
        assert_eq!(config.ignored_apps, vec!["Amphetamine", "zoom"]);
        assert!(config.ignores_app("Zoom"));
        assert!(config.ignores_app("amphetamine"));
        assert!(!config.ignores_app("Zoo"));
    }

    #[test]
    fn ignore_matching_is_unicode_case_insensitive() {
        // Non-ASCII case pairs must match the same way the dedup folds them;
        // eq_ignore_ascii_case would treat "CAFÉ" and "Café" as different apps.
        let mut config: TrayConfig =
            toml::from_str("ignored_apps = [\"Café Ω Tëst\", \"CAFÉ Ω TËST\"]\n").unwrap();
        config.normalize_ignored_apps();
        assert_eq!(config.ignored_apps.len(), 1);
        assert!(config.ignores_app("café ω tëst"));
        assert!(config.ignores_app("CAFÉ Ω TËST"));
        assert!(!config.ignores_app("Cafe Ω Tëst"));
    }

    #[test]
    fn empty_ignore_list_is_not_written_back() {
        let serialized = toml::to_string_pretty(&TrayConfig::default()).unwrap();
        assert!(!serialized.contains("ignored_apps"));
    }

    #[test]
    fn format_active_tooltip_combines_limit_and_app() {
        let apps = vec![WatchTarget::Bundle {
            bundle_id: "com.example".into(),
            name: "Example".into(),
        }];
        assert_eq!(
            format_active_tooltip(Some(90), &apps, false, None),
            "caffeinate2 (2m remaining · until Example quits)"
        );
        assert_eq!(
            format_active_tooltip(None, &apps, true, None),
            "caffeinate2 (waiting for Example)"
        );
    }

    #[test]
    fn format_active_tooltip_lists_and_truncates_multiple_apps() {
        let bundle = |n: &str| WatchTarget::Bundle {
            bundle_id: format!("com.example.{n}"),
            name: n.to_string(),
        };
        let two = [bundle("Slack"), bundle("Mail")];
        assert_eq!(
            format_active_tooltip(None, &two, false, None),
            "caffeinate2 (until Slack, Mail quit)"
        );
        let many = [bundle("A"), bundle("B"), bundle("C"), bundle("D")];
        assert_eq!(
            format_active_tooltip(None, &many, false, None),
            "caffeinate2 (until A, B, C +1 more quit)"
        );
    }

    fn upgrading(apps: &[String]) -> Option<UpgradeStatus<'_>> {
        Some(UpgradeStatus {
            apps,
            releasing_in: None,
        })
    }

    #[test]
    fn format_active_tooltip_shows_upgrade_target() {
        let one = ["Claude Code".to_string()];
        assert_eq!(
            format_active_tooltip(None, &[], false, upgrading(&one)),
            "caffeinate2 (upgrading Claude Code)"
        );
        // An upgrade session with no known process names still reads sensibly.
        let none: [String; 0] = [];
        assert_eq!(
            format_active_tooltip(None, &[], false, upgrading(&none)),
            "caffeinate2 (upgrading external app)"
        );
        // A handful of apps are listed inline.
        let two = ["Claude Code".to_string(), "Codex".to_string()];
        assert_eq!(
            format_active_tooltip(None, &[], false, upgrading(&two)),
            "caffeinate2 (upgrading Claude Code, Codex)"
        );
        // A long list is truncated with a "+N more" suffix.
        let many = [
            "A".to_string(),
            "B".to_string(),
            "C".to_string(),
            "D".to_string(),
            "E".to_string(),
        ];
        assert_eq!(
            format_active_tooltip(None, &[], false, upgrading(&many)),
            "caffeinate2 (upgrading A, B, C +2 more)"
        );
    }

    /// Through the release grace window the tooltip has to read as winding
    /// down: the holders stopped asserting, and the countdown says when the
    /// hold goes away.
    #[test]
    fn format_active_tooltip_counts_down_the_release_grace() {
        let releasing = |apps, secs| {
            Some(UpgradeStatus {
                apps,
                releasing_in: Some(secs),
            })
        };
        let one = ["Claude Code".to_string()];
        assert_eq!(
            format_active_tooltip(None, &[], false, releasing(&one, 14)),
            "caffeinate2 (Claude Code stopped asserting · releasing in 14s)"
        );
        let none: [String; 0] = [];
        assert_eq!(
            format_active_tooltip(None, &[], false, releasing(&none, 3)),
            "caffeinate2 (external app stopped asserting · releasing in 3s)"
        );
        let two = ["Claude Code".to_string(), "Codex".to_string()];
        assert_eq!(
            format_active_tooltip(None, &[], false, releasing(&two, 20)),
            "caffeinate2 (Claude Code, Codex stopped asserting · releasing in 20s)"
        );
    }
}
