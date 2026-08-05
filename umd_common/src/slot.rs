//! One payload type per DDI handle slot.
//!
//! Moved from `umd/src/forward/handles.rs:74-333` (`DECISIONS.md` D3b). The
//! encoding is D3-version-agnostic: it is about *a machine word inside private
//! memory the driver sized*, which is a fact of the WDDM UMD DDI shape and not
//! of D3D11. `umd` keeps the parts that are D3D11-specific — the
//! `com_handles!`/`boxed_handles!` **invocations**, which name `ddi` types, and
//! `boxed_slot`, whose payload structs are private to `forward`.
//!
//! # The problem this solves
//!
//! Every UMD DDI handle is a struct wrapping one `pDrvPrivate` pointer into
//! runtime-allocated memory that the driver sized (via the matching
//! `CalcPrivate*Size`) and owns the contents of. Different things get stored in
//! that one machine word — for D3D11: `*mut ResourceState`, `*mut RtvState`, or
//! a bare owning COM pointer.
//!
//! Before this module the discriminator was *which free function the caller
//! happened to call*. `handle_com_raw` read any slot as a `usize`: correct for
//! the bare-COM slots it is used on, silently the box pointer if applied to a
//! resource or RTV slot. `load_com::<ID3D11RenderTargetView>(h_rtv.pDrvPrivate)`
//! was type-correct, compiled, and yielded a `ManuallyDrop` whose vtable
//! pointer was the value of `RtvState::com_raw` — a wild call on first use.
//! (`ARCHITECTURE.md` §12 rule 7; R803.)
//!
//! # The encoding
//!
//! [`Slot<P>`] is a non-null pointer to the slot word, tagged with the payload
//! type `P` that the driver stores there. `P` is one of:
//!
//! - [`Com<T>`] — the word is an owning `T` COM pointer.
//! - [`Boxed<S>`] — the word is a `Box<S>` the driver allocated.
//!
//! The payload decides which operations exist: `Slot<Com<T>>` has no way to
//! reach a `Box`, and `Slot<Boxed<S>>` has no `load`/`release` that would
//! reinterpret the box pointer as a vtable. The non-null check happens once, at
//! construction, so the operations do not each repeat it.
//!
//! Which payload a given handle carries is then a property of the handle type,
//! not of the call site: [`ComHandle`] marks the bare-COM handles and
//! [`BoxedHandle`] names each boxed handle's one payload struct.
//!
//! # What stays unencodable
//!
//! That the runtime passed a slot which the matching `CalcPrivate*Size` sized,
//! and that the slot's previous contents were written by *this* driver, remain
//! preconditions of construction that no type can witness. `Slot` narrows the
//! trusted boundary to one module; it does not remove it.
//!
//! # Windows-only
//!
//! [`Com`] and the `Slot<Com<T>>` operations need `windows::core::Interface`,
//! so this module is `#[cfg(windows)]` and its dependency sits under
//! `[target.'cfg(windows)'.dependencies]`. `umd_common` must keep building on
//! Linux for `tools/format-table-check.rs` (crate docs).

use core::ffi::c_void;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use windows::core::{IUnknown, Interface};

/// Any DDI handle struct: a wrapper around one `pDrvPrivate` slot pointer.
///
/// Implemented for every handle type a driver stores into, regardless of
/// payload, because *clearing* a slot is payload-agnostic — it nulls the word
/// and touches nothing it pointed at. Every `Create*`/`Open*` DDI does exactly
/// that on entry so a failed create leaves a null handle rather than stale
/// garbage.
pub trait DdiHandle: Copy {
    fn drv_private(self) -> *mut c_void;
}

/// A DDI handle whose slot holds a **bare owning COM pointer**, not a `Box`.
///
/// This is the marker that makes R803's named invalid sequence a compile
/// error. In `umd`, `D3D10DDI_HRESOURCE`, `D3D10DDI_HRENDERTARGETVIEW` and
/// `D3D10DDI_HELEMENTLAYOUT` deliberately do NOT implement it, so
/// `load_com::<ID3D11RenderTargetView>(h_rtv)` — which used to compile and
/// yield a `ManuallyDrop` whose vtable pointer was `RtvState::com_raw` — no
/// longer names a callable function.
pub trait ComHandle: DdiHandle {}

