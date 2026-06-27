use std::collections::BTreeMap;

use config_traits::{StdConfig, StdConfigLoad};
use log::{debug, info, warn};
use rog_aura::aura_detection::LedSupportData;
use rog_aura::keyboard::LaptopAuraPower;
use rog_aura::{
    AuraDeviceType, AuraEffect, AuraModeNum, AuraZone, Direction, GRADIENT, LedBrightness, Speed,
};
use serde::{Deserialize, Serialize};

use crate::error::RogError;

#[derive(Deserialize, Serialize, Default, Debug, Clone)]
// #[serde(default)]
pub struct AuraConfig {
    #[serde(skip)]
    pub led_type: AuraDeviceType,
    #[serde(skip)]
    pub support_data: LedSupportData,
    pub config_name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ally_fix: Option<bool>,
    pub brightness: LedBrightness,
    pub current_mode: AuraModeNum,
    pub builtins: BTreeMap<AuraModeNum, AuraEffect>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub multizone: Option<BTreeMap<AuraModeNum, Vec<AuraEffect>>>,
    pub multizone_on: bool,
    pub enabled: LaptopAuraPower,
    #[serde(skip)]
    pub per_key_mode_active: bool,
}

impl StdConfig for AuraConfig {
    fn new() -> Self {
        panic!("This should not be used");
    }

    fn file_name(&self) -> String {
        if self.config_name.is_empty() {
            panic!("Config file name should not be empty");
        }
        self.config_name.to_owned()
    }

    fn config_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(crate::CONFIG_PATH_BASE)
    }
}

impl StdConfigLoad for AuraConfig {}

impl AuraConfig {
    /// Detect the keyboard type and load from default DB if data available
    pub fn new(prod_id: &str) -> Self {
        info!("Setting up AuraConfig for {prod_id:?}");
        // create a default config here
        let device_type = AuraDeviceType::from(prod_id);
        if device_type == AuraDeviceType::Unknown {
            warn!("idProduct:{prod_id:?} is unknown");
        }
        let support_data = LedSupportData::get_data(prod_id);
        let enabled = LaptopAuraPower::new(device_type, &support_data);
        let mut config = AuraConfig {
            led_type: device_type,
            support_data,
            config_name: format!("aura_{prod_id}.ron"),
            ally_fix: None,
            brightness: LedBrightness::Med,
            current_mode: AuraModeNum::Static,
            builtins: BTreeMap::new(),
            multizone: None,
            multizone_on: false,
            enabled,
            per_key_mode_active: false,
        };

        for n in &config.support_data.basic_modes {
            debug!("creating default for {n}");
            config
                .builtins
                .insert(*n, AuraEffect::default_with_mode(*n));
        }
        if !config.support_data.basic_zones.is_empty() {
            for n in &config.support_data.basic_modes {
                let mut default = vec![];
                for (i, tmp) in config.support_data.basic_zones.iter().enumerate() {
                    default.push(AuraEffect {
                        mode: *n,
                        zone: *tmp,
                        colour1: *GRADIENT.get(i).unwrap_or(&GRADIENT[0]),
                        colour2: *GRADIENT.get(GRADIENT.len() - i).unwrap_or(&GRADIENT[6]),
                        speed: Speed::Med,
                        direction: Direction::Left,
                    });
                }
                if let Some(m) = config.multizone.as_mut() {
                    m.insert(*n, default);
                } else {
                    let mut tmp = BTreeMap::new();
                    tmp.insert(*n, default);
                    config.multizone = Some(tmp);
                }
            }
        }
        config
    }

    /// Set the mode data, current mode, and if multizone enabled.
    ///
    /// Multipurpose, will accept `AuraEffect` with zones and put in the correct
    /// store.
    pub fn set_builtin(&mut self, effect: AuraEffect) {
        self.current_mode = effect.mode;
        if effect.zone() == AuraZone::None {
            self.builtins.insert(*effect.mode(), effect);
            self.multizone_on = false;
        } else {
            if let Some(multi) = self.multizone.as_mut() {
                if let Some(fx_vec) = multi.get_mut(effect.mode()) {
                    for fx in fx_vec.iter_mut() {
                        if fx.zone == effect.zone {
                            *fx = effect;
                            return;
                        }
                    }
                    fx_vec.push(effect);
                } else {
                    multi.insert(*effect.mode(), vec![effect]);
                }
            } else {
                let mut tmp = BTreeMap::new();
                tmp.insert(*effect.mode(), vec![effect]);
                self.multizone = Some(tmp);
            }
            self.multizone_on = true;
        }
    }

