# SCANOUT_DRM_MODIFIER_DESIGN.md — historical DRM-modifier primary proposal

**Status: SUPERSEDED by measured results (2026-07-11).** This file records the
37th-session hypothesis and is not an implementation plan. Its central claim
that every Venus DMA_BUF scanout needs an explicit DRM modifier is false:

- A dedicated, external-memory `VK_IMAGE_TILING_LINEAR` image with
  `DRM_FORMAT_MOD_INVALID` displayed correctly on the NVIDIA host in both the
  CachyOS oracle and KMD `ScanoutDiag=16`.
- A plain DWM `VK_IMAGE_TILING_OPTIMAL` primary also exports with
  `DRM_FORMAT_MOD_INVALID`, but EGL cannot infer its opaque tiling. That is a
  missing layout description at the display importer, not proof that LINEAR is
  invalid.
- Enabling DRM-modifier/DMA_BUF extensions on every DXVK device changed the
  import-side memory requirements of ordinary shared OPTIMAL images. DXVK's
  undersized-import check correctly refused them; bypassing it caused NVIDIA
  Xid 31. Do not restore or weaken that path.
- The current path directly selects the real OPTIMAL DWM primary. `qemu-helios`
  reconstructs its exact VkImage using the original blob allocation size and
  performs host Vulkan readback for VNC. No guest copy and no public virtio ABI
  addition are required, but the host readback means this is not end-to-end
  zero-copy.

An explicit DRM modifier remains a possible future contract for true host
zero-copy if it can be introduced only for the primary without regressing
ordinary shared imports. Every prescriptive statement below is retained as
historical reasoning and is superseded where it conflicts with the facts above.

---

## 1. TL;DR

At the time of this proposal, Helios activated display and scanout end-to-end
(v22.22.73.0 KMD + the UMD `Flags.Primary` fix): `VpCN=1 / VpSA=1 /
ScSet=1 / ScFlu=1`, and paintcap showed the fully composed desktop. The session
then made the following hypothesis, which later evidence disproved:

> A venus **DMA_BUF scanout image must be created with `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT`
> + an explicit DRM modifier.** A plain OPTIMAL *or* plain LINEAR image → exported dmabuf is
> `DRM_FORMAT_MOD_INVALID` → **the host paints black.**
> — `helios_vk_present.c:251-254` (verbatim): *"venus REJECTS a DMA_BUF scanout image whose
> tiling isn't TILING_DRM_FORMAT_MODIFIER_EXT ... a plain LINEAR image -> MOD_INVALID -> black."*

DWM's primary is a plain OPTIMAL DXVK render target with no explicit DRM
modifier. The historical proposal below attempted to make it a LINEAR modifier
image. That implementation direction is no longer active.

---

## 2. The proven recipe (reference — `icd/win-build/helios_vk_present.c`)

This standalone Vulkan program put a real magenta frame on the host SDL (Gate-7 PASS), zero-copy
over venus. Mirror it. Exact structs (`:251-409`):

```c
// extensions enabled if advertised: VK_EXT_image_drm_format_modifier,
//                                    VK_EXT_external_memory_dma_buf   (:224-235)
const VkExternalMemoryHandleTypeFlagBits htype = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
const uint64_t modifier = DRM_FORMAT_MOD_LINEAR;                                     // :259

VkImageDrmFormatModifierListCreateInfoEXT modList = { ...LIST..., 1, &modifier };    // :323
VkExternalMemoryImageCreateInfo extImg = { ...EXTERNAL_MEMORY..., &modList, htype }; // :328
VkImageCreateInfo ic = { .pNext=&extImg, .format=VK_FORMAT_B8G8R8A8_UNORM,
    .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT, .usage=usage,                 // :342
    .sharingMode=EXCLUSIVE, .initialLayout=UNDEFINED, ... };
vkCreateImage(dev, &ic, NULL, &image);                                              // :348

// memory: prefer DEVICE_LOCAL|HOST_VISIBLE (optional), dedicated + exportable      // :355-399
VkExportMemoryAllocateInfo   expMem = { ...EXPORT..., .handleTypes=htype };          // :382
VkMemoryDedicatedAllocateInfo dedi  = { ...DEDICATED..., &expMem, .image=image };    // :386
vkAllocateMemory(...); vkBindImageMemory(dev, image, mem, 0);                        // :398-399

// stride/offset for SET_SCANOUT_BLOB come from the MEMORY_PLANE aspect, NOT width*4:
VkImageSubresource sub = { VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT, 0, 0 };           // :403
vkGetImageSubresourceLayout(dev, image, &sub, &lay);
uint32_t stride = lay.rowPitch;  uint32_t offset = lay.offset;                       // :406-407
// -> SET_SCANOUT_BLOB(res_id, W, H, BGRA, stride, offset)                           // :533
```

