//! cxx bridge to the **statically linked** vkd3d-proton engine.
//!
//! The D3D12 UMD's `d3d12umddi` frontend (Rust) will call into vkd3d-proton's
//! `ID3D12*` COM objects through this bridge. The C++ side
//! (`bridge/vkd3d_bridge.cpp`, lane A) owns the `ID3D12Device*` inside an opaque
//! `HeliosVkd3dDevice`; Rust holds it via `UniquePtr`.
//!
//! ⭐ **There is no engine DLL.** `DECISIONS.md` D4 flipped: the engine is one
//! archive (`libhelios_d3d12_static.a`, a union of every vkd3d / dxil-spirv /
//! dxbc-spirv object) linked directly into `helios_umd12.dll`, plus `gdi32`.
//! ⛔ Never `dxgi` — a WDDM UMD sits *below* DXGI. So there is no `LoadLibrary`,
//! no module pin and no `FreeLibrary` anywhere on this path: the two engine
//! entry points the C++ side calls are ordinary archive symbols resolved at
//! link time.
//!
//! # Owned vs borrowed — the whole reason the wrappers below exist
//!
//! The bridge returns COM pointers as bare `usize`. Which side owns the
//! reference is a per-entry-point decision that the type system cannot see
//! across the FFI, and **both ways of getting it wrong are silent**:
//!
//!   * **Adopting a borrowed pointer is a double release.**
//!     `ID3D12Device::from_raw(bridge.d3d12_device_ptr() as *mut c_void)` is
//!     type-correct, compiles, and drops the bridge's only reference at end of
//!     scope — destroying the D3D12 device under a running DDI.
//!   * **Wrapping an owned pointer in `ManuallyDrop` is a leak**, and a leaked
//!     `ID3D12Device` pins the whole Vulkan device, its memory and its queues.
//!
//! Neither shows up where it was written. Each surfaces as a much later crash
//! (or an unexplained VRAM/handle plateau) in whatever process happened to be
//! driving the driver. This is not hypothetical for Helios: it is exactly the
//! hazard `umd/src/bridge.rs:252-271` documents for the D3D11 bridge's thirteen
//! `usize`-returning methods, and R813/R815 are what it cost to make the wrong
//! adoption *unwritable* rather than merely discouraged.
//!
//! ⚠ **S4 has exactly one COM accessor and it is BORROWED**
//! ([`BridgeDevice12::d3d12_device`], returning `ManuallyDrop`). An owned
//! counterpart was written and then removed, deliberately: nothing at this stage
//! takes a reference out of the bridge, so it would have been a hand-written
//! line carrying `#[allow(dead_code)]` — which `PARALLEL.md` §10 forbids
//! outright, citing R908 ("generated code may be allowed, hand-written code may
//! not"). The shape it must take when S6 gains its first owning caller, so it is
//! not re-derived from scratch:
//!
//! * C++ grows `std::size_t d3d12_device_addref() const noexcept` — `AddRef`
//!   then return the pointer, 0 if absent, with its own named counter for the
//!   absent case.
//! * Rust grows **one** `unsafe fn adopt_d3d12_device(raw: usize) ->
//!   Option<ID3D12Device>` — the single `from_raw` for every owning entry point
//!   (`ARCHITECTURE.md` §7.1 layer 1, mirroring `umd/src/bridge.rs`'s
//!   `adopt_resource`) — and the accessor calls only that.
//!
//! Layers 2 (borrowed getters return `ManuallyDrop`) and 3 (a sealed newtype
//! with no `Deref` and a private `inner`) are live below. Layer 1 is not
//! omitted, it is vacuous: it is "one `from_raw` per **owning entry point**",
//! and S4 has none.
//!
//! ECL completion uses an exact worker stream; admission uses the runtime context event.

