//! `config_traits` is a crate that broke out from the requirement to manage
//! various different config files, including parsing from different formats and
//! updating them from previous versions where fields or names are changed in
//! some way.
//!
//! The end canonical file format is `.ron` as this supports rust types well

use std::fs::{self, File, OpenOptions, create_dir};
use std::io::{Read, Write};
use std::path::PathBuf;

use log::{error, warn};
pub use ron;
use ron::ser::PrettyConfig;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Config file helper traits. Only `new()` and `file_name()` are required to be
/// implemented, the rest are intended to be free methods.
pub trait StdConfig
where
    Self: Serialize + DeserializeOwned,
{
    /// Taking over the standard `new()` to ensure things can be generic
    fn new() -> Self;

    /// Return the config files names, such as `wibble.cfg`
    fn file_name(&self) -> String;

    /// Return the full path to the directory the config file resides in
    fn config_dir() -> PathBuf;

    /// Return the full path to the config file
    fn file_path(&self) -> PathBuf {
        let mut config = Self::config_dir();
        if !config.exists() {
            create_dir(config.as_path())
                .unwrap_or_else(|e| panic!("Could not create {:?} {e}", Self::config_dir()));
        }
        config.push(self.file_name());
        let mut do_rename = !config.exists();
        let mut cfg_old = config.with_extension("cfg");
        // Migrating all configs to .ron format, so we do need to check for older ones
        if do_rename && cfg_old.exists() {
            // Now we gotta rename it
            warn!("Renaming {cfg_old:?} to {config:?}");
            std::fs::rename(&cfg_old, &config).unwrap_or_else(|err| {
                error!(
                    "Could not rename. Please remove {} then restart service: Error {}",
                    self.file_name(),
                    err
                );
            });
            do_rename = false;
        }
        if do_rename {
            cfg_old = config.with_extension("conf");
            if cfg_old.exists() {
                // Now we gotta rename it
                warn!("Renaming {cfg_old:?} to {config:?}");
                std::fs::rename(&cfg_old, &config).unwrap_or_else(|err| {
                    error!(
                        "Could not rename. Please remove {} then restart service: Error {}",
                        self.file_name(),
                        err
                    );
                });
            }
        }
        config
    }

    /// Directly open the config file for read and write. If the config file
    /// does not exist it is created, including the directories the file
    /// resides in.
    fn file_open(&self) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.file_path())
            .unwrap_or_else(|e| panic!("Could not open {:?} {e}", self.file_path()))
    }

    /// Open and parse the config file to self from ron format
    fn read(&mut self) {
        if let Ok(data) = fs::read_to_string(self.file_path()) {
            if data.is_empty() {
                warn!("File is empty {:?}", self.file_path());
            } else if let Ok(data) = ron::from_str(&data) {
                *self = data;
            } else {
                warn!("Could not deserialise {:?}", self.file_path());
            }
        }
    }

    /// Open and parse the config file to self from ron format
    fn read_new(&self) -> Option<Self> {
        if let Ok(data) = fs::read_to_string(self.file_path()) {
            if data.is_empty() {
                warn!("File is empty {:?}", self.file_path());
            } else if let Ok(data) = ron::from_str(&data) {
                return Some(data);
            } else {
                warn!("Could not deserialise {:?}", self.file_path());
            }
        }
        None
    }

    /// Write the config file data to pretty ron format
    fn write(&self) {
        let mut file = match File::create(self.file_path()) {
            Ok(data) => data,
            Err(e) => {
                error!(
                    "Couldn't overwrite config {:?}, error: {e}",
                    self.file_path()
                );
                return;
            }
        };
        let ron = match ron::ser::to_string_pretty(&self, PrettyConfig::new().depth_limit(4)) {
            Ok(data) => data,
            Err(e) => {
                error!("Parse {:?} to RON failed, error: {e}", self.file_path());
                return;
            }
        };
        file.write_all(ron.as_bytes())
            .unwrap_or_else(|err| error!("Could not write config: {err}"));
    }

    /// Renames the existing file to `<file>-old`
    fn rename_file_old(&self) {
        warn!("Renaming {} to {}-old", self.file_name(), self.file_name());
        let mut cfg_old = self.file_path();
        if let Some(name) = cfg_old.file_name() {
            let mut name = name.to_os_string();
            name.push("-old");
            cfg_old.set_file_name(name);
        }
        std::fs::rename(self.file_path(), cfg_old).unwrap_or_else(|err| {
            error!(
                "Could not rename. Please remove {} then restart service: Error {}",
                self.file_name(),
                err
            );
        });
    }
}

