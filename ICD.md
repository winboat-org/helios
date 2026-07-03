# ICD.md — Vulkan ICD Implementation Guide

> **⚠️ The HAND-WRITTEN ICD described below is SUPERSEDED by a port of Mesa's `venus` driver** (ARCH.md §5; `mesa-venus-icd-port` memory). The chosen ICD reuses Mesa's mature, byte-correct `vn_protocol_driver_*` Venus encoder and adds only a `vn_renderer_helios.c` backend over the IOCTL channel — we do **not** hand-roll the encoder. Treat the encoder/instance/device sections here as background. What REMAINS authoritative is the **ICD↔KMD contract**: the Vulkan-loader registration (Khronos registry JSON), the required `vk_icd*` exports, and the `DeviceIoControl` / `GUID_DEVINTERFACE_HELIOS` transport (§2.2) that `vn_renderer_helios.c` drives.
>
> **➡️ The Phase 5 implementation brief is [`icd/PHASE5_HANDOVER.md`](icd/PHASE5_HANDOVER.md)** — start there. It has the concrete, verified port plan: the vn_renderer→IOCTL vtable mapping, the exact meson edits + `meson setup` command + configure gates, the hardcoded `vn_renderer_info` (Helios has no GET_CAPSET IOCTL), the MSVC `.def` export requirement, and the ranked risks (the ring-shmem `abort()` / `blob_id=0` re-verify first). Mesa is vendored as a submodule at `icd/mesa` (fork: github.com/rupansh/mesa-helios).

## Overview

The Helios ICD (`helios_icd.dll`) is a **Vulkan Installable Client Driver** — a DLL that the Vulkan loader (`vulkan-1.dll`) loads when an application calls `vkCreateInstance`. The ICD implements the Vulkan API by encoding calls into the Venus protocol and submitting them to the KMD.

DXVK (for D3D11) and VKD3D-Proton (for D3D12) will call into this ICD as if it were a native Vulkan driver.

**References:**
- Vulkan Loader Interface: https://github.com/KhronosGroup/Vulkan-Loader/blob/main/docs/LoaderDriverInterface.md
- Vulkan ICD spec: https://vulkan.lunarg.com/doc/sdk/latest/windows/loader_driver_interface.html
- ash (Vulkan Rust bindings): https://docs.rs/ash/latest/ash/

---

## 1. Vulkan ICD Entry Points

The Vulkan loader identifies ICDs via a JSON manifest file and loads the DLL, then calls the negotiation function.

### 1.1 ICD Manifest (`helios_vulkan.json`)

```json
{
    "file_format_version": "1.0.0",
    "ICD": {
        "library_path": ".\\helios_icd.dll",
        "api_version": "1.3.0"
    }
}
```

Register this manifest in the Windows registry:
```
HKLM\SOFTWARE\Khronos\Vulkan\Drivers\
  "C:\Windows\System32\helios_vulkan.json" = DWORD:0
```

The KMD is a System-class KMDF universal INF, which **cannot write absolute `HKLM` values** — so the **ICD installer (or a postinstall script) writes this Khronos Vulkan Drivers registry value**, independent of the KMD INF. (Precedent: lavapipe and SwiftShader register via exactly this mechanism with no display adapter.)

For the Mesa Venus Helios ICD, the canonical deployment command is:

```powershell
Z:\tools\install-helios-icd.ps1
```

It installs the DLL under `C:\ProgramData\HeliosVulkan` using a content-hashed
filename, writes the JSON manifest there, removes stale Helios/Virtio registry
entries, and registers the ProgramData manifest under the Khronos Vulkan Drivers
key. Tests should use
`VK_DRIVER_FILES=C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json` when
forcing this ICD explicitly.

### 1.2 Required ICD Exports

