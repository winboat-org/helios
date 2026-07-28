//! The five embedded kernel dispatcher objects and their lifecycles: the venus
//! and scanout mutexes, the HPD worker's wake/exit events and thread, and the
//! VSync heartbeat timer/DPC pair.
//!
//! Moved verbatim out of `adapter.rs` by T8/R1101. Co-locating `init_vsync`,
//! `cancel_vsync` and (from R1102) `vsync_dpc_routine` is the point: the
//! cancel/flush/free-before-RemoveDevice argument can be stated once instead of
//! being implied in two files.

use wdk_sys::ntddk::{KeInitializeEvent, KeSetEvent, KeWaitForSingleObject};
use wdk_sys::PVOID;

use crate::dxgk::*;

use super::AdapterContext;

extern "C" {
    /// `extern POBJECT_TYPE *PsThreadType;` (ntddk.h) — the thread object type, for
    /// `ObReferenceObjectByHandle` validation when joining the HPD worker. A data
    /// export (ntoskrnl.lib), not in the wdk-sys function bindings, so declared here
    /// (same pattern as `ExEventObjectType` in `ddi/escape.rs`).
    static PsThreadType: *mut wdk_sys::POBJECT_TYPE;
}

impl AdapterContext {
    /// Idempotent-ish: does nothing if a thread handle is already stored.
    ///
    /// # Safety
    /// `self` must be at its final heap address and `dxgkrnl` already saved.
    pub unsafe fn init_hpd(&self) {
        use core::sync::atomic::Ordering;
        if self.hpd_thread.load(Ordering::Acquire) != 0 {
            return;
        }
        self.hpd_stop.store(0, Ordering::Release);
        self.scanout_refresh_pending.store(0, Ordering::Release);
        self.scanout_flush_inflight.store(0, Ordering::Release);
        // ⚠ `host_bound_scanout_resource` is zeroed here WITHOUT clearing
        // `active_scanout_resource`, so after a StopDevice/StartDevice cycle
        // the two disagree and `queue_active_scanout_refresh_locked` reaches
        // the `host_bound != resource_id` test below. That is the ONE path
        // that made the deleted async-bind arm reachable, and it is why the
        // refusal survives the deletion as `RfUnb` rather than falling through
        // to a RESOURCE_FLUSH against a resource the host never bound.
        self.host_bound_scanout_resource.store(0, Ordering::Release);
        let mut handle: wdk_sys::HANDLE = core::ptr::null_mut();
        const THREAD_ALL_ACCESS: u32 = 0x001F_FFFF;
        // SAFETY: PASSIVE_LEVEL; a kernel system thread in the system process
        // running `hpd_thread_routine` with this stable context as its argument.
        let st = unsafe {
            wdk_sys::ntddk::PsCreateSystemThread(
                &mut handle,
                THREAD_ALL_ACCESS,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                Some(crate::ddi::hpd::hpd_thread_routine),
                self as *const _ as PVOID,
            )
        };
        if st == STATUS_SUCCESS && !handle.is_null() {
            self.hpd_thread.store(handle as usize, Ordering::Release);
        } else {
            // 0x0B00_00EA = HPD-worker-create-failed. It was 0x0B00_00E7, which
            // start_device.rs also records for venus-bring-up-failed — and BOTH
            // happen inside the StartDevice window, so the ring could not
            // disambiguate them. `StHpd` vs `StVnu` already distinguish the two
            // as named counters; this makes the ring agree.
            crate::diag::record(0x0B00_00EA);
            crate::diag::fault(crate::diag::FaultCounter::StHpd, st as u32);
        }
    }

    /// Wake the HPD worker to re-indicate connection (from the interrupt DPC at
    /// DISPATCH_LEVEL — KeSetEvent with Wait=FALSE is legal there).
    pub fn signal_hpd(&self) {
        // SAFETY: hpd_event was initialized in place by init_kernel_events.
        unsafe { KeSetEvent(self.hpd_event.get(), 0, 0) };
    }

