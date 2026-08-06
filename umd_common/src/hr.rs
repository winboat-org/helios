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

/// The GPU device instance has been suspended. `winerror.h`:
/// `DXGI_ERROR_DEVICE_REMOVED`.
///
/// ⚠ The code the **engine** returns for a lost device, as against
/// [`D3DDDIERR_DEVICEREMOVED`], which is the code the **kernel DDI** uses for
/// the same condition. `umd12::device12::command_list_error_code` maps the one
/// to the other rather than folding a lost device into an application error:
/// the two call for different recovery.
pub const DXGI_ERROR_DEVICE_REMOVED: Hresult = 0x887A_0005u32 as Hresult;

/// The presentation was not redirected; the caller should present directly.
/// `winerror.h`: `DXGI_STATUS_NO_REDIRECTION`. A success code (severity 0).
pub const DXGI_STATUS_NO_REDIRECTION: Hresult = 0x087A_0004u32 as Hresult;

// ── The two `D3DDDIERR_*` codes a D3D12 command-list error may carry ────────
//
// ⛔ These are NOT `winerror.h` codes. They come from `d3dumddi.h`'s own
// facility, and they are here because `pfnSetCommandListErrorCb` accepts
// **exactly three** HRESULTs and two of them are these
// (`tmp/dx12/specs/d3d/CPUEfficiency.md:2143-2158`; the third is
// [`E_OUTOFMEMORY`], already above). Anything else sent to that callback is
// outside the contract, and the callback is the only way a D3D12 recording DDI
// can fail without taking the whole device down — see
// `umd12::device12::command_list_error_code`.
//
// ⚠ **Derived, not transcribed.** `d3dumddi.h:4716-4717` defines
// `_FACD3DDDI 0x876` and `MAKE_D3DDDIHRESULT(code) MAKE_HRESULT(1, _FACD3DDDI,
// code)`, i.e. `0x8000_0000 | (0x876 << 16) | code`. Both constants below are
// written as that expression over their decimal code, so the arithmetic is the
// compiler's and the only hand-written number is the one the header states.
// `ARCHITECTURE.md` §12 rule 1 forbids hand-transcribing an ABI, and a
// pre-computed hex literal here would be exactly that.

/// Build a `D3DDDIERR_*` value the way `d3dumddi.h:4717` does.
const fn make_d3dddi_hresult(code: u32) -> Hresult {
    (0x8000_0000u32 | (0x876u32 << 16) | code) as Hresult
}

/// The device was removed. `d3dumddi.h:4723`: `MAKE_D3DDDIHRESULT(2160)`.
///
/// One of the three codes `pfnSetCommandListErrorCb` accepts.
pub const D3DDDIERR_DEVICEREMOVED: Hresult = make_d3dddi_hresult(2160);

/// The application did something wrong. `d3dumddi.h:4734`:
/// `MAKE_D3DDDIHRESULT(2181)`.
///
/// ⭐ The code a D3D12 recording DDI uses to say *"this command list is
/// unusable because of what was recorded into it"*, which the runtime answers
/// by dropping further recording calls on **that list** rather than by removing
/// the device.
///
/// ⚠ The spec spells it `D3DDDIERROR_APPLICATIONERROR`; the header spells it
/// `D3DDDIERR_APPLICATIONERROR`. Same value, and the header's spelling is the
/// one used here because the header is what compiles.
pub const D3DDDIERR_APPLICATIONERROR: Hresult = make_d3dddi_hresult(2181);

// The numeric identity of every constant above, checked at build time against
// the values in `winerror.h`. These exist so the R801 consolidation is
// provably value-preserving and so a future edit cannot quietly retype one.
const _: () = assert!(S_OK == 0);
const _: () = assert!(E_FAIL as u32 == 0x8000_4005);
const _: () = assert!(E_NOTIMPL as u32 == 0x8000_4001);
const _: () = assert!(E_INVALIDARG as u32 == 0x8007_0057);
const _: () = assert!(E_OUTOFMEMORY as u32 == 0x8007_000E);
const _: () = assert!(DXGI_ERROR_UNSUPPORTED as u32 == 0x887A_0004);
const _: () = assert!(DXGI_ERROR_DRIVER_INTERNAL_ERROR as u32 == 0x887A_0020);
const _: () = assert!(DXGI_STATUS_NO_REDIRECTION as u32 == 0x087A_0004);
const _: () = assert!(DXGI_ERROR_DEVICE_REMOVED as u32 == 0x887A_0005);
// ⚠ These two are checked the other way round from the rest: the constants
// above are hand-written hex checked against a decimal expression, and these are
// a decimal expression checked against the hex `MAKE_D3DDDIHRESULT` produces.
// Both directions exist so a typo in either the code number or the facility is
// a build failure rather than a wrong HRESULT on a rarely-taken path.
const _: () = assert!(D3DDDIERR_DEVICEREMOVED as u32 == 0x8876_0870);
const _: () = assert!(D3DDDIERR_APPLICATIONERROR as u32 == 0x8876_0885);

// The two codes the pre-R801 tree conflated must stay distinct, and the
// unsupported/internal-error distinction must stay the right way round.
const _: () = assert!(DXGI_ERROR_UNSUPPORTED != DXGI_ERROR_DRIVER_INTERNAL_ERROR);

// Severity: the DXGI_ERROR_* codes are failures and DXGI_STATUS_* is a
// success. Getting this wrong inverts every `hr >= 0` test in the crate.
const _: () = assert!(DXGI_ERROR_UNSUPPORTED < 0);
const _: () = assert!(DXGI_ERROR_DRIVER_INTERNAL_ERROR < 0);
const _: () = assert!(DXGI_STATUS_NO_REDIRECTION >= 0);
