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

use helios_kmd_logic::snapshot_bind::SnapshotDescriptor;

use crate::dxgk::*;

// Legal retry status for DxgkDdiPresent when either the DMA or patch buffer
// cannot hold the complete operation (ntstatus.h).
pub(crate) const STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER: NTSTATUS = 0xC01E_0001u32 as NTSTATUS;

// Version 3 appends the exact WindowedBlt admission token. Do not decode an
// older layout as v3: the tail belongs to another private record there.
const PRESENT_SUBMISSION_VERSION: u32 = 3;
const PRESENT_SUBMISSION_MAGIC: u32 = 0x4850_424C; // "HPBL"
/// The same 32-byte record, written by `dxgkddi_render`'s D3D12 ECL arm instead
/// of by Present. "HD12" — distinct from `helios_protocol`'s wire-side
/// `HELIOS_D3D12_SUBMIT_MAGIC` ("HE12") because they identify different things:
/// that one is the UMD's command record, this one is the KMD's private handoff.
///
/// It exists so `decode` can tell the scheduler that a submission belongs to a
/// D3D12 command queue. The FIFO it feeds is adapter-global and strictly
/// head-of-line, so any experiment that delays a packet MUST be able to name the
/// packets it may delay; see `PresentSubmissionPrivate::mark_d3d12`.
const D3D12_SUBMISSION_MAGIC: u32 = 0x4844_3132; // "HD12"

/// Byte offset of [`PresentFlipPrivate`] inside the per-context DMA
/// private-data buffer. [`PresentSubmissionPrivate`] owns bytes 0..32, so the
/// flip record occupies bytes 32.. and the two never collide even when dxgkrnl
/// batches a BLT and a flip into one DMA buffer.
pub(crate) const PRESENT_FLIP_PRIVATE_OFFSET: usize = 32;

/// `DmaBufferPrivateDataSize` this driver requests per context (`device.rs`'s
/// CreateContext reads it from here). 40 bytes until D4b; the snapshot
/// descriptor grew [`PresentFlipPrivate`] by 32 bytes, and this is the OTHER
/// half of the "deliberate change to BOTH sites" the compile-time proof below
/// demands.
pub(crate) const PRESENT_DMA_PRIVATE_DATA_BYTES: u32 = 88;

const PRESENT_FLIP_MAGIC: u32 = 0x4850_464C; // "HPFL"
const PRESENT_FLIP_VERSION: u32 = 1;

