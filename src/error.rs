use thiserror::Error;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("no compatible Logitech HID++ device was found")]
    NoDevice,

    #[error("device selector '{0}' did not match a device")]
    DeviceNotFound(String),

    #[error("device selector '{0}' is ambiguous; use its numeric ID, serial, or hidraw path")]
    AmbiguousDevice(String),

    #[error("feature {id:#06x} ({name}) is not advertised by this device")]
    UnsupportedFeature { id: u16, name: &'static str },

    #[error(
        "permission denied opening {path}; install packaging/udev/70-open-mouse-memory.rules and reconnect the mouse or receiver"
    )]
    PermissionDenied { path: String },

    #[error("HID access failed for {path}: {message}")]
    Hid { path: String, message: String },

    #[error(
        "timeout after {timeout_ms} ms waiting for HID++ response\nDevice index: {device_index:#04x}\nFeature index: {feature_index:#04x}\nFunction: {function:#04x}\nRequest: {request}"
    )]
    Timeout {
        timeout_ms: i32,
        device_index: u8,
        feature_index: u8,
        function: u8,
        request: String,
    },

    #[error(
        "HID++ request failed\nDevice index: {device_index:#04x}\nFeature index: {feature_index:#04x}\nFunction: {function:#04x}\nError: {code:#04x} ({name})"
    )]
    Protocol {
        device_index: u8,
        feature_index: u8,
        function: u8,
        code: u8,
        name: &'static str,
    },

    #[error("invalid value: {0}")]
    Validation(String),

    #[error("write verification failed: {0}")]
    Verification(String),

    #[error("unsafe operation refused: {0}")]
    Unsafe(String),

    #[error("invalid profile format: {0}")]
    IncompatibleProfile(String),

    #[error("{0}")]
    Other(String),
}

impl AppError {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::NoDevice | Self::DeviceNotFound(_) => 2,
            Self::AmbiguousDevice(_) => 3,
            Self::UnsupportedFeature { .. } => 4,
            Self::PermissionDenied { .. } => 5,
            Self::Timeout { .. } => 6,
            Self::Protocol { .. } => 7,
            Self::Validation(_) | Self::Unsafe(_) => 8,
            Self::Verification(_) => 9,
            Self::IncompatibleProfile(_) => 10,
            _ => 1,
        }
    }

    pub fn hid(path: impl Into<String>, error: impl std::fmt::Display) -> Self {
        let path = path.into();
        let message = error.to_string();
        if message.to_ascii_lowercase().contains("permission denied") {
            Self::PermissionDenied { path }
        } else {
            Self::Hid { path, message }
        }
    }
}
