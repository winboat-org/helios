//! The four lock accessors and the proof tokens they mint.
//!
//! Lock order: `scanout_mutex -> venus_mutex -> virtio_lock`, with
//! `wddm_notify_lock` taken before the interrupt object. Moved verbatim out of
//! `adapter.rs` by T8/R1101.
//!
//! One LEAF spinlock sits below all of the above: `ReadLedger`'s
//! `scanout_event_lock` (D4a, `adapter/read_ledger.rs`). It is acquired LAST —
//! the retirement broadcast runs at DISPATCH under `virtio_lock` — and nothing
//! may be acquired while holding it; it guards only the 16-entry event table
//! and the ≤16 `KeSetEvent(Wait = FALSE)` calls of a broadcast. It has no
//! accessor here because it mints no token: `crate::sync::SpinLock`'s guard is
//! its whole discipline.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU32, Ordering};

use wdk_sys::ntddk::{
    KeAcquireSpinLockRaiseToDpc, KeReleaseSpinLock, KeSetEvent, KeWaitForSingleObject,
};
use wdk_sys::PVOID;

use crate::error::NotStarted;
use crate::irql::PassiveLevel;
use crate::virtio::VirtioGpu;

use super::AdapterContext;

/// `AdapterContext::with_virtio` entries whose `self` did not survive the
/// spinlock acquire (`WvTorn`). MUST READ 0.
///
/// The tripwire for ROADMAP defect 0ac — a 0xD1 bugcheck inside this accessor on
/// 22.22.212.0. A nonzero value here is a hard, attributable answer to a
/// question a stack-only post-mortem could not settle: the adapter pointer this
/// call was made with was not the one it woke up with. Mirrored from
/// `pacing_snapshot`, a PASSIVE site, because this one runs at DISPATCH.
pub(crate) static WITH_VIRTIO_TORN: AtomicU32 = AtomicU32::new(0);

/// Proof that this adapter's WDDM notification spinlock is currently held.
///
/// The fields are private and the type is neither `Copy` nor `Clone`, so safe
/// code can obtain a usable reference only inside [`AdapterContext::with_wddm_notify_lock`].
/// Fence-queue mutation and VidSch fence notifications accept this token rather
/// than relying on a caller-side comment or naming convention.
///
/// ⚠ LOCK ORDER, and the prohibition that goes with it: `wddm_notify_lock` is
/// taken BEFORE the interrupt object, because the closure may raise further to
/// the device DIRQL via `DxgkCbSynchronizeExecution` (`notify_at_dirql`) and
/// the interrupt DPC holds this lock across its whole retire loop. The converse
/// is therefore forbidden and is NOT enforced by anything: **the ISR must never
/// acquire `wddm_notify_lock`**. Adding
/// `adapter.with_wddm_notify_lock(..)` to `dxgkddi_interrupt_routine` while the
/// DPC holds it inside `DxgkCbSynchronizeExecution` is a hard DIRQL deadlock
/// with no attributable bugcheck. The `IsrPublish` projection that would make it
/// unreachable is a separate, larger change; this comment is not gated on it.
pub(crate) struct WddmNotifyGuard<'a> {
    adapter: &'a AdapterContext,
}

/// Proof that `wddm_notify_lock` was taken BEFORE `virtio_lock`, on the SAME
/// adapter, and is still held.
///
/// Mintable only inside [`WddmNotifyGuard::with_virtio`], which is the only way
/// to reach the six [`VirtioGpu`] methods whose contract depends on the notify
/// lock. That is what the plain `&WddmNotifyGuard` parameter used to claim and
/// did not deliver: a guard carries no relationship to the `&mut VirtioGpu`
/// being mutated, so `adapter_a.with_wddm_notify_lock(|g| adapter_b.with_virtio(
/// |v| v.note_wddm_submission(g, ..)))` compiled and mutated B's fence FIFO
/// under A's lock.
///
/// Honest gap: a caller that already holds `virtio_lock` and calls
/// `guard.with_virtio` recursively acquires it and self-deadlocks. Rust cannot
/// see that, so it stays a comment.
pub(crate) struct NotifyOrdered<'a> {
    _lock: PhantomData<&'a ()>,
}