/// KMD-private flip record for the DMA-BUFFER FLIP contract.
///
/// WHY THIS EXISTS. `DXGK_FLIPCAPS.FlipOnVSyncMmIo` covers only nonzero flip
/// intervals. An IMMEDIATE flip (interval 0 — every unthrottled fullscreen app)
/// goes down the DMA-buffer flip contract instead, in which dxgkrnl calls
/// `DxgkDdiPresent` with a real `pDmaBuffer` and NEVER calls
/// `DxgkDdiSetVidPnSourceAddress`: the driver is expected to program the
/// display itself when that DMA buffer executes.
///
/// Advertising `FlipImmediateMmIo` to force those flips onto the MMIO path
/// instead is NOT a valid substitute for implementing this, and was measured
/// not to be (2026-07-29): the MMIO contract requires the flip to be complete
/// when the DDI RETURNS, and Helios cannot do that, because a Helios flip is a
/// virtio `SET_SCANOUT_BLOB` round-trip that is illegal at the DIRQL the DDI
/// arrives at. Returning STATUS_SUCCESS from a DIRQL stash made dxgkrnl free
/// the previous buffer to the app and issue the next flip while nothing had
/// been programmed — measured as 80 dropped binds and 145 of 1245 present
/// markers writing the buffer that was on screen. The DMA-buffer contract is
/// the one designed for hardware that cannot flip synchronously, because the
/// submission fence puts the completion signal back under driver control.
///
/// It lives in the kernel-only `pDmaBufferPrivateData`, never in the DMA buffer
/// the UMD can see, so the allocation handle it carries is not user-influenced.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PresentFlipPrivate {
    magic: u32,
    version: u32,
    /// `hDeviceSpecificAllocation` of the flip source — the exact identity
    /// `create_allocation::scanout_alloc_info` resolves, same as the handle
    /// `SetVidPnSourceAddress` supplies on the MMIO path.
    allocation: u64,
    /// `DXGK_ALLOCATIONLIST::PhysicalAddress` of that allocation. This is what
    /// a later CRTC_VSYNC must carry for dxgkrnl to retire the flip.
    physical_address: u64,
    /// D4b snapshot BIND-TARGET substitution, carried BY VALUE (never a
    /// pointer, never an allocation handle): the venus resource the KMD binds
    /// and flushes INSTEAD of `allocation`'s own, already validated at the
    /// Present DDI (`helios_kmd_logic::snapshot_bind::validate`). 0 = no
    /// substitution — every pre-snapshot flip. The FLIP bookkeeping (epoch
    /// stamp, `physical_address`, CRTC_VSYNC retirement) stays entirely on
    /// `allocation` regardless.
    snap_resid: u32,
    snap_width: u32,
    snap_height: u32,
    snap_pitch: u32,
    snap_dxgi_format: u32,
    /// Validated `<= u32::MAX` at Present, so the wire's u64 narrows honestly.
    snap_plane_offset: u32,
    /// The undersize guard's right-hand side, meaningful only with
    /// `snap_resid != 0`.
    snap_alloc_size: u64,
}

impl PresentFlipPrivate {
    /// Write the flip record. `private_size` must cover the whole layout; a
    /// smaller buffer is a refusal, never a partial write.
    ///
    /// # Safety
    /// `private_data` points to `private_size` writable bytes supplied by
    /// dxgkrnl for this Present call.
    pub(crate) unsafe fn write(
        private_data: *mut c_void,
        private_size: u32,
        allocation: HANDLE,
        physical_address: u64,
        snapshot: Option<SnapshotDescriptor>,
    ) -> Result<(), NTSTATUS> {
        if private_data.is_null()
            || (private_size as usize)
                < PRESENT_FLIP_PRIVATE_OFFSET + core::mem::size_of::<PresentFlipPrivate>()
        {
            return Err(STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER);
        }
        // A `None` writes an all-zero descriptor: `snap_resid == 0` is the one
        // no-substitution sentinel, so a recycled buffer's stale descriptor
        // bytes can never be mistaken for a carried one.
        let snap = snapshot.unwrap_or(SnapshotDescriptor {
            resource_id: 0,
            width: 0,
            height: 0,
            pitch: 0,
            dxgi_format: 0,
            plane_offset: 0,
            venus_alloc_size: 0,
            memory_type_index: 0,
            purpose: 0,
        });
        let record = PresentFlipPrivate {
            magic: PRESENT_FLIP_MAGIC,
            version: PRESENT_FLIP_VERSION,
            allocation: allocation as usize as u64,
            physical_address,
            snap_resid: snap.resource_id,
            snap_width: snap.width,
            snap_height: snap.height,
            snap_pitch: snap.pitch,
            snap_dxgi_format: snap.dxgi_format,
            snap_plane_offset: snap.plane_offset as u32,
            snap_alloc_size: snap.venus_alloc_size,
        };
        // SAFETY: the size check above proves the record fits at its offset;
        // unaligned because dxgkrnl makes no alignment promise about the
        // private-data buffer beyond its size.
        unsafe {
            core::ptr::write_unaligned(
                private_data
                    .cast::<u8>()
                    .add(PRESENT_FLIP_PRIVATE_OFFSET)
                    .cast::<PresentFlipPrivate>(),
                record,
            );
        }
        Ok(())
    }

