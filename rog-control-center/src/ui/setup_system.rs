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

pub fn setup_system_page(ui: &MainWindow, config: Arc<Mutex<Config>>) {
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

    let backend = detect_display_backend();
    let handle_copy = ui.as_weak();
    ui.global::<SystemPageData>().on_cb_refresh_rate_changed(move |idx| {
        if let Some((name, _, modes)) = get_display_info(backend) {
            if let Some(mode) = modes.get(idx as usize) {
                set_display_mode(backend, &name, &mode.0);
                if let Some(handle) = handle_copy.upgrade() {
                    handle.global::<SystemPageData>().set_refresh_rate_active_idx(idx);
                }
            }
        }
    });

    let handle = ui.as_weak();
    let config_clone = config.clone();
    tokio::spawn(async move {
        let mut prev_ticks = read_cpu_ticks();
        let mut prev_online = None;
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

            let cpu_temp = get_cpu_temp();
            let gpu_temp = get_gpu_temp();
            let (cpu_fan, gpu_fan, mid_fan) = get_fan_rpms();
            let cpu_freq = get_cpu_frequency_mhz();
            let ram_usage = get_ram_usage_pct();
            let gpu_usage = get_gpu_usage_pct();

            let curr_ticks = read_cpu_ticks();
            let cpu_usage = if let (Some(p), Some(c)) = (&prev_ticks, &curr_ticks) {
                let idle_diff = c.idle.saturating_sub(p.idle) as f32;
                let total_diff = c.total.saturating_sub(p.total) as f32;
                if total_diff > 0.0 {
                    ((1.0 - (idle_diff / total_diff)) * 100.0).clamp(0.0, 100.0)
                } else {
                    0.0
                }
            } else {
                0.0
            };
            prev_ticks = curr_ticks;

            // Refresh rate auto-switching logic
            let display_info = get_display_info(backend);
            let mut refresh_rate_choices_strings = Vec::new();
            let mut active_idx = -1;
            let mut output_name = String::new();
            let mut current_mode_id = String::new();
            let mut available_modes_store = Vec::new();
            if let Some((name, curr_id, modes)) = display_info {
                output_name = name;
                current_mode_id = curr_id;
                available_modes_store = modes.clone();
                for (i, m) in modes.iter().enumerate() {
                    refresh_rate_choices_strings.push(format!("{:.2} Hz", m.2));
                    if m.0 == current_mode_id {
                        active_idx = i as i32;
                    }
                }
            }

            let curr_online = power.as_ref().map(|p| p.get_online().unwrap_or(1) == 1).unwrap_or(true);
            if let Some(prev) = prev_online {
                if prev != curr_online {
                    let auto_switch_enabled = if let Ok(lock) = config_clone.try_lock() {
                        lock.auto_refresh_rate
                    } else {
                        false
                    };
                    if auto_switch_enabled && !available_modes_store.is_empty() {
                        let target_mode = if curr_online {
                            available_modes_store.last()
                        } else {
                            available_modes_store.first()
                        };
                        if let Some(mode) = target_mode {
                            if mode.0 != current_mode_id {
                                set_display_mode(backend, &output_name, &mode.0);
                                current_mode_id = mode.0.clone();
                                if let Some(pos) = available_modes_store.iter().position(|x| x.0 == mode.0) {
                                    active_idx = pos as i32;
                                }
                                let msg = if curr_online {
                                    format!("AC connected. Refresh rate set to {:.2} Hz.", mode.2)
                                } else {
                                    format!("AC disconnected. Refresh rate set to {:.2} Hz for power saving.", mode.2)
                                };
                                tokio::spawn(async move {
                                    let _ = notify_rust::Notification::new()
                                        .summary("Display Refresh Rate")
                                        .body(&msg)
                                        .icon("rog-control-center")
                                        .show();
                                });
                            }
                        }
                    }
                }
            }
            prev_online = Some(curr_online);

            let choices_strings = refresh_rate_choices_strings.clone();
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
                data.set_cpu_temp_val(cpu_temp);
                data.set_gpu_temp_val(gpu_temp);
                data.set_cpu_usage_val(cpu_usage);
                data.set_gpu_usage_val(gpu_usage);
                data.set_ram_usage_val(ram_usage);
                data.set_cpu_freq_mhz(cpu_freq);
                data.set_cpu_fan_rpm(cpu_fan);
                data.set_gpu_fan_rpm(gpu_fan);
                data.set_mid_fan_rpm(mid_fan);

                // Refresh rate properties
                if active_idx != -1 {
                    if data.get_refresh_rate_active_idx() != active_idx {
                        data.set_refresh_rate_active_idx(active_idx);
                    }
                    let ui_choices = data.get_refresh_rate_choices();
                    let mut choices_changed = ui_choices.row_count() != choices_strings.len();
                    if !choices_changed {
                        for i in 0..ui_choices.row_count() {
                            if ui_choices.row_data(i) != Some(choices_strings[i].clone().into()) {
                                choices_changed = true;
                                break;
                            }
                        }
                    }
                    if choices_changed {
                        let model = slint::VecModel::default();
                        for choice in &choices_strings {
                            model.push(choice.clone().into());
                        }
                        data.set_refresh_rate_choices(slint::ModelRc::new(model));
                    }
                } else {
                    data.set_refresh_rate_active_idx(-1);
                }
            });

            if success.is_err() {
                break;
            }

            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
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

fn get_cpu_temp() -> f32 {
    if let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(name) = std::fs::read_to_string(path.join("name")) {
                let name = name.trim();
                if name == "k10temp" || name == "coretemp" || name == "zenpower" {
                    if let Ok(temp_str) = std::fs::read_to_string(path.join("temp1_input")) {
                        if let Ok(temp_val) = temp_str.trim().parse::<f32>() {
                            return temp_val / 1000.0;
                        }
                    }
                }
            }
        }
    }
    if let Ok(temp_str) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
        if let Ok(temp_val) = temp_str.trim().parse::<f32>() {
            return temp_val / 1000.0;
        }
    }
    0.0
}

