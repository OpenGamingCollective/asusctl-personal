// Plan:
// - Manager has udev monitor on USB looking for ROG devices
// - If a device is found, add it to watch
// - Add it to Zbus server
// - If udev sees device removed then remove the zbus path
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dmi_id::DMIID;
use log::{debug, error, info, warn};
use mio::{Events, Interest, Poll, Token};
use rog_platform::error::PlatformError;
use rog_platform::hid_raw::HidRaw;
use tokio::sync::Mutex;
use udev::{Device, MonitorBuilder};
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::Connection;

use crate::aura_anime::trait_impls::AniMeZbus;
use crate::aura_lamparray::trait_impls::LampArrayZbus;
use crate::aura_laptop::trait_impls::AuraZbus;
use crate::aura_scsi::trait_impls::ScsiZbus;
use crate::aura_slash::trait_impls::SlashZbus;
use crate::aura_types::DeviceHandle;
use crate::error::RogError;
use crate::ASUS_ZBUS_PATH;

const MOD_NAME: &str = "aura";

/// info! only in debug builds — release stays quiet.
macro_rules! debug_info {
    ($($arg:tt)*) => {{
        if cfg!(debug_assertions) {
            log::info!($($arg)*);
        }
    }};
}

/// Returns only the Device details concatenated in a form usable for
/// adding/appending to a filename
pub fn filename_partial(parent: &Device) -> Option<OwnedObjectPath> {
    if let Some(id_product) = parent.attribute_value("idProduct") {
        let id_product = id_product.to_string_lossy();
        let mut path = if let Some(devnum) = parent.attribute_value("devnum") {
            let devnum = devnum.to_string_lossy();
            if let Some(devpath) = parent.attribute_value("devpath") {
                let devpath = devpath.to_string_lossy();
                format!("{id_product}_{devnum}_{devpath}")
            } else {
                format!("{id_product}_{devnum}")
            }
        } else {
            format!("{id_product}")
        };
        if path.contains('.') {
            warn!("dbus path for {id_product} contains `.`, removing");
            path.replace('.', "").clone_into(&mut path);
        }
        return Some(ObjectPath::from_str_unchecked(&path).into());
    }
    None
}

fn dbus_path_for_dev(parent: &Device) -> Option<OwnedObjectPath> {
    if let Some(filename) = filename_partial(parent) {
        return Some(
            ObjectPath::from_str_unchecked(&format!("{ASUS_ZBUS_PATH}/{MOD_NAME}/{filename}"))
                .into(),
        );
    }
    None
}

fn dbus_path_for_tuf() -> OwnedObjectPath {
    ObjectPath::from_str_unchecked(&format!("{ASUS_ZBUS_PATH}/{MOD_NAME}/tuf")).into()
}

fn dbus_path_for_slash() -> OwnedObjectPath {
    ObjectPath::from_str_unchecked(&format!("{ASUS_ZBUS_PATH}/{MOD_NAME}/slash")).into()
}

fn dbus_path_for_anime() -> OwnedObjectPath {
    ObjectPath::from_str_unchecked(&format!("{ASUS_ZBUS_PATH}/{MOD_NAME}/anime")).into()
}

fn dbus_path_for_scsi(prod_id: &str) -> OwnedObjectPath {
    ObjectPath::from_str_unchecked(&format!("{ASUS_ZBUS_PATH}/{MOD_NAME}/{prod_id}_scsi")).into()
}

fn dev_prop_matches(dev: &Device, prop: &str, value: &str) -> bool {
    if let Some(p) = dev.property_value(prop) {
        return p == value;
    }
    false
}

/// A device.
///
/// Each controller within should track its dbus path so it can be removed if
/// required.
pub struct AsusDevice {
    device: DeviceHandle,
    dbus_path: OwnedObjectPath,
    hid_key: Option<String>,
}

