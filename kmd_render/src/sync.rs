//! Kernel synchronization primitives with RAII release obligations, plus a
//! vector that cannot allocate while protected by a spinlock.
//!
//! Three tables in this driver were the same hand-written construct: a raw
//! `KSPIN_LOCK` in an `UnsafeCell`, a `Vec` pre-reserved at construction, an
//! `unsafe impl Send/Sync` justified only by the sentence "every access to
//! `entries` is serialized by `lock`", and hand-paired
//! `KeAcquireSpinLockRaiseToDpc`/`KeReleaseSpinLock` around a manually created
//! `&mut`. Each copy also re-derived the "push stays inside reserved capacity so
//! it cannot allocate under the lock" rule in a comment — with three different
//! spellings, one of which (`entries.len() < entries.capacity()`) silently used
//! whatever over-allocation `Vec` chose rather than the declared maximum.
//!
//! # What the guard buys
//!
//! Release becomes an obligation the compiler discharges on EVERY exit path. No
//! such path exists today — every exit inside the three critical sections is a
//! `continue`, a `break`, or a fall-through — but adding an early `return false;`
//! inside `update_leaf`'s PTE loop is a natural edit, and exactly what a typed
//! paging dispatch invites. The spinlock would be left held and the next paging
//! op would deadlock the whole graphics stack at DISPATCH_LEVEL, with no counter
//! and no crash dump.
//!
//! `&mut` access is reachable only through the guard, so the `unsafe impl Sync`
//! justification becomes structurally true instead of prose.
//!
//! # What it does NOT buy
//!
//! The guard cannot enforce "do not allocate or block while holding it". That
//! stays a documented rule — but [`FixedVec`] makes the allocation half
//! impossible in practice, since it has no growth path at all.
//!
//! `panic = abort` means there is no unwind path to worry about in `Drop`.

use alloc::alloc::{alloc, dealloc};
use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use core::sync::atomic::{fence, AtomicUsize, Ordering};

use wdk_sys::ntddk::{
    KeAcquireSpinLockRaiseToDpc, KeInitializeMutex, KeReleaseMutex, KeReleaseSpinLock,
    KeWaitForSingleObject,
};
use wdk_sys::{KIRQL, KMUTANT, KSPIN_LOCK, PVOID};

use crate::irql::PassiveLevel;

/// A stable, fallibly-created atomic shared owner.
///
/// Stable `Arc::new` routes allocation failure through the kernel panic handler,
/// while `Arc::try_new` still requires Rust's unstable allocator API. Paging
/// callbacks must return `STATUS_NO_MEMORY`, not bugcheck, so this small owner
/// exposes a fallible constructor and a non-allocating, overflow-checked clone.
pub(crate) struct FallibleArc<T> {
    inner: NonNull<FallibleArcInner<T>>,
}

struct FallibleArcInner<T> {
    refs: AtomicUsize,
    value: T,
}

unsafe impl<T: Send + Sync> Send for FallibleArc<T> {}
unsafe impl<T: Send + Sync> Sync for FallibleArc<T> {}

impl<T> FallibleArc<T> {
    pub(crate) fn try_new(value: T) -> Result<Self, T> {
        let layout = Layout::new::<FallibleArcInner<T>>();
        // SAFETY: `layout` is nonzero because the header contains AtomicUsize.
        let raw = unsafe { alloc(layout) }.cast::<FallibleArcInner<T>>();
        let Some(inner) = NonNull::new(raw) else {
            return Err(value);
        };
        // SAFETY: `inner` points to a suitably aligned allocation of exactly
        // this layout and has not been initialized yet.
        unsafe {
            inner.as_ptr().write(FallibleArcInner {
                refs: AtomicUsize::new(1),
                value,
            });
        }
        Ok(Self { inner })
    }

    /// Clone without allocating; refuse the theoretical refcount-overflow
    /// case rather than making `Clone` hide a panic path.
    pub(crate) fn try_clone(&self) -> Option<Self> {
        let refs = unsafe { &self.inner.as_ref().refs };
        let mut current = refs.load(Ordering::Relaxed);
        loop {
            if current >= isize::MAX as usize {
                return None;
            }
            match refs.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Self { inner: self.inner }),
                Err(observed) => current = observed,
            }
        }
    }
}