fn get_gpu_temp() -> f32 {
    if let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(name) = std::fs::read_to_string(path.join("name")) {
                let name = name.trim();
                if name == "amdgpu" || name == "nouveau" || name == "nvidia" {
                    if let Ok(temp_str) = std::fs::read_to_string(path.join("temp1_input")) {
                        if let Ok(temp_val) = temp_str.trim().parse::<f32>() {
                            return temp_val / 1000.0;
                        }
                    }
                }
            }
        }
    }
    0.0
}

fn get_fan_rpms() -> (i32, i32, i32) {
    let mut cpu = 0;
    let mut gpu = 0;
    let mut mid = 0;
    if let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(name) = std::fs::read_to_string(path.join("name")) {
                if name.trim() == "asus" {
                    if let Ok(v) = std::fs::read_to_string(path.join("fan1_input")) {
                        cpu = v.trim().parse().unwrap_or(0);
                    }
                    if let Ok(v) = std::fs::read_to_string(path.join("fan2_input")) {
                        gpu = v.trim().parse().unwrap_or(0);
                    }
                    if let Ok(v) = std::fs::read_to_string(path.join("fan3_input")) {
                        mid = v.trim().parse().unwrap_or(0);
                    }
                    break;
                }
            }
        }
    }
    (cpu, gpu, mid)
}

fn get_cpu_frequency_mhz() -> f32 {
    let mut total_freq = 0.0;
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("cpu") && name[3..].chars().all(|c| c.is_ascii_digit()) {
                let freq_path = entry.path().join("cpufreq/scaling_cur_freq");
                if let Ok(freq_str) = std::fs::read_to_string(freq_path) {
                    if let Ok(freq_khz) = freq_str.trim().parse::<f32>() {
                        total_freq += freq_khz / 1000.0;
                        count += 1;
                    }
                }
            }
        }
    }
    if count == 0 {
        if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
            for line in cpuinfo.lines() {
                if line.starts_with("cpu MHz") {
                    if let Some(pos) = line.find(':') {
                        if let Ok(val) = line[pos + 1..].trim().parse::<f32>() {
                            total_freq += val;
                            count += 1;
                        }
                    }
                }
            }
        }
    }
    if count > 0 {
        total_freq / count as f32
    } else {
        0.0
    }
}

fn get_ram_usage_pct() -> f32 {
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        let mut total = 0.0;
        let mut available = 0.0;
        for line in meminfo.lines() {
            if line.starts_with("MemTotal:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(val) = parts.get(1) {
                    total = val.parse::<f32>().unwrap_or(0.0);
                }
            } else if line.starts_with("MemAvailable:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(val) = parts.get(1) {
                    available = val.parse::<f32>().unwrap_or(0.0);
                }
            }
        }
        if total > 0.0 {
            return ((total - available) / total) * 100.0;
        }
    }
    0.0
}

struct CpuTicks {
    idle: u64,
    total: u64,
}

fn read_cpu_ticks() -> Option<CpuTicks> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let first_line = stat.lines().next()?;
    if first_line.starts_with("cpu ") {
        let parts: Vec<&str> = first_line.split_whitespace().collect();
        let mut total = 0u64;
        let mut idle = 0u64;
        for (i, part) in parts.iter().skip(1).enumerate() {
            if let Ok(ticks) = part.parse::<u64>() {
                total += ticks;
                if i == 3 || i == 4 {
                    idle += ticks;
                }
            }
        }
        return Some(CpuTicks { idle, total });
    }
    None
}

fn get_gpu_usage_pct() -> f32 {
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let path = entry.path().join("device/gpu_busy_percent");
            if path.exists() {
                if let Ok(val_str) = std::fs::read_to_string(path) {
                    if let Ok(val) = val_str.trim().parse::<f32>() {
                        return val;
                    }
                }
            }
        }
    }
    0.0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaylandDisplayBackend {
    KScreenDoctor,
    WlrRandr,
    None,
}

