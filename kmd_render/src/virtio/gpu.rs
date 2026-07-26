//! The virtio-gpu device object, built on the `virtio-drivers` PCI transport.
//!
//! `VirtioGpu` owns the `PciTransport` (discovers/maps the virtio config
//! regions) and the control `VirtQueue`, and layers the virtio-gpu command
//! protocol (`helios_protocol`) on top. Built by `init` from
//! `DxgkDdiStartDevice` and stored in `AdapterContext::virtio`.
//!
//! Bring-up (all in `init`, at PASSIVE_LEVEL):
//!   M1 — `DxgkConfigAccess` → `PciRoot` → `PciTransport::new::<WdkHal,_>`
//!   M2 — feature negotiation via the `Transport` trait
//!   M3 — control `VirtQueue::<WdkHal>` setup + DRIVER_OK
//!   M4 — `GET_DISPLAY_INFO` polled round-trip (Phase-2 smoke test)
//!
//! ## C3/M3.4 async transport (2026-07-04)
//!
//! Every control-queue command is a tracked [`InFlight`] entry that OWNS its
//! device-visible DMA buffers until the device returns its descriptor chain on
//! the used ring ([`VirtioGpu::drain_used`], token-matched `peek_used` →
//! `pop_used` — ported from the proven System-class phase4e model in
//! `kmd/src/virtio/gpu.rs`). Nothing in this module ever waits:
//!
//!   * Fenced `SUBMIT_3D` is ASYNC ([`VirtioGpu::enqueue_async_submit`]): the
//!     KMD assigns a globally-monotonic WIRE fence id, queues the descriptors,
//!     notifies, and returns. Completion signals any registered
//!     [`FenceWaiter`] (KEVENT) and advances the WDDM pending FIFO.
//!   * Synchronous verbs (ctx/blob/map) are enqueued with an optional
//!     [`SyncWaitBlock`] waiter ([`VirtioGpu::enqueue_sync`]); the waiter
//!     blocks at PASSIVE_LEVEL in `virtio::ctrl`, NEVER at DISPATCH under the
//!     device spinlock (the 2026-07-04 Escape-convoy root cause).
//!   * Completed entries are parked (their `DmaBuffer`s are PASSIVE-only to
//!     free) and reaped by PASSIVE callers via [`VirtioGpu::begin_parked_reap`].
//!
//! The used-ring consumer is [`VirtioGpu::drain_used`], called from the
//! interrupt DPC (`ddi/interrupt.rs`) and opportunistically (under the same
//! spinlock) by enqueue paths and by `virtio::ctrl`'s wait slices, so waits
//! survive a lost interrupt with only slice-granularity latency.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use bytemuck::Zeroable;
use helios_protocol::{
    resp_is_ok, VirtioGpuCmdSubmit, VirtioGpuCtrlHdr, VirtioGpuRespDisplayInfo,
    HELIOS_OPTIONAL_FEATURES, HELIOS_REQUIRED_FEATURES, VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
    VIRTIO_GPU_CMD_SUBMIT_3D, VIRTIO_GPU_FLAG_FENCE, VIRTIO_GPU_FLAG_INFO_RING_IDX,
    VIRTIO_GPU_SHM_ID_HOST_VISIBLE, VIRTIO_PCI_CAP_ISR_CFG, VIRTIO_PCI_CAP_SHARED_MEMORY_CFG,
};
use virtio_drivers::queue::VirtQueue;
use virtio_drivers::transport::pci::bus::{ConfigurationAccess, DeviceFunction, PciRoot};
use virtio_drivers::transport::pci::PciTransport;
use virtio_drivers::transport::{DeviceStatus, Transport};
use wdk_sys::ntddk::{KeInitializeEvent, KeSetEvent, ObDereferenceObjectDeferDelete};
use wdk_sys::{KEVENT, PVOID};

use super::config::DxgkConfigAccess;
use super::hal::{DmaBuffer, DmaSpan, WdkHal};
use super::VirtioError;
use crate::dxgk::DXGKRNL_INTERFACE;

/// Control queue index (virtio-gpu controlq = 0; cursorq = 1 is unused).
const CTRL_QUEUE: u16 = 0;
/// Control-queue ring size — power of two, conservatively ≤ the device's max.
const CTRL_QUEUE_SIZE: usize = 64;
/// One page of contiguous DMA scratch for `init`'s inline polled round-trip.
const SCRATCH_BYTES: usize = 4096;
/// Busy-poll bound for `init`'s inline GET_DISPLAY_INFO round-trip — the ONLY
/// polled wait left (PASSIVE, pre-interrupt, single-threaded bring-up; every
/// runtime wait is a PASSIVE KEVENT wait in `virtio::ctrl`). Each iteration is
/// a volatile used-ring read + `spin_loop` (~10 ns) → bound ≈ 1 s.
const CTRL_POLL_SPINS: u64 = 100_000_000;

/// Count of synchronous control-command timeouts (a passive waiter gave up and
/// abandoned its in-flight slot). Unlike the old model this does NOT poison
/// the transport — the slot is reaped when the completion eventually arrives —
/// but nonzero still means the host stopped answering in time. Read by
/// `DxgkDdiCollectDbgInfo` / `HELIOS_ESCAPE_QUERY_STATS` (acceptance: stays 0).
pub static CTRL_TIMEOUT_COUNT: AtomicU32 = AtomicU32::new(0);

// ── C3/M3.4 async-transport telemetry (all DISPATCH-safe atomics) ────────────

/// Async SUBMIT_3D enqueues.
pub static ASYNC_SUBMIT_COUNT: AtomicU32 = AtomicU32::new(0);
/// Async SUBMIT_3D completions drained from the used ring.
pub static ASYNC_COMPLETE_COUNT: AtomicU32 = AtomicU32::new(0);
/// Async SUBMIT_3D completions whose ctrl response was not RESP_OK.
pub static ASYNC_RESP_ERRORS: AtomicU32 = AtomicU32::new(0);
/// WAIT_FENCE waiters registered (fence was still in flight at wait time).
pub static FENCE_WAIT_REGISTERED: AtomicU32 = AtomicU32::new(0);
/// WAIT_FENCE waits that timed out — the host did not complete this fence.
///
/// This is the counter the project reads as fence evidence and dumps into the
/// TDR report, so it must mean exactly one thing. It used to also count
/// [`FENCE_WAIT_TABLE_FULL`]'s condition, where the host is perfectly healthy.
pub static FENCE_WAIT_TIMEOUTS: AtomicU32 = AtomicU32::new(0);
/// WAIT_FENCE waits that gave up because all [`MAX_FENCE_WAITERS`] slots were
/// occupied for the whole retry budget — a *guest* table-size condition, not a
/// host one, and the fix is a bigger table rather than a host investigation.
///
/// `gpu.rs`'s own comment on `MAX_FENCE_WAITERS` says dwm plus several apps plus
/// WUDFHost are expected concurrently, so the 65th waiter is a reachable state.
/// Before the split it incremented [`FENCE_WAIT_TIMEOUTS`], so post-mortem
/// evidence could not distinguish a wedged host from a table that needs
/// resizing — and the TDR report blamed the host.
pub static FENCE_WAIT_TABLE_FULL: AtomicU32 = AtomicU32::new(0);
/// A `ctrl_roundtrip` whose waiter was abandoned because the transport went away
/// mid-wait (StopDevice), rather than because the host timed out. Previously
/// folded into "already completed successfully" by `unwrap_or(true)`.
pub static CTRL_TEARDOWN_ABANDONS: AtomicU32 = AtomicU32::new(0);
/// A `wait_fence` that found the transport gone. Previously reported as
/// `Complete`, which made `escape_wait_fence` tell the ICD that a wire fence had
/// retired when it never did.
pub static TRANSPORT_GONE_AT_WAIT: AtomicU32 = AtomicU32::new(0);
/// Used-ring completions whose token matched no in-flight entry (ring state
/// corrupt → transport latches failed).
pub static DRAIN_BAD_TOKEN: AtomicU32 = AtomicU32::new(0);
/// Enqueue attempts refused because the queue/parked tables were full.
pub static QUEUE_FULL_RETRIES: AtomicU32 = AtomicU32::new(0);
/// WDDM pending-fence FIFO overflows (degraded to immediate completion).
pub static WDDM_PENDING_OVERFLOWS: AtomicU32 = AtomicU32::new(0);
/// High-water of concurrently in-flight control-queue entries.
pub static INFLIGHT_HIGH_WATER: AtomicU32 = AtomicU32::new(0);
/// High-water of parked (completed, awaiting PASSIVE free) entries.
pub static PARKED_HIGH_WATER: AtomicU32 = AtomicU32::new(0);
/// Parked entries force-forgotten because the parked table was full (leaked
/// DMA memory — must stay 0; the enqueue gate makes this unreachable).
pub static PARKED_LEAKS: AtomicU32 = AtomicU32::new(0);
/// WDDM submissions completed by the DPC (real venus-driven fences).
pub static WDDM_FENCE_FROM_DPC: AtomicU32 = AtomicU32::new(0);
/// Fenced SUBMIT_3D enqueues carrying ring_idx >= 1 (GPU-completion fences —
/// WS1 #4 consumer-side ordering; these retire at host GPU completion, not
/// decode, so they legally stay in flight for the full GPU-work duration).
pub static RING_SUBMIT_COUNT: AtomicU32 = AtomicU32::new(0);
/// ring_idx >= 1 completions drained from the used ring.
pub static RING_COMPLETE_COUNT: AtomicU32 = AtomicU32::new(0);
/// Fire-and-forget control commands queued by PASSIVE workers (currently the
/// scanout RESOURCE_FLUSH path).  These own their DMA buffers until the normal
/// used-ring drain retires them, but never park a stack waiter.
pub static ASYNC_CTRL_COUNT: AtomicU32 = AtomicU32::new(0);
/// Completed fire-and-forget control commands.
pub static ASYNC_CTRL_COMPLETE_COUNT: AtomicU32 = AtomicU32::new(0);
/// Fire-and-forget control responses that were not VIRTIO_GPU_RESP_OK_*.
pub static ASYNC_CTRL_RESP_ERRORS: AtomicU32 = AtomicU32::new(0);
/// Reused PASSIVE-allocated command buffers served from the bounded DMA pool.
pub static DMA_POOL_HITS: AtomicU32 = AtomicU32::new(0);
/// Command buffers that required a fresh PASSIVE allocation.
pub static DMA_POOL_MISSES: AtomicU32 = AtomicU32::new(0);
/// Completed buffers not cached because they exceeded a pool bound.
pub static DMA_POOL_DROPS: AtomicU32 = AtomicU32::new(0);
/// Current bytes retained in the DMA pool (page-rounded capacities).
pub static DMA_POOL_CACHED_BYTES: AtomicU32 = AtomicU32::new(0);

// ── Fence-event table telemetry (REGISTER_FENCE_EVENT, KMD 22.22.54) ────────
// The usermode-event replacement for blocking WAIT_FENCE escapes (PSC WS2:
// parked escapes convoy the process's SUBMIT_VENUS escapes at the dxgkrnl
// escape layer). All named per the loud-failure rule; read by QUERY_STATS v2.

/// REGISTERs parked in the table (event will be signaled by the drain).
pub static FENCE_EVENT_REGISTERS: AtomicU32 = AtomicU32::new(0);
/// Events signaled at wire-fence retirement (drain path, DISPATCH).
pub static FENCE_EVENT_SIGNALS: AtomicU32 = AtomicU32::new(0);
/// REGISTERs answered ALREADY_COMPLETE (fence had retired; signaled inline).
pub static FENCE_EVENT_ALREADY_COMPLETE: AtomicU32 = AtomicU32::new(0);
/// REGISTERs refused because the fence-event table was full.
pub static FENCE_EVENT_OVERFLOWS: AtomicU32 = AtomicU32::new(0);
/// REGISTERs refused as duplicates of a parked (fence_id, event) pair.
pub static FENCE_EVENT_DUP_REJECTS: AtomicU32 = AtomicU32::new(0);
/// REGISTER/UNREGISTER rejections: fence id never assigned / handle failed
/// ObReferenceObjectByHandle (bumped by the escape handler at PASSIVE).
pub static FENCE_EVENT_INVALID: AtomicU32 = AtomicU32::new(0);
/// UNREGISTERs that found and removed a parked registration.
pub static FENCE_EVENT_CANCELS: AtomicU32 = AtomicU32::new(0);
/// Registrations dropped UNSIGNALED at transport teardown (loud: a waiter's
/// deadline will expire and its unregister will report NOT_FOUND with an
/// unsignaled event — the ICD must treat that as failure, not completion).
pub static FENCE_EVENT_TEARDOWN_DROPS: AtomicU32 = AtomicU32::new(0);
/// WDDM fences signalled immediately because the transport had already latched
/// its ring-corruption failure. Each one also cleared the pending FIFO, so a
/// nonzero value means the driver chose a TDR-visible completion over an
/// undrainable queue.
pub static WDDM_SIGNAL_AFTER_FAILURE: AtomicU32 = AtomicU32::new(0);
/// Parked reaps abandoned between `begin_parked_reap` and `finish_parked_reap`.
/// Exists so a future strand shows up as itself rather than as a generic
/// `QUEUE_FULL_RETRIES` climb.
pub static REAP_ABANDONED: AtomicU32 = AtomicU32::new(0);
/// High-water of `fence_events.len()` since driver start.
pub static FENCE_EVENT_HIGH_WATER: AtomicU32 = AtomicU32::new(0);

// ── DISPATCH-safe resource-table telemetry ──────────────────────────────────
// All updated under the device spinlock (DISPATCH_LEVEL), so they must be
// atomics, never `diag::record` (RtlWriteRegistryValue is PASSIVE-only — the
// same latent-IRQL class the 2026-07-03 audit removed from the venus client).
// Read by `DxgkDdiCollectDbgInfo` and `HELIOS_ESCAPE_QUERY_STATS`.

/// High-water of `blobs.len()` since driver start.
pub static BLOB_HIGH_WATER: AtomicU32 = AtomicU32::new(0);
/// ALLOC_BLOB / note_blob_size attempts rejected because the blob table was
/// full. Nonzero = the 2026-07-03 exhaustion class is live again.
pub static BLOB_FULL_REJECTS: AtomicU32 = AtomicU32::new(0);
/// High-water of `resources.len()` since driver start.
pub static RESOURCE_HIGH_WATER: AtomicU32 = AtomicU32::new(0);
/// resource_create_blob attempts rejected because the live-resource table was full.
pub static RESOURCE_FULL_REJECTS: AtomicU32 = AtomicU32::new(0);
/// Context tracking slots dropped because the context table was full.
pub static CONTEXT_FULL_DROPS: AtomicU32 = AtomicU32::new(0);
/// Freed window ranges dropped because the free list was full (leaked offsets).
pub static WINDOW_RANGE_DROPS: AtomicU32 = AtomicU32::new(0);
/// `take_live_resource` misses (duplicate-teardown suppressions). Replaces the
/// in-lock `diag::record(0x0D20_00E0)` breadcrumb.
pub static TAKE_LIVE_MISSES: AtomicU32 = AtomicU32::new(0);
/// `adopt_blob_for_allocation` rejections of a dead resource id. Replaces the
/// in-lock `diag::record(0x0D20_00E2)` breadcrumb.
pub static ADOPT_DEAD_REJECTS: AtomicU32 = AtomicU32::new(0);
/// `alloc_window_range` refusals (host-visible window offset space exhausted /
/// fragmented past the request). Each one fails a user MAP_BLOB with
/// STATUS_INSUFFICIENT_RESOURCES — loud-failure rule (2026-07-06 Doom triage:
/// three uncounted refusal sites all mapped to the same 0xC000009A).
pub static WINDOW_ALLOC_REJECTS: AtomicU32 = AtomicU32::new(0);
/// `map_io_pages_to_user` failures (MDL alloc / MmMapLockedPagesSpecifyCache
/// raise caught by the SEH shim). Bumped by the escape handler at PASSIVE.
pub static MAP_PAGES_FAILS: AtomicU32 = AtomicU32::new(0);
/// Stale-overlap scans that found more overlapping window placements than the
/// caller's fixed buffer could hold. Nonzero means an eviction pass ran against
/// an incomplete list and the map that followed it was REFUSED rather than
/// allowed to create an overlapping host window subregion.
pub static WINDOW_OVERLAP_TRUNCATED: AtomicU32 = AtomicU32::new(0);

/// The stale-overlap scan could not report every overlapping placement.
///
/// A distinct type rather than a `usize` the caller may ignore: acting on a
/// truncated list is what creates two host resources through one window
/// subregion.
#[derive(Clone, Copy, Debug)]
pub struct WindowOverlapTruncated;

/// Raise `hw` to at least `n` (relaxed; approximate under concurrency is fine
/// for telemetry).
fn bump_high_water(hw: &AtomicU32, n: usize) {
    let n = n as u32;
    if hw.load(Ordering::Relaxed) < n {
        hw.store(n, Ordering::Relaxed);
    }
}

/// Table capacity accessors for `HELIOS_ESCAPE_QUERY_STATS` (the consts are
/// module-private).
pub fn max_blobs() -> usize {
    MAX_BLOBS
}
pub fn max_resources() -> usize {
    MAX_RESOURCES
}

// ── Host-visible window discovery (Gate 5a Stage 2) ─────────────────────────
// Ported from the proven System-class `kmd/src/virtio/gpu.rs`. The host-visible
// window is a prefetchable 64-bit PCI BAR (QEMU `hostmem=`) that
// `RESOURCE_MAP_BLOB` injects HOST3D blob mappings into. The WDDM Lock2 path
// reports this window as a CPU-visible memory segment so dxgkrnl/VidMm can map
// blobs to user space (there is no DxgkDdiLock; see GATE5_STAGE2_ALLOC_DESIGN.md).

const PCI_CFG_STATUS: u16 = 0x04; // command (low 16) | status (high 16)
const PCI_STATUS_CAP_LIST: u32 = 1 << 4; // status bit 4: capability list present
const PCI_CFG_CAP_PTR: u16 = 0x34; // first capability offset (low byte)
const PCI_CFG_BAR0: u16 = 0x10; // BAR0; BARn at 0x10 + n*4
const PCI_CAP_ID_VNDR: u32 = 0x09; // generic PCI vendor-specific capability id