```rust
// src/lib.rs — ICD entry points

// These functions MUST be exported with these exact names.
// The Vulkan loader calls them by name.

/// Vulkan loader negotiation (called first)
#[no_mangle]
pub extern "C" fn vk_icdNegotiateLoaderICDInterfaceVersion(
    p_supported_version: *mut u32,
) -> VkResult {
    // We support loader interface version 5 (Vulkan 1.1+)
    // https://github.com/KhronosGroup/Vulkan-Loader/blob/main/docs/LoaderDriverInterface.md#icd-interface-versions
    const HELIOS_ICD_INTERFACE_VERSION: u32 = 5;
    unsafe {
        if *p_supported_version > HELIOS_ICD_INTERFACE_VERSION {
            *p_supported_version = HELIOS_ICD_INTERFACE_VERSION;
        }
    }
    VkResult::SUCCESS
}

/// Get instance-level proc addresses
#[no_mangle]
pub extern "C" fn vk_icdGetInstanceProcAddr(
    instance: VkInstance,
    p_name: *const i8,
) -> PFN_vkVoidFunction {
    let name = unsafe { std::ffi::CStr::from_ptr(p_name) };
    dispatch::get_instance_proc_addr(instance, name)
}

/// Get physical device proc addresses (loader interface v4+)
#[no_mangle]
pub extern "C" fn vk_icdGetPhysicalDeviceProcAddr(
    instance: VkInstance,
    p_name: *const i8,
) -> PFN_vkVoidFunction {
    let name = unsafe { std::ffi::CStr::from_ptr(p_name) };
    dispatch::get_physical_device_proc_addr(instance, name)
}
```

### 1.3 Dispatch Table

```rust
// src/dispatch.rs

pub fn get_instance_proc_addr(
    _instance: VkInstance,
    name: &std::ffi::CStr,
) -> PFN_vkVoidFunction {
    match name.to_bytes() {
        b"vkCreateInstance"          => Some(unsafe { std::mem::transmute(instance::vk_create_instance as *const ()) }),
        b"vkDestroyInstance"         => Some(unsafe { std::mem::transmute(instance::vk_destroy_instance as *const ()) }),
        b"vkEnumeratePhysicalDevices"=> Some(unsafe { std::mem::transmute(instance::vk_enumerate_physical_devices as *const ()) }),
        b"vkGetPhysicalDeviceProperties" => Some(unsafe { std::mem::transmute(phys_device::vk_get_physical_device_properties as *const ()) }),
        b"vkGetPhysicalDeviceFeatures"   => Some(unsafe { std::mem::transmute(phys_device::vk_get_physical_device_features as *const ()) }),
        b"vkCreateDevice"            => Some(unsafe { std::mem::transmute(device::vk_create_device as *const ()) }),
        b"vkDestroyDevice"           => Some(unsafe { std::mem::transmute(device::vk_destroy_device as *const ()) }),
        b"vkGetDeviceProcAddr"       => Some(unsafe { std::mem::transmute(get_device_proc_addr as *const ()) }),
        // ... (all Vulkan functions we implement)
        _ => None,
    }
}
```

---

## 2. Instance Creation

### 2.1 `vkCreateInstance`

```rust
// src/instance.rs

pub extern "C" fn vk_create_instance(
    p_create_info: *const VkInstanceCreateInfo,
    _p_allocator: *const VkAllocationCallbacks,
    p_instance: *mut VkInstance,
) -> VkResult {
    let create_info = unsafe { &*p_create_info };

    // 1. Open the KMD device interface (SetupDi + CreateFile, see §2.2)
    let kmd = match KmdConnection::open() {
        Ok(k) => k,
        Err(_) => return VkResult::ERROR_INITIALIZATION_FAILED,
    };

    // 2. Create a Venus context in the KMD
    let ctx_id = kmd.create_venus_context()
        .map_err(|_| VkResult::ERROR_INITIALIZATION_FAILED)?;

    // 3. Encode and submit Venus vkCreateInstance
    let mut enc = VnEncoder::new();
    encode_vkCreateInstance(&mut enc, create_info);
    let cmd_buf = enc.finish();

    let fence_id = kmd.next_fence();
    kmd.submit_venus(ctx_id, fence_id, &cmd_buf)
        .map_err(|_| VkResult::ERROR_INITIALIZATION_FAILED)?;

    // 4. Wait for completion and read back the VkInstance handle
    let instance_handle = kmd.wait_and_read_handle(fence_id)
        .map_err(|_| VkResult::ERROR_INITIALIZATION_FAILED)?;

    // 5. Allocate our InstanceState, store the Venus handle
    let state = Box::new(InstanceState {
        kmd,
        ctx_id,
        venus_handle: instance_handle,
    });

    // 6. Return an opaque pointer as VkInstance
    // VkInstance is a non-dispatchable handle (u64 on 64-bit)
    unsafe { *p_instance = Box::into_raw(state) as VkInstance; }

    VkResult::SUCCESS
}
```

