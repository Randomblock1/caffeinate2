mod app;
pub mod app_target;
mod assertion_watch;
mod error;
pub mod macos_activation;
pub mod macos_apps;
mod menu;
pub mod process_enum;
mod single_instance;
mod state;
pub mod tray_icons;
pub mod tray_mode;
mod upgrade_dialog;
mod wait_window;

pub use app::run;
pub use error::TrayError;
pub use single_instance::{InstanceProbe, probe};
