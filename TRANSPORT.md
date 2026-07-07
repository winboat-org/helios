# TRANSPORT.md — VirtIO-GPU + Venus Wire Protocol

> **⚠️ PARTIALLY SUPERSEDED (2026-07-05).** The Venus wire format and virtio-gpu
> CMD_SUBMIT_3D details below remain accurate, but the **carrier is wrong**: the
> active driver does NOT reach the KMD via `DeviceIoControl`/`GUID_DEVINTERFACE_HELIOS`.
> User mode reaches the WDDM render miniport (`kmd_render/`) through the **dxgkrnl
> DDI + venus-over-`D3DKMTEscape`** (`kmd_render/src/ddi/escape.rs`, `submit_command`).
> Treat the "ICD → DeviceIoControl → KMD" spine as archived; **CLAUDE.md/ROADMAP.md
> are authoritative** for transport.

## Overview

This document describes the exact wire format used to send Vulkan commands from the Windows guest (ICD) through the KMD and virtio-gpu device to virglrenderer on the Linux host.

The stack is:
```
ICD (user-mode)   →   KMD (kernel-mode)   →   virtio-gpu device   →   virglrenderer (host)
Venus command buffer   DeviceIoControl (IOCTL)   Virtqueue CMD_SUBMIT_3D   Venus decoder + Vulkan
```

---

## 1. Virtio-GPU Protocol

### 1.1 Device Identification

```
PCI Vendor ID: 0x1AF4 (Red Hat)
PCI Device ID: 0x1050 (VirtIO GPU)
PCI Subsystem: 0x0028 (GPU)
```

### 1.2 PCI Capability Scanning

VirtIO Modern (1.0+) uses PCI vendor capabilities (type 9) to describe config regions.

```
PCI Config Space → scan for capability type 0x09 (vendor-specific)
For each vendor cap:
  cfg_type = cap.data[0]
  bar      = cap.data[1]
  offset   = cap.data[4..8]  (u32 LE)
  length   = cap.data[8..12] (u32 LE)
```

| cfg_type | Name | What it maps |
|----------|------|-------------|
| 1 | VIRTIO_PCI_CAP_COMMON_CFG | VirtIO common config registers |
| 2 | VIRTIO_PCI_CAP_NOTIFY_CFG | Doorbell registers (one per queue) |
| 3 | VIRTIO_PCI_CAP_ISR_CFG | Interrupt status byte |
| 4 | VIRTIO_PCI_CAP_DEVICE_CFG | GPU-specific config |
| 5 | VIRTIO_PCI_CAP_PCI_CFG | Fallback config access |

### 1.3 Device Initialization Sequence

Follow exactly the VirtIO spec §3.1.1 "Driver Requirements: Device Initialization":

```
1. Reset device: CommonCfg.device_status = 0
2. Set ACKNOWLEDGE:  CommonCfg.device_status |= 1
3. Set DRIVER:       CommonCfg.device_status |= 2
4. Read device feature bits: CommonCfg.device_feature_select = 0..N
5. Negotiate features (write driver_feature):
   REQUIRED: VIRTIO_F_VERSION_1 (bit 32) — modern virtio
   REQUIRED: VIRTIO_GPU_F_VIRGL (bit 0) — 3D/virgl support
   REQUIRED: VIRTIO_GPU_F_EDID  (bit 1) — (even if we don't use it)
   REQUIRED: VIRTIO_GPU_F_RESOURCE_UUID (bit 2) — for Venus blob tracking
   REQUIRED: VIRTIO_GPU_F_RESOURCE_BLOB (bit 3) — zero-copy blobs
   REQUIRED: VIRTIO_GPU_F_CONTEXT_INIT (bit 4) — Venus context type
   REQUIRED: VIRTIO_F_RING_RESET (bit 40) — queue reset support
6. Set FEATURES_OK: CommonCfg.device_status |= 8
7. Verify FEATURES_OK is still set (device acknowledges features)
8. Configure queues (see 1.4)
9. Set DRIVER_OK: CommonCfg.device_status |= 4
```

**Rust constants:**
```rust
pub const VIRTIO_F_VERSION_1:              u64 = 1 << 32;
pub const VIRTIO_GPU_F_VIRGL:              u64 = 1 << 0;
pub const VIRTIO_GPU_F_EDID:               u64 = 1 << 1;
pub const VIRTIO_GPU_F_RESOURCE_UUID:      u64 = 1 << 2;
pub const VIRTIO_GPU_F_RESOURCE_BLOB:      u64 = 1 << 3;
pub const VIRTIO_GPU_F_CONTEXT_INIT:       u64 = 1 << 4;
pub const VIRTIO_F_RING_RESET:             u64 = 1 << 40;
```