### 2.2 KMD Connection

> **Transport: the KMD is a System-class KMDF driver, reached by `DeviceIoControl` on the device interface `GUID_DEVINTERFACE_HELIOS` — NOT `D3DKMT`/dxgkrnl.** There is no adapter, no device handle, no escape carrier. The ICD discovers the device with `SetupDiGetClassDevs` + `CreateFile`, then issues vendor IOCTLs whose input/output buffers carry the unchanged `helios_protocol` op structs. The six ops keep their exact wire layout; only the transport changes.

```rust
// src/transport.rs — KMDF DeviceIoControl communication

// We discover the KMD's device interface and open a HANDLE to it, then
// issue vendor IOCTLs. No D3DKMT/dxgkrnl involvement.
use windows::Win32::Devices::DeviceAndDriverInstallation::*; // SetupDiGetClassDevs, ...
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::CreateFileW;
use windows::Win32::System::IO::DeviceIoControl;

// CTL_CODE(DeviceType, Function, Method, Access)
//   = (DeviceType<<16) | (Access<<14) | (Function<<2) | Method
// DeviceType = FILE_DEVICE_UNKNOWN (0x22), Access = FILE_READ_DATA|FILE_WRITE_DATA (3),
// Methods: BUFFERED=0, IN_DIRECT=1, OUT_DIRECT=2, NEITHER=3. (ARCH.md §3)
const IOCTL_HELIOS_CTX_CREATE:   u32 = 0x0022_E400; // fn 0x900, BUFFERED
const IOCTL_HELIOS_CTX_DESTROY:  u32 = 0x0022_E404; // fn 0x901, BUFFERED
const IOCTL_HELIOS_SUBMIT_VENUS: u32 = 0x0022_E409; // fn 0x902, IN_DIRECT
const IOCTL_HELIOS_ALLOC_BLOB:   u32 = 0x0022_E40C; // fn 0x903, BUFFERED
const IOCTL_HELIOS_MAP_BLOB:     u32 = 0x0022_E412; // fn 0x904, OUT_DIRECT
const IOCTL_HELIOS_WAIT_FENCE:   u32 = 0x0022_E414; // fn 0x905, BUFFERED

pub struct KmdConnection {
    device:        HANDLE,
    fence_counter: std::sync::atomic::AtomicU64,
}

impl KmdConnection {
    pub fn open() -> Result<Self, KmdError> {
        // 1. Enumerate the present devices exposing GUID_DEVINTERFACE_HELIOS.
        let dev_info = unsafe {
            SetupDiGetClassDevsW(
                Some(&helios_protocol::GUID_DEVINTERFACE_HELIOS),
                None,
                None,
                DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
            )
        }?;

        // 2. Grab the first interface and resolve its device path.
        let mut iface = SP_DEVICE_INTERFACE_DATA {
            cbSize: core::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        unsafe {
            SetupDiEnumDeviceInterfaces(
                dev_info,
                None,
                &helios_protocol::GUID_DEVINTERFACE_HELIOS,
                0,
                &mut iface,
            )
        }?;
        // SetupDiGetDeviceInterfaceDetailW: first call sizes, second fills `path`.
        let path: Vec<u16> = get_device_interface_detail(dev_info, &mut iface)?;

        // 3. Open a HANDLE to the device.
        let device = unsafe {
            CreateFileW(
                windows::core::PCWSTR(path.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        }?;

        Ok(Self {
            device,
            fence_counter: std::sync::atomic::AtomicU64::new(1),
        })
    }

    pub fn submit_venus(
        &self,
        ctx_id: u32,
        fence_id: u64,
        venus_data: &[u8],
    ) -> Result<(), KmdError> {
        // Build the IOCTL input buffer: header + payload + venus data.
        // Byte layout is IDENTICAL to the old escape path — only the carrier
        // changed — so `helios_protocol`'s op struct is unchanged.
        let payload_size = core::mem::size_of::<HeliosSubmitVenusReq>() + venus_data.len();
        let mut ioctl_buf = vec![0u8; payload_size];

        let header = unsafe {
            &mut *(ioctl_buf.as_mut_ptr() as *mut HeliosSubmitVenusReq)
        };
        header.hdr.magic    = 0x48454C53;
        header.hdr.cmd_type = IOCTL_HELIOS_SUBMIT_VENUS;
        header.hdr.version  = 1;
        header.hdr.size     = payload_size as u32;
        header.ctx_id       = ctx_id;
        header.fence_id     = fence_id;
        header.buffer_size  = venus_data.len() as u32;

        let data_offset = core::mem::size_of::<HeliosSubmitVenusReq>();
        ioctl_buf[data_offset..].copy_from_slice(venus_data);

        // Issue the IOCTL. METHOD_IN_DIRECT delivers the variable payload via a
        // locked MDL; no output buffer.
        let mut bytes: u32 = 0;
        unsafe {
            DeviceIoControl(
                self.device,
                IOCTL_HELIOS_SUBMIT_VENUS,
                Some(ioctl_buf.as_ptr() as *const _),
                ioctl_buf.len() as u32,
                None,
                0,
                Some(&mut bytes),
                None,
            )
        }?;
        Ok(())
    }

    pub fn next_fence(&self) -> u64 {
        self.fence_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn wait_fence(&self, fence_id: u64, timeout_ns: u64) -> Result<(), KmdError> {
        // IOCTL_HELIOS_WAIT_FENCE (BUFFERED): the KMD blocks on the per-fence
        // KEVENT (fence_id -> KEVENT table) up to timeout_ns, then completes.
        // STUB: wire the real HeliosWaitFence request struct.
        let req = HeliosWaitFence { fence_id, timeout_ns };
        let mut bytes: u32 = 0;
        unsafe {
            DeviceIoControl(
                self.device,
                IOCTL_HELIOS_WAIT_FENCE,
                Some(&req as *const _ as *const _),
                core::mem::size_of::<HeliosWaitFence>() as u32,
                None,
                0,
                Some(&mut bytes),
                None,
            )
        }?;
        Ok(())
    }
}
```

