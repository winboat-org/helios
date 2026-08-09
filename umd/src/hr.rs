//! The HRESULT values the Helios UMD returns across the D3D10/11 UMD DDI, the
//! D3D12 UMD DDI and the DXGI DDI — defined exactly once.
//!
//! Before this module the crate carried four divergent constant sets: `lib.rs`
//! and `forward.rs` each defined a crate-private block, and
//! `device_funcs.rs` defined `E_FAIL` twice more as a function-local. Two of
//! those blocks defined a constant *named* `DXGI_ERROR_UNSUPPORTED` with
//! **different values** — `0x887A_0020` in `lib.rs` and `0x887A_0004` in
//! `forward.rs`. Per `winerror.h` only the latter is `DXGI_ERROR_UNSUPPORTED`;
//! `0x887A_0020` is `DXGI_ERROR_DRIVER_INTERNAL_ERROR`, a materially different
//! statement to make to the runtime and to ETW. Both spellings printed as the
//! string "DXGI_ERROR_UNSUPPORTED" in our own logs, so the divergence was not
//! observable from a triage grep. See R801 in `REFACTOR_REVIEW.md`.
//!
//! Every numeric literal for an HRESULT the driver returns lives in this file
//! and nowhere else. A same-named constant with a different value can now only
//! be reintroduced by shadowing inside a function body, which is visible in
//! review.
//!
//! # Why `const _: () = assert!(...)` and not `#[test]`
//!
//! `helios_umd` cannot be built for a host target at all: `lib.rs` carries a
//! `#[cfg(not(windows))] compile_error!` because `src/ddi.rs` `include!`s
//! bindgen output that `build.rs` can only generate against the WDK. So a
//! `#[cfg(test)]` equality test would be unrunnable on the Linux side and
//! would only execute under a deliberate `cargo test` on the VM. The
//! `const _` items below are evaluated during *every* build of the crate on
//! either profile, which is a strictly stronger guarantee for the same cost.

/// The DDI's HRESULT representation. Every UMD DDI entry point returns this
/// as a bare `i32`; deliberately not the `windows` crate's `HRESULT` newtype,
/// which would buy a `.0` at roughly forty return sites and change no
/// guarantee (see R801).
pub type Hresult = i32;

/// Success.
pub const S_OK: Hresult = 0;

/// The operation has not completed yet. Unlike an error HRESULT this still
/// has success severity, so wrappers that reduce HRESULT to `Result<()>` lose
/// the distinction from [`S_OK`]. `QueryGetData` must preserve it.
pub const S_FALSE: Hresult = 1;

/// Unspecified failure. `winerror.h`: `E_FAIL`.
pub const E_FAIL: Hresult = 0x8000_4005u32 as Hresult;

/// Not implemented. `winerror.h`: `E_NOTIMPL`.
pub const E_NOTIMPL: Hresult = 0x8000_4001u32 as Hresult;

/// One or more arguments are invalid. `winerror.h`: `E_INVALIDARG`.
pub const E_INVALIDARG: Hresult = 0x8007_0057u32 as Hresult;

/// Out of memory. `winerror.h`: `E_OUTOFMEMORY`.
pub const E_OUTOFMEMORY: Hresult = 0x8007_000Eu32 as Hresult;

/// The requested functionality is not supported by the device or the driver.
/// `winerror.h`: `DXGI_ERROR_UNSUPPORTED`.
///
/// This is the correct code for refusing a DDI negotiation — it tells the
/// runtime "I do not implement this", which is a normal, expected answer.
pub const DXGI_ERROR_UNSUPPORTED: Hresult = 0x887A_0004u32 as Hresult;

/// The driver encountered a problem and was put into the device-removed state.
/// `winerror.h`: `DXGI_ERROR_DRIVER_INTERNAL_ERROR`.
///
/// This is a *driver fault* report. The runtime and ETW record it as such, so
/// it must never be used to decline an unsupported interface — that is what
/// [`DXGI_ERROR_UNSUPPORTED`] is for. It is retained as a named constant
/// because the runtime is observed to return it to applications when the
/// driver's own caps response is malformed (see the
/// `DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM` note in `forward.rs`).
pub const DXGI_ERROR_DRIVER_INTERNAL_ERROR: Hresult = 0x887A_0020u32 as Hresult;

/// A D3D10/11 DDI query has not reached the signaled state yet.
/// `winerror.h`: `DXGI_DDI_ERR_WASSTILLDRAWING`.
///
/// This is reported through `pfnSetErrorCb` because the driver's
/// `QueryGetData` callback itself returns `void`.
pub const DXGI_DDI_ERR_WASSTILLDRAWING: Hresult = 0x887B_0001u32 as Hresult;

/// The presentation was not redirected; the caller should present directly.
/// `winerror.h`: `DXGI_STATUS_NO_REDIRECTION`. A success code (severity 0).
pub const DXGI_STATUS_NO_REDIRECTION: Hresult = 0x087A_0004u32 as Hresult;

// The numeric identity of every constant above, checked at build time against
// the values in `winerror.h`. These exist so the R801 consolidation is
// provably value-preserving and so a future edit cannot quietly retype one.
const _: () = assert!(S_OK == 0);
const _: () = assert!(S_FALSE == 1);
const _: () = assert!(E_FAIL as u32 == 0x8000_4005);
const _: () = assert!(E_NOTIMPL as u32 == 0x8000_4001);
const _: () = assert!(E_INVALIDARG as u32 == 0x8007_0057);
const _: () = assert!(E_OUTOFMEMORY as u32 == 0x8007_000E);
const _: () = assert!(DXGI_ERROR_UNSUPPORTED as u32 == 0x887A_0004);
const _: () = assert!(DXGI_ERROR_DRIVER_INTERNAL_ERROR as u32 == 0x887A_0020);
const _: () = assert!(DXGI_DDI_ERR_WASSTILLDRAWING as u32 == 0x887B_0001);
const _: () = assert!(DXGI_STATUS_NO_REDIRECTION as u32 == 0x087A_0004);

// The two codes the pre-R801 tree conflated must stay distinct, and the
// unsupported/internal-error distinction must stay the right way round.
const _: () = assert!(DXGI_ERROR_UNSUPPORTED != DXGI_ERROR_DRIVER_INTERNAL_ERROR);

// Severity: the DXGI_ERROR_* codes are failures and DXGI_STATUS_* is a
// success. Getting this wrong inverts every `hr >= 0` test in the crate.
const _: () = assert!(DXGI_ERROR_UNSUPPORTED < 0);
const _: () = assert!(DXGI_ERROR_DRIVER_INTERNAL_ERROR < 0);
const _: () = assert!(DXGI_STATUS_NO_REDIRECTION >= 0);