impl WddmNotifyGuard<'_> {
    pub(crate) fn completed_fence(&self) -> u32 {
        self.adapter.last_completed_fence.load(Ordering::Acquire)
    }

    pub(crate) fn set_completed_fence(&self, fence: u32) {
        self.adapter
            .last_completed_fence
            .store(fence, Ordering::Release);
    }

    /// Run `f` against this adapter's transport with the notify lock already
    /// held, handing it the [`NotifyOrdered`] token the ordered transport
    /// methods require.
    ///
    /// The closure receives no `&AdapterContext`, so it cannot re-enter the
    /// notify lock; and because the same `&self` mints both borrows, the
    /// cross-adapter call above stops compiling.
    ///
    /// ⚠ Do NOT grow this into a `with_notify_then_virtio` that holds both locks
    /// for one closure: `drain_used_and_complete` deliberately makes several
    /// separate `with_virtio` calls inside one notify scope and runs
    /// `request_scanout_refresh()` and `signal_dma_completed()` (which
    /// raises to the device DIRQL via `DxgkCbSynchronizeExecution`) between
    /// them. Folding those into one transport critical section would change
    /// frame-path timing.
    pub(crate) fn with_virtio<R>(
        &self,
        f: impl FnOnce(&NotifyOrdered<'_>, &mut VirtioGpu) -> R,
    ) -> Result<R, NotStarted> {
        self.adapter.with_virtio(|v| {
            let order = NotifyOrdered { _lock: PhantomData };
            f(&order, v)
        })
    }
}

/// Proof that this adapter's PASSIVE scanout-lifecycle mutex is currently held.
///
/// The fields are private and the type is neither `Copy` nor `Clone`, so safe
/// code can obtain a reference only inside
/// [`AdapterContext::with_scanout_lifecycle`]. Every helper whose contract is
/// "call only under `scanout_mutex`" takes `&ScanoutGuard<'_>` instead of
/// carrying a `_locked` suffix and a doc comment.
///
/// The lock order this token sits at the head of is
/// `scanout_mutex -> venus_mutex -> virtio_lock` (below which only the D4a
/// `scanout_event_lock` LEAF may nest — see the module doc);
/// [`Self::with_venus_client`] is the enforced path for the middle step.
///
/// ⚠ This does NOT make recursion unrepresentable: the guard is handed to the
/// very closure that could call a re-acquiring wrapper. Callers must still not
/// invoke [`AdapterContext::with_scanout_lifecycle`],
/// [`AdapterContext::queue_active_scanout_refresh`] or
/// [`AdapterContext::retire_scanout_allocation`] from inside a guarded closure —
/// `scanout_mutex` is a non-recursive `SynchronizationEvent` and a re-entry is a
/// permanent PASSIVE self-deadlock of the HPD worker with no bugcheck and no
/// counter. `request_scanout_refresh` is deliberately token-free: it only sets a
/// bit and signals an event, so it is legal from inside the lock and from
/// DISPATCH.
pub(crate) struct ScanoutGuard<'a> {
    adapter: &'a AdapterContext,
    /// The caller's PASSIVE proof (R614), carried so the helpers that already
    /// take this guard — `production_linear_scanout`,
    /// `submit_primary_scanout_copy`, `program_vidpn_source` — reach the venus
    /// gateway and `virtio::ctrl` without a second parameter beside the one they
    /// already have. It is genuinely implied: `with_scanout_lifecycle` waits on a
    /// `SynchronizationEvent`, so a DISPATCH caller could never have got here.
    passive: PassiveLevel,
    /// Makes the guard `!Send`: the guarded work never crosses threads.
    _not_send: PhantomData<*const ()>,
}