### 1.4 Queue Configuration

VirtIO GPU has 2 queues:

| Index | Name | Purpose |
|-------|------|---------|
| 0 | controlq | All GPU commands (3D, resource mgmt, etc.) |
| 1 | cursorq | Cursor updates (we ignore this — render-only) |

For each queue, write to CommonCfg:
```
queue_select = <queue index>    (u16)
queue_size   = <desired size>   (u16, must be power-of-2 ≤ max)
queue_desc   = <desc phys addr> (u64)
queue_driver = <avail phys addr>(u64)
queue_device = <used phys addr> (u64)
queue_msix_vector = <MSI-X vector> (u16, for MSI-X interrupt routing)
queue_enable = 1                (u16)
```

---

## 2. Venus Command Protocol

### 2.1 What is Venus?

Venus is a protocol that **serializes Vulkan API calls into a binary byte stream** which can be submitted through the virtio-gpu 3D command path. The host virglrenderer decodes this stream and replays the Vulkan calls on the physical GPU.

**Key design principle:** Venus is NOT a bytecode/shader language. It serializes the Vulkan C API struct-by-struct, pointer-by-pointer into a flat binary buffer. The host sees the same data structures the guest app would have passed to the real driver.

### 2.2 Venus Protocol Sources

The protocol is defined by codegen from `vk.xml`:

- **Protocol definition (codegen):** https://gitlab.freedesktop.org/virgl/venus-protocol
- **Host decoder (virglrenderer):** https://gitlab.freedesktop.org/virgl/virglrenderer/-/tree/master/src/venus
- **Linux guest encoder (Mesa):** https://gitlab.freedesktop.org/mesa/mesa/-/tree/main/src/virtio/vulkan

You MUST use compatible encoding with virglrenderer's decoder. The source of truth is `venus-protocol/`. Clone it and run the codegen to get the actual field layouts.

### 2.3 Venus Command Ring Buffer

The Venus protocol uses a ring buffer in shared memory (a virtio-gpu blob resource) for submitting commands. This avoids the overhead of individual virtqueue operations per Vulkan call.

```
┌────────────────────────────────────┐  ← shared blob resource (hostmem)
│  Ring header (32 bytes)            │
│  ┌──────────────────────────────┐  │
│  │ shmem_size: u32              │  │
│  │ ring_size: u32               │  │
│  │ producer_index: u32          │  │  ← written by guest (ICD)
│  │ consumer_index: u32          │  │  ← written by host (virglrenderer)
│  │ status: u32                  │  │
│  └──────────────────────────────┘  │
│  Ring data (variable)              │
│  ┌──────────────────────────────┐  │
│  │ [Venus commands ...]         │  │
│  └──────────────────────────────┘  │
└────────────────────────────────────┘
```

The ring is a classic single-producer single-consumer ring:
- Guest writes Venus commands starting at `data + (producer_index % ring_size)`
- Guest advances `producer_index`
- Host reads from `data + (consumer_index % ring_size)`, advances `consumer_index`

### 2.4 Venus Command Encoding

Each Venus command has the form:
```
[VnCommandTypeId: u16][VnCommandFlags: u16][command-specific fields ...]
```

Command IDs map to Vulkan functions. Example for `vkCreateBuffer`:

```
Offset  Size  Field
──────  ────  ─────────────────────────────────────────────────────────
0       2     VN_CMD_vkCreateBuffer (= 0x0041, from codegen)
2       2     flags (0 = synchronous)
4       8     device: VkDevice (u64 opaque handle)
12      4     pCreateInfo ptr (inline or offset)
...           [VkBufferCreateInfo fields inlined]
...     8     pAllocator ptr (null = 0)
...     8     pBuffer ptr (= inline response area offset)
```

**Inlining rule:** All pointers in Vulkan structs are inlined (dereferenced and their content embedded) into the Venus command stream. There are no raw pointer values — only offsets or NULLs.

**Handles:** Vulkan object handles (VkDevice, VkBuffer, etc.) are represented as 64-bit integers in the Venus stream. These are **host-side opaque handles** assigned by virglrenderer, not the guest's pointers.

