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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrayConfig {
    pub mode: TrayMode,
    pub time_limit_secs: Option<u64>,
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            mode: TrayMode::default(),
            time_limit_secs: None,
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

fn parse_config(content: &str) -> Option<TrayConfig> {
    let mut config = TrayConfig::default();
    let mut found_mode = false;

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
                found_mode = true;
            }
            "time_limit_secs" => {
                config.time_limit_secs = value.trim().parse().ok();
            }
            _ => {}
        }
    }

    found_mode.then_some(config)
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
    std::fs::write(path, content).map_err(|e| e.to_string())
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
    fn format_remaining_secs() {
        assert_eq!(format_remaining_secs(45), "45s remaining");
        assert_eq!(format_remaining_secs(90), "1m remaining");
        assert_eq!(format_remaining_secs(3661), "1h 1m remaining");
    }
}
