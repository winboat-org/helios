//! The virtio-gpu device object, built on the `virtio-drivers` PCI transport.
//!
//! `VirtioGpu` owns the `PciTransport` (discovers/maps the virtio config
//! regions), the control `VirtQueue`, and a contiguous DMA scratch page, and
//! layers the virtio-gpu command protocol (`helios_protocol`) on top. Built by
//! `init` from `DxgkDdiStartDevice` and stored in `AdapterContext::virtio`.
//!
//! Bring-up (all in `init`, at PASSIVE_LEVEL):
//!   M1 — `DxgkConfigAccess` → `PciRoot` → `PciTransport::new::<WdkHal,_>`
//!   M2 — feature negotiation via the `Transport` trait
//!   M3 — control `VirtQueue::<WdkHal>` setup + DRIVER_OK
//!   M4 — `GET_DISPLAY_INFO` polled round-trip (Phase-2 smoke test)

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

use alloc::vec::Vec;
use bytemuck::Zeroable;
use helios_protocol::{
    resp_is_ok, VirtioGpuCmdSubmit, VirtioGpuCtrlHdr, VirtioGpuCtxCreate, VirtioGpuCtxDestroy,
    VirtioGpuCtxResource, VirtioGpuResourceCreateBlob, VirtioGpuResourceMapBlob,
    VirtioGpuResourceUnmapBlob, VirtioGpuResourceUnref, VirtioGpuRespDisplayInfo,
    VirtioGpuRespMapInfo, HELIOS_OPTIONAL_FEATURES, HELIOS_REQUIRED_FEATURES,
    VIRTIO_GPU_CAPSET_VENUS, VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE, VIRTIO_GPU_CMD_CTX_CREATE,
    VIRTIO_GPU_CMD_CTX_DESTROY, VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE,
    VIRTIO_GPU_CMD_GET_DISPLAY_INFO, VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB,
    VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB, VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB,
    VIRTIO_GPU_CMD_RESOURCE_UNREF, VIRTIO_GPU_CMD_SUBMIT_3D, VIRTIO_GPU_FLAG_FENCE,
    VIRTIO_GPU_FLAG_INFO_RING_IDX, VIRTIO_GPU_MAP_CACHE_CACHED, VIRTIO_GPU_MAP_CACHE_MASK,
    VIRTIO_GPU_SHM_ID_HOST_VISIBLE,
    VIRTIO_PCI_CAP_ISR_CFG, VIRTIO_PCI_CAP_SHARED_MEMORY_CFG,
};
use virtio_drivers::queue::VirtQueue;
use virtio_drivers::transport::pci::bus::{ConfigurationAccess, DeviceFunction, PciRoot};
use virtio_drivers::transport::pci::PciTransport;
use virtio_drivers::transport::{DeviceStatus, Transport};
use virtio_drivers::{BufferDirection, Hal};

use super::config::DxgkConfigAccess;
use super::hal::WdkHal;
use super::VirtioError;
use crate::dxgk::DXGKRNL_INTERFACE;

/// Control queue index (virtio-gpu controlq = 0; cursorq = 1 is unused).
const CTRL_QUEUE: u16 = 0;
/// Control-queue ring size — power of two, conservatively ≤ the device's max.
const CTRL_QUEUE_SIZE: usize = 64;
/// One page of contiguous DMA scratch, split into request/response halves.
const SCRATCH_BYTES: usize = 4096;
/// Busy-poll bound for a control-queue round-trip (used-ring completion).
/// Each iteration is a volatile used-ring read + `spin_loop` (~10 ns), so the
/// bound is on the order of a second — generous for a healthy host (responses
/// are decoder-level acks, µs–ms), but finite for a wedged one. These
/// round-trips can run at DISPATCH_LEVEL under the device spinlock, where the
/// previous UNBOUNDED poll (`add_notify_wait_pop`) meant a wedged host became
/// a 0x101/0x133 bugcheck or a hard guest hang (observed 2026-07-03 04:0x).
/// One timeout poisons the transport (see `VirtioGpu::failed`).
const CTRL_POLL_SPINS: u64 = 100_000_000;

/// DISPATCH-safe count of control-queue round-trip timeouts (→ poison). Read
/// by `DxgkDdiCollectDbgInfo`; nonzero means the host stopped answering.
pub static CTRL_TIMEOUT_COUNT: AtomicU32 = AtomicU32::new(0);

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
            let va =
                unsafe { <WdkHal as Hal>::mmio_phys_to_virt(phys as virtio_drivers::PhysAddr, 16) };
            return va.as_ptr() as usize;
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
/// Max concurrently-tracked blobs. Generous for bring-up; table-full → alloc fails.
const MAX_BLOBS: usize = 256;
/// Max live virtio resources. This covers both escape blobs and KMD/WDDM standard
/// allocations, so teardown can suppress duplicate RESOURCE_UNREF commands.
const MAX_RESOURCES: usize = 1024;
/// Max concurrently-tracked virtio-gpu contexts (one per live device, generous).
const MAX_CONTEXTS: usize = 256;
/// Max coalescing free ranges in the window allocator's free list.
const MAX_WINDOW_RANGES: usize = 64;
/// Per-map size cap (also bounds the `IoAllocateMdl` ULONG length on the caller).
const MAX_BLOB_MAP_BYTES: u64 = 256 << 20;

