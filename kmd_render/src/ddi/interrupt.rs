//! ISR / DPC DDIs — the C3/M3.4 interrupt-driven used-ring drain.
//!
//! The virtio-gpu device is line-based INTx (`MSISupported=0`), i.e. *level*-
//! triggered: it asserts the shared INTx line when it pushes used-ring entries
//! and keeps it asserted until the driver reads the read-to-clear virtio
//! ISR-status register. The ISR reads that register (deasserting the line),
//! claims the interrupt, and queues the DPC via `DxgkCbQueueDpc`; the DPC
//! drains the used ring under the device spinlock (`VirtioGpu::drain_used` —
//! signaling sync/fence KEVENT waiters) and then completes every WDDM
//! submission whose venus watermark has been reached
//! (`DXGK_INTERRUPT_DMA_COMPLETED` at DIRQL via `signal_dma_completed`).
//!
//! IRQL: the ISR runs at the device's DIRQL — no allocations, no spinlocks, no
//! pageable calls; it touches only the lock-free published ISR-status VA and
//! the saved dxgkrnl callback table. The DPC runs at DISPATCH_LEVEL.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::adapter::AdapterContext;
use crate::dxgk::*;

// ── DIRQL/DISPATCH-safe instrumentation ──────────────────────────────────────
// `diag::record` is PASSIVE-only (RtlWriteRegistryValue), so the ISR/DPC cannot
// touch the registry ring. These atomics are incremented here at DIRQL/DISPATCH
// and dumped into the PASSIVE diag ring at DxgkDdiDestroyDevice.
pub static INT_ROUTINE_COUNT: AtomicU32 = AtomicU32::new(0);
pub static DPC_ROUTINE_COUNT: AtomicU32 = AtomicU32::new(0);
pub static CONTROL_INT_COUNT: AtomicU32 = AtomicU32::new(0);

/// `DxgkDdiInterruptRoutine` — runs at the device's DIRQL; returns TRUE if the
/// interrupt was ours.
//
// Read the ISR-status register (which DEASSERTS the level-triggered line), and
// if a bit was pending, claim the interrupt and queue the DPC. Without the
// read-to-clear, the line stays asserted and Windows' interrupt-storm detector
// disables the adapter (observed pre-fix: ~10000 unclaimed ISR calls → Code 43).
// The register VA is published lock-free by StartDevice (`isr_status`, Release)
// BEFORE the transport goes live, and `adapter.dxgkrnl` is written before that
// — so a nonzero `isr_status` implies a valid callback table.
pub unsafe extern "C" fn dxgkddi_interrupt_routine(
    miniport_device_context: *mut c_void,
    _message_number: u32,
) -> BOOLEAN {
    if miniport_device_context.is_null() {
        return 0;
    }
    // SAFETY: dxgkrnl passes our AdapterContext as the miniport device context;
    // it is valid for the device's lifetime and `isr_status` is an atomic.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
    let isr_va = adapter.isr_status.load(Ordering::Acquire);
    if isr_va == 0 {
        // Transport not up yet (or torn down): not in a position to claim it.
        return 0;
    }
    // SAFETY: `isr_va` is the mapped MMIO VA of the 1-byte read-to-clear
    // ISR-status register, published by StartDevice; the read clears + deasserts.
    let status = unsafe { core::ptr::read_volatile(isr_va as *const u8) };
    if status == 0 {
        // Shared line, but no virtio interrupt pending — not ours.
        return 0;
    }
    INT_ROUTINE_COUNT.fetch_add(1, Ordering::Relaxed);
    // Bit 1 = configuration change: the virtio-gpu raises it on a
    // VIRTIO_GPU_EVENT_DISPLAY (monitor connect / mode change). Latch it for the
    // DPC, which wakes the HPD worker to (re-)indicate the child connected — the
    // viogpu3d ISR_REASON_CHANGE path (`viogpu_adapter.cpp:1531`).
    if status & 0x2 != 0 {
        adapter.config_change_pending.store(1, Ordering::Release);
    }
    // Bit 0 = used-ring progress (drain), bit 1 = config change: either needs the
    // DPC. (The ISR-status read above already deasserted the line for both.)
    if status & 0x3 != 0 {
        if let Some(dxgkrnl) = adapter.dxgkrnl.as_ref() {
            if let Some(queue_dpc) = dxgkrnl.DxgkCbQueueDpc {
                // SAFETY: DxgkCbQueueDpc is callable from the ISR at DIRQL;
                // DeviceHandle is the live dxgkrnl device handle.
                unsafe { queue_dpc(dxgkrnl.DeviceHandle) };
            }
        }
    }
    1 // claimed + acknowledged (line now deasserted)
}