/// The host-visible memory window discovered from the SHARED_MEMORY_CFG /
/// HOST_VISIBLE virtio capability.
#[derive(Clone, Copy)]
pub struct HostVisibleWindow {
    /// Guest-physical base of the window (BAR base + the cap's offset).
    pub base: u64,
    /// Window length in bytes (== QEMU `hostmem=`).
    pub len: u64,
}

/// Read 4 bytes of our device's PCI config space at `off` via the Dxgkrnl
/// config-space callback. `off` is held in a `u16` (like the System-class scan)
/// so the `cap + 20` cap-structure reads never overflow the `u8` arithmetic;
/// PCI config space is 256 bytes, so the `as u8` truncation is lossless.
fn cfg_read32(access: &DxgkConfigAccess, off: u16) -> u32 {
    access.read_word(
        DeviceFunction {
            bus: 0,
            device: 0,
            function: 0,
        },
        off as u8,
    )
}

/// Read the guest-physical base a memory BAR was assigned, handling the 64-bit
/// (type 0b10) layout the prefetchable host-visible window uses.
fn bar_base(access: &DxgkConfigAccess, bar: u16) -> Option<u64> {
    if bar > 5 {
        return None;
    }
    let reg = PCI_CFG_BAR0 + bar * 4;
    let lo = cfg_read32(access, reg);
    if lo & 0x1 != 0 {
        return None; // I/O-space BAR — not the memory window
    }
    let base = (lo & 0xFFFF_FFF0) as u64;
    // Memory BAR type in bits [2:1]: 0b10 == 64-bit (high half in BARn+1).
    if (lo >> 1) & 0x3 == 0x2 {
        Some(base | ((cfg_read32(access, reg + 4) as u64) << 32))
    } else {
        Some(base)
    }
}

/// Walk the PCI capability list for the virtio `SHARED_MEMORY_CFG` capability
/// whose shmid is `HOST_VISIBLE`, returning its guest-physical (base, length).
/// virtio-drivers' `PciTransport` ignores cap type 8, so we scan it ourselves.
/// Returns `None` if absent (a device built without blob/hostmem), which makes
/// the blob map path unavailable rather than crashing.
fn scan_host_visible_window(access: &DxgkConfigAccess) -> Option<HostVisibleWindow> {
    if (cfg_read32(access, PCI_CFG_STATUS) >> 16) & PCI_STATUS_CAP_LIST == 0 {
        return None;
    }
    // Capability pointers are dword-aligned; mask the reserved low 2 bits.
    let mut cap = (cfg_read32(access, PCI_CFG_CAP_PTR) & 0xFF) as u16 & 0xFC;
    // Bounded walk — a corrupt cap_next cannot escape the 256-byte config space.
    for _ in 0..48 {
        if cap == 0 {
            break;
        }
        let d0 = cfg_read32(access, cap);
        let cap_id = d0 & 0xFF;
        let cap_next = ((d0 >> 8) & 0xFF) as u16 & 0xFC;
        let cfg_type = (d0 >> 24) & 0xFF;
        if cap_id == PCI_CAP_ID_VNDR && cfg_type == VIRTIO_PCI_CAP_SHARED_MEMORY_CFG as u32 {
            // `virtio_pci_cap`: bar at +4 byte0, id (shmid) at +4 byte1.
            let d1 = cfg_read32(access, cap + 4);
            let bar = (d1 & 0xFF) as u16;
            let shmid = (d1 >> 8) & 0xFF;
            if shmid == VIRTIO_GPU_SHM_ID_HOST_VISIBLE as u32 {
                // `virtio_pci_cap64`: offset lo/hi at +8/+16, length lo/hi at +12/+20.
                let off = cfg_read32(access, cap + 8) as u64
                    | ((cfg_read32(access, cap + 16) as u64) << 32);
                let len = cfg_read32(access, cap + 12) as u64
                    | ((cfg_read32(access, cap + 20) as u64) << 32);
                let base = bar_base(access, bar)?;
                return Some(HostVisibleWindow {
                    base: base + off,
                    len,
                });
            }
        }
        cap = cap_next;
    }
    None
}

/// Walk the PCI capability list for the virtio `ISR_CFG` capability and map its
/// 1-byte ISR-status register, returning the mapped kernel VA (0 if absent).
///
/// This register is **read-to-clear**: reading it returns the pending-interrupt
/// bits (bit0 = used-ring/queue interrupt, bit1 = config change) and DEASSERTS
/// the device's level-triggered INTx line. `DxgkDdiInterruptRoutine` reads it at
/// DIRQL to acknowledge the line (the device is line-based INTx — `MSISupported=0`
/// — so without this read the line stays high and Windows' interrupt-storm
/// detector disables the adapter → Code 43). virtio-drivers' `PciTransport`
/// owns this register internally and never exposes its VA, and its `ack_interrupt`
/// needs `&mut self` (the queue lock) which the ISR cannot take at DIRQL — so we
/// locate and map the register ourselves and read it lock-free.
fn map_isr_status_register(access: &DxgkConfigAccess) -> usize {
    if (cfg_read32(access, PCI_CFG_STATUS) >> 16) & PCI_STATUS_CAP_LIST == 0 {
        return 0;
    }
    let mut cap = (cfg_read32(access, PCI_CFG_CAP_PTR) & 0xFF) as u16 & 0xFC;
    for _ in 0..48 {
        if cap == 0 {
            break;
        }
        let d0 = cfg_read32(access, cap);
        let cap_id = d0 & 0xFF;
        let cap_next = ((d0 >> 8) & 0xFF) as u16 & 0xFC;
        let cfg_type = (d0 >> 24) & 0xFF;
        if cap_id == PCI_CAP_ID_VNDR && cfg_type == VIRTIO_PCI_CAP_ISR_CFG as u32 {
            // `virtio_pci_cap`: bar at +4 byte0; offset (u32) at +8.
            let bar = (cfg_read32(access, cap + 4) & 0xFF) as u16;
            let offset = cfg_read32(access, cap + 8) as u64;
            let Some(base) = bar_base(access, bar) else {
                return 0;
            };
            let phys = base + offset;
            // SAFETY: maps a real device BAR sub-region (the ISR-status register)
            // at PASSIVE_LEVEL via the shared MMIO cache; non-cached MMIO.
            //
            // try_mmio_map, NOT the infallible Hal method: that one returns
            // NonNull::dangling() (address 0x1) on failure, which this function
            // would convert to a nonzero usize and return as a valid VA. `init`
            // would then record the SUCCESS breadcrumb and read_volatile(1) to
            // clear the register - a fault at PASSIVE inside StartDevice, where
            // the documented degrade path 0x0B00_00E6 already existed.
            let va =
                unsafe { crate::virtio::hal::try_mmio_map(phys as virtio_drivers::PhysAddr, 16) };
            return match va {
                Some(p) => p.as_ptr() as usize,
                None => {
                    // Distinct from "no ISR cap present" (0x0B00_00E6): the cap
                    // is there and we failed to map it. 0x0B00_00E0/E5/E6/E7/E8
                    // are all taken.
                    crate::diag::record(0x0B00_00E9);
                    0
                }
            };
        }
        cap = cap_next;
    }
    0
}

// ── Host-visible blob mapping (Gate 5a Stage 2b, venus-over-Escape) ──────────
// Ported (synchronous variant) from the proven System-class `kmd/src/virtio/gpu.rs`.
// The venus ICD allocates HOST3D blobs (ALLOC_BLOB) and maps them into its address
// space (MAP_BLOB) over `DxgkDdiEscape`; the KMD picks a window offset, issues
// `RESOURCE_MAP_BLOB`, and the Escape handler maps `host_visible.base + offset`
// into the calling process with `MmMapLockedPagesSpecifyCache` — the zero-copy BAR
// model (no WDDM memory segment / GpuMmu; see GATE5_STAGE2_ALLOC_DESIGN.md).

/// Page granularity for blob window offsets/sizes.
const BLOB_PAGE: u64 = 4096;
/// Max concurrently-tracked blobs.
///
/// SIZING (2026-07-03 exhaustion incident): a live desktop legitimately holds
/// hundreds of blobs at once — every venus ring/reply/fence shmem of every
/// process, every host-visible DXVK memory chunk, every exported/shared
/// surface, and every KMD-standard GDI redirection surface is one slot. The
/// old cap of 256 filled after ~2 h of desktop churn, at which point every
/// new venus consumer failed guest-side (`vkCreateInstance` →
/// VK_ERROR_OUT_OF_HOST_MEMORY for new processes; dwm lost its device → no
/// IddCx swapchain offers). 8192 slots ≈ 448 KiB of non-paged pool, reserved
/// once at init. Exhaustion is now counted (`BLOB_FULL_REJECTS`) and visible
/// via `HELIOS_ESCAPE_QUERY_STATS`; hitting the new cap indicates a leak, not
/// a workload.
pub(crate) const MAX_BLOBS: usize = 8192;
/// Max live virtio resources. This covers both escape blobs and KMD/WDDM standard
/// allocations, so teardown can suppress duplicate RESOURCE_UNREF commands. Must
/// be ≥ MAX_BLOBS (every blob is a live resource; non-blob resources add more).
const MAX_RESOURCES: usize = 16384;
/// Max concurrently-tracked virtio-gpu contexts (one per live device, generous).
const MAX_CONTEXTS: usize = 1024;
/// Max coalescing free ranges in the window allocator's free list. Overflow
/// drops the freed range (leaks window offset space) — counted in
/// `WINDOW_RANGE_DROPS`.
const MAX_WINDOW_RANGES: usize = 1024;
/// Per-map size cap (also bounds the `IoAllocateMdl` ULONG length on the caller).
const MAX_BLOB_MAP_BYTES: u64 = 256 << 20;

// Rounds `n` up to the next `BLOB_PAGE` multiple (saturating). The body moved to
// `helios_kmd_logic` (host unit-tested, no `wdk-sys` edge); `BLOB_PAGE` stays
// here because the window allocator still uses it directly at `map_blob_prepare`.
use helios_kmd_logic::round_up_page;

/// Result of the under-lock phase of MAP_BLOB ([`VirtioGpu::map_blob_prepare`]): the
/// guest-physical range to map and the host's requested caching. The user-space
/// mapping (MDL + `MmMapLockedPagesSpecifyCache`) is built by the Escape handler at
/// PASSIVE_LEVEL, OUTSIDE the virtio spinlock.
#[derive(Clone, Copy)]
pub struct BlobMapPrep {
    /// Guest-physical base of the resource's mapping inside the host-visible window.
    pub gpa: u64,
    /// Page-rounded length to map, in bytes.
    pub size: u64,
    /// Host caching nibble (`VIRTIO_GPU_MAP_CACHE_*`) from `RESP_OK_MAP_INFO`.
    pub map_cache: u32,
}

/// One tracked blob resource.
/// A device that can own escape-tracked state: dxgkrnl's `DXGKARG_ESCAPE.hDevice`
/// for the escaping device, proven non-null.
///
/// The guest and kernel ownership domains used to share one `usize`
/// representation, and 0 belonged to BOTH: it was a legal (forgeable) escape
/// handle value AND the kernel's "KMD-owned, invisible to escape reclaim"
/// sentinel. `NonZeroUsize` separates them — `Option<DeviceOwner>` costs no
/// extra bytes (niche-optimised), `None` means KMD-owned, and an escape verb
/// that takes a `DeviceOwner` cannot be handed the kernel sentinel at all
/// (k-capsescape-01).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DeviceOwner(core::num::NonZeroUsize);

impl DeviceOwner {
    /// `None` for a null handle — the caller must refuse rather than substitute.
    pub fn new(raw: usize) -> Option<Self> {
        core::num::NonZeroUsize::new(raw).map(Self)
    }

    /// The opaque handle value, for tables that are not owner-typed (the
    /// user-mapping table) and for diagnostics.
    pub fn raw(self) -> usize {
        self.0.get()
    }
}

/// A context id resolved through the tracking table against its owner.
///
/// The wire-facing context calls take this rather than a raw `u32`, so a
/// guest-supplied id cannot reach the host without the table lookup. This is a
/// real guarantee only because context tracking is mandatory
/// (`VirtioGpu::reserve_context_slot`): with best-effort tracking the resolver
/// would have to fall back to trusting unknown ids, and the type would merely
/// relocate the check.
#[derive(Clone, Copy)]
pub struct OwnedCtx {
    id: u32,
}

impl OwnedCtx {
    /// The wire context id. Only reachable from a successful table lookup.
    pub fn id(self) -> u32 {
        self.id
    }
}

/// Which slots a blob lookup may match.
///
/// The two "no owner" concepts are deliberately NOT the same value: `Any` is a
/// wildcard used by kernel paths that resolve by resource id alone (the paging
/// engine, the Present blit, the scan-out diagnostics), while
/// `Exactly(None)` means specifically the KMD-owned slots. Collapsing them
/// would either break the kernel lookups or hand escapes a wildcard.
#[derive(Clone, Copy)]
pub enum OwnerFilter {
    Any,
    Exactly(Option<DeviceOwner>),
}

#[derive(Clone, Copy)]
struct BlobSlot {
    /// The owner that allocated this blob: `Some(device)` for an escape-owned
    /// blob, `None` for KMD-owned (venus infrastructure, or a blob adopted by a
    /// WDDM allocation). `DxgkDdiDestroyDevice` reclaims every
    /// blob tagged with the destroyed handle, so a crashing/forgetful ICD (e.g.
    /// the crash-looping LogonUI, or any process that skips RELEASE_BLOB) cannot
    /// leak the bounded blob table (`MAX_BLOBS`) and false-trip later allocations
    /// with `STATUS_INSUFFICIENT_RESOURCES`.
    owner: Option<DeviceOwner>,
    ctx_id: u32,
    resource_id: u32,
    /// Blob size in bytes (from ALLOC_BLOB; MAP_BLOB needs it to size the MDL).
    size: u64,
    /// RESOURCE_MAP_BLOB succeeded and must be paired with RESOURCE_UNMAP_BLOB.
    mapped: bool,
    /// A RESOURCE_MAP_BLOB round-trip is in flight for this slot (the window
    /// range is reserved; concurrent mappers must wait — see `blob_map_begin`).
    map_pending: bool,
    /// Host caching nibble from RESP_OK_MAP_INFO (valid once `mapped`).
    map_cache: u32,
    /// Host-visible window offset used for RESOURCE_MAP_BLOB.
    map_offset: u64,
    /// Rounded mapped length in the host-visible window.
    map_len: u64,
}

/// A free span in the host-visible window's offset space (bump + coalescing free).
#[derive(Clone, Copy)]
struct WindowRange {
    offset: u64,
    len: u64,
}

/// Point-in-time occupancy snapshot of the bounded tables (see
/// [`VirtioGpu::table_stats`]); consumed by `HELIOS_ESCAPE_QUERY_STATS`.
#[derive(Clone, Copy)]
pub struct TableStats {
    pub blobs_live: u32,
    pub resources_live: u32,
    pub contexts_live: u32,
    /// Bytes allocated in the window offset space (bump high-water minus free list).
    pub window_used: u64,
    /// Total window length (0 if the device exposes no host-visible window).
    pub window_len: u64,
}

/// One tracked virtio-gpu context, tagged with the owning device handle for
/// device-teardown reclamation.
#[derive(Clone, Copy)]
struct ContextSlot {
    /// Owning device, or `None` for the KMD's own persistent venus context.
    owner: Option<DeviceOwner>,
    ctx_id: u32,
}

// ── C3/M3.4 async submission machinery ───────────────────────────────────────

/// Max in-flight control-queue entries. Tokens are descriptor-chain heads, so
/// they are always `< CTRL_QUEUE_SIZE`; each chain uses ≥ 2 descriptors, which
/// caps real concurrency at half this.
pub const MAX_INFLIGHT: usize = CTRL_QUEUE_SIZE;
/// Parked (completed, awaiting PASSIVE free) entry capacity. Enqueues are
/// refused once `parked` crosses [`PARKED_ENQUEUE_GATE`], and one drain can
/// park at most `MAX_INFLIGHT` entries, so this bound is never exceeded.
pub const MAX_PARKED: usize = 4 * MAX_INFLIGHT;
/// Completed command buffers retained for reuse. Count, individual capacity,
/// and total bytes are all bounded: a rare large Venus CS must never pin a
/// correspondingly large physically-contiguous allocation for device lifetime.
const MAX_DMA_POOL: usize = 128;
const MAX_DMA_POOL_BUFFER_BYTES: usize = 64 * 1024;
const MAX_DMA_POOL_BYTES: usize = 2 * 1024 * 1024;
/// Enqueue refusal threshold for the parked table (forces the PASSIVE caller
/// to reap before submitting more).
const PARKED_ENQUEUE_GATE: usize = MAX_PARKED - MAX_INFLIGHT;
/// Max concurrent WAIT_FENCE waiters.
const MAX_FENCE_WAITERS: usize = 64;
/// Max parked fence-event registrations (REGISTER_FENCE_EVENT). Sized for
/// every venus process (dwm + apps + WUDFHost) to park several waits each
/// (retire thread + app fence waits + flip/acquire gates); overflow is
/// counted and the ICD falls back to the blocking-escape wait.
const MAX_FENCE_EVENTS: usize = 256;
/// Max WDDM submissions pending on venus completion.
const MAX_WDDM_PENDING: usize = 256;
/// Max response bytes a synchronous command may expect (copied into the
/// waiter's [`SyncWaitBlock`]; the largest runtime response is
/// `VirtioGpuRespMapInfo`. `init`'s big GET_DISPLAY_INFO reply stays on the
/// inline polled path and does not ride this machinery).
pub const SYNC_RESP_MAX: usize = 64;
/// Bytes for an async submit's metadata buffer: the device-read SUBMIT_3D
/// header followed by the device-written ctrl response.
pub const SUBMIT_META_BYTES: usize =
    core::mem::size_of::<VirtioGpuCmdSubmit>() + core::mem::size_of::<VirtioGpuCtrlHdr>();

