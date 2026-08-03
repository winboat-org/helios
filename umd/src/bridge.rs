//! # Where `unsafe` sits on these declarations (R814)
//!
//! Rust's one memory-safety signal used to be attached to the wrong six
//! declarations here, so a reviewer scanning for `unsafe` call sites looked in
//! the wrong places. Three pointer-laundering entry points were SAFE
//! (`set_resource_kmt_handles`, `transfer_resource_ownership`,
//! `open_ddi_texture2d`) while three that take only scalars and cannot violate
//! memory safety were `unsafe fn` (`present_frame_gate`, and
//! `present_sync_fence_id` / `present_flip_wait_arm`, both retired with the
//! kwait subsystem in T6/R912a).
//!
//! Scope limit worth stating, because it changes the finding's shape: cxx
//! REQUIRES `unsafe fn` for any signature containing a raw pointer, and every
//! raw-pointer declaration in this block already is. Those are correct and are
//! not touched. `present_vehicle_copy` takes `usize` COM pointers AND is
//! already unsafe, which is the correct end state; it is left alone too
//! (`present_sync_publish` was its twin until R912a retired it).

//! cxx bridge to DXVK's C++ engine.
//!
//! The UMD's `d3d10umddi` frontend (Rust) calls into DXVK's `DxvkInstance`/
//! `DxvkAdapter`/`DxvkDevice` through this bridge. The C++ side (`bridge/
//! dxvk_bridge.cpp`) owns the DXVK `Rc<>` objects inside an opaque
//! `HeliosDxvkDevice`; Rust holds it via `UniquePtr`.
//!
//! Backend Vulkan device = the Gate-5a venus ICD; the shim force-selects it via
//! `DXVK_FILTER_DEVICE_NAME="Virtio-GPU Venus"` before creating the instance.

