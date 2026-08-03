//! Per-D3D-device, per-context, and per-process state, plus their DDIs.
//!
//! Phase 1 implements device alloc/free (so the runtime can open a device
//! without crashing). Context and GPU-VA process DDIs are stubbed until the
//! Venus path (Phase 4) and the memory model (Phase 3) land.
//!
//! NOTE: the exact argument struct/handle types below come from the generated
//! `dxgk` bindings and may need a binding-alignment pass at first compile.

use alloc::boxed::Box;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::snapshot_bind::SnapshotDescriptor;

use crate::adapter::AdapterContext;
use crate::dxgk::*;

/// State for one D3D device opened on the adapter.
pub struct DeviceContext {
    /// Back-pointer to the owning adapter (valid for the device's lifetime).
    ///
    /// PRIVATE. The only route from a device handle to an `&AdapterContext` is
    /// [`DeviceHandleRef`]'s checked traversal — see its docs for the cast this
    /// prevents.
    adapter: *mut AdapterContext,
    /// Documented WDDM KMD-process token (`DXGKARG_CREATEDEVICE.hKmdProcess`).
    /// Present markers arrive through the UMD runtime's D3DKMT device rather
    /// than the ICD device that registered a stream, so this exact per-process
    /// object association — never a PID or current-thread inference — is the
    /// authorization edge between the two devices.  Zero means that async
    /// stream registration/marker use is refused while ordinary rendering stays
    /// available.
    creator_process: usize,
}

/// State for one scheduler context opened on a D3D device.
pub struct ContextContext {
    /// Back-pointer to the owning device (valid for the context's lifetime).
    /// PRIVATE, for the same reason as [`DeviceContext::adapter`].
    device: *mut DeviceContext,
    /// D4b snapshot descriptor STASH: the render→present handoff
    /// (FIX-DESIGN-d4b-snapshot.md §4, corrected delivery route).
    ///
    /// WHY IT EXISTS. dxgkrnl does NOT forward the UMD's PresentCb
    /// `pPrivateDriverData` to `DxgkDdiPresent` on flip presents (measured:
    /// `PBIdOk` reads "no payload" across three driver generations), so the
    /// descriptor's only working route into the KMD is the inline
    /// `HeliosPresentRenderCmd` that `DxgkDdiRender` already decodes — the
    /// same route the present-marker resid has always used. The UMD issues
    /// pfnRenderCb then pfnPresentCb back-to-back on one thread for the same
    /// hContext, so Render stashes HERE and the immediately following Present
    /// takes it.
    ///
    /// DISCIPLINE. `snap_resid == 0` = empty. The writer stores the payload
    /// fields Relaxed and the resid LAST with Release; the taker swaps the
    /// resid to 0 with Acquire and only then reads the payload — read+clear on
    /// EVERY present that resolves its context (flip, MMIO, BLT alike), so an
    /// orphaned stash (a Render whose Present failed) cannot outlive the next
    /// present on this context: staleness is bounded to one present, and the
    /// Present-time validation (extent vs the allocation list, layout,
    /// liveness at the executor) re-checks everything the stash claims.
    /// Atomics rather than plain fields because context state is reached
    /// through a shared reference; the same-thread Render→Present pairing is
    /// the expected regime, and under any cross-thread interleaving a torn
    /// stash degrades to a mix of two VALIDATED descriptors, which that same
    /// validation absorbs. The stash is plain data inside this Box — it dies
    /// with the context in `dxgkddi_destroy_context`, and dxgkrnl serializes
    /// context destruction against in-flight Render/Present on the handle, so
    /// nothing can dangle.
    snap_resid: AtomicU32,
    snap_width: AtomicU32,
    snap_height: AtomicU32,
    snap_pitch: AtomicU32,
    snap_dxgi_format: AtomicU32,
    /// Kept full-width: narrowing to u32 happens only AFTER Present-time
    /// validation proved `plane_offset <= u32::MAX` (a pre-narrow truncation
    /// could alias an invalid offset onto a valid-looking one).
    snap_plane_offset: AtomicU64,
    snap_alloc_size: AtomicU64,
    /// Registered-stream marker STASH. Exactly like the snapshot handoff, the
    /// Render→Present pair is the only documented route from UMD command bytes
    /// into the KMD-private DMA record consumed by SubmitCommand. The three
    /// fields are one lock-protected value: publishing/claiming them as separate
    /// atomics lets a later Render overwrite ctx/value after Present claims the
    /// prior cookie, cross-pairing two frames. The fixed Option neither allocates
    /// nor blocks below DISPATCH; `SpinLock` raises/restores IRQL around the
    /// handful of scalar accesses.
    present_stream_marker: crate::sync::SpinLock<Option<(u32, u32, u64)>>,
}