#[allow(clippy::too_many_arguments)]
#[cxx::bridge]
mod ffi {
    unsafe extern "C++" {
        include!("vkd3d_bridge.h");

        /// Opaque holder for the vkd3d device the DDI will forward to.
        ///
        /// ⚠ Its destructor is out-of-line in the `.cpp` (pimpl), so cxx's
        /// `UniquePtr<HeliosVkd3dDevice>` drop reaches a complete `Impl`. No
        /// engine, Vulkan or COM header is visible from `vkd3d_bridge.h`.
        type HeliosVkd3dDevice;

        /// Raw `ID3D12Device*` (as usize). 0 if not created. **BORROWED** — the
        /// bridge keeps the owning reference; wrap on the Rust side without
        /// taking ownership.
        fn d3d12_device_ptr(self: &HeliosVkd3dDevice) -> usize;

        /// KMD-issued wire fence of one command queue's own timeline, from the
        /// venus ICD's `helios_venus_queue_gpu_fence`. **0 = no boundary**, which
        /// is what every refusal returns (see the header comment).
        ///
        /// # Safety
        /// `queue` is an `ID3D12CommandQueue*` owned by the engine and alive for
        /// the call; the bridge acquires and releases the engine's queue lock.
        unsafe fn helios_umd12_queue_gpu_fence(queue: usize) -> u64;

        /// The venus context id this device's `VkInstance` belongs to (S4b),
        /// captured at create time on the creating thread. 0 if the ICD is
        /// absent or too old to export it.
        fn venus_context_id(self: &HeliosVkd3dDevice) -> u32;

        /// The **instance-scoped** venus context id (UP-5), captured at create
        /// time on the creating thread. 0 if the ICD is absent or predates
        /// `helios_venus_instance_ctx_id`.
        ///
        /// ⛔ This is the one an allocation identity may be stamped with;
        /// [`venus_context_id`](Self::venus_context_id) is process-global and
        /// `umd_common/bridge/bridge_icd_anchor.h` forbids stamping it.
        fn venus_instance_context_id(self: &HeliosVkd3dDevice) -> u32;

        /// The venus identity of the memory an `ID3D12Resource` is bound to
        /// (UP-2c). See the Rust wrapper
        /// [`BridgeDevice12::resource_venus_identity`].
        ///
        /// # Safety
        /// `resource` is an `ID3D12Resource*` as a `usize`, **BORROWED** and
        /// created by this bridge's engine; all seven out-pointers must address
        /// writable storage and are written on every path.
        unsafe fn resource_venus_identity(
            self: &HeliosVkd3dDevice,
            resource: usize,
            out_vk_memory: *mut u64,
            out_memory_offset: *mut u64,
            out_memory_size: *mut u64,
            out_memory_type_index: *mut u32,
            out_venus_res_id: *mut u32,
            out_venus_alloc_size: *mut u64,
            out_status: *mut u32,
        ) -> bool;

        /// Hand the venus resource behind `resource`'s memory to the WDDM
        /// allocation that has just adopted it (UP-5). Returns the res_id it
        /// transferred, or 0 — which is a defect, see the wrapper.
        ///
        /// # Safety
        /// As [`resource_venus_identity`](Self::resource_venus_identity)'s
        /// `resource`.
        unsafe fn transfer_resource_ownership(self: &HeliosVkd3dDevice, resource: usize) -> u32;

        /// Create a vkd3d device on the Helios adapter identified by the split
        /// LUID. Returns a null `UniquePtr` on failure (adapter not found,
        /// engine refused, ...).
        ///
        /// ⛔ Deliberately **not** named `helios_vkd3d_create_device`: that C
        /// symbol is *defined* in the engine archive that is in this very link,
        /// and a same-named C++ function is a link-time ambush.
        fn helios_vkd3d_bridge_create_device(
            luid_low: u32,
            luid_high: i32,
            minimum_feature_level: u32,
        ) -> UniquePtr<HeliosVkd3dDevice>;

        fn native_optional_caps(
            self: &HeliosVkd3dDevice,
            maximum_feature_level: &mut u32,
            shader_model: &mut u32,
            raytracing_tier: &mut u32,
            device_uuid: &mut [u8],
        ) -> bool;

        /// Stateless forward to the engine's second entry point.
        ///
        /// # Safety
        /// `desc` is a `const D3D12_ROOT_SIGNATURE_DESC*` that must be live for
        /// the call. `blob_out` and `err_out` are written with **owned**
        /// `ID3DBlob*` values (0 when absent) and must point at writable
        /// `usize` storage.
        unsafe fn helios_vkd3d_bridge_serialize_root_signature(
            desc: usize,
            version: u32,
            blob_out: *mut usize,
            err_out: *mut usize,
        ) -> i32;

        /// # Safety
        /// Device and versioned descriptor tree are borrowed for the call;
        /// output receives one owned root-signature COM reference.
        unsafe fn helios_vkd3d_bridge_create_root_signature(
            device: usize,
            node_mask: u32,
            desc: usize,
            root_out: *mut usize,
        ) -> i32;

        /// # Safety
        /// A live engine command list, exclusively borrowed during its DDI.
        unsafe fn helios_vkd3d_bridge_clear_root_arguments(list: usize) -> i32;

        /// # Safety
        /// Device and complete pipeline stream are borrowed engine/API objects
        /// live through this call. Output receives one owned PSO reference.
        unsafe fn helios_vkd3d_bridge_create_stream_output_pipeline(
            device: usize,
            desc: usize,
            pipeline_out: *mut usize,
        ) -> i32;

        /// Queue a complete ECL batch behind the runtime's exact context event.
        /// # Safety
        /// Queue/lists and event are live for the call; the worker duplicates
        /// the event and retains all command allocators before returning.
        unsafe fn helios_vkd3d_bridge_execute(
            queue: usize,
            lists: &[usize],
            admission_event: usize,
            ctx: *mut u32,
            value: *mut u32,
            cookie: *mut u64,
        ) -> i32;

        /// # Safety
        /// All engine objects and API-typed arrays are live for this call.
        /// The engine copies mapping data and duplicates the admission event.
        unsafe fn helios_vkd3d_bridge_update_tiles(
            queue: usize,
            resource: usize,
            region_count: u32,
            coords: usize,
            sizes: usize,
            heap: usize,
            range_count: u32,
            flags: usize,
            offsets: usize,
            counts: usize,
            mapping_flags: i32,
            admission: usize,
            ctx: *mut u32,
            value: *mut u32,
            cookie: *mut u64,
        ) -> i32;
        /// # Safety
        /// Engine objects and API coordinate/size structures are live for the
        /// call. Source mappings are resolved on the worker after admission.
        unsafe fn helios_vkd3d_bridge_copy_tiles(
            queue: usize,
            dst: usize,
            dst_coord: usize,
            src: usize,
            src_coord: usize,
            size: usize,
            flags: i32,
            admission: usize,
            ctx: *mut u32,
            value: *mut u32,
            cookie: *mut u64,
        ) -> i32;

        /// # Safety
        /// Queue is live. Called after runtime context destruction, or on failure.
        unsafe fn helios_vkd3d_bridge_cancel_execution(queue: usize, reason: i32);

        /// S_FALSE preserves execution-owned storage; S_OK resets it.
        /// # Safety
        /// Live borrowed engine allocator with externally serialized use.
        unsafe fn helios_vkd3d_bridge_try_reset_allocator(allocator: usize) -> i32;

        /// Commit a producer boundary after preceding work on the exact queue.
        /// # Safety
        /// Queue/resource are live engine objects of the same device; allocation
        /// names this resource's exact dxgkrnl allocation. Outputs are writable.
        unsafe fn helios_vkd3d_bridge_publish_producer(
            queue: usize,
            resource: usize,
            allocation: u32,
            admission_event: usize,
            ctx: *mut u32,
            value: *mut u32,
            cookie: *mut u64,
        ) -> bool;

    }
}

