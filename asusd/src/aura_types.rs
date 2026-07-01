use std::sync::Arc;

use config_traits::{StdConfig, StdConfigLoad};
use log::{debug, error, info};
use rog_anime::error::AnimeError;
use rog_anime::usb::get_anime_type;
use rog_anime::AnimeType;
use rog_aura::AuraDeviceType;
use rog_platform::hid_raw::HidRaw;
use rog_platform::keyboard_led::KeyboardBacklight;
use rog_platform::usb_raw::USBRaw;
use rog_scsi::{open_device, ScsiType};
use rog_slash::error::SlashError;
use rog_slash::SlashType;
use tokio::sync::Mutex;

use crate::aura_anime::config::AniMeConfig;
use crate::aura_anime::AniMe;
use crate::aura_lamparray::LampArray;
use crate::aura_laptop::config::AuraConfig;
use crate::aura_laptop::Aura;
use crate::aura_scsi::config::ScsiConfig;
use crate::aura_scsi::ScsiAura;
use crate::aura_slash::config::SlashConfig;
use crate::aura_slash::Slash;
use crate::error::RogError;

/// info! only in debug builds — release stays quiet.
macro_rules! debug_info {
    ($($arg:tt)*) => {{
        if cfg!(debug_assertions) {
            log::info!($($arg)*);
        }
    }};
}

pub enum _DeviceHandle {
    /// The AniMe devices require USBRaw as they are not HID devices
    Usb(USBRaw),
    HidRaw(HidRaw),
    LedClass(KeyboardBacklight),
    /// TODO
    MulticolourLed,
    None,
}

#[derive(Clone)]
pub enum DeviceHandle {
    Aura(Aura),
    LampArray(LampArray),
    Slash(Slash),
    /// The AniMe devices require USBRaw as they are not HID devices
    AniMe(AniMe),
    Scsi(ScsiAura),
    Ally(Arc<Mutex<HidRaw>>),
    OldAura(Arc<Mutex<HidRaw>>),
    /// TUF laptops have an aditional set of attributes added to the LED /sysfs/
    TufLedClass(Arc<Mutex<HidRaw>>),
    /// TODO
    MulticolourLed,
    None,
}

impl DeviceHandle {
    /// Try Slash HID. If one exists it is initialsed and returned.
    pub async fn new_slash_hid(
        device: Arc<Mutex<HidRaw>>,
        prod_id: &str,
    ) -> Result<Self, RogError> {
        debug!("Testing for HIDRAW Slash");
        let slash_type = SlashType::from_dmi();
        if matches!(slash_type, SlashType::Unsupported)
            || slash_type
                .prod_id_str()
                .to_lowercase()
                .trim_start_matches("0x")
                != prod_id
        {
            log::info!("Unknown or invalid slash: {prod_id:?}, skipping");
            return Err(RogError::NotFound("No slash device".to_string()));
        }
        info!("Found slash type {slash_type:?}: {prod_id}");
        let mut config = SlashConfig::new().load();
        config.slash_type = slash_type;
        let slash = Slash::new(Some(device), None, Arc::new(Mutex::new(config)));
        slash.do_initialization().await?;
        Ok(Self::Slash(slash))
    }

    /// Try Slash USB. If one exists it is initialsed and returned.
    pub async fn new_slash_usb() -> Result<Self, RogError> {
        debug!("Testing for USB Slash");
        let slash_type = SlashType::from_dmi();
        if matches!(slash_type, SlashType::Unsupported) {
            return Err(RogError::Slash(SlashError::NoDevice));
        }
        if let Ok(usb) = USBRaw::new(slash_type.prod_id()) {
            info!("Found Slash USB {slash_type:?}");
            let mut config = SlashConfig::new().load();
            config.slash_type = slash_type;
            let slash = Slash::new(
                None,
                Some(Arc::new(Mutex::new(usb))),
                Arc::new(Mutex::new(config)),
            );
            slash.do_initialization().await?;
            Ok(Self::Slash(slash))
        } else {
            Err(RogError::NotFound("No slash device found".to_string()))
        }
    }

    /// Try AniMe Matrix HID. If one exists it is initialsed and returned.
    pub async fn maybe_anime_hid(
        _device: Arc<Mutex<HidRaw>>,
        _prod_id: &str,
    ) -> Result<Self, RogError> {
        Err(RogError::NotFound(
            "Can't use anime over hidraw yet. Skip.".to_string(),
        ))
    }