impl ScanoutGuard<'_> {
    /// The scanout-lifecycle-ordered path to the Venus client.
    ///
    /// Forwards to [`AdapterContext::with_venus_client`]. Calling it through the
    /// guard is what makes `scanout_mutex`-before-`venus_mutex` a signature
    /// rather than a comment at the two sites that run under the scanout lock.
    /// `AdapterContext::with_venus_client` stays public because eleven of its
    /// thirteen callers legitimately hold no scanout lock.
    pub(crate) fn with_venus_client<R>(
        &self,
        f: impl FnOnce(&mut crate::virtio::venus::VenusClient) -> R,
    ) -> Result<R, NotStarted> {
        self.adapter.with_venus_client(self.passive, f)
    }

    /// The caller's PASSIVE proof, for the guarded helpers that reach
    /// `virtio::ctrl` directly instead of through the venus client.
    pub(crate) fn passive(&self) -> PassiveLevel {
        self.passive
    }
}

impl AdapterContext {
    /// Acquire the PASSIVE venus mutex (blocks; PASSIVE_LEVEL only).
    pub(super) fn acquire_venus_mutex(&self) {
        // SAFETY: the event was initialized in place by `init_kernel_events`;
        // an infinite Executive/KernelMode wait at PASSIVE_LEVEL. The
        // SynchronizationEvent auto-clears on a satisfied wait (mutex acquire).
        let _ = unsafe {
            KeWaitForSingleObject(
                self.venus_mutex.get() as PVOID,
                0, // Executive
                0, // KernelMode
                0, // non-alertable
                core::ptr::null_mut(),
            )
        };
    }

    /// Release the PASSIVE venus mutex.
    pub(super) fn release_venus_mutex(&self) {
        // SAFETY: initialized event; KeSetEvent with Wait=FALSE is callable at
        // <= DISPATCH_LEVEL (we are at PASSIVE).
        unsafe { KeSetEvent(self.venus_mutex.get(), 0, 0) };
    }

    /// Serialize a PASSIVE scanout operation against exact-resource retirement.
    ///
    /// The closure may block on virtio/Venus work, so this cannot use the
    /// DISPATCH-safe transport spinlock. Callers must not invoke it recursively —
    /// see [`ScanoutGuard`] for what the token does and does not prove.
    pub(crate) fn with_scanout_lifecycle<R>(
        &self,
        passive: PassiveLevel,
        f: impl FnOnce(&ScanoutGuard<'_>) -> R,
    ) -> R {
        // SAFETY: initialized in place by `init_kernel_events`; all callers are
        // PASSIVE-level display worker or allocation-lifecycle paths.
        let _ = unsafe {
            KeWaitForSingleObject(
                self.scanout_mutex.get() as PVOID,
                0, // Executive
                0, // KernelMode
                0, // non-alertable
                core::ptr::null_mut(),
            )
        };
        let guard = ScanoutGuard {
            adapter: self,
            passive,
            _not_send: PhantomData,
        };
        let result = f(&guard);
        // SAFETY: release the synchronization-event mutex acquired above.
        unsafe { KeSetEvent(self.scanout_mutex.get(), 0, 0) };
        result
    }

    /// Run `f` against the persistent venus client under the PASSIVE venus
    /// mutex. Returns `DeviceNotFound` if no client is installed. PASSIVE_LEVEL
    /// only — `f` may block on host round-trips (`virtio::ctrl`) and venus ring
    /// progress.
    ///
    /// ⚠ THE VENUS GATEWAY (R614). This is the only path to a `&mut VenusClient`
    /// after StartDevice: `venus_client` is a private `UnsafeCell` and the only
    /// other accessor, `set_venus_client`, moves the client by value. That is
    /// what makes the single `PassiveLevel` stored in `VenusRing` at bring-up
    /// sound for the ~89 `VenusClient` methods that take no token of their own:
    /// each of them ran because some caller proved PASSIVE *here*. The
    /// pre-installation client is reachable only from
    /// `venus::allocate_host_visible_blob`, which threads a real parameter.
    ///
    /// `_passive` is a PRECONDITION, not data — nothing in this body reads it.
    /// Its job is to make the gateway unreachable from code that has no proof.
    pub fn with_venus_client<R>(
        &self,
        _passive: PassiveLevel,
        f: impl FnOnce(&mut crate::virtio::venus::VenusClient) -> R,
    ) -> Result<R, NotStarted> {
        self.acquire_venus_mutex();
        // SAFETY: the venus mutex gives exclusive access to the cell.
        let result = match unsafe { &mut *self.venus_client.get() } {
            Some(client) => Ok(f(client)),
            None => Err(NotStarted),
        };
        self.release_venus_mutex();
        result
    }

    /// Serialize one scheduler notification at DISPATCH_LEVEL. The closure
    /// must not wait or allocate; it may raise further to the device DIRQL via
    /// `DxgkCbSynchronizeExecution`. The closure receives an unforgeable proof
    /// token required by every operation whose contract depends on this lock.
    pub(crate) fn with_wddm_notify_lock<R>(&self, f: impl FnOnce(&WddmNotifyGuard<'_>) -> R) -> R {
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.wddm_notify_lock.get()) };
        let guard = WddmNotifyGuard { adapter: self };
        let result = f(&guard);
        unsafe { KeReleaseSpinLock(self.wddm_notify_lock.get(), irql) };
        result
    }