### 2.5 Venus Context Creation

Before sending any Venus commands, create a Venus context:

```rust
// Guest side: send VIRTIO_GPU_CMD_CTX_CREATE via the control virtqueue
let mut cmd = VirtioGpuCtxCreate::zeroed();
cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_CREATE;
cmd.hdr.ctx_id = ctx_id;     // guest-assigned context ID (1..=0xFFFFFFFE)
cmd.context_init = VIRTIO_GPU_CAPSET_VENUS;  // = 4
cmd.nlen = 0;  // no debug name

// Send via ctrl virtqueue, wait for VIRTIO_GPU_RESP_OK_NODATA
virtio.send_cmd_sync(&cmd, &mut response)?;
```

The `context_init` field with value 4 (VENUS) tells virglrenderer to create a Venus context type, not a VirGL OpenGL context.

### 2.6 Venus Ring Setup (after context creation)

```rust
// 1. Create a blob resource for the command ring
let ring_size: u64 = 1024 * 1024; // 1 MB ring

let mut create_blob = VirtioGpuResourceCreateBlob::zeroed();
create_blob.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB;
create_blob.hdr.ctx_id = ctx_id;
create_blob.resource_id = new_resource_id();
create_blob.blob_mem    = VIRTIO_GPU_BLOB_MEM_HOST3D;
create_blob.blob_flags  = VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
create_blob.blob_id     = 0;  // host picks the blob
create_blob.size        = ring_size;
create_blob.nr_entries  = 0;

virtio.send_cmd_sync(&create_blob, &mut response)?;

// 2. Map the blob into the calling process
// (The KMD maps this to a user VA for the ICD to write to — see §6.2)
let mut map_blob = VirtioGpuResourceMapBlob::zeroed();
map_blob.hdr.type_    = VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB;
map_blob.hdr.ctx_id   = ctx_id;
map_blob.resource_id  = create_blob.resource_id;
map_blob.offset       = 0;  // map from start

virtio.send_cmd_sync(&map_blob, &mut map_response)?;
// map_response contains the GPA where the blob is mapped in guest physical memory

// 3. Initialize the ring header
let ring_header_ptr = map_response.offset as *mut VenusRingHeader;
unsafe {
    (*ring_header_ptr).shmem_size   = ring_size as u32;
    (*ring_header_ptr).ring_size    = (ring_size - 32) as u32;
    (*ring_header_ptr).producer_index = 0;
    (*ring_header_ptr).consumer_index = 0;
    (*ring_header_ptr).status       = 0;
}
```

### 2.7 Submitting Venus Commands

Once the ring is set up, the ICD writes commands into the ring and submits via:

```rust
// Option A: Ring-based (preferred, lower overhead)
// Write Venus bytes directly to the ring buffer (shared memory).
// Then kick the device with VIRTIO_GPU_CMD_SUBMIT_3D, size=0
// to notify virglrenderer of new ring data.

// Option B: DMA buffer submit (for large/one-shot commands)
// Put Venus bytes in the DMA buffer, submit via CMD_SUBMIT_3D with size>0.
```

**For our driver, use Option B initially (simpler), then optimize to Option A.**

```rust
// Submitting a Venus command buffer via the KMD (from the ICD via DeviceIoControl):

// ICD opens the device once (cached for the process lifetime):
//   SetupDiGetClassDevs(&GUID_DEVINTERFACE_HELIOS, ...) → enumerate the
//   interface → SetupDiGetDeviceInterfaceDetail (device path) →
//   CreateFileW(path, GENERIC_READ|GENERIC_WRITE, ...) → handle.

// ICD prepares a Venus-encoded buffer:
let mut buf = Vec::<u8>::new();
venus_encode_vkCreateInstance(&mut buf, &create_info);

// ICD issues IOCTL_HELIOS_SUBMIT_VENUS (METHOD_IN_DIRECT): a small fixed
// header (ctx_id / fence_id / buffer_size) rides the buffered system buffer
// and the variable Venus blob is carried via the locked input MDL.
let hdr = HeliosSubmitVenus { fence_id: next_fence_id(), ctx_id, buffer_size: buf.len() as u32, .. };
DeviceIoControl(handle, IOCTL_HELIOS_SUBMIT_VENUS, &hdr_and_blob, .., &mut bytes_returned, null)?;

// KMD's EvtIoDeviceControl reads the input, builds VirtioGpuCmdSubmit, sends via
// the ctrl virtqueue.
// virglrenderer decodes Venus bytes, calls vkCreateInstance on host GPU.
// Fence completion comes back via the virtio interrupt → DPC signals the
// per-fence KEVENT (or completes the pended WAIT_FENCE IOCTL).
```