Host support notes (`:200-204, 277-280`): Intel/ANV is the known-good dmabuf exporter; **NVIDIA
host-visible export scans out black** (host-GPU dependent, not our bug). The
`vkGetPhysicalDeviceImageFormatProperties2` probe is advisory — "the host is the final judge".

---

## 3. Pipeline (where the primary is created → scanned out)

```
DWM (D3D11) --CreateTexture2D(pPrimaryDesc)--> UMD forward.rs create_resource
    -> [MARKER LOST TODAY: api_bind_flags drops BIND_PRESENT (forward.rs:807,823)]
    -> DXVK D3D11Device::CreateTexture2D -> D3D11CommonTexture ctor (d3d11_texture.cpp:20)
        -> DxvkImageCreateInfo{ tiling=OPTIMAL (:53), shared, sharing.type=OPAQUE_WIN32 }
        -> DxvkImage::allocateStorageWithUsage (dxvk_image.cpp:391)
            -> heliosRendererHandleType = OPAQUE_FD (:433)   <-- must become DMA_BUF
            -> VkExternalMemoryImageCreateInfo (:586) + VkExportMemoryAllocateInfo (:598)
            -> vkCreateImage (venus) -> HOST creates the VkImage
    -> UMD allocate_wddm_resource: meta.pitch = round_up(width*4,256) (forward.rs:945-950)  <-- guessed, not queried
    -> KMD create_one: AllocationContext.pitch/dxgi_format (create_allocation.rs)  [DONE this session]
    -> KMD SetVidPnSourceAddress -> SET_SCANOUT_BLOB(resid, w, h, BGRA, stride, offset)
       (display.rs; ctrl.rs set_scanout_blob) -> HOST exports dmabuf -> SDL
```

Two breaks: (1) the DXVK image has no DRM modifier / DMA_BUF handle → host dmabuf is
`MOD_INVALID` → black; (2) the stride is a 256-aligned guess, not the image's real `rowPitch`,
and the plane `offset` is never passed.

---

## 4. Change list per component

Ordered so each layer can be built + smoke-tested before the next. **All behind a knob** (§7).

### 4.1 Mesa venus ICD  (`icd/mesa/src/virtio/vulkan/`) — ~2 lines
The ONLY hard gate. Everything else (modifier ext advertisement, DMA_BUF in
`supported_handle_types`, Windows-skipped tiling rejection, host device force-add, image/memory
serialization) is **already wired** (agent-verified).

1. **`vn_physical_device.c` ~1319** (Windows `#else` branch of the `1304-1320` block): add
   ```c
   exts->EXT_external_memory_dma_buf = true;   // was compiled out on Windows
   ```
   Without this DXVK cannot enable `VK_EXT_external_memory_dma_buf` at `vkCreateDevice`
   (`VK_ERROR_EXTENSION_NOT_PRESENT`) and cannot legally chain the DMA_BUF external structs.
   `EXT_image_drm_format_modifier` is ALREADY advertised on Windows (`:1536`, unconditional).
2. **`vn_device_memory.c` `vn_GetMemoryFdKHR` (~:778-797)** — defensive: if
   `renderer->bo_ops.export_dma_buf == NULL` return `VK_ERROR_FEATURE_NOT_PRESENT` instead of
   calling the NULL fn-ptr (the Helios renderer NULLs it, `vn_renderer_helios.c:3405`). Latent
   even today (KHR_external_memory_fd already advertised); DXVK on Windows uses Win32 NT handles
   for its normal shares, not fd export, so this path shouldn't fire — but guard it.

