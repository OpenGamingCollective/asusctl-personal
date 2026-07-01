//! LampArray dynamic effects: frame generation for
//! Breathe / RainbowCycle / RainbowWave / Pulse.
//!
//! Split out from `mod.rs` so the animation math stays isolated from the
//! outer `LampArray` bookkeeping (config lock, effect_task handle).
use std::sync::Arc;
use std::time::Duration;

use log::info;
use rog_aura::{AuraEffect, AuraModeNum, Speed};
use rog_platform::hid_raw::HidRaw;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::error::RogError;

/// info! only in debug builds — release stays quiet.
macro_rules! debug_info {
    ($($arg:tt)*) => {{
        if cfg!(debug_assertions) {
            log::info!($($arg)*);
        }
    }};
}

/// Spawn the animation task. Probes LampCount once (blocking-ish, but only
/// runs on effect switch), then loops at ~30 FPS pushing LampRangeUpdate
/// feature reports until aborted.
pub async fn spawn_effect_task(
    hid: Arc<Mutex<HidRaw>>,
    runtime_handle: &Handle,
    mode: AuraEffect,
    intensity: u8,
) -> Result<JoinHandle<()>, RogError> {
    // Probe LampCount once, up-front, so the task doesn't need to touch
    // GET_FEATURE at 30 FPS.
    let lamp_count = {
        let hid = hid.lock().await;
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
    let period_ms = speed_to_period_ms(mode.speed);
    let frame_ms: u64 = 33; // ~30 FPS
    let total_frames: u32 =
        ((period_ms as f32) / (frame_ms as f32)).max(1.0) as u32;
    let mode_kind = mode.mode;
    let colour1 = mode.colour1;
    info!(
        "lamparray_effect_task: starting mode={:?} period={}ms frames={} rgb1=({},{},{}) intensity_cap={}",
        mode_kind, period_ms, total_frames, colour1.r, colour1.g, colour1.b, intensity
    );

    let hid_for_task = hid.clone();
    let handle = runtime_handle.spawn(async move {
        let mut ticker =
            tokio::time::interval(Duration::from_millis(frame_ms));
        // Discard the immediate first tick so the loop pacing is stable.
        ticker.tick().await;
        let mut frame: u32 = 0;
        loop {
            let t = (frame % total_frames) as f32 / (total_frames as f32);
            let (r, g, b, i) = match mode_kind {
                AuraModeNum::Breathe => {
                    // Pure sinusoid on I; keep colour1 as the hue.
                    let s = (2.0 * std::f32::consts::PI * t).sin();
                    let level = ((s + 1.0) * 0.5) * intensity as f32;
                    (
                        colour1.r,
                        colour1.g,
                        colour1.b,
                        level.round().clamp(0.0, 255.0) as u8,
                    )
                }
                AuraModeNum::Pulse => {
                    // Sharp attack, slow decay — "heartbeat" style.
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
                    // On LampCount=1 there is no spatial "wave" to encode —
                    // a single lamp is scalar. Keep the same hue rotation as
                    // RainbowCycle but sweep hue backwards for a visual
                    // difference between the two modes.
                    let hue = (360.0 - (t * 360.0)) % 360.0;
                    let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);
                    (r, g, b, intensity)
                }
                // Should not happen: Static and unhandled modes go through
                // the single-push path in write_effect.
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
                        "lamparray_effect_task: set_feature_report failed: {e:?} — stopping"
                    );
                    break;
                }
            }
            frame = frame.wrapping_add(1);
            ticker.tick().await;
        }
        debug_info!("lamparray_effect_task: exited");
    });
    Ok(handle)
}

/// Map the abstract rog_aura::Speed enum to a period in milliseconds for
/// one full cycle of the animation.
pub fn speed_to_period_ms(s: Speed) -> u32 {
    match s {
        Speed::Low => 4000,
        Speed::Med => 2000,
        Speed::High => 800,
    }
}

/// Convert an HSV colour (hue in degrees, s/v in [0, 1]) to 8-bit RGB.
/// Standard formula from https://en.wikipedia.org/wiki/HSL_and_HSV.
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
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
