# KMD.md — Kernel-Mode Driver Implementation Guide

> **⚠️ SUPERSEDED (2026-07-05) — ARCHIVED REFERENCE ONLY.** This guide covers the
> abandoned System-class KMDF `kmd/` crate (IOCTL device interface, no dxgkrnl).
> The active kernel driver is the **WDDM 3.2 render-only miniport `kmd_render/`**:
> it binds via dxgkrnl, implements the full DDI table (`query_adapter_info`,
> `create_allocation`, `build_paging_buffer`, `submit_command`, `escape`, …), and
> uses GpuMmu + monitored fences + CpuHostAperture segments + venus-over-
> `D3DKMTEscape`. **CLAUDE.md and ROADMAP.md are authoritative** (references below
> to `ARCH.md` as "canonical" are stale — ARCH.md is itself archived).

## Overview

The KMD (Kernel-Mode Driver) is a **System-class KMDF function driver** for the virtio-gpu PCI device. It lives in the Windows kernel, talks to the virtio-gpu device, and exposes the virtio-gpu Venus transport to user mode via a **DeviceIoControl device interface** (`GUID_DEVINTERFACE_HELIOS`). It is **not** a display/WDDM miniport: there is no dxgkrnl, no GPU-VA / segment / monitored-fence contract, and no user-mode display driver. The Vulkan ICD (a Windows port of Mesa's `venus`) reaches the KMD purely through IOCTLs on that device interface, and is enumerated independently by the Windows Vulkan loader via the Khronos registry JSON.

See `ARCH.md` (canonical) and `SYSTEM_CLASS_REFOCUS_2026_06_07.md` for the active architecture; this guide is the implementation companion for the `kmd/` crate.

**References:**
- KMDF getting started: https://learn.microsoft.com/en-us/windows-hardware/drivers/wdf/getting-started-with-kmdf
- WdfDriverCreate: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdfdriver/nf-wdfdriver-wdfdrivercreate
- WdfDeviceCreateDeviceInterface: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdfdevice/nf-wdfdevice-wdfdevicecreatedeviceinterface
- EvtIoDeviceControl: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdfio/nc-wdfio-evt_wdf_io_queue_io_device_control

---

## ⚠️ Implementation Reality (read before the code below)

The working, building source under `kmd/` is the ground truth — prefer it over the snippets below. Key facts for the System-class KMDF model (confirmed against the WDK 10.0.26100 / KMDF 1.33 bindings):

- **KMDF active build — `wdk-sys` + WDF.** The driver uses the WDF function table that `wdk-sys` auto-wires; calls go through `wdk_sys::call_unsafe_wdf_function_binding!`. The active build should not link `displib` or depend on `DxgkInitialize*`, `DRIVER_INITIALIZATION_DATA`, `DXGK_DRIVERCAPS`, the QUERYSEGMENT/GPUMMU caps, the render-DDI list, or any `DXGK*` symbol. The DOD-scoped `dispmprt.h`/`d3dkmddi.h` bindgen / `src/dxgk.rs` may remain in the repository as archived reference material, but it is not part of the active System-class KMDF path.
- **`Cargo.toml` driver-type is `KMDF`** with `kmdf-version` set (1.33); the bindgen build-dependency is dropped.
- **Panic handler:** do **not** define your own `#[panic_handler]` — just `extern crate wdk_panic;` (it supplies one; a second is a duplicate lang item).
- **Build on local disk, never `Z:\`** (cargo/wdk IO fails on the 9p share — OS error 87, windows-drivers-rs#481): the `win` MCP `win_cargo` tool robocopy-mirrors to `C:\Users\Rupansh\helios-vgpu` and builds there. See TOOLCHAIN.md.

---

## Phase 1: DriverEntry and KMDF Skeleton

### 1.1 `src/lib.rs` — Entry Point

```rust
#![no_std]
#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]

extern crate alloc;

use wdk_alloc::WdkAllocator;
extern crate wdk_panic; // supplies the #[panic_handler]; do NOT define a second one
use wdk_sys::{
    ntddk::*,
    *,
};

// Required: kernel allocator
#[global_allocator]
static ALLOCATOR: WdkAllocator = WdkAllocator;

mod adapter;
mod interrupt;
mod ioctl;
mod pnp;
mod virtio;

/// DriverEntry: registered in the INF as the driver entrypoint.
/// Called by the OS when the driver is first loaded.
///
/// # Safety
/// Called by the OS with valid pointers. Standard WDM contract.
#[export_name = "DriverEntry"]
pub unsafe extern "system" fn driver_entry(
    driver_object: *mut DRIVER_OBJECT,
    registry_path: *mut UNICODE_STRING,
) -> NTSTATUS {
    // Build the WDF driver config with our single PnP callback,
    // then hand off to the framework. No DDI table — KMDF wires
    // PnP/power/IO through the WDF object model.
    let mut config = WDF_DRIVER_CONFIG {
        Size: core::mem::size_of::<WDF_DRIVER_CONFIG>() as u32,
        EvtDriverDeviceAdd: Some(pnp::evt_device_add),
        ..unsafe { core::mem::zeroed() }
    };

    // SAFETY: driver_object and registry_path are valid for the call; the WDF
    //         function-table macro forwards to the framework's WdfDriverCreate.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver_object,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut config,
            WDF_NO_HANDLE as *mut _,
        )
    };
    status
}
```

`evt_device_add` (in `pnp.rs`) builds the device object, registers the IOCTL device interface, and creates the default IO queue:

```rust
// src/pnp.rs (sketch)