---

## 3. Physical Device Enumeration

```rust
// src/phys_device.rs

pub extern "C" fn vk_enumerate_physical_devices(
    instance: VkInstance,
    p_physical_device_count: *mut u32,
    p_physical_devices: *mut VkPhysicalDevice,
) -> VkResult {
    let state = unsafe { &mut *(instance as *mut InstanceState) };

    // Query from host via Venus: vkEnumeratePhysicalDevices
    let mut enc = VnEncoder::new();
    encode_vkEnumeratePhysicalDevices(&mut enc, state.venus_handle, unsafe { *p_physical_device_count });
    let cmd_buf = enc.finish();

    let fence_id = state.kmd.next_fence();
    state.kmd.submit_venus(state.ctx_id, fence_id, &cmd_buf).ok()?;
    
    // Read response: [u32 count, u64 handles...]
    let response = state.kmd.wait_and_read_response(fence_id).ok()?;
    let count = u32::from_le_bytes(response[0..4].try_into().unwrap());

    if p_physical_devices.is_null() {
        unsafe { *p_physical_device_count = count; }
        return VkResult::SUCCESS;
    }

    let out_count = unsafe { *p_physical_device_count }.min(count);
    for i in 0..out_count as usize {
        let venus_phys = u64::from_le_bytes(response[4 + i*8..4 + i*8 + 8].try_into().unwrap());
        // Wrap Venus physical device handle in our PhysDevState
        let phys = Box::new(PhysDevState {
            venus_handle: venus_phys,
            instance_state: state as *mut _,
        });
        unsafe { *p_physical_devices.add(i) = Box::into_raw(phys) as VkPhysicalDevice; }
    }
    unsafe { *p_physical_device_count = out_count; }

    if out_count < count { VkResult::INCOMPLETE } else { VkResult::SUCCESS }
}
```

---

## 4. Implementation Order for Vulkan Functions

Implement in this order (each enables more test coverage):

### Tier 1: Minimum viable (`vulkaninfo` passes)
- [ ] `vkCreateInstance` / `vkDestroyInstance`
- [ ] `vkEnumeratePhysicalDevices`
- [ ] `vkGetPhysicalDeviceProperties` / `vkGetPhysicalDeviceProperties2`
- [ ] `vkGetPhysicalDeviceFeatures` / `vkGetPhysicalDeviceFeatures2`
- [ ] `vkGetPhysicalDeviceQueueFamilyProperties`
- [ ] `vkGetPhysicalDeviceMemoryProperties`
- [ ] `vkGetPhysicalDeviceFormatProperties`
- [ ] `vkEnumerateDeviceExtensionProperties`
- [ ] `vkEnumerateInstanceExtensionProperties`

