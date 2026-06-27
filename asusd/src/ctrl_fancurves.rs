use std::path::PathBuf;
use std::sync::Arc;

use config_traits::{StdConfig, StdConfigLoad};
use log::info;
use rog_platform::platform::{PlatformProfile, RogPlatform};
use rog_profiles::error::ProfileError;
use rog_profiles::fan_curve_set::CurveData;
use rog_profiles::{FanCurvePU, FanCurveProfiles, find_fan_curve_node};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use zbus::{Connection, interface};

use rog_platform::asus_armoury::FirmwareAttributes;
use rog_platform::power::AsusPower;

use crate::CONFIG_PATH_BASE;
use crate::asus_armoury::set_config_or_default;
use crate::config::Config;
use crate::error::RogError;

pub const FAN_CURVE_ZBUS_NAME: &str = "FanCurves";
pub const FAN_CURVE_ZBUS_PATH: &str = "/xyz/ljones";

#[derive(Deserialize, Serialize, Debug, Default)]
pub struct FanCurveConfig {
    pub profiles: FanCurveProfiles,
    #[serde(skip)]
    pub current: PlatformProfile,
}

impl StdConfig for FanCurveConfig {
    /// Create a new config. The defaults are zeroed so the device must be read
    /// to get the actual device defaults.
    fn new() -> Self {
        Self::default()
    }

    fn file_name(&self) -> String {
        "fan_curves.ron".to_owned()
    }

    fn config_dir() -> std::path::PathBuf {
        PathBuf::from(CONFIG_PATH_BASE)
    }
}

impl StdConfigLoad for FanCurveConfig {}

#[derive(Clone)]
pub struct CtrlFanCurveZbus {
    config: Arc<Mutex<FanCurveConfig>>,
    platform: RogPlatform,
    platform_config: Option<Arc<Mutex<Config>>>,
    power: Option<AsusPower>,
}

// Manual impl because Config does not derive Debug; platform_config and
// power are intentionally omitted from the output.
impl std::fmt::Debug for CtrlFanCurveZbus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CtrlFanCurveZbus")
            .field("config", &self.config)
            .field("platform", &self.platform)
            .finish()
    }
}

// Non-zbus-derive impl
impl CtrlFanCurveZbus {
    pub fn config(&self) -> Arc<Mutex<FanCurveConfig>> {
        self.config.clone()
    }

    /// Set the platform config and power references needed to re-apply PPT
    /// values after fan curve changes.
    pub fn set_platform_refs(&mut self, config: Arc<Mutex<Config>>, power: AsusPower) {
        self.platform_config = Some(config);
        self.power = Some(power);
    }

    /// Re-apply PPT values after fan curve writes. Fan curve changes reset
    /// the EC fan mode, and PPT values must be re-sent afterwards.
    async fn reapply_ppt(&self, profile: PlatformProfile) {
        let (Some(platform_config), Some(power)) = (&self.platform_config, &self.power) else {
            return;
        };
        let power_plugged = power.get_online().unwrap_or_default();
        let attrs = FirmwareAttributes::new();
        set_config_or_default(
            &attrs,
            &mut *platform_config.lock().await,
            power_plugged == 1,
            profile,
        )
        .await;
    }

    pub fn new() -> Result<Self, RogError> {
        let platform = RogPlatform::new()?;
        if platform.has_platform_profile() {
            info!("Device has profile control available");
            find_fan_curve_node()?;
            info!("Device has fan curves available");
            let mut config = FanCurveConfig::new().load();
            let mut fan_curves = FanCurveProfiles::default();

            // Only do defaults if the config doesn't already exist\
            if config.profiles.balanced.is_empty() || !config.file_path().exists() {
                info!("Fetching default fan curves");

                let current = platform.get_platform_profile()?;
                let profiles = platform.get_platform_profile_choices()?;
                for this in profiles {
                    // For each profile we need to switch to it before we
                    // can read the existing values from hardware. The ACPI method used
                    // for this is what limits us.
                    platform.set_platform_profile(this.into())?;
                    let mut dev = find_fan_curve_node()?;
                    fan_curves.set_active_curve_to_defaults(this, &mut dev)?;

                    info!("{this:?}:");
                    for curve in fan_curves.get_fan_curves_for(this) {
                        info!("{}", String::from(curve));
                    }
                }
                platform.set_platform_profile(current.as_str())?;
                config.profiles = fan_curves;
                config.write();
            } else {
                info!("Fan curves previously stored, loading...");
                config = config.load();
            }

            config.current = platform.get_platform_profile()?.into();

            return Ok(Self {
                config: Arc::new(Mutex::new(config)),
                platform,
                platform_config: None,
                power: None,
            });
        }

        Err(ProfileError::NotSupported.into())
    }
}

