use std::convert::TryFrom;
use std::path::Path;
use std::process::Command;
use std::thread::sleep;

use anime_cli::{AnimeActions, AnimeCommand};
use aura_cli::{LedPowerCommand1, LedPowerCommand2};
use dmi_id::DMIID;
use fan_curve_cli::FanCurveCommand;
use log::{LevelFilter, error, info};
use rog_anime::usb::get_anime_type;
use rog_anime::{AnimTime, AnimeDataBuffer, AnimeDiagonal, AnimeGif, AnimeImage, AnimeType, Vec2};
use rog_aura::keyboard::{AuraPowerState, LaptopAuraPower};
use rog_aura::{self, AuraEffect, PowerZones};
use rog_dbus::asus_armoury::AsusArmouryProxyBlocking;
use rog_dbus::list_iface_blocking;
use rog_dbus::scsi_aura::ScsiAuraProxyBlocking;
use rog_dbus::zbus_anime::AnimeProxyBlocking;
use rog_dbus::zbus_aura::AuraProxyBlocking;
use rog_dbus::zbus_backlight::BacklightProxyBlocking;
use rog_dbus::zbus_fan_curves::FanCurvesProxyBlocking;
use rog_dbus::zbus_platform::PlatformProxyBlocking;
use rog_platform::platform::{PlatformProfile, Properties};
use rog_profiles::error::ProfileError;
use rog_scsi::AuraMode;
use ron::ser::PrettyConfig;
use scsi_cli::ScsiCommand;
use zbus::blocking::Connection;
use zbus::blocking::proxy::ProxyImpl;

use crate::cli_opts::*;
use crate::slash_cli::{
    SlashCommand, SlashSubCommand, handle_slash_get, handle_slash_list, handle_slash_set,
};

mod anime_cli;
mod aura_cli;
mod cli_opts;
mod fan_curve_cli;
mod scsi_cli;
mod slash_cli;
mod xgm_led_cli;

fn main() {
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
        .target(env_logger::Target::Stderr)
        .format_timestamp(None)
        .init();

    let parsed: CliStart = argh::from_env();

    let conn = Connection::system().unwrap();
    if let Ok(platform_proxy) = PlatformProxyBlocking::new(&conn).map_err(|e| {
        check_service("asusd");
        println!("\nError: {e}\n");
        print_info();
    }) {
        let asusd_version = match platform_proxy.version() {
            Ok(version) => version,
            Err(e) => {
                error!(
                    "Could not get asusd version: {e:?}\nIs asusd.service running? {}",
                    check_service("asusd")
                );
                return;
            }
        };

        let self_version = env!("CARGO_PKG_VERSION");
        if asusd_version != self_version {
            println!("Version mismatch: asusctl = {self_version}, asusd = {asusd_version}");
            return;
        }

        let supported_properties = match platform_proxy.supported_properties() {
            Ok(props) => props,
            Err(e) => {
                error!("Could not get supported properties: {e:?}");
                return;
            }
        };
        let supported_interfaces = match list_iface_blocking() {
            Ok(ifaces) => ifaces,
            Err(e) => {
                error!("Could not get supported interfaces: {e:?}");
                return;
            }
        };

        if let Err(err) = do_parsed(&parsed, &supported_interfaces, &supported_properties, conn) {
            print_error_help(&*err, &supported_interfaces, &supported_properties);
        }
    }
}

fn print_error_help(
    err: &dyn std::error::Error,
    supported_interfaces: &[String],
    supported_properties: &[Properties],
) {
    check_service("asusd");
    println!("\nError: {}\n", err);
    print_info();
    println!();
    println!("Supported interfaces:\n\n{:#?}\n", supported_interfaces);
    println!(
        "Supported properties on Platform:\n\n{:#?}\n",
        supported_properties
    );
}

fn print_info() {
    let dmi = DMIID::new().unwrap_or_default();
    let board_name = dmi.board_name;
    let prod_family = dmi.product_family;
    println!("Software version: {}", env!("CARGO_PKG_VERSION"));
    println!("  Product family: {}", prod_family.trim());
    println!("      Board name: {}", board_name.trim());
}

