use hidapi::HidDevice;

use crate::error::{AppError, Result};

pub trait HidIo {
    fn write_report(&self, data: &[u8]) -> Result<usize>;
    fn read_report(&self, data: &mut [u8], timeout_ms: i32) -> Result<usize>;
}

pub struct HidDeviceIo {
    device: HidDevice,
    path: String,
}

impl HidDeviceIo {
    pub fn new(device: HidDevice, path: impl Into<String>) -> Self {
        Self {
            device,
            path: path.into(),
        }
    }

    pub fn report_descriptor(&self) -> Result<Vec<u8>> {
        let mut buffer = vec![0u8; hidapi::MAX_REPORT_DESCRIPTOR_SIZE];
        let size = self
            .device
            .get_report_descriptor(&mut buffer)
            .map_err(|error| AppError::hid(&self.path, error))?;
        buffer.truncate(size);
        Ok(buffer)
    }
}

impl HidIo for HidDeviceIo {
    fn write_report(&self, data: &[u8]) -> Result<usize> {
        self.device
            .write(data)
            .map_err(|error| AppError::hid(&self.path, error))
    }

    fn read_report(&self, data: &mut [u8], timeout_ms: i32) -> Result<usize> {
        self.device
            .read_timeout(data, timeout_ms)
            .map_err(|error| AppError::hid(&self.path, error))
    }
}
