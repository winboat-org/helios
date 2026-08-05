# SUBSTRATE.md — vkd3d-proton and the Vulkan substrate

**What this is:** everything a future session needs to build vkd3d-proton, point it at Helios, know
exactly what the engine demands of the Vulkan layer, know exactly what Helios supplies, and know
which gaps are real work. It is the reference behind `DECISIONS.md` **D6** ("the substrate is
solved — stop re-litigating it") and behind `DX12.md` §2 item 1 and §5. (⚠ `DX12.md` §2 is a flat
numbered list — "What the research settled, in five lines", `DX12.md:65-89` — with no subsections;
there is no §2.1.)

**What this is not:** the presentation story (`PRESENT.md` owns P-A/P-B/P-C and the DXVK-DXGI
requirement), the DDI contract (`DDI_REFERENCE.md`), the UMD split (`ARCHITECTURE.md`), or the gate
definitions (`GATES.md`). Where this document touches those it says so and stops.

**Conventions.** `file:line` citations are into the tree as it stands at
`vkd3d-proton-helios` = `2c7ba22c53261458a7a204c55f3098ad9855cb15` (verified this session,
`git rev-parse HEAD`). ⚠ = hazard, ⛔ = prohibition. Anything unproven is marked **UNVERIFIED**
with the experiment that settles it; §14 collects them.

**Evidence classes**, used inline where it matters: `[VKD3D]` read in vkd3d's source ·
`[MESA]` read in `icd/mesa` · `[VIRGL]` read in **virglrenderer 1.3.0 source** (see below) ·
`[CAPTURE]` read in the committed guest capture `docs/dx12/research/guest-vulkaninfo-full.txt` ·
`[LIVE]` measured on the running VM / Linux host **this session** · `[INFER]` my inference.

⚠ **`[VIRGL]` needs a source checkout, and there is none in this tree.** The host runs the packaged
binary — `virglrenderer 1.3.0-2` (`pacman -Qi virglrenderer`), mapped into QEMU as
`/usr/lib/libvirglrenderer.so.1.11.0` — and a `.so` cannot be cited by `file:line`. Every `[VIRGL]`
`file:line` in this document (§4.1's `vkr_extension_table`, §6.1's `vkr_queue.c`) is a read of the
**matching upstream source tarball**, which must be re-fetched before those citations resolve:

```bash
# Arch's virglrenderer 1.3.0-2 is built from the GitLab archive of tag `virglrenderer-1.3.0`.
curl -L -o /tmp/virgl.tar.gz \
  https://gitlab.freedesktop.org/virgl/virglrenderer/-/archive/virglrenderer-1.3.0/virglrenderer-virglrenderer-1.3.0.tar.gz
mkdir -p /home/rupansh/helios-vgpu/tmp/dx12/virgl
tar -C /home/rupansh/helios-vgpu/tmp/dx12/virgl -xf /tmp/virgl.tar.gz
# → tmp/dx12/virgl/virglrenderer-virglrenderer-1.3.0/src/venus/
# Fallback if the archive URL moves:
#   git clone --depth 1 --branch virglrenderer-1.3.0 \
#     https://gitlab.freedesktop.org/virgl/virglrenderer.git
```

⚠ **Pin-check before trusting a `[VIRGL]` citation** — the checkout must be the *same build* as the
installed `.so`, or every line number below is fiction. Two cheap equalities, both hold today:

```bash
sed -n '26p' tmp/dx12/virgl/virglrenderer-virglrenderer-1.3.0/meson.build      # version: '1.3.0',
strings -a /usr/lib/libvirglrenderer.so.1 | grep -c '^VK_'                     # 185
grep -c '{ *"VK_' tmp/dx12/virgl/virglrenderer-virglrenderer-1.3.0/src/venus/venus-protocol/vn_protocol_renderer_info.h
                                                                               # 185 — must match
```

(As of this session the checkout lived at the volatile path `/tmp/virglrenderer-virglrenderer-1.3.0`;
do not rely on it surviving a reboot. S4/S5-S12 all mean *editing this tree*, so getting it back is a
prerequisite, not a nicety.)

---

> ## ⚠ Scope note — `DECISIONS.md` D2 changed, 2026-08-05
>
> **There is no app-facing vkd3d arm** (owner directive). Helios never ships or measures vkd3d's
> `d3d12.dll`/`d3d12core.dll` as an application's D3D12; vkd3d is an **engine linked behind
> `helios_umd12.dll`**, reached through the two Helios entry points — ⛔ *since 2026-08-05 linked as
> **static archives** (`helios_d3d12_static`), not exported from a DLL; `DECISIONS.md` D4* (D4). What that
> changes in this document:
>
> - **⛔ DXVK's `dxgi.dll` is not part of any deliverable.** Anywhere this file discusses shipping it
>   alongside vkd3d, or an "(ii) app-local" deliverable shape, that is background — the shipping
>   answer is shape (i), the UMD.
> - **`libs/d3d12core/main.c`'s adapter resolution, the Agility-SDK `D3D12SDKVersion` mechanism, and
>   the app-local placement rules are all bypassed** by D4's direct `vkd3d_create_device` entry.
>   They are retained here because they explain *why* the bypass exists.
> - **Everything else is unaffected and is the load-bearing content:** the Vulkan version floor and
>   the nine hard device-creation gates (§3), the three-layer coverage table and the measured
>   `VP_D3D12_FL_12_2_baseline` result (§4, §9), the gap list (§5), sparse and raytracing tiers
>   (§6), the `driverID`/shader-model question (§7), and the build recipe (§8).
> - **The two substrate work items keep their ids `V1` / `V2`** (`VK_KHR_external_memory_win32`
>   absent-and-unguarded; no 32-bit venus ICD). ⚠ `V1` gets *more* important under D2, not less:
>   `D3D12_HEAP_FLAG_SHARED` is reached through the UMD like everything else, and vkd3d calls
>   `vkGetMemoryWin32HandleKHR` unguarded — a NULL function pointer, not a graceful degrade.

## 1. Verdict

**The live Helios guest satisfies `VP_D3D12_FL_12_2_baseline` in full — zero feature misses, zero
extension misses, zero property misses.** All nine of vkd3d's hard device-creation gates pass.
Nothing in the Vulkan substrate blocks `D3D12CreateDevice`. (`DECISIONS.md` D6.)

### 1.1 The measurement, re-run and independently reproduced

R12 made this claim; I re-ran it from scratch this session against the profile document vkd3d ships
(`vkd3d-proton-helios/VP_D3D12_VKD3D_PROTON_profile.json`, 9 profiles / 21 capability sets) and the
guest Vulkan-Profiles capture, and got the same numbers.

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
| `shader_model_67` | 1 (`maintenance8`) | 1 (`VK_KHR_maintenance8`) — profile-only, see §3.4 |
| `optimal_performance` | 7 | 7 |
| `fl_11_0_properties` / `fl_12_0_properties` / `fl_12_2_properties` | — | **0 property misses** |
| `subgroups_60` / `subgroups_66` | — | **0 property misses** |

`optimal_performance` misses, exactly: features `descriptorBuffer`, `descriptorBufferPushDescriptors`,
`shaderModuleIdentifier`, `presentId`, `presentWait`, `maintenance9`, `maintenance10`; extensions
`VK_EXT_descriptor_buffer`, `VK_EXT_shader_module_identifier`, `VK_KHR_present_id`,
`VK_KHR_present_wait`, `VK_AMD_buffer_marker`, `VK_KHR_maintenance9`, `VK_KHR_maintenance10`.

