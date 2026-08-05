# R3 — vkd3d-proton internals and separability

**Lane:** R3. **Subject:** `vkd3d-proton-helios/` @ `2c7ba22c53261458a7a204c55f3098ad9855cb15`
(`git describe --tags` → `v3.0.1-254-g2c7ba22c`; meson `project(version : '3.1.0')`,
`meson.build:2`).
**Method:** read the source. Every claim below carries a `file:line` or the exact command run.
Unresolved items are marked **UNVERIFIED** with the read/experiment that settles them.

Citation convention used throughout:
- *"the code does X"* = I read the named lines.
- *"the README/docs say X"* = vkd3d-proton's own prose, which I flag separately when the code
  disagrees (it does, in §9).
- *"I infer"* = explicitly labelled.

---

## 0. TL;DR for the implementer

1. **The fork is not a fork.** `vkd3d-proton-helios` is byte-identical to upstream
   `HansKristian-Work/vkd3d-proton` master. Zero local commits, clean tree. DX12.md §1.3 is
   correct. **New:** all three of its own submodules (`dxil-spirv`, `Vulkan-Headers`,
   `SPIRV-Headers`) are **uninitialised empty directories** — the tree cannot even be configured
   today (§10).
2. **A native MSVC x64 build is supported and CI-tested** (`.github/workflows/test-build-windows.yml`),
   contradicting the "mingw only" assumption. It needs `widl` (Strawberry Perl) and
   `glslangValidator` on PATH, plus meson + VS2022 (§8).
3. **vkd3d-proton does not implement DXGI and never creates a `VkSurfaceKHR` itself.** It exposes
   `IDXGIVkSwapChainFactory` off `ID3D12CommandQueue` and receives the surface from an
   `IDXGIVkSurfaceFactory` supplied by the caller. The only shipping implementation of that caller
   is **DXVK's `dxgi.dll`** — and `dxvk-helios` already has the matching interface with the same
   UUID. This is the single most consequential architectural fact for Helios (§7).
4. **The COM boundary is thin but the vtable is load-bearing.** The impl structs are plain C
   structs with a leading `ID3D12*_iface` and `CONTAINING_RECORD` casts; internal code almost never
   dispatches through vtables. But vkd3d *hot-swaps the device and command-list vtables* as its
   descriptor-specialisation mechanism (`d3d12_device_replace_vtable`, device.c:11302-11400), so
   "just delete COM" is not free (§2).
5. **GPU VAs are real Vulkan buffer device addresses, not fabricated.** `va_map.c` is a *reverse*
   map (VA → resource), not an allocator. `GetGPUVirtualAddress` returns
   `resource->res.va` = `vkGetBufferDeviceAddress` (§5).
6. **`d3dkmt.c` is a Wine-targeted opportunistic path**, keyed on a Wine-private escape
   (`D3DKMT_ESCAPE_UPDATE_RESOURCE_WINE = 0x80000000`). It degrades gracefully to the Vulkan
   external-memory + DXVK-shared-metadata path when D3DKMT fails (§6).
7. **Vulkan 1.3 is a hard floor** (`VKD3D_MIN_API_VERSION == VKD3D_MAX_API_VERSION == VK_API_VERSION_1_3`),
   plus 8 hard-fail feature/extension gates enumerated in §9 — including `maintenance5` **and**
   `maintenance6`, which the README does not list. Hand this list to R12.

---

## 1. Module map

### 1.1 LOC (exact; `wc -l` over `*.c *.h`)

```
$ find libs include -name '*.c' -o -name '*.h' | xargs wc -l | tail -1
 117940 total
```

| Module | LOC | What it is |
|---|---|---|
| `libs/vkd3d-common` | 1,458 | Platform shims: `debug.c` (481), `platform.c` (261), `profiling.c` (198), `file_utils.c` (188), `utf8.c` (143), `string.c` (137), `memory.c` (50). No D3D12, no Vulkan. |
| `libs/vkd3d-shader` | 5,891 | `dxil.c` (2474) = glue to the **external** dxil-spirv compiler; `dxbc.c` (1879) = DXBC *container* + signature + **root-signature** blob parsing only; `vkd3d_shader_main.c` (1001); `checksum.c` (99, MD5). |
| `libs/vkd3d` | 101,062 (`.c`+`.h`, incl. 100,793 in the flat file list) | The translation core. |
| `libs/d3d12core` | 1,537 | `main.c` (1355) + `debug.c` (149) + `debug.h` (33): the loadable `d3d12core.dll`. |
| `libs/d3d12` | 341 | `main.c` only: the thin `d3d12.dll` forwarder. |
| `include/` | 15,321 | Public headers + **17 `.idl` files** compiled by `widl`. |
| `tests/` | 151,964 across 40 `.c` files (98,616 in the top-level `tests/*.c` sum) | The D3D12 conformance suite. |
| `demos/` | 2,870 | `triangle.c`, `gears.c` + Win32/XCB shims. |
| `programs/` | 1,403 | `vkd3d-compiler`, `vkd3d-rs-parse`, `vkd3d-hlsl-build`, profiling scripts. |

Largest files in `libs/vkd3d` (`wc -l libs/vkd3d/*.c libs/vkd3d/*.h | sort -rn`):
`command.c` 26,534 · `device.c` 12,144 · `resource.c` 11,388 · `state.c` 8,444 ·
`vkd3d_private.h` 7,163 · `swapchain.c` 4,179 · `cache.c` 3,555 · `workgraphs.c` 3,403 ·
`raytracing_pipeline.c` 2,888 · `meta.c` 2,461 · `memory.c` 2,276 · `utils.c` 1,966 ·
`bundle.c` 1,929 · … · `va_map.c` 489 · `d3dkmt.c` 449 · `heap.c` 422.

### 1.2 Dependency direction

From the meson files (`libs/meson.build:1-5` orders the subdirs):

```
vkd3d-common  (static lib, no deps)
      ▲
      ├── vkd3d-shader (static lib; deps: vkd3d_common_dep, dxil_spirv_dep)   libs/vkd3d-shader/meson.build:9-11
      │        ▲
      └────────┴── vkd3d      (static lib; deps: vkd3d_common_dep, vkd3d_shader_dep)  libs/vkd3d/meson.build:119-121
                       ▲
                       └── d3d12core.dll (shared; deps: vkd3d_dep, gdi32, dxgi)  libs/d3d12core/meson.build:16-22
                                ▲  (loaded by name at runtime, NOT linked)
                                └── d3d12.dll (shared; deps: vkd3d_common_dep, gdi32, dxgi)  libs/d3d12/meson.build:22-28
```

`libs/vkd3d` is a **static** library. There is no `libvkd3d.dll`; the only shared objects are
`d3d12.dll` and `d3d12core.dll` (on Windows) / `libvkd3d-proton-d3d12{,core}.so` (elsewhere,
`libs/d3d12/meson.build:9`, `libs/d3d12core/meson.build:10`).

Compiled shaders: 46 GLSL sources listed at `libs/vkd3d/meson.build:1-64` are compiled by
`glslang`/`glslangValidator` to embedded C arrays (`meson.build:83-88`, target env `vulkan1.3`).

### 1.3 What `d3d12.dll` exports

`libs/d3d12/d3d12.def` (verbatim):

```
LIBRARY d3d12.dll

EXPORTS
    D3D12CreateDevice @101
    D3D12GetDebugInterface @102
    D3D12CreateRootSignatureDeserializer
    D3D12CreateVersionedRootSignatureDeserializer

    D3D12EnableExperimentalFeatures
    D3D12SerializeRootSignature
    D3D12SerializeVersionedRootSignature
    D3D12GetInterface
```

Eight exports; the two ordinals `@101`/`@102` match native `d3d12.dll`. Every one of them is a
one-line forward into `d3d12core.dll` through a private COM interface — e.g.
`libs/d3d12/main.c:143-152`:

```c
HRESULT WINAPI DLLEXPORT D3D12CreateDevice(IUnknown *adapter, D3D_FEATURE_LEVEL minimum_feature_level,
        REFIID iid, void **device)
{
    ...
    if (!load_d3d12core())
        return E_NOINTERFACE;
    return IVKD3DCoreInterface_CreateDevice(core, adapter, minimum_feature_level, iid, device);
}
```

The one thing `d3d12.dll` implements locally is `ID3D12SDKConfiguration1`
(`libs/d3d12/main.c:217-314`), because — comment at `main.c:322` — *"The vtable for this must live
in d3d12.dll. d3d12core.dll should not be loaded yet."*

`load_d3d12core()` (`main.c:66-141`) does `vkd3d_dlopen("d3d12core.dll")` +
`vkd3d_dlsym("D3D12GetInterface")`, then `D3D12GetInterface(&CLSID_VKD3DCore,
&IID_IVKD3DCoreInterface, &core)`. Comment at `main.c:74-76` explains why dlopen and not a link:
both DLLs export `D3D12GetInterface`. On Windows there is a fallback to
`GetSystemDirectoryA() + "\\d3d12core.dll"` (`main.c:117-129`) for games that ship their own
`D3D12Core.dll` next to the exe. `SONAME_D3D12CORE` is `"d3d12core.dll"` on Windows
(`include/vkd3d_sonames.h:26`).

### 1.4 What `d3d12core.dll` exports

`libs/d3d12core/d3d12core.def` (verbatim):

```
LIBRARY d3d12core.dll

EXPORTS
    D3D12GetInterface
    D3D12SDKVersion DATA PRIVATE
```

`D3D12SDKVersion` is a **data export**, `libs/d3d12core/main.c:1353-1355`:

```c
/* Just expose the latest stable AgilitySDK version.
 * This is actually exported as a UINT and not a function it seems. */
DLLEXPORT const UINT D3D12SDKVersion = D3D12_SDK_VERSION;
```

`D3D12GetInterface` in d3d12core (`main.c:1300-1351`) recognises exactly three CLSIDs:
- `CLSID_D3D12DeviceFactory` → a fresh `ID3D12DeviceFactory` / `ID3D12DeviceConfiguration1`
  (`create_device_factory`, `main.c:1288-1298`);
- `CLSID_VKD3DCore` → the singleton `IVKD3DCoreInterface` (`main.c:1046-1049`);
- `CLSID_VKD3DDebugControl` → the singleton `IVKD3DDebugControlInterface` (`main.c:1041-1044`).

The private core interface is an 8-method vtable, `main.c:828-838`:
`CreateDevice`, `CreateRootSignatureDeserializer`, `SerializeRootSignature`,
`CreateVersionedRootSignatureDeserializer`, `SerializeVersionedRootSignature`,
`GetDebugInterface`, `EnableExperimentalFeatures`, `GetInterface`.

**Drop-in verdict.** The export surface is the Agility-SDK shape: `d3d12.dll` +
`d3d12core.dll` with `D3D12SDKVersion` as a data export and `D3D12GetInterface` reaching
`CLSID_D3D12DeviceFactory`. `D3D12EnableExperimentalFeatures` is a stub that returns
`E_NOINTERFACE` (`main.c:807-814`) and `SetSDKVersion` is a `FIXME` returning `S_OK`
(`libs/d3d12/main.c:267-273`). So it *presents* as an Agility SDK redistributable but does not
honour version pinning. **UNVERIFIED:** whether Windows' loader-side Agility SDK path
(`D3D12SDKVersion`/`D3D12SDKPath` exported by the *app*, `D3D12Core.dll` in a subdirectory) picks
up vkd3d-proton's `d3d12core.dll` correctly on native Windows 11 24H2. *Settling experiment:*
drop `d3d12.dll`+`d3d12core.dll` next to a D3D12 sample from `dx-samples-research-only/` on the
win11 VM and check whether the loaded module list shows vkd3d's DLLs (Process Explorer / `listdlls`)
and whether `TRACE` output appears in the vkd3d log.

---

## 2. The COM boundary, and can the core be driven without it?

### 2.1 Shape of the impl structs

Every D3D12 object is a plain C struct whose **first** member is the COM interface, recovered with
`CONTAINING_RECORD`. Verbatim from `libs/vkd3d/vkd3d_private.h`:

```c
/* :1161 */ typedef ID3D12Resource2 d3d12_resource_iface;
/* :1163 */ struct d3d12_resource
            {
                d3d12_resource_iface ID3D12Resource_iface;
                LONG refcount;
                LONG internal_refcount;

                D3D12_RESOURCE_DESC1 desc;
                D3D12_HEAP_PROPERTIES heap_properties;
                D3D12_HEAP_FLAGS heap_flags;
                struct vkd3d_memory_allocation mem;
                struct vkd3d_memory_allocation private_mem;
                struct vkd3d_unique_resource res;
                D3DKMT_HANDLE kmt_local;
                ...
                struct vkd3d_private_store private_store;
                struct d3d_destruction_notifier destruction_notifier;
            };
```

```c
/* :3352 */ struct d3d12_command_list
            {
                d3d12_command_list_iface ID3D12GraphicsCommandList_iface;          /* = ID3D12GraphicsCommandList10, :2919 */
                d3d12_command_list_vkd3d_ext_iface ID3D12GraphicsCommandListExt_iface;
                LONG refcount;

                D3D12_COMMAND_LIST_TYPE type;
                VkQueueFlags vk_queue_flags;

                bool is_recording;
                bool is_valid;
                ...
                struct d3d12_command_list_sequence cmd;
                ...
                VkPipeline current_pipeline;
                VkPipeline command_buffer_pipeline;
                struct vkd3d_rendering_info rendering_info;
                struct vkd3d_dynamic_state dynamic_state;
                struct vkd3d_pipeline_bindings graphics_bindings;
                struct vkd3d_pipeline_bindings compute_bindings;
                enum vkd3d_pipeline_type active_pipeline_type;
                ...
            };
```

```c
/* :5744 */ struct d3d12_device
            {
                d3d12_device_iface ID3D12Device_iface;              /* = ID3D12Device15, :5456 */
                d3d12_device_vkd3d_ext_iface ID3D12DeviceExt_iface;
                d3d12_dxvk_interop_device_iface ID3D12DXVKInteropDevice_iface;
                d3d_low_latency_device_iface ID3DLowLatencyDevice_iface;
                IAmdExtAntiLagApi IAmdExtAntiLagApi_iface;
                ID3D12DeviceConfiguration1 ID3D12DeviceConfiguration1_iface;
                LONG refcount;

                VkDevice vk_device;
                uint32_t api_version;
                VkPhysicalDevice vk_physical_device;
                struct vkd3d_vk_device_procs vk_procs;
                ...
                IUnknown *parent;
                LUID adapter_luid;
                D3DKMT_HANDLE kmt_local;
                ...
            };
```

```c
/* :3839 */ struct d3d12_command_queue
            {
                d3d12_command_queue_iface ID3D12CommandQueue_iface;   /* = ID3D12CommandQueue, :3837 */
                d3d12_command_queue_vkd3d_ext_iface ID3D12CommandQueueExt_iface;
                LONG refcount;
                D3D12_COMMAND_QUEUE_DESC desc;
                struct vkd3d_queue *vkd3d_queue;
                struct d3d12_device *device;
                pthread_mutex_t queue_lock;
                pthread_cond_t queue_cond;
                pthread_t submission_thread;
                struct d3d12_command_queue_submission *submissions;
                ...
                struct dxgi_vk_swap_chain_factory vk_swap_chain_factory;
                ...
            };
```

The interface version each object targets (`grep "typedef ID3D12.*d3d12_.*_iface;" vkd3d_private.h`):

| typedef | line | interface |
|---|---|---|
| `d3d12_fence_iface` | 82 | `ID3D12Fence1` |
| `d3d12_heap_iface` | 1005 | `ID3D12Heap1` |
| `d3d12_resource_iface` | 1161 | `ID3D12Resource2` |
| `d3d12_pipeline_library_iface` | 2545 | `ID3D12PipelineLibrary1` |
| `d3d12_command_list_iface` | 2919 | `ID3D12GraphicsCommandList10` |
| `d3d12_command_queue_iface` | 3837 | `ID3D12CommandQueue` |
| `d3d12_device_iface` | 5456 | `ID3D12Device15` |
| `d3d12_state_object_iface` | 6233 | `ID3D12StateObject` |
| `d3d12_meta_command_iface` | 6512 | `ID3D12MetaCommand` |

37 distinct `CONST_VTBL` definitions exist in `libs/vkd3d/*.c`, of which 30 unique vtable **types**
(command: `grep -ohE "CONST_VTBL (struct )?[A-Za-z0-9_]+Vtbl" libs/vkd3d/*.c | sort -u`), including
`IDXGIVkSwapChain2Vtbl`, `IDXGIVkSwapChainFactoryVtbl`, `ID3D12DXVKInteropDevice3Vtbl`,
`ID3DLowLatencyDeviceVtbl`, `IAmdExtAntiLagApiVtbl`, `ID3DDestructionNotifierVtbl`.

### 2.2 How much internal code dispatches through COM?

```
$ grep -ohE "\bID3D12[A-Za-z0-9]+_[A-Za-z0-9_]+\(" libs/vkd3d/*.c | sort | uniq -c | sort -rn | head
      8 ID3D12CommandQueue_Release(
      7 ID3D12RootSignature_Release(
      7 ID3D12CommandQueue_QueryInterface(
      6 ID3D12Resource_Release(
      6 ID3D12Pageable_QueryInterface(
      4 ID3D12CommandQueue_AddRef(
      3 ID3D12Heap_Release(
      3 ID3D12Fence_Release(
      2 ID3D12GraphicsCommandList10_SetPipelineState(
      ...
```

Nearly all of it is `AddRef`/`Release`/`QueryInterface`. The only *functional* internal vtable
dispatch is **bundle replay**: `libs/vkd3d/bundle.c` (1,929 LOC) records D3D12 calls as a linked
list of `struct d3d12_bundle_command { pfn_d3d12_bundle_command proc; ... }`
(`vkd3d_private.h:3584-3590`) and replays them through
`d3d12_bundle_execute(bundle, d3d12_command_list_iface *list)` (`vkd3d_private.h:3610`), which
calls `ID3D12GraphicsCommandList10_*` on the target list.

So: **COM is a skin, not a skeleton.** The translation logic lives in `d3d12_command_list_*`,
`d3d12_device_*`, `d3d12_resource_*` static functions that take the impl struct after one
`impl_from_*` cast at the top.

### 2.3 …except that the vtable *is* a specialisation mechanism

Two places make the vtable load-bearing rather than decorative.

**(a) `d3d12_device_replace_vtable`** (`device.c:11302-11400`, called from device init at
`device.c:11608`) swaps `device->ID3D12Device_iface.lpVtbl` between **11** hand-tuned variants
keyed on the descriptor layout the driver reports:

```c
if (d3d12_device_use_descriptor_heap(device))
{
    if ((device->bindless_state.flags & VKD3D_BINDLESS_MUTABLE_EMBEDDED_PACKED_METADATA) &&
            device->bindless_state.cbv_srv_uav_size == 64 &&
            device->bindless_state.sampler_size == 16)
    {
        /* RDNA2 */
        device->ID3D12Device_iface.lpVtbl = &d3d12_device_vtbl_heap_64_16_packed;
    }
    else if (... 32/16 ...) /* RDNA3+ */   device->...lpVtbl = &d3d12_device_vtbl_heap_32_16_planar;
    else if (... 32/32 ...) /* NV */       device->...lpVtbl = &d3d12_device_vtbl_heap_32_32_planar;
    else if (... 128/32 ...) /* Intel */   device->...lpVtbl = &d3d12_device_vtbl_heap_128_32_planar;
    else                                   device->...lpVtbl = &d3d12_device_vtbl_heap_generic;
}
else if (d3d12_device_use_embedded_mutable_descriptors(device)) { ... 3 variants ... }
else if (d3d12_device_uses_descriptor_buffers(device))          { ... 2 variants ... }
```

These variants exist to make `CreateShaderResourceView` / `CopyDescriptors` compile to a
straight-line memcpy for a known descriptor size. *(Note for Helios: the venus/Mesa ICD will land
in `d3d12_device_vtbl_heap_generic` or the descriptor-buffer generic path unless its descriptor
sizes happen to match one of the four hardcoded shapes — see §12 open question 3.)*

**(b) `VKD3D_DECLARE_D3D12_GRAPHICS_COMMAND_LIST_VARIANT`** (`command.c:21956-22059`) declares
**6** `ID3D12GraphicsCommandList10Vtbl` variants (`default`, `embedded_64_16`, `embedded_32_16`,
`embedded_32_32`, `embedded_128_32`, `embedded_default`, `command.c:22061-22066`). They differ in
exactly two slots — `SetComputeRootDescriptorTable_##variant` and
`SetGraphicsRootDescriptorTable_##variant`. With `VKD3D_ENABLE_PROFILING` a seventh
(`d3d12_command_list_vtbl_profiled`, from `command_list_profiled.h`) is selected at
`command.c:22128-22131`.

`88` of the command-list DDI-equivalent methods are declared as
`static ... STDMETHODCALLTYPE d3d12_command_list_*` (`grep -c "STDMETHODCALLTYPE d3d12_command_list_"
libs/vkd3d/command.c` → 88; `grep -c "^static.*STDMETHODCALLTYPE" command.c` → 136 including
allocator/queue/signature). **All are `static`** — e.g. `d3d12_command_list_Close`
(command.c:6969), `..._DrawInstanced` (9246), `..._Dispatch` (9488), `..._ResourceBarrier`
(13516). They are not linkable from outside `command.c` today.

### 2.4 Verdict: can a `d3d12umddi` frontend drive the core?

**Yes in principle, at a cost that is real but bounded — and it is the wrong lever for Helios.**

Specifically, the surgery:

1. **De-`static` the 88 command-list entry points and the ~120 device/resource entry points**, or
   (cleaner) add a parallel `struct vkd3d_d3d12_core_ops` table populated from the same function
   pointers the vtable macro already lists. The macro at `command.c:21956` is literally already
   *"a table of every method in call order"* — the mechanical work is turning it into a
   non-`STDMETHODCALLTYPE` ops struct. Cost: mechanical, large diff, permanent rebase burden
   against a project that rewrites `command.c` constantly (26,534 LOC, the most-churned file).
2. **Replace `CONTAINING_RECORD(iface, …)` at the top of every method** — trivially the same edit
   as (1), since the frontend would pass the impl struct directly.
3. **Reimplement the vtable-swap specialisation** (`device.c:11302`, `command.c:22061`) as an
   ops-table swap. Without this you lose the descriptor fast paths *and* the profiling build.
4. **Deal with refcounting.** `d3d12_resource` carries **two** counters (`refcount`,
   `internal_refcount`, `vkd3d_private.h:1166-1167`); `d3d12_fence`/`d3d12_shared_fence` carry
   `refcount_internal` + `refcount` (`:656-657`, `:723-724`). `vkd3d_resource_incref` /
   `vkd3d_resource_decref` are *public API* (`include/vkd3d.h:126-127`). A non-COM frontend has to
   drive both lifetimes correctly or leak/UAF.
5. **Rewrite `bundle.c` wholesale** (1,929 LOC) — it is the one subsystem that genuinely records
   and replays through the COM vtable.

Additionally, four *unavoidable* impedance mismatches with `d3d12umddi.h` that no amount of
de-COM-ing fixes, and which are the real reason not to do this:

- **`d3d12_command_queue` owns a POSIX thread** (`submission_thread`, `vkd3d_private.h:3849`;
  worker at `command.c:25249`). The D3D12 UMD DDI has no queue object at all — the runtime owns
  submission. You would be deleting the entire `enum vkd3d_submission_type` machinery
  (`vkd3d_private.h:3689-3698`) and the `d3d12_command_queue_submission` union (`:3775-3786`).
- **vkd3d creates the `VkDevice` itself** (§3) — under d3d12umddi the runtime hands you an
  adapter/device pair and expects you to be the *driver*, not a Vulkan client.
- **`ID3D12Fence` semantics are implemented in userspace** with a virtual-value fixup layer
  (`d3d12_fence.virtual_value`, `pending_updates`, `wait_tickets`, `vkd3d_private.h:655-689`)
  because D3D12 fences allow arbitrary out-of-order signalling that Vulkan timeline semaphores
  forbid. That layer duplicates what dxgkrnl's monitored fences already do.
