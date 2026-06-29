use std::collections::HashMap;
use std::sync::Arc;

use config_traits::StdConfig;
use log::{debug, error, info, warn};
use rog_platform::asus_armoury::{
    AttrValue, Attribute, FirmwareAttribute, FirmwareAttributeType, FirmwareAttributes,
};
use rog_platform::platform::{PlatformProfile, RogPlatform};
use rog_platform::power::AsusPower;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Type, Value};
use zbus::{fdo, interface, Connection};

use crate::config::Config;
use crate::error::RogError;
use crate::{Reloadable, ASUS_ZBUS_PATH};

const MOD_NAME: &str = "asus_armoury";

#[derive(Debug, Default, Clone, Deserialize, Serialize, Type, Value, OwnedValue)]
pub struct PossibleValues {
    strings: Vec<String>,
    nums: Vec<i32>,
}

fn dbus_path_for_attr(attr_name: &str) -> OwnedObjectPath {
    ObjectPath::from_str_unchecked(&format!("{ASUS_ZBUS_PATH}/{MOD_NAME}/{attr_name}")).into()
}

#[derive(Clone)]
pub struct AsusArmouryAttribute {
    attr: Attribute,
    config: Arc<Mutex<Config>>,
    queued_gpu: Arc<Mutex<HashMap<FirmwareAttribute, i32>>>,
    /// platform control required here for access to PPD or Throttle profile
    platform: RogPlatform,
    power: AsusPower,
}

impl AsusArmouryAttribute {
    pub fn new(
        attr: Attribute,
        platform: RogPlatform,
        power: AsusPower,
        config: Arc<Mutex<Config>>,
        queued_gpu: Arc<Mutex<HashMap<FirmwareAttribute, i32>>>,
    ) -> Self {
        Self {
            attr,
            config,
            queued_gpu,
            platform,
            power,
        }
    }

    pub fn attribute_name(&self) -> String {
        String::from(self.attr.name())
    }

    fn resolve_i32_value(refreshed: Option<i32>, cached: &AttrValue) -> i32 {
        refreshed
            .or(match cached {
                AttrValue::Integer(i) => Some(*i),
                _ => None,
            })
            .unwrap_or(-1)
    }

    pub async fn emit_limits(&self, connection: &Connection) -> Result<(), RogError> {
        let path = dbus_path_for_attr(self.attr.name());
        let signal = SignalEmitter::new(connection, path)?;
        self.min_value_changed(&signal).await?;
        self.max_value_changed(&signal).await?;
        self.scalar_increment_changed(&signal).await?;
        self.current_value_changed(&signal).await?;
        Ok(())
    }

    pub async fn move_to_zbus(self, connection: &Connection) -> Result<(), RogError> {
        let path = dbus_path_for_attr(self.attr.name());
        connection
            .object_server()
            .at(path.clone(), self)
            .await
            .map_err(|e| error!("Couldn't add server at path: {path}, {e:?}"))
            .ok();
        Ok(())
    }