fn check_service(name: &str) -> bool {
    if name != "asusd" && !check_systemd_unit_enabled(name) {
        println!(
            "\n\x1b[0;31m{} is not enabled, enable it with `systemctl enable {}\x1b[0m",
            name, name
        );
        return true;
    } else if !check_systemd_unit_active(name) {
        println!(
            "\n\x1b[0;31m{} is not running, start it with `systemctl start {}\x1b[0m",
            name, name
        );
        return true;
    }
    false
}

fn find_iface<T>(iface_name: &str) -> Result<Vec<T>, Box<dyn std::error::Error>>
where
    T: ProxyImpl<'static> + From<zbus::Proxy<'static>>,
{
    let conn = zbus::blocking::Connection::system().unwrap();
    let f = zbus::blocking::fdo::ObjectManagerProxy::new(&conn, "xyz.ljones.Asusd", "/").unwrap();
    let interfaces = f.get_managed_objects().unwrap();
    let mut paths = Vec::new();
    for v in interfaces.iter() {
        // let o: Vec<zbus::names::OwnedInterfaceName> = v.1.keys().map(|e|
        // e.to_owned()).collect(); println!("{}, {:?}", v.0, o);
        for k in v.1.keys() {
            if k.as_str() == iface_name {
                // println!("Found {iface_name} device at {}, {}", v.0, k);
                paths.push(v.0.clone());
            }
        }
    }
    if paths.len() > 1 {
        println!("Multiple asusd interfaces devices found");
    }
    if !paths.is_empty() {
        let mut ctrl = Vec::new();
        paths.sort_by(|a, b| a.cmp(b));
        for path in paths {
            ctrl.push(
                T::builder(&conn)
                    .path(path.clone())?
                    .destination("xyz.ljones.Asusd")?
                    .build()?,
            );
        }
        return Ok(ctrl);
    }

    Err(format!("Did not find {iface_name}").into())
}

fn do_parsed(
    parsed: &CliStart,
    supported_interfaces: &[String],
    supported_properties: &[Properties],
    conn: Connection,
) -> Result<(), Box<dyn std::error::Error>> {
    match &parsed.command {
        CliCommand::Aura(a) => match &a.command {
            crate::cli_opts::AuraSubCommand::Effect(mode) => handle_led_mode(mode)?,
            crate::cli_opts::AuraSubCommand::PowerTuf(pow) => handle_led_power1(pow)?,
            crate::cli_opts::AuraSubCommand::Power(pow) => handle_led_power2(pow)?,
        },
        CliCommand::Brightness(cmd) => handle_brightness(cmd)?,
        CliCommand::Profile(cmd) => handle_throttle_profile(&conn, supported_properties, cmd)?,
        CliCommand::FanCurve(cmd) => handle_fan_curve(&conn, cmd)?,
        CliCommand::Anime(cmd) => handle_anime(cmd)?,
        CliCommand::Slash(cmd) => handle_slash(cmd)?,
        CliCommand::Scsi(cmd) => handle_scsi(cmd)?,
        CliCommand::Armoury(cmd) => handle_armoury_command(cmd)?,
        CliCommand::Backlight(cmd) => handle_backlight(cmd)?,
        CliCommand::Battery(cmd) => handle_battery(cmd, &conn)?,
        CliCommand::XgmLed(cmd) => xgm_led_cli::handle_xgm_led(&cmd.command)?,
        CliCommand::Info(info_opt) => {
            handle_info(info_opt, supported_interfaces, supported_properties)?
        }
    }

    Ok(())
}

fn handle_battery(
    cmd: &BatteryCommand,
    conn: &Connection,
) -> Result<(), Box<dyn std::error::Error>> {
    match &cmd.command {
        BatterySubCommand::Limit(l) => {
            let proxy = PlatformProxyBlocking::new(conn)?;
            proxy.set_charge_control_end_threshold(l.limit)?;
        }
        BatterySubCommand::OneShot(o) => {
            let proxy = PlatformProxyBlocking::new(conn)?;
            if let Some(p) = o.percent {
                proxy.set_charge_control_end_threshold(p)?;
            }
            proxy.one_shot_full_charge()?;
        }
        BatterySubCommand::Info(_) => {
            let proxy = PlatformProxyBlocking::new(conn)?;
            let limit = proxy.charge_control_end_threshold()?;
            println!("Current battery charge limit: {}%", limit);
        }
    }

    Ok(())
}

