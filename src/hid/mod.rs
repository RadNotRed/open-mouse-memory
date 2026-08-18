pub mod descriptor;
pub mod transport;

use std::collections::BTreeMap;
use std::ffi::CStr;

use hidapi::{BusType, HidApi};
use serde::Serialize;

use crate::error::{AppError, Result};

pub const LOGITECH_VENDOR_ID: u16 = 0x046d;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HidEndpoint {
    pub path: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub usage_page: u16,
    pub usage: u16,
    pub collections: Vec<HidCollection>,
    pub interface_number: i32,
    pub bus_type: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct HidCollection {
    pub usage_page: u16,
    pub usage: u16,
}

impl HidEndpoint {
    pub fn is_receiver(&self) -> bool {
        (0xc500..=0xc5ff).contains(&self.product_id)
    }

    pub fn is_probable_hidpp_interface(&self) -> bool {
        self.interface_number == 2 || self.usage_page >= 0xff00
    }

    pub fn is_direct_hidpp_device(&self) -> bool {
        (0xc07d..=0xc0ff).contains(&self.product_id)
            || (0xc32b..=0xc3ff).contains(&self.product_id)
            || (self.interface_number == 2
                && self.usage_page >= 0xff00
                && !self
                    .product
                    .as_deref()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    .contains("receiver"))
    }

    pub fn open(&self, api: &HidApi) -> Result<hidapi::HidDevice> {
        let c_path = std::ffi::CString::new(self.path.as_bytes())
            .map_err(|_| AppError::Other(format!("invalid HID path {}", self.path)))?;
        api.open_path(&c_path)
            .map_err(|error| AppError::hid(&self.path, error))
    }
}

pub fn enumerate_logitech(api: &HidApi) -> Vec<HidEndpoint> {
    let mut by_path = BTreeMap::<String, HidEndpoint>::new();
    for info in api
        .device_list()
        .filter(|info| info.vendor_id() == LOGITECH_VENDOR_ID)
    {
        let path = cstr_lossy(info.path());
        let collection = HidCollection {
            usage_page: info.usage_page(),
            usage: info.usage(),
        };
        if let Some(endpoint) = by_path.get_mut(&path) {
            if !endpoint.collections.contains(&collection) {
                endpoint.collections.push(collection);
            }
            if collection.usage_page > endpoint.usage_page {
                endpoint.usage_page = collection.usage_page;
                endpoint.usage = collection.usage;
            }
            continue;
        }
        by_path.insert(
            path.clone(),
            HidEndpoint {
                path,
                vendor_id: info.vendor_id(),
                product_id: info.product_id(),
                serial_number: info.serial_number().map(str::to_owned),
                manufacturer: info.manufacturer_string().map(str::to_owned),
                product: info.product_string().map(str::to_owned),
                usage_page: collection.usage_page,
                usage: collection.usage,
                collections: vec![collection],
                interface_number: info.interface_number(),
                bus_type: bus_name(info.bus_type()).to_owned(),
            },
        );
    }
    let mut endpoints: Vec<_> = by_path.into_values().collect();
    endpoints.sort_by(|a, b| {
        (a.product_id, a.interface_number, &a.path).cmp(&(b.product_id, b.interface_number, &b.path))
    });
    endpoints
}

pub fn refresh_api() -> Result<HidApi> {
    HidApi::new().map_err(|error| AppError::Hid {
        path: "hidapi".to_owned(),
        message: error.to_string(),
    })
}

fn cstr_lossy(value: &CStr) -> String {
    value.to_string_lossy().into_owned()
}

fn bus_name(bus: BusType) -> &'static str {
    match bus {
        BusType::Usb => "usb",
        BusType::Bluetooth => "bluetooth",
        BusType::I2c => "i2c",
        BusType::Spi => "spi",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_superlight_2_wired_usb_identity() {
        let endpoint = HidEndpoint {
            path: "/dev/hidraw23".to_owned(),
            vendor_id: LOGITECH_VENDOR_ID,
            product_id: 0xc09b,
            serial_number: Some("0723ECD7".to_owned()),
            manufacturer: Some("Logitech".to_owned()),
            product: Some("PRO X 2".to_owned()),
            usage_page: 0xff00,
            usage: 1,
            collections: vec![HidCollection {
                usage_page: 0xff00,
                usage: 1,
            }],
            interface_number: 2,
            bus_type: "usb".to_owned(),
        };
        assert!(endpoint.is_direct_hidpp_device());
        assert!(!endpoint.is_receiver());
    }
}