pub unsafe extern "C" fn evt_device_add(
    _driver: WDFDRIVER,
    device_init: PWDFDEVICE_INIT,
) -> NTSTATUS {
    // 1. PnP/power callbacks (prepare/release hardware, D0 entry/exit).
    let mut pnp = WDF_PNPPOWER_EVENT_CALLBACKS {
        Size: core::mem::size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>() as u32,
        EvtDevicePrepareHardware: Some(evt_device_prepare_hardware),
        EvtDeviceReleaseHardware: Some(evt_device_release_hardware),
        ..unsafe { core::mem::zeroed() }
    };
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitSetPnpPowerEventCallbacks, device_init, &mut pnp);
    }

    // 2. Typed context (AdapterContext) attached to the device object.
    let mut attribs = WDF_OBJECT_ATTRIBUTES::default();
    // WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(AdapterContext) equivalent…

    // 3. Create the device.
    let mut device: WDFDEVICE = core::ptr::null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate, &mut device_init, &mut attribs, &mut device)
    };
    if !nt_success(status) { return status; }

    // 4. Register the IOCTL device interface (auto-enables on D0).
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateDeviceInterface,
            device,
            &helios_protocol::escape::GUID_DEVINTERFACE_HELIOS,
            core::ptr::null_mut());
    }

    // 5. Default parallel IO queue dispatching EvtIoDeviceControl.
    let mut queue_config = WDF_IO_QUEUE_CONFIG::default(); // INIT_DEFAULT_QUEUE(Parallel)
    queue_config.EvtIoDeviceControl = Some(crate::ioctl::evt_io_device_control);
    let mut queue: WDFQUEUE = core::ptr::null_mut();
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoQueueCreate, device, &mut queue_config,
            WDF_NO_OBJECT_ATTRIBUTES, &mut queue)
    }
}
```

There is **no** user-mode driver registration of any kind (no `UserModeDriverName`, no `OpenAdapter` handshake). The KMD exposes only the device interface; the Vulkan ICD is independent (registry JSON, see ARCH.md §7).

---

## Phase 1: evt_device_add — Device Context

`evt_device_add` is called when the PnP manager binds the driver to a matching device. It creates the WDF device object and attaches the `AdapterContext` typed context. Retrieve the context anywhere via `WdfObjectGetTypedContext(device, AdapterContext::type_info())`.

```rust
// src/adapter.rs

use wdk_sys::*;

pub struct AdapterContext {
    /// VirtIO device state, guarded by an internal spinlock.
    pub virtio: Option<crate::virtio::VirtioGpu>,
    /// Spinlock guarding `virtio` for parallel IOCTL / DPC access.
    pub virtio_lock: KSPIN_LOCK,
    /// Fence sequence counter.
    pub fence_seq: u64,
    /// fence_id -> KEVENT table for the async WAIT_FENCE path.
    pub fences: crate::adapter::FenceTable,
}

// SAFETY: the spinlock serializes access to `virtio`; the fence table is
// likewise lock-guarded. WDF owns the context's lifetime.
unsafe impl Send for AdapterContext {}
unsafe impl Sync for AdapterContext {}

impl AdapterContext {
    /// `with_virtio` runs a closure under `virtio_lock`; `set_virtio` installs
    /// or clears the transport during prepare/release hardware.
    pub fn with_virtio<R>(&self, f: impl FnOnce(&mut crate::virtio::VirtioGpu) -> R)
        -> Result<R, DriverError> { /* acquire virtio_lock, run f */ todo!() }
    pub fn set_virtio(&mut self, v: Option<crate::virtio::VirtioGpu>) { /* … */ }
}

pub enum DriverError {
    InsufficientResources,
    InvalidParameter,
    DeviceNotFound,
    IoError,
}