Which cap sets each profile composes (from the profile file's own `capabilities` lists):

```
VP_D3D12_FL_12_2_baseline = baseline_features + fl_11_1_features + subgroups_60
                          + fl_12_0_features + fl_12_1_features + fl_12_1_features_rov
                          + fl_12_2_features + fl_12_2_properties + shader_model_60
VP_D3D12_FL_12_2_optimal  = the above + subgroups_66 + shader_model_66 + optimal_performance
```

⚠ **One correction to R12 §9.** R12 says "properties spot-checked separately (all pass)". That is
true of every property set any profile actually uses, but **`fl_12_2_optimal_properties` has two
misses** — `shaderDenormFlushToZeroFloat32` and `shaderDenormPreserveFloat32`, both `false` on the
guest. Those are precisely the H5 denorm bits of §7, and the profile author put them in an
*optimal* set. It costs nothing: `fl_12_2_optimal_properties` is referenced by **no profile at all**
(`VP_D3D12_FL_12_2_optimal` uses `fl_12_2_properties`), so the FL 12.2 verdict is unaffected. It is
worth knowing because it is the profile independently flagging the same bit §7 hinges on.

### 1.2 Re-measuring it: the exact commands

**A. Is the VM up?** (Linux host — if this prints nothing every `[LIVE]` number is stale, and
⛔ relaunching the VM is owner-gated per CLAUDE.md.)

```bash
pgrep -af qemu-system-x86_64
```

**B. Capture the guest** (`win` MCP `win_exec`; `vulkaninfo` is session-0 safe, no window needed):

```powershell
New-Item -ItemType Directory -Force -Path Z:\tmp\dx12\research\capture | Out-Null
& vulkaninfo --summary 2>&1 | Out-File -Encoding utf8 Z:\tmp\dx12\research\capture\vulkaninfo-summary.txt
& vulkaninfo --json=0 -o Z:\tmp\dx12\research\capture\vulkaninfo.json
& vulkaninfo 2>$null   | Out-File -Encoding utf8 Z:\tmp\dx12\research\capture\vulkaninfo-full.txt
```

`--json=0` emits the **Vulkan Profiles document** for physical device 0 — the only form that can be
diffed against `VP_D3D12_VKD3D_PROTON_profile.json` mechanically. `Z:\` is the repo, so the files
are readable from Linux immediately.

**C. Diff it against vkd3d's profile** (Linux host). This is the script I ran; it reproduces the
table in §1.1 in about a second:

```bash
cd /home/rupansh/helios-vgpu && python3 - <<'EOF'
import json
guest = json.load(open('tmp/dx12/research/capture/vulkaninfo.json'))['capabilities']['device']
prof  = json.load(open('vkd3d-proton-helios/VP_D3D12_VKD3D_PROTON_profile.json'))
gext  = set(guest.get('extensions', {}))
flat  = {}
for s, members in guest.get('features', {}).items():
    for m, v in members.items():
        flat.setdefault(m, set()).add(bool(v))
for name in ('baseline_features','fl_11_1_features','fl_12_0_features','fl_12_1_features',
             'fl_12_1_features_rov','fl_12_2_features','shader_model_60','shader_model_66',
             'shader_model_67','optimal_performance'):
    c = prof['capabilities'].get(name, {})
    me = sorted(e for e in c.get('extensions', {}) if e not in gext)
    mf = [f'{s}.{m}' for s, mem in c.get('features', {}).items() for m, v in mem.items()
          if v is True and True not in flat.get(m, set())]
    print(f'{name:24s} feat_miss={len(mf):2d} ext_miss={len(me):2d}  {mf or ""} {me or ""}')
EOF
```

Property sets need the same shape with `guest['properties']` and `c['properties']`; the only
non-boolean comparisons that matter are the `maxPerStageDescriptorUpdateAfterBind*` minimums
(`>= 1000000`) and `bufferImageGranularity` (`<= 65536`) — both pass by a wide margin (§4.2).

**D. Once vkd3d actually runs, the one-line version.** `VKD3D_DEBUG=info` makes the engine print
its own conclusions:

| Line printed | Site | Level | ⚠ |
|---|---|---|---|
| `"Enabling support for SM 6.2."` | `libs/vkd3d/device.c:10709` | **`TRACE`** | ⚠ **not printed at `info`** — needs `VKD3D_DEBUG=trace` |
| `"Enabling support for SM 6.6."` | `:10766` | `INFO` | the practical §7 answer |
| `"Enabling support for SM 6.7."` | `:10801` | `INFO` | |
| `"DXR support enabled."` | `:9953` | `INFO` | tier 1.0 reached |
| `"DXR 1.1 support enabled."` | `:9969` | `INFO` | tier 1.1 reached |

Their presence or absence answers §6 and §7 without any parsing. ⚠ Because 6.2 is `TRACE`, the
*visible* signal at `info` is 6.6: if `"Enabling support for SM 6.6."` appears, the §7.1 swizzle
fired (6.6 is unreachable without 6.2). If it does not, run at `trace` to distinguish "6.2 failed"
from "6.6's own gate failed".

### 1.3 Guest identity, this capture

```
Vulkan Instance Version: 1.4.350
GPU0:
    apiVersion         = 1.4.341        (vkd3d needs >= 1.3 and clamps DOWN to 1.3 — §3.1)
    driverVersion      = 26.1.99
    vendorID           = 0x10de   deviceID = 0x2bb1   deviceType = DISCRETE_GPU
    deviceName         = Virtio-GPU Venus (NVIDIA RTX PRO 6000 Blackwell Workstation Edition)
    driverID           = DRIVER_ID_MESA_VENUS          ← the whole of §7 hinges on this
    driverName         = venus
    driverInfo         = Mesa 26.2.0-devel (git-f023e5ce48)
    deviceLUID         = 09760000-00000000
    deviceLUIDValid    = true                          ← §10 depends on this
```

(`docs/dx12/research/guest-vulkaninfo-full.txt:273-278`, `:668-672`, `:711-713`.) 168 device
extensions, 6 queue families, 2 memory heaps (95.59 GiB DEVICE_LOCAL + 70.31 GiB host,
`:1150-1156`).

---

## 2. vkd3d-proton anatomy

Enough of a map that the reader can find anything without grepping 118k lines blind.

### 2.1 Modules, LOC, dependency direction

| Module | LOC | What it is |
|---|---|---|
| `libs/vkd3d-common` | 1,458 | platform shims: `debug.c`, `platform.c`, `profiling.c`, `file_utils.c`, `utf8.c`, `string.c`, `memory.c`. No D3D12, no Vulkan. |
| `libs/vkd3d-shader` | 5,891 | `dxil.c` (glue to the external dxil-spirv compiler), `dxbc.c` (DXBC *container* + signatures + root-signature blobs **only**), `vkd3d_shader_main.c`, `checksum.c`, **and `3rdparty/md5/` (`md5.c` 291 L + `md5.h` 43 L + `README.md`) — vendored, in-tree today, compiled in unconditionally (`libs/vkd3d-shader/meson.build:6`)**. 5,891 includes those 334 lines; the vkd3d-authored figure is 5,557. See §12.2 — this is the one vendored component that can be licence-checked *without* a submodule init. |
| `libs/vkd3d` | ~101,000 | the translation core. `command.c` 26,534 · `device.c` 12,144 · `resource.c` 11,388 · `state.c` 8,444 · `vkd3d_private.h` 7,163 · `swapchain.c` 4,179 · `cache.c` 3,555 · `workgraphs.c` 3,403 · `raytracing_pipeline.c` 2,888 · `meta.c` 2,461 · `memory.c` 2,276 · `bundle.c` 1,929 · `queue_timeline.c` 718 · `va_map.c` 489 · `d3dkmt.c` 449 · `heap.c` 422. |
| `libs/d3d12core` | 1,537 | `main.c` (1,355) — the loadable `d3d12core.dll`. |
| `libs/d3d12` | 341 | `main.c` only — the thin `d3d12.dll` forwarder. |
| `include/` | 15,321 | public headers + **17 `.idl`** compiled by `widl`. |
| `tests/` | ~152,000 across 40 `.c` | the D3D12 conformance suite (`GATES.md` / `R9` own it). |
| `demos/` | 2,870 | `triangle.c`, `gears.c` + Win32/XCB shims — the cheapest first-light targets. |

```
vkd3d-common  (static, no deps)
      ▲
      ├── vkd3d-shader (static; deps vkd3d_common_dep, dxil_spirv_dep)   libs/vkd3d-shader/meson.build:9-11
      │        ▲
      └────────┴── vkd3d (static; deps vkd3d_common_dep, vkd3d_shader_dep)  libs/vkd3d/meson.build:119-121
                       ▲
                       └── d3d12core.dll (shared; + gdi32, dxgi)   libs/d3d12core/meson.build:16-22
                                ▲  (dlopen'd BY NAME at runtime, NOT linked)
                                └── d3d12.dll (shared; + gdi32, dxgi)  libs/d3d12/meson.build:22-28
```

⚠ **`libs/vkd3d` is a static library. There is no `libvkd3d.dll`.** The only shared objects on
Windows are `d3d12.dll` and `d3d12core.dll`. This is what `DECISIONS.md` D4 is reacting to: reaching
the engine "across a DLL boundary" means adding an export to vkd3d's `d3d12core` target, not linking
a pre-existing engine DLL.

### 2.2 What `d3d12.dll` exports

`libs/d3d12/d3d12.def`, verbatim:

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

The ordinals `@101`/`@102` match native `d3d12.dll`. Every export is a one-line forward into
`d3d12core.dll` through the private core interface, e.g. `libs/d3d12/main.c:143-152`:

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
(`libs/d3d12/main.c:217-314`), because — comment at `main.c:322` — *"The vtable for this must live in
d3d12.dll. d3d12core.dll should not be loaded yet."* `SetSDKVersion` is a `FIXME` returning `S_OK`
(`main.c:267-273`) and `D3D12EnableExperimentalFeatures` returns `E_NOINTERFACE`
(`d3d12core/main.c:807-814`).

### 2.3 What `d3d12core.dll` exports

`libs/d3d12core/d3d12core.def`, verbatim:

```
LIBRARY d3d12core.dll

EXPORTS
    D3D12GetInterface
    D3D12SDKVersion DATA PRIVATE
```

`D3D12SDKVersion` is a **data** export (`libs/d3d12core/main.c:1353-1355`):

```c
/* Just expose the latest stable AgilitySDK version.
 * This is actually exported as a UINT and not a function it seems. */
DLLEXPORT const UINT D3D12SDKVersion = D3D12_SDK_VERSION;
```

`D3D12GetInterface` (`main.c:1300-1351`) recognises exactly three CLSIDs: `CLSID_D3D12DeviceFactory`
→ a fresh `ID3D12DeviceFactory`/`ID3D12DeviceConfiguration1`; `CLSID_VKD3DCore` → the singleton
`IVKD3DCoreInterface`; `CLSID_VKD3DDebugControl` → the singleton `IVKD3DDebugControlInterface`.

### 2.4 The private `IVKD3DCoreInterface`

An 8-method vtable (`libs/d3d12core/main.c:828-838`): `CreateDevice`,
`CreateRootSignatureDeserializer`, `SerializeRootSignature`,
`CreateVersionedRootSignatureDeserializer`, `SerializeVersionedRootSignature`, `GetDebugInterface`,
`EnableExperimentalFeatures`, `GetInterface`. This is the handshake `load_d3d12core()`
(`libs/d3d12/main.c:66-141`) performs: `vkd3d_dlopen("d3d12core.dll")` →
`vkd3d_dlsym("D3D12GetInterface")` → `D3D12GetInterface(&CLSID_VKD3DCore, &IID_IVKD3DCoreInterface,
&core)`. The comment at `main.c:74-76` explains the dlopen: *both* DLLs export
`D3D12GetInterface`, so linking would be ambiguous.

⚠ **The System32 fallback is a trap on native Windows.** If the first `dlopen("d3d12core.dll")`
fails, `main.c:117-129` retries `GetSystemDirectoryA() + "\\d3d12core.dll"`. That fallback exists for
*Wine*, where a prefix's `system32` holds vkd3d's own DLLs. On real Windows it loads **Microsoft's**
`D3D12Core.dll`, which does not answer `CLSID_VKD3DCore`, and the whole thing `ERR`s out. So an
application that ships its own Agility-SDK `D3D12Core.dll` app-local will break a vkd3d drop-in.
(`R10` Q3.4.) Any Helios install/verify script for the Phase-0 arm must detect an app-local
Microsoft `D3D12Core.dll`.

### 2.5 Where the public C API lives

`include/vkd3d.h`. The parts D4 depends on:

```c
/* :53-54 */
#define VKD3D_MIN_API_VERSION VK_API_VERSION_1_3
#define VKD3D_MAX_API_VERSION VK_API_VERSION_1_3

/* :74-91 */
struct vkd3d_device_create_info
{
    D3D_FEATURE_LEVEL minimum_feature_level;
    struct vkd3d_instance *instance;
    const struct vkd3d_instance_create_info *instance_create_info;
    VkPhysicalDevice vk_physical_device;
    const char * const *device_extensions;          uint32_t device_extension_count;
    const char * const *optional_device_extensions;  uint32_t optional_device_extension_count;
    IUnknown *parent;
    LUID adapter_luid;
    D3D12_DEVICE_FACTORY_FLAGS device_factory_flags;
    bool independent;
};

/* :104 */ HRESULT vkd3d_create_instance(const struct vkd3d_instance_create_info *create_info,
                   struct vkd3d_instance **instance);
/* :110 */ HRESULT vkd3d_create_device(const struct vkd3d_device_create_info *create_info,
                   REFIID iid, void **device);
/* :129 */ HRESULT vkd3d_serialize_root_signature(const D3D12_ROOT_SIGNATURE_DESC *desc,
                   D3D_ROOT_SIGNATURE_VERSION version, ID3DBlob **blob, ID3DBlob **error_blob);
```

The **caller** picks the `VkPhysicalDevice` and supplies the LUID; `vkd3d_create_instance` takes a
`PFN_vkGetInstanceProcAddr`, so vkd3d never hard-links a loader. That is exactly why D4's new export
can skip `libs/d3d12core/main.c` — the only thing that file adds is DXGI-based adapter resolution
(§10), which a WDDM UMD must not do.

⚠ **Be precise about *which* function that is.** The exported `D3D12CreateDevice` is **not** in
`libs/d3d12core/main.c` at all — it lives in `libs/d3d12/main.c:143`, in the separate thin
`d3d12.dll` target that Helios does not use (§2.2). Inside `d3d12core.dll` the DXGI-touching path is
**`d3d12core_CreateDeviceFromFactory`** (`libs/d3d12core/main.c:643`), reachable only via
`D3D12GetInterface` → `CLSID_VKD3DCore`/`CLSID_D3D12DeviceFactory` (its two callers are `:745` and
`:1191`), and it is that function which calls `d3d12_get_adapter` (`:674`) → `CreateDXGIFactory1` at
`:383` and `:406`. **That** is the call D4's export exists to bypass. (`DECISIONS.md` D4 reason 1.)

**Two added exports, not one.** `vkd3d_serialize_root_signature` at `:129` is the function
`DECISIONS.md` H3 needs for root-signature re-serialisation — `d3d12umddi` hands the driver a
**parsed** `D3D12DDI_ROOT_SIGNATURE` while `ID3D12Device::CreateRootSignature` wants a serialized
DXBC `RTS0` blob. It exists in `vkd3d.h` but is **not in either `.def`**
(`libs/d3d12core/d3d12core.def` exports only `D3D12GetInterface` + the `D3D12SDKVersion` data
symbol, §2.3), so `DECISIONS.md` D4 now specifies **both**
`helios_vkd3d_create_device` and `helios_vkd3d_serialize_root_signature`. This is settled, not open.

---

## 3. Requirements, definitively

### 3.1 Vulkan 1.3 exactly — min == max

```c
/* include/vkd3d.h:53-54 */
#define VKD3D_MIN_API_VERSION VK_API_VERSION_1_3
#define VKD3D_MAX_API_VERSION VK_API_VERSION_1_3
```

Three enforcement sites `[VKD3D]`:

| Site | What it does |
|---|---|
| `libs/vkd3d/device.c:1455-1462` | loader `vkEnumerateInstanceVersion` below 1.3 → `E_INVALIDARG` |
| `libs/vkd3d/device.c:3538-3542` | a `VkPhysicalDevice` with `apiVersion < 1.3` is **skipped** during selection |
| `libs/d3d12core/main.c:491-495` | the Windows LUID matcher skips it: `WARN("Skipped adapter %s as it is below our minimum API version.\n", …)` |

`device.c:1465-1466` and `:4105` clamp *down* to 1.3, so a 1.4 device is driven as a 1.3 device.
**Guest reports 1.4.341** `[CAPTURE:273]` — comfortably above, and the clamp means the 1.4-only
extensions the guest exposes are simply not reached by vkd3d. **A 1.2 ICD would be fatal; a 1.4 one
is fine.**

### 3.2 ⚠ The nine hard-fail gates — every one verified against the source *and* the capture

All nine live in `vkd3d_init_device_caps()` (opens at `libs/vkd3d/device.c:3243`) and every one
returns `E_INVALIDARG`, killing `D3D12CreateDevice`. Line numbers and error strings are verbatim
from the tree at `2c7ba22c` (I read each one; the `ERR` line is given because that is the string you
grep for in a `VKD3D_LOG_FILE`).

| # | Condition that must hold | Cond. lines | `ERR` at | Error string | Guest value `[CAPTURE]` |
|---|---|---|---|---|---|
| 1 | `vertex_divisor_features.vertexAttributeInstanceRateDivisor` **&&** `…RateZeroDivisor` | 3288-3292 | **3291** | `"Lacking support for VK_EXT_vertex_attribute_divisor."` | both `true` (`:1710-1711`); ext rev **3** (`:1003`) |
| 2 | `xfb_properties.transformFeedbackQueries` | 3294-3298 | **3297** | `"Lacking support for transform feedback."` | `true` (`:657`) |
| 3 | storage **and** uniform texel-buffer single-texel alignment (or `…AlignmentBytes == 1`) | 3300-3313 | **3312** | `"Lacking support for single texel alignment."` | both `true` (`:838`, `:840`) |
| 4 | `vulkan_1_2_features.samplerMirrorClampToEdge` | 3426-3430 | **3428** | `"samplerMirrorClampToEdge is not supported by this implementation. This is required for correct operation."` | `true` (`:1631`) |
| 5 | `robustness2_features.robustBufferAccess2` **&&** `robustImageAccess2` | 3432-3437 | **3435** | `"Robustness2 features not supported. This is required."` | both `true` (`:1529-1530`) |
| 6 | `robustness2_features.nullDescriptor` | 3439-3443 | **3441** | `"Null descriptor in VK_EXT_robustness2 is not supported by this implementation. This is required for correct operation."` | `true` (`:1531`) |
| 7 | `vulkan_1_1_features.shaderDrawParameters` | 3448-3452 | **3450** | `"shaderDrawParameters is not supported by this implementation. This is required for correct operation."` | `true` (`:1627`) |
| 8 | `vulkan_info->KHR_push_descriptor` | 3454-3458 | **3456** | `"Push descriptors are not supported by this implementation. This is required for correct operation."` | ext present |
| 9 | `maintenance_5_features.maintenance5` **&&** `maintenance_6_features.maintenance6` | 3460-3465 | **3463** | `"maintenance5 and/or maintenance6 not supported by this implementation. This is required for correct operation."` | both `true` (`:1714-1715`); exts `:1048-1049` |

**All nine pass on the live guest.**

⚠ **The README omits four of them.** `vkd3d-proton-helios/README.md:19-35` lists Vulkan 1.3,
descriptor indexing ≥1,000,000 UpdateAfterBind, `samplerMirrorClampToEdge`, `shaderDrawParameters`,
`VK_EXT_robustness2`, `VK_KHR_push_descriptor` — i.e. gates 4-8 — and says nothing about **1
(vertex attribute divisor), 2 (transform-feedback queries), 3 (texel alignment) or 9
(maintenance5+6)**. ⛔ Do not use the README as the requirements list. Gate 9 in particular is a
recent tightening and is the one most likely to trip a Mesa/venus stack on a future Mesa bump.

The one *non*-fatal shortfall in the same function: `device.c:3413-3423` — a missing
`robustBufferAccessUpdateAfterBind` produces only a `WARN` ("Device does not expose robust buffer
access for the update after bind feature, enabling it anyways") and robustness is enabled regardless.
Guest has it `true` (`[CAPTURE]:742`) anyway.

Second-order but silent: `device.c:3271-3276` requires
`graphics_pipeline_library_properties.graphicsPipelineLibraryIndependentInterpolationDecoration`,
and if it is absent GPL is **silently disabled** with no error. Guest passes.

### 3.3 Required instance and device extensions

Everything below is what `libs/d3d12core/main.c` passes into `vkd3d_create_device`. **A Helios
`helios_vkd3d_create_device` export (D4) must pass an equivalent list itself** — this is the exact
content that file provides and D4 skips.

| Kind | Extensions | Site |
|---|---|---|
| Instance, **required** | `VK_KHR_surface`, and under `#ifdef _WIN32` `VK_KHR_win32_surface` | `d3d12core/main.c:574-580` |
| Instance, optional | `VK_KHR_surface_maintenance1`, `VK_EXT_surface_maintenance1`, `VK_KHR_get_surface_capabilities2` | `:582-593` |
| Device, **required** | **`VK_KHR_swapchain` — and that is the entire list** | `:659-662` |
| Device, optional | `VK_KHR_swapchain_maintenance1`, `VK_EXT_swapchain_maintenance1` | `:664-668` |

```c
/* libs/d3d12core/main.c:659-662 */
static const char * const device_extensions[] =
{
    VK_KHR_SWAPCHAIN_EXTENSION_NAME,
};
```

Guest has all of `VK_KHR_surface` + `VK_KHR_win32_surface` (instance) and `VK_KHR_swapchain` +
`VK_EXT_swapchain_maintenance1` (device) `[CAPTURE]`. ⚠ Note `VK_KHR_swapchain` and the
`swapchain_maintenance1` pair are **venus-native** extensions (`vn_physical_device.c:1342-1352`),
not passthrough — they exist because the ICD implements them, not because virglrenderer forwards
them. §4.1 explains why that distinction changes how you read the layer table.

### 3.4 Optional extensions, grouped by what each unlocks

`optional_device_extensions[]` at `libs/vkd3d/device.c:66-167` has ~91 rows, each of the shape
(struct at `:40-58`):

```
{ name, offsetof(vkd3d_vulkan_info, member), enable_config_flags, disable_config_flags, min_spec_version }
```

The groups worth knowing:

⚠ **Line numbers in this table were re-derived from `device.c:66-167` this session.** The row block
is `:69-166`, with comment/preprocessor lines interleaved at `:68`, `:99`, `:102`, `:106`, `:142`,
`:149`, `:162`, `:165` — which is exactly why an eyeballed offset lands on the wrong extension. If
the pin moves off `2c7ba22c`, re-derive with
`grep -n 'VK_EXTENSION' libs/vkd3d/device.c | awk -F: '$1>=66 && $1<=170'` before quoting any of
these.

| Group | Extensions | Unlocks |
|---|---|---|
| Descriptor model | `VK_EXT_descriptor_buffer` (:125) · `VK_EXT_mutable_descriptor_type` (:122) / `VK_VALVE_` alias (:163) · `VK_EXT_descriptor_heap` (:141, **opt-in only**, `VKD3D_CONFIG=descriptor_heap`) | the four backends in §4.4 |
| Raytracing | `VK_KHR_ray_tracing_pipeline`, `acceleration_structure`, `deferred_host_operations`, `ray_query`, `ray_tracing_maintenance1` (:71-75), **`VK_KHR_opacity_micromap`** (:98), `VK_EXT_pipeline_library_group_handles` (:126) — **all disabled by `VKD3D_CONFIG=nodxr`** | `RaytracingTier` (§6) |
| Feature-level tiers | `VK_EXT_mesh_shader` (:121) · `VK_KHR_fragment_shading_rate` (:76) · `VK_EXT_fragment_shader_interlock` (:129) · `VK_EXT_conservative_rasterization` (:108) | `MeshShaderTier`, `VariableShadingRateTier`, `ROVsSupported`, `ConservativeRasterizationTier` |
| Shader model | `VK_KHR_compute_shader_derivatives` (:90), `VK_EXT_shader_image_atomic_int64` (:120) → SM 6.6 · `VK_KHR_shader_maximal_reconvergence` (:88), `VK_KHR_shader_quad_control` (:89) → SM 6.7 · `VK_KHR_maintenance8` (:84) → `OPTIONS14.AdvancedTextureOpsSupported` **only** | §7.2 |
| Residency / memory | `VK_EXT_pageable_device_local_memory` (:130) + `VK_EXT_memory_priority` (:131) · `VK_EXT_zero_initialize_device_memory` (:139) · `VK_EXT_external_memory_host` (:119) | `ResourceHeapTier` (§5), `OpenExistingHeapFromAddress` |
| Debug | `VK_EXT_device_fault` / `VK_EXT_device_address_binding_report` (:135, :137, behind `VKD3D_CONFIG=fault`) · `VK_NV_device_diagnostic_checkpoints` (:155, behind `breadcrumbs`) · `VK_AMD_buffer_marker` (:143) | breadcrumbs, GPU-fault reports |
| Windows interop | `#ifdef _WIN32` block at `:99-102`: **`VK_KHR_external_memory_win32`** (:100), `VK_KHR_external_semaphore_win32` (:101) | shared heaps / shared `ID3D12Fence` — **see §5.1, this is the landmine** |

⛔ **It is `VK_KHR_opacity_micromap`, not `VK_EXT_`.** At this pin vkd3d requires the **KHR**
extension exclusively: the table row is
`VK_EXTENSION_DISABLE_COND(KHR_OPACITY_MICROMAP, KHR_opacity_micromap, …)` at `device.c:98`, the
`vulkan_info` bool is `KHR_opacity_micromap` (`vkd3d_private.h:159`), and the feature struct is
`VkPhysicalDeviceOpacityMicromapFeaturesKHR` (`device.c:2563`, chained under
`if (vulkan_info->KHR_opacity_micromap)` at `:2561`).
`grep -rn 'EXT_opacity_micromap' libs/ include/` returns **only** the unrelated vkd3d-ext enumerator
`D3D12_VK_EXT_OPACITY_MICROMAP` (`device_vkd3d_ext.c:150`), which is a D3D-side extension ID, not a
Vulkan extension name. The two are **distinct, real Vulkan extensions**
(`icd/mesa/include/vulkan/vulkan_core.h:14620` and `:21439`), so writing EXT here is a wrong-feature
error, not a typo — and it changes S8's attribution (§5): the host NVIDIA driver exposes only the
**EXT** variant, so the KHR one is not merely unimplemented in venus, it is **absent from the host
GPU as well**.

⛔ **Escape hatch, and it is the right A/B tool:** `VKD3D_DISABLE_EXTENSIONS=<comma list>` disables
any of them at runtime (`device.c:186-194`). Use it to make a "would this still work without X"
question a single env var instead of a rebuild.

⚠ **The profile and the code disagree in two places.** `VP_D3D12_VKD3D_PROTON_profile.json` lists
`VK_KHR_calibrated_timestamps` and `VK_EXT_dynamic_rendering_unused_attachments` in
`baseline_features`, but `device.c:91` and `:132` have both in `optional_device_extensions` with no
hard check; and `shader_model_67` lists `VK_KHR_maintenance8`, which the SM 6.7 code (§7.2) does not
require. **The nine `E_INVALIDARG` checks are what actually fails device creation**; the profile is
aspirational. Both are satisfied by Helios anyway, so the disagreement is moot here — but do not
"fix" a profile miss that the code never checks.

---

## 4. What Helios exposes

### 4.1 The layering mechanism — read this before reading the table

An extension reaches a guest app only after **four** filters, in this order:

1. **Host GPU driver.** NVIDIA 610.43.03 on an RTX PRO 6000 Blackwell `[LIVE, host]`. 281 device
   extensions (`docs/reference/host-vulkan-profile-rtx-pro-6000-blackwell.json`).
2. **virglrenderer's `vkr_extension_table`.** `vkr_physical_device_init_extensions()` keeps an entry
   only if `vkr_extension_get_spec_version(name) != 0`, which is a lookup into a static table with
   **182** `= true` entries indexed by a venus-protocol name table of **185** names.
   `[VIRGL]` — the table is `static const struct vn_info_extension_table vkr_extension_table` at
   `src/venus/vkr_common.c:17`, its lookup guard at `:257`
   (`if (index < 0 || !vkr_extension_table.enabled[index])`), and the name table is
   `src/venus/venus-protocol/vn_protocol_renderer_info.h`. ⚠ Both paths are relative to **the
   virglrenderer 1.3.0 source checkout the Conventions section (top of this document) tells you how
   to fetch** — nothing here is committed in this repo. Reproduce both counts there:
   ```bash
   V=tmp/dx12/virgl/virglrenderer-virglrenderer-1.3.0
   sed -n '17,230p' $V/src/venus/vkr_common.c | grep -c '= true'            # 182
   grep -c '{ *"VK_' $V/src/venus/venus-protocol/vn_protocol_renderer_info.h  # 185
   ```
   `[LIVE]` cross-check against the *installed binary*, which is what actually runs —
   `strings -a /usr/lib/libvirglrenderer.so.1 | grep -c '^VK_'` prints **185**. Installed version
   **1.3.0-2** (`pacman -Qi virglrenderer`), mapped into QEMU as
   `/usr/lib/libvirglrenderer.so.1.11.0`.
3. **Mesa venus's `passthrough` table.** `vn_physical_device_get_passthrough_extensions()`
   (`icd/mesa/src/virtio/vulkan/vn_physical_device.c:1378-1591`), ~172 entries. The combining rule
   (`vn_physical_device_init_supported_extensions()`, `:1593-1623`) is verbatim:
   ```c
   if (native.extensions[i]) { ...supported... }
   else if (passthrough.extensions[i] && physical_dev->renderer_extensions.extensions[i]) { ...supported... }
   ```
4. **Mesa venus's `native` table.** `vn_physical_device_get_native_extensions()` (`:1248-1375`) —
   ICD-side implementations that need no renderer support: `VK_KHR_swapchain`,
   `VK_KHR_swapchain_maintenance1`, `VK_EXT_swapchain_maintenance1`,
   `VK_KHR_swapchain_mutable_format`, `VK_KHR_incremental_present`, `VK_EXT_hdr_metadata`,
   `VK_KHR_deferred_host_operations`, `VK_KHR_map_memory2`, `VK_EXT_tooling_info`,
   `VK_EXT_device_memory_report`, `VK_EXT_pci_bus_info`, the Helios-specific
   `VK_KHR_external_memory_fd` + `VK_EXT_external_memory_dma_buf` pair (`:1327-1328`), and on
   Windows `VK_KHR_external_semaphore_win32` (assignment at `:1277`, inside the
   `#if DETECT_OS_WINDOWS` block `:1273-1279`).

⇒ **A "NO" in the virglrenderer or passthrough column does not mean the guest lacks it.** The guest
column is authoritative; the middle columns tell you *which layer you must change* if the guest
column says NO.

### 4.2 The three-layer table

Columns: what vkd3d wants · guest ICD `[CAPTURE]` · virglrenderer 1.3.0's protocol name table
`[LIVE]` · Mesa venus passthrough table `[MESA]` · host GPU · **the layer that limits it**.

| Extension | vkd3d needs it for | guest | vkr | passthru | host | **limiter** |
|---|---|---|---|---|---|---|
| `VK_KHR_swapchain` | REQUIRED (d3d12core) | yes | NO | NO | yes | — (venus-native) |
| `VK_KHR_push_descriptor` | REQUIRED (gate 8) | yes | yes | yes | yes | — |
| `VK_KHR_maintenance5` | REQUIRED (gate 9) | yes | yes | yes | yes | — |
| `VK_KHR_maintenance6` | REQUIRED (gate 9) | yes | yes | yes | yes | — |
| `VK_EXT_robustness2` | REQUIRED (gates 5,6) | yes | yes | yes | yes | — |
| `VK_EXT_transform_feedback` | REQUIRED (gate 2) | yes | yes | yes | yes | — |
| `VK_EXT_vertex_attribute_divisor` | REQUIRED (gate 1), spec ver ≥3 | yes (rev 3) | yes | yes | yes | — |
| `VK_EXT_custom_border_color` | profile baseline | yes | yes | yes | yes | — |
| `VK_EXT_depth_clip_enable` | profile baseline | yes | yes | yes | yes | — |
| `VK_EXT_dynamic_rendering_unused_attachments` | profile baseline | yes | yes | yes | yes | — |
| `VK_KHR_calibrated_timestamps` | profile baseline | yes | yes | yes | yes | — |
| `VK_EXT_descriptor_indexing` | core 1.2, required features | yes | yes | yes | yes | — |
| `VK_EXT_image_view_min_lod` | README "should" | yes (`minLod=true`) | yes | yes | yes | — |
| `VK_EXT_mutable_descriptor_type` | recommended | yes | yes | yes | yes | — |
| **`VK_EXT_descriptor_heap`** | best descriptor model, opt-in | **NO** | **NO** | yes (`:1531`) | yes | **virglrenderer ONLY** |
| **`VK_EXT_descriptor_buffer`** | 2nd-best descriptor model | **NO** | **NO** | NO | yes | venus-protocol + Mesa + vkr |
| `VK_EXT_conservative_rasterization` | ConsRast tiers / FL 12.1 | yes | yes | yes | yes | — |
| `VK_EXT_fragment_shader_interlock` | ROVs (FL 12.1) | yes | yes | yes | yes | — |
| `VK_KHR_acceleration_structure` | DXR | yes | yes | yes | yes | — |
| `VK_KHR_ray_tracing_pipeline` | DXR 1.0 | yes | yes | yes | yes | — |
| `VK_KHR_ray_query` | DXR 1.1 | yes | yes | yes | yes | — |
| `VK_KHR_deferred_host_operations` | DXR | yes | NO | NO | yes | — (venus-native) |
| `VK_KHR_ray_tracing_maintenance1` | DXR 1.1 | yes | yes | yes | yes | — |
| `VK_KHR_pipeline_library` | DXR / GPL | yes | yes | yes | yes | — |
| `VK_EXT_pipeline_library_group_handles` | DXR | yes | yes | yes | yes | — |
| ⚠ **`VK_KHR_opacity_micromap`** | DXR **1.2** (`device.c:98`) | **NO** | **NO** | NO | **NO** | venus-protocol + Mesa + vkr **AND host GPU** — see below |
| `VK_EXT_mesh_shader` | MeshShaderTier (FL 12.2) | yes | yes | yes | yes | — |
| `VK_KHR_fragment_shading_rate` | VariableShadingRateTier | yes | yes | yes | yes | — |
| `VK_EXT_shader_image_atomic_int64` | SM 6.6 | yes | yes | yes | yes | — |
| `VK_KHR_compute_shader_derivatives` | SM 6.6 | yes | yes | yes | yes | — |
| `VK_KHR_shader_maximal_reconvergence` | SM 6.7 | yes | yes | yes | yes | — |
| `VK_KHR_shader_quad_control` | SM 6.7 | yes | yes | yes | yes | — |
| **`VK_KHR_maintenance8`** | `OPTIONS14.AdvancedTextureOps` **only** | **NO** | **NO** | NO | yes | venus-protocol + Mesa + vkr |
| **`VK_KHR_maintenance9` / `maintenance10`** | perf | **NO** | **NO** | NO | yes | venus-protocol + Mesa + vkr |
| `VK_KHR_shader_float_controls2` | perf | yes | yes | yes | yes | — |
| `VK_EXT_graphics_pipeline_library` | PSO perf | yes | yes | yes | yes | — |
| `VK_EXT_extended_dynamic_state2` / `3` | perf | yes | yes | yes | yes | — |
| **`VK_EXT_shader_module_identifier`** | PSO-cache perf | **NO** | **NO** | NO | yes | venus-protocol + Mesa + vkr |
| **`VK_KHR_present_id` / `present_wait`** | frame pacing / latency | **NO** | **NO** | NO (`#ifndef _WIN32`) | yes | Mesa `#ifdef` **+** vkr |
| `VK_EXT_swapchain_maintenance1` | d3d12core optional | yes | NO | NO | yes | — (venus-native) |
| **`VK_EXT_memory_budget`** | `QueryVideoMemoryInfo` quality | **NO** | **yes** | gated | yes | **one guest env var** |
| **`VK_EXT_memory_priority`** | residency | **NO** | NO | NO | yes | venus-protocol + Mesa + vkr |
| **`VK_EXT_pageable_device_local_memory`** | one route to `ResourceHeapTier 2` | **NO** | NO | NO | yes | venus-protocol + Mesa + vkr |
| **`VK_EXT_device_generated_commands`** | ExecuteIndirect w/ state, work graphs | **NO** | NO | NO | yes | venus-protocol + Mesa + vkr |
| **`VK_EXT_external_memory_host`** | `OpenExistingHeapFromAddress` | **NO** | NO | NO | yes | venus-protocol + Mesa + vkr |
| ⚠ **`VK_KHR_external_memory_win32`** | `HEAP_FLAG_SHARED` — **used UNGUARDED** | **NO** | NO | NO | NO | **Mesa venus (new native ext)** |
| `VK_KHR_external_semaphore_win32` | shared `ID3D12Fence` | yes | NO | NO | NO | — (venus-native, `:1277`) |
| `VK_EXT_shader_stencil_export` | `PSSpecifiedStencilRef` | NO | yes | yes | **NO** | **host GPU** — NVIDIA lacks it |
| `VK_EXT_conditional_rendering` | predication | yes | yes | yes | yes | — |
| `VK_EXT_scalar_block_layout` | SM 6.0 cbuffer | yes | yes | yes | yes | — |
| `VK_EXT_line_rasterization` / `index_type_uint8` / `image_sliced_view_of_3d` | misc | yes | yes | yes | yes | — |
| **`VK_AMD_buffer_marker` / `VK_EXT_device_fault` / `VK_NV_device_diagnostic_checkpoints`** | breadcrumbs | **NO** | NO | NO | yes | venus-protocol + Mesa + vkr |
| **`VK_KHR_unified_image_layouts` / `VK_EXT_zero_initialize_device_memory`** | perf | **NO** | NO | NO | yes | venus-protocol + Mesa + vkr |

⚠ **`VK_KHR_opacity_micromap` is the one row where the *host GPU* is also a limiter**, which puts it
in the same class as `VK_EXT_shader_stencil_export` and takes it off the "just do venus work" list.
The host profile `docs/reference/host-vulkan-profile-rtx-pro-6000-blackwell.json:65` carries
`"VK_EXT_opacity_micromap": 2` and **no `VK_KHR_opacity_micromap` entry at all** — NVIDIA 610.43.03
exposes only the EXT variant, while vkd3d at this pin consumes only the KHR one (§3.4). So S8 is not
merely "large": it is **currently unreachable through venus alone** and needs an NVIDIA driver that
ships the KHR promotion (or a vkd3d fork patch that accepts the EXT variant, which is a much bigger
change than a name swap — the KHR and EXT structs are separate types).

`[LIVE]` spot-verification of the vkr column this session — the host protocol table does **not**
contain `VK_EXT_descriptor_heap`, `VK_EXT_descriptor_buffer`, `VK_KHR_maintenance8`,
`VK_KHR_maintenance9`, `VK_KHR_present_wait`, or **either** opacity-micromap name, and **does**
contain `VK_EXT_memory_budget`, `VK_KHR_maintenance5/6/7`:

```bash
strings -a /usr/lib/libvirglrenderer.so.1 | grep -E \
  '^VK_(EXT_descriptor_(heap|buffer)|KHR_maintenance[5-9]|EXT_memory_budget|KHR_present_wait|(KHR|EXT)_opacity_micromap)$' \
  | sort -u
# → VK_EXT_memory_budget, VK_KHR_maintenance5, VK_KHR_maintenance6, VK_KHR_maintenance7
```

⚠ The earlier revision of this document ran that grep with `EXT_opacity_micromap` only — the wrong
name — so its "NO" was accidentally right for the wrong reason. Both names are now tested, and
neither is present. The guest side is equally empty:
`grep -n 'opacity_micromap' icd/mesa/src/virtio/venus-protocol/vn_protocol_driver_info.h
icd/mesa/src/virtio/vulkan/vn_physical_device.c` returns nothing.

And `[LIVE]` the guest-side protocol table **does** carry descriptor_heap, which is why the limiter
is virglrenderer alone:

```bash
grep -n 'VK_EXT_descriptor_heap' icd/mesa/src/virtio/venus-protocol/vn_protocol_driver_info.h
# 43:   { "VK_EXT_descriptor_heap", 136, 1 },
grep -c '{ *"VK_' icd/mesa/src/virtio/venus-protocol/vn_protocol_driver_info.h   # 187 (host: 185)
```

⚠ **`VK_KHR_present_id`/`present_wait` need a two-layer fix**, not one: even if virglrenderer gained
them, the Helios Mesa fork disables them on Windows by preprocessor
(`icd/mesa/src/virtio/vulkan/vn_physical_device.c:1336-1341`, inside the
`if (physical_dev->renderer_sync_fd.semaphore_importable)` arm that opens at `:1334`):

```c
#ifndef VK_USE_PLATFORM_WIN32_KHR
      exts->KHR_present_id = true;
      exts->KHR_present_id2 = true;
      exts->KHR_present_wait = true;
      exts->KHR_present_wait2 = true;
#endif /* VK_USE_PLATFORM_WIN32_KHR */
```

### 4.3 Limits, queue families, and memory — venus is a bit-exact passthrough

Every field of `VkPhysicalDeviceProperties.limits` is identical guest-vs-host (R12 §2.1, 0
differences). The values vkd3d cares about `[CAPTURE]`:

| Limit / property | Guest | Needed for |
|---|---|---|
| `maxPerStageDescriptorUpdateAfterBind{StorageBuffers,SampledImages,StorageImages}` | **1 048 576** (`:746-748`) | profile wants ≥1 000 000; README §"Drivers" |
| `robustBufferAccessUpdateAfterBind` | `true` (`:742`) | `fl_11_0_properties` |
| `bufferImageGranularity` | 1024 (≤ 64 KiB) | `ResourceHeapTier 2` first clause (`device.c:9996`) |
| `filterMinmaxSingleComponentFormats` | `true` (`:772`) | `TiledResourcesTier ≥ 2` |
| `sparseAddressSpaceSize` | 0x100_0000_0000 = 1 TiB (`:295`) | reserved resources |
| `subgroupSize` | 32 (`:673`), 11 ops, all 14 stages | SM 6.0 gate |
| `maxVertexAttribDivisor` | 0xFFFFFFFF (`:664`) | gate 1 |
| `maxPushConstantsSize` / `maxBoundDescriptorSets` | 256 / 32 | root signature layout |

**Queue families** `[CAPTURE]:1097-1145` — all six carry `VK_QUEUE_SPARSE_BINDING_BIT`, all six have
`minImageTransferGranularity = (1,1,1)` (which matters: `vkd3d_find_queue`, `device.c:3751-3770`,
**skips** any TRANSFER family with a zero granularity component):

| # | flags | count |
|---|---|---|
| 0 | GRAPHICS \| COMPUTE \| TRANSFER \| SPARSE_BINDING | 16 |
| 1 | TRANSFER \| SPARSE_BINDING | 2 |
| 2 | COMPUTE \| TRANSFER \| SPARSE_BINDING | 8 |
| 3 | TRANSFER \| SPARSE_BINDING | 4 |
| 4 | TRANSFER \| SPARSE_BINDING | 3 |
| 5 | TRANSFER \| SPARSE_BINDING \| OPTICAL_FLOW_NV | 1 |

Walking `vkd3d_select_queues` (`device.c:3788-3850`) against those values by hand — the match rule
is `(queueFlags & mask) == want`:

| vkd3d family | mask / want | Lands on |
|---|---|---|
| `VKD3D_QUEUE_FAMILY_GRAPHICS` | mask `G\|C`, want `G\|C` (`:3806`) | **family 0** |
| `VKD3D_QUEUE_FAMILY_COMPUTE` | mask `G\|C`, want `C` (`:3809`) | **family 2** |
| `VKD3D_QUEUE_FAMILY_SPARSE_BINDING` | 1st try mask `G\|C\|T\|S` want `S` (`:3814`) → **no match** (every sparse family also has TRANSFER); 2nd try mask `G\|S` want `S` (`:3820`) | **family 1** |
| `VKD3D_QUEUE_FAMILY_TRANSFER` | mask `G\|C\|T`, want `T` (`:3835`) | **family 1** |
| `VKD3D_QUEUE_FAMILY_OPTICAL_FLOW` | only if `NV_optical_flow` | not taken — the guest does not expose `VK_NV_optical_flow` |

⇒ **D3D12's DIRECT / COMPUTE / COPY queues land on three distinct real Vulkan families (0 / 2 / 1),
and sparse binding shares family 1 with COPY.** `[INFER]` from `[CAPTURE]` + `[VKD3D]`;
**UNVERIFIED** until a real device logs it — `VKD3D_DEBUG=info` prints the selection.

⚠ **Family 1 is a queue family the Helios D3D11 stack has never used.** DXVK on Helios has only
ever driven the graphics family. Whether the KMD/venus ring plumbing handles a second family's
timeline is not this document's lane (hand to `KMD_IMPACT.md`), but it is the single most likely
place for a first-run wedge. `VKD3D_CONFIG=single_queue` (`device.c:3843-3847`) collapses COMPUTE
and TRANSFER onto GRAPHICS and is the first A/B to reach for; `VN_DEBUG=no_second_queue` is the
venus-side mirror.

**Memory** `[CAPTURE]:1150-1220`: 2 heaps (95.59 GiB DEVICE_LOCAL, 70.31 GiB host), 5 memory types
including `HOST_VISIBLE|HOST_COHERENT` and `HOST_VISIBLE|HOST_COHERENT|HOST_CACHED`.

### 4.4 Which descriptor backend Helios lands in

vkd3d picks one of four at device init and **hot-swaps the device and command-list vtables** to
match (`d3d12_device_replace_vtable`, `device.c:11302-11400`):

| Backend | Predicate | Vulkan mechanism | Reachable on Helios? |
|---|---|---|---|
| Descriptor heap | `VKD3D_BINDLESS_HEAP` | `VK_EXT_descriptor_heap`, opt-in via `VKD3D_CONFIG=descriptor_heap` | **No** — virglrenderer lacks it |
| Embedded mutable | `d3d12_device_use_embedded_mutable_descriptors()` | descriptor buffer + mutable type | **No** — needs descriptor_buffer |
| Descriptor buffer | `d3d12_device_uses_descriptor_buffers()` | `VK_EXT_descriptor_buffer` | **No** |
| **Legacy sets** | otherwise | plain `VkDescriptorSet` + `vkUpdateDescriptorSets` | **Yes — this is what Helios gets** |

`[INFER]`, **UNVERIFIED**: settle by running any vkd3d client with `VKD3D_DEBUG=info` and reading
the bindless-state log, or by breakpointing `d3d12_device_replace_vtable` (`device.c:11302`). This
matters because vkd3d compiles fast paths for specific `(cbv_srv_uav, sampler)` descriptor sizes —
`(64,16)` RDNA2, `(32,16)` RDNA3+, `(32,32)` NV, `(128,32)` Intel (`device.c:11324-11395`) — and
"legacy sets" means **none of them apply**. Fixing `VK_EXT_descriptor_heap` in virglrenderer (§5.3)
is the cheapest way to move off this floor.

---

## 4.5 ⭐ `VulkanOn12.md` — Microsoft's own D3D12↔Vulkan impedance list, read backwards

*(2026-08-05. `VulkanOn12.md` is the highest-signal document in the DirectX-Specs corpus for this
project: **11 of its 11 distinct `PFND3D12DDI_*` typedefs exist in SDK 26100** — the only spec in the
corpus with a 100 % hit rate. It is Microsoft's account of what D3D12 had to **add** so that Vulkan
semantics could be expressed on it. Helios runs that mapping in reverse, so every addition marks a seam
where the two models do not line up — i.e. exactly where a forwarding UMD breaks.)*

⛔ **The load-bearing consequence: thirteen of its behaviours are mandatory by DDI *version floor*
alone.** They carry no cap, and the driver cannot decline them — negotiating `D3D12DDI_SUPPORTED_0110`
signs Helios up for all of them. Only **five BOOL caps**, spread across four of the spec's seventeen
features, are declinable (`RelaxedFormatCastingSupported`,
`UnrestrictedBufferTextureCopyPitchSupported`, `UnrestrictedVertexElementAlignmentSupported`, and two
others in `D3D12DDI_OPTIONS_DATA_0090`/`_0091`).

⚠ **This sharpens the `_0040`-vs-`_0110` trade in `DDI_REFERENCE.md` §1.6, which had been framed purely
as a slot count.** `_0110` is not just 214 slots instead of 169 — it is also a behavioural contract with
no opt-out. The four that matter most for a vkd3d forwarder:

| seam | what `_0110` obliges | why it bites a Vulkan-backed forwarder |
|---|---|---|
| **Triangle fans** | DDI 0097 revives `D3D12DDI_PRIMITIVE_TOPOLOGY_TRIANGLEFAN` (value 6) and makes it **mandatory** at 0097+ | Microsoft revived it *because* "software emulation for it is expensive" for a Vulkan-on-D3D12 layer. Helios is the same mapping in reverse and inherits the same expense — core Vulkan 1.3 has no triangle fan without `VK_KHR_portability_subset`/`VK_EXT_primitive_topology_list_restart` semantics |
| **Mismatched RT/DS sizes** | at 0102+ the driver must accept render targets and depth buffers of **differing** width/height, and D3D withdraws its guarantee of an implicit scissor to `{0,0,width,height}` | Beyond the smallest output view the result is explicitly **undefined**, and the spec permits GPU hangs/faults as a legal outcome. ⛔ On this stack a "GPU fault" is not confined to the offending app — it takes the host virglrenderer context, and historically the guest desktop with it (`GATES.md` §7.26) |
| **Dynamic-state PSO flags are HINTS** | the `D3D12DDI_PIPELINE_STATE_FLAG_DYNAMIC_*` flags do **not** relieve the driver of applying the PSO's own depth-bias and IB-strip-cut values on every `pfnSetPipelineState` | ⚠ This is a **precise inversion** of the Vulkan mental model a vkd3d-shaped forwarder brings: in Vulkan, declaring `VK_DYNAMIC_STATE_DEPTH_BIAS` means the pipeline's baked value is *ignored*. Here it is not |
| **Non-normalized sampler coords** | mandatory at 0100+: sampling outside the valid range is undefined **in the device state left behind**, not merely in the value returned, unless AddressU/V are CLAMP or BORDER | `D3D12DDI_SAMPLER_FLAG_NON_NORMALIZED_COORDINATES = 0x02` is live in the header and can arrive on a dynamic sampler descriptor |

⚠ **One ABI trap in the same family:** `DepthBias` silently changed from `INT` to `FLOAT` in the DDI
rasterizer desc at 0099, and 0102 revs the struct again (`D3D12DDI_RASTERIZER_DESC_0102`) replacing
`MultisampleEnable`/`AntialiasedLineEnable` with a single `LineRasterizationMode` enum. At 0110
`pfnCreateRasterizerState` receives the 0102 shape, where a `FLOAT DepthBias` sits at the same offset an
older `INT` did — a reinterpretation no compiler will flag.

⚠ **A substrate ceiling this raises, not yet settled:** `MaxSamplerDescriptorHeapSize` must be reported
**≥ 4000** at 0102+, and the host GPU's `maxSamplerAllocationCount` is **exactly 4000** — zero headroom
if vkd3d allocates one `VkSampler` per descriptor. `GATES.md` §7.24 owns it.

⚠ **What `VulkanOn12.md` does *not* cover, despite the title:** image layouts, barriers, resource
state, fences, semaphores, and queue submission ordering are **entirely absent** — the word does not
appear. Given that WS1 stability is a synchronisation question, that silence is worth knowing about
rather than assuming coverage. Barrier/layout impedance is owned by `DDI_REFERENCE.md` §9.10.1 instead.

## 5. The gap, cheapest-first

Nothing in the **required** band is missing. Every item below is optional to `D3D12CreateDevice`
succeeding, and each is attributed to the one layer that must change. The first two are the ones
that are real work; the rest are ordered by cost.

| # | Gap | Layer that must change | What D3D12 loses | Work |
|---|---|---|---|---|
| **S1** | ⚠ `VK_KHR_external_memory_win32` absent **and used unguarded** | **Mesa venus ICD** (new native ext) | `HEAP_FLAG_SHARED`, `CreateSharedHandle`, D3D11On12 — and today it **crashes**, not degrades | medium |
| **S2** | ⚠ no 32-bit (WOW64) Vulkan ICD registered | packaging / Mesa build | a 32-bit `d3d12.dll` finds **zero** physical devices | small–medium |
| S3 | `VK_EXT_memory_budget` off | **one guest env var** | `QueryVideoMemoryInfo` budget accuracy | **trivial** |
| S4 | `VK_EXT_descriptor_heap` | **virglrenderer only** | vkd3d's best descriptor model | small |
| S5 | `VK_EXT_descriptor_buffer` | venus-protocol + Mesa + vkr | 2nd-best descriptor model | large |
| S6 | `VK_KHR_present_id` / `present_wait` | Mesa `#ifdef` **+** vkr | frame pacing, DXGI waitable-object fidelity | medium |
| S7 | `VK_KHR_maintenance8` | venus-protocol + Mesa + vkr | `OPTIONS14.AdvancedTextureOpsSupported=false` only — **not** an SM 6.7 blocker | medium |
| S8 | ⚠ **`VK_KHR_opacity_micromap`** (not the EXT — §3.4) | venus-protocol + Mesa + vkr **AND the host GPU** — NVIDIA 610.43.03 exposes only `VK_EXT_opacity_micromap` (`docs/reference/host-vulkan-profile-rtx-pro-6000-blackwell.json:65`) | DXR 1.2 → capped at 1.1 | **large, and not currently reachable through venus alone** |
| S9 | `pageable_device_local_memory` + `memory_priority` | venus-protocol + Mesa + vkr | residency quality; one of two routes to `RESOURCE_HEAP_TIER_2` | medium |
| S10 | `device_generated_commands` | venus-protocol + Mesa + vkr | ExecuteIndirect with state changes; work-graph mesh nodes | large |
| S11 | `shader_module_identifier`, `AMD_buffer_marker`, `EXT_device_fault`, `NV_device_diagnostic_checkpoints`, `maintenance9/10`, `unified_image_layouts`, `zero_initialize_device_memory` | venus-protocol + Mesa + vkr | PSO-cache perf; breadcrumbs; misc perf | medium each |
| S12 | `VK_EXT_external_memory_host` | venus-protocol + Mesa + vkr | `OpenExistingHeapFromAddress` | large (host-pointer import over venus is architecturally awkward) |
| — | `VK_EXT_shader_stencil_export` | **host GPU** — NVIDIA does not expose it | `PSSpecifiedStencilRefSupported=false`; vkd3d has an explicit fallback (`meta.c:863`, `:905`, `:1200`, `:1332`) | **none available, and not a Helios gap** |

### 5.1 ⚠ S1 — `VK_KHR_external_memory_win32`: a NULL function pointer, not a graceful degrade

This is the one correctness landmine in the substrate. **Read this before writing any D3D12 test
that touches shared resources.**

**The guest exposes the semaphore half and not the memory half.** `[CAPTURE]`:
`VK_KHR_external_semaphore_win32` present, `VK_KHR_external_memory_win32` absent. The Mesa fork
implements the semaphore half natively — `icd/mesa/src/virtio/vulkan/vn_physical_device.c:1273-1279`
(the `#if DETECT_OS_WINDOWS` block; the assignment itself is `:1277`):

```c
#if DETECT_OS_WINDOWS
      if (physical_dev->external_binary_semaphore_handles &
          (VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT |
           VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_KMT_BIT)) {
         exts->KHR_external_semaphore_win32 = true;
      }
#endif
```

and deliberately routes the memory half through the **fd**-named extensions, which describe the
*wire* (renderer-side) handle type — `vn_physical_device.c:1305-1328`, with a long Helios comment
("no POSIX fd ever crosses into the guest"). There is a standing note at `:1050` that *"when the
renderer runs on Windows, `VK_KHR_external_memory_win32` might be required"* — the memory half is
exactly the analogous native work the semaphore half already got.

**vkd3d does not check for it.** `grep -rn KHR_external_memory_win32 libs/ include/` returns
**four** hits, verified this session:

```
libs/vkd3d/vulkan_procs.h:257   #ifdef VK_KHR_external_memory_win32   (guards the PFN declarations)
libs/vkd3d/vulkan_procs.h:258   /* VK_KHR_external_memory_win32 */    (the comment inside that guard)
libs/vkd3d/vkd3d_private.h:138  bool KHR_external_memory_win32;
libs/vkd3d/device.c:100         VK_EXTENSION(KHR_EXTERNAL_MEMORY_WIN32, KHR_external_memory_win32)
```

**The bool is written and never read.** Then, on `_WIN32`, inside
`if (heap_flags & D3D12_HEAP_FLAG_SHARED)` (`libs/vkd3d/resource.c:4405-4431`), the code **branches
on whether a shared handle was supplied** — it is not one unconditional chain, and getting this
wrong misreads which arm a test exercises:

- **import arm**, `resource.c:4410-4420` — when `shared_handle && shared_handle != INVALID_HANDLE_VALUE`,
  it chains `VkImportMemoryWin32HandleInfoKHR` (`OPAQUE_WIN32_KMT` if the handle's top bits are set,
  else `OPAQUE_WIN32`);
- **create/export arm**, `resource.c:4421-4427` — the `else`, which chains:
  ```c
  export_info.sType       = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO;
  export_info.pNext       = allocate_info.pNext;
  export_info.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT;
  allocate_info.pNext     = &export_info;
  ```
- then `resource.c:4468-4469` calls `d3d12_resource_open_export_kmt()` — but only when
  `export_info.handleTypes == VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT`, i.e. only after the
  export arm ran — and that function at `libs/vkd3d/d3dkmt.c:113-119` does
  `VK_CALL(vkGetMemoryWin32HandleKHR(...))`, a PFN that is **NULL** when the extension was never
  enabled;
- and `d3d12_device_CreateSharedHandle` (`device.c:7646-7651`) calls the same PFN with **no
  extension guard whatsoever**, on any resource, import arm or not.

⚠ **Neither arm is guarded on `device->vk_info.KHR_external_memory_win32`** (confirmed at both call
sites this session), so both routes reach a NULL `vkGetMemoryWin32HandleKHR`. The hazard conclusion
is unchanged; only the *shape* of the entry into it was wrong. `DECISIONS.md` S1 carries the same
"unconditionally chains … for any `HEAP_FLAG_SHARED` allocation" wording and needs the same
correction — fixing one without the other leaves the directory inconsistent.

⚠ Note the `d3dkmt.c` call is only reached when `device->kmt_local` is set (`d3dkmt.c:97-101`), and
on native Windows with a real WDDM driver it **will** be set (§11). So the NULL call is reachable on
Helios in a way it is not under a stub D3DKMT.

`[INFER]` Expected failure: either `vkAllocateMemory` rejects the unknown-handle-type `pNext`
(`VK_ERROR_INVALID_EXTERNAL_HANDLE`), or a NULL-pointer call at `d3dkmt.c:118` /
`device.c:7651`. **UNVERIFIED** — nobody has run it. **Settling experiment:** a minimal D3D12
program that does `CreateCommittedResource` with `D3D12_HEAP_FLAG_SHARED` under vkd3d on the guest,
run from a **session-1 scheduled task**, with `VKD3D_DEBUG=warn VKD3D_LOG_FILE=Z:\tmp\vkd3d.log`.
Watch for the crash and for the log's last line.

⛔ **Until S1 is settled, no Phase-0 or gate run may include a shared-heap workload.** The vkd3d
suite has such tests (`tests/d3d12_dxvk_interop_device.c`, parts of `d3d12_resource.c`) — expect
them to be the crash class, not the fail class, and exclude them by name with
`VKD3D_TEST_EXCLUDE=` until fixed.

*Two ways to close it, in order of honesty:* (a) implement `VK_KHR_external_memory_win32` as a
**native** venus extension over the existing blob/res-id export machinery
(`vn_wsi_get_helios_resource_identity`, the blob res_id path) — the export machinery already exists,
this is the analogue of the semaphore twin; (b) upstream-shaped guard in vkd3d
(`if (!device->vk_info.KHR_external_memory_win32) return E_NOTIMPL;` at the three call sites),
which would be a legitimate second patch for `vkd3d-proton-helios` and is arguably an upstream bug
fix. ⛔ Do **not** "fix" it by making the ICD advertise the extension without implementing
`vkGetMemoryWin32HandleKHR` — that converts a crash into a silent wrong-handle, which is worse.

### 5.2 ⚠ S2 — no 32-bit (WOW64) Vulkan ICD

`[LIVE]`, re-verified this boot via `win_exec`:

```powershell
(Get-Item 'HKLM:\SOFTWARE\Khronos\Vulkan\Drivers').GetValueNames()
# → C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json
Test-Path 'HKLM:\SOFTWARE\WOW6432Node\Khronos\Vulkan\Drivers'
# → False
```

The ICD manifest declares `"library_arch": "64"`, `"api_version": "1.4.352"`, pointing at
`C:/ProgramData/HeliosVulkan/vulkan_virtio-b4408c6de1c2.dll`. **A 32-bit D3D12 client calls
`vkEnumeratePhysicalDevices` and gets zero devices**, so `d3d12_find_physical_device` cannot even
reach its `physical_devices[0]` fallback — `vkd3d_create_device` fails.

vkd3d-proton itself is built both ways by `package-release.sh` (`build.64` + `build.86`), and the
Linux host has the i686 mingw cross toolchain installed (§8.2), so the vkd3d side is free. The cost
is entirely on the venus ICD build + registration.

⚠ `3DMarkNightRaid.exe` ships a Win32 build, which would otherwise be a free WOW64 arm for `DX12.md`
phase P6. **Decide and write it down:** ship a 32-bit venus ICD, or declare Helios D3D12 64-bit-only
in `ARCHITECTURE.md` and in the INF (`R11` §1.5/§3 Variant B+ own the registration side).

### 5.3 S3 and S4 — the two cheap ones

**S3 — `VK_EXT_memory_budget` is one env var.** virglrenderer supports it `[LIVE]`; Mesa gates it
the other way round: `.EXT_memory_budget = VN_DEBUG(MEM_BUDGET)`
(`icd/mesa/src/virtio/vulkan/vn_physical_device.c:1553`), i.e. **off unless asked for**. Setting
`VN_DEBUG=mem_budget` in the client's environment turns it on and improves
`ID3D12Device::QueryVideoMemoryInfo` (`libs/vkd3d/memory.c:819-825`). Take it on day one; it costs
nothing and it is exactly the number a residency bug would be diagnosed from.

**S4 — `VK_EXT_descriptor_heap` is virglrenderer-only.** The guest protocol table has it
(`vn_protocol_driver_info.h:43`, `{"VK_EXT_descriptor_heap", 136, 1}`), Mesa venus enables it
(`vn_physical_device.c:1531`, `.EXT_descriptor_heap = !VN_DEBUG(NO_DESC_HEAP)`), the host GPU has
it — only `vkr_extension_table` lacks it, because the installed virglrenderer 1.3.0's venus-protocol
copy is older (185 names vs the guest's 187). **The fix is to resync virglrenderer's
`src/venus/venus-protocol/` and add the table row**, and it is also the vehicle for S5–S12: every
one of those needs the same resync plus its own protocol commands. `VKD3D_CONFIG=descriptor_heap`
then opts vkd3d in — the row is
`VK_EXTENSION_COND(EXT_DESCRIPTOR_HEAP, EXT_descriptor_heap, VKD3D_CONFIG_FLAG_STATIC(DESCRIPTOR_HEAP))`
at **`device.c:141`** (⚠ **not** `:145`, which is `AMD_SHADER_CORE_PROPERTIES`), declared at
`include/private/config_flag_decl.h:65`; it is **not** on by default.

---

## 6. Sparse / reserved resources, and raytracing

### 6.1 Sparse — supported end to end, and the tier walks to TIER_4

D3D12 reserved (tiled) resources need Vulkan sparse binding. Every layer says yes:

1. **Guest features `[CAPTURE]`** — `sparseBinding`, `sparseResidencyBuffer`,
   `sparseResidencyImage2D`, `sparseResidencyImage3D` (`:1275`), `sparseResidency{2,4,8,16}Samples`,
   `sparseResidencyAliased` (`:1280`), `shaderResourceResidency`, `shaderResourceMinLod` (`:1271`)
   — all `true`. `sparseProperties` (`:445-449`): `residencyStandard2DBlockShape=true`,
   `residencyStandard3DBlockShape=true`, `residencyAlignedMipSize=false`,
   `residencyNonResidentStrict=true`. `sparseAddressSpaceSize` = 1 TiB (`:295`).
2. **Guest queue families** — all six carry `SPARSE_BINDING`; §4.3 shows vkd3d lands on family 1.
3. **Guest ICD `[MESA]`** — `vn_QueueBindSparse()` at `icd/mesa/src/virtio/vulkan/vn_queue.c:2445`,
   with `vn_queue_bind_sparse_submit()` (`:2271`) / `…_batch()` (`:2298`) and the semaphore-feedback
   interlock at `:2340`.
4. **Protocol `[MESA]`** — `icd/mesa/src/virtio/venus-protocol/vn_protocol_driver_queue.h:1117-1310`
   encodes `VK_COMMAND_TYPE_vkQueueBindSparse_EXT`.
5. **Host `[VIRGL]`** — `vkr_dispatch_vkQueueBindSparse()` is registered and dispatched
   (virglrenderer 1.3.0 `src/venus/vkr_queue.c:385-398`, the body forwarding straight to
   `vk->QueueBindSparse` under `queue->vk_mutex` at `:394-397`; registration
   `dispatch->dispatch_vkQueueBindSparse = vkr_dispatch_vkQueueBindSparse;` at `:655`).
   ⚠ These are line numbers **into the 1.3.0 source tarball**, not into anything committed here —
   fetch it per the Conventions section before following them.

**Walking vkd3d's own tier function** — `d3d12_device_determine_tiled_resources_tier`,
`libs/vkd3d/device.c:9845-9868`, read in full this session:

| Clause | Requires | Guest | Result |
|---|---|---|---|
| `:9850-9855` | `sparseBinding` && `sparseResidencyAliased` && `sparseResidencyBuffer` && `sparseResidencyImage2D` && `residencyStandard2DBlockShape` && a sparse queue family with `queue_count > 0` | all yes, family 1 has 2 queues | not `TIER_NOT_SUPPORTED` |
| `:9857-9862` | `shaderResourceResidency` && `shaderResourceMinLod` && `!residencyAlignedMipSize` && `residencyNonResidentStrict` && `filterMinmaxSingleComponentFormats` | all yes | past `TIER_1` |
| `:9864-9866` | `sparseResidencyImage3D` && `residencyStandard3DBlockShape` | both yes | past `TIER_2` |
| `:9868` | — | — | **`D3D12_TILED_RESOURCES_TIER_4`** |

⚠ Note there is **no `TIER_3` branch** — the function returns `TIER_1`, `TIER_2`, or `TIER_4`.
`[INFER]` from `[CAPTURE]` + `[VKD3D]`; **UNVERIFIED** until a real device reports it (settle with
`d3d12.exe --test test_tiled_resources` or a `CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS)`
probe).

Nothing is lost if sparse were absent: the tier returns `TIER_NOT_SUPPORTED` and device creation
still succeeds; only FL 12.0 is lost (`device.c:10562-10567` needs `TiledResourcesTier >= 2`).
`VN_DEBUG=no_sparse` (`vn_physical_device.c:976-978`, masking at `:1777-1798`) is the clean A/B.

### 6.2 Raytracing — DXR 1.1 reachable, DXR 1.2 is not

Guest exposes `[CAPTURE]`: `VK_KHR_acceleration_structure` (rev 13), `VK_KHR_ray_tracing_pipeline`,
`VK_KHR_ray_query`, `VK_KHR_ray_tracing_maintenance1`, `VK_KHR_ray_tracing_position_fetch`,
`VK_KHR_deferred_host_operations` (rev 4), `VK_KHR_pipeline_library`,
`VK_EXT_pipeline_library_group_handles`; features `accelerationStructure`, `rayTracingPipeline`,
`rayTracingPipelineTraceRaysIndirect`, `rayTraversalPrimitiveCulling`, `rayQuery`,
`rayTracingMaintenance1`, `pipelineLibraryGroupHandles` all `true`.
(`accelerationStructureIndirectBuild` and `accelerationStructureHostCommands` are `false` — vkd3d
force-clears both anyway at `device.c:3380-3382`.)
Properties `[CAPTURE]:618-625`: `shaderGroupHandleSize=32`, `shaderGroupBaseAlignment=64`,
`shaderGroupHandleAlignment=32`, `maxRayHitAttributeSize=32`, `maxRayRecursionDepth=31`.

**Walking `d3d12_device_determine_ray_tracing_tier`** (`libs/vkd3d/device.c:9906-9979`, read in
full):

| Step | Requirement (line) | Guest | |
|---|---|---|---|
| Tier 1.0 entry | `rayTracingPipeline` && `accelerationStructure` && `maxRayHitAttributeSize >= 32` && `shaderGroupHandleSize == 32` && `shaderGroupBaseAlignment <= 64` && `shaderGroupHandleAlignment <= 32` (`:9936-9944`) | 32 / 32 / 64 / 32 | ✅ |
| Tier 1.0 formats | all six of `R32G32_SFLOAT`, `R32G32B32_SFLOAT`, `R16G16_SFLOAT`, `R16G16_SNORM`, `R16G16B16A16_SFLOAT`, `R16G16B16A16_SNORM` carry `FORMAT_FEATURE_ACCELERATION_STRUCTURE_VERTEX_BUFFER_BIT_KHR` in `bufferFeatures` (`:9917-9924`, checked by `:9946-9953`) | all six ✅ | → `TIER_1_0`, logs `"DXR support enabled."` |
| Tier 1.1 | `rayQuery` && `rayTraversalPrimitiveCulling` (`:9958`) + all seven extra formats `R16G16B16A16_UNORM`, `R16G16_UNORM`, `A2B10G10R10_UNORM_PACK32`, `R8G8B8A8_UNORM`, `R8G8_UNORM`, `R8G8B8A8_SNORM`, `R8G8_SNORM` (`:9926-9934`) | all ✅ | → `TIER_1_1`, logs `"DXR 1.1 support enabled."` |
| Tier 1.2 | `info->supports_opacity_micromap` ← `opacity_micromap_features.micromap` (`device.c:2655`, checked at `:9974`: `if (tier == D3D12_RAYTRACING_TIER_1_1 && info->supports_opacity_micromap)`) | ❌ **`VK_KHR_opacity_micromap`** absent — and absent from the *host GPU* too (§4.2) | capped at **1.1** |

⇒ **`D3D12_RAYTRACING_TIER_1_1`.** `[INFER]` from `[CAPTURE]` + `[VKD3D]`; settle with
`VKD3D_DEBUG=info` and grep for the two `INFO` strings above.

If RT were absent vkd3d does **not** refuse: `RaytracingTier` stays `TIER_NOT_SUPPORTED` and only
FL 12.2 is lost (`device.c:10579`). Two clean A/Bs exist and they are symmetric:
`VKD3D_CONFIG=nodxr` (disables the extensions in vkd3d's own table — the `DISABLE_COND(…NO_DXR)`
rows at `device.c:71-75`, plus `:98`, `:126` and `:161`) and `VN_DEBUG=no_ray_tracing` (clears
`physical_dev->ray_tracing`, gating the RT rows of the passthrough table).
⚠ `VKD3D_CONFIG=dxr12` exists (`config_flag_decl.h:32`) and enables experimental DXR 1.2 *if*
**`VK_KHR_opacity_micromap`** is available — it is not, on either the guest or the host, so the flag
is inert here.

### 6.3 `ResourceHeapTier` — genuinely open

`d3d12_device_determine_heap_tier` (`device.c:9983-10008`, wired at `:10190`) has two ways to fail to
`TIER_1` — the clauses are `:9996-9998` and `:10003-10005`:

```c
if ((limits->bufferImageGranularity > D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT) ||
        !(non_cpu_domain->buffer_type_mask & non_cpu_domain->sampled_type_mask & non_cpu_domain->rt_ds_type_mask))
    return D3D12_RESOURCE_HEAP_TIER_1;

if (!device->device_info.pageable_device_memory_features.pageableDeviceLocalMemory &&
        !(fallback_domain->buffer_type_mask & fallback_domain->sampled_type_mask & fallback_domain->rt_ds_type_mask))
    return D3D12_RESOURCE_HEAP_TIER_1;
```

The first clause passes (`bufferImageGranularity` = 1024 ≤ 64 KiB). The second is the one Helios is
exposed to: **`VK_EXT_pageable_device_local_memory` is absent** (S9), so the result depends entirely
on whether the runtime-computed `fallback_domain` memory-type masks intersect across
buffer/sampled/RT-DS on the guest's five memory types. **UNVERIFIED** — settle with a
`CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS).ResourceHeapTier` probe, or `VKD3D_DEBUG=info`.
This is the concrete D3D12 consequence of S9 and the reason S9 is not merely "residency quality".

---

## 7. ✅ The shader-model / `driverID` question (`DECISIONS.md` H5) — CLOSED 2026-08-05

**Answer first: the swizzle fires. The nested `VkPhysicalDeviceDriverProperties` carries
`VK_DRIVER_ID_NVIDIA_PROPRIETARY`, and a live vkd3d device on this guest reports SM 6.8 and
FL 12_2.** Everything below is retained because the *mechanism* still has to be understood by anyone
touching shader caps — but §7.4's three candidate fixes are moot, and the "plan for SM 6.0" hedge is
retired. Evidence: `tmp/dx12/gates/H5/driverid-probe.txt` (the Vulkan-level prediction) and
`tmp/dx12/gates/G1/{bridge_probe.txt,vkd3d.log}` (the D3D12-level confirmation).

⛔ **One correction the walk below got wrong: the ladder does not stop at 6.7.** `device.c:10817-10820`
adds an unconditional 6.7 → 6.8 step, which §7.2's table omitted. The guest logs
`Enabling support for SM 6.6.` → `6.7.` → `6.8.`, and `CheckFeatureSupport(D3D12_FEATURE_SHADER_MODEL)`
answers 6.8. **Canonical: SM 6.8.**

### 7.1 The chain, in full

**(a) The gate.** vkd3d gates Shader Model 6.2 — and therefore 6.3/6.5/6.6/6.7, which chain off it —
on FP32 denorm control. `libs/vkd3d/device.c:10692-10704`, verbatim:

```c
        /* DXIL allows control over denorm behavior for FP32 only.
         * shaderDenorm handling appears to work just fine on NV, despite the properties struct saying otherwise.
         * Assume that this is just a driver oversight, since otherwise we cannot expose SM 6.2 there ... */
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

The NVIDIA exemption is the `!=` on line **10699**.

**(b) The guest values** `[CAPTURE]`:
`denormBehaviorIndependence = SHADER_FLOAT_CONTROLS_INDEPENDENCE_ALL` (`:719`),
`shaderDenormPreserveFloat32 = false` (`:725`), `shaderDenormFlushToZeroFloat32 = false`
(`:728`), `driverID = DRIVER_ID_MESA_VENUS` (`:711`). **The host's values are identical except
`driverID = NVIDIA_PROPRIETARY`** — so the properties are a faithful passthrough and "make venus
report the host's real denorm properties" is *not* a fix; the host reports `false` too.

Naively that caps Helios at `D3D_SHADER_MODEL_6_0`, and because FL 12.2 needs
`max_shader_model >= D3D_SHADER_MODEL_6_5` (`device.c:10572`), the feature level would cap at 12.1.

**(c) The escape hatch, and its ordering is proven.** vkd3d already handles layered implementations
via `VK_KHR_maintenance7`. `libs/vkd3d/device.c:2323-2343` builds the chain (read verbatim this
session — note the nested `properties` member, which the probe must reproduce exactly):

```c
    if (vulkan_info->KHR_maintenance7)
    {
        ...
        layered_props.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_LAYERED_API_PROPERTIES_KHR;
        /* assume a potentially single-layered implementation ... */
        layered_props_list.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_LAYERED_API_PROPERTIES_LIST_KHR;
        layered_props_list.layeredApiCount = 1;
        layered_props_list.pLayeredApis = &layered_props;
        vk_prepend_struct(&info->properties2, &layered_props_list);

        vk_layered_props.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_LAYERED_API_VULKAN_PROPERTIES_KHR;
        vk_prepend_struct(&layered_props, &vk_layered_props);

        real_driver_props.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES_KHR;
        vk_prepend_struct(&vk_layered_props.properties, &real_driver_props);   /* ← into properties2, not the layer */
    }
```

and `:2657-2664` swizzles:

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

**Ordering, verified this session:** the swizzle is inside `vkd3d_physical_device_info_init`
(opens `device.c:2066`), which is called at **`device.c:4129`**; shader-model caps init is
`d3d12_device_caps_init_shader_model` (opens `:10640`), called from `d3d12_device_caps_init`
(`:10939`, line `:10941`), which is called at **`device.c:11599`**. **4129 < 11599 — the swizzle
runs first.** That much is *not* in question.

**(d) What the guest reports live** `[CAPTURE]:535-543`:

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

and the ICD fills that struct **before** it rewrites `driverID`: `vn_physical_device.c:870`
(`layer->driver.driverID = props->driverID;`) runs inside `vn_physical_device_init_properties()`,
whereas `vn_physical_device_sanitize_properties()` — which does
`props->driverID = VK_DRIVER_ID_MESA_VENUS;` at **`:571`** — is only called at **`:905`**.

**(e) ✅ Observed 2026-08-05.** `vulkaninfo` prints the layered list but does **not** chain the nested
`VkPhysicalDeviceDriverProperties`, which is why this sat UNVERIFIED. `tools/vk_layered_driverid_probe.cpp`
chains it and prints `NESTED driverID = 4 (NVIDIA_PROPRIETARY) driverName=NVIDIA`, with
`VK_KHR_maintenance7 = PRESENT` and `layerVendorID = 0x10de`. ⇒ `real_driver_props.driverID ==
VK_DRIVER_ID_NVIDIA_PROPRIETARY` is now **measured**, and the D3D12-level consequence is confirmed
independently at `D12-G1`. **Canonical: SM 6.8.**

### 7.2 The full shader-model ladder, walked against the live values

`d3d12_device_caps_init_shader_model()` (`device.c:10640-10805`) is a strict ladder — each step is
gated on `max_shader_model == <previous>`, so one failure freezes everything above it.

| Step | Gate (lines) | Live guest | Result |
|---|---|---|---|
| **6.0** | `subgroupSize >= 4`; subgroup ops ⊇ ARITHMETIC\|BASIC\|BALLOT\|SHUFFLE\|QUAD\|VOTE; stages ⊇ COMPUTE\|FRAGMENT; `scalarBlockLayout \|\| uniformBufferStandardLayout`; `shaderInt16` (`:10665-10670`) | 32; all 11 ops; all 14 stages; both true; true | ✅ |
| **6.2** | `denormBehaviorIndependence != NONE` **&&** (`driverID == NVIDIA_PROPRIETARY` **\|\|** (`shaderDenormFlushToZeroFloat32 && shaderDenormPreserveFloat32`)) (`:10693-10704`) | `INDEPENDENCE_ALL`; both denorm bits **false**; `driverID` = **§7.1's open question** | ⚠ **hinges entirely on §7.1** |
| **6.3** | unconditional once 6.2 (`:10716-10721`) | — | follows 6.2 |
| **6.5** | unconditional once 6.3 (`:10739-10745`) | — | follows 6.2 |
| **6.6** | (`computeDerivativeGroupLinear` \|\| `driverID == NVIDIA`) && `shaderBufferInt64Atomics` && `shaderInt8` && required-subgroup-size for COMPUTE (`:10759-10770`) | `computeDerivativeGroupLinear=true` (`[CAPTURE]:1327`); `shaderBufferInt64Atomics=true` (`:1636`); `shaderInt8=true` (`:1639`); `subgroupSizeControl=true` (`:1688`) | ✅ *if* 6.2 passed |
| **6.7** | `shaderMaximalReconvergence && shaderQuadControl`, **or** `VKD3D_CONFIG=enable_experimental_features` (`:10794-10797`) | both `true` (`[CAPTURE]:1571`, `:1575`) | ✅ *if* 6.6 passed |
| **6.8** | ⛔ **unconditional once 6.7** (`:10817-10820`, `if (max_shader_model == D3D_SHADER_MODEL_6_7) … = D3D_SHADER_MODEL_6_8;`) — this row was missing from the original walk, which is why the doc set said "the ladder walks to 6.7" | — | ✅ **observed live**: `Enabling support for SM 6.8.` |

⚠ **Correction to a natural assumption: `VK_KHR_maintenance8` does NOT gate SM 6.7.** The code
(read verbatim at `:10794-10797`) requires only maximal-reconvergence + quad-control.
`maintenance8` appears solely at `device.c:10418-10419` — `options14->AdvancedTextureOpsSupported =
max_shader_model >= 6_7 && (maintenance8 || experimental)` — plus
`resource.c:690` and `command.c:9887`. The profile's `shader_model_67` set lists it, but the profile
is aspirational (§3.4). `options14->WriteableMSAATexturesSupported` also needs
`shaderStorageImageMultisample`, which the guest reports `true` (`[CAPTURE]:1258`).

⇒ **The whole shader-model story reduces to one bit.** ✅ **The bit is a 1**: the swizzle fires and the
ladder runs 6.2 → 6.3 → 6.5 → 6.6 → 6.7 → **6.8**. (Had it not fired: SM 6.0, FL capped at 12.1.)

⚠ **Canonical phrasing, superseding `DECISIONS.md` §6.1's original answer: SM 6.8**, observed on a
live vkd3d device (`D12-G1`, `VKD3D_DEBUG=info` + `CheckFeatureSupport`). The old hedge ("plan for
6.0, treat anything above as upside") is retired with H5.
⚠ **What still matters at feature-level granularity is 6.5**, not 6.8: the FL 12.2 gate asks
`>= D3D_SHADER_MODEL_6_5` (`device.c:10572`). Everything above 6.5 is shader-feature reach, not
feature level — so a later regression that dropped the ceiling to 6.5 would not show up as an FL
change. If shader-model coverage is ever a gate criterion, assert on the
`Enabling support for SM …` lines, not on `MaxSupportedFeatureLevel`.

**Other `driverID`-conditional vkd3d paths** that will behave differently depending on the answer —
worth knowing because they are not all cosmetic: `device.c:1883`, `:1912`, `:1921`, `:1937` (memory
model), `:3224`, `:3961-3986` (the switch where `VK_DRIVER_ID_MESA_VENUS` is explicitly grouped with
MoltenVK/Dozen under "layered implementations are handled transparently"), `:4144`
(`vkd3d_driver_has_fast_concurrent_transfer_queue`), `:10163`, `:10470-10472`, `:11097`, `:11191`,
`:11417`; `command.c:180`, `:417`, `:11636-11638`; `resource.c:426-437`, `:647`, `:5265`;
`memory.c:2013`. ⚠ And `state.c:3143`, `raytracing_pipeline.c:1934`, `workgraphs.c:2194` feed
`compile_args.driver_id` into **dxil-spirv**, which changes its SPIR-V output per driver. So the
answer changes generated shader code, not only an advertised number.

### 7.2b The FL 12.2 predicate, walked conjunct by conjunct

`d3d12_device_caps_init_feature_level` gates `D3D_FEATURE_LEVEL_12_2` on a **twelve-conjunct**
predicate at `device.c:10572-10582` (assignment at `:10583`). Naming only the four or five
interesting ones is how a "the substrate reaches 12.2" claim goes stale, so here is every clause,
with its source in vkd3d and its live value.

| # | Conjunct (line) | Where vkd3d sets it | Live guest | |
|---|---|---|---|---|
| 1 | `max_feature_level >= 12_1` (`:10572`) | the whole ladder below it in `d3d12_device_caps_init_feature_level` (`:10549`): **11.1** at `:10557-10560` (`OutputMergerLogicOp` + `vertexPipelineStoresAndAtomics` + `maxPerStageDescriptorStorage{Buffers,Images} >= D3D12_UAV_SLOT_COUNT`), **12.0** at `:10562-10566` (`TiledResourcesTier >= 2`, `ResourceBindingTier >= 2`, `TypedUAVLoadAdditionalFormats`), **12.1** at `:10568-10570` (`ROVsSupported` + `ConservativeRasterizationTier >= 1`) | `logicOp=true` (`[CAPTURE]:1236`), `vertexPipelineStoresAndAtomics=true` (`:1253`), both storage limits 1 048 576 ≫ 64 (`:299`, `:301`); TiledResourcesTier **4**, ResourceBindingTier **3**; ROVs `true` — `fragmentShaderPixelInterlock` + `fragmentShaderSampleInterlock` at `:10180-10181`, both `true` (`[CAPTURE]:1425-1426`); ConsRast **Tier 3**. ⚠ `TypedUAVLoadAdditionalFormats` is the one clause **not** decidable from the capture: `d3d12_device_determine_additional_typed_uav_support` (`:10010`, wired at `:10179`) issues live `vkGetPhysicalDeviceFormatProperties` calls, and `vulkaninfo --summary` does not carry them — booked as **U14** | ⚠ one clause open |
| 2 | `max_shader_model >= D3D_SHADER_MODEL_6_5` (`:10572`) | §7.2 ladder | ✅ **6.8** — §7.1 is closed | ✅ |
| 3 | `VPAndRTArrayIndexFromAnyShaderFeedingRasterizerSupportedWithoutGSEmulation` (`:10573`) | `:10187-10189` = `shaderOutputViewportIndex && shaderOutputLayer` | both `true` (`[CAPTURE]:1675-1676`) | ✅ |
| 4 | `options1.WaveOps` (`:10574`) | `:10197` = `max_shader_model >= D3D_SHADER_MODEL_6_0` | SM 6.0 passes unconditionally (§7.2 row 1) | ✅ |
| 5 | `options1.Int64ShaderOps` (`:10574`) | `:10231` = `features2.features.shaderInt64` | `true` (`[CAPTURE]:1268`) | ✅ |
| 6 | `options2.DepthBoundsTestSupported` (`:10574`) | `:10241` = `features->depthBounds` | `true` (`[CAPTURE]:1242`) | ✅ |
| 7 | `options3.CopyQueueTimestampQueriesSupported` (`:10575`) | `:10250` = `!!queue_families[VKD3D_QUEUE_FAMILY_TRANSFER]->timestamp_bits` | COPY lands on Vulkan family 1 (§4.3), whose `timestampValidBits = 64` (`[CAPTURE]:1112`) | ✅ |
| 8 | `options3.CastingFullyTypedFormatSupported` (`:10575`) | `:10251` — hardcoded `TRUE` | — | ✅ unconditional |
| 9 | `options.ResourceBindingTier >= TIER_3` (`:10576`) | `:10177` — hardcoded `D3D12_RESOURCE_BINDING_TIER_3` | — | ✅ unconditional |
| 10 | `options.ConservativeRasterizationTier >= TIER_3` (`:10577`) | `d3d12_device_determine_conservative_rasterization_tier`, `:9870-9884`: needs `degenerateTrianglesRasterized` **and** `fullyCoveredFragmentShaderInputVariable` | both `true` (`[CAPTURE]:482`, `:484`) ⇒ **TIER_3** | ✅ |
| 11 | `options.TiledResourcesTier >= TIER_3` (`:10578`) | §6.1 walk | **TIER_4** | ✅ |
| 12 | `options5.RaytracingTier >= TIER_1_1` (`:10579`) | §6.2 walk | **TIER_1_1** | ✅ |
| 13 | `options6.VariableShadingRateTier >= TIER_2` (`:10580`) | `d3d12_device_determine_variable_shading_rate_tier`, `:1777-1793`, over the two predicates at `:1738-1744` and `:1767-1776` | tier 1: `pipelineFragmentShadingRate=true` (`[CAPTURE]:1431`) + `framebufferColorSampleCounts` has `SAMPLE_COUNT_2_BIT` (`:373-377`). tier 2: `fragmentShadingRateNonTrivialCombinerOps=true` (`:515`), `attachmentFragmentShadingRate=true` (`:1433`), `primitiveFragmentShadingRate=true` (`:1432`), and `d3d12_determine_shading_rate_image_tile_size` (`:1746-1765`) returns **16** because min == max texel size == 16×16 (`:506-511`) and 16 ∈ {8,16,32} ⇒ **TIER_2** | ✅ |
| 14 | `options7.MeshShaderTier >= TIER_1` (`:10581`) | `:10340` | `VK_EXT_mesh_shader` present | ✅ |
| 15 | `options7.SamplerFeedbackTier >= TIER_0_9` (`:10582`) | `d3d12_device_determine_sampler_feedback_tier`, `:10060-10069`, wired at `:10341` | see the correction below ⇒ **TIER_0_9** | ✅ |

(Fifteen rows for a twelve-conjunct predicate because clauses 4-6 and 7-8 share source lines.)

⛔ **Correction of record: `SamplerFeedbackTier` is *not* an exception on this guest, and vkd3d does
implement it.** `d3d12_device_determine_sampler_feedback_tier` (`device.c:10060-10069`) returns
`D3D12_SAMPLER_FEEDBACK_TIER_0_9` — with the comment `/* Enough for FL 12.2. */` — whenever
`features2.features.shaderInt64` and
`shader_image_atomic_int64_features.shaderImageInt64Atomics` are both set. The guest has both
(`[CAPTURE]:1268`, `:1566`). The FL 12.2 gate at `:10582` asks for `>= TIER_0_9`, which is exactly
what the function returns. The `"(TODO: missing sampler feedback)"` string that produced the old
claim lives in the **`description` fields of the two FL-12.2 profiles inside
`VP_D3D12_VKD3D_PROTON_profile.json`** — `:754` (`VP_D3D12_FL_12_2_baseline`) and `:771`
(`VP_D3D12_FL_12_2_optimal`) — i.e. it describes what the *profile document* does not yet encode,
not what the driver does not implement.

⇒ ✅ **All twelve conjuncts pass on the live guest, and the predicate is no longer a prediction.**
`D12-G1` read the answer straight off a real vkd3d device:
`CheckFeatureSupport(D3D12_FEATURE_FEATURE_LEVELS)` → **`MaxSupportedFeatureLevel = 12_2`**, and
`VKD3D_DEBUG=info` printed `DX Ultimate supported!` (`device.c:10588`), which is the `INFO` emitted
on the FL 12.2 arm.

⚠ **U14 is settled as a side effect, and it settled the way this section predicted:**
`D3D12_FEATURE_D3D12_OPTIONS` reports **`TypedUAVLoadAdditionalFormats = 1`** — the one clause that
needed live `vkGetPhysicalDeviceFormatProperties` calls and could not be read from the
`vulkaninfo` capture. The rest of the same query also matches this walk exactly:
`ResourceBindingTier = 3`, `TiledResourcesTier = 4`, `ConservativeRasterizationTier = 3`,
`ROVsSupported = 1`, and `OPTIONS5.RaytracingTier = 11` (`D3D12_RAYTRACING_TIER_1_1`). Nothing in the
predicate walk needs re-deriving.

### 7.3 The probe that settles it

`tools/vk_layered_driverid_probe.cpp` — new file, ~40 lines, read-only, no build of vkd3d needed.
It must reproduce vkd3d's chain **in the one respect that decides the answer**: the driver
properties hang off `vk_layered_props.properties.pNext` (a *nested* `VkPhysicalDeviceProperties2`),
not off the layer struct.

⚠ **One deliberate difference from vkd3d, and it is the reason this note exists.** vkd3d does **not**
set the nested `properties.sType`: `device.c:2318-2321` `memset`s all four structs to zero and
`:2338-2342` sets only `vk_layered_props.sType` before `vk_prepend_struct(&vk_layered_props.properties,
&real_driver_props)` — so `vk_layered_props.properties.sType` stays **0** on the wire. The probe
below matches that (it leaves the field zeroed) so that probe and engine cannot disagree. It is
harmless against the current ICD either way — venus's
`vn_GetPhysicalDeviceProperties2` layered path copies struct guts and then walks
`layered_vk_props->properties.pNext` **without inspecting the base sType**
(`icd/mesa/src/virtio/vulkan/vn_physical_device.c:2237-2253`, the `vk_foreach_struct` at `:2240`) —
but an ICD that ever gated on that sType would make a probe that sets it succeed where vkd3d fails,
which is the worst possible outcome for a probe whose entire job is to predict vkd3d.

```cpp
// tools/vk_layered_driverid_probe.cpp — settles DECISIONS.md H5.
// Chains VkPhysicalDeviceLayeredApiPropertiesListKHR -> ...VulkanPropertiesKHR ->
// VkPhysicalDeviceDriverProperties exactly as vkd3d-proton does at
// vkd3d-proton-helios/libs/vkd3d/device.c:2323-2343, and prints the nested driverID.
#include <vulkan/vulkan.h>
#include <stdio.h>

int main(void)
{
    VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO };
    app.pApplicationName = "vk_layered_driverid_probe";
    app.apiVersion = VK_API_VERSION_1_3;              /* same floor vkd3d uses */
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO };
    ici.pApplicationInfo = &app;
    VkInstance inst = VK_NULL_HANDLE;
    if (vkCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) { printf("vkCreateInstance failed\n"); return 1; }

    uint32_t n = 0; vkEnumeratePhysicalDevices(inst, &n, NULL);
    if (!n) { printf("ZERO physical devices (32-bit ICD? see SUBSTRATE.md S2)\n"); return 1; }
    VkPhysicalDevice pd[8]; if (n > 8) n = 8; vkEnumeratePhysicalDevices(inst, &n, pd);

    for (uint32_t i = 0; i < n; i++) {
        VkPhysicalDeviceDriverProperties real = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES };
        VkPhysicalDeviceLayeredApiVulkanPropertiesKHR vkl = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_LAYERED_API_VULKAN_PROPERTIES_KHR };
        /* NOTE: vkl.properties.sType is deliberately left 0 — vkd3d leaves it 0 too
         * (device.c:2318-2321 memsets, :2338-2342 sets only the outer sType), and venus
         * does not read it (vn_physical_device.c:2240). Match the engine, not the spec. */
        vkl.properties.pNext = &real;                       /* ← the nested properties2, as vkd3d does */
        VkPhysicalDeviceLayeredApiPropertiesKHR layer = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_LAYERED_API_PROPERTIES_KHR };
        layer.pNext = &vkl;
        VkPhysicalDeviceLayeredApiPropertiesListKHR list = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_LAYERED_API_PROPERTIES_LIST_KHR };
        list.layeredApiCount = 1; list.pLayeredApis = &layer;
        VkPhysicalDeviceDriverProperties top = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES };
        top.pNext = &list;
        VkPhysicalDeviceProperties2 p2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2 };
        p2.pNext = &top;
        vkGetPhysicalDeviceProperties2(pd[i], &p2);

        printf("pd[%u] %s\n  top driverID   = %u (%s)\n  layeredApiCount= %u  layerVendor=0x%04x layerName=%s\n"
               "  NESTED driverID= %u (%s)  ==> vkd3d %s swizzle -> SM %s\n",
               i, p2.properties.deviceName, top.driverID, top.driverName,
               list.layeredApiCount, layer.vendorID, layer.deviceName,
               real.driverID, real.driverName,
               real.driverID ? "WILL" : "will NOT",
               real.driverID == VK_DRIVER_ID_NVIDIA_PROPRIETARY ? "6.6+ (FL 12_2)" : "6.0 (FL 12_1)");
    }
    return 0;
}
```

**Build and run** (VM, `win_exec`; the Vulkan SDK is present — `[LIVE]` this session,
`C:\VulkanSDK\1.4.350.0`). This mirrors the pattern `tools/helios-ownership-soak.ps1:43-45` uses:

```powershell
$sdk = 'C:\VulkanSDK\1.4.350.0'
$dir = 'C:\Users\Rupansh\helios-probe'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
Copy-Item Z:\tools\vk_layered_driverid_probe.cpp $dir -Force
$vcvars = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
# ⛔ Build the command string FIRST, then run it. See the trap note below.
$build = "call `"$vcvars`" >nul && cd /d `"$dir`" && cl /nologo /EHsc /W4 /O2 /I`"$sdk\Include`" " +
         "vk_layered_driverid_probe.cpp /link /LIBPATH:`"$sdk\Lib`" vulkan-1.lib /OUT:`"$dir\vkdrvid.exe`""