/// Typed borrowed view of a scheduler context handle.
///
/// DDI handle types are all C `HANDLE`s, so a direct cast can compile even when
/// the callback actually received an hContext rather than an hAdapter. Keeping
/// the traversal here makes the ownership chain explicit:
/// ContextContext -> DeviceContext -> AdapterContext.
pub struct ContextHandleRef<'a> {
    context: &'a ContextContext,
}

impl<'a> ContextHandleRef<'a> {
    /// # Safety
    /// `handle` must be a live hContext returned by [`dxgkddi_create_context`].
    pub unsafe fn from_raw(handle: HANDLE) -> Option<Self> {
        let context = unsafe { (handle as *const ContextContext).as_ref() }?;
        Some(Self { context })
    }

    pub fn adapter(&self) -> Option<&'a AdapterContext> {
        let device = unsafe { self.context.device.as_ref() }?;
        unsafe { device.adapter.as_ref() }
    }

    /// The exact documented KMD-process object token that created this
    /// context's device.
    pub fn creator_process(&self) -> Option<usize> {
        let device = unsafe { self.context.device.as_ref() }?;
        Some(device.creator_process)
    }

    /// Stash a VALIDATED-shape D4b snapshot descriptor from `DxgkDdiRender`'s
    /// `HeliosPresentRenderCmd` for the present that follows on this context.
    /// See [`ContextContext::snap_resid`] for the whole contract; the payload
    /// stores precede the resid's Release publication so the taker's Acquire
    /// swap observes a complete descriptor.
    pub fn stash_snapshot(&self, snap: &SnapshotDescriptor) {
        let ctx = self.context;
        ctx.snap_width.store(snap.width, Ordering::Relaxed);
        ctx.snap_height.store(snap.height, Ordering::Relaxed);
        ctx.snap_pitch.store(snap.pitch, Ordering::Relaxed);
        ctx.snap_dxgi_format
            .store(snap.dxgi_format, Ordering::Relaxed);
        ctx.snap_plane_offset
            .store(snap.plane_offset, Ordering::Relaxed);
        ctx.snap_alloc_size
            .store(snap.venus_alloc_size, Ordering::Relaxed);
        ctx.snap_resid.store(snap.resource_id, Ordering::Release);
    }

    /// Take (read + CLEAR) the stashed snapshot descriptor, or `None`.
    ///
    /// Called on EVERY present that resolves this context — including the
    /// MMIO/desktop and BLT arms, which never substitute — because the clear
    /// is the orphan bound: a stash whose present failed must not leak past
    /// the next present. The swap claims the descriptor exactly once.
    pub fn take_snapshot_stash(&self) -> Option<SnapshotDescriptor> {
        let ctx = self.context;
        let resid = ctx.snap_resid.swap(0, Ordering::Acquire);
        if resid == 0 {
            return None;
        }
        Some(SnapshotDescriptor {
            resource_id: resid,
            width: ctx.snap_width.load(Ordering::Relaxed),
            height: ctx.snap_height.load(Ordering::Relaxed),
            pitch: ctx.snap_pitch.load(Ordering::Relaxed),
            dxgi_format: ctx.snap_dxgi_format.load(Ordering::Relaxed),
            plane_offset: ctx.snap_plane_offset.load(Ordering::Relaxed),
            venus_alloc_size: ctx.snap_alloc_size.load(Ordering::Relaxed),
        })
    }

    /// Stash one complete nonzero stream marker for the immediately following
    /// Present on this context.  The KMD validates registry/process ownership
    /// while consuming it; this handoff only preserves the exact UMD boundary
    /// until the WDDM private-data buffer exists.
    pub fn stash_present_stream_marker(&self, ctx_id: u32, value: u32, cookie: u64) {
        if ctx_id == 0 || value == 0 || cookie == 0 {
            return;
        }
        *self.context.present_stream_marker.lock() = Some((ctx_id, value, cookie));
    }

    /// Take and clear the stream marker stash, bounding an orphaned Render to
    /// one following Present just like the snapshot descriptor.
    pub fn take_present_stream_marker_stash(&self) -> Option<(u32, u32, u64)> {
        self.context.present_stream_marker.lock().take()
    }
}