impl DriverError {
    pub fn into_ntstatus(self) -> NTSTATUS {
        match self {
            Self::InsufficientResources => STATUS_INSUFFICIENT_RESOURCES,
            Self::InvalidParameter      => STATUS_INVALID_PARAMETER,
            Self::DeviceNotFound        => STATUS_DEVICE_DOES_NOT_EXIST,
            Self::IoError               => STATUS_IO_DEVICE_ERROR,
        }
    }
}
```

The context **drops** the WDDM-era `dxgkrnl: Option<DXGKRNL_INTERFACE>` field and the `pdo`/`dxgkrnl()` accessor — no Dxgkrnl exists under KMDF. It **adds** the `fence_id -> KEVENT` table consumed by `WAIT_FENCE` (signalled from the interrupt DPC).

---

## Phase 1: EvtDevicePrepareHardware / EvtDeviceReleaseHardware

`EvtDevicePrepareHardware` is the critical initialization callback — it maps hardware resources and brings up the transport. KMDF hands it the raw and translated CM resource lists.

**Reference:** https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdfdevice/nc-wdfdevice-evt_wdf_device_prepare_hardware

```rust
// src/pnp.rs

pub unsafe extern "C" fn evt_device_prepare_hardware(
    device: WDFDEVICE,
    _resources_raw: WDFCMRESLIST,
    resources_translated: WDFCMRESLIST,
) -> NTSTATUS {
    let adapter = unsafe { &mut *adapter_context(device) };

    // 1. Walk the translated resource list; map each memory BAR.
    let count = unsafe {
        call_unsafe_wdf_function_binding!(WdfCmResourceListGetCount, resources_translated)
    };
    for i in 0..count {
        let desc = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfCmResourceListGetDescriptor, resources_translated, i)
        };
        // SAFETY: desc points to a valid CM_PARTIAL_RESOURCE_DESCRIPTOR.
        let desc = unsafe { &*desc };
        if desc.Type == CmResourceTypeMemory as u8 {
            // MmMapIoSpaceEx the BAR's Start/Length; record base for the cap scan.
            // …
        }
    }

    // 2. Obtain the PCI bus interface for config-space access.
    let mut bus_if: BUS_INTERFACE_STANDARD = unsafe { core::mem::zeroed() };
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfFdoQueryForInterface,
            device,
            &GUID_BUS_INTERFACE_STANDARD,
            &mut bus_if as *mut _ as *mut INTERFACE,
            core::mem::size_of::<BUS_INTERFACE_STANDARD>() as u16,
            1, // version
            core::ptr::null_mut())
    };
    if !nt_success(status) { return status; }

    // 3. Bring up the transport over the config-access shim, then store it.
    let cfg = crate::virtio::KmdfConfigAccess::new(bus_if);
    let virtio = match crate::virtio::VirtioGpu::init(&cfg /* + mapped BARs */) {
        Ok(v) => v,
        Err(e) => return e.into_ntstatus(),
    };
    adapter.set_virtio(Some(virtio));

    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_device_release_hardware(
    device: WDFDEVICE,
    _resources_translated: WDFCMRESLIST,
) -> NTSTATUS {
    let adapter = unsafe { &mut *adapter_context(device) };
    adapter.set_virtio(None);
    // MmUnmapIoSpace each mapped BAR.
    STATUS_SUCCESS
}
```

The WDF interrupt object is created here (or in `evt_device_add`) with `WdfInterruptCreate`; see Interrupt Handling below.

---

## Capability advertisement is gone

There is no `QueryAdapterInfo`/`DXGK_DRIVERCAPS`/segment/GpuMmu cap surface anymore — the host owns all GPU memory and scheduling under Venus replay, so the guest driver advertises nothing to a graphics stack. User mode reaches the device purely through the IOCTL interface below.

---

## Phase 2: VirtIO PCI Initialization

> **STATUS (2026-06-04): Phase 2 COMPLETE — implemented via the `virtio-drivers` 0.13 crate, NOT the hand-rolled code in this section.** The `struct Virtqueue` + helpers below (`add_buffer`/`notify_head`/`poll_used`/`free_chain`/`desc_phys`, and the `VirtioGpuCmd*` redefinitions) **do not exist in the codebase** — they are reference-only prose. The real transport is `VirtQueue<WdkHal, 64>` + `PciTransport` in `kmd/src/virtio/{gpu,hal,config}.rs` (`WdkHal` impls `virtio_drivers::Hal`; `KmdfConfigAccess` impls `ConfigurationAccess` over a `BUS_INTERFACE_STANDARD` obtained from `WdfFdoQueryForInterface` — its `read_word`/`write_word` call `GetBusData`/`SetBusData` on `PCI_WHICHSPACE_CONFIG`). Wire structs come from `helios_protocol`. `GET_DISPLAY_INFO` round-trips on real HW. See the `phase2-virtio-drivers` memory.

### VirtIO PCI Device Layout

The virtio-gpu device presents as PCI vendor `0x1AF4`, device `0x1050`.

VirtIO devices use PCI capabilities to describe their config regions:

| Cap type | Meaning | BAR use |
|----------|---------|---------|
| 1 (COMMON_CFG) | Common config (feature bits, queue config) | BAR varies |
| 2 (NOTIFY_CFG) | Queue notification doorbells | BAR varies |
| 3 (ISR_CFG) | Interrupt status register | BAR varies |
| 4 (DEVICE_CFG) | Device-specific config (display info) | BAR varies |
| 8 (PCI_CFG) | Alternative config access | N/A |

**Reference:** VirtIO spec §4.1 https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html#sec-virtio-over-pci-bus

### VirtIO Command Structures

Key structures from `virtio_gpu.h` (must match virglrenderer's definitions):

```rust
// src/virtio/gpu.rs