---

## 3. ICD ↔ KMD Communication

> **ERRATA (2026-06-04): `helios_protocol::escape` (protocol/src/escape.rs) is the SOURCE OF TRUTH for the op-struct layouts (and the IOCTL codes + `GUID_DEVINTERFACE_HELIOS`), not the sketches below.** The op structs are `Pod` (8-byte aligned, padding-free) — e.g. `HeliosSubmitVenus` is **`fence_id`-first**, a different field order from the prose. The KMDF `EvtIoDeviceControl` handler dispatches **all six** ops (CTX_CREATE, CTX_DESTROY, SUBMIT_VENUS, ALLOC_BLOB, MAP_BLOB, WAIT_FENCE). Validate every guest-supplied size/offset against the WDF-reported in/out length before reading (guest→kernel trust boundary).

### 3.1 Device Interface & IOCTL Codes

User mode reaches the KMD via `DeviceIoControl` on a device interface — there is no WDDM escape carrier. The ICD discovers and opens the device:

- `SetupDiGetClassDevs(&GUID_DEVINTERFACE_HELIOS, NULL, NULL, DIGCF_DEVICEINTERFACE|DIGCF_PRESENT)`
- `SetupDiEnumDeviceInterfaces` → `SetupDiGetDeviceInterfaceDetail` (device path)
- `CreateFileW(path, GENERIC_READ|GENERIC_WRITE, FILE_SHARE_READ|FILE_SHARE_WRITE, OPEN_EXISTING, ...)`
- `DeviceIoControl(handle, IOCTL_HELIOS_*, in_buf, in_len, out_buf, out_len, &bytes, NULL)`

The handle is opened **once** and cached for the process lifetime (MAP_BLOB maps into the calling process, so the ICD must issue subsequent IOCTLs from the same process that opened the handle). `GUID_DEVINTERFACE_HELIOS` is defined once in `helios_protocol` and shared by KMD and ICD.

IOCTL control codes are built with the standard macro:

```
CTL_CODE(DeviceType, Function, Method, Access)
    = (DeviceType<<16) | (Access<<14) | (Function<<2) | Method
```

with `DeviceType = FILE_DEVICE_UNKNOWN (0x22)`, `Access = FILE_READ_DATA|FILE_WRITE_DATA (3)`, function base `0x900` (vendor range ≥ 0x800). Methods: `METHOD_BUFFERED=0`, `METHOD_IN_DIRECT=1`, `METHOD_OUT_DIRECT=2`, `METHOD_NEITHER=3`. (`protocol/src/ioctl.rs` is the asserted source of truth.)

```rust
// Shared between ICD and KMD (protocol/src/ioctl.rs is the source of truth).
const fn ctl_code(function: u32, method: u32) -> u32 {
    // FILE_DEVICE_UNKNOWN << 16 | (FILE_READ_DATA|FILE_WRITE_DATA) << 14 | function << 2 | method
    (0x22 << 16) | (3 << 14) | (function << 2) | method
}

pub const IOCTL_HELIOS_CTX_CREATE:   u32 = ctl_code(0x900, 0); // METHOD_BUFFERED   = 0x0022E400
pub const IOCTL_HELIOS_CTX_DESTROY:  u32 = ctl_code(0x901, 0); // METHOD_BUFFERED   = 0x0022E404
pub const IOCTL_HELIOS_SUBMIT_VENUS: u32 = ctl_code(0x902, 1); // METHOD_IN_DIRECT  = 0x0022E409
pub const IOCTL_HELIOS_ALLOC_BLOB:   u32 = ctl_code(0x903, 0); // METHOD_BUFFERED   = 0x0022E40C
pub const IOCTL_HELIOS_MAP_BLOB:     u32 = ctl_code(0x904, 2); // METHOD_OUT_DIRECT = 0x0022E412
pub const IOCTL_HELIOS_WAIT_FENCE:   u32 = ctl_code(0x905, 0); // METHOD_BUFFERED   = 0x0022E414
```

