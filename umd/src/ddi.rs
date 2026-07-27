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

/// The `D3D10DDIARG_CREATEDEVICE` offsets this driver reads **positionally**.
///
/// # Why these and not the 6,336 bindgen assertions
///
/// `build.rs` sets `layout_tests(true)`, so bindgen already emits a
/// compile-time size/alignment/offset assertion for all 818 generated types.
/// Those are not a substitute for what follows: bindgen derives *both* the
/// struct and its assertions from the same `d3d10umddi.h`, so they are
/// self-consistent by construction. A WDK revision that moved a field would
/// regenerate both and still pass. What they catch is bindgen disagreeing with
/// the C compiler — not the ABI moving under us.
///
/// These assertions are different: they pin the numbers *this crate's code*
/// depends on positionally, so a WDK revision that moves a field fails the
/// build here instead of silently changing what we read.
///
/// The dependency is real and narrow. `create_device`'s `CreateDevice raw
/// args:` dump walks the argument as `*const u64` and prints words `0..=10`
/// **by index**, bounding itself at 11 words on the premise that the struct is
/// exactly 88 bytes. If `D3D10DDIARG_CREATEDEVICE` ever grew, that dump would
/// read past the runtime's object — an access violation inside the caller's
/// `D3D11CreateDevice` when the arg sits at the end of a page. If it shrank or
/// a field moved, the per-word commentary at that site (`[5]` = `hDrvDevice`,
/// `[9]` = `Flags`, …) would be quietly wrong while still looking like ABI
/// evidence. Both failure modes are now build errors.
///
/// `__bindgen_anon_1` is the device-funcs union and `__bindgen_anon_2` the
/// UM-callbacks union; every member of each aliases at its offset, which is
/// why naming the anonymous field is enough to pin the union's position.
///
/// Refs R802 sub-commit 3 (u-core-08), REFACTOR_REVIEW.md T5.
mod abi_offsets {
    use super::*;
    use core::mem::{offset_of, size_of};

    // The 88-byte total is the dump's bound; the rest are its word indices.
    const _: () = assert!(size_of::<D3D10DDIARG_CREATEDEVICE>() == 88);
    const _: () = assert!(offset_of!(D3D10DDIARG_CREATEDEVICE, hRTDevice) == 0);
    const _: () = assert!(offset_of!(D3D10DDIARG_CREATEDEVICE, Interface) == 8);
    const _: () = assert!(offset_of!(D3D10DDIARG_CREATEDEVICE, Version) == 12);
    const _: () = assert!(offset_of!(D3D10DDIARG_CREATEDEVICE, pKTCallbacks) == 16);
    const _: () = assert!(offset_of!(D3D10DDIARG_CREATEDEVICE, __bindgen_anon_1) == 24);
    const _: () = assert!(offset_of!(D3D10DDIARG_CREATEDEVICE, hDrvDevice) == 32);
    const _: () = assert!(offset_of!(D3D10DDIARG_CREATEDEVICE, DXGIBaseDDI) == 40);
    const _: () = assert!(offset_of!(D3D10DDIARG_CREATEDEVICE, hRTCoreLayer) == 56);
    const _: () = assert!(offset_of!(D3D10DDIARG_CREATEDEVICE, __bindgen_anon_2) == 64);
    const _: () = assert!(offset_of!(D3D10DDIARG_CREATEDEVICE, Flags) == 72);
    const _: () = assert!(offset_of!(D3D10DDIARG_CREATEDEVICE, ppfnRetrieveSubObject) == 80);

    // `DXGIBaseDDI` is embedded by value at 40 and the next field is at 56, so
    // its own size is what makes those two consistent.
    const _: () = assert!(size_of::<DXGI_DDI_BASE_ARGS>() == 16);

    // The other two runtime-supplied argument structs on the adapter path.
    // Both are already read field-by-name, so unlike the dump above nothing
    // here would misread if a field moved. These are a coarser tripwire: the
    // exported `OpenAdapter10`/`OpenAdapter10_2`/`GetCaps` signatures are the
    // negotiation surface the OS loader binds to, and a WDK revision that
    // resized either one is a change we want to be told about rather than
    // absorb silently.
    const _: () = assert!(size_of::<D3D10DDIARG_OPENADAPTER>() == 40);
    const _: () = assert!(size_of::<D3D10_2DDIARG_GETCAPS>() == 32);
}