/// A DDI handle whose slot holds a **`Box<Self::State>`** the driver
/// allocated.
///
/// The associated type is the boxed-side counterpart of [`ComHandle`]. Where
/// `ComHandle` says only "this word is a COM pointer", `BoxedHandle` names the
/// one payload struct that belongs in this handle's slot, so the payload is
/// selected by the handle's *type* rather than by which free function the
/// caller happened to reach for.
///
/// ⚠ The associated type may be a type private to the implementing crate. That
/// is deliberate and is why this trait is `pub` here while `umd`'s payload
/// structs stay private to `forward`: a trait impl does not export its
/// associated type.
pub trait BoxedHandle: DdiHandle {
    type State;
}

/// Implement [`DdiHandle`] + [`ComHandle`] for handle types whose slot holds a
/// bare COM pointer.
///
/// ⚠ **Takes full paths, one per handle, on purpose.** The obvious shorter
/// form — bare `crate::ddi::$ty` inside the macro body, relying on
/// `macro_rules!` resolving unqualified paths at the *expansion* site — works
/// today only because both cdylibs happen to call their bindgen module `ddi`.
/// It would silently stop resolving the moment one of them does not, and the
/// error would point into this file rather than at the call site. Writing the
/// path out costs one prefix per line and cannot be wrong.
///
/// `$crate` is used for the trait paths, so the caller need not import them.
///
/// ```ignore
/// helios_umd_common::com_handles!(
///     crate::ddi::D3D10DDI_HSHADER,
///     crate::ddi::D3D10DDI_HBLENDSTATE,
/// );
/// ```
#[macro_export]
macro_rules! com_handles {
    ($($ty:ty),* $(,)?) => {$(
        impl $crate::slot::DdiHandle for $ty {
            fn drv_private(self) -> *mut ::core::ffi::c_void {
                self.pDrvPrivate
            }
        }
        impl $crate::slot::ComHandle for $ty {}
    )*};
}

/// Implement [`DdiHandle`] + [`BoxedHandle`] for handle types whose slot holds
/// a `Box`, pairing each with the payload struct it stores. Clearing is still
/// legal for these (it is payload-agnostic); decoding as COM is not.
///
/// Full paths, same reason as [`com_handles!`].
///
/// ```ignore
/// helios_umd_common::boxed_handles!(
///     crate::ddi::D3D10DDI_HRESOURCE => super::ResourceState,
/// );
/// ```
#[macro_export]
macro_rules! boxed_handles {
    ($($ty:ty => $state:ty),* $(,)?) => {$(
        impl $crate::slot::DdiHandle for $ty {
            fn drv_private(self) -> *mut ::core::ffi::c_void {
                self.pDrvPrivate
            }
        }
        impl $crate::slot::BoxedHandle for $ty {
            type State = $state;
        }
    )*};
}

/// Payload marker: the slot word is an owning COM pointer to `T`.
pub struct Com<T: Interface>(PhantomData<fn() -> T>);

/// Payload marker: the slot word is a `Box<S>` the driver allocated.
pub struct Boxed<S>(PhantomData<fn() -> S>);

/// A DDI handle's private slot, tagged with what the driver stores in it.
///
/// Construction establishes non-nullness of the *slot*; the value inside it may
/// still be null (an unwritten or already-released handle), which every read
/// below reports as `None`.
pub struct Slot<P> {
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
    pub unsafe fn from_priv(handle_priv: *mut c_void) -> Option<Self> {
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
    ///
    /// # Safety
    /// Same precondition as [`Self::from_priv`].
    pub unsafe fn word(self) -> usize {
        unsafe { self.cell.as_ptr().read() as usize }
    }

    unsafe fn write(self, value: *mut c_void) {
        unsafe { self.cell.as_ptr().write(value) }
    }

    /// Null the slot without touching whatever it pointed at. Callers that own
    /// the payload must release it first.
    ///
    /// # Safety
    /// Same precondition as [`Self::from_priv`].
    pub unsafe fn clear(self) {
        unsafe { self.write(core::ptr::null_mut()) }
    }
}

impl<T: Interface> Slot<Com<T>> {
    /// Move `obj`'s reference into the slot.
    ///
    /// # Safety
    /// Same precondition as [`Slot::from_priv`].
    pub unsafe fn store(self, obj: T) {
        unsafe { self.write(obj.into_raw()) }
    }

    /// Store an already-raw COM pointer whose reference is being transferred.
    ///
    /// # Safety
    /// Same precondition as [`Slot::from_priv`], and `raw` must be a COM
    /// pointer to a `T` whose reference the caller is giving up.
    pub unsafe fn store_raw(self, raw: usize) {
        unsafe { self.write(raw as *mut c_void) }
    }