use core::ffi::c_void;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicU64, Ordering};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::ID3D12Device;

/// Compile a native-DDI SO pipeline with explicit physical-register origin.
/// # Safety
/// Every pointer inside `desc` is valid and unchanged for this call, and
/// `device` is the live bridge engine device owning its referenced objects.
pub(crate) unsafe fn create_stream_output_pipeline(
    device: &ID3D12Device,
    desc: &windows::Win32::Graphics::Direct3D12::D3D12_PIPELINE_STATE_STREAM_DESC,
) -> windows::core::Result<windows::Win32::Graphics::Direct3D12::ID3D12PipelineState> {
    let mut raw = 0usize;
    // SAFETY: the caller guarantees the device and complete borrowed stream;
    // `raw` is writable storage and the bridge initializes it on every path.
    let hr = unsafe {
        ffi::helios_vkd3d_bridge_create_stream_output_pipeline(
            device.as_raw() as usize,
            core::ptr::from_ref(desc) as usize,
            &mut raw,
        )
    };
    if hr < 0 {
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
            hr,
        )));
    }
    if raw == 0 {
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
            helios_umd_common::hr::E_FAIL,
        )));
    }
    // SAFETY: success transfers exactly one owned ID3D12PipelineState reference.
    Ok(unsafe {
        windows::Win32::Graphics::Direct3D12::ID3D12PipelineState::from_raw(raw as *mut c_void)
    })
}

/// The vkd3d bridge device, with the raw cxx surface sealed off.
///
/// ⛔ **No `Deref`, and `inner` is private.** Module privacy alone is NOT
/// sufficient and this is measured, not stylistic: cxx generates
/// `d3d12_device_ptr` as an **inherent** method on the public opaque
/// `ffi::HeliosVkd3dDevice`, and inherent methods of a re-exported public type
/// stay callable regardless of module visibility. A newtype with no `Deref` is
/// the only encoding that actually seals them — R815 in `umd`, and the reason
/// that crate's `BridgeDevice` has the same shape.
///
/// The C++ side still returns `usize`, so the ABI is unchanged by the sealing.
pub struct BridgeDevice12 {
    inner: cxx::UniquePtr<ffi::HeliosVkd3dDevice>,
}

// SAFETY: the uniquely owned C++ holder contains a free-threaded ID3D12Device
// and immutable context identities captured at creation. Its normal native DDI
// owner already permits destruction on another thread. This grants ownership
// transfer only, not unsynchronized shared access (no Sync implementation).
unsafe impl Send for BridgeDevice12 {}

// Keep the capability-discovery engine until the first native CreateDevice can
// take ownership. Closing an adapter discards any unclaimed engine. The mutex
// protects only the handoff; device creation/destruction run outside that lock.
static CAPABILITY_ENGINE: std::sync::Mutex<Option<BridgeDevice12>> = std::sync::Mutex::new(None);

pub(crate) fn discard_capability_engine() {
    let pending = match CAPABILITY_ENGINE.lock() {
        Ok(mut slot) => slot.take(),
        Err(_) => {
            crate::note_refusal(&crate::UMD12_REFUSALS.caps_engine_unavailable);
            return;
        }
    };
    drop(pending);
}

/// Actual selected-engine capabilities, before the native UMD's tier ceilings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct NativeOptionalCaps {
    pub maximum_feature_level: u32,
    pub shader_model: u32,
    pub raytracing_tier: u32,
    pub device_uuid: [u8; 16],
}

