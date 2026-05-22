use crate::app_target::AppTarget;
use crate::duration_parser::format_remaining_secs;
use crate::sleep_mode::{ActiveSleepHold, SleepMode};
use serde::{Deserialize, Serialize};

pub use crate::sleep_mode::SleepMode as TrayMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeLimitPreset {
    pub label: &'static str,
    pub seconds: Option<u64>,
}

impl TimeLimitPreset {
    pub const ALL: [TimeLimitPreset; 7] = [
        TimeLimitPreset {
            label: "Off",
            seconds: None,
        },
        TimeLimitPreset {
            label: "15 minutes",
            seconds: Some(15 * 60),
        },
        TimeLimitPreset {
            label: "30 minutes",
            seconds: Some(30 * 60),
        },
        TimeLimitPreset {
            label: "1 hour",
            seconds: Some(60 * 60),
        },
        TimeLimitPreset {
            label: "2 hours",
            seconds: Some(2 * 60 * 60),
        },
        TimeLimitPreset {
            label: "4 hours",
            seconds: Some(4 * 60 * 60),
        },
        TimeLimitPreset {
            label: "8 hours",
            seconds: Some(8 * 60 * 60),
        },
    ];
}

pub type ActiveMode = ActiveSleepHold;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TrayConfig {
    #[serde(default)]
    pub mode: SleepMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_limit_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for_app: Option<AppTarget>,
}

pub fn config_path() -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())?;
    Ok(home
        .join("Library/Application Support/caffeinate2")
        .join("tray.toml"))
}

pub fn load_config() -> TrayConfig {
    let path = match config_path() {
        Ok(p) => p,
        Err(_) => return TrayConfig::default(),
    };
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return TrayConfig::default(),
    };
    match toml::from_str::<TrayConfig>(&content) {
        Ok(config) => config,
        Err(_) => migrate_legacy_config(&content).unwrap_or_default(),
    }
}

fn migrate_legacy_config(content: &str) -> Option<TrayConfig> {
    let mut config = TrayConfig::default();
    let mut any = false;
    let mut bundle_id = None::<String>;
    let mut app_name = None::<String>;

    for line in content.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "mode" => {
                let value = value.trim().trim_matches('"');
                config.mode = toml::from_str(&format!("\"{value}\"")).ok()?;
                any = true;
            }
            "time_limit_secs" => {
                config.time_limit_secs = value.trim().parse().ok();
                any = true;
            }
            "wait_for_bundle_id" => {
                bundle_id = Some(value.trim().trim_matches('"').to_string());
                any = true;
            }
            "wait_for_app_name" => {
                app_name = Some(value.trim().trim_matches('"').to_string());
                any = true;
            }
            _ => {}
        }
    }

    if let Some(id) = bundle_id.filter(|s| !s.is_empty()) {
        config.wait_for_app = Some(AppTarget {
            bundle_id: id.clone(),
            name: app_name.unwrap_or(id),
        });
    }

    any.then_some(config)
}

pub fn save_config(config: &TrayConfig) -> Result<(), String> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = toml::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(path, content).map_err(|e| e.to_string())
}

/// Build menu bar tooltip text while sleep prevention is active.
pub fn format_active_tooltip(
    remaining_secs: Option<u64>,
    wait_for_app: Option<&AppTarget>,
    waiting_for_app_launch: bool,
) -> String {
    let mut parts = Vec::new();
    if let Some(secs) = remaining_secs.filter(|&s| s > 0) {
        parts.push(format_remaining_secs(secs));
    }
    if let Some(app) = wait_for_app {
        if waiting_for_app_launch {
            parts.push(format!("waiting for {}", app.name));
        } else {
            parts.push(format!("until {} quits", app.name));
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
    fn migrate_legacy_flat_wait_for_app_keys() {
        let config = migrate_legacy_config(
            "mode = \"system\"\nwait_for_bundle_id = \"com.example.app\"\nwait_for_app_name = \"Example\"\n",
        )
        .unwrap();
        let app = config.wait_for_app.unwrap();
        assert_eq!(app.bundle_id, "com.example.app");
        assert_eq!(app.name, "Example");
    }

    #[test]
    fn parse_config_reads_wait_for_app() {
        let config: TrayConfig = toml::from_str(
            r#"
mode = "system"

[wait_for_app]
bundle_id = "com.example.app"
name = "Example"
"#,
        )
        .unwrap();
        let app = config.wait_for_app.unwrap();
        assert_eq!(app.bundle_id, "com.example.app");
        assert_eq!(app.name, "Example");
    }

    #[test]
    fn format_active_tooltip_combines_limit_and_app() {
        let app = AppTarget {
            bundle_id: "com.example".into(),
            name: "Example".into(),
        };
        assert_eq!(
            format_active_tooltip(Some(90), Some(&app), false),
            "caffeinate2 (1m remaining · until Example quits)"
        );
        assert_eq!(
            format_active_tooltip(None, Some(&app), true),
            "caffeinate2 (waiting for Example)"
        );
    }
}
