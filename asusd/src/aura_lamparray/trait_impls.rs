//! zbus interface for LampArray devices.
//!
//! Registered at `/xyz/ljones/aura/lamparray_<pid>` — the path must stay
//! identical to the previous incarnation so `rog-control-center` keeps
//! working across the refactor.
//!
//! Exposes the same `xyz.ljones.Aura` interface name as [`AuraZbus`] so
//! clients cannot tell the two apart — the split is purely internal.
use std::collections::BTreeMap;

use config_traits::StdConfig;
use log::{debug, error, info, warn};
use rog_aura::keyboard::{AuraLaptopUsbPackets, LaptopAuraPower};
use rog_aura::{AuraDeviceType, AuraEffect, AuraModeNum, AuraZone, LedBrightness, PowerZones};
use zbus::fdo::Error as ZbErr;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedObjectPath;
use zbus::{interface, Connection};

use super::LampArray;
use crate::error::RogError;
use crate::{CtrlTask, Reloadable};

#[derive(Clone)]
pub struct LampArrayZbus(LampArray);

impl LampArrayZbus {
    pub fn new(lamparray: LampArray) -> Self {
        Self(lamparray)
    }

    pub async fn start_tasks(
        mut self,
        connection: &Connection,
        path: OwnedObjectPath,
    ) -> Result<(), RogError> {
        self.reload()
            .await
            .unwrap_or_else(|err| warn!("Controller error: {}", err));
        connection
            .object_server()
            .at(path.clone(), self)
            .await
            .map_err(|e| error!("Couldn't add server at path: {path}, {e:?}"))
            .ok();
        Ok(())
    }
}

/// The main interface for changing, reading, or notifying.
///
/// Same interface name as the USB/asus-wmi [`AuraZbus`], so downstream
/// clients (`rog-control-center`, `asusctl aura`) interact with LampArray
/// devices without any protocol awareness.
#[interface(name = "xyz.ljones.Aura")]
impl LampArrayZbus {
    /// Return the device type for this Aura keyboard
    #[zbus(property)]
    async fn device_type(&self) -> AuraDeviceType {
        self.0.config.lock().await.led_type
    }

    /// Return the current LED brightness (from config — LampArray has no
    /// sysfs backlight node).
    #[zbus(property)]
    async fn brightness(&self) -> Result<LedBrightness, ZbErr> {
        Ok(self.0.config.lock().await.brightness)
    }

    /// Set the keyboard brightness level (0-3).
    #[zbus(property)]
    async fn set_brightness(&mut self, brightness: LedBrightness) -> Result<(), ZbErr> {
        self.0.set_brightness(brightness.into()).await?;
        let mut config = self.0.config.lock().await;
        config.brightness = brightness;
        config.write();
        Ok(())
    }

    /// Total levels of brightness available
    #[zbus(property)]
    async fn supported_brightness(&self) -> Vec<LedBrightness> {
        vec![
            LedBrightness::Off,
            LedBrightness::Low,
            LedBrightness::Med,
            LedBrightness::High,
        ]
    }

    /// The total available modes
    #[zbus(property)]
    async fn supported_basic_modes(&self) -> Result<Vec<AuraModeNum>, ZbErr> {
        let config = self.0.config.lock().await;
        Ok(config.builtins.keys().cloned().collect())
    }

    #[zbus(property)]
    async fn supported_basic_zones(&self) -> Result<Vec<AuraZone>, ZbErr> {
        let config = self.0.config.lock().await;
        Ok(config.support_data.basic_zones.clone())
    }

    #[zbus(property)]
    async fn supported_power_zones(&self) -> Result<Vec<PowerZones>, ZbErr> {
        let config = self.0.config.lock().await;
        Ok(config.support_data.power_zones.clone())
    }

    /// The current mode data
    #[zbus(property)]
    async fn led_mode(&self) -> Result<AuraModeNum, ZbErr> {
        if let Ok(config) = self.0.config.try_lock() {
            Ok(config.current_mode)
        } else {
            Err(ZbErr::Failed("LampArray control couldn't lock self".to_string()))
        }
    }

    /// Set an Aura effect if the effect mode or zone is supported.
    ///
    /// On success the aura config file is read to refresh cached values,
    /// then the effect is stored and config written to disk.
    #[zbus(property)]
    async fn set_led_mode(&mut self, num: AuraModeNum) -> Result<(), ZbErr> {
        let mut config = self.0.config.lock().await;
        config.current_mode = num;
        if config.brightness == LedBrightness::Off {
            config.brightness = LedBrightness::Med;
        }
        self.0.write_current_config_mode(&mut config).await?;
        // write_current_config_mode already pushed both colour and intensity
        // in one HID feature report (via write_effect_locked). Avoid a
        // second push that would race the colour with the white fallback in
        // set_brightness.
        config.write();
        Ok(())
    }

