//! The three `helios_umd12_probe_*_v1` exports: **test-only evidence
//! instruments**, and nothing else.
//!
//! They are the `D12-G1` **`umd12` arm** — `tools/d3d12_bridge_probe.cpp`
//! compiled with `-DHELIOS_G1_UMD12` resolves these three names out of
//! `helios_umd12.dll` and runs its existing 28 steps through them, so the
//! triangle is drawn by the engine *inside this DLL* rather than inside a probe
//! `.exe` that linked the same archive.
//!
//! ⛔ **No runtime, no loader and no ICD resolves any of these by name.** That
//! is deliberate and it is guardrail 7 of the S4 contract: the Mesa venus ICD
//! walks every loaded module looking for `helios_umd_set_present_source`,
//! `helios_umd_wait_last_present` and `helios_umd_get_present_result`,
//! first-hit-wins — so a D3D12 export inside that family would silently steal
//! the D3D11 present vehicle from `helios_umd.dll`. The `helios_umd12_probe_`
//! prefix is outside it by construction.
//!
//! # Why the arm exists at all
//!
//! `D12-G1`'s static arm already proved the clang-cl archives draw a triangle —
//! but it proved it in a probe `.exe`. `helios_umd12.dll` is a different
//! artifact: a Rust `cdylib` with `panic = "abort"`, `lto = "thin"`, the cxx
//! glue and the MSVC CRT. ⚠ `_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH` is
//! *required* to build it (MSVC 14.44's `<yvals_core.h>` hard-asserts a Clang
//! ≥ 19 and the installed clang-cl is 17.0.6) — that is a risk acknowledgement,
//! not a fix, and it is the first suspect if the engine misbehaves here but not
//! in the static arm. These exports are how that difference gets measured
//! instead of argued.

use core::ffi::c_void;

use helios_umd_common::hr::{Hresult, E_FAIL, E_INVALIDARG, S_OK};
use windows::core::Interface;

use crate::bridge12::{self, BridgeDevice12};
use crate::{init_once, note_refusal, UMD12_REFUSALS};

