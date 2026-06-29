pub mod setup_anime;
pub mod setup_aura;
pub mod setup_fans;
pub mod setup_gpu;
pub mod setup_slash;
pub mod setup_system;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static TOAST_SEQ: AtomicU64 = AtomicU64::new(0);

use config_traits::StdConfig;
use log::warn;
use rog_dbus::list_iface_blocking;
use slint::{ComponentHandle, SharedString, Weak};

use crate::config::Config;
use crate::ui::setup_anime::setup_anime_page;
use crate::ui::setup_aura::setup_aura_page;
use crate::ui::setup_fans::setup_fan_curve_page;
use crate::ui::setup_slash::setup_slash_page;
use crate::ui::setup_system::{setup_system_page, setup_system_page_callbacks};
use crate::{AppSettingsPageData, MainWindow};

// this macro sets up:
// - a link from UI callback -> dbus proxy property
// - a link from dbus property signal -> UI state
// conv1 and conv2 are type conversion args
#[macro_export]
macro_rules! set_ui_callbacks {
    ($handle:ident, $data:ident($($conv1: tt)*),$proxy:ident.$proxy_fn:tt($($conv2: tt)*),$success:literal,$failed:literal) => {
        let handle_copy = $handle.as_weak();
        let proxy_copy = $proxy.clone();
        let data = $handle.global::<$data>();
        concat_idents::concat_idents!(on_set = on_cb_, $proxy_fn {
        data.on_set(move |value| {
            let proxy_copy = proxy_copy.clone();
            let handle_copy = handle_copy.clone();
            tokio::spawn(async move {
                concat_idents::concat_idents!(set = set_, $proxy_fn {
                show_toast(
                    format!($success, value).into(),
                    $failed.into(),
                    handle_copy,
                    proxy_copy.set(value $($conv2)*).await,
                );
                });
            });
            });
        });
        let handle_copy = $handle.as_weak();
        let proxy_copy = $proxy.clone();
        concat_idents::concat_idents!(receive = receive_, $proxy_fn, _changed {
        // spawn required since the while let never exits
        tokio::spawn(async move {
            let mut x = proxy_copy.receive().await;
            concat_idents::concat_idents!(set = set_, $proxy_fn {
            use futures_util::StreamExt;
            while let Some(e) = x.next().await {
                if let Ok(out) = e.get().await {
                    handle_copy.upgrade_in_event_loop(move |handle| {
                        handle.global::<$data>().set(out $($conv1)*);
                    }).ok();
                }
            }
            });
        });
        });
    };
}

pub fn show_toast(
    success: SharedString,
    fail: SharedString,
    handle: Weak<MainWindow>,
    result: zbus::Result<()>,
) {
    // bump sequence so that any previously spawned timers won't clear newer toasts
    let seq = TOAST_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    match result {
        Ok(_) => {
            let delayed_handle = handle.clone();
            let delayed_text = success.clone();
            slint::invoke_from_event_loop(move || handle.unwrap().invoke_show_toast(success)).ok();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if TOAST_SEQ.load(Ordering::SeqCst) == seq {
                    slint::invoke_from_event_loop(move || {
                        delayed_handle
                            .unwrap()
                            .invoke_clear_toast_if_matches(delayed_text)
                    })
                    .ok();
                }
            });
        }
        Err(e) => {
            let delayed_handle = handle.clone();
            let delayed_text = fail.clone();
            slint::invoke_from_event_loop(move || {
                log::warn!("{fail}: {e}");
                handle.unwrap().invoke_show_toast(fail)
            })
            .ok();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if TOAST_SEQ.load(Ordering::SeqCst) == seq {
                    slint::invoke_from_event_loop(move || {
                        delayed_handle
                            .unwrap()
                            .invoke_clear_toast_if_matches(delayed_text)
                    })
                    .ok();
                }
            });
        }
    };
}