fn handle_info(
    info_opt: &InfoCommand,
    supported_interfaces: &[String],
    supported_properties: &[Properties],
) -> Result<(), Box<dyn std::error::Error>> {
    println!("asusctl v{}", env!("CARGO_PKG_VERSION"));
    println!();
    print_info();
    println!();

    if info_opt.show_supported {
        println!("Supported Core Functions:\n{:#?}", supported_interfaces);
        println!(
            "Supported Platform Properties:\n{:#?}",
            supported_properties
        );
        if let Ok(aura) = find_iface::<AuraProxyBlocking>("xyz.ljones.Aura") {
            // TODO: multiple RGB check
            let bright = aura.first().unwrap().supported_brightness()?;
            let modes = aura.first().unwrap().supported_basic_modes()?;
            let zones = aura.first().unwrap().supported_basic_zones()?;
            let power = aura.first().unwrap().supported_power_zones()?;
            println!("Supported Keyboard Brightness:\n{:#?}", bright);
            println!("Supported Aura Modes:\n{:#?}", modes);
            println!("Supported Aura Zones:\n{:#?}", zones);
            println!("Supported Aura Power Zones:\n{:#?}", power);
        } else {
            println!("No aura interface found");
        }
    }

    Ok(())
}

fn handle_backlight(cmd: &BacklightCommand) -> Result<(), Box<dyn std::error::Error>> {
    if cmd.screenpad_brightness.is_none()
        && cmd.screenpad_gamma.is_none()
        && cmd.sync_screenpad_brightness.is_none()
    {
        let backlights = find_iface::<BacklightProxyBlocking>("xyz.ljones.Backlight")?;
        for backlight in backlights {
            println!("Current screenpad settings:");
            println!("  Brightness: {}", backlight.screenpad_brightness()?);
            println!("  Gamma: {}", backlight.screenpad_gamma()?);
            println!(
                "  Sync with primary: {}",
                backlight.screenpad_sync_with_primary()?
            );
        }

        return Ok(());
    }

    let backlights = find_iface::<BacklightProxyBlocking>("xyz.ljones.Backlight")?;
    for backlight in backlights {
        if let Some(brightness) = cmd.screenpad_brightness {
            backlight.set_screenpad_brightness(brightness)?;
        }

        if let Some(gamma) = cmd.screenpad_gamma {
            backlight.set_screenpad_gamma(gamma.to_string().as_str())?;
        }

        if let Some(sync) = cmd.sync_screenpad_brightness {
            backlight.set_screenpad_sync_with_primary(sync)?;
        }
    }

    Ok(())
}

fn handle_brightness(cmd: &BrightnessCommand) -> Result<(), Box<dyn std::error::Error>> {
    let Ok(aura_proxies) = find_iface::<AuraProxyBlocking>("xyz.ljones.Aura") else {
        println!("No aura interface found");
        return Ok(());
    };

    match &cmd.command {
        BrightnessSubCommand::Set(s) => {
            for aura in aura_proxies.iter() {
                if let Some(level) = s.level.level() {
                    aura.set_brightness(rog_aura::LedBrightness::from(level))?;
                } else {
                    let current = aura.brightness()?;
                    println!("Current keyboard led brightness: {current:?}");
                }
            }
        }
        BrightnessSubCommand::Get(_) => {
            for aura in aura_proxies.iter() {
                let level = aura.brightness()?;
                println!("Current keyboard led brightness: {level:?}");
            }

            return Ok(());
        }
        BrightnessSubCommand::Next(_) => {
            for aura in aura_proxies.iter() {
                let brightness = aura.brightness()?;
                aura.set_brightness(brightness.next())?;
            }
        }
        BrightnessSubCommand::Prev(_) => {
            for aura in aura_proxies.iter() {
                let brightness = aura.brightness()?;
                aura.set_brightness(brightness.prev())?;
            }
        }
    }

    Ok(())
}

