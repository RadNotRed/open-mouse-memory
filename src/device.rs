use std::collections::HashSet;

use hidapi::HidApi;
use serde::Serialize;

use crate::error::{AppError, Result};
use crate::hid::transport::{HidDeviceIo, HidIo};
use crate::hid::{HidEndpoint, enumerate_logitech};
use crate::hidpp::features::{
    ADJUSTABLE_DPI, BATTERY_STATUS, BATTERY_VOLTAGE, DEVICE_FW_VERSION, DEVICE_NAME, EXTENDED_ADJUSTABLE_DPI,
    EXTENDED_REPORT_RATE, FeatureTable, ONBOARD_PROFILES, REPORT_RATE, UNIFIED_BATTERY,
};
use crate::hidpp::{HidppTransport, ProtocolVersion};
use crate::profile::{ButtonAction, ButtonBinding, MouseButton, Profile};

const LIVE_TIMEOUT_MS: i32 = 750;
const PROBE_TIMEOUT_MS: i32 = 750;

#[derive(Debug, Clone, Serialize)]
pub struct LogicalDevice {
    pub endpoint: HidEndpoint,
    pub device_index: u8,
    pub protocol: ProtocolVersion,
    pub name: String,
    pub features: FeatureTable,
}

impl LogicalDevice {
    pub fn open(&self, api: &HidApi, trace: bool) -> Result<HidppTransport<HidDeviceIo>> {
        let device = self.endpoint.open(api)?;
        Ok(HidppTransport::new(
            HidDeviceIo::new(device, self.endpoint.path.clone()),
            self.device_index,
            LIVE_TIMEOUT_MS,
            trace,
        ))
    }
}

#[derive(Debug, Default)]
pub struct DiscoveryOutcome {
    pub endpoints: Vec<HidEndpoint>,
    pub devices: Vec<LogicalDevice>,
    pub probe_errors: Vec<String>,
    pub permission_denied_paths: Vec<String>,
}

pub fn discover(api: &HidApi, trace: bool) -> DiscoveryOutcome {
    let endpoints = enumerate_logitech(api);
    let mut candidates: Vec<_> = endpoints
        .iter()
        .filter(|endpoint| {
            endpoint.is_probable_hidpp_interface()
                && (endpoint.is_receiver() || endpoint.is_direct_hidpp_device())
        })
        .cloned()
        .collect();
    candidates.sort_by_key(|endpoint| {
        (
            if endpoint.interface_number == 2 { 0 } else { 1 },
            if endpoint.usage_page >= 0xff00 { 0 } else { 1 },
            endpoint.product_id,
        )
    });

    let mut outcome = DiscoveryOutcome {
        endpoints,
        ..DiscoveryOutcome::default()
    };
    let mut seen = HashSet::new();
    for endpoint in candidates {
        let indexes: Vec<u8> = if endpoint.is_receiver() {
            if is_single_device_lightspeed(endpoint.product_id) {
                vec![1]
            } else {
                (1..=6).collect()
            }
        } else {
            vec![0xff]
        };
        let opened = endpoint.open(api);
        let device = match opened {
            Ok(device) => device,
            Err(AppError::PermissionDenied { path }) => {
                outcome
                    .probe_errors
                    .push(AppError::PermissionDenied { path: path.clone() }.to_string());
                outcome.permission_denied_paths.push(path);
                continue;
            }
            Err(error) => {
                outcome.probe_errors.push(error.to_string());
                continue;
            }
        };
        let io = HidDeviceIo::new(device, endpoint.path.clone());
        for (position, index) in indexes.iter().copied().enumerate() {
            let mut transport = HidppTransport::new(io_ref(&io), index, PROBE_TIMEOUT_MS, trace);
            let protocol = match transport.ping() {
                Ok(protocol) if protocol.major >= 2 => protocol,
                Ok(_) => continue,
                Err(AppError::Timeout { .. }) => {
                    if position == 0 && is_single_device_lightspeed(endpoint.product_id) {
                        break;
                    }
                    continue;
                }
                Err(error) => {
                    outcome
                        .probe_errors
                        .push(format!("{} index {index}: {error}", endpoint.path));
                    continue;
                }
            };
            let features = match FeatureTable::discover(&mut transport) {
                Ok(features) => features,
                Err(error) => {
                    outcome
                        .probe_errors
                        .push(format!("{} index {index}: {error}", endpoint.path));
                    continue;
                }
            };
            let name = read_device_name(&mut transport, &features).unwrap_or_else(|_| {
                endpoint
                    .product
                    .clone()
                    .unwrap_or_else(|| format!("Logitech HID++ device {index}"))
            });
            let key = (endpoint.product_id, index, name.clone());
            if seen.insert(key) {
                outcome.devices.push(LogicalDevice {
                    endpoint: endpoint.clone(),
                    device_index: index,
                    protocol,
                    name,
                    features,
                });
            }
        }
    }
    outcome
        .devices
        .sort_by(|a, b| (&a.name, a.device_index).cmp(&(&b.name, b.device_index)));
    outcome
}

// reuse one hid handle while probing receiver indexes
struct BorrowedIo<'a>(&'a HidDeviceIo);

fn io_ref(io: &HidDeviceIo) -> BorrowedIo<'_> {
    BorrowedIo(io)
}

impl HidIo for BorrowedIo<'_> {
    fn write_report(&self, data: &[u8]) -> Result<usize> {
        self.0.write_report(data)
    }

    fn read_report(&self, data: &mut [u8], timeout_ms: i32) -> Result<usize> {
        self.0.read_report(data, timeout_ms)
    }
}

fn is_single_device_lightspeed(product_id: u16) -> bool {
    matches!(
        product_id,
        0xc539 | 0xc53a | 0xc53d | 0xc53f | 0xc541 | 0xc545 | 0xc547 | 0xc54d
    )
}