**No** guest dma_buf export/import machinery is needed: the HOST exports the dmabuf from the
res_id for `SET_SCANOUT_BLOB` (`helios_vk_present.c:512-538`); `has_dma_buf_import=false` only
strips the IMPORTABLE feature bit, EXPORTABLE stays. Build: `win_meson` (Mesa ICD).

### 4.2 DXVK  (`dxvk-helios/src/`)
1. **Advertise the two device extensions.** `dxvk/dxvk_device_info.h` — add `extImageDrmFormatModifier`
   + `extExternalMemoryDmaBuf` bools (~:90) + `VkExtensionProperties` entries (~:161, names
   `VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME` / `VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME`).
   `dxvk/dxvk_device_info.cpp` — `HANDLE_EXT(...)` (~:48) + `ENABLE_EXT(..., false)` (~:1018).
   Mirror `khrExternalMemoryFd` (`dxvk_device_info.h:161`, `.cpp:1018`).
2. **Marker field.** `dxvk/dxvk_image.h:84` — add `VkBool32 heliosScanoutPrimary = VK_FALSE;`
   next to the existing `heliosDirectImportAlias` (copy that precedent).
3. **D3D11 ctor** `d3d11/d3d11_texture.cpp` (after the shared-flags block ~:53-88), when the
   marker is set (§4.3):
   - `imageInfo.heliosScanoutPrimary = VK_TRUE;`
   - `imageInfo.tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT;`
   - `imageInfo.shared = true; imageInfo.sharing.mode = Export; imageInfo.sharing.type =
     VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;`
   - **Guard the re-tiling paths** so the modifier tiling survives: skip the `!CheckImageSupport(
     ...,OPTIMAL)->LINEAR` (~:205-206), the `MAP_MODE_DIRECT->LINEAR` (~:215-216), and
     `OptimizeLayout` (~:258-259) when `heliosScanoutPrimary`. Zero the view-format list (the
     shared path at :198-201 already does this). Pass the modifier tiling through `CheckImageSupport`
     (~:576-604) so `getFormatLimits` runs a DRM-modifier query.
4. **Allocator** `dxvk/dxvk_image.cpp`:
   - `~:433` — when `m_info.heliosScanoutPrimary`, set `heliosRendererHandleType =
     VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT` (the existing `VkExternalMemoryImageCreateInfo`
     :586 + `VkExportMemoryAllocateInfo` :598 then propagate it).
   - Chain a `VkImageDrmFormatModifierListCreateInfoEXT{ 1, &DRM_FORMAT_MOD_LINEAR }` onto
     `imageInfo.pNext` (same `std::exchange` idiom as :421-422/:589; model on `helios_vk_present.c:322-332`).
   - Force dedicated allocation (`allocationInfo.forceDedicated`, ~:634) like the recipe (:386-390).
5. **Real row pitch/offset** `dxvk/dxvk_device.cpp` `queryImageSubresourceLayout` (~:71-106):
   it hardcodes `info.tiling = LINEAR` (:84) and is wired only for MAP_MODE_DIRECT. For a modifier
   primary, query with the real create-info + the **`VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT`**
   aspect (recipe :401-407) and expose `rowPitch` + `offset` to the UMD bridge (§4.3).
6. `DetermineMapMode` (~:617-719): a primary has no CPU access → already `{MAP_MODE_NONE,
   DEVICE_LOCAL}` (:621-622). **No change** (host-visible is optional; DEVICE_LOCAL export is valid).

