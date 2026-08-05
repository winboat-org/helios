//! A runtime-owned buffer window: a pointer and its capacity as one value.
//!
//! Moved verbatim out of `umd/src/device_funcs.rs` by the `umd_common`
//! extraction (`DECISIONS.md` D3b). **No behaviour changed.**
//!
//! ⚠ The D3D12 relevance is not hypothetical: `D3D12DDIARG_CREATEDEVICE_0109`
//! carries `pKTCallbacks`, the same 65-entry `D3DDDI_DEVICECALLBACKS` table the
//! D3D11 UMD drives, so a D3D12 forwarder that calls `pfnRenderCb` handles the
//! same pointer/size pairs (`DDI_REFERENCE.md` §8.2, `DECISIONS.md` P-C).

/// One runtime-owned buffer window: a pointer and the capacity that describes
/// it, which are only ever meaningful together.
///
/// Pre-R808 these were six independent `Cell`s (`command_buffer` +
/// `command_buffer_size`, `allocation_list` + `allocation_list_size`,
/// `patch_list` + `patch_list_size`), so a pointer could be updated without its
/// size. Pairing them makes that unrepresentable.
pub struct Window<T> {
    pub ptr: core::ptr::NonNull<T>,
    pub capacity: u32,
}

// Hand-written rather than derived: `derive` would add a `T: Copy` bound, and
// the pointee types here (c_void, the DDI list structs) are not Copy. A window
// is a pointer and an integer; copying it never copies a `T`.
impl<T> Clone for Window<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Window<T> {}

impl<T> Window<T> {
    /// `None` for a null pointer, which is how the runtime spells "no window".
    /// A zero capacity is retained rather than rejected: `pfnRenderCb` returns
    /// pointer/size pairs and only the pointer decides presence, exactly as the
    /// pre-R808 `is_null()` checks did.
    pub fn new(ptr: *mut T, capacity: u32) -> Option<Self> {
        Some(Self {
            ptr: core::ptr::NonNull::new(ptr)?,
            capacity,
        })
    }
}
