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

// ⚠ `too_many_arguments` is allowed for this MODULE and not for a function,
// because the lint fires on a declaration inside a `#[cxx::bridge]` block and cxx
// passes through only a fixed set of attributes — an `#[allow]` on the extern `fn`
// itself is not one of them.
//
// The declaration it fires on is `resource_venus_identity`, and its width is the
// FFI's shape rather than a design choice: the alternative is a shared `#[repr(C)]`
// struct, which cxx emits into its own generated header — a header
// `vkd3d_bridge.cpp` deliberately does not include (it hand-declares every
// signature instead, see `vkd3d_bridge.h`'s banner), and which itself includes
// `vkd3d_bridge.h`. So a struct out-param would mean either a duplicated POD
// declaration or an include cycle. Seven out-params it is, cleared by the C++ side
// before anything that can fail. The precedent is `umd/src/bridge.rs:315`, the same
// lint on the same kind of accessor in the D3D11 bridge.
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
        unsafe fn transfer_resource_ownership(
            self: &HeliosVkd3dDevice,
            resource: usize,
        ) -> u32;

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
        ) -> UniquePtr<HeliosVkd3dDevice>;

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

        /// Drain one `ID3D12CommandQueue`'s vkd3d submission worker (K-F1).
        ///
        /// ⭐ A CPU-side wait for `vkQueueSubmit`, **not** for GPU completion —
        /// see the Rust wrapper [`drain_queue`], which carries the whole
        /// argument for why that distinction makes this legal.
        ///
        /// `queue` is an `ID3D12CommandQueue*` as a `usize`, **BORROWED**: the
        /// C++ side takes no reference and releases none, so the caller's
        /// interface must outlive the call. Returns `false` (counted and logged
        /// on the C++ side) for a 0 queue or an engine that declined.
        ///
        /// ⚠ Declared `unsafe` because the integer is a raw pointer in
        /// disguise: nothing in the signature stops a caller passing a stale or
        /// foreign value, and the C++ body dereferences it through vkd3d's
        /// `CONTAINING_RECORD`.
        ///
        /// `out_wire_fence` / `out_fence_status` are passed **both or neither**
        /// and both are always written when non-null (0 and a status). See
        /// [`drain_queue`] for the status mapping and [`FenceStatus`] for why a
        /// zero fence has four distinguishable causes.
        unsafe fn helios_vkd3d_bridge_drain_queue(
            queue: usize,
            out_wire_fence: *mut u64,
            out_fence_status: *mut u32,
        ) -> bool;
    }
}

use core::ffi::c_void;
use core::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::ID3D12Device;

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