fn handle_anime(cmd: &AnimeCommand) -> Result<(), Box<dyn std::error::Error>> {
    if cmd.command.is_none()
        && cmd.enable_display.is_none()
        && cmd.enable_powersave_anim.is_none()
        && cmd.brightness.is_none()
        && cmd.off_when_lid_closed.is_none()
        && cmd.off_when_suspended.is_none()
        && cmd.off_when_unplugged.is_none()
        && cmd.off_with_his_head.is_none()
        && !cmd.clear
    {
        println!("Missing arg or command; run 'asusctl anime --help' for usage");
    }

    let animes = find_iface::<AnimeProxyBlocking>("xyz.ljones.Anime").map_err(|e| {
        error!("Did not find any interface for xyz.ljones.Anime: {e:?}");
        e
    })?;

    for proxy in animes {
        if let Some(enable) = cmd.enable_display {
            proxy.set_enable_display(enable)?;
        }
        if let Some(enable) = cmd.enable_powersave_anim {
            proxy.set_builtins_enabled(enable)?;
        }
        if let Some(bright) = cmd.brightness {
            proxy.set_brightness(bright)?;
        }
        if let Some(enable) = cmd.off_when_lid_closed {
            proxy.set_off_when_lid_closed(enable)?;
        }
        if let Some(enable) = cmd.off_when_suspended {
            proxy.set_off_when_suspended(enable)?;
        }
        if let Some(enable) = cmd.off_when_unplugged {
            proxy.set_off_when_unplugged(enable)?;
        }
        if cmd.off_with_his_head.is_some() {
            println!("Did Alice _really_ make it back from Wonderland?");
        }

        let mut anime_type = get_anime_type();
        if let AnimeType::Unsupported = anime_type {
            if let Some(model) = cmd.override_type {
                anime_type = model;
            }
        }

        if cmd.clear {
            let data = vec![255u8; anime_type.data_length()];
            let tmp = AnimeDataBuffer::from_vec(anime_type, data)?;
            proxy.write(tmp)?;
        }

        if let Some(action) = cmd.command.as_ref() {
            match action {
                AnimeActions::Image(image) => {
                    if image.path.is_empty() {
                        println!(
                            "Missing arg or command; run 'asusctl anime image --help' for usage"
                        );
                        return Ok(());
                    }
                    verify_brightness(image.bright);

                    let matrix = AnimeImage::from_png(
                        Path::new(&image.path),
                        image.scale,
                        image.angle,
                        Vec2::new(image.x_pos, image.y_pos),
                        image.bright,
                        anime_type,
                    )?;

                    proxy.write(<AnimeDataBuffer>::try_from(&matrix)?)?;
                }
                AnimeActions::PixelImage(image) => {
                    if image.path.is_empty() {
                        println!(
                            "Missing arg or command; run 'asusctl anime pixel-image --help' for usage"
                        );
                        return Ok(());
                    }
                    verify_brightness(image.bright);

                    let matrix = AnimeDiagonal::from_png(
                        Path::new(&image.path),
                        None,
                        image.bright,
                        anime_type,
                    )?;

                    proxy.write(matrix.into_data_buffer(anime_type)?)?;
                }
                AnimeActions::Gif(gif) => {
                    if gif.path.is_empty() {
                        println!(
                            "Missing arg or command; run 'asusctl anime gif --help' for usage"
                        );
                        return Ok(());
                    }
                    verify_brightness(gif.bright);

                    let matrix = AnimeGif::from_gif(
                        Path::new(&gif.path),
                        gif.scale,
                        gif.angle,
                        Vec2::new(gif.x_pos, gif.y_pos),
                        AnimTime::Count(1),
                        gif.bright,
                        anime_type,
                    )?;

                    let mut loops = gif.loops as i32;
                    loop {
                        for frame in matrix.frames() {
                            proxy.write(frame.frame().clone())?;
                            sleep(frame.delay());
                        }
                        if loops >= 0 {
                            loops -= 1;
                        }
                        if loops == 0 {
                            break;
                        }
                    }
                }
                AnimeActions::PixelGif(gif) => {
                    if gif.path.is_empty() {
                        println!(
                            "Missing arg or command; run 'asusctl anime pixel-gif --help' for usage"
                        );
                        return Ok(());
                    }
                    verify_brightness(gif.bright);

                    let matrix = AnimeGif::from_diagonal_gif(
                        Path::new(&gif.path),
                        AnimTime::Count(1),
                        gif.bright,
                        anime_type,
                    )?;

                    let mut loops = gif.loops as i32;
                    loop {
                        for frame in matrix.frames() {
                            proxy.write(frame.frame().clone())?;
                            sleep(frame.delay());
                        }
                        if loops >= 0 {
                            loops -= 1;
                        }
                        if loops == 0 {
                            break;
                        }
                    }
                }
                AnimeActions::SetBuiltins(builtins) => {
                    if builtins.set.is_none() {
                        println!("Missing arg; run 'asusctl anime set-builtins --help' for usage");
                        return Ok(());
                    }

                    proxy.set_builtin_animations(rog_anime::Animations {
                        boot: builtins.boot,
                        awake: builtins.awake,
                        sleep: builtins.sleep,
                        shutdown: builtins.shutdown,
                    })?;
                }
            }
        }
    }
    Ok(())
}

