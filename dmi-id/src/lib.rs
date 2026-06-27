use log::warn;

#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Clone)]
pub struct DMIID {
    pub id_model: String,
    pub dmi_family: String,
    pub dmi_vendor: String,
    pub board_name: String,
    pub board_vendor: String,
    pub bios_date: String,
    pub bios_release: String,
    pub bios_vendor: String,
    pub bios_version: String,
    pub product_family: String,
    pub product_name: String,
}

impl DMIID {
    pub fn new() -> Result<Self, String> {
        let mut enumerator = udev::Enumerator::new().map_err(|err| {
            warn!("{err}");
            format!("dmi enumerator failed: {err}")
        })?;

        enumerator.match_subsystem("dmi").map_err(|err| {
            warn!("{err}");
            format!("dmi match_subsystem failed: {err}")
        })?;

        let mut result = enumerator.scan_devices().map_err(|err| {
            warn!("{err}");
            format!("dmi scan_devices failed: {err}")
        })?;

        if let Some(device) = (result).next() {
            let get_prop = |name| {
                device
                    .property_value(name)
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Unknown".to_string())
            };
            let get_attr = |name| {
                device
                    .attribute_value(name)
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Unknown".to_string())
            };

            return Ok(Self {
                id_model: get_prop("ID_MODEL"),
                dmi_family: get_prop("DMI_FAMILY"),
                dmi_vendor: get_prop("DMI_VENDOR"),
                board_name: get_attr("board_name"),
                board_vendor: get_attr("board_vendor"),
                bios_date: get_attr("bios_date"),
                bios_release: get_attr("bios_release"),
                bios_vendor: get_attr("bios_vendor"),
                bios_version: get_attr("bios_version"),
                product_family: get_attr("product_family"),
                product_name: get_attr("product_name"),
            });
        }
        Err("dmi not found".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "Does not run in docker images"]
    fn dmi_sysfs_properties_not_unknown() {
        let dmi = DMIID::new().unwrap();

        assert_ne!(dmi.id_model, "Unknown".to_string());
        dbg!(dmi.id_model);
        assert_ne!(dmi.dmi_family, "Unknown".to_string());
        dbg!(dmi.dmi_family);
        assert_ne!(dmi.dmi_vendor, "Unknown".to_string());
        dbg!(dmi.dmi_vendor);
        assert_ne!(dmi.board_name, "Unknown".to_string());
        dbg!(dmi.board_name);
        assert_ne!(dmi.board_vendor, "Unknown".to_string());
        dbg!(dmi.board_vendor);
        assert_ne!(dmi.product_family, "Unknown".to_string());
        dbg!(dmi.product_family);
        assert_ne!(dmi.product_name, "Unknown".to_string());
        dbg!(dmi.product_name);
    }
}
