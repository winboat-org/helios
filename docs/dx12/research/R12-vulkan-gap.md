# R12 — The Vulkan feature gap: what vkd3d-proton needs vs what the Helios Venus ICD + QEMU host expose

**Lane:** R12. **Author:** research agent, 2026-08-05. **Scope:** the *substrate* only — Vulkan
version, device extensions, features, properties, sparse, raytracing. Presentation mechanics belong
to R7; the D3DKMT surface belongs to R6; vkd3d internals belong to R3. Where this lane touches those,
it says so and stops.

**Evidence classes used below, always labelled:**
`[HDR]` the header/source says · `[VKD3D]` vkd3d-proton's code does · `[MESA]` the Helios Mesa venus
fork's code does · `[VIRGL]` virglrenderer 1.3.0's code does · `[LIVE]` measured on the running VM
this session · `[INFER]` my inference. Anything unproven is marked **UNVERIFIED** with the settling
experiment.

**The VM was running during this research** (QEMU pid 4447, `tools/launch-helios-gtk.sh`,
`HELIOS_DISPLAY=sdl`). All `[LIVE]` numbers were captured this boot via `win_exec` and are archived
under `tmp/dx12/research/capture/`.

---

## 0. Verdict first

**YES — the Helios substrate can carry a baseline D3D12 device today, and materially more than
baseline.** The live guest Venus ICD satisfies **every** requirement of vkd3d-proton's own Vulkan
profile up to and including `VP_D3D12_FL_12_2_baseline` — zero feature misses, zero extension
misses. Nothing in the substrate blocks `D3D12CreateDevice`.

The gaps are all in the *optimal/nice-to-have* band plus one correctness landmine:

| Rank | Gap | Layer that must change | Consequence |
|---|---|---|---|
| 1 | `VK_KHR_external_memory_win32` absent, but vkd3d uses `VkExportMemoryAllocateInfo`/`vkGetMemoryWin32HandleKHR` **unguarded** on `_WIN32` | Mesa venus ICD (new native ext) | `D3D12_HEAP_FLAG_SHARED` → likely NULL-fn crash or invalid pNext. See §3.1 |
| 2 | 32-bit (WOW64) Vulkan ICD not registered | packaging / Mesa build | 32-bit `d3d12.dll` finds no adapter |
| 3 | `VK_EXT_descriptor_heap` (guest ready, host not) | **virglrenderer only** | slowest descriptor model; cheapest single fix |
| 4 | `VK_EXT_descriptor_buffer` | venus-protocol + Mesa + virglrenderer | second-best descriptor model unavailable |
| 5 | `VK_KHR_present_id` / `present_wait` (venus disables them on Windows by `#ifdef`) | Mesa venus ICD + virglrenderer | no frame pacing / latency control |
| 6 | `VK_KHR_maintenance8` | venus-protocol + Mesa + virglrenderer | `OPTIONS14.AdvancedTextureOpsSupported = false`. **SM 6.7 itself is still reachable** — see §7.1 |
| 7 | `VK_EXT_opacity_micromap` | venus-protocol + Mesa + virglrenderer | DXR 1.2 → capped at DXR 1.1 |
| 8 | `VK_EXT_memory_budget` | **one guest env var**: `VN_DEBUG=mem_budget` | `QueryVideoMemoryInfo` budget quality |
| 9 | `VK_EXT_device_generated_commands`, `pageable_device_local_memory`, `memory_priority`, `shader_module_identifier`, `AMD_buffer_marker`, `EXT_device_fault`, `maintenance9/10` | venus-protocol + Mesa + virglrenderer | ExecuteIndirect tiering, residency quality, PSO cache perf, breadcrumbs |

Ordered substrate work, cheapest first: **(a)** register a 32-bit ICD *or* decide 64-bit-only;
**(b)** guard or implement `VK_KHR_external_memory_win32`; **(c)** `VN_DEBUG=mem_budget`;
**(d)** resync virglrenderer's venus-protocol + `vkr_extension_table` (unlocks `descriptor_heap`
immediately, and is the vehicle for everything in rows 4–9).

---

## 1. vkd3d-proton's requirements — the definitive list

Submodule at `vkd3d-proton-helios/`, HEAD `2c7ba22c` (`git log --oneline -1`, run this session).

### 1.1 Vulkan version — **1.3 minimum, and vkd3d never opts in above 1.3**

`vkd3d-proton-helios/include/vkd3d.h:53-54`, verbatim:

```c
#define VKD3D_MIN_API_VERSION VK_API_VERSION_1_3
#define VKD3D_MAX_API_VERSION VK_API_VERSION_1_3
```

Three enforcement sites `[VKD3D]`:
- `libs/vkd3d/device.c:1455-1462` — loader `vkEnumerateInstanceVersion` below 1.3 → `E_INVALIDARG`.
- `libs/vkd3d/device.c:3538-3542` — a `VkPhysicalDevice` with `apiVersion < 1.3` is **skipped**
  during selection.
- `libs/d3d12core/main.c:490-495` — the Windows LUID→`VkPhysicalDevice` matcher skips
  sub-1.3 adapters ("Skipped adapter %s as it is below our minimum API version.").

`libs/vkd3d/device.c:1465-1466` and `:4105` clamp *down* to 1.3, so a 1.4 device is used as a 1.3
device. **Helios guest reports 1.4.341** `[LIVE]` — comfortably above.

### 1.2 Hard-required device extensions

Only **one** device extension is in the non-optional list that `d3d12core` passes to
`vkd3d_create_device` — `libs/d3d12core/main.c:659-662`:

```c
static const char * const device_extensions[] =
{
    VK_KHR_SWAPCHAIN_EXTENSION_NAME,
};
```

and optional (`:664-668`) `VK_KHR_swapchain_maintenance1` / `VK_EXT_swapchain_maintenance1`.

Instance side, `libs/d3d12core/main.c:574-581`: required `VK_KHR_surface` +
(`#ifdef _WIN32`) `VK_KHR_win32_surface`; optional `VK_KHR_surface_maintenance1`,
`VK_EXT_surface_maintenance1`, `VK_KHR_get_surface_capabilities2`.

**But that list is misleading.** The real gate is `vkd3d_init_device_caps()`
(`libs/vkd3d/device.c:3243-3489`), which returns `E_INVALIDARG` — killing device creation — on nine
conditions. Verbatim error strings and line numbers:

| Line | Condition that must hold | Error string |
|---|---|---|
| `3288-3292` (ERR at `3291`) | `vertexAttributeInstanceRateDivisor && vertexAttributeInstanceRateZeroDivisor` | `"Lacking support for VK_EXT_vertex_attribute_divisor."` |
| `3295-3298` (ERR at `3297`) | `xfb_properties.transformFeedbackQueries` | `"Lacking support for transform feedback."` |
| `3301-3313` (ERR at `3312`) | storage **and** uniform texel buffer offset single-texel alignment (or alignment bytes == 1) | `"Lacking support for single texel alignment."` |
| `3425-3429` | `vulkan_1_2_features.samplerMirrorClampToEdge` | `"samplerMirrorClampToEdge is not supported…"` |
| `3431-3436` | `robustness2_features.robustBufferAccess2 && robustImageAccess2` | `"Robustness2 features not supported. This is required."` |
| `3438-3442` | `robustness2_features.nullDescriptor` | `"Null descriptor in VK_EXT_robustness2 is not supported…"` |
| `3447-3451` | `vulkan_1_1_features.shaderDrawParameters` | `"shaderDrawParameters is not supported…"` |
| `3453-3457` | `vulkan_info->KHR_push_descriptor` | `"Push descriptors are not supported…"` |
| `3459-3464` | `maintenance_5_features.maintenance5 && maintenance_6_features.maintenance6` | `"maintenance5 and/or maintenance6 not supported…"` |