fn verify_brightness(brightness: f32) {
    if !(0.0..=1.0).contains(&brightness) {
        println!(
            "Image and global brightness must be between 0.0 and 1.0 (inclusive), was {}",
            brightness
        );
    }
}

fn handle_slash(cmd: &SlashCommand) -> Result<(), Box<dyn std::error::Error>> {
    match &cmd.command {
        SlashSubCommand::Get(_) => handle_slash_get()?,
        SlashSubCommand::Set(cmd) => handle_slash_set(cmd)?,
        SlashSubCommand::List(_) => handle_slash_list(),
    }

    Ok(())
}

fn handle_scsi(cmd: &ScsiCommand) -> Result<(), Box<dyn std::error::Error>> {
    if !cmd.list && cmd.enable.is_none() && cmd.mode.is_none() && cmd.colours.is_empty() {
        println!("Missing arg or command; run 'asusctl scsi --help' for usage");
    }

    let scsis = find_iface::<ScsiAuraProxyBlocking>("xyz.ljones.ScsiAura")?;

    for scsi in scsis {
        if let Some(enable) = cmd.enable {
            scsi.set_enabled(enable)?;
        }

        if let Some(mode) = cmd.mode {
            dbg!(mode as u8);
            scsi.set_led_mode(mode).unwrap();
        }

        let mut mode = scsi.led_mode_data()?;
        let mut do_update = false;
        if !cmd.colours.is_empty() {
            for (count, c) in cmd.colours.iter().enumerate() {
                if count == 0 {
                    mode.colour1 = *c;
                }
                if count == 1 {
                    mode.colour2 = *c;
                }
                if count == 2 {
                    mode.colour3 = *c;
                }
                if count == 3 {
                    mode.colour4 = *c;
                }
            }
            do_update = true;
        }

        if let Some(speed) = cmd.speed {
            mode.speed = speed;
            do_update = true;
        }

        if let Some(dir) = cmd.direction {
            mode.direction = dir;
            do_update = true;
        }

        if do_update {
            scsi.set_led_mode_data(mode.clone())?;
        }

        // let mode_ret = scsi.led_mode_data()?;
        // assert_eq!(mode, mode_ret);
        println!("{mode}");
    }

    if cmd.list {
        let res = AuraMode::list();
        for p in &res {
            println!("{:?}", p);
        }
    }

    Ok(())
}

fn handle_led_mode(mode: &LedModeCommand) -> Result<(), Box<dyn std::error::Error>> {
    if mode.command.is_none() && !mode.prev_mode && !mode.next_mode {
        println!("Missing arg or command; run 'asusctl aura --help' for usage");
        // print available modes when possible
        if let Ok(aura) = find_iface::<AuraProxyBlocking>("xyz.ljones.Aura") {
            let modes = aura.first().unwrap().supported_basic_modes()?;
            println!("Available modes:");
            for m in modes {
                println!("  {:?}", m);
            }
        }
        return Ok(());
    }

    if mode.next_mode && mode.prev_mode {
        println!("Please specify either next or previous");
        return Ok(());
    }
    let aura = find_iface::<AuraProxyBlocking>("xyz.ljones.Aura")?;
    if mode.next_mode {
        for aura in aura {
            let mode = aura.led_mode()?;
            let modes = aura.supported_basic_modes()?;
            let mut pos = modes.iter().position(|m| *m == mode).unwrap() + 1;
            if pos >= modes.len() {
                pos = 0;
            }
            aura.set_led_mode(modes[pos])?;
        }
    } else if mode.prev_mode {
        for aura in aura {
            let mode = aura.led_mode()?;
            let modes = aura.supported_basic_modes()?;
            let mut pos = modes.iter().position(|m| *m == mode).unwrap();
            if pos == 0 {
                pos = modes.len() - 1;
            } else {
                pos -= 1;
            }
            aura.set_led_mode(modes[pos])?;
        }
    } else if let Some(mode) = mode.command.as_ref() {
        for aura in aura {
            aura.set_led_mode_data(<AuraEffect>::from(mode))?;
        }
    }

    Ok(())
}