/// Create a vkd3d device on the adapter with this LUID and hand back both
/// halves of it.
///
/// ⛔ **The LUID crosses as two scalars, never as a `LUID` struct by value.**
/// `winnt.h` declares `typedef struct _LUID { DWORD LowPart; LONG HighPart; }`
/// — unsigned low first, signed high second — and an 8-byte POD does travel in
/// one register under the MS x64 ABI, so a `#[repr(C)]` twin would almost
/// certainly be correct. Scalars are chosen because they cannot be *almost*
/// correct: the whole chain is scalar from here down — this export →
/// [`BridgeDevice12::create`]`(u32, i32)` → cxx → `vkd3d_bridge.cpp`, which is
/// the single place the two halves are reassembled into a `LUID`.
///
/// ⭐ **Transport is measured, not assumed.** `D12-G1`'s `umd12` arm passes the
/// adapter's real LUID and step 04 reads `GetAdapterLuid` back off the created
/// device: `tmp/dx12/gates/G1-umd12/bridge_probe_umd12.txt` shows
/// `Helios adapter luid=00000000:000076b5` and
/// `device reports AdapterLuid = 00000000:000076b5`. A swapped pair would read
/// `000076b5:00000000`, so that echo is a real end-to-end check of this seam.
///
/// ⚠ What it does **not** prove is LUID-based *selection*. That echo is vkd3d
/// returning the `adapter_luid` it was handed, and `helios_entry.c:172-179`
/// passes `vk_physical_device = VK_NULL_HANDLE` — delegating to
/// `vkd3d_select_physical_device`, which that comment says in as many words "is
/// NOT LUID matching". On a single-GPU guest a *wrong* LUID would still yield a
/// working device. Selection becomes checkable only once vkd3d chains
/// `VkPhysicalDeviceIDProperties` and matches `deviceLUID`.
///
/// * `out_bridge` receives an **opaque owned handle** — the boxed
///   [`BridgeDevice12`]. The caller must return it to
///   [`helios_umd12_probe_destroy_device_v1`] exactly once.
/// * `out_device` receives a **BORROWED** `ID3D12Device*`: the bridge keeps the
///   owning reference for as long as the handle above lives. ⚠ The probe's
///   third arm therefore `AddRef`s it before use, so that its own symmetric
///   `Release` — shared byte-for-byte with the other two arms — stays correct.
///
/// Returns `S_OK`, or `E_INVALIDARG` / `E_FAIL` with a named counter bumped.
///
/// # Safety
/// The caller guarantees that `out_bridge` and `out_device`, when non-null,
/// each point at writable storage for one pointer, valid for the duration of
/// the call and not aliased by another thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_umd12_probe_create_device_v1(
    luid_low: u32,
    luid_high: i32,
    out_bridge: *mut *mut c_void,
    out_device: *mut *mut c_void,
) -> Hresult {
    // ⛔ Above the first log line of this entry point: `log::init`'s basename
    // defaults to "umd", and the log path is latched by whichever line gets
    // there first. See `crate::init_once`.
    init_once();

    if out_bridge.is_null() || out_device.is_null() {
        note_refusal(&UMD12_REFUSALS.probe12_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: both pointers were null-checked immediately above, and the caller
    // guarantees each points at writable single-pointer storage. Zeroing first
    // means every failure return below leaves both outs null without a second
    // write on each path.
    unsafe {
        *out_bridge = core::ptr::null_mut();
        *out_device = core::ptr::null_mut();
    }

    let Some(bridge) = BridgeDevice12::create(luid_low, luid_high) else {
        // The C++ side has already logged the engine's HRESULT into
        // `umd12-<pid>.log`; this is the countable half of the same event.
        note_refusal(&UMD12_REFUSALS.probe12_create_failed);
        return E_FAIL;
    };

    // BORROWED, and it must stay borrowed: `d3d12_device` hands back a
    // `ManuallyDrop`, so the wrapper going out of scope at the end of this
    // function issues no `Release`. Adopting it instead would drop the bridge's
    // only reference here and destroy the device the caller is about to use —
    // the exact failure `bridge12`'s module doc names.
    let Some(device) = bridge.d3d12_device() else {
        // `bridge` is dropped by this early return, which releases the engine
        // reference. ⚠ That drop is the point: a failure path that boxed the
        // bridge first and then bailed would leak the whole Vulkan device.
        note_refusal(&UMD12_REFUSALS.probe12_no_device);
        return E_FAIL;
    };
    let device_raw = device.as_raw();

    // Ownership of the bridge transfers to the caller here and nowhere earlier,
    // so every path above this line drops it.
    let handle = Box::into_raw(Box::new(bridge)).cast::<c_void>();
    // SAFETY: as above — both out-params are non-null, writable and unaliased.
    // `handle` is a fresh `Box::into_raw` and `device_raw` is the bridge's live
    // device, which `handle` keeps alive.
    unsafe {
        *out_bridge = handle;
        *out_device = device_raw;
    }
    S_OK
}

/// Drop the bridge and its engine reference. Null-tolerant.
///
/// ⚠ The probe's third arm calls this **before** its own `Release`, so that the
/// probe's release is the last one and step 28's "refcount 0" assertion is
/// arm-identical. Calling it after would make the count differ by one purely
/// because of how the arm obtained the device.
///
/// # Safety
/// `bridge` must be null, or a pointer returned by
/// [`helios_umd12_probe_create_device_v1`] that has not already been passed
/// here. Passing it twice is a double free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_umd12_probe_destroy_device_v1(bridge: *mut c_void) {
    if bridge.is_null() {
        // Not a refusal and deliberately uncounted: "safe to call with NULL" is
        // this export's documented contract (§3.3), so a null here is the
        // caller using the API correctly on a path where create failed.
        return;
    }
    // SAFETY: the caller guarantees this is a live, not-yet-destroyed handle
    // from `helios_umd12_probe_create_device_v1`, which produced it with
    // `Box::into_raw(Box::new(BridgeDevice12))` — so the pointee type, the
    // allocator and the alignment all match. Dropping the box drops the
    // `cxx::UniquePtr`, which runs the out-of-line C++ destructor and releases
    // the engine's `ID3D12Device`.
    drop(unsafe { Box::from_raw(bridge.cast::<BridgeDevice12>()) });
}

/// Serialize a root signature through the engine. Stateless — no bridge handle
/// and no device involved.
///
/// `desc` is a `const D3D12_ROOT_SIGNATURE_DESC*`, `version` a
/// `D3D_ROOT_SIGNATURE_VERSION`. `blob_out` and `err_out` receive **owned**
/// `ID3DBlob*` values (null when absent); the caller `Release`s each non-null
/// one exactly once.
///
/// # Safety
/// `desc` must point at a live `D3D12_ROOT_SIGNATURE_DESC` (including every
/// parameter/sampler array it references) for the duration of the call.
/// `blob_out`, when non-null, must point at writable single-pointer storage;
/// `err_out` may be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_umd12_probe_serialize_root_signature_v1(
    desc: *const c_void,
    version: u32,
    blob_out: *mut *mut c_void,
    err_out: *mut *mut c_void,
) -> Hresult {
    init_once();

    // ⚠ `err_out` is explicitly allowed to be null (that is the shape of
    // `D3D12SerializeRootSignature` itself), so it is NOT part of this check.
    if desc.is_null() || blob_out.is_null() {
        note_refusal(&UMD12_REFUSALS.probe12_bad_arg);
        return E_INVALIDARG;
    }

    // Locals rather than a cast of the caller's `void**` to `usize*`: the two
    // are the same width on x64, but the bridge's declared type is `*mut usize`
    // and laundering a pointer-to-pointer through it is the kind of "obviously
    // fine" cast that stops being fine the moment either side's type changes.
    let mut blob: usize = 0;
    let mut err: usize = 0;
    // SAFETY: `desc` is live per the caller's guarantee above; `blob` and `err`
    // are stack locals borrowed only for this call. The C++ side zeroes both
    // outs before forwarding and writes them only on success.
    let hr = unsafe { bridge12::serialize_root_signature(desc as usize, version, &mut blob, &mut err) };

    // SAFETY: `blob_out` was null-checked above. This write transfers the
    // engine's owned `ID3DBlob*` reference to the caller.
    unsafe {
        *blob_out = blob as *mut c_void;
    }

    if err_out.is_null() {
        // ⚠ A null `err_out` with a real error blob is a LEAK if the blob is
        // merely dropped on the floor — the engine AddRef'd it for us and there
        // is no second owner. Release it here instead, and count the fact that
        // a diagnostic message was thrown away: on a failing serialize that
        // blob is the only text saying *why*, so silently discarding it turns a
        // named error into a bare HRESULT.
        if err != 0 {
            note_refusal(&UMD12_REFUSALS.probe12_err_blob_dropped);
            // SAFETY: `err` is non-zero, so it is an `ID3DBlob*` the engine
            // AddRef'd for this caller. Every COM interface is an `IUnknown`,
            // and dropping the adopted wrapper issues exactly the one matching
            // `Release`. Adopted as `IUnknown` rather than `ID3DBlob` only
            // because `Release` is all that is wanted and `IUnknown` needs no
            // extra `windows` feature.
            drop(unsafe { windows::core::IUnknown::from_raw(err as *mut c_void) });
        }
    } else {
        // SAFETY: non-null per the branch, and writable single-pointer storage
        // per the caller's guarantee. Transfers the owned reference.
        unsafe {
            *err_out = err as *mut c_void;
        }
    }
    hr
}
