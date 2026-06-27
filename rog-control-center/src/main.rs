use std::env::{self, args};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::{Arc, Mutex};
use std::thread::{self, sleep};
use std::time::Duration;

use config_traits::{StdConfig, StdConfigLoad1};
use dmi_id::DMIID;
use gumdrop::Options;
use log::{LevelFilter, debug, info, warn};
use rog_control_center::cli_options::CliStart;
use rog_control_center::config::Config;
use rog_control_center::error::Result;
use rog_control_center::notify::start_notifications;
use rog_control_center::slint::ComponentHandle;
use rog_control_center::tray::init_tray;
use rog_control_center::ui::setup_window;
use rog_control_center::zbus_proxies::{
    AppState, ROGCCZbus, ROGCCZbusProxyBlocking, ZBUS_IFACE, ZBUS_PATH,
};
use rog_control_center::{MainWindow, print_versions};
use tokio::runtime::Runtime;

#[tokio::main]
async fn main() -> Result<()> {
    // Ensure tracing spans are quiet by default unless user overrides
    if std::env::var_os("RUST_LOG").is_none() {
        unsafe {
            std::env::set_var("RUST_LOG", "warn,tracing=error,zbus=error");
        }
    }
    let mut logger = env_logger::Builder::new();
    logger
        .parse_default_env()
        .filter_level(LevelFilter::Info)
        .parse_default_env()
        .target(env_logger::Target::Stderr)
        .format_timestamp(None)
        .init();

    // If we're running under gamescope we have to set WAYLAND_DISPLAY for winit to
    // use
    if let Ok(gamescope) = env::var("GAMESCOPE_WAYLAND_DISPLAY") {
        debug!("Gamescope detected");
        if !gamescope.is_empty() {
            debug!("Setting WAYLAND_DISPLAY to {}", gamescope);
            unsafe {
                env::set_var("WAYLAND_DISPLAY", gamescope);
            }
        }
        // gamescope-0
        else if let Ok(wayland) = env::var("WAYLAND_DISPLAY") {
            debug!("Wayland display detected");
            if wayland.is_empty() {
                debug!("Setting WAYLAND_DISPLAY to gamescope-0");
                unsafe {
                    env::set_var("WAYLAND_DISPLAY", "gamescope-0");
                }
            }
        }
    }

    // Try to open a proxy and check for app state first
    {
        let user_con = zbus::blocking::Connection::session()?;
        if let Ok(proxy) = ROGCCZbusProxyBlocking::new(&user_con) {
            if let Ok(state) = proxy.state() {
                info!("App is already running: {state:?}, opening the window");
                // if there is a proxy connection assume the app is already running
                proxy.set_state(AppState::MainWindowShouldOpen)?;
                std::process::exit(0);
            }
        }
    }

    // version checks
    let self_version = env!("CARGO_PKG_VERSION");
    let zbus_con = zbus::blocking::Connection::system()?;
    let platform_proxy = rog_dbus::zbus_platform::PlatformProxyBlocking::new(&zbus_con)?;
    let asusd_version = platform_proxy
        .version()
        .map_err(|e| {
            println!("Could not get asusd version: {e:?}\nIs asusd.service running?");
        })
        .unwrap();
    if asusd_version != self_version {
        println!("Version mismatch: asusctl = {self_version}, asusd = {asusd_version}");
        // return Ok(());
    }

    // start tokio
    let rt = Runtime::new().expect("Unable to create Runtime");
    // Enter the runtime so that `tokio::spawn` is available immediately.
    let _enter = rt.enter();

    #[cfg(feature = "tokio-debug")]
    console_subscriber::init();

    let state_zbus = ROGCCZbus::new();
    let app_state = state_zbus.clone_state();
    let _conn = zbus::connection::Builder::session()?
        .name(ZBUS_IFACE)?
        .serve_at(ZBUS_PATH, state_zbus)?
        .build()
        .await
        .map_err(|err| {
            warn!("{}: add_to_server {}", ZBUS_PATH, err);
            err
        })?;

    let dmi = DMIID::new().unwrap_or_default();
    let board_name = dmi.board_name;
    let prod_family = dmi.product_family;
    info!("Running on {board_name}, product: {prod_family}");

    let args: Vec<String> = args().skip(1).collect();

    let cli_parsed = match CliStart::parse_args_default(&args) {
        Ok(p) => p,
        Err(err) => {
            panic!("source {}", err);
        }
    };

    if do_cli_help(&cli_parsed) {
        return Ok(());
    }

    let supported_properties = platform_proxy.supported_properties().unwrap_or_default();

    // Startup
    let mut config = Config::new().load();
    if cli_parsed.fullscreen {
        config.start_fullscreen = true;
        if cli_parsed.width_fullscreen != 0 {
            config.fullscreen_width = cli_parsed.width_fullscreen;
        }
        if cli_parsed.height_fullscreen != 0 {
            config.fullscreen_height = cli_parsed.height_fullscreen;
        }
    } else if cli_parsed.windowed {
        config.start_fullscreen = false;
    }

    let is_rog_ally = {
        #[cfg(feature = "rog_ally")]
        {
            board_name == "RC71L" || board_name == "RC72L" || prod_family == "ROG Ally"
        }
        #[cfg(not(feature = "rog_ally"))]
        {
            false
        }
    };

    let is_tuf = {
        let b = board_name.to_lowercase();
        let p = prod_family.to_lowercase();
        b.contains("tuf") || p.contains("tuf")
    };

    #[cfg(feature = "rog_ally")]
    if is_rog_ally {
        config.notifications.enabled = false;
        config.enable_tray_icon = false;
        config.run_in_background = false;
        config.startup_in_background = false;
        config.start_fullscreen = true;
    }

    config.write();

    let enable_tray_icon = config.enable_tray_icon;
    let startup_in_background = config.startup_in_background;
    let config = Arc::new(Mutex::new(config));

    start_notifications(config.clone(), &rt)?;

    if enable_tray_icon {
        init_tray(supported_properties, config.clone());
    }

    thread_local! { pub static UI: std::cell::RefCell<Option<MainWindow>> = Default::default()};
    // i_slint_backend_selector::with_platform(|_| Ok(())).unwrap();

    if !startup_in_background {
        if let Ok(mut app_state) = app_state.lock() {
            *app_state = AppState::MainWindowShouldOpen;
        }
    }

    if std::env::var("RUST_TRANSLATIONS").is_ok() {
        // don't care about content
        log::debug!("---- Using local-dir translations");
        slint::init_translations!("/usr/share/locale/");
    } else {
        log::debug!("Using system installed translations");
        slint::init_translations!(concat!(env!("CARGO_MANIFEST_DIR"), "/translations/"));
    }

    // Prefetch supported Aura modes once at startup and move into the
    // spawned UI thread so the UI uses a stable, immutable list.
    let prefetched_supported: std::sync::Arc<Option<Vec<i32>>> = std::sync::Arc::new(
        rog_control_center::ui::setup_aura::prefetch_supported_basic_modes().await,
    );

    thread::spawn(move || {
        let mut state = AppState::StartingUp;
        loop {
            if is_rog_ally {
                let config_copy_2 = config.clone();
                let newui = setup_window(config.clone(), prefetched_supported.clone(), is_tuf);
                newui.window().on_close_requested(move || {
                    exit(0);
                });

                let ui_copy = newui.as_weak();
                newui
                    .window()
                    .set_rendering_notifier(move |s, _| {
                        if let slint::RenderingState::BeforeRendering = s {
                            let config = config_copy_2.clone();
                            ui_copy
                                .upgrade_in_event_loop(move |w| {
                                    let fullscreen =
                                        config.lock().is_ok_and(|c| c.start_fullscreen);
                                    if fullscreen && !w.window().is_fullscreen() {
                                        w.window().set_fullscreen(fullscreen);
                                    }
                                })
                                .ok();
                        }
                    })
                    .ok();

                continue;
            }

            // save as a var, don't hold the lock the entire time or deadlocks happen
            if let Ok(app_state) = app_state.lock() {
                state = *app_state;
            }

            // This sleep is required to give the event loop time to react
            sleep(Duration::from_millis(300));
            if state == AppState::MainWindowShouldOpen {
                if let Ok(mut app_state) = app_state.lock() {
                    *app_state = AppState::MainWindowOpen;
                }

                let config_copy = config.clone();
                let app_state_copy = app_state.clone();
                // Avoid moving the original `prefetched_supported` into the
                // closure — clone an Arc for the closure to capture.
                let pref_for_invoke = prefetched_supported.clone();
                slint::invoke_from_event_loop(move || {
                    UI.with(|ui| {
                        let app_state_copy = app_state_copy.clone();
                        let mut ui = ui.borrow_mut();
                        if let Some(ui) = ui.as_mut() {
                            ui.window().show().unwrap();
                            ui.window().on_close_requested(move || {
                                if let Ok(mut app_state) = app_state_copy.lock() {
                                    *app_state = AppState::MainWindowClosed;
                                }
                                slint::CloseRequestResponse::HideWindow
                            });
                        } else {
                            let config_copy_2 = config_copy.clone();
                            let newui = setup_window(config_copy, pref_for_invoke.clone(), is_tuf);
                            newui.window().on_close_requested(move || {
                                if let Ok(mut app_state) = app_state_copy.lock() {
                                    *app_state = AppState::MainWindowClosed;
                                }
                                slint::CloseRequestResponse::HideWindow
                            });

                            let ui_copy = newui.as_weak();
                            newui
                                .window()
                                .set_rendering_notifier(move |s, _| {
                                    if let slint::RenderingState::RenderingSetup = s {
                                        let config = config_copy_2.clone();
                                        ui_copy
                                            .upgrade_in_event_loop(move |w| {
                                                let fullscreen =
                                                    config.lock().is_ok_and(|c| c.start_fullscreen);
                                                if fullscreen && !w.window().is_fullscreen() {
                                                    w.window().set_fullscreen(fullscreen);
                                                }
                                            })
                                            .ok();
                                    }
                                })
                                .ok();
                            ui.replace(newui);
                        }
                    });
                })
                .unwrap();
            } else if state == AppState::QuitApp {
                slint::quit_event_loop().unwrap();
                exit(0);
            } else if state != AppState::MainWindowOpen {
                if let Ok(config) = config.lock() {
                    if !config.run_in_background {
                        slint::quit_event_loop().unwrap();
                        exit(0);
                    }
                }
            }
        }
    });

    slint::run_event_loop_until_quit().unwrap();
    rt.shutdown_background();
    Ok(())
}

fn do_cli_help(parsed: &CliStart) -> bool {
    if parsed.help {
        println!("{}", CliStart::usage());
        println!();
        if let Some(cmdlist) = CliStart::command_list() {
            let commands: Vec<String> = cmdlist.lines().map(|s| s.to_owned()).collect();
            for command in &commands {
                println!("{}", command);
            }
        }
    }

    if parsed.version {
        print_versions();
        println!();
    }

    parsed.help
}

pub fn get_layout_path(path: &Path, layout_name: &str) -> PathBuf {
    let mut data_path = PathBuf::from(path);
    let layout_file = format!("{}_US.ron", layout_name);
    data_path.push("layouts");
    data_path.push(layout_file);
    data_path
}
