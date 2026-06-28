use std::fs::create_dir;

use config_traits::{StdConfig, StdConfigLoad1};
use serde::{Deserialize, Serialize};

use crate::notify::EnabledNotifications;

const CFG_DIR: &str = "rog";
const CFG_FILE_NAME: &str = "rog-control-center.cfg";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub run_in_background: bool,
    pub startup_in_background: bool,
    pub enable_tray_icon: bool,
    #[serde(default)]
    pub enable_autostart: bool,
    pub ac_command: String,
    pub bat_command: String,
    pub dark_mode: bool,
    // intended for use with devices like the ROG Ally
    pub start_fullscreen: bool,
    pub fullscreen_width: u32,
    pub fullscreen_height: u32,
    // This field must be last
    pub notifications: EnabledNotifications,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            run_in_background: true,
            startup_in_background: false,
            enable_tray_icon: true,
            enable_autostart: false,
            dark_mode: true,
            start_fullscreen: false,
            fullscreen_width: 1920,
            fullscreen_height: 1080,
            notifications: EnabledNotifications::default(),
            ac_command: String::new(),
            bat_command: String::new(),
        }
    }
}

impl StdConfig for Config {
    fn new() -> Self {
        Config {
            ..Default::default()
        }
    }

    fn file_name(&self) -> String {
        CFG_FILE_NAME.to_owned()
    }

    fn config_dir() -> std::path::PathBuf {
        let mut path = dirs::config_dir().unwrap_or_default();

        path.push(CFG_DIR);
        if !path.exists() {
            create_dir(path.clone())
                .map_err(|e| log::error!("Could not create config dir: {e}"))
                .ok();
            log::info!("Created {path:?}");
        }
        path
    }
}

impl StdConfigLoad1<Config461> for Config {}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config461 {
    pub run_in_background: bool,
    pub startup_in_background: bool,
    pub ac_command: String,
    pub bat_command: String,
    pub enable_dgpu_notifications: bool,
    pub dark_mode: bool,
    // This field must be last
    pub enabled_notifications: EnabledNotifications,
}

impl From<Config461> for Config {
    fn from(c: Config461) -> Self {
        Self {
            run_in_background: c.run_in_background,
            startup_in_background: c.startup_in_background,
            enable_tray_icon: true,
            enable_autostart: false,
            ac_command: c.ac_command,
            bat_command: c.bat_command,
            dark_mode: true,
            start_fullscreen: false,
            fullscreen_width: 1920,
            fullscreen_height: 1080,
            notifications: c.enabled_notifications,
        }
    }
}

pub fn update_autostart(enable: bool) {
    let autostart_dir = match dirs::config_dir() {
        Some(mut p) => {
            p.push("autostart");
            p
        }
        None => {
            log::error!("Could not find config directory for autostart");
            return;
        }
    };

    let desktop_file = autostart_dir.join("rog-control-center.desktop");

    if enable {
        if !autostart_dir.exists() {
            if let Err(e) = std::fs::create_dir_all(&autostart_dir) {
                log::error!("Failed to create autostart directory: {e}");
                return;
            }
        }

        let content = "[Desktop Entry]\n\
                       Version=1.0\n\
                       Type=Application\n\
                       Name=ROG Control Center\n\
                       Comment=Make your ASUS ROG Laptop go Brrrrr!\n\
                       Categories=Settings;\n\
                       Icon=rog-control-center\n\
                       Exec=rog-control-center\n\
                       Terminal=false\n";

        if let Err(e) = std::fs::write(&desktop_file, content) {
            log::error!("Failed to write autostart desktop file: {e}");
        } else {
            log::info!("Created autostart entry at {:?}", desktop_file);
        }
    } else {
        if desktop_file.exists() {
            if let Err(e) = std::fs::remove_file(&desktop_file) {
                log::error!("Failed to remove autostart desktop file: {e}");
            } else {
                log::info!("Removed autostart entry at {:?}", desktop_file);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_autostart() {
        let path = dirs::config_dir().map(|mut p| {
            p.push("autostart");
            p.push("rog-control-center.desktop");
            p
        });

        if let Some(path) = path {
            // Backup existing file if any
            let backup = if path.exists() {
                Some(std::fs::read_to_string(&path).unwrap())
            } else {
                None
            };

            // Test enabling
            update_autostart(true);
            assert!(path.exists());
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(content.contains("Name=ROG Control Center"));
            assert!(content.contains("Exec=rog-control-center"));

            // Test disabling
            update_autostart(false);
            assert!(!path.exists());

            // Restore backup if any
            if let Some(ref backup_content) = backup {
                std::fs::write(&path, backup_content).ok();
            }
        }
    }
}