use bytemuck::{Pod, Zeroable};

/// VirtIO GPU control command header (prepended to every command)
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuCtrlHdr {
    pub type_: u32,   // VIRTIO_GPU_CMD_* or VIRTIO_GPU_RESP_*
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
    pub padding: [u8; 3],
}

// Command types
pub const VIRTIO_GPU_CMD_GET_DISPLAY_INFO:  u32 = 0x0100;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
pub const VIRTIO_GPU_CMD_RESOURCE_UNREF:    u32 = 0x0102;
pub const VIRTIO_GPU_CMD_SET_SCANOUT:       u32 = 0x0103;
pub const VIRTIO_GPU_CMD_RESOURCE_FLUSH:    u32 = 0x0104;
pub const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
pub const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
pub const VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
pub const VIRTIO_GPU_CMD_CTX_CREATE:        u32 = 0x0200;
pub const VIRTIO_GPU_CMD_CTX_DESTROY:       u32 = 0x0201;
pub const VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
pub const VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
pub const VIRTIO_GPU_CMD_SUBMIT_3D:         u32 = 0x0204;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB: u32 = 0x0208;
pub const VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB:   u32 = 0x0209;
pub const VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB: u32 = 0x020a;

// Response types
pub const VIRTIO_GPU_RESP_OK_NODATA:        u32 = 0x1100;
pub const VIRTIO_GPU_RESP_OK_DISPLAY_INFO:  u32 = 0x1101;
pub const VIRTIO_GPU_RESP_ERR_UNSPEC:       u32 = 0x1200;
pub const VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
pub const VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
pub const VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
pub const VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;

// Flags
pub const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;

/// Context creation (for Venus: capset_id = VIRTIO_GPU_CAPSET_VENUS = 4)
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuCtxCreate {
    pub hdr: VirtioGpuCtrlHdr,
    pub nlen: u32,
    pub context_init: u32,  // capset_id for Venus
    pub debug_name: [u8; 64],
}

pub const VIRTIO_GPU_CAPSET_VIRGL:  u32 = 1;
pub const VIRTIO_GPU_CAPSET_VIRGL2: u32 = 2;
pub const VIRTIO_GPU_CAPSET_VENUS:  u32 = 4;  // Vulkan via Venus

/// Submit 3D command (Venus commands go here)
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuCmdSubmit {
    pub hdr: VirtioGpuCtrlHdr,
    pub size: u32,
    pub padding: u32,
    // Followed by `size` bytes of command data
}

/// Blob resource creation (zero-copy memory between guest and host)
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuResourceCreateBlob {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub nr_entries: u32,
    pub blob_id: u64,
    pub size: u64,
}

pub const VIRTIO_GPU_BLOB_MEM_GUEST:           u32 = 1;
pub const VIRTIO_GPU_BLOB_MEM_HOST3D:          u32 = 2;
pub const VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST:    u32 = 3;
pub const VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE:   u32 = 1;
pub const VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE:  u32 = 2;
pub const VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE: u32 = 4;
```

### Virtqueue Implementation

```rust
// src/virtio/queue.rs
// 
// A split virtqueue as per VirtIO spec §2.7.
// Reference: https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html#sec-split-virtqueues

use wdk_sys::ntddk::*;
use wdk_sys::*;

const QUEUE_SIZE: usize = 256; // must be power of 2

/// Descriptor table entry (16 bytes each)
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VirtqDesc {
    addr:  u64,   // guest physical address
    len:   u32,
    flags: u16,   // VIRTQ_DESC_F_*
    next:  u16,   // next descriptor index (if VIRTQ_DESC_F_NEXT)
}

const VIRTQ_DESC_F_NEXT:     u16 = 1;
const VIRTQ_DESC_F_WRITE:    u16 = 2; // device-writable (for responses)
const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// Available ring (driver → device)
#[repr(C)]
#[derive(Debug)]
struct VirtqAvail {
    flags: u16,
    idx:   u16,
    ring:  [u16; QUEUE_SIZE],
    used_event: u16,
}

/// Used ring entry
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VirtqUsedElem {
    id:  u32,
    len: u32,
}

/// Used ring (device → driver)
#[repr(C)]
#[derive(Debug)]
struct VirtqUsed {
    flags: u16,
    idx:   u16,
    ring:  [VirtqUsedElem; QUEUE_SIZE],
    avail_event: u16,
}

