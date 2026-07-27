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
pub mod ffi {
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
        fn set_resource_kmt_handles(
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
        fn transfer_resource_ownership(self: &HeliosDxvkDevice, d3d11_resource_ptr: usize) -> bool;
        fn open_ddi_texture2d(
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

        unsafe fn open_kmd_scanout_target(
            self: &HeliosDxvkDevice,
            out_resource_id: *mut u32,
            out_width: *mut u32,
            out_height: *mut u32,
            out_pitch: *mut u32,
            out_generation: *mut u32,
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
        unsafe fn present_frame_gate(self: &HeliosDxvkDevice, timeout_us: u32) -> bool;
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
        unsafe fn present_sync_fence_id(self: &HeliosDxvkDevice) -> u32;
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
        unsafe fn present_flip_wait_arm(
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
        // SAFETY: the bridge transfers one reference on success.
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

    /// Geometry of the KMD VidPn primary, plus one owned import of it.
    ///
    /// The five out-params become a returned tuple so a caller cannot read them
    /// after a failed open, when they are whatever the bridge zeroed them to.
    pub(crate) fn open_scanout_target(&self) -> Option<(ID3D11Resource, ScanoutImportInfo)> {
        let mut info = ScanoutImportInfo::default();
        // SAFETY: every out-param points at a live local for the call's
        // duration; the bridge writes each before returning non-zero.
        let raw = unsafe {
            self.open_kmd_scanout_target(
                &mut info.resource_id,
                &mut info.width,
                &mut info.height,
                &mut info.pitch,
                &mut info.generation,
            )
        };
        // SAFETY: the bridge transfers one reference on success.
        unsafe { adopt_resource(raw) }.map(|r| (r, info))
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

/// Geometry the KMD reports for its VidPn primary, filled by
/// [`ffi::HeliosDxvkDevice::open_scanout_target`].
#[derive(Clone, Copy, Default)]
pub(crate) struct ScanoutImportInfo {
    pub(crate) resource_id: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pitch: u32,
    pub(crate) generation: u32,
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