    /// Take a flip record at submit time, or `None` when this DMA buffer
    /// carries none. Validating BOTH magic and version means an uninitialised
    /// or BLT-only private buffer cannot be mistaken for a flip, and the read
    /// CONSUMES the record so a recycled buffer cannot replay it.
    ///
    /// The third tuple element is the D4b snapshot descriptor the Present DDI
    /// validated and carried, or `None` (`snap_resid == 0`) for an ordinary
    /// flip.
    ///
    /// # Safety
    /// `private_data` points to `private_size` writable bytes from the DMA
    /// submission dxgkrnl is handing back.
    pub(crate) unsafe fn take(
        private_data: *mut c_void,
        private_size: u32,
    ) -> Option<(HANDLE, u64, Option<SnapshotDescriptor>)> {
        if private_data.is_null()
            || (private_size as usize)
                < PRESENT_FLIP_PRIVATE_OFFSET + core::mem::size_of::<PresentFlipPrivate>()
        {
            return None;
        }
        let slot = unsafe {
            private_data
                .cast::<u8>()
                .add(PRESENT_FLIP_PRIVATE_OFFSET)
                .cast::<PresentFlipPrivate>()
        };
        // SAFETY: size-checked above; unaligned because dxgkrnl makes no
        // alignment promise about the private-data buffer beyond its size.
        let record = unsafe { core::ptr::read_unaligned(slot) };
        if record.magic != PRESENT_FLIP_MAGIC || record.version != PRESENT_FLIP_VERSION {
            return None;
        }
        if record.allocation == 0 {
            return None;
        }
        // CONSUME IT. dxgkrnl recycles DMA private-data buffers between
        // submissions, so a record left behind would be read again by the next
        // submission that happens to reuse this buffer and would re-arm a bind
        // for a stale — possibly destroyed — allocation. Zeroing the magic
        // makes the record strictly one-shot, which is what "this submission
        // carries a flip" has to mean.
        //
        // SAFETY: same slot, same size check; only the magic word is written.
        unsafe { core::ptr::write_unaligned(slot.cast::<u32>(), 0) };
        let snapshot = if record.snap_resid != 0 {
            Some(SnapshotDescriptor {
                resource_id: record.snap_resid,
                width: record.snap_width,
                height: record.snap_height,
                pitch: record.snap_pitch,
                dxgi_format: record.snap_dxgi_format,
                plane_offset: record.snap_plane_offset as u64,
                venus_alloc_size: record.snap_alloc_size,
                memory_type_index: 0,
                purpose: 0,
            })
        } else {
            None
        };
        Some((
            record.allocation as usize as HANDLE,
            record.physical_address,
            snapshot,
        ))
    }
}

/// Compile-time proof the two private records fit the per-context private-data
/// buffer (`PRESENT_DMA_PRIVATE_DATA_BYTES`, which `device.rs`'s CreateContext
/// reports). Growing either record past it has to be a deliberate change to
/// BOTH sites, not a silent truncation here.
const _: () = {
    assert!(
        PRESENT_FLIP_PRIVATE_OFFSET + core::mem::size_of::<PresentFlipPrivate>()
            <= PRESENT_DMA_PRIVATE_DATA_BYTES as usize,
        "PresentFlipPrivate does not fit the DMA private-data buffer"
    );
    assert!(
        core::mem::size_of::<PresentSubmissionPrivate>() <= PRESENT_FLIP_PRIVATE_OFFSET,
        "PresentSubmissionPrivate overlaps PresentFlipPrivate"
    );
};

/// DISPATCH-safe proof that Present wrote a scheduler handoff marker.
pub(crate) static PRESENT_MARKER_WRITES: AtomicU32 = AtomicU32::new(0);
pub(crate) static PRESENT_MARKER_LAST_FENCE: AtomicU32 = AtomicU32::new(0);
pub(crate) static PRESENT_MARKER_LAST_SIZE: AtomicU32 = AtomicU32::new(0);

