# OVERVIEW.md — Helios vGPU Architecture

> **⚠️ SUPERSEDED (2026-07-05) — ARCHIVED REFERENCE ONLY.** This document
> describes the abandoned System-class KMDF + `DeviceIoControl` design and states
> the driver does NOT composite the desktop. Both are now false: the active
> project is a **WDDM 3.2 render-only miniport + DXVK-bridged D3D11 UMD + Mesa-
> Venus ICD (`vulkan_virtio.dll`) + IddCx/Looking Glass**, and **DWM composites
> the whole Windows desktop on Helios** (the milestone, met 2026-07-05). The Rust
> `helios_icd.dll` named below no longer exists (the ICD is a C Mesa-Venus port).
> **CLAUDE.md and ROADMAP.md are authoritative.** Read below as history only.

> **DIRECTION RESET (2026-06-07):** the active project direction is again
> **System-class KMDF + DeviceIoControl + Mesa Venus**. The WDDM DOD/display pivot is archived, not deleted;
> see [`SYSTEM_CLASS_REFOCUS_2026_06_07.md`](SYSTEM_CLASS_REFOCUS_2026_06_07.md) for the decision record.

## Project Goal

Build a virtio-gpu driver for a Windows 11 VM under KVM/QEMU that gives guest apps a performant
Vulkan-capable GPU through Venus. Display integration is a separate layer and must not block renderer
performance work.

- **Render (active, working baseline):** a Mesa-venus Vulkan ICD encodes Vulkan over the **virtio-gpu Venus protocol** to
  virglrenderer on the Linux host, executing on the host's physical GPU (ANV/RADV). DirectX rides DXVK/VKD3D →
  Vulkan in the guest. (Reached end-to-end in the System-class phase.)
- **Display (future/separate):** windowed Win32 WSI and DOD scanout experiments are archived. Revisit display
  only after offscreen Venus submit/fence/blob throughput is fast enough.
- Does NOT use GPU passthrough — host retains full GPU access.
- Does NOT GPU-composite the Windows desktop. A native WDDM render adapter remains rejected because it requires
  a native D3D-to-venus UMD.

---

## Driver Model (System-class KMDF, no WDDM)

Helios is a **System-class KMDF function driver** for the virtio-gpu PCI device — NOT a display/WDDM miniport. Setup class is **System {4d36e97d-e325-11ce-bfc1-08002be10318}**. The framework is **KMDF (WDF)** over `wdk-sys` (driver-type `KMDF`, KMDF 1.33 on the 10.0.26100 WDK / Win11 24H2); the WDF function table is auto-wired and calls go through `wdk_sys::call_unsafe_wdf_function_binding!`. There is **no dxgkrnl, no `DxgkInitialize`, no `DRIVER_INITIALIZATION_DATA` DDI table**, and consequently **no GPU-VA/GpuMmu/segment/monitored-fence contract, no stub D3D user-mode driver, no Code 43, and no AddAdapter capability/version handshake**.

User mode reaches the KMD via **`DeviceIoControl` on a device interface** (`GUID_DEVINTERFACE_HELIOS`, discovered with `SetupDiGetClassDevs` + `CreateFile`). The six existing `helios_protocol` ops (CTX_CREATE, CTX_DESTROY, SUBMIT_VENUS, ALLOC_BLOB, MAP_BLOB, WAIT_FENCE) keep their exact wire layout and simply ride IOCTL input/output buffers instead of a kernel-display escape carrier. The Vulkan ICD is enumerated by the Windows Vulkan loader purely through `HKLM\SOFTWARE\Khronos\Vulkan\Drivers` registry JSON — exactly how the non-WDDM ICDs **SwiftShader** and **Mesa lavapipe** enumerate with no display adapter present.

**Lifecycle callbacks (replace the entire DxgkDdi table):**