impl BridgeDevice12 {
    /// Create a vkd3d device on the Helios adapter with this LUID. `None` when
    /// the bridge returned nothing — folding the old `is_null()` check into
    /// construction so a `BridgeDevice12` that exists is always usable.
    pub fn create(luid_low: u32, luid_high: i32) -> Option<Self> {
        let expected = crate::caps12::native_optional_caps()?;
        let pending = if luid_low == 0 && luid_high == 0 {
            CAPABILITY_ENGINE.lock().ok()?.take()
        } else {
            None
        };
        let device = match pending {
            Some(device) => {
                crate::log_error!(
                    "Native CreateDevice takes ownership of capability-discovery engine"
                );
                device
            }
            None => Self::create_for_caps(luid_low, luid_high)?,
        };
        let actual = device.optional_caps()?;
        if actual != expected {
            crate::note_refusal(&crate::UMD12_REFUSALS.caps_engine_mismatch);
            crate::log_error!("Native adapter/device capability mismatch: advertised={expected:?} actual={actual:?}");
            return None;
        }
        Some(device)
    }

    // Discovery runs once under caps12's initialization mutex. Native creation
    // consumes the engine; caps-only adapter closure releases it normally.
    pub(crate) fn probe_optional_caps() -> Option<NativeOptionalCaps> {
        let device = Self::create_for_caps(0, 0)?;
        let caps = device.optional_caps()?;
        *CAPABILITY_ENGINE.lock().ok()? = Some(device);
        Some(caps)
    }

    fn optional_caps(&self) -> Option<NativeOptionalCaps> {
        let mut caps = NativeOptionalCaps::default();
        self.get()?
            .native_optional_caps(
                &mut caps.maximum_feature_level,
                &mut caps.shader_model,
                &mut caps.raytracing_tier,
                &mut caps.device_uuid,
            )
            .then_some(caps)
    }

    fn create_for_caps(luid_low: u32, luid_high: i32) -> Option<Self> {
        let inner = ffi::helios_vkd3d_bridge_create_device(
            luid_low,
            luid_high,
            crate::caps12::REQUIRED_ENGINE_FEATURE_LEVEL,
        );
        (!inner.is_null()).then_some(Self { inner })
    }

    /// The only path from the newtype to the sealed type, and it is private
    /// (`ARCHITECTURE.md` §7.1 layer 3).
    fn get(&self) -> Option<&ffi::HeliosVkd3dDevice> {
        self.inner.as_ref()
    }

    // -- borrowed COM ------------------------------------------------------
    //
    // `ManuallyDrop` is the whole point: the bridge owns the reference, so the
    // returned wrapper must never release it.

    pub(crate) fn d3d12_device(&self) -> Option<ManuallyDrop<ID3D12Device>> {
        let p = self.get()?.d3d12_device_ptr();
        // SAFETY: a non-zero `d3d12_device_ptr` is the bridge's live
        // ID3D12Device, kept alive by `HeliosVkd3dDeviceImpl` for as long as
        // this `BridgeDevice12` exists (the `UniquePtr` we hold is what keeps
        // the impl alive, and `&self` borrows it for the returned wrapper's
        // whole life). `ManuallyDrop` borrows it without taking a reference,
        // so no Release is ever issued against a reference we do not own.
        (p != 0).then(|| ManuallyDrop::new(unsafe { ID3D12Device::from_raw(p as *mut c_void) }))
    }

    /// The venus context id this device's Vulkan instance belongs to (S4b).
    ///
    /// ⭐ This is the S4b pass criterion's instrument: with `helios_umd.dll` and
    /// `helios_umd12.dll` both live in one process, this value and the D3D11
    /// bridge's must be **non-zero and equal**, because
    /// `helios_icd_anchor_v1` forced both to the same ICD module. If they were
    /// ever unequal the two drivers would be handing each other foreign
    /// `VkDeviceMemory`/`VkInstance` handles.
    pub(crate) fn venus_context_id(&self) -> u32 {
        self.get().map_or(0, |d| d.venus_context_id())
    }

    /// The **instance-scoped** venus context id — the one an allocation identity
    /// may carry (UP-5).
    ///
    /// ⛔ Not interchangeable with [`Self::venus_context_id`], and the difference
    /// is a live hazard rather than a preference. That one reads the ICD's
    /// process-global `helios_current_ctx_id`, which is last-writer-wins across
    /// `VkInstance` creations; `umd_common/bridge/bridge_icd_anchor.h` states the
    /// rule outright — *"evidence only. Never stamp an identity with this
    /// value"*. This one reads `helios_venus_instance_ctx_id`, a `_Thread_local`
    /// written by the CTX_CREATE of the thread that created **this** device's
    /// instance, which no concurrent create can replace.
    ///
    /// 0 means the ICD is absent or predates the export. ⚠ A 0 is not fatal: the
    /// KMD's adopt path never reads `ctx_id` (`helios_protocol::classify` returns
    /// `AdoptedUmdResource` from `adopt_resource_id` alone, and the adopt arm of
    /// `build_backing` does not consult it), so the field is a diagnostic that
    /// reaches `HeliosWddmOpenIdentity::ctx_id` — which that record's own doc
    /// calls *"diagnostic only"*. It is counted rather than refused for exactly
    /// that reason.
    pub(crate) fn venus_instance_context_id(&self) -> u32 {
        self.get().map_or(0, |d| d.venus_instance_context_id())
    }

