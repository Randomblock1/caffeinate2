use crate::power_management::{self, AssertionType, PowerAssertion};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TrayMode {
    Display,
    Disk,
    #[default]
    System,
    SystemOnAc,
    UserActive,
    Entirely,
}

impl TrayMode {
    pub fn label(self) -> &'static str {
        match self {
            TrayMode::Display => "Display",
            TrayMode::Disk => "Disk",
            TrayMode::System => "System",
            TrayMode::SystemOnAc => "System (on AC)",
            TrayMode::UserActive => "User active",
            TrayMode::Entirely => "Entirely",
        }
    }

    pub fn all() -> [TrayMode; 6] {
        [
            TrayMode::Display,
            TrayMode::Disk,
            TrayMode::System,
            TrayMode::SystemOnAc,
            TrayMode::UserActive,
            TrayMode::Entirely,
        ]
    }

    pub fn enable(self) -> Result<ActiveMode, u32> {
        match self {
            TrayMode::Display => Ok(ActiveMode::Assertion(power_management::create_assertion(
                AssertionType::PreventUserIdleDisplaySleep,
                true,
                false,
            )?)),
            TrayMode::Disk => Ok(ActiveMode::Assertion(power_management::create_assertion(
                AssertionType::PreventDiskIdle,
                true,
                false,
            )?)),
            TrayMode::System => Ok(ActiveMode::Assertion(power_management::create_assertion(
                AssertionType::PreventUserIdleSystemSleep,
                true,
                false,
            )?)),
            TrayMode::SystemOnAc => Ok(ActiveMode::Assertion(power_management::create_assertion(
                AssertionType::PreventSystemSleep,
                true,
                false,
            )?)),
            TrayMode::UserActive => Ok(ActiveMode::Assertion(
                power_management::declare_user_activity(true, false)?,
            )),
            TrayMode::Entirely => {
                let client = crate::helper_ipc::HelperClient::new();
                if !client.is_available() {
                    return Err(0);
                }
                client.hold().map_err(|_| 0u32)?;
                Ok(ActiveMode::EntirelyHold(client))
            }
        }
    }
}

pub enum ActiveMode {
    Assertion(PowerAssertion),
    EntirelyHold(crate::helper_ipc::HelperClient),
}

impl Drop for ActiveMode {
    fn drop(&mut self) {
        match self {
            ActiveMode::Assertion(_) => {}
            ActiveMode::EntirelyHold(client) => {
                if let Err(e) = client.release() {
                    eprintln!("Error releasing entirely hold: {e}");
                }
            }
        }
    }
}

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

/// Stop sleep prevention when no running instance of this bundle remains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitForApp {
    pub bundle_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayConfig {
    pub mode: TrayMode,
    pub time_limit_secs: Option<u64>,
    pub wait_for_app: Option<WaitForApp>,
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            mode: TrayMode::default(),
            time_limit_secs: None,
            wait_for_app: None,
        }
    }
}

pub fn format_remaining_secs(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m remaining")
        } else {
            format!("{hours}h remaining")
        }
    } else if minutes > 0 {
        format!("{minutes}m remaining")
    } else {
        format!("{seconds}s remaining")
    }
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
    parse_config(&content).unwrap_or_default()
}

fn parse_quoted_value(value: &str) -> Option<String> {
    let value = value.trim();
    if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        Some(value[1..value.len() - 1].to_string())
    } else {
        None
    }
}

fn parse_config(content: &str) -> Option<TrayConfig> {
    let mut config = TrayConfig::default();
    let mut any = false;

    for line in content.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "mode" => {
                let value = value.trim().trim_matches('"');
                config.mode = match value {
                    "display" => TrayMode::Display,
                    "disk" => TrayMode::Disk,
                    "system" => TrayMode::System,
                    "system_on_ac" => TrayMode::SystemOnAc,
                    "user_active" => TrayMode::UserActive,
                    "entirely" => TrayMode::Entirely,
                    _ => return None,
                };
                any = true;
            }
            "time_limit_secs" => {
                config.time_limit_secs = value.trim().parse().ok();
                any = true;
            }
            "wait_for_bundle_id" => {
                let bundle_id = parse_quoted_value(value)?;
                if bundle_id.is_empty() {
                    config.wait_for_app = None;
                } else {
                    let name = config
                        .wait_for_app
                        .as_ref()
                        .map(|a| a.name.clone())
                        .unwrap_or_else(|| bundle_id.clone());
                    config.wait_for_app = Some(WaitForApp { bundle_id, name });
                }
                any = true;
            }
            "wait_for_app_name" => {
                if let Some(name) = parse_quoted_value(value) {
                    if let Some(app) = &mut config.wait_for_app {
                        app.name = name;
                    }
                }
                any = true;
            }
            _ => {}
        }
    }

    any.then_some(config)
}

pub fn save_config(config: &TrayConfig) -> Result<(), String> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut content = format!("mode = \"{}\"\n", serde_variant(config.mode));
    if let Some(secs) = config.time_limit_secs {
        content.push_str(&format!("time_limit_secs = {secs}\n"));
    }
    if let Some(app) = &config.wait_for_app {
        content.push_str(&format!(
            "wait_for_bundle_id = \"{}\"\n",
            app.bundle_id.replace('\\', "\\\\").replace('"', "\\\"")
        ));
        content.push_str(&format!(
            "wait_for_app_name = \"{}\"\n",
            app.name.replace('\\', "\\\\").replace('"', "\\\"")
        ));
    }
    std::fs::write(path, content).map_err(|e| e.to_string())
}

/// Build menu bar tooltip text while sleep prevention is active.
pub fn format_active_tooltip(
    remaining_secs: Option<u64>,
    wait_for_app: Option<&WaitForApp>,
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

fn serde_variant(mode: TrayMode) -> &'static str {
    match mode {
        TrayMode::Display => "display",
        TrayMode::Disk => "disk",
        TrayMode::System => "system",
        TrayMode::SystemOnAc => "system_on_ac",
        TrayMode::UserActive => "user_active",
        TrayMode::Entirely => "entirely",
    }
}

#[cfg(all(test, feature = "tray"))]
mod tests {
    use super::*;

    #[test]
    fn parse_config_defaults_time_limit_to_off() {
        let config = parse_config("mode = \"system\"\n").unwrap();
        assert_eq!(config.mode, TrayMode::System);
        assert_eq!(config.time_limit_secs, None);
    }

    #[test]
    fn parse_config_reads_time_limit() {
        let config = parse_config("mode = \"display\"\ntime_limit_secs = 1800\n").unwrap();
        assert_eq!(config.mode, TrayMode::Display);
        assert_eq!(config.time_limit_secs, Some(1800));
    }

    #[test]
    fn format_remaining_secs_display() {
        assert_eq!(super::format_remaining_secs(45), "45s remaining");
        assert_eq!(super::format_remaining_secs(90), "1m remaining");
        assert_eq!(super::format_remaining_secs(3661), "1h 1m remaining");
    }

    #[test]
    fn parse_config_reads_wait_for_app() {
        let config = parse_config(
            "mode = \"system\"\nwait_for_bundle_id = \"com.example.app\"\nwait_for_app_name = \"Example\"\n",
        )
        .unwrap();
        let app = config.wait_for_app.unwrap();
        assert_eq!(app.bundle_id, "com.example.app");
        assert_eq!(app.name, "Example");
    }

    #[test]
    fn format_active_tooltip_combines_limit_and_app() {
        let app = WaitForApp {
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