fn handle_led_power1(power: &LedPowerCommand1) -> Result<(), Box<dyn std::error::Error>> {
    let aura = find_iface::<AuraProxyBlocking>("xyz.ljones.Aura")?;
    for aura in aura {
        let dev_type = aura.device_type()?;
        if !dev_type.is_old_laptop() && !dev_type.is_tuf_laptop() {
            println!("This option applies only to keyboards 2021+");
        }

        if power.awake.is_none()
            && power.sleep.is_none()
            && power.boot.is_none()
            && !power.keyboard
            && !power.lightbar
        {
            println!("Missing arg or command; run 'asusctl aura power-tuf --help' for usage");
            return Ok(());
        }

        if dev_type.is_old_laptop() || dev_type.is_tuf_laptop() {
            handle_led_power_1_do_1866(&aura, power)?;
            return Ok(());
        }
    }

    println!("These options are for keyboards of product ID 0x1866 or TUF only");
    Ok(())
}

fn handle_led_power_1_do_1866(
    aura: &AuraProxyBlocking,
    power: &LedPowerCommand1,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut states = Vec::new();
    if power.keyboard {
        states.push(AuraPowerState {
            zone: PowerZones::Keyboard,
            boot: power.boot.unwrap_or_default(),
            awake: power.awake.unwrap_or_default(),
            sleep: power.sleep.unwrap_or_default(),
            shutdown: false,
        });
    }
    if power.lightbar {
        states.push(AuraPowerState {
            zone: PowerZones::Lightbar,
            boot: power.boot.unwrap_or_default(),
            awake: power.awake.unwrap_or_default(),
            sleep: power.sleep.unwrap_or_default(),
            shutdown: false,
        });
    }

    let states = LaptopAuraPower { states };
    aura.set_led_power(states)?;
    Ok(())
}

fn handle_led_power2(power: &LedPowerCommand2) -> Result<(), Box<dyn std::error::Error>> {
    let aura = find_iface::<AuraProxyBlocking>("xyz.ljones.Aura")?;
    for aura in aura {
        let dev_type = aura.device_type()?;
        if !dev_type.is_new_laptop() {
            println!("This option applies only to keyboards 2021+");
            continue;
        }

        if power.command.is_none() {
            println!("Missing arg or command; run 'asusctl aura power --help' for usage");
            println!("Commands available");
            return Ok(());
        }

        if let Some(_pow) = power.command.as_ref() {
            let mut states = aura.led_power()?;
            let mut set =
                |zone: PowerZones, boot_v: bool, awake_v: bool, sleep_v: bool, shutdown_v: bool| {
                    for state in states.states.iter_mut() {
                        if state.zone == zone {
                            state.boot = boot_v;
                            state.awake = awake_v;
                            state.sleep = sleep_v;
                            state.shutdown = shutdown_v;
                            break;
                        }
                    }
                };

            if let Some(cmd) = &power.command {
                match cmd {
                    aura_cli::SetAuraZoneEnabled::Keyboard(k) => {
                        set(PowerZones::Keyboard, k.boot, k.awake, k.sleep, k.shutdown)
                    }
                    aura_cli::SetAuraZoneEnabled::Logo(l) => {
                        set(PowerZones::Logo, l.boot, l.awake, l.sleep, l.shutdown)
                    }
                    aura_cli::SetAuraZoneEnabled::Lightbar(l) => {
                        set(PowerZones::Lightbar, l.boot, l.awake, l.sleep, l.shutdown)
                    }
                    aura_cli::SetAuraZoneEnabled::Lid(l) => {
                        set(PowerZones::Lid, l.boot, l.awake, l.sleep, l.shutdown)
                    }
                    aura_cli::SetAuraZoneEnabled::RearGlow(r) => {
                        set(PowerZones::RearGlow, r.boot, r.awake, r.sleep, r.shutdown)
                    }
                    aura_cli::SetAuraZoneEnabled::Ally(r) => {
                        set(PowerZones::Ally, r.boot, r.awake, r.sleep, r.shutdown)
                    }
                }
            }

            aura.set_led_power(states)?;
        }
    }

    Ok(())
}