#[cxx::bridge]
mod ffi {
    unsafe extern "C++" {
        include!("dxvk_bridge.h");

        /// Opaque holder for the DXVK instance + adapter + device + the DXVK
        /// D3D11 COM device the DDI forwards to.
        type HeliosDxvkDevice;

        /// Raw `ID3D11Device*` / `ID3D11DeviceContext*` (as usize) the DDI
        /// device-funcs forward to. 0 if not created. Borrowed — the bridge keeps
        /// the owning ref; wrap on the Rust side without taking ownership.
        fn d3d11_device_ptr(self: &HeliosDxvkDevice) -> usize;
        fn d3d11_context_ptr(self: &HeliosDxvkDevice) -> usize;
        fn venus_context_id(self: &HeliosDxvkDevice) -> u32;
        /// # Safety
        /// `deferred_context_ptr` is borrowed and live. `command_list_ptr`
        /// transfers one owned COM reference on true; false leaves ownership
        /// with the caller, which must reconstruct and release it.
        unsafe fn recycle_deferred_command_list(
            self: &HeliosDxvkDevice,
            deferred_context_ptr: usize,
            command_list_ptr: usize,
        ) -> bool;
        /// # Safety
        /// `deferred_context_ptr` must be a live deferred context created by
        /// this bridge immediately before this call.
        unsafe fn enable_deferred_context_ddi_logical_reset(
            self: &HeliosDxvkDevice,
            deferred_context_ptr: usize,
        ) -> bool;
        /// # Safety
        /// `d3d11_resource_ptr` must be a live `ID3D11Resource*`; the bridge
        /// `reinterpret_cast`s it and calls `GetCommonTexture` on it.
        unsafe fn set_resource_kmt_handles(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptr: usize,
            local: u32,
            global: u32,
        ) -> bool;
        unsafe fn get_resource_memory_info(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptr: usize,
            memory: *mut u64,
            size: *mut u64,
            offset: *mut u64,
            resource_id: *mut u32,
        ) -> bool;
        /// C1 identity: exact creating-`vkAllocateMemory` size + memoryTypeIndex
        /// of the resource's backing venus memory (recorded into the WDDM
        /// allocation trailer for cross-process openers).
        unsafe fn get_resource_alloc_identity(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptr: usize,
            venus_alloc_size: *mut u64,
            memory_type_index: *mut u32,
        ) -> bool;
        /// # Safety
        /// `d3d11_resource_ptr` must be a live `ID3D11Resource*`.
        unsafe fn transfer_resource_ownership(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptr: usize,
        ) -> bool;
        /// # Safety
        /// Returns an OWNED COM pointer the caller must release; the safe
        /// wrapper `open_texture2d` is the only thing that should call it.
        unsafe fn open_ddi_texture2d(
            self: &HeliosDxvkDevice,
            width: u32,
            height: u32,
            format: u32,
            bind_flags: u32,
            misc_flags: u32,
            global: u32,
            renderer_resource_id: u32,
            venus_alloc_size: u64,
            memory_type_index: u32,
            scanout_linear: bool,
            linear_scanout_target: bool,
            cross_context_optimal: bool,
        ) -> usize;

        /// Create the DWM scan-out primary as a dedicated OPTIMAL,
        /// DMA_BUF-exportable image and report logical scanout metadata for
        /// exact host reconstruction.
        /// Returns an owned `ID3D11Resource*` (as usize), or 0 on failure.
        unsafe fn create_ddi_scanout_texture2d(
            self: &HeliosDxvkDevice,
            width: u32,
            height: u32,
            format: u32,
            bind_flags: u32,
            misc_flags: u32,
            out_row_pitch: *mut u64,
            out_offset: *mut u64,
        ) -> usize;

        unsafe fn create_vertex_shader(
            self: &HeliosDxvkDevice,
            code: *const u8,
            len: usize,
        ) -> usize;
        unsafe fn create_pixel_shader(
            self: &HeliosDxvkDevice,
            code: *const u8,
            len: usize,
        ) -> usize;
        /// >=11.1 DDI shader create carrying the typed I/O signatures. `kind`:
        /// 0 = vertex, 1 = pixel, 2 = geometry. `sig_words` layout:
        /// [n_in, n_out, (sysval, register, mask, comptype, stream) x n_in,
        /// the same x n_out].
        unsafe fn create_shader_sig(
            self: &HeliosDxvkDevice,
            kind: u32,
            code: *const u8,
            len: usize,
            sig_words: *const u32,
            sig_words_len: usize,
        ) -> usize;
        /// Tessellation shader create carrying input/output/patch-constant
        /// signatures. `kind`: 0 = hull, 1 = domain. `sig_words` layout:
        /// [n_in, n_out, n_patch, then (sysval, register, mask, comptype,
        /// stream) entries for each group].
        unsafe fn create_tess_shader_sig(
            self: &HeliosDxvkDevice,
            kind: u32,
            code: *const u8,
            len: usize,
            sig_words: *const u32,
            sig_words_len: usize,
        ) -> usize;
        /// Flip-model identity rotation: texture i takes texture i+1's DXVK
        /// storage (memory + VkImage + KMT handles); the last takes the
        /// first's. The swap executes on the CS thread (ordered); no drain.
        unsafe fn rotate_resource_backings(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptrs: *const usize,
            count: usize,
        ) -> bool;
        /// Present-path frame-completion gate: bounded wait (timeout_us)
        /// until the current flush's submission completes on the GPU, so the
        /// IddCx consumer never copies a buffer whose writes are in flight.
        /// Returns false on timeout (caller proceeds — bounded by design).
        fn present_frame_gate(
            self: &HeliosDxvkDevice,
            timeout_us: u32,
            order_mode: u32,
        ) -> bool;

        /// # Safety
        /// The three output pointers are live writable u32/u32/u64 storage for
        /// the duration of the call. `d3d11_resource_ptr` is a live D3D11
        /// resource COM pointer, as documented by the C++ bridge method.
        unsafe fn publish_present_order(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptr: usize,
            out_ctx_id: *mut u32,
            out_value32: *mut u32,
            out_cookie: *mut u64,
        ) -> bool;

        /// D4a scanout acquire: hand the per-device KMD retirement event to
        /// the DXVK device's signaler thread (auto-reset HANDLE as usize,
        /// never 0). The UMD keeps ownership — DXVK only waits on it, and
        /// DestroyDevice closes it after the bridge device has dropped (the
        /// signaler joins inside ~DxvkDevice, so no waiter can outlive the
        /// handle).
        fn set_scanout_acquire_event(self: &HeliosDxvkDevice, event_handle: usize) -> bool;
        /// Dcomp present vehicle: image-level copy of the imported ICD frame
        /// (src) into the vehicle backbuffer texture (dst), sourcing the
        /// import's LIVE storage (staging alias when present). The copy-time
        /// consumer present-wait orders it against the producer's GPU
        /// writes. 0 = ok, 1 = copied with a (counted) geometry mismatch,
        /// negative = failure — fail the present loudly, do not flip.
        unsafe fn present_vehicle_copy(
            self: &HeliosDxvkDevice,
            dst_resource_ptr: usize,
            src_resource_ptr: usize,
        ) -> i32;
        /// D4b snapshot ring: image-level copy of the presented primary (src)
        /// into a snapshot-ring image (dst), recorded on the open command
        /// list BEFORE the present-time Flush so it rides frame N's own
        /// command stream (queue-ordered after the frame's draws — no waits,
        /// no CPU stalls). Clone of `present_vehicle_copy` minus the
        /// staging-alias substitution: both operands are this device's own
        /// images, never imports. 0 = ok, 1 = copied with a (counted)
        /// geometry mismatch — the caller must NOT substitute the descriptor
        /// for this present — negative = failure (present exactly as today).
        unsafe fn present_snapshot_copy(
            self: &HeliosDxvkDevice,
            dst_resource_ptr: usize,
            src_resource_ptr: usize,
        ) -> i32;
        unsafe fn create_geometry_shader(
            self: &HeliosDxvkDevice,
            code: *const u8,
            len: usize,
        ) -> usize;
        unsafe fn create_hull_shader(self: &HeliosDxvkDevice, code: *const u8, len: usize)
            -> usize;
        unsafe fn create_domain_shader(
            self: &HeliosDxvkDevice,
            code: *const u8,
            len: usize,
        ) -> usize;
        unsafe fn create_compute_shader(
            self: &HeliosDxvkDevice,
            code: *const u8,
            len: usize,
        ) -> usize;

        /// Create a DXVK instance and logical device on the Helios venus adapter.
        ///
        /// `luid_low`/`luid_high` identify the WDDM adapter to match; pass `(0, 0)`
        /// to take the first enumerated adapter. Returns a null `UniquePtr` on
        /// failure (no adapter, device creation threw, etc.). Never panics across
        /// the FFI boundary — the C++ side catches all exceptions.
        fn helios_dxvk_create_device(luid_low: u32, luid_high: i32) -> UniquePtr<HeliosDxvkDevice>;
    }
}

