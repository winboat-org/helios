//! Structural validation and patch-list emission for `DXGKARG_PRESENT`.
//!
//! The allocation list has fixed source/destination slots, but either slot may
//! carry a null handle.  Only a non-null handle is a legal DMA-buffer
//! reference.  Keep that invariant in the type system so neither legacy
//! Present nor PresentToHwQueue can accidentally make a missing allocation
//! resident.

use core::ffi::c_void;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::dxgk::*;

// Legal retry status for DxgkDdiPresent when either the DMA or patch buffer
// cannot hold the complete operation (ntstatus.h).
pub(crate) const STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER: NTSTATUS = 0xC01E_0001u32 as NTSTATUS;

const PRESENT_SUBMISSION_MAGIC: u32 = 0x4850_424C; // "HPBL"
const PRESENT_SUBMISSION_VERSION: u32 = 1;

/// DISPATCH-safe proof that Present wrote a scheduler handoff marker.
pub(crate) static PRESENT_MARKER_WRITES: AtomicU32 = AtomicU32::new(0);
pub(crate) static PRESENT_MARKER_LAST_FENCE: AtomicU32 = AtomicU32::new(0);
pub(crate) static PRESENT_MARKER_LAST_SIZE: AtomicU32 = AtomicU32::new(0);

/// KMD-private scheduler handoff for a BLT submitted while building Present.
///
/// This lives only in the 40-byte DMA private-data buffer allocated by dxgkrnl
/// for each Helios context. It is not part of the Helios or virtio-gpu wire ABI.
/// The outer ring-1 fence denotes GPU completion of the recorded Vulkan copy;
/// SubmitCommand uses it to retire the corresponding WDDM DMA fence exactly.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PresentSubmissionPrivate {
    magic: u32,
    version: u32,
    gpu_fence_id: u64,
}

impl PresentSubmissionPrivate {
    #[inline]
    fn for_fence(gpu_fence_id: u64) -> Self {
        Self {
            magic: PRESENT_SUBMISSION_MAGIC,
            version: PRESENT_SUBMISSION_VERSION,
            gpu_fence_id,
        }
    }

