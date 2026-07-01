#![deny(unused_must_use)]
/// Configuration loading, saving
pub mod config;
pub mod ctrl_backlight;
/// Control platform profiles + fan-curves if available
pub mod ctrl_fancurves;
/// GPU power status monitoring and D-Bus exposure
pub mod ctrl_gpu;
/// Control ASUS bios function such as boot sound, Optimus/Dedicated gfx mode
pub mod ctrl_platform;
pub mod ctrl_xgm_led;

pub mod asus_armoury;
pub mod aura_anime;
pub mod aura_lamparray;
pub mod aura_laptop;
pub mod aura_manager;
pub mod aura_scsi;
pub mod aura_slash;
pub mod aura_types;
pub mod error;

use std::future::Future;
use std::time::Duration;

use dmi_id::DMIID;
use futures_util::stream::StreamExt;
use log::{debug, info, warn};
use logind_zbus::manager::ManagerProxy;
use tokio::time::sleep;
use zbus::object_server::{Interface, SignalEmitter};
use zbus::proxy::CacheProperties;
use zbus::zvariant::ObjectPath;
use zbus::Connection;

use crate::error::RogError;

const CONFIG_PATH_BASE: &str = "/etc/asusd/";
pub const ASUS_ZBUS_PATH: &str = "/xyz/ljones";

pub static DBUS_NAME: &str = "xyz.ljones.Asusd";
pub static DBUS_PATH: &str = "/xyz/ljones/Daemon";
pub static DBUS_IFACE: &str = "xyz.ljones.Asusd";

/// This macro adds a function which spawns an `inotify` task on the passed in
/// `Executor`.
///
/// The generated function is `watch_<name>()`. Self requires the following
/// methods to be available:
/// - `<name>() -> SomeValue`, functionally is a getter, but is allowed to have
///   side effects.
/// - `notify_<name>(SignalEmitter, SomeValue)`
///
/// In most cases if `SomeValue` is stored in a config then `<name>()` getter is
/// expected to update it. The getter should *never* write back to the path or
/// attribute that is being watched or an infinite loop will occur.
///
/// # Example
///
/// ```ignore
/// impl RogPlatform {
///     task_watch_item!(panel_od platform);
///     task_watch_item!(gpu_mux_mode platform);
/// }
/// ```\
/// // TODO: this is kind of useless if it can't trigger some action
#[macro_export]
macro_rules! task_watch_item {
    ($name:ident $name_str:literal $self_inner:ident) => {
        concat_idents::concat_idents!(fn_name = watch_, $name {
        async fn fn_name(
            &self,
            signal_ctxt: SignalEmitter<'static>,
        ) -> Result<(), RogError> {
            use futures_util::StreamExt;

            let ctrl = self.clone();
            concat_idents::concat_idents!(watch_fn = monitor_, $name {
                match self.$self_inner.watch_fn() {
                    Ok(watch) => {
                        tokio::spawn(async move {
                            let mut buffer = [0; 32];
                            watch.into_event_stream(&mut buffer).unwrap().for_each(|_| async {
                                if let Ok(value) = ctrl.$name() { // get new value from zbus method
                                    if ctrl.config.lock().await.$name != value {
                                        log::debug!("{} was changed to {} externally", $name_str, value);
                                        concat_idents::concat_idents!(notif_fn = $name, _changed {
                                            ctrl.notif_fn(&signal_ctxt).await.ok();
                                        });
                                        let mut lock = ctrl.config.lock().await;
                                        lock.$name = value;
                                        lock.write();
                                    }
                                }
                            }).await;
                        });
                    }
                    Err(e) => info!("inotify watch failed: {}. You can ignore this if your device does not support the feature", e),
                }
            });
            Ok(())
        }
        });
    };
}

