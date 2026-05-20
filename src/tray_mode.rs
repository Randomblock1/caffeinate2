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
                client.hold().map_err(|_| 0)?;
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

pub fn config_path() -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())?;
    Ok(home
        .join("Library/Application Support/caffeinate2")
        .join("tray.toml"))
}

pub fn load_config() -> TrayMode {
    let path = match config_path() {
        Ok(p) => p,
        Err(_) => return TrayMode::default(),
    };
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return TrayMode::default(),
    };
    parse_config_mode(&content).unwrap_or_default()
}

fn parse_config_mode(content: &str) -> Option<TrayMode> {
    for line in content.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "mode" {
                let value = value.trim().trim_matches('"');
                return Some(match value {
                    "display" => TrayMode::Display,
                    "disk" => TrayMode::Disk,
                    "system" => TrayMode::System,
                    "system_on_ac" => TrayMode::SystemOnAc,
                    "user_active" => TrayMode::UserActive,
                    "entirely" => TrayMode::Entirely,
                    _ => return None,
                });
            }
        }
    }
    None
}

pub fn save_config(mode: TrayMode) -> Result<(), String> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = format!("mode = \"{}\"\n", serde_variant(mode));
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