// ---------------------------------------------------------------------------
// Safe wrappers: one owned/borrowed decision per bridge entry point.
// ---------------------------------------------------------------------------
//
// Thirteen bridge methods return a COM pointer as a bare `usize`. Two are
// BORROWED -- the bridge keeps the owning reference -- and eleven are OWNED and
// the Rust side must `Release`. Before R813 that discipline was a doc comment on
// one side and a `// SAFETY:` comment on the other, repeated at every call site.
// Every existing site was correct; the exposure was entirely future sites, and
// both failure modes are silent:
//
//   * adopting a borrowed pointer  -> a double release. `ID3D11Resource::
//     from_raw(dev.dxvk.d3d11_device_ptr() as *mut c_void)` is type-correct,
//     compiles, and drops the device's only reference at end of scope --
//     destroying the D3D11 device under a running DDI.
//   * wrapping an owned pointer in `ManuallyDrop` -> a leak.
//
// Each surfaces as a much later crash in dwm. The wrappers below make the
// correct adoption exist in exactly ONE place per entry point; R815 is what
// makes the wrong one unreachable.

use core::ffi::c_void;
use core::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Resource};

/// Adopt an owned COM pointer the bridge returned, or `None` for its 0-failure
/// sentinel. The single `from_raw` for every owning bridge entry point.
///
/// # Safety
/// `raw`, when non-zero, must be an `ID3D11Resource*` whose reference the
/// bridge transferred to this caller.
unsafe fn adopt_resource(raw: usize) -> Option<ID3D11Resource> {
    (raw != 0).then(|| unsafe { ID3D11Resource::from_raw(raw as *mut c_void) })
}

impl ffi::HeliosDxvkDevice {
    // -- borrowed ----------------------------------------------------------
    //
    // `ManuallyDrop` is the whole point: the bridge owns the reference, so the
    // returned wrapper must never release it.