/// Round `n` up to the next [`BLOB_PAGE`] multiple (saturating).
const fn round_up_page(n: u64) -> u64 {
    n.saturating_add(BLOB_PAGE - 1) & !(BLOB_PAGE - 1)
}

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
#[derive(Clone, Copy)]
struct BlobSlot {
    /// The owning D3D device handle (`DXGKARG_ESCAPE.hDevice`, as an opaque
    /// `usize`) that allocated this blob. `DxgkDdiDestroyDevice` reclaims every
    /// blob tagged with the destroyed handle, so a crashing/forgetful ICD (e.g.
    /// the crash-looping LogonUI, or any process that skips RELEASE_BLOB) cannot
    /// leak the bounded blob table (`MAX_BLOBS`) and false-trip later allocations
    /// with `STATUS_INSUFFICIENT_RESOURCES`.
    owner: usize,
    ctx_id: u32,
    resource_id: u32,
    /// Blob size in bytes (from ALLOC_BLOB; MAP_BLOB needs it to size the MDL).
    size: u64,
    /// RESOURCE_MAP_BLOB succeeded and must be paired with RESOURCE_UNMAP_BLOB.
    mapped: bool,
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

/// One tracked virtio-gpu context, tagged with the owning device handle for
/// device-teardown reclamation.
#[derive(Clone, Copy)]
struct ContextSlot {
    /// Owning D3D device handle (`DXGKARG_ESCAPE.hDevice` as an opaque `usize`).
    owner: usize,
    ctx_id: u32,
}

/// An initialized virtio-gpu transport.
pub struct VirtioGpu {
    /// The virtio-modern PCI transport (owns the mapped cfg-region VAs).
    transport: PciTransport,
    /// Control virtqueue (queue 0) — all GPU commands ride this.
    control: VirtQueue<WdkHal, CTRL_QUEUE_SIZE>,
    /// Contiguous DMA scratch page for synchronous command buffers. Freed in
    /// teardown (M6).
    scratch: NonNull<u8>,
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
    /// Every host-live virtio resource id created through this transport.
    /// Removal is one-shot and gates CTX_DETACH_RESOURCE/RESOURCE_UNREF, avoiding
    /// qemu `RESOURCE_UNREF: resource does not exist` errors from duplicate DDI
    /// teardown paths.
    resources: Vec<u32>,
    /// Live virtio-gpu contexts, tagged with the owning device handle, so
    /// `DxgkDdiDestroyDevice` can `CTX_DESTROY` any context an ICD created but did
    /// not tear down (crash / skipped CTX_DESTROY) — otherwise leaked contexts
    /// accumulate host-side state and eventually wedge the render server. Reserved
    /// to MAX_CONTEXTS at init (no realloc under the spinlock).
    contexts: Vec<ContextSlot>,
    /// Bump high-water for the host-visible window offset allocator.
    next_window_offset: u64,
    /// Coalescing free list for released window ranges (bounded by MAX_WINDOW_RANGES).
    free_window_ranges: Vec<WindowRange>,
    /// Poison latch: set when a control-queue round-trip times out (wedged
    /// host/device). A timed-out descriptor is still in flight — the device may
    /// complete it at any time — so the ring state is no longer trustworthy and
    /// every subsequent command fails fast with `DeviceError` instead of
    /// re-spinning at DISPATCH_LEVEL under the device spinlock (the 2026-07-03
    /// guest wedge: each new call burned another full spin budget).
    failed: bool,
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
        // the device, response is written by it.
        let (scratch_pa, scratch) = WdkHal::dma_alloc(1, BufferDirection::Both);
        if scratch_pa == 0 {
            // dma_alloc signals failure with a zero physaddr + dangling ptr;
            // bail rather than write into the dangling page.
            return Err(VirtioError::OutOfMemory);
        }
        // SAFETY: `scratch` is a freshly-allocated, owned, contiguous page.
        let buf = unsafe { core::slice::from_raw_parts_mut(scratch.as_ptr(), SCRATCH_BYTES) };
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
            let token = unsafe { control.add(inputs, outputs) }
                .map_err(|_| VirtioError::DeviceError)?;
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

        let mut gpu = Self {
            transport,
            control,
            scratch,
            next_ctx_id: AtomicU32::new(1),
            next_resource_id: AtomicU32::new(1),
            host_visible,
            isr_status_va,
            blobs: Vec::with_capacity(MAX_BLOBS),
            resources: Vec::with_capacity(MAX_RESOURCES),
            contexts: Vec::with_capacity(MAX_CONTEXTS),
            next_window_offset: 0,
            free_window_ranges: Vec::with_capacity(MAX_WINDOW_RANGES),
            failed: false,
        };

        // Gate-2 bring-up validation (diagnostic; runs at PASSIVE_LEVEL from
        // StartDevice, so the diag registry tracer is safe here): prove the venus
        // 3D-context lifecycle works on the live device — a real prerequisite for
        // the venus-backed allocation flow. Records 0x1100 on success / 0xFFFFFFFF
        // on failure. Never gates StartDevice — Gate 1 stays start-safe.
        // (A HOST3D blob can't be smoke-tested standalone: it needs a venus memory
        // id from the UMD's vkAllocateMemory; confirmed by the .56 ERR_UNSPEC run
        // and the proven System-class kmd::alloc_blob reference.)
        // TODO(gate2): remove once CreateAllocation/UMD own the resource lifecycle.
        let resp = gpu.self_test_venus_context();
        crate::diag::record(0x0B00_0010);
        crate::diag::record(resp);