- **vkd3d fabricates nothing at the D3DKMT layer**; it consumes it (§6). The d3d12umddi frontend
  would need the *opposite* direction.

**Recommendation to the D3D12 plan:** the separability question ("strategy (a)") is answerable —
the answer is "possible, ~4 named surgeries plus a bundle rewrite, plus a permanent rebase tax on
the 26.5k-LOC file upstream churns hardest" — and it is dominated by strategy (b) (ship
`d3d12.dll`/`d3d12core.dll` unmodified), because vkd3d's whole design assumes it *is* a Vulkan
client, which on Helios it already can be.

---

## 3. The device model

### 3.1 Public creation API (`include/vkd3d.h:74-111`)

```c
struct vkd3d_device_create_info
{
    D3D_FEATURE_LEVEL minimum_feature_level;

    struct vkd3d_instance *instance;
    const struct vkd3d_instance_create_info *instance_create_info;

    VkPhysicalDevice vk_physical_device;

    const char * const *device_extensions;
    uint32_t device_extension_count;

    const char * const *optional_device_extensions;
    uint32_t optional_device_extension_count;

    IUnknown *parent;
    LUID adapter_luid;

    D3D12_DEVICE_FACTORY_FLAGS device_factory_flags;
    bool independent;
};

HRESULT vkd3d_create_device(const struct vkd3d_device_create_info *create_info,
        REFIID iid, void **device);
```

So the *caller* picks the `VkPhysicalDevice` and supplies the LUID. `vkd3d_create_instance` takes a
`PFN_vkGetInstanceProcAddr` — vkd3d never hard-links to a loader.

### 3.2 How `d3d12core.dll` fills that in on Windows (`libs/d3d12core/main.c`)

1. **Load Vulkan** (`load_modules_once`, `main.c:319-365`): tries `winevulkan.dll` **first**, then
   `vulkan-1.dll` — *"If possible, load winevulkan directly in order to bypass issues with
   third-party overlays hooking the Vulkan loader"* (`main.c:335-336`).
   ⚠ On native Windows there is no `winevulkan.dll`, so it falls through to `vulkan-1.dll`.
2. **Get a DXGI adapter** (`d3d12_get_adapter`, `main.c:375-444`): if the app passed no adapter,
   `CreateDXGIFactory1(&IID_IDXGIFactory4, …)` + `EnumAdapters(0)`. If the app passed an
   `IDXCoreAdapter`, read its `InstanceLuid` property and scan DXGI adapters for a LUID match. Else
   `QueryInterface(IID_IDXGIAdapter)`. File-top comment `main.c:374`:
   *"TODO: We need to attempt to dlopen() native DXVK DXGI."*
3. **Create the instance** (`vkd3d_create_instance_global`, `main.c:569-641`) with **required**
   instance extensions `VK_KHR_surface` + `VK_KHR_win32_surface` (`main.c:574-580`) and optional
   `VK_KHR_surface_maintenance1`, `VK_EXT_surface_maintenance1`,
   `VK_KHR_get_surface_capabilities2` (`main.c:582-593`). It additionally splices in OpenVR and
   OpenXR instance extensions read out of `HKCU\Software\Wine\VR` and `wineopenxr.dll`
   (`main.c:129-249`, `main.c:613-628`) — all no-ops on native Windows.
4. **Match the physical device by LUID** (`d3d12_find_physical_device`, `main.c:446-566`). This is
   the load-bearing function for Helios. Its algorithm, in order:

   ```c
   /* pass 1 — LUID */
   if (properties2.properties.apiVersion < VKD3D_MIN_API_VERSION) continue;      /* :492 */
   id_properties.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES;         /* :498 */
   ...
   if (id_properties.deviceLUIDValid &&
       !memcmp(id_properties.deviceLUID, &adapter_desc->AdapterLuid, VK_LUID_SIZE))  /* :506 */
   {
       /* tie-break on deviceID/vendorID, then on deviceName vs DXGI Description */ /* :510-532 */
   }

   /* pass 2 — PCI IDs */
   if (properties2.properties.deviceID == adapter_desc->DeviceId &&
       properties2.properties.vendorID == adapter_desc->VendorId)                /* :547-548 */

   /* pass 3 */
   FIXME("Could not find Vulkan physical device for DXGI adapter.\n");
   WARN("Using first available physical device...\n");
   vk_physical_device = vk_physical_devices[0];                                  /* :558-560 */
   ```

   It uses `VkPhysicalDeviceIDProperties::deviceLUID` + `deviceLUIDValid`, **not**
   `VK_EXT_pci_bus_info` and **not** `VK_KHR_driver_properties`. Neither appears in
   `optional_device_extensions` (`device.c:66-172`).

   **Helios relevance:** memory `[30TH]` records that the venus ICD already reports the WDDM
   adapter LUID. If that holds under this exact read — `VkPhysicalDeviceIDProperties.deviceLUID`
   with `deviceLUIDValid == VK_TRUE`, byte-compared against `DXGI_ADAPTER_DESC::AdapterLuid` —
   pass 1 succeeds and vkd3d binds the right device. If not, pass 3 silently picks device 0, which
   on a single-GPU guest is still correct but is a landmine on any multi-adapter guest.

5. **Create the device** (`main.c:643-740`): required device extension `VK_KHR_swapchain`
   (`main.c:659-662`); optional `VK_KHR_swapchain_maintenance1`, `VK_EXT_swapchain_maintenance1`
   (`main.c:664-668`); `device_create_info.parent = (IUnknown *)dxgi_adapter` and
   `memcpy(&device_create_info.adapter_luid, &adapter_desc.AdapterLuid, VK_LUID_SIZE)`
   (`main.c:707-708`).

### 3.3 Physical-device fallback inside `libs/vkd3d`

If the caller passes `vk_physical_device == VK_NULL_HANDLE`, `vkd3d_select_physical_device`
(`device.c:3489-3572`) skips any device below `VKD3D_MIN_API_VERSION` (`:3538`), honours
`VKD3D_FILTER_DEVICE_NAME`, then prefers `VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU` >
`INTEGRATED_GPU` > `physical_devices[0]` (`:3554-3557`, `:3829` region).

### 3.4 Queue families (`device.c:3789-3850`)

Six logical families (`vkd3d_private.h:5403-5413`): `GRAPHICS`, `COMPUTE`, `TRANSFER`,
`OPTICAL_FLOW`, `INTERNAL_COMPUTE`, `SPARSE_BINDING`.

```
GRAPHICS        = find(mask GRAPHICS|COMPUTE, want GRAPHICS|COMPUTE)
COMPUTE         = find(mask GRAPHICS|COMPUTE, want COMPUTE)                 → falls back to GRAPHICS
SPARSE_BINDING  = find(GRAPHICS|COMPUTE|TRANSFER|SPARSE, want SPARSE)
                  → find(GRAPHICS|SPARSE, want SPARSE)
                  → find(SPARSE, want SPARSE)
TRANSFER        = find(GRAPHICS|COMPUTE|TRANSFER, want TRANSFER)            → falls back to COMPUTE
OPTICAL_FLOW    = only if NV_optical_flow
INTERNAL_COMPUTE= COMPUTE
```

`VKD3D_CONFIG=single_queue` (`include/private/config_flag_decl.h:8`) collapses COMPUTE and TRANSFER
onto GRAPHICS (`device.c:3843-3847`). **This is the first knob to try on venus** if the guest ICD
exposes a single queue family or if async-queue submission destabilises the transport.

There is also an `internal_sparse_queue` reservation (`d3d12_device_reserve_internal_sparse_queue`,
called at `device.c:11616`) and a `sparse_init_timeline` semaphore
(`vkd3d_private.h:5790-5792`) — vkd3d uses sparse binding for *initialisation*, not only for
tiled resources.

---

## 4. Fences, queues, ExecuteCommandLists

### 4.1 ⚠ Correction to the lane brief: `queue_timeline.c` is a profiler, not fence machinery

`libs/vkd3d/queue_timeline.c` (718 LOC) is a **Chrome-trace emitter**, gated on the
`VKD3D_QUEUE_PROFILE` environment variable:

```c
/* queue_timeline.c:28-42 */
HRESULT vkd3d_queue_timeline_trace_init(struct vkd3d_queue_timeline_trace *trace, struct d3d12_device *device)
{
    ...
    if (!vkd3d_get_env_var("VKD3D_QUEUE_PROFILE", env, sizeof(env)))
        return S_OK;

    trace->file = fopen(env, "w");
    ...
        fputs("[\n", trace->file);
```

`NUM_ENTRIES (256 * 1024)` cookie slots (`:26`). It emits regions for `EXECUTE`/`WAIT`/`SIGNAL`/
`DRAIN`/`SPARSE`/`CALLBACK`/`STOP`, plus `register_present_wait` (:488), `register_present_block`
(:510), `register_pso_compile` (:496), `register_command_list` (:417), `register_swapchain_blit`
(:409), `register_low_latency_sleep` (:518). `VKD3D_QUEUE_PROFILE_ABSOLUTE=1` zeroes the timebase
so it lines up with Wine's QPC-relative logs (`:57-67`).

**This is directly reusable as a Helios instrument.** It produces exactly the class of evidence
ROADMAP WS2 needed for the present-queue stall, from inside the D3D12 client. No code change.

### 4.2 The real fence implementation

Two fence flavours, both behind `d3d12_fence_iface = ID3D12Fence1` (`vkd3d_private.h:82`):

**`struct d3d12_fence`** (`vkd3d_private.h:655-689`) — the normal case. It does **not** map 1:1
onto a Vulkan timeline semaphore. It keeps a *virtual* value and a fixup list, because D3D12
permits signalling a fence to a *lower* value and out of order:

```c
struct d3d12_fence
{
    d3d12_fence_iface ID3D12Fence_iface;
    LONG refcount_internal;
    LONG refcount;
    D3D12_FENCE_FLAGS d3d12_flags;

    /* only used for shared semaphores */
    VkSemaphore timeline_semaphore;

    uint64_t max_pending_virtual_timeline_value;
    uint64_t virtual_value;
    uint64_t signal_count;
    uint64_t update_count;
    struct d3d12_fence_value *pending_updates;      /* {virtual_value, update_count, vk_semaphore, vk_semaphore_value} :622-628 */
    ...
    struct vkd3d_waiting_event *events;             /* :639-647 */
    struct vkd3d_fence_wait_ticket *wait_tickets;   /* :649-653 */
    uint64_t wait_ticket_counter;
    ...
};
```

Note the comment at `:665`: the `VkSemaphore timeline_semaphore` member is *"only used for shared
semaphores"* — an ordinary `ID3D12Fence` is backed by the queue's semaphores plus this bookkeeping,
not by one dedicated semaphore.

**`struct d3d12_shared_fence`** (`vkd3d_private.h:720-737`) — used when the fence is shared. It
*is* backed 1:1 by a `VkSemaphore timeline_semaphore`, carries a `D3DKMT_HANDLE kmt_local`, and
runs its **own waiter thread** (`pthread_t thread`, `:729`) servicing a `struct list events`.

Dispatch between them is by vtable-pointer identity, not by a flag —
`is_shared_ID3D12Fence1()` (`vkd3d_private.h:757-765`):

```c
static inline bool is_shared_ID3D12Fence1(ID3D12Fence1 *iface)
{
    extern CONST_VTBL struct ID3D12Fence1Vtbl d3d12_shared_fence_vtbl;
    extern CONST_VTBL struct ID3D12Fence1Vtbl d3d12_fence_vtbl;
    assert(iface->lpVtbl ==  &d3d12_shared_fence_vtbl || iface->lpVtbl == &d3d12_fence_vtbl);

    return iface->lpVtbl ==  &d3d12_shared_fence_vtbl;
}
```

*(This is a fourth site where the COM vtable is semantically load-bearing — add it to §2.4's
surgery list.)*

### 4.3 The submission thread

`ExecuteCommandLists` (`command.c:22764`) does **not** call `vkQueueSubmit2`. It builds a
`struct d3d12_command_queue_submission` (`vkd3d_private.h:3775-3786`) and pushes it to a per-queue
worker:

```c
enum vkd3d_submission_type            /* vkd3d_private.h:3689-3698 */
{
    VKD3D_SUBMISSION_WAIT,
    VKD3D_SUBMISSION_SIGNAL,
    VKD3D_SUBMISSION_EXECUTE,
    VKD3D_SUBMISSION_BIND_SPARSE,
    VKD3D_SUBMISSION_STOP,
    VKD3D_SUBMISSION_QUEUE_USING_CALLBACK,
    VKD3D_SUBMISSION_DRAIN,
    VKD3D_SUBMISSION_RESOURCE_RETAIN
};
```

The `EXECUTE` payload (`vkd3d_private.h:3735-3756`) carries `VkCommandBufferSubmitInfo *cmd`,
`uint32_t *cmd_cost`, the owning `d3d12_command_allocator **`, a `low_latency_frame_id`, an
initial-layout `vkd3d_initial_transition *transitions` list, a
`vkd3d_queue_timeline_trace_cookie`, and `bool split_submission`.

`d3d12_command_queue_submission_worker_main` (`command.c:25249-25412`) is a classic
mutex+condvar consumer: `vkd3d_set_thread_name("vkd3d_queue")` (`:25265`), pops FIFO with a
`memmove`, flushes pending sparse binds for any non-`BIND_SPARSE` submission (`:25283`), then
per-type:
- `WAIT` → `d3d12_command_queue_wait_shared()` for shared fences, `d3d12_command_queue_wait()` otherwise;
- `SIGNAL` → `d3d12_command_queue_flush_waiters(queue, 0)` then `d3d12_command_queue_signal_inline()`;
- `EXECUTE` → build the initial-transition command buffer from `pool` and call
  `d3d12_command_queue_execute()`;
- `DRAIN` → `flush_waiters(EXTERNAL|SERIALIZING)`, bump `queue_drain_count`, signal;
- `QUEUE_USING_CALLBACK` → `flush_waiters(EXTERNAL|SERIALIZING)` then run the callback
  (this is the WSI hook — the swapchain presents from *inside* the submission thread).

Deferred wait bookkeeping lives on the queue itself: `struct vkd3d_fence_virtual_wait *wait_fences`
and `VkSemaphoreSubmitInfo *wait_semaphores` (`vkd3d_private.h:3872, :3876`), flushed by
`d3d12_command_queue_flush_waiters(queue, flags)` where
`VKD3D_WAIT_SEMAPHORES_EXTERNAL = 1<<0` and `VKD3D_WAIT_SEMAPHORES_SERIALIZING = 1<<1`
(`vkd3d_private.h:3820-3821`). There is a `VkSemaphore serializing_semaphore`
(`vkd3d_private.h:3880`) used to serialise against external consumers.

### 4.4 Bridging with WDDM monitored fences — the honest read

- vkd3d **never** touches a WDDM monitored fence directly. The only D3DKMT sync object it opens is
  `D3DKMTOpenSyncObjectFromNtHandle` on a handle it obtained from
  `vkGetSemaphoreWin32HandleKHR(..., VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_D3D12_FENCE_BIT)`
  (`d3dkmt.c:51-77`). So the *Vulkan driver* is the one that must produce a WDDM-shareable
  semaphore.
- `VK_KHR_external_semaphore_win32` is in `optional_device_extensions` under `#ifdef _WIN32`
  (`device.c:99-102`) — **optional**, so an ICD that lacks it degrades rather than fails.
- Consequence for Helios: the fence-interop question is *entirely* a question about the Mesa venus
  ICD's `VK_KHR_external_semaphore_win32` support with the `D3D12_FENCE` handle type, and about
  whether `kmd_render` backs the resulting monitored fence. That belongs to R5/R6/R12; from R3's
  side there is nothing to build, only a capability to check.

---

## 5. Descriptors, memory, and GPU virtual addresses

### 5.1 Descriptor strategy: four alternatives, chosen at device init

`enum` at `vkd3d_private.h:4451-4463` (bindless mode flags):

```
VKD3D_BINDLESS_CBV_AS_SSBO                      = 1<<0
VKD3D_BINDLESS_RAW_SSBO                         = 1<<1
VKD3D_BINDLESS_MUTABLE_TYPE                     = 1<<6
VKD3D_BINDLESS_MUTABLE_TYPE_RAW_SSBO            = 1<<8
VKD3D_BINDLESS_MUTABLE_EMBEDDED                 = 1<<9
VKD3D_BINDLESS_MUTABLE_EMBEDDED_PACKED_METADATA = 1<<10
VKD3D_BINDLESS_MUTABLE_TYPE_SPLIT_RAW_TYPED     = 1<<11
VKD3D_BINDLESS_HEAP                             = 1<<12
```

and set-kind flags at `:4470-4485` (`SAMPLER`, `CBV`, `SRV`, `UAV`, `IMAGE`, `BUFFER`, `RAW_SSBO`,
`MUTABLE`, `MUTABLE_RAW`, `MUTABLE_TYPED`, plus four `EXTRA_*` aux-buffer bindings masked by
`VKD3D_BINDLESS_SET_EXTRA_MASK = 0xff000000`).

The four backends, in descending preference (selection logic in
`d3d12_device_replace_vtable`, `device.c:11302-11400`, and predicates at
`vkd3d_private.h:6046-6054`, `6148`):

| Backend | Predicate | Vulkan mechanism |
|---|---|---|
| **Descriptor heap** | `d3d12_device_use_descriptor_heap()` → `VKD3D_BINDLESS_HEAP` | `VK_EXT_descriptor_heap` — gated behind `VKD3D_CONFIG=descriptor_heap` (`device.c:141`: `VK_EXTENSION_COND(EXT_DESCRIPTOR_HEAP, EXT_descriptor_heap, VKD3D_CONFIG_FLAG_STATIC(DESCRIPTOR_HEAP))`, flag at `config_flag_decl.h:65`) |
| **Embedded mutable** | `d3d12_device_use_embedded_mutable_descriptors()` → `VKD3D_BINDLESS_MUTABLE_EMBEDDED` | descriptor buffer + mutable type, descriptors written straight into the D3D12 heap allocation |
| **Descriptor buffer** | `d3d12_device_uses_descriptor_buffers()` | `VK_EXT_descriptor_buffer` (`device.c:127`); `VK_DESCRIPTOR_SET_LAYOUT_CREATE_DESCRIPTOR_BUFFER_BIT_EXT` at `state.c:1422`, `state.c:1598`; `VK_PIPELINE_CREATE_2_DESCRIPTOR_BUFFER_BIT_EXT` at `state.c:3581, 4539, 4698, 6780, 6995` |
| **Legacy sets** | otherwise | plain `VkDescriptorSet` + `vkUpdateDescriptorSets` — `resource.c` is the **only** file with any `vkUpdateDescriptorSets*` calls, 11 of them (`grep -c`), everything else writes descriptor memory directly |

Mutable descriptors come from either `VK_EXT_mutable_descriptor_type` (`device.c:122`) or the
`VALVE` alias (`device.c:163`); the feature struct is chained at `device.c:2200-2203`.
`VKD3D_MAX_DESCRIPTOR_SIZE` is `256u` — *"Maximum allowed value in VK_EXT_descriptor_buffer/heap"*
(`vkd3d_private.h:65`).

Descriptor **sizes** the fast paths are compiled for (device.c:11324-11395): `(cbv_srv_uav,
sampler)` = `(64,16)` packed [RDNA2], `(32,16)` planar [RDNA3+], `(32,32)` planar [NV], `(128,32)`
planar [Intel], plus descriptor-buffer `(16,·,4)` [NV Turing+] and `(64,·,32)` [Intel/ANV].
Anything else → the generic path.

Shader-visible heaps get their CPU VA range registered into the va_map so a `D3D12_GPU_DESCRIPTOR_HANDLE`
can be resolved to a heap offset — `vkd3d_va_map_insert_descriptor_heap` /
`vkd3d_va_map_query_descriptor_heap_offset` (`va_map.c:405-490`); the lookup is a **linear scan**
with the comment *"We don't expect there to be that many shader visible descriptor heaps live on
the device, so a simple linear search is perfectly fine."* (`va_map.c:477-478`).

### 5.2 Memory model (`memory.c`, 2,276 LOC; `heap.c`, 422 LOC)

Entry points (`grep "^HRESULT\|^void\|^bool " libs/vkd3d/memory.c`):
`vkd3d_select_memory_flags` (:734) → `vkd3d_create_global_buffer` (:796) →
`vkd3d_try_allocate_device_memory` (:895) / `vkd3d_allocate_device_memory` (:1097) →
`vkd3d_allocation_assign_gpu_address` (:1176) → `vkd3d_memory_allocation_init` (:1310).
Sub-allocation: `vkd3d_memory_chunk_allocate_range` (:1601), `vkd3d_memory_chunk_create` (:1749),
`vkd3d_memory_allocator_try_suballocate_memory` (:1891), `vkd3d_suballocate_memory` (:1964),
`vkd3d_allocate_memory` (:2084), `vkd3d_allocate_heap_memory` (:2165).
Host-pointer import: `vkd3d_import_host_memory` (:1147) — `VK_EXT_external_memory_host`
(`device.c:120`).

Chunk sizing: `VKD3D_MEMORY_CHUNK_SIZE = VKD3D_VA_BLOCK_SIZE * 8` = 16 MiB
(`vkd3d_private.h:793`), `VKD3D_MEMORY_IMAGE_HEAP_SUBALLOCATE_THRESHOLD = 8 MiB` (`:794`),
`VKD3D_MEMORY_LARGE_CHUNK_SIZE = 32 MiB` (`:795`).

Allocation flags (`vkd3d_private.h:775-790`): `GLOBAL_BUFFER`, `GPU_ADDRESS`, `CPU_ACCESS`,
`ALLOW_WRITE_WATCH`, `NO_FALLBACK`, `DEDICATED`, `INTERNAL_SCRATCH`
(*"never suballocated since we do that ourselves, and we do not consume space in the VA map"*),
`ALLOW_IMAGE_SUBALLOCATION`.

There is a dedicated **memory transfer queue** (`vkd3d_memory_transfer_queue`,
`vkd3d_private.h:937`; `memory.c:113-606`) used for zeroing, `WriteToSubresource`, and building
the empty null-RTAS — an async background uploader with its own timeline.

Suballocation policy is quirked by driver capability
(`d3d12_device_allow_committed_texture_suballocation`, `vkd3d_private.h:6006-6013`):
*"Default is chosen due to Diablo 4 regressing CPU perf massively when we don't suballocate
committed textures."*, gated on `VK_EXT_zero_initialize_device_memory` +
`VK_EXT_pageable_device_local_memory` (`vkd3d_private.h:5997-6004`).

### 5.3 GPU virtual addresses — **vkd3d does not fabricate them**

The lane brief asked "how does vkd3d fabricate D3D12 GPU virtual addresses over Vulkan buffer
device address?" — **it doesn't**. The D3D12 GPU VA *is* the Vulkan device address, verbatim:

```c
/* resource.c:2656-2663 */
static D3D12_GPU_VIRTUAL_ADDRESS STDMETHODCALLTYPE d3d12_resource_GetGPUVirtualAddress(d3d12_resource_iface *iface)
{
    struct d3d12_resource *resource = impl_from_ID3D12Resource2(iface);
    TRACE("iface %p.\n", iface);
    return resource->res.va;
}
```

`resource->res` is a `struct vkd3d_unique_resource` (`vkd3d_private.h:841-855`):

```c
struct vkd3d_unique_resource
{
    union { VkBuffer vk_buffer; VkImage vk_image; };
    struct vkd3d_cookie cookie;
    VkDeviceAddress va;
    VkDeviceSize size;
    struct vkd3d_view_map *view_map;  /* only for RTAS */
};
```

and `va` comes from `vkd3d_get_buffer_device_address` (`resource.c:7378-7388`), i.e.
`vkGetBufferDeviceAddress`. Slicing a suballocation just adds the offset
(`vkd3d_memory_allocation_slice`, `vkd3d_private.h:881-891`: `dst->resource.va += offset;`).