### 4.3 UMD  (`umd/src/forward.rs`, `umd/bridge/dxvk_bridge.cpp`) — the marker + real pitch
The primary marker is currently DISCARDED (`api_bind_flags` drops `DDI_BIND_PRESENT`,
`forward.rs:807,823`; the public `D3D11_TEXTURE2D_DESC` has no scanout field). **Option 1 (recommended
by the DXVK sweep — reuses the proven `pHeliosImport` threading):**
1. New cxx bridge method `create_ddi_scanout_texture2d(...)` next to `open_ddi_texture2d`
   (`umd/bridge/dxvk_bridge.cpp:1157-1238`): constructs `new dxvk::D3D11Texture2D(device,&desc,
   nullptr,nullptr, &ci)` with a new `D3D11_HELIOS_CREATE_INFO{ ScanoutPrimary=true }` (define next
   to `D3D11_HELIOS_IMPORT_INFO`, `d3d11/d3d11_texture.h:85-89`) threaded through the
   `D3D11CommonTexture` ctor (`d3d11_texture.h:105-114`) — the exact `pHeliosImport` pattern.
   Bypasses public MiscFlags validation; no fake D3D bits.
2. `forward.rs create_resource` (RES_TEX2D arm ~:1219): when `!a.pPrimaryDesc.is_null() ||
   (BindFlags & DDI_BIND_PRESENT)`, call the new bridge method instead of `device.CreateTexture2D`.
3. The bridge returns the image's **real `rowPitch` + `offset`** (from §4.2.5); `allocate_wddm_resource`
   writes them into the trailer (`meta.pitch` = queried rowPitch, and a NEW `meta.offset`) instead
   of `cross_adapter_pitch` (`forward.rs:945-950`).

### 4.4 KMD  (`kmd_render/`) — pitch/format DONE, offset TODO
- **DONE this session** (`create_allocation.rs` + `display.rs`, compiles): `AllocationContext`
  carries `pitch` + `dxgi_format`; `ScanoutInfo` returns them; `SET_SCANOUT_BLOB` uses the real
  pitch (not width*4) + format from `dxgi_format`; diag `ScPch`/`ScFmt`.
- **TODO**: plumb the plane **offset** too. Add `offset` to `HeliosWddmAllocMeta` + `AllocationContext`
  + `ScanoutInfo`; pass it as `set_scanout_blob(..., offset)` (`display.rs` currently hardcodes the
  last arg 0; `ctrl.rs set_scanout_blob` already takes an offset param). The recipe passes `lay.offset`.
- The `set_scanout_blob` VIRTIO struct carries `strides[4]`/`offsets[4]` — plane 0 is what we set
  (`ctrl.rs:311-334`).

---

## 5. Correctness invariants (do not violate)

| Rule | Why |
|------|-----|
| Scanout image MUST be `TILING_DRM_FORMAT_MODIFIER_EXT` + explicit modifier + DMA_BUF export | plain OPTIMAL/LINEAR → `MOD_INVALID` → host black (`helios_vk_present.c:251-254`) |
| Stride = queried `VkGetImageSubresourceLayout(MEMORY_PLANE_0).rowPitch`, NOT `width*4`/256-align | a wrong stride shears the scanout; modifier images report per-MEMORY_PLANE layout |
| Pass the plane `offset` (offsets[0]) too | modifier images may place plane 0 at nonzero offset |
| Only the marked scanout primary gets the modifier path; all other DXVK textures unchanged | avoid perf/compat regressions on the general render path |
| Fail LOUDLY if `vkCreateImage`/bind rejects the modifier+DMA_BUF+usage combo | no silent fallback to a black OPTIMAL image (CLAUDE.md: loud failure over fake success) |
| GpuMmu / WDDM 3.2 / the v73 aperture-for-CpuVisible fix all STAY | orthogonal; already proven this session |

---

## 6. Risks / open questions (highest first)

1. **Usage-flag tension (BIGGEST unknown).** DWM's primary needs
   `COLOR_ATTACHMENT | SAMPLED | TRANSFER_*` (`d3d11_texture.cpp:116-184`), but the proven recipe
   used only `TRANSFER_DST` (`helios_vk_present.c:258`). A `DRM_FORMAT_MOD_LINEAR` image with full
   render-target usage may be unsupported by the host — run a `getFormatLimits` query with the
   modifier tiling + the real usage BEFORE committing; if unsupported, this may force the **copy
   path** (render optimal, blit into a `TRANSFER_DST` LINEAR-modifier scanout image per present)
   instead of zero-copy. Decide empirically.