    pub(crate) fn d3d11_device(&self) -> Option<ManuallyDrop<ID3D11Device>> {
        let p = self.d3d11_device_ptr();
        // SAFETY: a non-zero `d3d11_device_ptr` is the bridge's live
        // ID3D11Device, kept alive by the bridge for as long as this device
        // exists. ManuallyDrop borrows it without taking a reference.
        (p != 0).then(|| ManuallyDrop::new(unsafe { ID3D11Device::from_raw(p as *mut c_void) }))
    }

    pub(crate) fn d3d11_context(&self) -> Option<ManuallyDrop<ID3D11DeviceContext>> {
        let p = self.d3d11_context_ptr();
        // SAFETY: as above, for the immediate context.
        (p != 0)
            .then(|| ManuallyDrop::new(unsafe { ID3D11DeviceContext::from_raw(p as *mut c_void) }))
    }

    // -- owned -------------------------------------------------------------

    /// # Safety
    /// Caller upholds `open_ddi_texture2d`'s preconditions (a live KMT handle
    /// and a renderer resource id the host still has).
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn open_texture2d(
        &self,
        width: u32,
        height: u32,
        format: u32,
        bind_flags: u32,
        misc_flags: u32,
        global: u32,
        renderer_resource_id: u32,
        venus_alloc_size: u64,
        memory_type_index: u32,
        scanout_linear: bool,
        linear_scanout_target: bool,
        cross_context_optimal: bool,
    ) -> Option<ID3D11Resource> {
        // SAFETY: the caller upholds the resource-id/handle preconditions
        // above, and the bridge transfers one reference on success.
        unsafe {
            adopt_resource(self.open_ddi_texture2d(
                width,
                height,
                format,
                bind_flags,
                misc_flags,
                global,
                renderer_resource_id,
                venus_alloc_size,
                memory_type_index,
                scanout_linear,
                linear_scanout_target,
                cross_context_optimal,
            ))
        }
    }

    /// The DWM scan-out primary as a dedicated OPTIMAL, DMA_BUF-exportable
    /// image, plus the logical scan-out metadata the host reconstructs from.
    pub(crate) fn create_scanout_texture2d(
        &self,
        width: u32,
        height: u32,
        format: u32,
        bind_flags: u32,
        misc_flags: u32,
    ) -> Option<(ID3D11Resource, u64, u64)> {
        let mut row_pitch: u64 = 0;
        let mut offset: u64 = 0;
        // SAFETY: both out-params point at live locals; the bridge zeroes them
        // on entry and writes them before returning non-zero.
        let raw = unsafe {
            self.create_ddi_scanout_texture2d(
                width,
                height,
                format,
                bind_flags,
                misc_flags,
                &mut row_pitch,
                &mut offset,
            )
        };
        // SAFETY: the bridge transfers one reference on success.
        unsafe { adopt_resource(raw) }.map(|r| (r, row_pitch, offset))
    }
}

// NOT wrapped, deliberately: the eight shader creates.
//
// R813 suggests an `Option<usize>` (or NonZero) wrapper for them too. Audited
// instead: all TEN shader-create call sites already guard the bridge's
// 0-failure sentinel with `if raw != 0` before `store_raw_com`, and storing a
// zero would be harmless anyway -- `load_com` null-checks the slot. There is no
// wrong-adoption hazard here either, because the result goes into a slot as a
// raw word rather than being wrapped as owned or borrowed. A newtype would be
// ceremony across ten correct sites, so it is left out by the review's own
// "rejected as cosmetic" standard.

// ---------------------------------------------------------------------------
// BridgeDevice: the sealed public API (R815)
// ---------------------------------------------------------------------------
//
// After R813 the safe wrappers exist, but the raw `usize`-returning methods
// remain callable at every site, so nothing prevents a future caller choosing
// the wrong adoption. Module privacy alone is NOT sufficient: cxx generates the
// raw methods as INHERENT methods on the public opaque type, and inherent
// methods of a re-exported public type stay callable regardless of module
// visibility. A newtype with no `Deref` is the only encoding that actually
// seals them -- which is why `BridgeDevice` deliberately has none, and why
// `inner` is private.
//
// The C++ side still returns `usize`, so the ABI is unchanged and this
// migration cannot break the wire.