    /// The venus identity of the memory an `ID3D12Resource` is bound to.
    ///
    /// Returns the fields **and** the status, always. ⭐ The fields are readable
    /// even when the status is not [`IdentityStatus::Resolved`], deliberately: on
    /// [`IdentityStatus::IcdRefused`] the engine half (`vk_memory`, `offset`,
    /// `size`, `memory_type_index`) is real and the venus half is 0, and that
    /// combination is the single most informative log line on this path — it says
    /// *"vkd3d bound this memory and the ICD has no venus resource for it"*, i.e.
    /// the export chain did not engage. Collapsing it to `None` would throw away
    /// the evidence that distinguishes it from *"this resource has no memory"*.
    ///
    /// ⛔ **Only `Resolved` may be used to build an allocation.** Every other
    /// status leaves `venus_res_id == 0`, and 0 is precisely the value that makes
    /// the KMD *create* a resource instead of adopting ours
    /// (`create_allocation.rs:2377`), so passing one through would silently
    /// produce an allocation backed by memory nothing renders into.
    ///
    /// # Safety
    /// `resource` must be a live `ID3D12Resource*` **created by this bridge's
    /// vkd3d engine**, valid for the duration of the call. It is borrowed: no
    /// reference is taken and none is released. ⛔ A resource from any other D3D12
    /// implementation would be `CONTAINING_RECORD`-cast to a
    /// `struct d3d12_resource` it is not.
    pub(crate) unsafe fn resource_venus_identity(
        &self,
        resource: usize,
    ) -> (ResourceVenusIdentity, IdentityStatus) {
        let mut id = ResourceVenusIdentity::default();
        let mut raw_status: u32 = 0;
        let Some(device) = self.get() else {
            return (id, IdentityStatus::BadArg);
        };
        // SAFETY: the caller's guarantee above is exactly the cxx declaration's
        // precondition, and every out-pointer addresses a live local for the whole
        // call. The C++ side clears all seven before anything that can fail, so
        // they are defined on every path including a `false` return.
        let resolved = unsafe {
            device.resource_venus_identity(
                resource,
                &mut id.vk_memory,
                &mut id.memory_offset,
                &mut id.memory_size,
                &mut id.memory_type_index,
                &mut id.venus_res_id,
                &mut id.venus_alloc_size,
                &mut raw_status,
            )
        };
        let status = IdentityStatus::from_raw(raw_status);
        // ⛔ The intersection, not either alone. The C++ side returns `true` on
        // exactly the path that sets `RESOLVED`, so the two agree by construction
        // today — and this is an FFI the type system cannot check, so a future
        // divergence must fall to the SAFE side rather than to whichever field the
        // caller happened to read.
        if resolved && matches!(status, IdentityStatus::Resolved) {
            (id, IdentityStatus::Resolved)
        } else if matches!(status, IdentityStatus::Resolved) {
            (id, IdentityStatus::Unknown(raw_status))
        } else {
            (id, status)
        }
    }

    /// Hand the venus resource behind `resource`'s memory over to the WDDM
    /// allocation that has just adopted it. Returns the transferred res_id, or 0.
    ///
    /// ⛔ **Call only AFTER `pfnAllocateCb` has succeeded**, and treat a 0 as a
    /// defect rather than a degraded read: the ICD stops unref'ing the host
    /// resource only once this has run, so a 0 leaves the resource owned by both
    /// the ICD and the kernel allocation and it is unref'd twice — the res-45
    /// invalid-import class `create_allocation.rs`'s adopt arm exists to prevent.
    /// Transferring *before* the allocation would be the mirror-image bug: an
    /// allocation failure would then leave the resource owned by nobody.
    ///
    /// # Safety
    /// As [`Self::resource_venus_identity`].
    pub(crate) unsafe fn transfer_resource_ownership(&self, resource: usize) -> u32 {
        let Some(device) = self.get() else {
            return 0;
        };
        // SAFETY: as `resource_venus_identity`; no out-params.
        unsafe { device.transfer_resource_ownership(resource) }
    }
}

/// The venus identity of one `ID3D12Resource`'s bound memory.
///
/// ⚠ Plain integers, `Default`-constructible, and **not** a validity claim: a
/// value of this type says nothing about whether the identity resolved. The
/// paired [`IdentityStatus`] is the only thing that does, which is why
/// [`BridgeDevice12::resource_venus_identity`] returns them together and never
/// one without the other.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ResourceVenusIdentity {
    /// The `VkDeviceMemory` the resource's image or buffer is BOUND to, as a
    /// 64-bit handle. ⚠ For a CPU-accessible texture this is `private_mem`, not
    /// the host-visible staging buffer — the engine method branches with the bind
    /// site, which is what makes it *the memory the image is bound to* rather than
    /// *the resource's first allocation*.
    pub(crate) vk_memory: u64,
    /// The resource's byte offset within `vk_memory`. ⚠ **Must be 0 for anything
    /// that gets a WDDM allocation.** One venus resource id covering several D3D12
    /// resources breaks the one-resource-one-allocation rule, and the D3D11 adopt
    /// path requires `memory_offset == 0` outright
    /// (`umd/src/forward/resource.rs:488-490`).
    pub(crate) memory_offset: u64,
    /// The whole `VkDeviceMemory`'s `VkMemoryAllocateInfo::allocationSize`, as
    /// vkd3d recorded it — **not** the resource's size.
    pub(crate) memory_size: u64,
    /// vkd3d's `memoryTypeIndex` for `vk_memory`.
    pub(crate) memory_type_index: u32,
    /// The venus resource id backing `vk_memory`, i.e. the value that becomes
    /// `HeliosWddmAllocPrivate::adopt_resource_id`. Non-zero only on
    /// [`IdentityStatus::Resolved`].
    pub(crate) venus_res_id: u32,
    /// The ICD's own record of the creating `vkAllocateMemory`'s `allocationSize`.
    /// Expected to equal [`Self::memory_size`] — two independent sources for one
    /// number, kept apart so a disagreement is visible.
    pub(crate) venus_alloc_size: u64,
}

