//! The counting noop-DDI idiom, and the size-derived table stubber.
//!
//! Moved from `umd/src/device_funcs.rs:640-754, 1128-1145` (`DECISIONS.md` D3b,
//! stage S2).
//!
//! ⛔ **`ddi_calc_size`'s 256-byte answer did NOT move.** D3b names it: that is
//! a D3D11 claim about `CalcPrivate*Size` stubs, not a shared fact, and the
//! D3D12 private-data sizes are a different contract. It stays in `umd`.
//!
//! # What is shared, and why it is worth sharing
//!
//! A WDDM UMD is handed a table of function pointers and must fill **every**
//! slot before returning, or the runtime calls through an uninitialised one.
//! Both drivers therefore need: a uniform stub signature, a stub that counts
//! its hits, a one-shot backtrace so an unexpected hit names its caller, and a
//! "fill every slot" primitive that cannot disagree with the table's size.

use core::ffi::c_void;

/// Uniform stub signature (one machine word in, one out).
///
/// Legal to install in any DDI slot on this ABI: the callee pops nothing on
/// x64, and a stub that ignores its argument and returns 0 is correct for both
/// the `HRESULT`-returning slots (`S_OK`) and the `void` ones (ignored).
pub type UniformFn = unsafe extern "C" fn(usize) -> usize;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn RtlCaptureStackBackTrace(
        frames_to_skip: u32,
        frames_to_capture: u32,
        back_trace: *mut *mut c_void,
        back_trace_hash: *mut u32,
    ) -> u16;
}

/// Log up to 32 return addresses, tagged.
///
/// The first hit of a noop stub is the interesting one: it says which runtime
/// call reached a slot the driver never implemented, which is the difference
/// between "this DDI is missing" and "this DDI is missing *and something
/// actually calls it*". Subsequent hits log only a count — capturing and
/// formatting 32 frames on every hit was itself a measured cost.
///
/// # Safety
/// Calls `RtlCaptureStackBackTrace`, which walks the caller's stack. Safe in
/// any ordinary user-mode context; not callable from a context where the stack
/// is being unwound.
pub unsafe fn log_backtrace(tag: &str) {
    let mut frames = [core::ptr::null_mut::<c_void>(); 32];
    let captured = unsafe {
        RtlCaptureStackBackTrace(
            0,
            frames.len() as u32,
            frames.as_mut_ptr(),
            core::ptr::null_mut(),
        )
    };
    let mut out = String::new();
    for i in 0..captured as usize {
        out.push_str(&format!(" #{i}=0x{:x}", frames[i] as usize));
    }
    crate::log_error!("{tag} stack{out}");
}

/// Write `noop` into every pointer-sized slot of a `bytes`-byte DDI table.
///
/// ⭐ **This is the primitive, and it takes a BYTE COUNT, not a type.**
/// `ARCHITECTURE.md` §12 rule 16 is the R702 class: 24H2 passed **576** bytes
/// for a 592-byte `DRIVERCAPS`, and `pfnFillDDITable` parameterises the size
/// explicitly (`d3d12umddi.h:2527-2528`). A D3D12 filler must derive its slot
/// count from the runtime's argument; deriving it from `size_of::<T>()` would
/// write past the end of a table the runtime deliberately sized smaller.
///
/// [`stub_fill_sized_table`] is the D3D11-shaped convenience on top, and it is
/// the one that is *only* correct because the D3D11 DDI does not pass a size.
///
/// # Safety
/// `funcs` must point at `bytes` writable bytes, pointer-aligned, every one of
/// which the caller owns and intends to be a function-pointer slot. `bytes` is
/// rounded DOWN to a whole number of slots; a trailing partial slot is left
/// untouched rather than half-written.
pub unsafe fn stub_fill_bytes(funcs: *mut c_void, bytes: usize, noop: UniformFn) {
    let n = bytes / core::mem::size_of::<usize>();
    let slots = funcs as *mut Option<UniformFn>;
    for i in 0..n {
        unsafe { *slots.add(i) = Some(noop) };
    }
}

/// Fill every entry of a `T`-shaped DDI table with `noop`, deriving the slot
/// count from `size_of::<T>()`.
///
/// The slot count cannot disagree with the table actually being filled. That
/// matters: the failure mode this replaces is a wrong length under-stubbing a
/// table and leaving uninitialised slots past the prefix, which is precisely
/// what `fill_dxgi_1_3_base_funcs`'s comment exists to warn about — the three
/// D3D11 fills each spelled the length out by hand.
///
/// ⚠ Correct **only** where the runtime does not tell the driver the table
/// size. That is true of the d3d10umddi device-funcs tables and NOT of
/// `pfnFillDDITable` — see [`stub_fill_bytes`].
///
/// # Safety
/// `funcs` must point to a writable `T` whose every field is a pointer-sized
/// `Option<fn>`.
pub unsafe fn stub_fill_sized_table<T>(funcs: *mut T, noop: UniformFn) {
    unsafe { stub_fill_bytes(funcs.cast::<c_void>(), core::mem::size_of::<T>(), noop) }
}
