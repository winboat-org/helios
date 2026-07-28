//! The three `helios_umd_*` exports the Mesa ICD's WSI resolves BY NAME.
//!
//! Moved verbatim out of `lib.rs` by T8/R1106. `#[no_mangle]` means the module
//! they live in does not change the exported symbol, so this is a pure
//! relocation as far as the loader is concerned.
//!
//! ⚠ ALL THREE MUST KEEP EXISTING.
//! `icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:876-886` resolves all three by
//! name and fails the entire dcomp vehicle path with `E_NOINTERFACE`
//! (incrementing `helios_vehicle_export_miss`) if any one is missing. A
//! UMD-only deploy that drops one kills the vehicle.

use crate::forward;
use crate::log_error;

/// Dcomp present vehicle (road 4 unit 2), in-process export for the ICD's
/// WSI: hand over the frame to present — venus resid + WS1 #4 fence value +
/// geometry + the creator's exact allocation identity — immediately before
/// calling Present() on the vehicle swapchain ON THE SAME THREAD. The next
/// dxgi_present on this thread consumes the slot (see
/// forward::set_present_source / vehicle_present_prepare).
/// Returns 0 = stored, 1 = stored but overwrote a pending source (counted
/// contract violation), -1 = refused (zero resid/geometry).
#[no_mangle]
pub extern "system" fn helios_umd_set_present_source(
    resid: u32,
    fence_value: u64,
    width: u32,
    height: u32,
    dxgi_format: u32,
    alloc_size: u64,
    memory_type_index: u32,
) -> i32 {
    forward::set_present_source(
        resid,
        fence_value,
        width,
        height,
        dxgi_format,
        alloc_size,
        memory_type_index,
    )
}

/// Companion export: bounded wait (µs) until the last vehicle present on
/// THIS thread — the frame copy included — completed on the GPU. The ICD's
/// present worker calls this after Present() returns and only then recycles
/// the frame image, closing the copy-vs-rerender race. Returns 0 =
/// complete, 1 = timeout (counted caller-side), -1 = no vehicle present
/// recorded on this thread.
#[no_mangle]
pub extern "system" fn helios_umd_wait_last_present(timeout_us: u32) -> i32 {
    forward::wait_last_present(timeout_us)
}

/// Companion export: RETIRED STUB, and it must keep existing.
///
/// R912(a) retired the kwait subsystem, so there is no present result to hand
/// back and this always returns -1 -- the ICD then falls back to
/// `helios_umd_wait_last_present`, which is what it already did on every frame
/// (measured: `kwait_armed = 0`, misses == presents, ROADMAP 7g(d)).
///
/// ⚠ DO NOT DELETE THE EXPORT.
/// `icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:876-886` resolves all three
/// `helios_umd_*` exports BY NAME and fails the entire dcomp vehicle path with
/// `E_NOINTERFACE` (incrementing `helios_vehicle_export_miss`) if any one is
/// missing. A UMD-only deploy that drops it kills the vehicle.
///
/// Logged ONCE per process rather than counted per call: returning -1 is now
/// the designed steady state, so a per-call counter would be measuring normal
/// operation. What is still worth seeing is that the ICD is asking at all.
#[no_mangle]
pub extern "system" fn helios_umd_get_present_result(fence_id: *mut u32, value: *mut u64) -> i32 {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ANNOUNCED: AtomicBool = AtomicBool::new(false);
    if !ANNOUNCED.swap(true, Ordering::Relaxed) {
        log_error!(
            "get_present_result: retired stub (R912a) -- always -1; the ICD's \
             serial wait_last_present is the recycle gate"
        );
    }
    let _ = (fence_id, value);
    -1
}