#[macro_export]
macro_rules! task_watch_item_notify {
    ($name:ident $self_inner:ident) => {
        concat_idents::concat_idents!(fn_name = watch_, $name {
        async fn fn_name(
            &self,
            signal_ctxt: SignalEmitter<'static>,
        ) -> Result<(), RogError> {
            use futures_util::StreamExt;

            let ctrl = self.clone();
            concat_idents::concat_idents!(watch_fn = monitor_, $name {
                match self.$self_inner.watch_fn() {
                    Ok(watch) => {
                        tokio::spawn(async move {
                            let mut buffer = [0; 32];
                            watch.into_event_stream(&mut buffer).unwrap().for_each(|_| async {
                                concat_idents::concat_idents!(notif_fn = $name, _changed {
                                    ctrl.notif_fn(&signal_ctxt).await.ok();
                                });
                            }).await;
                        });
                    }
                    Err(e) => info!("inotify watch failed: {}. You can ignore this if your device does not support the feature", e),
                }
            });
            Ok(())
        }
        });
    };
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn print_board_info() {
    let dmi = DMIID::new().unwrap_or_default();
    info!("Product family: {}", dmi.product_family);
    info!("Board name: {}", dmi.board_name);
}

pub trait Reloadable {
    fn reload(&mut self) -> impl Future<Output = Result<(), RogError>> + Send;
}

pub trait ReloadAndNotify {
    type Data: Send;

    fn reload_and_notify(
        &mut self,
        signal_context: &SignalEmitter<'static>,
        data: Self::Data,
    ) -> impl Future<Output = Result<(), RogError>> + Send;
}

pub trait ZbusRun {
    fn add_to_server(self, server: &mut Connection) -> impl Future<Output = ()> + Send;

    fn add_to_server_helper(
        iface: impl Interface,
        path: &str,
        server: &mut Connection,
    ) -> impl Future<Output = ()> + Send {
        async move {
            server
                .object_server()
                .at(&ObjectPath::from_str_unchecked(path), iface)
                .await
                .map_err(|err| {
                    warn!("{}: add_to_server {}", path, err);
                    err
                })
                .ok();
        }
    }
}

/// Set up a task to run on the async executor
pub trait CtrlTask {
    fn zbus_path() -> &'static str;

    fn signal_context(connection: &Connection) -> Result<SignalEmitter<'static>, zbus::Error> {
        SignalEmitter::new(connection, Self::zbus_path())
    }

    /// Implement to set up various tasks that may be required, using the
    /// `Executor`. No blocking loops are allowed, or they must be run on a
    /// separate thread.
    fn create_tasks(
        &self,
        signal: SignalEmitter<'static>,
    ) -> impl Future<Output = Result<(), RogError>> + Send;

    // /// Create a timed repeating task
    // async fn repeating_task(&self, millis: u64, mut task: impl FnMut() + Send +
    // 'static) {     use std::time::Duration;
    //     use tokio::time;
    //     let mut timer = time::interval(Duration::from_millis(millis));
    //     tokio::spawn(async move {
    //         timer.tick().await;
    //         task();
    //     });
    // }

    /// Free helper method to create tasks to run on: sleep, wake, shutdown,
    /// boot
    ///
    /// The closures can potentially block, so execution time should be the
    /// minimal possible such as save a variable.
    fn create_sys_event_tasks<Fut1, Fut2, Fut3, Fut4, F1, F2, F3, F4>(
        &self,
        mut on_prepare_for_sleep: F1,
        mut on_prepare_for_shutdown: F2,
        mut on_lid_change: F3,
        mut on_external_power_change: F4,
    ) -> impl Future<Output = ()> + Send
    where
        F1: FnMut(bool) -> Fut1 + Send + 'static,
        F2: FnMut(bool) -> Fut2 + Send + 'static,
        F3: FnMut(bool) -> Fut3 + Send + 'static,
        F4: FnMut(bool) -> Fut4 + Send + 'static,
        Fut1: Future<Output = ()> + Send,
        Fut2: Future<Output = ()> + Send,
        Fut3: Future<Output = ()> + Send,
        Fut4: Future<Output = ()> + Send,
    {
        async {
            let connection = Connection::system()
                .await
                .expect("Controller could not create dbus connection");

            let manager = ManagerProxy::builder(&connection)
                .cache_properties(CacheProperties::No)
                .build()
                .await
                .expect("Controller could not create ManagerProxy");

            let manager1 = manager.clone();
            tokio::spawn(async move {
                if let Ok(mut notif) = manager1.receive_prepare_for_shutdown().await {
                    while let Some(event) = notif.next().await {
                        // blocks thread :|
                        if let Ok(args) = event.args() {
                            debug!("Doing on_prepare_for_shutdown({})", args.start);
                            on_prepare_for_shutdown(args.start).await;
                        }
                    }
                }
            });

            let manager2 = manager.clone();
            tokio::spawn(async move {
                if let Ok(mut notif) = manager2.receive_prepare_for_sleep().await {
                    while let Some(event) = notif.next().await {
                        // blocks thread :|
                        if let Ok(args) = event.args() {
                            debug!("Doing on_prepare_for_sleep({})", args.start);
                            on_prepare_for_sleep(args.start).await;
                        }
                    }
                }
            });

            let manager3 = manager.clone();
            tokio::spawn(async move {
                let mut last_power = manager3.on_external_power().await.unwrap_or_default();

                loop {
                    if let Ok(next) = manager3.on_external_power().await {
                        if next != last_power {
                            last_power = next;
                            on_external_power_change(next).await;
                        }
                    }
                    sleep(Duration::from_secs(2)).await;
                }
            });

            tokio::spawn(async move {
                let mut last_lid = manager.lid_closed().await.unwrap_or_default();
                // need to loop on these as they don't emit signals
                loop {
                    if let Ok(next) = manager.lid_closed().await {
                        if next != last_lid {
                            last_lid = next;
                            on_lid_change(next).await;
                        }
                    }
                    sleep(Duration::from_secs(2)).await;
                }
            });
        }
    }
}

pub trait GetSupported {
    type A;

    fn get_supported() -> Self::A;
}

pub async fn start_tasks<T>(
    mut zbus: T,
    connection: &mut Connection,
    signal_ctx: SignalEmitter<'static>,
) -> Result<(), RogError>
where
    T: ZbusRun + Reloadable + CtrlTask + Clone,
{
    let zbus_clone = zbus.clone();

    zbus.reload()
        .await
        .unwrap_or_else(|err| warn!("Controller error: {}", err));
    zbus.add_to_server(connection).await;

    zbus_clone.create_tasks(signal_ctx).await.ok();
    Ok(())
}