/// `NotificationEvent` (`EVENT_TYPE` value 0): stays signaled until cleared —
/// the right semantics for one-shot completion events.
const NOTIFICATION_EVENT: i32 = 0;
/// `IO_NO_INCREMENT` priority boost for `KeSetEvent`.
const IO_NO_INCREMENT: i32 = 0;

/// A PASSIVE waiter's completion block. Lives on the waiter's stack; the
/// registered pointer stays valid because the waiter ALWAYS deregisters (or
/// observes completion) under the device spinlock before returning.
pub struct SyncWaitBlock {
    /// Signaled (under the device spinlock) when the entry completes.
    pub event: KEVENT,
    /// Set (Release) before the event is signaled; the waiter reads it
    /// (Acquire) after the wait / under the lock.
    done: AtomicBool,
    /// The device-written response bytes, copied out of the entry's DMA buffer
    /// by `drain_used` before the event is signaled.
    resp: UnsafeCell<[u8; SYNC_RESP_MAX]>,
}

/// Stable adapter-owned notification target for a scanout copy submitted on a
/// GPU-completion ring. The used-ring drain sets `pending` and wakes `event`
/// only after a successful ring-1 SUBMIT_3D completion.
///
/// Fields are PRIVATE and there is exactly one constructor,
/// [`Self::for_adapter`], which derives all four pointers from a single
/// `&AdapterContext`. That is what makes "the four pointers always come from the
/// same adapter" structural rather than assembled field by field.
///
/// It can only be attached to a submission through
/// [`VirtioGpu::enqueue_scanout_submit`], which hard-codes ring 1 — the ring the
/// drain actually honours. A notify on any other ring used to be silently
/// discarded at completion with no counter, and because the drain is the ONLY
/// clear of `vidpn_programming` on the copied-primary path, that left the gate at
/// 1 and suppressed every further CRTC_VSYNC indefinitely.
///
/// The four `NonNull`s' validity rests on the adapter outliving the transport,
/// which StopDevice enforces by ordering (`set_virtio(None)` after cancel/join),
/// and on `init_kernel_events` having run before any `KeSetEvent` on `hpd_event`.
/// Neither is encodable — the self-referential lifetime (`VirtioGpu` lives inside
/// the `AdapterContext` it points back into) is what defeats it — so both are
/// documented HERE, once, instead of on four fields.
/// The virtio ring whose used-ring completion represents real host GPU
/// completion, and therefore the only one on which a [`ScanoutNotify`] may be
/// honoured. Ring 0 retires at host DECODE, which is too early to publish pixels.
pub(crate) const SCANOUT_RING_IDX: u32 = 1;

#[derive(Clone, Copy)]
pub struct ScanoutNotify {
    pending: NonNull<AtomicU32>,
    /// Address of the Windows primary whose compatibility copy this submission
    /// performs. Published as displayed only on successful GPU completion.
    displayed_primary: NonNull<AtomicU64>,
    /// Exact SetVidPnSourceAddress programming gate, packed `(seq << 32) | active`.
    /// Cleared after the copy succeeds or fails so VSync can resume with
    /// authoritative state — but only for THIS submission's interval, named by
    /// `ticket`.
    programming: NonNull<AtomicU64>,
    /// The programming generation this submission belongs to, carried BY VALUE
    /// rather than only as a pointer to the gate. A completion that arrives
    /// after a newer interval was raised fails its compare-exchange and counts
    /// `ScStale` instead of clearing a gate that is not its own.
    ticket: crate::adapter::ProgrammingTicket,
    primary_address: u64,
    event: NonNull<KEVENT>,
}

impl ScanoutNotify {
    /// The ONE construction site. Reached through
    /// `AdapterContext::scanout_notify`.
    pub(crate) fn for_adapter(
        adapter: &crate::adapter::AdapterContext,
        primary_address: u64,
        ticket: crate::adapter::ProgrammingTicket,
    ) -> Self {
        Self {
            pending: NonNull::from(&adapter.scanout_refresh_pending),
            displayed_primary: NonNull::from(&adapter.last_primary_address),
            programming: NonNull::from(&adapter.vidpn_programming),
            ticket,
            primary_address,
            // SAFETY: hpd_event is embedded in the stable adapter and
            // initialized by init_kernel_events before StartDevice creates any
            // Venus submissions.
            event: unsafe { NonNull::new_unchecked(adapter.hpd_event.get()) },
        }
    }
}

impl SyncWaitBlock {
    /// Run `f` with a wait block that is zeroed and initialised in place on
    /// THIS frame and is never nameable by the caller.
    ///
    /// The three invariants — initialised before registration, never moved
    /// after, always deregistered before the frame dies — used to be carried by
    /// comments over a `new_zeroed()` -> `unsafe { init() }` ->
    /// `NonNull::from(&mut block)` dance at two call sites. A KEVENT dispatcher
    /// header is self-referential, so a move after `init` corrupts the wait list
    /// silently, and BOTH misuses compiled with zero `unsafe`, because
    /// `enqueue_sync` and `fence_wait_prepare` are safe fns taking a
    /// `NonNull<SyncWaitBlock>`:
    ///
    ///     let mut b = SyncWaitBlock::new_zeroed();
    ///     enqueue_sync(.., NonNull::from(&mut b));     // never init'ed
    ///
    /// and so did returning the block from a helper between `init` and
    /// registration. Neither is expressible now: the value has no name outside
    /// this function.
    ///
    /// Deregistration still depends on the caller's control flow, which is why
    /// the abandon/cancel logic belongs in the closure's own epilogue.
    pub fn with<R>(f: impl FnOnce(&WaitBlockRef<'_>) -> R) -> R {
        let mut block = Self::new_zeroed();
        // SAFETY: `block` is a local of THIS frame, so it is at its final
        // address, and it is never moved afterwards — `f` only ever sees a
        // `WaitBlockRef` borrowing it. It outlives the call to `f`.
        unsafe { block.init() };
        let block_ref = WaitBlockRef {
            ptr: NonNull::from(&mut block),
            _frame: PhantomData,
        };
        f(&block_ref)
    }

    /// A zeroed block. Private: reachable only through [`Self::with`], which is
    /// what makes "registered but never initialised" unrepresentable.
    fn new_zeroed() -> Self {
        // SAFETY: a zeroed KEVENT/AtomicBool/byte-array is a valid *inert*
        // value; `init` initializes the dispatcher header before any use.
        unsafe { core::mem::zeroed() }
    }

    /// Initialize the embedded KEVENT (NotificationEvent, unsignaled).
    ///
    /// # Safety
    /// `self` must be at its final (pinned) address.
    unsafe fn init(&mut self) {
        // SAFETY: valid, stable KEVENT storage per the fn contract.
        unsafe { KeInitializeEvent(&mut self.event, NOTIFICATION_EVENT, 0) };
        self.done.store(false, Ordering::Relaxed);
    }

    /// Whether the entry completed (Acquire — pairs with the drain's Release).
    fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Copy the response bytes out (only valid once [`Self::is_done`]).
    fn copy_resp(&self, out: &mut [u8]) {
        let n = out.len().min(SYNC_RESP_MAX);
        // SAFETY: `resp` is only written by the drain BEFORE `done` is set
        // (Release); the caller reads AFTER observing `done` (Acquire).
        let src = unsafe { &*self.resp.get() };
        out[..n].copy_from_slice(&src[..n]);
    }
}

/// The only handle a [`SyncWaitBlock::with`] closure gets: a pointer to hand
/// the transport, plus the two reads the waiter needs. Borrows the block, so it
/// cannot outlive the frame the block lives on.
pub struct WaitBlockRef<'a> {
    ptr: NonNull<SyncWaitBlock>,
    _frame: PhantomData<&'a SyncWaitBlock>,
}

impl WaitBlockRef<'_> {
    /// The registration pointer for `enqueue_sync` / `fence_wait_prepare`.
    pub fn as_ptr(&self) -> NonNull<SyncWaitBlock> {
        self.ptr
    }

    pub fn is_done(&self) -> bool {
        // SAFETY: the block is alive for this borrow's whole lifetime; `done`
        // is an atomic, so the concurrent DISPATCH-level writer is fine.
        unsafe { self.ptr.as_ref() }.is_done()
    }

    pub fn copy_resp(&self, out: &mut [u8]) {
        // SAFETY: as above; `copy_resp`'s own contract covers the ordering.
        unsafe { self.ptr.as_ref() }.copy_resp(out);
    }
}

/// What an in-flight entry is.
enum InFlightKind {
    /// A synchronous control command; `waiter` (if any) is signaled on
    /// completion. `None` = the waiter timed out and abandoned the entry.
    Sync {
        waiter: Option<NonNull<SyncWaitBlock>>,
    },
    /// An async fenced SUBMIT_3D carrying `fence_id` (KMD-assigned wire id).
    /// `ring_idx` 0 = host CPU ring (retires at decode); >= 1 = a per-queue
    /// GPU-completion fence (virglrenderer vkr sync thread) that legally stays
    /// in flight for the full GPU-work duration.
    AsyncVenus {
        fence_id: u64,
        ring_idx: u8,
        scanout_notify: Option<ScanoutNotify>,
    },
    /// A control command whose caller must not wait for the host response.
    /// `completion` is a stable adapter-owned 0/1 gate; the used-ring drain
    /// clears it and wakes the stable worker event.  This lets scanout refresh
    /// coalesce to one outstanding RESOURCE_FLUSH without a synchronous ctrl
    /// round-trip limiting presentation cadence.
    AsyncControl {
        completion: NonNull<AtomicU32>,
        completion_errors: NonNull<AtomicU32>,
        wake_event: NonNull<KEVENT>,
        success_store: Option<(NonNull<AtomicU32>, u32)>,
        resubmit: Option<NonNull<AtomicU32>>,
    },
}

/// One outstanding control-queue submission. Owns its device-visible buffers
/// for as long as the device may DMA them (until `pop_used`); afterwards the
/// entry is parked and dropped at PASSIVE_LEVEL (DmaBuffer frees are
/// PASSIVE-only).
pub struct InFlight {
    /// Descriptor-chain head returned by `VirtQueue::add` (the pop_used token).
    token: u16,
    kind: InFlightKind,
    /// `[in0 | in1? | resp]` — request span(s) followed by the device-written
    /// response span, all in one contiguous DMA buffer.
    meta: DmaBuffer,
    /// The exact shape `add` was called with. One value, matched exhaustively,
    /// instead of three independent length fields the drain re-derived by hand.
    chain: Chain,
    resp_len: usize,
    /// Separate device-read venus payload (async submits).
    venus: Option<DmaBuffer>,
}

/// The descriptor-chain shape of one submission.
///
/// `add` and `pop_used` must be handed the SAME buffer list, and virtio-drivers
/// does not validate that: `pop_used` -> `recycle_descriptors` walks the chain
/// against the caller-supplied list and PANICS on a mismatch
/// (`virtio-drivers-0.13.0/src/queue.rs:461,477,479,501`), which under
/// `panic = "abort"` with `wdk_panic` is a `KeBugCheck` from the interrupt DPC
/// while the device spinlock is held.
///
/// The agreement used to be expressed only by three independent length fields
/// plus the comment "exactly the spans `add` was called with", re-derived in
/// four separate blocks of raw-pointer arithmetic. As one exhaustively-matched
/// value it is a compile error to add a shape without teaching both sides.
#[derive(Clone, Copy)]
enum Chain {
    /// `[in0] -> [resp]`. Async control commands.
    Meta1 { in0_len: usize },
    /// `[in0, in1] -> [resp]`. Sync control with a second device-read span.
    Meta2 { in0_len: usize, in1_len: usize },
    /// `[hdr, venus stream] -> [resp]`, the stream in its own buffer.
    MetaPlusVenus { hdr_len: usize, venus_len: usize },
}

impl Chain {
    /// Byte offset of the response span inside `meta`.
    ///
    /// All three shapes place it immediately after the meta-resident request
    /// spans, i.e. at `in0_len + in1_len` — preserved verbatim, because
    /// `drain_used` reads the `VIRTIO_GPU_RESP_*` word and copies the sync
    /// response from exactly this offset.
    fn resp_offset(self) -> usize {
        match self {
            Self::Meta1 { in0_len } => in0_len,
            Self::Meta2 { in0_len, in1_len } => in0_len + in1_len,
            Self::MetaPlusVenus { hdr_len, .. } => hdr_len,
        }
    }

    /// The device-READ spans (up to two) and the device-WRITTEN response span,
    /// or `None` if any of them falls outside its buffer.
    ///
    /// The ONLY producer of the buffer list, so `add` and `pop_used` are handed
    /// literally the same code and the arm selection cannot differ between
    /// them. It also replaces the per-arm ad-hoc `t <= meta.as_slice().len()`
    /// bounds checks: every span is proved in `DmaBuffer::span`.
    fn spans(
        self,
        meta: &DmaBuffer,
        venus: Option<&DmaBuffer>,
        resp_len: usize,
    ) -> Option<([DmaSpan; 2], usize, DmaSpan)> {
        let resp = meta.span(self.resp_offset(), resp_len)?;
        Some(match self {
            Self::Meta1 { in0_len } => ([meta.span(0, in0_len)?, DmaSpan::EMPTY], 1, resp),
            Self::Meta2 { in0_len, in1_len } => (
                [meta.span(0, in0_len)?, meta.span(in0_len, in1_len)?],
                2,
                resp,
            ),
            Self::MetaPlusVenus { hdr_len, venus_len } => (
                [meta.span(0, hdr_len)?, venus?.span(0, venus_len)?],
                2,
                resp,
            ),
        })
    }
}

impl InFlight {
    /// Recover the entry-owned DMA buffers after the device has consumed them.
    /// Called only by the PASSIVE reaper after the entry leaves `parked`.
    pub fn into_dma_buffers(self) -> (DmaBuffer, Option<DmaBuffer>) {
        (self.meta, self.venus)
    }
}

/// A non-`Copy`, non-`Clone` receipt for one registered synchronous submission.
///
/// The token, the `NonNull<SyncWaitBlock>` and the block's own pinned storage
/// are three values the caller had to keep in sync across enqueue / PASSIVE
/// wait / abandon. Making the token move-only means it cannot be reused after
/// the abandonment that consumes it.
pub struct SyncTicket {
    token: u16,
}

impl SyncTicket {
    /// The raw descriptor-chain head, for a diagnostic breadcrumb only. Reading
    /// it does not consume the ticket; `abandon_sync` still does.
    pub fn raw(&self) -> u16 {
        self.token
    }
}

/// What [`VirtioGpu::abandon_sync`] found.
///
/// This replaces a bool whose fall-through returned `true` — "already
/// completed, treat as success" — for EVERY case that was not an exact
/// (token, Sync, same-waiter) match, including a token now owned by a different
/// command and a kind mismatch. The caller then ran `copy_resp` on a block that
/// may never have been written. That was safe only because (a) a token cannot
/// be re-issued until its chain is popped, which implies the old waiter was
/// signalled, and (b) `new_zeroed` leaves `resp` all-zero and 0 is not a
/// RESP_OK code, so `ctrl_roundtrip_ok` still errored out — i.e. correctness
/// rested on a virtio-ring property and a zero-init accident, neither of them
/// stated at that function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// No in-flight entry holds this token: the drain already completed and
    /// signalled it. The response bytes are valid.
    AlreadyCompleted,
    /// The waiter was deregistered before completion. The response bytes were
    /// never written.
    Abandoned,
    /// The token names an entry that is not this waiter's — a NEW error
    /// population, and the one behaviour change in this item. It converts a
    /// silent read of an unwritten buffer into a counted error.
    NotOurs,
}

/// A registered WAIT_FENCE waiter.
struct FenceWaiter {
    fence_id: u64,
    block: NonNull<SyncWaitBlock>,
}

/// A usermode event registered for one-shot signaling at wire-fence retirement
/// (REGISTER_FENCE_EVENT, KMD 22.22.54). `event` is the executive event
/// object's body (a `KEVENT`) from `ObReferenceObjectByHandle` — the escape
/// handler took a reference, so the object outlives the registering process;
/// whoever removes the entry MUST dereference it (the drain uses
/// `ObDereferenceObjectDeferDelete` — a plain deref at DISPATCH could run the
/// object's PASSIVE-only deletion if it drops the last reference).
struct FenceEventEntry {
    fence_id: u64,
    event: NonNull<KEVENT>,
}

/// Result of [`VirtioGpu::fence_event_register`].
pub enum FenceEventReg {
    /// Parked; the drain will KeSetEvent + deref at retirement.
    Registered,
    /// The fence has already retired. NOT parked, no reference kept by the
    /// table — the caller signals + derefs.
    AlreadyComplete,
    /// The id was never assigned by this transport instance.
    Invalid,
    /// Table full (counted) — the caller falls back to the blocking wait.
    TableFull,
    /// This (fence_id, event) pair is already parked (counted, refused).
    Duplicate,
}

/// A WDDM submission whose `DXGK_INTERRUPT_DMA_COMPLETED` is gated on venus
/// completion: it may signal once every async wire fence `< watermark` has
/// retired (and strictly in FIFO order — SubmissionFenceIds are watermarks to
/// dxgkrnl, so they must complete monotonically).
struct WddmPending {
    fence: u32,
    watermark: u64,
    wait_gpu: bool,
    refresh_scanout: bool,
}

/// A WDDM submission popped from the pending FIFO whose `DMA_COMPLETED` has not
/// yet been delivered.
///
/// Deliberately **not** `Copy`, and consumed by exactly two operations
/// ([`WddmReady::delivered`] and [`VirtioGpu::requeue_wddm_front`]). While this
/// value is alive the entry is in no queue at all: the fence is out of
/// `wddm_pending` and the completed watermark still points below it. Dropping it
/// without doing either loses the fence permanently, VidSch never sees it retire,
/// and the only symptom is a TDR. The `Copy` derive it used to have was what made
/// the old `[WddmReady; 8]` batch array possible, which is why the one-at-a-time
/// taker is a precondition of this encoding rather than a stylistic choice.
#[must_use = "a popped WDDM fence must be delivered or requeued, never dropped"]
pub struct WddmReady {
    pending: WddmPending,
}