/// A **source** resource pointer for the present path.
///
/// `present_vehicle_copy(dst, src)` took the same two COM pointers in the
/// OPPOSITE order to the neighbouring `present_sync_publish(src, dst)` (retired
/// in R912a), they were called ~30 lines apart in the same function, and a
/// transposition compiled cleanly on both sides of the FFI. Transposing
/// `present_vehicle_copy` is ALWAYS harmful -- the bridge copies the vehicle
/// backbuffer INTO the imported ICD frame, the geometry check passes because
/// both are the same size, `EXT_GEOM_MISMATCH` never fires, and the flipped
/// backbuffer shows whatever it held last frame. That is exactly the
/// stale-frame symptom class this project has already spent multiple sessions
/// chasing.
///
/// With `SrcRes`/`DstRes` the transposition is a type error regardless of
/// parameter order. R816.
#[derive(Clone, Copy)]
pub(crate) struct SrcRes(pub(crate) usize);

/// A **destination** resource pointer for the present path. See [`SrcRes`].
#[derive(Clone, Copy)]
pub(crate) struct DstRes(pub(crate) usize);

/// The optional KMD correlation for one ordinary present's exact producer
/// submission. Zero is the only fallback representation: callers cannot carry
/// a partial `{ctx,value,cookie}` into a KMD marker.
#[derive(Clone, Copy, Default)]
pub(crate) struct PresentStreamCorrelation {
    pub(crate) ctx_id: u32,
    pub(crate) value32: u32,
    pub(crate) cookie: u64,
}

impl PresentStreamCorrelation {
    pub(crate) fn is_complete(self) -> bool {
        self.ctx_id != 0 && self.value32 != 0 && self.cookie != 0
    }
}

/// The DXVK bridge device, with the raw cxx surface sealed off.
pub struct BridgeDevice {
    inner: cxx::UniquePtr<ffi::HeliosDxvkDevice>,
}

impl BridgeDevice {
    /// Create a DXVK instance and logical device on the Helios venus adapter.
    /// `None` when the bridge returned a null device (no adapter, creation
    /// threw, ...) -- folding the old `is_null()` check into construction so a
    /// `BridgeDevice` that exists is always usable.
    pub fn create(luid_low: u32, luid_high: i32) -> Option<Self> {
        let inner = ffi::helios_dxvk_create_device(luid_low, luid_high);
        (!inner.is_null()).then_some(Self { inner })
    }

    /// The only path from the newtype to the sealed type, and it is private.
    fn get(&self) -> Option<&ffi::HeliosDxvkDevice> {
        self.inner.as_ref()
    }

    // -- borrowed COM ------------------------------------------------------

    pub(crate) fn d3d11_device(&self) -> Option<ManuallyDrop<ID3D11Device>> {
        self.get()?.d3d11_device()
    }

    pub(crate) fn d3d11_context(&self) -> Option<ManuallyDrop<ID3D11DeviceContext>> {
        self.get()?.d3d11_context()
    }

    /// # Safety
    /// `deferred_context_ptr` must be a live DXVK deferred interface.
    /// `command_list_ptr` transfers one owned COM reference iff this returns
    /// true; false leaves it owned by the caller.
    pub(crate) unsafe fn recycle_deferred_command_list(
        &self,
        deferred_context_ptr: usize,
        command_list_ptr: usize,
    ) -> bool {
        self.get().is_some_and(|d| unsafe {
            d.recycle_deferred_command_list(deferred_context_ptr, command_list_ptr)
        })
    }

    /// # Safety
    /// `deferred_context_ptr` must be the newly-created live DXVK deferred
    /// context owned by this UMD device.
    pub(crate) unsafe fn enable_deferred_context_ddi_logical_reset(
        &self,
        deferred_context_ptr: usize,
    ) -> bool {
        self.get().is_some_and(|d| unsafe {
            d.enable_deferred_context_ddi_logical_reset(deferred_context_ptr)
        })
    }

    // -- owned COM ---------------------------------------------------------