**Method rationale:** the small fixed verbs use `METHOD_BUFFERED` (the I/O manager double-buffers — the mvisor pattern). SUBMIT_VENUS's Venus stream can be megabytes, so `METHOD_IN_DIRECT` carries the variable payload via a locked input MDL while a small fixed header (ctx_id / fence_id / buffer_size) rides the buffered system buffer. MAP_BLOB uses `METHOD_OUT_DIRECT` and returns a **user VA** (8-byte pointer) in its OUT buffer; the page mapping is the side effect (see §6.2).

The op-struct byte layouts (`HeliosCtxCreate`, `HeliosCtxDestroy`, `HeliosSubmitVenus`, `HeliosAllocBlob`, `HeliosMapBlob`, `HeliosWaitFence`) live in `protocol/src/escape.rs` as `Pod` types. The old 16-byte `HeliosEscapeHeader` (magic/cmd_type/version/size) is **redundant** — the IOCTL control code *is* the verb and WDF validates the in/out lengths — so it is dropped (or kept only as a cheap version sanity check); the op-struct layouts are unchanged.

### 3.2 KMD EvtIoDeviceControl Handler

```rust
// src/ioctl.rs

pub extern "C" fn evt_io_device_control(
    queue: WDFQUEUE,
    request: WDFREQUEST,
    output_buffer_length: usize,
    input_buffer_length: usize,
    io_control_code: u32,
) {
    // SAFETY: WDF guarantees the queue's parent device carries our AdapterContext.
    let adapter = adapter_from_queue(queue);

    let status = match io_control_code {
        IOCTL_HELIOS_CTX_CREATE   => ioctl_ctx_create(adapter, request),
        IOCTL_HELIOS_CTX_DESTROY  => ioctl_ctx_destroy(adapter, request),
        IOCTL_HELIOS_SUBMIT_VENUS => ioctl_submit_venus(adapter, request),
        IOCTL_HELIOS_ALLOC_BLOB   => ioctl_alloc_blob(adapter, request),
        IOCTL_HELIOS_MAP_BLOB     => ioctl_map_blob(adapter, request),
        IOCTL_HELIOS_WAIT_FENCE   => ioctl_wait_fence(adapter, request),
        _ => STATUS_INVALID_DEVICE_REQUEST,
    };

    // ioctl_* set `bytes_returned` for ops with an OUT buffer; default 0.
    unsafe { WdfRequestCompleteWithInformation(request, status, /* bytes */ 0) };
}

fn ioctl_submit_venus(adapter: &AdapterContext, request: WDFREQUEST) -> NTSTATUS {
    // Buffered system buffer carries the fixed header (fence_id/ctx_id/buffer_size);
    // the variable Venus blob arrives as a locked input MDL (METHOD_IN_DIRECT).
    let hdr: HeliosSubmitVenus = match retrieve_input_pod(request) {
        Ok(h) => h,
        Err(s) => return s,
    };
    let venus_data = match retrieve_input_mdl(request, hdr.buffer_size as usize) {
        Ok(d) => d,           // validated: MDL byte count >= buffer_size
        Err(s) => return s,
    };

    // Same body as the previous ddi/escape.rs handler — ports 1:1.
    adapter.with_virtio(|v| v.submit_venus(hdr.ctx_id, hdr.fence_id, venus_data))
}
```

Each `ioctl_*` retrieves its buffers (`WdfRequestRetrieveInputBuffer` / `WdfRequestRetrieveOutputBuffer` for buffered ops; `WdfRequestRetrieveInputWdmMdl` for SUBMIT_VENUS), validates every guest-supplied size/offset against the WDF-reported in/out length, runs the same `adapter.with_virtio(|v| v.ctx_create / ctx_destroy / submit_venus / alloc_blob / map_blob / ...)` body the WDDM-era `ddi/escape.rs` used, then completes the request with `WdfRequestCompleteWithInformation(request, status, bytes_returned)`. WAIT_FENCE may pend the request and complete it later from the DPC (see §5.1).

---

## 4. Venus Command Encoding (Rust)

The Venus encoding is mechanically generated from `vk.xml` in the real Mesa implementation. For our Rust implementation, we will write the encoder manually for the subset of Vulkan we need, cross-referencing the virglrenderer decoder source.

### 4.1 Encoding Primitives