cmd /c $build
& "$dir\vkdrvid.exe"
```

⛔ **PowerShell trap, verified live this session: `cmd /c "…" + "…"` does NOT concatenate.** In
PowerShell *argument mode* — which is what you are in after `cmd /c` — the `+` is not an operator; it
is passed through as a literal token. `cmd /c "echo AAA " + "BBB"` prints **`AAA  + BBB`**. Written
that way, `cl` receives a stray `+` as a source-file argument and the compile fails with a confusing
error. Assigning to `$build` first puts the `+` in *expression* mode, where it is string
concatenation — which is exactly why `tools/helios-ownership-soak.ps1:43-45` builds `$build` and then
runs `cmd /c $build`. Every multi-line `cmd /c` in this document follows that shape.

No window, so `win_exec` (session 0) is fine.

**Reading the result:**

- prints `NESTED driverID = 4 (NVIDIA)` ⇒ the swizzle fires. **SM 6.6 at minimum — §7.1's ladder
  walks to 6.7 — and FL 12_2 is on the table, with no fix needed.** Confirm end-to-end later with
  `VKD3D_DEBUG=info` and grep
  `"Enabling support for SM 6.6."` — ⚠ **not** `"…SM 6.2."`, which is a `TRACE` and will not appear
  at `info` (§1.2 D).
- prints `NESTED driverID = 0` or `MESA_VENUS` ⇒ the ceiling is SM 6.0 / FL 12_1 and one of the
  fixes below is required.

✅ **It printed `NESTED driverID = 4 (NVIDIA_PROPRIETARY)` on 2026-08-05, and the end-to-end
confirmation landed the same day at `D12-G1`:** `Enabling support for SM 6.6.` / `6.7.` / `6.8.` and
`DX Ultimate supported!` in `tmp/dx12/gates/G1/vkd3d.log`, with `HighestShaderModel = 6.8` and
`MaxSupportedFeatureLevel = 12_2` from `CheckFeatureSupport`. **The as-built probe** (committed at
`tools/vk_layered_driverid_probe.cpp`) also prints whether `VK_KHR_maintenance7` is advertised,
because a `0` answer is not attributable without it — vkd3d only builds the layered chain when the
extension is present (`device.c:2323`).

### 7.4 ⛔ The three candidate fixes — ALL MOOT (H5 closed, 2026-08-05)

**Rank 1 is what turned out to be true**: the ICD's layered chain already reports the host driver
honestly, so there was nothing to repair on either side. Ranks 2 and 3 must **not** be applied — a
`device.c:10699` fork patch would now be dead code guarding a condition that never occurs, and
`VKD3D_SHADER_MODEL=6_6` would be an override that lowers the real ceiling of 6.8. The table is kept
because it is the right analysis to re-run if a host, ICD or virglrenderer change ever makes the
nested `driverID` go to zero.

⚠ **The regression test for that is one line**: rebuild and re-run
`tools/vk_layered_driverid_probe.cpp`, or grep a `VKD3D_DEBUG=info` log for
`Enabling support for SM 6.6.`. Do not infer it from `MaxSupportedFeatureLevel`, which only needs
SM 6.5 (§7.2b).

| Rank | Fix | Where | Why this order |
|---|---|---|---|
| **1** | **Make the layered chain report the real driver** — i.e. verify/repair the ICD side so `VkPhysicalDeviceLayeredApiVulkanPropertiesKHR`'s nested `VkPhysicalDeviceDriverProperties` carries the host's `driverID` | `icd/mesa/src/virtio/vulkan/vn_physical_device.c:851-883` (the layered fill) vs `:571`/`:905` (the sanitize that rewrites `driverID`) | This is the *honest* fix: `maintenance7` exists precisely to let a layered implementation tell the truth about what is underneath, vkd3d already consumes it, and it fixes every `driverID`-conditional path in §7.2 at once — including the `driver_id` handed to dxil-spirv. If the probe says the chain already works, there is nothing to do. |
| **2** | **A `vkd3d-proton-helios` patch at `device.c:10699`**, extending the exemption to the layered-venus case | `libs/vkd3d/device.c:10699` | ⚠ **This would be the first real content of the fork** (`DX12.md` §3.3: the submodule is byte-identical to upstream today). ⛔ It must be **conditioned on something venus can actually observe about the host, never hardcoded to `MESA_VENUS`** (`DECISIONS.md` §5). The observable is already in hand: `VkPhysicalDeviceLayeredApiPropertiesKHR::vendorID == 0x10de` with `layeredAPI == VULKAN_KHR` (`[CAPTURE]:539-542`). A patch reading *that* is defensible and upstreamable; `driverID == MESA_VENUS ⇒ exempt` is not, because venus over an AMD or Intel host has different denorm semantics. |
| **3** | **`VKD3D_SHADER_MODEL=6_6`** as a measurement unblocker | env var, `d3d12_device_caps_shader_model_override()`, `device.c:10591-10637`, env read at `:10617` | It is an **override, not a fix** — it raises `device->d3d12_caps.max_shader_model` unconditionally with no backing check. Acceptable to unblock a *diagnostic* run; ⛔ **never in a gate run** (same class as `VKD3D_FEATURE_LEVEL`, §9.3). |

---

## 8. Building vkd3d-proton for Windows

### 8.1 ✅ Step zero: the nested submodules — DONE 2026-08-05

They were uninitialised (a leading `-` in `git submodule status`, all three directories empty), and
`meson.build:177-178` does `subproject('dxil-spirv')` while `meson.build:62` includes
`./khronos/Vulkan-Headers/include` + `./khronos/SPIRV-Headers/include`, so nothing configured until:

```bash
cd /home/rupansh/helios-vgpu/vkd3d-proton-helios && git submodule update --init --recursive
```

Now populated: `khronos/SPIRV-Headers f88a2d76`, `khronos/Vulkan-Headers 0e9de566` (v1.4.351),
`subprojects/dxil-spirv cc75a0c9`.

⚠ **`--recursive` was load-bearing: `dxil-spirv` has four nested submodules of its own** — this
settles the UNVERIFIED item `GATES.md` G0 booked. From `subprojects/dxil-spirv/.gitmodules`:

| Path | Upstream | At |
|---|---|---|
| `subprojects/dxbc-spirv` | `github.com/doitsujin/dxbc-spirv` | `d5b06435` |
| `third_party/SPIRV-Cross` | `KhronosGroup/SPIRV-Cross` | `4b7bcb7e` |
| `third_party/SPIRV-Tools` | `KhronosGroup/SPIRV-Tools` | `199cb207` |
| `third_party/spirv-headers` | `KhronosGroup/SPIRV-Headers` | `c63848ec` |

⇒ **seven repositories link into `helios_vkd3d.dll`**, not three, and each carries its own licence
(`ARCHITECTURE.md` §7.4's component table, UNVERIFIED-10, needs all seven rows).

✅ **The fork is wired.** The checkout's `origin` is still `HansKristian-Work/vkd3d-proton`; the
Helios fork named by the superproject's `.gitmodules` is now a second remote:

```bash
git -C vkd3d-proton-helios remote add helios git@github-rupansh:rupansh/vkd3d-proton
git -C vkd3d-proton-helios push -u helios helios     # branch `helios`, forked at 2c7ba22c
```

⛔ **Push to `helios`, never to `origin`.** The Helios branch is `helios`; the submodule now points
at `fc35d37d` (D4's two exports, `ARCHITECTURE.md` §7.4), so `DX12.md` §3.3's "zero local commits,
clean tree" is history.

### 8.2 The **PRIMARY** arm: mingw cross on the Linux host — zero installs, today

⚠ **This is the decided default** (`DECISIONS.md` §6.1: *"Linux mingw cross is the primary"*), for
two reasons and not as a preference: the Linux host already has the entire toolchain on `PATH`, so it
needs **zero installation**; and it is **the configuration vkd3d-proton itself ships**
(`.github/workflows/artifacts.yml`), so a failure there is a Helios failure rather than an
unsupported-configuration failure. Native MSVC on the VM (§8.3) is the **fallback, taken when a
Windows debugger is wanted** — `GATES.md` G0 and `ARCHITECTURE.md` §8.3 must both say the same.

`[LIVE]`, checked this session on the Linux host — **every dependency is already installed**:

| Tool | Path | Version |
|---|---|---|
| `x86_64-w64-mingw32-gcc` / `-g++` / `-ar` | `/usr/bin/` | GCC **16.1.0** |
| `i686-w64-mingw32-gcc` / `-g++` | `/usr/bin/` | present (the 32-bit arm for S2) |
| `widl` | `/usr/bin/widl` | from wine — and `meson.build:73-77` looks for plain `widl` **first** |
| `glslangValidator` / `glslang` | `/usr/bin/` | 11:16.4.0 (`meson.build:83-88` finds either) |
| `meson` | `/usr/bin/meson` | 1.11.2 (needs ≥ 0.49) |
| `ninja` | `/usr/bin/ninja` | present |

So the shipping build works with no installation at all:

```bash
cd /home/rupansh/helios-vgpu/vkd3d-proton-helios
git submodule update --init --recursive
./package-release.sh helios /home/rupansh/helios-vgpu/tmp/dx12/vkd3d --no-package
# → tmp/dx12/vkd3d/vkd3d-proton-helios/x64/{d3d12.dll,d3d12core.dll}
#   tmp/dx12/vkd3d/vkd3d-proton-helios/x86/{d3d12.dll,d3d12core.dll}
```

⛔ **That command is not re-runnable as written.** `package-release.sh:17-20` aborts before doing
anything if its build directory already exists:

```sh
if [ -e "$VKD3D_BUILD_DIR" ]; then          # $VKD3D_BUILD_DIR = $(realpath "$2")/vkd3d-proton-$1
  echo "Build directory $VKD3D_BUILD_DIR already exists"
  exit 1