fn handle_throttle_profile(
    conn: &Connection,
    supported: &[Properties],
    cmd: &ProfileCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    if !supported.contains(&Properties::ThrottlePolicy) {
        println!("Profiles not supported by either this kernel or by the laptop.");
        return Err(ProfileError::NotSupported.into());
    }

    let proxy = PlatformProxyBlocking::new(conn)?;
    let current = proxy.platform_profile()?;
    let choices = proxy.platform_profile_choices()?;

    match &cmd.command {
        crate::cli_opts::ProfileSubCommand::Next(_) => {
            proxy.set_platform_profile(PlatformProfile::next(current, &choices))?;
        }
        crate::cli_opts::ProfileSubCommand::Set(s) => {
            if !s.ac && !s.battery {
                proxy.set_platform_profile(s.profile)?;
            } else {
                if s.ac {
                    proxy.set_platform_profile_on_ac(s.profile)?;
                }
                if s.battery {
                    proxy.set_platform_profile_on_battery(s.profile)?;
                }
            }
        }
        crate::cli_opts::ProfileSubCommand::List(_) => {
            for p in &choices {
                println!("{:?}", p);
            }
        }
        crate::cli_opts::ProfileSubCommand::Get(_) => {
            println!("Active profile: {current:?}");
            println!();
            println!("AC profile {:?}", proxy.platform_profile_on_ac()?);
            println!("Battery profile {:?}", proxy.platform_profile_on_battery()?);
        }
    }

    Ok(())
}

fn handle_fan_curve(
    conn: &Connection,
    cmd: &FanCurveCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    let Ok(fan_proxy) = FanCurvesProxyBlocking::new(conn).map_err(|e| {
        println!("Fan-curves not supported by either this kernel or by the laptop: {e:?}");
    }) else {
        return Err(ProfileError::NotSupported.into());
    };

    if !cmd.get_enabled && !cmd.default && cmd.mod_profile.is_none() {
        println!("Missing arg or command; run 'asusctl fan-curve --help' for usage");
        return Ok(());
    }

    if (cmd.enable_fan_curves.is_some() || cmd.fan.is_some() || cmd.data.is_some())
        && cmd.mod_profile.is_none()
    {
        println!(
            "--enable-fan-curves, --enable-fan-curve, --fan, and --data options require \
             --mod-profile"
        );
        return Ok(());
    }

    let plat_proxy = PlatformProxyBlocking::new(conn)?;
    if cmd.get_enabled {
        let profile = plat_proxy.platform_profile()?;
        let curves = fan_proxy.fan_curve_data(profile)?;
        for curve in curves.iter() {
            println!("{}", String::from(curve));
        }
    }

    if cmd.default {
        let active = plat_proxy.platform_profile()?;
        fan_proxy.set_curves_to_defaults(active)?;
    }

    if let Some(profile) = cmd.mod_profile {
        if cmd.enable_fan_curves.is_none() && cmd.data.is_none() {
            let data = fan_proxy.fan_curve_data(profile)?;
            let ron = ron::ser::to_string_pretty(&data, PrettyConfig::new().depth_limit(4))?;
            println!("\nFan curves for {:?}\n\n{}", profile, ron);
        }

        if let Some(enabled) = cmd.enable_fan_curves {
            fan_proxy.set_fan_curves_enabled(profile, enabled)?;
        }

        if let Some(enabled) = cmd.enable_fan_curve {
            if let Some(fan) = cmd.fan {
                fan_proxy.set_profile_fan_curve_enabled(profile, fan, enabled)?;
            } else {
                println!(
                    "--enable-fan-curves, --enable-fan-curve, --fan, and --data options require \
                     --mod-profile"
                );
            }
        }

        if let Some(mut curve) = cmd.data.clone() {
            let fan = cmd.fan.unwrap_or_default();
            curve.set_fan(fan);
            fan_proxy.set_fan_curve(profile, curve)?;
        }
    }

    Ok(())
}

fn check_systemd_unit_active(name: &str) -> bool {
    if let Ok(out) = Command::new("systemctl")
        .arg("is-active")
        .arg(name)
        .output()
    {
        let buf = String::from_utf8_lossy(&out.stdout);
        return !buf.contains("inactive") && !buf.contains("failed");
    }
    false
}