    /// Merge a newly queued BLT fence into the current DMA buffer's marker.
    ///
    /// Multiple Present calls may append to one scheduler DMA buffer. Ring-1
    /// fence ids are monotonic and ordered, so waiting for the largest id also
    /// waits for every earlier copy in that buffer.
    ///
    /// # Safety
    /// `private_data` points to `private_size` writable bytes supplied by
    /// dxgkrnl for this Present call.
    pub(crate) unsafe fn merge_fence(
        private_data: *mut c_void,
        private_size: u32,
        gpu_fence_id: u64,
    ) -> Result<(), NTSTATUS> {
        if private_data.is_null()
            || (private_size as usize) < core::mem::size_of::<PresentSubmissionPrivate>()
        {
            return Err(STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER);
        }

        let old =
            unsafe { core::ptr::read_unaligned(private_data.cast::<PresentSubmissionPrivate>()) };
        let merged =
            if old.magic == PRESENT_SUBMISSION_MAGIC && old.version == PRESENT_SUBMISSION_VERSION {
                old.gpu_fence_id.max(gpu_fence_id)
            } else {
                gpu_fence_id
            };
        unsafe {
            core::ptr::write_unaligned(
                private_data.cast::<PresentSubmissionPrivate>(),
                Self::for_fence(merged),
            );
        }
        PRESENT_MARKER_LAST_FENCE.store(merged as u32, Ordering::Relaxed);
        PRESENT_MARKER_LAST_SIZE.store(private_size, Ordering::Relaxed);
        PRESENT_MARKER_WRITES.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Decode a scheduler submission's KMD-private data.
    ///
    /// Unknown data is deliberately treated as an ordinary submission rather
    /// than reinterpreted as a fence id.
    ///
    /// # Safety
    /// `private_data` points to `private_size` readable bytes supplied by
    /// dxgkrnl for this SubmitCommand call.
    pub(crate) unsafe fn decode(private_data: *const c_void, private_size: u32) -> Option<u64> {
        if private_data.is_null()
            || (private_size as usize) < core::mem::size_of::<PresentSubmissionPrivate>()
        {
            return None;
        }
        let value =
            unsafe { core::ptr::read_unaligned(private_data.cast::<PresentSubmissionPrivate>()) };
        (value.magic == PRESENT_SUBMISSION_MAGIC
            && value.version == PRESENT_SUBMISSION_VERSION
            && value.gpu_fence_id != 0)
            .then_some(value.gpu_fence_id)
    }

    /// Locate a valid marker anywhere in a bounded private-data snapshot.
    ///
    /// This is diagnostic only. SubmitCommand must never use a scan result for
    /// scheduling correctness: the exact WDDM private-data offset contract has
    /// to be identified and encoded explicitly.
    ///
    /// # Safety
    /// `private_data` points to `private_size` readable bytes supplied by
    /// dxgkrnl for the current SubmitCommand call.
    pub(crate) unsafe fn diagnostic_find_offset(
        private_data: *const c_void,
        private_size: u32,
    ) -> Option<u32> {
        const MAX_DIAGNOSTIC_BYTES: usize = 256;
        let record_size = core::mem::size_of::<PresentSubmissionPrivate>();
        let size = (private_size as usize).min(MAX_DIAGNOSTIC_BYTES);
        if private_data.is_null() || size < record_size {
            return None;
        }
        let base = private_data.cast::<u8>();
        for offset in 0..=size - record_size {
            if unsafe {
                Self::decode(
                    base.add(offset).cast(),
                    (size - offset).min(u32::MAX as usize) as u32,
                )
            }
            .is_some()
            {
                return Some(offset as u32);
            }
        }
        None
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PresentAllocation {
    handle: NonNull<c_void>,
    allocation_index: u32,
    slot_id: u32,
    driver_id: u32,
}

impl PresentAllocation {
    #[inline]
    pub(crate) fn handle(self) -> HANDLE {
        self.handle.as_ptr()
    }
}

/// `DXGK_ALLOCATIONLIST` patch slot ids, and the driver ids we echo back.
///
/// They were bare `1` and `2` written into a bitfield union's raw `Value` with
/// nothing saying what the numbers meant. The values are the WDK's fixed present
/// slots — `DXGK_PRESENT_DESTINATION_INDEX` = 1 and `DXGK_PRESENT_SOURCE_INDEX`
/// = 2 — and the driver id echoes the slot so a patch entry identifies itself.
const PATCH_SLOT_DESTINATION: u32 = DXGK_PRESENT_DESTINATION_INDEX;
const PATCH_SLOT_SOURCE: u32 = DXGK_PRESENT_SOURCE_INDEX;

/// Which arm of `DXGKARG_PRESENT.__bindgen_anon_1` this present carries.
///
/// The union has three arms — `pAllocationList`, `pAllocationInfo` and
/// `pPresentMultiPlaneOverlayInfo` (`tmp/dxgk_bindings.rs:37178-37182`) — and
/// both call sites used to pick `pAllocationList` implicitly, while
/// `from_allocation_list`'s SAFETY paragraph asserted the array shape without
/// ever having seen the present flags. Decoding the arm ONCE, from
/// `args.Flags`, means the choice happens in one audited place instead of
/// implicitly at two call sites.
pub(crate) enum PresentPayload<'a> {
    /// The fixed source/destination allocation array. Every present this driver
    /// services today.
    AllocationList(PresentAllocationList<'a>),
    /// `FlipWithMultiPlaneOverlay` — the `pPresentMultiPlaneOverlayInfo` arm.
    ///
    /// Unreachable: the driver does not register the MPO3 KMD interface
    /// (`query_adapter_info.rs`'s cap surface), so dxgkrnl never sets this. It
    /// is a named variant rather than an assumption so that if it ever arrives,
    /// the code refuses instead of reinterpreting an MPO struct as an
    /// allocation array.
    MultiPlaneOverlay,
}

impl<'a> PresentPayload<'a> {
    /// `DXGK_PRESENTFLAGS.FlipWithMultiPlaneOverlay`, verified against the
    /// generated bitfield order.
    const FLAG_FLIP_WITH_MPO: u32 = 1 << 12;

    /// Decode the union arm from the present flags.
    ///
    /// # Safety
    /// `args` is dxgkrnl's present argument struct, and the arm named by its
    /// flags is the one it initialised.
    pub(crate) unsafe fn decode(args: &'a DXGKARG_PRESENT) -> Self {
        // SAFETY: `Flags` is a POD union of a bitfield struct and a `UINT`
        // `Value`; reading the `Value` view is a read of initialized memory.
        let flags = unsafe { args.Flags.__bindgen_anon_1.Value };
        if flags & Self::FLAG_FLIP_WITH_MPO != 0 {
            return Self::MultiPlaneOverlay;
        }
        // SAFETY: not an MPO present, so the allocation-list arm is live.
        let list = unsafe { args.__bindgen_anon_1.pAllocationList };
        Self::AllocationList(PresentAllocationList {
            list,
            _present: core::marker::PhantomData,
        })
    }

    /// The allocation list, or `None` for an arm that has none.
    pub(crate) fn allocation_list(&self) -> Option<&PresentAllocationList<'a>> {
        match self {
            Self::AllocationList(list) => Some(list),
            Self::MultiPlaneOverlay => None,
        }
    }
}

/// The fixed present allocation array, as a value only [`PresentPayload::decode`]
/// can produce.
///
/// A wrapper rather than a raw pointer so the OTHER two union arms cannot be
/// reinterpreted as an allocation list. It adds provenance, not a new runtime
/// check: the count it uses is the WDK's fixed `DXGK_PRESENT_MAX_INDEX + 1`, NOT
/// `NumSrcAllocations`/`NumDstAllocations`, and it must stay that way — dxgkrnl
/// supplies the full fixed array and encodes an absent source or destination as
/// a NULL `hDeviceSpecificAllocation`. Passing the Num* counts as the bound
/// "while we're here" changes which slots are read for the absent case and
/// silently drops presents.
pub(crate) struct PresentAllocationList<'a> {
    list: *mut DXGK_ALLOCATIONLIST,
    _present: core::marker::PhantomData<&'a DXGKARG_PRESENT>,
}

impl PresentAllocationList<'_> {
    /// Whether dxgkrnl supplied an array at all (`PBalst`/`PHQalst`).
    pub(crate) fn is_present(&self) -> bool {
        !self.list.is_null()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PresentAllocations {
    source: Option<PresentAllocation>,
    destination: Option<PresentAllocation>,
}

impl PresentAllocations {
    /// Decode the fixed DXGK present allocation slots.
    ///
    /// A source or destination that is not part of this operation is encoded
    /// by dxgkrnl as a null `hDeviceSpecificAllocation`.  Converting only
    /// non-null handles to `PresentAllocation` makes it impossible for patch
    /// emission to reference an absent allocation.
    ///
    /// # Safety
    /// `list` came from [`PresentPayload::decode`], so it is the fixed present
    /// allocation array supplied by dxgkrnl and contains the source and
    /// destination indices defined by the WDK.
    pub(crate) unsafe fn from_allocation_list(list: &PresentAllocationList<'_>) -> Self {
        let allocation_list = list.list;
        if allocation_list.is_null() {
            return Self {
                source: None,
                destination: None,
            };
        }

        let source_handle = unsafe {
            (*allocation_list.add(DXGK_PRESENT_SOURCE_INDEX as usize)).hDeviceSpecificAllocation
        };
        let destination_handle = unsafe {
            (*allocation_list.add(DXGK_PRESENT_DESTINATION_INDEX as usize))
                .hDeviceSpecificAllocation
        };

        Self {
            source: NonNull::new(source_handle).map(|handle| PresentAllocation {
                handle,
                allocation_index: DXGK_PRESENT_SOURCE_INDEX,
                slot_id: PATCH_SLOT_SOURCE,
                driver_id: PATCH_SLOT_SOURCE,
            }),
            destination: NonNull::new(destination_handle).map(|handle| PresentAllocation {
                handle,
                allocation_index: DXGK_PRESENT_DESTINATION_INDEX,
                slot_id: PATCH_SLOT_DESTINATION,
                driver_id: PATCH_SLOT_DESTINATION,
            }),
        }
    }

    #[inline]
    pub(crate) fn source(self) -> Option<PresentAllocation> {
        self.source
    }

    #[inline]
    pub(crate) fn destination(self) -> Option<PresentAllocation> {
        self.destination
    }

    #[inline]
    pub(crate) fn reference_count(self) -> usize {
        usize::from(self.destination.is_some()) + usize::from(self.source.is_some())
    }

    /// Validate patch-list capacity without mutating the runtime's output
    /// cursor. Present must call this before it submits any host GPU work, so an
    /// insufficient-buffer retry cannot duplicate a BLT.
    ///
    /// Returns the proof, not `()`. [`PatchCapacity`] is non-`Copy` and
    /// non-`Clone` and carries the checked pointer and count, so
    /// [`Self::write_patch_references`] cannot run without it, cannot re-derive
    /// the capacity expression, and cannot be called twice against a stale
    /// cursor. Same shape as `WddmNotifyGuard` and T1b's `RenderGdiPlan`.
    pub(crate) fn validate_patch_capacity(
        self,
        args: &DXGKARG_PRESENT,
    ) -> Result<PatchCapacity, NTSTATUS> {
        let required = self.reference_count();
        if required == 0 {
            return Ok(PatchCapacity {
                first: core::ptr::null_mut(),
                required: 0,
            });
        }
        if args.pPatchLocationListOut.is_null()
            || (args.PatchLocationListOutSize as usize) < required
        {
            return Err(STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER);
        }
        Ok(PatchCapacity {
            first: args.pPatchLocationListOut,
            required,
        })
    }

    /// Emit exactly the allocation references represented by this value.
    ///
    /// Consumes the [`PatchCapacity`] token, so the "validate before any host
    /// GPU work" ordering is discharged by the type system rather than by the
    /// programmer remembering: `display.rs` validates at the top and submits the
    /// BLT before writing, and moving the submission above the validation — or
    /// adding a second one after it — no longer compiles into a path that can
    /// duplicate GPU copies on an insufficient-buffer retry (the same defect
    /// class as T1b's `k-paging-01`). `scheduler.rs` acquires the token
    /// immediately before writing, which is what documents that it queues
    /// nothing.
    ///
    /// # Safety
    /// The output patch list is supplied by dxgkrnl; `capacity` records that we
    /// checked the count dxgkrnl declared, not that the memory is mapped.
    pub(crate) unsafe fn write_patch_references(
        self,
        capacity: PatchCapacity,
        args: &mut DXGKARG_PRESENT,
    ) -> Result<(), NTSTATUS> {
        let PatchCapacity { first, required } = capacity;
        if required == 0 {
            return Ok(());
        }

        let mut written = 0usize;
        for reference in [self.destination, self.source].into_iter().flatten() {
            let patch = unsafe { first.add(written) };
            unsafe {
                core::ptr::write_bytes(patch, 0, 1);
                (*patch).AllocationIndex = reference.allocation_index;
                (*patch).__bindgen_anon_1.Value = reference.slot_id;
                (*patch).DriverId = reference.driver_id;
            }
            written += 1;
        }

        // `written == required` by construction: both are derived from the same
        // two Options, and `PatchCapacity` carries the count computed from them.
        // The `debug_assert_eq!` that used to stand here was a LIVE KeBugCheck
        // site in the shipped image — [profile.dev] does not disable
        // debug-assertions and cargo-make ships that profile — so it traded a
        // structurally-impossible mismatch for a real bugcheck.
        args.pPatchLocationListOut = unsafe { first.add(written) };
        // Decrement the declared capacity with the cursor. A second call would
        // otherwise validate against a stale count. `display.rs` records
        // PatchLocationListOutSize into PBPatch BEFORE the write, so the
        // breadcrumb is unaffected — keep it that way.
        args.PatchLocationListOutSize = args
            .PatchLocationListOutSize
            .saturating_sub(written as u32);
        Ok(())
    }
}

/// Proof that the patch-list capacity was checked, carrying what was checked.
///
/// Non-`Copy` and non-`Clone` on purpose: it is consumed by
/// [`PresentAllocations::write_patch_references`], so it cannot be reused for a
/// second write against an advanced cursor.
///
/// The token must CARRY the pointer and count and the writer must stop
/// re-deriving them — otherwise this is a relocation of the same runtime check.
/// That is why the capacity expression now exists exactly once, in
/// [`PresentAllocations::validate_patch_capacity`], instead of being duplicated
/// there and in the writer where the two copies could silently diverge.
pub(crate) struct PatchCapacity {
    /// The checked `pPatchLocationListOut`. Null iff `required == 0`.
    first: *mut D3DDDI_PATCHLOCATIONLIST,
    /// How many entries were proven to fit.
    required: usize,
}