fi
```

With `helios` as the version argument that path is `<dst>/vkd3d-proton-helios`, so **any** second
invocation — including the retry after a failed first one — exits 1 until the directory is removed by
hand. Make the removal part of the command so a retry is one line:

```bash
rm -rf /home/rupansh/helios-vgpu/tmp/dx12/vkd3d/vkd3d-proton-helios
./package-release.sh helios /home/rupansh/helios-vgpu/tmp/dx12/vkd3d --no-package
```

⚠ Do not point `<dst>` at the repo root or anywhere near `vkd3d-proton-helios/` itself — the output
directory is named `vkd3d-proton-helios` and an `rm -rf` on the wrong parent deletes the checkout.

`package-release.sh` (read in full) does, per arch (`:53-77`, `:90-93`):

```
meson setup --cross-file build-win{64,32}.txt --buildtype release --prefix <dst> \
      --strip --bindir x{64,86} --libdir x{64,86} <builddir>   &&   ninja install
```

The cross files pin `x86_64-w64-mingw32-gcc` etc. and
`widl-mingw-tools-fallback` (`build-win64.txt`, `build-win32.txt`).

⚠ **`--no-package` also skips `--dev-build`'s behaviour:** without `--dev-build` the script deletes
every non-`.dll` from the output and removes the build directory (`:70-74`). Use
`./package-release.sh helios <dst> --dev-build` when you want the tests, the demos and an incremental
build tree to survive.

To get the **conformance suite and the demos** — which is what `GATES.md` D12-G1..G4 need — build
directly rather than through the release script:

```bash
cd /home/rupansh/helios-vgpu/vkd3d-proton-helios
meson setup --cross-file build-win64.txt --buildtype release \
      -Denable_tests=true -Denable_extras=true /home/rupansh/helios-vgpu/tmp/dx12/vkd3d/b64
