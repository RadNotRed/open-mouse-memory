use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReportDescriptorSummary {
    pub byte_length: usize,
    pub report_ids: Vec<u8>,
    pub input_bits: BTreeMap<u8, usize>,
    pub output_bits: BTreeMap<u8, usize>,
    pub feature_bits: BTreeMap<u8, usize>,
    pub usage_pages: Vec<u32>,
    pub raw_hex: String,
}

#[derive(Clone, Copy)]
enum ReportKind {
    Input,
    Output,
    Feature,
}

pub fn inspect(bytes: &[u8]) -> ReportDescriptorSummary {
    let mut offset = 0;
    let mut report_id = 0u8;
    let mut report_size = 0usize;
    let mut report_count = 0usize;
    let mut ids = Vec::new();
    let mut usage_pages = Vec::new();
    let mut input_bits = BTreeMap::new();
    let mut output_bits = BTreeMap::new();
    let mut feature_bits = BTreeMap::new();

    while offset < bytes.len() {
        let prefix = bytes[offset];
        offset += 1;
        if prefix == 0xfe {
            if offset + 2 > bytes.len() {
                break;
            }
            let len = bytes[offset] as usize;
            offset = (offset + 2 + len).min(bytes.len());
            continue;
        }
        let size = match prefix & 0x03 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        if offset + size > bytes.len() {
            break;
        }
        let value = little_value(&bytes[offset..offset + size]);
        offset += size;
        let item_type = (prefix >> 2) & 0x03;
        let tag = (prefix >> 4) & 0x0f;
        match (item_type, tag) {
            (1, 0) if !usage_pages.contains(&value) => usage_pages.push(value),
            (1, 7) => report_size = value as usize,
            (1, 8) => {
                report_id = value as u8;
                if !ids.contains(&report_id) {
                    ids.push(report_id);
                }
            }
            (1, 9) => report_count = value as usize,
            (0, 8) => add_bits(
                &mut input_bits,
                report_id,
                report_size,
                report_count,
                ReportKind::Input,
            ),
            (0, 9) => add_bits(
                &mut output_bits,
                report_id,
                report_size,
                report_count,
                ReportKind::Output,
            ),
            (0, 11) => add_bits(
                &mut feature_bits,
                report_id,
                report_size,
                report_count,
                ReportKind::Feature,
            ),
            _ => {}
        }
    }
    ids.sort_unstable();
    usage_pages.sort_unstable();
    ReportDescriptorSummary {
        byte_length: bytes.len(),
        report_ids: ids,
        input_bits,
        output_bits,
        feature_bits,
        usage_pages,
        raw_hex: hex::encode(bytes),
    }
}

fn little_value(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .enumerate()
        .fold(0, |value, (index, byte)| value | ((*byte as u32) << (index * 8)))
}

fn add_bits(
    map: &mut BTreeMap<u8, usize>,
    report_id: u8,
    report_size: usize,
    report_count: usize,
    _kind: ReportKind,
) {
    *map.entry(report_id).or_default() += report_size.saturating_mul(report_count);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_report_ids_and_lengths() {
        let descriptor = [
            0x06, 0x00, 0xff, // usage page 0xff00
            0x85, 0x10, // report id 0x10
            0x75, 0x08, // report size 8
            0x95, 0x06, // report count 6
            0x91, 0x00, // output
            0x95, 0x06, 0x81, 0x00, // input
            0x85, 0x11, 0x95, 0x13, 0x91, 0x00, // 19-byte output
        ];
        let parsed = inspect(&descriptor);
        assert_eq!(parsed.report_ids, [0x10, 0x11]);
        assert_eq!(parsed.usage_pages, [0xff00]);
        assert_eq!(parsed.output_bits[&0x10], 48);
        assert_eq!(parsed.input_bits[&0x10], 48);
        assert_eq!(parsed.output_bits[&0x11], 152);
    }
}