/// Typed borrowed view of a D3D **device** handle.
///
/// [`ContextHandleRef`] existed precisely because "DDI handle types are all C
/// `HANDLE`s, so a direct cast can compile even when the callback actually
/// received an hContext rather than an hAdapter" — and it was used at exactly ONE
/// site. Five other sites did the cast it was written to prevent, and both
/// back-pointer fields were `pub`, so nothing forced the checked traversal. The
/// sharpest was `create_allocation.rs`'s
/// `&*(*(h_device as *const DeviceContext)).adapter`, which dereferenced the
/// back-pointer in one expression with NO null check, unlike its two siblings.
///
/// With the fields private, the only route from a device handle to an
/// `&AdapterContext` outside this module is through here, so an adapter-scoped
/// DDI slot handed a device handle no longer compiles.
///
/// What this does NOT buy: newtyped handles (`struct HDevice(HANDLE)`) would go
/// further, but the DDI signatures are bindgen-fixed at `*mut c_void`, so an
/// `unsafe fn from_raw` is still needed at each entry. The win is that the cast
/// happens once per DDI in one module instead of inline in four subsystems.
pub struct DeviceHandleRef<'a> {
    device: &'a DeviceContext,
}

impl<'a> DeviceHandleRef<'a> {
    /// # Safety
    /// `handle` must be a live hDevice returned by [`dxgkddi_create_device`].
    /// That it really is one is dxgkrnl's contract and is not encodable; the null
    /// check below is the only part we can enforce.
    pub unsafe fn from_raw(handle: HANDLE) -> Option<Self> {
        let device = unsafe { (handle as *const DeviceContext).as_ref() }?;
        Some(Self { device })
    }

    pub fn adapter(&self) -> Option<&'a AdapterContext> {
        unsafe { self.device.adapter.as_ref() }
    }

    /// The raw back-pointer, for the one caller that stores it rather than
    /// borrowing through it (`scheduler.rs`'s `HwContext`).
    pub fn adapter_ptr(&self) -> *mut AdapterContext {
        self.device.adapter
    }

    /// The exact documented KMD-process object token that created this device.
    pub fn creator_process(&self) -> usize {
        self.device.creator_process
    }
}

/// State for one GPU process object (WDDM 2.0 GPU-VA requirement). We keep no
/// per-process GPU virtual address space (host-owned VA), but dxgkrnl requires a
/// non-NULL driver handle it can round-trip through every per-process DDI and
/// hand back at DestroyProcess, so we allocate a real object to back the handle.
///
/// DELIBERATELY EMPTY. It used to carry an `adapter` back-pointer that was
/// written at CreateProcess and never read, which implied an ownership edge that
/// does not exist. The allocation stays — dxgkrnl needs the non-NULL handle — but
/// the object is an opaque token and nothing more.
pub struct ProcessContext {
    /// Zero-sized structs still get a unique non-null address from `Box`, but a
    /// field makes the allocation's purpose (a handle dxgkrnl can round-trip)
    /// legible and keeps `Box::into_raw` returning a real pointer under any
    /// future allocator.
    _opaque: u32,
}