        // Read-to-clear the ISR-status register once: the GET_DISPLAY_INFO + venus
        // self-test commands above completed via the polled path, which never
        // touches this register, so the device may still be asserting INTx from
        // those completions. Clear it now (PASSIVE) so the line starts deasserted
        // before dxgkrnl connects our interrupt.
        if gpu.isr_status_va != 0 {
            // SAFETY: `isr_status_va` is the mapped MMIO VA of the 1-byte
            // read-to-clear ISR-status register; a volatile read clears it.
            let _ = unsafe { core::ptr::read_volatile(gpu.isr_status_va as *const u8) };
        }

        Ok(gpu)
    }

    // ── Venus control path (Phase 3, M3.2) ──────────────────────────────────
    //
    // All three methods drive the control virtqueue *synchronously* via
    // `add_notify_wait_pop` (polled used-ring round-trip), like `init`. They take
    // `&mut self` and assume the caller holds the AdapterContext spinlock so the
    // shared `scratch` page and control queue are not touched concurrently
    // (escape submits at PASSIVE today; the DPC drain arrives in M3.4). They run
    // under that spinlock at DISPATCH_LEVEL, so they perform NO allocation — any
    // payload buffer (the Venus stream) is allocated by the caller at PASSIVE and
    // passed in already contiguous.

    /// Create a virtio-gpu 3D context bound to `capset_id` (Venus = 4) and return
    /// the guest-assigned context id. `owner` is the D3D device handle that owns
    /// the context, recorded so `DxgkDdiDestroyDevice` can reclaim a context the
    /// ICD created but never explicitly destroyed.
    pub fn ctx_create(&mut self, capset_id: u32, owner: usize) -> Result<u32, VirtioError> {
        let ctx_id = self.next_ctx_id.fetch_add(1, Ordering::Relaxed);
        let mut cmd = VirtioGpuCtxCreate::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_CREATE;
        cmd.hdr.ctx_id = ctx_id;
        // With VIRTIO_GPU_F_CONTEXT_INIT, context_init carries the capset id.
        cmd.context_init = capset_id;
        // A debug name helps host-side (virglrenderer) logs; purely cosmetic.
        const NAME: &[u8] = b"helios";
        cmd.nlen = NAME.len() as u32;
        cmd.debug_name[..NAME.len()].copy_from_slice(NAME);
        crate::diag::record(0x0D20_0000 | (ctx_id & 0xFFFF));
        let resp = self.ctrl_roundtrip_typed(bytemuck::bytes_of(&cmd))?;
        crate::diag::record(0x0D21_0000 | (resp & 0xFFFF));
        if !resp_is_ok(resp) {
            return Err(VirtioError::DeviceError);
        }
        // Track for device-teardown reclamation. `push` stays within the reserved
        // capacity (no realloc under the spinlock); if the registry is somehow full
        // we still hand back the context — it just won't be auto-reclaimed.
        if self.contexts.len() < MAX_CONTEXTS {
            self.contexts.push(ContextSlot { owner, ctx_id });
        }
        Ok(ctx_id)
    }

    /// Destroy a previously created 3D context and drop its tracking slot.
    pub fn ctx_destroy(&mut self, ctx_id: u32) -> Result<(), VirtioError> {
        if let Some(idx) = self.contexts.iter().position(|c| c.ctx_id == ctx_id) {
            self.contexts.swap_remove(idx);
        }
        let mut cmd = VirtioGpuCtxDestroy::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_DESTROY;
        cmd.hdr.ctx_id = ctx_id;
        self.ctrl_roundtrip(bytemuck::bytes_of(&cmd))
    }

