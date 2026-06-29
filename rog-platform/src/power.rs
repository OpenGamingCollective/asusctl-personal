use std::path::PathBuf;

use log::{info, warn};

use crate::error::{PlatformError, Result};
use crate::{attr_num, to_device};

/// The "platform" device provides access to things like:
/// - `dgpu_disable`
/// - `egpu_enable`
/// - `panel_od`
/// - `gpu_mux`
/// - `keyboard_mode`, set keyboard RGB mode and speed
/// - `keyboard_state`, set keyboard power states
#[derive(Debug, PartialEq, Eq, PartialOrd, Clone)]
pub struct AsusPower {
    mains: PathBuf,
    battery: PathBuf,
    usb: Option<PathBuf>,
}

impl AsusPower {
    attr_num!("charge_control_end_threshold", battery, u8);

    attr_num!("online", mains, u8);

    /// When checking for battery this will look in order:
    /// - if attr `manufacturer` contains `asus`
    /// - if attr `charge_control_end_threshold` exists and `energy_full_design`
    ///   >= 50 watt
    /// - if syspath end conatins `BAT`
    /// - if attr `type` is `battery` (last resort)
    pub fn new() -> Result<Self> {
        let mut mains = PathBuf::new();
        let mut battery = None;
        let mut usb = None;

        let mut enumerator = udev::Enumerator::new().map_err(|err| {
            warn!("{}", err);
            PlatformError::Udev("enumerator failed".into(), err)
        })?;
        enumerator.match_subsystem("power_supply").map_err(|err| {
            warn!("{}", err);
            PlatformError::Udev("match_subsystem failed".into(), err)
        })?;

        for device in enumerator.scan_devices().map_err(|err| {
            warn!("{}", err);
            PlatformError::Udev("scan_devices failed".into(), err)
        })? {
            if let Some(attr) = device.attribute_value("type") {
                info!("Power: Checking {:?}", device.syspath());
                match attr.to_string_lossy().to_ascii_lowercase().trim() {
                    "mains" => {
                        info!("Found mains power at {:?}", device.sysname());
                        mains = device.syspath().to_path_buf();
                    }
                    "battery" => {
                        // Priortised list of checks
                        info!("Found a battery");
                        if battery.is_none() {
                            info!("Checking battery attributes");
                            if let Some(current) =
                                device.attribute_value("charge_control_end_threshold")
                            {
                                info!(
                                    "Found battery power at {:?}, matched \
                                     charge_control_end_threshold. Current level: {current:?}",
                                    device.sysname()
                                );
                                battery = Some(device.syspath().to_path_buf());
                            } else if device.sysname().to_string_lossy().starts_with("BAT") {
                                info!(
                                    "Found battery power at {:?}, sysfs path ended with BAT<n>",
                                    device.sysname()
                                );
                                battery = Some(device.syspath().to_path_buf());
                            } else {
                                info!(
                                    "Last resort: Found battery power at {:?} using type = Battery",
                                    device.sysname()
                                );
                                battery = Some(device.syspath().to_path_buf());
                            }
                        }
                    }
                    "usb" => {
                        info!("Found USB-C power at {:?}", device.sysname());
                        usb = Some(device.syspath().to_path_buf());
                    }
                    _ => {}
                };
            }
        }

        if let Some(battery) = battery {
            return Ok(Self {
                mains,
                battery,
                usb,
            });
        }

        // No battery found. Return an AsusPower with an empty battery path so
        // callers can still be constructed and query `has_*` methods which
        // will correctly report absence. This avoids hard-failing on systems
        // where the asus-nb-wmi driver loads on desktops with no battery.
        info!("Did not find a battery, continuing without battery support");
        Ok(Self {
            mains,
            battery: PathBuf::new(),
            usb,
        })
    }

    pub fn has_battery(&self) -> bool {
        !self.battery.as_os_str().is_empty()
    }

    pub fn get_battery_cycle_count(&self) -> Result<i32> {
        let path = self.battery.join("cycle_count");
        if !path.exists() {
            return Err(PlatformError::Read(
                path.to_string_lossy().into(),
                std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"),
            ));
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| PlatformError::Read(path.to_string_lossy().into(), e))?;
        content.trim().parse::<i32>().map_err(|e| {
            PlatformError::Read(
                path.to_string_lossy().into(),
                std::io::Error::new(std::io::ErrorKind::InvalidData, e),
            )
        })
    }