pub fn select_device<'a>(devices: &'a [LogicalDevice], selector: Option<&str>) -> Result<&'a LogicalDevice> {
    if devices.is_empty() {
        return Err(AppError::NoDevice);
    }
    let Some(selector) = selector else {
        return if devices.len() == 1 {
            Ok(&devices[0])
        } else {
            Err(AppError::AmbiguousDevice("default".to_owned()))
        };
    };
    if let Ok(id) = selector.parse::<usize>() {
        return devices
            .get(id.saturating_sub(1))
            .ok_or_else(|| AppError::DeviceNotFound(selector.to_owned()));
    }
    let needle = selector.to_ascii_lowercase();
    let matches: Vec<_> = devices
        .iter()
        .filter(|device| {
            device.name.to_ascii_lowercase().contains(&needle)
                || device.endpoint.path.to_ascii_lowercase() == needle
                || device
                    .endpoint
                    .serial_number
                    .as_deref()
                    .is_some_and(|serial| serial.eq_ignore_ascii_case(selector))
        })
        .collect();
    match matches.as_slice() {
        [device] => Ok(device),
        [] => Err(AppError::DeviceNotFound(selector.to_owned())),
        _ => Err(AppError::AmbiguousDevice(selector.to_owned())),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceIdentity {
    pub name: String,
    pub hidpp_version: ProtocolVersion,
    pub receiver: ReceiverIdentity,
    pub device_index: u8,
    pub serial: Option<String>,
    pub unit_id: Option<String>,
    pub model_id: Option<String>,
    pub transport_ids: Vec<TransportId>,
    pub profile_format: Option<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReceiverIdentity {
    pub vid: String,
    pub pid: String,
    pub path: String,
    pub serial: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransportId {
    pub transport: String,
    pub id: String,
}

pub fn read_identity<I: HidIo>(
    transport: &mut HidppTransport<I>,
    device: &LogicalDevice,
) -> Result<DeviceIdentity> {
    let mut unit_id = None;
    let mut model_id = None;
    let mut transport_ids = Vec::new();
    if let Some(feature) = device.features.get(DEVICE_FW_VERSION) {
        let response = transport.transact_read(feature.index, 0x00, &[])?;
        if response.len() >= 13 {
            unit_id = Some(hex::encode_upper(&response[1..5]));
            model_id = Some(hex::encode_upper(&response[7..13]));
            let flags = response[6];
            let mut offset = 7;
            for (bit, name) in [
                (0x01, "bluetooth"),
                (0x02, "bluetooth-le"),
                (0x04, "wireless"),
                (0x08, "usb"),
            ] {
                if flags & bit != 0 && offset + 2 <= response.len() {
                    transport_ids.push(TransportId {
                        transport: name.to_owned(),
                        id: hex::encode_upper(&response[offset..offset + 2]),
                    });
                    offset += 2;
                }
            }
        }
    }
    let profile_format = if device.features.get(ONBOARD_PROFILES).is_some() {
        Some(read_onboard_info(transport, &device.features)?.profile_format)
    } else {
        None
    };
    let serial = unit_id.clone().or_else(|| device.endpoint.serial_number.clone());
    Ok(DeviceIdentity {
        name: device.name.clone(),
        hidpp_version: device.protocol,
        receiver: ReceiverIdentity {
            vid: format!("{:04x}", device.endpoint.vendor_id),
            pid: format!("{:04x}", device.endpoint.product_id),
            path: device.endpoint.path.clone(),
            serial: device.endpoint.serial_number.clone(),
        },
        device_index: device.device_index,
        serial,
        unit_id,
        model_id,
        transport_ids,
        profile_format,
    })
}

pub fn read_device_name<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
) -> Result<String> {
    let feature = features.require(DEVICE_NAME)?;
    let response = transport.transact_read(feature.index, 0x00, &[])?;
    let length = response
        .first()
        .copied()
        .ok_or_else(|| AppError::Other("DEVICE_NAME.GetCount returned no length".to_owned()))?
        as usize;
    let mut name = Vec::with_capacity(length);
    while name.len() < length {
        let response = transport.transact_read(feature.index, 0x10, &[name.len() as u8])?;
        let take = (length - name.len()).min(response.len());
        if take == 0 {
            return Err(AppError::Other(
                "DEVICE_NAME returned an empty fragment".to_owned(),
            ));
        }
        name.extend_from_slice(&response[..take]);
    }
    String::from_utf8(name).map_err(|error| AppError::Other(format!("device name is not UTF-8: {error}")))
}

#[derive(Debug, Clone, Serialize)]
pub struct FirmwareInfo {
    pub kind: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub extra_hex: Option<String>,
}

pub fn read_firmware<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
) -> Result<Vec<FirmwareInfo>> {
    let feature = features.require(DEVICE_FW_VERSION)?;
    let response = transport.transact_read(feature.index, 0x00, &[])?;
    let count = *response
        .first()
        .ok_or_else(|| AppError::Other("firmware count is missing".to_owned()))?;
    let mut firmware = Vec::new();
    for index in 0..count {
        let response = transport.transact_read(feature.index, 0x10, &[index])?;
        if response.len() < 2 {
            return Err(AppError::Other(format!("firmware record {index} is truncated")));
        }
        let level = response[0] & 0x0f;
        if level <= 1 && response.len() >= 8 {
            let name = String::from_utf8_lossy(&response[1..4])
                .trim_end_matches('\0')
                .to_owned();
            let build = u16::from_be_bytes([response[6], response[7]]);
            let mut version = format!("{:02X}.{:02X}", response[4], response[5]);
            if build != 0 {
                version.push_str(&format!(".B{build:04X}"));
            }
            let extra = response.get(9..).unwrap_or_default();
            let extra = extra
                .iter()
                .copied()
                .take_while(|byte| *byte != 0)
                .collect::<Vec<_>>();
            firmware.push(FirmwareInfo {
                kind: if level == 0 { "firmware" } else { "bootloader" }.to_owned(),
                name: Some(name),
                version: Some(version),
                extra_hex: (!extra.is_empty()).then(|| hex::encode_upper(extra)),
            });
        } else if level == 2 {
            firmware.push(FirmwareInfo {
                kind: "hardware".to_owned(),
                name: None,
                version: Some(response[1].to_string()),
                extra_hex: None,
            });
        } else {
            firmware.push(FirmwareInfo {
                kind: format!("other-{level}"),
                name: None,
                version: None,
                extra_hex: Some(hex::encode_upper(response)),
            });
        }
    }
    Ok(firmware)
}

#[derive(Debug, Clone, Serialize)]
pub struct BatteryInfo {
    pub source: String,
    pub percentage: Option<u8>,
    pub approximate_level: Option<String>,
    pub next_level: Option<u8>,
    pub status: String,
    pub voltage_mv: Option<u16>,
    pub raw_hex: String,
}

pub fn read_battery<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
) -> Result<BatteryInfo> {
    if let Some(feature) = features.get(UNIFIED_BATTERY) {
        let response = transport.transact_read(feature.index, 0x10, &[])?;
        if response.len() < 4 {
            return Err(AppError::Other(
                "UNIFIED_BATTERY response is truncated".to_owned(),
            ));
        }
        return Ok(BatteryInfo {
            source: feature.name.clone(),
            percentage: (response[0] != 0).then_some(response[0]),
            approximate_level: Some(
                match response[1] {
                    8 => "full",
                    4 => "good",
                    2 => "low",
                    1 => "critical",
                    _ => "empty",
                }
                .to_owned(),
            ),
            next_level: None,
            status: battery_status(response[2]).to_owned(),
            voltage_mv: None,
            raw_hex: hex::encode_upper(response),
        });
    }
    if let Some(feature) = features.get(BATTERY_STATUS) {
        let response = transport.transact_read(feature.index, 0x00, &[])?;
        if response.len() < 3 {
            return Err(AppError::Other("BATTERY_STATUS response is truncated".to_owned()));
        }
        return Ok(BatteryInfo {
            source: feature.name.clone(),
            percentage: (response[0] != 0).then_some(response[0]),
            approximate_level: None,
            next_level: Some(response[1]),
            status: battery_status(response[2]).to_owned(),
            voltage_mv: None,
            raw_hex: hex::encode_upper(response),
        });
    }
    if let Some(feature) = features.get(BATTERY_VOLTAGE) {
        let response = transport.transact_read(feature.index, 0x00, &[])?;
        if response.len() < 3 {
            return Err(AppError::Other(
                "BATTERY_VOLTAGE response is truncated".to_owned(),
            ));
        }
        let voltage = u16::from_be_bytes([response[0], response[1]]);
        return Ok(BatteryInfo {
            source: feature.name.clone(),
            percentage: estimate_battery_percentage(voltage),
            approximate_level: None,
            next_level: None,
            status: if response[2] & 0x80 != 0 {
                "charging"
            } else {
                "discharging"
            }
            .to_owned(),
            voltage_mv: Some(voltage),
            raw_hex: hex::encode_upper(response),
        });
    }
    Err(AppError::UnsupportedFeature {
        id: BATTERY_STATUS,
        name: "BATTERY",
    })
}

