//! `helios_umd12.dll` — the Helios D3D12 user-mode display driver.
//!
//! # Status: SKELETON. `OpenAdapter12` refuses, and must keep refusing.
//!
//! This crate exists so the two-cdylib layout — build, mirror, sign, install,
//! and `UserModeDriverName[3]` — can be proven end to end **before** any DDI
//! code is written. It implements no DDI, fills no table, and loads no engine.
//!
//! ⛔ **The standing rule** (`DECISIONS.md` §7.1, `DX12.md` §3.2):
//!
//! > `OpenAdapter12` must stop refusing **in the same commit** that makes its
//! > body reachable — or the body must not be written yet.
//!
//! R908 (`e315d03`) is what that rule cost to learn: five hand-written
//! `D3d12Ddi*` ABI structs, eight `d3d12_*` handlers, a whole hand-transcribed
//! caps policy and `D3D12_SUPPORTED_DDI_VERSIONS` sat behind an unconditional
//! early return with `#[allow(unreachable_code)]` silencing the compiler's
//! proof that it was dead. ~230 lines that read as a live contract and could
//! never run. **Do not write a body here until the commit that reaches it.**
//!
//! # What comes next, in order (`ARCHITECTURE.md` §11)
//!
//! | stage | content |
//! |---|---|
//! | **S3** | `build.rs` + bindgen of `d3d12umddi.h` with `layout_tests(true)` + `ddi12.rs`. The layout assertions ARE the deliverable: if it compiles, the ABI is machine-checked. Still refusing, still not deployed. |
//! | **S4** | `vkd3d_bridge.{h,cpp}` + `bridge12.rs` — `helios_vkd3d_create_device` only, reached by a `tools/` probe. No runtime, no INF, no registry. |
//! | **S4b** | The ICD anchor (`helios_icd_anchor_v1`) — must land before the first two-engine run. |
//! | **S5** | INF + slot 3; `umd` **drops** its `OpenAdapter12` export and this one becomes reachable and stops refusing — **all in one commit** — with the `UmdD3D12` kill switch, default OFF. |
//! | **S6** | The DDI surface in `forward12/*`: caps first (H4), then device/queue/command-list, then descriptors, then present. |
//!
//! # Two things measured about this DDI that shape the crate (`D12-G5`)
//!
//! - The negotiated version is `D3D12DDI_SUPPORTED_0110`, but **`_0040` is
//!   accepted by this Windows build and a triangle presents on it** — 96 core +
//!   58 CL slots instead of 124 + 75. That choice belongs to P3.
//!   ⚠ `_0110` is not merely a bigger table: it is a behavioural contract with
//!   thirteen `VulkanOn12` obligations that carry no cap and cannot be declined
//!   (`SUBSTRATE.md` §4.5).
//! - **There is no DXGI table.** `D3D12DDI_TABLE_TYPE_DXGI` was never requested
//!   across 20 flip-model presents; present arrives on the command-list table.
//!   `ResourceHeaps.md` says why: *"the entire [DXGI] table is deprecated."*
//!
//! # Why there is no `log_error!` here yet
//!
//! `umd`'s `log` module has not moved to `umd_common` — that is stage **S2**.
//! ⛔ Copying it would be exactly the duplication D3b forbids ("copy-paste
//! between `umd` and `umd12` is a defect, not a shortcut"), so this crate uses
//! the raw `OutputDebugStringA` primitive until `log` is shared, at which point
//! `log::init("umd12")` gives this DLL `umd12-<pid>.log` beside D3D11's
//! `umd-<pid>.log`.

#![deny(deprecated)]

// Mirrors `umd/src/lib.rs`: this is a Windows display driver and nothing in it
// is meaningful on another target. Failing at the top is clearer than failing
// deep inside a platform intrinsic.
#[cfg(not(windows))]
compile_error!(
    "helios_umd12 is a Windows-only WDDM user-mode display driver; it cannot be built for a \
     host target"
);

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

use helios_umd_common::hr::{Hresult, DXGI_ERROR_UNSUPPORTED};

/// How many times the runtime asked this driver for a D3D12 adapter and was
/// refused.
///
/// CLAUDE.md rule 2: *every skipped/refused path gets a named counter — loud
/// failure over fake success.* This is that counter for the one refusal the
/// crate currently has. It is read through the debug string below rather than a
/// registry value because `knobs`/`log` have not moved to `umd_common` yet (S2);
/// when they do, this becomes an ordinary named refusal counter on the shared
/// mechanism.
static OPEN_ADAPTER12_REFUSALS: AtomicUsize = AtomicUsize::new(0);

/// Emit one line to the debugger. Deliberately the rawest possible primitive —
/// see the crate doc for why this is not `log_error!`.
fn debug_line(s: &str) {
    // SAFETY: `OutputDebugStringA` reads a NUL-terminated byte string and does
    // not retain the pointer. `buf` is a local `Vec<u8>` that outlives the call
    // and is NUL-terminated immediately below, so the pointer is valid for the
    // whole call. The function is safe at any IRQL-equivalent user-mode context
    // and has no failure mode we can observe.
    unsafe {
        let mut buf = Vec::with_capacity(s.len() + 1);
        buf.extend_from_slice(s.as_bytes());
        buf.push(0);
        windows::Win32::System::Diagnostics::Debug::OutputDebugStringA(windows::core::PCSTR(
            buf.as_ptr(),
        ));
    }
}

/// The D3D12 adapter entry point — **exported and refusing, on purpose.**
///
/// The loader resolves this by name, so a missing export is a different and
/// worse failure than a clean refusal.
///
/// `open_data` is `*mut c_void` because nothing here reads it. Giving it a real
/// type would mean either hand-transcribing `D3D12DDIARG_OPENADAPTER` — banned
/// by `DECISIONS.md` §7.2, which records a 376..392-byte out-of-bounds write
/// into the runtime's heap from exactly that mistake — or bindgen'ing the D3D12
/// header, which is stage S3.
///
/// ⚠ Until S5, `helios_umd.dll` also exports a refusing `OpenAdapter12` and is
/// the one the INF registers. Both refusing simultaneously is the intended
/// state; S5 deletes `umd`'s export and registers this DLL at slot 3 in the
/// same commit that makes this body reachable.
#[no_mangle]
pub unsafe extern "system" fn OpenAdapter12(open_data: *mut c_void) -> Hresult {
    let n = OPEN_ADAPTER12_REFUSALS.fetch_add(1, Ordering::Relaxed);
    let _ = open_data;
    debug_line(&format!(
        "helios_umd12: OpenAdapter12 -> DXGI_ERROR_UNSUPPORTED (skeleton; refusal #{})\n",
        n + 1
    ));
    // ⛔ Declining an unimplemented DDI is DXGI_ERROR_UNSUPPORTED (0x887A_0004),
    // NEVER DXGI_ERROR_DRIVER_INTERNAL_ERROR (0x887A_0020) — the latter is
    // recorded by the runtime and by ETW as a *driver fault*, so a client's
    // ordinary "this driver has no D3D12 DDI" negotiation would be logged as a
    // Helios bug. `umd`'s copy of this export returned the wrong one until R801
    // because the two shared a constant name and both printed identically in
    // our own logs. `helios_umd_common::hr` is why that can no longer recur:
    // one numeric literal per HRESULT, in one file, with build-time asserts.
    DXGI_ERROR_UNSUPPORTED
}