2. **Row-pitch/offset must be the queried values**, not the 256-align guess (§4.3.3 / §4.4). If the
   host's `DRM_FORMAT_MOD_LINEAR` rowPitch ≠ `round_up(width*4,256)`, a guess shears the image.
3. **NVIDIA host** scans dmabuf-exported images black (`helios_vk_present.c:200-204`); Intel/ANV is
   known-good. Host-GPU dependent — verify on the actual host GPU before deep debugging.
4. **`canShareImage`** (`dxvk_image.cpp:923-970`) keys on `khrExternalMemoryWin32`; confirm the
   DMA_BUF/modifier primary takes a path where it returns true (or route through the Helios KMT
   bypass `:928-936`), else DXVK refuses to mark it shared.
5. **View creation on modifier images** — keep MUTABLE_FORMAT but zero the view-format list (the
   shared path already does, `:198-201`); verify BGRA SRV/RTV still create.
6. **Layout pinning** — modifier images must stay `GENERAL`; `pickLayout` coerces non-OPTIMAL to
   GENERAL (`dxvk_image.h:581-583`) — confirm no `OptimizeLayout` runs (guard `:258`).

---

## 7. Reversibility / gating (non-negotiable)

- Gate the whole scanout-primary-modifier path behind the existing **`DisplayHalf`** service knob
  (it only matters when Helios is the display) OR a new `ScanoutModifier` REG_DWORD, so a bad build
  reverts by flipping the knob + reboot without an unbootable default. The DXVK/ICD side can gate on
  an env var (e.g. `HELIOS_SCANOUT_MODIFIER=1`, read once) so it A/Bs without a rebuild.
- Each layer is independently testable: (ICD) `vkGetPhysicalDeviceImageFormatProperties2` for
  BGRA+DMA_BUF+modifier returns SUCCESS in a probe; (DXVK) the primary's VkImage reports
  `tiling=DRM_FORMAT_MODIFIER`; (UMD) the trailer carries the queried rowPitch/offset; (KMD)
  `ScPch`/`ScSet`=1 + SDL shows the desktop.
- Recovery to the known-good render-only desktop: `Enable-PnpDevice ROOT\DISPLAY\0000` +
  `DisplayHalf=0` + reboot.

---

## 8. Validation plan

1. **ICD probe first (no DXVK):** extend/rerun `helios_vk_present.c` (or its probe at :277) on the
   live host to confirm BGRA + `COLOR_ATTACHMENT|SAMPLED` + DRM_FORMAT_MOD_LINEAR + DMA_BUF is
   host-supported. If only `TRANSFER_DST` works → copy path (§6.1).
2. **ICD change** (§4.1) → `win_meson` → confirm DXVK can enable both extensions.
3. **DXVK + UMD** (§4.2/4.3) → confirm the primary's VkImage is a modifier image and the bridge
   returns a sane rowPitch/offset.
4. **KMD offset** (§4.4) → build+reboot → read `ScPch` (real pitch), watch SDL.
5. **Ground truth = the SDL window / `screen_copy.png`.** A correct frame = GO. Black = re-check the
   modifier/handle-type on the actual scanned resid + host GPU; sheared = pitch/offset wrong.

---

## 9. What is already DONE (37th session) vs TODO

- **DONE:** activation (v73 aperture-for-all-CpuVisible), UMD `Flags.Primary` fix (dwm primaries
  allocate), KMD stride+format plumbing (`ScanoutInfo` pitch/dxgi_format, real pitch not width*4).
- **TODO (this doc):** ICD 2-line gate (§4.1), DXVK modifier-image path (§4.2), UMD marker bridge +
  queried pitch/offset (§4.3), KMD offset plumbing (§4.4). Deploy: `win_meson` (ICD) + `win_dxvk`
  (DXVK) + `win_cargo umd` + `win_install_umd` + reboot; KMD via `win_build_kmd`/`win_install_kmd`.