- `DriverEntry(driver, registry_path)` → build `WDF_DRIVER_CONFIG { EvtDriverDeviceAdd, .. }`, `WdfDriverCreate(...)`.
- `evt_device_add(driver, device_init)` → set PnP/power callbacks (PrepareHardware/ReleaseHardware/D0Entry/D0Exit), `WdfDeviceCreate` with the `AdapterContext` context type, `WdfDeviceCreateDeviceInterface(&GUID_DEVINTERFACE_HELIOS)`, then a default parallel I/O queue whose `EvtIoDeviceControl` dispatches the IOCTL verbs.
- `evt_device_prepare_hardware` → walk the translated CM resource list, `MmMapIoSpaceEx` each BAR, obtain `BUS_INTERFACE_STANDARD` via `WdfFdoQueryForInterface`, init the virtio-gpu transport, store it in the device context. Create the WDF interrupt object (`WdfInterruptCreate` with EvtInterruptIsr + EvtInterruptDpc).
- `evt_device_release_hardware` → drop the transport, `MmUnmapIoSpace` each BAR.

### Helios phases ↔ KMDF milestones

| Helios phase | KMDF flow |
|---|---|
| 0 Toolchain | KMDF toolchain + signing prereqs |
| 1 Adapter enumeration | `DriverEntry` → `WdfDriverCreate` → `evt_device_add` → `WdfDeviceCreate` + `WdfDeviceCreateDeviceInterface` |
| 2 VirtIO PCI + virtqueue | `evt_device_prepare_hardware`: BAR map + `BUS_INTERFACE_STANDARD` + virtio-gpu bring-up |
| 3 IOCTL spine | default queue `EvtIoDeviceControl` → op dispatch (CTX_CREATE/SUBMIT_VENUS/ALLOC_BLOB/MAP_BLOB/WAIT_FENCE), `fence_id`→KEVENT |
| 4 Venus context + submit | `submit_venus` over virtqueue; `WdfInterruptCreate` ISR/DPC signals the per-fence KEVENT |
| 5 Vulkan ICD | Mesa venus port over IOCTL; loader-registry JSON |
| 6 DXVK / VKD3D | App-level validation |

**STATUS (updated 2026-06-07):** Phases 0–5 + WSI bring-up reached a usable working baseline — the System-class KMDF driver + the Mesa
**venus ICD** render end-to-end on real hardware: `vulkaninfo` reports `driverName venus`, cached
`HOST_VISIBLE|HOST_COHERENT|HOST_CACHED` memory works through the Windows BAR mapping path, `vkCmdFillBuffer`
readback passes, and `vkcube` renders normally through Win32 WSI. The ICD now keeps Windows cache visibility
explicit for cached coherent mappings and feedback slots, and device teardown tolerates clients that exit with
mapped coherent memory still live. The WDDM **render** miniport stays abandoned. The DOD display pivot is archived
unless a display/scanout experiment is explicitly chosen. **NEXT:** benchmark renderer throughput separately from
QXL/SPICE display transport, then decide whether display work should be a separate virtio-gpu display device,
IDD, or DOD scanout project.

**KMDF callbacks the driver registers:** `EvtDriverDeviceAdd`, `EvtDevicePrepareHardware`, `EvtDeviceReleaseHardware`, `EvtDeviceD0Entry`, `EvtDeviceD0Exit`, `EvtIoDeviceControl`, `EvtInterruptIsr`, `EvtInterruptDpc`.

---

## System Diagram