impl<T> Deref for FallibleArc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: every owner contributes one positive ref and final release
        // destroys the allocation only after that count reaches zero.
        unsafe { &self.inner.as_ref().value }
    }
}

impl<T> Drop for FallibleArc<T> {
    fn drop(&mut self) {
        let refs = unsafe { &self.inner.as_ref().refs };
        if refs.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        fence(Ordering::Acquire);
        // SAFETY: this was the final owner. Drop the initialized header/value,
        // then release the allocation with the identical layout.
        unsafe {
            core::ptr::drop_in_place(self.inner.as_ptr());
            dealloc(
                self.inner.as_ptr().cast::<u8>(),
                Layout::new::<FallibleArcInner<T>>(),
            );
        }
    }
}

/// A kernel mutex for operations which may block and therefore must remain at
/// PASSIVE_LEVEL.
///
/// `KMUTANT` contains an intrusive dispatcher wait list, so it must not move
/// after `KeInitializeMutex`. Keeping it behind a fallibly allocated shared
/// owner makes that address stable even when the Rust owner moves, without an
/// infallible kernel allocation on the adapter-creation path.
pub(crate) struct PassiveMutex {
    raw: FallibleArc<UnsafeCell<KMUTANT>>,
}

unsafe impl Send for PassiveMutex {}
unsafe impl Sync for PassiveMutex {}

impl PassiveMutex {
    pub(crate) fn try_new(_passive: PassiveLevel) -> Option<Self> {
        // SAFETY: KMUTANT is an opaque kernel dispatcher object.  It is placed
        // at its final heap address before the kernel initializes every field.
        let raw = FallibleArc::try_new(UnsafeCell::new(unsafe { core::mem::zeroed() })).ok()?;
        unsafe { KeInitializeMutex(raw.get(), 0) };
        Some(Self { raw })
    }

    /// Wait indefinitely and non-alertably for exclusive ownership.
    pub(crate) fn lock(&self, _passive: PassiveLevel) -> Option<PassiveMutexGuard<'_>> {
        // SAFETY: `raw` remains at the address initialized by KeInitializeMutex;
        // Executive=0, KernelMode=0, Alertable=FALSE and NULL timeout form a
        // legal indefinite PASSIVE_LEVEL wait.
        let status = unsafe {
            KeWaitForSingleObject(
                self.raw.get().cast::<core::ffi::c_void>() as PVOID,
                0,
                0,
                0,
                core::ptr::null_mut(),
            )
        };
        (status >= 0).then_some(PassiveMutexGuard {
            owner: self,
            _not_send: PhantomData,
        })
    }
}

/// Releases a [`PassiveMutex`] on every return path.
pub(crate) struct PassiveMutexGuard<'a> {
    owner: &'a PassiveMutex,
    /// KMUTEX ownership belongs to the acquiring thread.
    _not_send: PhantomData<*const ()>,
}

impl Drop for PassiveMutexGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: this guard exists only after a successful wait by this
        // thread, and it cannot outlive its mutex.
        unsafe { KeReleaseMutex(self.owner.raw.get(), 0) };
    }
}

/// A value protected by a `KSPIN_LOCK`.
pub(crate) struct SpinLock<T> {
    lock: UnsafeCell<KSPIN_LOCK>,
    value: UnsafeCell<T>,
}

// SAFETY: the ONLY route to `value` is `lock()`, which serializes access behind
// the `KSPIN_LOCK`. Unlike the three hand-written copies this replaces, that is
// now a property of the API rather than a claim in a comment.
unsafe impl<T: Send> Send for SpinLock<T> {}
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self {
            lock: UnsafeCell::new(0),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquire, raising to DISPATCH_LEVEL.
    ///
    /// Callable from PASSIVE or DISPATCH: the guard saves the `KIRQL` the
    /// acquire RETURNED and restores exactly that, so it does not assume the
    /// caller was at DISPATCH. Getting that wrong is the specific hazard in this
    /// conversion.
    pub(crate) fn lock(&self) -> SpinLockGuard<'_, T> {
        // SAFETY: `lock` is a live KSPIN_LOCK owned by this object; the WDK
        // contract for KeAcquireSpinLockRaiseToDpc is IRQL <= DISPATCH_LEVEL.
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.lock.get()) };
        SpinLockGuard {
            owner: self,
            irql,
            _not_send: PhantomData,
        }
    }
}