ninja -C /home/rupansh/helios-vgpu/tmp/dx12/vkd3d/b64
# → b64/tests/d3d12.exe            (the 40-file suite; tests/meson.build:47-52)
#   b64/tests/descriptor-performance.exe
#   b64/demos/triangle.exe, b64/demos/gears.exe   (gui_app; demos/meson.build:19-29)
#   b64/libs/d3d12/d3d12.dll, b64/libs/d3d12core/d3d12core.dll
```

Meson options, all booleans defaulting `false` except `enable_trace` (`meson_options.txt`):
`enable_tests`, `enable_extras`, `enable_profiling`, `enable_renderdoc`, `enable_descriptor_qa`,
`enable_extended_emulation`, `enable_trace` (combo `false|true|auto`, `auto` follows buildtype;
`enable_breadcrumbs` follows `enable_trace`, `meson.build:57-60`). `subdir('tests')` is gated on
`enable_tests` (`meson.build:199`); `subdir('demos')` + `subdir('programs')` on `enable_extras`
(`:209-210`).

⚠ **Load-bearing detail for `DECISIONS.md` D2** — on Windows,
`lib_d3d12 = vkd3d_compiler.find_library('d3d12')` (`meson.build:186`), i.e. the tests and demos
link the **system** `d3d12` import library, not vkd3d's own DLL. That is precisely why the *same*
`d3d12.exe` binary tests both arms: with vkd3d's `d3d12.dll` beside it, the loader picks that up
(neither `d3d12.dll` nor `d3d12core.dll` nor `dxgi.dll` is a KnownDLL on this machine — `R10` Q3.2);
without it, the binary reaches the OS `d3d12.dll` → `UserModeDriverName[3]` → `helios_umd12.dll`.

### 8.3 The **FALLBACK** arm: native MSVC x64 on the VM — take it when you want a Windows debugger

Upstream **CI-gates** this (`.github/workflows/test-build-windows.yml`, `runs-on: windows-2022`,
both x86 and x64), so it is supported, not merely tolerated; `README.md:136-140` frames it as a
developer/testing configuration. The meson build carries MSVC handling throughout —
`vkd3d_is_msvc = compiler.get_id() == 'msvc' or 'clang-cl'` (`meson.build:9`),
`vs_module_defs : 'd3d12.def'` on the MSVC path vs `objects : 'd3d12.def'` for mingw
(`libs/d3d12/meson.build:20,27-28`).

**Dependency status on *this* VM**, `[LIVE]` this session via `win_exec`:

| Dependency | Status |
|---|---|
| Visual Studio 2022 Community + vcvars64 | ✅ `C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat` |
| `meson` | ✅ `C:\Users\Rupansh\AppData\Local\Programs\Python\Python312\Scripts\meson.exe` (on PATH) |
| `glslangValidator` | ✅ `C:\VulkanSDK\1.4.350.0\Bin\glslangValidator.exe` (on PATH) — **the CI's download step is unnecessary here** |
| `ninja` | ✅ `C:\Users\Rupansh\AppData\Local\Programs\Python\Python312\Scripts\ninja.exe` (on PATH) |
| **`widl`** | ✅ **PRESENT and already on PATH** — `C:\Users\Rupansh\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin\widl.exe`, 692 238 bytes (`[LIVE]`, `Get-Command widl`). |
| WinLibs mingw64 GCC | ✅ `gcc.exe` / `g++.exe` **16.1.0** (`MinGW-W64 x86_64-ucrt-posix-seh`) in the same `…\WinGet\Packages\…\mingw64\bin`, on PATH |

⇒ **Nothing is missing. There is no install step for the MSVC arm.**

⛔ **Do not `choco install strawberryperl`.** An earlier revision of this document reported `widl` as
missing and prescribed Strawberry Perl; that was a **search artefact** — the search was depth-limited
to 4 and `widl.exe` sits at depth 8 under `%LOCALAPPDATA%\Microsoft\WinGet\Packages\`. `C:\Strawberry`
genuinely does not exist, but nothing needs it. Verify for yourself in one line:

```powershell
(Get-Command widl -ErrorAction SilentlyContinue).Source     # → the WinGet WinLibs path above
```

⇒ **And there is a third build arm nobody had noticed: native mingw *on the VM*.** The WinLibs
package supplies `gcc`/`g++`/`ar`/`widl` for `x86_64-w64-mingw32` at the same GCC 16.1.0 the Linux
host uses (§8.2), so vkd3d's *native* (non-cross) mingw configuration is available on the VM without
MSVC at all — useful when you want the mingw ABI (identical to the shipping build) but the binaries
produced where the debugger lives. It is untried; it is **U15**.

The MSVC build itself, mirroring `test-build-windows.yml` but with a **local C: build dir**:

```powershell
# ⛔ NEVER configure or build under Z:\ — see the rule in §8.4.
# ⛔ Build the string first; `cmd /c "…" + "…"` passes a LITERAL `+` (see §7.3).
# ⚠ $src is the win_cargo mirror TODAY. Once W1 (§8.4.1) lands, vkd3d is excluded from that
#   mirror and $src becomes VKD3D_MIRROR = 'C:\Users\Rupansh\vkd3d-proton-helios'.
$src = 'C:\Users\Rupansh\helios-vgpu\vkd3d-proton-helios'
$vsdevcmd = 'C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\Tools\VsDevCmd.bat'
$build = "`"$vsdevcmd`" -arch=x64 -host_arch=x64 -no_logo && cd /d `"$src`" && " +
         "meson setup -Denable_tests=True -Denable_extras=True --buildtype release " +
         "--backend vs2022 C:\Users\Rupansh\vkd3d-build-x64 && " +
         "msbuild -m C:\Users\Rupansh\vkd3d-build-x64\vkd3d-proton.sln"
