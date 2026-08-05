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