### Tier 2: Device creation (`vkcube` passes)
- [ ] `vkCreateDevice` / `vkDestroyDevice`
- [ ] `vkGetDeviceQueue`
- [ ] `vkCreateCommandPool` / `vkDestroyCommandPool`
- [ ] `vkAllocateCommandBuffers` / `vkFreeCommandBuffers`
- [ ] `vkBeginCommandBuffer` / `vkEndCommandBuffer`
- [ ] `vkQueueSubmit` / `vkQueueWaitIdle`
- [ ] `vkCreateFence` / `vkDestroyFence` / `vkWaitForFences` / `vkResetFences`
- [ ] `vkCreateSemaphore` / `vkDestroySemaphore`

### Tier 3: Memory (`vkAllocateMemory` chain)
- [ ] `vkAllocateMemory` / `vkFreeMemory`
- [ ] `vkMapMemory` / `vkUnmapMemory`
- [ ] `vkFlushMappedMemoryRanges` / `vkInvalidateMappedMemoryRanges`
- [ ] `vkCreateBuffer` / `vkDestroyBuffer`
- [ ] `vkGetBufferMemoryRequirements`
- [ ] `vkBindBufferMemory`
- [ ] `vkCreateImage` / `vkDestroyImage`
- [ ] `vkGetImageMemoryRequirements`
- [ ] `vkBindImageMemory`

### Tier 4: Rendering (DXVK works)
- [ ] `vkCreateRenderPass` / `vkDestroyRenderPass`
- [ ] `vkCreateFramebuffer` / `vkDestroyFramebuffer`
- [ ] `vkCreateShaderModule` / `vkDestroyShaderModule`
- [ ] `vkCreateGraphicsPipelines` / `vkDestroyPipeline`
- [ ] `vkCreatePipelineLayout` / `vkDestroyPipelineLayout`
- [ ] `vkCreateDescriptorSetLayout` / `vkDestroyDescriptorSetLayout`
- [ ] `vkCreateDescriptorPool` / `vkDestroyDescriptorPool`
- [ ] `vkAllocateDescriptorSets` / `vkFreeDescriptorSets`
- [ ] `vkUpdateDescriptorSets`
- [ ] `vkCmdBeginRenderPass` / `vkCmdEndRenderPass`
- [ ] `vkCmdBindPipeline`
- [ ] `vkCmdBindVertexBuffers` / `vkCmdBindIndexBuffer`
- [ ] `vkCmdDrawIndexed` / `vkCmdDraw`
- [ ] `vkCmdCopyBuffer` / `vkCmdCopyImage`

### Tier 5: Presentation (Win32 WSI through Mesa Venus)
The active ICD is Mesa Venus with Helios' `vn_renderer_helios.c` backend, and Win32 WSI is now functional enough
for `vkcube` on the System-class KMDF path. This is still **application WSI**, not a WDDM desktop present path:
the Helios KMD is not a display miniport and does not participate in DWM/DXGI scanout ownership. Looking Glass IDD
+ KVMFR/ivshmem is the current desktop-output path.

Recent caveats:

- `HOST_VISIBLE|HOST_COHERENT` behavior is implemented with cached coherent mappings and explicit renderer-side
  synchronization; do not reintroduce the old Venus fence-shortcut patches that marked submits complete before
  their host fence retired.
- Sync values are retired only after `IOCTL_HELIOS_WAIT_FENCE` confirms the corresponding fence. This avoids WSI
  observing a render/present sync as complete while an older host frame can still surface.
- Windowed WSI performance should be measured separately from Looking Glass IDD capture/export stalls. A hitch in
  the displayed desktop can come from either Venus/KMD waits or the IDD/KVMFR copy path.
- `HELIOS_WSI_PERF=1` enables opt-in Mesa WSI timing. Use it with `HELIOS_WSI_PERF_FILE=%USERPROFILE%\helios-doom-wsi-perf.txt`
  to split a software-present frame into common WSI fence wait, memory invalidate, Win32 copy, `GetDC`, and
  `StretchDIBits` cost. Keep `HELIOS_PERF_LIVE` unset for Doom runs; live per-IOCTL logging perturbs timing.