    /// The current mode data
    #[zbus(property)]
    async fn led_mode_data(&self) -> Result<AuraEffect, ZbErr> {
        if let Ok(config) = self.0.config.try_lock() {
            let mode = config.current_mode;
            match config.builtins.get(&mode) {
                Some(effect) => Ok(effect.clone()),
                None => Err(ZbErr::Failed("Could not get the current effect".into())),
            }
        } else {
            Err(ZbErr::Failed("LampArray control couldn't lock self".to_string()))
        }
    }

    /// Set an Aura effect if the effect mode or zone is supported.
    ///
    /// On success the aura config file is read to refresh cached values,
    /// then the effect is stored and config written to disk.
    #[zbus(property)]
    async fn set_led_mode_data(&mut self, effect: AuraEffect) -> Result<(), ZbErr> {
        let mut config = self.0.config.lock().await;
        if !config.support_data.basic_modes.contains(&effect.mode)
            || effect.zone != AuraZone::None
                && !config.support_data.basic_zones.contains(&effect.zone)
        {
            return Err(ZbErr::NotSupported(format!(
                "The Aura effect is not supported: {effect:?}"
            )));
        }
        if config.brightness == LedBrightness::Off {
            config.brightness = LedBrightness::Med;
        }
        // LampArray: a single HID feature report carries both colour and
        // intensity, so we must push them together. Use write_effect_locked
        // to avoid the try_lock fallback in write_effect that would clobber
        // the colour with a Med/white default. Skip the subsequent
        // set_brightness() call because set_brightness would race the
        // colour we just wrote (try_lock fails -> white fallback push).
        self.0.write_effect_locked(&config, &effect).await?;
        config.set_builtin(effect);
        config.write();
        Ok(())
    }

    /// Get the data set for every mode available
    async fn all_mode_data(&self) -> BTreeMap<AuraModeNum, AuraEffect> {
        let config = self.0.config.lock().await;
        config.builtins.clone()
    }

    #[zbus(property)]
    async fn led_power(&self) -> LaptopAuraPower {
        let config = self.0.config.lock().await;
        config.enabled.clone()
    }

    /// Set a variety of states, input is array of enum.
    /// `enabled` sets if the sent array should be disabled or enabled.
    #[zbus(property)]
    async fn set_led_power(&mut self, options: LaptopAuraPower) -> Result<(), ZbErr> {
        let mut config = self.0.config.lock().await;
        for opt in options.states {
            let zone = opt.zone;
            for state in config.enabled.states.iter_mut() {
                if state.zone == zone {
                    *state = opt;
                    break;
                }
            }
        }
        config.write();
        Ok(self.0.set_aura_power(&config).await.map_err(|e| {
            warn!("{}", e);
            e
        })?)
    }

    /// Direct addressing not supported on LampArray — the Microsoft HID
    /// LampArray protocol has no per-key primitive on LampCount=1 devices.
    /// Kept as a no-op so the interface signature matches [`AuraZbus`].
    async fn direct_addressing_raw(&self, _data: AuraLaptopUsbPackets) -> Result<(), ZbErr> {
        debug!("LampArray: direct_addressing_raw ignored (no per-key primitive)");
        Ok(())
    }
}

impl CtrlTask for LampArrayZbus {
    fn zbus_path() -> &'static str {
        "/xyz/ljones"
    }

    async fn create_tasks(&self, _: SignalEmitter<'static>) -> Result<(), RogError> {
        let inner_sleep = self.0.clone();
        let inner_shutdown = self.0.clone();
        self.create_sys_event_tasks(
            move |sleeping| {
                let inner = inner_sleep.clone();
                async move {
                    if !sleeping {
                        info!("LampArray CtrlKbdLedTask: reloading brightness and modes");
                        let mut config = inner.config.lock().await;
                        inner
                            .write_current_config_mode(&mut config)
                            .await
                            .map_err(|e| {
                                error!("LampArray CtrlKbdLedTask: {e}");
                                e
                            })
                            .unwrap();
                    }
                }
            },
            move |_shutting_down| {
                let inner = inner_shutdown.clone();
                async move {
                    // Nothing to persist beyond config on shutdown for
                    // LampArray — brightness lives in config only.
                    let _ = inner;
                }
            },
            move |_lid_closed| async move {},
            move |_power_plugged| async move {},
        )
        .await;
        Ok(())
    }
}

impl Reloadable for LampArrayZbus {
    async fn reload(&mut self) -> Result<(), RogError> {
        debug!("reloading LampArray keyboard mode");
        let mut config = self.0.lock_config().await;
        self.0.write_current_config_mode(&mut config).await?;
        debug!("reloading LampArray power states");
        self.0
            .set_aura_power(&config)
            .await
            .map_err(|err| warn!("{err}"))
            .ok();
        Ok(())
    }
}
