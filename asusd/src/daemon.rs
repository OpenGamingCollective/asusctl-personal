use std::env;
use std::error::Error;
use std::sync::Arc;

use ::zbus::object_server::SignalEmitter;
use ::zbus::Connection;
use asusd::asus_armoury::{start_attributes_zbus, ArmouryAttributeRegistry};
use asusd::aura_manager::DeviceManager;
use asusd::config::Config;
use asusd::ctrl_backlight::CtrlBacklight;
use asusd::ctrl_fancurves::CtrlFanCurveZbus;
use asusd::ctrl_gpu::CtrlGpu;
use asusd::ctrl_platform::CtrlPlatform;
use asusd::ctrl_xgm_led::CtrlXgmLed;
use asusd::{print_board_info, start_tasks, CtrlTask, Reloadable, ZbusRun, DBUS_NAME};
use config_traits::{StdConfig, StdConfigLoad2};
use log::{error, info, warn};
use rog_platform::asus_armoury::FirmwareAttributes;
use rog_platform::platform::RogPlatform;
use rog_platform::power::AsusPower;
use tokio::sync::Mutex;
use zbus::fdo::ObjectManager;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting asusd daemon...");

    // console_subscriber::init();
    let mut logger = env_logger::Builder::new();
    logger
        .parse_default_env()
        .target(env_logger::Target::Stdout)
        .format_timestamp(None)
        .filter_level(log::LevelFilter::Debug)
        .init();

    let is_service = match env::var_os("IS_SERVICE") {
        Some(val) => val == "1",
        None => true,
    };

    if !is_service {
        println!("asusd schould be only run from the right systemd service");
        println!(
            "do not run in your terminal, if you need an logs please use journalctl -b -u asusd"
        );
        println!("asusd will now exit");
        return Ok(());
    }

    info!("       daemon v{}", asusd::VERSION);
    info!("    rog-anime v{}", rog_anime::VERSION);
    info!("    rog-slash v{}", rog_slash::VERSION);
    info!("     rog-aura v{}", rog_aura::VERSION);
    info!(" rog-profiles v{}", rog_profiles::VERSION);
    info!("rog-platform v{}", rog_platform::VERSION);

    start_daemon().await?;
    Ok(())
}

/// The actual main loop for the daemon
async fn start_daemon() -> Result<(), Box<dyn Error>> {
    // let supported = SupportedFunctions::get_supported();
    print_board_info();
    // println!("{:?}", supported.supported_functions());

    // Start zbus server
    let mut server = Connection::system().await?;
    if let Err(e) = server.object_server().at("/", ObjectManager).await {
        error!("Failed to register ObjectManager at root '/': {e:?}");
    }

    let config = Config::new().load();
    let cfg_path = config.file_path();
    let config = Arc::new(Mutex::new(config));

    // supported.add_to_server(&mut connection).await;
    let platform = RogPlatform::new()?; // TODO: maybe needs async mutex?
    let power = AsusPower::new()?; // TODO: maybe needs async mutex?
    let attributes = FirmwareAttributes::new();
    let armoury_registry = match start_attributes_zbus(
        &server,
        platform.clone(),
        power.clone(),
        attributes.clone(),
        config.clone(),
    )
    .await
    {
        Ok(registry) => {
            info!("attribute on zbus initialized");
            registry
        }
        Err(e) => {
            error!("Failed to initialize firmware attributes over zbus: {e:?}");
            ArmouryAttributeRegistry::default()
        }
    };

    let fan_curve_config = match CtrlFanCurveZbus::new() {
        Ok(mut ctrl) => {
            info!("FanCurves: found supported fancurves");
            ctrl.set_platform_refs(config.clone(), power.clone());
            let fc_config = Some(ctrl.config());
            ctrl.reload()
                .await
                .unwrap_or_else(|err| warn!("FanCurves reload: {}", err));
            ctrl.add_to_server(&mut server).await;
            info!("FanCurves: initialized");
            fc_config
        }
        Err(err) => {
            error!("FanCurves: {}", err);
            None
        }
    };

    match CtrlBacklight::new(config.clone()) {
        Ok(backlight) => {
            info!("Backlight: found supported backlight");
            backlight.start_watch_primary().await?;
            backlight.add_to_server(&mut server).await;
            info!("Backlight: initialized");
        }
        Err(err) => {
            error!("Backlight: {}", err);
        }
    }

    match CtrlPlatform::new(
        platform,
        power,
        attributes,
        config.clone(),
        &cfg_path,
        CtrlPlatform::signal_context(&server)?,
        server.clone(),
        armoury_registry,
        fan_curve_config,
    ) {
        Ok(ctrl) => {
            info!("CtrlPlatform: initialized");
            let sig_ctx = CtrlPlatform::signal_context(&server)?;
            start_tasks(ctrl, &mut server, sig_ctx).await?;
            info!("CtrlPlatform: tasks started");
        }
        Err(err) => {
            error!("CtrlPlatform: {}", err);
        }
    }

    // XG Mobile LED (non-fatal if not attached)
    match CtrlXgmLed::try_new(config.clone()) {
        Ok(Some(ctrl)) => {
            info!("XG Mobile LED: found and initialized");
            ctrl.add_to_server(&mut server).await;
        }
        Ok(None) => info!("XG Mobile LED: not present"),
        Err(e) => error!("XG Mobile LED: {e}"),
    }

    // GPU power status monitoring (non-fatal if detection fails)
    {
        let gpu_ctrl = CtrlGpu::new(server.clone());
        let sig_ctx = SignalEmitter::new(&server, "/xyz/ljones/Gpu")
            .map_err(|e| error!("CtrlGpu: failed to create signal context: {e}"))
            .ok();
        gpu_ctrl.clone().add_to_server(&mut server).await;
        if let Some(ctx) = sig_ctx {
            if let Err(e) = gpu_ctrl.start_watcher(ctx).await {
                error!("CtrlGpu: watcher failed: {e}");
            }
        }
        info!("CtrlGpu: initialized");
    }

    // Request the well-known dbus name BEFORE we go enumerate hidraw nodes.
    // systemd's `Type=dbus` ready signal is the appearance of this name on
    // the bus; if we delay it behind device probing (which on FA608WV
    // includes a LampArray HID conversation that can stall on a kernel
    // hidraw read), systemd kills us with a TimeoutSec=10 timeout and the
    // service ends up reported as ServiceUnknown / "not activatable".
    // We can still register additional zbus objects after request_name; the
    // object server accepts new paths at any time.
    server.request_name(DBUS_NAME).await?;

    info!("Startup ready on dbus name {DBUS_NAME}: hidraw enumeration runs in background");

    // Now do the hidraw enumeration. The well-known name is already on the
    // bus so systemd considers us ready, and if a device probe stalls it
    // cannot trigger the systemd start timeout. (We can't easily push this
    // off into a `tokio::spawn` because `udev::Enumerator` and friends are
    // not `Send`.)
    match DeviceManager::new(server.clone()).await {
        Ok(_) => info!("DeviceManager initialized"),
        Err(e) => error!("DeviceManager init failed: {e:?}"),
    }

    info!("Entering dbus server loop");
    loop {
        // This is just a blocker to idle and ensure the reator reacts
        server.executor().tick().await;
    }
}
