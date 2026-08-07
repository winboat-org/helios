//! Validated descriptors for the WDDM allocation path.
//!
//! `allocate_wddm_resource` took eight interdependent scalars —
//! `backing_blob_id`, `backing_blob_size`, `backing_resource_id`,
//! `venus_alloc_size`, `memory_type_index`, `direct_scanout_primary`,
//! `scanout_pitch`, `scanout_offset` — expressing three real modes (no backing
//! / venus-backed non-primary / venus-backed direct scan-out primary), and
//! re-derived which mode it was in from six independent `!= 0` conjunctions
//! spread through the body. Four were `u64`/`u32` pairs that would swap
//! silently, and two of the three call sites passed `0, 0, 0, 0, 0, false, 0, 0`.
//!
//! R806 groups them into the two descriptors below. The per-field zero
//! semantics are preserved exactly, which matters more than it looks:
//!
//! - `blob_id` is the ONLY field that gates a mode. A live `blob_id` with
//!   `blob_size == 0` must still fall back to the computed linear size.
//! - `resource_id == 0` cannot describe an importable Venus backing. Passing
//!   the device-memory id onward without a resource to adopt makes the host
//!   try to export non-exportable memory, so construction rejects it.
//! - `direct_scanout_primary` stays a separate argument and is deliberately
//!   NOT folded into `Option<ScanoutGeometry>`: dropping
//!   `HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT` from the KMD meta because the
//!   geometry happened to be absent would be a wire-visible behaviour change.

use core::num::{NonZeroU32, NonZeroU64};
use helios_protocol::GlobalVidMmTracker;

/// A venus device-memory allocation this UMD already created, which the KMD
/// should adopt rather than allocate against.
///
/// Its presence is the mode discriminator: `None` is a plain KMD-backed
/// standard allocation, `Some` is venus-backed.
#[derive(Clone, Copy)]
pub(crate) struct VenusBacking {
    /// The venus device-memory id. Non-zero by construction — this is the
    /// field the three `blob_id != 0` conjunctions used to test independently.
    pub(crate) blob_id: NonZeroU64,
    /// Size of the backing allocation. Zero is legal and means "fall back to
    /// the computed linear size", which is why this is not a `NonZeroU64`.
    pub(crate) blob_size: u64,
    /// The existing virtio resource id for the KMD to adopt. A Venus backing
    /// without one cannot be exported by the host and is therefore invalid.
    pub(crate) resource_id: NonZeroU32,
    /// The creating `vkAllocateMemory`'s exact size, for cross-process import.
    pub(crate) alloc_size: u64,
    /// The creating `vkAllocateMemory`'s memory type index.
    pub(crate) memory_type_index: u32,
    /// System-wide KMT share and association cookie for this memory's full-size
    /// VidMm tracker.
    /// `None` is the fail-safe value for an older ICD or any best-effort
    /// tracker failure; the adopted allocation then keeps its full charge.
    pub(crate) global_vidmm_tracker: Option<GlobalVidMmTracker>,
}

impl VenusBacking {
    /// Build a backing from the raw values `dxvk_resource_memory_info` and
    /// `get_resource_alloc_identity` produce. Returns `None` when there is no
    /// importable backing. Both the device-memory id and its already-exportable
    /// virtio resource id are required.
    pub(crate) fn new(
        blob_id: u64,
        blob_size: u64,
        resource_id: u32,
        alloc_size: u64,
        memory_type_index: u32,
        global_vidmm_tracker: u64,
    ) -> Option<Self> {
        let tracker = GlobalVidMmTracker {
            global_share: global_vidmm_tracker as u32,
            cookie: (global_vidmm_tracker >> 32) as u32,
        };
        let resource_id = NonZeroU32::new(resource_id)?;
        Some(Self {
            blob_id: NonZeroU64::new(blob_id)?,
            blob_size,
            resource_id,
            alloc_size,
            memory_type_index,
            global_vidmm_tracker: (blob_size != 0
                && alloc_size != 0
                && tracker.global_share != 0
                && tracker.cookie != 0)
                .then_some(tracker),
        })
    }

    /// `adopt_resource_id` for the wire record.
    pub(crate) fn adopt_resource_id(&self) -> u32 {
        self.resource_id.get()
    }
}

/// Where a scan-out primary's pixels actually are.
///
/// A zero pitch is unrepresentable, which is the point: it is the operand a
/// scan-out primary cannot be described without, and a wrong or absent stride
/// shears the scanned-out image.
#[derive(Clone, Copy)]
pub(crate) struct ScanoutGeometry {
    /// Row stride the KMD hands to `SET_SCANOUT_BLOB`.
    pub(crate) pitch: NonZeroU32,
    /// Memory-plane-0 offset; the KMD adds it to the blob base.
    pub(crate) plane_offset: u64,
}

impl ScanoutGeometry {
    /// `None` for a zero pitch. Note the arithmetic that produces the pitch is
    /// deliberately untouched by R806/R822 — `(width * 4 + 255) & !255`, giving
    /// 7680 for a 1896-wide primary, is what the frozen host reconstruction
    /// expects.
    pub(crate) fn new(pitch: u32, plane_offset: u64) -> Option<Self> {
        Some(Self {
            pitch: NonZeroU32::new(pitch)?,
            plane_offset,
        })
    }
}