`libs/vkd3d/device.c:3413-3423` is the one *non*-fatal shortfall: `robustBufferAccessUpdateAfterBind`
absent only produces a `WARN` and robustness is enabled anyway ("otherwise we cannot support D3D12 at
all").

### 1.3 The authoritative machine-readable list: `VP_D3D12_VKD3D_PROTON_profile.json`

The submodule ships a Vulkan Profiles document (848 lines) with **nine** profiles. The relevant ones:

| Profile | Label | Capability sets |
|---|---|---|
| `VP_D3D12_FL_11_0_baseline` | "Minimum baseline to create a device at all." | `baseline_features`, `fl_11_0_properties`, `subgroups_none` |
| `VP_D3D12_FL_11_1_baseline` | FL 11.1 | + `fl_11_1_features` |
| `VP_D3D12_FL_12_0_baseline` | FL 12.0 | + `fl_12_0_features`, `fl_12_0_properties`, `subgroups_60`, `shader_model_60` |
| `VP_D3D12_FL_12_1_baseline` | FL 12.1 | + `fl_12_1_features`, `fl_12_1_features_rov` |
| `VP_D3D12_FL_12_2_baseline` | FL 12.2 (DX Ultimate) | + `fl_12_2_features`, `fl_12_2_properties` |
| `VP_D3D12_FL_12_0_optimal` / `FL_12_2_optimal` | + `shader_model_66`, `optimal_performance` | |

`baseline_features` extensions (11): `VK_KHR_push_descriptor`, `VK_KHR_swapchain`,
`VK_KHR_calibrated_timestamps`, `VK_KHR_maintenance5`, `VK_KHR_maintenance6`,
`VK_EXT_custom_border_color`, `VK_EXT_depth_clip_enable`, `VK_EXT_robustness2`,
`VK_EXT_transform_feedback`, `VK_EXT_vertex_attribute_divisor` (**spec version 3**),
`VK_EXT_dynamic_rendering_unused_attachments`.

`baseline_features` core features include the whole descriptor-indexing set,
`timelineSemaphore`, `bufferDeviceAddress`, `vulkanMemoryModel(+DeviceScope)`, `hostQueryReset`,
`separateDepthStencilLayouts`, `drawIndirectCount`, `samplerMirrorClampToEdge` (Vulkan 1.2);
`shaderDemoteToHelperInvocation`, `synchronization2`, `dynamicRendering`, `maintenance4` (1.3);
`shaderDrawParameters` (1.1); and 29 `VkPhysicalDeviceFeatures` bits ending
`shaderInt16` + `shaderStorageImageWriteWithoutFormat`.

`fl_11_0_properties` requires `robustBufferAccessUpdateAfterBind: true` and
`maxPerStageDescriptorUpdateAfterBind{StorageBuffers,SampledImages,StorageImages} >= 1000000` —
i.e. **1 000 000 UpdateAfterBind descriptors** (`README.md:22-23`).

`fl_12_0_features` is where **sparse** enters: `sparseBinding`, `sparseResidencyAliased`,
`sparseResidencyBuffer`, `sparseResidencyImage2D`, `shaderResourceResidency`, `shaderResourceMinLod`,
plus `samplerFilterMinmax`. `fl_12_0_properties` adds `residencyStandard2DBlockShape: true`,
`residencyAlignedMipSize: false`, `residencyNonResidentStrict: true`.

`fl_12_2_features` is DXR + mesh: `VK_KHR_ray_tracing_pipeline`, `VK_KHR_acceleration_structure`,
`VK_KHR_deferred_host_operations`, `VK_KHR_ray_query`, `VK_KHR_pipeline_library`,
`VK_KHR_fragment_shading_rate`, `VK_EXT_pipeline_library_group_handles`,
`VK_KHR_ray_tracing_maintenance1`, `VK_EXT_mesh_shader`, `VK_EXT_conservative_rasterization`.

`optimal_performance` (14 extensions): `VK_EXT_descriptor_buffer`, `VK_EXT_mutable_descriptor_type`,
`VK_EXT_shader_module_identifier`, `VK_KHR_present_id`, `VK_KHR_present_wait`,
`VK_EXT_extended_dynamic_state2`, `VK_EXT_graphics_pipeline_library`, `VK_KHR_pipeline_library`,
`VK_AMD_buffer_marker`, `VK_EXT_scalar_block_layout`, `VK_EXT_swapchain_maintenance1`,
`VK_KHR_maintenance9`, `VK_KHR_maintenance10`, `VK_KHR_shader_float_controls2`.

`shader_model_66`: `VK_KHR_compute_shader_derivatives` + `VK_EXT_shader_image_atomic_int64`,
`shaderIntegerDotProduct`, `shaderBufferInt64Atomics`, `shaderSharedInt64Atomics`, `shaderInt8`.
`shader_model_67`: `VK_KHR_shader_maximal_reconvergence`, `VK_KHR_shader_quad_control`,
`VK_KHR_maintenance8`.

> ⚠ The profile and the code disagree in two places `[HDR vs VKD3D]`: the profile lists
> `VK_KHR_calibrated_timestamps` and `VK_EXT_dynamic_rendering_unused_attachments` as baseline, but
> `libs/vkd3d/device.c:91` and `:143` have both in `optional_device_extensions` with no hard check.
> **The nine `E_INVALIDARG` checks in §1.2 are what actually fails device creation**; the profile is
> the aspirational spec. Both are satisfied by Helios anyway, so the disagreement is moot here.

### 1.4 The `optional_device_extensions` table — 91 entries

`libs/vkd3d/device.c:66-167`. Structure at `:40-58`: each row is
`{name, offsetof(vkd3d_vulkan_info, member), enable_config_flags, disable_config_flags, minimum_spec_version}`.
Conditional rows worth knowing:
- `VK_EXTENSION_DISABLE_COND(..., VKD3D_CONFIG_FLAG_STATIC(NO_DXR))` for the seven DXR extensions
  (`:70-75`, `:98`, `:127`, `:161`) — `VKD3D_CONFIG=no_dxr` turns them all off.
- `VK_EXTENSION_COND(EXT_DESCRIPTOR_HEAP, ..., VKD3D_CONFIG_FLAG_STATIC(DESCRIPTOR_HEAP))` (`:145`)
  — `VK_EXT_descriptor_heap` is **opt-in via `VKD3D_CONFIG=descriptor_heap`**, not on by default.
- `VK_EXTENSION_COND(EXT_DEVICE_FAULT/EXT_DEVICE_ADDRESS_BINDING_REPORT, …, FAULT)` (`:138`, `:141`)
  and `NV_DEVICE_DIAGNOSTIC_CHECKPOINTS` behind `BREADCRUMBS` (`:154`).
- `#ifdef _WIN32` block at `:99-103`: `VK_KHR_external_memory_win32`, `VK_KHR_external_semaphore_win32`.

### 1.5 README's own statement of hard requirements

`vkd3d-proton-helios/README.md:19-35`, verbatim:

```
There are some hard requirements on drivers to be able to implement D3D12 in a reasonably performant way.

- Vulkan 1.3
- Descriptor indexing with at least 1000000 UpdateAfterBind descriptors for all types except UniformBuffer.
  Essentially all features in `VkPhysicalDeviceDescriptorIndexingFeatures` must be supported.
- Further, the following device features are required:
  - `samplerMirrorClampToEdge`
  - `shaderDrawParameters`
- `VK_EXT_robustness2`
- `VK_KHR_push_descriptor`

Some notable extensions that **should** be supported for optimal or correct behavior.
These extensions will likely become mandatory later.

- `VK_EXT_image_view_min_lod`

`VK_EXT_mutable_descriptor_type` (or the vendor `VALVE` alias) and `VK_EXT_descriptor_buffer` are also highly recommended, but not mandatory.
```

Helios has `VK_EXT_image_view_min_lod` (`minLod: true`) and `VK_EXT_mutable_descriptor_type`
(`mutableDescriptorType: true`) `[LIVE]`. It lacks `VK_EXT_descriptor_buffer`.

---

## 2. What the Helios stack exposes today — the three layers

### 2.0 The layering mechanism, proven

An extension reaches a guest app only if it survives **four** filters, in this order:

1. **Host GPU driver** — NVIDIA 610.43.03 on an RTX PRO 6000 Blackwell `[LIVE, host]`.
2. **virglrenderer's `vkr_extension_table`** — `vkr_physical_device_init_extensions()`
   (`/tmp/virglrenderer-virglrenderer-1.3.0/src/venus/vkr_physical_device.c:245-302`, function opens at `:246`) enumerates the
   real device's extensions and keeps an entry **only if `vkr_extension_get_spec_version(name) != 0`**
   (`:279-284`). That function (`src/venus/vkr_common.c:253-262`, function opens at `:254`) returns 0 unless
   `vkr_extension_table.enabled[index]` — a static table at `vkr_common.c:17` with **182** `= true`
   entries, and the venus-protocol name table it indexes has only **185** names.
3. **Mesa venus's `passthrough` table** — `vn_physical_device_get_passthrough_extensions()`
   (`icd/mesa/src/virtio/vulkan/vn_physical_device.c:1378-1591`), **172** entries. Combining rule at
   `vn_physical_device_init_supported_extensions()` (`:1593-1623`), verbatim:
   ```c
   if (native.extensions[i]) { ...supported... }
   else if (passthrough.extensions[i] && physical_dev->renderer_extensions.extensions[i]) { ...supported... }
   ```
4. **Mesa venus's `native` table** — `vn_physical_device_get_native_extensions()` (`:1248-1375`),
   driver-side implementations that need no renderer support: `VK_KHR_swapchain`,
   `VK_KHR_swapchain_maintenance1`, `VK_EXT_swapchain_maintenance1`, `VK_KHR_swapchain_mutable_format`,
   `VK_KHR_incremental_present`, `VK_EXT_hdr_metadata`, `VK_KHR_deferred_host_operations`,
   `VK_KHR_map_memory2`, `VK_EXT_tooling_info`, `VK_EXT_device_memory_report`, `VK_EXT_pci_bus_info`,
   `VK_KHR_external_memory_fd` + `VK_EXT_external_memory_dma_buf` (Helios-specific, `:1300-1324`),
   and on Windows `VK_KHR_external_semaphore_win32` (`:1271-1277`).

**The host binary in use is confirmed**: `/proc/4447/maps` maps
`/usr/lib/libvirglrenderer.so.1.11.0`, which `pacman -Qi virglrenderer` reports as version
**1.3.0-2**. `qemu-helios/build-helios/hw-display-virtio-gpu-gl.so` links it (`ldd`). And the
1.3.0 source tarball's protocol name table matches the shipped `.so`'s string table **exactly**
(185 names, set-equal) — so the tarball I read is the code that is running.

> ⚠ Note on `VK_KHR_present_id`/`present_wait`: even if virglrenderer gained them, the Helios Mesa
> fork **explicitly disables them on Windows** — `vn_physical_device.c:1334-1341`:
> ```c
> #ifndef VK_USE_PLATFORM_WIN32_KHR
>       exts->KHR_present_id = true;
>       exts->KHR_present_id2 = true;
>       exts->KHR_present_wait = true;
>       exts->KHR_present_wait2 = true;
> #endif /* VK_USE_PLATFORM_WIN32_KHR */
> ```
> That is a two-layer fix.

### 2.1 Guest ICD, live `[LIVE]`

Captured this session on the running VM, archived at `tmp/dx12/research/capture/`:
`vulkaninfo.json` (206 KB, Vulkan Profiles format), `vulkaninfo-full.txt` (77 KB),
`vulkaninfo-summary.txt`.

```
Vulkan Instance Version: 1.4.350
GPU0:
    apiVersion    = 1.4.341
    driverVersion = 26.1.99
    vendorID      = 0x10de
    deviceID      = 0x2bb1
    deviceType    = PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
    deviceName    = Virtio-GPU Venus (NVIDIA RTX PRO 6000 Blackwell Workstation Edition)
    driverID      = DRIVER_ID_MESA_VENUS
    driverName    = venus
    driverInfo    = Mesa 26.2.0-devel (git-f023e5ce48)
    conformanceVersion = 1.4.0.0
```
(`vulkaninfo-summary.txt:23-35`; `vulkaninfo-full.txt:700-703`.)

- **168 device extensions**, **68 feature structs**, **176 format entries**, **6 queue families**.
- Instance extensions include `VK_KHR_surface` and `VK_KHR_win32_surface` — vkd3d's required pair.
- Presentable surface works: `vulkaninfo-full.txt:126-143` shows `VK_KHR_win32_surface` with
  formats `B8G8R8A8_UNORM`, `R8G8B8A8_UNORM`, `B8G8R8A8_SRGB` and present modes
  `IMMEDIATE`/`MAILBOX`/`FIFO`. **No 10-bit or HDR surface format** — a `DXGI_FORMAT_R10G10B10A2_UNORM`
  D3D12 swapchain has nowhere to land. (Hand-off to R7.)

**Venus is a bit-exact passthrough for limits.** Comparing every field of
`VkPhysicalDeviceProperties.limits` guest-vs-host: **0 differences.** Spot values:
`maxPushConstantsSize=256`, `maxBoundDescriptorSets=32`,
`maxPerStageDescriptorUpdateAfterBind{StorageBuffers,SampledImages,StorageImages}=1048576`
(profile needs 1 000 000 ✅), `sparseAddressSpaceSize=1 TiB`, `bufferImageGranularity=1024`,
`maxImageDimension2D=32768`, `robustBufferAccessUpdateAfterBind=true`,
`filterMinmaxSingleComponentFormats=true`, `storage/uniformTexelBufferOffsetSingleTexelAlignment=true`.

Queue families (`vulkaninfo.json`, `queueFamiliesProperties`) — identical to the host's, except that
`vn_physical_device.c:944-951` strips `VIDEO_DECODE`/`VIDEO_ENCODE` bits on Windows:

| # | flags | count |
|---|---|---|
| 0 | GRAPHICS \| COMPUTE \| TRANSFER \| **SPARSE_BINDING** | 16 |
| 1 | TRANSFER \| **SPARSE_BINDING** | 2 |
| 2 | COMPUTE \| TRANSFER \| **SPARSE_BINDING** | 8 |
| 3 | TRANSFER \| SPARSE_BINDING | 4 |
| 4 | TRANSFER \| SPARSE_BINDING | 3 |
| 5 | TRANSFER \| SPARSE_BINDING \| OPTICAL_FLOW_NV | 1 |

So D3D12's DIRECT / COMPUTE / COPY queues each get a **distinct real Vulkan queue family**, and
vkd3d's `VKD3D_QUEUE_FAMILY_SPARSE_BINDING` selection (`libs/vkd3d/device.c:3811-3826`) will land on
family **1**. (The WDDM miniport reports one 3D engine node — but nothing in this path goes through
the WDDM engine model. Hand-off to R5/R6.)

Memory: 2 heaps (95.59 GiB DEVICE_LOCAL, 70.31 GiB host), 5 memory types including
`HOST_VISIBLE|HOST_COHERENT` and `HOST_VISIBLE|HOST_COHERENT|HOST_CACHED`
(`vulkaninfo-full.txt:1136-1200`).

### 2.2 Guest ICD source — the Helios-specific knobs

`icd/mesa/src/virtio/vulkan/vn_common.c:23-39` — `VN_DEBUG` env var, `parse_debug_string(os_get_option("VN_DEBUG"), …)` at `:70`:
`init`, `result`, `vtest`, `wsi`, `no_abort`, `log_ctx_info`, `cache`, **`no_sparse`**, `no_gpl`,
`no_second_queue`, **`no_ray_tracing`**, **`mem_budget`**, **`no_desc_heap`**.
`VN_PERF` at `:42-58` has 14 more.

Sparse is on by default: `physical_dev->sparse_binding_disabled` is set **only** by
`VN_DEBUG(NO_SPARSE)` (`vn_physical_device.c:976-978`), and the masking function
`vn_physical_device_disable_sparse_binding()` (`:1777-1798`) zeroes all nine sparse features and six
sparse properties. The live capture shows them **all true**, so the knob is not set this boot.

`VK_EXT_memory_budget` is gated the other way: `.EXT_memory_budget = VN_DEBUG(MEM_BUDGET)`
(`vn_physical_device.c:1553`) — **off unless asked for**, even though virglrenderer supports it.

### 2.3 Host virglrenderer `[VIRGL]`

virglrenderer **1.3.0-2**, `/usr/lib/libvirglrenderer.so.1.11.0`.
- `VKR_MAX_API_VERSION = VK_API_VERSION_1_4` (`src/venus/vkr_common.h:39`), applied by
  `vkr_api_version_cap_minor()` at `vkr_physical_device.c:314`.
- Its venus-protocol name table declares `{ "VK_MESA_venus_protocol", 385, 3 }` — **spec version 3**
  (`src/venus/venus-protocol/vn_protocol_renderer_info.h:399`).
- **`vkQueueBindSparse` is fully dispatched**: `vkr_dispatch_vkQueueBindSparse()` at
  `src/venus/vkr_queue.c:385-397`, registered at `:655`
  (`dispatch->dispatch_vkQueueBindSparse = vkr_dispatch_vkQueueBindSparse;`), calling
  `vk->QueueBindSparse(args->queue, args->bindInfoCount, args->pBindInfo, args->fence)`.

**Protocol-version asymmetry, and it is currently harmless.** The guest side hardcodes
`info->vk_mesa_venus_protocol_spec_version = 4` in `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:3673`
— the comment at `:3665-3670` says why ("Helios has no GET_CAPSET IOCTL, so these are hardcoded").
The host declares 3. `vn_instance.c:220-221` clamps a *renderer*-reported value down to the guest's
own, so 4 survives. The only consumers are `>= 2` (`vn_ring.c:512`) and `< 3`
(`vn_physical_device.c:539` — the 1.3 apiVersion clamp and the `EXT_host_image_copy` gate at
`:1490`). **Nothing reads `>= 4`**, so the over-claim has no effect today. It is a latent trap the
moment venus adds a v4-gated path.

### 2.4 Host GPU `[LIVE, host]`

Fresh `vulkaninfo` on the Linux host this session (`/tmp/host_vulkaninfo.txt`): GPU0 =
`NVIDIA RTX PRO 6000 Blackwell Workstation Edition`, `driverName = NVIDIA`,
`driverInfo = 610.43.03`, `driverID = DRIVER_ID_NVIDIA_PROPRIETARY`, `conformanceVersion 1.4.3.3`.
The archived profile `docs/reference/host-vulkan-profile-rtx-pro-6000-blackwell.json` (610.43.02,
2026-07-09) lists **281** device extensions and matches the fresh capture on every point I re-checked.

One correction to a natural assumption: **the NVIDIA driver does NOT expose
`VK_EXT_shader_stencil_export`** — it appears in the host `vulkaninfo` only under GPU1 (Intel ARL)
and GPU2 (llvmpipe), never in GPU0's block (`grep -n VK_EXT_shader_stencil_export /tmp/host_vulkaninfo.txt`
→ lines 8759, 10298 only; GPU0's block is lines 1932-7986). So
`D3D12_FEATURE_DATA_D3D12_OPTIONS.PSSpecifiedStencilRefSupported` would be `false`
(`libs/vkd3d/device.c:10178`) even on native NVIDIA Vulkan — this is **not** a Helios gap.

---

## 3. The three-column table

Columns: what vkd3d wants · guest ICD (live) · virglrenderer's `vkr_extension_table` ·
Mesa venus `passthrough` table · host GPU.
Reading note: `VK_KHR_swapchain`, `VK_KHR_swapchain_maintenance1`, `VK_EXT_swapchain_maintenance1`,
`VK_KHR_deferred_host_operations` and `VK_KHR_external_semaphore_win32` show `NO` in the middle two
columns because they are **venus-native** (§2.0 filter 4), not passthrough — the guest column is
authoritative.

| Extension | vkd3d needs | guest ICD (live) | host venus (vkr 1.3.0) | Mesa venus passthrough tbl | host GPU |
|---|---|---|---|---|---|
| `VK_KHR_swapchain` | REQUIRED (d3d12core) | yes | NO | NO | yes |
| `VK_KHR_push_descriptor` | REQUIRED (hard check) | yes | yes | yes | yes |
| `VK_KHR_maintenance5` | REQUIRED (hard check) | yes | yes | yes | yes |
| `VK_KHR_maintenance6` | REQUIRED (hard check) | yes | yes | yes | yes |
| `VK_EXT_robustness2` | REQUIRED (hard check) | yes | yes | yes | yes |
| `VK_EXT_transform_feedback` | REQUIRED (hard check) | yes | yes | yes | yes |
| `VK_EXT_vertex_attribute_divisor` | REQUIRED (hard check) | yes | yes | yes | yes |
| `VK_EXT_custom_border_color` | REQUIRED (profile baseline) | yes | yes | yes | yes |
| `VK_EXT_depth_clip_enable` | REQUIRED (profile baseline) | yes | yes | yes | yes |
| `VK_EXT_dynamic_rendering_unused_attachments` | REQUIRED (profile baseline) | yes | yes | yes | yes |
| `VK_KHR_calibrated_timestamps` | REQUIRED (profile baseline) | yes | yes | yes | yes |
| `VK_EXT_descriptor_indexing` | core 1.2 (REQUIRED features) | yes | yes | yes | yes |
| `VK_EXT_image_view_min_lod` | SHOULD (README:33) | yes | yes | yes | yes |
| `VK_EXT_mutable_descriptor_type` | recommended (README:35) | yes | yes | yes | yes |
| `VK_EXT_descriptor_buffer` | recommended (README:35) | NO | NO | NO | yes |
| `VK_EXT_descriptor_heap` | optional (DESCRIPTOR_HEAP cfg flag) | NO | NO | yes | yes |
| `VK_EXT_conservative_rasterization` | opt -> FL 12.1 / ConsRast tiers | yes | yes | yes | yes |
| `VK_EXT_fragment_shader_interlock` | opt -> ROVs (FL 12.1) | yes | yes | yes | yes |
| `VK_KHR_acceleration_structure` | opt -> DXR | yes | yes | yes | yes |
| `VK_KHR_ray_tracing_pipeline` | opt -> DXR 1.0 | yes | yes | yes | yes |
| `VK_KHR_ray_query` | opt -> DXR 1.1 | yes | yes | yes | yes |
| `VK_KHR_deferred_host_operations` | opt -> DXR | yes | NO | NO | yes |
| `VK_KHR_ray_tracing_maintenance1` | opt -> DXR 1.1 | yes | yes | yes | yes |
| `VK_KHR_pipeline_library` | opt -> DXR / GPL | yes | yes | yes | yes |
| `VK_EXT_pipeline_library_group_handles` | opt -> DXR | yes | yes | yes | yes |
| `VK_EXT_opacity_micromap` | opt -> DXR 1.2 (Tier 1_2) | NO | NO | NO | yes |
| `VK_EXT_mesh_shader` | opt -> MeshShaderTier (FL 12.2) | yes | yes | yes | yes |
| `VK_KHR_fragment_shading_rate` | opt -> VariableShadingRateTier | yes | yes | yes | yes |
| `VK_EXT_shader_image_atomic_int64` | opt -> SM 6.6 image atomics | yes | yes | yes | yes |
| `VK_KHR_compute_shader_derivatives` | opt -> SM 6.6 | yes | yes | yes | yes |
| `VK_KHR_shader_maximal_reconvergence` | opt -> SM 6.7 | yes | yes | yes | yes |
| `VK_KHR_shader_quad_control` | opt -> SM 6.7 | yes | yes | yes | yes |
| `VK_KHR_maintenance8` | opt -> SM 6.7 AdvancedTextureOps | NO | NO | NO | yes |
| `VK_KHR_maintenance9` | opt (perf) | NO | NO | NO | yes |
| `VK_KHR_maintenance10` | opt (perf) | NO | NO | NO | yes |
| `VK_KHR_shader_float_controls2` | opt (perf) | yes | yes | yes | yes |
| `VK_EXT_graphics_pipeline_library` | opt (PSO perf) | yes | yes | yes | yes |
| `VK_EXT_extended_dynamic_state2` | opt (perf) | yes | yes | yes | yes |
| `VK_EXT_extended_dynamic_state3` | opt (perf) | yes | yes | yes | yes |
| `VK_EXT_shader_module_identifier` | opt (PSO cache perf) | NO | NO | NO | yes |
| `VK_KHR_present_id` | opt (frame pacing) | NO | NO | NO | yes |
| `VK_KHR_present_wait` | opt (frame pacing / latency) | NO | NO | NO | yes |
| `VK_EXT_swapchain_maintenance1` | opt (d3d12core optional) | yes | NO | NO | yes |
| `VK_EXT_memory_budget` | opt (QueryVideoMemoryInfo) | NO | yes | yes | yes |
| `VK_EXT_memory_priority` | opt (residency) | NO | NO | NO | yes |
| `VK_EXT_pageable_device_local_memory` | opt -> ResourceHeapTier 2 path | NO | NO | NO | yes |
| `VK_EXT_device_generated_commands` | opt -> ExecuteIndirect / work graphs | NO | NO | NO | yes |
| `VK_EXT_external_memory_host` | opt -> OpenExistingHeapFromAddress | NO | NO | NO | yes |
| `VK_KHR_external_memory_win32` | used UNGUARDED for HEAP_FLAG_SHARED | NO | NO | NO | NO |
| `VK_KHR_external_semaphore_win32` | opt -> shared ID3D12Fence | yes | NO | NO | NO |
| `VK_EXT_shader_stencil_export` | opt -> PSSpecifiedStencilRef | NO | yes | yes | NO |
| `VK_EXT_conditional_rendering` | opt -> predication | yes | yes | yes | yes |
| `VK_EXT_image_sliced_view_of_3d` | opt | yes | yes | yes | yes |
| `VK_AMD_buffer_marker` | opt (breadcrumbs) | NO | NO | NO | yes |
| `VK_EXT_device_fault` | opt (breadcrumbs) | NO | NO | NO | yes |
| `VK_NV_device_diagnostic_checkpoints` | opt (breadcrumbs) | NO | NO | NO | yes |
| `VK_EXT_line_rasterization` | opt | yes | yes | yes | yes |
| `VK_EXT_scalar_block_layout` | opt (SM 6.0 cbuffer) | yes | yes | yes | yes |
| `VK_EXT_index_type_uint8` | opt | yes | yes | yes | yes |
| `VK_KHR_unified_image_layouts` | opt (perf) | NO | NO | NO | yes |
| `VK_EXT_zero_initialize_device_memory` | opt (perf) | NO | NO | NO | yes |


### 3.1 ⚠ The one correctness landmine: `VK_KHR_external_memory_win32`

The guest exposes `VK_KHR_external_semaphore_win32` but **not** `VK_KHR_external_memory_win32`
`[LIVE]`. The Helios Mesa fork implements only the semaphore half — `vn_physical_device.c:1271-1277`:

```c
#if DETECT_OS_WINDOWS
      if (physical_dev->external_binary_semaphore_handles &
          (VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT |
           VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_KMT_BIT)) {
         exts->KHR_external_semaphore_win32 = true;
      }
#endif
```

and the memory half is deliberately routed through the **fd**-named extensions describing the wire
handle type (`:1300-1324`, long Helios comment: "the fd-based external-memory extensions describe the
WIRE (renderer-side) handle types; no POSIX fd ever crosses into the guest").
There is also a standing note at `vn_physical_device.c:1050` that "the renderer runs on Windows,
`VK_KHR_external_memory_win32` might be…" (truncated context).

**vkd3d does not check for it.** `grep -rn "KHR_external_memory_win32" libs/vkd3d/` returns exactly
three hits — the table row `device.c:100`, the proc declaration `vulkan_procs.h:257-258`, and the
bool field `vkd3d_private.h:138`. **The bool is written and never read.** On `_WIN32`,
`libs/vkd3d/resource.c:4405-4429` unconditionally chains
`VkExportMemoryAllocateInfo{ handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT }`
for any `D3D12_HEAP_FLAG_SHARED` allocation, then `:4468-4469` calls
`d3d12_resource_open_export_kmt()`, which at `libs/vkd3d/d3dkmt.c:113-119` does
`VK_CALL(vkGetMemoryWin32HandleKHR(...))` — a function pointer that is NULL when the extension was
never enabled. `d3d12_device_CreateSharedHandle` (`device.c:7645-7651`) does the same.

`[INFER]` Expected failure: either `vkCreateDevice`/`vkAllocateMemory` rejects the unknown-handle-type
pNext, or a NULL-pointer call in `d3dkmt.c:118`. Either way `D3D12_HEAP_FLAG_SHARED` — which is what
D3D11On12, DXGI interop and cross-process sharing use — is not merely degraded but hazardous.
**UNVERIFIED** in the sense that nobody has run it. Settling experiment: a minimal D3D12 program that
does `CreateCommittedResource` with `D3D12_HEAP_FLAG_SHARED` under vkd3d-proton on the guest, run
from a session-1 scheduled task; watch for the crash and for `VKD3D_DEBUG=warn` output.

### 3.2 32-bit / WOW64 `[LIVE]`

```
HKLM:\SOFTWARE\Khronos\Vulkan\Drivers          -> C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json
HKLM:\SOFTWARE\WOW6432Node\Khronos\Vulkan\Drivers -> NO WOW6432Node Khronos\Vulkan\Drivers KEY
```
ICD manifest contents: `"library_path": "C:/ProgramData/HeliosVulkan/vulkan_virtio-b4408c6de1c2.dll"`,
`"library_arch": "64"`, `"api_version": "1.4.352"`.

vkd3d-proton's `package-release.sh` builds both `build.64` and `build.86` (README "Building"
section). A 32-bit `d3d12.dll` on this guest would call `vkEnumeratePhysicalDevices` and get zero
devices. Either ship a 32-bit venus ICD or decide 64-bit-only and say so.

---

## 4. Gap analysis — what vkd3d wants that Helios does not have

Nothing in the **required** band is missing. Every gap below is optional, and each is attributed to
the layer that must change.

| Extension | Attributed to | Why that layer | What D3D12 loses | Work estimate |
|---|---|---|---|---|
| `VK_KHR_external_memory_win32` | **Mesa venus ICD** (new *native* extension over the existing blob/res-id export) | not in either protocol name table; the semaphore twin is already native | `HEAP_FLAG_SHARED`, `CreateSharedHandle`, D3D11On12 — and today it **crashes** rather than degrades | medium; the export machinery exists (`vn_wsi_get_helios_resource_identity`, blob res_id) |
| `VK_EXT_descriptor_heap` | **virglrenderer only** | guest protocol table **has** it (`vn_protocol_driver_info.h:43`, `{"VK_EXT_descriptor_heap", 136, 1}`), Mesa venus enables it (`vn_physical_device.c:1541`), the host GPU has it; only `vkr_extension_table` lacks it | vkd3d's newest optimal descriptor model (`VKD3D_CONFIG=descriptor_heap`) | **small** — resync virglrenderer's `venus-protocol/` + add one table row |
| `VK_EXT_descriptor_buffer` | venus-protocol **+** Mesa **+** virglrenderer | absent from *both* protocol name tables (187 guest / 185 host) | second-best descriptor model; falls back to descriptor sets | large (new protocol commands) |
| `VK_KHR_present_id`, `VK_KHR_present_wait` | Mesa venus (`#ifndef VK_USE_PLATFORM_WIN32_KHR`, `:1334-1341`) **+** virglrenderer | disabled on Windows by preprocessor, and absent from both protocol tables | frame pacing / latency control; DXGI waitable-object fidelity | medium |
| `VK_KHR_maintenance8` | venus-protocol + Mesa + virglrenderer | absent from both name tables | `D3D12_FEATURE_DATA_D3D12_OPTIONS14.AdvancedTextureOpsSupported = false` (`device.c:10418-10419`) + two behavioural fallbacks (`resource.c:690`, `command.c:9887`). **Not** an SM 6.7 blocker — see §7.1 | medium |
| `VK_EXT_opacity_micromap` | venus-protocol + Mesa + virglrenderer | absent from both | **DXR 1.2 → capped at 1.1** (`device.c:9974-9978`) | large |
| `VK_EXT_device_generated_commands` | venus-protocol + Mesa + virglrenderer | absent from both | ExecuteIndirect with state changes; work-graph mesh nodes | large |
| `VK_EXT_pageable_device_local_memory`, `VK_EXT_memory_priority` | venus-protocol + Mesa + virglrenderer | absent from both | residency quality; one of two routes to `D3D12_RESOURCE_HEAP_TIER_2` (`device.c:10000-10007`) | medium |
| `VK_EXT_memory_budget` | **guest env only** | vkr enables it; Mesa gates on `VN_DEBUG(MEM_BUDGET)` (`vn_physical_device.c:1553`) | `QueryVideoMemoryInfo` budget accuracy (`memory.c:819-825`) | **trivial: set `VN_DEBUG=mem_budget`** |
| `VK_EXT_shader_module_identifier`, `VK_AMD_buffer_marker`, `VK_EXT_device_fault`, `VK_NV_device_diagnostic_checkpoints`, `VK_KHR_maintenance9/10`, `VK_KHR_unified_image_layouts`, `VK_EXT_zero_initialize_device_memory` | venus-protocol + Mesa + virglrenderer | absent from both | PSO-cache perf; breadcrumb/fault debugging; misc perf | medium each |
| `VK_EXT_external_memory_host` | venus-protocol + Mesa + virglrenderer | absent from both | `OpenExistingHeapFromAddress` (`heap.c:287-291`, `device.c:5914`) | large (host-pointer import over venus is architecturally awkward) |
| `VK_EXT_shader_stencil_export` | **host GPU** — NVIDIA does not expose it | see §2.4 | `PSSpecifiedStencilRefSupported=false`; vkd3d has an explicit fallback (`meta.c:863`, `:905`, `:1200`, `:1332`) | none available |
| 32-bit ICD | packaging / Mesa build | §3.2 | 32-bit D3D12 apps get no adapter | small–medium |

---

## 5. Sparse / reserved resources over venus — **supported, end to end**

D3D12 reserved (tiled) resources need Vulkan sparse binding. The answer is yes at every layer:

1. **Guest features `[LIVE]`** — `VkPhysicalDeviceFeatures`: `sparseBinding`, `sparseResidencyBuffer`,
   `sparseResidencyImage2D`, `sparseResidencyImage3D`, `sparseResidency{2,4,8,16}Samples`,
   `sparseResidencyAliased`, `shaderResourceResidency` — **all true**.
   `sparseProperties`: `residencyStandard2DBlockShape=true`, `residencyStandard3DBlockShape=true`,
   `residencyAlignedMipSize=false`, `residencyNonResidentStrict=true`.
   `limits.sparseAddressSpaceSize = 1 099 511 627 776` (1 TiB). Every value identical to the host's.
2. **Guest queue families `[LIVE]`** — all six carry `VK_QUEUE_SPARSE_BINDING_BIT`; family 1 is a
   dedicated non-graphics sparse queue, exactly what
   `vkd3d_select_queues` (`libs/vkd3d/device.c:3811-3826`) prefers.
3. **Guest ICD implementation `[MESA]`** — `vn_QueueBindSparse()` at
   `icd/mesa/src/virtio/vulkan/vn_queue.c:2445-2500`, with batching helpers
   `vn_queue_bind_sparse_submit()` (`:2271-2295`) and `vn_queue_bind_sparse_submit_batch()`
   (`:2298-2440`), including the semaphore-feedback interlock at `:2340` ("so that the vkQueueSubmit
   waits on the vkQueueBindSparse signal").
4. **Protocol `[MESA]`** — `icd/mesa/src/virtio/venus-protocol/vn_protocol_driver_queue.h:1117-1310`
   encodes `VK_COMMAND_TYPE_vkQueueBindSparse_EXT` (sizeof/encode/decode-reply/submit).
5. **Host `[VIRGL]`** — `vkr_dispatch_vkQueueBindSparse()` at `vkr_queue.c:385-397`, registered at
   `:655`.

**Consequence for D3D12:** applying vkd3d's tier rule (`libs/vkd3d/device.c:9845-9868`) to the live
values, Helios clears every clause — `sparseBinding`, `sparseResidencyAliased`,
`sparseResidencyBuffer`, `sparseResidencyImage2D`, `residencyStandard2DBlockShape`, a sparse queue
family; then `shaderResourceResidency`, `shaderResourceMinLod`, `!residencyAlignedMipSize`,
`residencyNonResidentStrict`, `filterMinmaxSingleComponentFormats`; then `sparseResidencyImage3D` and
`residencyStandard3DBlockShape` — landing on **`D3D12_TILED_RESOURCES_TIER_4`**. `[INFER]` from
`[LIVE]` values + `[VKD3D]` code; **UNVERIFIED** until a real device reports it.

Nothing is lost, and device creation would succeed even if sparse were absent — the tier function
returns `TIER_NOT_SUPPORTED`, which only costs FL 12.0 (`device.c:10562-10567` needs
`TiledResourcesTier >= 2`).

> ⚠ Note for whoever runs this: sparse binding is the **one** Vulkan path that submits work on a queue
> family Helios' D3D11 stack has never exercised (family 1, TRANSFER|SPARSE). Whether the KMD/venus
> ring plumbing handles a second queue family's timeline is outside this lane — flag to R5/R6.

---

## 6. Raytracing — **DXR 1.1 is reachable today; DXR 1.2 is not**

Guest exposes `[LIVE]`: `VK_KHR_acceleration_structure` (rev 13), `VK_KHR_ray_tracing_pipeline`,
`VK_KHR_ray_query`, `VK_KHR_ray_tracing_maintenance1`, `VK_KHR_ray_tracing_position_fetch`,
`VK_KHR_deferred_host_operations` (rev 4), `VK_KHR_pipeline_library`,
`VK_EXT_pipeline_library_group_handles`.

Features: `accelerationStructure=true`, `descriptorBindingAccelerationStructureUpdateAfterBind=true`,
`rayTracingPipeline=true`, `rayTracingPipelineTraceRaysIndirect=true`,
`rayTraversalPrimitiveCulling=true`, `rayQuery=true`, `rayTracingMaintenance1=true`,
`rayTracingPipelineTraceRaysIndirect2=true`, `pipelineLibraryGroupHandles=true`.
(`accelerationStructureIndirectBuild=false` and `accelerationStructureHostCommands=false` — both are
force-cleared by vkd3d anyway at `device.c:3380-3382`.)

Properties: `shaderGroupHandleSize=32`, `maxRayRecursionDepth=31`, `shaderGroupBaseAlignment=64`,
`shaderGroupHandleAlignment=32`, `maxRayHitAttributeSize=32`.

Checked against `d3d12_device_determine_ray_tracing_tier()` (`libs/vkd3d/device.c:9906-9979`):
- `shaderGroupHandleSize == D3D12_SHADER_IDENTIFIER_SIZE_IN_BYTES` (32) ✅
- `maxRayHitAttributeSize >= D3D12_RAYTRACING_MAX_ATTRIBUTE_SIZE_IN_BYTES` (32) ✅
- `shaderGroupBaseAlignment <= D3D12_RAYTRACING_SHADER_TABLE_BYTE_ALIGNMENT` (64) ✅
- `shaderGroupHandleAlignment <= D3D12_RAYTRACING_SHADER_RECORD_BYTE_ALIGNMENT` (32) ✅
- Tier 1.0 RTAS vertex formats — all six (`R32G32_SFLOAT`, `R32G32B32_SFLOAT`, `R16G16_SFLOAT`,
  `R16G16_SNORM`, `R16G16B16A16_SFLOAT`, `R16G16B16A16_SNORM`) carry
  `VK_FORMAT_FEATURE_ACCELERATION_STRUCTURE_VERTEX_BUFFER_BIT_KHR` in the guest's `bufferFeatures` ✅
- Tier 1.1 extra formats — all seven (`R16G16B16A16_UNORM`, `R16G16_UNORM`,
  `A2B10G10R10_UNORM_PACK32`, `R8G8B8A8_UNORM`, `R8G8_UNORM`, `R8G8B8A8_SNORM`, `R8G8_SNORM`) ✅
- Tier 1.2 needs `info->supports_opacity_micromap` ← `opacity_micromap_features.micromap`, and
  `VK_EXT_opacity_micromap` is not on the guest ❌

⇒ `D3D12_RAYTRACING_TIER_1_1`. `[INFER]` from `[LIVE]` + `[VKD3D]`.

**What vkd3d does when RT is absent:** it does **not** refuse. `RaytracingTier` stays
`D3D12_RAYTRACING_TIER_NOT_SUPPORTED` and the device is created normally; only FL 12.2 is lost
(`device.c:10579`). `VKD3D_CONFIG=no_dxr` produces the same result by disabling the extensions in the
table (`device.c:70-75`). Venus has the mirror knob: `VN_DEBUG=no_ray_tracing` clears
`physical_dev->ray_tracing` (`vn_physical_device.c:1600`), which gates
`KHR_acceleration_structure`/`ray_query`/`ray_tracing_pipeline`/`ray_tracing_maintenance1`/
`pipeline_library_group_handles` in the passthrough table (`:1489-1499`, `:1560`) — a clean A/B.

---

## 7. ⚠ The `driverID` question — the single subtlest finding in this lane

**The problem.** vkd3d gates Shader Model 6.2 (and therefore 6.3/6.5/6.6/6.7, which chain off it) on
FP32 denorm control — `libs/vkd3d/device.c:10693-10704`:

```c
        denorm_behavior = device->device_info.vulkan_1_2_properties.denormBehaviorIndependence !=
                VK_SHADER_FLOAT_CONTROLS_INDEPENDENCE_NONE;
        if (denorm_behavior)
        {
            if (device->device_info.vulkan_1_2_properties.driverID != VK_DRIVER_ID_NVIDIA_PROPRIETARY)
            {
                denorm_behavior = device->device_info.vulkan_1_2_properties.shaderDenormFlushToZeroFloat32 &&
                        device->device_info.vulkan_1_2_properties.shaderDenormPreserveFloat32;
            }
        }
```

The comment at `:10693-10695` explains the NVIDIA exception: "shaderDenorm handling appears to work
just fine on NV, despite the properties struct saying otherwise. Assume that this is just a driver
oversight, since otherwise we cannot expose SM 6.2 there…"

Live guest values `[LIVE]`: `shaderDenormFlushToZeroFloat32 = false`,
`shaderDenormPreserveFloat32 = false`, `driverID = VK_DRIVER_ID_MESA_VENUS`.
Host values are identical except `driverID = VK_DRIVER_ID_NVIDIA_PROPRIETARY`.

**Naively that caps Helios at `D3D_SHADER_MODEL_6_0`**, and because FL 12.2 needs
`max_shader_model >= D3D_SHADER_MODEL_6_5` (`device.c:10572`), the feature level would cap at 12.1.

**The escape hatch, and it looks like it fires.** vkd3d already handles layered implementations via
`VK_KHR_maintenance7`. `libs/vkd3d/device.c:2323-2343` chains
`VkPhysicalDeviceLayeredApiPropertiesListKHR` → `VkPhysicalDeviceLayeredApiVulkanPropertiesKHR` →
`VkPhysicalDeviceDriverProperties real_driver_props`, and `:2657-2664`:

```c
    /* if nonzero, this is a layered implementation */
    if (real_driver_props.driverID)
    {
        /* store the layer ID here in case it's needed */
        info->layer_driver_id = info->vulkan_1_2_properties.driverID;
        /* swizzle the underlying driver ID here so everything else will use it */
        info->vulkan_1_2_properties.driverID = real_driver_props.driverID;
    }
```

The guest **has** `VK_KHR_maintenance7` `[LIVE]`, and the Helios Mesa fork fills the layered-API
struct **before** it rewrites `driverID` to `MESA_VENUS`:
`vn_physical_device.c:851-883` (`layer->driver.driverID = props->driverID;` at **:870**) runs inside
`vn_physical_device_init_properties()`, whereas
`vn_physical_device_sanitize_properties()` — which does `props->driverID = VK_DRIVER_ID_MESA_VENUS;`
at **:571** — is only called at **:905**, i.e. **after**. So the layered chain should report the
*real* `VK_DRIVER_ID_NVIDIA_PROPRIETARY`.

Live corroboration `[LIVE]`, `vulkaninfo-full.txt:524-532`:

```
VkPhysicalDeviceLayeredApiPropertiesListKHR:
	layeredApiCount = 1
	pLayeredApis: count = 1
		0:
			vendorID   = 0x10de
			deviceID   = 0x2bb1
			layeredAPI = PHYSICAL_DEVICE_LAYERED_API_VULKAN_KHR
			deviceName = NVIDIA RTX PRO 6000 Blackwell Workstation Edition
```

vulkaninfo does not chain the nested `VkPhysicalDeviceDriverProperties`, so the final link —
`real_driver_props.driverID == VK_DRIVER_ID_NVIDIA_PROPRIETARY` — is **proven by source ordering, not
yet observed**. ⇒ **UNVERIFIED.**

**Settling experiment (cheap, read-only, no build of vkd3d needed):** write a ~40-line Vulkan probe
under `tools/` that chains `VkPhysicalDeviceLayeredApiPropertiesListKHR` →
`VkPhysicalDeviceLayeredApiVulkanPropertiesKHR` → `VkPhysicalDeviceDriverProperties` on the guest and
prints `driverID`. Alternatively, once vkd3d runs at all: `VKD3D_DEBUG=info` prints
`"Enabling support for SM 6.6."` (`device.c:10766`, an `INFO`) — its presence or absence answers the
question in one line.

**If it turns out NOT to fire**, the workaround is one env var: `VKD3D_SHADER_MODEL=6_6`
(`d3d12_device_caps_shader_model_override()`, `device.c:10600-10638`, env read at `:10617`) — but
that is an override, not a fix; the real fix would be a vkd3d patch adding
`VK_DRIVER_ID_MESA_VENUS` to the exception at `:10699`, or the Mesa fork forwarding the host's real
float-controls semantics.

**Other `driverID`-conditional vkd3d paths** that now see NVIDIA-or-VENUS and will behave
differently depending on which: `device.c:1883`, `:1912`, `:1921`, `:1937` (memory model),
`:3224`, `:3961-3986` (the switch where `VK_DRIVER_ID_MESA_VENUS` is explicitly grouped with
MoltenVK/Dozen under "layered implementations are handled transparently"), `:4144`
(`vkd3d_driver_has_fast_concurrent_transfer_queue`), `:10163`, `:10470-10472`, `:11097`, `:11191`,
`:11417`; `command.c:180`, `:417`, `:11636-11638`; `resource.c:426-437`, `:647`, `:5265`;
`memory.c:2013`; plus `state.c:3143`, `raytracing_pipeline.c:1934`, `workgraphs.c:2194` which feed
`compile_args.driver_id` into **dxil-spirv**. That last one matters: dxil-spirv changes its SPIR-V
output per driver. Which `driver_id` it receives is the same open question.

---

### 7.1 The full shader-model chain, walked against the live values

`d3d12_device_caps_init_shader_model()` (`libs/vkd3d/device.c:10640-10805`) is a strict ladder — each
step is gated on `max_shader_model == <previous>`, so a single failure freezes the whole chain.
Walked against the guest's live capabilities:

| Step | Gate (line) | Live guest value | Result |
|---|---|---|---|
| **6.0** | `subgroupSize >= 4`; subgroup ops ⊇ ARITHMETIC\|BASIC\|BALLOT\|SHUFFLE\|QUAD\|VOTE; stages ⊇ COMPUTE\|FRAGMENT; `scalarBlockLayout \|\| uniformBufferStandardLayout`; `shaderInt16` (`:10665-10670`) | 32; all 11 ops; all 14 stages; both true; true | ✅ |
| **6.2** | `denormBehaviorIndependence != NONE` **and** (`driverID == NVIDIA_PROPRIETARY` **or** (`shaderDenormFlushToZeroFloat32 && shaderDenormPreserveFloat32`)) (`:10693-10704`) | `INDEPENDENCE_ALL`; both denorm bits **false**; `driverID` = **the open question in §7** | ⚠ **hinges entirely on §7** |
| **6.3** | unconditional once 6.2 (`:10716-10721`) | — | follows 6.2 |
| **6.5** | unconditional once 6.3 (`:10739-10745`) | — | follows 6.2 |
| **6.6** | `computeDerivativeGroupLinear \|\| driverID == NVIDIA`; `shaderBufferInt64Atomics`; `shaderInt8`; required-subgroup-size for COMPUTE (`:10761-10767`) | `computeDerivativeGroupLinear = true`; both true; `requiredSubgroupSizeStages` includes COMPUTE | ✅ *if* 6.2 passed |
| **6.7** | `shaderMaximalReconvergence && shaderQuadControl` (or `VKD3D_CONFIG=experimental`) (`:10794-10797`) | both **true** (`VK_KHR_shader_maximal_reconvergence`, `VK_KHR_shader_quad_control` both on the guest) | ✅ *if* 6.6 passed |

**Correction to a natural assumption:** `VK_KHR_maintenance8` does **not** gate SM 6.7. The code
requires only maximal-reconvergence + quad-control. `maintenance8` appears solely at
`device.c:10418-10419` (`options14->AdvancedTextureOpsSupported = max_shader_model >= 6_7 &&
(maintenance8 || experimental)`), `resource.c:690` and `command.c:9887`. The profile's
`shader_model_67` capability set lists it, but the profile is aspirational (§1.3 note).
`options14->WriteableMSAATexturesSupported` also needs `shaderStorageImageMultisample`, which the
guest reports **true**.

**So the whole shader-model story reduces to one bit: which `driverID` vkd3d ends up using.**
If the maintenance7 swizzle fires → **SM 6.7** and, combined with §5 (Tiled Tier 4), §6 (DXR 1.1),
conservative-raster Tier 3, mesh shaders and VRS, the substrate reaches **`D3D_FEATURE_LEVEL_12_2`**
except for `SamplerFeedbackTier` (`device.c:10583`), which vkd3d itself flags as TODO in the profile
labels ("TODO: missing sampler feedback"). If it does not fire → **SM 6.0** and the feature level
caps at **12.1**.

---

## 8. Shaders / SPIR-V — venus is transparent

`vn_CreateShaderModule()` (`icd/mesa/src/virtio/vulkan/vn_pipeline.c:282-302`) forwards the whole
`VkShaderModuleCreateInfo` — including `pCode` — with `vn_async_vkCreateShaderModule(...)`. There is
no SPIR-V validation, rewriting or capability filtering in venus.

**⇒ SPIR-V capability support is the host NVIDIA driver's, verbatim.** Anything dxil-spirv emits that
NVIDIA 610.43 accepts will work; venus adds no SPIR-V gap. The only SPIR-V-adjacent things venus can
break are the *feature/property* bits that authorise a capability, and §2.1 shows those are
bit-identical passthrough. Guest exposes `VK_KHR_spirv_1_4`, `VK_KHR_shader_float_controls` (rev 4),
`VK_KHR_shader_float_controls2`, `VK_KHR_shader_integer_dot_product`,
`VK_KHR_shader_subgroup_extended_types`, `VK_KHR_shader_subgroup_rotate`,
`VK_KHR_shader_maximal_reconvergence`, `VK_KHR_shader_quad_control`, `VK_KHR_shader_clock`,
`VK_KHR_shader_expect_assume`, `VK_KHR_shader_bfloat16`, `VK_EXT_shader_float8`,
`VK_KHR_workgroup_memory_explicit_layout`, `VK_KHR_cooperative_matrix` (rev 2),
`VK_EXT_shader_image_atomic_int64`, `VK_KHR_compute_shader_derivatives` `[LIVE]`.
Subgroup: size 32, ops = BASIC|VOTE|ARITHMETIC|BALLOT|SHUFFLE|SHUFFLE_RELATIVE|CLUSTERED|QUAD|
ROTATE|ROTATE_CLUSTERED|PARTITIONED, supported in all 14 stages — comfortably above the profile's
`subgroups_66`. (Deeper DXIL work is lane R8.)

---

## 9. Profile conformance — the actual measurement

Method: parse `tmp/dx12/research/capture/vulkaninfo.json` against
`vkd3d-proton-helios/VP_D3D12_VKD3D_PROTON_profile.json`, resolving `EXT`↔`KHR` aliases and
promotion into `VkPhysicalDeviceVulkan1{1,2,3,4}Features`. Result:

| Profile capability set | feature misses | extension misses |
|---|---|---|
| `baseline_features` | **0** | **0** |
| `fl_11_1_features` | **0** | **0** |
| `fl_12_0_features` | **0** | **0** |
| `fl_12_1_features` | **0** | **0** |
| `fl_12_1_features_rov` | **0** | **0** |
| `fl_12_2_features` | **0** | **0** |
| `shader_model_60` | **0** | **0** |
| `shader_model_66` | **0** | **0** |
| `shader_model_67` | 1 (`maintenance8`) | 1 (`VK_KHR_maintenance8`) — *profile only; the code does not require it, see §7.1* |
| `optimal_performance` | 7 | 7 |

`optimal_performance` misses: `descriptorBuffer`, `descriptorBufferPushDescriptors`,
`shaderModuleIdentifier`, `presentId`, `presentWait`, `maintenance9`, `maintenance10`; extensions
`VK_EXT_descriptor_buffer`, `VK_EXT_shader_module_identifier`, `VK_KHR_present_id`,
`VK_KHR_present_wait`, `VK_AMD_buffer_marker`, `VK_KHR_maintenance9`, `VK_KHR_maintenance10`.

Properties spot-checked separately (all pass): UpdateAfterBind limits ≥ 1 000 000;
`robustBufferAccessUpdateAfterBind`; `filterMinmaxSingleComponentFormats`;
`bufferImageGranularity = 1024` ≤ 65536; sparse residency shapes; texel-buffer single-texel
alignment; `transformFeedbackQueries`; `graphicsPipelineLibraryIndependentInterpolationDecoration`
(required by `device.c:3271-3276`, else GPL is silently disabled — no error); conservative-raster
`degenerateTrianglesRasterized` + `fullyCoveredFragmentShaderInputVariable`;
`fragmentShadingRateNonTrivialCombinerOps`; `maxVertexAttribDivisor = 0xFFFFFFFF`;
`VK_EXT_vertex_attribute_divisor` spec version **3** (the profile's minimum).

**Statement of record: the live Helios guest satisfies `VP_D3D12_FL_12_2_baseline` in full.**
The only D3D12 ceilings the substrate imposes are DXR 1.2 (no opacity micromap),
`OPTIONS14.AdvancedTextureOpsSupported` (no maintenance8), and — pending §7 — possibly SM 6.2+.

---

## 10. Ground-truth capture commands (read-only)

These are exactly what I ran. All are reads; none builds, installs or reboots anything.

**A. Is the VM up?** (Linux host)
```bash
pgrep -af qemu-system-x86_64
```
This session: pid 4447, `HELIOS_QEMU_BIN=…/qemu-helios/build-helios/qemu-system-x86_64`,
`-device {"driver":"virtio-gpu-gl-pci",…,"venus":true,"blob":true,…}`, `-display sdl,gl=on`.
If it prints nothing, the VM is down and every `[LIVE]` number below must be re-taken —
**relaunching the VM is owner-gated** (CLAUDE.md).

**B. Guest capability set** — via the `win` MCP `win_exec` (no window needed; `vulkaninfo` is
session-0 safe):
```powershell
New-Item -ItemType Directory -Force -Path Z:\tmp\dx12\research\capture | Out-Null
& vulkaninfo --summary 2>&1 | Out-File -Encoding utf8 Z:\tmp\dx12\research\capture\vulkaninfo-summary.txt
& vulkaninfo --json=0 -o Z:\tmp\dx12\research\capture\vulkaninfo.json
& vulkaninfo 2>$null | Out-File -Encoding utf8 Z:\tmp\dx12\research\capture\vulkaninfo-full.txt
```
`--json=0` emits the Vulkan Profiles document for physical device 0 — that is the form that can be
diffed against `VP_D3D12_VKD3D_PROTON_profile.json` mechanically. Files land on `Z:\` = the repo, so
they are readable from Linux immediately.

**C. ICD registration / bitness**
```powershell
(Get-Item 'HKLM:\SOFTWARE\Khronos\Vulkan\Drivers').GetValueNames()
$k = Get-Item 'HKLM:\SOFTWARE\WOW6432Node\Khronos\Vulkan\Drivers' -ErrorAction SilentlyContinue
if ($k) { $k.GetValueNames() } else { "NO WOW6432Node key" }
Get-Content C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json
```

**D. Guest ICD diagnostics** (env-var knobs, read-only in effect)
```powershell
$env:VN_DEBUG = "init,result,wsi"        # venus init/result/WSI tracing
$env:VN_DEBUG = "mem_budget"             # turns ON VK_EXT_memory_budget
$env:VN_DEBUG = "no_sparse"              # A/B: remove sparse
$env:VN_DEBUG = "no_ray_tracing"         # A/B: remove DXR
```
Names from `icd/mesa/src/virtio/vulkan/vn_common.c:23-38`, parsed at `:70`
(`parse_debug_string(os_get_option("VN_DEBUG"), vn_debug_options)`).
⚠ `win_exec` lands in **session 0** — anything that needs a window must go through a cloned
scheduled task (CLAUDE.md / memory 60th).

**E. Host renderer identity**
```bash
pacman -Qi virglrenderer | head -3
grep virglrenderer /proc/$(pgrep -f 'qemu-system-x86_64 -L' | head -1)/maps | head -1
ldd qemu-helios/build-helios/hw-display-virtio-gpu-gl.so | grep virgl
strings -a /usr/lib/libvirglrenderer.so.1 | grep '^VK_' | sort -u    # 185 names = the protocol table
```

**F. Host GPU**
```bash
vulkaninfo > /tmp/host_vulkaninfo.txt      # GPU0 = the NVIDIA block
```
Archived reference: `docs/reference/host-vulkan-profile-rtx-pro-6000-blackwell.json`.

**G. The one command that would end the argument** — build the pinned submodule and run its test
suite against the guest ICD (DX12.md §2 question 1, D0 gate). **Not run here**: builds are out of
scope for this lane.

---

## 11. Hand-offs

- **R3 (vkd3d internals):** the `driverID` swizzle at `device.c:2657-2664` and the `MESA_VENUS` case
  at `:3980` mean vkd3d already has a layered-implementation concept. Whether it is complete for
  venus is §7.
- **R5/R6 (KMD/D3DKMT):** vkd3d-proton on Windows **does** touch D3DKMT — `libs/vkd3d/d3dkmt.c`
  calls `D3DKMTCreateDevice`, `D3DKMTOpenResourceFromNtHandle`, `D3DKMTShareObjects`,
  `D3DKMTCreateSynchronizationObject`. Strategy (b) is therefore *not* KMD-free.
  Also: sparse binding will submit on a second Vulkan queue family the D3D11 stack has never used.
- **R7 (present):** the Helios Mesa fork **forces software WSI on Windows** —
  `icd/mesa/src/virtio/vulkan/vn_wsi.c:168-186`, `use_sw_device = true` unconditionally under
  `#ifdef _WIN32`, with a comment saying the hardware DXGI/dcomp path fails
  (`dxgi_get_factory() fails on this guest -> VK_ERROR_INITIALIZATION_FAILED -> vkEnumeratePhysicalDevices returns 0 devices`).
  So a vkd3d D3D12 swapchain presents through `wsi_common_win32`'s GDI/DIB CPU blit. Surface formats
  are BGRA8/RGBA8/BGRA8_SRGB only — no 10-bit, no HDR.
- **R8 (shaders):** §8 — venus is transparent for SPIR-V; the interesting variable is which
  `driver_id` reaches dxil-spirv (`state.c:3143`, `raytracing_pipeline.c:1934`, `workgraphs.c:2194`).
- **R11 (packaging):** §3.2, the missing 32-bit ICD.

---

## 12. Artifacts produced by this lane

| Path | What |
|---|---|
| `tmp/dx12/research/R12-vulkan-gap.md` | this dossier |
| `tmp/dx12/research/capture/vulkaninfo.json` | guest Vulkan Profiles capture, 206 KB, this boot |
| `tmp/dx12/research/capture/vulkaninfo-full.txt` | guest full `vulkaninfo`, 77 KB, this boot |
| `tmp/dx12/research/capture/vulkaninfo-summary.txt` | guest `--summary` |

(Reference material read but not copied in: `/tmp/host_vulkaninfo.txt` — fresh host capture;
`/tmp/virglrenderer-virglrenderer-1.3.0/` — the 1.3.0 source tarball, fetched from
`gitlab.freedesktop.org` because the installed `.so` is stripped. Both are regenerable with the
commands in §10.)
