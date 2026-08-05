//! The D3D11 half of the typed-handle-slot encoding.
//!
//! ⚠ **The encoding itself now lives in `helios_umd_common::slot`** — `Slot<P>`,
//! `Com<T>`, `Boxed<S>`, the three traits and the two `impl` macros moved there
//! at stage S1 (`DECISIONS.md` D3b: *"if a D3D12 file would contain code that
//! also exists in `umd/`, that code moves to `umd_common` first"*). Read that
//! module for the design, the R803 hazard it makes uncallable, and the
//! `Slot<Boxed<S>>::get()` soundness argument.
//!
//! What stays here is exactly what is D3D11-specific and cannot move:
//!
//! - **which** handle types carry a bare COM pointer and which carry a `Box`,
//!   because both lists name `crate::ddi` types generated from `d3d10umddi.h`;
//! - the payload structs those boxed handles name (`ResourceState`, `RtvState`,
//!   `LayoutData`), which are private to `forward`;
//! - [`boxed_slot`], whose signature mentions them.
//!
//! # Runtime-tagged slots
//!
//! Three DDIs (`Discard`, `ClearView`, the tiled-resource barrier) receive a
//! bare `pDrvPrivate` plus a `D3D11DDI_HANDLETYPE` that selects the payload at
//! *run* time, so no static handle type is available to key on. Those call the
//! `*_at` forms in `forward`, which take the raw pointer and name the payload
//! explicitly at the call site after the tag has been matched.

use helios_umd_common::{boxed_handles, com_handles};

// Re-exported at their original paths so no call site in `forward/*` moved when
// the encoding did. ⛔ `DdiHandle`/`ComHandle`/`BoxedHandle` are re-exported as
// well because the macro-generated impls above are only useful to callers that
// can name the traits; importing them from two places would be the start of the
// duplication D3b forbids.
pub(crate) use helios_umd_common::slot::{Boxed, Com, ComHandle, DdiHandle, Slot};
pub(super) use helios_umd_common::slot::BoxedHandle;

com_handles!(
    crate::ddi::D3D10DDI_HSHADER,
    crate::ddi::D3D10DDI_HBLENDSTATE,
    crate::ddi::D3D10DDI_HDEPTHSTENCILSTATE,
    crate::ddi::D3D10DDI_HRASTERIZERSTATE,
    crate::ddi::D3D10DDI_HSAMPLER,
    crate::ddi::D3D10DDI_HQUERY,
    crate::ddi::D3D10DDI_HSHADERRESOURCEVIEW,
    crate::ddi::D3D10DDI_HDEPTHSTENCILVIEW,
    crate::ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    // IC-side region owns the `ID3D11CommandList` COM word (Phase C). A
    // DC-local command-list region is a borrowed COPY of the same word,
    // closed by clearing — the DC table's close shim, never `release`.
    crate::ddi::D3D11DDI_HCOMMANDLIST,
);

boxed_handles!(
    crate::ddi::D3D10DDI_HRESOURCE => super::ResourceState,
    crate::ddi::D3D10DDI_HRENDERTARGETVIEW => super::RtvState,
    crate::ddi::D3D10DDI_HELEMENTLAYOUT => super::LayoutData,
);

/// The slot behind a boxed-payload DDI handle, typed with the payload that
/// handle names. `None` when the runtime handed us a null slot.
///
/// This is the boxed counterpart of taking `impl ComHandle`: the caller passes
/// the handle itself rather than its `pDrvPrivate`, so the payload type is
/// derived rather than chosen, and the `*mut c_void` stays out of `forward`.
///
/// # Safety
/// Same precondition as `Slot::from_priv`: `h`'s slot, when non-null, must
/// lie inside the private memory the paired `CalcPrivate*Size` sized.
pub(super) unsafe fn boxed_slot<H: BoxedHandle>(h: H) -> Option<Slot<Boxed<H::State>>> {
    unsafe { Slot::from_priv(h.drv_private()) }
}