fn battery_status(status: u8) -> &'static str {
    match status {
        0 => "discharging",
        1 => "recharging",
        2 => "almost-full",
        3 => "full",
        4 => "slow-recharge",
        5 => "invalid-battery",
        6 => "thermal-error",
        _ => "unknown",
    }
}

fn estimate_battery_percentage(mv: u16) -> Option<u8> {
    const POINTS: [(u16, u8); 11] = [
        (4186, 100),
        (4067, 90),
        (3989, 80),
        (3922, 70),
        (3859, 60),
        (3811, 50),
        (3778, 40),
        (3751, 30),
        (3731, 20),
        (3716, 10),
        (3500, 0),
    ];
    if mv < POINTS.last()?.0 || mv > POINTS.first()?.0 + 200 {
        return None;
    }
    for window in POINTS.windows(2) {
        let (high_mv, high_pct) = window[0];
        let (low_mv, low_pct) = window[1];
        if (low_mv..=high_mv).contains(&mv) {
            let span = (high_mv - low_mv) as u32;
            let position = (mv - low_mv) as u32;
            return Some(low_pct + ((high_pct - low_pct) as u32 * position / span.max(1)) as u8);
        }
    }
    Some(100)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DpiCapabilities {
    pub feature: String,
    pub x_values: Vec<u16>,
    pub y_values: Option<Vec<u16>>,
    pub minimum: u16,
    pub maximum: u16,
    pub step: Option<u16>,
    pub separate_xy: bool,
    pub lift_off_distance: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DpiState {
    pub x: u16,
    pub y: u16,
    pub lift_off_distance: Option<u8>,
}

pub fn dpi_capabilities<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
) -> Result<DpiCapabilities> {
    if let Some(feature) = features.get(EXTENDED_ADJUSTABLE_DPI) {
        let info = transport.transact_read(feature.index, 0x10, &[0])?;
        if info.len() < 3 {
            return Err(AppError::Other(
                "EXTENDED_ADJUSTABLE_DPI sensor info is truncated".to_owned(),
            ));
        }
        let separate_xy = info[2] & 0x01 != 0;
        let lift_off_distance = info[2] & 0x02 != 0;
        let x_values = read_dpi_values(transport, feature.index, 0x20, 0, 3)?;
        let y_values = if separate_xy {
            Some(read_dpi_values(transport, feature.index, 0x20, 1, 3)?)
        } else {
            None
        };
        return dpi_capability_record(
            feature.name.clone(),
            x_values,
            y_values,
            separate_xy,
            lift_off_distance,
        );
    }
    let feature = features.require(ADJUSTABLE_DPI)?;
    let values = read_dpi_values(transport, feature.index, 0x10, 0, 1)?;
    dpi_capability_record(feature.name.clone(), values, None, false, false)
}

fn dpi_capability_record(
    feature: String,
    x_values: Vec<u16>,
    y_values: Option<Vec<u16>>,
    separate_xy: bool,
    lift_off_distance: bool,
) -> Result<DpiCapabilities> {
    let minimum = *x_values
        .first()
        .ok_or_else(|| AppError::Other("device returned an empty DPI list".to_owned()))?;
    let maximum = *x_values.last().unwrap();
    let step = infer_step(&x_values);
    Ok(DpiCapabilities {
        feature,
        x_values,
        y_values,
        minimum,
        maximum,
        step,
        separate_xy,
        lift_off_distance,
    })
}

fn read_dpi_values<I: HidIo>(
    transport: &mut HidppTransport<I>,
    feature_index: u8,
    function: u8,
    direction: u8,
    ignore: usize,
) -> Result<Vec<u16>> {
    let mut encoded = Vec::new();
    for page in 0..=u8::MAX {
        let response = transport.transact_read(feature_index, function, &[0, direction, page])?;
        if response.len() <= ignore {
            return Err(AppError::Other("DPI capability response is truncated".to_owned()));
        }
        encoded.extend_from_slice(&response[ignore..]);
        if encoded.windows(2).any(|pair| pair == [0, 0]) {
            break;
        }
    }
    decode_dpi_list(&encoded)
}

fn decode_dpi_list(bytes: &[u8]) -> Result<Vec<u16>> {
    let mut values: Vec<u16> = Vec::new();
    let mut offset = 0;
    while offset + 1 < bytes.len() {
        let value = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
        offset += 2;
        if value == 0 {
            break;
        }
        if value >> 13 == 0b111 {
            let step = value & 0x1fff;
            if step == 0 || offset + 1 >= bytes.len() || values.is_empty() {
                return Err(AppError::Other("invalid encoded DPI range".to_owned()));
            }
            let maximum = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
            offset += 2;
            let start = values.last().copied().unwrap().saturating_add(step);
            if maximum < start {
                return Err(AppError::Other(
                    "DPI range maximum precedes its minimum".to_owned(),
                ));
            }
            values.extend((start..=maximum).step_by(step as usize));
        } else {
            values.push(value);
        }
    }
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

fn infer_step(values: &[u16]) -> Option<u16> {
    let mut differences = values.windows(2).map(|pair| pair[1] - pair[0]);
    let first = differences.next()?;
    differences.all(|difference| difference == first).then_some(first)
}

pub fn read_dpi<I: HidIo>(transport: &mut HidppTransport<I>, features: &FeatureTable) -> Result<DpiState> {
    if let Some(feature) = features.get(EXTENDED_ADJUSTABLE_DPI) {
        let response = transport.transact_read(feature.index, 0x50, &[])?;
        if response.len() < 10 {
            return Err(AppError::Other(
                "EXTENDED_ADJUSTABLE_DPI current value is truncated".to_owned(),
            ));
        }
        let x = selected_or_default(&response[1..5]);
        let y = selected_or_default(&response[5..9]);
        return Ok(DpiState {
            x,
            y: if y == 0 { x } else { y },
            lift_off_distance: Some(response[9]),
        });
    }
    let feature = features.require(ADJUSTABLE_DPI)?;
    let response = transport.transact_read(feature.index, 0x20, &[0])?;
    if response.len() < 5 {
        return Err(AppError::Other(
            "ADJUSTABLE_DPI current value is truncated".to_owned(),
        ));
    }
    let dpi = selected_or_default(&response[1..5]);
    Ok(DpiState {
        x: dpi,
        y: dpi,
        lift_off_distance: None,
    })
}

fn selected_or_default(bytes: &[u8]) -> u16 {
    let selected = u16::from_be_bytes([bytes[0], bytes[1]]);
    if selected == 0 {
        u16::from_be_bytes([bytes[2], bytes[3]])
    } else {
        selected
    }
}

pub fn set_dpi<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
    x: u16,
    y: Option<u16>,
) -> Result<DpiState> {
    require_host_mode_for_live_write(transport, features, "DPI")?;
    let capabilities = dpi_capabilities(transport, features)?;
    validate_choice("X DPI", x, &capabilities.x_values)?;
    let y = y.unwrap_or(x);
    if capabilities.separate_xy {
        validate_choice(
            "Y DPI",
            y,
            capabilities.y_values.as_deref().unwrap_or(&capabilities.x_values),
        )?;
    } else if y != x {
        return Err(AppError::Validation(
            "device does not advertise separate X/Y DPI".to_owned(),
        ));
    }

    if let Some(feature) = features.get(EXTENDED_ADJUSTABLE_DPI) {
        let current = read_dpi(transport, features)?;
        let lod = current.lift_off_distance.unwrap_or(0);
        let mut payload = vec![0];
        payload.extend_from_slice(&x.to_be_bytes());
        payload.extend_from_slice(&y.to_be_bytes());
        payload.push(lod);
        transport.transact(feature.index, 0x60, &payload)?;
    } else {
        let feature = features.require(ADJUSTABLE_DPI)?;
        let [high, low] = x.to_be_bytes();
        transport.transact(feature.index, 0x30, &[0, high, low])?;
    }
    let actual = read_dpi(transport, features)?;
    if actual.x != x || (capabilities.separate_xy && actual.y != y) {
        return Err(AppError::Verification(format!(
            "requested {x}x{y} DPI but device reports {}x{}; disable onboard mode first if a profile controls DPI",
            actual.x, actual.y
        )));
    }
    Ok(actual)
}

fn validate_choice(label: &str, value: u16, values: &[u16]) -> Result<()> {
    if values.contains(&value) {
        Ok(())
    } else {
        let minimum = values.first().copied().unwrap_or_default();
        let maximum = values.last().copied().unwrap_or_default();
        Err(AppError::Validation(format!(
            "{label} {value} is unsupported; device range is {minimum}-{maximum} and values are not silently clamped"
        )))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RateCapabilities {
    pub feature: String,
    pub rates_hz: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RateState {
    pub hz: u32,
    pub interval_microseconds: u32,
}

const EXTENDED_RATES: [u32; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];

pub fn rate_capabilities<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
) -> Result<RateCapabilities> {
    if let Some(feature) = features.get(EXTENDED_REPORT_RATE) {
        let response = transport.transact_read(feature.index, 0x10, &[])?;
        if response.len() < 2 {
            return Err(AppError::Other(
                "extended report-rate capabilities are truncated".to_owned(),
            ));
        }
        let flags = u16::from_be_bytes([response[0], response[1]]);
        let rates_hz = EXTENDED_RATES
            .iter()
            .enumerate()
            .filter_map(|(index, rate)| (flags & (1 << index) != 0).then_some(*rate))
            .collect();
        return Ok(RateCapabilities {
            feature: feature.name.clone(),
            rates_hz,
        });
    }
    let feature = features.require(REPORT_RATE)?;
    let response = transport.transact_read(feature.index, 0x00, &[])?;
    let flags = *response
        .first()
        .ok_or_else(|| AppError::Other("report-rate capabilities are empty".to_owned()))?;
    let rates_hz = (1..=8)
        .filter(|interval| flags & (1 << (interval - 1)) != 0)
        .map(|interval| 1000 / interval as u32)
        .collect();
    Ok(RateCapabilities {
        feature: feature.name.clone(),
        rates_hz,
    })
}

pub fn read_rate<I: HidIo>(transport: &mut HidppTransport<I>, features: &FeatureTable) -> Result<RateState> {
    if let Some(feature) = features.get(EXTENDED_REPORT_RATE) {
        let connection = extended_rate_connection(transport.device_index());
        let response = transport.transact_read(feature.index, 0x20, &[connection, 0, 0])?;
        let code = *response
            .first()
            .ok_or_else(|| AppError::Other("extended report rate is empty".to_owned()))?;
        let hz = EXTENDED_RATES
            .get(code as usize)
            .copied()
            .ok_or_else(|| AppError::Other(format!("unknown extended report-rate code {code}")))?;
        return Ok(rate_state(hz));
    }
    let feature = features.require(REPORT_RATE)?;
    let response = transport.transact_read(feature.index, 0x10, &[])?;
    let interval = *response
        .first()
        .ok_or_else(|| AppError::Other("report rate is empty".to_owned()))?;
    if !(1..=8).contains(&interval) {
        return Err(AppError::Other(format!(
            "unknown report-rate interval {interval} ms"
        )));
    }
    Ok(RateState {
        hz: 1000 / interval as u32,
        interval_microseconds: interval as u32 * 1000,
    })
}

pub fn set_rate<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
    hz: u32,
) -> Result<RateState> {
    require_host_mode_for_live_write(transport, features, "report rate")?;
    let capabilities = rate_capabilities(transport, features)?;
    if !capabilities.rates_hz.contains(&hz) {
        return Err(AppError::Validation(format!(
            "{hz} Hz is unsupported; supported values: {}",
            capabilities
                .rates_hz
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if let Some(feature) = features.get(EXTENDED_REPORT_RATE) {
        let code = EXTENDED_RATES.iter().position(|rate| *rate == hz).unwrap() as u8;
        transport.transact(feature.index, 0x30, &[code, 0, 0])?;
    } else {
        let feature = features.require(REPORT_RATE)?;
        let interval = (1000 / hz) as u8;
        transport.transact(feature.index, 0x20, &[interval])?;
    }
    let actual = read_rate(transport, features)?;
    if actual.hz != hz {
        return Err(AppError::Verification(format!(
            "requested {hz} Hz but device reports {} Hz; disable onboard mode first if a profile controls report rate",
            actual.hz
        )));
    }
    Ok(actual)
}

fn rate_state(hz: u32) -> RateState {
    RateState {
        hz,
        interval_microseconds: 1_000_000 / hz,
    }
}

fn extended_rate_connection(device_index: u8) -> u8 {
    if device_index == 0xff { 0 } else { 1 }
}

fn require_host_mode_for_live_write<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
    setting: &str,
) -> Result<()> {
    if features.get(ONBOARD_PROFILES).is_some() {
        let status = read_onboard_status(transport, features)?;
        if status.mode_code != 2 {
            return Err(AppError::Validation(format!(
                "an onboard profile currently controls {setting}; run 'open-mouse-memory onboard disable' explicitly first"
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OnboardStatus {
    pub mode: String,
    pub mode_code: u8,
    pub active_sector: Option<u16>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OnboardInfo {
    pub memory_model: u8,
    pub profile_format: u8,
    pub macro_format: u8,
    pub profile_count: u8,
    pub rom_profile_count: u8,
    pub button_count: u8,
    pub sector_count: u8,
    pub sector_size: u16,
    pub mechanical_layout: u8,
    pub various_info: u8,
    pub reserved_hex: String,
    pub raw_hex: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OnboardProfileHeader {
    pub slot: u8,
    pub sector: u16,
    pub enabled: bool,
    pub enabled_code: u8,
    pub flags: u8,
    pub raw_hex: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OnboardSector {
    pub sector: u16,
    pub size: u16,
    pub stored_crc: u16,
    pub computed_crc: u16,
    pub crc_valid: bool,
    pub data_hex: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OnboardMemoryBackup {
    pub schema_version: u8,
    pub device: DeviceIdentity,
    pub info: OnboardInfo,
    pub status: OnboardStatus,
    pub profiles: Vec<OnboardProfileHeader>,
    pub sectors: Vec<OnboardSector>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OnboardProfile {
    pub slot: u8,
    pub sector: u16,
    pub enabled: bool,
    pub active: bool,
    pub crc_valid: bool,
    pub profile: Profile,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OnboardProfileWrite {
    pub slot: u8,
    pub sector: u16,
    pub profile_written: bool,
    pub directory_written: bool,
    pub crc_valid: bool,
}

pub fn read_onboard_status<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
) -> Result<OnboardStatus> {
    let feature = features.require(ONBOARD_PROFILES)?;
    let response = transport.transact_read(feature.index, 0x20, &[])?;
    let mode = *response
        .first()
        .ok_or_else(|| AppError::Other("onboard mode response is empty".to_owned()))?;
    let active_sector = if mode == 1 {
        let response = transport.transact_read(feature.index, 0x40, &[])?;
        (response.len() >= 2).then(|| u16::from_be_bytes([response[0], response[1]]))
    } else {
        None
    };
    Ok(OnboardStatus {
        mode: match mode {
            1 => "onboard",
            2 => "host",
            _ => "unknown",
        }
        .to_owned(),
        mode_code: mode,
        active_sector,
    })
}

pub fn set_onboard_mode<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
    onboard: bool,
) -> Result<OnboardStatus> {
    let feature = features.require(ONBOARD_PROFILES)?;
    let requested = if onboard { 1 } else { 2 };
    transport.transact(feature.index, 0x10, &[requested])?;
    let actual = read_onboard_status(transport, features)?;
    if actual.mode_code != requested {
        return Err(AppError::Verification(format!(
            "requested mode {requested:#04x} but device reports {:#04x}",
            actual.mode_code
        )));
    }
    Ok(actual)
}

pub fn read_onboard_info<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
) -> Result<OnboardInfo> {
    let feature = features.require(ONBOARD_PROFILES)?;
    let response = transport.transact_read(feature.index, 0x00, &[])?;
    if response.len() < 16 {
        return Err(AppError::Other(format!(
            "ONBOARD_PROFILES.GetInfo returned {} bytes; expected 16",
            response.len()
        )));
    }
    Ok(OnboardInfo {
        memory_model: response[0],
        profile_format: response[1],
        macro_format: response[2],
        profile_count: response[3],
        rom_profile_count: response[4],
        button_count: response[5],
        sector_count: response[6],
        sector_size: u16::from_be_bytes([response[7], response[8]]),
        mechanical_layout: response[9],
        various_info: response[10],
        reserved_hex: hex::encode_upper(&response[11..16]),
        raw_hex: hex::encode_upper(&response[..16]),
    })
}

pub fn read_onboard_profile_headers<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
) -> Result<Vec<OnboardProfileHeader>> {
    let info = read_onboard_info(transport, features)?;
    let feature = features.require(ONBOARD_PROFILES)?;
    let mut bytes = Vec::new();
    let required = info.profile_count as usize * 4 + 4;
    for offset in (0..required).step_by(16) {
        let response = read_onboard_chunk(transport, feature.index, 0, offset as u16)?;
        bytes.extend_from_slice(&response);
    }
    Ok(decode_onboard_profile_headers(&bytes, info.profile_count))
}

pub fn read_onboard_sector<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
    sector: u16,
    size: u16,
) -> Result<OnboardSector> {
    if size < 16 {
        return Err(AppError::Validation(
            "onboard sector size must be at least 16 bytes".to_owned(),
        ));
    }
    let feature = features.require(ONBOARD_PROFILES)?;
    let size = size as usize;
    let mut bytes = vec![0; size];
    let mut offsets = (0..=size - 16).step_by(16).collect::<Vec<_>>();
    let final_offset = size - 16;
    if offsets.last().copied() != Some(final_offset) {
        offsets.push(final_offset);
    }
    for offset in offsets {
        let response = read_onboard_chunk(transport, feature.index, sector, offset as u16)?;
        bytes[offset..offset + 16].copy_from_slice(&response[..16]);
    }
    let crc_offset = bytes.len() - 2;
    let stored_crc = u16::from_be_bytes([bytes[crc_offset], bytes[crc_offset + 1]]);
    let computed_crc = onboard_crc16(&bytes[..crc_offset]);
    Ok(OnboardSector {
        sector,
        size: size as u16,
        stored_crc,
        computed_crc,
        crc_valid: stored_crc == computed_crc,
        data_hex: hex::encode_upper(bytes),
    })
}

pub fn read_onboard_backup<I: HidIo>(
    transport: &mut HidppTransport<I>,
    device: &LogicalDevice,
) -> Result<OnboardMemoryBackup> {
    let identity = read_identity(transport, device)?;
    let info = read_onboard_info(transport, &device.features)?;
    let status = read_onboard_status(transport, &device.features)?;
    let profiles = read_onboard_profile_headers(transport, &device.features)?;
    let mut sectors = Vec::with_capacity(info.sector_count as usize);
    for sector in 0..info.sector_count as u16 {
        sectors.push(read_onboard_sector(
            transport,
            &device.features,
            sector,
            info.sector_size,
        )?);
    }
    Ok(OnboardMemoryBackup {
        schema_version: 1,
        device: identity,
        info,
        status,
        profiles,
        sectors,
    })
}

pub fn read_onboard_profiles<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
) -> Result<Vec<OnboardProfile>> {
    let info = read_onboard_info(transport, features)?;
    validate_format_07(&info)?;
    let status = read_onboard_status(transport, features)?;
    let current_dpi = if status.mode_code == 1 {
        read_onboard_current_dpi_index(transport, features).ok()
    } else {
        None
    };
    let headers = read_onboard_profile_headers(transport, features)?;
    let mut profiles = Vec::with_capacity(headers.len());
    for header in headers {
        let sector = read_onboard_sector(transport, features, header.sector, info.sector_size)?;
        if !sector.crc_valid {
            return Err(AppError::Verification(format!(
                "onboard profile slot {} has an invalid checksum",
                header.slot
            )));
        }
        let is_active = status.active_sector == Some(header.sector);
        profiles.push(decode_format_07_profile(
            &header,
            &sector,
            is_active,
            is_active.then_some(current_dpi).flatten(),
        )?);
    }
    Ok(profiles)
}

pub fn write_onboard_profile<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
    slot: u8,
    profile: &Profile,
) -> Result<OnboardProfileWrite> {
    validate_profile_for_onboard(profile)?;
    let info = read_onboard_info(transport, features)?;
    validate_format_07(&info)?;
    let headers = read_onboard_profile_headers(transport, features)?;
    let header = headers
        .iter()
        .find(|header| header.slot == slot)
        .ok_or_else(|| AppError::Validation(format!("onboard profile slot {slot} does not exist")))?;
    if header.sector >= info.sector_count as u16 {
        return Err(AppError::Verification(format!(
            "onboard profile slot {slot} points outside memory at sector {:#06x}",
            header.sector
        )));
    }

    let original_profile = read_onboard_sector(transport, features, header.sector, info.sector_size)?;
    let original_directory = read_onboard_sector(transport, features, 0, info.sector_size)?;
    if !original_profile.crc_valid || !original_directory.crc_valid {
        return Err(AppError::Verification(
            "onboard memory has an invalid checksum and will not be written".to_owned(),
        ));
    }

    let original_profile_bytes = onboard_sector_bytes(&original_profile)?;
    let original_directory_bytes = onboard_sector_bytes(&original_directory)?;
    let mut profile_bytes = original_profile_bytes.clone();
    encode_format_07_profile(&mut profile_bytes, profile)?;
    set_onboard_crc(&mut profile_bytes)?;
    let profile_written = profile_bytes != original_profile_bytes;

    if profile_written {
        if let Err(error) = write_onboard_sector_bytes(transport, features, header.sector, &profile_bytes) {
            let rollback =
                write_onboard_sector_bytes(transport, features, header.sector, &original_profile_bytes);
            return Err(write_failure_with_rollback(error, rollback));
        }
    }

    let mut directory_bytes = original_directory_bytes.clone();
    let directory_offset = usize::from(slot.saturating_sub(1)) * 4;
    if directory_offset + 4 > directory_bytes.len() - 2 {
        if profile_written {
            let _ = write_onboard_sector_bytes(transport, features, header.sector, &original_profile_bytes);
        }
        return Err(AppError::Verification(format!(
            "onboard profile slot {slot} has no directory record"
        )));
    }
    directory_bytes[directory_offset..directory_offset + 2].copy_from_slice(&header.sector.to_be_bytes());
    directory_bytes[directory_offset + 2] = 1;
    directory_bytes[directory_offset + 3] = 0;
    set_onboard_crc(&mut directory_bytes)?;
    let directory_written = directory_bytes != original_directory_bytes;

    if directory_written {
        if let Err(error) = write_onboard_sector_bytes(transport, features, 0, &directory_bytes) {
            let directory_rollback =
                write_onboard_sector_bytes(transport, features, 0, &original_directory_bytes);
            let profile_rollback = if profile_written {
                write_onboard_sector_bytes(transport, features, header.sector, &original_profile_bytes)
            } else {
                Ok(())
            };
            let rollback = directory_rollback.and(profile_rollback);
            return Err(write_failure_with_rollback(error, rollback));
        }
    }

    let verified = read_onboard_sector(transport, features, header.sector, info.sector_size)?;
    if verified.data_hex != hex::encode_upper(&profile_bytes) || !verified.crc_valid {
        let directory_rollback = if directory_written {
            write_onboard_sector_bytes(transport, features, 0, &original_directory_bytes)
        } else {
            Ok(())
        };
        let profile_rollback = if profile_written {
            write_onboard_sector_bytes(transport, features, header.sector, &original_profile_bytes)
        } else {
            Ok(())
        };
        let rollback = directory_rollback.and(profile_rollback);
        return Err(write_failure_with_rollback(
            AppError::Verification(format!("onboard profile slot {slot} did not match after writing")),
            rollback,
        ));
    }

    Ok(OnboardProfileWrite {
        slot,
        sector: header.sector,
        profile_written,
        directory_written,
        crc_valid: verified.crc_valid,
    })
}

pub fn set_onboard_active_profile<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
    slot: u8,
) -> Result<OnboardStatus> {
    let headers = read_onboard_profile_headers(transport, features)?;
    let header = headers
        .iter()
        .find(|header| header.slot == slot && header.enabled)
        .ok_or_else(|| AppError::Validation(format!("onboard profile slot {slot} is not enabled")))?;
    let feature = features.require(ONBOARD_PROFILES)?;
    transport.transact(feature.index, 0x30, &[0, slot])?;
    let status = read_onboard_status(transport, features)?;
    if status.active_sector != Some(header.sector) {
        return Err(AppError::Verification(format!(
            "requested onboard profile slot {slot} but active sector is {:?}",
            status.active_sector
        )));
    }
    Ok(status)
}

pub fn set_onboard_current_dpi_index<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
    index: u8,
) -> Result<u8> {
    if index >= 5 {
        return Err(AppError::Validation(format!(
            "onboard dpi index {index} is outside the five available stages"
        )));
    }
    let feature = features.require(ONBOARD_PROFILES)?;
    transport.transact(feature.index, 0xc0, &[index])?;
    let actual = read_onboard_current_dpi_index(transport, features)?;
    if actual != index {
        return Err(AppError::Verification(format!(
            "requested onboard dpi index {index} but the device reports {actual}"
        )));
    }
    Ok(actual)
}

pub fn verify_onboard_sector_write<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
    sector: u16,
) -> Result<OnboardSector> {
    let info = read_onboard_info(transport, features)?;
    validate_format_07(&info)?;
    if sector >= info.sector_count as u16 {
        return Err(AppError::Validation(format!(
            "sector {sector:#06x} is outside onboard memory"
        )));
    }
    let original = read_onboard_sector(transport, features, sector, info.sector_size)?;
    if !original.crc_valid {
        return Err(AppError::Verification(format!(
            "onboard sector {sector:#06x} has an invalid checksum"
        )));
    }
    let bytes = onboard_sector_bytes(&original)?;
    if let Err(error) = write_onboard_sector_bytes(transport, features, sector, &bytes) {
        let rollback = write_onboard_sector_bytes(transport, features, sector, &bytes);
        return Err(write_failure_with_rollback(error, rollback));
    }
    let verified = read_onboard_sector(transport, features, sector, info.sector_size)?;
    if verified != original {
        let rollback = write_onboard_sector_bytes(transport, features, sector, &bytes);
        return Err(write_failure_with_rollback(
            AppError::Verification(format!(
                "onboard sector {sector:#06x} changed during write verification"
            )),
            rollback,
        ));
    }
    Ok(verified)
}

fn read_onboard_current_dpi_index<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
) -> Result<u8> {
    let feature = features.require(ONBOARD_PROFILES)?;
    let response = transport.transact_read(feature.index, 0xb0, &[])?;
    response
        .first()
        .copied()
        .ok_or_else(|| AppError::Other("current onboard dpi response is empty".to_owned()))
}

fn validate_format_07(info: &OnboardInfo) -> Result<()> {
    if info.memory_model != 1 || info.profile_format != 7 || info.sector_size < 70 {
        return Err(AppError::Validation(format!(
            "onboard profile format 0x{:02x} with memory model 0x{:02x} is not supported for profile editing",
            info.profile_format, info.memory_model
        )));
    }
    Ok(())
}

fn decode_format_07_profile(
    header: &OnboardProfileHeader,
    sector: &OnboardSector,
    active: bool,
    current_dpi: Option<u8>,
) -> Result<OnboardProfile> {
    let bytes = onboard_sector_bytes(sector)?;
    let report_rate = EXTENDED_RATES
        .get(bytes[0] as usize)
        .copied()
        .ok_or_else(|| AppError::Other(format!("unknown onboard report-rate code {}", bytes[0])))?;
    let mut indexed_dpi = Vec::new();
    for index in 0..5 {
        let offset = 3 + index * 5;
        let x = u16::from_le_bytes([bytes[offset + 1], bytes[offset + 2]]);
        let y = u16::from_le_bytes([bytes[offset + 3], bytes[offset + 4]]);
        if x != y {
            return Err(AppError::Verification(format!(
                "onboard profile slot {} has separate x and y dpi values",
                header.slot
            )));
        }
        if x != 0 && x != u16::MAX {
            indexed_dpi.push((index, x));
        }
    }
    if indexed_dpi.is_empty() {
        return Err(AppError::Verification(format!(
            "onboard profile slot {} contains no dpi points",
            header.slot
        )));
    }
    let requested_active = current_dpi.unwrap_or(bytes[1]) as usize;
    let active_dpi = indexed_dpi
        .iter()
        .position(|(index, _)| *index == requested_active)
        .unwrap_or(0);
    let shift_dpi = (bytes[2] != u8::MAX)
        .then(|| {
            indexed_dpi
                .iter()
                .position(|(index, _)| *index == bytes[2] as usize)
        })
        .flatten();
    let dpi_points = indexed_dpi.into_iter().map(|(_, dpi)| dpi).collect();
    let bindings = MouseButton::ALL
        .into_iter()
        .enumerate()
        .map(|(index, button)| {
            let offset = 48 + index * 4;
            ButtonBinding {
                button,
                action: decode_format_07_button(&bytes[offset..offset + 4]),
            }
        })
        .collect();
    Ok(OnboardProfile {
        slot: header.slot,
        sector: header.sector,
        enabled: header.enabled,
        active,
        crc_valid: sector.crc_valid,
        profile: Profile {
            name: format!("Onboard Slot {}", header.slot),
            dpi_points,
            active_dpi,
            shift_dpi,
            report_rate,
            bindings,
        },
    })
}

fn decode_format_07_button(bytes: &[u8]) -> ButtonAction {
    match bytes {
        [0x80, 0x01, 0x00, 0x01] => ButtonAction::PrimaryClick,
        [0x80, 0x01, 0x00, 0x02] => ButtonAction::SecondaryClick,
        [0x80, 0x01, 0x00, 0x04] => ButtonAction::MiddleClick,
        [0x80, 0x01, 0x00, 0x08] => ButtonAction::Back,
        [0x80, 0x01, 0x00, 0x10] => ButtonAction::Forward,
        [0x90, 0x03, _, _] => ButtonAction::DpiUp,
        [0x90, 0x04, _, _] => ButtonAction::DpiDown,
        [0x90, 0x05, _, _] => ButtonAction::DpiCycle,
        [0x90, 0x07, _, _] => ButtonAction::DpiShift,
        [0xff, _, _, _] => ButtonAction::Disabled,
        _ => ButtonAction::OnboardRaw(hex::encode_upper(bytes)),
    }
}

fn validate_profile_for_onboard(profile: &Profile) -> Result<()> {
    if profile.dpi_points.is_empty() || profile.dpi_points.len() > 5 {
        return Err(AppError::Validation(
            "onboard profiles require one to five dpi points".to_owned(),
        ));
    }
    if profile.active_dpi >= profile.dpi_points.len()
        || profile
            .shift_dpi
            .is_some_and(|index| index >= profile.dpi_points.len())
    {
        return Err(AppError::Validation("onboard dpi indexes are invalid".to_owned()));
    }
    if profile.dpi_points.iter().any(|dpi| *dpi == 0 || *dpi == u16::MAX) {
        return Err(AppError::Validation(
            "onboard dpi points contain an invalid value".to_owned(),
        ));
    }
    if !EXTENDED_RATES.contains(&profile.report_rate) {
        return Err(AppError::Validation(format!(
            "{} Hz is not supported by format 0x07 onboard profiles",
            profile.report_rate
        )));
    }
    Ok(())
}

fn encode_format_07_profile(bytes: &mut [u8], profile: &Profile) -> Result<()> {
    if bytes.len() < 70 {
        return Err(AppError::Verification(
            "onboard profile sector is truncated".to_owned(),
        ));
    }
    bytes[0] = EXTENDED_RATES
        .iter()
        .position(|rate| *rate == profile.report_rate)
        .map(|index| index as u8)
        .ok_or_else(|| AppError::Validation(format!("unsupported report rate {}", profile.report_rate)))?;
    bytes[1] = profile.active_dpi as u8;
    bytes[2] = profile.shift_dpi.map(|index| index as u8).unwrap_or(u8::MAX);
    for index in 0..5 {
        let offset = 3 + index * 5;
        let dpi = profile.dpi_points.get(index).copied().unwrap_or(0);
        let dpi = dpi.to_le_bytes();
        bytes[offset + 1..offset + 3].copy_from_slice(&dpi);
        bytes[offset + 3..offset + 5].copy_from_slice(&dpi);
    }
    for (index, button) in MouseButton::ALL.into_iter().enumerate() {
        let action = profile
            .bindings
            .iter()
            .find(|binding| binding.button == button)
            .map(|binding| &binding.action)
            .unwrap_or(&ButtonAction::Disabled);
        if let Some(encoded) = encode_format_07_button(action)? {
            let offset = 48 + index * 4;
            bytes[offset..offset + 4].copy_from_slice(&encoded);
        }
    }
    Ok(())
}

fn encode_format_07_button(action: &ButtonAction) -> Result<Option<[u8; 4]>> {
    let encoded = match action {
        ButtonAction::PrimaryClick => [0x80, 0x01, 0x00, 0x01],
        ButtonAction::SecondaryClick => [0x80, 0x01, 0x00, 0x02],
        ButtonAction::MiddleClick => [0x80, 0x01, 0x00, 0x04],
        ButtonAction::Back => [0x80, 0x01, 0x00, 0x08],
        ButtonAction::Forward => [0x80, 0x01, 0x00, 0x10],
        ButtonAction::DpiUp => [0x90, 0x03, 0x00, 0x00],
        ButtonAction::DpiDown => [0x90, 0x04, 0x00, 0x00],
        ButtonAction::DpiCycle => [0x90, 0x05, 0x00, 0x00],
        ButtonAction::DpiShift => [0x90, 0x07, 0x00, 0x00],
        ButtonAction::Disabled => [0xff; 4],
        ButtonAction::OnboardRaw(_) => return Ok(None),
        ButtonAction::Keystroke(_) => {
            return Err(AppError::Validation(
                "free-form keys cannot be stored onboard yet".to_owned(),
            ));
        }
        ButtonAction::Macro(_) => {
            return Err(AppError::Validation(
                "macros cannot be stored onboard yet".to_owned(),
            ));
        }
    };
    Ok(Some(encoded))
}

fn onboard_sector_bytes(sector: &OnboardSector) -> Result<Vec<u8>> {
    let bytes = hex::decode(&sector.data_hex)
        .map_err(|error| AppError::Other(format!("cannot decode onboard sector: {error}")))?;
    if bytes.len() != sector.size as usize {
        return Err(AppError::Verification(format!(
            "onboard sector {:#06x} has {} bytes instead of {}",
            sector.sector,
            bytes.len(),
            sector.size
        )));
    }
    Ok(bytes)
}

fn set_onboard_crc(bytes: &mut [u8]) -> Result<()> {
    if bytes.len() < 2 {
        return Err(AppError::Validation("onboard sector is too small".to_owned()));
    }
    let crc_offset = bytes.len() - 2;
    let crc = onboard_crc16(&bytes[..crc_offset]).to_be_bytes();
    bytes[crc_offset..].copy_from_slice(&crc);
    Ok(())
}

fn write_onboard_sector_bytes<I: HidIo>(
    transport: &mut HidppTransport<I>,
    features: &FeatureTable,
    sector: u16,
    bytes: &[u8],
) -> Result<()> {
    if bytes.len() < 16 || bytes.len() > u16::MAX as usize {
        return Err(AppError::Validation("onboard sector size is invalid".to_owned()));
    }
    let mut data = bytes.to_vec();
    set_onboard_crc(&mut data)?;
    let feature = features.require(ONBOARD_PROFILES)?;
    let mut start = Vec::with_capacity(6);
    start.extend_from_slice(&sector.to_be_bytes());
    start.extend_from_slice(&0u16.to_be_bytes());
    start.extend_from_slice(&(data.len() as u16).to_be_bytes());
    transport.transact(feature.index, 0x60, &start)?;
    for chunk in data.chunks(16) {
        let mut payload = [0xff; 16];
        payload[..chunk.len()].copy_from_slice(chunk);
        if let Err(error) = transport.transact(feature.index, 0x70, &payload) {
            let _ = transport.transact(feature.index, 0x80, &[]);
            return Err(error);
        }
    }
    transport.transact(feature.index, 0x80, &[])?;
    let verified = read_onboard_sector(transport, features, sector, data.len() as u16)?;
    if !verified.crc_valid || verified.data_hex != hex::encode_upper(data) {
        return Err(AppError::Verification(format!(
            "onboard sector {sector:#06x} failed readback verification"
        )));
    }
    Ok(())
}

fn write_failure_with_rollback(error: AppError, rollback: Result<()>) -> AppError {
    match rollback {
        Ok(()) => AppError::Other(format!("{error}; the original onboard data was restored")),
        Err(rollback_error) => AppError::Other(format!(
            "{error}; restoring the original onboard data also failed: {rollback_error}"
        )),
    }
}

fn read_onboard_chunk<I: HidIo>(
    transport: &mut HidppTransport<I>,
    feature_index: u8,
    sector: u16,
    offset: u16,
) -> Result<Vec<u8>> {
    let [sector_high, sector_low] = sector.to_be_bytes();
    let [offset_high, offset_low] = offset.to_be_bytes();
    let response = transport.transact_read(
        feature_index,
        0x50,
        &[sector_high, sector_low, offset_high, offset_low],
    )?;
    if response.len() < 16 {
        return Err(AppError::Other(format!(
            "onboard sector {sector:#06x} offset {offset:#06x} returned {} bytes; expected 16",
            response.len()
        )));
    }
    Ok(response)
}

fn decode_onboard_profile_headers(bytes: &[u8], count: u8) -> Vec<OnboardProfileHeader> {
    bytes
        .chunks_exact(4)
        .take(count as usize)
        .enumerate()
        .take_while(|(_, record)| record[0..2] != [0xff, 0xff])
        .map(|(index, record)| {
            let enabled_code = record[2];
            OnboardProfileHeader {
                slot: index as u8 + 1,
                sector: u16::from_be_bytes([record[0], record[1]]),
                enabled: enabled_code == 1,
                enabled_code,
                flags: record[3],
                raw_hex: hex::encode_upper(record),
            }
        })
        .collect()
}

fn onboard_crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0xffffu16;
    for byte in bytes {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_compact_dpi_range() {
        let values = decode_dpi_list(&[0x00, 0x64, 0xe0, 0x32, 0x01, 0x2c, 0, 0]).unwrap();
        assert_eq!(values, [100, 150, 200, 250, 300]);
    }

    #[test]
    fn rejects_broken_dpi_range() {
        assert!(decode_dpi_list(&[0xe0, 0x32, 0x01, 0x2c, 0, 0]).is_err());
    }

    #[test]
    fn decodes_onboard_profile_directory() {
        let headers = decode_onboard_profile_headers(
            &[
                0x00, 0x01, 0x01, 0x00, 0x00, 0x02, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff,
            ],
            5,
        );
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].sector, 1);
        assert!(headers[0].enabled);
        assert_eq!(headers[1].flags, 0xff);
    }

    #[test]
    fn calculates_onboard_crc16() {
        assert_eq!(onboard_crc16(b"123456789"), 0x29b1);
    }

    #[test]
    fn decodes_format_07_profile() {
        let mut bytes = vec![0xff; 70];
        bytes[..68].copy_from_slice(&[
            0x04, 0x03, 0x00, 0x00, 0x20, 0x03, 0x20, 0x03, 0x02, 0xb0, 0x04, 0xb0, 0x04, 0x02, 0x40, 0x06,
            0x40, 0x06, 0x02, 0x60, 0x09, 0x60, 0x09, 0x02, 0x80, 0x0c, 0x80, 0x0c, 0x02, 0x00, 0x00, 0x00,
            0x00, 0xff, 0x00, 0xff, 0xff, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x3c, 0x00, 0x2c, 0x01,
            0x80, 0x01, 0x00, 0x01, 0x80, 0x01, 0x00, 0x02, 0x80, 0x01, 0x00, 0x04, 0x80, 0x01, 0x00, 0x08,
            0x80, 0x01, 0x00, 0x10,
        ]);
        set_onboard_crc(&mut bytes).unwrap();
        let stored_crc = u16::from_be_bytes([bytes[68], bytes[69]]);
        let sector = OnboardSector {
            sector: 1,
            size: 70,
            stored_crc,
            computed_crc: stored_crc,
            crc_valid: true,
            data_hex: hex::encode_upper(bytes),
        };
        let header = OnboardProfileHeader {
            slot: 1,
            sector: 1,
            enabled: true,
            enabled_code: 1,
            flags: 0,
            raw_hex: "00010100".to_owned(),
        };
        let decoded = decode_format_07_profile(&header, &sector, true, Some(0)).unwrap();
        assert_eq!(decoded.profile.report_rate, 2000);
        assert_eq!(decoded.profile.dpi_points, [800, 1200, 1600, 2400, 3200]);
        assert_eq!(decoded.profile.active_dpi, 0);
        assert_eq!(decoded.profile.shift_dpi, Some(0));
        assert_eq!(
            decoded.profile.action(MouseButton::Forward),
            &ButtonAction::Forward
        );
    }

    #[test]
    fn encodes_format_07_without_touching_unknown_bytes() {
        let mut bytes = vec![0xa5; 255];
        let profile = Profile::default();
        encode_format_07_profile(&mut bytes, &profile).unwrap();
        assert_eq!(bytes[0], 3);
        assert_eq!(&bytes[4..8], &[0x90, 0x01, 0x90, 0x01]);
        assert_eq!(&bytes[48..52], &[0x80, 0x01, 0x00, 0x01]);
        assert_eq!(bytes[100], 0xa5);
    }

    #[test]
    fn maps_extended_rates() {
        assert_eq!(
            rate_state(8000),
            RateState {
                hz: 8000,
                interval_microseconds: 125
            }
        );
    }

    #[test]
    fn selects_extended_rate_connection() {
        assert_eq!(extended_rate_connection(0xff), 0);
        assert_eq!(extended_rate_connection(1), 1);
    }
}
