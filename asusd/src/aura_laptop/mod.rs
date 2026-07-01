use std::sync::Arc;

use config::AuraConfig;
use config_traits::StdConfig;
use log::info;
use rog_aura::keyboard::{AuraLaptopUsbPackets, LedUsbPackets};
use rog_aura::usb::{AURA_LAPTOP_LED_APPLY, AURA_LAPTOP_LED_SET};
use rog_aura::{AuraDeviceType, AuraEffect, PowerZones, AURA_LAPTOP_LED_MSG_LEN, LedBrightness};
use rog_platform::hid_raw::HidRaw;
use rog_platform::keyboard_led::KeyboardBacklight;
use tokio::sync::{Mutex, MutexGuard};

use crate::error::RogError;

pub mod config;
pub mod trait_impls;

/// Aura keyboard controller for USB/asus-wmi path.
///
/// This is the legacy path: HID over USB with the ASUS proprietary aura
/// protocol, or TUF sysfs backlight. LampArray devices (HID Usage Page 0x59
/// on I2C-HID) live in the sibling module `aura_lamparray` and no longer
/// pollute this struct with `is_lamparray` branches.
#[derive(Debug, Clone)]
pub struct Aura {
    pub hid: Option<Arc<Mutex<HidRaw>>>,
    pub backlight: Option<Arc<Mutex<KeyboardBacklight>>>,
    pub config: Arc<Mutex<AuraConfig>>,
}

impl Aura {
    /// Initialise the device if required.
    pub async fn do_initialization(&self) -> Result<(), RogError> {
        Ok(())
    }