    pub async fn maybe_anime_usb() -> Result<Self, RogError> {
        debug!("Testing for USB AniMe");
        let anime_type = get_anime_type();
        if matches!(anime_type, AnimeType::Unsupported) {
            info!("No Anime Matrix capable laptop found");
            return Err(RogError::Anime(AnimeError::NoDevice));
        }
        if let Ok(usb) = USBRaw::new(0x193b) {
            info!("Found AniMe Matrix USB {anime_type:?}");
            let mut config = AniMeConfig::new().load();
            config.anime_type = anime_type;
            let mut anime = AniMe::new(
                None,
                Some(Arc::new(Mutex::new(usb))),
                Arc::new(Mutex::new(config)),
            );
            anime.do_initialization().await?;
            Ok(Self::AniMe(anime))
        } else {
            Err(RogError::NotFound(
                "No AnimeMatrix device found".to_string(),
            ))
        }
    }

    pub async fn maybe_scsi(dev_node: &str, prod_id: &str) -> Result<Self, RogError> {
        debug!("Testing for SCSI");
        let prod_id = ScsiType::from(prod_id);
        if prod_id == ScsiType::Unsupported {
            log::info!("Unknown or invalid SCSI: {prod_id:?}, skipping");
            return Err(RogError::NotFound("No SCSI device".to_string()));
        }
        info!("Found SCSI device {prod_id:?} on {dev_node}");
        let mut config = ScsiConfig::new().load();
        config.dev_type = AuraDeviceType::ScsiExtDisk;
        let dev = Arc::new(Mutex::new(open_device(dev_node)?));
        let scsi = ScsiAura::new(dev, Arc::new(Mutex::new(config)));
        scsi.do_initialization().await?;
        Ok(Self::Scsi(scsi))
    }

    pub async fn maybe_laptop_aura(
        device: Option<Arc<Mutex<HidRaw>>>,
        prod_id: &str,
    ) -> Result<Self, RogError> {
        debug!("Testing for laptop aura");
        let aura_type = AuraDeviceType::from(prod_id);
        if !matches!(
            aura_type,
            AuraDeviceType::LaptopKeyboard2021
                | AuraDeviceType::LaptopKeyboardPre2021
                | AuraDeviceType::LaptopKeyboardTuf
                | AuraDeviceType::Ally
        ) {
            log::info!("Unknown or invalid laptop aura: {prod_id:?}, skipping");
            return Err(RogError::NotFound("No laptop aura device".to_string()));
        }
        info!("Found laptop aura type {prod_id:?}");
        let backlight = KeyboardBacklight::new()
            .map_err(|e| error!("Keyboard backlight error: {e:?}"))
            .map_or(None, |k| {
                info!("Found sysfs backlight control");
                Some(Arc::new(Mutex::new(k)))
            });
        // Load saved mode, colours, brightness, power from disk; apply on reload
        let mut config = AuraConfig::load_and_update_config(prod_id);
        config.led_type = aura_type;
        let aura = Aura {
            hid: device,
            backlight,
            config: Arc::new(Mutex::new(config)),
        };
        aura.do_initialization().await?;
        Ok(Self::Aura(aura))
    }

    /// Try the HID LampArray (Microsoft HID LampArray usage page) path used by
    /// I2C-HID controllers on newer ASUS TUF laptops (e.g. FA608WV / ITE5570).
    /// We open the hidraw R/W, run HIDIOCGRAWINFO and only proceed if the
    /// device declares ASUS VID 0x0b05 and a whitelisted LampArray PID.
    pub async fn maybe_lamparray(
        device: Arc<Mutex<HidRaw>>,
        prod_id: &str,
    ) -> Result<Self, RogError> {
        let dev_path = {
            let g = device.lock().await;
            g.devfs_path().clone()
        };
        debug_info!("LampArray init: device {dev_path:?} prod_id={prod_id:?}");
        debug_info!("LampArray init: about to call raw_info on {dev_path:?}");
        let info_res = {
            let g = device.lock().await;
            g.raw_info()
        };
        let info = match info_res {
            Ok(i) => i,
            Err(e) => {
                error!("LampArray init: raw_info FAILED on {dev_path:?}: {e:?}");
                return Err(RogError::Platform(e));
            }
        };
        let vid = (info.vendor as u16) as u32;
        let pid = (info.product as u16) as u32;
        debug_info!(
            "LampArray init: HIDIOCGRAWINFO {dev_path:?} VID:{vid:04x} PID:{pid:04x}"
        );
        if vid != 0x0b05 {
            return Err(RogError::NotFound(format!("Not ASUS: {vid:04x}")));
        }
        if !matches!(pid, 0x19b6) {
            return Err(RogError::NotFound(format!(
                "PID {pid:04x} not on LampArray whitelist"
            )));
        }
        let aura_type = AuraDeviceType::from(prod_id);
        info!("Found HID LampArray ASUS keyboard 0b05:{pid:04x}");
        let mut config = AuraConfig::load_and_update_config(prod_id);
        config.led_type = aura_type;
        let lamparray = LampArray::new(device, Arc::new(Mutex::new(config)));
        lamparray.do_initialization().await?;
        info!("LampArray ready: 0b05:{pid:04x} on {dev_path:?}");
        Ok(Self::LampArray(lamparray))
    }
}
