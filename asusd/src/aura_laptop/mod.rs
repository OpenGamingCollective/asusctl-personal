use std::sync::Arc;
use std::time::Duration;

use config::AuraConfig;
use config_traits::StdConfig;
use log::info;
use rog_aura::keyboard::{AuraLaptopUsbPackets, LedUsbPackets};
use rog_aura::usb::{AURA_LAPTOP_LED_APPLY, AURA_LAPTOP_LED_SET};
use rog_aura::{AuraDeviceType, AuraEffect, AuraModeNum, LedBrightness, PowerZones, Speed, AURA_LAPTOP_LED_MSG_LEN};
use rog_platform::hid_raw::HidRaw;
use rog_platform::keyboard_led::KeyboardBacklight;
use tokio::runtime::Handle;
use tokio::sync::{Mutex, MutexGuard};
use tokio::task::JoinHandle;

use crate::error::RogError;

pub mod config;
pub mod trait_impls;

#[derive(Debug, Clone)]
pub struct Aura {
    pub hid: Option<Arc<Mutex<HidRaw>>>,
    pub backlight: Option<Arc<Mutex<KeyboardBacklight>>>,
    pub config: Arc<Mutex<AuraConfig>>,
    /// True when this Aura is driven via the HID LampArray usage page
    /// (I2C-HID controllers on newer ASUS TUF laptops). When true the
    /// asus-wmi sysfs backlight is absent and all interactions go through
    /// HIDIOC[SG]FEATURE feature reports.
    pub is_lamparray: bool,
    /// Handle for the currently-running LampArray dynamic-effect task.
    /// The LampArray chip is passive: animations (Breathe, RainbowCycle,
    /// RainbowWave, Pulse, ...) are driven by the host pushing
    /// LampRangeUpdate frames at ~30 FPS from a tokio task. When a new
    /// effect is written we abort the old task first so we never have two
    /// loops fighting over the hid lock.
    pub effect_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Tokio runtime handle captured at construction. Used to spawn the
    /// dynamic-effect task from methods invoked on the zbus executor thread
    /// (which is *not* a Tokio runtime thread). `Handle::spawn()` works from
    /// any thread; bare `tokio::spawn()` would panic there.
    pub runtime_handle: Handle,
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
        let bright = if self.is_lamparray {
            // LampArray brightness lives entirely in our config (no sysfs node).
            config.brightness.into()
        } else if let Some(bl) = self.backlight.as_ref() {
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
                        if self.is_lamparray {
                            // Caller already owns `config`; re-locking
                            // `self.config` inside `lamparray_write_effect`
                            // used to deadlock us at init time.
                            self.lamparray_write_effect_locked(config, &mode).await?;
                        } else {
                            self.write_effect_and_apply(config.led_type, &mode).await?;
                        }
                    }
                }
            }
        } else {
            let mode = config.current_mode;
            if let Some(effect) = config.builtins.get(&mode).cloned() {
                if self.is_lamparray {
                    self.lamparray_write_effect_locked(config, &effect).await?;
                } else {
                    self.write_effect_and_apply(config.led_type, &effect)
                        .await?;
                }
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
        if self.is_lamparray {
            info!(
                "LampArray write_effect_and_apply: dev_type={:?} mode={:?}",
                dev_type, mode.mode
            );
            return self.lamparray_write_effect(mode).await;
        }
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
        if self.is_lamparray {
            return self.lamparray_set_brightness(value).await;
        }
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
        if self.is_lamparray {
            return self.lamparray_set_aura_power(config).await;
        }
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

    /// LampArray helper - write the current static colour to the whole
    /// keyboard at the requested intensity (0-255). The protocol is the
    /// Microsoft HID LampArray usage page:
    ///   * report 0x46 - "autonomous mode" toggle (we disable so the OS owns)
    ///   * report 0x41 - LampArrayAttributes (read to discover LampCount)
    ///   * report 0x45 - LampArrayMultiUpdate / RangeUpdate
    async fn lamparray_push_rgb_i(
        &self,
        r: u8,
        g: u8,
        b: u8,
        intensity: u8,
    ) -> Result<(), RogError> {
        let hid_arc = self
            .hid
            .as_ref()
            .ok_or(RogError::NoAuraKeyboard)?
            .clone();
        let hid = hid_arc.lock().await;
        // Disable autonomous so we own the lamp array
        hid.set_feature_report(&[0x46, 0x00])?;
        // Read LampArrayAttributes to discover the lamp count
        let mut attr = vec![0u8; 23];
        attr[0] = 0x41;
        hid.get_feature_report(&mut attr)?;
        let lamp_count = u16::from_le_bytes([attr[1], attr[2]]);
        if lamp_count == 0 {
            return Err(RogError::MissingFunction(
                "LampArray reports zero lamps".to_string(),
            ));
        }
        let last = lamp_count - 1;
        // RangeUpdate: 0x45, flags, start_lo, start_hi, end_lo, end_hi, r,g,b,i
        let payload = [
            0x45,
            0x01,
            0x00,
            0x00,
            (last & 0xff) as u8,
            ((last >> 8) & 0xff) as u8,
            r,
            g,
            b,
            intensity,
        ];
        hid.set_feature_report(&payload)?;
        info!(
            "LampArray ready: LampCount={lamp_count} rgb=({r:02x},{g:02x},{b:02x}) i={intensity}"
        );
        Ok(())
    }

    /// Write a single effect (static colour for now) to a LampArray device.
    ///
    /// IMPORTANT: this used to take `self.config.lock().await` to read
    /// brightness, but the typical call chain comes from a caller that ALREADY
    /// holds the config lock (e.g. `write_current_config_mode`,
    /// `set_led_mode_data`, `reload`). Re-locking caused an async deadlock at
    /// init time, which made systemd kill asusd on the `Type=dbus` timeout.
    /// We now use `try_lock` and fall back to `LedBrightness::Med` when the
    /// lock is held by the caller. Callers that already have a locked
    /// `AuraConfig` should prefer `lamparray_write_effect_locked` to avoid
    /// the fallback path entirely.
    pub async fn lamparray_write_effect(&self, mode: &AuraEffect) -> Result<(), RogError> {
        // Always stop any previous animation loop before doing anything else,
        // so two effect tasks never race to push frames.
        self.lamparray_stop_effect_task().await;
        let brightness = match self.config.try_lock() {
            Ok(cfg) => cfg.brightness,
            Err(_) => {
                info!(
                    "lamparray_write_effect: config already locked by caller, using Med fallback"
                );
                LedBrightness::Med
            }
        };
        let intensity = Self::brightness_to_intensity(brightness);
        match mode.mode {
            AuraModeNum::Static => {
                let r = mode.colour1.r;
                let g = mode.colour1.g;
                let b = mode.colour1.b;
                info!("lamparray_write_effect: Static, single push");
                self.lamparray_push_rgb_i(r, g, b, intensity).await
            }
            _ => {
                info!(
                    "lamparray_write_effect: dynamic mode {:?}, spawning effect task",
                    mode.mode
                );
                self.lamparray_spawn_effect(mode.clone(), intensity).await
            }
        }
    }

    /// Variant for callers that already hold the config lock. Pass the
    /// already-locked config in to avoid the deadlock that re-locking would
    /// cause.
    pub async fn lamparray_write_effect_locked(
        &self,
        config: &AuraConfig,
        mode: &AuraEffect,
    ) -> Result<(), RogError> {
        // Same rule as `lamparray_write_effect`: kill any running animation
        // before dispatching so we don't accumulate tasks across reloads.
        self.lamparray_stop_effect_task().await;
        let intensity = Self::brightness_to_intensity(config.brightness);
        match mode.mode {
            AuraModeNum::Static => {
                let r = mode.colour1.r;
                let g = mode.colour1.g;
                let b = mode.colour1.b;
                info!("lamparray_write_effect_locked: Static, single push");
                self.lamparray_push_rgb_i(r, g, b, intensity).await
            }
            _ => {
                info!(
                    "lamparray_write_effect_locked: dynamic mode {:?}, spawning effect task",
                    mode.mode
                );
                self.lamparray_spawn_effect(mode.clone(), intensity).await
            }
        }
    }

    fn brightness_to_intensity(b: LedBrightness) -> u8 {
        match b {
            LedBrightness::Off => 0,
            LedBrightness::Low => 64,
            LedBrightness::Med => 128,
            LedBrightness::High => 255,
        }
    }

    /// Brightness -> intensity mapping for LampArray. Reuses the colour from
    /// the currently active builtin effect in config so the keyboard keeps
    /// the same hue when the user only changes brightness.
    ///
    /// Uses `try_lock` to avoid the init-time deadlock when a caller higher
    /// in the stack already owns the config lock (see comment on
    /// `lamparray_write_effect`).
    pub async fn lamparray_set_brightness(&self, value: u8) -> Result<(), RogError> {
        let level = match value {
            0 => LedBrightness::Off,
            1 => LedBrightness::Low,
            2 => LedBrightness::Med,
            _ => LedBrightness::High,
        };
        let intensity = Self::brightness_to_intensity(level);
        let (r, g, b) = match self.config.try_lock() {
            Ok(mut cfg) => {
                cfg.brightness = level;
                let mode = cfg.current_mode;
                if let Some(eff) = cfg.builtins.get(&mode) {
                    (eff.colour1.r, eff.colour1.g, eff.colour1.b)
                } else {
                    (0xff, 0xff, 0xff)
                }
            }
            Err(_) => {
                info!(
                    "lamparray_set_brightness: config already locked by caller, defaulting to white"
                );
                (0xff, 0xff, 0xff)
            }
        };
        info!("lamparray_set_brightness: about to push rgb (no lock held)");
        self.lamparray_push_rgb_i(r, g, b, intensity).await
    }

    /// Aura power states on LampArray - we collapse the per-zone flags into a
    /// simple on/off: any zone enabled -> full intensity with the saved RGB,
    /// all disabled -> intensity 0.
    pub async fn lamparray_set_aura_power(
        &self,
        config: &AuraConfig,
    ) -> Result<(), RogError> {
        let any_on = config.enabled.states.iter().any(|s| {
            // Treat the "new" zone state as on if any bit is set.
            s.new_to_byte() != 0
        });
        let (r, g, b) = {
            let mode = config.current_mode;
            if let Some(eff) = config.builtins.get(&mode) {
                (eff.colour1.r, eff.colour1.g, eff.colour1.b)
            } else {
                (0xff, 0xff, 0xff)
            }
        };
        let intensity = if any_on { 255 } else { 0 };
        // A power-state change also implies "stop whatever animation was
        // running", otherwise the loop would happily override our push.
        self.lamparray_stop_effect_task().await;
        self.lamparray_push_rgb_i(r, g, b, intensity).await
    }

    /// Abort the current LampArray effect task, if any. Safe to call even
    /// when no task is running.
    pub async fn lamparray_stop_effect_task(&self) {
        let mut slot = self.effect_task.lock().await;
        if let Some(handle) = slot.take() {
            handle.abort();
            info!("lamparray_effect_task: cancelled");
        }
    }

    /// Spawn a tokio task that drives one of the dynamic LampArray effects
    /// (Breathe / RainbowCycle / RainbowWave / Pulse). The task loops at
    /// ~30 FPS pushing LampRangeUpdate feature reports. `intensity` is the
    /// current brightness cap (0..=255): brightness-driven effects
    /// (Breathe, Pulse) modulate I within this cap; HSV effects
    /// (RainbowCycle, RainbowWave) pass it straight through.
    async fn lamparray_spawn_effect(
        &self,
        mode: AuraEffect,
        intensity: u8,
    ) -> Result<(), RogError> {
        let hid_arc = self
            .hid
            .as_ref()
            .ok_or(RogError::NoAuraKeyboard)?
            .clone();

        // Probe LampCount once, up-front, so the task doesn't need to touch
        // GET_FEATURE at 30 FPS.
        let lamp_count = {
            let hid = hid_arc.lock().await;
            hid.set_feature_report(&[0x46, 0x00])?;
            let mut attr = vec![0u8; 23];
            attr[0] = 0x41;
            hid.get_feature_report(&mut attr)?;
            u16::from_le_bytes([attr[1], attr[2]])
        };
        if lamp_count == 0 {
            return Err(RogError::MissingFunction(
                "LampArray reports zero lamps".to_string(),
            ));
        }

        let period_ms = Self::speed_to_period_ms(mode.speed);
        let frame_ms: u64 = 33; // ~30 FPS
        let total_frames: u32 =
            ((period_ms as f32) / (frame_ms as f32)).max(1.0) as u32;
        let mode_kind = mode.mode;
        let colour1 = mode.colour1;

        info!(
            "lamparray_effect_task: starting mode={:?} period={}ms frames={}              rgb1=({},{},{}) intensity_cap={}",
            mode_kind,
            period_ms,
            total_frames,
            colour1.r,
            colour1.g,
            colour1.b,
            intensity
        );

        let hid_for_task = hid_arc.clone();
        let handle = self.runtime_handle.spawn(async move {
            let mut ticker =
                tokio::time::interval(Duration::from_millis(frame_ms));
            // Discard the immediate first tick so the loop pacing is stable.
            ticker.tick().await;
            let mut frame: u32 = 0;
            loop {
                let t = (frame % total_frames) as f32
                    / (total_frames as f32);
                let (r, g, b, i) = match mode_kind {
                    AuraModeNum::Breathe => {
                        // Pure sinusoid on I; keep colour1 as the hue.
                        let s = (2.0
                            * std::f32::consts::PI
                            * t)
                            .sin();
                        let level = ((s + 1.0) * 0.5) * intensity as f32;
                        (
                            colour1.r,
                            colour1.g,
                            colour1.b,
                            level.round().clamp(0.0, 255.0) as u8,
                        )
                    }
                    AuraModeNum::Pulse => {
                        // Sharp attack, slow decay - "heartbeat" style.
                        let phase = t;
                        let level = if phase < 0.2 {
                            (phase / 0.2) * intensity as f32
                        } else {
                            (1.0 - (phase - 0.2) / 0.8) * intensity as f32
                        };
                        (
                            colour1.r,
                            colour1.g,
                            colour1.b,
                            level.round().clamp(0.0, 255.0) as u8,
                        )
                    }
                    AuraModeNum::RainbowCycle => {
                        let hue = (t * 360.0) % 360.0;
                        let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);
                        (r, g, b, intensity)
                    }
                    AuraModeNum::RainbowWave => {
                        // On LampCount=1 there is no spatial "wave" to
                        // encode - a single lamp is scalar. We keep the
                        // same hue rotation as RainbowCycle but sweep the
                        // hue backwards to give the user a visual
                        // difference between the two modes.
                        let hue = (360.0 - (t * 360.0)) % 360.0;
                        let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);
                        (r, g, b, intensity)
                    }
                    // Should not happen: Static and unhandled modes go
                    // through the single-push path in write_effect.
                    _ => (colour1.r, colour1.g, colour1.b, intensity),
                };

                let last = lamp_count - 1;
                let payload = [
                    0x45,
                    0x01,
                    0x00,
                    0x00,
                    (last & 0xff) as u8,
                    ((last >> 8) & 0xff) as u8,
                    r,
                    g,
                    b,
                    i,
                ];
                // Hold the hid lock only for the write, so brightness/other
                // callers can interleave between frames.
                {
                    let hid = hid_for_task.lock().await;
                    if let Err(e) = hid.set_feature_report(&payload) {
                        log::warn!(
                            "lamparray_effect_task: set_feature_report failed: {e:?} - stopping"
                        );
                        break;
                    }
                }

                frame = frame.wrapping_add(1);
                ticker.tick().await;
            }
            info!("lamparray_effect_task: exited");
        });

        let mut slot = self.effect_task.lock().await;
        *slot = Some(handle);
        Ok(())
    }

    /// Map the abstract rog_aura::Speed enum to a period in milliseconds
    /// for one full cycle of the animation.
    fn speed_to_period_ms(s: Speed) -> u32 {
        match s {
            Speed::Low => 4000,
            Speed::Med => 2000,
            Speed::High => 800,
        }
    }
}

/// Convert an HSV colour (hue in degrees, s/v in [0, 1]) to 8-bit RGB.
/// Standard formula from https://en.wikipedia.org/wiki/HSL_and_HSV.
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let hp = (h / 60.0) % 6.0;
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let (r1, g1, b1) = if hp < 1.0 {
        (c, x, 0.0)
    } else if hp < 2.0 {
        (x, c, 0.0)
    } else if hp < 3.0 {
        (0.0, c, x)
    } else if hp < 4.0 {
        (0.0, x, c)
    } else if hp < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = v - c;
    (
        ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}