pub struct DeviceManager {
    _dbus_connection: Connection,
    _hid_handles: Arc<Mutex<HashMap<String, Arc<Mutex<HidRaw>>>>>,
}

impl DeviceManager {
    #[allow(clippy::type_complexity)]
    async fn get_or_create_hid_handle(
        handles: &Arc<Mutex<HashMap<String, Arc<Mutex<HidRaw>>>>>,
        endpoint: &Device,
    ) -> Result<(Arc<Mutex<HidRaw>>, String), RogError> {
        let dev_node = endpoint
            .devnode()
            .ok_or_else(|| RogError::MissingFunction("hidraw devnode missing".to_string()))?;
        let key = dev_node.to_string_lossy().to_string();
        if let Some(existing) = handles.lock().await.get(&key).cloned() {
            return Ok((existing, key));
        }
        let hidraw = HidRaw::from_device(endpoint.clone())?;
        let handle = Arc::new(Mutex::new(hidraw));
        handles.lock().await.insert(key.clone(), handle.clone());
        Ok((handle, key))
    }

    async fn init_hid_devices(
        connection: &Connection,
        device: Device,
        handles: Arc<Mutex<HashMap<String, Arc<Mutex<HidRaw>>>>>,
    ) -> Result<Vec<AsusDevice>, RogError> {
        let mut devices = Vec::new();
        let sysname_dbg = device.sysname().to_string_lossy().to_string();
        let devnode_dbg = device
            .devnode()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "<none>".to_string());
        debug_info!(
            "init_hid_devices: probing hidraw sysname={sysname_dbg} devnode={devnode_dbg}"
        );