    async fn watch_and_notify(
        &mut self,
        signal_ctxt: SignalEmitter<'static>,
    ) -> Result<(), RogError> {
        use futures_util::StreamExt;

        let name = self.name();
        macro_rules! watch_value_notify {
            ($attr_str:expr, $fn_prop_changed:ident) => {
                match self.attr.get_watcher() {
                    Ok(watch) => {
                        let name = <&str>::from(name);
                        let ctrl = self.clone();
                        let sig = signal_ctxt.clone();
                        tokio::spawn(async move {
                            let mut buffer = [0; 32];
                            if let Ok(stream) = watch.into_event_stream(&mut buffer) {
                                stream
                                    .for_each(|_| async {
                                        debug!("{} changed", name);
                                        ctrl.$fn_prop_changed(&sig).await.ok();
                                    })
                                    .await;
                            } else {
                                info!(
                                    "inotify event stream failed for {} ({}). You can ignore this \
                                     if unsupported",
                                    name, $attr_str
                                );
                            }
                        });
                    }
                    Err(e) => info!(
                        "inotify watch failed: {}. You can ignore this if your device does not \
                         support the feature",
                        e
                    ),
                }
            };
        }

        // "current_value", "default_value", "min_value", "max_value"
        watch_value_notify!("current_value", current_value_changed);
        watch_value_notify!("default_value", default_value_changed);
        watch_value_notify!("min_value", min_value_changed);
        watch_value_notify!("max_value", max_value_changed);

        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct ArmouryAttributeRegistry {
    attrs: Vec<AsusArmouryAttribute>,
}

impl ArmouryAttributeRegistry {
    pub fn push(&mut self, attr: AsusArmouryAttribute) {
        self.attrs.push(attr);
    }

    pub async fn emit_limits(&self, connection: &Connection) -> Result<(), RogError> {
        let mut last_err: Option<RogError> = None;
        for attr in &self.attrs {
            if let Err(e) = attr.emit_limits(connection).await {
                error!(
                    "Failed to emit updated limits for attribute '{}': {e:?}",
                    attr.attribute_name()
                );
                last_err = Some(e);
            }
        }
        if let Some(err) = last_err {
            Err(err)
        } else {
            Ok(())
        }
    }
}

impl crate::Reloadable for AsusArmouryAttribute {
    async fn reload(&mut self) -> Result<(), RogError> {
        info!("Reloading {}", self.attr.name());
        let attribute: FirmwareAttribute = self.attr.name().into();
        let name = self.attr.name();

        let mut config = self.config.lock().await;
        let apply_value = match attribute.property_type() {
            FirmwareAttributeType::Ppt => {
                let profile: PlatformProfile = self.platform.get_platform_profile()?.into();
                let power_plugged = self
                    .power
                    .get_online()
                    .map_err(|e| {
                        error!("Could not get power status: {e:?}");
                        e
                    })
                    .unwrap_or_default()
                    == 1;

                let apply_value = {
                    config.select_tunings_ref(power_plugged, profile).and_then(
                        |tuning| match tuning.enabled {
                            true => tuning.group.get(&self.name()).copied(),
                            false => None,
                        },
                    )
                };

                apply_value.map_or(AttrValue::None, AttrValue::Integer)
            }
            FirmwareAttributeType::Gpu => {
                if config.armoury_settings.remove(&attribute).is_some() {
                    info!("Removed persisted GPU attribute {name} from config");
                    config.write();
                }
                info!("Reload called on GPU attribute {name}: doing nothing");
                AttrValue::None
            }
            FirmwareAttributeType::ReadOnly => {
                if config.armoury_settings.remove(&attribute).is_some() {
                    info!("Removed stale persisted read-only attribute {name} from config");
                    config.write();
                }
                info!("Reload called on read-only attribute {name}: doing nothing");
                AttrValue::None
            }
            FirmwareAttributeType::Norestore => {
                if config.armoury_settings.remove(&attribute).is_some() {
                    info!("Removed stale persisted norestore attribute {name} from config");
                    config.write();
                }
                info!("Reload called on norestore attribute {name}: doing nothing");
                AttrValue::None
            }
            _ => {
                info!("Reload called on firmware attribute {name}");
                match config.armoury_settings.get(&attribute) {
                    Some(saved_value) => AttrValue::Integer(*saved_value),
                    None => AttrValue::None,
                }
            }
        };

        match apply_value {
            AttrValue::None => {
                info!(
                    "No saved value for attribute {}: skipping.",
                    self.attr.name()
                );
            }
            _ => {
                info!("Applying value {apply_value:?} to attribute {name}");
                self.attr.set_current_value(&apply_value).map_err(|e| {
                    error!("Could not set {name} value: {e:?}");
                    self.attr.base_path_exists();
                    e
                })?;

                info!("Restored asus-armoury setting {name} to {apply_value:?}");
            }
        }

        Ok(())
    }
}

/// If return is `-1` on a property then there is available value for that
/// property
#[interface(name = "xyz.ljones.AsusArmoury")]
impl AsusArmouryAttribute {
    #[zbus(property)]
    fn name(&self) -> FirmwareAttribute {
        self.attr.name().into()
    }

    #[zbus(property)]
    async fn available_attrs(&self) -> Vec<String> {
        let mut attrs = Vec::new();
        if !matches!(self.attr.default_value(), AttrValue::None) {
            attrs.push("default_value".to_string());
        }
        if !matches!(self.attr.min_value(), AttrValue::None) {
            attrs.push("min_value".to_string());
        }
        if !matches!(self.attr.max_value(), AttrValue::None) {
            attrs.push("max_value".to_string());
        }
        if !matches!(self.attr.scalar_increment(), AttrValue::None) {
            attrs.push("scalar_increment".to_string());
        }
        if !matches!(self.attr.possible_values(), AttrValue::None) {
            attrs.push("possible_values".to_string());
        }
        // TODO: Don't unwrap, use error
        if let Ok(value) = self.attr.current_value().map_err(|e| {
            error!("Failed to read: {e:?}");
            e
        }) {
            if !matches!(value, AttrValue::None) {
                attrs.push("current_value".to_string());
            }
        }
        attrs
    }

    /// If return is `-1` then there is no default value
    #[zbus(property)]
    async fn default_value(&self) -> i32 {
        match self.attr.default_value() {
            AttrValue::Integer(i) => *i,
            _ => -1,
        }
    }

    async fn restore_default(&self) -> fdo::Result<()> {
        self.attr.restore_default()?;
        if self.name().property_type() == FirmwareAttributeType::Ppt {
            let profile: PlatformProfile = self.platform.get_platform_profile()?.into();
            let power_plugged = self
                .power
                .get_online()
                .map_err(|e| {
                    error!("Could not get power status: {e:?}");
                    e
                })
                .unwrap_or_default();

            let mut config = self.config.lock().await;
            let tuning = config.select_tunings(power_plugged == 1, profile);
            if let Some(tune) = tuning.group.get_mut(&self.name()) {
                if let AttrValue::Integer(i) = self.attr.default_value() {
                    *tune = *i;
                }
            }
            if tuning.enabled {
                self.attr
                    .set_current_value(self.attr.default_value())
                    .map_err(|e| {
                        error!("Could not set value: {e:?}");
                        e
                    })?;
            }
            config.write();
        }
        Ok(())
    }

    #[zbus(property)]
    async fn min_value(&self) -> i32 {
        Self::resolve_i32_value(self.attr.refresh_min_value(), self.attr.min_value())
    }

    #[zbus(property)]
    async fn max_value(&self) -> i32 {
        Self::resolve_i32_value(self.attr.refresh_max_value(), self.attr.max_value())
    }

    #[zbus(property)]
    async fn scalar_increment(&self) -> i32 {
        Self::resolve_i32_value(
            self.attr.refresh_scalar_increment(),
            self.attr.scalar_increment(),
        )
    }

    #[zbus(property)]
    async fn possible_values(&self) -> Vec<i32> {
        match self.attr.possible_values() {
            AttrValue::EnumInt(i) => i.clone(),
            _ => Vec::default(),
        }
    }

    #[zbus(property)]
    async fn current_value(&self) -> fdo::Result<i32> {
        if self.name().property_type() == FirmwareAttributeType::Ppt {
            let profile: PlatformProfile = self.platform.get_platform_profile()?.into();
            let power_plugged = self
                .power
                .get_online()
                .map_err(|e| {
                    error!("Could not get power status: {e:?}");
                    e
                })
                .unwrap_or_default()
                == 1;
            let config = self.config.lock().await;
            if let Some(tuning) = config.select_tunings_ref(power_plugged, profile) {
                if let Some(tune) = tuning.group.get(&self.name()) {
                    return Ok(*tune);
                }
            }
            if let AttrValue::Integer(i) = self.attr.default_value() {
                return Ok(*i);
            }
            return Err(fdo::Error::Failed(
                "Could not read current value".to_string(),
            ));
        }

        /*
        // This code would override the current_value with queued GPU value if present
        // but I don't want to do that for now because it would cause confusion where
        // current_value doesn't reflect actual firmware state until apply_queued_gpu_value is called. Instead, queued GPU values are only visible through the queued_gpu_value property and are applied on shutdown without affecting current_value.
        if self.name().property_type() == FirmwareAttributeType::Gpu {
            if let Some(saved_value) = self.queued_gpu.lock().await.get(&self.name()) {
                return Ok(*saved_value);
            }
        }
        */

        if let Ok(AttrValue::Integer(i)) = self.attr.current_value() {
            return Ok(i);
        }
        Err(fdo::Error::Failed(
            "Could not read current value".to_string(),
        ))
    }

    #[zbus(property)]
    async fn set_current_value(&mut self, value: i32) -> fdo::Result<()> {
        let name = self.attr.name();

        // if read-only, don't even attempt to set or persist value
        if self.name().property_type() == FirmwareAttributeType::ReadOnly {
            warn!("Attempted to set read-only attribute {name}: write discarded");
            return Err(fdo::Error::NotSupported(format!(
                "{name} is read-only and cannot be changed"
            )));
        }

        let apply_value = match self.name().property_type() {
            FirmwareAttributeType::Ppt => {
                let profile: PlatformProfile = self.platform.get_platform_profile()?.into();
                let power_plugged = self
                    .power
                    .get_online()
                    .map_err(|e| {
                        error!("Could not get power status: {e:?}");
                        e
                    })
                    .unwrap_or_default();

                let mut config = self.config.lock().await;
                let tuning = config.select_tunings(power_plugged == 1, profile);

                if let Some(tune) = tuning.group.get_mut(&self.name()) {
                    *tune = value;
                } else {
                    tuning.group.insert(self.name(), value);
                    debug!("Store tuning config for {name} = {:?}", value);
                }

                match tuning.enabled {
                    true => {
                        debug!("Tuning is enabled: setting value to PPT property {name} = {value}");
                        AttrValue::Integer(value)
                    }
                    false => {
                        warn!("Tuning is disabled: skipping setting value to PPT property {name}");
                        AttrValue::None
                    }
                }
            }
            FirmwareAttributeType::Gpu => {
                debug!("Queueing GPU attribute {name} = {value} for delayed apply");
                self.queued_gpu.lock().await.insert(self.name(), value);
                return Ok(());
            }
            FirmwareAttributeType::Norestore => {
                debug!("Setting norestore attribute {name} = {value} synchronously");
                self.attr
                    .set_current_value(&AttrValue::Integer(value))
                    .map_err(|e| {
                        error!("Could not set value {value} to attribute {name}: {e:?}");
                        e
                    })?;
                return Ok(());
            }
            _ => {
                let mut settings = self.config.lock().await;
                settings
                    .armoury_settings
                    .entry(self.name())
                    .and_modify(|setting| {
                        debug!("Set config for {name} = {value}");
                        *setting = value;
                    })
                    .or_insert_with(|| {
                        debug!("Adding config for {name} = {value}");
                        value
                    });

                AttrValue::Integer(value)
            }
        };

        // Only write to sysfs if we have a real value to apply.
        // When tuning is disabled, the value is stored in config but not
        // written to hardware — it will be applied when tuning is enabled.
        if !matches!(apply_value, AttrValue::None) {
            self.attr.set_current_value(&apply_value).map_err(|e| {
                error!("Could not set value {value} to attribute {name}: {e:?}");
                e
            })?;
        }

        // write config after setting value
        self.config.lock().await.write();

        // When an nv_* attribute (Nvidia TDP/temp) is written, restart
        // nvidia-powerd so it re-reads the new TDP limits from hardware.
        match self.name() {
            FirmwareAttribute::NvDynamicBoost
            | FirmwareAttribute::NvTempTarget
            | FirmwareAttribute::DgpuTgp => {
                let _ = std::process::Command::new("systemctl")
                    .args([
                        "try-restart",
                        "nvidia-powerd.service",
                    ])
                    .output();
            }
            _ => {}
        }

        Ok(())
    }

    /// Returns queued GPU value when present, otherwise `-1`.
    #[zbus(property)]
    async fn queued_gpu_value(&self) -> fdo::Result<i32> {
        if self.name().property_type() != FirmwareAttributeType::Gpu {
            return Ok(-1);
        }

        Ok(self
            .queued_gpu
            .lock()
            .await
            .get(&self.name())
            .copied()
            .unwrap_or(-1))
    }

    /// Applies queued GPU value if present and returns whether anything was applied.
    async fn apply_queued_gpu_value(&mut self) -> fdo::Result<bool> {
        if self.name().property_type() != FirmwareAttributeType::Gpu {
            return Ok(false);
        }

        let name = self.name();
        let value = {
            let queue = self.queued_gpu.lock().await;
            let Some(value) = queue.get(&name).copied() else {
                return Ok(false);
            };
            value
        };

        self.attr
            .set_current_value(&AttrValue::Integer(value))
            .map_err(|e| {
                error!(
                    "Could not apply queued GPU attribute {} = {value}: {e:?}",
                    <&str>::from(name)
                );
                e
            })?;

        info!(
            "Applied queued GPU attribute {} = {value}",
            <&str>::from(name)
        );

        // Remove only after successful firmware write so transient failures do
        // not lose deferred shutdown values.
        self.queued_gpu.lock().await.remove(&name);

        Ok(true)
    }
}

pub async fn start_attributes_zbus(
    conn: &Connection,
    platform: RogPlatform,
    power: AsusPower,
    attributes: FirmwareAttributes,
    config: Arc<Mutex<Config>>,
) -> Result<ArmouryAttributeRegistry, RogError> {
    let mut registry = ArmouryAttributeRegistry::default();
    let queued_gpu = Arc::new(Mutex::new(HashMap::new()));
    for attr in attributes.attributes() {
        let mut attr = AsusArmouryAttribute::new(
            attr.clone(),
            platform.clone(),
            power.clone(),
            config.clone(),
            queued_gpu.clone(),
        );

        let registry_attr = attr.clone();

        if let Err(e) = attr.reload().await {
            error!(
                "Skipping attribute '{}' due to reload error: {e:?}",
                attr.attr.name()
            );
            continue;
        }

        let attr_name = attr.attribute_name();

        let path = dbus_path_for_attr(attr_name.as_str());
        match zbus::object_server::SignalEmitter::new(conn, path) {
            Ok(sig) => {
                if let Err(e) = attr.watch_and_notify(sig).await {
                    error!("Failed to start watcher for '{}': {e:?}", attr.attr.name());
                }
            }
            Err(e) => {
                error!(
                    "Failed to create SignalEmitter for '{}': {e:?}",
                    attr.attr.name()
                );
            }
        }

        if let Err(e) = attr.move_to_zbus(conn).await {
            error!("Failed to register attribute '{attr_name}' on zbus: {e:?}");
            continue;
        }

        registry.push(registry_attr);
    }
    Ok(registry)
}

pub async fn set_config_or_default(
    attrs: &FirmwareAttributes,
    config: &mut Config,
    power_plugged: bool,
    profile: PlatformProfile,
) {
    let mut changed = false;
    for attr in attrs.attributes().iter() {
        let name: FirmwareAttribute = attr.name().into();
        match name.property_type() {
            FirmwareAttributeType::Ppt => {
                let tuning = config.select_tunings(power_plugged, profile);
                if !tuning.enabled {
                    debug!("Tuning group is not enabled, skipping");
                    continue;
                }

                if let Some(tune) = tuning.group.get(&name) {
                    attr.set_current_value(&AttrValue::Integer(*tune))
                        .map_err(|e| {
                            error!("Failed to set {}: {e}", <&str>::from(name));
                        })
                        .ok();
                } else {
                    let default = attr.default_value();
                    attr.set_current_value(default)
                        .map_err(|e| {
                            error!("Failed to set {}: {e}", <&str>::from(name));
                        })
                        .ok();
                    if let AttrValue::Integer(i) = default {
                        tuning.group.insert(name, *i);
                        info!(
                            "Set default tuning config for {} = {:?}",
                            <&str>::from(name),
                            i
                        );
                        changed = true;
                    }
                }
            }
            FirmwareAttributeType::Gpu => {
                // Clean stale persisted queue from older versions. GPU deferred
                // writes are now in-memory and are applied only on shutdown.
                if config.armoury_settings.remove(&name).is_some() {
                    info!(
                        "Removed stale persisted GPU attribute {} from config",
                        <&str>::from(name)
                    );
                    changed = true;
                }
            }
            FirmwareAttributeType::ReadOnly => {
                if config.armoury_settings.remove(&name).is_some() {
                    info!(
                        "Removed stale persisted read-only attribute {} from config",
                        <&str>::from(name)
                    );
                    changed = true;
                }
                // Never restore or apply read-only attributes
            }
            FirmwareAttributeType::Norestore => {
                if config.armoury_settings.remove(&name).is_some() {
                    info!(
                        "Removed stale persisted norestore attribute {} from config",
                        <&str>::from(name)
                    );
                    changed = true;
                }
                // Never restore or apply norestore attributes
            }
            _ => {
                if let Some(saved_value) = config.armoury_settings.get(&name) {
                    attr.set_current_value(&AttrValue::Integer(*saved_value))
                        .map_err(|e| {
                            error!("Failed to set {}: {e}", <&str>::from(name));
                        })
                        .ok();
                    info!(
                        "Restored armoury setting for {} = {:?}",
                        <&str>::from(name),
                        saved_value
                    );
                }
            }
        }
    }
    if changed {
        config.write();
    }
}
