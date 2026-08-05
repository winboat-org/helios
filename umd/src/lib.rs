//! Helios WDDM user-mode display driver bring-up DLL.
//!
//! This is not a D3D implementation yet. It gives the display package a real
//! UMD DLL with the WDDM adapter-open exports that the OS/runtime can resolve.
//! Until the DXVK/VKD3D-backed adapter/device path exists, device creation still
//! fails explicitly. The adapter-open handshake itself must succeed because
//! dxgkrnl calls it during render-adapter start validation.
// R420's static guarantee, at DENY level so it is a compile ERROR and not a
// warning: `log_line` is `#[deprecated]` purely as an internal marker, and the
// only things allowed to call it are the `trace_line!` and `log_error!` macros
// (each wraps the call in `#[allow(deprecated)]`). A new per-op site therefore
// cannot reach the unconditional writer by accident — it does not compile.
// Verified by fault injection: a direct `crate::log_line(..)` yields
// "error: use of deprecated function `log_line`: use log_error! ... or
// trace_line! ...". No dependency trips this lint.
#![deny(deprecated)]

// This crate builds for Windows targets only. `src/ddi.rs` unconditionally
// `include!`s `$OUT_DIR/d3d10umddi.rs`, which `build.rs` can only generate on
// Windows (bindgen over the WDK's d3d10umddi.h). Without this, a host
// `cargo check` produced two contradictory diagnostics — build.rs reporting a
// deliberate skip, then rustc reporting a missing generated file — neither of
// which names the actual constraint. The predicate matches `build.rs`'s own
// `TARGET`-based guard: `cfg(windows)` is evaluated against the target.
#[cfg(not(windows))]
compile_error!(
    "helios_umd builds for windows targets only: src/ddi.rs includes WDK-derived \
     bindgen output that build.rs can only generate on Windows"
);

mod adapter;
mod bridge;
pub(crate) mod caps;
mod ddi;
mod device_funcs;
mod forward;
mod knobs;

mod scanout_acquire;
mod vehicle_exports;

// `format` and `hr` moved to the shared `umd_common` crate at stage S1, `log`
// (with the rest of the mechanisms) at S2 (`DECISIONS.md` D3b). Re-exported at
// the crate root under their original names so every `crate::format::…`,
// `crate::hr::…` and `crate::log::…` path in this crate resolves unchanged —
// the move is a relocation, not a rename, and no call site was touched.
pub(crate) use helios_umd_common::{format, hr, log};

/// The DLL entry point, present for exactly one reason: to release this
/// module's process-lifetime handles when it is unloaded.
///
/// `helios_umd.dll` is loaded and unloaded ONCE PER D3D11 DEVICE — that is the
/// runtime's behaviour, not ours, and it is measured, not assumed
/// (`tools/helios_handle_types.cpp` reads `GetModuleHandleW` as NO / yes / NO
/// across one `D3D11CreateDevice` + `Release`). Rust `static`s are never
/// dropped and the loader closes nothing a module opened, so every such
/// unload stranded the log-file handle: one leaked kernel handle per device,
/// linear, no plateau. See [`log::close_at_detach`].
///
/// Rules this body obeys, all of them loader-lock rules:
/// * no allocation, no I/O, no `LoadLibrary`, no thread waits;
/// * no panic — a panic through `extern "system"` in DllMain aborts the
///   process, and this runs inside dwm;
/// * nothing at all on the `lpv_reserved != NULL` (process-exit) path, where
///   the kernel reclaims every handle and other threads are already dead.
///
/// The MSVC CRT's `_DllMainCRTStartup` calls this by name; returning non-zero
/// is "succeeded" and is the only legal answer for a DETACH notification.
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(
    _instance: *mut core::ffi::c_void,
    reason: u32,
    lpv_reserved: *mut core::ffi::c_void,
) -> i32 {
    const DLL_PROCESS_DETACH: u32 = 0;
    if reason == DLL_PROCESS_DETACH && lpv_reserved.is_null() {
        log::close_at_detach();
    }
    1
}

pub(crate) use knobs::{
    feature_level_mode, log_knob_inventory, scanout_acquire_knob, scanout_snapshot_knob,
    umd_async_present_stream, umd_command_lists, umd_deferred_diagnostics, umd_free_threaded,
    vehicle_flip_gate_us,
};
// The two writers are `#[macro_export]`ed by `umd_common`, so they live at that
// crate's root rather than in its `log` module. Re-exported here under their
// original names: every `crate::log_error!` / `crate::trace_line!` call site —
// ~430 of them — is untouched by the move.
pub(crate) use helios_umd_common::{log_error, trace_line};
pub(crate) use log::{log_self_module_path, trace_enabled};
// R420's `#![deny(deprecated)]` guard, preserved across the move: `log_line` is
// `#[deprecated]` so that only `trace_line!` and `log_error!` may reach the
// unconditional writer. Deprecation is CROSS-CRATE, so the guarantee survives
// `log_line` living in `umd_common` — verified by fault injection at each move.
// The re-export has to name it once; every CALL site that names
// `crate::log_line` still trips the deny, which is the whole guarantee.
//
// ⚠ It is now referenced by nothing, and that is its job. The macros resolve
// `$crate::log::log_line` inside `umd_common`, so this import exists ONLY to
// keep the name `crate::log_line` resolvable — which is what makes the
// documented fault injection ("add a bare `crate::log_line("x")` to a
// `forward/*` file") fail with *use of deprecated function* rather than with an
// unresolved-path error. A guard that fails for the wrong reason is not the
// guard. `allow(unused_imports)` states that, rather than deleting a test
// surface to silence a warning.
#[allow(deprecated, unused_imports)]
pub(crate) use log::log_line;

pub(crate) use caps::get_caps;