    /// Borrow the stored interface. `ManuallyDrop` because the slot keeps the
    /// reference; dropping the returned value would release the slot's.
    ///
    /// # Safety
    /// Same precondition as [`Slot::from_priv`].
    pub unsafe fn load(self) -> Option<ManuallyDrop<T>> {
        let raw = unsafe { self.cell.as_ptr().read() };
        if raw.is_null() {
            return None;
        }
        Some(ManuallyDrop::new(unsafe { T::from_raw(raw) }))
    }

    /// Move the stored COM reference out and empty the slot.
    ///
    /// This is the ownership-preserving counterpart to [`Self::load`]. BUILD_2
    /// command-list recycling uses it to transfer the IC-side escrow reference
    /// into its originating deferred-context handoff; callers that decline the
    /// object simply drop the returned interface, exactly like `release`.
    ///
    /// # Safety
    /// Same precondition as [`Slot::from_priv`].
    pub unsafe fn take(self) -> Option<T> {
        let raw = unsafe { self.cell.as_ptr().read() };
        if raw.is_null() {
            return None;
        }
        unsafe { self.clear() };
        Some(unsafe { T::from_raw(raw) })
    }

    /// Release the stored reference and empty the slot. Idempotent.
    ///
    /// # Safety
    /// Same precondition as [`Slot::from_priv`].
    pub unsafe fn release(self) {
        let raw = unsafe { self.cell.as_ptr().read() };
        if !raw.is_null() {
            drop(unsafe { IUnknown::from_raw(raw) });
            unsafe { self.clear() };
        }
    }
}

impl<S> Slot<Boxed<S>> {
    /// Box `state` and move ownership into the slot.
    ///
    /// # Safety
    /// Same precondition as [`Slot::from_priv`].
    pub unsafe fn store(self, state: S) {
        let boxed = Box::new(state);
        unsafe { self.write(Box::into_raw(boxed).cast::<c_void>()) }
    }

    /// Borrow the boxed state. `None` for an empty slot.
    ///
    /// # Safety
    ///
    /// There is no interior locking, so the caller must supply the argument
    /// that no borrow overlaps the box's teardown.
    ///
    /// ⛔ **The argument is per-runtime and does NOT come with the code.**
    /// D3b: *"the `Slot<Boxed<S>>::get() -> &'static S` soundness argument rests
    /// on the D3D11 runtime's `CUseCountedObject` ordering; it must be
    /// re-derived for D3D12, not assumed."* The move to `umd_common` moves the
    /// code, not the claim.
    ///
    /// - **D3D11 — established.** Under THREADING caps = 0 the runtime
    ///   serializes every DDI call (the original R811 contract); under
    ///   FREETHREADED (Phase B) the runtime's use-counting guarantees an
    ///   object's Destroy DDI — the only [`Self::take`] path — never runs
    ///   concurrently with a DDI still using the handle (`CUseCountedObject`,
    ///   first-created/last-destroyed ordering). Either way no borrow can
    ///   overlap teardown; what FREETHREADED does allow is concurrent *reads*,
    ///   which `&S` permits by construction.
    /// - **D3D12 — NOT established.** `d3d12umddi` has no THREADING cap and no
    ///   `CUseCountedObject` statement has been located for it. Whoever first
    ///   calls this from `umd12` owes the equivalent derivation, in a comment at
    ///   that call site, or must reach for [`Self::ptr`] and carry the lifetime
    ///   themselves.
    pub unsafe fn get(self) -> Option<&'static S> {
        let p = unsafe { self.cell.as_ptr().read() }.cast::<S>();
        if p.is_null() {
            None
        } else {
            Some(unsafe { &*p })
        }
    }

    /// The boxed state as a raw pointer, null when the slot is empty.
    ///
    /// For callers that legitimately need raw pointers rather than a borrow:
    /// `umd`'s `dxgi_rotate_resource_identities` holds several states at once
    /// and moves fields between them, which `&`/`&mut` cannot express without
    /// aliasing. Keeping it here means the `*mut c_void` -> `*mut S` cast still
    /// exists only in this module.
    ///
    /// # Safety
    /// Same precondition as [`Slot::from_priv`].
    pub unsafe fn ptr(self) -> *mut S {
        unsafe { self.cell.as_ptr().read() }.cast::<S>()
    }

    /// Take the box out and empty the slot, transferring ownership to the
    /// caller. `None` for an already-empty slot, so a double release is a
    /// no-op rather than a double free.
    ///
    /// # Safety
    /// Same precondition as [`Slot::from_priv`].
    pub unsafe fn take(self) -> Option<Box<S>> {
        let p = unsafe { self.cell.as_ptr().read() }.cast::<S>();
        if p.is_null() {
            return None;
        }
        unsafe { self.clear() };
        Some(unsafe { Box::from_raw(p) })
    }
}