    /// Stop + join the HPD worker before teardown (StopDevice / Drop). Idempotent.
    /// PASSIVE_LEVEL — it blocks on the worker's exit.
    pub fn stop_hpd(&self) {
        use core::sync::atomic::Ordering;
        let h = self.hpd_thread.swap(0, Ordering::AcqRel);
        if h == 0 {
            return;
        }
        self.hpd_stop.store(1, Ordering::Release);
        // SAFETY: initialized event; wake the worker so it observes hpd_stop.
        unsafe { KeSetEvent(self.hpd_event.get(), 0, 0) };
        // Join: reference the thread object from its handle, wait for it to exit,
        // deref, then close the handle. Without the join, RemoveDevice could free
        // this context while the worker still runs → UAF.
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const KERNEL_MODE: i8 = 0;
        let mut obj: PVOID = core::ptr::null_mut();
        // SAFETY: `h` is a live thread handle from PsCreateSystemThread; PsThreadType
        // validates it. On success we hold a reference to the ETHREAD.
        let st = unsafe {
            wdk_sys::ntddk::ObReferenceObjectByHandle(
                h as wdk_sys::HANDLE,
                SYNCHRONIZE,
                *PsThreadType,
                KERNEL_MODE,
                &mut obj,
                core::ptr::null_mut(),
            )
        };
        // The join is the ONLY thing keeping RemoveDevice from freeing a context
        // the worker still dereferences. Two ways it used to fail silently:
        //
        // 1. If ObReferenceObjectByHandle failed, the wait was skipped entirely,
        //    the handle was closed, and stop_hpd returned () - success-shaped.
        //    StopDevice then dropped the transport and RemoveDevice freed the
        //    box while the worker was still touching adapter.hpd_event and the
        //    scanout fields. A use-after-free with no breadcrumb.
        // 2. The wait passed a NULL Timeout - the only unbounded wait in this
        //    file - while the worker can be parked in a synchronous host
        //    round-trip (set_scanout_blob) or under the venus mutex. A wedged
        //    host hung PnP stop forever, again with no counter.
        //
        // Both now mean "a worker may still be running", which is recorded and
        // latched so the free path can consult it.
        // 5 s, relative (negative = relative to now, in 100 ns units). Long
        // enough for the worst observed set_scanout_blob round-trip, short
        // enough that PnP stop does not hang indefinitely.
        const JOIN_TIMEOUT_100NS: i64 = -50_000_000;

        // Primary join: the worker's own "I exited" NotificationEvent, set at
        // both of its exit sites immediately before PsTerminateSystemThread.
        // This does NOT depend on the handle-to-object lookup below succeeding,
        // which is the failure that used to skip the wait entirely. A
        // NotificationEvent stays signalled, so the join cannot miss it.
        let mut timeout: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
        timeout.QuadPart = JOIN_TIMEOUT_100NS;
        // SAFETY: initialized in place by init_kernel_events; PASSIVE_LEVEL.
        let exited =
            unsafe { KeWaitForSingleObject(self.hpd_exited.get() as PVOID, 0, 0, 0, &mut timeout) };
        let mut joined = exited == STATUS_SUCCESS;

        if st == STATUS_SUCCESS && !obj.is_null() {
            // Secondary: the thread object itself. The exit event is set just
            // BEFORE PsTerminateSystemThread, so this closes the remaining
            // window between the two - the worker is not yet fully torn down
            // when it signals.
            let mut timeout: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
            timeout.QuadPart = JOIN_TIMEOUT_100NS;
            // SAFETY: waiting on the ETHREAD dispatcher object at PASSIVE_LEVEL.
            let wait = unsafe { KeWaitForSingleObject(obj, 0, 0, 0, &mut timeout) };
            // SAFETY: releasing the reference taken above.
            unsafe { wdk_sys::ntddk::ObfDereferenceObject(obj) };
            joined = wait == STATUS_SUCCESS;
        }
        if !joined {
            // Trading a hang (or a UAF) for a permanent allocation leak plus a
            // live worker. That is the correct trade for a kernel driver, but it
            // must be counted: on a healthy host StHpdX never moves.
            self.hpd_worker_leaked.store(1, Ordering::Release);
            crate::diag::fault(crate::diag::FaultCounter::StHpdX, st as u32);
        }
        // SAFETY: closing the thread handle we created.
        let _ = unsafe { wdk_sys::ntddk::ZwClose(h as wdk_sys::HANDLE) };
    }

    /// True if [`Self::stop_hpd`] could not prove the worker exited, so this
    /// context must never be freed. Consulted by `dxgkddi_remove_device`.
    pub fn hpd_worker_may_be_running(&self) -> bool {
        self.hpd_worker_leaked
            .load(core::sync::atomic::Ordering::Acquire)
            != 0
    }

