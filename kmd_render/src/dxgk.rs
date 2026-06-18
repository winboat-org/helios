//! WDDM display-miniport DDI bindings.
//!
//! These are generated at build time by `build.rs` (bindgen over `dispmprt.h` +
//! `d3dkmddi.h`) because `wdk-sys` has no display `ApiSubset`. The generated
//! file re-exports `wdk_sys::*`, so everything the DDI code needs — both the
//! display types (DRIVER_INITIALIZATION_DATA, DXGKARG_*, DXGKRNL_INTERFACE, ...)
//! and the base NT types (DEVICE_OBJECT, NTSTATUS, STATUS_*, ...) — is reachable
//! through `crate::dxgk`.

#[allow(
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    unused_imports,
    unnecessary_transmutes,
    clippy::all
)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/dxgk_bindings.rs"));
}

pub use bindings::*;