/// `DxgkDdiCreateDevice` — allocate per-device state.
pub unsafe extern "C" fn dxgkddi_create_device(
    miniport_device_context: *mut c_void,
    create_device: *mut DXGKARG_CREATEDEVICE,
) -> NTSTATUS {
    if miniport_device_context.is_null() || create_device.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: Dxgkrnl passes our adapter context and a valid args struct.
    let args = unsafe { &mut *create_device };
    let ctx = Box::new(DeviceContext {
        adapter: miniport_device_context as *mut AdapterContext,
        creator_process: args.hKmdProcess as usize,
    });
    // Hand the device handle back to Dxgkrnl; reclaimed in destroy_device.
    args.hDevice = Box::into_raw(ctx) as *mut c_void;
    STATUS_SUCCESS
}

/// `DxgkDdiDestroyDevice` — free per-device state, after unmapping any host-visible
/// blob views this device opened (Gate 5a Stage 2b). The user VAs were mapped by
/// `HELIOS_ESCAPE_MAP_BLOB` (tagged with this `h_device` as owner) and MUST be
/// unmapped here — in the creating process, at PASSIVE_LEVEL — or the kernel
/// bugchecks `0x76 PROCESS_HAS_LOCKED_PAGES` at process exit. This DDI runs in the
/// context of the thread destroying the device (the ICD's process), so the unmap is
/// in-process. The mapping table is on the AdapterContext (independent spinlock), so
/// teardown is correct even if the virtio transport is already gone.
/// Mappings harvested per spinlock acquisition in DestroyDevice.
///
/// 64 pairs = 1 KiB of stack, which is affordable on the PASSIVE DestroyDevice
/// frame and turns an 8192-mapping teardown from 8192 acquisitions into 128.
const MAPPING_DRAIN_BATCH: usize = 64;

