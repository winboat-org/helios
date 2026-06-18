//! Driver error type mapped to NTSTATUS codes.

use crate::dxgk::*;

#[derive(Debug, Clone, Copy)]
pub enum DriverError {
    InsufficientResources,
    InvalidParameter,
    DeviceNotFound,
    IoError,
    NotImplemented,
}

impl DriverError {
    pub fn into_ntstatus(self) -> NTSTATUS {
        match self {
            Self::InsufficientResources => STATUS_INSUFFICIENT_RESOURCES,
            Self::InvalidParameter => STATUS_INVALID_PARAMETER,
            Self::DeviceNotFound => STATUS_DEVICE_DOES_NOT_EXIST,
            Self::IoError => STATUS_IO_DEVICE_ERROR,
            Self::NotImplemented => STATUS_NOT_IMPLEMENTED,
        }
    }
}

impl From<DriverError> for NTSTATUS {
    fn from(e: DriverError) -> Self {
        e.into_ntstatus()
    }
}

/// Convenience: turn a `Result<(), DriverError>` into an NTSTATUS.
pub fn status_of(result: Result<(), DriverError>) -> NTSTATUS {
    match result {
        Ok(()) => STATUS_SUCCESS,
        Err(e) => e.into_ntstatus(),
    }
}