/// KMD-private scheduler handoff for a BLT submitted while building Present.
///
/// This lives only in the per-context DMA private-data buffer allocated by
/// dxgkrnl ([`PRESENT_DMA_PRIVATE_DATA_BYTES`]). It is not part of the Helios
/// or virtio-gpu wire ABI.
/// The outer ring-1 fence denotes GPU completion of the recorded Vulkan copy;
/// SubmitCommand uses it to retire the corresponding WDDM DMA fence exactly.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PresentSubmissionPrivate {
    magic: u32,
    version: u32,
    gpu_fence_id: u64,
    /// Exact opaque stream boundary captured from the UMD present marker.
    /// Bit 63 selects the stream namespace; zero means no stream marker and
    /// SubmitCommand keeps its legacy current-wire behavior.
    stream_boundary: u64,
    /// Monotonic WindowedBlt request token. Zero means this submission has no
    /// deferred BLT admission to promote.
    blt_token: u64,
}

/// Whether a D3D12 ECL record found one of its own already in this DMA buffer.
///
/// Named rather than a bare `bool` because the two cases mean different things
/// and only one of them is expected: several ECLs batched into one DMA buffer
/// (`MergedWithPredecessor`, benign — the largest wire fence subsumes the rest),
/// versus the first record in a fresh or consumed buffer (`First`, the norm).
/// After [`PresentSubmissionPrivate::decode`] began consuming the D3D12 record,
/// a predecessor can ALSO mean a buffer whose earlier submission never reached
/// SubmitCommand (preempted, or abandoned by a TDR epoch); merging is
/// conservative there too — it waits for work that really was enqueued.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum D3d12Mark {
    First,
    MergedWithPredecessor,
}

/// The scheduler-relevant contents of one exact KMD private-data record.
#[derive(Clone, Copy)]
pub(crate) struct PresentSubmissionBoundary {
    pub gpu_fence_id: u64,
    pub stream_boundary: u64,
    pub blt_token: u64,
    /// This submission carried a `HeliosD3D12SubmitCmd` — the scoping signal for
    /// anything that must apply to D3D12 ECL packets and to nothing else. Never
    /// a boundary: a `true` here says WHO submitted, not WHAT to wait for.
    pub d3d12: bool,
}

impl PresentSubmissionPrivate {
    /// `magic` says WHICH DDI arm wrote this record. The two are the same 32-byte
    /// shape with the same field meanings; the distinction exists only so
    /// [`Self::decode`] can report a D3D12 ECL submission as itself — see
    /// [`D3D12_SUBMISSION_MAGIC`].
    #[inline]
    fn for_parts(magic: u32, gpu_fence_id: u64, stream_boundary: u64, blt_token: u64) -> Self {
        Self {
            magic,
            version: PRESENT_SUBMISSION_VERSION,
            gpu_fence_id,
            stream_boundary,
            blt_token,
        }
    }