```
┌─────────────────────────────────────────────────────────────────────┐
│  Windows 11 Guest (KVM VM)                                          │
│                                                                     │
│  ┌────────────┐  ┌──────────┐  ┌──────────────────────────────┐   │
│  │  D3D11 app │  │ D3D12 app│  │  Vulkan app                  │   │
│  └─────┬──────┘  └────┬─────┘  └──────────────┬───────────────┘   │
│        │              │                         │                   │
│  ┌─────▼──────────────▼─────┐                  │                   │
│  │   DXVK (d3d11.dll)       │                  │                   │
│  │   VKD3D-Proton (d3d12.dll│                  │                   │
│  └─────────────┬────────────┘                  │                   │
│                │ Vulkan calls                   │ Vulkan calls      │
│  ┌─────────────▼────────────────────────────────▼───────────────┐  │
│  │           Helios Vulkan ICD (helios_icd.dll)                  │  │
│  │                  Venus command encoder                        │  │
│  └────────────────────────────┬──────────────────────────────────┘  │
│                               │ DeviceIoControl (IOCTL →           │
│                               │ GUID_DEVINTERFACE)                  │
│  ┌────────────────────────────▼──────────────────────────────────┐  │
│  │        Helios KMD (helios_kmd.sys)                            │  │
│  │   System-class KMDF function driver (DeviceIoControl)         │  │
│  └────────────────────────────┬──────────────────────────────────┘  │
│                               │ VirtIO PCI (virtqueues)             │
│                               │ VEN_1AF4 DEV_1050                   │
└───────────────────────────────┼─────────────────────────────────────┘
                                │ VirtIO transport (shared memory ring)
┌───────────────────────────────┼─────────────────────────────────────┐
│  Linux Host (KVM)             │                                     │
│                               ▼                                     │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │  QEMU 9.2+ virtio-gpu-gl device model                       │   │
│  │  (blob=on, hostmem=8G, venus=on)                            │   │
│  └──────────────────────────┬──────────────────────────────────┘   │
│                             │ virgl_renderer_* API                  │
│  ┌──────────────────────────▼──────────────────────────────────┐   │
│  │  virglrenderer (Venus context type)                         │   │
│  │  Venus command decoder + Vulkan replay                      │   │
│  └──────────────────────────┬──────────────────────────────────┘   │
│                             │ Vulkan API                            │
│  ┌──────────────────────────▼──────────────────────────────────┐   │
│  │  Host GPU Driver (RADV/ANV/NVIDIA)                          │   │
│  └─────────────────────────────────────────────────────────────┘   │
│  Physical GPU (AMD/Intel/NVIDIA) — also used by host               │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Component Breakdown

### 1. Helios KMD (`kmd/`) — Kernel-Mode Driver

**Language:** Rust (no_std) using `windows-drivers-rs` / `wdk-sys`  
**Driver model:** KMDF (WDF) via `wdk-sys`; System device class {4d36e97d-e325-11ce-bfc1-08002be10318}. See "Driver Model (System-class KMDF, no WDDM)" above.  

The KMD is the only piece that runs in the Windows kernel. It:
- Presents a PCI device to the OS (virtio-gpu at VEN_1AF4&DEV_1050)
- Registers a device interface (`GUID_DEVINTERFACE_HELIOS`) that user mode opens via SetupDi + CreateFile
- Allocates virtio-gpu blob resources on demand (the host owns GPU VA + scheduling via Venus)
- Forwards opaque Venus command buffers (from the ICD) through the virtqueues
- Handles fences/synchronization between CPU and the host-side Venus renderer (`fence_id`→KEVENT)

**KMDF callbacks implemented:**

| Callback | Purpose |
|----------|---------|
| `EvtDriverDeviceAdd` | Create the WDF device + device interface + default I/O queue |
| `EvtDevicePrepareHardware` | Map PCI BARs, get `BUS_INTERFACE_STANDARD`, init virtio-gpu |
| `EvtDeviceReleaseHardware` | Tear down transport, unmap BARs |
| `EvtIoDeviceControl` | Dispatch the IOCTL verbs (see below) |
| `EvtInterruptIsr` / `EvtInterruptDpc` | Ack virtio interrupt, pop used ring, signal per-fence KEVENT |

**IOCTL ops (`GUID_DEVINTERFACE_HELIOS`):**

| Op | IOCTL constant | Value | Purpose |
|----|----------------|-------|---------|
| CTX_CREATE | `IOCTL_HELIOS_CTX_CREATE` | `0x0022E400` | Create a Venus context (capset id in, ctx id out) |
| CTX_DESTROY | `IOCTL_HELIOS_CTX_DESTROY` | `0x0022E404` | Destroy a Venus context |
| SUBMIT_VENUS | `IOCTL_HELIOS_SUBMIT_VENUS` | `0x0022E409` | Submit an opaque Venus command blob (METHOD_IN_DIRECT via MDL) |
| ALLOC_BLOB | `IOCTL_HELIOS_ALLOC_BLOB` | `0x0022E40C` | Allocate a virtio-gpu blob resource (resource id out) |
| MAP_BLOB | `IOCTL_HELIOS_MAP_BLOB` | `0x0022E412` | Map a blob into the caller's address space (user VA out) |
| WAIT_FENCE | `IOCTL_HELIOS_WAIT_FENCE` | `0x0022E414` | Block on a `fence_id`→KEVENT with timeout |

### 2. Helios ICD (`icd/`) — Vulkan Installable Client Driver

**Language:** Rust (std available, this is a user-mode DLL)  
**Output:** `helios_icd.dll` + `helios_vulkan.json` (Vulkan loader manifest)  

The ICD sits between the Vulkan loader and the KMD. It:
- Exposes `vk_icdGetInstanceProcAddr` (Vulkan ICD entry point)
- Encodes Vulkan API calls into Venus binary protocol
- Submits Venus command buffers to the KMD via `DeviceIoControl` + shared/blob memory
- Deserializes Venus response/event stream for return values and completions

DXVK and VKD3D-Proton are not modified — they see a standard Vulkan ICD.

### 3. Host daemon (`host/`) — optional helper

For Venus, QEMU + virglrenderer handle everything natively. The host/ crate is for:
- Diagnostics (dumping Venus streams)
- Configuration helpers
- Future: custom transport bypassing QEMU's virgl path for lower latency

---

## Venus Protocol (the critical path)

Venus is a **Vulkan command serialization protocol** used to send Vulkan calls over the virtio-gpu transport. The guest (ICD) encodes API calls; the host (virglrenderer) decodes and replays them against the physical GPU.

```
Guest ICD                            Host virglrenderer
─────────────────                    ──────────────────
vkCreateBuffer(...)                  
  → vn_encode_vkCreateBuffer(...)    
  → VIRTIO_GPU_CMD_SUBMIT_3D         
    [cmd_buf = Venus bytes]    ──►   vn_decode_vkCreateBuffer(...)
                                       → vkCreateBuffer() on host GPU
                               ◄──   VkResult + handle returned via
                                       VIRTIO_GPU_RESP_OK_NODATA or
                                       response ring