/// Exclusive access to a [`SpinLock`]'s value. Releases on drop.
///
/// `!Send` via `PhantomData<*const ()>`: a spinlock must be released on the
/// thread and at the IRQL that took it, so the guard must not cross threads.
pub(crate) struct SpinLockGuard<'a, T> {
    owner: &'a SpinLock<T>,
    /// The IRQL to restore — what the acquire returned, not an assumption.
    irql: KIRQL,
    _not_send: PhantomData<*const ()>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: we hold the lock for the guard's lifetime.
        unsafe { &*self.owner.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we hold the lock, and `&mut self` proves this is the only
        // live borrow through the guard.
        unsafe { &mut *self.owner.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: paired with the acquire in `SpinLock::lock`, restoring the
        // IRQL that acquire returned.
        unsafe { KeReleaseSpinLock(self.owner.lock.get(), self.irql) };
    }
}

/// A vector with a fixed capacity and NO growth path.
///
/// The three tables each documented "push stays inside reserved capacity so it
/// cannot allocate under the lock" and each checked it differently. Here the
/// rule is one function with one spelling, and the type has no `reserve`, no
/// `insert` and no `Deref<Target = Vec<T>>` — so "allocate under a spinlock" is
/// not expressible, rather than merely discouraged.
///
/// Backed by a `Vec` reserved once at construction (PASSIVE_LEVEL) rather than
/// an inline array, because two of the three tables are large enough that an
/// inline array would be a multi-megabyte struct: `MAX_PAGING_SYSTEM_PTES` is
/// 65,536 entries and `MAX_MAPPINGS` is 8,192.
pub(crate) struct FixedVec<T> {
    entries: alloc::vec::Vec<T>,
    /// The DECLARED maximum. Not `Vec::capacity()`, which reports whatever
    /// over-allocation the allocator chose — `adapter.rs` checked against that
    /// and so silently admitted more entries than its own constant allowed.
    max: usize,
}

impl<T> FixedVec<T> {
    /// Reserve once. PASSIVE_LEVEL — this is the only allocation the type ever
    /// performs.
    pub(crate) fn with_max(max: usize) -> Self {
        let mut entries = alloc::vec::Vec::new();
        // Best-effort: a failed reservation leaves `max` unchanged, and `push`
        // then refuses at the smaller real capacity rather than allocating.
        let _ = entries.try_reserve_exact(max);
        Self { entries, max }
    }

    /// Append if there is room. Returns `false` when full — never allocates.
    pub(crate) fn push(&mut self, value: T) -> bool {
        if self.entries.len() >= self.max || self.entries.len() >= self.entries.capacity() {
            return false;
        }
        self.entries.push(value);
        true
    }

    /// Append without dropping `value` on failure. This matters when `T::drop`
    /// may call PASSIVE-only kernel APIs and the caller currently holds a
    /// spinlock: the rejected value can then be released after the guard.
    pub(crate) fn try_push(&mut self, value: T) -> Result<(), T> {
        if self.entries.len() >= self.max || self.entries.len() >= self.entries.capacity() {
            return Err(value);
        }
        self.entries.push(value);
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_full(&self) -> bool {
        self.entries.len() >= self.max || self.entries.len() >= self.entries.capacity()
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        &self.entries
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.entries
    }

    /// Retain in place. Cannot allocate.
    pub(crate) fn retain<F: FnMut(&T) -> bool>(&mut self, f: F) {
        self.entries.retain(f);
    }

    /// Replace the entry at `index`, returning the old value.
    pub(crate) fn replace_at(&mut self, index: usize, value: T) -> T {
        core::mem::replace(&mut self.entries[index], value)
    }

    /// Remove by swapping the last entry into `index`. O(1), order-destroying —
    /// use only on tables with no order invariant.
    pub(crate) fn swap_remove(&mut self, index: usize) -> T {
        self.entries.swap_remove(index)
    }
}
