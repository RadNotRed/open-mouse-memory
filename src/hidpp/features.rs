use serde::Serialize;

use crate::error::{AppError, Result};
use crate::hid::transport::HidIo;
use crate::hidpp::protocol::HidppTransport;

pub const ROOT: u16 = 0x0000;
pub const FEATURE_SET: u16 = 0x0001;
pub const DEVICE_FW_VERSION: u16 = 0x0003;
pub const DEVICE_NAME: u16 = 0x0005;
pub const DEVICE_FRIENDLY_NAME: u16 = 0x0007;
pub const BATTERY_STATUS: u16 = 0x1000;
pub const BATTERY_VOLTAGE: u16 = 0x1001;
pub const UNIFIED_BATTERY: u16 = 0x1004;
pub const ADJUSTABLE_DPI: u16 = 0x2201;
pub const EXTENDED_ADJUSTABLE_DPI: u16 = 0x2202;
pub const REPORT_RATE: u16 = 0x8060;
pub const EXTENDED_REPORT_RATE: u16 = 0x8061;
pub const COLOR_LED_EFFECTS: u16 = 0x8070;
pub const RGB_EFFECTS: u16 = 0x8071;
pub const ONBOARD_PROFILES: u16 = 0x8100;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Feature {
    pub index: u8,
    pub id: u16,
    pub name: String,
    pub feature_type: u8,
    pub version: u8,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct FeatureTable {
    pub features: Vec<Feature>,
}

impl FeatureTable {
    pub fn discover<I: HidIo>(transport: &mut HidppTransport<I>) -> Result<Self> {
        let feature_set = lookup_feature(transport, FEATURE_SET)?;
        if feature_set.index == 0 {
            return Err(AppError::UnsupportedFeature {
                id: FEATURE_SET,
                name: feature_name(FEATURE_SET),
            });
        }
        let count_response = transport.transact_read(feature_set.index, 0x00, &[])?;
        let count = count_response
            .first()
            .copied()
            .ok_or_else(|| AppError::Other("FEATURE_SET.GetCount returned no count".to_owned()))?;
        let mut features = Vec::with_capacity(count as usize + 1);
        for index in 0..=count {
            let response = transport.transact_read(feature_set.index, 0x10, &[index])?;
            if response.len() < 4 {
                return Err(AppError::Other(format!(
                    "FEATURE_SET.GetFeatureId({index}) returned {} bytes; expected 4",
                    response.len()
                )));
            }
            let id = u16::from_be_bytes([response[0], response[1]]);
            features.push(Feature {
                index,
                id,
                name: feature_name(id).to_owned(),
                feature_type: response[2],
                version: response[3],
            });
        }
        Ok(Self { features })
    }

    pub fn get(&self, id: u16) -> Option<&Feature> {
        self.features.iter().find(|feature| feature.id == id)
    }

    pub fn require(&self, id: u16) -> Result<&Feature> {
        self.get(id).ok_or(AppError::UnsupportedFeature {
            id,
            name: feature_name(id),
        })
    }
}

pub fn lookup_feature<I: HidIo>(transport: &mut HidppTransport<I>, id: u16) -> Result<Feature> {
    let [high, low] = id.to_be_bytes();
    let response = transport.transact_read(ROOT as u8, 0x00, &[high, low])?;
    if response.len() < 3 {
        return Err(AppError::Other(format!(
            "ROOT.GetFeature({id:#06x}) returned fewer than 3 bytes"
        )));
    }
    Ok(Feature {
        index: response[0],
        id,
        name: feature_name(id).to_owned(),
        feature_type: response[1],
        version: response[2],
    })
}

pub fn feature_name(id: u16) -> &'static str {
    match id {
        ROOT => "ROOT",
        FEATURE_SET => "FEATURE_SET",
        0x0002 => "FEATURE_INFO",
        DEVICE_FW_VERSION => "DEVICE_FW_VERSION",
        0x0004 => "DEVICE_UNIT_ID",
        DEVICE_NAME => "DEVICE_NAME",
        DEVICE_FRIENDLY_NAME => "DEVICE_FRIENDLY_NAME",
        0x0020 => "CONFIG_CHANGE",
        BATTERY_STATUS => "BATTERY_STATUS",
        BATTERY_VOLTAGE => "BATTERY_VOLTAGE",
        UNIFIED_BATTERY => "UNIFIED_BATTERY",
        0x1500 => "FORCE_PAIRING",
        0x1802 => "DEVICE_RESET",
        0x1805 => "OOBSTATE",
        0x1806 => "CONFIG_DEVICE_PROPS",
        0x1d4b => "WIRELESS_DEVICE_STATUS",
        0x1e00 => "ENABLE_HIDDEN_FEATURES",
        ADJUSTABLE_DPI => "ADJUSTABLE_DPI",
        EXTENDED_ADJUSTABLE_DPI => "EXTENDED_ADJUSTABLE_DPI",
        0x2250 => "XY_STATS",
        0x2251 => "WHEEL_STATS",
        REPORT_RATE => "REPORT_RATE",
        EXTENDED_REPORT_RATE => "EXTENDED_ADJUSTABLE_REPORT_RATE",
        0x8090 => "MODE_STATUS",
        0x80e0 => "BUNNY_HOPPING",
        COLOR_LED_EFFECTS => "COLOR_LED_EFFECTS",
        RGB_EFFECTS => "RGB_EFFECTS",
        ONBOARD_PROFILES => "ONBOARD_PROFILES",
        0x8110 => "MOUSE_BUTTON_SPY",
        _ => "UNKNOWN",
    }
}