```rust
// src/icd/venus/encode.rs

pub struct VnEncoder {
    buf: Vec<u8>,
}

impl VnEncoder {
    pub fn new() -> Self { Self { buf: Vec::with_capacity(4096) } }

    pub fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn write_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn write_i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn write_handle(&mut self, h: u64) {
        // Venus handles are u64 (64-bit opaque)
        self.write_u64(h);
    }
    pub fn write_pointer_opt<T>(&mut self, ptr: Option<&T>, write_fn: impl Fn(&mut Self, &T)) {
        if let Some(v) = ptr {
            self.write_u32(1); // non-null
            write_fn(self, v);
        } else {
            self.write_u32(0); // null
        }
    }
    pub fn finish(self) -> Vec<u8> { self.buf }
}

// Venus command IDs (from virglrenderer/src/venus/vn_protocol_driver_info.h)
// These must EXACTLY match what virglrenderer expects.
// Generate from: venus-protocol repo's codegen output
pub const VN_CMD_vkCreateInstance:     u16 = 0x0000;
pub const VN_CMD_vkDestroyInstance:    u16 = 0x0001;
pub const VN_CMD_vkEnumeratePhysicalDevices: u16 = 0x0002;
pub const VN_CMD_vkCreateDevice:       u16 = 0x000B;
// ... (full list from venus-protocol codegen)

/// Encode a Venus command header
pub fn encode_cmd_header(enc: &mut VnEncoder, cmd_id: u16) {
    enc.write_u32(cmd_id as u32);  // type (upper 16 bits = 0 for standard cmds)
}

/// Example: encode vkCreateInstance
pub fn encode_vkCreateInstance(
    enc: &mut VnEncoder,
    p_create_info: &VkInstanceCreateInfo,
    // p_allocator: always NULL for Venus
) {
    encode_cmd_header(enc, VN_CMD_vkCreateInstance);
    encode_VkInstanceCreateInfo(enc, p_create_info);
    enc.write_u32(0); // pAllocator = NULL
    // Response area: Venus will write the VkInstance handle here
    // (handled via the response ring, not inline)
}

fn encode_VkInstanceCreateInfo(enc: &mut VnEncoder, info: &VkInstanceCreateInfo) {
    enc.write_u32(info.s_type as u32);
    enc.write_u32(0); // pNext = NULL (simplified)
    enc.write_u32(info.flags);
    // pApplicationInfo
    enc.write_pointer_opt(info.p_application_info.as_ref(), |enc, app| {
        enc.write_u32(app.s_type as u32);
        enc.write_u32(0); // pNext
        // pApplicationName: encode as length-prefixed string
        encode_string_opt(enc, app.p_application_name);
        enc.write_u32(app.application_version);
        encode_string_opt(enc, app.p_engine_name);
        enc.write_u32(app.engine_version);
        enc.write_u32(app.api_version);
    });
    // ppEnabledLayerNames
    enc.write_u32(info.enabled_layer_count);
    for i in 0..info.enabled_layer_count as usize {
        let name = unsafe { core::ffi::CStr::from_ptr(info.pp_enabled_layer_names.add(i).read()) };
        encode_string(enc, name.to_bytes());
    }
    // ppEnabledExtensionNames
    enc.write_u32(info.enabled_extension_count);
    for i in 0..info.enabled_extension_count as usize {
        let name = unsafe { core::ffi::CStr::from_ptr(info.pp_enabled_extension_names.add(i).read()) };
        encode_string(enc, name.to_bytes());
    }
}

fn encode_string(enc: &mut VnEncoder, s: &[u8]) {
    enc.write_u32(s.len() as u32 + 1);  // length including null
    enc.buf.extend_from_slice(s);
    enc.buf.push(0);  // null terminator
    // align to 4 bytes
    while enc.buf.len() % 4 != 0 { enc.buf.push(0); }
}

fn encode_string_opt(enc: &mut VnEncoder, ptr: *const i8) {
    if ptr.is_null() {
        enc.write_u32(0);
    } else {
        let s = unsafe { core::ffi::CStr::from_ptr(ptr) };
        encode_string(enc, s.to_bytes());
    }
}
```

**⚠️ CRITICAL:** The encoding above is a sketch. The actual Venus encoding is generated from `vk.xml` by the venus-protocol codegen. You MUST cross-reference `virglrenderer/src/venus/vn_protocol_driver_*.h` to get the exact field order and sizes. Any mismatch will result in virglrenderer silently producing garbage or crashing.

### 4.2 Getting the Actual Command Encoding