    /// # Safety
    /// See [`ffi::HeliosDxvkDevice::open_texture2d`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn open_texture2d(
        &self,
        width: u32,
        height: u32,
        format: u32,
        bind_flags: u32,
        misc_flags: u32,
        global: u32,
        renderer_resource_id: u32,
        venus_alloc_size: u64,
        memory_type_index: u32,
        scanout_linear: bool,
        linear_scanout_target: bool,
        cross_context_optimal: bool,
    ) -> Option<ID3D11Resource> {
        unsafe {
            self.get()?.open_texture2d(
                width,
                height,
                format,
                bind_flags,
                misc_flags,
                global,
                renderer_resource_id,
                venus_alloc_size,
                memory_type_index,
                scanout_linear,
                linear_scanout_target,
                cross_context_optimal,
            )
        }
    }

    pub(crate) fn create_scanout_texture2d(
        &self,
        width: u32,
        height: u32,
        format: u32,
        bind_flags: u32,
        misc_flags: u32,
    ) -> Option<(ID3D11Resource, u64, u64)> {
        self.get()?
            .create_scanout_texture2d(width, height, format, bind_flags, misc_flags)
    }

    // -- scalar passthroughs ----------------------------------------------

    pub(crate) fn venus_context_id(&self) -> u32 {
        self.get().map_or(0, |d| d.venus_context_id())
    }

    pub(crate) fn present_frame_gate(&self, timeout_us: u32, order_mode: u32) -> bool {
        self.get()
            .is_some_and(|d| d.present_frame_gate(timeout_us, order_mode))
    }

    /// Publish this present on the device's named timeline so a consumer can
    /// order its read GPU-side. `d3d11_resource_ptr` is the presented source
    /// resource's COM pointer; the bridge derives the venus resource id the
    /// consumer imports by. Slot publication remains the boolean result; the
    /// optional stream correlation is fully zero unless all three fields name
    /// this exact signal.
    pub(crate) fn publish_present_order(
        &self,
        d3d11_resource_ptr: usize,
    ) -> (bool, PresentStreamCorrelation) {
        let Some(d) = self.get() else {
            return (false, PresentStreamCorrelation::default());
        };
        let mut correlation = PresentStreamCorrelation::default();
        // SAFETY: `d` is the bridge-owned live C++ device and all three
        // out-pointers borrow initialized local fields for this synchronous
        // call. The C++ bridge zeroes them before reporting failure.
        let published = unsafe {
            d.publish_present_order(
                d3d11_resource_ptr,
                &mut correlation.ctx_id,
                &mut correlation.value32,
                &mut correlation.cookie,
            )
        };
        if !published || !correlation.is_complete() {
            correlation = PresentStreamCorrelation::default();
        }
        (published, correlation)
    }

    /// D4a scanout acquire: deliver this device's KMD retirement event handle
    /// to the DXVK signaler. See the ffi declaration for the ownership rule.
    pub(crate) fn set_scanout_acquire_event(&self, event_handle: usize) -> bool {
        self.get()
            .is_some_and(|d| d.set_scanout_acquire_event(event_handle))
    }

    // -- pointer-laundering passthroughs -----------------------------------
    //
    // `unsafe` for the reason R814 established: each hands the bridge a raw
    // address it reinterpret_casts.

    /// # Safety
    /// `d3d11_resource_ptr` must be a live `ID3D11Resource*`.
    pub(crate) unsafe fn set_resource_kmt_handles(
        &self,
        d3d11_resource_ptr: usize,
        local: u32,
        global: u32,
    ) -> bool {
        self.get().is_some_and(|d| unsafe {
            d.set_resource_kmt_handles(d3d11_resource_ptr, local, global)
        })
    }

    /// # Safety
    /// `d3d11_resource_ptr` must be live; the out-params must be writable.
    pub(crate) unsafe fn get_resource_memory_info(
        &self,
        d3d11_resource_ptr: usize,
        memory: *mut u64,
        size: *mut u64,
        offset: *mut u64,
        resource_id: *mut u32,
    ) -> bool {
        self.get().is_some_and(|d| unsafe {
            d.get_resource_memory_info(d3d11_resource_ptr, memory, size, offset, resource_id)
        })
    }

    /// # Safety
    /// `d3d11_resource_ptr` must be live; the out-params must be writable.
    pub(crate) unsafe fn get_resource_alloc_identity(
        &self,
        d3d11_resource_ptr: usize,
        venus_alloc_size: *mut u64,
        memory_type_index: *mut u32,
    ) -> bool {
        self.get().is_some_and(|d| unsafe {
            d.get_resource_alloc_identity(d3d11_resource_ptr, venus_alloc_size, memory_type_index)
        })
    }

    /// # Safety
    /// `d3d11_resource_ptr` must be a live `ID3D11Resource*`.
    pub(crate) unsafe fn transfer_resource_ownership(&self, d3d11_resource_ptr: usize) -> bool {
        self.get()
            .is_some_and(|d| unsafe { d.transfer_resource_ownership(d3d11_resource_ptr) })
    }

    /// # Safety
    /// `d3d11_resource_ptrs` must point at `count` live `ID3D11Resource*`.
    pub(crate) unsafe fn rotate_resource_backings(
        &self,
        d3d11_resource_ptrs: *const usize,
        count: usize,
    ) -> bool {
        self.get()
            .is_some_and(|d| unsafe { d.rotate_resource_backings(d3d11_resource_ptrs, count) })
    }

    /// # Safety
    /// Both pointers must be live `ID3D11Resource*`.
    pub(crate) unsafe fn present_vehicle_copy(&self, dst: DstRes, src: SrcRes) -> i32 {
        // C++ order here is (dst, src) -- the opposite of publish above, which
        // is the whole hazard. The named types mean the two orders no longer
        // have to agree for the call to be correct.
        self.get()
            .map_or(-1, |d| unsafe { d.present_vehicle_copy(dst.0, src.0) })
    }

    /// D4b snapshot blit: S_i <- presented primary, recorded before the
    /// present-time Flush. See the ffi declaration for the return contract.
    ///
    /// # Safety
    /// Both pointers must be live `ID3D11Resource*`.
    pub(crate) unsafe fn present_snapshot_copy(&self, dst: DstRes, src: SrcRes) -> i32 {
        // Same (dst, src) C++ order and the same transposition hazard as
        // `present_vehicle_copy`; the newtypes are what make it a type error.
        self.get()
            .map_or(-1, |d| unsafe { d.present_snapshot_copy(dst.0, src.0) })
    }

    // -- shader creates ----------------------------------------------------
    //
    // These return the bridge's owned COM pointer as a raw word because the IA
    // caches key on the pointer VALUE; see the note above on why they are not
    // additionally newtyped.

    /// # Safety
    /// `code` must point at `len` readable bytes.
    pub(crate) unsafe fn create_vertex_shader(&self, code: *const u8, len: usize) -> usize {
        self.get()
            .map_or(0, |d| unsafe { d.create_vertex_shader(code, len) })
    }

    /// # Safety
    /// `code` must point at `len` readable bytes.
    pub(crate) unsafe fn create_pixel_shader(&self, code: *const u8, len: usize) -> usize {
        self.get()
            .map_or(0, |d| unsafe { d.create_pixel_shader(code, len) })
    }

    /// # Safety
    /// `code` must point at `len` readable bytes.
    pub(crate) unsafe fn create_geometry_shader(&self, code: *const u8, len: usize) -> usize {
        self.get()
            .map_or(0, |d| unsafe { d.create_geometry_shader(code, len) })
    }

    /// # Safety
    /// `code` must point at `len` readable bytes.
    pub(crate) unsafe fn create_hull_shader(&self, code: *const u8, len: usize) -> usize {
        self.get()
            .map_or(0, |d| unsafe { d.create_hull_shader(code, len) })
    }

    /// # Safety
    /// `code` must point at `len` readable bytes.
    pub(crate) unsafe fn create_domain_shader(&self, code: *const u8, len: usize) -> usize {
        self.get()
            .map_or(0, |d| unsafe { d.create_domain_shader(code, len) })
    }

    /// # Safety
    /// `code` must point at `len` readable bytes.
    pub(crate) unsafe fn create_compute_shader(&self, code: *const u8, len: usize) -> usize {
        self.get()
            .map_or(0, |d| unsafe { d.create_compute_shader(code, len) })
    }

    /// # Safety
    /// `code`/`sig_words` must point at `len`/`sig_words_len` readable items.
    pub(crate) unsafe fn create_shader_sig(
        &self,
        kind: u32,
        code: *const u8,
        len: usize,
        sig_words: *const u32,
        sig_words_len: usize,
    ) -> usize {
        self.get().map_or(0, |d| unsafe {
            d.create_shader_sig(kind, code, len, sig_words, sig_words_len)
        })
    }

    /// # Safety
    /// `code`/`sig_words` must point at `len`/`sig_words_len` readable items.
    pub(crate) unsafe fn create_tess_shader_sig(
        &self,
        kind: u32,
        code: *const u8,
        len: usize,
        sig_words: *const u32,
        sig_words_len: usize,
    ) -> usize {
        self.get().map_or(0, |d| unsafe {
            d.create_tess_shader_sig(kind, code, len, sig_words, sig_words_len)
        })
    }
}