pub struct Virtqueue {
    /// Descriptor table (QUEUE_SIZE entries × 16 bytes)
    desc_phys:  u64,
    desc_virt:  *mut VirtqDesc,
    /// Available ring
    avail_phys: u64,
    avail_virt: *mut VirtqAvail,
    /// Used ring
    used_phys:  u64,
    used_virt:  *mut VirtqUsed,
    /// Notification address (doorbell)
    notify_addr: *mut u16,
    notify_mult: u32,         // multiply queue_notify_off by this
    /// Free descriptor management
    free_head: u16,
    num_free: u16,
    /// Last seen used idx (for processing completions)
    last_used_idx: u16,
    /// Queue index (0 = ctrl, 1 = cursor)
    queue_idx: u16,
}

impl Virtqueue {
    /// Allocate and initialize the virtqueue.
    ///
    /// # Safety
    /// notify_addr must point to a valid MMIO doorbell register.
    pub unsafe fn new(
        queue_idx: u16,
        notify_addr: *mut u16,
        notify_mult: u32,
    ) -> Result<Self, super::VirtioError> {
        // Allocate contiguous physical memory for descriptor + avail + used rings.
        // Total: (16 * QUEUE_SIZE) + (6 + 2*QUEUE_SIZE) + (6 + 8*QUEUE_SIZE) bytes
        // Align to 4096 bytes.
        //
        // For simplicity, allocate one contigious MDL-pinned buffer.
        // TODO: In production, use MmAllocateContiguousMemory or separate MDLs.

        let desc_size  = core::mem::size_of::<VirtqDesc>() * QUEUE_SIZE;
        let avail_size = 6 + 2 * QUEUE_SIZE;
        let used_size  = 6 + 8 * QUEUE_SIZE;
        let total = desc_size + avail_size + used_size + 4096; // extra for alignment

        // SAFETY: PASSIVE_LEVEL, non-paged pool
        let virt = unsafe {
            ExAllocatePool2(POOL_FLAG_NON_PAGED, total as u64, u32::from_be_bytes(*b"VRTQ"))
        };
        if virt.is_null() {
            return Err(super::VirtioError::OutOfMemory);
        }
        // Zero-initialize
        unsafe { core::ptr::write_bytes(virt as *mut u8, 0, total); }

        // Get physical address
        let phys = unsafe { MmGetPhysicalAddress(virt) };

        let desc_virt  = virt as *mut VirtqDesc;
        let avail_virt = unsafe { (virt as *mut u8).add(desc_size) as *mut VirtqAvail };
        let used_virt  = unsafe { (virt as *mut u8).add(desc_size + avail_size) as *mut VirtqUsed };

        // Initialize free list (linked list via desc.next)
        for i in 0..(QUEUE_SIZE - 1) {
            unsafe { (*desc_virt.add(i)).next = (i + 1) as u16; }
        }
        unsafe { (*desc_virt.add(QUEUE_SIZE - 1)).next = 0; }

        Ok(Self {
            desc_phys: phys.QuadPart as u64,
            desc_virt,
            avail_phys: phys.QuadPart as u64 + desc_size as u64,
            avail_virt,
            used_phys: phys.QuadPart as u64 + desc_size as u64 + avail_size as u64,
            used_virt,
            notify_addr,
            notify_mult,
            free_head: 0,
            num_free: QUEUE_SIZE as u16,
            last_used_idx: 0,
            queue_idx,
        })
    }

    /// Allocate a descriptor chain for a scatter-gather I/O.
    /// `bufs`: list of (phys_addr, len, writable) tuples.
    pub fn add_buffer(
        &mut self,
        bufs: &[(u64, u32, bool)],
    ) -> Option<u16> {
        if bufs.len() > self.num_free as usize { return None; }

        let head = self.free_head;
        let mut idx = head;

        for (i, &(addr, len, writable)) in bufs.iter().enumerate() {
            let desc = unsafe { &mut *self.desc_virt.add(idx as usize) };
            desc.addr  = addr;
            desc.len   = len;
            desc.flags = if writable { VIRTQ_DESC_F_WRITE } else { 0 };
            if i + 1 < bufs.len() {
                desc.flags |= VIRTQ_DESC_F_NEXT;
                desc.next   = unsafe { (*self.desc_virt.add(idx as usize)).next };
                idx         = desc.next;
            }
        }

        self.free_head = unsafe { (*self.desc_virt.add(idx as usize)).next };
        self.num_free -= bufs.len() as u16;

        Some(head)
    }

    /// Make a descriptor chain available to the device.
    pub fn notify_head(&mut self, head: u16) {
        let avail = unsafe { &mut *self.avail_virt };
        let pos = avail.idx as usize & (QUEUE_SIZE - 1);
        avail.ring[pos] = head;
        // Memory barrier: ensure descriptor writes are visible before idx update
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        avail.idx = avail.idx.wrapping_add(1);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        // Kick the device (write queue index to doorbell)
        unsafe { core::ptr::write_volatile(self.notify_addr, self.queue_idx); }
    }