    /// Arm the display-half VSync heartbeat: initialize the embedded KDPC/KTIMER
    /// in place and start a periodic ~16 ms `SynchronizationTimer` whose DPC
    /// (`vsync_dpc_routine`) synthesizes `DXGK_INTERRUPT_CRTC_VSYNC`. Idempotent
    /// (no-op if already armed). PASSIVE_LEVEL only (StartDevice).
    ///
    /// # Safety
    /// `self` must be at its final heap address (dxgkrnl holds it as the miniport
    /// device context) and `dxgkrnl` must already be saved (StartDevice ordering).
    pub unsafe fn init_vsync(&self) {
        use wdk_sys::ntddk::{KeInitializeDpc, KeInitializeTimerEx, KeSetTimerEx};
        if self
            .vsync_armed
            .swap(1, core::sync::atomic::Ordering::AcqRel)
            != 0
        {
            return;
        }
        self.vsync_enabled
            .store(1, core::sync::atomic::Ordering::Release);
        // SAFETY: the KDPC/KTIMER live in this stable boxed context; the DPC
        // context is the adapter pointer, valid for the device lifetime.
        unsafe {
            KeInitializeDpc(
                self.vsync_dpc.get(),
                Some(crate::ddi::vsync_dpc_routine),
                self as *const _ as PVOID,
            );
            KeInitializeTimerEx(
                self.vsync_timer.get(),
                wdk_sys::_TIMER_TYPE::SynchronizationTimer,
            );
            // Relative due time -16 ms (100 ns units); Period 16 ms (recurring).
            let mut due: wdk_sys::LARGE_INTEGER = core::mem::zeroed();
            due.QuadPart = -160_000;
            KeSetTimerEx(self.vsync_timer.get(), due, 16, self.vsync_dpc.get());
        }
    }

    /// Cancel the VSync heartbeat timer (StopDevice / teardown). Idempotent.
    /// PASSIVE_LEVEL. After the flush returns, no VSync DPC is running or queued —
    /// so it is safe for a subsequent RemoveDevice to free this context.
    pub fn cancel_vsync(&self) {
        use core::sync::atomic::Ordering;
        if self.vsync_armed.swap(0, Ordering::AcqRel) == 0 {
            return;
        }
        self.vsync_enabled.store(0, Ordering::Release);
        // SAFETY: the timer was initialized by `init_vsync`; KeCancelTimer is
        // callable at <= DISPATCH_LEVEL and safe on an idle timer. KeFlushQueuedDpcs
        // (PASSIVE_LEVEL only — StopDevice is PASSIVE) then drains any DPC the timer
        // already queued on another CPU before we return, closing the free-after-DPC
        // window against RemoveDevice.
        unsafe {
            wdk_sys::ntddk::KeCancelTimer(self.vsync_timer.get());
            wdk_sys::ntddk::KeFlushQueuedDpcs();
        }
    }

    /// Initialize the embedded kernel dispatcher objects. MUST be called once,
    /// after the context reaches its final (heap) address and before any DDI
    /// can run — `dxgkddi_add_device` calls it right after boxing.
    ///
    /// # Safety
    /// `self` must be at its final address and not yet visible to any other
    /// thread.
    pub unsafe fn init_kernel_events(&self) {
        // SAFETY: per the fn contract; SynchronizationEvent (type 1), initially
        // signaled (the mutex starts free).
        unsafe { KeInitializeEvent(self.venus_mutex.get(), 1, 1) };
        // Same synchronization-event mutex shape as `venus_mutex`, but with a
        // distinct lock order and purpose: scanout lifecycle operations never
        // hold this while acquiring it recursively.
        unsafe { KeInitializeEvent(self.scanout_mutex.get(), 1, 1) };
        // HPD worker wake event: SynchronizationEvent (auto-clears on a satisfied
        // wait), initially unsignaled — the worker's own timeout drives the first
        // indication; later signals come from the config-change DPC.
        // SAFETY: per the fn contract; stable in-place KEVENT storage.
        unsafe { KeInitializeEvent(self.hpd_event.get(), 1, 0) };
        // Worker-exited latch: NotificationEvent (type 0) so it STAYS signalled
        // once set, initially unsignaled. A synchronization event would be
        // consumed by the first waiter and a second stop_hpd would block.
        // SAFETY: per the fn contract; stable in-place KEVENT storage.
        unsafe { KeInitializeEvent(self.hpd_exited.get(), 0, 0) };
    }
}
