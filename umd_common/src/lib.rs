//! Code shared by the Helios D3D11 (`helios_umd.dll`) and D3D12
//! (`helios_umd12.dll`) user-mode display drivers.
//!
//! # Why this crate exists
//!
//! `DECISIONS.md` D3 ships **two DLLs**: `helios_umd.dll` keeps
//! `UserModeDriverName` slots 0–2 and `helios_umd12.dll` takes slot 3. That
//! buys blast-radius isolation — D3D12 is one registry rewrite away from being
//! disabled with D3D11 provably untouched — and it keeps vkd3d + dxil-spirv out
//! of the DLL dwm loads and unloads once per D3D11 device create.
//!
//! D3b is the rule that stops the split from costing duplication:
//!
//! > ⛔ If a D3D12 file would contain code that also exists in `umd/`, that code
//! > moves to `umd_common` first and both crates depend on it. Copy-paste
//! > between `umd` and `umd12` is a defect, not a shortcut.
//!
//! # What may live here
//!
//! Only code that is genuinely **not D3D-version-specific**. The bindgen'd DDI
//! types do not qualify — D3D11 and D3D12 generate different headers, so each
//! cdylib keeps its own `ddi` module and its own `build.rs`.
//!
//! ⚠ **This crate must build on Linux as well as Windows.** That is not
//! aesthetic: `tools/format-table-check.rs` `#[path]`-includes [`format`] and
//! runs on the Linux host, and it is the only test of the DXGI format table
//! that executes anywhere. Anything Windows-only goes behind
//! `#[cfg(windows)]` with its dependency under
//! `[target.'cfg(windows)'.dependencies]`.
//!
//! # Ordering — load-bearing, not stylistic
//!
//! D3b requires the extraction to land **before** `umd12` has any content:
//! extracting from one caller is a mechanical, provable refactor; extracting
//! after a second caller exists is a merge. The instrument that proves a move
//! was behaviour-preserving is `log_knob_inventory()`'s output coming out
//! byte-identical (`ARCHITECTURE.md` §11, `S2-check`).
//!
//! # Migration status
//!
//! | module | from | state |
//! |---|---|---|
//! | [`hr`] | `umd/src/hr.rs` | ✅ moved |
//! | [`format`] | `umd/src/format.rs` | ✅ moved |
//! | [`throttle`] | `umd/src/forward.rs` | ✅ moved |
//! | [`window`] | `umd/src/device_funcs.rs` | ✅ moved |
//! | `log` | `umd/src/log.rs` | ⏳ S2 — needs `init(basename)` so D3D11 keeps `umd-<pid>.log` and D3D12 writes `umd12-<pid>.log` |
//! | `knobs` | `umd/src/knobs.rs` | ⏳ S2 — the `reg_dword` FFI site and the inventory mechanism move; the knob VALUES stay per-crate |
//! | `refusals` / `noop` | `umd/src/forward.rs`, `umd/src/device_funcs.rs` | ⏳ S2 — the MECHANISM moves, generalised to `RefusalCounter { count, name }`; the eleven D3D11 counter fields stay in `umd` |
//! | [`slot`] | `umd/src/forward/handles.rs` | ✅ moved (`#[cfg(windows)]`) |
//! | C++ `bridge/` | `umd/bridge/` | ✅ moved — `bridge_common.h`, `bridge_util.h` (`PeriodicStat`, `qpc_elapsed_us`, `ComRelease<T>`), `bridge_guard.h` |
//! | `log` | `umd/src/log.rs` | ⏳ S2 — needs `init(basename)` so D3D11 keeps `umd-<pid>.log` and D3D12 writes `umd12-<pid>.log` |
//! | `knobs` | `umd/src/knobs.rs` | ⏳ S2 — the `reg_dword` FFI site and the inventory mechanism move; the knob VALUES stay per-crate |
//! | `refusals` / `noop` | `umd/src/forward.rs`, `umd/src/device_funcs.rs` | ⏳ S2 — the MECHANISM moves, generalised to `RefusalCounter { count, name }`; the eleven D3D11 counter fields stay in `umd` |
//!
//! ⛔ **`slot` moved the code and NOT the claim.** The
//! `Slot<Boxed<S>>::get() -> &'static S` soundness argument rests on the D3D11
//! runtime's `CUseCountedObject` ordering; [`slot::Slot::get`]'s doc now states
//! that it is **established for D3D11 and NOT established for D3D12**, and
//! names what the first `umd12` caller owes. Re-derive, do not inherit (D3b).
//!
//! ⛔ **`bridge_guard` kept its `static_assert`** (commit `ead692e`). A second
//! guard template written from scratch is exactly how that truncation bug —
//! which crash-looped dwm and LogonUI at cold boot — comes back.
//! `git grep -n 'static_assert' umd/bridge umd12/bridge umd_common/bridge`
//! must return exactly one hit.

#![deny(deprecated)]

pub mod format;
pub mod hr;
#[cfg(windows)]
pub mod slot;
pub mod throttle;
pub mod window;