/// Why a resource's venus identity came back as it did.
///
/// ⛔ Seven outcomes and not a `bool`, to distinguish each failed interface boundary: they
/// are different findings and sharing one counter produces exactly the
/// un-attributable number this project has corrected four times in the KMD's own
/// counters. In particular [`Self::IcdRefused`] — *"vkd3d bound memory the ICD has
/// no venus resource for"* — is the one that says the export chain
/// (`VKD3D_HEAP_FLAG_HELIOS_VENUS_EXPORT`) did not engage, and it must never be
/// confused with [`Self::EngineRefused`], which says the resource has no memory at
/// all.
///
/// ⚠ **The numbers are the C++ side's** — `HELIOS_VKD3D_IDENTITY_*` in
/// `umd12/bridge/vkd3d_bridge.h`, the single declaration. [`Self::Unknown`] exists
/// rather than a catch-all arm so a drift is loud instead of absorbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityStatus {
    /// Everything answered; `venus_res_id` and `venus_alloc_size` are real.
    Resolved,
    /// A null resource, or no device behind the bridge.
    BadArg,
    /// This engine build has no `ID3D12DXVKInteropDevice4`.
    NoInterop,
    /// `GetVulkanResourceMemoryInfo` failed — the resource has no bound device
    /// memory (a reserved/sparse resource, for example).
    EngineRefused,
    /// No venus ICD module in this process, or the S4b anchor refused because two
    /// ICD images are live.
    NoIcd,
    /// The anchored ICD predates the memory identity exports.
    NoExport,
    /// ⛔ The exports ran and answered 0: this `VkDeviceMemory` has **no venus
    /// resource**, i.e. it was not allocated on the ICD's export arm. The engine
    /// half of the record is still valid and worth logging.
    IcdRefused,
    /// The C++ side returned a value this enum does not know, or returned
    /// `RESOLVED` with a `false`. ⛔ A drift between `vkd3d_bridge.h`'s constants
    /// and this mapping.
    Unknown(u32),
}

impl IdentityStatus {
    /// Map the C++ side's `HELIOS_VKD3D_IDENTITY_*` value. ⛔ The authority for
    /// these numbers is `umd12/bridge/vkd3d_bridge.h`; keep both in sync.
    fn from_raw(raw: u32) -> Self {
        match raw {
            0 => Self::Resolved,
            1 => Self::BadArg,
            2 => Self::NoInterop,
            3 => Self::EngineRefused,
            4 => Self::NoIcd,
            5 => Self::NoExport,
            6 => Self::IcdRefused,
            other => Self::Unknown(other),
        }
    }
}

/// Serialize a root signature through the engine. Stateless — no device
/// involved, which is why it is a free function rather than a method.
///
/// Returns the engine's HRESULT; on failure the C++ side has already zeroed
/// both out-params.
///
/// # Safety
/// `desc` must be a live `*const D3D12_ROOT_SIGNATURE_DESC` for the duration of
/// the call. `blob_out` must be a writable `*mut usize`; `err_out` may be null.
/// Both receive **owned** `ID3DBlob*` values (0 when absent) — the caller
/// `Release`s each non-zero one exactly once.
pub(crate) unsafe fn serialize_root_signature(
    desc: usize,
    version: u32,
    blob_out: *mut usize,
    err_out: *mut usize,
) -> i32 {
    // SAFETY: forwarded unchanged; the caller's guarantees above are exactly
    // the cxx declaration's own preconditions, and the C++ side additionally
    // null-checks `blob_out` before writing it.
    unsafe { ffi::helios_vkd3d_bridge_serialize_root_signature(desc, version, blob_out, err_out) }
}

/// # Safety
/// Device and all pointers in the versioned descriptor tree remain live for the
/// synchronous call. The output receives an owned engine COM reference.
pub(crate) unsafe fn create_root_signature(
    device: usize,
    node_mask: u32,
    desc: usize,
    root_out: *mut usize,
) -> i32 {
    // SAFETY: caller supplies the complete borrowed tree and writable output.
    unsafe { ffi::helios_vkd3d_bridge_create_root_signature(device, node_mask, desc, root_out) }
}