**What `va_map.c` actually is:** a lock-free *reverse* lookup, VA → `vkd3d_unique_resource`,
needed because D3D12 passes raw GPU VAs in APIs (root CBV/SRV/UAV, `ExecuteIndirect` argument
buffers, RTAS addresses) that Vulkan expresses as `(VkBuffer, offset)`.

Structure (`vkd3d_private.h:321-331`, `353-400`):

```
VKD3D_VA_BLOCK_SIZE_BITS  21   → VKD3D_VA_BLOCK_SIZE  = 2 MiB, VKD3D_VA_LO_MASK  = 2 MiB-1
VKD3D_VA_BLOCK_BITS       20   → VKD3D_VA_BLOCK_COUNT = 1 Mi entries per tree node
VKD3D_VA_NEXT_BITS        12   → VKD3D_VA_NEXT_COUNT  = 4096 child pointers per node

struct vkd3d_va_entry { VkDeviceAddress va; const struct vkd3d_unique_resource *resource; };
struct vkd3d_va_block { struct vkd3d_va_entry l, r; };
struct vkd3d_va_tree  { struct vkd3d_va_block blocks[VKD3D_VA_BLOCK_COUNT];
                        struct vkd3d_va_tree *next[VKD3D_VA_NEXT_COUNT]; };
```

- Resources **≥ 2 MiB** go in the radix tree, one `l`/`r` half-entry per covered 2 MiB block
  (`vkd3d_va_map_insert`, `va_map.c:126-174`). Lookup (`vkd3d_va_map_deref_mutable`, `:222-243`)
  is a two-compare branch, entirely atomic loads, **no lock**.
- Resources **< 2 MiB** go into a mutex-guarded sorted array `small_entries` searched by binary
  search (`vkd3d_va_map_find_small_entry`, `:95-124`).
- Tree nodes are CAS-installed (`vkd3d_atomic_ptr_compare_exchange`, `:66`) and only freed at
  `vkd3d_va_map_cleanup` (`:396-403`) — **the tree never shrinks**. Each `vkd3d_va_tree` is
  `1Mi * 32 B + 4096 * 8 B ≈ 32 MiB`. **UNVERIFIED:** the actual steady-state footprint of the va
  tree under a real title on a 64-bit VA space. *Settling experiment:* run a D3D12 sample under
  vkd3d with `VKD3D_DEBUG=warn` and instrument `vkd3d_va_map_get_block`'s `vkd3d_calloc` call site
  (`va_map.c:65`) with a counter, or just watch process working set.
- The map also carries the RTAS placement state machine
  (`vkd3d_va_map_place_acceleration_structure`, `:299-388`) with the
  `UNKNOWN → TLAS|NON_TLAS → MUTATED` CAS loop documented at `:353-357`.

---

## 6. `libs/vkd3d/d3dkmt.c` — read in full (449 lines)

**Header:** `Copyright 2025 Rémi Bernon for Codeweavers`. Added in **8 commits over 2025-10-15 …
2025-10-30** (`git log --oneline -- libs/vkd3d/d3dkmt.c include/private/vkd3d_d3dkmt.h`):
`fa8d2f92` *Open device D3DKMT local handle* · `c5c63a08` *…shared fence…* · `85bdeb41`
*…shared resource…* · `1cfeb0be` *Create shared handles using D3DKMTShareObjects* · `e5404251`
*Update the resource runtime data after creation* · `47f761a9` *Open shared resources using the
D3DKMT API* · `fbcec58c` *Avoid some redundant KMT destroy calls* · `f9028fc1` *formatting*.

### 6.1 Platform

The whole file is `#ifdef _WIN32` (`:23`) with a no-op `#else` block (`:419-449`) that just
`WARN("Not implemented on this platform")`. But **"Windows" here means Wine**: the escape type it
uses is Wine-private.

`include/private/vkd3d_d3dkmt.h:119-122`:

```c
typedef enum _D3DKMT_ESCAPETYPE
{
    D3DKMT_ESCAPE_UPDATE_RESOURCE_WINE = 0x80000000
} D3DKMT_ESCAPETYPE;
```

That is not a Microsoft `D3DKMT_ESCAPETYPE` value; it is Wine's `win32u` D3DKMT emulation.
The whole header is a **hand-written re-declaration** of the D3DKMT ABI (381 lines,
`D3DKMT_CREATEDEVICE`, `D3DDDI_ALLOCATIONLIST`, `D3DDDI_PATCHLOCATIONLIST`,
`D3DKMT_OPENRESOURCEFROMNTHANDLE`, `D3DKMT_QUERYRESOURCEINFO*`, `D3DKMTShareObjects`, …) rather
than an include of `d3dkmthk.h` — because vkd3d builds against widl/mingw headers, not the WDK.

### 6.2 What it does

Six entry points, all called opportunistically and all failing soft:

| Function | Called from | D3DKMT used |
|---|---|---|
| `d3d12_device_open_kmt` (:25) | `device.c:11617`, at the end of device create | `D3DKMTOpenAdapterFromLuid(device->adapter_luid)` → `D3DKMTCreateDevice` → `D3DKMTCloseAdapter`; stores `device->kmt_local` |
| `d3d12_device_close_kmt` (:44) | `device.c:4885` | `D3DKMTDestroyDevice` |
| `d3d12_shared_fence_open_export_kmt` (:51) | `command.c:2227` | `vkGetSemaphoreWin32HandleKHR(VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_D3D12_FENCE_BIT)` → `D3DKMTOpenSyncObjectFromNtHandle` → `fence->kmt_local` |
| `d3d12_resource_open_export_kmt` (:89) | `resource.c:4469` | `vkGetMemoryWin32HandleKHR(OPAQUE_WIN32)` → `D3DKMTOpenResourceFromNtHandle` → `resource->kmt_local`, then `D3DKMTEscape(D3DKMT_ESCAPE_UPDATE_RESOURCE_WINE)` to stamp the runtime descriptor |
| `d3d12_resource_close_export_kmt` (:199) | | `D3DKMTDestroyAllocation` |
| `d3d12_device_open_resource_descriptor` (:210) | `device.c:7770` (`OpenSharedHandle`) | `D3DKMTQueryResourceInfo{,FromNtHandle}` + `D3DKMTOpenResource2` / `D3DKMTOpenResourceFromNtHandle` to *read* the undocumented D3D runtime private data, then `D3DKMTDestroyAllocation` |

Every one of them opens with:

```c
    if (!device->kmt_local)
    {
        /* D3DKMT API isn't supported */
        return;                 /* or E_NOTIMPL */
    }
```

so on a platform where `D3DKMTOpenAdapterFromLuid`/`D3DKMTCreateDevice` fails, the whole feature
silently vanishes.

### 6.3 The undocumented runtime descriptors

`vkd3d_d3dkmt.h:249-341` reverse-engineers the D3D runtime's *private runtime data* blob that
rides on a shared resource, with hard size asserts:

```c
struct d3dkmt_dxgi_desc  { UINT size; UINT version; UINT width; UINT height; DXGI_FORMAT format;
                           UINT unknown_0, unknown_1; UINT keyed_mutex;
                           D3DKMT_HANDLE mutex_handle, sync_handle;
                           UINT nt_shared; UINT unknown_2, unknown_3, unknown_4; };

struct d3dkmt_d3d9_desc  { struct d3dkmt_dxgi_desc dxgi; D3DFORMAT format; D3DRESOURCETYPE type; UINT usage; union {...}; };
C_ASSERT( sizeof(struct d3dkmt_d3d9_desc)  == 0x58 );

struct d3dkmt_d3d11_desc { struct d3dkmt_dxgi_desc dxgi; D3D11_RESOURCE_DIMENSION dimension;
                           union { D3D11_BUFFER_DESC d3d11_buf; D3D11_TEXTURE1D_DESC d3d11_1d;
                                   D3D11_TEXTURE2D_DESC d3d11_2d; D3D11_TEXTURE3D_DESC d3d11_3d; }; };
C_ASSERT( sizeof(struct d3dkmt_d3d11_desc) == 0x68 );

struct d3dkmt_d3d12_desc { struct d3dkmt_d3d11_desc d3d11; UINT unknown_5[4]; UINT resource_size;
                           UINT unknown_6[7]; UINT resource_align; UINT unknown_7[9];
                           union { D3D12_RESOURCE_DESC desc; D3D12_RESOURCE_DESC1 desc1; UINT __pad[16]; };
                           UINT64 unknown_8[1]; };
C_ASSERT( sizeof(struct d3dkmt_d3d12_desc) == 0x108 );
```

Discrimination rule, from the union comments at `:335-341` and the code at `d3dkmt.c:313-358`:

| blob | condition |
|---|---|
| D3D12 | `size == sizeof(d3d12) (0x108) && dxgi.size == sizeof(d3d11) (0x68) && (dxgi.version == 0 \|\| dxgi.version == 4)` |
| D3D11 | `size == sizeof(d3d11) (0x68) && dxgi.size == 0x68 && dxgi.version == 4` |
| D3D9  | `size == sizeof(d3d9) (0x58) && dxgi.size == 0x58 && dxgi.version == 1` |

Handle-kind discrimination: `(UINT_PTR)handle & 0xc0000000` selects the *global-share* (KMT)
path vs the NT-handle path (`d3dkmt.c:222`); separately `handle_is_kmt_style()` at
`device.c:7738-7741` uses `((ULONG_PTR)handle & 0x40000000) && ((ULONG_PTR)handle - 2) % 4 == 0`.

### 6.4 Graceful degradation to the DXVK path

`d3d12_device_CreateSharedHandle` (`device.c:~7590-7735`) tries D3DKMT **first** and falls back:

```c
        if (D3DKMTShareObjects(1, &resource->kmt_local, &attr, access, handle) == STATUS_SUCCESS)
        {
            ID3D12Resource_Release(resource_iface);
            return S_OK;
        }
        ...
        win32_handle_info.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT;
        vr = VK_CALL(vkGetMemoryWin32HandleKHR(device->vk_device, &win32_handle_info, handle));
        ...
                if (!vkd3d_set_shared_metadata(*handle, &metadata, sizeof(metadata)))
                    ERR("Failed to set metadata for shared resource, importing created handle will fail.\n");
```

`struct DxvkSharedTextureMetadata` is the DXVK-compatible sidecar. It is transported through
`libs/vkd3d/shared_metadata.c` (Windows-only, `libs/vkd3d/meson.build:111-113`), which is
**also Wine-specific**:

```c
/* shared_metadata.c:24-26 */
#define IOCTL_SHARED_GPU_RESOURCE_SET_METADATA  CTL_CODE(FILE_DEVICE_VIDEO, 4, METHOD_BUFFERED, FILE_WRITE_ACCESS)
#define IOCTL_SHARED_GPU_RESOURCE_GET_METADATA  CTL_CODE(FILE_DEVICE_VIDEO, 5, METHOD_BUFFERED, FILE_READ_ACCESS)
#define IOCTL_SHARED_GPU_RESOURCE_OPEN          CTL_CODE(FILE_DEVICE_VIDEO, 1, METHOD_BUFFERED, FILE_WRITE_ACCESS)
/* :56 */
    HANDLE nt_handle = CreateFileA("\\\\.\\SharedGpuResource", GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING, ...);
```

`\\.\SharedGpuResource` is Wine's shared-GPU-resource device. **On native Windows it does not
exist**, so `vkd3d_set_shared_metadata`/`vkd3d_get_shared_metadata` fail and the *fallback* fails
too.

### 6.5 R3's verdict for R6/R7

On **native Windows 11 with a real WDDM driver** (which is what Helios is), the picture is:

- `D3DKMTOpenAdapterFromLuid` + `D3DKMTCreateDevice` are real, documented, and should succeed
  against `kmd_render` → `device->kmt_local` gets set → the D3DKMT path activates.
- …but `D3DKMTEscape(Type = 0x80000000)` is a **Wine-only escape**. `kmd_render`'s
  `DxgkDdiEscape` would see an unrecognised type. Whether that is a benign refusal or a hard
  failure is a Helios decision, but note the return value of that `D3DKMTEscape` is **ignored**
  (`d3dkmt.c:195`), so a refusal is silent — the resource simply has no runtime descriptor, and a
  later `OpenSharedHandle` from another process returns `E_INVALIDARG` from
  `d3d12_device_open_resource_descriptor` (`d3dkmt.c:415-416`).
