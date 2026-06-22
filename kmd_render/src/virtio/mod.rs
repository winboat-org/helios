//! virtio-gpu PCI transport (KMD-local).
//!
//! A hand-rolled virtio-modern-PCI + split-virtqueue driver that brings the
//! virtio-gpu device online from `DxgkDdiStartDevice` (Phase 2). The on-wire
//! virtio-gpu command/response structs and the feature/status/cap constants live
//! in the shared `helios_protocol` crate (single source of truth shared with the
//! ICD); this module owns only the guest-internal transport — PCI capability
//! scan, BAR mapping, feature negotiation, and the split virtqueue — none of
//! which the ICD ever touches, so they are deliberately KMD-local rather than in
//! the shared crate.
//!
//! Build-up order (see KMD.md Phase 2): M0 types → M1 cap scan/BAR map →
//! M2 feature negotiation → M3 control virtqueue → M4 GET_DISPLAY_INFO →
//! M5 MSI-X ISR/DPC → M6 teardown.

pub mod config;
pub mod gpu;
pub mod hal;
pub mod venus;

pub use gpu::VirtioGpu;

use wdk_sys::{
    NTSTATUS, STATUS_INSUFFICIENT_RESOURCES, STATUS_IO_DEVICE_ERROR, STATUS_NOT_IMPLEMENTED,
};

/// Errors from virtio-gpu bring-up. Mapped to NTSTATUS so `StartDevice` can fail
/// loudly (and distinguishably) rather than leaving a half-initialized adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtioError {
    /// A non-paged / contiguous allocation failed.
    OutOfMemory,
    /// A required virtio PCI capability (common/notify cfg) was not found.
    CapNotFound,
    /// The device dropped a feature we require during FEATURES_OK negotiation.
    FeatureRejected,
    /// Mapping a device BAR into kernel VA failed.
    MmioMapFailed,
    /// The device reported an error or behaved unexpectedly.
    DeviceError,
    /// Not yet implemented (scaffolding).
    NotImplemented,
}

impl From<VirtioError> for NTSTATUS {
    fn from(e: VirtioError) -> Self {
        match e {
            VirtioError::OutOfMemory | VirtioError::MmioMapFailed => STATUS_INSUFFICIENT_RESOURCES,
            VirtioError::CapNotFound | VirtioError::FeatureRejected | VirtioError::DeviceError => {
                STATUS_IO_DEVICE_ERROR
            }
            VirtioError::NotImplemented => STATUS_NOT_IMPLEMENTED,
        }
    }
}