/// `DxgkDdiDpcRoutine` — runs at DISPATCH_LEVEL after the ISR (or a
/// `signal_dma_completed` notify pair) queues a DPC.
pub unsafe extern "C" fn dxgkddi_dpc_routine(miniport_device_context: *mut c_void) {
    DPC_ROUTINE_COUNT.fetch_add(1, Ordering::Relaxed);
    if miniport_device_context.is_null() {
        return;
    }
    // SAFETY: our AdapterContext, valid for the device's lifetime.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };

    // A latched config-change (ISR bit 1): wake the HPD worker to re-indicate the
    // child connected. KeSetEvent (Wait=FALSE) is legal at DISPATCH_LEVEL.
    if adapter.config_change_pending.swap(0, Ordering::AcqRel) != 0 {
        adapter.signal_hpd();
    }

    // Let dxgkrnl process any interrupt data queued by DxgkCbNotifyInterrupt
    // (the WDDM fence completions signaled below re-queue this DPC, and this
    // call drains their packets — the viogpu3d NotifyDpcRoutine ordering).
    if let Some(dxgkrnl) = adapter.dxgkrnl.as_ref() {
        if let Some(notify_dpc) = dxgkrnl.DxgkCbNotifyDpc {
            // SAFETY: DISPATCH_LEVEL DPC context; live device handle.
            unsafe { notify_dpc(dxgkrnl.DeviceHandle) };
        }
    }

    // Drain the used ring: completes in-flight entries, copies sync responses,
    // signals sync/fence KEVENT waiters, parks retired buffers for a PASSIVE
    // reap. KeSetEvent at DISPATCH (Wait=FALSE) is legal.
    let _ = adapter.with_virtio(|v| v.drain_used());

    // Complete every WDDM submission whose venus watermark has been reached —
    // strictly FIFO (SubmissionFenceIds are watermarks to dxgkrnl). The
    // DIRQL notify (DxgkCbSynchronizeExecution → DxgkCbNotifyInterrupt →
    // DxgkCbQueueDpc) must run OUTSIDE the device spinlock.
    loop {
        let mut ready = [0u32; 8];
        let n = adapter
            .with_virtio(|v| v.take_ready_wddm(&mut ready))
            .unwrap_or(0);
        if n == 0 {
            break;
        }
        for &fence in &ready[..n] {
            adapter.last_completed_fence.store(fence, Ordering::Release);
            if let Some(dxgkrnl) = adapter.dxgkrnl.as_ref() {
                // SAFETY: live callback interface; signal at the correct IRQL
                // (DxgkCbSynchronizeExecution raises to DIRQL internally).
                let _ = unsafe { super::submit_command::signal_dma_completed(dxgkrnl, fence) };
            }
        }
    }
}

/// `DxgkDdiControlInterrupt` — enable/disable a class of GPU interrupts. Called at
/// up to DIRQL, so this path touches only atomics (no registry / pageable calls).
//
// The OS drives CRTC_VSYNC here. The display half (DisplayHalf on) services it: it
// toggles the free-running VSync heartbeat's delivery gate and returns SUCCESS.
// A render-only adapter (0 video-present sources) drives no VSYNC → NOT_IMPLEMENTED
// (MSDN requires that for any type the driver does not service); the virtio
// used-ring interrupt is not an OS-controlled class.
pub unsafe extern "C" fn dxgkddi_control_interrupt(
    h_adapter: IN_CONST_HANDLE,
    interrupt_type: IN_CONST_DXGK_INTERRUPT_TYPE,
    enable: IN_BOOLEAN,
) -> NTSTATUS {
    CONTROL_INT_COUNT.fetch_add(1, Ordering::Relaxed);
    let p = h_adapter as *const AdapterContext;
    if !p.is_null()
        && interrupt_type == _DXGK_INTERRUPT_TYPE::DXGK_INTERRUPT_CRTC_VSYNC
        // SAFETY: dxgkrnl hands our AdapterContext; `display_half` is a plain bool
        // set once at StartDevice.
        && unsafe { (*p).display_half }
    {
        // SAFETY: valid for the device lifetime.
        let adapter = unsafe { &*p };
        adapter
            .vsync_enabled
            .store((enable != 0) as u32, Ordering::Release);
        return STATUS_SUCCESS;
    }
    STATUS_NOT_IMPLEMENTED
}
