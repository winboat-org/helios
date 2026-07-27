//! One payload type per DDI handle slot.
//!
//! # The problem this solves
//!
//! Every D3D11 UMD DDI handle is a struct wrapping one `pDrvPrivate` pointer
//! into runtime-allocated memory that this driver sized (via the matching
//! `CalcPrivate*Size`) and owns the contents of. Three different things get
//! stored in that one machine word:
//!
//! | slot kind                     | word holds              |
//! |-------------------------------|-------------------------|
//! | `D3D10DDI_HRESOURCE`          | `*mut ResourceState`    |
//! | `D3D10DDI_HRENDERTARGETVIEW`  | `*mut RtvState`         |
//! | shader / DSV / blend / …      | a bare owning COM ptr   |
//!
//! Before this module the discriminator was *which free function the caller
//! happened to call*. `handle_com_raw` read any slot as a `usize`: correct for
//! the bare-COM slots it is used on, silently the box pointer if applied to a
//! resource or RTV slot. `load_com::<ID3D11RenderTargetView>(h_rtv.pDrvPrivate)`
//! was type-correct, compiled, and yielded a `ManuallyDrop` whose vtable
//! pointer was the value of `RtvState::com_raw` — a wild call on first use.
//!
//! No current call site does that: every `load_com` site targets a bare-COM
//! slot and all three runtime-tag dispatches use `load_rtv` for the RTV arm.
//! This is a latent hazard plus a real duplication, not a live crash. See R803.
//!
//! # The encoding
//!
//! [`Slot<P>`] is a non-null pointer to the slot word, tagged with the payload
//! type `P` that this driver stores there. `P` is one of:
//!
//! - [`Com<T>`] — the word is an owning `T` COM pointer.
//! - [`Boxed<S>`] — the word is a `Box<S>` this driver allocated.
//!
//! The payload decides which operations exist: `Slot<Com<T>>` has no way to
//! reach a `Box`, and `Slot<Boxed<S>>` has no `load`/`release` that would
//! reinterpret the box pointer as a vtable. The non-null check happens once, at
//! construction, so the operations do not each repeat it.
//!
//! The `*mut c_void` → payload cast exists in this module and nowhere else,
//! which is the precondition T8's split of `forward.rs` depends on.
//!
//! # What stays unencodable
//!
//! That the runtime passed a slot which the matching `CalcPrivate*Size` sized,
//! and that the slot's previous contents were written by *this* driver, remain
//! preconditions of construction that no type can witness. `Slot` narrows the
//! trusted boundary to one module; it does not remove it.

use core::ffi::c_void;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use windows::core::{IUnknown, Interface};

/// Any DDI handle struct: a wrapper around one `pDrvPrivate` slot pointer.
///
/// Implemented for every handle type this driver stores into, regardless of
/// payload, because *clearing* a slot is payload-agnostic — it nulls the word
/// and touches nothing it pointed at. Every `Create*`/`Open*` DDI does exactly
/// that on entry so a failed create leaves a null handle rather than stale
/// garbage.
pub(crate) trait DdiHandle: Copy {
    fn drv_private(self) -> *mut c_void;
}

/// A DDI handle whose slot holds a **bare owning COM pointer**, not a `Box`.
///
/// This is the marker that makes R803's named invalid sequence a compile
/// error. `D3D10DDI_HRESOURCE`, `D3D10DDI_HRENDERTARGETVIEW` and
/// `D3D10DDI_HELEMENTLAYOUT` deliberately do NOT implement it, so
/// `load_com::<ID3D11RenderTargetView>(h_rtv)` — which used to compile and
/// yield a `ManuallyDrop` whose vtable pointer was `RtvState::com_raw` — no
/// longer names a callable function.
pub(crate) trait ComHandle: DdiHandle {}

/// Implement both traits for handle types whose slot holds a bare COM pointer.
macro_rules! com_handles {
    ($($ty:ident),* $(,)?) => {$(
        impl DdiHandle for crate::ddi::$ty {
            fn drv_private(self) -> *mut c_void {
                self.pDrvPrivate
            }
        }
        impl ComHandle for crate::ddi::$ty {}
    )*};
}

/// Implement only `DdiHandle` for handle types whose slot holds a `Box`.
/// Clearing is still legal; decoding as COM is not.
macro_rules! boxed_handles {
    ($($ty:ident),* $(,)?) => {$(
        impl DdiHandle for crate::ddi::$ty {
            fn drv_private(self) -> *mut c_void {
                self.pDrvPrivate
            }
        }
    )*};
}

com_handles!(
    D3D10DDI_HSHADER,
    D3D10DDI_HBLENDSTATE,
    D3D10DDI_HDEPTHSTENCILSTATE,
    D3D10DDI_HRASTERIZERSTATE,
    D3D10DDI_HSAMPLER,
    D3D10DDI_HQUERY,
    D3D10DDI_HSHADERRESOURCEVIEW,
    D3D10DDI_HDEPTHSTENCILVIEW,
    D3D11DDI_HUNORDEREDACCESSVIEW,
);

boxed_handles!(
    D3D10DDI_HRESOURCE,
    D3D10DDI_HRENDERTARGETVIEW,
    D3D10DDI_HELEMENTLAYOUT,
);