    /// Process used ring, returning completed descriptor head indices.
    pub fn poll_used(&mut self, completed: &mut [u16]) -> usize {
        let used = unsafe { &*self.used_virt };
        let mut count = 0;
        while self.last_used_idx != used.idx && count < completed.len() {
            let pos = self.last_used_idx as usize & (QUEUE_SIZE - 1);
            completed[count] = used.ring[pos].id as u16;
            self.last_used_idx = self.last_used_idx.wrapping_add(1);
            count += 1;
        }
        count
    }

    /// Free a descriptor chain back to the free list.
    pub fn free_chain(&mut self, head: u16) {
        let mut idx = head;
        loop {
            let desc = unsafe { &mut *self.desc_virt.add(idx as usize) };
            let has_next = desc.flags & VIRTQ_DESC_F_NEXT != 0;
            let next = desc.next;
            desc.flags = 0;
            self.num_free += 1;
            if !has_next { break; }
            idx = next;
        }
        // Prepend to free list
        unsafe { (*self.desc_virt.add(idx as usize)).next = self.free_head; }
        self.free_head = head;
    }

    /// Physical address of the descriptor table (for device config)
    pub fn desc_phys(&self) -> u64 { self.desc_phys }
    pub fn avail_phys(&self) -> u64 { self.avail_phys }
    pub fn used_phys(&self) -> u64 { self.used_phys }
}
```

---

## Phase 3: IOCTL Interface

User mode reaches the KMD through `DeviceIoControl` on the device interface `GUID_DEVINTERFACE_HELIOS` (defined once in `helios_protocol` so the KMD and the Vulkan ICD share the constant). The ICD discovers and opens it with `SetupDiGetClassDevs(DIGCF_DEVICEINTERFACE|DIGCF_PRESENT)` → `SetupDiEnumDeviceInterfaces` → `SetupDiGetDeviceInterfaceDetail` → `CreateFileW` → `DeviceIoControl`.

The six wire ops keep their exact `helios_protocol` layout (`protocol/src/escape.rs`); they simply move from the old D3DKMTEscape carrier to IOCTL input/output buffers. The IOCTL **control code is the verb**, and WDF validates the in/out buffer lengths, so the 16-byte `HeliosEscapeHeader` (magic/cmd_type/version/size) becomes redundant — keep it only as an optional cheap version sanity check.

### IOCTL constants

`CTL_CODE(DeviceType, Function, Method, Access) = (DeviceType<<16)|(Access<<14)|(Function<<2)|Method`, with `DeviceType = FILE_DEVICE_UNKNOWN (0x22)`, `Access = FILE_READ_DATA|FILE_WRITE_DATA (3)`, function base `0x900` (vendor range), methods `BUFFERED=0 / IN_DIRECT=1 / OUT_DIRECT=2 / NEITHER=3`. The constants live in `helios_protocol::ioctl` (asserted source of truth):

| Op | IOCTL constant | Value | Method |
|----|----------------|-------|--------|
| CTX_CREATE   | `IOCTL_HELIOS_CTX_CREATE`   | `0x0022E400` | BUFFERED  |
| CTX_DESTROY  | `IOCTL_HELIOS_CTX_DESTROY`  | `0x0022E404` | BUFFERED  |
| SUBMIT_VENUS | `IOCTL_HELIOS_SUBMIT_VENUS` | `0x0022E409` | IN_DIRECT |
| ALLOC_BLOB   | `IOCTL_HELIOS_ALLOC_BLOB`   | `0x0022E40C` | BUFFERED  |
| MAP_BLOB     | `IOCTL_HELIOS_MAP_BLOB`     | `0x0022E412` | OUT_DIRECT |
| WAIT_FENCE   | `IOCTL_HELIOS_WAIT_FENCE`   | `0x0022E414` | BUFFERED  |

**Method rationale:** small fixed verbs use `METHOD_BUFFERED` (the I/O manager double-buffers the system buffer). `SUBMIT_VENUS`'s Venus stream can be megabytes, so it uses `METHOD_IN_DIRECT`: a small fixed header (`ctx_id`/`fence_id`/`buffer_size`) rides the buffered system buffer while the variable Venus blob arrives as a locked MDL (`WdfRequestRetrieveInputWdmMdl`). `MAP_BLOB` returns a **user VA**, not a GPA: the kernel does `MmMapLockedPagesSpecifyCache(blobMdl, UserMode, …)` and writes the resulting user VA into the OUT buffer — hence the op struct field is `out_user_va` (renamed from the old `out_gpa`).

### Dispatch

`evt_io_device_control` switches on the control code, retrieves the buffers, runs the **same op bodies as before** against the transport, and completes the request:

```rust
// src/ioctl.rs

