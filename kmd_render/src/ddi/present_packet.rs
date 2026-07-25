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
    /// When non-null, `allocation_list` is the fixed present allocation array
    /// supplied by dxgkrnl and therefore contains the source and destination
    /// indices defined by the WDK.
    pub(crate) unsafe fn from_allocation_list(allocation_list: *mut DXGK_ALLOCATIONLIST) -> Self {
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
                slot_id: 2,
                driver_id: 2,
            }),
            destination: NonNull::new(destination_handle).map(|handle| PresentAllocation {
                handle,
                allocation_index: DXGK_PRESENT_DESTINATION_INDEX,
                slot_id: 1,
                driver_id: 1,
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
    pub(crate) fn validate_patch_capacity(self, args: &DXGKARG_PRESENT) -> Result<(), NTSTATUS> {
        let required = self.reference_count();
        if required != 0
            && (args.pPatchLocationListOut.is_null()
                || (args.PatchLocationListOutSize as usize) < required)
        {
            Err(STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER)
        } else {
            Ok(())
        }
    }

    /// Emit exactly the allocation references represented by this value.
    ///
    /// The capacity check happens before any write, so callers either get a
    /// complete valid list or an unchanged output pointer.
    ///
    /// # Safety
    /// `args` and its output patch list are supplied by dxgkrnl.  The function
    /// validates the pointer and capacity before writing.
    pub(crate) unsafe fn write_patch_references(
        self,
        args: &mut DXGKARG_PRESENT,
    ) -> Result<(), NTSTATUS> {
        let required = self.reference_count();
        if required == 0 {
            return Ok(());
        }
        if args.pPatchLocationListOut.is_null()
            || (args.PatchLocationListOutSize as usize) < required
        {
            return Err(STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER);
        }

        let first = args.pPatchLocationListOut;
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

        // `written` and `required` are derived from the same two Options.
        debug_assert_eq!(written, required);
        args.pPatchLocationListOut = unsafe { first.add(written) };
        Ok(())
    }
}