cmd /c $build
```

No `set PATH=…` prepend is needed: `widl`, `meson`, `glslangValidator` and `ninja` are all already on
the machine PATH (table above), and `VsDevCmd.bat` only *adds* MSVC's entries.

**UNVERIFIED (U8):** whether a native MSVC x64 build actually succeeds on *this* VM — no install
precondition, just run the block above. (The VM has no known blockers; memory `[20TH]` records "no
clang-cl on the VM", but `win_dxvk` proves LLVM *is* installed at `C:\Program Files\LLVM\bin` — that
memory is stale for the toolchain, and irrelevant here since this arm uses MSVC.)

### 8.4 ⚠ The local-C:-path rule, and the mirror

⛔ **Never configure or build on `Z:\`.** CLAUDE.md's `CARGO_TARGET_DIR` rule is about cargo, but the
underlying failure — build-system artifact IO on the 9p/virtio share failing with `OS error 87` — is
not cargo-specific, and `win_dxvk` exists precisely because the DXVK meson build has to read a
**local** checkout. Configure into `C:\Users\Rupansh\vkd3d-build-x64`; keep sources on the mirror.

**`[LIVE]` finding, settling `R3` UNVERIFIED #5:** the `win_cargo`/`win_build_kmd` robocopy mirror
excludes `/XD … vkd3d-proton …` (`tools/win-mcp/src/main.rs:576`, `:843`), and the question was
whether that also excludes `vkd3d-proton-helios`. **It does not** — checked on the VM this session:

```
Test-Path 'C:\Users\Rupansh\helios-vgpu\vkd3d-proton-helios\meson.build'   → True
```

Robocopy's `/XD <bare-name>` matches directory *names* exactly, not as a prefix. So the mirror
already carries the full vkd3d tree (and will carry the ~1 GB of submodules once they are
initialised).

#### 8.4.1 ⚠ W1 — exclude `vkd3d-proton-helios` from the win_cargo mirror. Do this BEFORE the first submodule init.

**This is an ordered work item, not a consideration, and it is the only thing that gates the first
build.** `win_cargo` and `win_build_kmd` `/MIR` the *whole* share on every invocation. Today that
copies ~40 MB of vkd3d sources — annoying. After
`git submodule update --init --recursive` it copies **~1 GB** of dxil-spirv + Khronos headers on
every KMD build and every UMD build, forever, for a tree that is never a cargo input.

**Decision: exclude it, and give vkd3d its own mirror, exactly as DXVK has.** DXVK is the precedent
and it exists for this reason (`DXVK_SRC` → `DXVK_MIRROR`, `tools/win-mcp/src/main.rs:64-65`).

The exact edits, all in `tools/win-mcp/src/main.rs`:

1. **Both robocopy lines** — `main.rs:576` (`win_cargo`) and `main.rs:843` (`win_build_kmd`) — carry
   the identical `/XD` list. Add `vkd3d-proton-helios` to each, next to the existing
   `vkd3d-proton` entry (which, per the measurement above, does **not** cover it):
   ```
   /XD target .git "{MESA_SRC}" dxvk dxvk-research-only vkd3d-proton vkd3d-proton-helios virtio-research-only-3d windows-driver-docs-research-only
   ```
2. **Add the source/mirror consts** beside `DXVK_SRC`/`DXVK_MIRROR` at `main.rs:64-65`:
   ```rust
   const VKD3D_SRC: &str = "Z:\\vkd3d-proton-helios";
   const VKD3D_MIRROR: &str = "C:\\Users\\Rupansh\\vkd3d-proton-helios";
   const VKD3D_BUILD: &str = "C:\\Users\\Rupansh\\vkd3d-build-x64";
   ```
3. `win_vkd3d` (§8.5) then does its own mirror with `/XD .git /XF .git`, the shape `win_dxvk` uses at
   `main.rs:750`.

⚠ **Ordering:** do step 1 *before* running `git submodule update --init --recursive`, or the next
`win_cargo` pays the full ~1 GB copy once before the exclusion takes effect. Sequence is: edit
`main.rs` → rebuild/restart the `win` MCP server → submodule init → build.

### 8.5 The `win_vkd3d` MCP tool that should exist

There is no `win_vkd3d` tool today (`grep -n 'fn win_' tools/win-mcp/src/main.rs` — the set is
`win_exec`, `win_cargo`, `win_meson`, `win_looking_glass`, `win_looking_glass_idd`, `win_dxvk`,
`win_install_umd`, `win_build_kmd`, `win_install_kmd`). **Copy `win_dxvk`**
(`tools/win-mcp/src/main.rs:736-776`) — it is the closest-shaped tool and already solves the two
hard parts:

1. a robocopy source mirror `Z:\<sub>` → a **local** git checkout, with `/XD .git /XF .git` so the
   local checkout's git state is never touched (`:750`);
2. a `cmd /c 'set PATH=… && call "<vcvars>" && meson …'` invocation, with the ⚠ comment at `:757-761`
   explaining why the PATH prepend must come **before** `call vcvars` (cmd expands `%PATH%` at
   **parse** time, so a prepend placed after vcvars silently discards MSVC's `lib.exe`/`link.exe`).

Concretely, the new tool needs the three consts from §8.4.1 step 2 —
`VKD3D_SRC = "Z:\\vkd3d-proton-helios"`, `VKD3D_MIRROR = "C:\\Users\\Rupansh\\vkd3d-proton-helios"`,
`VKD3D_BUILD = "C:\\Users\\Rupansh\\vkd3d-build-x64"` — and **no PATH prepend at all**:
`widl`, `meson`, `glslangValidator` and `ninja` are already on the VM's machine PATH (§8.3), so the
command reduces to `cmd /c 'call "<vcvars>" && meson <args>'`. Default `args` =
`compile -C <VKD3D_BUILD>`, matching `win_dxvk`'s default.

⛔ **Do not copy `win_dxvk`'s `set "PATH=…;%PATH%" &&` prefix into it.** That prefix exists only
because DXVK needs clang-cl from `C:\Program Files\LLVM\bin`, and its ⚠ ordering comment
(`main.rs:755-760`) is about *that*. vkd3d needs no such prepend, and an earlier revision of this
document specified one pointing at `C:\Strawberry\c\bin` — a directory that **does not exist on this
VM** (§8.3), which would make every invocation prepend a phantom path.

### 8.6 ⚠ vkd3d does not use the Windows SDK's `d3d12.h`

vkd3d compiles its **own** IDLs with `widl` — `vkd3d_d3d12.idl`, `vkd3d_d3d12sdklayers.idl`,
`vkd3d_dxgi*.idl`, `vkd3d_dxcapi.idl`, `vkd3d_core_interface.idl`, `vkd3d_swapchain_factory.idl`,
`vkd3d_{device,command_list,command_queue}_vkd3d_ext.idl` (`include/meson.build:1-19`) — on top of
its own `vkd3d_windows.h` / `vkd3d_win32.h` shims. **Even the MSVC build goes through `widl`.**

Two consequences for the D1 bridge:

- The `D3D12_*` type layouts vkd3d compiles against are **vkd3d's own transcription of the D3D12
  ABI**, not `tmp/dx12/sdk/d3d12.h`'s. Any structural comparison between the DDI structs
  (`d3d12umddi.h`, bindgen'd per `DECISIONS.md` §7.2) and vkd3d's `ID3D12*` arguments must be done
  **deliberately**, field by field, not assumed. ⚠ This is the same hazard class as `DECISIONS.md`
  H3's by-value descriptor-handle return.
- `widl` must exist on whichever machine configures the build — **and it does, on both**: Linux
  `/usr/bin/widl` (from wine, §8.2) and the VM's WinLibs mingw64 `widl.exe`, already on PATH (§8.3).
  Neither arm needs anything installed for it.

---

## 9. Driving it — the environment surface

vkd3d has no config file; everything is environment variables. `VKD3D_CONFIG` is a comma/space list
of flags read once at `device.c:1385`; there are **70** of them
(`grep -c VKD3D_DECL_CONFIG include/private/config_flag_decl.h` → 70).

### 9.1 `VKD3D_CONFIG` flags worth reaching for on a virtualised GPU

| Flag string | Decl | Effect | Why here |
|---|---|---|---|
| `single_queue` | `:8` | collapse COMPUTE and TRANSFER onto GRAPHICS (`device.c:3843-3847`) | **first stability A/B.** §4.3: sparse/COPY land on Vulkan family 1, which Helios has never driven |
| `nodxr` | `:10` | drop all raytracing extensions (`device.c:70-75`) | **second stability A/B.** Removes the entire RTAS/SBT surface from the first bring-up |
| `descriptor_heap` | `:65` | opt into `VK_EXT_descriptor_heap` | inert today (§5.3 S4); becomes the payoff switch the moment virglrenderer is resynced |
| `no_upload_hvv` | `:13` | never use host-visible VRAM for the UPLOAD heap | the BAR/aperture is Helios' most constrained resource; this is the knob if UPLOAD allocations misbehave |
| `force_host_cached` | `:16` | force all host-visible allocations CACHED | pairs with the MOVNTDQA/WC-read lesson (memory `[21ST]`); also "greatly accelerates captures" |
| `debug_utils` / `vk_debug` | `:5` / `:3` | enable `VK_EXT_debug_utils` / load validation | pairs with host-side `HELIOS_VKR_DEBUG=validate` |
| `breadcrumbs` | `:28` | instrument command lists with `VK_AMD_buffer_marker` or NV checkpoints | ⚠ **inert on Helios** — neither extension is exposed (§5, S11). Do not expect breadcrumb output |
| `fault` | `:11` | enable `VK_EXT_device_fault` reporting | ⚠ also inert today, same reason |
| `enable_experimental_features` | `:44` | among other things, forces SM 6.7 without reconvergence/quad-control (`device.c:10794-10797`) | ⛔ never in a gate run |
| `dxr12` | `:32` | experimental DXR 1.2 if **`VK_KHR_opacity_micromap`** present (⚠ the KHR one — §3.4) | inert (§6.2): absent on the guest *and* on the host GPU |
| `skip_application_workarounds` | `:4` | disable vkd3d's per-app hacks | use when a benchmark behaves differently under vkd3d than expected |

### 9.2 The rest of the `VKD3D_*` surface

| Variable | Read at | What it does |
|---|---|---|
| `VKD3D_DEBUG` | `vkd3d-common/debug.c:51`, init at `:82-99` | log level for the API channel: `none`, `err`, `info`, `fixme`, `warn`, `trace`. **Default is `fixme`.** `info` is the one that prints the caps conclusions (§1.2 D) |
| `VKD3D_SHADER_DEBUG` | `debug.c:52` | same levels, for the shader compilers |
| `VKD3D_LOG_FILE` | `debug.c:110-118` | redirect the log to a file instead of stderr. ⚠ **Essential here** — a session-1 scheduled task has no console, so without this there is no log at all |
| `VKD3D_LOG_BUFFERED[=bytes]` | `debug.c:101-109` | buffer the log in `bytes`-sized chunks (default 64 KiB); use with `trace` to cut stdio overhead |
| `VKD3D_SHADER_MODEL` | `device.c:10617` (`d3d12_device_caps_shader_model_override`, opens `:10591`) | force `max_shader_model`; accepts `5_1`, `6_0`…`6_9`. §7.4 rank 3 |
| `VKD3D_FEATURE_LEVEL` | `device.c:10888` (`d3d12_device_caps_override`, opens `:10867`) | force `max_feature_level`; accepts `11_0`, `11_1`, `12_0`, `12_1`, `12_2`. ⛔ see §9.3 |
| `VKD3D_FILTER_DEVICE_NAME` | `device.c:3506` | skip physical devices whose `deviceName` does not contain the substring. On this multi-adapter VM, `VKD3D_FILTER_DEVICE_NAME=Venus` is the belt-and-braces for §10 |
| `VKD3D_VULKAN_DEVICE` | `README.md:216-217` | zero-based physical-device index override |
| `VKD3D_DISABLE_EXTENSIONS` | `device.c:194` | comma list of Vulkan extensions vkd3d must not use even if available. The cheapest "does X matter" experiment there is |
| `VKD3D_QUEUE_PROFILE` | `queue_timeline.c:33` | **see §9.4** |
| `VKD3D_QUEUE_PROFILE_ABSOLUTE=1` | `queue_timeline.c:58-67` | zero the trace timebase so it lines up with an externally captured timeline |
| `VKD3D_SHADER_CACHE_PATH` | `README.md:262-266` | pipeline cache location; `=0` disables the internal cache |
| `VKD3D_SHADER_DUMP_PATH` / `VKD3D_SHADER_OVERRIDE` | `README.md:291-295` | dump every DXIL/SPIR-V; substitute `$hash.spv` for a shader. The direct analogue of the DXVK shader-dump workflow |
| `VKD3D_TEST_FILTER` / `_MATCH` / `_EXCLUDE` / `_DEBUG` / `_PLATFORM` / `_BUG` | `README.md:221-236` | the suite's own selectors — `GATES.md`/`R9` own these |
| `DXIL_SPIRV_CONFIG` | `device.c:11289-11294` | passes through to dxil-spirv |

⚠ `VKD3D_SHADER_MODEL`, `VKD3D_FEATURE_LEVEL` and `VKD3D_QUEUE_PROFILE` are **not documented in the
README** — they exist only in the code. Do not expect to rediscover them from the docs.

### 9.3 ⛔ `VKD3D_FEATURE_LEVEL` must never appear in a gate run

`d3d12_device_caps_override()` (`device.c:10867-10936`) does not check anything. It `max()`es the
tiers up to whatever the requested feature level implies. Verbatim, `:10923-10933`:

```c
    if (fl_override >= D3D_FEATURE_LEVEL_12_2)
    {
        caps->options5.RaytracingTier = max(caps->options5.RaytracingTier, D3D12_RAYTRACING_TIER_1_1);
        caps->options6.VariableShadingRateTier = max(caps->options6.VariableShadingRateTier, D3D12_VARIABLE_SHADING_RATE_TIER_1);
        caps->options.ResourceBindingTier = max(caps->options.ResourceBindingTier, D3D12_RESOURCE_BINDING_TIER_3);
        caps->options.TiledResourcesTier = max(caps->options.TiledResourcesTier, D3D12_TILED_RESOURCES_TIER_3);
        caps->options.ConservativeRasterizationTier = max(caps->options.ConservativeRasterizationTier, D3D12_CONSERVATIVE_RASTERIZATION_TIER_3);
        caps->max_shader_model = max(caps->max_shader_model, D3D_SHADER_MODEL_6_5);
        caps->options7.MeshShaderTier = max(caps->options7.MeshShaderTier, D3D12_MESH_SHADER_TIER_1);
        caps->options7.SamplerFeedbackTier = max(caps->options7.SamplerFeedbackTier, D3D12_SAMPLER_FEEDBACK_TIER_1_0);
    }