- The Win32 software WSI backend copies mapped Venus image data into the normal DIB by default before calling
  GDI. The fallback path keeps GDI on its own DIB section and presents with `BitBlt`; directly passing the mapped
  Venus image to GDI is kept only as `HELIOS_WSI_DIRECT_MAP=1` for A/B testing. The real bottleneck was cache
  coherency when GDI read the Venus-mapped BAR memory directly, not `StretchDIBits` being inherently slow.
- The direct Looking Glass producer path (`HELIOS_LG_DIRECT`, `\\.\pipe\LookingGlassIDDHelios`, and the client-side
  Helios overlay queue) was removed on 2026-06-17. The default GDI DIB-shadow + `BitBlt` path measures equivalently
  after the cache-coherency fix, so the extra KVMFR overlay producer only added maintenance risk.
- **NVIDIA host renderer: root cause found and fixed (2026-06-10).** The Doom-on-NVIDIA white screen (working only
  early after host boot, escalating to Xid 31 `FAULT_UNSUPPORTED_APERTURE` GPU faults and Xid 13 FECS fatals) was
  spec-violating Venus behavior that ANV tolerates and NVIDIA does not: vkr force-exports every HOST_VISIBLE
  allocation (`VkExportMemoryAllocateInfo`, dma_buf on NVIDIA 610), while venus created buffers/images with no
  matching `VkExternalMemoryBufferCreateInfo`/`ImageCreateInfo` — every host-visible bind violated VUID 02726/02728
  (this hole exists upstream too). A second bug created WSI swapchain command pools on queue families never enabled
  on the logical device (VUID 01937; NVIDIA exposes 5–6 families vs Intel's 1–2). Both fixed in `icd/mesa`
  (`venus: match vkr's force-export external memory…`, `wsi: only create swapchain cmd pools…`); found via
  host-side validation layers in the render server (`HELIOS_VKR_DEBUG=validate`, log tee'd to
  `/tmp/helios-qemu-stderr.log` by the launcher). Doom 2016 now renders correctly at 120+ fps on the NVIDIA host.
  Known accepted residue: a LINEAR+PREINITIALIZED app image (e.g. vkcube's staging texture) legally cannot carry
  external info (VUID 01443) and keeps the upstream bind mismatch.
- **The Xid 13 "Graphics FECS Exception: UCODE Fatal Error" host freeze is fixed (2026-06-18, by codex).** It was
  not a standing defect to design around: the freeze was tripped by the guest Venus external-memory bind UB +
  host-visible cache-coherency bugs above (open-gpu-kernel-modules#1080 documents the GB202 GSP halt that the
  mis-issued work used to trigger). With those fixed in `icd/mesa`, NVIDIA runs the full Venus path without the FECS
  freeze. The earlier guidance — "never leave a broken-rendering run sitting" and keeping the host compositor on the
  Intel iGPU during NVIDIA runs — is now precautionary only, not a hard requirement.

### Performance improvement candidates (2026-06-10, NVIDIA baseline 120+ fps)

Ordered by expected payoff; measure with `HELIOS_PERF=1` + `HELIOS_WSI_PERF=1` before/after each.

1. **Re-measure clean.** The 120 fps figure was taken with `HELIOS_VKR_DEBUG=validate` active (host-side validation
   layers in the render server are expensive). Unset it — and make sure `VN_PERF=no_multi_ring` (a removed
   diagnostic artifact) is not set — before reading any number.
2. **Coherent-cached clflush sweeps.** `vn_device_memory_flush/invalidate_coherent_cached_mappings` cache-op the
   FULL mapped range of every registered HOST_CACHED mapping on every `vkQueueSubmit` and every successful
   fence/semaphore wait — O(total mapped bytes) each time. Candidates: dirty-range tracking; skipping the op
   entirely when guest mapping and host `map_info` are both CACHED (KVM with `honor-guest-pat=on` is already
   coherent for host-WB memory); registering only memories the WSI/GDI path actually reads.
3. **KMD `quiesce_into` bubbles.** Every synchronous control-queue op (ALLOC_BLOB / MAP_BLOB / RELEASE_BLOB) drains
   all in-flight fenced submits first. Doom streams allocations during gameplay, so each one is a pipeline stall.
   Candidate: quiesce only what the op actually orders against (e.g. the blob's own mem-alloc batch), not the world.
4. **`helios_wait` slow path.** Issues one `IOCTL_HELIOS_WAIT_FENCE` per pending fence, sequentially, each granted
   the full timeout. Batch into one IOCTL taking a fence list, or wait only the max fence per ring (virtio fences
   retire in order per ring). Longer term: a shared fence page mapped user-mode would make fence polls syscall-free.
5. **Present copy path.** Each present is a full-frame CPU copy (Venus image → DIB shadow or KVMFR slot). Dirty-rect
   tracking in the WSI copy, and skipping the GDI shadow entirely while the Looking Glass direct producer is active,
   are both unexplored.
6. **External-info injection cost.** All buffers now carry the renderer handle type; if NVIDIA penalizes external
   allocations (alignment padding showed up in vkr's opaque-fd path), a refinement is injecting only for buffers
   whose usage can plausibly bind host-visible memory. Only worth doing if profiling shows it.
7. **Default render node.** The launcher still defaults QEMU/Venus to Intel (`HELIOS_QEMU_RENDER_GPU=intel`,
   40–45 fps). With the Xid 13 FECS host freeze fixed (2026-06-18), NVIDIA no longer needs a stability soak before
   being trusted — flipping the default to NVIDIA is a free ~3x whenever desired.
- WDDM/DXGI remains a possible future escape hatch, not the next default step. It would require a real WDDM render
  adapter path rather than only the current System-class DeviceIoControl KMD, and it would not by itself implement
  D3D12. Optimize and measure the current Venus path first.

---

## 5. Extensions to Expose

Venus supports a large set of Vulkan extensions. Expose the ones DXVK requires:

**Minimum for DXVK 2.x:**
```
VK_KHR_swapchain (needed for Win32 WSI / vkcube-style windowed presentation)
VK_KHR_maintenance1/2/3/4
VK_KHR_shader_draw_parameters
VK_EXT_vertex_attribute_divisor
VK_EXT_transform_feedback
VK_EXT_depth_clip_enable
VK_EXT_extended_dynamic_state
VK_KHR_timeline_semaphore
VK_KHR_synchronization2
VK_KHR_dynamic_rendering
VK_EXT_robustness2
VK_EXT_memory_budget
```

Query from the host via `vkEnumerateDeviceExtensionProperties` and pass through the ones in our allowlist.

---

## 6. Testing

```powershell
# Install the Helios ICD by placing helios_vulkan.json and helios_icd.dll
# and registering the manifest.

# Test 1: Loader finds the ICD
vulkaninfo 2>&1 | Select-String "Helios"

# Test 2: Enumerate physical device
$env:VK_LOADER_DEBUG = "all"
vulkaninfo

# Test 3: Render a frame
vkcube  # from VulkanSDK

# Test 4: DXVK smoke test
# Download a D3D11 demo, set DXVK_LOG_LEVEL=info
# Ensure DXVK picks up our ICD:
$env:DXVK_CONFIG_FILE = "dxvk.conf"
# dxvk.conf: d3d11.maxFeatureLevel = 11_1
```

---

## 7. ICD Loader Interface Reference

Full loader-ICD interface spec:
https://github.com/KhronosGroup/Vulkan-Loader/blob/main/docs/LoaderDriverInterface.md

Key interface version history:
| Version | Added capability |
|---------|-----------------|
| 0 | Initial (vk_icdGetInstanceProcAddr) |
| 1 | vk_icdGetInstanceProcAddr stability |
| 2 | Higher-performance `VkPhysicalDevice` dispatchable |
| 3 | `vk_icdNegotiateLoaderICDInterfaceVersion` |
| 4 | `vk_icdGetPhysicalDeviceProcAddr` |
| 5 | Loader calls `vk_icdGetInstanceProcAddr(NULL, "vkGetInstanceProcAddr")` |
| 6 | `vk_icdEnumerateAdapterPhysicalDevices` (DXGI adapter ordering) — **N/A for Helios**: a non-WDDM ICD does not participate in DXGI adapter ordering; the loader enumerates it purely via the `HKLM\SOFTWARE\Khronos\Vulkan\Drivers` JSON manifest (no PnP/WDDM association). |

Target version 5 minimum.
