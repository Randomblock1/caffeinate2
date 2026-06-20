use crate::sleep::sleep_mode::SleepMode;
use crate::tray::app_target::{AppTarget, WatchTarget};
use crate::util::duration_parser::format_remaining_secs;
use crate::util::fs_util;
use serde::{Deserialize, Serialize};

/// Bumped to 2 when the single `wait_for_app` target became the multi-select
/// `wait_for_apps` list.
///
/// Migration is handled by `normalize_legacy`, not the version field (the
/// loader never gates parsing on it).
pub const CONFIG_VERSION: u32 = 2;

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
    #[serde(default)]
    pub mode: SleepMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_limit_secs: Option<u64>,
    /// Multi-select list of programs the session waits on; empty means no app
    /// watch. `serde(default)` yields an empty vec when the key is absent — it
    /// must NOT fall back to the whole-struct `Default`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wait_for_apps: Vec<WatchTarget>,
    /// Legacy v1 single target. Read-only on load (`skip_serializing`, never
    /// written back); `normalize_legacy` folds it into `wait_for_apps`. Kept so
    /// an old `[wait_for_app]` table still deserializes instead of tripping the
    /// `unwrap_or_default()` fallback and wiping every other setting.
    #[serde(default, skip_serializing)]
    pub wait_for_app: Option<AppTarget>,
    /// Watch for low-level sleep assertions from other processes and upgrade
    /// them to Entirely mode while they last. `serde(default)` keeps old
    /// configs (without this key) loadable.
    #[serde(default)]
    pub upgrade_external: bool,
}

const fn default_config_version() -> u32 {
    CONFIG_VERSION
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            version: default_config_version(),
            mode: SleepMode::default(),
            time_limit_secs: None,
            wait_for_apps: Vec::new(),
            wait_for_app: None,
            upgrade_external: false,
        }
    }
}

impl TrayConfig {
    /// Fold a legacy v1 single `wait_for_app` target into `wait_for_apps`, then
    /// clear it so it is never written back. Idempotent; a no-op once migrated.
    fn normalize_legacy(&mut self) {
        if let Some(old) = self.wait_for_app.take() {
            let target = WatchTarget::from_app_target(old);
            if !self.wait_for_apps.iter().any(|t| t.key() == target.key()) {
                self.wait_for_apps.push(target);
            }
        }
    }
}

///
/// # Errors
///
/// Returns an error if `HOME` is not set.
pub fn config_path() -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())?;
    Ok(home
        .join("Library/Application Support/caffeinate2")
        .join("tray.toml"))
}

#[must_use] 
pub fn load_config() -> TrayConfig {
    let Ok(path) = config_path() else {
        return TrayConfig::default();
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return TrayConfig::default();
    };
    let mut config: TrayConfig = toml::from_str(&content).unwrap_or_default();
    config.normalize_legacy();
    config
}

///
/// # Errors
///
/// Returns an error if the config directory or file cannot be written.
pub fn save_config(config: &TrayConfig) -> Result<(), String> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut to_save = config.clone();
    to_save.version = CONFIG_VERSION;
    let content = toml::to_string_pretty(&to_save).map_err(|e| e.to_string())?;
    fs_util::atomic_write(&path, content.as_bytes()).map_err(|e| e.to_string())
}