    pub fn get_multizone(&self, aura_type: AuraModeNum) -> Option<&[AuraEffect]> {
        if let Some(multi) = &self.multizone {
            return multi.get(&aura_type).map(|v| v.as_slice());
        }
        None
    }

    /// Create a default for the `current_mode` if multizone and no config
    /// exists.
    pub fn create_multizone_default(&mut self) -> Result<(), RogError> {
        let mut default = vec![];
        for (i, tmp) in self.support_data.basic_zones.iter().enumerate() {
            default.push(AuraEffect {
                mode: self.current_mode,
                zone: *tmp,
                colour1: *GRADIENT.get(i).unwrap_or(&GRADIENT[0]),
                colour2: *GRADIENT.get(GRADIENT.len() - i).unwrap_or(&GRADIENT[6]),
                speed: Speed::Med,
                direction: Direction::Left,
            });
        }
        if default.is_empty() {
            return Err(RogError::AuraEffectNotSupported);
        }

        if let Some(multizones) = self.multizone.as_mut() {
            multizones.insert(self.current_mode, default);
        } else {
            let mut tmp = BTreeMap::new();
            tmp.insert(self.current_mode, default);
            self.multizone = Some(tmp);
        }
        Ok(())
    }

    /// Reload the config from disk then verify and update it if required.
    /// Always rewrites the file to disk.
    pub fn load_and_update_config(prod_id: &str) -> AuraConfig {
        // New loads data from the DB also
        let mut config_init = AuraConfig::new(prod_id);
        // config_init.set_filename(prod_id);
        let mut config_loaded = config_init.clone().load();
        // update the initialised data with what we loaded from disk
        for mode_init in &mut config_init.builtins {
            // update init values from loaded values if they exist
            if let Some(loaded) = config_loaded.builtins.get(mode_init.0) {
                *mode_init.1 = loaded.clone();
            }
        }
        // Then replace just incase the initialised data contains new modes added
        config_loaded.builtins = config_init.builtins;
        config_loaded.support_data = config_init.support_data;
        config_loaded.led_type = config_init.led_type;
        config_loaded.ally_fix = config_init.ally_fix;

        for enabled_init in &mut config_init.enabled.states {
            for enabled in &mut config_loaded.enabled.states {
                if enabled.zone == enabled_init.zone {
                    *enabled_init = *enabled;
                    break;
                }
            }
        }
        config_loaded.enabled = config_init.enabled;

        if let (Some(mut multizone_init), Some(multizone_loaded)) =
            (config_init.multizone, config_loaded.multizone.as_mut())
        {
            for mode in multizone_init.iter_mut() {
                // update init values from loaded values if they exist
                if let Some(loaded) = multizone_loaded.get(mode.0) {
                    let mut new_set = Vec::new();
                    let data = LedSupportData::get_data(prod_id);
                    // only reuse a zone mode if the mode is supported
                    for mode in loaded {
                        if data.basic_modes.contains(&mode.mode) {
                            new_set.push(mode.clone());
                        }
                    }
                    *mode.1 = new_set;
                }
            }
            *multizone_loaded = multizone_init;
        }

        config_loaded.write();
        config_loaded
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use rog_aura::keyboard::AuraPowerState;
    use rog_aura::{
        AuraEffect, AuraModeNum, AuraZone, Colour, Direction, LedBrightness, PowerZones, Speed,
    };

    use super::AuraConfig;

    // Global mutex to serialize tests that rely on process-wide environment
    // variables
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

    fn test_lock() -> MutexGuard<'static, ()> {
        TEST_MUTEX
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("TEST_MUTEX poisoned")
    }

    #[test]
    fn set_multizone_4key_config() {
        let _guard = test_lock();
        unsafe {
            std::env::set_var("BOARD_NAME", "");
        }
        let mut config = AuraConfig::new("19b6");

        let effect = AuraEffect {
            colour1: Colour {
                r: 0xff,
                g: 0x00,
                b: 0xff,
            },
            zone: AuraZone::Key1,
            ..Default::default()
        };
        config.set_builtin(effect);

        assert!(config.multizone.is_some());

        let effect = AuraEffect {
            colour1: Colour {
                r: 0x00,
                g: 0xff,
                b: 0xff,
            },
            zone: AuraZone::Key2,
            ..Default::default()
        };
        config.set_builtin(effect);

        let effect = AuraEffect {
            colour1: Colour {
                r: 0xff,
                g: 0xff,
                b: 0x00,
            },
            zone: AuraZone::Key3,
            ..Default::default()
        };
        config.set_builtin(effect);

        let effect = AuraEffect {
            colour1: Colour {
                r: 0x00,
                g: 0xff,
                b: 0x00,
            },
            zone: AuraZone::Key4,
            ..Default::default()
        };
        let effect_clone = effect.clone();
        config.set_builtin(effect);
        // This should replace existing
        config.set_builtin(effect_clone);

        let res = config.multizone.unwrap();
        let sta = res.get(&AuraModeNum::Static).unwrap();
        assert_eq!(sta.len(), 4);
        assert_eq!(
            sta[0].colour1,
            Colour {
                r: 0xff,
                g: 0x00,
                b: 0xff
            }
        );
        assert_eq!(
            sta[1].colour1,
            Colour {
                r: 0x00,
                g: 0xff,
                b: 0xff
            }
        );
        assert_eq!(
            sta[2].colour1,
            Colour {
                r: 0xff,
                g: 0xff,
                b: 0x00
            }
        );
        assert_eq!(
            sta[3].colour1,
            Colour {
                r: 0x00,
                g: 0xff,
                b: 0x00
            }
        );
    }