impl WddmReady {
    pub fn fence(&self) -> u32 {
        self.pending.fence
    }

    pub fn refresh_scanout(&self) -> bool {
        self.pending.refresh_scanout
    }

    /// Consume the token after `DMA_COMPLETED` was delivered successfully.
    /// Exists so the success path *states* that it consumed the fence rather
    /// than letting it fall out of scope.
    pub fn delivered(self) {}
}

/// Result of [`VirtioGpu::fence_wait_prepare`].
pub enum FenceWaitPrep {
    /// The fence already completed (or the id predates the tracked window).
    Complete,
    /// Registered; wait on the block's event.
    Registered,
    /// The id was never assigned by this transport instance.
    Invalid,
    /// Waiter table full — retry after a short PASSIVE sleep.
    TableFull,
}

/// Result of [`VirtioGpu::blob_map_begin`].
pub enum BlobMapBegin {
    /// Already mapped — here is the existing mapping.
    Mapped(BlobMapPrep),
    /// Range reserved; the caller must run the RESOURCE_MAP_BLOB round-trip and
    /// then call [`VirtioGpu::blob_map_finish`].
    Start { offset: u64, len: u64 },
    /// Another mapper's round-trip is in flight — retry after a PASSIVE sleep.
    Busy,
    /// Unknown resource / no host-visible window / size out of range / window
    /// exhausted.
    Failed(VirtioError),
}

/// Result of [`VirtioGpu::blob_remap_begin`] (fixed-offset, VidMm-dictated maps
/// for the CPU-visible BAR memory segment — `build_paging_buffer.rs`).
pub enum BlobRemapBegin {
    /// Already mapped at exactly the requested offset — nothing to do.
    Mapped(BlobMapPrep),
    /// Reserved. The caller must (1) RESOURCE_UNMAP_BLOB if `old` is
    /// `Some((offset, len))` and return that range via
    /// [`VirtioGpu::free_window_range_pub`] (a no-op for VidMm-partition
    /// offsets), (2) RESOURCE_MAP_BLOB at the new offset, then (3) call
    /// [`VirtioGpu::blob_map_finish`] with the new offset.
    Start { old: Option<(u64, u64)>, len: u64 },
    /// Another mapper's round-trip is in flight — retry after a PASSIVE sleep.
    Busy,
    /// Unknown resource / out-of-partition target / bad size.
    Failed(VirtioError),
}

/// Result of [`VirtioGpu::blob_map_finish`].
pub enum BlobMapFinish {
    /// Mapping recorded.
    Done(BlobMapPrep),
    /// The host rejected the map (range returned to the allocator).
    HostRejected,
    /// The slot vanished mid-map (owner teardown raced the round-trip): the
    /// caller must issue RESOURCE_UNMAP_BLOB and return the range via
    /// [`VirtioGpu::free_window_range_pub`].
    SlotGone,
}

/// An initialized virtio-gpu transport.
pub struct VirtioGpu {
    /// The virtio-modern PCI transport (owns the mapped cfg-region VAs).
    transport: PciTransport,
    /// Control virtqueue (queue 0) — all GPU commands ride this.
    control: VirtQueue<WdkHal, CTRL_QUEUE_SIZE>,
    /// Next virtio-gpu 3D context id to hand out (guest-assigned; 0 is the
    /// reserved global context, so we start at 1). Phase 3.
    next_ctx_id: AtomicU32,
    /// Next virtio-gpu resource id to hand out (0 is reserved). Phase 3 (M3.5).
    next_resource_id: AtomicU32,
    /// Host-visible blob window (SHARED_MEMORY_CFG/HOST_VISIBLE BAR), discovered
    /// in `init`. `None` if the device exposes no host-visible window — the WDDM
    /// blob-map path is then unavailable (Stage 2 fails honestly). Gate 5a Stage 2.
    host_visible: Option<HostVisibleWindow>,
    /// Mapped kernel VA of the virtio ISR-status register (read-to-clear), or 0 if
    /// the device exposes no ISR cap. `DxgkDdiInterruptRoutine` reads this at DIRQL
    /// to acknowledge the line-based INTx (the device is `MSISupported=0`). See
    /// [`map_isr_status_register`].
    isr_status_va: usize,
    /// Tracked blobs (resource_id → size/mapping state). Heap-reserved to MAX_BLOBS
    /// at init so `push` under the spinlock never reallocates (the 0x7F lesson).
    blobs: Vec<BlobSlot>,
    /// Blob-table slots reserved by in-flight (multi-phase) creates, counted
    /// against MAX_BLOBS so a burst of concurrent creates cannot overshoot the
    /// reserved capacity (push under the spinlock must never reallocate).
    blobs_reserved: usize,
    /// Every host-live virtio resource id created through this transport.
    /// Removal is one-shot and gates CTX_DETACH_RESOURCE/RESOURCE_UNREF, avoiding
    /// qemu `RESOURCE_UNREF: resource does not exist` errors from duplicate DDI
    /// teardown paths.
    resources: Vec<u32>,
    /// Live-resource slots reserved by in-flight creates (see `blobs_reserved`).
    resources_reserved: usize,
    /// Context tracking slots reserved by in-flight CTX_CREATEs. Tracking is
    /// MANDATORY (see `reserve_context_slot`), so a context is reserved before
    /// the wire round-trip and committed after it.
    contexts_reserved: usize,
    /// Live virtio-gpu contexts, tagged with the owning device handle, so
    /// `DxgkDdiDestroyDevice` can `CTX_DESTROY` any context an ICD created but did
    /// not tear down (crash / skipped CTX_DESTROY) — otherwise leaked contexts
    /// accumulate host-side state and eventually wedge the render server. Reserved
    /// to MAX_CONTEXTS at init (no realloc under the spinlock).
    contexts: Vec<ContextSlot>,
    /// Bump high-water for the host-visible window offset allocator.
    next_window_offset: u64,
    /// First `vidmm_reserved` bytes of the host-visible window are OWNED BY
    /// VIDMM (the CPU-visible BAR memory segment, `query_adapter_info`): VidMm's
    /// segment allocator assigns offsets there and `BuildPagingBuffer` maps each
    /// allocation's blob at the assigned offset. The KMD bump/free allocator
    /// never hands out or reclaims offsets below this mark (see
    /// [`Self::reserve_window_prefix`] / [`Self::free_window_range`]).
    vidmm_reserved: u64,
    /// Coalescing free list for released window ranges (bounded by MAX_WINDOW_RANGES).
    free_window_ranges: Vec<WindowRange>,
    /// In-flight control-queue entries (token-matched; capacity MAX_INFLIGHT,
    /// reserved at init — pushes never reallocate under the spinlock).
    inflight: Vec<InFlight>,
    /// Completed entries awaiting a PASSIVE reap (`swap_parked`). Capacity
    /// MAX_PARKED, reserved at init.
    parked: Vec<InFlight>,
    /// Empty pre-reserved vectors swapped through the PASSIVE reaper. This
    /// removes two kernel-heap allocations from every tiny Venus completion.
    parked_spare: Vec<InFlight>,
    reap_buffers_spare: Vec<DmaBuffer>,
    reap_in_progress: bool,
    /// PASSIVE-reaped DMA buffers ready for another command. Accessed under the
    /// existing virtio spinlock, but allocation/free never occurs there.
    dma_pool: Vec<DmaBuffer>,
    dma_pool_bytes: usize,
    /// Registered WAIT_FENCE waiters (capacity MAX_FENCE_WAITERS).
    fence_waiters: Vec<FenceWaiter>,
    /// Usermode events awaiting wire-fence retirement (capacity
    /// MAX_FENCE_EVENTS, reserved at init — pushes never reallocate under the
    /// spinlock). Entries hold an object reference each.
    fence_events: Vec<FenceEventEntry>,
    /// Next wire fence id to assign (globally monotonic, starts at 1; 0 is
    /// never a valid wire fence).
    next_wire_fence: u64,
    /// WDDM submissions pending on venus completion, FIFO (capacity
    /// MAX_WDDM_PENDING, reserved at init).
    wddm_pending: VecDeque<WddmPending>,
    /// Latest DWM/primary dirty marker waiting for every Venus wire fence that
    /// preceded it to retire. Markers coalesce to the newest watermark so idle
    /// wakeups do not depend on which WDDM SubmitCommand DDI VidSch selects.
    scanout_refresh_watermark: Option<u64>,
    /// Ring-corruption latch: set when the used ring returns a token we do not
    /// track or `pop_used` fails structurally. The ring state is then
    /// untrustworthy and every subsequent command fails fast. NOTE: unlike the
    /// old model, a slow host does NOT set this — waiter timeouts abandon
    /// their entry and the transport keeps working.
    failed: bool,
    /// Scanout-0 preferred size `(width, height)` reported by the host in the
    /// `GET_DISPLAY_INFO` reply at `init` (`pmodes[0]`), or `None` if the host
    /// reported nothing usable. The display half uses this as the VidPn mode +
    /// generated-EDID native resolution so we present the size QEMU actually wants
    /// on scanout 0 (instead of a hardcoded guess). Read once by StartDevice.
    display_mode: Option<(u32, u32)>,
}

impl VirtioGpu {
    /// Bring the virtio-gpu device online and prove it with `GET_DISPLAY_INFO`.
    pub fn init(dxgkrnl: &DXGKRNL_INTERFACE) -> Result<Self, VirtioError> {
        // ── M1: discover the device + map BARs through Dxgkrnl ──────────────
        // A miniport doesn't own the bus, so config space is reached via the
        // Dxgkrnl callbacks; the DeviceFunction is a formality (DxgkConfigAccess
        // ignores it and addresses our own device via the DeviceHandle).
        let access = DxgkConfigAccess::new(dxgkrnl);
        let mut root = PciRoot::new(access);
        let device_function = DeviceFunction {
            bus: 0,
            device: 0,
            function: 0,
        };
        let mut transport = PciTransport::new::<WdkHal, _>(&mut root, device_function)
            .map_err(|_| VirtioError::DeviceError)?;

        // ── M2: feature negotiation (VirtIO 1.2 spec §3.1.1) ────────────────
        transport.set_status(DeviceStatus::empty()); // reset
        let mut spins = 0u32;
        while !transport.get_status().is_empty() && spins < 100_000 {
            spins += 1;
            core::hint::spin_loop();
        }
        // The bound was previously indistinguishable from success: the loop fell
        // through to ACKNOWLEDGE either way, so a device that never cleared its
        // status was driven through the whole init as if it had reset. Every
        // later assumption in this function rests on that reset. The sibling
        // GET_DISPLAY_INFO poll below already returns Err on its own bound.
        if !transport.get_status().is_empty() {
            crate::diag::fault(crate::diag::FaultCounter::StVioR, spins);
            return Err(VirtioError::DeviceError);
        }
        transport.set_status(DeviceStatus::ACKNOWLEDGE);
        transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);

        let offered = transport.read_device_features();
        let accepted = offered & (HELIOS_REQUIRED_FEATURES | HELIOS_OPTIONAL_FEATURES);
        transport.write_driver_features(accepted);
        transport.set_status(
            DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK,
        );
        if !transport.get_status().contains(DeviceStatus::FEATURES_OK)
            || accepted & HELIOS_REQUIRED_FEATURES != HELIOS_REQUIRED_FEATURES
        {
            transport.set_status(DeviceStatus::FAILED);
            return Err(VirtioError::FeatureRejected);
        }