pub unsafe extern "C" fn dxgkddi_destroy_device(h_device: *mut c_void) -> NTSTATUS {
    if !h_device.is_null() {
        // SAFETY: h_device came from Box::into_raw in create_device; its `adapter`
        // back-pointer is valid for the device's lifetime.
        let Some(adapter) =
            (unsafe { DeviceHandleRef::from_raw(h_device) }).and_then(|d| d.adapter())
        else {
            // The Box still has to be reclaimed even if the back-pointer is
            // somehow null — leaking it would be the worse failure.
            // SAFETY: produced by Box::into_raw in create_device.
            drop(unsafe { Box::from_raw(h_device as *mut DeviceContext) });
            return STATUS_SUCCESS;
        };
        let owner = h_device as usize;
        // SAFETY: `DxgkDdiDestroyDevice` is documented "IRQL: PASSIVE_LEVEL" (WDK
        // DXGKDDI_DESTROYDEVICE), and the unmap loop below already depends on it
        // — `MmUnmapLockedPages` is PASSIVE-only, which is exactly why the table
        // lock is dropped between batches. Minted HERE rather than further down
        // because the diag dumps between also require it; still ONE mint for
        // this DDI.
        let passive = unsafe { crate::irql::PassiveLevel::assume() };
        // Drain THIS device's mappings in batches, unmapping outside the table
        // lock (MmUnmapLockedPages needs PASSIVE; the table lock raises to
        // DISPATCH). One acquisition per entry was O(n) acquisitions and O(n^2)
        // comparisons, and MAX_MAPPINGS is 8192 because a DOOM level load really
        // does hold thousands.
        let mut batch = [(0u64, 0usize); MAPPING_DRAIN_BATCH];
        loop {
            let n = adapter.mappings.drain_for(owner, &mut batch);
            if n == 0 {
                break;
            }
            for &(user_va, mdl) in &batch[..n] {
                // SAFETY: PASSIVE_LEVEL in the creating process; pair from a
                // prior MAP_BLOB on this device handle.
                unsafe { crate::ddi::unmap_io_pages_from_user(user_va, mdl as *mut wdk_sys::MDL) };
            }
        }
        // Reclaim any virtio blobs / contexts this device allocated but did not
        // release (ICD crash, or a process that skipped RELEASE_BLOB/CTX_DESTROY —
        // e.g. a crash-looping client). Without this the bounded blob table fills
        // across device creations and later ALLOC_BLOBs fail STATUS_INSUFFICIENT_
        // RESOURCES, surfacing as spurious "venus wedge" / render corruption. If
        // the transport is already gone (StopDevice), there is nothing to reclaim.
        // DIAG: 0x0E00_0001 = DestroyDevice entry (unconditional). Low 16 bits of
        // the owning handle so we can correlate with ALLOC_BLOB's owner.
        crate::diag::record(0x0E00_0001);
        crate::diag::record(0x0E01_0000 | ((owner as u32) & 0xFFFF));
        // Mirror the GpuMmu page-table-DDI tracers into the PASSIVE ring so the
        // post-CreateContext Code-43 failure stage is visible over SSH without
        // ntoseye (Step-2 decorative-GpuMmu bring-up).
        crate::ddi::diag_dump_gpummu_atomics(passive);
        // Engine-path tracers (SubmitCommand/Render/Patch/ISR/DPC counts): show
        // whether VidSch exercised the submission engine at all before the
        // post-CreateContext VidSchTerminateAdapter Code-43 (Step-2 coherent fence).
        crate::ddi::diag_dump_engine_atomics();
        // Present-path tracers: the steady-state registry ring is too noisy for
        // per-call breadcrumbs, so mirror the latest cross-adapter present args
        // here at PASSIVE_LEVEL.
        crate::ddi::diag_dump_present_atomics();
        // D4a: drop this device's scanout retirement-event registrations —
        // dereference ONLY, no signal (the process is exiting; a wake would
        // land nowhere). Its read-ledger page mapping needs nothing here: it
        // rides the MappingTable and was unmapped by the drain above.
        adapter.read_ledger.reclaim_events_for_owner(owner);
        // Sweep exactly this device's slots. A null hDevice would sweep the
        // KMD-owned ones, so the token is minted rather than cast.
        let device_owner = crate::virtio::gpu::DeviceOwner::new(owner);
        // Present-stream slots borrow this DeviceContext's KMD-process token,
        // so purge them before the device handle can disappear.
        let purged_streams = adapter.with_wddm_notify_lock(|guard| {
            guard
                .with_virtio(|order, v| v.purge_present_streams_for_owner(order, device_owner))
                .unwrap_or(0)
        });
        if purged_streams != 0 {
            crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
        }
        let before = adapter.with_virtio(|v| v.blob_count() as u32).unwrap_or(0);
        let blobs = crate::virtio::ctrl::release_blobs_for_owner(passive, adapter, device_owner);
        let contexts =
            crate::virtio::ctrl::destroy_contexts_for_owner(passive, adapter, device_owner);
        // Opportunistic PASSIVE reap of completed transport entries.
        crate::virtio::ctrl::reap_parked(passive, adapter);
        // 0x0E02_BBBB = blob-table size BEFORE reclaim (saturated to 16 bits).
        crate::diag::record(0x0E02_0000 | before.min(0xFFFF));
        // 0x0E03_RRCC = reclaimed blobs (RR) + contexts (CC).
        crate::diag::record(0x0E03_0000 | ((blobs.min(0xFF) << 8) | contexts.min(0xFF)));
        // SAFETY: produced by Box::into_raw in create_device; destroyed exactly once.
        drop(unsafe { Box::from_raw(h_device as *mut DeviceContext) });
    }
    STATUS_SUCCESS
}