use helios_protocol::escape::*;
use wdk_sys::*;

pub unsafe extern "C" fn evt_io_device_control(
    queue: WDFQUEUE,
    request: WDFREQUEST,
    _output_buffer_length: usize,
    _input_buffer_length: usize,
    io_control_code: u32,
) {
    let device = unsafe {
        call_unsafe_wdf_function_binding!(WdfIoQueueGetDevice, queue)
    };
    let adapter = unsafe { &mut *crate::pnp::adapter_context(device) };

    // Retrieve input/output buffers (buffered) or the input MDL (SUBMIT_VENUS),
    // validating every guest-supplied size/offset against the WDF-reported
    // lengths before use. Reads use bytemuck::pod_read_unaligned. This logic
    // ports 1:1 from the former escape handler.
    let (status, bytes_returned) = match io_control_code {
        IOCTL_HELIOS_CTX_CREATE => {
            // WdfRequestRetrieveInputBuffer/OutputBuffer -> HeliosCtxCreate
            // adapter.with_virtio(|v| v.ctx_create(capset_id)) -> out_ctx_id
            todo!()
        }
        IOCTL_HELIOS_CTX_DESTROY => {
            // adapter.with_virtio(|v| v.ctx_destroy(ctx_id))
            todo!()
        }
        IOCTL_HELIOS_SUBMIT_VENUS => {
            // header from input buffer + Venus blob from WdfRequestRetrieveInputWdmMdl
            // adapter.with_virtio(|v| v.submit_3d(ctx_id, fence_id, mdl_bytes))
            todo!()
        }
        IOCTL_HELIOS_ALLOC_BLOB => {
            // adapter.with_virtio(|v| v.alloc_blob(..)) -> out_resource_id
            todo!()
        }
        IOCTL_HELIOS_MAP_BLOB => {
            // adapter.with_virtio(|v| v.map_blob(resource_id)) -> out_user_va
            todo!()
        }
        IOCTL_HELIOS_WAIT_FENCE => {
            // KeWaitForSingleObject on the fence_id -> KEVENT (timeout_ns)
            todo!()
        }
        _ => (STATUS_INVALID_DEVICE_REQUEST, 0usize),
    };

    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCompleteWithInformation, request, status, bytes_returned as u64)
    };
}
```

**SUBMIT_VENUS** maps to `VirtioGpu::submit_3d` (kmd/src/virtio/gpu.rs) over the virtio-drivers token API (`VirtQueue::{add_notify_wait_pop, add, pop_used}`). The KMD forwards opaque Venus bytes; encoding is the ICD's job.

**Trust boundary (preserved):** every guest-supplied size/offset is validated against the WDF-reported in/out length before use; reads use `pod_read_unaligned`. See `protocol/src/escape.rs` for the op-struct layouts.

---

## Phase 3: Interrupt Handling

The KMDF interrupt object is created with `WdfInterruptCreate` (during prepare-hardware or device-add), naming an ISR and a DPC.

```rust
// src/interrupt.rs

/// EvtInterruptIsr — runs at DIRQL. Must be fast. No allocations, no pageable code.
pub unsafe extern "C" fn evt_interrupt_isr(
    interrupt: WDFINTERRUPT,
    _message_id: u32,
) -> BOOLEAN {
    let device = unsafe {
        call_unsafe_wdf_function_binding!(WdfInterruptGetDevice, interrupt)
    };
    let adapter = unsafe { &*crate::pnp::adapter_context(device) };

    // Acknowledge the interrupt at the device (also tells us if it was ours).
    let ours = adapter.with_virtio(|v| v.transport_ack_interrupt()).unwrap_or(false);
    if !ours { return 0; } // not our interrupt

    // Defer used-ring processing to the DPC.
    unsafe { call_unsafe_wdf_function_binding!(WdfInterruptQueueDpcForIsr, interrupt) };
    1
}

/// EvtInterruptDpc — runs at DISPATCH_LEVEL after the ISR.
pub unsafe extern "C" fn evt_interrupt_dpc(
    interrupt: WDFINTERRUPT,
    _associated_object: WDFOBJECT,
) {
    let device = unsafe {
        call_unsafe_wdf_function_binding!(WdfInterruptGetDevice, interrupt)
    };
    let adapter = unsafe { &*crate::pnp::adapter_context(device) };

    // Pop completed tokens from the used ring under the queue lock, map each
    // token -> fence_id, and signal the per-fence KEVENT so a pending
    // WAIT_FENCE wakes. Interim fence model: fence_id <-> KEVENT table.
    let _ = adapter.with_virtio(|v| {
        while let Some(token) = v.pop_used() {
            if let Some(fid) = v.fence_for_token(token) {
                // KeSetEvent on adapter.fences[fid]
                let _ = fid;
            }
        }
    });
}
```

All `DxgkCbNotifyInterrupt` / `DxgkCbQueueDpc` / `DXGKARGCB_*` monitored-fence callbacks are deleted — there is no Dxgkrnl, and fences are signalled through the KMD's own `fence_id -> KEVENT` table.

---

## Phase 3: INF File

The INF tells Windows how to install the driver and which hardware ID to match. As a System-class KMDF driver it carries a `[.Wdf]` KMDF directive and **no** display SoftwareSettings, **no** `LoadOrderGroup=Video`, and **no** CoInstallers (inbox KMDF on Win10 1709+ / the 16299 floor; the legacy `CoInstallers32` pattern is forbidden in a universal INF).

```ini
; helios_kmd.inx
; Generated values: %HELIOS_DRIVER_NAME%, etc. are replaced by
; the wdk-build toolchain during package creation.