/// Build menu bar tooltip text while sleep prevention is active.
///
/// When the session was started by the upgrade watcher, `upgrading` is `Some`
/// with the names of the external processes whose assertions are being upgraded
/// (empty when none are known yet), and the other parameters are irrelevant
/// (upgrade sessions ignore the time limit and app watch). A long list is
/// truncated so the tooltip stays readable; the full set is shown in the menu.
pub fn format_active_tooltip(
    remaining_secs: Option<u64>,
    wait_for_apps: &[WatchTarget],
    waiting_for_app_launch: bool,
    upgrading: Option<&[String]>,
) -> String {
    if let Some(apps) = upgrading {
        return match apps {
            [] => "caffeinate2 (upgrading external app)".to_string(),
            [one] => format!("caffeinate2 (upgrading {one})"),
            _ => {
                // Cap the inline list; the menu carries the complete set.
                const MAX: usize = 3;
                if apps.len() <= MAX {
                    format!("caffeinate2 (upgrading {})", apps.join(", "))
                } else {
                    format!(
                        "caffeinate2 (upgrading {} +{} more)",
                        apps[..MAX].join(", "),
                        apps.len() - MAX
                    )
                }
            }
        };
    }

    let mut parts = Vec::new();
    if let Some(secs) = remaining_secs.filter(|&s| s > 0) {
        parts.push(format_remaining_secs(secs));
    }
    if !wait_for_apps.is_empty() {
        // Cap the inline list like the upgrade branch; the picker holds the
        // full set.
        const MAX: usize = 3;
        let names: Vec<&str> = wait_for_apps.iter().map(WatchTarget::name).collect();
        let shown = if names.len() <= MAX {
            names.join(", ")
        } else {
            format!("{} +{} more", names[..MAX].join(", "), names.len() - MAX)
        };
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

#[cfg(all(test, feature = "tray"))]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_current_version() {
        assert_eq!(TrayConfig::default().version, CONFIG_VERSION);
    }

    #[test]
    fn parse_config_defaults_time_limit_to_off() {
        let config: TrayConfig = toml::from_str("mode = \"system\"\n").unwrap();
        assert_eq!(config.mode, SleepMode::System);
        assert_eq!(config.time_limit_secs, None);
    }

    #[test]
    fn parse_config_reads_time_limit() {
        let config: TrayConfig =
            toml::from_str("mode = \"display\"\ntime_limit_secs = 1800\n").unwrap();
        assert_eq!(config.mode, SleepMode::Display);
        assert_eq!(config.time_limit_secs, Some(1800));
    }

    #[test]
    fn parse_config_migrates_v1_single_app() {
        let mut config: TrayConfig = toml::from_str(
            r#"
mode = "system"

[wait_for_app]
bundle_id = "com.example.app"
name = "Example"
"#,
        )
        .unwrap();
        config.normalize_legacy();
        // Legacy single target folds into the multi-select list and is dropped.
        assert_eq!(config.wait_for_app, None);
        assert_eq!(
            config.wait_for_apps,
            vec![WatchTarget::Bundle {
                bundle_id: "com.example.app".into(),
                name: "Example".into(),
            }]
        );
    }

    #[test]
    fn parse_config_reads_wait_for_apps_array() {
        let config: TrayConfig = toml::from_str(
            r#"
mode = "system"

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
    fn migration_preserves_other_settings() {
        // The legacy `[wait_for_app]` table must not trip the parse fallback and
        // discard mode/time_limit/upgrade_external.
        let mut config: TrayConfig = toml::from_str(
            r#"
mode = "display"
time_limit_secs = 1800
upgrade_external = true

[wait_for_app]
bundle_id = "com.example.app"
name = "Example"
"#,
        )
        .unwrap();
        config.normalize_legacy();
        assert_eq!(config.mode, SleepMode::Display);
        assert_eq!(config.time_limit_secs, Some(1800));
        assert!(config.upgrade_external);
        assert_eq!(config.wait_for_apps.len(), 1);
    }

    #[test]
    fn save_omits_legacy_wait_for_app_key() {
        let mut config = TrayConfig {
            wait_for_app: Some(AppTarget {
                bundle_id: "com.example.app".into(),
                name: "Example".into(),
            }),
            ..TrayConfig::default()
        };
        config.normalize_legacy();
        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(!serialized.contains("[wait_for_app]"));
        assert!(serialized.contains("wait_for_apps"));
    }

    #[test]
    fn parse_config_defaults_upgrade_external_to_false() {
        let config: TrayConfig = toml::from_str("mode = \"system\"\n").unwrap();
        assert!(!config.upgrade_external);
    }

    #[test]
    fn parse_config_reads_upgrade_external() {
        let config: TrayConfig =
            toml::from_str("mode = \"system\"\nupgrade_external = true\n").unwrap();
        assert!(config.upgrade_external);
    }

    #[test]
    fn format_active_tooltip_combines_limit_and_app() {
        let apps = vec![WatchTarget::Bundle {
            bundle_id: "com.example".into(),
            name: "Example".into(),
        }];
        assert_eq!(
            format_active_tooltip(Some(90), &apps, false, None),
            "caffeinate2 (1m remaining · until Example quits)"
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

    #[test]
    fn format_active_tooltip_shows_upgrade_target() {
        let one = ["Claude Code".to_string()];
        assert_eq!(
            format_active_tooltip(None, &[], false, Some(&one[..])),
            "caffeinate2 (upgrading Claude Code)"
        );
        // An upgrade session with no known process names still reads sensibly.
        let none: [String; 0] = [];
        assert_eq!(
            format_active_tooltip(None, &[], false, Some(&none[..])),
            "caffeinate2 (upgrading external app)"
        );
        // A handful of apps are listed inline.
        let two = ["Claude Code".to_string(), "Codex".to_string()];
        assert_eq!(
            format_active_tooltip(None, &[], false, Some(&two[..])),
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
            format_active_tooltip(None, &[], false, Some(&many[..])),
            "caffeinate2 (upgrading A, B, C +2 more)"
        );
    }
}