/// # Safety
/// List is an exclusively borrowed live engine command list for this DDI.
pub(crate) unsafe fn clear_root_arguments(list: usize) -> i32 {
    // SAFETY: the native DDI owns the list during this synchronous operation.
    unsafe { ffi::helios_vkd3d_bridge_clear_root_arguments(list) }
}

/// Reset only when the engine's execution references have retired. S_FALSE is
/// an ownership result, never an attestation of GPU completion.
/// # Safety
/// `allocator` is a live borrowed engine allocator; its use is serialized.
pub(crate) unsafe fn try_reset_allocator(allocator: usize) -> i32 {
    // SAFETY: the caller owns and serializes the engine allocator.
    unsafe { ffi::helios_vkd3d_bridge_try_reset_allocator(allocator) }
}

/// # Safety
/// queue and resource are live engine COM objects of the same device for this
/// call. The allocation belongs to that exact resource's current incarnation.
pub(crate) unsafe fn publish_producer(
    queue: usize,
    resource: usize,
    allocation: u32,
    admission_event: usize,
) -> Option<(u32, u32, u64)> {
    let (mut ctx, mut value, mut cookie) = (0, 0, 0);
    // SAFETY: caller supplies exact live objects; outputs are writable locals.
    let ok = unsafe {
        ffi::helios_vkd3d_bridge_publish_producer(
            queue,
            resource,
            allocation,
            admission_event,
            &mut ctx,
            &mut value,
            &mut cookie,
        )
    };
    (ok && ctx != 0 && value != 0 && cookie != 0).then_some((ctx, value, cookie))
}

/// # Safety
/// Exact live engine queue, command lists and runtime admission event.
pub(crate) unsafe fn execute(
    queue: usize,
    lists: &[usize],
    admission_event: usize,
) -> Result<(u32, u32, u64), i32> {
    let (mut ctx, mut value, mut cookie) = (0, 0, 0);
    // SAFETY: forwarded live borrowed objects; outputs are writable locals.
    let hr = unsafe {
        ffi::helios_vkd3d_bridge_execute(
            queue,
            lists,
            admission_event,
            &mut ctx,
            &mut value,
            &mut cookie,
        )
    };
    if hr < 0 {
        Err(hr)
    } else if ctx == 0 || value == 0 || cookie == 0 {
        Err(helios_umd_common::hr::E_FAIL)
    } else {
        Ok((ctx, value, cookie))
    }
}

/// # Safety
/// Queue is still owned by QueueState while workers are cancelled.
/// The wire fence to gate one D3D12 packet on, or 0 for "no boundary".
///
/// ⛔ Zero is NOT an error path here: it is returned for every refusal in the ICD
/// export, and it restores the previous behaviour exactly (the packet retires on
/// worker completion). The KMD counts carried and absent fences separately
/// (`D12Fnc`/`D12Fn0`), so an inert wire is visible rather than assumed.
///
/// ⚠ Cost: the export issues one SUBMIT_VENUS escape per call and its fences stay
/// in flight until host GPU completion, so this is **rate-limited** — see
/// [`FENCE_INTERVAL_MS`](crate::knobs12::UMD12_GPU_FENCE_INTERVAL_MS). The bridge
/// takes and releases the engine queue lock *before* the escape (it needs the
/// drain, not the lock), so the escape does not hold it.
///
/// # Safety
/// `queue` is an `ID3D12CommandQueue*` (as `usize`) owned by the engine and alive
/// for the call.
#[inline]
pub(crate) unsafe fn queue_gpu_fence(queue: usize) -> u64 {
    // ⛔ THE ONE GATE for UV1's lever (`Umd12GpuFence`, `knobs12`). It sits at the
    // choke point rather than at the three producer call sites so that "no
    // boundary" is decided in exactly one place and a disabled fence cannot be
    // reached past a half-applied change. `0` is the KMD's documented "no
    // boundary" value, so the disabled arm is the pre-K-F retire domain with no
    // other difference — see the knob's doc for why that control is worth having.
    if !crate::knobs12::umd12_gpu_fence() {
        return 0;
    }
    let interval_ms = crate::knobs12::umd12_gpu_fence_interval_ms();
    if interval_ms == 0 {
        // SAFETY: forwarded live queue; the bridge owns the escape's ordering.
        return unsafe { ffi::helios_umd12_queue_gpu_fence(queue) };
    }
    // ⛔ THE THROTTLE, and the reason it is here rather than in the ICD: the ICD
    // export is one escape AND one wire fence that stays in flight until host GPU
    // completion, so calling it per packet is what `helios_venus_queue_gpu_fence`'s
    // own COST note names as the pressure limit (~1200/s) and what PENDING.md §4
    // calls "per-fence completion unbatched". Measured 2026-09-13 on .283: with the
    // fence per producer the `allocator` oracle never completed in 150 s, and with
    // `Umd12GpuFence=0` (no escape at all) the same case reached a verdict.
    //
    // Within the interval the LAST MINTED fence is reused rather than a second one
    // minted. That can only UNDER-order: a boundary minted earlier retires no later
    // than one minted now, so no packet can be retired before the host work the
    // reused fence already covers. It never over-orders, which is the direction
    // that would signal a fence before host completion. The cost is that a packet
    // submitted inside the window is gated on the window's opening boundary rather
    // than on its own work — the trade the interval knob exists to expose.
    let now = fence_clock_ms();
    // ACQUIRE, pairing with the RELEASE stores below: a thread that reads the new
    // stamp must also see the fence stored before it, or it would reuse a fence
    // from an older window than the stamp claims. (Reading a stale fence is only
    // ever an under-order, i.e. the safe direction, but there is no reason to
    // design for it.)
    let last_ms = LAST_MINT_MS.load(Ordering::Acquire);
    if last_ms != 0 && now.saturating_sub(last_ms) < u64::from(interval_ms) {
        return LAST_FENCE.load(Ordering::Relaxed);
    }
    // SAFETY: forwarded live queue; the bridge owns the escape's ordering.
    let fence = unsafe { ffi::helios_umd12_queue_gpu_fence(queue) };
    if fence != 0 {
        // Publish the fence BEFORE the stamp that makes it reusable: a thread that
        // observes a fresh stamp must observe a fence that has already been stored,
        // or it would reuse a fence from an older window than the stamp claims.
        LAST_FENCE.store(fence, Ordering::Release);
        LAST_MINT_MS.store(now, Ordering::Release);
    }
    fence
}