- The DXVK-metadata fallback is unavailable on native Windows.
- ⇒ **D3D12 cross-process/cross-API resource sharing under vkd3d-proton on native Windows is
  UNVERIFIED and, on this reading, likely broken by construction.** *Settling experiment:*
  build vkd3d-proton for Windows (§8), run
  `tools/d3d11_open_shared_probe.cpp`-shaped test but for D3D12 —
  `CreateCommittedResource(D3D12_HEAP_FLAG_SHARED)` → `CreateSharedHandle` → `OpenSharedHandle` in
  a second process — on the win11 VM against the Microsoft WARP driver *first* (to isolate
  Helios), then against Helios. Watch for the `ERR("Failed to set metadata for shared resource…")`
  line in the vkd3d log.
- Practical read: **this does not block a first D3D12 milestone.** Nothing in the single-process
  D3D12 path (device, queues, command lists, resources, present via DXVK DXGI) requires D3DKMT.
  `d3d12_device_open_kmt` failing is a `WARN`, not an error.

---

## 7. `swapchain.c` — how vkd3d presents (4,179 LOC)

### 7.1 It implements the DXVK interop interfaces, not DXGI

`swapchain.c` implements exactly two COM objects:

- `struct dxgi_vk_swap_chain_factory { IDXGIVkSwapChainFactory IDXGIVkSwapChainFactory_iface;
  struct d3d12_command_queue *queue; }` (`vkd3d_private.h:3796-3800`) — embedded **in the command
  queue** (`vkd3d_private.h:3869`).
- `struct dxgi_vk_swap_chain { IDXGIVkSwapChain2 IDXGIVkSwapChain_iface; … VkSurfaceKHR vk_surface; … }`
  (`swapchain.c:158-174`).

Discovery is by `QueryInterface` on the **command queue** (`command.c:22282-22287`):

```c
    if (IsEqualGUID(riid, &IID_IDXGIVkSwapChainFactory))
    {
        IDXGIVkSwapChainFactory_AddRef(&command_queue->vk_swap_chain_factory.IDXGIVkSwapChainFactory_iface);
        *object = &command_queue->vk_swap_chain_factory;
        return S_OK;
    }
```

The full QI set on the queue (`command.c:22253-22300`): `ID3D12CommandQueue`, `ID3D12Pageable`,
`ID3D12DeviceChild`, `ID3D12Object`, `IUnknown`, `ID3D12ExtDummyInterface`, `ID3D12CommandQueueExt`,
`IDXGIVkSwapChainFactory`, `ID3DDestructionNotifier`. **Anything else → `E_NOINTERFACE`.**

### 7.2 vkd3d never creates the surface

```c
/* swapchain.c:1536 */
    vr = IDXGIVkSurfaceFactory_CreateSurface(pFactory, vk_instance, vk_physical_device, &chain->vk_surface);
/* :1544 */
    vr = VK_CALL(vkGetPhysicalDeviceSurfaceSupportKHR(vk_physical_device,
            chain->queue->vkd3d_queue->vk_family_index, chain->vk_surface, &supported));
```

`IDXGIVkSurfaceFactory` (`include/vkd3d_swapchain_factory.idl:40-50`):

```
[ object, local, uuid(1e7895a1-1bc3-4f9c-a670-290a4bc9581a) ]
interface IDXGIVkSurfaceFactory : IUnknown {
    VkResult CreateSurface(VkInstance instance, VkPhysicalDevice adapter, VkSurfaceKHR *pSurface);
};
```

There is **no `vkCreateWin32SurfaceKHR` call anywhere in `libs/vkd3d`** — the only surface-related
Vulkan entry points used are `vkDestroySurfaceKHR` (`:556`),
`vkGetPhysicalDeviceSurfaceCapabilitiesKHR` (`:1022, :2115`),
`vkGetPhysicalDeviceSurfaceFormatsKHR` (`:1409, :1425`),
`vkGetPhysicalDeviceSurfacePresentModesKHR` (`:1815`), `vkCreateSwapchainKHR` (`:2248`).
It *does* request `VK_KHR_win32_surface` at instance level (`d3d12core/main.c:578`) so that the
caller's surface factory can use it.

### 7.3 The counterpart already exists in `dxvk-helios`

`dxvk-helios/src/dxgi/dxgi_interfaces.h:482`:

```cpp
__CRT_UUID_DECL(IDXGIVkSwapChainFactory,   0xe7d6c3ca,0x23a0,0x4e08,0x9f,0x2f,0xea,0x52,0x31,0xdf,0x66,0x33);
```

which is **byte-identical** to vkd3d's `uuid(e7d6c3ca-23a0-4e08-9f2f-ea5231df6633)`
(`include/vkd3d_swapchain_factory.idl:137-138`), as are the `IDXGIVkSurfaceFactory`
(`1e7895a1-…`, `dxgi_interfaces.h:59`) and `IDXGIVkSwapChain` (`e4a9059e-…`,
`dxgi_interfaces.h:75`) UUIDs.

And `dxvk-helios/src/dxgi/dxgi_factory.cpp:524-579` is the exact consumer:

```cpp
    Com<IDXGIVkSwapChainFactory> dxvkFactory;

    if (SUCCEEDED(pDevice->QueryInterface(IID_PPV_ARGS(&dxvkFactory)))) {
      Com<IDXGIVkSurfaceFactory> surfaceFactory = new DxgiSurfaceFactory(
        m_instance->vki()->getLoaderProc(), hWnd);

      Com<IDXGIVkSwapChain> presenter;
      HRESULT hr = dxvkFactory->CreateSwapChain(surfaceFactory.ptr(), &desc, &presenter);
      ...
      frontendSwapChain = new DxgiSwapChain(this, presenter.ptr(), hWnd, &desc, &fsDesc, pDevice);
    } else {
      Logger::err("DXGI: CreateSwapChainForHwnd: Unsupported device type");
      return DXGI_ERROR_UNSUPPORTED;
    }
```

Note the `else` branch: DXVK's DXGI **only** knows how to make a swapchain for a device that
exposes `IDXGIVkSwapChainFactory`. Symmetrically, Microsoft's `dxgi.dll` has no idea what a
vkd3d-proton `ID3D12CommandQueue` is.

### 7.4 The present path itself

- Present modes parsed from config (`swapchain.c:27-53`): `IMMEDIATE`, `MAILBOX`, `FIFO`,
  `FIFO_RELAXED`, `FIFO_LATEST_READY`.
- Pacing/backpressure uses `VK_KHR_present_wait` / `VK_KHR_present_id` (device.c:81-82) and,
  when present, `VK_KHR_present_wait2` / `VK_KHR_present_id2` (device.c:95-96) and
  `VK_EXT_present_timing` (device.c:97). Comment at `swapchain.c:56-63` explains why
  `FIFO_LATEST_READY` is excluded from present-wait pacing.
- Presentation runs on the queue's **submission thread**, entered via
  `VKD3D_SUBMISSION_QUEUE_USING_CALLBACK` (`command.c:25391-25396`), which first does
  `d3d12_command_queue_flush_waiters(queue, EXTERNAL|SERIALIZING)`.
- `dxgi_vk_swap_chain_Present` at `swapchain.c:1121`; `vkCreateSwapchainKHR` at `:2248`.
- Low-latency (`NV_low_latency2`) chains are registered on the device
  (`d3d12_device_register_swapchain`, `swapchain.c:3963`).

### 7.5 Consequence for the Helios D3D12 plan (hand to R7)

**Any D3D12-over-vkd3d plan for Helios requires shipping a `dxgi.dll` that implements
`IDXGIVkSwapChainFactory` consumption — i.e. DXVK's DXGI — or writing one.** Today `dxvk-helios`
is consumed as a *linked engine* inside `umd/`, not as a system `dxgi.dll`. That is a real,
named, previously-unrecorded piece of work.

Two options for the implementer, both grounded in the above:
1. **Build and ship `dxvk-helios`'s `dxgi.dll`** alongside vkd3d's `d3d12.dll`/`d3d12core.dll` for
   D3D12 apps. It already has all three interfaces at matching UUIDs. Cost: a second DXGI in the
   system for D3D12 apps only; D3D11 keeps going through the WDDM UMD.
2. **Implement `IDXGIVkSurfaceFactory` + a minimal DXGI shim** in Helios so vkd3d's swapchain lands
   on the same dcomp/present vehicle the D3D11 path uses. Cost: reimplementing what DXVK already
   has.

Either way, the present path lands on the **Vulkan client class** that ROADMAP already flags as
lacking a hardware present (DX12.md §"CONSEQUENCE").

---

## 8. Building on Windows

### 8.1 The three supported configurations, from CI

`.github/workflows/` contains exactly three workflows.

**(a) `artifacts.yml` — the shipping build.** `ubuntu-24.04`, `misyltoad/arch-mingw-github-action@v8`,
`./package-release.sh ${VERSION_NAME} build --no-package`. → mingw-w64 cross, x86 + x64.

**(b) `test-build-linux.yml`** — mingw x86, mingw x64, native GCC x86, native GCC x64.

**(c) `test-build-windows.yml` — a genuine native MSVC build, and it is CI-gated:**

```yaml
    runs-on: windows-2022
    - name: Setup widl and glslangValidator
      run: |
        choco install strawberryperl -y
        Invoke-WebRequest -Uri "https://raw.githubusercontent.com/HansKristian-Work/vkd3d-proton-ci/main/glslangValidator.exe" -OutFile "C:\Strawberry\c\bin\glslangValidator.exe"
        Write-Output "C:\Strawberry\c\bin" | Out-File -FilePath "${Env:GITHUB_PATH}" -Append
    - name: Setup Meson
      run: pip install meson
    - name: Build MSVC x64
      run: |
        ... VsDevCmd.bat -arch=x64 -host_arch=x64 ...
        meson setup -Denable_tests=True -Denable_extras=True --buildtype release --backend vs2022 build-msvc-x64
        msbuild -m build-msvc-x64/vkd3d-proton.sln
```

**So: MSVC x64 is supported, not merely tolerated.** The meson build carries MSVC-specific
handling throughout: `vkd3d_is_msvc = compiler.get_id() == 'msvc' or 'clang-cl'`
(`meson.build:9`); `/wd4244 /wd4101 /wd4267 /wd4996 /wd4334 /wd4146 /wd4305` in the warning list
(`meson.build:115-121`); `/NOIMPLIB` and `/NOEXP` link args (`meson.build:159-164`);
`vs_module_defs : 'd3d12.def'` for the MSVC path vs `objects : 'd3d12.def'` for mingw
(`libs/d3d12/meson.build:20,27-28`); and even a `VKD3D_CONFIG_FLAG_INIT_STATIC` macro whose comment
is *"MSVC is supremely dumb here."* (`include/private/config_flags.h:49-50`).

### 8.2 Dependency list for a native Windows build

| Dependency | Why | Where |
|---|---|---|
| **meson ≥ 0.49** | build system | `meson.build:2` |
| **MSVC (VS2022) or clang-cl**, or mingw-w64 ≥ 7.0 | compiler | CI + README:73 |
| **`widl`** (Wine IDL compiler; on Windows from **Strawberry Perl**) | compiles the 17 `.idl` in `include/` into headers | `meson.build:73-81`, README:69-70, `include/meson.build:1-19` |
| **`glslang` / `glslangValidator`** | compiles the 46 GLSL meta-shaders to embedded arrays with `--target-env vulkan1.3` | `meson.build:83-88` |
| **`dxil-spirv`** (git submodule at `subprojects/dxil-spirv`, C++) | *the* DXIL→SPIR-V compiler | `meson.build:177-178`, `.gitmodules` |
| **Vulkan-Headers** (submodule `khronos/Vulkan-Headers`) | headers only — no loader link; entry points come from `PFN_vkGetInstanceProcAddr` | `meson.build:62` |
| **SPIRV-Headers** (submodule `khronos/SPIRV-Headers`) | headers | `meson.build:62` |
| `gdi32`, `dxgi` import libs | Windows link | `meson.build:96-97` |
| `pthreads` (`dependency('threads')`) | vkd3d uses pthreads API throughout; on Windows this resolves via the win32 shim in `include/private/vkd3d_threads.h` | `meson.build:90` |