[Version]
Signature   = "$Windows NT$"
Class       = System
ClassGUID   = {4D36E97D-E325-11CE-BFC1-08002BE10318}
Provider    = %ProviderName%
DriverVer   = ; auto-stamped
CatalogFile = helios_kmd.cat
PnpLockdown = 1

[DestinationDirs]
DefaultDestDir = 13  ; driver store (universal INF)

[SourceDisksNames]
1 = %DiskName%,,,""

[SourceDisksFiles]
helios_kmd.sys = 1,,

; ──── Manufacturer / Models ────────────────────────────────────────────────

[Manufacturer]
%ProviderName% = Helios_Models,NTamd64

[Helios_Models.NTamd64]
%DeviceDesc% = Helios_Install, PCI\VEN_1AF4&DEV_1050

; ──── Installation ─────────────────────────────────────────────────────────

[Helios_Install.NT]
CopyFiles = Helios_CopyFiles

[Helios_Install.NT.Services]
AddService = helios_kmd, 0x00000002, Helios_ServiceInstall

[Helios_ServiceInstall]
ServiceType   = 1                   ; SERVICE_KERNEL_DRIVER
StartType     = 3                   ; SERVICE_DEMAND_START
ErrorControl  = 1                   ; SERVICE_ERROR_NORMAL
ServiceBinary = %13%\helios_kmd.sys

; ──── KMDF directive ───────────────────────────────────────────────────────

[Helios_Install.NT.Wdf]
KmdfService = helios_kmd, helios_wdfsect

[helios_wdfsect]
KmdfLibraryVersion = $KMDFVERSION$   ; resolves to 1.33

[Helios_CopyFiles]
helios_kmd.sys

; ──── Strings ──────────────────────────────────────────────────────────────

[Strings]
ProviderName = "Helios Project"
DeviceDesc   = "Helios vGPU"
DiskName     = "Helios vGPU Driver Disk"
```

The device interface (`GUID_DEVINTERFACE_HELIOS`) is registered in code via `WdfDeviceCreateDeviceInterface`, so the INF needs nothing extra for it. The Vulkan ICD's Khronos registry JSON is written by the ICD installer, independent of this INF.

---

## Troubleshooting

### BSOD: DRIVER_IRQL_NOT_LESS_OR_EQUAL
Paging in a non-paged function. Mark all ISR code as `#[link_section = ".text"]` (non-pageable), and anything called from DISPATCH_LEVEL.

### BSOD: SYSTEM_SERVICE_EXCEPTION in helios_kmd
Almost always a bad pointer dereference. Check that the KMDF device context retrieved via `WdfObjectGetTypedContext` is your `AdapterContext`, not something shifted. Add `DbgBreakPoint()` assertions in debug builds.

### Device shows as "Unknown Device" in Device Manager
The INF hardware ID doesn't match the actual PCI ID. Verify:
```powershell
Get-PnpDevice | Where-Object { $_.HardwareID -like "*1AF4*" }
```

### `WdfDriverCreate` fails / device interface not enumerated
If `WdfDriverCreate` returns an error, the `[.Wdf]` KMDF directive or `KmdfLibraryVersion` in the INF is wrong (must resolve to 1.33) — confirm the `[helios_wdfsect]` section is present and referenced from `[Helios_Install.NT.Wdf]`. If the driver loads but the Vulkan loader can't find the device, check both that the device interface is registered (`WdfDeviceCreateDeviceInterface(GUID_DEVINTERFACE_HELIOS)` succeeded and the device reached D0) **and** that the Khronos JSON value exists under `HKLM\SOFTWARE\Khronos\Vulkan\Drivers` pointing at the ICD DLL.

### Virtqueue stuck / no responses
1. Check that you're posting descriptors in the right order (readable before writable)
2. Verify the notification doorbell address — wrong BAR or wrong offset will silently drop notifications
3. (Note: `VIRGL_DEBUG` may produce no readable host logs with the render-server setup — see HOST.md §5.1; use QEMU `-d guest_errors` for `RESP_ERR_*` instead)