/// Payload marker: the slot word is an owning COM pointer to `T`.
pub(crate) struct Com<T: Interface>(PhantomData<fn() -> T>);

/// Payload marker: the slot word is a `Box<S>` this driver allocated.
pub(crate) struct Boxed<S>(PhantomData<fn() -> S>);

/// A DDI handle's private slot, tagged with what this driver stores in it.
///
/// Construction establishes non-nullness of the *slot*; the value inside it may
/// still be null (an unwritten or already-released handle), which every read
/// below reports as `None`.
pub(crate) struct Slot<P> {
    cell: NonNull<*mut c_void>,
    _payload: PhantomData<fn() -> P>,
}

// Deriving would demand `P: Copy`, which the payload markers deliberately are
// not (they are uninhabited-by-construction tags, never values).
impl<P> Clone for Slot<P> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<P> Copy for Slot<P> {}

impl<P> Slot<P> {
    /// Wrap a DDI handle's `pDrvPrivate`. `None` when the runtime handed us a
    /// null slot, which is the one check every accessor used to repeat.
    ///
    /// # Safety
    /// `handle_priv`, when non-null, must point at a writable machine word this
    /// driver owns — i.e. inside the private memory the paired
    /// `CalcPrivate*Size` sized for a handle of this kind.
    pub(crate) unsafe fn from_priv(handle_priv: *mut c_void) -> Option<Self> {
        NonNull::new(handle_priv.cast::<*mut c_void>()).map(|cell| Self {
            cell,
            _payload: PhantomData,
        })
    }

    /// The slot word as an integer, 0 when the slot is empty.
    ///
    /// Note this is deliberately available for every payload: reading the word
    /// is always defined. What the old free `handle_com_raw` got wrong was not
    /// reading it, but that its *name* promised a COM pointer while it would
    /// equally return a `Box` pointer from a resource or RTV slot.
    pub(crate) unsafe fn word(self) -> usize {
        unsafe { self.cell.as_ptr().read() as usize }
    }

    unsafe fn write(self, value: *mut c_void) {
        unsafe { self.cell.as_ptr().write(value) }
    }

    /// Null the slot without touching whatever it pointed at. Callers that own
    /// the payload must release it first.
    pub(crate) unsafe fn clear(self) {
        unsafe { self.write(core::ptr::null_mut()) }
    }
}

impl<T: Interface> Slot<Com<T>> {
    /// Move `obj`'s reference into the slot.
    pub(crate) unsafe fn store(self, obj: T) {
        unsafe { self.write(obj.into_raw()) }
    }

    /// Store an already-raw COM pointer whose reference is being transferred.
    pub(crate) unsafe fn store_raw(self, raw: usize) {
        unsafe { self.write(raw as *mut c_void) }
    }

    /// Borrow the stored interface. `ManuallyDrop` because the slot keeps the
    /// reference; dropping the returned value would release the slot's.
    pub(crate) unsafe fn load(self) -> Option<ManuallyDrop<T>> {
        let raw = unsafe { self.cell.as_ptr().read() };
        if raw.is_null() {
            return None;
        }
        Some(ManuallyDrop::new(unsafe { T::from_raw(raw) }))
    }

    /// Release the stored reference and empty the slot. Idempotent.
    pub(crate) unsafe fn release(self) {
        let raw = unsafe { self.cell.as_ptr().read() };
        if !raw.is_null() {
            drop(unsafe { IUnknown::from_raw(raw) });
            unsafe { self.clear() };
        }
    }
}

impl<S> Slot<Boxed<S>> {
    /// Box `state` and move ownership into the slot.
    pub(crate) unsafe fn store(self, state: S) {
        let boxed = Box::new(state);
        unsafe { self.write(Box::into_raw(boxed).cast::<c_void>()) }
    }

    /// Borrow the boxed state. `None` for an empty slot.
    ///
    /// # Safety
    /// The single-threaded-DDI contract (THREADING caps = 0, see R811) is what
    /// makes handing out a `&S` here sound; there is no interior locking.
    pub(crate) unsafe fn get(self) -> Option<&'static S> {
        let p = unsafe { self.cell.as_ptr().read() }.cast::<S>();
        if p.is_null() {
            None
        } else {
            Some(unsafe { &*p })
        }
    }

    /// The boxed state as a raw pointer, null when the slot is empty.
    ///
    /// For the one caller that legitimately needs raw pointers rather than a
    /// borrow: `dxgi_rotate_resource_identities` holds several states at once
    /// and moves fields between them, which `&`/`&mut` cannot express without
    /// aliasing. Keeping it here means the `*mut c_void` -> `*mut S` cast still
    /// exists only in this module.
    pub(crate) unsafe fn ptr(self) -> *mut S {
        unsafe { self.cell.as_ptr().read() }.cast::<S>()
    }

    /// Take the box out and empty the slot, transferring ownership to the
    /// caller. `None` for an already-empty slot, so a double release is a
    /// no-op rather than a double free.
    pub(crate) unsafe fn take(self) -> Option<Box<S>> {
        let p = unsafe { self.cell.as_ptr().read() }.cast::<S>();
        if p.is_null() {
            return None;
        }
        unsafe { self.clear() };
        Some(unsafe { Box::from_raw(p) })
    }
}
