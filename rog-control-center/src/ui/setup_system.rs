use std::sync::{Arc, Mutex};

use concat_idents::concat_idents;
use log::{debug, error};
use rog_dbus::asus_armoury::AsusArmouryProxy;
use rog_dbus::zbus_backlight::BacklightProxy;
use rog_dbus::zbus_platform::{PlatformProxy, PlatformProxyBlocking};
use rog_platform::asus_armoury::FirmwareAttribute;
use rog_platform::platform::Properties;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use super::show_toast;
use crate::config::Config;
use crate::zbus_proxies::find_iface_async;
use crate::{set_ui_callbacks, AttrMinMax, MainWindow, SystemPageData};

const MINMAX: AttrMinMax = AttrMinMax {
    min: 0,
    max: 0,
    current: -1.0,
};

pub fn setup_system_page(ui: &MainWindow, _config: Arc<Mutex<Config>>) {
    let conn = zbus::blocking::Connection::system()
        .map_err(|e| error!("DBus system connection failed: {e:?}"))
        .unwrap();
    let platform = PlatformProxyBlocking::builder(&conn)
        .build()
        .map_err(|e| error!("PlatformProxy failed: {e:?}"))
        .unwrap();
    // let armoury_attrs =
    // find_iface::<AsusArmouryProxyBlocking>("xyz.ljones.AsusArmoury").unwrap();

    // Null everything before the setup step
    debug!("Defaulting system page values");
    ui.global::<SystemPageData>()
        .set_charge_control_end_threshold(-1.0);
    ui.global::<SystemPageData>()
        .set_charge_control_enabled(false);
    ui.global::<SystemPageData>().set_battery_health(-1);
    ui.global::<SystemPageData>().set_battery_cycle_count(-1);
    ui.global::<SystemPageData>()
        .set_battery_power_consumption(-1.0);
    ui.global::<SystemPageData>()
        .set_battery_status("Unknown".into());
    ui.global::<SystemPageData>()
        .set_battery_time_estimate("".into());
    ui.global::<SystemPageData>().set_platform_profile(-1);
    ui.global::<SystemPageData>().set_panel_overdrive(-1);
    ui.global::<SystemPageData>().set_boot_sound(-1);
    ui.global::<SystemPageData>().set_screen_auto_brightness(-1);
    ui.global::<SystemPageData>().set_mcu_powersave(-1);
    ui.global::<SystemPageData>().set_mini_led_mode(-1);
    ui.global::<SystemPageData>().set_screenpad_brightness(-1);
    ui.global::<SystemPageData>().set_ppt_pl1_spl(MINMAX);
    ui.global::<SystemPageData>().set_ppt_pl2_sppt(MINMAX);
    ui.global::<SystemPageData>().set_ppt_pl3_fppt(MINMAX);
    ui.global::<SystemPageData>().set_ppt_fppt(MINMAX);
    ui.global::<SystemPageData>().set_ppt_apu_sppt(MINMAX);
    ui.global::<SystemPageData>().set_ppt_platform_sppt(MINMAX);
    ui.global::<SystemPageData>().set_nv_tgp(MINMAX);
    ui.global::<SystemPageData>().set_nv_dynamic_boost(MINMAX);
    ui.global::<SystemPageData>().set_nv_temp_target(MINMAX);
    ui.global::<SystemPageData>().set_ppt_enabled(false);
    ui.global::<SystemPageData>()
        .set_ppt_enabled_available(false);

    if let Ok(sys_props) = platform
        .supported_properties()
        .map_err(|e| log::error!("Failed to get supported properties: {}", e))
    {
        log::debug!("Available system properties: {:?}", &sys_props);
        if sys_props.contains(&Properties::ChargeControlEndThreshold) {
            ui.global::<SystemPageData>()
                .set_charge_control_end_threshold(60.0);
            ui.global::<SystemPageData>()
                .set_charge_control_enabled(true);
        }
    }

    let handle = ui.as_weak();
    tokio::spawn(async move {
        loop {
            let power = rog_platform::power::AsusPower::new().ok();
            let (has_bat, health, cycles, consumption, status, estimate_str) =
                if let Some(ref p) = power {
                    if p.has_battery() {
                        let health = p.get_battery_health().unwrap_or(0) as i32;
                        let cycles = p.get_battery_cycle_count().unwrap_or(-1);
                        let consumption = p.get_battery_power_consumption().unwrap_or(-1.0);
                        let status = p
                            .get_battery_status()
                            .unwrap_or_else(|_| "Unknown".to_string());
                        let estimate = p.get_battery_time_estimate().ok().flatten();
                        let est_str = if let Some((_, h, m)) = estimate {
                            if h > 0 {
                                format!("{}h {}m", h, m)
                            } else {
                                format!("{}m", m)
                            }
                        } else {
                            "".to_string()
                        };
                        (true, health, cycles, consumption, status, est_str)
                    } else {
                        (false, -1, -1, -1.0, "Unknown".to_string(), "".to_string())
                    }
                } else {
                    (false, -1, -1, -1.0, "Unknown".to_string(), "".to_string())
                };

            let success = handle.upgrade_in_event_loop(move |ui| {
                let data = ui.global::<SystemPageData>();
                if has_bat {
                    data.set_battery_health(health);
                    data.set_battery_cycle_count(cycles);
                    data.set_battery_power_consumption(consumption);
                    data.set_battery_status(status.into());
                    data.set_battery_time_estimate(estimate_str.into());
                } else {
                    data.set_battery_health(-1);
                }
            });

            if success.is_err() {
                break;
            }

            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
}

macro_rules! convert_value {
    (bool, $value:expr) => {
        $value == 1
    };
    (i32, $value:expr) => {
        $value as i32
    };
    (f32, $value:expr) => {
        $value as f32
    };
}

macro_rules! convert_to_dbus {
    (bool, $value:expr) => {
        if $value {
            1
        } else {
            0
        }
    };
    (i32, $value:expr) => {
        $value as i32
    };
    (f32, $value:expr) => {
        $value as i32
    };
}

macro_rules! init_property {
    ($property:ident, $handle:expr, $value:expr, $type:tt) => {{
        concat_idents!(setter = set_, $property {
            $handle.global::<SystemPageData>().setter(convert_value!($type, $value));
        });
    }};
}

// For initial setup of min/max/val values
macro_rules! init_minmax_property {
    ($property:ident, $handle:expr, $attr:expr) => {
        let proxy_copy = $attr.clone();
        let handle_copy = $handle.as_weak();
        tokio::spawn(async move {
            let min = proxy_copy.min_value().await.unwrap();
            let max = proxy_copy.max_value().await.unwrap();
            let current = proxy_copy.current_value().await.unwrap() as f32;
            handle_copy
                .upgrade_in_event_loop(move |handle| {
                    concat_idents!(setter = set_, $property {
                        handle
                            .global::<SystemPageData>()
                            .setter(AttrMinMax { min, max, current });
                    });
                })
                .ok();
        });
    };
}

// For handling callbacks from UI value changes
macro_rules! setup_callback {
    ($property:ident, $handle:expr, $attr:expr, $type:tt) => {
        let handle_copy = $handle.as_weak();
        let proxy_copy = $attr.clone();
        concat_idents!(on_callback = on_cb_, $property {
            $handle
                .global::<SystemPageData>()
                .on_callback(move |v| {
                    let handle_copy = handle_copy.clone();
                    let proxy_copy = proxy_copy.clone();
                    tokio::spawn(async move {
                        show_toast(
                            format!("{} successfully set to {}", stringify!($property), v).into(),
                            format!("Setting {} failed", stringify!($property)).into(),
                            handle_copy,
                            proxy_copy.set_current_value(convert_to_dbus!($type, v)).await,
                        );
                    });
                });
        });
    };
}

// For handling callbacks from UI value changes
macro_rules! setup_callback_restore_default {
    ($property:ident, $handle:expr, $attr:expr) => {
        let proxy_copy = $attr.clone();
        concat_idents!(on_callback = on_cb_default_, $property {
            $handle
                .global::<SystemPageData>()
                .on_callback(move || {
                    let proxy_copy = proxy_copy.clone();
                    tokio::spawn(async move {
                        proxy_copy.restore_default().await.ok();
                    });
                });
        });
    };
}

macro_rules! setup_external {
    ($property:ident, $type:tt, $handle:expr, $attr:expr, $value:expr) => {{
        // EXTERNAL CHANGES
        let handle_copy = $handle.as_weak();
        let proxy_copy = $attr.clone();
        concat_idents!(setter = set_, $property {
            tokio::spawn(async move {
                let mut x = proxy_copy.receive_current_value_changed().await;
                use futures_util::StreamExt;
                while let Some(e) = x.next().await {
                    if let Ok(out) = e.get().await {
                        handle_copy
                            .upgrade_in_event_loop(move |handle| {
                                handle
                                    .global::<SystemPageData>()
                                    .setter(convert_value!($type, out));
                            })
                            .ok();
                    }
                }
            });
        });
    }};
}

// For handling external value changes
macro_rules! setup_value_watch {
    ($property:ident, $handle:expr, $proxy:expr, $value_type:ident $($conv: tt)*) => {
        let handle_copy = $handle.as_weak();
        let proxy_copy = $proxy.clone();
        tokio::spawn(async move {
            let mut x = concat_idents!(recv = receive_, $value_type, _value_changed {
                proxy_copy.recv().await
            });
            use futures_util::StreamExt;
            while let Some(e) = x.next().await {
                if let Ok(out) = e.get().await {
                    concat_idents!(getter = get_, $property {
                        handle_copy
                            .upgrade_in_event_loop(move |handle| {
                                let mut tmp: AttrMinMax =
                                    handle.global::<SystemPageData>().getter();
                                tmp.$value_type = out $($conv)*;
                                concat_idents!(setter = set_, $property {
                                    handle.global::<SystemPageData>().setter(tmp);
                                });
                            })
                            .ok();
                    });
                }
            }
        });
    };
}

macro_rules! setup_minmax_external {
    ($property:ident, $handle:expr, $attr:expr, $platform:expr) => {
        setup_value_watch!($property, $handle, $attr, current as f32);
        setup_value_watch!($property, $handle, $attr, min);
        setup_value_watch!($property, $handle, $attr, max);

        let handle_copy = $handle.as_weak();
        let proxy_copy = $attr.clone();
        let platform_proxy_copy = $platform.clone();
        tokio::spawn(async move {
            let mut x = platform_proxy_copy.receive_platform_profile_changed().await;
            use futures_util::StreamExt;
            while let Some(e) = x.next().await {
                if let Ok(_) = e.get().await {
                    debug!("receive_platform_profile_changed, getting new {}", stringify!(attr));
                    let min = proxy_copy.min_value().await.unwrap();
                    let max = proxy_copy.max_value().await.unwrap();
                    let current = proxy_copy.current_value().await.unwrap() as f32;
                    handle_copy
                        .upgrade_in_event_loop(move |handle| {
                            concat_idents!(setter = set_, $property {
                                handle
                                    .global::<SystemPageData>()
                                    .setter(AttrMinMax { min, max, current });
                            });
                        })
                        .ok();
                }
            }
        });
    };
}

// This macro expects are consistent naming between proxy calls and slint
// globals
#[macro_export]
macro_rules! set_ui_props_async {
    ($ui:ident, $proxy:ident, $global:ident, $proxy_fn:ident) => {
        if let Ok(value) = $proxy.$proxy_fn().await {
            $ui.upgrade_in_event_loop(move |handle| {
                concat_idents::concat_idents!(set = set_, $proxy_fn {
                    handle.global::<$global>().set(value.into());
                });
            }).ok();
        }
    };
}

pub fn setup_system_page_callbacks(ui: &MainWindow, _states: Arc<Mutex<Config>>) {
    // This tokio spawn exists only to prevent blocking the UI, and to enable use of
    // async zbus interfaces
    let handle = ui.as_weak();

    tokio::spawn(async move {
        // Create the connections/proxies here to prevent future delays in process
        let conn = zbus::Connection::system()
            .await
            .map_err(|e| {
                log::error!("Failed to connect to system bus: {}", e);
            })
            .unwrap();
        let platform = PlatformProxy::builder(&conn)
            .build()
            .await
            .map_err(|e| {
                log::error!("Failed to create platform proxy: {}", e);
            })
            .unwrap();
        let backlight = BacklightProxy::builder(&conn)
            .build()
            .await
            .map_err(|e| {
                log::error!("Failed to create backlight proxy: {}", e);
            })
            .unwrap();

        debug!("Setting up system page profile callbacks");
        set_ui_props_async!(
            handle,
            platform,
            SystemPageData,
            charge_control_end_threshold
        );

        let platform_copy = platform.clone();
        if let Ok(mut value) = platform.platform_profile_choices().await {
            debug!("Available platform profile choices: {:?}", value);
            handle
                .upgrade_in_event_loop(move |handle| {
                    value.sort();
                    let translate: Vec<SharedString> = handle
                        .global::<SystemPageData>()
                        .get_platform_profile_choices()
                        .iter()
                        .collect();
                    let mut indexes = Vec::new();
                    let strings: Vec<SharedString> = value
                        .iter()
                        .filter_map(|p| {
                            let index = i32::from(*p) as usize;
                            if index < translate.len() {
                                indexes.push(index as i32);
                                Some(translate[index].clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    let choices = ModelRc::new(VecModel::from(strings));
                    handle
                        .global::<SystemPageData>()
                        .set_platform_profile_choices(choices);
                    handle
                        .global::<SystemPageData>()
                        .set_platform_profile_indexes(ModelRc::from(indexes.as_slice()));

                    // Set current only after setting the choices up
                    let handle = handle.as_weak();
                    tokio::spawn(async move {
                        if let Ok(value) = platform_copy.platform_profile().await {
                            let profile_value = <i32>::from(value);
                            handle
                                .upgrade_in_event_loop(move |handle| {
                                    if let Some(position) =
                                        indexes.iter().position(|&index| index == profile_value)
                                    {
                                        handle
                                            .global::<SystemPageData>()
                                            .set_platform_profile(position as i32);
                                    }
                                })
                                .ok();
                        }
                    });
                })
                .ok();
        }

        set_ui_props_async!(
            handle,
            platform,
            SystemPageData,
            platform_profile_linked_epp
        );
        set_ui_props_async!(handle, platform, SystemPageData, profile_balanced_epp);
        set_ui_props_async!(handle, platform, SystemPageData, profile_performance_epp);
        set_ui_props_async!(handle, platform, SystemPageData, profile_quiet_epp);
        set_ui_props_async!(
            handle,
            platform,
            SystemPageData,
            platform_profile_on_battery
        );
        set_ui_props_async!(
            handle,
            platform,
            SystemPageData,
            change_platform_profile_on_battery
        );
        set_ui_props_async!(handle, platform, SystemPageData, platform_profile_on_ac);
        set_ui_props_async!(
            handle,
            platform,
            SystemPageData,
            change_platform_profile_on_ac
        );

        set_ui_props_async!(handle, platform, SystemPageData, enable_ppt_group);

        set_ui_props_async!(handle, backlight, SystemPageData, screenpad_brightness);
        if let Ok(value) = backlight.screenpad_gamma().await {
            handle
                .upgrade_in_event_loop(move |handle| {
                    handle
                        .global::<SystemPageData>()
                        .set_screenpad_gamma(value.parse().unwrap_or(1.0));
                })
                .ok();
        }

        set_ui_props_async!(
            handle,
            backlight,
            SystemPageData,
            screenpad_sync_with_primary
        );

        let platform_copy = platform.clone();
        handle
            .upgrade_in_event_loop(move |handle| {
                debug!("Setting up system page standard callbacks");

                let handle_copy = handle.as_weak();
                let proxy_copy = platform_copy.clone();
                handle
                    .global::<SystemPageData>()
                    .on_cb_platform_profile(move |value| {
                        let proxy_copy = proxy_copy.clone();
                        let handle_copy = handle_copy.clone();
                        tokio::spawn(async move {
                            show_toast(
                                format!("Throttle policy set to {}", value).into(),
                                "Setting Throttle policy failed".into(),
                                handle_copy,
                                proxy_copy.set_platform_profile(value.into()).await,
                            );
                        });
                    });

                let handle_copy = handle.as_weak();
                let proxy_copy = platform_copy.clone();
                // spawn required since the while let never exits
                tokio::spawn(async move {
                    let mut x = proxy_copy.receive_platform_profile_changed().await;
                    use futures_util::StreamExt;
                    while let Some(e) = x.next().await {
                        if let Ok(out) = e.get().await {
                            handle_copy
                                .upgrade_in_event_loop(move |handle| {
                                    let indexes = handle
                                        .global::<SystemPageData>()
                                        .get_platform_profile_indexes();
                                    handle
                                        .global::<SystemPageData>()
                                        .set_platform_profile(out as i32);
                                    let profile_value = <i32>::from(out);
                                    if let Some(position) =
                                        indexes.iter().position(|index| index == profile_value)
                                    {
                                        handle
                                            .global::<SystemPageData>()
                                            .set_platform_profile(position as i32);
                                    }
                                })
                                .ok();
                        }
                    }
                });

                set_ui_callbacks!(handle,
                    SystemPageData(as bool),
                    platform_copy.enable_ppt_group(as bool),
                    "Applied PPT group settings {}",
                    "Setting PPT group settings failed"
                );

                set_ui_callbacks!(handle,
                    SystemPageData(as f32),
                    platform_copy.charge_control_end_threshold(as u8),
                    "Charge limit successfully set to {}",
                    "Setting Charge limit failed"
                );
                // set_ui_callbacks!(handle,
                //     SystemPageData(as i32),
                //     platform_copy.platform_profile(.into()),
                //     "Throttle policy set to {}",
                //     "Setting Throttle policy failed"
                // );
                set_ui_callbacks!(handle,
                    SystemPageData(as i32),
                    platform_copy.profile_balanced_epp(.into()),
                    "Throttle policy EPP set to {}",
                    "Setting Throttle policy EPP failed"
                );
                set_ui_callbacks!(handle,
                    SystemPageData(as i32),
                    platform_copy.profile_performance_epp(.into()),
                    "Throttle policy EPP set to {}",
                    "Setting Throttle policy EPP failed"
                );
                set_ui_callbacks!(handle,
                    SystemPageData(as i32),
                    platform_copy.profile_quiet_epp(.into()),
                    "Throttle policy EPP set to {}",
                    "Setting Throttle policy EPP failed"
                );
                set_ui_callbacks!(
                    handle,
                    SystemPageData(),
                    platform_copy.platform_profile_linked_epp(),
                    "Throttle policy linked to EPP: {}",
                    "Setting Throttle policy linked to EPP failed"
                );
                set_ui_callbacks!(handle,
                    SystemPageData(as i32),
                    platform_copy.platform_profile_on_ac(.into()),
                    "Throttle policy on AC set to {}",
                    "Setting Throttle policy on AC failed"
                );
                set_ui_callbacks!(handle,
                    SystemPageData(as bool),
                    platform_copy.change_platform_profile_on_ac(.into()),
                    "Throttle policy on AC enabled: {}",
                    "Setting Throttle policy on AC failed"
                );
                set_ui_callbacks!(handle,
                    SystemPageData(as i32),
                    platform_copy.platform_profile_on_battery(.into()),
                    "Throttle policy on battery set to {}",
                    "Setting Throttle policy on battery failed"
                );
                set_ui_callbacks!(handle,
                    SystemPageData(as bool),
                    platform_copy.change_platform_profile_on_battery(.into()),
                    "Throttle policy on battery enabled: {}",
                    "Setting Throttle policy on AC failed"
                );

                set_ui_callbacks!(handle,
                    SystemPageData(as i32),
                    backlight.screenpad_brightness(as i32),
                    "Screenpad successfully set to {}",
                    "Setting screenpad brightness failed"
                );

                set_ui_callbacks!(handle,
                    SystemPageData(as bool),
                    backlight.screenpad_sync_with_primary(as bool),
                    "Screenpad successfully set to {}",
                    "Setting screenpad brightness failed"
                );

                set_ui_callbacks!(handle,
                    SystemPageData(.parse().unwrap_or(1.0)),
                    backlight.screenpad_gamma(.to_string().as_str()),
                    "Screenpad successfully set to {}",
                    "Setting screenpad brightness failed"
                );
            })
            .ok();

        let armoury_attrs;
        if let Ok(attrs) = find_iface_async::<AsusArmouryProxy>("xyz.ljones.AsusArmoury").await {
            debug!("Found AsusArmoury interfaces");
            armoury_attrs = attrs;
            handle
                .upgrade_in_event_loop(|ui| {
                    ui.global::<SystemPageData>().set_asus_armoury_loaded(true)
                })
                .ok();
        } else {
            error!(
                "The kernel module asus-armoury is required, if you do not have this you will \
                 need to either build or install a kernel which includes the patchwork. This \
                 driver is in process of being upstreamed"
            );
            return;
        }

        for attr in armoury_attrs {
            if let Ok(value) = attr.current_value().await {
                if let Ok(name) = attr.name().await {
                    debug!("Setting up {} = {value}", <&str>::from(name));
                    let platform = platform.clone();
                    handle
                        .upgrade_in_event_loop(move |handle| match name {
                            FirmwareAttribute::ApuMem => {}
                            FirmwareAttribute::CoresPerformance => {}
                            FirmwareAttribute::CoresEfficiency => {}
                            FirmwareAttribute::PptEnabled => {
                                init_property!(ppt_enabled, handle, value, bool);
                                setup_callback!(ppt_enabled, handle, attr, bool);
                                let handle_copy = handle.as_weak();
                                let proxy_copy = attr.clone();
                                tokio::spawn(async move {
                                    let mut x = proxy_copy.receive_current_value_changed().await;
                                    use futures_util::StreamExt;
                                    while let Some(e) = x.next().await {
                                        if let Ok(out) = e.get().await {
                                            handle_copy
                                                .upgrade_in_event_loop(move |handle| {
                                                    handle
                                                        .global::<SystemPageData>()
                                                        .set_enable_ppt_group(out == 1);
                                                    handle
                                                        .global::<SystemPageData>()
                                                        .set_ppt_enabled(out == 1);
                                                })
                                                .ok();
                                        }
                                    }
                                });
                                handle
                                    .global::<SystemPageData>()
                                    .set_ppt_enabled_available(true);
                                handle
                                    .global::<SystemPageData>()
                                    .set_enable_ppt_group(value == 1);
                            }
                            FirmwareAttribute::PptPl1Spl => {
                                init_minmax_property!(ppt_pl1_spl, handle, attr);
                                setup_callback!(ppt_pl1_spl, handle, attr, i32);
                                setup_callback_restore_default!(ppt_pl1_spl, handle, attr);
                                setup_minmax_external!(ppt_pl1_spl, handle, attr, platform);
                            }
                            FirmwareAttribute::PptPl2Sppt => {
                                init_minmax_property!(ppt_pl2_sppt, handle, attr);
                                setup_callback!(ppt_pl2_sppt, handle, attr, i32);
                                setup_callback_restore_default!(ppt_pl2_sppt, handle, attr);
                                setup_minmax_external!(ppt_pl2_sppt, handle, attr, platform);
                            }
                            FirmwareAttribute::PptPl3Fppt => {
                                init_minmax_property!(ppt_pl3_fppt, handle, attr);
                                setup_callback!(ppt_pl3_fppt, handle, attr, i32);
                                setup_callback_restore_default!(ppt_pl3_fppt, handle, attr);
                                setup_minmax_external!(ppt_pl3_fppt, handle, attr, platform);
                            }
                            FirmwareAttribute::PptFppt => {
                                init_minmax_property!(ppt_fppt, handle, attr);
                                setup_callback!(ppt_fppt, handle, attr, i32);
                                setup_callback_restore_default!(ppt_fppt, handle, attr);
                                setup_minmax_external!(ppt_fppt, handle, attr, platform);
                            }
                            FirmwareAttribute::PptApuSppt => {
                                init_minmax_property!(ppt_apu_sppt, handle, attr);
                                setup_callback!(ppt_apu_sppt, handle, attr, i32);
                                setup_callback_restore_default!(ppt_apu_sppt, handle, attr);
                                setup_minmax_external!(ppt_apu_sppt, handle, attr, platform);
                            }
                            FirmwareAttribute::PptPlatformSppt => {
                                init_minmax_property!(ppt_platform_sppt, handle, attr);
                                setup_callback!(ppt_platform_sppt, handle, attr, i32);
                                setup_callback_restore_default!(ppt_platform_sppt, handle, attr);
                                setup_minmax_external!(ppt_platform_sppt, handle, attr, platform);
                            }
                            FirmwareAttribute::NvDynamicBoost => {
                                init_minmax_property!(nv_dynamic_boost, handle, attr);
                                setup_callback!(nv_dynamic_boost, handle, attr, i32);
                                setup_callback_restore_default!(nv_dynamic_boost, handle, attr);
                                setup_minmax_external!(nv_dynamic_boost, handle, attr, platform);
                            }
                            FirmwareAttribute::NvTempTarget => {
                                init_minmax_property!(nv_temp_target, handle, attr);
                                setup_callback!(nv_temp_target, handle, attr, i32);
                                setup_callback_restore_default!(nv_temp_target, handle, attr);
                                setup_minmax_external!(nv_temp_target, handle, attr, platform);
                            }
                            FirmwareAttribute::DgpuBaseTgp => {}
                            FirmwareAttribute::DgpuTgp => {
                                init_minmax_property!(nv_tgp, handle, attr);
                                setup_callback!(nv_tgp, handle, attr, i32);
                                setup_callback_restore_default!(nv_tgp, handle, attr);
                                setup_minmax_external!(nv_tgp, handle, attr, platform);
                            }
                            FirmwareAttribute::ChargeMode => {}
                            FirmwareAttribute::BootSound => {
                                init_property!(boot_sound, handle, value, i32);
                                setup_callback!(boot_sound, handle, attr, i32);
                                setup_external!(boot_sound, i32, handle, attr, value)
                            }
                            FirmwareAttribute::ScreenAutoBrightness => {
                                init_property!(screen_auto_brightness, handle, value, i32);
                                setup_callback!(screen_auto_brightness, handle, attr, i32);
                                setup_external!(screen_auto_brightness, i32, handle, attr, value)
                            }
                            FirmwareAttribute::McuPowersave => {
                                init_property!(mcu_powersave, handle, value, i32);
                                setup_callback!(mcu_powersave, handle, attr, i32);
                                setup_external!(mcu_powersave, i32, handle, attr, value)
                            }
                            FirmwareAttribute::PanelOverdrive => {
                                init_property!(panel_overdrive, handle, value, i32);
                                setup_callback!(panel_overdrive, handle, attr, i32);
                                setup_external!(panel_overdrive, i32, handle, attr, value)
                            }
                            FirmwareAttribute::PanelHdMode => {}
                            FirmwareAttribute::EgpuConnected => {}
                            FirmwareAttribute::EgpuEnable => {}
                            FirmwareAttribute::DgpuDisable => {}
                            FirmwareAttribute::GpuMuxMode => {}
                            FirmwareAttribute::MiniLedMode => {
                                init_property!(mini_led_mode, handle, value, i32);
                                setup_callback!(mini_led_mode, handle, attr, i32);
                                setup_external!(mini_led_mode, i32, handle, attr, value);

                                // possible_values count tells us how many dimming
                                // modes the device has: 2 (MODE1) or 3 (MODE2).
                                let handle_copy = handle.as_weak();
                                let proxy_copy = attr.clone();
                                tokio::spawn(async move {
                                    let count = proxy_copy
                                        .possible_values()
                                        .await
                                        .map(|v| v.len())
                                        .unwrap_or(2);
                                    handle_copy
                                        .upgrade_in_event_loop(move |handle| {
                                            let data = handle.global::<SystemPageData>();
                                            let choices = if count >= 3 {
                                                data.get_mini_led_choices_modes()
                                            } else {
                                                data.get_mini_led_choices_onoff()
                                            };
                                            data.set_mini_led_mode_choices(choices);
                                        })
                                        .ok();
                                });
                            }
                            FirmwareAttribute::PendingReboot => {}
                            FirmwareAttribute::None => {}
                        })
                        .ok();
                } else {
                    error!("Attribute with no name, skipping");
                }
            }
        }
        handle
            .upgrade_in_event_loop(|ui| {
                debug!(
                    "get_asus_armoury_loaded: {:?}",
                    ui.global::<SystemPageData>().get_asus_armoury_loaded()
                );
                debug!(
                    "get_ppt_enabled_available: {:?}",
                    ui.global::<SystemPageData>().get_ppt_enabled_available()
                );
            })
            .ok();
    });
}