```bash
# On Linux (build machine):
git clone https://gitlab.freedesktop.org/virgl/venus-protocol.git
cd venus-protocol
# The headers in include/ show the exact encoding per-command
ls include/
# vn_protocol_driver_info.h  — command IDs
# vn_protocol_driver_*.h    — per-type encoders
```

For each Vulkan function you implement, look at:
- `include/vn_protocol_driver_<vulkan_type>.h` in venus-protocol
- `src/venus/vn_protocol_renderer_<type>.h` in virglrenderer (decoder side)

---

## 5. Synchronization and Fencing

### 5.1 Fence Flow

```
Guest ICD                  KMD                              virglrenderer
─────────                  ───                              ─────────────
Submit Venus cmd ─────►  EvtIoDeviceControl
                         (IOCTL_HELIOS_SUBMIT_VENUS)
                         submit via virtqueue ──────────► decode + execute Vulkan
                                                          write fence completion
                         ◄── virtio ISR                    ◄── used ring entry
                         → DPC drains used ring
                         → signal fence_id KEVENT
ICD unblocks ◄────────── (or complete pended WAIT_FENCE IOCTL)
```

### 5.2 Fence Encoding in virtio-gpu

Every command with `VIRTIO_GPU_FLAG_FENCE` set causes virglrenderer to:
1. Process the command
2. Write a `VIRTIO_GPU_RESP_OK_NODATA` response with the same `fence_id`
3. Mark the descriptor as used in the used ring

The virtio ISR acks the interrupt and queues a DPC; the DPC drains the used ring and signals the fence value.

### 5.3 Fence Signaling (KMDF)

There are no WDDM monitored fences. The KMD keeps a `fence_id → KEVENT` table in the `AdapterContext`. On submit, the requested `fence_id` rides the `VIRTIO_GPU_FLAG_FENCE` command. When the DPC drains the matching used-ring entry it signals that `fence_id`'s KEVENT — and, if a `IOCTL_HELIOS_WAIT_FENCE` request was pended on that fence, completes the pended IOCTL. WAIT_FENCE carries `fence_id` + `timeout_ns`; the handler either returns immediately if the fence is already signaled, or blocks on (pends against) the KEVENT until the DPC fires or the timeout elapses.

---

## 6. Resource Management

### 6.1 Virtio-GPU Resource IDs

Every GPU buffer/image in the guest is backed by a virtio-gpu **resource ID** (u32). The lifecycle:

```
ICD: vkCreateBuffer()
  → ICD issues IOCTL_HELIOS_ALLOC_BLOB
  → KMD assigns resource_id (monotonically increasing u32)
  → KMD sends VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB (for host memory)
  → KMD sends VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE (attach to Venus ctx)
  → Returns resource handle (out_resource_id) to ICD

ICD: vkDestroyBuffer()
  → ICD issues the free/unref IOCTL
  → KMD sends VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE
  → KMD sends VIRTIO_GPU_CMD_RESOURCE_UNREF
```

### 6.2 Blob Resource Memory

For Vulkan memory objects (`VkDeviceMemory`), we use blob resources with `VIRTIO_GPU_BLOB_MEM_HOST3D`:

```rust
let mut cmd = VirtioGpuResourceCreateBlob::zeroed();
cmd.hdr.type_   = VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB;
cmd.hdr.ctx_id  = ctx_id;
cmd.resource_id = new_resource_id();
cmd.blob_mem    = VIRTIO_GPU_BLOB_MEM_HOST3D;
cmd.blob_flags  = VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE
                | VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE;
cmd.blob_id     = 0;  // virglrenderer assigns (set via Venus VkDeviceMemory handle)
cmd.size        = allocation_size;
cmd.nr_entries  = 0;
```

The `blob_id` field links the blob resource to a Venus `VkDeviceMemory` object that was previously created via the Venus command stream. The mapping between Venus handles and blob IDs is managed by virglrenderer internally.

---

## 7. Reference Implementations & Reusable Patterns