/// `DxgkDdiCreateContext` — GPU execution context.
///
// STUB: Phase 4 — create the Venus virtio-gpu context here.
// NOTE: a TEMP int3 debug breakpoint lived here during the Step-2 GpuMmu bring-up
// and was removed (ntoseye is a gdbstub, not a Windows KD — a bare int3 BSODs the
// guest instead of trapping). Use a spin-loop released via ntoseye write_memory if
// a live pause is needed. This comment also forces a clean recompile.
pub unsafe extern "C" fn dxgkddi_create_context(
    h_device: *mut c_void,
    create_context: *mut DXGKARG_CREATECONTEXT,
) -> NTSTATUS {
    crate::diag::record(0x0800_0001);
    if h_device.is_null() || create_context.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    let args = unsafe { &mut *create_context };
    let ctx = Box::new(ContextContext {
        device: h_device as *mut DeviceContext,
        snap_resid: AtomicU32::new(0),
        snap_width: AtomicU32::new(0),
        snap_height: AtomicU32::new(0),
        snap_pitch: AtomicU32::new(0),
        snap_dxgi_format: AtomicU32::new(0),
        snap_plane_offset: AtomicU64::new(0),
        snap_alloc_size: AtomicU64::new(0),
        present_stream_marker: crate::sync::SpinLock::new(None),
    });
    args.hContext = Box::into_raw(ctx) as HANDLE;

    // Use the paging aperture for DMA buffers. With the decorative GpuMmu model,
    // dxgkrnl's CDD context creates a privileged DMA pool with GPU-VA mapping
    // enabled; if this is 0, dxgmms2 uses contiguous system memory, skips creating
    // a VIDMM allocation object, then later dereferences that null allocation in
    // VidMmInitDmaPool. A nonzero aperture segment set makes VidMm back the pool
    // through the normal aperture allocation path.
    args.ContextInfo.DmaBufferSegmentSet = 1; // segment id 1 (aperture)
    args.ContextInfo.DmaBufferSize = 256 * 1024;
    // ONE definition site: present_packet.rs, beside the two records that live
    // in the buffer and the compile-time proof they fit it (40 -> 80 with the
    // D4b snapshot descriptor plus the stream-boundary scheduler handoff).
    args.ContextInfo.DmaBufferPrivateDataSize =
        crate::ddi::present_packet::PRESENT_DMA_PRIVATE_DATA_BYTES;
    args.ContextInfo.AllocationListSize = DXGK_ALLOCATION_LIST_SIZE_GDICONTEXT;
    args.ContextInfo.PatchLocationListSize = DXGK_ALLOCATION_LIST_SIZE_GDICONTEXT;

    STATUS_SUCCESS
}

/// `DxgkDdiDestroyContext`.
// STUB: Phase 4 — tear down the Venus context.
pub unsafe extern "C" fn dxgkddi_destroy_context(h_context: *mut c_void) -> NTSTATUS {
    crate::diag::record(0x0800_0002);
    if !h_context.is_null() {
        drop(unsafe { Box::from_raw(h_context as *mut ContextContext) });
    }
    STATUS_SUCCESS
}

/// `DxgkDdiCreateProcess` — GPU-VA process object (WDDM 2.0 requirement).
///
/// dxgkrnl creates a process object during GPU-VA adapter bring-up and expects a
/// non-NULL driver handle back in `hKmdProcess`; leaving this a
/// `STATUS_NOT_IMPLEMENTED` stub fails post-StartDevice (one of the Code-43
/// triggers). We allocate a `ProcessContext`, hand its pointer back as the
/// handle, and reclaim it in DestroyProcess. No GPU virtual address space is
/// tracked (host-owned VA — see build_paging_buffer.rs).
pub unsafe extern "C" fn dxgkddi_create_process(
    miniport_device_context: *mut c_void,
    args: *mut DXGKARG_CREATEPROCESS,
) -> NTSTATUS {
    // DIAG: confirm dxgkrnl reaches CreateProcess during AddAdapter.
    crate::diag::record(0x0600_0000);
    if miniport_device_context.is_null() || args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: Dxgkrnl passes our adapter context and a valid args struct.
    let args = unsafe { &mut *args };
    // The handle is an opaque token: dxgkrnl only round-trips it. The old
    // `adapter` back-pointer here was written and never read.
    let ctx = Box::new(ProcessContext { _opaque: 0 });
    // Hand the process handle back to Dxgkrnl; reclaimed in destroy_process.
    args.hKmdProcess = Box::into_raw(ctx) as HANDLE;
    STATUS_SUCCESS
}

/// `DxgkDdiDestroyProcess` — free the per-process state from CreateProcess.
pub unsafe extern "C" fn dxgkddi_destroy_process(
    _miniport_device_context: *mut c_void,
    h_process: *mut c_void,
) -> NTSTATUS {
    if !h_process.is_null() {
        // SAFETY: h_process was produced by Box::into_raw in create_process and
        // is destroyed exactly once.
        drop(unsafe { Box::from_raw(h_process as *mut ProcessContext) });
    }
    STATUS_SUCCESS
}