    /// Submit an opaque Venus command stream to `ctx_id`, fenced with `fence_id`.
    ///
    /// `venus` MUST be physically contiguous (carve it from a [`DmaBuffer`]) — it
    /// rides a single device-readable descriptor. The command is fenced and this
    /// blocks (polled) until the device acknowledges it on the used ring, so by
    /// the time it returns the work is host-visible-complete (interim sync fence
    /// model; the async/KEVENT model lands in M3.4).
    pub fn submit_venus(
        &mut self,
        ctx_id: u32,
        fence_id: u64,
        ring_idx: u32,
        venus: &[u8],
    ) -> Result<(), VirtioError> {
        if venus.is_empty() {
            return Err(VirtioError::DeviceError);
        }
        crate::diag::record(0x0D10_0000 | ((venus.len() as u32) & 0xFFFF));
        let mut cmd = VirtioGpuCmdSubmit::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_SUBMIT_3D;
        cmd.hdr.flags = VIRTIO_GPU_FLAG_FENCE;
        cmd.hdr.fence_id = fence_id;
        cmd.hdr.ctx_id = ctx_id;
        if ring_idx != 0 {
            cmd.hdr.flags |= VIRTIO_GPU_FLAG_INFO_RING_IDX;
            cmd.hdr.ring_idx = ring_idx.min(u8::MAX as u32) as u8;
        }
        cmd.size = venus.len() as u32;

        let hdr_len = core::mem::size_of::<VirtioGpuCmdSubmit>();
        let resp_len = core::mem::size_of::<VirtioGpuCtrlHdr>();
        // SAFETY: `scratch` is our owned contiguous page; the low half holds the
        // submit request (device-read), the high half the response (device-write).
        // Disjoint halves; serialized by the caller's spinlock.
        let buf = unsafe { core::slice::from_raw_parts_mut(self.scratch.as_ptr(), SCRATCH_BYTES) };
        let (req_buf, resp_buf) = buf.split_at_mut(SCRATCH_BYTES / 2);
        req_buf[..hdr_len].copy_from_slice(bytemuck::bytes_of(&cmd));

        // SUBMIT_3D is a virtio-gpu command header plus a second device-readable
        // descriptor containing the opaque Venus stream. Keep this split to match
        // the proven system KMD transport and avoid the host mis-parsing a 32-byte
        // submit header as another control command.
        crate::diag::record(0x0D12_0000 | ((venus.len() as u32) & 0xFFFF));
        self.ctrl_queue_bounded_roundtrip(
            &[&req_buf[..hdr_len], venus],
            &mut [&mut resp_buf[..resp_len]],
        )?;
        let resp: &VirtioGpuCtrlHdr = bytemuck::from_bytes(&resp_buf[..resp_len]);
        if resp_is_ok(resp.type_) {
            Ok(())
        } else {
            Err(VirtioError::DeviceError)
        }
    }

    // ── Resource lifecycle (Gate 2) ─────────────────────────────────────────
    //
    // Like the Venus control path above, these drive the control virtqueue
    // synchronously and assume the caller holds the AdapterContext spinlock so
    // the shared scratch page / control queue are not touched concurrently. The
    // resource id is guest-assigned (we own the namespace), so it is known before
    // the round-trip and returned on a successful create.