#[macro_export]
macro_rules! std_config_load {
    ($trait_name:ident: $($generic:ident),*) => {
        /// Base trait for loading/parsing. This is intended to be used to help update
        /// configs to new versions
        ///
        /// # Example
        /// ```rust
        /// use std::path::PathBuf;
        /// use serde::{Deserialize, Serialize};
        /// use config_traits::{StdConfig, StdConfigLoad2};
        ///
        /// #[derive(Deserialize, Serialize, Debug)]
        /// struct FanCurveConfigOld {}
        ///
        /// #[derive(Deserialize, Serialize, Debug)]
        /// struct FanCurveConfigOlder {}
        ///
        /// #[derive(Deserialize, Serialize, Debug)]
        /// struct FanCurveConfig {}
        ///
        /// impl From<FanCurveConfigOld> for FanCurveConfig {
        ///     fn from(_: FanCurveConfigOld) -> Self { Self {} }
        /// }
        ///
        /// impl From<FanCurveConfigOlder> for FanCurveConfig {
        ///     fn from(_: FanCurveConfigOlder) -> Self { Self {} }
        /// }
        ///
        /// impl StdConfig for FanCurveConfig {
        ///     fn new() -> Self { Self {} }
        ///
        ///     fn file_name(&self) -> std::string::String { "test_name.conf".to_owned() }
        ///
        ///     fn config_dir() -> PathBuf { PathBuf::from("/tmp") }
        /// }
        ///
        /// impl StdConfigLoad2<FanCurveConfigOld, FanCurveConfigOlder> for FanCurveConfig {}
        /// ```
        ///
        /// If all of the generics fails to parse, then the old config is renamed and a
        /// new one created
        pub trait $trait_name<$($generic),*>
        where
            Self: $crate::StdConfig + DeserializeOwned + Serialize,
            $($generic: DeserializeOwned + Into<Self>),*
        {
            fn load(mut self) -> Self {
                let mut file = self.file_open();
                let mut buf = String::new();
                if let Ok(read_len) = file.read_to_string(&mut buf) {
                    if read_len != 0 {
                        if let Ok(data) = ron::from_str(&buf) {
                            self = data;
                            log::info!("Parsed RON for {:?}", std::any::type_name::<Self>());
                        }  $(else if let Ok(data) = ron::from_str::<$generic>(&buf) {
                            self = data.into();
                            log::info!("New version failed, trying previous: Parsed RON for {:?}", std::any::type_name::<$generic>());
                        })* else {
                            self.rename_file_old();
                            self = Self::new();
                        }
                    } else {
                        error!("Config file {} zero read length", self.file_name());
                    }
                }
                self.write();
                self
            }
        }
    };
}

std_config_load!(StdConfigLoad:);
std_config_load!(StdConfigLoad1: T1);
std_config_load!(StdConfigLoad2: T1, T2);
std_config_load!(StdConfigLoad3: T1, T2, T3);
std_config_load!(StdConfigLoad4: T1, T2, T3, T4);

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn check_macro_from_1() {
        #[derive(serde::Deserialize, serde::Serialize, Debug)]
        struct Test {}

        #[derive(serde::Deserialize, serde::Serialize, Debug)]
        struct Old1 {}

        impl crate::StdConfig for Test {
            fn new() -> Self {
                Self {}
            }

            fn file_name(&self) -> String {
                String::new()
            }

            fn config_dir() -> PathBuf {
                PathBuf::new()
            }
        }

        impl From<Old1> for Test {
            fn from(_: Old1) -> Self {
                Self {}
            }
        }

        let _ = Test {};

        impl crate::StdConfigLoad1<Old1> for Test {}
    }

    #[test]
    fn check_macro_from_3() {
        #[derive(serde::Deserialize, serde::Serialize, Debug)]
        struct Test {}

        #[derive(serde::Deserialize, serde::Serialize, Debug)]
        struct Old1 {}

        #[derive(serde::Deserialize, serde::Serialize, Debug)]
        struct Old2 {}

        #[derive(serde::Deserialize, serde::Serialize, Debug)]
        struct Old3 {}

        impl crate::StdConfig for Test {
            fn new() -> Self {
                Self {}
            }

            fn file_name(&self) -> String {
                String::new()
            }

            fn config_dir() -> PathBuf {
                PathBuf::new()
            }
        }

        impl From<Old1> for Test {
            fn from(_: Old1) -> Self {
                Self {}
            }
        }

        impl From<Old2> for Test {
            fn from(_: Old2) -> Self {
                Self {}
            }
        }

        impl From<Old3> for Test {
            fn from(_: Old3) -> Self {
                Self {}
            }
        }

        let _ = Test {};

        impl crate::StdConfigLoad3<Old1, Old2, Old3> for Test {}
    }
}