fn detect_display_backend() -> WaylandDisplayBackend {
    if std::process::Command::new("kscreen-doctor").arg("--help").output().is_ok() {
        WaylandDisplayBackend::KScreenDoctor
    } else if std::process::Command::new("wlr-randr").arg("--help").output().is_ok() {
        WaylandDisplayBackend::WlrRandr
    } else {
        WaylandDisplayBackend::None
    }
}

fn get_display_info(backend: WaylandDisplayBackend) -> Option<(String, String, Vec<(String, String, f64)>)> {
    match backend {
        WaylandDisplayBackend::KScreenDoctor => get_kscreen_info(),
        WaylandDisplayBackend::WlrRandr => get_wlr_info(),
        WaylandDisplayBackend::None => None,
    }
}

fn set_display_mode(backend: WaylandDisplayBackend, output_name: &str, mode_id: &str) {
    match backend {
        WaylandDisplayBackend::KScreenDoctor => set_kscreen_mode(output_name, mode_id),
        WaylandDisplayBackend::WlrRandr => set_wlr_mode(output_name, mode_id),
        WaylandDisplayBackend::None => {}
    }
}

#[derive(serde::Deserialize, Debug)]
struct KScreenOutput {
    name: String,
    #[serde(rename = "currentModeId")]
    current_mode_id: String,
    modes: Vec<KScreenMode>,
    #[serde(rename = "type")]
    output_type: i32,
}

#[derive(serde::Deserialize, Debug)]
struct KScreenMode {
    id: String,
    name: String,
    #[serde(rename = "refreshRate")]
    refresh_rate: f64,
}

#[derive(serde::Deserialize, Debug)]
struct KScreenConfig {
    outputs: Vec<KScreenOutput>,
}

fn get_kscreen_info() -> Option<(String, String, Vec<(String, String, f64)>)> {
    let output = std::process::Command::new("kscreen-doctor")
        .arg("-j")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let config: KScreenConfig = serde_json::from_slice(&output.stdout).ok()?;
    let panel = config.outputs.iter().find(|o| o.output_type == 7 || o.name.starts_with("eDP"))?;
    let current_mode = panel.modes.iter().find(|m| m.id == panel.current_mode_id)?;
    let current_res = current_mode.name.split('@').next()?.trim();
    
    let mut available_rates = Vec::new();
    for m in &panel.modes {
        if let Some(res) = m.name.split('@').next() {
            if res.trim() == current_res {
                available_rates.push((m.id.clone(), m.name.clone(), m.refresh_rate));
            }
        }
    }
    available_rates.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
    
    Some((panel.name.clone(), current_mode.id.clone(), available_rates))
}

fn set_kscreen_mode(output_name: &str, mode_id: &str) {
    let arg = format!("output.{}.mode.{}", output_name, mode_id);
    let _ = std::process::Command::new("kscreen-doctor")
        .arg(arg)
        .output();
}

fn get_wlr_info() -> Option<(String, String, Vec<(String, String, f64)>)> {
    let output = std::process::Command::new("wlr-randr")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    
    let mut current_output = String::new();
    let mut is_edp = false;
    let mut current_mode_str = String::new();
    let mut modes = Vec::new();
    
    for line in stdout.lines() {
        let trimmed = line.trim();
        if line.starts_with(' ') || line.starts_with('\t') {
            if is_edp {
                if trimmed.starts_with("Modes:") {
                    continue;
                }
                if trimmed.contains("px,") && trimmed.contains("Hz") {
                    let parts: Vec<&str> = trimmed.split(',').collect();
                    if parts.len() >= 2 {
                        let res = parts[0].replace("px", "").trim().to_string();
                        let hz_part = parts[1].trim();
                        
                        let is_current = hz_part.contains("(current)");
                        let clean_hz_part = hz_part.replace("(current)", "").replace("(preferred)", "").replace("Hz", "");
                        let hz_str = clean_hz_part.trim();
                        if let Ok(hz) = hz_str.parse::<f64>() {
                            let mode_id = format!("{}@{}", res, hz_str);
                            let mode_name = format!("{} px, {} Hz", res, hz_str);
                            if is_current {
                                current_mode_str = mode_id.clone();
                            }
                            modes.push((mode_id, mode_name, hz));
                        }
                    }
                }
            }
        } else {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if let Some(name) = parts.first() {
                if name.starts_with("eDP") {
                    current_output = name.to_string();
                    is_edp = true;
                } else {
                    is_edp = false;
                }
            }
        }
    }
    
    if !current_output.is_empty() && !modes.is_empty() {
        modes.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        if current_mode_str.is_empty() {
            current_mode_str = modes.first()?.0.clone();
        }
        Some((current_output, current_mode_str, modes))
    } else {
        None
    }
}

fn set_wlr_mode(output_name: &str, mode_id: &str) {
    let _ = std::process::Command::new("wlr-randr")
        .arg("--output")
        .arg(output_name)
        .arg("--mode")
        .arg(mode_id)
        .output();
}