impl BridgeDevice12 {
    /// Create a vkd3d device on the Helios adapter with this LUID. `None` when
    /// the bridge returned nothing — folding the old `is_null()` check into
    /// construction so a `BridgeDevice12` that exists is always usable.
    pub fn create(luid_low: u32, luid_high: i32) -> Option<Self> {
        let inner = ffi::helios_vkd3d_bridge_create_device(luid_low, luid_high);
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
/// ⛔ Seven outcomes and not a `bool`, for the reason [`FenceStatus`] gives: they
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

/// Drain one command queue's vkd3d submission worker. `true` when the drain ran.
///
/// # ⭐⭐ This is a wait for `vkQueueSubmit`, NOT for GPU completion
///
/// State that plainly, because a later reader will otherwise mistake it for
/// `tmp/dx12/FENCE-BRIDGE-DESIGN.md`'s **design A — which is REJECTED** and must
/// not be reintroduced under any name. The difference is the whole permission:
///
/// * design A blocks the producer thread until the GPU has *finished*, which
///   destroys CPU/GPU overlap and is the producer-side CPU present stall the owner
///   forbade outright (`umd/src/knobs.rs:31-43`, `KMD_IMPACT.md` §14a.5);
/// * this blocks only until vkd3d's own submission worker has *handed the work to
///   Vulkan*. The GPU has typically not started, and nothing waits for it.
///
/// It is required rather than defensive: vkd3d's `ID3D12CommandQueue::
/// ExecuteCommandLists` is asynchronous — it pushes a submission onto a worker
/// thread's queue (`libs/vkd3d/command.c`'s `d3d12_command_queue_add_submission`)
/// — so without the drain the WDDM packet submitted immediately afterwards could
/// be *ordered ahead of* the `vkQueueSubmit` it is supposed to fence, and the
/// application's `ID3D12Fence` would be exactly as untruthful as it is today.
///
/// ⭐ It is the same discipline `HeliosWaitFrameSubmitted` gives the D3D11 present
/// path, and `KMD_IMPACT.md` §14a.2 says so in as many words.
///
/// ⚠ One cost, stated because it is not visible from here: the paired
/// `vkd3d_release_vk_queue` submits an empty `vkQueueSubmit2` that signals the
/// queue's submission timeline. `vkd3d_bridge.cpp`'s comment at the call has the
/// citation and the reason it must not be avoided.
///
/// # Safety
/// `queue` must be a live `ID3D12CommandQueue*` **created by this bridge's vkd3d
/// engine**, valid for the duration of the call. It is borrowed: no reference is
/// taken and none is released. ⛔ A queue from any other D3D12 implementation
/// would be `CONTAINING_RECORD`-cast to a `struct d3d12_command_queue` it is not.
pub(crate) unsafe fn drain_queue(queue: usize) -> bool {
    // SAFETY: forwarded unchanged; the caller's guarantee above is exactly the
    // cxx declaration's precondition, and the C++ side additionally refuses a 0.
    // Null out-params ask for no fence sample and the C++ side skips resolving the
    // export entirely — which is the `Umd12EclFence=0` arm's whole cost.
    unsafe {
        ffi::helios_vkd3d_bridge_drain_queue(queue, core::ptr::null_mut(), core::ptr::null_mut())
    }
}

/// Why a GPU-completion boundary came back as it did.
///
/// ⛔ **A zero fence is a LEGAL outcome** — `HeliosD3D12SubmitCmd`'s documented
/// "submit the packet, order it against nothing" arm — so the caller cannot learn
/// anything from the value alone. These are the four *reasons*, and they are four
/// different findings: an ICD that is not there, an ICD too old to have the export,
/// an export that ran and declined (ring 0, an undecodable handle, no venus ctx),
/// and a real boundary. ⛔ Sharing one counter between them would produce exactly
/// the un-attributable number this project has now corrected four times in the
/// KMD's own counters.
///
/// ⚠ **The numbers are the C++ side's** — `HELIOS_VKD3D_FENCE_*` in
/// `umd12/bridge/vkd3d_bridge.h`, which is the single declaration. This mapping is
/// by value across an FFI the type system cannot check, so [`Self::Unknown`] exists
/// rather than a `_ => Refused` that would silently absorb a drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FenceStatus {
    /// A non-zero wire fence retiring at host GPU completion. The real boundary.
    Sampled,
    /// No venus ICD module in this process, or the S4b anchor refused because two
    /// ICD images are live. Counted and logged once, at resolution.
    NoIcd,
    /// The anchored ICD module predates `helios_venus_queue_gpu_fence`. ⛔ The
    /// designed graceful path, not an error: an older ICD still gets a submission,
    /// carrying the 0 boundary.
    NoExport,
    /// The export ran and declined. ⚠ Its own most important arm is `ring_idx == 0`,
    /// which it refuses unconditionally because a ring-0 wire fence retires at
    /// decode — a fence that would lie about GPU completion.
    Refused,
    /// The C++ side returned a value this enum does not know. ⛔ A drift between
    /// `vkd3d_bridge.h`'s constants and this mapping, and it must be loud: silently
    /// folding it into `Refused` would make the next status added invisible.
    Unknown(u32),
}

impl FenceStatus {
    /// Map the C++ side's `HELIOS_VKD3D_FENCE_*` value. ⛔ The authority for these
    /// numbers is `umd12/bridge/vkd3d_bridge.h`; keep both in sync.
    fn from_raw(raw: u32) -> Self {
        match raw {
            0 => Self::Sampled,
            1 => Self::NoIcd,
            2 => Self::NoExport,
            3 => Self::Refused,
            other => Self::Unknown(other),
        }
    }
}

/// Drain one command queue **and** sample the venus wire fence that retires at host
/// GPU completion of everything now submitted to it.
///
/// Returns `(drained, wire_fence, status)`. `drained` is [`drain_queue`]'s value and
/// is independent of the fence: a queue can drain successfully and still yield no
/// boundary, which is what `status` explains.
///
/// # ⛔ The ordering obligation lives at the C++ call site, not here
///
/// The sample happens **after** the `VKD3D_SUBMISSION_DRAIN` and **while both of
/// vkd3d's queue locks are still held** — see `vkd3d_bridge.cpp`, which carries the
/// argument in full. The short form, because it is the whole correctness of the
/// boundary: reading a *larger* ring seqno than needed is harmless (it over-orders),
/// while reading a **stale smaller** one yields a fence covering less work than the
/// caller believes, and nothing inside the ICD export can detect that — only the
/// call's position can.
///
/// # Safety
/// As [`drain_queue`].
pub(crate) unsafe fn drain_queue_with_fence(queue: usize) -> (bool, u64, FenceStatus) {
    let mut wire_fence: u64 = 0;
    let mut raw_status: u32 = 0;
    // SAFETY: as `drain_queue`, plus two live writable locals for the out-params.
    // The C++ side clears both before anything that can fail, so they are defined on
    // every path including a false return.
    let drained =
        unsafe { ffi::helios_vkd3d_bridge_drain_queue(queue, &mut wire_fence, &mut raw_status) };
    (drained, wire_fence, FenceStatus::from_raw(raw_status))
}
