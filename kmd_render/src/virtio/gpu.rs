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

use bytemuck::Zeroable;
use helios_protocol::{
    resp_is_ok, VirtioGpuCmdSubmit, VirtioGpuCtrlHdr, VirtioGpuCtxCreate, VirtioGpuCtxDestroy,
    VirtioGpuCtxResource, VirtioGpuResourceCreateBlob, VirtioGpuResourceMapBlob,
    VirtioGpuResourceUnmapBlob, VirtioGpuResourceUnref, VirtioGpuRespDisplayInfo,
    VirtioGpuRespMapInfo, HELIOS_OPTIONAL_FEATURES, HELIOS_REQUIRED_FEATURES, VIRTIO_GPU_CAPSET_VENUS,
    VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE, VIRTIO_GPU_CMD_CTX_CREATE,
    VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE, VIRTIO_GPU_CMD_CTX_DESTROY,
    VIRTIO_GPU_CMD_GET_DISPLAY_INFO, VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB,
    VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB, VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB,
    VIRTIO_GPU_CMD_RESOURCE_UNREF, VIRTIO_GPU_CMD_SUBMIT_3D, VIRTIO_GPU_FLAG_FENCE,
    VIRTIO_GPU_MAP_CACHE_MASK, VIRTIO_GPU_SHM_ID_HOST_VISIBLE, VIRTIO_PCI_CAP_SHARED_MEMORY_CFG,
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
                let off =
                    cfg_read32(access, cap + 8) as u64 | ((cfg_read32(access, cap + 16) as u64) << 32);
                let len =
                    cfg_read32(access, cap + 12) as u64 | ((cfg_read32(access, cap + 20) as u64) << 32);
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

        control
            .add_notify_wait_pop(
                &[&req_buf[..hdr_len]],
                &mut [&mut resp_buf[..resp_len]],
                &mut transport,
            )
            .map_err(|_| VirtioError::DeviceError)?;

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

        let mut gpu = Self {
            transport,
            control,
            scratch,
            next_ctx_id: AtomicU32::new(1),
            next_resource_id: AtomicU32::new(1),
            host_visible,
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
    /// the guest-assigned context id.
    pub fn ctx_create(&mut self, capset_id: u32) -> Result<u32, VirtioError> {
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
        self.ctrl_roundtrip(bytemuck::bytes_of(&cmd))?;
        Ok(ctx_id)
    }

    /// Destroy a previously created 3D context.
    pub fn ctx_destroy(&mut self, ctx_id: u32) -> Result<(), VirtioError> {
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
        venus: &[u8],
    ) -> Result<(), VirtioError> {
        if venus.is_empty() {
            return Err(VirtioError::DeviceError);
        }
        let mut cmd = VirtioGpuCmdSubmit::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_SUBMIT_3D;
        cmd.hdr.flags = VIRTIO_GPU_FLAG_FENCE;
        cmd.hdr.fence_id = fence_id;
        cmd.hdr.ctx_id = ctx_id;
        cmd.size = venus.len() as u32;

        let hdr_len = core::mem::size_of::<VirtioGpuCmdSubmit>();
        let resp_len = core::mem::size_of::<VirtioGpuCtrlHdr>();
        // SAFETY: `scratch` is our owned contiguous page; the low half holds the
        // submit header (device-read), the high half the response (device-write).
        // Disjoint halves; serialized by the caller's spinlock.
        let buf = unsafe { core::slice::from_raw_parts_mut(self.scratch.as_ptr(), SCRATCH_BYTES) };
        let (hdr_buf, resp_buf) = buf.split_at_mut(SCRATCH_BYTES / 2);
        hdr_buf[..hdr_len].copy_from_slice(bytemuck::bytes_of(&cmd));

        // Two device-readable descriptors (submit header + Venus stream) and one
        // device-writable response descriptor (TRANSPORT §7 two-descriptor + resp).
        self.control
            .add_notify_wait_pop(
                &[&hdr_buf[..hdr_len], venus],
                &mut [&mut resp_buf[..resp_len]],
                &mut self.transport,
            )
            .map_err(|_| VirtioError::DeviceError)?;
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
        Ok(resource_id)
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
        self.control
            .add_notify_wait_pop(
                &[&req_buf[..req.len()]],
                &mut [&mut resp_buf[..resp_len]],
                &mut self.transport,
            )
            .map_err(|_| VirtioError::DeviceError)?;
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
        match self.ctx_create(VIRTIO_GPU_CAPSET_VENUS) {
            Ok(ctx) => {
                // Best-effort cleanup; ignore the destroy result (diagnostic path).
                let _ = self.ctx_destroy(ctx);
                0x1100
            }
            Err(_) => 0xFFFF_FFFF,
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
        self.control
            .add_notify_wait_pop(
                &[&req_buf[..req.len()]],
                &mut [&mut resp_buf[..resp_len]],
                &mut self.transport,
            )
            .map_err(|_| VirtioError::DeviceError)?;
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