**Not** required: SPIRV-Tools (not referenced anywhere in the meson files), DXC.

Note `meson.build:73-77`: `widl` is looked up as `widl`, then `widl-stable`, then
`widl-mingw-tools-fallback` (the last defined by `build-win64.txt:6` /`build-widl.txt:1`).

Meson options (`meson_options.txt` / used at `meson.build:16-22`): `enable_tests`, `enable_extras`,
`enable_profiling`, `enable_renderdoc`, `enable_descriptor_qa`, `enable_extended_emulation`,
`enable_trace`. `enable_trace='auto'` follows buildtype; `enable_breadcrumbs` follows
`enable_trace` (`meson.build:57-60`).

### 8.3 A caution about header sourcing

vkd3d-proton does **not** use the Windows SDK's `d3d12.h`. It compiles its own IDLs —
`vkd3d_d3d12.idl`, `vkd3d_d3d12sdklayers.idl`, `vkd3d_dxgi*.idl`, `vkd3d_dxcapi.idl`,
`vkd3d_core_interface.idl`, `vkd3d_swapchain_factory.idl`, `vkd3d_{device,command_list,command_queue}_vkd3d_ext.idl`
(`include/meson.build:1-19`) — on top of its own `vkd3d_windows.h` / `vkd3d_win32.h` shims. Even
the MSVC build goes through `widl`. **Implications:** (i) the type layouts are vkd3d's own
transcription of the D3D12 ABI, not the SDK's, so any comparison against `tmp/dx12/sdk/d3d12.h`
must be done deliberately; (ii) `widl` must run on the win11 VM (Strawberry Perl ships it).