The closest prior art is **[tenclass/mvisor-win-vgpu-driver](https://github.com/tenclass/mvisor-win-vgpu-driver)** (GPLv3, C) — a Windows guest GPU driver over a virtio-gpu-style device. It is **architecturally different** from Helios: a generic WDF/KMDF device exposing DRM-virtgpu `DeviceIoControl`, driving a Mesa **virgl OpenGL** ICD (its own `opengl32.dll`). It is OpenGL/virgl (not Vulkan/Venus) and bypasses WDDM entirely (no Dxgkrnl/D3DKMT). But the layer *beneath* the Helios KMD — the virtio-gpu 3D transport — is nearly identical, so it's a strong reference for Phases 2–4. Patterns worth adopting:

- **Two-descriptor `SUBMIT_3D`** (Phase 4): a small *mutable* `virtio_gpu_cmd_submit` header in descriptor 0, and the large command body passed **by physical address** in descriptor 1, pointing into a pre-reserved page-aligned slice of a contiguous pool. Avoids per-submit allocation; keeps the fence-flag header writable.
- **Bitmap page sub-allocator** (Phase 3): one big `MmAllocateContiguousMemory` region sub-allocated with an `RtlBitmap` (page-granular, last-free-index hint), instead of many per-allocation contiguous allocs.
- **Host-visible blob mapping** (Phase 3): `RESOURCE_CREATE_BLOB(HOST3D)` → `RESOURCE_MAP_BLOB` → `MmMapLockedPagesSpecifyCache(mdl, UserMode, …)` using the **cache mode the host returns in `map_info`** (CACHED / WC / UNCACHED), returning a **user VA** to the ICD (see §6.2). That `map_info` byte chooses `PAGE_WRITECOMBINE` vs cached — the spec-sanctioned coherent-host-memory path.
  > **RESOLVED (2026-06-04):** the blob GPA is **not** in `VirtioGpuRespMapInfo` (protocol/src/virtio_gpu.rs, 32B — it carries only the `map_info` caching byte + padding). QEMU exposes a host-visible memory window as a prefetchable 64-bit PCI BAR (typically BAR4) advertised by a `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` cap (cap type 8) with `shmid == VIRTIO_GPU_SHM_ID_HOST_VISIBLE`, size = device `hostmem=` (256M..8G). The KMD records that BAR's guest-physical base during the cap scan; on MAP_BLOB it computes `gpa = host_visible_bar_base + offset` (`RESOURCE_MAP_BLOB` injects the resource's mapping at that offset), builds an MDL over those pages, and `MmMapLockedPagesSpecifyCache(mdl, UserMode, <cache>)` to return the user VA. (See ARCH.md §6.)
- **Capset / `context_init` negotiation** (Phase 4): `GET_CAPSET_INFO`/`GET_CAPSET` + a supported-capset bitmask + `CTX_CREATE` with `context_init` = capset id. Identical in shape for Venus — use `VIRTIO_GPU_CAPSET_VENUS` and parse the Venus caps.
- **ISR/DPC split** (Phase 2): the ISR only reads ISR/MSI status and queues a DPC; the DPC drains the used ring, dispatches on `hdr.type`, frees command buffers, and signals fences — per-queue spinlock + MSI-X message→queue map. Enforces the invariant *free descriptors only on used-ring completion*.
- **Fence signaling**: a `fence_id → KEVENT` map; the DPC signals the matching KEVENT (and completes any pended WAIT_FENCE IOCTL) on used-ring completion + a blocking wait in WAIT_FENCE.

**Gotchas:**
- mvisor indexes queues **COMMAND=0 / CONTROL=1** — *opposite* of standard virtio-gpu (controlq=0). Helios uses the **standard** virtio-gpu layout (control queue = 0). Do not copy mvisor's ordering.
- mvisor invents a **custom** virtio device (`DEV_105B`, custom config struct); Helios uses the **standard** virtio-gpu device (`0x1050`) with standard feature/config negotiation.

**Other prior art:** [Keenuts/virtio-gpu-win-icd](https://github.com/Keenuts/virtio-gpu-win-icd), [kjliew/qemu-3dfx](https://github.com/kjliew/qemu-3dfx).

**Chosen architecture:** Helios follows the mvisor model — a System-class KMDF function driver exposing a private device interface (`DeviceIoControl`), with our own Vulkan ICD enumerated by the Windows Vulkan loader via registry JSON. WDDM / Dxgkrnl / D3DKMTEscape are dropped entirely (see ARCH.md): a WDDM render adapter would have to pass dxgkrnl's AddAdapter capability/version contract for GPU scheduling + memory features the host already owns under Venus replay — pure cost, no benefit. The Vulkan-loader + DXVK/VKD3D path still gets us D3D11/12 (not just GL), which is the mandate, and the KMDF + own-ICD IOCTL channel is the proven, simpler route there.
