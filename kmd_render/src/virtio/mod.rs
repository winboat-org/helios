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
pub mod counters;
pub mod ctrl;
pub mod gpu;
pub mod hal;
pub mod pci_caps;
pub mod venus;

// `gpu::CompletedBind` is deliberately NOT re-exported: its only consumer names
// it through inference (`take_completed_bind`), and an unused re-export is a
// warning in a crate that keeps its build clean.
pub use gpu::{
    FastBindDispatch, ScanoutBindRequest, ScanoutFlushToken, ScanoutNotify, VirtioGpu, WddmTake,
    WorkerBindDispatch,
};

use wdk_sys::{
    NTSTATUS, STATUS_DEVICE_BUSY, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_DEVICE_REQUEST,
    STATUS_IO_DEVICE_ERROR, STATUS_IO_TIMEOUT, STATUS_NOT_IMPLEMENTED,
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
    /// The control queue / in-flight tables are full — retry after a PASSIVE
    /// sleep (natural backpressure; NOT a device failure).
    QueueFull,
    /// A synchronous control command did not complete within its PASSIVE wait
    /// budget (the in-flight slot was abandoned; the transport keeps working).
    Timeout,
    /// Not yet implemented (scaffolding).
    NotImplemented,
    /// The calling device does not own the context this operation names. An
    /// authorization refusal, not a device failure — the escape layer maps it to
    /// STATUS_INVALID_DEVICE_REQUEST and counts it (EscCtxOwn).
    NotOwned,
}

impl From<VirtioError> for NTSTATUS {
    fn from(e: VirtioError) -> Self {
        match e {
            VirtioError::OutOfMemory | VirtioError::MmioMapFailed => STATUS_INSUFFICIENT_RESOURCES,
            VirtioError::CapNotFound | VirtioError::FeatureRejected | VirtioError::DeviceError => {
                STATUS_IO_DEVICE_ERROR
            }
            VirtioError::QueueFull => STATUS_DEVICE_BUSY,
            VirtioError::Timeout => STATUS_IO_TIMEOUT,
            VirtioError::NotImplemented => STATUS_NOT_IMPLEMENTED,
            VirtioError::NotOwned => STATUS_INVALID_DEVICE_REQUEST,
        }
    }
}