fn check_systemd_unit_enabled(name: &str) -> bool {
    if let Ok(out) = Command::new("systemctl")
        .arg("is-enabled")
        .arg(name)
        .output()
    {
        let buf = String::from_utf8_lossy(&out.stdout);
        return buf.contains("enabled") || buf.contains("linked");
    }
    false
}

fn print_firmware_attr(attr: &AsusArmouryProxyBlocking) -> Result<(), Box<dyn std::error::Error>> {
    let name = attr.name()?;
    println!("{}:", <&str>::from(name));

    // Be resilient to DBus read failures: if any read fails, show "unavailable"
    let attrs = attr.available_attrs().unwrap_or_default();

    let has_min = attrs.contains(&"min_value".to_string());
    let has_max = attrs.contains(&"max_value".to_string());
    let has_current = attrs.contains(&"current_value".to_string());
    let has_possible = attrs.contains(&"possible_values".to_string());
    let has_default = attrs.contains(&"default_value".to_string());

    if has_current && (has_min || has_max) {
        let c = attr.current_value().ok();
        let min = if has_min { attr.min_value().ok() } else { None };
        let max = if has_max { attr.max_value().ok() } else { None };
        match (min, c, max) {
            (Some(min), Some(c), Some(max)) => println!("  current: {min}..[{c}]..{max}"),
            (Some(min), Some(c), None) => println!("  current: {min}..[{c}]"),
            (None, Some(c), Some(max)) => println!("  current: [{c}]..{max}"),
            _ => println!("  current: unavailable"),
        }

        if has_default {
            match attr.default_value().ok() {
                Some(d) => println!("  default: {}\n", d),
                None => println!("  default: unavailable\n"),
            }
        } else {
            println!();
        }
    } else if has_possible && has_current {
        let c = attr.current_value().ok();
        let v = attr.possible_values().ok();
        if let (Some(c), Some(v)) = (c, v) {
            for p in v.iter().enumerate() {
                if p.0 == 0 {
                    print!("  current: [");
                }
                if *p.1 == c {
                    print!("({c})");
                } else {
                    print!("{}", p.1);
                }
                if p.0 < v.len() - 1 {
                    print!(",");
                }
                if p.0 == v.len() - 1 {
                    print!("]");
                }
            }
            if has_default {
                match attr.default_value().ok() {
                    Some(d) => println!("  default: {}\n", d),
                    None => println!("  default: unavailable\n"),
                }
            } else {
                println!("\n");
            }
        } else {
            println!("  current: unavailable\n");
        }
    } else if has_current {
        match attr.current_value().ok() {
            Some(c) => println!("  current: {c}\n"),
            None => println!("  current: unavailable\n"),
        }
    } else {
        println!("  unavailable\n");
    }

    Ok(())
}

#[allow(clippy::manual_is_multiple_of, clippy::nonminimal_bool)]
fn handle_armoury_command(cmd: &ArmouryCommand) -> Result<(), Box<dyn std::error::Error>> {
    // If nested subcommand provided, handle set/get/list.
    match &cmd.command {
        ArmourySubCommand::List(_) => {
            if let Ok(attrs) = find_iface::<AsusArmouryProxyBlocking>("xyz.ljones.AsusArmoury") {
                for attr in attrs.iter() {
                    print_firmware_attr(attr)?;
                }
            }
            Ok(())
        }
        ArmourySubCommand::Get(g) => {
            if let Ok(attrs) = find_iface::<AsusArmouryProxyBlocking>("xyz.ljones.AsusArmoury") {
                for attr in attrs.iter() {
                    let name = attr.name()?;
                    if <&str>::from(name) == g.property {
                        print_firmware_attr(attr)?;
                    }
                }
            }
            Ok(())
        }
        ArmourySubCommand::Set(s) => {
            if let Ok(attrs) = find_iface::<AsusArmouryProxyBlocking>("xyz.ljones.AsusArmoury") {
                for attr in attrs.iter() {
                    let name = attr.name()?;
                    if <&str>::from(name) == s.property {
                        let mut value: i32 = s.value;
                        if value == -1 {
                            info!("Setting to default");
                            value = attr.default_value()?;
                        }
                        attr.set_current_value(value)?;
                        print_firmware_attr(attr)?;
                    }
                }
            }
            Ok(())
        }
    }
}