```

**Spec / codegen source:** https://gitlab.freedesktop.org/virgl/venus-protocol  
**Host implementation:** https://gitlab.freedesktop.org/virgl/virglrenderer (src/venus/)  
**Reference guest (Linux):** https://gitlab.freedesktop.org/mesa/mesa (src/virtio/vulkan/)

The Venus wire format is auto-generated from `vk.xml`. You must use the same codegen to ensure compatibility with virglrenderer. See `TRANSPORT.md` for the full wire format.

---

## What Is NOT a Passthrough

This design specifically avoids GPU passthrough (VFIO/IOMMU). Here's the comparison:

| Property | Passthrough | Helios vGPU |
|----------|-------------|-------------|
| Host GPU access | ❌ Host loses GPU | ✅ Host keeps GPU |
| Multiple VMs | ❌ One VM only | ✅ Multiple VMs possible |
| Performance | ~95% native | ~40–60% native (target: 50%) |
| Driver complexity | Low (host driver does all work) | High (Venus stack) |
| Guest Vulkan | ✅ Native | ✅ Via Venus |
| Guest DirectX | ✅ Native | ✅ Via DXVK+Venus |

---

## Memory Model (host-owned GPU VA; guest blob alloc/map)

The **host** owns GPU virtual addressing and command scheduling under Venus replay. The guest never manages segments or page tables; it only allocates and maps blob resources over IOCTL (`ALLOC_BLOB` / `MAP_BLOB`). The memory model:

```
┌──────────────────────────────────────────┐
│  Guest physical memory (Windows RAM)     │
│  ┌───────────────────────────────────┐   │
│  │  Blob resource backing store      │   │  ← virtiogpu hostmem= region
│  │  (host-visible BAR window)        │   │     mapped into guest GPA
│  └───────────────────────────────────┘   │
│  ┌───────────────────────────────────┐   │
│  │  Venus command ring buffer        │   │  ← Shared memory for commands
│  └───────────────────────────────────┘   │
└──────────────────────────────────────────┘
```

- **Blob backing:** the virtio-gpu `hostmem=` region is exposed as a prefetchable 64-bit PCI BAR advertised by a `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` cap with `shmid == VIRTIO_GPU_SHM_ID_HOST_VISIBLE`. The KMD records that BAR's guest-physical base during the cap scan; on `MAP_BLOB` it computes `gpa = host_visible_bar_base + offset`, builds an MDL, `MmMapLockedPagesSpecifyCache(mdl, UserMode, ...)`, and returns the resulting **user VA** to the ICD (see `ARCH.md` §6).
- **Allocations:** Each Vulkan VkBuffer/VkImage = one virtio-gpu blob resource ID + backing pages (`ALLOC_BLOB`).
- **Command buffers:** Venus-encoded commands submitted via the `SUBMIT_VENUS` IOCTL, forwarded opaquely through the virtqueue.

---

## Performance Architecture

To hit the 40–60% native GPU performance target:

1. **Zero-copy memory:** Use blob resources + `hostmem=8G` so the guest directly writes into host-visible memory without a memcpy through QEMU.
2. **Batching:** Accumulate Venus commands in a per-context ring buffer; flush on `vkQueueSubmit` boundary, not per-command.
3. **Async fence polling:** Use `VIRTIO_GPU_FLAG_FENCE` + a `fence_id`→KEVENT signalled from the interrupt DPC (the `WAIT_FENCE` IOCTL blocks on it) to avoid spinning.
4. **Descriptor caching:** Venus supports pipeline/descriptor set handle caching; use it to avoid re-encoding static descriptors.
5. **NO QEMU FPS CAP:** QEMU limits virtio-gpu fence polling to 100fps by default. This only affects scanout, not 3D submit. Venus compute/render is not affected.

---

## Security Boundary

The KMD runs in kernel-mode and accepts command buffers from the ICD (user-mode). Since we are targeting a VM scenario (not production signing), the threat model is:
- **In scope:** Crashing the guest kernel must not affect the host
- **In scope:** Invalid Venus commands must not crash virglrenderer
- **Out of scope:** Malicious guest kernel exploiting the host (KVM's responsibility)

The KMD must validate:
- Command buffer bounds (pointer + length within the mapped segment)
- Resource IDs are valid before use
- Fence IDs are within range

---

## Key References

| Topic | URL |
|-------|-----|
| KMDF (WDF) overview | https://learn.microsoft.com/en-us/windows-hardware/drivers/wdf/getting-started-with-kmdf |
| WdfDeviceCreateDeviceInterface | https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdfdevice/nf-wdfdevice-wdfdevicecreatedeviceinterface |
| SetupDiGetClassDevs (device-interface discovery) | https://learn.microsoft.com/en-us/windows/win32/api/setupapi/nf-setupapi-setupdigetclassdevsw |
| DeviceIoControl / CTL_CODE | https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-deviceiocontrol |
| Khronos Vulkan loader — ICD registration | https://github.com/KhronosGroup/Vulkan-Loader/blob/main/docs/LoaderDriverInterface.md |
| windows-drivers-rs | https://github.com/microsoft/windows-drivers-rs |
| Venus protocol repo | https://gitlab.freedesktop.org/virgl/venus-protocol |
| virglrenderer Venus src | https://gitlab.freedesktop.org/virgl/virglrenderer/-/tree/master/src/venus |
| Mesa Venus driver (Linux ref) | https://gitlab.freedesktop.org/mesa/mesa/-/tree/main/src/virtio/vulkan |
| virtio-win kvm drivers | https://github.com/virtio-win/kvm-guest-drivers-windows/tree/master/viogpu |
| Prior art: mvisor Win vGPU (virgl/GL, WDF) — transport reference | https://github.com/tenclass/mvisor-win-vgpu-driver |
| Prior art: virtio-gpu-win-icd | https://github.com/Keenuts/virtio-gpu-win-icd |
| Prior art: qemu-3dfx (ship-your-own-ICD model) | https://github.com/kjliew/qemu-3dfx |
| VirtIO spec 1.2 GPU section | https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html#sec-gpu |
| QEMU virtio-gpu-gl | https://www.qemu.org/docs/master/system/devices/virtio-gpu.html |