```

**It raises advertised tiers without backing them.** `SamplerFeedbackTier` is the clearest example
on this guest: `d3d12_device_determine_sampler_feedback_tier` computes `TIER_0_9`
(`device.c:10060-10069`, §7.2b) and the override `max()`es it to **`TIER_1_0`** at `:10932` — a
strictly higher tier than anything the code derived, handed out on the strength of an env var.
(⛔ Not "vkd3d does not implement sampler feedback at all" — it does, and it reaches 0_9 here; the
defect is the unbacked *promotion*, which is the same defect for every other line in the block.)
This is the exact hazard `DECISIONS.md` §7.8 names ("advertising a
capability that is not backed is a lie the OS acts on"), delivered by an env var. The test binary's
`--feature-level` argument is the same lever. ⛔ Both are banned from gate runs; record the
environment of every gate run so the ban is checkable.

### 9.4 `VKD3D_QUEUE_PROFILE` — adopt this on day one

`libs/vkd3d/queue_timeline.c` (718 LOC) is **not fence machinery** — it is a Chrome-trace emitter,
gated purely on the env var (`:28-42`):

```c
HRESULT vkd3d_queue_timeline_trace_init(struct vkd3d_queue_timeline_trace *trace, struct d3d12_device *device)
{
    ...
    if (!vkd3d_get_env_var("VKD3D_QUEUE_PROFILE", env, sizeof(env)))
        return S_OK;

    trace->file = fopen(env, "w");
    ...
        fputs("[\n", trace->file);
```

256×1024 cookie slots (`:26`). It emits regions for `EXECUTE` / `WAIT` / `SIGNAL` / `DRAIN` /
`SPARSE` / `CALLBACK` / `STOP`, plus `register_present_wait` (`:488`), `register_present_block`
(`:510`), `register_pso_compile` (`:496`), `register_command_list` (`:417`),
`register_swapchain_blit` (`:409`), `register_low_latency_sleep` (`:518`).

**This is exactly the evidence class ROADMAP WS2 needed for the present-queue stall, already
written, at zero cost, from inside the D3D12 client.** The WS2 investigation reconstructed
submit/wait/signal/present-block timing from `Microsoft-Windows-DxgKrnl` ETW `BlockThread` events;
`VKD3D_QUEUE_PROFILE` gives the *producer* side of the same frame directly. Run both and you have
both halves:

```powershell
$env:VKD3D_QUEUE_PROFILE = 'C:\Users\Rupansh\vkd3d-trace.json'
$env:VKD3D_QUEUE_PROFILE_ABSOLUTE = '1'     # zero the timebase so it aligns with the ETW slice
# ... run the workload (via a session-1 scheduled task if it has a window) ...
# then open vkd3d-trace.json in chrome://tracing / Perfetto
```

⚠ The trace file is written with `fopen(..., "w")` at device create and appended thereafter — a
crash mid-run leaves an unterminated JSON array; Perfetto tolerates it, `chrome://tracing` may not.
Append `]` by hand if it will not load.

### 9.5 The venus knobs that pair with them

All from `icd/mesa/src/virtio/vulkan/vn_common.c:24-40` (`VN_DEBUG`) and `:43-59` (`VN_PERF`),
parsed at `:70` by `parse_debug_string(os_get_option("VN_DEBUG"), vn_debug_options)`.

| Var | Effect | Pairs with |
|---|---|---|
| `VN_DEBUG=mem_budget` | **turns ON `VK_EXT_memory_budget`** (`vn_physical_device.c:1553`) | S3 — take it on day one; improves `QueryVideoMemoryInfo` |
| `VN_DEBUG=no_sparse` | clears every sparse feature/property (`vn_physical_device.c:976-978`, mask at `:1777-1798`) | the A/B for §6.1 / `TiledResourcesTier` |
| `VN_DEBUG=no_ray_tracing` | clears `physical_dev->ray_tracing`, gating the five RT rows of the passthrough table | the ICD-side mirror of `VKD3D_CONFIG=nodxr` (§6.2) |
| `VN_DEBUG=no_second_queue` | one queue only | the ICD-side mirror of `VKD3D_CONFIG=single_queue` (§4.3) |
| `VN_DEBUG=no_desc_heap` | clears `EXT_descriptor_heap` (`:1531`) | the A/B once S4 lands |
| `VN_DEBUG=no_gpl` | disables `VK_EXT_graphics_pipeline_library` | PSO-path bisection |
| `VN_DEBUG=init,result,wsi` | venus init / VkResult / WSI tracing | first-run triage |
| `VN_PERF=no_async_queue_submit`, `no_cmd_batching`, `no_fence_feedback`, `no_semaphore_feedback`, `no_multi_ring`, … (14 total) | serialise the venus wire | the standard "is this a batching race" bisection |
| host: `HELIOS_VKR_DEBUG=validate` | host-side Vulkan validation layers | `/tmp/helios-qemu-stderr.log` |

⚠ `win_exec` lands in **session 0**. Anything with a window — the demos, 3DMark, anything that
creates a swapchain — must run through a cloned scheduled task in session 1
(`schtasks /run /tn <name>`), and the env vars must be set **inside** the task's command, not in the
`win_exec` shell.

---

## 10. How vkd3d finds the adapter

Relevant to the Phase-0 (app-local) arm; the D1/D4 arm bypasses this file entirely (that is reason 1
of `DECISIONS.md` D4).

`d3d12_find_physical_device` (`libs/d3d12core/main.c:446-566`) is called from the device-create path
at `:706` with the `DXGI_ADAPTER_DESC` obtained by `d3d12_get_adapter` (`:375-444`): if the app
passed no adapter, `CreateDXGIFactory1(IID_IDXGIFactory4)` + `EnumAdapters(0)` — **DXGI adapter 0**.
Then, in order:

```c
/* pass 1 — LUID */
if (properties2.properties.apiVersion < VKD3D_MIN_API_VERSION) continue;                 /* :491-495 */
id_properties.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES;                   /* :498 */
...
if (id_properties.deviceLUIDValid &&
    !memcmp(id_properties.deviceLUID, &adapter_desc->AdapterLuid, VK_LUID_SIZE))         /* :506 */
{ /* tie-break on deviceID/vendorID, then deviceName vs the DXGI Description   :510-532 */ }

/* pass 2 — PCI IDs */
if (properties2.properties.deviceID == adapter_desc->DeviceId &&
    properties2.properties.vendorID == adapter_desc->VendorId)                           /* :547-548 */
{ vk_physical_device = vk_physical_devices[i]; break; }

/* pass 3 — the silent fallback */
FIXME("Could not find Vulkan physical device for DXGI adapter.\n");                       /* :558 */
WARN("Using first available physical device...\n");                                       /* :559 */
vk_physical_device = vk_physical_devices[0];                                              /* :560 */
```

It uses `VkPhysicalDeviceIDProperties::deviceLUID` + `deviceLUIDValid` — **not**
`VK_EXT_pci_bus_info`, **not** `VK_KHR_driver_properties`. Neither appears in
`optional_device_extensions`.

**Why this matters on this VM.** There are **two** display devices
(`Get-CimInstance Win32_VideoController` → "Looking Glass Indirect Display Device" and
"Helios vGPU Render Adapter (WDDM bring-up)", `R9` §1.7), so **DXGI adapter 0 is not necessarily
Helios**. Meanwhile the guest exposes exactly **one** `VkPhysicalDevice`
(`[CAPTURE]:121` "Devices: count = 1"). So:

- If DXGI adapter 0 is Helios and the LUIDs match, pass 1 succeeds — correct and intended.
- If DXGI adapter 0 is the *other* device, pass 1 and pass 2 both fail and **pass 3 silently picks
  `physical_devices[0]`, which is the Helios venus device** — producing an `ID3D12Device` whose
  `adapter_luid` is the wrong adapter's while its Vulkan device is Helios'. The device works; the
  identity is a lie. ⚠ Everything downstream that keys on LUID — swapchain adapter matching, DXGI
  output enumeration, `SetStablePowerState`, our own KMD escapes — is then mismatched. **The FIXME
  and WARN are the only symptom, and only at `VKD3D_DEBUG=fixme`, which is the default level.**

**Practical rules for every Phase-0 run:**

1. Read the DXGI index of the Helios adapter **before each suite run** with
   `tools/dxgi_luid_dump.cpp` (it prints `adapter[i] luid=HI:LO vendor= device= name=`) and pass it
   explicitly. ⚠ `tests/d3d12_crosstest.h:445-465`: the harness only passes an adapter when
   `use_warp_device || use_adapter_idx`, so **`--adapter 0` behaves exactly like no argument**.
2. Belt-and-braces on the Vulkan side: `VKD3D_FILTER_DEVICE_NAME=Venus` (`device.c:3506`) or
   `VKD3D_VULKAN_DEVICE=0`.
3. Grep every run's log for `"Could not find Vulkan physical device for DXGI adapter."` — its
   presence invalidates the identity of that run.

**UNVERIFIED:** whether the guest's `deviceLUID = 09760000-00000000` (`[CAPTURE]:670`,
`deviceLUIDValid = true` at `:672`) byte-matches `DXGI_ADAPTER_DESC::AdapterLuid` for the Helios
adapter. Memory `[30TH]` records that venus reports the WDDM adapter LUID, but that was measured on
a different code path. *Settling experiment:* run `tools/dxgi_luid_dump.cpp` on the VM and compare
its `luid=HI:LO` for the Helios row against those eight bytes (little-endian: `LowPart = 0x00007609`,
`HighPart = 0x00000000`). Two read-only commands, no build of vkd3d needed.

⚠ Related known upstream failure mode: vkd3d-proton issue **#2790**, "DX12 swapchain creation fails
on multi-GPU NVIDIA systems (duplicate LUID)". A duplicate or mismatched LUID between the DXGI
adapter and the `VkPhysicalDevice` is a *documented* vkd3d failure, not a Helios novelty.

---

## 11. Interop and `libs/vkd3d/d3dkmt.c`

**Read this so you do not plan a milestone on D3D12 resource sharing.**

`libs/vkd3d/d3dkmt.c` is 449 lines, `Copyright 2025 Rémi Bernon for Codeweavers`, added in eight
commits over 2025-10-15..30. The whole file is `#ifdef _WIN32` (`:23`) with a no-op `#else`
(`:419-449`) that just `WARN`s. **But "Windows" here means Wine.**

### 11.1 Why it is Wine-oriented

The escape type it uses is Wine-private — `include/private/vkd3d_d3dkmt.h:119-122`:

```c
typedef enum _D3DKMT_ESCAPETYPE
{
    D3DKMT_ESCAPE_UPDATE_RESOURCE_WINE = 0x80000000
} D3DKMT_ESCAPETYPE;
```

That is not a Microsoft `D3DKMT_ESCAPETYPE` value; it is Wine's `win32u` D3DKMT emulation. The whole
header is a **hand-written re-declaration** of the D3DKMT ABI (381 lines) rather than an include of
`d3dkmthk.h`, because vkd3d builds against widl/mingw headers and not the WDK (§8.6).

The fallback path is Wine-specific too. `libs/vkd3d/shared_metadata.c:24-26`, `:56`:

```c
#define IOCTL_SHARED_GPU_RESOURCE_SET_METADATA  CTL_CODE(FILE_DEVICE_VIDEO, 4, METHOD_BUFFERED, FILE_WRITE_ACCESS)
...
    HANDLE nt_handle = CreateFileA("\\\\.\\SharedGpuResource", GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING, ...);
```

`\\.\SharedGpuResource` is **Wine's** shared-GPU-resource device. On native Windows it does not
exist, so `vkd3d_set_shared_metadata` / `vkd3d_get_shared_metadata` fail — and that is the
*fallback* for when D3DKMT fails.

### 11.2 What it does, and that it fails soft

Six entry points, all opportunistic, all guarded by `if (!device->kmt_local) return;`
(e.g. `d3dkmt.c:97-101`):

| Function | Called from | D3DKMT used |
|---|---|---|
| `d3d12_device_open_kmt` (`:25`) | `device.c:11617`, end of device create | `D3DKMTOpenAdapterFromLuid` → `D3DKMTCreateDevice` → `D3DKMTCloseAdapter`; stores `device->kmt_local` |
| `d3d12_device_close_kmt` (`:44`) | `device.c:4885` | `D3DKMTDestroyDevice` |
| `d3d12_shared_fence_open_export_kmt` (`:51`) | `command.c:2227` | `vkGetSemaphoreWin32HandleKHR(D3D12_FENCE_BIT)` → `D3DKMTOpenSyncObjectFromNtHandle` |
| `d3d12_resource_open_export_kmt` (`:89`) | `resource.c:4469` | `vkGetMemoryWin32HandleKHR(OPAQUE_WIN32)` → `D3DKMTOpenResourceFromNtHandle` → `D3DKMTEscape(D3DKMT_ESCAPE_UPDATE_RESOURCE_WINE)` |
| `d3d12_resource_close_export_kmt` (`:199`) | | `D3DKMTDestroyAllocation` |
| `d3d12_device_open_resource_descriptor` (`:210`) | `device.c:7770` (`OpenSharedHandle`) | `D3DKMTQueryResourceInfo{,FromNtHandle}` + `D3DKMTOpenResource2` / `…FromNtHandle` to *read* the undocumented D3D runtime private data |

The last one reverse-engineers the D3D runtime's private blob with hard size asserts
(`vkd3d_d3dkmt.h:249-341`): `sizeof(d3dkmt_d3d9_desc) == 0x58`, `d3d11 == 0x68`, `d3d12 == 0x108`;
discrimination at `d3dkmt.c:313-358` on `(size, dxgi.size, dxgi.version)`.

### 11.3 ⚠ What this means on Helios — native Windows with a real WDDM driver

- `D3DKMTOpenAdapterFromLuid` + `D3DKMTCreateDevice` are real, documented, and **should succeed**
  against `kmd_render` ⇒ `device->kmt_local` gets set ⇒ **the D3DKMT path activates**, including the
  unguarded `vkGetMemoryWin32HandleKHR` of §5.1.