    pub fn get_battery_health(&self) -> Result<u8> {
        let full = if self.battery.join("energy_full").exists() {
            let content = std::fs::read_to_string(self.battery.join("energy_full"))
                .map_err(|e| PlatformError::Read("energy_full".into(), e))?;
            content.trim().parse::<f64>().ok()
        } else if self.battery.join("charge_full").exists() {
            let content = std::fs::read_to_string(self.battery.join("charge_full"))
                .map_err(|e| PlatformError::Read("charge_full".into(), e))?;
            content.trim().parse::<f64>().ok()
        } else {
            None
        };

        let design = if self.battery.join("energy_full_design").exists() {
            let content = std::fs::read_to_string(self.battery.join("energy_full_design"))
                .map_err(|e| PlatformError::Read("energy_full_design".into(), e))?;
            content.trim().parse::<f64>().ok()
        } else if self.battery.join("charge_full_design").exists() {
            let content = std::fs::read_to_string(self.battery.join("charge_full_design"))
                .map_err(|e| PlatformError::Read("charge_full_design".into(), e))?;
            content.trim().parse::<f64>().ok()
        } else {
            None
        };

        match (full, design) {
            (Some(f), Some(d)) if d > 0.0 => {
                let health = (f / d * 100.0).round().clamp(0.0, 100.0) as u8;
                Ok(health)
            }
            _ => Err(PlatformError::Read(
                self.battery.to_string_lossy().into(),
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "energy/charge attributes not found",
                ),
            )),
        }
    }

    pub fn get_battery_power_consumption(&self) -> Result<f32> {
        if self.battery.join("power_now").exists() {
            let content = std::fs::read_to_string(self.battery.join("power_now"))
                .map_err(|e| PlatformError::Read("power_now".into(), e))?;
            let power = content.trim().parse::<f32>().map_err(|e| {
                PlatformError::Read(
                    "power_now".into(),
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                )
            })?;
            Ok(power / 1_000_000.0)
        } else if self.battery.join("current_now").exists()
            && self.battery.join("voltage_now").exists()
        {
            let current_str = std::fs::read_to_string(self.battery.join("current_now"))
                .map_err(|e| PlatformError::Read("current_now".into(), e))?;
            let voltage_str = std::fs::read_to_string(self.battery.join("voltage_now"))
                .map_err(|e| PlatformError::Read("voltage_now".into(), e))?;
            let current = current_str.trim().parse::<f32>().map_err(|e| {
                PlatformError::Read(
                    "current_now".into(),
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                )
            })?;
            let voltage = voltage_str.trim().parse::<f32>().map_err(|e| {
                PlatformError::Read(
                    "voltage_now".into(),
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                )
            })?;
            Ok((current * voltage) / 1_000_000_000.0)
        } else {
            Err(PlatformError::Read(
                self.battery.to_string_lossy().into(),
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "power/current/voltage attributes not found",
                ),
            ))
        }
    }

    pub fn get_battery_status(&self) -> Result<String> {
        let path = self.battery.join("status");
        if !path.exists() {
            return Err(PlatformError::Read(
                path.to_string_lossy().into(),
                std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"),
            ));
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| PlatformError::Read(path.to_string_lossy().into(), e))?;
        Ok(content.trim().to_string())
    }

    pub fn get_battery_remaining_energy_wh(&self) -> Result<f64> {
        if self.battery.join("energy_now").exists() {
            let content = std::fs::read_to_string(self.battery.join("energy_now"))
                .map_err(|e| PlatformError::Read("energy_now".into(), e))?;
            let val = content.trim().parse::<f64>().map_err(|e| {
                PlatformError::Read(
                    "energy_now".into(),
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                )
            })?;
            Ok(val / 1_000_000.0)
        } else if self.battery.join("charge_now").exists()
            && self.battery.join("voltage_now").exists()
        {
            let charge_str = std::fs::read_to_string(self.battery.join("charge_now"))
                .map_err(|e| PlatformError::Read("charge_now".into(), e))?;
            let voltage_str = std::fs::read_to_string(self.battery.join("voltage_now"))
                .map_err(|e| PlatformError::Read("voltage_now".into(), e))?;
            let charge = charge_str.trim().parse::<f64>().map_err(|e| {
                PlatformError::Read(
                    "charge_now".into(),
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                )
            })?;
            let voltage = voltage_str.trim().parse::<f64>().map_err(|e| {
                PlatformError::Read(
                    "voltage_now".into(),
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                )
            })?;
            Ok((charge * voltage) / 1_000_000_000.0)
        } else {
            Err(PlatformError::Read(
                self.battery.to_string_lossy().into(),
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "energy_now/charge_now attributes not found",
                ),
            ))
        }
    }

    pub fn get_battery_full_energy_wh(&self) -> Result<f64> {
        if self.battery.join("energy_full").exists() {
            let content = std::fs::read_to_string(self.battery.join("energy_full"))
                .map_err(|e| PlatformError::Read("energy_full".into(), e))?;
            let val = content.trim().parse::<f64>().map_err(|e| {
                PlatformError::Read(
                    "energy_full".into(),
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                )
            })?;
            Ok(val / 1_000_000.0)
        } else if self.battery.join("charge_full").exists()
            && self.battery.join("voltage_now").exists()
        {
            let charge_str = std::fs::read_to_string(self.battery.join("charge_full"))
                .map_err(|e| PlatformError::Read("charge_full".into(), e))?;
            let voltage_str = std::fs::read_to_string(self.battery.join("voltage_now"))
                .map_err(|e| PlatformError::Read("voltage_now".into(), e))?;
            let charge = charge_str.trim().parse::<f64>().map_err(|e| {
                PlatformError::Read(
                    "charge_full".into(),
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                )
            })?;
            let voltage = voltage_str.trim().parse::<f64>().map_err(|e| {
                PlatformError::Read(
                    "voltage_now".into(),
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                )
            })?;
            Ok((charge * voltage) / 1_000_000_000.0)
        } else {
            Err(PlatformError::Read(
                self.battery.to_string_lossy().into(),
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "energy_full/charge_full attributes not found",
                ),
            ))
        }
    }

    pub fn get_battery_time_estimate(&self) -> Result<Option<(bool, i32, i32)>> {
        let status = self.get_battery_status()?;
        let is_charging = match status.as_str() {
            "Charging" => true,
            "Discharging" => false,
            _ => return Ok(None),
        };

        let power_draw = self.get_battery_power_consumption().unwrap_or(0.0).abs();
        if power_draw < 0.1 {
            return Ok(None);
        }

        let energy_now = self.get_battery_remaining_energy_wh()?;
        let energy_full = self.get_battery_full_energy_wh()?;

        let remaining_wh = if is_charging {
            let limit = self.get_charge_control_end_threshold().unwrap_or(100) as f64;
            let target_wh = energy_full * (limit / 100.0);
            if energy_now >= target_wh {
                return Ok(None);
            }
            target_wh - energy_now
        } else {
            energy_now
        };

        let hours_float = remaining_wh / power_draw as f64;
        if hours_float < 0.0 || hours_float.is_nan() || hours_float.is_infinite() {
            return Ok(None);
        }

        let total_minutes = (hours_float * 60.0).round() as i32;
        let hours = total_minutes / 60;
        let minutes = total_minutes % 60;

        Ok(Some((is_charging, hours, minutes)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn test_battery_methods() {
        let temp_dir = std::env::temp_dir().join("fake_battery");
        fs::create_dir_all(&temp_dir).unwrap();

        // Write fake files
        fs::write(temp_dir.join("cycle_count"), "42\n").unwrap();
        fs::write(temp_dir.join("energy_full"), "80000000\n").unwrap();
        fs::write(temp_dir.join("energy_full_design"), "100000000\n").unwrap();
        fs::write(temp_dir.join("energy_now"), "45000000\n").unwrap();
        fs::write(temp_dir.join("power_now"), "15000000\n").unwrap();
        fs::write(temp_dir.join("status"), "Discharging\n").unwrap();

        let power = AsusPower {
            mains: PathBuf::new(),
            battery: temp_dir.clone(),
            usb: None,
        };

        assert!(power.has_battery());
        assert_eq!(power.get_battery_cycle_count().unwrap(), 42);
        assert_eq!(power.get_battery_health().unwrap(), 80);
        assert_eq!(power.get_battery_power_consumption().unwrap(), 15.0);
        assert_eq!(power.get_battery_status().unwrap(), "Discharging");
        assert_eq!(power.get_battery_remaining_energy_wh().unwrap(), 45.0);
        assert_eq!(power.get_battery_full_energy_wh().unwrap(), 80.0);
        assert_eq!(
            power.get_battery_time_estimate().unwrap(),
            Some((false, 3, 0))
        );

        // Test charging estimation
        fs::write(temp_dir.join("status"), "Charging\n").unwrap();
        assert_eq!(
            power.get_battery_time_estimate().unwrap(),
            Some((true, 2, 20))
        );

        // Clean up
        fs::remove_dir_all(&temp_dir).ok();
    }
}