    /// Run `f` against the live virtio transport while holding `virtio_lock`.
    ///
    /// Returns `DeviceNotFound` if the transport is not currently up. `f` runs at
    /// DISPATCH_LEVEL (spinlock held): it must not allocate or call pageable code.
    /// Stage any payload (e.g. a Venus stream) into a `DmaBuffer` *before* calling
    /// this, then pass a slice of it into `f`.
    pub fn with_virtio<R>(&self, f: impl FnOnce(&mut VirtioGpu) -> R) -> Result<R, NotStarted> {
        // ── The `WvTorn` tripwire (ROADMAP defect 0ac) ───────────────────────
        //
        // The 0xD1 bugcheck on 22.22.212.0 faulted in this function's prologue
        // region with an adapter pointer that did not survive the acquire. This
        // costs one stack slot and one compare on a path that already takes a
        // spinlock, and it turns "dereference whatever `self` now is" into the
        // ordinary transport-down error arm. Zero behaviour change when healthy;
        // `WvTorn` must read 0.
        //
        // `read_volatile` on both sides is what makes it a real check: without
        // it the compiler is free to rematerialize the comparison from one value
        // and fold it away, which would leave an inert tripwire that looks live.
        //
        // ⚠ THE LOCK ADDRESS IS HOISTED ONCE, HERE, from the `self` that has not
        // been questioned yet, and BOTH releases use that saved value. Deriving
        // it again after a detected mismatch would spell
        // `KeReleaseSpinLock(<corrupt self>.virtio_lock, ..)` — a wild WRITE at
        // DISPATCH_LEVEL, performed by the very arm whose job is to stop this
        // frame from touching that pointer. It is also simply the correct
        // release: the address that must be released is the one that was
        // acquired.
        let lock = self.virtio_lock.get();
        let slot = self as *const Self as usize;
        // SAFETY: `&slot` is a live, aligned, initialized stack slot owned by
        // this frame. The volatile read exists only to force the value to
        // memory, so the compare below cannot be folded away.
        let before = unsafe { core::ptr::read_volatile(&slot) };
        // SAFETY: `virtio_lock` is an embedded, in-place initialized KSPIN_LOCK
        // stable for the adapter's lifetime; the raise-to-DPC form is callable at
        // <= DISPATCH_LEVEL, which every caller of this accessor is.
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(lock) };
        if core::hint::black_box(self) as *const Self as usize != before {
            // SAFETY: `lock` is the exact address this frame acquired above, read
            // from `self` BEFORE the acquire, and `irql` is what that acquire
            // returned. Nothing here dereferences the failed `self` — this
            // release would otherwise be the first wild use of it.
            unsafe { KeReleaseSpinLock(lock, irql) };
            WITH_VIRTIO_TORN.fetch_add(1, Ordering::Relaxed);
            return Err(NotStarted);
        }
        // SAFETY: spinlock-guarded exclusive access to the cell's contents for the
        // duration of the critical section.
        let result = match unsafe { &mut *self.virtio.get() } {
            Some(v) => Ok(f(v)),
            None => Err(NotStarted),
        };
        // SAFETY: same address/IRQL pair the acquire above produced.
        unsafe { KeReleaseSpinLock(lock, irql) };
        result
    }
}
