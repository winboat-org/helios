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
mod format;
mod forward;
mod hr;
mod knobs;

mod log;
mod vehicle_exports;

pub(crate) use knobs::{feature_level_mode, present_gate_us, trace_enabled, vehicle_flip_gate_us};
pub(crate) use log::{log_error, log_knob_inventory, log_self_module_path, trace_line};
// R420's `#![deny(deprecated)]` guard, preserved across the move: `log_line` is
// `#[deprecated]` so that only `trace_line!` and `log_error!` may reach the
// unconditional writer. The re-export has to name it once; every CALL site that
// names `crate::log_line` still trips the deny, which is the whole guarantee.
#[allow(deprecated)]
pub(crate) use log::log_line;

pub(crate) use caps::get_caps;