    #[test]
    fn set_multizone_multimode_config() {
        let _guard = test_lock();
        unsafe {
            std::env::set_var("BOARD_NAME", "");
        }
        let mut config = AuraConfig::new("19b6");

        let effect = AuraEffect {
            zone: AuraZone::Key1,
            ..Default::default()
        };
        config.set_builtin(effect);

        assert!(config.multizone.is_some());

        let effect = AuraEffect {
            zone: AuraZone::Key2,
            mode: AuraModeNum::Breathe,
            ..Default::default()
        };
        config.set_builtin(effect);

        let effect = AuraEffect {
            zone: AuraZone::Key3,
            mode: AuraModeNum::Comet,
            ..Default::default()
        };
        config.set_builtin(effect);

        let effect = AuraEffect {
            zone: AuraZone::Key4,
            mode: AuraModeNum::Pulse,
            ..Default::default()
        };
        config.set_builtin(effect);

        let res = config.multizone.unwrap();
        let sta = res.get(&AuraModeNum::Static).unwrap();
        assert_eq!(sta.len(), 1);

        let sta = res.get(&AuraModeNum::Breathe).unwrap();
        assert_eq!(sta.len(), 1);

        let sta = res.get(&AuraModeNum::Comet).unwrap();
        assert_eq!(sta.len(), 1);

        let sta = res.get(&AuraModeNum::Pulse).unwrap();
        assert_eq!(sta.len(), 1);
    }

    #[test]
    fn verify_0x1866_g531i() {
        let _guard = test_lock();
        unsafe {
            std::env::set_var("BOARD_NAME", "G513I");
        }
        let mut config = AuraConfig::new("1866");

        assert_eq!(config.brightness, LedBrightness::Med);
        assert_eq!(config.builtins.len(), 5);
        assert_eq!(
            config.builtins.first_entry().unwrap().get(),
            &AuraEffect {
                mode: AuraModeNum::Static,
                zone: AuraZone::None,
                colour1: Colour { r: 166, g: 0, b: 0 },
                colour2: Colour { r: 0, g: 0, b: 0 },
                speed: Speed::Med,
                direction: Direction::Right
            }
        );
        assert_eq!(config.enabled.states.len(), 2);
        assert_eq!(
            config.enabled.states[0],
            AuraPowerState {
                zone: PowerZones::Keyboard,
                boot: true,
                awake: true,
                sleep: true,
                shutdown: true
            }
        );
        assert_eq!(
            config.enabled.states[1],
            AuraPowerState {
                zone: PowerZones::Lightbar,
                boot: true,
                awake: true,
                sleep: true,
                shutdown: true
            }
        );
    }

    #[test]
    fn verify_0x19b6_g634j() {
        let _guard = test_lock();
        unsafe {
            std::env::set_var("BOARD_NAME", "G634J");
        }
        let mut config = AuraConfig::new("19b6");

        assert_eq!(config.brightness, LedBrightness::Med);
        assert_eq!(config.builtins.len(), 12);
        assert_eq!(
            config.builtins.first_entry().unwrap().get(),
            &AuraEffect {
                mode: AuraModeNum::Static,
                zone: AuraZone::None,
                colour1: Colour { r: 166, g: 0, b: 0 },
                colour2: Colour { r: 0, g: 0, b: 0 },
                speed: Speed::Med,
                direction: Direction::Right
            }
        );
        assert_eq!(config.enabled.states.len(), 4);
        assert_eq!(
            config.enabled.states[0],
            AuraPowerState {
                zone: PowerZones::Keyboard,
                boot: true,
                awake: true,
                sleep: true,
                shutdown: true
            }
        );
    }
}
