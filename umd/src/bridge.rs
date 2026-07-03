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