pub fn setup_window(
    config: Arc<Mutex<Config>>,
    prefetched_supported: std::sync::Arc<Option<Vec<i32>>>,
    is_tuf: bool,
) -> MainWindow {
    slint::set_xdg_app_id("rog-control-center")
        .map_err(|e| warn!("Couldn't set application ID: {e:?}"))
        .ok();
    let ui = MainWindow::new()
        .map_err(|e| warn!("Couldn't create main window: {e:?}"))
        .unwrap();
    // propagate TUF flag to the UI so the sidebar can swap logo branding
    ui.set_is_tuf(is_tuf);
    ui.window()
        .show()
        .map_err(|e| warn!("Couldn't show main window: {e:?}"))
        .unwrap();

    let available = list_iface_blocking().unwrap_or_default();
    ui.set_sidebar_items_avilable(
        [
            // Needs to match the order of slint sidebar items
            available.contains(&"xyz.ljones.Platform".to_string()),
            available.contains(&"xyz.ljones.Aura".to_string()),
            available.contains(&"xyz.ljones.Anime".to_string()),
            available.contains(&"xyz.ljones.Slash".to_string()),
            available.contains(&"xyz.ljones.FanCurves".to_string()),
            true, // GPU Configuration
            available.contains(&"xyz.ljones.Platform".to_string()), // Battery Info
            true, // App Settings
            true, // About
        ]
        .into(),
    );

    ui.on_exit_app(move || {
        slint::quit_event_loop().unwrap();
    });

    setup_app_settings_page(&ui, config.clone());
    if available.contains(&"xyz.ljones.Platform".to_string()) {
        setup_system_page(&ui, config.clone());
        setup_system_page_callbacks(&ui, config.clone());
    }
    if available.contains(&"xyz.ljones.Aura".to_string()) {
        setup_aura_page(&ui, config.clone(), prefetched_supported.as_ref().clone());
    }
    if available.contains(&"xyz.ljones.Anime".to_string()) {
        setup_anime_page(&ui, config.clone());
    }
    if available.contains(&"xyz.ljones.Slash".to_string()) {
        setup_slash_page(&ui, config.clone());
    }
    if available.contains(&"xyz.ljones.FanCurves".to_string()) {
        setup_fan_curve_page(&ui, config.clone());
    }

    // Populate GPU page choices and callbacks
    setup_gpu::setup_gpu_page(&ui);

    ui
}

pub fn setup_app_settings_page(ui: &MainWindow, config: Arc<Mutex<Config>>) {
    let config_copy = config.clone();
    let global = ui.global::<AppSettingsPageData>();
    global.on_set_run_in_background(move |enable| {
        if let Ok(mut lock) = config_copy.try_lock() {
            lock.run_in_background = enable;
            lock.write();
        }
    });
    let config_copy = config.clone();
    global.on_set_startup_in_background(move |enable| {
        if let Ok(mut lock) = config_copy.try_lock() {
            lock.startup_in_background = enable;
            lock.write();
        }
    });
    let config_copy = config.clone();
    global.on_set_enable_tray_icon(move |enable| {
        if let Ok(mut lock) = config_copy.try_lock() {
            lock.enable_tray_icon = enable;
            lock.write();
        }
    });
    let config_copy = config.clone();
    global.on_set_enable_dgpu_notifications(move |enable| {
        if let Ok(mut lock) = config_copy.try_lock() {
            lock.notifications.enabled = enable;
            lock.write();
        }
    });
    let config_copy = config.clone();
    global.on_set_auto_refresh_rate(move |enable| {
        if let Ok(mut lock) = config_copy.try_lock() {
            lock.auto_refresh_rate = enable;
            lock.write();
        }
    });

    if let Ok(lock) = config.try_lock() {
        global.set_run_in_background(lock.run_in_background);
        global.set_startup_in_background(lock.startup_in_background);
        global.set_enable_tray_icon(lock.enable_tray_icon);
        global.set_enable_dgpu_notifications(lock.notifications.enabled);
        global.set_auto_refresh_rate(lock.auto_refresh_rate);
    }
}