- But `D3DKMTEscape(Type = 0x80000000)` is Wine-only. `kmd_render`'s `DxgkDdiEscape` sees an
  unrecognised type. ⚠ **The return value of that `D3DKMTEscape` is ignored** (`d3dkmt.c:195`), so
  a refusal is silent — the resource simply has no runtime descriptor, and a later `OpenSharedHandle`
  from another process returns `E_INVALIDARG` from `d3d12_device_open_resource_descriptor`
  (`d3dkmt.c:415-416`).
- The DXVK-metadata fallback (`\\.\SharedGpuResource`) is unavailable.

⇒ **D3D12 cross-process / cross-API resource sharing under vkd3d-proton on native Windows is
UNVERIFIED and, on this reading, likely broken by construction.** *Settling experiment:* build
vkd3d (§8), then a D3D12-shaped version of `tools/d3d11_open_shared_probe.cpp` —
`CreateCommittedResource(D3D12_HEAP_FLAG_SHARED)` → `CreateSharedHandle` → `OpenSharedHandle` in a
second process — run **against WARP first** to isolate Helios, then against Helios, with
`VKD3D_DEBUG=warn VKD3D_LOG_FILE=…`. Watch for
`ERR("Failed to set metadata for shared resource, importing created handle will fail.")`
(`device.c:~7690`). ⚠ §5.1 says this probe may **crash** rather than fail; treat it accordingly.

⚠ Note for `kmd_render`: an escape with an unrecognised `Type` should be **counted, not silently
dropped** (CLAUDE.md rule 2). If a D3D12 client starts firing `0x80000000` escapes, a named counter
is what tells you.

**And, clearly: this does not block a first milestone.** Nothing in the single-process D3D12 path —
device, queues, command lists, resources, present through the DXVK-DXGI arm — requires D3DKMT.
`d3d12_device_open_kmt` failing is a `WARN`, not an error (`d3dkmt.c:25-43`). ⛔ Do not plan a
feature on D3D12 shared handles for P0–P4.

---

## 12. Licence and packaging

### 12.1 The licence, stated exactly

`vkd3d-proton-helios/COPYING`, read in full:

> Copyright 2016-2024 the vkd3d-proton project authors (see the file AUTHORS for a complete list)
>
> vkd3d-proton is free software; you can redistribute it and/or modify it under the terms of the
> **GNU Lesser General Public License** as published by the Free Software Foundation; either
> **version 2.1** of the License, **or (at your option) any later version.**

`vkd3d-proton-helios/LICENSE` is the 502-line verbatim LGPL-2.1 text ("Version 2.1, February 1999").
Every source file carries the same header (e.g. `libs/d3d12/main.c:6-18`).

⇒ **LGPL-2.1-or-later.**

### 12.2 The dynamic-vs-static consequence (`DECISIONS.md` D4)

| Shape | Obligation |
|---|---|
| **Ship `d3d12.dll` + `d3d12core.dll` as separate, dynamically-loaded DLLs** — and reach them via an added export (D4) | Helios distributes modified LGPL libraries: carry the licence text, state the changes, provide corresponding source for the LGPL parts. **`helios_umd12.dll` stays outside the LGPL boundary** because it links across a DLL boundary. |
| **Statically link `libvkd3d` into `helios_umd12.dll`** — the shape `umd/build.rs:218-223` already uses for DXVK's `libhelios_d3d11_static.a` / `libdxvk.a` | LGPL 2.1 **§6** then requires shipping either the object files or a mechanism allowing the user to relink `helios_umd12.dll` against a modified vkd3d. |

⚠ **The precedent does not carry over.** DXVK is zlib/libpng-licensed, which is why static linking
was free for D3D11. **vkd3d is the first LGPL component that would enter a Helios UMD binary.** This
is an owner decision; `DECISIONS.md` D4's default avoids it, and D4's stated fallback (a
`helios_d3d12_static` meson target excluding `d3d12core/main.c`, `R4` §6.4) must record the licence
decision in the commit that does it.

**One vendored non-LGPL component is in-tree today and is already settled: `md5`.**
`libs/vkd3d-shader/3rdparty/md5/` (`md5.c` 291 L, `md5.h` 43 L, `README.md` 38 L) is present in a
bare checkout — no submodule init needed — and is compiled into `vkd3d-shader` **unconditionally**
(`libs/vkd3d-shader/meson.build:6`, the `'3rdparty/md5/md5.c'` entry in `vkd3d_shader_src`), so it
ships in `d3d12core.dll` on every build. Its terms, verbatim from `md5.c:11-23`: Alexander Peslyak
("Solar Designer"), *"No copyright is claimed, and the software is hereby placed in the public
domain"*, with a fallback grant described in the file itself as *"a heavily cut-down 'BSD license'"*
— redistribution permitted in source and binary form, no attribution clause, no warranty.
⇒ **Public-domain / permissive; it imposes no obligation beyond keeping the header, and it does not
touch the LGPL analysis above.**

**UNVERIFIED (U10):** whether the **submodules** vendor further components with their own terms.
*Settling read:* the licence headers under `vkd3d-proton-helios/subprojects/dxil-spirv/` and
`vkd3d-proton-helios/khronos/{Vulkan,SPIRV}-Headers/` — ⚠ **those three, and only those three, cannot
be read until §8.1's `git submodule update --init --recursive` has run**, since they are empty today.
Do it in the same session that first initialises them.

### 12.3 The three deliverable shapes, and which is machine-wide

| Shape | Machine-wide? | Registration | Licence exposure |
|---|---|---|---|
| **(i) D3D12 UMD DDI** — `helios_umd12.dll` implements `d3d12umddi.h`, engine reached across a DLL boundary | **Yes** — every D3D12 app on the adapter, including dwm | INF `UserModeDriverName[3]` (`R11` §3 Variant B) | LGPL only if vkd3d code is linked in |
| **(ii) app-local vkd3d `d3d12.dll` + `d3d12core.dll` (+ DXVK `dxgi.dll`)** | **No** — per application directory | none (file placement) | LGPL, dynamic |
| **(iii) Agility-style, app exports `D3D12SDKPath`** | **No** — the app must be rebuilt | none | LGPL, dynamic |

Shape (ii) works because **neither `d3d12.dll` nor `d3d12core.dll` nor `dxgi.dll` is a KnownDLL** on
this machine — the load-bearing half, re-verified on the VM this session: **zero `d3d*` and zero
`dxgi*` entries** under
`HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\KnownDLLs` (`R10` Q3.2). ⚠ The total is
**38** values, not the 37 an earlier revision recorded; the count is incidental and drifts with
servicing, so do not gate anything on it — gate on the `d3d*`/`dxgi*` absence:
```powershell
$k = Get-Item 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\KnownDLLs'
($k.GetValueNames() | Where-Object { $_ -like 'd3d*' -or $_ -like 'dxgi*' }).Count   # must be 0
```
Because none is a KnownDLL, the
executable's directory wins the loader search. ⛔ Replacing `C:\Windows\System32\d3d12.dll`
(10.0.26100.8737, 146,168 bytes) is blocked by WRP/TrustedInstaller, would be reverted by servicing,
and DXVK's own guidance is explicit: *"DO NOT replace Windows DLLs in `System32` or `SysWOW64` with
DXVK's. This will break your Windows install."*

⇒ Shapes (ii)/(iii) are an **evidence vehicle** — no INF, no signing, no reboot, and they exercise
the whole Vulkan/venus substrate end to end (`DECISIONS.md` D2, Phase 0) — but they can never be the
shipping answer, because dwm, Store apps and 3DMark's D3D12 workloads all reach the OS `d3d12.dll` →
`UserModeDriverName[3]` path.

### 12.4 ⚠ The Agility SDK, and the precise reason it cannot help

The mechanism (<https://microsoft.github.io/DirectX-Specs/d3d/D3D12Redistributable.html>): the
**application** exports two symbols **from its own exe**:

```cpp
extern "C" { __declspec(dllexport) extern const UINT  D3D12SDKVersion = n; }
extern "C" { __declspec(dllexport) extern const char* D3D12SDKPath    = u8".\\D3D12\\"; }
```

The system `d3d12.dll` reads those, loads `D3D12Core.dll` from that **app-relative** subdirectory,
and fails `D3D12CreateDevice` if the exported version does not match the DLL's. Rules: the path is
relative to the exe (absolute paths and env vars break deployment); the redist must live in a
subdirectory, not beside the exe; if the requested version is same-or-older than the OS inbox D3D12,
the inbox version wins; `ID3D12SDKConfiguration::SetSDKVersion` works only in **Windows Developer
Mode**.

**The reason it cannot be used to insert vkd3d under an unmodified app, stated precisely:**

1. **It is keyed off the application's own exports.** A third party who does not control the exe
   cannot opt an app in. There is no machine-wide or per-adapter switch.
2. **It replaces `D3D12Core.dll` — the D3D12 *runtime* — and nothing below it.** The redistributed
   runtime still calls **`OpenAdapter12` in the driver's UMD** and still needs a driver implementing
   `d3d12umddi.h`. The DirectX-Specs page's own framing is that the redist preserves "contract
   integrity with kernel thunks". ⇒ **The Agility mechanism cannot make apps load a Helios/vkd3d
   D3D12 implementation while `OpenAdapter12` refuses.**
3. vkd3d-proton's `D3D12SDKVersion` data export (`libs/d3d12core/main.c:1355`) is **shape**
   compatibility with the Microsoft split, not participation in the Agility loader: vkd3d's own
   `d3d12.dll` finds its core by `dlopen("d3d12core.dll")` + the private `CLSID_VKD3DCore` query
   (§2.4), never by reading `D3D12SDKPath`. And `SetSDKVersion` is a `FIXME` returning `S_OK`
   (`libs/d3d12/main.c:267-273`), so it does not honour version pinning either.

**UNVERIFIED:** whether Windows 11 24H2's Agility loader path picks up vkd3d's `d3d12core.dll`
correctly when an app *does* opt in. Irrelevant to shape (i), relevant only if a packaged
Helios-controlled app ever uses shape (iii). *Settling experiment:* drop `d3d12.dll` +
`d3d12core.dll` beside a `D3D12HelloWorld` sample on the VM and check the loaded module list
(`listdlls` / Process Explorer) plus vkd3d `TRACE` output.

---

## 13. Prior art worth knowing

Three facts, and the third is the important one.

**(a) Upstream venus was deliberately driven to vkd3d-proton FL 12_2, and the Helios ICD is
downstream of that push.** Phoronix, "Venus Vulkan Driver Lands Mesh Shader Support In Mesa 26.0",
5 December 2025 (<https://www.phoronix.com/news/Venus-Vulkan-Mesh-Shader>), on Mesa MR
<https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/38739>: the mesh-shader implementation is
*"the last piece needed for getting Venus to VKD3D-Proton Feature Level 12_2 for the Direct3D 12
feature level atop Vulkan."* `icd/mesa` is Mesa `26.2.0-devel` at
`3af97415bc56f34010811dcfb1110e67e986b123` and advertises it
(`vn_physical_device.c:1554  .EXT_mesh_shader = true`). ⇒ **The specific Vulkan implementation
Helios uses has been targeted at vkd3d-proton by its own upstream, recently, with a named
feature-level goal.** That is the strongest external evidence for D6, and it explains why §1's
measurement came out clean. It says nothing about the *host* (virglrenderer + NVIDIA) or the
Windows-guest transport.

**(b) Bringing a non-IHV Vulkan driver up to vkd3d-proton is a known, tractable, incremental
workstream — and the vkd3d test suite is the measure.** Mesa wired lavapipe up for vkd3d-proton
deliberately (MR <https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/23996>, "lavapipe:
Bringup vkd3d-proton"; later `VK_EXT_fragment_shader_interlock`, `KHR_shader_quad_control`,
`shaderResourceMinLod`, `VK_EXT_shader_image_atomic_int64`, 64-bit image clears/ops merged for Mesa
25.2 — <https://www.phoronix.com/news/Lavapipe-VKD3D-Proton-Features>). Mesa even keeps a CI
harness for it: `.gitlab-ci/container/build-vkd3d-proton.sh`, plus per-driver failure trackers
(e.g. <https://gitlab.freedesktop.org/mesa/mesa/-/issues/5004>, "[anv] multiple failures running the
vkd3d-proton testsuite"). ⇒ **The shape to copy is: implement the feature, rerun the suite, count
passes** — which is exactly what `GATES.md` D12-G1..G3 should encode. Winlator/Cassia establish that
vkd3d-proton runs on non-IHV Mesa drivers in adverse environments, but their Vulkan is Turnip, not
venus, so they are weaker evidence than lavapipe.

**(c) ⚠ Nobody has published vkd3d-proton in a Windows guest over virtio-gpu/venus.** `R10` Q4.4
searched for it several ways and found nothing: **every** venus+vkd3d datapoint is a *Linux* guest
running Proton. There is also no public report of vkd3d-proton over any paravirtualised **Windows**
GPU driver, and no other virtual-GPU vendor ships D3D12 in a guest via their own UMD (`R10` Q5:
VMware SVGA3D stops at D3D11 FL11_0; VirtualBox 7.x is D3D11 via DXVK; Parallels is D3D11 over
Metal; Hyper-V GPU-PV and NVIDIA vGPU ship the *host IHV's* UMD into the guest, which Helios cannot
do). ⛔ **Treat "someone has surely done this" as false.** There is no precedent to plan around and
no external failure list to pre-empt; the D0/G0-G4 gates *are* the experiment.

---

## 14. UNVERIFIED

Every open item in this document, with the experiment that settles it. Nothing here blocks starting
work; several of them are single commands.

| # | Question | Settling experiment | Cost |
|---|---|---|---|
| **U1** | ⚠ **§7 / H5.** Does vkd3d's `maintenance7` layered-`driverID` swizzle actually fire on Helios — i.e. does the nested `VkPhysicalDeviceDriverProperties` report `NVIDIA_PROPRIETARY`? Decides **SM 6.0 / FL 12_1** vs **SM 6.6-at-minimum (ladder walks to 6.7) / FL 12_2**. | `tools/vk_layered_driverid_probe.cpp`, §7.3. Read-only, no build of vkd3d, no reboot, session-0 safe. | **~1 h. Run this first.** |
| **U2** | ⚠ **§5.1 / S1.** What actually happens on `CreateCommittedResource(D3D12_HEAP_FLAG_SHARED)` with `VK_KHR_external_memory_win32` absent — `VK_ERROR_INVALID_EXTERNAL_HANDLE`, or a NULL-PFN crash? | Minimal D3D12 program under vkd3d on the guest, session-1 scheduled task, `VKD3D_DEBUG=warn VKD3D_LOG_FILE=Z:\tmp\vkd3d.log`. ⚠ Expect a crash. | S (after §8 build) |
| **U3** | **§10.** Does the guest's `deviceLUID = 09760000-00000000` byte-match `DXGI_ADAPTER_DESC::AdapterLuid` for the Helios adapter, or does vkd3d land on the silent `physical_devices[0]` fallback? | Run `tools/dxgi_luid_dump.cpp` on the VM; compare against `[CAPTURE]:670`. Two read-only commands. | **XS** |
| **U4** | **§4.4.** Which descriptor backend does vkd3d select on venus, and what are `cbv_srv_uav_size` / `sampler_size`? Determines whether *any* fast path applies. | Any vkd3d client with `VKD3D_DEBUG=info`; read the bindless-state log. Or breakpoint `device.c:11302`. | S |
| **U5** | **§6.1 / §6.2.** Do the tier walks actually produce `TILED_RESOURCES_TIER_4` and `RAYTRACING_TIER_1_1`? | `VKD3D_DEBUG=info` → grep `"DXR support enabled."` / `"DXR 1.1 support enabled."`; and a `CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS)` probe for the tiled tier. | S |
| **U6** | **§6.3.** `ResourceHeapTier` — TIER_1 or TIER_2? Depends on runtime-computed `fallback_domain` memory-type masks, since `VK_EXT_pageable_device_local_memory` is absent. | Same `CheckFeatureSupport` probe as U5. | S |
| **U7** | **§4.3.** Does the queue-family walk land where predicted (DIRECT→0, COMPUTE→2, COPY/sparse→1), and does the KMD/venus ring survive a second queue family's timeline? | `VKD3D_DEBUG=info` for the selection; then A/B `VKD3D_CONFIG=single_queue` on the first workload that touches COPY or sparse. | S–M |
| **U8** | **§8.3.** Does a native MSVC x64 build of vkd3d succeed on *this* VM? **No install precondition** — `widl`, `meson`, `glslangValidator`, `ninja` and VS2022 are all confirmed present and on PATH (`[LIVE]` this session). | Run the `$build` / `cmd /c $build` block in §8.3 verbatim via `win_exec`; then `Test-Path C:\Users\Rupansh\vkd3d-build-x64\libs\d3d12core\d3d12core.dll`. ⛔ The earlier "once Strawberry Perl is installed" premise was false — do not install it. | S |
| **U9** | **§11.** Does D3D12 shared-resource create/open work at all on native Windows, given `\\.\SharedGpuResource` does not exist and the escape is Wine-only? | Two-process `CreateSharedHandle`/`OpenSharedHandle` probe, **WARP first** to isolate Helios, then Helios. ⚠ overlaps U2's crash risk. | M |
| **U10** | **§12.2.** Does vkd3d-proton vendor non-LGPL components with their own terms? | Read licence headers under `subprojects/dxil-spirv/` and `khronos/*/` — **only possible after §8.1's submodule init**. | XS |
| **U11** | **§12.4.** Does Windows 11 24H2's Agility loader path pick up vkd3d's `d3d12core.dll` when an app opts in? | Drop both DLLs beside a `D3D12HelloWorld` sample; check loaded modules + vkd3d TRACE. Only matters for deliverable shape (iii). | S |
| ~~**U12**~~ | ~~**§2.5.** `vkd3d_serialize_root_signature` (`include/vkd3d.h:129`) is not in either `.def` — does D4's added-export approach need a second export for it?~~ **SETTLED — yes.** `DECISIONS.md` D4 now specifies **two** added exports, `helios_vkd3d_create_device` **and** `helios_vkd3d_serialize_root_signature`, because the DDI delivers root signatures already parsed and vkd3d exports no serializer. | — (closed; `ARCHITECTURE.md` owns the bridge shape) | — |
| **U13** | **§9.1.** `VKD3D_CONFIG=breadcrumbs` and `fault` are inert on Helios (neither backing extension is exposed). Is there *any* GPU-fault breadcrumb path available, or is post-mortem debugging blind on this substrate? | Read `libs/vkd3d/breadcrumbs.c` for a no-extension fallback; if none, this becomes a named S11 sub-item with a real cost. | S |
| **U14** | **§7.2b clause 1.** Is `options.TypedUAVLoadAdditionalFormats` `TRUE` on the guest? It is the one FL-ladder clause that is **runtime-computed** — `d3d12_device_determine_additional_typed_uav_support` (`device.c:10010`, wired `:10179`) issues live `vkGetPhysicalDeviceFormatProperties` calls, which `vulkaninfo --summary` does not capture — and it gates **FL 12.0**, i.e. everything. | Falls out of U1's follow-up run for free: `VKD3D_DEBUG=trace` and read `"Max feature level: %#x."` (`device.c:10585`) — 12_0 or better ⇒ it passed; 11_1 ⇒ it failed. Standalone alternative: a `CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS)` probe (same binary as U5/U6) and read the field directly. | **XS** (rides U5) |
| **U15** | **§8.3.** The VM carries a full **WinLibs mingw64 GCC 16.1.0 + `widl`** toolchain on PATH (`[LIVE]` this session) — the same compiler generation as the Linux cross arm. Does vkd3d's *native* mingw configuration build on the VM, giving mingw-ABI binaries produced where the debugger lives? | `win_exec`: `meson setup --buildtype release -Denable_tests=true C:\Users\Rupansh\vkd3d-build-mingw` from `C:\Users\Rupansh\helios-vgpu\vkd3d-proton-helios` (or `C:\Users\Rupansh\vkd3d-proton-helios` once W1 lands) with the WinLibs `mingw64\bin` on PATH and no vcvars, then `ninja -C …`. Nothing to install. Purely additive — neither the primary (§8.2) nor the fallback (§8.3) arm depends on the answer. | S |

**Already settled by this document** (so nobody re-runs them):

- R3's UNVERIFIED #5 — the win-mcp robocopy `/XD vkd3d-proton` exclusion does **not** cover
  `vkd3d-proton-helios` (§8.4, measured). The follow-on decision is **W1** in §8.4.1: exclude it and
  give vkd3d its own mirror, with the exact edits listed there.
- R3's build-dependency question is **closed, not partially closed**: the **Linux host has the
  complete mingw cross toolchain today** (§8.2) *and* the **VM has every MSVC-arm dependency,
  including `widl`, already on PATH** (§8.3, measured `[LIVE]`). ⛔ There is **no install step on
  either machine** — in particular no Strawberry Perl, which an earlier revision of this document
  wrongly prescribed off a depth-limited filesystem search.
- The `SamplerFeedbackTier` question: vkd3d **does** implement it and reaches `TIER_0_9` on this
  guest, so it is **not** an FL 12.2 exception (§7.2b). The `"(TODO: missing sampler feedback)"`
  string is a profile-document `description` field, not a driver limitation.
- The extension vkd3d needs for DXR 1.2 is **`VK_KHR_opacity_micromap`**, not the EXT variant
  (§3.4, §4.2, §6.2) — and the host GPU exposes only the EXT one, which makes S8 unreachable through
  venus work alone.
