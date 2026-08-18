use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::{AppError, Result};
use crate::hid::transport::HidIo;

pub const SHORT_REPORT_ID: u8 = 0x10;
pub const LONG_REPORT_ID: u8 = 0x11;
pub const SHORT_REPORT_LEN: usize = 7;
pub const LONG_REPORT_LEN: usize = 20;

static NEXT_SOFTWARE_ID: AtomicU8 = AtomicU8::new(8);

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ProtocolVersion {
    pub major: u8,
    pub minor: u8,
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidppMessage {
    pub report_id: u8,
    pub device_index: u8,
    pub feature_index: u8,
    pub function: u8,
    pub software_id: u8,
    pub parameters: Vec<u8>,
}

impl HidppMessage {
    pub fn request(
        device_index: u8,
        feature_index: u8,
        function: u8,
        parameters: &[u8],
        long: bool,
    ) -> Result<Self> {
        if function & 0x0f != 0 {
            return Err(AppError::Validation(format!(
                "HID++ function must use its upper nibble only (got {function:#04x})"
            )));
        }
        let max = if long { 16 } else { 3 };
        if parameters.len() > max {
            return Err(AppError::Validation(format!(
                "{} parameters do not fit in a {} HID++ report (maximum {max})",
                parameters.len(),
                if long { "long" } else { "short" }
            )));
        }
        Ok(Self {
            report_id: if long { LONG_REPORT_ID } else { SHORT_REPORT_ID },
            device_index,
            feature_index,
            function,
            software_id: next_software_id(),
            parameters: parameters.to_vec(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let len = if self.report_id == LONG_REPORT_ID {
            LONG_REPORT_LEN
        } else {
            SHORT_REPORT_LEN
        };
        let mut report = vec![0u8; len];
        report[0] = self.report_id;
        report[1] = self.device_index;
        report[2] = self.feature_index;
        report[3] = self.function | (self.software_id & 0x0f);
        report[4..4 + self.parameters.len()].copy_from_slice(&self.parameters);
        report
    }

    fn function_and_software_id(&self) -> u8 {
        self.function | (self.software_id & 0x0f)
    }
}

fn next_software_id() -> u8 {
    let value = NEXT_SOFTWARE_ID.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(if current >= 15 { 8 } else { current + 1 })
    });
    value.unwrap_or(8)
}

pub struct HidppTransport<I: HidIo> {
    io: I,
    device_index: u8,
    timeout_ms: i32,
    trace: bool,
}

impl<I: HidIo> HidppTransport<I> {
    pub fn new(io: I, device_index: u8, timeout_ms: i32, trace: bool) -> Self {
        Self {
            io,
            device_index,
            timeout_ms,
            trace,
        }
    }

    pub fn device_index(&self) -> u8 {
        self.device_index
    }

    pub fn ping(&mut self) -> Result<ProtocolVersion> {
        let short = self.transact_with_report(0, 0x10, &[0, 0, 0], false);
        let response = match short {
            Ok(response) => response,
            Err(AppError::Timeout { .. }) => self.transact_with_report(0, 0x10, &[0, 0, 0], true)?,
            Err(error) => return Err(error),
        };
        if response.len() < 2 {
            return Err(AppError::Other(
                "ROOT.GetProtocolVersion returned fewer than two bytes".to_owned(),
            ));
        }
        Ok(ProtocolVersion {
            major: response[0],
            minor: response[1],
        })
    }

    pub fn transact(&mut self, feature_index: u8, function: u8, parameters: &[u8]) -> Result<Vec<u8>> {
        self.transact_with_report(feature_index, function, parameters, true)
    }

    pub fn transact_read(&mut self, feature_index: u8, function: u8, parameters: &[u8]) -> Result<Vec<u8>> {
        match self.transact(feature_index, function, parameters) {
            Err(AppError::Timeout { .. }) => {
                if self.trace {
                    eprintln!(
                        "RETRY dev={:#04x} feature={feature_index:#04x} function={function:#04x}",
                        self.device_index
                    );
                }
                self.transact(feature_index, function, parameters)
            }
            result => result,
        }
    }

    pub fn transact_no_reply(&mut self, feature_index: u8, function: u8, parameters: &[u8]) -> Result<()> {
        let request = HidppMessage::request(self.device_index, feature_index, function, parameters, true)?;
        let encoded = request.encode();
        self.drain_input()?;
        self.trace("TX", &encoded);
        self.io.write_report(&encoded)?;
        Ok(())
    }

    fn transact_with_report(
        &mut self,
        feature_index: u8,
        function: u8,
        parameters: &[u8],
        long: bool,
    ) -> Result<Vec<u8>> {
        let request = HidppMessage::request(self.device_index, feature_index, function, parameters, long)?;
        let encoded = request.encode();
        self.drain_input()?;
        self.trace("TX", &encoded);
        self.io.write_report(&encoded)?;

        let started = Instant::now();
        let timeout = Duration::from_millis(self.timeout_ms.max(1) as u64);
        let mut buffer = [0u8; 64];
        while started.elapsed() < timeout {
            let remaining = timeout.saturating_sub(started.elapsed()).as_millis().max(1) as i32;
            let read = self.io.read_report(&mut buffer, remaining)?;
            if read == 0 {
                break;
            }
            let response = &buffer[..read];
            self.trace("RX", response);
            if response.len() < 4 || response[1] != self.device_index {
                continue;
            }
            if response[2] == 0xff
                && response.len() >= 6
                && response[3] == feature_index
                && response[4] == request.function_and_software_id()
            {
                let code = response[5];
                return Err(AppError::Protocol {
                    device_index: self.device_index,
                    feature_index,
                    function,
                    code,
                    name: protocol_error_name(code),
                });
            }
            if response[2] == feature_index && response[3] == request.function_and_software_id() {
                return Ok(response[4..].to_vec());
            }
        }
        Err(AppError::Timeout {
            timeout_ms: self.timeout_ms,
            device_index: self.device_index,
            feature_index,
            function,
            request: spaced_hex(&encoded),
        })
    }

    fn drain_input(&self) -> Result<()> {
        let mut buffer = [0u8; 64];
        for _ in 0..32 {
            if self.io.read_report(&mut buffer, 1)? == 0 {
                break;
            }
        }
        Ok(())
    }

    fn trace(&self, direction: &str, bytes: &[u8]) {
        if self.trace {
            eprintln!("{direction} dev={:#04x} {}", self.device_index, spaced_hex(bytes));
        }
    }
}

pub fn spaced_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn protocol_error_name(code: u8) -> &'static str {
    match code {
        0x00 => "NO_ERROR",
        0x01 => "UNKNOWN",
        0x02 => "INVALID_ARGUMENT",
        0x03 => "OUT_OF_RANGE",
        0x04 => "HARDWARE_ERROR",
        0x05 => "LOGITECH_INTERNAL",
        0x06 => "INVALID_FEATURE_INDEX",
        0x07 => "INVALID_FUNCTION",
        0x08 => "BUSY",
        0x09 => "UNSUPPORTED",
        _ => "UNKNOWN_ERROR",
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    struct FakeIo {
        writes: RefCell<Vec<Vec<u8>>>,
        reads: RefCell<VecDeque<Vec<u8>>>,
    }

    impl FakeIo {
        fn new(reads: impl IntoIterator<Item = Vec<u8>>) -> Self {
            Self {
                writes: RefCell::new(Vec::new()),
                reads: RefCell::new(reads.into_iter().collect()),
            }
        }
    }

    impl HidIo for FakeIo {
        fn write_report(&self, data: &[u8]) -> Result<usize> {
            self.writes.borrow_mut().push(data.to_vec());
            Ok(data.len())
        }

        fn read_report(&self, data: &mut [u8], timeout_ms: i32) -> Result<usize> {
            if timeout_ms == 1 {
                return Ok(0);
            }
            let Some(mut next) = self.reads.borrow_mut().pop_front() else {
                return Ok(0);
            };
            if let Some(request) = self.writes.borrow().last() {
                if next.get(2) == Some(&0xff) && next.get(4) == Some(&0xfe) {
                    next[4] = request[3];
                } else if next.get(3) == Some(&0xfe) {
                    next[3] = request[3];
                }
            }
            data[..next.len()].copy_from_slice(&next);
            Ok(next.len())
        }
    }

    #[test]
    fn encodes_long_message() {
        let mut message = HidppMessage::request(1, 0x0a, 0x20, &[0x03, 0x20], true).unwrap();
        message.software_id = 8;
        let encoded = message.encode();
        assert_eq!(&encoded[..6], &[0x11, 0x01, 0x0a, 0x28, 0x03, 0x20]);
        assert_eq!(encoded.len(), 20);
    }

    #[test]
    fn correlates_response_and_ignores_notification() {
        let io = FakeIo::new([]);
        let mut transport = HidppTransport::new(io, 1, 10, false);
        transport.io.reads.borrow_mut().extend([
            vec![0x10, 1, 0x0a, 0x20, 0, 0, 0],
            vec![0x11, 1, 0x0a, 0xfe, 0x03, 0x20, 0, 0],
        ]);
        let response = transport.transact(0x0a, 0x20, &[]).unwrap();
        assert_eq!(&response[..2], &[0x03, 0x20]);
    }

    #[test]
    fn decodes_protocol_error() {
        let io = FakeIo::new([vec![0x11, 1, 0xff, 0x10, 0xfe, 0x07]]);
        let mut transport = HidppTransport::new(io, 1, 10, false);
        let error = transport.transact(0x10, 0x20, &[]).unwrap_err();
        assert!(matches!(error, AppError::Protocol { code: 7, .. }));
    }

    #[test]
    fn retries_a_timed_out_read_once() {
        let io = FakeIo::new([vec![], vec![0x11, 1, 0x0a, 0xfe, 0x03, 0x20]]);
        let mut transport = HidppTransport::new(io, 1, 10, false);
        let response = transport.transact_read(0x0a, 0x20, &[]).unwrap();
        assert_eq!(&response[..2], &[0x03, 0x20]);
        assert_eq!(transport.io.writes.borrow().len(), 2);
    }
}
