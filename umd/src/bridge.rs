//! # Where `unsafe` sits on these declarations (R814)
//!
//! Rust's one memory-safety signal used to be attached to the wrong six
//! declarations here, so a reviewer scanning for `unsafe` call sites looked in
//! the wrong places. Three pointer-laundering entry points were SAFE
//! (`set_resource_kmt_handles`, `transfer_resource_ownership`,
//! `open_ddi_texture2d`) while three that take only scalars and cannot violate
//! memory safety were `unsafe fn` (`present_frame_gate`,
//! `present_sync_fence_id`, `present_flip_wait_arm`).
//!
//! Scope limit worth stating, because it changes the finding's shape: cxx
//! REQUIRES `unsafe fn` for any signature containing a raw pointer, and every
//! raw-pointer declaration in this block already is. Those are correct and are
//! not touched. `present_sync_publish` and `present_vehicle_copy` take `usize`
//! COM pointers AND are already unsafe, which is the correct end state; they
//! are left alone too.

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
        fn present_frame_gate(self: &HeliosDxvkDevice, timeout_us: u32) -> bool;
        /// WS1 #4 producer: record a named-present-fence signal on the
        /// frame's open command list (submits with the caller's following
        /// Flush; retires at host GPU completion) and publish
        /// (resid -> pid, value) for the presented resources. NO wait on the
        /// present thread. Returns the published value, or 0 when the path
        /// is unavailable (caller keeps relying on the present gate).
        /// `kwait_ordered` advertises in the slot (fenceId bit 30) that this
        /// present's flip is kernel-held until the value retires, so staged
        /// consumers may skip an unretired re-stage instead of blocking.
        unsafe fn present_sync_publish(
            self: &HeliosDxvkDevice,
            src_resource_ptr: usize,
            dst_resource_ptr: usize,
            kwait_ordered: bool,
        ) -> u64;
        /// Name discriminator of this device's named present fence; 0 until
        /// the first successful publish created it (or path disabled). Pairs
        /// with publish's value for the vehicle acquire-side recycle gate.
        fn present_sync_fence_id(self: &HeliosDxvkDevice) -> u32;
        /// Kernel flip-wait plumbing (25th session): hand the bridge the
        /// runtime's pfnSignalSynchronizationObjectFromCpuCb (as a raw fn
        /// address), the runtime device handle, the runtime-device monitored
        /// fence to signal, and its CPU value VA. The bridge owns the signal
        /// path (present-fence enqueueWait callbacks) and a wedge watchdog
        /// (CPU-signals the fence forward when the copy chain stalls, so a
        /// poisoned fence degrades to today's stale-frame semantics instead
        /// of parking the present context forever). Returns false when the
        /// producer fence path is disabled (caller keeps the CPU gate).
        unsafe fn present_flip_wait_setup(
            self: &HeliosDxvkDevice,
            signal_cb: usize,
            h_rt_device: usize,
            h_fence: u32,
            fence_cpu_va: usize,
        ) -> bool;
        /// Arm one present's kernel flip wait: when this device's present
        /// fence reaches `target_value` (the copy's completion, signaled by
        /// the ICD retire thread), CPU-signal the flip fence to
        /// `flip_value`. Enqueue-only — never waits. Returns false when the
        /// present fence is unavailable (caller falls back to the CPU gate
        /// for this present and must NOT queue the GPU wait).
        fn present_flip_wait_arm(
            self: &HeliosDxvkDevice,
            target_value: u64,
            flip_value: u64,
        ) -> bool;
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
/// `present_sync_publish(src, dst)` and `present_vehicle_copy(dst, src)` take
/// the same two COM pointers in OPPOSITE order, are called ~30 lines apart in
/// the same function, and a transposition compiles cleanly on both sides of the
/// FFI. Blast radius is asymmetric: transposing publish where dst is 0 is
/// benign, but at the two two-non-zero-pointer sites it advertises the wrong
/// resid to the shared present-sync table, and transposing
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

    pub(crate) fn present_frame_gate(&self, timeout_us: u32) -> bool {
        self.get().is_some_and(|d| d.present_frame_gate(timeout_us))
    }

    pub(crate) fn present_sync_fence_id(&self) -> u32 {
        self.get().map_or(0, |d| d.present_sync_fence_id())
    }

    pub(crate) fn present_flip_wait_arm(&self, target_value: u64, flip_value: u64) -> bool {
        self.get()
            .is_some_and(|d| d.present_flip_wait_arm(target_value, flip_value))
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
    /// Both pointers must be live `ID3D11Resource*` or 0.
    pub(crate) unsafe fn present_sync_publish(
        &self,
        src: SrcRes,
        dst: DstRes,
        kwait_ordered: bool,
    ) -> u64 {
        // The C++ order is (src, dst) and is NOT reordered here -- R816 says to
        // pick one change, and the types are the one that makes ordering
        // unrepresentable rather than merely documented.
        self.get()
            .map_or(0, |d| unsafe { d.present_sync_publish(src.0, dst.0, kwait_ordered) })
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

    /// # Safety
    /// `signal_cb` must be a valid `PFND3DDDI_SIGNALSYNCHRONIZATIONOBJECTFROMCPUCB`,
    /// and `h_rt_device` / `fence_cpu_va` must outlive the device.
    pub(crate) unsafe fn present_flip_wait_setup(
        &self,
        signal_cb: usize,
        h_rt_device: usize,
        h_fence: u32,
        fence_cpu_va: usize,
    ) -> bool {
        self.get().is_some_and(|d| unsafe {
            d.present_flip_wait_setup(signal_cb, h_rt_device, h_fence, fence_cpu_va)
        })
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