/// Milliseconds since this process's first fence fetch.
///
/// ⚠ `Instant`, not wall clock: the interval is a rate, and a clock step (NTP,
/// suspend/resume) must not be able to make a throttle window negative or
/// arbitrarily long. `OnceLock` keeps the baseline per process, which is the
/// scope the knob is read at.
fn fence_clock_ms() -> u64 {
    static CLOCK: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    CLOCK.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// When the last wire fence was minted (0 = never), and which one it was.
///
/// ⚠ PROCESS-WIDE, deliberately: the pressure being throttled is the adapter's
/// in-flight-fence budget, and the ICD's ring is ring-global across this device's
/// queues (`vn_renderer_helios.c`'s `dev->primary_ring` note), so a per-queue
/// window would multiply the escape rate by the queue count for no benefit.
static LAST_MINT_MS: AtomicU64 = AtomicU64::new(0);
static LAST_FENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) unsafe fn cancel_execution(queue: usize, reason: i32) {
    // SAFETY: forwarded live queue; no reference escapes the call.
    unsafe { ffi::helios_vkd3d_bridge_cancel_execution(queue, reason) };
}

/// Borrowed API data for an admitted tile update. Optional arrays are represented
/// by null pointers; their counts still describe the API's documented defaults.
pub(crate) struct TileUpdate {
    pub resource: usize,
    pub region_count: u32,
    pub coords: usize,
    pub sizes: usize,
    pub heap: usize,
    pub range_count: u32,
    pub range_flags: usize,
    pub offsets: usize,
    pub counts: usize,
    pub flags: i32,
}

/// # Safety
/// Live engine queue/resource/heap and correctly typed API arrays in `update`.
/// All pointers and the event remain live through this synchronous call.
pub(crate) unsafe fn update_tiles(
    queue: usize,
    update: &TileUpdate,
    admission: usize,
) -> Result<(u32, u32, u64), i32> {
    let (mut ctx, mut value, mut cookie) = (0, 0, 0);
    // SAFETY: the caller owns the borrowed engine objects/arrays/event; outputs
    // are writable locals. The engine retains its own worker dependencies.
    let hr = unsafe {
        ffi::helios_vkd3d_bridge_update_tiles(
            queue,
            update.resource,
            update.region_count,
            update.coords,
            update.sizes,
            update.heap,
            update.range_count,
            update.range_flags,
            update.offsets,
            update.counts,
            update.flags,
            admission,
            &mut ctx,
            &mut value,
            &mut cookie,
        )
    };
    tile_boundary(hr, ctx, value, cookie)
}

pub(crate) struct TileCopy {
    pub dst: usize,
    pub dst_coord: usize,
    pub src: usize,
    pub src_coord: usize,
    pub size: usize,
    pub flags: i32,
}

/// # Safety
/// Live engine queue/resources, API coordinates/size and admission event. The
/// worker retains the resources and copies the scalar mapping description.
pub(crate) unsafe fn copy_tiles(
    queue: usize,
    copy: &TileCopy,
    admission: usize,
) -> Result<(u32, u32, u64), i32> {
    let (mut ctx, mut value, mut cookie) = (0, 0, 0);
    // SAFETY: caller's live API-typed arguments, writable output locals.
    let hr = unsafe {
        ffi::helios_vkd3d_bridge_copy_tiles(
            queue,
            copy.dst,
            copy.dst_coord,
            copy.src,
            copy.src_coord,
            copy.size,
            copy.flags,
            admission,
            &mut ctx,
            &mut value,
            &mut cookie,
        )
    };
    tile_boundary(hr, ctx, value, cookie)
}

fn tile_boundary(hr: i32, ctx: u32, value: u32, cookie: u64) -> Result<(u32, u32, u64), i32> {
    if hr < 0 {
        Err(hr)
    } else if ctx == 0 || value == 0 || cookie == 0 {
        Err(helios_umd_common::hr::E_FAIL)
    } else {
        Ok((ctx, value, cookie))
    }
}
