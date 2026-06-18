//! d3d10umddi DDI types, generated from the WDK `d3d10umddi.h` by bindgen
//! (see `build.rs::generate_d3d10umddi_bindings`).
//!
//! These are the exact ABI structs the OS D3D11 runtime lowers app/DWM calls
//! into: the adapter-funcs tables, `D3D10DDIARG_CREATEDEVICE`, the 152-entry
//! `D3D11DDI_DEVICEFUNCS` device-funcs table, the DXGI base DDI, and the
//! kernel/runtime callback tables. We fill the device-funcs table (backed by the
//! DXVK device from the cxx bridge) to make `D3D11CreateDevice(Helios)` succeed.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/d3d10umddi.rs"));