        // ── M3: control virtqueue (queue 0), then DRIVER_OK ─────────────────
        let mut control = VirtQueue::<WdkHal, CTRL_QUEUE_SIZE>::new(
            &mut transport,
            CTRL_QUEUE,
            /* indirect */ false,
            /* event_idx */ false,
        )
        .map_err(|_| VirtioError::DeviceError)?;
        // Runtime ctrl completion is interrupt-driven. Be explicit instead of
        // relying on the freshly-zeroed avail.flags value: bit 0 clear asks the
        // device to interrupt after it adds a used element.
        control.set_dev_notify(true);
        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );

        // ── M4: GET_DISPLAY_INFO polled round-trip (smoke test) ─────────────
        // Request + response live in one contiguous page so each buffer is
        // physically contiguous for the device (our Hal::share is identity — no
        // bounce buffer). Halves are disjoint (split_at_mut): request is read by
        // the device, response is written by it. The page is a local RAII
        // `DmaBuffer` — the runtime paths own per-command buffers instead
        // (C3/M3.4), so no shared scratch survives init.
        let mut scratch = DmaBuffer::new(SCRATCH_BYTES).ok_or(VirtioError::OutOfMemory)?;
        let buf = scratch.as_mut_slice();
        let (req_buf, resp_buf) = buf.split_at_mut(SCRATCH_BYTES / 2);

        let hdr_len = core::mem::size_of::<VirtioGpuCtrlHdr>();
        let resp_len = core::mem::size_of::<VirtioGpuRespDisplayInfo>();
        let mut req = VirtioGpuCtrlHdr::zeroed();
        req.type_ = VIRTIO_GPU_CMD_GET_DISPLAY_INFO;
        req_buf[..hdr_len].copy_from_slice(bytemuck::bytes_of(&req));

        // Bounded inline round-trip (`Self` does not exist yet, so the
        // `ctrl_queue_bounded_roundtrip` helper is unavailable): a host that
        // never answers GET_DISPLAY_INFO must fail StartDevice cleanly, not
        // hang it forever. PASSIVE_LEVEL, no spinlock held.
        {
            let inputs: &[&[u8]] = &[&req_buf[..hdr_len]];
            let outputs: &mut [&mut [u8]] = &mut [&mut resp_buf[..resp_len]];
            // SAFETY: the scratch-page buffers stay valid for the whole block;
            // on timeout we bail out of init and never reuse this queue.
            let token =
                unsafe { control.add(inputs, outputs) }.map_err(|_| VirtioError::DeviceError)?;
            if control.should_notify() {
                transport.notify(CTRL_QUEUE);
            }
            let mut spins = 0u64;
            while !control.can_pop() {
                spins += 1;
                if spins >= CTRL_POLL_SPINS {
                    return Err(VirtioError::DeviceError);
                }
                core::hint::spin_loop();
            }
            // SAFETY: same buffers as `add`, still valid; `can_pop()` was true.
            unsafe { control.pop_used(token, inputs, outputs) }
                .map_err(|_| VirtioError::DeviceError)?;
        }

        let resp: &VirtioGpuRespDisplayInfo = bytemuck::from_bytes(&resp_buf[..resp_len]);
        if !resp_is_ok(resp.hdr.type_) {
            return Err(VirtioError::DeviceError);
        }
        crate::kmsg(c"Helios: virtio-gpu GET_DISPLAY_INFO OK\n");
        // Remember scanout 0's host-preferred size for the display half's VidPn
        // mode + generated EDID. QEMU reports it in `pmodes[0].r` even before a
        // scanout is bound; take it only when both dimensions look sane (a
        // 0×0 / not-yet-configured scanout falls back to the default in
        // StartDevice). Recorded so the host's report is visible live.
        let m0 = resp.pmodes[0].r;
        let display_mode =
            if m0.width >= 320 && m0.height >= 240 && m0.width <= 16384 && m0.height <= 16384 {
                Some((m0.width, m0.height))
            } else {
                None
            };
        crate::diag::record_named_bytes(
            b"DpInf",
            (m0.width.min(0xFFFF) << 16) | (m0.height & 0xFFFF),
        );

        // Discover the host-visible blob window (a fresh config accessor — the
        // original `access` was moved into `PciRoot` above; `DxgkConfigAccess` is
        // a cheap Copy of the device handle + callbacks). Gate 5a Stage 2.
        let host_visible = scan_host_visible_window(&DxgkConfigAccess::new(dxgkrnl));
        crate::diag::record(if host_visible.is_some() {
            0x0B00_0005
        } else {
            0x0B00_00E5
        });

        // Locate + map the ISR-status register so the (real) ISR can read-to-clear
        // the level-triggered INTx line and stop the unhandled-interrupt storm.
        let isr_status_va = map_isr_status_register(&DxgkConfigAccess::new(dxgkrnl));
        crate::diag::record(if isr_status_va != 0 {
            0x0B00_0006
        } else {
            0x0B00_00E6
        });
        // A map failure here is NOT a benign degrade on this INTx device: with no
        // ISR ack the level-triggered line stays asserted and Windows' interrupt
        // storm detector Code-43s the adapter. Report it ungated.
        let mmio_fails = crate::virtio::hal::MMIO_MAP_FAILS.load(Ordering::Relaxed);
        if mmio_fails != 0 {
            crate::diag::fault(crate::diag::FaultCounter::StIsr, mmio_fails);
        }

        let gpu = Self {
            transport,
            control,
            next_ctx_id: AtomicU32::new(1),
            next_resource_id: AtomicU32::new(1),
            host_visible,
            isr_status_va,
            blobs: Vec::with_capacity(MAX_BLOBS),
            blobs_reserved: 0,
            resources: Vec::with_capacity(MAX_RESOURCES),
            resources_reserved: 0,
            contexts_reserved: 0,
            contexts: Vec::with_capacity(MAX_CONTEXTS),
            next_window_offset: 0,
            vidmm_reserved: 0,
            free_window_ranges: Vec::with_capacity(MAX_WINDOW_RANGES),
            inflight: Vec::with_capacity(MAX_INFLIGHT),
            parked: Vec::with_capacity(MAX_PARKED),
            parked_spare: Vec::with_capacity(MAX_PARKED),
            reap_buffers_spare: Vec::with_capacity(2 * MAX_PARKED),
            reap_in_progress: false,
            dma_pool: Vec::with_capacity(MAX_DMA_POOL),
            dma_pool_bytes: 0,
            fence_waiters: Vec::with_capacity(MAX_FENCE_WAITERS),
            fence_events: Vec::with_capacity(MAX_FENCE_EVENTS),
            next_wire_fence: 1,
            wddm_pending: VecDeque::with_capacity(MAX_WDDM_PENDING),
            scanout_refresh_watermark: None,
            failed: false,
            display_mode,
        };
        // (The old Gate-2 venus ctx self-test is gone: the StartDevice venus
        // client bring-up right after transport init exercises the full context
        // + blob lifecycle for real.)

        // Read-to-clear the ISR-status register once: the GET_DISPLAY_INFO
        // round-trip above completed via the polled path, which never touches
        // this register, so the device may still be asserting INTx from that
        // completion. Clear it now (PASSIVE) so the line starts deasserted
        // before the interrupt-driven runtime paths take over.
        if gpu.isr_status_va != 0 {
            // SAFETY: `isr_status_va` is the mapped MMIO VA of the 1-byte
            // read-to-clear ISR-status register; a volatile read clears it.
            let _ = unsafe { core::ptr::read_volatile(gpu.isr_status_va as *const u8) };
        }

        // `scratch` (the init round-trip page) drops here — the descriptor
        // chain it backed was popped above, so the device no longer references
        // it.
        drop(scratch);
        Ok(gpu)
    }

    // ── C3/M3.4 queue machinery ──────────────────────────────────────────────
    //
    // Every method here runs under the AdapterContext virtio spinlock at
    // DISPATCH_LEVEL and NEVER waits, allocates, or frees DMA memory. Waiting
    // happens in `virtio::ctrl` (PASSIVE, KEVENT); freeing happens when a
    // PASSIVE caller reaps the parked list.

    /// Ring-corruption latch (a wedged-slow host does NOT set this).
    pub fn transport_failed(&self) -> bool {
        self.failed
    }

    /// Enqueue a synchronous control command. `meta` = `[in0 | in1? | resp]`
    /// (one contiguous DMA buffer); the entry owns it until completion. The
    /// waiter's block is signaled (resp copied in) by [`Self::drain_used`].
    /// On `QueueFull` the buffer is handed back for a PASSIVE retry.
    pub fn enqueue_sync(
        &mut self,
        meta: DmaBuffer,
        in0_len: usize,
        in1_len: usize,
        resp_len: usize,
        waiter: NonNull<SyncWaitBlock>,
    ) -> Result<SyncTicket, (DmaBuffer, VirtioError)> {
        if self.failed {
            return Err((meta, VirtioError::DeviceError));
        }
        // The shape is decided ONCE, here, and carried on the entry; the drain
        // no longer re-derives it from `in1_len > 0`.
        let chain = if in1_len > 0 {
            Chain::Meta2 { in0_len, in1_len }
        } else {
            Chain::Meta1 { in0_len }
        };
        // Validate BEFORE the capacity gate, exactly as the old summed-total
        // check did: a malformed request must report DeviceError even when the
        // queue also happens to be full. The `t <= meta.as_slice().len()` test
        // this replaces now lives in DmaBuffer::span, per span rather than on
        // the sum.
        if in0_len == 0 || resp_len == 0 || resp_len > SYNC_RESP_MAX {
            return Err((meta, VirtioError::DeviceError));
        }
        let Some((reads, count, resp)) = chain.spans(&meta, None, resp_len) else {
            return Err((meta, VirtioError::DeviceError));
        };
        if self.inflight.len() >= MAX_INFLIGHT || self.parked.len() >= PARKED_ENQUEUE_GATE {
            QUEUE_FULL_RETRIES.fetch_add(1, Ordering::Relaxed);
            return Err((meta, VirtioError::QueueFull));
        }
        // SAFETY: the spans are disjoint sub-ranges of `meta`, which the
        // InFlight entry owns until the matching pop_used (moving the DmaBuffer
        // moves the owning struct, not the DMA bytes). The borrows end at `add`.
        let added = unsafe {
            let reads = [reads[0].as_slice(), reads[1].as_slice()];
            self.control.add(&reads[..count], &mut [resp.as_mut_slice()])
        };
        let token = match added {
            Ok(t) => t,
            Err(virtio_drivers::Error::QueueFull) => {
                QUEUE_FULL_RETRIES.fetch_add(1, Ordering::Relaxed);
                return Err((meta, VirtioError::QueueFull));
            }
            Err(_) => {
                self.failed = true;
                return Err((meta, VirtioError::DeviceError));
            }
        };
        if self.control.should_notify() {
            self.transport.notify(CTRL_QUEUE);
        }
        self.inflight.push(InFlight {
            token,
            kind: InFlightKind::Sync {
                waiter: Some(waiter),
            },
            meta,
            chain,
            resp_len,
            venus: None,
        });
        bump_high_water(&INFLIGHT_HIGH_WATER, self.inflight.len());
        Ok(SyncTicket { token })
    }

    /// Enqueue a control command without a blocking waiter.  Completion still
    /// consumes and validates the device response in [`Self::drain_used`], owns
    /// `meta` until then, clears the adapter-owned `completion` gate, and wakes
    /// `wake_event`.  The pointed-to objects must remain live until transport
    /// teardown; the scanout caller uses fields embedded in `AdapterContext`,
    /// whose lifetime encloses the virtio transport.
    pub fn enqueue_async_control(
        &mut self,
        meta: DmaBuffer,
        in0_len: usize,
        resp_len: usize,
        completion: NonNull<AtomicU32>,
        completion_errors: NonNull<AtomicU32>,
        wake_event: NonNull<KEVENT>,
        success_store: Option<(NonNull<AtomicU32>, u32)>,
        resubmit: Option<NonNull<AtomicU32>>,
    ) -> Result<(), (DmaBuffer, VirtioError)> {
        if self.failed {
            return Err((meta, VirtioError::DeviceError));
        }
        let chain = Chain::Meta1 { in0_len };
        // Validated before the capacity gate, as in enqueue_sync.
        if in0_len == 0 || resp_len == 0 || resp_len > SYNC_RESP_MAX {
            return Err((meta, VirtioError::DeviceError));
        }
        let Some((reads, count, resp)) = chain.spans(&meta, None, resp_len) else {
            return Err((meta, VirtioError::DeviceError));
        };
        if self.inflight.len() >= MAX_INFLIGHT || self.parked.len() >= PARKED_ENQUEUE_GATE {
            QUEUE_FULL_RETRIES.fetch_add(1, Ordering::Relaxed);
            return Err((meta, VirtioError::QueueFull));
        }
        // SAFETY: the request/response spans are disjoint inside `meta`, which
        // moves into the InFlight entry and remains device-owned until pop_used.
        let added = unsafe {
            let reads = [reads[0].as_slice(), reads[1].as_slice()];
            self.control.add(&reads[..count], &mut [resp.as_mut_slice()])
        };
        let token = match added {
            Ok(t) => t,
            Err(virtio_drivers::Error::QueueFull) => {
                QUEUE_FULL_RETRIES.fetch_add(1, Ordering::Relaxed);
                return Err((meta, VirtioError::QueueFull));
            }
            Err(_) => {
                self.failed = true;
                return Err((meta, VirtioError::DeviceError));
            }
        };
        self.inflight.push(InFlight {
            token,
            kind: InFlightKind::AsyncControl {
                completion,
                completion_errors,
                wake_event,
                success_store,
                resubmit,
            },
            meta,
            chain,
            resp_len,
            venus: None,
        });
        ASYNC_CTRL_COUNT.fetch_add(1, Ordering::Relaxed);
        bump_high_water(&INFLIGHT_HIGH_WATER, self.inflight.len());
        // Publish token ownership before ringing the device doorbell: a fast
        // host may complete immediately and enter the ISR/DPC on another CPU.
        if self.control.should_notify() {
            self.transport.notify(CTRL_QUEUE);
        }
        Ok(())
    }

    /// Enqueue an ASYNC fenced SUBMIT_3D and return the KMD-assigned wire
    /// fence id. Returns at queue time — completion arrives on the used ring
    /// (interrupt DPC), which signals WAIT_FENCE waiters and advances the WDDM
    /// pending FIFO. `meta` carries `[SUBMIT_3D hdr | ctrl resp]`; `venus` is
    /// the opaque stream (second device-read descriptor — kept split so the
    /// host never mis-parses the submit header as another control command).
    pub fn enqueue_async_submit(
        &mut self,
        ctx_id: u32,
        ring_idx: u32,
        meta: DmaBuffer,
        venus: DmaBuffer,
        venus_len: usize,
    ) -> Result<u64, (DmaBuffer, DmaBuffer, VirtioError)> {
        self.enqueue_submit_inner(ctx_id, ring_idx, meta, venus, venus_len, None)
    }

    /// Enqueue the scan-out copy: an ASYNC fenced SUBMIT_3D on ring 1 carrying
    /// the notification target whose completion publishes the displayed primary
    /// and clears the programming gate.
    ///
    /// The ring is NOT a parameter. It used to be, independently of the notify,
    /// while the drain only honoured a notify on ring 1 — so
    /// `enqueue_async_submit(ctx, 0, .., Some(notify))` compiled, completed
    /// through the `ring_idx != 1` path, and silently discarded the notify with
    /// no counter and no error. Because that drain is the ONLY clear of
    /// `vidpn_programming` on the copied-primary path, the gate stayed at 1 and
    /// every further CRTC_VSYNC was suppressed for the rest of the boot. Making
    /// the ring an implicit property of this entry point takes the
    /// `(ring, notify)` mismatch out of the type space entirely.
    pub fn enqueue_scanout_submit(
        &mut self,
        ctx_id: u32,
        meta: DmaBuffer,
        venus: DmaBuffer,
        venus_len: usize,
        notify: ScanoutNotify,
    ) -> Result<u64, (DmaBuffer, DmaBuffer, VirtioError)> {
        self.enqueue_submit_inner(ctx_id, SCANOUT_RING_IDX, meta, venus, venus_len, Some(notify))
    }

    /// Shared body of the two entry points above. Private: the notify/ring
    /// pairing is theirs to decide, not a caller's.
    fn enqueue_submit_inner(
        &mut self,
        ctx_id: u32,
        ring_idx: u32,
        mut meta: DmaBuffer,
        venus: DmaBuffer,
        venus_len: usize,
        scanout_notify: Option<ScanoutNotify>,
    ) -> Result<u64, (DmaBuffer, DmaBuffer, VirtioError)> {
        let hdr_len = core::mem::size_of::<VirtioGpuCmdSubmit>();
        let resp_len = core::mem::size_of::<VirtioGpuCtrlHdr>();
        if self.failed {
            return Err((meta, venus, VirtioError::DeviceError));
        }
        if venus_len == 0
            || venus_len > venus.as_slice().len()
            || hdr_len + resp_len > meta.as_slice().len()
        {
            return Err((meta, venus, VirtioError::DeviceError));
        }
        if self.inflight.len() >= MAX_INFLIGHT || self.parked.len() >= PARKED_ENQUEUE_GATE {
            QUEUE_FULL_RETRIES.fetch_add(1, Ordering::Relaxed);
            return Err((meta, venus, VirtioError::QueueFull));
        }
        let fence_id = self.next_wire_fence;
        let mut cmd = VirtioGpuCmdSubmit::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_SUBMIT_3D;
        cmd.hdr.flags = VIRTIO_GPU_FLAG_FENCE;
        cmd.hdr.fence_id = fence_id;
        cmd.hdr.ctx_id = ctx_id;
        if ring_idx != 0 {
            cmd.hdr.flags |= VIRTIO_GPU_FLAG_INFO_RING_IDX;
            cmd.hdr.ring_idx = ring_idx.min(u8::MAX as u32) as u8;
        }
        cmd.size = venus_len as u32;
        meta.as_mut_slice()[..hdr_len].copy_from_slice(bytemuck::bytes_of(&cmd));

        let chain = Chain::MetaPlusVenus { hdr_len, venus_len };
        let Some((reads, count, resp)) = chain.spans(&meta, Some(&venus), resp_len) else {
            return Err((meta, venus, VirtioError::DeviceError));
        };
        // SAFETY: spans live inside `meta`/`venus`, owned by the InFlight entry
        // until pop_used; the borrows end at `add`.
        let added = unsafe {
            let reads = [reads[0].as_slice(), reads[1].as_slice()];
            self.control.add(&reads[..count], &mut [resp.as_mut_slice()])
        };
        let token = match added {
            Ok(t) => t,
            Err(virtio_drivers::Error::QueueFull) => {
                QUEUE_FULL_RETRIES.fetch_add(1, Ordering::Relaxed);
                return Err((meta, venus, VirtioError::QueueFull));
            }
            Err(_) => {
                self.failed = true;
                return Err((meta, venus, VirtioError::DeviceError));
            }
        };
        self.next_wire_fence += 1;
        let ring = cmd.hdr.ring_idx;
        self.inflight.push(InFlight {
            token,
            kind: InFlightKind::AsyncVenus {
                fence_id,
                ring_idx: ring,
                scanout_notify,
            },
            meta,
            chain,
            resp_len,
            venus: Some(venus),
        });
        ASYNC_SUBMIT_COUNT.fetch_add(1, Ordering::Relaxed);
        if ring != 0 {
            RING_SUBMIT_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        bump_high_water(&INFLIGHT_HIGH_WATER, self.inflight.len());
        // Publish token/fence/callback ownership before the device can race a
        // completion into the ISR/DPC on another CPU.
        if self.control.should_notify() {
            self.transport.notify(CTRL_QUEUE);
        }
        Ok(fence_id)
    }

    /// Set the ring-corruption latch and fail every in-flight entry exactly
    /// once. Call at the moment the latch is set, never later, and always under
    /// the device spinlock.
    ///
    /// Before this, latching `failed` made `drain_used` return immediately and
    /// left every entry in `inflight` forever. `async_retired_up_to` then
    /// reported false for every watermark above those stuck ids,
    /// `note_wddm_submission` never returned "signal now" and `take_ready_wddm`
    /// never popped, so DMA_COMPLETED was never delivered again. ResetFromTimeout
    /// cleared the FIFO but neither the latch nor the stuck entries, so dxgkrnl
    /// resubmitted with fresh ids into the same wedge: a TDR loop. And the
    /// `AsyncControl` entries that own `scanout_bind_inflight` /
    /// `scanout_flush_inflight` were abandoned with their completion gates still
    /// set, so `queue_active_scanout_refresh` returned Busy forever and the HPD
    /// worker spun its 4 ms lost-interrupt poll for the rest of the boot.
    ///
    /// Everything here mirrors the success path's ordering exactly, because a
    /// mistake in the Sync-waiter sequence is a use-after-free of a stack block.
    fn latch_failed_and_fail_inflight(&mut self) {
        self.failed = true;
        while let Some(entry) = self.inflight.pop() {
            match entry.kind {
                InFlightKind::Sync { waiter } => {
                    if let Some(block) = waiter {
                        // No response is copied on purpose: `SyncWaitBlock::new_zeroed`
                        // zeroes `resp`, and `resp_is_ok(0)` is false, so every
                        // waiter observes failure rather than a stale success.
                        // SAFETY: a registered block stays valid until its owner
                        // deregisters under this same lock, which has not happened
                        // (waiter is still Some). `done` (Release) BEFORE
                        // KeSetEvent, both inside the critical section: after a
                        // release the block's stack frame may be reused at once.
                        unsafe {
                            let b = block.as_ptr();
                            (*b).done.store(true, Ordering::Release);
                            KeSetEvent(&mut (*b).event, IO_NO_INCREMENT, 0);
                        }
                    }
                }
                InFlightKind::AsyncControl {
                    completion,
                    completion_errors,
                    wake_event,
                    ..
                } => {
                    // Clear the gate and wake the worker, so the display's
                    // coalescing gates unstick instead of reading Busy forever.
                    // No `success_store`, no `resubmit`: nothing succeeded.
                    // SAFETY: all three name stable AdapterContext fields whose
                    // lifetime encloses this transport entry.
                    unsafe {
                        completion_errors.as_ref().fetch_add(1, Ordering::Relaxed);
                        completion.as_ref().store(0, Ordering::Release);
                        KeSetEvent(wake_event.as_ptr(), IO_NO_INCREMENT, 0);
                    }
                }
                InFlightKind::AsyncVenus { scanout_notify, .. } => {
                    if let Some(notify) = scanout_notify {
                        // Publish nothing as displayed - the copy did not happen -
                        // but DO clear the programming gate, or the VSync
                        // heartbeat stops exactly as in R202. Ticketed: if a
                        // newer interval was raised meanwhile, that gate is not
                        // ours to lower.
                        // SAFETY: stable AdapterContext fields; the adapter owns
                        // this transport and outlives every in-flight entry.
                        unsafe {
                            crate::adapter::clear_programming_gate(
                                notify.programming.as_ref(),
                                notify.ticket,
                            );
                            KeSetEvent(notify.event.as_ptr(), IO_NO_INCREMENT, 0);
                        }
                    }
                }
            }
            // Park, never free: the host may still be DMAing into these buffers,
            // and DmaBuffer frees are PASSIVE-only. Same policy as the success
            // path.
            if self.parked.len() < MAX_PARKED {
                self.parked.push(entry);
                bump_high_water(&PARKED_HIGH_WATER, self.parked.len());
            } else {
                PARKED_LEAKS.fetch_add(1, Ordering::Relaxed);
                core::mem::forget(entry);
            }
        }

        // No wire fence can ever retire now, so release every parked waiter and
        // usermode event rather than leaving them blocked on a dead transport.
        while let Some(w) = self.fence_waiters.pop() {
            // SAFETY: registered blocks stay valid until deregistration, which
            // happens under this same lock.
            unsafe {
                let b = w.block.as_ptr();
                (*b).done.store(true, Ordering::Release);
                KeSetEvent(&mut (*b).event, IO_NO_INCREMENT, 0);
            }
        }
        while let Some(e) = self.fence_events.pop() {
            // SAFETY: the entry holds an object reference taken by the escape
            // handler. The deref MUST be deferred: dropping the last reference
            // with a plain deref at DISPATCH would run the object's PASSIVE-only
            // deletion.
            unsafe {
                KeSetEvent(e.event.as_ptr(), IO_NO_INCREMENT, 0);
                ObDereferenceObjectDeferDelete(e.event.as_ptr() as PVOID);
            }
            FENCE_EVENT_SIGNALS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Drain every completed entry off the used ring: pop the descriptor chain
    /// (token-matched), signal sync/fence waiters, and park the entry for a
    /// PASSIVE reap. The ONLY used-ring consumer (interrupt DPC + opportunistic
    /// callers under the same spinlock).
    pub fn drain_used(&mut self) {
        if self.failed {
            return;
        }
        loop {
            let Some(token) = self.control.peek_used() else {
                return;
            };
            let Some(idx) = self.inflight.iter().position(|e| e.token == token) else {
                // A completion we do not track: the ring state is corrupt.
                DRAIN_BAD_TOKEN.fetch_add(1, Ordering::Relaxed);
                self.latch_failed_and_fail_inflight();
                return;
            };
            // Rebuild the spans through the SAME producer `add` used, so the
            // two lists cannot drift. The result is copied out so no borrow of
            // `self.inflight` is held across the `self.control` call.
            let (spans, resp_len) = {
                let e = &self.inflight[idx];
                (e.chain.spans(&e.meta, e.venus.as_ref(), e.resp_len), e.resp_len)
            };
            // The entry's own spans were proved at enqueue and nothing has
            // resized the buffers since, so this cannot fail; treat it as a
            // corrupt entry rather than assuming.
            let Some((reads, count, resp)) = spans else {
                DRAIN_BAD_TOKEN.fetch_add(1, Ordering::Relaxed);
                self.latch_failed_and_fail_inflight();
                return;
            };
            // SAFETY: exactly the spans `add` was called with; the entry still
            // owns both buffers.
            let popped = unsafe {
                let read_slices = [reads[0].as_slice(), reads[1].as_slice()];
                self.control
                    .pop_used(token, &read_slices[..count], &mut [resp.as_mut_slice()])
            };
            if popped.is_err() {
                self.latch_failed_and_fail_inflight();
                return;
            }
            let entry = self.inflight.swap_remove(idx);
            let resp_base = {
                // SAFETY: the resp span is within the entry-owned meta buffer.
                unsafe { resp.as_slice() }.as_ptr()
            };
            // First u32 of the device-written response = VIRTIO_GPU_RESP_*.
            // SAFETY: as above; unaligned because the offset is command-shaped.
            let resp_type = unsafe { core::ptr::read_unaligned(resp_base as *const u32) };
            match entry.kind {
                InFlightKind::Sync { waiter } => {
                    if let Some(block) = waiter {
                        // SAFETY: a registered SyncWaitBlock stays valid until
                        // its owner deregisters under this same lock
                        // (`abandon_sync`) — which has not happened (waiter is
                        // still Some). Response copied BEFORE the Release store
                        // on `done`; KeSetEvent is DISPATCH-safe (Wait=FALSE).
                        // Signaling under the lock is required: after a release,
                        // the block's stack frame may be reused immediately.
                        unsafe {
                            let b = block.as_ptr();
                            let n = resp_len.min(SYNC_RESP_MAX);
                            core::ptr::copy_nonoverlapping(
                                resp_base,
                                (*b).resp.get() as *mut u8,
                                n,
                            );
                            (*b).done.store(true, Ordering::Release);
                            KeSetEvent(&mut (*b).event, IO_NO_INCREMENT, 0);
                        }
                    }
                }
                InFlightKind::AsyncControl {
                    completion,
                    completion_errors,
                    wake_event,
                    success_store,
                    resubmit,
                } => {
                    ASYNC_CTRL_COMPLETE_COUNT.fetch_add(1, Ordering::Relaxed);
                    let response_ok = resp_is_ok(resp_type);
                    if !response_ok {
                        ASYNC_CTRL_RESP_ERRORS.fetch_add(1, Ordering::Relaxed);
                        // SAFETY: adapter-owned atomic; see enqueue contract.
                        unsafe { completion_errors.as_ref() }.fetch_add(1, Ordering::Relaxed);
                    } else if let Some((target, value)) = success_store {
                        // Publish which scanout the host accepted before the
                        // worker consumes the follow-up dirty edge.
                        unsafe { target.as_ref() }.store(value, Ordering::Release);
                    }
                    // Publish completion before waking the coalescing worker.
                    // SAFETY: both pointers refer to stable AdapterContext
                    // fields whose lifetime encloses this transport entry.
                    unsafe {
                        // A rejected SET_SCANOUT_BLOB must not become a
                        // self-sustaining retry loop. New exact-primary/dirty
                        // publication is the only retry trigger; successful
                        // binds re-arm once to issue their first flush.
                        if response_ok {
                            if let Some(pending) = resubmit {
                                pending.as_ref().store(1, Ordering::Release);
                            }
                        }
                        completion.as_ref().store(0, Ordering::Release);
                        KeSetEvent(wake_event.as_ptr(), IO_NO_INCREMENT, 0);
                    }
                }
                InFlightKind::AsyncVenus {
                    fence_id,
                    ring_idx,
                    scanout_notify,
                } => {
                    ASYNC_COMPLETE_COUNT.fetch_add(1, Ordering::Relaxed);
                    if ring_idx != 0 {
                        RING_COMPLETE_COUNT.fetch_add(1, Ordering::Relaxed);
                    }
                    let response_ok = resp_is_ok(resp_type);
                    if !response_ok {
                        ASYNC_RESP_ERRORS.fetch_add(1, Ordering::Relaxed);
                    }
                    // ring_idx=1 is the GPU-completion domain. Queue the host
                    // display refresh only after the copy has really completed;
                    // a decode-level or failed submit must never publish pixels.
                    // A notify can now only exist on this ring
                    // (`enqueue_scanout_submit`), so the test is a belt-and-braces
                    // check rather than the sole guard it used to be.
                    if ring_idx == SCANOUT_RING_IDX as u8 {
                        if let Some(notify) = scanout_notify {
                            // SAFETY: both pointers name stable AdapterContext
                            // fields; the adapter owns this transport and outlives
                            // every in-flight entry.
                            unsafe {
                                if response_ok {
                                    notify
                                        .displayed_primary
                                        .as_ref()
                                        .store(notify.primary_address, Ordering::Release);
                                    notify.pending.as_ref().store(1, Ordering::Release);
                                }
                                // Ticketed clear, unconditional on response_ok
                                // exactly as before: a failed copy must still
                                // release OUR interval or VSync stops. What it
                                // must NOT do is release a NEWER interval that a
                                // second SetVidPnSourceAddress raised while this
                                // copy was outstanding — that is the stale clear
                                // this ticket exists to reject.
                                crate::adapter::clear_programming_gate(
                                    notify.programming.as_ref(),
                                    notify.ticket,
                                );
                                KeSetEvent(notify.event.as_ptr(), IO_NO_INCREMENT, 0);
                            }
                        }
                    }
                    // Wake every waiter registered on this wire fence.
                    let mut j = 0;
                    while j < self.fence_waiters.len() {
                        if self.fence_waiters[j].fence_id == fence_id {
                            let w = self.fence_waiters.swap_remove(j);
                            // SAFETY: registered blocks stay valid until
                            // deregistration (`fence_wait_cancel`), which removes
                            // them from this list under the same lock.
                            unsafe {
                                let b = w.block.as_ptr();
                                (*b).done.store(true, Ordering::Release);
                                KeSetEvent(&mut (*b).event, IO_NO_INCREMENT, 0);
                            }
                        } else {
                            j += 1;
                        }
                    }
                    // Signal + consume every usermode fence-event registration
                    // on this wire fence (one-shot). Runs at DISPATCH under the
                    // device spinlock: KeSetEvent (Wait=FALSE) is legal, and the
                    // deref MUST be ObDereferenceObjectDeferDelete — dropping
                    // the LAST reference with a plain deref at DISPATCH would
                    // run the object's PASSIVE-only deletion (the registering
                    // process may have exited and closed its handle).
                    let mut j = 0;
                    while j < self.fence_events.len() {
                        if self.fence_events[j].fence_id == fence_id {
                            let e = self.fence_events.swap_remove(j);
                            // SAFETY: the entry holds an object reference taken
                            // by the escape handler, so `event` is a live KEVENT
                            // regardless of the registering process's fate.
                            unsafe {
                                KeSetEvent(e.event.as_ptr(), IO_NO_INCREMENT, 0);
                                ObDereferenceObjectDeferDelete(e.event.as_ptr() as PVOID);
                            }
                            FENCE_EVENT_SIGNALS.fetch_add(1, Ordering::Relaxed);
                        } else {
                            j += 1;
                        }
                    }
                }
            }
            // Park the entry for a PASSIVE reap (DmaBuffer frees are
            // PASSIVE-only).
            if self.parked.len() < MAX_PARKED {
                self.parked.push(entry);
                bump_high_water(&PARKED_HIGH_WATER, self.parked.len());
            } else {
                // Unreachable given PARKED_ENQUEUE_GATE; never reallocate or
                // free under the spinlock — leak loudly instead.
                PARKED_LEAKS.fetch_add(1, Ordering::Relaxed);
                core::mem::forget(entry);
            }
        }
    }

    /// Number of completed entries awaiting a PASSIVE reap.
    pub fn parked_len(&self) -> usize {
        self.parked.len()
    }

    /// Begin one PASSIVE reap by swapping in the empty pre-reserved parked
    /// vector and lending the pre-reserved DMA-buffer scratch vector. A second
    /// caller returns None and leaves the active reaper to finish.
    pub fn begin_parked_reap(&mut self) -> Option<(Vec<InFlight>, Vec<DmaBuffer>)> {
        if self.reap_in_progress || self.parked.is_empty() {
            return None;
        }
        self.reap_in_progress = true;
        let fresh = core::mem::take(&mut self.parked_spare);
        debug_assert!(fresh.capacity() >= MAX_PARKED);
        let dead = core::mem::replace(&mut self.parked, fresh);
        let buffers = core::mem::take(&mut self.reap_buffers_spare);
        debug_assert!(buffers.capacity() >= 2 * MAX_PARKED);
        Some((dead, buffers))
    }

    /// Return the emptied pre-reserved reap vectors after excess DMA buffers
    /// were dropped at PASSIVE_LEVEL.
    pub fn finish_parked_reap(&mut self, mut entries: Vec<InFlight>, mut buffers: Vec<DmaBuffer>) {
        // These were debug_assert-only, i.e. absent from the release driver
        // (kmd_render sets no debug-assertions). They guard the capacity the
        // `parked.push` path relies on to never reallocate under the spinlock,
        // so enforce them for real: clear, and replace any vector whose capacity
        // fell below its reserve with a freshly reserved one. Both allocations
        // happen HERE, at PASSIVE, never under the lock.
        entries.clear();
        buffers.clear();
        if entries.capacity() < MAX_PARKED {
            entries = Vec::with_capacity(MAX_PARKED);
        }
        if buffers.capacity() < 2 * MAX_PARKED {
            buffers = Vec::with_capacity(2 * MAX_PARKED);
        }
        self.parked_spare = entries;
        self.reap_buffers_spare = buffers;
        self.reap_in_progress = false;
    }

    /// Undo [`Self::begin_parked_reap`] on a failure path, restoring both spares
    /// and clearing the in-progress flag.
    ///
    /// Without this, an early return between begin and finish stranded
    /// `reap_in_progress` at true AND left both pre-reserved spares taken, so
    /// reaping was permanently disabled and every later enqueue hit the
    /// `PARKED_ENQUEUE_GATE` refusal. Latent today only because the sole
    /// reachable early return is a concurrent StopDevice, after which the whole
    /// `VirtioGpu` is replaced - but that is a property of today's callers, not
    /// of the protocol.
    pub fn abort_parked_reap(&mut self, entries: Vec<InFlight>, buffers: Vec<DmaBuffer>) {
        REAP_ABANDONED.fetch_add(1, Ordering::Relaxed);
        self.finish_parked_reap(entries, buffers);
    }

    /// Take one already-allocated DMA buffer whose page capacity covers `len`.
    /// The caller allocates a new buffer at PASSIVE only when this returns None.
    pub fn take_dma_buffer(&mut self, len: usize) -> Option<DmaBuffer> {
        let Some((idx, _)) = self
            .dma_pool
            .iter()
            .enumerate()
            .filter(|(_, buf)| buf.capacity() >= len)
            .min_by_key(|(_, buf)| buf.capacity())
        else {
            DMA_POOL_MISSES.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let mut buf = self.dma_pool.swap_remove(idx);
        self.dma_pool_bytes = self.dma_pool_bytes.saturating_sub(buf.capacity());
        DMA_POOL_CACHED_BYTES.store(self.dma_pool_bytes as u32, Ordering::Relaxed);
        if !buf.reset(len) {
            DMA_POOL_MISSES.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        DMA_POOL_HITS.fetch_add(1, Ordering::Relaxed);
        Some(buf)
    }

    /// Move eligible completed buffers into the bounded pool without allocation.
    /// Any excess remains in `buffers` and is returned for PASSIVE-level drop.
    pub fn recycle_dma_buffers(&mut self, mut buffers: Vec<DmaBuffer>) -> Vec<DmaBuffer> {
        let mut i = 0;
        while i < buffers.len() {
            let capacity = buffers[i].capacity();
            let eligible = capacity <= MAX_DMA_POOL_BUFFER_BYTES
                && self.dma_pool.len() < MAX_DMA_POOL
                && self.dma_pool_bytes.saturating_add(capacity) <= MAX_DMA_POOL_BYTES;
            if !eligible {
                DMA_POOL_DROPS.fetch_add(1, Ordering::Relaxed);
                i += 1;
                continue;
            }
            let buf = buffers.swap_remove(i);
            self.dma_pool_bytes += capacity;
            self.dma_pool.push(buf);
        }
        DMA_POOL_CACHED_BYTES.store(self.dma_pool_bytes as u32, Ordering::Relaxed);
        buffers
    }

    /// Abandon a timed-out synchronous entry: detach its waiter so the eventual
    /// completion signals nobody (the entry itself is reaped when it completes).
    /// Returns `true` if the entry had ALREADY completed — the wait raced the
    /// drain and the caller should treat it as success.
    pub fn abandon_sync(&mut self, ticket: SyncTicket, block: NonNull<SyncWaitBlock>) -> SyncOutcome {
        for e in self.inflight.iter_mut() {
            if e.token != ticket.token {
                continue;
            }
            if let InFlightKind::Sync { waiter } = &mut e.kind {
                if *waiter == Some(block) {
                    *waiter = None;
                    return SyncOutcome::Abandoned;
                }
                // The token is ours but the waiter is a DIFFERENT block, or the
                // entry is not a Sync at all. Either way this waiter's response
                // buffer was never written.
                return SyncOutcome::NotOurs;
            }
            return SyncOutcome::NotOurs;
        }
        SyncOutcome::AlreadyCompleted
    }

    // ── Wire-fence table (WAIT_FENCE) ────────────────────────────────────────

    /// Prepare a wait on wire fence `fence_id`, registering `block` if the
    /// fence is still in flight. Runs under the device spinlock — the
    /// in-flight check and the registration are atomic with respect to
    /// [`Self::drain_used`], so a completion can never fall between them.
    ///
    /// Completion predicate (System-class phase4e model): wire ids are
    /// assigned by this transport, monotonic and never reused, and every
    /// assigned id lives in `inflight` until its used-ring completion — so
    /// `id < next_wire_fence && not in-flight` ⇒ complete.
    pub fn fence_wait_prepare(
        &mut self,
        fence_id: u64,
        block: NonNull<SyncWaitBlock>,
    ) -> FenceWaitPrep {
        // A failed transport can never retire a fence, so parking a PASSIVE
        // waiter against one is a guaranteed timeout at best.
        if self.failed || fence_id == 0 || fence_id >= self.next_wire_fence {
            return FenceWaitPrep::Invalid;
        }
        let in_flight = self.inflight.iter().any(|e| match e.kind {
            InFlightKind::AsyncVenus { fence_id: f, .. } => f == fence_id,
            _ => false,
        });
        if !in_flight {
            return FenceWaitPrep::Complete;
        }
        if self.fence_waiters.len() >= MAX_FENCE_WAITERS {
            return FenceWaitPrep::TableFull;
        }
        self.fence_waiters.push(FenceWaiter { fence_id, block });
        FENCE_WAIT_REGISTERED.fetch_add(1, Ordering::Relaxed);
        FenceWaitPrep::Registered
    }

    /// Deregister a timed-out fence waiter. Returns `true` if the fence had
    /// ALREADY completed (the drain signaled + removed the waiter first).
    pub fn fence_wait_cancel(&mut self, block: NonNull<SyncWaitBlock>) -> bool {
        if let Some(i) = self.fence_waiters.iter().position(|w| w.block == block) {
            self.fence_waiters.swap_remove(i);
            false
        } else {
            true
        }
    }

    // ── Fence-event table (REGISTER_FENCE_EVENT, KMD 22.22.54) ──────────────

    /// Park `event` for one-shot signaling when wire fence `fence_id` retires.
    /// Runs under the device spinlock — the completion check and the insert
    /// are atomic against [`Self::drain_used`], so a wakeup can never be lost
    /// (same predicate as [`Self::fence_wait_prepare`]: assigned ids live in
    /// `inflight` until their used-ring completion).
    ///
    /// Ownership: on `Registered` the TABLE owns the caller's object
    /// reference (released by the drain / unregister / teardown). On every
    /// other outcome the caller still owns it and must deref.
    pub fn fence_event_register(&mut self, fence_id: u64, event: NonNull<KEVENT>) -> FenceEventReg {
        // As in fence_wait_prepare. `Invalid` leaves the object reference with
        // the caller, per the ownership contract documented on this function.
        if self.failed || fence_id == 0 || fence_id >= self.next_wire_fence {
            return FenceEventReg::Invalid;
        }
        let in_flight = self.inflight.iter().any(|e| match e.kind {
            InFlightKind::AsyncVenus { fence_id: f, .. } => f == fence_id,
            _ => false,
        });
        if !in_flight {
            FENCE_EVENT_ALREADY_COMPLETE.fetch_add(1, Ordering::Relaxed);
            return FenceEventReg::AlreadyComplete;
        }
        if self
            .fence_events
            .iter()
            .any(|e| e.fence_id == fence_id && e.event == event)
        {
            FENCE_EVENT_DUP_REJECTS.fetch_add(1, Ordering::Relaxed);
            return FenceEventReg::Duplicate;
        }
        if self.fence_events.len() >= MAX_FENCE_EVENTS {
            FENCE_EVENT_OVERFLOWS.fetch_add(1, Ordering::Relaxed);
            return FenceEventReg::TableFull;
        }
        self.fence_events.push(FenceEventEntry { fence_id, event });
        bump_high_water(&FENCE_EVENT_HIGH_WATER, self.fence_events.len());
        FENCE_EVENT_REGISTERS.fetch_add(1, Ordering::Relaxed);
        FenceEventReg::Registered
    }

    /// Remove a parked (fence_id, event) registration. Returns `true` if it
    /// was found and removed — the TABLE's object reference transfers back to
    /// the caller (who must deref it); `false` = no such entry (the drain
    /// consumed it — the event was signaled — or it was never parked).
    pub fn fence_event_unregister(&mut self, fence_id: u64, event: NonNull<KEVENT>) -> bool {
        if let Some(i) = self
            .fence_events
            .iter()
            .position(|e| e.fence_id == fence_id && e.event == event)
        {
            self.fence_events.swap_remove(i);
            FENCE_EVENT_CANCELS.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Current fence-event table occupancy (QUERY_STATS v2).
    pub fn fence_events_live(&self) -> u32 {
        self.fence_events.len() as u32
    }

    // ── WDDM pending-fence FIFO (SubmitCommand → DPC completion) ─────────────

    /// Capture the exact Venus ordering boundary for a scanout dirty marker.
    ///
    /// The `NotifyOrdered` token is mintable only inside
    /// `WddmNotifyGuard::with_virtio`, so reaching this method proves that THIS
    /// adapter's `wddm_notify` lock was taken before its `virtio` lock and is
    /// still held. (The previous `&WddmNotifyGuard` parameter proved only that
    /// SOME adapter's notify lock was held somewhere — it carried no
    /// relationship to the `&mut VirtioGpu` being mutated.) Returns true when
    /// all preceding GPU work has already retired and the caller may refresh
    /// immediately.
    pub fn note_scanout_refresh(
        &mut self,
        _order: &crate::adapter::NotifyOrdered<'_>,
    ) -> bool {
        let watermark = self.next_wire_fence;
        if self.async_retired_up_to(watermark, true) {
            self.scanout_refresh_watermark = None;
            true
        } else {
            self.scanout_refresh_watermark = Some(watermark);
            false
        }
    }

    /// Consume a completion-ordered scanout marker after the used-ring drain.
    /// Must be called under the same statically witnessed notification lock as
    /// [`Self::note_scanout_refresh`].
    pub fn take_ready_scanout_refresh(
        &mut self,
        _order: &crate::adapter::NotifyOrdered<'_>,
    ) -> bool {
        let Some(watermark) = self.scanout_refresh_watermark else {
            return false;
        };
        if !self.async_retired_up_to(watermark, true) {
            return false;
        }
        self.scanout_refresh_watermark = None;
        true
    }

    /// Whether every RING-0 async wire fence `< watermark` has retired.
    ///
    /// Ring-0 only: the WDDM pending FIFO exists to order DMA_COMPLETED behind
    /// the venus escape traffic queued before it, and ring-0 fences retire at
    /// host DECODE — the ordering domain that contract was built on. ring >= 1
    /// fences (WS1 #4) retire at host GPU COMPLETION and stay in flight for
    /// the full GPU-work duration; counting them here would couple every WDDM
    /// DMA fence (GDI/paging pacing) to unrelated multi-ms GPU work. Consumers
    /// that need GPU completion wait on those fences explicitly (WAIT_FENCE).
    fn async_retired_up_to(&self, watermark: u64, wait_gpu: bool) -> bool {
        watermark == 0
            || !self.inflight.iter().any(|e| match e.kind {
                InFlightKind::AsyncVenus {
                    fence_id, ring_idx, ..
                } => fence_id < watermark && (wait_gpu || ring_idx == 0),
                _ => false,
            })
    }

    /// Record a WDDM submission (`SubmissionFenceId = fence`). Returns `true`
    /// if the caller should signal DMA_COMPLETED immediately (no venus work
    /// outstanding, nothing queued ahead); otherwise the interrupt DPC
    /// completes it via [`Self::take_ready_wddm`] once every async submission
    /// queued before it has retired. Paging buffers carry no venus work
    /// (watermark 0) but still queue FIFO behind earlier render submissions —
    /// SubmissionFenceIds are watermarks to dxgkrnl and must complete
    /// monotonically.
    pub fn note_wddm_submission(
        &mut self,
        _order: &crate::adapter::NotifyOrdered<'_>,
        fence: u32,
        paging: bool,
        gpu_completion_fence: Option<u64>,
        refresh_scanout: bool,
    ) -> bool {
        if self.failed {
            // Nothing will ever retire, so queueing this fence guarantees a TDR.
            // Signal it now - and clear the FIFO in the SAME critical section:
            // dxgkrnl requires monotonic SubmissionFenceId completion, so
            // signalling the newest fence while older ones stay queued would
            // break the invariant.
            WDDM_SIGNAL_AFTER_FAILURE.fetch_add(1, Ordering::Relaxed);
            self.wddm_pending.clear();
            return true;
        }
        let wait_gpu = gpu_completion_fence.is_some();
        let watermark = if paging {
            0
        } else if let Some(gpu_fence_id) = gpu_completion_fence {
            // async_retired_up_to uses an exclusive watermark. A marker written
            // by Present names the exact ring-1 fence that owns its copy.
            if gpu_fence_id != 0 && gpu_fence_id < self.next_wire_fence {
                gpu_fence_id.saturating_add(1)
            } else {
                // A malformed/stale private marker must not manufacture an
                // impossible future dependency. Conservatively gate on all
                // work actually enqueued before this WDDM submission.
                self.next_wire_fence
            }
        } else {
            self.next_wire_fence
        };
        if self.wddm_pending.is_empty() && self.async_retired_up_to(watermark, wait_gpu) {
            return true;
        }
        if self.wddm_pending.len() >= MAX_WDDM_PENDING {
            // Degrade to the old immediate model for this fence — signaling the
            // newest (monotonically largest) fence implicitly completes the
            // queued older ones, so drop them too. Loud, counted, and
            // practically unreachable (VidSch queues far fewer than 256).
            WDDM_PENDING_OVERFLOWS.fetch_add(1, Ordering::Relaxed);
            self.wddm_pending.clear();
            return true;
        }
        self.wddm_pending.push_back(WddmPending {
            fence,
            watermark,
            wait_gpu,
            refresh_scanout,
        });
        false
    }

    /// Pop every head-of-FIFO WDDM submission whose venus watermark has been
    /// reached, up to `out.len()` of them. The DPC signals DMA_COMPLETED for
    /// each, in order, OUTSIDE the device spinlock.
    pub fn take_one_ready_wddm(
        &mut self,
        _order: &crate::adapter::NotifyOrdered<'_>,
    ) -> Option<WddmReady> {
        let (watermark, wait_gpu) = {
            let head = self.wddm_pending.front()?;
            (head.watermark, head.wait_gpu)
        };
        if !self.async_retired_up_to(watermark, wait_gpu) {
            return None;
        }
        let pending = self.wddm_pending.pop_front()?;
        WDDM_FENCE_FROM_DPC.fetch_add(1, Ordering::Relaxed);
        Some(WddmReady { pending })
    }

    /// Put a popped-but-undelivered submission back at the head of the FIFO.
    ///
    /// dxgkrnl requires monotonic SubmissionFenceId completion, so the entry
    /// must go back where it came from — `push_front` of the entry just popped
    /// is the only correct form. It also cannot exceed the
    /// `VecDeque::with_capacity(MAX_WDDM_PENDING)` reserve, so nothing
    /// reallocates under the device spinlock.
    pub fn requeue_wddm_front(
        &mut self,
        _order: &crate::adapter::NotifyOrdered<'_>,
        ready: WddmReady,
    ) {
        self.wddm_pending.push_front(ready.pending);
    }

    /// Preemption: drop every pending WDDM submission (dxgkrnl resubmits the
    /// unfinished DMA buffers with fresh fence ids after the preempt completes;
    /// the underlying venus work keeps executing host-side). Returns the count
    /// dropped.
    pub fn preempt_flush(&mut self, _order: &crate::adapter::NotifyOrdered<'_>) -> u32 {
        let n = self.wddm_pending.len() as u32;
        self.wddm_pending.clear();
        n
    }

    // ── Table helpers (Gate 2 → C3/M3.4 phased flows) ────────────────────────
    //
    // The control round-trips themselves live in `virtio::ctrl` (PASSIVE
    // waits); these lock-context helpers keep the bounded tables consistent
    // across the multi-phase flows. Reservation counters guarantee `push`
    // never exceeds the capacity reserved at init (no realloc under the
    // spinlock), even with concurrent multi-phase creates.

    /// Allocate a fresh guest context id (namespace owned by the KMD).
    pub fn alloc_ctx_id(&self) -> u32 {
        self.next_ctx_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Allocate a fresh guest resource id (namespace owned by the KMD).
    pub fn alloc_resource_id(&self) -> u32 {
        self.next_resource_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Reserve a context tracking slot for an in-flight CTX_CREATE.
    ///
    /// TRACKING IS MANDATORY, the same rule `reserve_resource_slot` already
    /// applies to resources: refuse the create when the table is full rather
    /// than creating a context that works but is untracked. Tracking used to be
    /// best-effort, which had two consequences — an untracked context is never
    /// reclaimed at device teardown, and (the reason this changed) an ownership
    /// check against a best-effort table would have to trust an unknown id,
    /// which is not a check at all. With reserve-then-commit, "live but
    /// untracked" cannot exist, so `resolve_owned_ctx` is authoritative.
    ///
    /// Same-boot evidence for the safety of refusing: contexts_live = 9 against
    /// MAX_CONTEXTS = 1024 with context_full_drops = 0 on a full desktop
    /// (escape_owner_probe, 2026-07-26), so no live workload approaches the cap.
    pub fn reserve_context_slot(&mut self) -> bool {
        if self.contexts.len() + self.contexts_reserved >= MAX_CONTEXTS {
            // Keeps its name and its QUERY_STATS field; the event it counts is
            // now "CTX_CREATE refused because the table is full" rather than
            // "tracking silently dropped".
            CONTEXT_FULL_DROPS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.contexts_reserved += 1;
        true
    }

    /// Commit a reserved context slot once the host has created the context.
    pub fn commit_context(&mut self, owner: Option<DeviceOwner>, ctx_id: u32) {
        self.contexts_reserved = self.contexts_reserved.saturating_sub(1);
        self.contexts.push(ContextSlot { owner, ctx_id });
    }

    /// Release a reserved context slot after a failed create.
    pub fn cancel_context_reservation(&mut self) {
        self.contexts_reserved = self.contexts_reserved.saturating_sub(1);
    }

    /// Resolve `ctx_id` for `owner`, or `None` if it is untracked or tracked by
    /// a DIFFERENT owner.
    pub fn resolve_owned_ctx(&self, owner: Option<DeviceOwner>, ctx_id: u32) -> Option<OwnedCtx> {
        self.contexts
            .iter()
            .find(|c| c.ctx_id == ctx_id && c.owner == owner)
            .map(|c| OwnedCtx { id: c.ctx_id })
    }

    /// Drop a context's tracking slot, but only for its owner. Returns the
    /// resolved id, or `None` if the caller does not own it.
    pub fn untrack_owned_context(&mut self, owner: Option<DeviceOwner>, ctx_id: u32) -> Option<u32> {
        let idx = self
            .contexts
            .iter()
            .position(|c| c.ctx_id == ctx_id && c.owner == owner)?;
        Some(self.contexts.swap_remove(idx).ctx_id)
    }

    /// Pop one context still owned by `owner` (device-teardown reclamation
    /// iterates this, running the CTX_DESTROY round-trip outside the lock).
    pub fn take_context_for_owner(&mut self, owner: Option<DeviceOwner>) -> Option<u32> {
        let idx = self.contexts.iter().position(|c| c.owner == owner)?;
        Some(self.contexts.swap_remove(idx).ctx_id)
    }

    /// Reserve a live-resource table slot for an in-flight create. The table is
    /// load-bearing (OpenAllocation / ATTACH liveness validation reads it), so
    /// an untracked-but-live resource must never exist: refuse the create when
    /// the table is full instead of creating and dropping the tracking entry.
    pub fn reserve_resource_slot(&mut self) -> bool {
        if self.resources.len() + self.resources_reserved >= MAX_RESOURCES {
            RESOURCE_FULL_REJECTS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.resources_reserved += 1;
        true
    }

    /// Commit a reserved resource slot with the now-host-live id.
    pub fn commit_resource(&mut self, resource_id: u32) {
        self.resources_reserved = self.resources_reserved.saturating_sub(1);
        self.resources.push(resource_id);
        bump_high_water(&RESOURCE_HIGH_WATER, self.resources.len());
    }

    /// Release a reserved resource slot after a failed create.
    pub fn cancel_resource_reservation(&mut self) {
        self.resources_reserved = self.resources_reserved.saturating_sub(1);
    }

    /// Reserve a blob-table slot for an in-flight ALLOC_BLOB.
    pub fn reserve_blob_slot(&mut self) -> bool {
        if self.blobs.len() + self.blobs_reserved >= MAX_BLOBS {
            BLOB_FULL_REJECTS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.blobs_reserved += 1;
        true
    }

    /// Commit a reserved blob slot.
    pub fn commit_blob(
        &mut self,
        owner: Option<DeviceOwner>,
        ctx_id: u32,
        resource_id: u32,
        size: u64,
    ) {
        self.blobs_reserved = self.blobs_reserved.saturating_sub(1);
        self.blobs.push(BlobSlot {
            owner,
            ctx_id,
            resource_id,
            size,
            mapped: false,
            map_pending: false,
            map_cache: 0,
            map_offset: 0,
            map_len: 0,
        });
        bump_high_water(&BLOB_HIGH_WATER, self.blobs.len());
    }

    /// Release a reserved blob slot after a failed create.
    pub fn cancel_blob_reservation(&mut self) {
        self.blobs_reserved = self.blobs_reserved.saturating_sub(1);
    }

    /// Pop the blob slot matching (`owner`, `ctx_id`, `resource_id`) — the
    /// RELEASE_BLOB path. The caller unmaps/detaches/unrefs outside the lock
    /// and returns the window range via [`Self::free_window_range_pub`].
    /// Takes a `DeviceOwner`, not an `Option`: the KMD-owned slots are
    /// unreachable from this path by TYPE, which is the whole point — an escape
    /// with a null hDevice used to match every blob the KMD had adopted for a
    /// live WDDM allocation.
    pub fn take_blob_matching(
        &mut self,
        owner: DeviceOwner,
        ctx_id: u32,
        resource_id: u32,
    ) -> Option<(u32, bool, u64, u64)> {
        let idx = self.blobs.iter().position(|s| {
            s.owner == Some(owner) && s.ctx_id == ctx_id && s.resource_id == resource_id
        })?;
        let slot = self.blobs.swap_remove(idx);
        Some((slot.resource_id, slot.mapped, slot.map_offset, slot.map_len))
    }

    /// Pop one blob still owned by `owner` (device-teardown reclamation).
    /// Returns `(ctx_id, resource_id, mapped, map_offset, map_len)`.
    pub fn take_blob_for_owner(
        &mut self,
        owner: Option<DeviceOwner>,
    ) -> Option<(u32, u32, bool, u64, u64)> {
        let idx = self.blobs.iter().position(|s| s.owner == owner)?;
        let slot = self.blobs.swap_remove(idx);
        Some((
            slot.ctx_id,
            slot.resource_id,
            slot.mapped,
            slot.map_offset,
            slot.map_len,
        ))
    }

    /// Whether `resource_id` is alive host-side, per the KMD's authoritative
    /// live-resource table (the KMD owns the resid namespace: every blob create
    /// and every unref goes through it, so this mirrors the host's global
    /// resource table exactly).
    ///
    /// This exists because the host's CTX_ATTACH_RESOURCE path CANNOT be
    /// trusted to report failure: `virgl_renderer_ctx_attach_resource` is void
    /// and silently no-ops on an unknown resource, so QEMU replies OK_NODATA
    /// for an attach that never happened — the exact mechanism behind the
    /// boot-#3 `vkr: failed to import resource: invalid res_id 45` dwm kill.
    /// OpenAllocation and the ATTACH_RESOURCE escape validate against this
    /// table and fail loudly instead.
    pub fn resource_is_live(&self, resource_id: u32) -> bool {
        self.resources.iter().any(|&r| r == resource_id)
    }

    /// Remove a live resource id from the one-shot ownership table.
    ///
    /// Returns true only for the first teardown claimant. Later duplicate release
    /// paths must skip host DETACH/UNREF, because the host has already destroyed
    /// the resource and returns ERR_INVALID_RESOURCE_ID.
    pub fn take_live_resource(&mut self, resource_id: u32) -> bool {
        let Some(idx) = self.resources.iter().position(|&r| r == resource_id) else {
            // Atomic, not diag::record — callers hold the device spinlock
            // (DISPATCH_LEVEL); the registry tracer is PASSIVE-only.
            TAKE_LIVE_MISSES.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        self.resources.swap_remove(idx);
        true
    }

    /// Record a blob's size in the tracking table so a later map can size the
    /// mapping. Used by the in-kernel venus client, which creates its
    /// ring/reply/page-table blobs directly (it owns the resource lifecycle for
    /// the device lifetime rather than per-escape). `owner = 0` marks a
    /// KMD-internal blob (not reclaimed by an escape owner). No-ops (counted)
    /// if the table is full — a later map then fails honestly.
    pub fn note_blob_size(&mut self, resource_id: u32, size: u64) {
        if self.blobs.iter().any(|s| s.resource_id == resource_id) {
            return;
        }
        if self.blobs.len() + self.blobs_reserved >= MAX_BLOBS {
            BLOB_FULL_REJECTS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // Record with ctx_id 0 / KMD owner: these blobs are not driven by an escape
        // device handle; teardown unrefs them explicitly via the venus client.
        self.blobs.push(BlobSlot {
            owner: None,
            ctx_id: 0,
            resource_id,
            size,
            mapped: false,
            map_pending: false,
            map_cache: 0,
            map_offset: 0,
            map_len: 0,
        });
    }

    /// Look up a blob's tracking state by resource id (any owner). Returns
    /// `(owner, size, mapped)` if the resource is a tracked, host-visible-mappable
    /// blob. Used by the Present blit to decide whether the composition source /
    /// IddCx destination can be CPU-mapped for a coherence copy.
    pub fn blob_lookup(&self, resource_id: u32) -> Option<(Option<DeviceOwner>, u64, bool)> {
        self.blobs
            .iter()
            .find(|s| s.resource_id == resource_id)
            .map(|s| (s.owner, s.size, s.mapped))
    }

    /// Begin mapping a blob into the host-visible window: if already mapped,
    /// return the mapping; otherwise reserve a window range and hand the
    /// RESOURCE_MAP_BLOB round-trip to the caller (PASSIVE, outside this lock),
    /// who then calls [`Self::blob_map_finish`]. [`OwnerFilter::Any`] resolves by
    /// resource id alone (the kernel paths' any-owner lookup); `Exactly` is the
    /// owner-scoped escape path (resource ids can repeat across adapter restarts
    /// while stale clients unwind) and also names the KMD-owned slots as
    /// `Exactly(None)`.
    pub fn blob_map_begin(&mut self, owner: OwnerFilter, resource_id: u32) -> BlobMapBegin {
        let Some(window) = self.host_visible else {
            return BlobMapBegin::Failed(VirtioError::DeviceError);
        };
        let Some(idx) = self
            .blobs
            .iter()
            .position(|s| {
                s.resource_id == resource_id
                    && match owner {
                        OwnerFilter::Any => true,
                        OwnerFilter::Exactly(o) => s.owner == o,
                    }
            })
        else {
            return BlobMapBegin::Failed(VirtioError::DeviceError);
        };
        if self.blobs[idx].mapped {
            let s = &self.blobs[idx];
            return BlobMapBegin::Mapped(BlobMapPrep {
                gpa: window.base + s.map_offset,
                size: s.map_len,
                map_cache: s.map_cache,
            });
        }
        if self.blobs[idx].map_pending {
            return BlobMapBegin::Busy;
        }
        let map_len = round_up_page(self.blobs[idx].size);
        if map_len == 0 || map_len > MAX_BLOB_MAP_BYTES {
            return BlobMapBegin::Failed(VirtioError::DeviceError);
        }
        let offset = match self.alloc_window_range(map_len, window.len) {
            Ok(o) => o,
            Err(e) => return BlobMapBegin::Failed(e),
        };
        let s = &mut self.blobs[idx];
        s.map_pending = true;
        s.map_offset = offset;
        s.map_len = map_len;
        BlobMapBegin::Start {
            offset,
            len: map_len,
        }
    }

    /// Finish a [`Self::blob_map_begin`] `Start`: record the host's verdict.
    /// `cache = Some(nibble)` on RESP_OK_MAP_INFO; `None` on host rejection
    /// (the reserved range is returned to the allocator).
    pub fn blob_map_finish(
        &mut self,
        resource_id: u32,
        offset: u64,
        len: u64,
        cache: Option<u32>,
    ) -> BlobMapFinish {
        let Some(idx) = self
            .blobs
            .iter()
            .position(|s| s.resource_id == resource_id && s.map_pending && s.map_offset == offset)
        else {
            // Owner teardown raced the round-trip and took the slot. The caller
            // must undo the host mapping (UNMAP round-trip) and then return the
            // range via `free_window_range_pub`.
            if cache.is_none() {
                self.free_window_range(offset, len);
                return BlobMapFinish::HostRejected;
            }
            return BlobMapFinish::SlotGone;
        };
        let window_base = self.host_visible.map_or(0, |w| w.base);
        let s = &mut self.blobs[idx];
        s.map_pending = false;
        match cache {
            Some(c) => {
                s.mapped = true;
                s.map_cache = c;
                BlobMapFinish::Done(BlobMapPrep {
                    gpa: window_base + offset,
                    size: len,
                    map_cache: c,
                })
            }
            None => {
                s.map_offset = 0;
                s.map_len = 0;
                self.free_window_range(offset, len);
                BlobMapFinish::HostRejected
            }
        }
    }

    /// Return a window range to the allocator (PASSIVE flows that unmapped a
    /// blob outside the lock).
    pub fn free_window_range_pub(&mut self, offset: u64, len: u64) {
        self.free_window_range(offset, len);
    }

    /// Reserve the first `len` bytes of the host-visible window for VidMm (the
    /// CPU-visible BAR memory segment). Called once from StartDevice, before
    /// any blob is mapped: the KMD offset allocator starts past the partition,
    /// and freed ranges inside it are never recycled by the KMD side.
    pub fn reserve_window_prefix(&mut self, len: u64) {
        self.vidmm_reserved = len;
        if self.next_window_offset < len {
            self.next_window_offset = len;
        }
    }

    /// Begin a fixed-offset (re)map of a blob at the VidMm-assigned window
    /// offset `offset` (must lie inside the VidMm partition). Unlike
    /// [`Self::blob_map_begin`] the offset is dictated by the caller, and an
    /// existing mapping at a DIFFERENT offset is handed back for unmapping —
    /// blob content is intrinsic to the host memory object, so a remap is
    /// content-preserving. Any-owner resolve (kernel path, like the executor).
    pub fn blob_remap_begin(&mut self, resource_id: u32, offset: u64) -> BlobRemapBegin {
        if self.host_visible.is_none() {
            return BlobRemapBegin::Failed(VirtioError::DeviceError);
        }
        let window_base = self.host_visible.map_or(0, |w| w.base);
        let Some(idx) = self.blobs.iter().position(|s| s.resource_id == resource_id) else {
            return BlobRemapBegin::Failed(VirtioError::DeviceError);
        };
        if self.blobs[idx].map_pending {
            return BlobRemapBegin::Busy;
        }
        if self.blobs[idx].mapped && self.blobs[idx].map_offset == offset {
            let s = &self.blobs[idx];
            return BlobRemapBegin::Mapped(BlobMapPrep {
                gpa: window_base + s.map_offset,
                size: s.map_len,
                map_cache: s.map_cache,
            });
        }
        let map_len = round_up_page(self.blobs[idx].size);
        if map_len == 0 || map_len > MAX_BLOB_MAP_BYTES {
            return BlobRemapBegin::Failed(VirtioError::DeviceError);
        }
        // The target range must sit entirely inside the VidMm partition —
        // anything else would collide with the KMD-side offset allocator.
        if offset % BLOB_PAGE != 0
            || offset
                .checked_add(map_len)
                .map_or(true, |e| e > self.vidmm_reserved)
        {
            return BlobRemapBegin::Failed(VirtioError::DeviceError);
        }
        let old = if self.blobs[idx].mapped {
            Some((self.blobs[idx].map_offset, self.blobs[idx].map_len))
        } else {
            None
        };
        let s = &mut self.blobs[idx];
        s.map_pending = true;
        s.mapped = false;
        s.map_offset = offset;
        s.map_len = map_len;
        BlobRemapBegin::Start { old, len: map_len }
    }

    /// Blobs currently mapped overlapping `[offset, offset+len)` inside the
    /// VidMm partition, EXCLUDING `keep_resource_id`. Such mappings are stale
    /// VidMm placements (an eviction this driver missed or dropped): VidMm
    /// never double-books segment ranges, so before mapping a new blob into
    /// the range the caller must RESOURCE_UNMAP_BLOB each returned id and
    /// clear it via [`Self::blob_note_unmapped`]. Bounded scan under the lock.
    ///
    /// Returns `Err(WindowOverlapTruncated)` if a further overlap is found once
    /// `out` is full. The scan used to stop RECORDING at that point and return
    /// the truncated count, which the caller read as the complete set: a ninth
    /// overlapping mapping was neither unmapped nor reported, and the
    /// RESOURCE_MAP_BLOB that followed created exactly the overlapping host
    /// window subregion the eviction pass exists to prevent — two host resources
    /// through one window subregion, against the blob-window-offset invariant
    /// (k-gputransport-04). The buffer deliberately stays fixed-size: this scan
    /// runs under the device spinlock, where allocation is forbidden.
    pub fn blobs_overlapping(
        &self,
        offset: u64,
        len: u64,
        keep_resource_id: u32,
        out: &mut [u32],
    ) -> Result<usize, WindowOverlapTruncated> {
        let end = offset.saturating_add(len);
        let mut n = 0;
        for s in self.blobs.iter() {
            if s.mapped
                && s.resource_id != keep_resource_id
                && s.map_offset < self.vidmm_reserved
                && s.map_offset < end
                && s.map_offset.saturating_add(s.map_len) > offset
            {
                if n == out.len() {
                    WINDOW_OVERLAP_TRUNCATED.fetch_add(1, Ordering::Relaxed);
                    return Err(WindowOverlapTruncated);
                }
                out[n] = s.resource_id;
                n += 1;
            }
        }
        Ok(n)
    }

    /// The blob currently mapped exactly at window `offset`, if any. Used by
    /// `DxgkDdiUnmapCpuHostAperture`, which names aperture pages but not the
    /// allocation.
    pub fn blob_resid_at_offset(&self, offset: u64) -> Option<u32> {
        self.blobs
            .iter()
            .find(|s| s.mapped && s.map_offset == offset)
            .map(|s| s.resource_id)
    }

    /// Record that a blob's host mapping was torn down outside the normal
    /// release path (stale-placement eviction in `map_blob_at`). No window
    /// range is freed here — VidMm-partition offsets never enter the free list.
    pub fn blob_note_unmapped(&mut self, resource_id: u32) {
        if let Some(s) = self
            .blobs
            .iter_mut()
            .find(|s| s.resource_id == resource_id && s.mapped)
        {
            s.mapped = false;
            s.map_offset = 0;
            s.map_len = 0;
        }
    }

    /// Transfer a blob's lifetime ownership from its escape owner (the D3DKMT
    /// device handle the ICD allocated it under) to the WDDM allocation adopting
    /// it in `DxgkDdiCreateAllocation`. Returns whether the resource is LIVE —
    /// adopting a dead resid must fail the CreateAllocation loudly.
    ///
    /// This closes the res-45 lifetime hole (2026-07-03 boot #3): without the
    /// re-tag, `DxgkDdiDestroyDevice`'s `release_blobs_for_owner` sweep unrefs
    /// the host resource when the CREATING process's device dies, even though
    /// the shared WDDM allocation (and its cross-process openers) still
    /// reference it. Re-tagging to the KMD owner removes it from every escape-owner
    /// reclaim path; from here the allocation destroy path
    /// (`destroy_allocation_ctx` → `forget_allocation_blob` + guarded unref)
    /// owns the lifetime, matching KMD-created standard allocations.
    pub fn adopt_blob_for_allocation(&mut self, resource_id: u32) -> bool {
        if !self.resource_is_live(resource_id) {
            // Atomic, not diag::record — CreateAllocation calls this under the
            // device spinlock (DISPATCH_LEVEL); the registry tracer is PASSIVE-only.
            ADOPT_DEAD_REJECTS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if let Some(slot) = self.blobs.iter_mut().find(|s| s.resource_id == resource_id) {
            slot.owner = None;
        }
        true
    }

    /// Drop the KMD-internal (KMD-owned) tracking slot for an allocation's blob at
    /// DestroyAllocation time. Returns `Some((mapped, map_offset, map_len))` if
    /// the slot existed — when `mapped`, the caller (PASSIVE, outside the lock)
    /// must run the RESOURCE_UNMAP_BLOB round-trip and then return the range
    /// via [`Self::free_window_range_pub`]. Host detach/unref stays with the
    /// caller (the allocation owns the resource lifetime).
    pub fn forget_allocation_blob(&mut self, resource_id: u32) -> Option<(bool, u64, u64)> {
        let idx = self
            .blobs
            .iter()
            .position(|s| s.owner.is_none() && s.resource_id == resource_id)?;
        let slot = self.blobs.swap_remove(idx);
        Some((slot.mapped, slot.map_offset, slot.map_len))
    }

    /// Current number of tracked blob slots (diagnostics).
    pub fn blob_count(&self) -> usize {
        self.blobs.len()
    }

    /// Point-in-time table occupancy + host-visible-window usage for
    /// `HELIOS_ESCAPE_QUERY_STATS`. Called under the device spinlock; pure reads.
    pub fn table_stats(&self) -> TableStats {
        let free: u64 = self.free_window_ranges.iter().map(|r| r.len).sum();
        TableStats {
            blobs_live: self.blobs.len() as u32,
            resources_live: self.resources.len() as u32,
            contexts_live: self.contexts.len() as u32,
            window_used: self.next_window_offset.saturating_sub(free),
            window_len: self.host_visible.map_or(0, |w| w.len),
        }
    }

    /// Allocate a page-rounded `len`-byte range in the host-visible window: reuse a
    /// free range if one fits, else bump the high-water mark (bounded by `window_len`).
    fn alloc_window_range(&mut self, len: u64, window_len: u64) -> Result<u64, VirtioError> {
        if let Some(idx) = self.free_window_ranges.iter().position(|r| r.len >= len) {
            let offset = self.free_window_ranges[idx].offset;
            if self.free_window_ranges[idx].len == len {
                self.free_window_ranges.swap_remove(idx);
            } else {
                self.free_window_ranges[idx].offset += len;
                self.free_window_ranges[idx].len -= len;
            }
            return Ok(offset);
        }
        let offset = self.next_window_offset;
        let end = match offset.checked_add(len) {
            Some(e) if e <= window_len => e,
            _ => {
                WINDOW_ALLOC_REJECTS.fetch_add(1, Ordering::Relaxed);
                return Err(VirtioError::OutOfMemory);
            }
        };
        self.next_window_offset = end;
        Ok(offset)
    }

    /// Return a window range to the allocator: drop the high-water mark if it abuts,
    /// else coalesce into an adjacent free range, else record a new free range (or
    /// silently leak if the bounded free list is full — bring-up acceptable).
    fn free_window_range(&mut self, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        // VidMm-partition offsets are owned by VidMm's segment allocator — they
        // must never enter the KMD free list (a later KMD-side map would collide
        // with a VidMm placement). Every release path funnels here, so this one
        // guard covers DestroyAllocation/ReleaseBlob/teardown of VidMm-placed
        // blobs uniformly.
        if offset < self.vidmm_reserved {
            return;
        }
        if offset.checked_add(len) == Some(self.next_window_offset) {
            self.next_window_offset = offset;
            while let Some(idx) = self
                .free_window_ranges
                .iter()
                .position(|r| r.offset.checked_add(r.len) == Some(self.next_window_offset))
            {
                let r = self.free_window_ranges.swap_remove(idx);
                self.next_window_offset = r.offset;
            }
            return;
        }
        for range in &mut self.free_window_ranges {
            if range.offset.checked_add(range.len) == Some(offset) {
                range.len += len;
                return;
            }
            if offset.checked_add(len) == Some(range.offset) {
                range.offset = offset;
                range.len += len;
                return;
            }
        }
        if self.free_window_ranges.len() < MAX_WINDOW_RANGES {
            self.free_window_ranges.push(WindowRange { offset, len });
        } else {
            // Free list full: the range is dropped and its window offset space
            // is leaked until driver restart. Counted so QUERY_STATS makes the
            // leak visible instead of silent.
            WINDOW_RANGE_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The host-visible blob window, or `None` if the device exposes none.
    /// `DxgkDdiQueryAdapterInfo` uses `base`/`len` to describe the CPU-visible
    /// memory segment, and `DxgkDdiBuildPagingBuffer` adds the VidMm-assigned
    /// segment offset to `base` for the user mapping. Gate 5a Stage 2.
    pub fn host_visible(&self) -> Option<HostVisibleWindow> {
        self.host_visible
    }

    /// Scanout-0 preferred `(width, height)` from the host's `GET_DISPLAY_INFO` at
    /// init, or `None` if the host reported nothing usable. The display half drives
    /// its VidPn mode + generated EDID from this so it presents the size QEMU wants.
    pub fn display_mode(&self) -> Option<(u32, u32)> {
        self.display_mode
    }

    /// Mapped kernel VA of the virtio ISR-status register (read-to-clear), or 0 if
    /// the device exposes no ISR cap. `DxgkDdiStartDevice` copies this into the
    /// `AdapterContext` so the DIRQL ISR can acknowledge the INTx line lock-free.
    pub fn isr_status_addr(&self) -> usize {
        self.isr_status_va
    }
}

impl Drop for VirtioGpu {
    fn drop(&mut self) {
        // Drop any fence-event registrations still parked: dereference WITHOUT
        // signaling (the fences will never retire on a dead transport, and a
        // signal here would report fake completion — the waiter's own deadline
        // fires instead, and its unregister sees NOT_FOUND with an UNSIGNALED
        // event, which the ICD treats as failure). PASSIVE_LEVEL, outside the
        // device lock, so the deferred-delete variant is not required — but it
        // is unconditionally legal, and using it keeps a single deref path.
        for e in self.fence_events.drain(..) {
            FENCE_EVENT_TEARDOWN_DROPS.fetch_add(1, Ordering::Relaxed);
            // SAFETY: the entry owns an object reference taken at registration.
            unsafe { ObDereferenceObjectDeferDelete(e.event.as_ptr() as PVOID) };
        }

        // Quiesce the device (resets queues) so it stops touching the rings and
        // the in-flight/parked entry buffers we are about to free. Runs at
        // PASSIVE_LEVEL (StopDevice / set_virtio(None) drops outside the lock),
        // so the InFlight DmaBuffers in `inflight`/`parked` free legally as
        // part of this struct's drop.
        //
        // The BAR MMIO mappings made inside `PciTransport` are intentionally NOT
        // freed here: `WdkHal` caches them by physical address and reuses them on
        // the next StartDevice (the BARs are stable across stop/start), so there
        // is no per-cycle leak. The cache is released wholesale in
        // `DxgkDdiUnload` via `WdkHal::unmap_all`.
        self.transport.set_status(DeviceStatus::empty());
    }
}
