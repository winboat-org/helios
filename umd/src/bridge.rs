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

        /// Opaque holder for the DXVK instance + adapter + device.
        type HeliosDxvkDevice;

        /// Create a DXVK instance and logical device on the Helios venus adapter.
        ///
        /// `luid_low`/`luid_high` identify the WDDM adapter to match; pass `(0, 0)`
        /// to take the first enumerated adapter. Returns a null `UniquePtr` on
        /// failure (no adapter, device creation threw, etc.). Never panics across
        /// the FFI boundary — the C++ side catches all exceptions.
        fn helios_dxvk_create_device(luid_low: u32, luid_high: i32) -> UniquePtr<HeliosDxvkDevice>;
    }
}