    pub async fn lock_config(&self) -> MutexGuard<'_, AuraConfig> {
        self.config.lock().await
    }

    /// Will lock the internal config and update. If anything else has locked
    /// this in scope then a deadlock can occur.
    pub async fn update_config(&self) -> Result<(), RogError> {
        let mut config = self.config.lock().await;
        let bright = if let Some(bl) = self.backlight.as_ref() {
            bl.lock().await.get_brightness().unwrap_or_default()
        } else {
            config.brightness.into()
        };
        config.read();
        config.brightness = bright.into();
        config.write();
        Ok(())
    }

    pub async fn write_current_config_mode(&self, config: &mut AuraConfig) -> Result<(), RogError> {
        if config.multizone_on {
            let mode = config.current_mode;
            let mut create = false;
            // There is no multizone config for this mode so create one here
            // using the colours of rainbow if it exists, or first available
            // mode, or random
            if config.multizone.is_none() {
                create = true;
            } else if let Some(multizones) = config.multizone.as_ref() {
                if !multizones.contains_key(&mode) {
                    create = true;
                }
            }
            if create {
                info!("No user-set config for zone founding, attempting a default");
                config.create_multizone_default()?;
            }
            if let Some(multizones) = config.multizone.as_mut() {
                if let Some(set) = multizones.get(&mode) {
                    for mode in set.clone() {
                        self.write_effect_and_apply(config.led_type, &mode).await?;
                    }
                }
            }
        } else {
            let mode = config.current_mode;
            if let Some(effect) = config.builtins.get(&mode).cloned() {
                self.write_effect_and_apply(config.led_type, &effect)
                    .await?;
            }
        }
        Ok(())
    }

    /// Write the AuraEffect to the device. Will lock `backlight` or `hid`.
    ///
    /// If per-key or software-mode is active it must be marked as disabled in
    /// config.
    pub async fn write_effect_and_apply(
        &self,
        dev_type: AuraDeviceType,
        mode: &AuraEffect,
    ) -> Result<(), RogError> {
        if matches!(dev_type, AuraDeviceType::LaptopKeyboardTuf) {
            if let Some(platform) = &self.backlight {
                let buf = [
                    1, mode.mode as u8, mode.colour1.r, mode.colour1.g, mode.colour1.b,
                    mode.speed as u8,
                ];
                platform.lock().await.set_kbd_rgb_mode(&buf)?;
            }
        } else if let Some(hid_raw) = &self.hid {
            // Some keyboard controllers (e.g. G533QS firmware) silently drop
            // short HID writes and only honour packets matching the OUTPUT
            // report size declared in the HID descriptor (64 bytes for the
            // 0x5d report). Pad effect/SET/APPLY here so we keep working on
            // newer Strix/Zephyrus models without regressing older laptops.
            const PADDED_LEN: usize = 64;
            let bytes: [u8; AURA_LAPTOP_LED_MSG_LEN] = mode.into();
            let mut effect_padded = [0u8; PADDED_LEN];
            effect_padded[..AURA_LAPTOP_LED_MSG_LEN].copy_from_slice(&bytes);
            let mut set_padded = [0u8; PADDED_LEN];
            set_padded[..AURA_LAPTOP_LED_MSG_LEN].copy_from_slice(&AURA_LAPTOP_LED_SET);
            let mut apply_padded = [0u8; PADDED_LEN];
            apply_padded[..AURA_LAPTOP_LED_MSG_LEN].copy_from_slice(&AURA_LAPTOP_LED_APPLY);
            let hid_raw = hid_raw.lock().await;
            hid_raw.write_bytes(&effect_padded)?;
            hid_raw.write_bytes(&set_padded)?;
            // Changes won't persist unless apply is set
            hid_raw.write_bytes(&apply_padded)?;
        } else {
            return Err(RogError::NoAuraKeyboard);
        }
        Ok(())
    }

    pub async fn set_brightness(&self, value: u8) -> Result<(), RogError> {
        if let Some(backlight) = &self.backlight {
            backlight.lock().await.set_brightness(value)?;
            return Ok(());
        }
        Err(RogError::MissingFunction(
            "No LED backlight control available".to_string(),
        ))
    }

    /// Set combination state for boot animation/sleep animation/all leds/keys
    /// leds/side leds LED active
    pub async fn set_power_states(&self, config: &AuraConfig) -> Result<(), RogError> {
        if matches!(config.led_type, rog_aura::AuraDeviceType::LaptopKeyboardTuf) {
            if let Some(backlight) = &self.backlight {
                // TODO: tuf bool array
                let buf = config.enabled.to_bytes(config.led_type);
                backlight.lock().await.set_kbd_rgb_state(&buf)?;
            }
        } else if let Some(hid_raw) = &self.hid {
            let hid_raw = hid_raw.lock().await;
            if let Some(p) = config.enabled.states.first() {
                if p.zone == PowerZones::Ally {
                    let msg = [
                        0x5d,
                        0xd1,
                        0x09,
                        0x01,
                        p.new_to_byte() as u8,
                        0x0,
                        0x0,
                    ];
                    hid_raw.write_bytes(&msg)?;
                    return Ok(());
                }
            }
            let bytes = config.enabled.to_bytes(config.led_type);
            let msg = [
                0x5d, 0xbd, 0x01, bytes[0], bytes[1], bytes[2], bytes[3],
            ];
            hid_raw.write_bytes(&msg)?;
        }
        Ok(())
    }

    /// Write an effect block. This is for per-key, but can be repurposed to
    /// write the raw factory mode packets - when doing this it is expected that
    /// only the first `Vec` (`effect[0]`) is valid.
    pub async fn write_effect_block(
        &self,
        config: &mut AuraConfig,
        effect: &AuraLaptopUsbPackets,
    ) -> Result<(), RogError> {
        if config.brightness == LedBrightness::Off {
            config.brightness = LedBrightness::Med;
            config.write();
        }
        let pkt_type = effect[0][1];
        const PER_KEY_TYPE: u8 = 0xbc;
        if let Some(hid_raw) = &self.hid {
            let hid_raw = hid_raw.lock().await;
            if pkt_type != PER_KEY_TYPE {
                config.per_key_mode_active = false;
                hid_raw.write_bytes(&effect[0])?;
                hid_raw.write_bytes(&AURA_LAPTOP_LED_SET)?;
                // hid_raw.write_bytes(&LED_APPLY)?;
            } else {
                if !config.per_key_mode_active {
                    let init = LedUsbPackets::get_init_msg();
                    hid_raw.write_bytes(&init)?;
                    config.per_key_mode_active = true;
                }
                for row in effect.iter() {
                    hid_raw.write_bytes(row)?;
                }
            }
        } else if matches!(config.led_type, rog_aura::AuraDeviceType::LaptopKeyboardTuf) {
            if let Some(tuf) = &self.backlight {
                for row in effect.iter() {
                    let r = row[9];
                    let g = row[10];
                    let b = row[11];
                    tuf.lock().await.set_kbd_rgb_mode(&[
                        0, 0, r, g, b, 0,
                    ])?;
                }
            }
        }
        Ok(())
    }

    pub async fn fix_ally_power(&mut self) -> Result<(), RogError> {
        if self.config.lock().await.led_type == AuraDeviceType::Ally {
            if let Some(hid_raw) = &self.hid {
                let mut config = self.config.lock().await;
                if config.ally_fix.is_none() {
                    let msg = [
                        0x5d, 0xbd, 0x01, 0xff, 0xff, 0xff, 0xff,
                    ];
                    hid_raw.lock().await.write_bytes(&msg)?;
                    info!("Reset Ally power settings to base");
                    config.ally_fix = Some(true);
                }
                config.write();
            }
        }
        Ok(())
    }
}