#[interface(name = "xyz.ljones.FanCurves")]
impl CtrlFanCurveZbus {
    /// Set all fan curves for a profile to enabled status. Will also activate a
    /// fan curve if in the same profile mode
    async fn set_fan_curves_enabled(
        &mut self,
        profile: PlatformProfile,
        enabled: bool,
    ) -> zbus::fdo::Result<()> {
        self.config
            .lock()
            .await
            .profiles
            .set_profile_curves_enabled(profile, enabled);
        let active: PlatformProfile = self.platform.get_platform_profile()?.into();
        if active == profile {
            self.config
                .lock()
                .await
                .profiles
                .write_profile_curve_to_platform(profile, &mut find_fan_curve_node()?)?;
            self.reapply_ppt(profile).await;
        }
        self.config.lock().await.write();
        Ok(())
    }

    /// Set a single fan curve for a profile to enabled status. Will also
    /// activate a fan curve if in the same profile mode
    async fn set_profile_fan_curve_enabled(
        &mut self,
        profile: PlatformProfile,
        fan: FanCurvePU,
        enabled: bool,
    ) -> zbus::fdo::Result<()> {
        self.config
            .lock()
            .await
            .profiles
            .set_profile_fan_curve_enabled(profile, fan, enabled);
        let active: PlatformProfile = self.platform.get_platform_profile()?.into();
        if active == profile {
            self.config
                .lock()
                .await
                .profiles
                .write_profile_curve_to_platform(profile, &mut find_fan_curve_node()?)?;
            self.reapply_ppt(profile).await;
        }
        self.config.lock().await.write();
        Ok(())
    }

    /// Get the fan-curve data for the currently active ThrottlePolicy
    async fn fan_curve_data(
        &mut self,
        profile: PlatformProfile,
    ) -> zbus::fdo::Result<Vec<CurveData>> {
        let curve = self
            .config
            .lock()
            .await
            .profiles
            .get_fan_curves_for(profile)
            .to_vec();
        Ok(curve)
    }

    /// Set the fan curve for the specified profile.
    /// Will also activate the fan curve if the user is in the same mode.
    async fn set_fan_curve(
        &mut self,
        profile: PlatformProfile,
        curve: CurveData,
    ) -> zbus::fdo::Result<()> {
        self.config
            .lock()
            .await
            .profiles
            .save_fan_curve(curve, profile)?;
        let active: PlatformProfile = self.platform.get_platform_profile()?.into();
        if active == profile {
            self.config
                .lock()
                .await
                .profiles
                .write_profile_curve_to_platform(profile, &mut find_fan_curve_node()?)?;
            self.reapply_ppt(profile).await;
        }
        self.config.lock().await.write();
        Ok(())
    }

    /// Reset the stored (self) and device curves to the defaults of the
    /// platform.
    ///
    /// Each platform_profile has a different default and the default can be
    /// read only for the currently active profile.
    async fn set_curves_to_defaults(&mut self, profile: PlatformProfile) -> zbus::fdo::Result<()> {
        let active = self.platform.get_platform_profile()?;
        self.platform.set_platform_profile(profile.into())?;
        self.config
            .lock()
            .await
            .profiles
            .set_active_curve_to_defaults(profile, &mut find_fan_curve_node()?)?;
        self.platform.set_platform_profile(active.as_str())?;
        self.config.lock().await.write();
        Ok(())
    }
}

impl crate::ZbusRun for CtrlFanCurveZbus {
    async fn add_to_server(self, server: &mut Connection) {
        Self::add_to_server_helper(self, FAN_CURVE_ZBUS_PATH, server).await;
    }
}

impl crate::Reloadable for CtrlFanCurveZbus {
    /// Fetch the active profile and use that to set all related components up
    async fn reload(&mut self) -> Result<(), RogError> {
        let active = self.platform.get_platform_profile()?.into();
        let mut config = self.config.lock().await;
        if let Ok(mut device) = find_fan_curve_node() {
            config
                .profiles
                .write_profile_curve_to_platform(active, &mut device)?;
        }
        config.current = active;

        Ok(())
    }
}