**UNVERIFIED:** whether a native MSVC build succeeds *on this project's win11 VM specifically*
(it has no clang-cl, per memory `[20TH]`; it does have VS build tools per TOOLCHAIN.md).
*Settling experiment:* after `git submodule update --init --recursive` in
`vkd3d-proton-helios`, install Strawberry Perl + glslangValidator + meson on win11, then
`win_exec` the two `meson setup --backend vs2022` + `msbuild` lines from
`test-build-windows.yml`, mirroring to a **local C: path** (never `Z:\`, per CLAUDE.md).

---

## 9. Vulkan requirements — hand this table to R12

### 9.1 API version: a hard, exact floor of Vulkan 1.3

```c
/* include/vkd3d.h:53-54 */
#define VKD3D_MIN_API_VERSION VK_API_VERSION_1_3
#define VKD3D_MAX_API_VERSION VK_API_VERSION_1_3
```

Enforced three times: loader version (`device.c:1455-1466`, and the loader version is *clamped* to
1.3 at `:1466`), physical-device filter (`device.c:3538-3542`, skips the device with a `WARN`), and
`d3d12core`'s own LUID matcher (`d3d12core/main.c:492-496`).

### 9.2 HARD-FAIL gates — device creation returns `E_INVALIDARG`

All from `vkd3d_init_device_caps` (`device.c:3243-3487`) unless noted:

| # | Requirement | Line | Error text |
|---|---|---|---|
| 1 | `vertexAttributeInstanceRateDivisor` **and** `vertexAttributeInstanceRateZeroDivisor` (`VK_EXT_vertex_attribute_divisor` / VK 1.4 core) | 3286-3292 | *"Lacking support for VK_EXT_vertex_attribute_divisor."* |
| 2 | `VkPhysicalDeviceTransformFeedbackPropertiesEXT::transformFeedbackQueries` (`VK_EXT_transform_feedback`) | 3294-3298 | *"Lacking support for transform feedback."* |
| 3 | single-texel alignment for **both** storage and uniform texel buffers (`storageTexelBufferOffsetSingleTexelAlignment` or `…AlignmentBytes == 1`, same for uniform) | 3300-3313 | *"Lacking support for single texel alignment."* |
| 4 | `VkPhysicalDeviceVulkan12Features::samplerMirrorClampToEdge` | 3426-3430 | *"samplerMirrorClampToEdge is not supported…"* |
| 5 | `VK_EXT_robustness2`: `robustBufferAccess2` **and** `robustImageAccess2` | 3432-3437 | *"Robustness2 features not supported. This is required."* |
| 6 | `VK_EXT_robustness2`: `nullDescriptor` | 3439-3443 | *"Null descriptor in VK_EXT_robustness2 is not supported…"* |
| 7 | `VkPhysicalDeviceVulkan11Features::shaderDrawParameters` | 3448-3452 | *"shaderDrawParameters is not supported…"* |
| 8 | `VK_KHR_push_descriptor` | 3454-3458 | *"Push descriptors are not supported…"* |
| 9 | `VK_KHR_maintenance5` **and** `VK_KHR_maintenance6` | 3460-3465 | *"maintenance5 and/or maintenance6 not supported…"* |

⚠ **The README is out of date.** `README.md:19-33` lists Vulkan 1.3, descriptor indexing with
≥1,000,000 UpdateAfterBind descriptors, `samplerMirrorClampToEdge`, `shaderDrawParameters`,
`VK_EXT_robustness2`, `VK_KHR_push_descriptor` — but **omits** requirements 1, 2, 3 and 9
(maintenance5/6, transform feedback, vertex attribute divisor, texel alignment). The code is
ground truth. Requirement 9 in particular is a recent tightening and is the one most likely to trip
a Mesa/venus stack.

⚠ The README's "1,000,000 UpdateAfterBind descriptors" claim is **not** enforced as a hard gate in
`vkd3d_init_device_caps`; the only `1000000` in `device.c` is `device.c:10118`
(`count = min(1000000, useable_size >> device->bindless_state.sampler_size_log2)` — a clamp, not a
check). **UNVERIFIED:** whether an ICD reporting a small `maxDescriptorSetUpdateAfterBind*` limit
fails later, or just fails to reach a feature level. *Settling experiment:* read
`vkd3d_bindless_state_init` in `state.c` (around `:7350-7460`) and `d3d12_device_caps_init` for
where descriptor limits gate `D3D12_RESOURCE_BINDING_TIER` / feature level.

### 9.3 Soft-degrade features (present ⇒ better path, absent ⇒ still works)

Notable ones from `optional_device_extensions[]` (`device.c:66-172`), which has **~95 entries**:

*Structural / performance-critical:*
`VK_EXT_descriptor_buffer` (:127) · `VK_EXT_mutable_descriptor_type` (:122) /
`VK_VALVE_mutable_descriptor_type` (:163) · `VK_EXT_descriptor_heap` (:141, behind
`VKD3D_CONFIG=descriptor_heap`) · `VK_KHR_maintenance7/8/9/10/11` (:83-87) ·
`VK_EXT_graphics_pipeline_library` (:130) + `VK_KHR_pipeline_library` (:69) ·
`VK_EXT_extended_dynamic_state2/3` (:115-116) · `VK_EXT_pageable_device_local_memory` (:133) +
`VK_EXT_memory_priority` (:134) · `VK_EXT_zero_initialize_device_memory` (:139) ·
`VK_KHR_dynamic_rendering_local_read` (:104) · `VK_KHR_unified_image_layouts` (:93).

*Feature-tier gates:*
`VK_KHR_ray_tracing_pipeline` / `acceleration_structure` / `deferred_host_operations` /
`ray_query` / `ray_tracing_maintenance1` / `opacity_micromap` (:70-74, :97 — all disabled by
`VKD3D_CONFIG=nodxr`) · `VK_EXT_mesh_shader` (:121) · `VK_KHR_fragment_shading_rate` (:76) ·
`VK_EXT_fragment_shader_interlock` (:132) · `VK_EXT_conservative_rasterization` (:108) ·
`VK_EXT_transform_feedback` (:113 — but see hard gate #2) · `VK_EXT_shader_stencil_export` (:112) ·
`VK_EXT_device_generated_commands` (:110) · `VK_EXT_shader_image_atomic_int64` (:120) ·
`VK_KHR_index_type_uint8` (:102) · `VK_EXT_image_view_min_lod` (:111 — README calls this
"should be supported").

*Windows/interop:* `VK_KHR_external_memory_win32` and `VK_KHR_external_semaphore_win32`
(`device.c:99-102`, `#ifdef _WIN32`) — **optional**.

*Present:* `VK_KHR_swapchain` is **required** at device-create time by `d3d12core`
(`d3d12core/main.c:659-662`); `VK_KHR_present_id`/`present_wait` (:80-82),
`VK_KHR_present_id2`/`present_wait2` (:95-96), `VK_EXT_present_timing` (:97),
`VK_KHR_present_mode_fifo_latest_ready` (:79),
`VK_{KHR,EXT}_swapchain_maintenance1` (`optional_extensions_user[]`, device.c:174-180) are optional.

*Instance:* required `VK_KHR_surface` + `VK_KHR_win32_surface`
(`d3d12core/main.c:574-580`); optional `VK_KHR_surface_maintenance1`, `VK_EXT_surface_maintenance1`,
`VK_KHR_get_surface_capabilities2` (`main.c:582-593`); optional `VK_EXT_debug_utils`
(`device.c:60-64`, gated on `VKD3D_CONFIG` `DEBUG_UTILS`/`FAULT`).

*Escape hatch:* `VKD3D_DISABLE_EXTENSIONS=<comma list>` disables any of them at runtime
(`device.c:186-194`).

### 9.4 The configuration knobs that matter for a virtualised GPU

70 flags in `include/private/config_flag_decl.h` (`grep -c VKD3D_DECL_CONFIG` → 70), set via
`VKD3D_CONFIG` (read at `device.c:1385`). The ones an implementer should reach for first:

| Flag | String | Effect |
|---|---|---|
| `SINGLE_QUEUE` | `single_queue` (:8) | collapse COMPUTE/TRANSFER onto GRAPHICS (`device.c:3843`) |
| `NO_DXR` | `nodxr` (:10) | drop all six raytracing extensions |
| `NO_UPLOAD_HVV` | `no_upload_hvv` (:13) | don't use host-visible VRAM for UPLOAD heaps |
| `FORCE_HOST_CACHED` | `force_host_cached` (:16) | force cached host memory |
| `DESCRIPTOR_HEAP` | `descriptor_heap` (:65) | opt into `VK_EXT_descriptor_heap` |

Other useful env vars: `VKD3D_QUEUE_PROFILE=<path>` (Chrome trace, §4.1),
`VKD3D_FILTER_DEVICE_NAME=<substr>` (`device.c:3505`), `VKD3D_DISABLE_EXTENSIONS`,
`DXIL_SPIRV_CONFIG` (`device.c:11289-11294`).

---

## 10. The Helios "fork" — exact status

Commands run and their output:

```
$ git submodule status
 2c7ba22c53261458a7a204c55f3098ad9855cb15 vkd3d-proton-helios (vkd3d-1.1-5456-g2c7ba22c)

$ cd vkd3d-proton-helios && git rev-parse HEAD origin/master
2c7ba22c53261458a7a204c55f3098ad9855cb15
2c7ba22c53261458a7a204c55f3098ad9855cb15

$ git log --oneline origin/master..HEAD        # (empty — zero local commits)
$ git status --porcelain                       # (empty — clean tree)
$ git describe --tags
v3.0.1-254-g2c7ba22c

$ git remote -v
origin  git@github-rupansh:HansKristian-Work/vkd3d-proton (fetch)
origin  git@github-rupansh:HansKristian-Work/vkd3d-proton (push)
```

vs. the superproject's declaration:

```
$ cat .gitmodules      # (excerpt)
[submodule "vkd3d-proton-helios"]
	path = vkd3d-proton-helios
	url = https://github.com/rupansh/vkd3d-proton
```

**Findings:**

1. **Zero divergence.** `HEAD == origin/master`, no local commits, clean worktree.
   DX12.md §1.3 is fully confirmed.
2. **Remote mismatch confirmed.** `.gitmodules` names `rupansh/vkd3d-proton`; the checkout's
   `origin` is `HansKristian-Work/vkd3d-proton` via the `github-rupansh` SSH host alias. Any Helios
   change needs the fork wired as a push remote first. (DX12.md already flags this.)
3. **NEW — the nested submodules are not checked out:**

   ```
   $ cd vkd3d-proton-helios && git submodule status
   -f88a2d766840fc825af1fc065977953ba1fa4a91 khronos/SPIRV-Headers
   -0e9de566b7d4051c5cc1b762e242c46565956bdf khronos/Vulkan-Headers
   -cc75a0c98d34d7bcc03560527c799b52e48b4d1f subprojects/dxil-spirv
   ```

   The leading `-` means *uninitialised*; `ls` shows all three directories empty. **The tree cannot
   be configured, let alone built, until `git submodule update --init --recursive` is run inside
   `vkd3d-proton-helios`.** This is the very first step of any D3D12 build task and it is not
   recorded anywhere in the repo.
   Pinned commits: dxil-spirv `cc75a0c98d34d7bcc03560527c799b52e48b4d1f`, Vulkan-Headers
   `0e9de566b7d4051c5cc1b762e242c46565956bdf`, SPIRV-Headers `f88a2d766840fc825af1fc065977953ba1fa4a91`
   (`.gitmodules` inside the submodule names their upstreams:
   `HansKristian-Work/dxil-spirv`, `KhronosGroup/Vulkan-Headers`, `KhronosGroup/SPIRV-Headers`).
4. **Nothing in the Helios tree builds it.** Full grep:

   ```
   $ grep -rn "vkd3d" --include=*.rs --include=*.toml --include=*.sh --include=*.ps1 \
       --include=*.yml --include=*.inx tools/ ci/ .github/ kmd_render/ umd/ packaging/ dxvk-helios/src
   tools/win-mcp/src/main.rs:576: … /XD target .git "{MESA_SRC}" dxvk dxvk-research-only vkd3d-proton virtio-research-only-3d …
   tools/win-mcp/src/main.rs:843: … (identical)
   ```

   Two hits, both the robocopy exclusion list, both naming a bare `vkd3d-proton` — a directory that
   no longer exists (the tree has `vkd3d-proton-helios`).
   **UNVERIFIED:** whether robocopy `/XD vkd3d-proton` also excludes `vkd3d-proton-helios`
   (robocopy's `/XD` name matching is documented ambiguously and is substring-ish for path forms).
   This decides whether `win_cargo`/`win_exec` mirroring copies ~118k LOC + submodules to the VM or
   skips them. *Settling experiment:* run the mirror once via `win_exec` and
   `Test-Path C:\Users\Rupansh\helios-vgpu\vkd3d-proton-helios\meson.build`.
5. **Why it was forked: still unrecorded.** Confirmed — no rationale anywhere. Do not invent one.

---

## 11. Load-bearing facts other lanes must not contradict

1. `vkd3d-proton-helios` is upstream `2c7ba22c`, zero divergence, **and its own three submodules are
   uninitialised**.
2. `libs/vkd3d` is a **static** library. The only shared objects are `d3d12.dll` (8 exports, all
   forwarders) and `d3d12core.dll` (`D3D12GetInterface` + the `D3D12SDKVersion` **data** export).
3. **vkd3d-proton implements no DXGI and creates no `VkSurfaceKHR`.** It exposes
   `IDXGIVkSwapChainFactory` (uuid `e7d6c3ca-23a0-4e08-9f2f-ea5231df6633`) from
   `ID3D12CommandQueue::QueryInterface` and consumes `IDXGIVkSurfaceFactory`
   (uuid `1e7895a1-1bc3-4f9c-a670-290a4bc9581a`). `dxvk-helios` already declares both UUIDs
   identically and `DxgiFactory::CreateSwapChainBase` is the matching consumer.
4. D3D12 GPU virtual addresses **are** `vkGetBufferDeviceAddress` results. `va_map.c` is a reverse
   lookup (2 MiB-granular radix tree + a sorted small-entry array), not an address allocator.
5. Hard Vulkan floor: **exactly 1.3**, plus the nine hard-fail gates in §9.2 — including
   `maintenance5` + `maintenance6`, which the README omits.
6. `d3dkmt.c` is Wine-oriented (`D3DKMT_ESCAPE_UPDATE_RESOURCE_WINE = 0x80000000`) and every path
   in it fails soft. `shared_metadata.c`'s fallback opens `\\.\SharedGpuResource`, a Wine device.
7. A **native MSVC x64 build is CI-supported** (`test-build-windows.yml`, meson `--backend vs2022`
   + msbuild), needing `widl` (Strawberry Perl) + `glslangValidator` + meson.
8. Only **DXIL** is translated; DXBC bytecode is not (`dxbc.c` handles container, signatures, and
   root-signature blobs only — `vkd3d_shader_compile_dxbc` unconditionally calls
   `vkd3d_shader_compile_dxil`, `vkd3d_shader_main.c:196-217`). The translator itself is the
   external `dxil-spirv` C++ subproject.
9. `queue_timeline.c` is a **Chrome-trace profiler** gated on `VKD3D_QUEUE_PROFILE`, not fence
   machinery. The fence machinery is `d3d12_fence` (virtual-value fixup) and `d3d12_shared_fence`
   (1:1 timeline semaphore + own waiter thread) in `command.c`/`vkd3d_private.h:622-770`.
10. `ID3D12CommandQueue::ExecuteCommandLists` does not submit; it enqueues onto a **per-queue POSIX
    worker thread** named `vkd3d_queue`.

---

## 12. UNVERIFIED / open questions (each with its settling experiment)

| # | Question | Settling experiment |
|---|---|---|
| 1 | Does the Agility-SDK loader path on Windows 11 24H2 actually pick up vkd3d's `d3d12core.dll`? | Drop both DLLs next to a `dx-samples-research-only/Samples/Desktop/D3D12HelloWorld` binary on win11; check loaded modules + vkd3d TRACE output. |
| 2 | Does `VkPhysicalDeviceIDProperties::deviceLUID` from the Helios venus ICD byte-match `DXGI_ADAPTER_DESC::AdapterLuid`? (If not, `d3d12_find_physical_device` falls to `physical_devices[0]`, `d3d12core/main.c:558-560`.) | `vulkaninfo` on the guest → read `deviceLUID`/`deviceLUIDValid`; compare with `tools/`'s DXGI adapter probes. Memory `[30TH]` says yes for the WDDM adapter LUID — re-verify against *this* code path. |
| 3 | Which descriptor backend does the venus ICD land vkd3d in — descriptor buffer, mutable-embedded, or legacy sets? And what are `cbv_srv_uav_size`/`sampler_size`? | Run a vkd3d D3D12 app with `VKD3D_DEBUG=info` and read the bindless-state log, or breakpoint `d3d12_device_replace_vtable` (`device.c:11302`). Determines whether any fast path applies at all. |
| 4 | Does D3D12 shared-resource creation/opening work on native Windows at all, given `\\.\SharedGpuResource` does not exist and `D3DKMT_ESCAPE_UPDATE_RESOURCE_WINE` is Wine-only? | Two-process `CreateSharedHandle`/`OpenSharedHandle` probe under vkd3d, first on WARP (isolate Helios), then on Helios. Watch for `ERR("Failed to set metadata for shared resource…")`. |
| 5 | Does the win-mcp mirror's `/XD vkd3d-proton` exclusion also exclude `vkd3d-proton-helios`? | Run the mirror; `Test-Path C:\Users\Rupansh\helios-vgpu\vkd3d-proton-helios\meson.build`. |
| 6 | Does a native MSVC x64 build succeed on *this* win11 VM (Strawberry Perl widl + glslangValidator + meson + VS2022)? | Init submodules, install the three tools, run the two CI lines against a **local C:** build dir. |
| 7 | Is the README's "1,000,000 UpdateAfterBind descriptors" a hard gate or only a feature-tier gate? | Read `vkd3d_bindless_state_init` (`state.c:~7350-7460`) and `d3d12_device_caps_init` for where descriptor limits feed `D3D12_RESOURCE_BINDING_TIER`. |
| 8 | Steady-state memory footprint of the `vkd3d_va_tree` (32 MiB/node, never shrinks) under a real title on a 64-bit VA space. | Counter on the `vkd3d_calloc` at `va_map.c:65`, or watch working set with a D3D12 sample. |
| 9 | Can vkd3d run at all with whatever queue-family topology venus exposes, and does `VKD3D_CONFIG=single_queue` change stability? | A/B a D3D12 sample with and without the flag once anything runs. |

---

## 13. Direct implications for the Helios D3D12 plan

1. **First action is mechanical and unrecorded: `git submodule update --init --recursive` inside
   `vkd3d-proton-helios`.** Nothing else in this lane can proceed without it (§10.3).
2. **Strategy (b) — ship vkd3d-proton as `d3d12.dll`/`d3d12core.dll` — is strongly favoured by the
   code.** Strategy (a) (drive the translation core from a `d3d12umddi` frontend) is *possible* but
   needs: de-`static`ing ~200 methods or building a parallel ops table, reimplementing two
   vtable-swap specialisation mechanisms, untangling dual refcounts, rewriting `bundle.c`, and
   deleting the queue's own submission thread — against the project's most-churned 26.5k-LOC file.
   §2.4 names the five surgeries; hand that list to whoever owns the decision.
3. **A DXGI is required, and Helios does not currently ship one.** vkd3d cannot present without a
   caller that implements `IDXGIVkSurfaceFactory` and consumes `IDXGIVkSwapChainFactory`. That
   caller is DXVK's `dxgi.dll`. `dxvk-helios` has the interfaces at matching UUIDs but Helios
   consumes DXVK as a *linked engine*, not as a system DXGI. **This is a new, named work item.**
   (R7 owns the design; R3's contribution is the exact interface contract in §7.)
4. **Hand §9.2's nine hard gates to R12 verbatim.** The gates most at risk on Mesa/venus are
   `VK_KHR_maintenance5`+`maintenance6`, `VK_EXT_robustness2` (`robustBufferAccess2`,
   `robustImageAccess2`, `nullDescriptor`), `transformFeedbackQueries`, and the texel-alignment
   properties. Any one missing ⇒ `D3D12CreateDevice` returns `E_INVALIDARG` with a specific
   `ERR()` string, which makes the failure mode cheap to diagnose. And the required Vulkan version
   is **exactly 1.3** — a 1.4 ICD is fine (it is clamped), a 1.2 ICD is not.
5. **Adopt `VKD3D_QUEUE_PROFILE` on day one.** It is a zero-cost, already-written Chrome-trace
   instrument covering submit/wait/signal/present-wait/present-block/PSO-compile, which is exactly
   the evidence class ROADMAP WS2 needed. Pair it with the existing ETW `Microsoft-Windows-DxgKrnl`
   recipe to see both sides of the same frame.
6. **Reach for `VKD3D_CONFIG=single_queue` and `nodxr` early.** They are the two knobs most likely
   to turn "does not run" into "runs slowly" on a virtualised GPU, and both are single env-var A/Bs.
7. **Do not plan on D3D12 resource sharing for the first milestone.** §6.5: it needs either a
   Wine-shaped `\\.\SharedGpuResource` or Helios honouring a Wine-private escape. Single-process
   D3D12 needs none of it, and `d3d12_device_open_kmt` failing is a `WARN`.
8. **The conformance asset is already in the tree.** 40 test files / ~152k LOC in
   `vkd3d-proton-helios/tests/` (`d3d12.c`, `d3d12_bindless.c`, `d3d12_descriptors.c`,
   `d3d12_sparse.c`, `d3d12_raytracing.c`, `d3d12_enhanced_barriers.c`, …), buildable with
   `-Denable_tests=True` in the same MSVC configuration CI uses. Hand this to R9 — it is a far
   larger and better-maintained D3D12 suite than anything Helios would write, and it runs against
   *any* D3D12 implementation, so it can be baselined on WARP first.
9. **`demos/triangle.c` + `demos/gears.c` (`-Denable_extras=True`) are the cheapest first-light
   targets** — smaller than the DirectX-Graphics-Samples and already wired to vkd3d's own Win32
   shim (`demos/demo_win32.h`).