    /// Whether a record already in the buffer is one PRESENT wrote.
    ///
    /// ⚠ Deliberately NOT "either magic". The three Present writers must keep
    /// their exact prior behaviour, and accepting the D3D12 magic here would not
    /// be neutral: a D3D12 record's `stream_boundary`/`blt_token` are always 0, so
    /// preserving those would write the same zeros, but its `gpu_fence_id` would
    /// start being inherited into a Present record where before it was discarded.
    /// `mark_d3d12` runs the mirror-image check on its own magic instead.
    #[inline]
    fn is_present_record(&self) -> bool {
        self.magic == PRESENT_SUBMISSION_MAGIC && self.version == PRESENT_SUBMISSION_VERSION
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
        let recognised = old.is_present_record();
        let merged = if recognised {
            old.gpu_fence_id.max(gpu_fence_id)
        } else {
            gpu_fence_id
        };
        unsafe {
            core::ptr::write_unaligned(
                private_data.cast::<PresentSubmissionPrivate>(),
                Self::for_parts(
                    PRESENT_SUBMISSION_MAGIC,
                    merged,
                    if recognised { old.stream_boundary } else { 0 },
                    if recognised { old.blt_token } else { 0 },
                ),
            );
        }
        PRESENT_MARKER_LAST_FENCE.store(merged as u32, Ordering::Relaxed);
        PRESENT_MARKER_LAST_SIZE.store(private_size, Ordering::Relaxed);
        PRESENT_MARKER_WRITES.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Stamp the D3D12 ECL identity, and its boundary, onto the private data of
    /// the `DxgkDdiRender` call that carried `HeliosD3D12SubmitCmd`.
    ///
    /// # Why a second magic instead of a flag
    ///
    /// The record is exactly full — 32 bytes at offset 0, with `PresentFlipPrivate`
    /// immediately after it and a compile-time assert forbidding overlap — so
    /// there is no spare field, and `PRESENT_DMA_PRIVATE_DATA_BYTES` is exactly
    /// consumed. Identity also cannot ride the fence: `gpu_wire_fence` is legally
    /// 0 (the documented *order it against nothing* arm), and [`Self::decode`]
    /// rejects an all-zero payload under Present's magic — as it must, so a zeroed
    /// buffer is never read as a boundary. A distinct magic is positively
    /// identifying with every payload field still 0, which is precisely what
    /// `HELIOS_D3D12_SUBMIT_MAGIC`'s own doc means by *"its mere presence is what
    /// identifies a submission as a D3D12 ECL packet"*.
    ///
    /// Deliberately does NOT touch the `PRESENT_MARKER_*` trio: `PmWr`/`PmWFn`/
    /// `PmWSz` name the Present handoff, and `D12Rec` already counts this arm.
    ///
    /// # It is called for EVERY D3D12 record, including `gpu_fence_id == 0`
    ///
    /// ⛔ THIS IS WHAT MAKES A RECYCLED BUFFER SAFE, and it is not optional.
    /// dxgkrnl reuses DMA private-data buffers between submissions — the reason
    /// [`PresentFlipPrivate::take`] consumes its record, stated at that call site.
    /// If a zero-fence ECL declined to write, a previous ECL's `F1` would still be
    /// sitting at offset 0, SubmitCommand would gate the new packet on a fence
    /// that has already retired, the application's `ID3D12Fence` would signal
    /// early, and every counter would read healthy: the original defect wearing
    /// the instrumentation of the fix. Writing unconditionally makes "no boundary"
    /// EXPLICIT for this submission instead of inherited from another one.
    ///
    /// # What it preserves, and what it clears
    ///
    /// It always writes a full record, never a partial one. The fence takes the
    /// max only over a predecessor under THIS magic — several ECL records can be
    /// batched into one DMA buffer, exactly as several Presents can, and wire
    /// fence ids are monotonic so the largest subsumes the earlier ones.
    /// `stream_boundary` and `blt_token` are always ZEROED: a D3D12 ECL has no
    /// present stream and no windowed BLT, so inheriting either from whatever
    /// last used this buffer would attach a foreign dependency to this packet.
    ///
    /// # Safety
    /// `private_data` points to `private_size` writable bytes supplied by dxgkrnl
    /// for this Render call.
    pub(crate) unsafe fn mark_d3d12(
        private_data: *mut c_void,
        private_size: u32,
        gpu_fence_id: u64,
    ) -> Result<D3d12Mark, NTSTATUS> {
        if private_data.is_null()
            || (private_size as usize) < core::mem::size_of::<PresentSubmissionPrivate>()
        {
            return Err(STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER);
        }
        let old =
            unsafe { core::ptr::read_unaligned(private_data.cast::<PresentSubmissionPrivate>()) };
        let predecessor = old.magic == D3D12_SUBMISSION_MAGIC
            && old.version == PRESENT_SUBMISSION_VERSION
            && old.gpu_fence_id != 0;
        unsafe {
            core::ptr::write_unaligned(
                private_data.cast::<PresentSubmissionPrivate>(),
                Self::for_parts(
                    D3D12_SUBMISSION_MAGIC,
                    if predecessor {
                        old.gpu_fence_id.max(gpu_fence_id)
                    } else {
                        gpu_fence_id
                    },
                    0,
                    0,
                ),
            );
        }
        Ok(if predecessor {
            D3d12Mark::MergedWithPredecessor
        } else {
            D3d12Mark::First
        })
    }

    /// Merge one same-context registered stream boundary into this submission.
    /// A context owns at most one live stream, so repeated Present records can
    /// only advance the same generation-qualified handle.  Different handles
    /// are refused instead of being numerically compared across namespaces.
    pub(crate) unsafe fn merge_stream_boundary(
        private_data: *mut c_void,
        private_size: u32,
        boundary: u64,
    ) -> Result<(), NTSTATUS> {
        if private_data.is_null()
            || (private_size as usize) < core::mem::size_of::<PresentSubmissionPrivate>()
        {
            return Err(STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER);
        }
        // This private record carries only the opaque tagged stream namespace;
        // a legacy wire fence must stay in `gpu_fence_id` rather than being
        // reinterpreted as a stream handle.
        if boundary >> 63 != 1 || ((boundary >> 32) & 0x7fff_ffff) == 0 || boundary as u32 == 0 {
            return Err(STATUS_INVALID_PARAMETER);
        }
        let old =
            unsafe { core::ptr::read_unaligned(private_data.cast::<PresentSubmissionPrivate>()) };
        let (gpu_fence_id, old_boundary, blt_token) = if old.is_present_record() {
            (old.gpu_fence_id, old.stream_boundary, old.blt_token)
        } else {
            (0, 0, 0)
        };
        let merged_boundary = if old_boundary == 0 || old_boundary == boundary {
            boundary
        } else if (old_boundary >> 63) == 1
            && (boundary >> 63) == 1
            && ((old_boundary >> 32) & 0x7fff_ffff) == ((boundary >> 32) & 0x7fff_ffff)
        {
            // Same stream handle: values are monotonic, so the later/larger
            // value subsumes the earlier marker without crossing namespaces.
            let value = (old_boundary as u32).max(boundary as u32);
            (boundary & !0xffff_ffff) | value as u64
        } else {
            return Err(STATUS_INVALID_PARAMETER);
        };
        unsafe {
            core::ptr::write_unaligned(
                private_data.cast::<PresentSubmissionPrivate>(),
                Self::for_parts(
                    PRESENT_SUBMISSION_MAGIC,
                    gpu_fence_id,
                    merged_boundary,
                    blt_token,
                ),
            );
        }
        PRESENT_MARKER_LAST_SIZE.store(private_size, Ordering::Relaxed);
        PRESENT_MARKER_WRITES.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Carry a bounded WindowedBlt request token into the exact DMA submission
    /// that admits its destination residency. Tokens may merge only under one
    /// generation-qualified stream; the largest token then represents the
    /// same-stream prefix SubmitCommand is allowed to promote.
    pub(crate) unsafe fn merge_windowed_blt_token(
        private_data: *mut c_void,
        private_size: u32,
        token: u64,
        boundary: u64,
    ) -> Result<(), NTSTATUS> {
        if token == 0
            || boundary >> 63 != 1
            || ((boundary >> 32) & 0x7fff_ffff) == 0
            || boundary as u32 == 0
        {
            return Err(STATUS_INVALID_PARAMETER);
        }
        if private_data.is_null()
            || (private_size as usize) < core::mem::size_of::<PresentSubmissionPrivate>()
        {
            return Err(STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER);
        }
        let old =
            unsafe { core::ptr::read_unaligned(private_data.cast::<PresentSubmissionPrivate>()) };
        let (gpu_fence_id, old_boundary, old_token) = if old.is_present_record() {
            (old.gpu_fence_id, old.stream_boundary, old.blt_token)
        } else {
            (0, 0, 0)
        };
        let same_stream = old_boundary == 0
            || (old_boundary >> 63) == 1
                && ((old_boundary >> 32) & 0x7fff_ffff) == ((boundary >> 32) & 0x7fff_ffff);
        if !same_stream {
            return Err(STATUS_INVALID_PARAMETER);
        }
        let merged_boundary = if old_boundary == 0 || old_boundary == boundary {
            boundary
        } else {
            (boundary & !0xffff_ffff) | u64::from((old_boundary as u32).max(boundary as u32))
        };
        unsafe {
            core::ptr::write_unaligned(
                private_data.cast::<PresentSubmissionPrivate>(),
                Self::for_parts(
                    PRESENT_SUBMISSION_MAGIC,
                    gpu_fence_id,
                    merged_boundary,
                    old_token.max(token),
                ),
            );
        }
        PRESENT_MARKER_LAST_SIZE.store(private_size, Ordering::Relaxed);
        PRESENT_MARKER_WRITES.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Decode a scheduler submission's KMD-private data.
    ///
    /// Unknown data is deliberately treated as an ordinary submission rather
    /// than reinterpreted as a fence id.
    ///
    /// ⚠ THE TWO ARMS HAVE DIFFERENT ACCEPTANCE RULES, on purpose. Under
    /// Present's magic an all-zero payload is REJECTED: that magic can also be
    /// what a recycled buffer happens to hold, and a boundary of nothing is not
    /// worth trusting. Under [`D3D12_SUBMISSION_MAGIC`] an all-zero payload is
    /// ACCEPTED, because there the record's job is identity — `gpu_wire_fence`
    /// is legally 0 — and only `mark_d3d12` ever writes that magic.
    ///
    /// ⛔ AND THE D3D12 ARM IS CONSUMED, exactly as [`PresentFlipPrivate::take`]
    /// consumes its own record and for the same stated reason: dxgkrnl recycles
    /// these buffers between submissions, and a boundary left behind would be
    /// read a second time by whatever submission next reuses the buffer — gating
    /// that packet on a fence that has already retired, which signals an
    /// application's `ID3D12Fence` early while every counter reads healthy.
    ///
    /// The asymmetry is deliberate: Present's record is NOT consumed, because the
    /// Present path read-modify-writes it across several `merge_*` calls that
    /// legitimately accumulate into one DMA buffer, and consuming it here would
    /// change those semantics. That is why the D3D12 arm's protection is
    /// "`mark_d3d12` writes unconditionally" FIRST and this consume second: the
    /// write is what makes each submission's boundary its own, and the consume is
    /// what makes a missing write fail safe (no boundary) instead of silently
    /// inheriting the last one.
    ///
    /// Consuming can only ever make a packet wait LONGER — a second submission
    /// out of the same buffer falls back to the ordinary wire watermark — while
    /// not consuming makes it wait less than it should. That direction is the
    /// whole argument.
    ///
    /// # Safety
    /// `private_data` points to `private_size` readable-and-writable bytes
    /// supplied by dxgkrnl for this SubmitCommand call.
    pub(crate) unsafe fn decode(
        private_data: *mut c_void,
        private_size: u32,
    ) -> Option<PresentSubmissionBoundary> {
        let boundary = unsafe { Self::peek(private_data, private_size) }?;
        if boundary.d3d12 {
            // CONSUME IT — only the magic word, so a re-read finds an
            // unrecognised record rather than this boundary.
            //
            // SAFETY: `peek` proved `private_data` non-null and at least
            // `size_of::<PresentSubmissionPrivate>()` bytes; `magic` is the first
            // field of that record at offset 0.
            unsafe { core::ptr::write_unaligned(private_data.cast::<u32>(), 0) };
        }
        Some(boundary)
    }

    /// Read the record WITHOUT consuming it — the shared body of [`Self::decode`]
    /// and of the diagnostic scan, which must never mutate dxgkrnl's buffer while
    /// merely looking for an offset.
    ///
    /// # Safety
    /// `private_data` points to `private_size` readable bytes supplied by dxgkrnl.
    unsafe fn peek(
        private_data: *const c_void,
        private_size: u32,
    ) -> Option<PresentSubmissionBoundary> {
        if private_data.is_null()
            || (private_size as usize) < core::mem::size_of::<PresentSubmissionPrivate>()
        {
            return None;
        }
        let value =
            unsafe { core::ptr::read_unaligned(private_data.cast::<PresentSubmissionPrivate>()) };
        if value.version != PRESENT_SUBMISSION_VERSION {
            return None;
        }
        let d3d12 = value.magic == D3D12_SUBMISSION_MAGIC;
        let carries_boundary =
            value.gpu_fence_id != 0 || value.stream_boundary != 0 || value.blt_token != 0;
        (d3d12 || (value.magic == PRESENT_SUBMISSION_MAGIC && carries_boundary)).then_some(
            PresentSubmissionBoundary {
                gpu_fence_id: value.gpu_fence_id,
                stream_boundary: value.stream_boundary,
                blt_token: value.blt_token,
                d3d12,
            },
        )
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
                Self::peek(
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
    /// `DXGK_ALLOCATIONLIST::PhysicalAddress` — where VidMm has this
    /// allocation right now.
    ///
    /// Carried because the DMA-BUFFER FLIP path has no other source for it.
    /// On the MMIO-flip path `DxgkDdiSetVidPnSourceAddress` hands the driver
    /// the primary address explicitly; in the DMA-flip contract that DDI is
    /// never called, so the allocation list is the only place dxgkrnl states
    /// the address that a later CRTC_VSYNC must match to retire the flip.
    physical_address: u64,
    /// `DXGK_ALLOCATIONLIST::SegmentId`, alongside the address for the same
    /// reason.
    segment_id: u32,
}

impl PresentAllocation {
    #[inline]
    pub(crate) fn handle(self) -> HANDLE {
        self.handle.as_ptr()
    }

    #[inline]
    pub(crate) fn physical_address(self) -> u64 {
        self.physical_address
    }

    #[inline]
    pub(crate) fn segment_id(self) -> u32 {
        self.segment_id
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

        // SAFETY: both reads index the fixed WDK present slots of the array
        // `PresentPayload::decode` validated, and the address/segment fields
        // are plain data in the same entry as the handle.
        let source_entry = unsafe { &*allocation_list.add(DXGK_PRESENT_SOURCE_INDEX as usize) };
        let destination_entry =
            unsafe { &*allocation_list.add(DXGK_PRESENT_DESTINATION_INDEX as usize) };
        let source_handle = source_entry.hDeviceSpecificAllocation;
        let destination_handle = destination_entry.hDeviceSpecificAllocation;
        // `PhysicalAddress` and `VirtualAddress` are a union; this driver's
        // present allocations are physically addressed (the GpuMmu model is
        // decorative — see `gpummu.rs`), so the physical arm is the correct one.
        let address_of = |entry: &DXGK_ALLOCATIONLIST| -> u64 {
            // SAFETY: reading the PhysicalAddress arm of the address union.
            unsafe { entry.__bindgen_anon_2.PhysicalAddress.as_ref().QuadPart as u64 }
        };

        Self {
            source: NonNull::new(source_handle).map(|handle| PresentAllocation {
                handle,
                allocation_index: DXGK_PRESENT_SOURCE_INDEX,
                slot_id: PATCH_SLOT_SOURCE,
                driver_id: PATCH_SLOT_SOURCE,
                physical_address: address_of(source_entry),
                segment_id: source_entry.__bindgen_anon_1.SegmentId(),
            }),
            destination: NonNull::new(destination_handle).map(|handle| PresentAllocation {
                handle,
                allocation_index: DXGK_PRESENT_DESTINATION_INDEX,
                slot_id: PATCH_SLOT_DESTINATION,
                driver_id: PATCH_SLOT_DESTINATION,
                physical_address: address_of(destination_entry),
                segment_id: destination_entry.__bindgen_anon_1.SegmentId(),
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
        args.PatchLocationListOutSize =
            args.PatchLocationListOutSize.saturating_sub(written as u32);
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