    /// Create a HOST3D virtio-gpu blob resource in venus context `ctx_id`,
    /// referencing venus device-memory `blob_id`, and attach it to the context.
    /// Returns the guest-assigned resource id.
    ///
    /// Mirrors the proven System-class `kmd::alloc_blob` sequence
    /// (create_blob → ctx_attach_resource). A HOST3D mappable blob with
    /// `blob_id = 0` is rejected by the host with `RESP_ERR_UNSPEC` (it has no
    /// venus memory to bind), so `blob_id` must be a real venus mem id obtained
    /// from the UMD's `vkAllocateMemory` venus stream — i.e. this is only callable
    /// once the UMD allocation path supplies one. `blob_mem`/`blob_flags` are
    /// `VIRTIO_GPU_BLOB_MEM_*` / `VIRTIO_GPU_BLOB_FLAG_*`. `nr_entries = 0`: HOST3D
    /// blobs are host-backed, so no guest page list follows the command.
    pub fn resource_create_blob(
        &mut self,
        ctx_id: u32,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
    ) -> Result<u32, VirtioError> {
        // The live-resource table is load-bearing (OpenAllocation / ATTACH
        // liveness validation reads it), so an untracked-but-live resource must
        // never exist: refuse the create when the table is full instead of
        // creating and silently dropping the tracking entry.
        if self.resources.len() >= MAX_RESOURCES {
            crate::diag::record(0x0D20_00E1);
            return Err(VirtioError::OutOfMemory);
        }
        let resource_id = self.next_resource_id.fetch_add(1, Ordering::Relaxed);
        let mut cmd = VirtioGpuResourceCreateBlob::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB;
        cmd.hdr.ctx_id = ctx_id;
        cmd.resource_id = resource_id;
        cmd.blob_mem = blob_mem;
        cmd.blob_flags = blob_flags;
        cmd.nr_entries = 0;
        cmd.blob_id = blob_id;
        cmd.size = size;
        self.ctrl_roundtrip(bytemuck::bytes_of(&cmd))?;
        self.ctx_attach_resource(ctx_id, resource_id)?;
        self.resources.push(resource_id);
        Ok(resource_id)
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
            crate::diag::record(0x0D20_00E0);
            return false;
        };
        self.resources.swap_remove(idx);
        true
    }

    /// `HELIOS_ESCAPE_ALLOC_BLOB` — create a HOST3D blob (create + ctx-attach) and
    /// record its size so a later MAP_BLOB can size the MDL. Returns the resource id.
    pub fn alloc_blob(
        &mut self,
        ctx_id: u32,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
        owner: usize,
    ) -> Result<u32, VirtioError> {
        if size == 0 {
            return Err(VirtioError::DeviceError);
        }
        if self.blobs.len() >= MAX_BLOBS {
            return Err(VirtioError::OutOfMemory);
        }
        let resource_id = self.resource_create_blob(ctx_id, blob_mem, blob_flags, blob_id, size)?;
        // `push` stays within the reserved capacity (no realloc under the lock).
        self.blobs.push(BlobSlot {
            owner,
            ctx_id,
            resource_id,
            size,
            mapped: false,
            map_offset: 0,
            map_len: 0,
        });
        Ok(resource_id)
    }

    /// Record a blob's size in the tracking table so a later [`map_blob_prepare`]
    /// can size the mapping. Used by the in-kernel venus client, which creates its
    /// ring/reply/page-table blobs via `resource_create_blob` directly (it owns the
    /// resource lifecycle for the device lifetime rather than per-escape) and must
    /// register the size the same way [`alloc_blob`] does for the escape path.
    /// `owner = 0` marks a KMD-internal blob (not reclaimed by an escape owner).
    /// Silently no-ops if the table is full (the map_prepare then fails honestly).
    pub fn note_blob_size(&mut self, resource_id: u32, size: u64) {
        if self.blobs.iter().any(|s| s.resource_id == resource_id) {
            return;
        }
        if self.blobs.len() >= MAX_BLOBS {
            return;
        }
        // Record with ctx_id 0 / owner 0: these blobs are not driven by an escape
        // device handle; teardown unrefs them explicitly via the venus client.
        self.blobs.push(BlobSlot {
            owner: 0,
            ctx_id: 0,
            resource_id,
            size,
            mapped: false,
            map_offset: 0,
            map_len: 0,
        });
    }

    /// Look up a blob's tracking state by resource id (any owner). Returns
    /// `(owner, size, mapped)` if the resource is a tracked, host-visible-mappable
    /// blob. Used by the Present blit to decide whether the composition source /
    /// IddCx destination can be CPU-mapped for a coherence copy.
    pub fn blob_lookup(&self, resource_id: u32) -> Option<(usize, u64, bool)> {
        self.blobs
            .iter()
            .find(|s| s.resource_id == resource_id)
            .map(|s| (s.owner, s.size, s.mapped))
    }

    /// Kernel-side view of a blob for the software GDI executor: the guest-physical
    /// range of its host-visible mapping, mapping the blob into the window on first
    /// use (same `RESOURCE_MAP_BLOB` path as the user-mode escape). Any-owner lookup:
    /// GDI surfaces are KMD-self-backed standard allocations, so the executor
    /// resolves them by resource id alone.
    pub fn blob_kernel_range(&mut self, resource_id: u32) -> Result<BlobMapPrep, VirtioError> {
        let window = self.host_visible.ok_or(VirtioError::DeviceError)?;
        let slot = self
            .blobs
            .iter()
            .find(|s| s.resource_id == resource_id)
            .copied()
            .ok_or(VirtioError::DeviceError)?;
        if slot.mapped {
            return Ok(BlobMapPrep {
                gpa: window.base + slot.map_offset,
                size: slot.map_len,
                // Host-visible venus memory is WB (see `KernelMap::new`); the nibble
                // is only consulted for the caching type, so CACHED is correct here.
                map_cache: VIRTIO_GPU_MAP_CACHE_CACHED,
            });
        }
        self.map_blob_prepare_for_owner(slot.owner, resource_id)
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
    /// reference it. Re-tagging to owner 0 removes it from every escape-owner
    /// reclaim path; from here the allocation destroy path
    /// (`destroy_allocation_ctx` → `forget_allocation_blob` + guarded unref)
    /// owns the lifetime, matching KMD-created standard allocations.
    pub fn adopt_blob_for_allocation(&mut self, resource_id: u32) -> bool {
        if !self.resource_is_live(resource_id) {
            crate::diag::record(0x0D20_00E2);
            return false;
        }
        if let Some(slot) = self
            .blobs
            .iter_mut()
            .find(|s| s.resource_id == resource_id)
        {
            slot.owner = 0;
        }
        true
    }

    /// Drop the KMD-internal (owner-0) tracking slot for an allocation's blob at
    /// DestroyAllocation time, unmapping the host-visible window mapping the GDI
    /// executor may have opened and returning its window range. Host detach/unref
    /// stays with the caller (the allocation owns the resource lifetime). Returns
    /// `true` if a live mapping was unmapped here (so the caller must not send a
    /// second host unmap for the same resource).
    pub fn forget_allocation_blob(&mut self, resource_id: u32) -> bool {
        let Some(idx) = self
            .blobs
            .iter()
            .position(|s| s.owner == 0 && s.resource_id == resource_id)
        else {
            return false;
        };
        let slot = self.blobs.swap_remove(idx);
        if slot.mapped {
            let _ = self.resource_unmap_blob(slot.resource_id);
            self.free_window_range(slot.map_offset, slot.map_len);
            return true;
        }
        false
    }

    /// `HELIOS_ESCAPE_MAP_BLOB` under-lock phase: pick a window offset, issue
    /// `RESOURCE_MAP_BLOB`, and return the guest-physical range + host caching for the
    /// caller to map into user space (PASSIVE, outside the lock). The guest chooses
    /// the window offset, so VidMm is never involved — the host backs exactly the
    /// `host_visible.base + offset` range we report back.
    pub fn map_blob_prepare(&mut self, resource_id: u32) -> Result<BlobMapPrep, VirtioError> {
        self.map_blob_prepare_for_owner(0, resource_id)
    }

    /// Owner-scoped variant used by user-mode escapes. Resource ids can repeat
    /// after adapter restart while stale clients are still unwinding, so escape
    /// callers must not map another D3DKMT device handle's new blob by id alone.
    pub fn map_blob_prepare_for_owner(
        &mut self,
        owner: usize,
        resource_id: u32,
    ) -> Result<BlobMapPrep, VirtioError> {
        let window = self.host_visible.ok_or(VirtioError::DeviceError)?;
        let size = self
            .blobs
            .iter()
            .find(|s| s.owner == owner && s.resource_id == resource_id)
            .map(|s| s.size)
            .ok_or(VirtioError::DeviceError)?;
        let map_len = round_up_page(size);
        if map_len == 0 || map_len > MAX_BLOB_MAP_BYTES {
            return Err(VirtioError::DeviceError);
        }
        let offset = self.alloc_window_range(map_len, window.len)?;

        // Host round-trip before recording the blob as mapped; on rejection return
        // the reserved window range for later reuse.
        let map_cache = match self.resource_map_blob(resource_id, offset) {
            Ok(c) => c,
            Err(e) => {
                self.free_window_range(offset, map_len);
                return Err(e);
            }
        };

        if let Some(slot) = self
            .blobs
            .iter_mut()
            .find(|s| s.owner == owner && s.resource_id == resource_id)
        {
            slot.mapped = true;
            slot.map_offset = offset;
            slot.map_len = map_len;
        }
        Ok(BlobMapPrep {
            gpa: window.base + offset,
            size: map_len,
            map_cache,
        })
    }

    /// `HELIOS_ESCAPE_RELEASE_BLOB` — unmap (if mapped) + detach + unref a blob and
    /// drop its tracking slot, returning its window range to the free list.
    pub fn release_blob_for_owner(
        &mut self,
        owner: usize,
        ctx_id: u32,
        resource_id: u32,
    ) -> Result<(), VirtioError> {
        let Some(idx) = self
            .blobs
            .iter()
            .position(|s| s.owner == owner && s.ctx_id == ctx_id && s.resource_id == resource_id)
        else {
            return Ok(());
        };
        let slot = self.blobs.swap_remove(idx);
        if slot.mapped {
            let _ = self.resource_unmap_blob(slot.resource_id);
            self.free_window_range(slot.map_offset, slot.map_len);
        }
        if self.take_live_resource(slot.resource_id) {
            let _ = self.ctx_detach_resource(slot.ctx_id, slot.resource_id);
            self.resource_unref(slot.resource_id)
        } else {
            Ok(())
        }
    }

    /// Reclaim every blob still owned by `owner` (a destroyed D3D device handle):
    /// unmap (if mapped), detach, unref, and return the window range. This is the
    /// KMD-side safety net for an ICD that crashes or skips RELEASE_BLOB — without
    /// it the bounded blob table (`MAX_BLOBS`) fills across device creations and
    /// later allocations fail with `STATUS_INSUFFICIENT_RESOURCES`, which manifests
    /// as spurious render corruption / "venus wedge". Returns the count reclaimed.
    /// Called under the virtio spinlock at `DxgkDdiDestroyDevice`; `swap_remove`
    /// never reallocates, so it stays lock-safe.
    /// Current number of tracked blob slots (diagnostics).
    pub fn blob_count(&self) -> usize {
        self.blobs.len()
    }

    pub fn release_blobs_for_owner(&mut self, owner: usize) -> u32 {
        let mut reclaimed = 0u32;
        let mut i = 0;
        while i < self.blobs.len() {
            if self.blobs[i].owner != owner {
                i += 1;
                continue;
            }
            let slot = self.blobs.swap_remove(i);
            if slot.mapped {
                let _ = self.resource_unmap_blob(slot.resource_id);
                self.free_window_range(slot.map_offset, slot.map_len);
            }
            if self.take_live_resource(slot.resource_id) {
                let _ = self.ctx_detach_resource(slot.ctx_id, slot.resource_id);
                let _ = self.resource_unref(slot.resource_id);
            }
            reclaimed += 1;
            // Do not advance `i`: `swap_remove` moved the last element into slot `i`.
        }
        reclaimed
    }

    /// `CTX_DESTROY` and drop every context still owned by `owner`. Complements
    /// [`release_blobs_for_owner`]; together they leave the host with no dangling
    /// venus state for a device that tore down uncleanly. Returns the count.
    pub fn destroy_contexts_for_owner(&mut self, owner: usize) -> u32 {
        let mut destroyed = 0u32;
        let mut i = 0;
        while i < self.contexts.len() {
            if self.contexts[i].owner != owner {
                i += 1;
                continue;
            }
            let slot = self.contexts.swap_remove(i);
            let mut cmd = VirtioGpuCtxDestroy::zeroed();
            cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_DESTROY;
            cmd.hdr.ctx_id = slot.ctx_id;
            let _ = self.ctrl_roundtrip(bytemuck::bytes_of(&cmd));
            destroyed += 1;
            // Do not advance `i`: `swap_remove` filled slot `i` with the last entry.
        }
        destroyed
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
        let end = offset.checked_add(len).ok_or(VirtioError::OutOfMemory)?;
        if end > window_len {
            return Err(VirtioError::OutOfMemory);
        }
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
        }
    }

    /// Bind a resource to its 3D context (`VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE`).
    /// Required before a HOST3D blob can be mapped or used by the venus ring (the
    /// resource id namespace is per-context for venus).
    pub fn ctx_attach_resource(
        &mut self,
        ctx_id: u32,
        resource_id: u32,
    ) -> Result<(), VirtioError> {
        let mut cmd = VirtioGpuCtxResource::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE;
        cmd.hdr.ctx_id = ctx_id;
        cmd.resource_id = resource_id;
        self.ctrl_roundtrip(bytemuck::bytes_of(&cmd))
    }

    /// Detach a resource from its 3D context (inverse of `ctx_attach_resource`).
    pub fn ctx_detach_resource(
        &mut self,
        ctx_id: u32,
        resource_id: u32,
    ) -> Result<(), VirtioError> {
        let mut cmd = VirtioGpuCtxResource::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE;
        cmd.hdr.ctx_id = ctx_id;
        cmd.resource_id = resource_id;
        self.ctrl_roundtrip(bytemuck::bytes_of(&cmd))
    }

    /// Drop the host's reference to a resource (inverse of a create).
    pub fn resource_unref(&mut self, resource_id: u32) -> Result<(), VirtioError> {
        let mut cmd = VirtioGpuResourceUnref::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_UNREF;
        cmd.resource_id = resource_id;
        self.ctrl_roundtrip(bytemuck::bytes_of(&cmd))
    }

    /// The host-visible blob window, or `None` if the device exposes none.
    /// `DxgkDdiQueryAdapterInfo` uses `base`/`len` to describe the CPU-visible
    /// memory segment, and `DxgkDdiBuildPagingBuffer` adds the VidMm-assigned
    /// segment offset to `base` for the user mapping. Gate 5a Stage 2.
    pub fn host_visible(&self) -> Option<HostVisibleWindow> {
        self.host_visible
    }

    /// Mapped kernel VA of the virtio ISR-status register (read-to-clear), or 0 if
    /// the device exposes no ISR cap. `DxgkDdiStartDevice` copies this into the
    /// `AdapterContext` so the DIRQL ISR can acknowledge the INTx line lock-free.
    pub fn isr_status_addr(&self) -> usize {
        self.isr_status_va
    }

    /// Map a HOST3D mappable blob into the host-visible window at `offset` — for
    /// the WDDM path, the offset VidMm assigned the allocation within the
    /// CPU-visible segment, so the host backing lands exactly where dxgkrnl will
    /// expose the pages on `D3DKMTLock2`. Returns the host caching nibble
    /// (`VIRTIO_GPU_MAP_CACHE_*`). Caller holds the AdapterContext spinlock.
    pub fn resource_map_blob(&mut self, resource_id: u32, offset: u64) -> Result<u32, VirtioError> {
        let mut cmd = VirtioGpuResourceMapBlob::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB;
        cmd.resource_id = resource_id;
        cmd.offset = offset;
        let map_info = self.map_blob_roundtrip(&cmd)?;
        Ok(map_info & VIRTIO_GPU_MAP_CACHE_MASK)
    }

    /// Tear down a blob's host-visible mapping (inverse of `resource_map_blob`).
    pub fn resource_unmap_blob(&mut self, resource_id: u32) -> Result<(), VirtioError> {
        let mut cmd = VirtioGpuResourceUnmapBlob::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB;
        cmd.resource_id = resource_id;
        self.ctrl_roundtrip(bytemuck::bytes_of(&cmd))
    }

    /// `RESOURCE_MAP_BLOB` round-trip that reads the host's `map_info` caching
    /// word from the `RESP_OK_MAP_INFO` reply (the generic `ctrl_roundtrip` only
    /// reads the header). Reuses the scratch page (req low / resp high).
    fn map_blob_roundtrip(&mut self, cmd: &VirtioGpuResourceMapBlob) -> Result<u32, VirtioError> {
        let req = bytemuck::bytes_of(cmd);
        let resp_len = core::mem::size_of::<VirtioGpuRespMapInfo>();
        // SAFETY: owned contiguous page; disjoint req/resp halves, serialized by
        // the caller's spinlock.
        let buf = unsafe { core::slice::from_raw_parts_mut(self.scratch.as_ptr(), SCRATCH_BYTES) };
        let (req_buf, resp_buf) = buf.split_at_mut(SCRATCH_BYTES / 2);
        if req.len() > req_buf.len() || resp_len > resp_buf.len() {
            return Err(VirtioError::DeviceError);
        }
        req_buf[..req.len()].copy_from_slice(req);
        self.ctrl_queue_bounded_roundtrip(
            &[&req_buf[..req.len()]],
            &mut [&mut resp_buf[..resp_len]],
        )?;
        let resp: &VirtioGpuRespMapInfo = bytemuck::from_bytes(&resp_buf[..resp_len]);
        if resp_is_ok(resp.hdr.type_) {
            Ok(resp.map_info)
        } else {
            Err(VirtioError::DeviceError)
        }
    }

    /// Gate-2 bring-up validation: prove the venus 3D-context lifecycle works on
    /// the live device — create a context bound to the Venus capset, then destroy
    /// it. Returns `VIRTIO_GPU_RESP_OK_NODATA` (0x1100) on success or `0xFFFF_FFFF`
    /// on failure. This is a real prerequisite for the venus-backed allocation
    /// flow; a standalone HOST3D blob can't be smoke-tested here because it needs a
    /// venus memory id that only the UMD's `vkAllocateMemory` can produce.
    pub fn self_test_venus_context(&mut self) -> u32 {
        // owner = 0: this diagnostic context is destroyed immediately below, so it
        // needs no device-teardown reclamation; ctx_destroy deregisters it anyway.
        match self.ctx_create(VIRTIO_GPU_CAPSET_VENUS, 0) {
            Ok(ctx) => {
                // Best-effort cleanup; ignore the destroy result (diagnostic path).
                let _ = self.ctx_destroy(ctx);
                0x1100
            }
            Err(_) => 0xFFFF_FFFF,
        }
    }

    /// Bounded synchronous round-trip on the control queue: add → notify →
    /// poll the used ring (bounded by [`CTRL_POLL_SPINS`]) → pop. Replaces the
    /// virtio-drivers `add_notify_wait_pop`, whose used-ring poll has NO bound
    /// — with a wedged host that spin ran forever at DISPATCH_LEVEL under the
    /// device spinlock.
    ///
    /// On timeout the transport is POISONED (`self.failed = true`) and every
    /// later round-trip fails fast: the timed-out descriptor is still owned by
    /// the device (it may complete at any time), so the ring state cannot be
    /// trusted again. The request buffers a late completion would DMA-read
    /// live in our owned scratch page (never freed until Drop); the one
    /// exception is the venus payload in `submit_venus`, which the caller owns
    /// — a documented residual risk, removed for good when the C3 async
    /// submission path (ISR/DPC-driven fences) replaces these polled trips.
    fn ctrl_queue_bounded_roundtrip<'a>(
        &mut self,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
    ) -> Result<(), VirtioError> {
        if self.failed {
            return Err(VirtioError::DeviceError);
        }
        // SAFETY: the buffers remain borrowed for the whole call; we either pop
        // the same token below or poison the transport so the in-flight
        // descriptor slot is never handed out again.
        let token = match unsafe { self.control.add(inputs, outputs) } {
            Ok(t) => t,
            Err(_) => return Err(VirtioError::DeviceError),
        };
        if self.control.should_notify() {
            self.transport.notify(CTRL_QUEUE);
        }
        let mut spins = 0u64;
        while !self.control.can_pop() {
            spins += 1;
            if spins >= CTRL_POLL_SPINS {
                self.failed = true;
                CTRL_TIMEOUT_COUNT.fetch_add(1, Ordering::Relaxed);
                return Err(VirtioError::DeviceError);
            }
            core::hint::spin_loop();
        }
        // SAFETY: same buffers as `add`, still valid; `can_pop()` returned true.
        match unsafe { self.control.pop_used(token, inputs, outputs) } {
            Ok(_len) => Ok(()),
            Err(_) => Err(VirtioError::DeviceError),
        }
    }

    /// Send a single-buffer control command (already serialized to `req` bytes)
    /// and return the device's response header `type_` (a `VIRTIO_GPU_RESP_*`
    /// code). `Err` only when the round-trip itself fails to complete. Reuses the
    /// scratch page (request in the low half, response in the high half).
    fn ctrl_roundtrip_typed(&mut self, req: &[u8]) -> Result<u32, VirtioError> {
        let resp_len = core::mem::size_of::<VirtioGpuCtrlHdr>();
        // SAFETY: owned contiguous page; disjoint req/resp halves, serialized by
        // the caller's spinlock.
        let buf = unsafe { core::slice::from_raw_parts_mut(self.scratch.as_ptr(), SCRATCH_BYTES) };
        let (req_buf, resp_buf) = buf.split_at_mut(SCRATCH_BYTES / 2);
        if req.len() > req_buf.len() || resp_len > resp_buf.len() {
            return Err(VirtioError::DeviceError);
        }
        req_buf[..req.len()].copy_from_slice(req);
        self.ctrl_queue_bounded_roundtrip(
            &[&req_buf[..req.len()]],
            &mut [&mut resp_buf[..resp_len]],
        )?;
        let resp: &VirtioGpuCtrlHdr = bytemuck::from_bytes(&resp_buf[..resp_len]);
        Ok(resp.type_)
    }

    /// Send a control command and require a success response.
    fn ctrl_roundtrip(&mut self, req: &[u8]) -> Result<(), VirtioError> {
        if resp_is_ok(self.ctrl_roundtrip_typed(req)?) {
            Ok(())
        } else {
            Err(VirtioError::DeviceError)
        }
    }
}

impl Drop for VirtioGpu {
    fn drop(&mut self) {
        // Quiesce the device (resets queues) so it stops touching the rings we
        // are about to free.
        self.transport.set_status(DeviceStatus::empty());
        // Free the DMA scratch page. The control `VirtQueue` frees its own ring
        // memory on its own drop (via `Hal::dma_dealloc`).
        //
        // The BAR MMIO mappings made inside `PciTransport` are intentionally NOT
        // freed here: `WdkHal` caches them by physical address and reuses them on
        // the next StartDevice (the BARs are stable across stop/start), so there
        // is no per-cycle leak. The cache is released wholesale in
        // `DxgkDdiUnload` via `WdkHal::unmap_all`.
        //
        // SAFETY: `scratch` was returned by `WdkHal::dma_alloc(1, ..)` in `init`
        // and is freed exactly once (here, when the VirtioGpu is dropped).
        unsafe { WdkHal::dma_dealloc(0, self.scratch, 1) };
    }
}