        if let Some(usb_device) = device.parent_with_subsystem_devtype("usb", "usb_device")? {
            if let Some(usb_id) = usb_device.attribute_value("idProduct") {
                if let Some(vendor_id) = usb_device.attribute_value("idVendor") {
                    if vendor_id != "0b05" {
                        debug!("Not ASUS vendor ID: {}", vendor_id.to_string_lossy());
                        return Ok(devices);
                    }
                    // Almost all devices are identified by the productId.
                    // So let's see what we have and:
                    // 1. Generate an interface path
                    // 2. Create the device
                    // Use the top-level endpoint, not the parent
                    if let Ok((dev, hid_key)) =
                        Self::get_or_create_hid_handle(&handles, &device).await
                    {
                        debug!("Testing device {usb_id:?}");
                        // SLASH DEVICE
                        if let Ok(dev_type) = DeviceHandle::new_slash_hid(
                            dev.clone(),
                            usb_id.to_str().unwrap_or_default(),
                        )
                        .await
                        {
                            if let DeviceHandle::Slash(slash) = dev_type.clone() {
                                let path =
                                    dbus_path_for_dev(&usb_device).unwrap_or(dbus_path_for_slash());
                                let ctrl = SlashZbus::new(slash);
                                ctrl.start_tasks(connection, path.clone()).await.unwrap();
                                devices.push(AsusDevice {
                                    device: dev_type,
                                    dbus_path: path,
                                    hid_key: Some(hid_key.clone()),
                                });
                            }
                        }
                        // ANIME MATRIX DEVICE
                        if let Ok(dev_type) = DeviceHandle::maybe_anime_hid(
                            dev.clone(),
                            usb_id.to_str().unwrap_or_default(),
                        )
                        .await
                        {
                            if let DeviceHandle::AniMe(anime) = dev_type.clone() {
                                let path =
                                    dbus_path_for_dev(&usb_device).unwrap_or(dbus_path_for_anime());
                                let ctrl = AniMeZbus::new(anime);
                                ctrl.start_tasks(connection, path.clone()).await.unwrap();
                                devices.push(AsusDevice {
                                    device: dev_type,
                                    dbus_path: path,
                                    hid_key: Some(hid_key.clone()),
                                });
                            }
                        }
                        // AURA LAPTOP DEVICE
                        if let Ok(dev_type) = DeviceHandle::maybe_laptop_aura(
                            Some(dev),
                            usb_id.to_str().unwrap_or_default(),
                        )
                        .await
                        {
                            if let DeviceHandle::Aura(aura) = dev_type.clone() {
                                let path =
                                    dbus_path_for_dev(&usb_device).unwrap_or(dbus_path_for_tuf());
                                let ctrl = AuraZbus::new(aura);
                                ctrl.start_tasks(connection, path.clone()).await.unwrap();
                                devices.push(AsusDevice {
                                    device: dev_type,
                                    dbus_path: path,
                                    hid_key: Some(hid_key),
                                });
                            }
                        }
                    } else {
                        warn!("Failed to initialise shared hid handle for {usb_id:?}");
                    }
                }
            }
        }
        if devices.is_empty() {
            debug_info!(
                "init_hid_devices: no USB-side device matched for {sysname_dbg}, trying I2C-HID fallback"
            );
            match Self::init_i2c_hid_device(connection, &device, handles.clone()).await {
                Ok(mut found) => {
                    debug_info!(
                        "init_hid_devices: I2C-HID fallback for {sysname_dbg} returned {} device(s)",
                        found.len()
                    );
                    devices.append(&mut found);
                }
                Err(e) => {
                    error!(
                        "init_hid_devices: I2C-HID fallback for {sysname_dbg} failed: {e:?}"
                    );
                }
            }
        }
        Ok(devices)
    }

    async fn init_i2c_hid_device(
        connection: &Connection,
        endpoint: &Device,
        handles: Arc<Mutex<HashMap<String, Arc<Mutex<HidRaw>>>>>,
    ) -> Result<Vec<AsusDevice>, RogError> {
        let mut devices = Vec::new();
        let sysname = endpoint.sysname().to_string_lossy().to_string();
        debug_info!("I2C-HID probe: examining hidraw {sysname}");
        let mut hid_id: Option<String> = None;
        let mut cur = endpoint.parent();
        while let Some(p) = cur {
            if let Some(val) = p.property_value("HID_ID") {
                hid_id = Some(val.to_string_lossy().to_string());
                break;
            }
            cur = p.parent();
        }
        let Some(hid_id) = hid_id else {
            debug_info!("I2C-HID probe: no HID_ID property found for {sysname}");
            return Ok(devices);
        };
        debug_info!("I2C-HID probe: {sysname} HID_ID={hid_id}");
        let parts: Vec<&str> = hid_id.split(':').collect();
        if parts.len() != 3 {
            debug_info!("I2C-HID probe: HID_ID has unexpected shape: {hid_id}");
            return Ok(devices);
        }
        // The HID_ID format is "BUS:VENDOR:PRODUCT" where VENDOR and PRODUCT are
        // 8-char hex strings (e.g. "0018:00000B05:000019B6"). We must parse them
        // numerically — a naive string compare against "0B05" fails because the
        // actual VID string is "00000B05".
        let vendor_str = parts[1];
        let product_str = parts[2];
        let vendor = u32::from_str_radix(vendor_str, 16).unwrap_or(0xFFFF_FFFF);
        let product = u32::from_str_radix(product_str, 16).unwrap_or(0xFFFF_FFFF);
        debug_info!(
            "I2C-HID probe: VID parsing raw='{vendor_str}' parsed=0x{vendor:08x}; PID parsing raw='{product_str}' parsed=0x{product:08x}"
        );
        if vendor != 0x0b05 {
            debug_info!(
                "I2C-HID probe: not ASUS vendor 0x{vendor:08x} (raw '{vendor_str}'), skipping {sysname}"
            );
            return Ok(devices);
        }
        let vid_upper = format!("{:04X}", vendor as u16);
        let pid_upper = format!("{:04X}", product as u16);
        let prod_id_str = pid_upper.to_lowercase();
        info!(
            "Found ASUS HID LampArray candidate: {sysname} VID={vid_upper} PID={pid_upper}"
        );
        debug_info!("VID match: proceeding to open hidraw for {sysname}");
        debug_info!("Step1: about to query devnode for {sysname}");
        let dev_node = match endpoint.devnode() {
            Some(n) => n,
            None => {
                error!("I2C-HID probe: hidraw devnode missing for {sysname}");
                return Err(RogError::MissingFunction(
                    "I2C-HID hidraw devnode missing".to_string(),
                ));
            }
        };
        debug_info!("Step2: devnode={dev_node:?} for {sysname}");
        let key = dev_node.to_string_lossy().to_string();
        debug_info!("Step3a: about to take handles map lock for cache lookup key={key}");
        let cached = handles.lock().await.get(&key).cloned();
        debug_info!(
            "Step3b: handles map lock released for {key} cached={}",
            cached.is_some()
        );
        let handle = if let Some(existing) = cached {
            debug_info!("I2C-HID probe: reusing existing hidraw handle for {key}");
            existing
        } else {
            debug_info!(
                "Step3c: about to call HidRaw::from_i2c_device for {key} prod_id={prod_id_str}"
            );
            // udev::Device contains a raw `*mut udev` and is !Send, so we
            // cannot ship it into spawn_blocking. Instead we call the
            // synchronous constructor here — the actual blocking syscall is
            // the OpenOptions::open in from_i2c_device, and we mitigate that
            // separately by passing O_NONBLOCK there.
            debug_info!("Step3d: calling HidRaw::from_i2c_device synchronously for {key}");
            let hidraw_res = HidRaw::from_i2c_device(endpoint.clone(), &prod_id_str);
            let hidraw = match hidraw_res {
                Ok(h) => h,
                Err(e) => {
                    error!(
                        "I2C-HID probe: HidRaw::from_i2c_device FAILED for {key}: {e:?}"
                    );
                    return Err(e.into());
                }
            };
            debug_info!("Step4: hidraw handle created for {key}");
            let h = Arc::new(Mutex::new(hidraw));
            debug_info!("Step4b: about to insert handle into handles map key={key}");
            handles.lock().await.insert(key.clone(), h.clone());
            debug_info!("Step4c: handle inserted into handles map key={key}");
            h
        };
        debug_info!(
            "Step5: about to call DeviceHandle::maybe_lamparray for {sysname} prod_id={prod_id_str}"
        );
        debug_info!("Calling DeviceHandle::maybe_lamparray for {sysname} prod_id={prod_id_str}");
        let result = DeviceHandle::maybe_lamparray(handle, &prod_id_str).await;
        debug_info!("Step6: maybe_lamparray returned for {sysname}");
        match result {
            Ok(dev_type) => {
                debug_info!("maybe_lamparray OK for {sysname}");
                if let DeviceHandle::LampArray(lamparray) = dev_type.clone() {
                    let path: OwnedObjectPath = ObjectPath::from_str_unchecked(&format!(
                        "{ASUS_ZBUS_PATH}/{MOD_NAME}/lamparray_{prod_id_str}"
                    ))
                    .into();
                    info!("Registering LampArray device on zbus at path={path:?}");
                    let ctrl = LampArrayZbus::new(lamparray);
                    match ctrl.start_tasks(connection, path.clone()).await {
                        Ok(_) => debug_info!("LampArray zbus start_tasks OK for {path:?}"),
                        Err(e) => {
                            error!(
                                "LampArray zbus start_tasks FAILED for {path:?}: {e:?}"
                            );
                            return Ok(devices);
                        }
                    }
                    devices.push(AsusDevice {
                        device: dev_type,
                        dbus_path: path.clone(),
                        hid_key: Some(key),
                    });
                    debug_info!("LampArray device added to manager at {path:?}");
                } else {
                    debug_info!(
                        "maybe_lamparray returned non-LampArray variant for {sysname}, ignoring"
                    );
                }
            }
            Err(e) => {
                error!("maybe_lamparray FAILED for {sysname}: {e:?}");
            }
        }
        Ok(devices)
    }

    /// To be called on daemon startup
    async fn init_all_hid(
        connection: &Connection,
        handles: Arc<Mutex<HashMap<String, Arc<Mutex<HidRaw>>>>>,
    ) -> Result<Vec<AsusDevice>, RogError> {
        // Ensure we only process one hidraw interface per physical USB device.
        // A USB device can expose multiple HID interfaces (and thus multiple hidraw nodes).
        // Processing more than one causes duplicate device initialisation which can
        // interfere with the kernel's own HID driver and trigger a USB reset loop.
        let mut seen_usb_parents: HashSet<String> = HashSet::new();
        let mut devices: Vec<AsusDevice> = Vec::new();
        let mut enumerator = udev::Enumerator::new().map_err(|err| {
            warn!("{}", err);
            PlatformError::Udev("enumerator failed".into(), err)
        })?;
        enumerator.match_subsystem("hidraw").map_err(|err| {
            warn!("{}", err);
            PlatformError::Udev("match_subsystem failed".into(), err)
        })?;
        debug_info!("LampArray initial enumeration: scanning hidraw devices");
        for device in enumerator
            .scan_devices()
            .map_err(|e| PlatformError::IoPath("enumerator".to_owned(), e))?
        {
            // Only deduplicate ASUS devices; non-ASUS multi-interface devices are unaffected.
            if let Ok(Some(usb_parent)) = device.parent_with_subsystem_devtype("usb", "usb_device")
            {
                if usb_parent.attribute_value("idVendor") == Some(std::ffi::OsStr::new("0b05")) {
                    let syspath = usb_parent.syspath().to_string_lossy().to_string();
                    if !seen_usb_parents.insert(syspath) {
                        debug!("Skipping duplicate hidraw for USB parent already processed");
                        continue;
                    }
                }
            }
            devices.append(&mut Self::init_hid_devices(connection, device, handles.clone()).await?);
        }
        Ok(devices)
    }

    async fn init_scsi(
        connection: &Connection,
        device: &Device,
        path: OwnedObjectPath,
    ) -> Option<AsusDevice> {
        // "ID_MODEL_ID" "1932"
        // "ID_VENDOR_ID" "0b05"
        if dev_prop_matches(device, "ID_VENDOR_ID", "0b05") {
            if let Some(dev_node) = device.devnode() {
                let prod_id = device
                    .property_value("ID_MODEL_ID")
                    .unwrap_or_default()
                    .to_string_lossy();
                if let Ok(dev_type) =
                    DeviceHandle::maybe_scsi(dev_node.as_os_str().to_str().unwrap(), &prod_id).await
                {
                    if let DeviceHandle::Scsi(scsi) = dev_type.clone() {
                        let ctrl = ScsiZbus::new(scsi);
                        ctrl.start_tasks(connection, path.clone()).await.unwrap();
                        return Some(AsusDevice {
                            device: dev_type,
                            dbus_path: path,
                            hid_key: None,
                        });
                    }
                }
            }
        }
        None
    }

    async fn init_all_scsi(connection: &Connection) -> Result<Vec<AsusDevice>, RogError> {
        // track and ensure we use only one hidraw per prod_id
        // let mut interfaces = HashSet::new();
        let mut devices: Vec<AsusDevice> = Vec::new();
        let mut enumerator = udev::Enumerator::new().map_err(|err| {
            warn!("{}", err);
            PlatformError::Udev("enumerator failed".into(), err)
        })?;
        enumerator.match_subsystem("block").map_err(|err| {
            warn!("{}", err);
            PlatformError::Udev("match_subsystem failed".into(), err)
        })?;
        let mut found = Vec::new();
        for device in enumerator
            .scan_devices()
            .map_err(|e| PlatformError::IoPath("enumerator".to_owned(), e))?
        {
            if let Some(serial) = device.property_value("ID_SERIAL_SHORT") {
                let serial = serial.to_string_lossy().to_string();
                let path = dbus_path_for_scsi(&serial);
                if found.contains(&path) {
                    continue;
                }
                if let Some(dev) = Self::init_scsi(connection, &device, path.clone()).await {
                    devices.push(dev);
                    found.push(path);
                }
            } else {
                debug!("No serial for SCSI device: {:?}", device.devpath());
            }
        }
        Ok(devices)
    }

    pub async fn find_all_devices(
        connection: &Connection,
        handles: Arc<Mutex<HashMap<String, Arc<Mutex<HidRaw>>>>>,
    ) -> Vec<AsusDevice> {
        let mut devices: Vec<AsusDevice> = Vec::new();
        // HID first, always
        if let Ok(devs) = &mut Self::init_all_hid(connection, handles.clone()).await {
            devices.append(devs);
        }
        // USB after, need to check if HID picked something up and if so, skip it
        let mut do_anime = true;
        let mut do_slash = true;
        let mut do_kb_backlight = true;
        for dev in devices.iter() {
            if matches!(dev.device, DeviceHandle::Slash(_)) {
                do_slash = false;
            }
            if matches!(dev.device, DeviceHandle::AniMe(_)) {
                do_anime = false;
            }
            if matches!(
                dev.device,
                DeviceHandle::Aura(_) | DeviceHandle::OldAura(_) | DeviceHandle::LampArray(_)
            ) {
                do_kb_backlight = false;
            }
        }
        if do_slash {
            if let Ok(dev_type) = DeviceHandle::new_slash_usb().await {
                if let DeviceHandle::Slash(slash) = dev_type.clone() {
                    let path = dbus_path_for_slash();
                    let ctrl = SlashZbus::new(slash);
                    ctrl.start_tasks(connection, path.clone()).await.unwrap();
                    devices.push(AsusDevice {
                        device: dev_type,
                        dbus_path: path,
                        hid_key: None,
                    });
                }
            } else {
                info!("Tested device was not Slash");
            }
        }
        if do_anime {
            if let Ok(dev_type) = DeviceHandle::maybe_anime_usb().await {
                // TODO: this is copy/pasted
                if let DeviceHandle::AniMe(anime) = dev_type.clone() {
                    let path = dbus_path_for_anime();
                    let ctrl = AniMeZbus::new(anime);
                    if ctrl
                        .start_tasks(connection, path.clone())
                        .await
                        .map_err(|e| error!("Failed to start tasks: {e:?}, not adding this device"))
                        .is_ok()
                    {
                        devices.push(AsusDevice {
                            device: dev_type,
                            dbus_path: path,
                            hid_key: None,
                        });
                    }
                }
            } else {
                info!("Tested device was not AniMe Matrix");
            }
        }
        if do_kb_backlight {
            // TUF AURA LAPTOP DEVICE
            // product_name = ASUS TUF Gaming F15 FX507ZE_FX507ZE
            // product_family = ASUS TUF Gaming F15
            let product_name = DMIID::new().unwrap_or_default().product_name;
            let product_family = DMIID::new().unwrap_or_default().product_family;
            info!(
                "No USB keyboard aura, system is {product_name}, try using sysfs backlight control"
            );
            if product_name.contains("TUF") || product_family.contains("TUF") {
                info!("TUF laptop, try using sysfs backlight control");
                if let Ok(dev_type) = DeviceHandle::maybe_laptop_aura(None, "tuf").await {
                    if let DeviceHandle::Aura(aura) = dev_type.clone() {
                        let path = dbus_path_for_tuf();
                        let ctrl = AuraZbus::new(aura);
                        ctrl.start_tasks(connection, path.clone()).await.unwrap();
                        devices.push(AsusDevice {
                            device: dev_type,
                            dbus_path: path,
                            hid_key: None,
                        });
                    }
                }
            }
        }
        if let Ok(devs) = &mut Self::init_all_scsi(connection).await {
            devices.append(devs);
        }
        devices
    }

    pub async fn new(connection: Connection) -> Result<Self, RogError> {
        let conn_copy = connection.clone();
        let hid_handles = Arc::new(Mutex::new(HashMap::new()));
        let devices = Self::find_all_devices(&conn_copy, hid_handles.clone()).await;
        info!("Found {} valid devices on startup", devices.len());
        let devices = Arc::new(Mutex::new(devices));
        let manager = Self {
            _dbus_connection: connection,
            _hid_handles: hid_handles.clone(),
        };
        // TODO: The /sysfs/ LEDs don't cause events, so they need to be manually
        // checked for and added
        let hid_handles_thread = hid_handles.clone();
        std::thread::spawn(move || {
            let mut monitor = MonitorBuilder::new()?.listen()?;
            let mut poll = Poll::new()?;
            let mut events = Events::with_capacity(1024);
            poll.registry()
                .register(&mut monitor, Token(0), Interest::READABLE)?;
            let rt = tokio::runtime::Runtime::new().expect("Unable to create Runtime");
            let _enter = rt.enter();
            loop {
                if poll.poll(&mut events, None).is_err() {
                    continue;
                }
                for event in monitor.iter() {
                    let action = event
                        .action()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    let subsys = if let Some(subsys) = event.subsystem() {
                        subsys.to_string_lossy().to_string()
                    } else {
                        continue;
                    };
                    let devices = devices.clone();
                    let conn_copy = conn_copy.clone();
                    let hid_handles = hid_handles_thread.clone();
                    rt.block_on(async move {
                        // SCSCI devs
                        if subsys == "block" {
                            if action == "remove" {
                                if let Some(serial) =
                                    event.device().property_value("ID_SERIAL_SHORT")
                                {
                                    let serial = serial.to_string_lossy().to_string();
                                    let path = dbus_path_for_scsi(&serial);
                                    let index = if let Some(index) = devices
                                        .lock()
                                        .await
                                        .iter()
                                        .position(|dev| dev.dbus_path == path)
                                    {
                                        index
                                    } else {
                                        if dev_prop_matches(&event.device(), "ID_VENDOR_ID", "0b05")
                                        {
                                            warn!("No device for dbus path: {path:?}");
                                        }
                                        return Ok(());
                                    };
                                    info!("removing: {path:?}");
                                    let dev = devices.lock().await.remove(index);
                                    let path = path.clone();
                                    if let DeviceHandle::Scsi(_) = dev.device {
                                        conn_copy
                                            .object_server()
                                            .remove::<ScsiZbus, _>(&path)
                                            .await?;
                                    }
                                }
                            } else if action == "add" {
                                let evdev = event.device();
                                if let Some(serial) = evdev.property_value("ID_SERIAL_SHORT") {
                                    let serial = serial.to_string_lossy().to_string();
                                    let path = dbus_path_for_scsi(&serial);
                                    if let Some(new_devs) =
                                        Self::init_scsi(&conn_copy, &evdev, path).await
                                    {
                                        devices.lock().await.append(&mut vec![new_devs]);
                                    }
                                }
                            };
                        }
                        if subsys == "hidraw" {
                            if action == "remove" {
                                // Key cleanup off the removed hidraw node itself, NOT the
                                // USB parent. By the time a `remove` uevent arrives the USB
                                // parent is usually already detached from sysfs, so
                                // `parent_with_subsystem_devtype` returns None; gating the
                                // cleanup on it meant we kept the `Arc<Mutex<HidRaw>>` (held
                                // by both the handle map and the live zbus object) alive, so
                                // the open hidraw `File` was never dropped. The kernel only
                                // frees a hidraw minor once no fd remains open on it, so every
                                // re-enumeration (s2idle resume, dock/undock) leaked one
                                // minor until the 64-entry pool was exhausted system-wide.
                                // The uevent always carries DEVNAME, so the devnode is the
                                // stable key (and is exactly what we store as `hid_key`).
                                let removed_node = event
                                    .device()
                                    .devnode()
                                    .map(|n| n.to_string_lossy().to_string());
                                if let Some(removed_node) = removed_node {
                                    // Tear down any zbus objects backed by this node.
                                    let removals: Vec<usize> = devices
                                        .lock()
                                        .await
                                        .iter()
                                        .enumerate()
                                        .filter_map(|(i, dev)| {
                                            if dev.hid_key.as_deref()
                                                == Some(removed_node.as_str())
                                            {
                                                Some(i)
                                            } else {
                                                None
                                            }
                                        })
                                        .collect();
                                    // Iter in reverse so as to not screw up indexing
                                    for index in removals.iter().rev() {
                                        let dev = devices.lock().await.remove(*index);
                                        let path = dev.dbus_path.clone();
                                        let res = match dev.device {
                                            DeviceHandle::Aura(_) => {
                                                conn_copy
                                                    .object_server()
                                                    .remove::<AuraZbus, _>(&path)
                                                    .await?
                                            }
                                            DeviceHandle::LampArray(_) => {
                                                conn_copy
                                                    .object_server()
                                                    .remove::<LampArrayZbus, _>(&path)
                                                    .await?
                                            }
                                            DeviceHandle::Slash(_) => {
                                                conn_copy
                                                    .object_server()
                                                    .remove::<SlashZbus, _>(&path)
                                                    .await?
                                            }
                                            DeviceHandle::AniMe(_) => {
                                                conn_copy
                                                    .object_server()
                                                    .remove::<AniMeZbus, _>(&path)
                                                    .await?
                                            }
                                            DeviceHandle::Scsi(_) => {
                                                conn_copy
                                                    .object_server()
                                                    .remove::<ScsiZbus, _>(&path)
                                                    .await?
                                            }
                                            // sysfs/USB-backed handles (e.g. OldAura) own no
                                            // shared hidraw fd; nothing to remove here.
                                            _ => false,
                                        };
                                        info!("AuraManager removed: {path:?}, {res}");
                                    }
                                    // Always drop the shared handle for this node, even if no
                                    // AsusDevice referenced it, so the fd (and minor) is freed.
                                    if hid_handles.lock().await.remove(&removed_node).is_some() {
                                        info!("Dropped hid handle for {removed_node}");
                                    }
                                }
                            } else if action == "add" {
                                if let Some(parent) =
                                    event.parent_with_subsystem_devtype("usb", "usb_device")?
                                {
                                    // Guard against initialising a second hidraw interface for a
                                    // USB device we already track. Without this, a USB reset
                                    // (e.g. triggered by an earlier duplicate init) fires
                                    // remove+add events that cause another duplicate init and a
                                    // permanent reset loop.
                                    if let Some(path) = dbus_path_for_dev(&parent) {
                                        if devices.lock().await.iter().any(|d| d.dbus_path == path) {
                                            debug!("Hotplug add: device {path:?} already registered, skipping");
                                            return Ok(());
                                        }
                                    }
                                    let evdev = event.device();
                                    if let Ok(mut new_devs) = Self::init_hid_devices(
                                        &conn_copy,
                                        evdev,
                                        hid_handles.clone(),
                                    )
                                    .await
                                    .map_err(|e| error!("Couldn't add new device: {e:?}"))
                                    {
                                        devices.lock().await.append(&mut new_devs);
                                    }
                                }
                            }
                        }
                        Ok::<(), RogError>(())
                    })
                    .map_err(|e| error!("{e:?}"))
                    .ok();
                }
            }
            // Required for return type on spawn
            #[allow(unreachable_code)]
            Ok::<(), RogError>(())
        });
        Ok(manager)
    }
}
