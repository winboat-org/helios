# Gate 5a Stage 2 — Allocations over D3DKMTCreateAllocation + Lock2

> **PROGRESS (2026-06-18, all LIVE-validated):**
> - **Inc 2a DONE:** KMD host-visible window plumbing ported into `kmd_render`
>   (`scan_host_visible_window`, `resource_map_blob`, `ctx_detach_resource`); real
>   `DxgkDdiCreateAllocation` (reads `HeliosWddmAllocPrivate` → `resource_create_blob`)
>   + `DxgkDdiOpenAllocation` (the load-bearing missing DDI — dxgkrnl opens every
>   allocation onto its creating device) + `DxgkDdiDestroyAllocation`.
>   `D3DKMTCreateAllocation` returns SUCCESS for a venus HOST3D blob.
>   **`blob_id=0` HOST3D blob creation WORKS** (the `.56` RESP_ERR_UNSPEC fear was
>   host-config-dependent and is gone).
> - **Inc 2b first light:** `D3DKMTLock2` already returns a writable CPU VA (no
>   segment reshape / no real BuildPagingBuffer), but VidMm backs it with SYSTEM
>   memory, not the host-visible BAR. Remaining 2b = the segment reshape below so
>   Lock2 maps the host-visible blob.
> - The "hard constraint" below (no DxgkDdiLock; Lock2 is segment-driven) is
>   confirmed — but the mapping machinery works out of the box; we only need to
>   point the segment at the BAR (`CpuTranslatedAddress = host_visible.base`).
> - ICD `D3DKMT_CREATEALLOCATION`/`ALLOCATIONINFO`/`LOCK2`/`DESTROYALLOCATION` come
>   from the real vendored WDK header now — NO struct hand-extraction needed.
> See `HANDOFF_NEXT_SESSION.md` for the exact next steps + the `gate5a-venus-d3dkmt`
> memory for every gotcha (escape-needs-hDevice, diag-ring-flooded-use-ntoseye,
> devcon-churn-degrades-stack).

Status: design, 2026-06-18. Grounded in live code reading of `kmd/src/virtio/gpu.rs`
(the proven System-class blob/map path), `kmd_render/src/virtio/gpu.rs`,
`kmd_render/src/ddi/{create_allocation,build_paging_buffer,query_adapter_info}.rs`,
`protocol/src/wddm.rs`, and `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c`.
Prereq: Stage 1 DONE (venus context up over D3DKMTEscape — see
[gate5a-venus-d3dkmt memory] / GATE5_VENUS_WDDM_DESIGN.md §8).

## The one hard constraint (confirmed by code reading)

`D3DKMTLock2` does NOT reuse the System-class trick. The System-class `kmd`
maps a blob to user space by building an MDL over the host-visible BAR pages and
calling `MmMapLockedPagesSpecifyCache(UserMode)` inside `IOCTL_HELIOS_MAP_BLOB`
(`kmd/src/ioctl.rs::handle_map_blob`). **There is no `DxgkDdiLock` callback** —
under WDDM, `D3DKMTLock2` is serviced by dxgkrnl/VidMm using the *segment
descriptor*, not the driver. So the KMD cannot hand back a user VA directly; it
must make VidMm able to map the allocation, by:
1. reporting the host-visible window as a **CPU-visible MEMORY segment** (not the
   current aperture segment), whose `BaseAddress` is the guest-physical base of
   the SHARED_MEMORY_CFG/HOST_VISIBLE BAR, and
2. ensuring the host has `RESOURCE_MAP_BLOB`'d the blob to the **same offset**
   within that window that VidMm assigned the allocation in the segment.

The enabling fact (from `kmd/src/virtio/gpu.rs`): **the guest chooses the
host-visible window offset** passed to `VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB` (the
System-class path uses a `next_window_offset` high-water allocator with
free-range reuse). So the KMD can take the VidMm-assigned segment offset and tell
the host to map the blob there — the two offset namespaces are made to coincide.

## Data model

- HOST3D mappable blob, `blob_mem = VIRTIO_GPU_BLOB_MEM_HOST3D`,
  `blob_flags = VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE`.
- `kind = SHMEM` → `blob_id = 0` (venus command/staging ring). `kind =
  DEVICE_MEMORY` → `blob_id = venus mem id` from the ICD's `vkAllocateMemory`.
  Both come from the ICD via `HeliosWddmAllocPrivate` (protocol/src/wddm.rs, 48B,
  already defined + C-mirrorable).
- NOTE the `.56` finding: a bare `RESOURCE_CREATE_BLOB(HOST3D, blob_id=0)` was
  rejected `RESP_ERR_UNSPEC` on kmd_render. But the System-class path renders
  fully (vulkaninfo/vkcube/Doom) with the host render-server config, so blob_id=0
  HOST3D rings DO work there. Re-verify on the current host config FIRST; if it
  still fails, the shmem ring may need a different blob_mem (GUEST) or the host
  render-server `supports_blob_id_0`. This is the first thing to test in Stage 2.

## KMD changes (`kmd_render`)

1. **Port host-visible window plumbing** from `kmd/src/virtio/gpu.rs` into
   `kmd_render/src/virtio/gpu.rs` (currently absent):
   - `HostVisibleWindow { base, len }` + `scan_host_visible_window()` (PCI cap
     walk for SHARED_MEMORY_CFG / `VIRTIO_GPU_SHM_ID_HOST_VISIBLE`) + `bar_base()`.
     Call it in `VirtioGpu::init`; store `host_visible: Option<HostVisibleWindow>`.
   - `resource_map_blob(resource_id, offset) -> map_cache` issuing
     `VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB` (parse `RESP_OK_MAP_INFO`), and
     `resource_unmap_blob`. (Mirror `map_blob_prepare`/`BlobMapPrep`.)
   - A per-(ctx,resource) blob table tracking `{resource_id, blob_id, size,
     map_offset, map_len, mapped}` so DestroyAllocation can UNMAP+DETACH+UNREF
     (mirror `BlobSlot` + `release_blob_slot`). VidMm assigns offsets, so the KMD
     does NOT need its own offset allocator — it uses the segment offset.

2. **Segment reporting** (`query_adapter_info.rs::query_segments`): replace the
   single 64MiB aperture segment with a **CPU-visible memory segment** describing
   the host-visible window: `Flags.CpuVisible = 1`, NOT `Aperture`,
   `BaseAddress.QuadPart = host_visible.base` (guest-physical), `Size =
   host_visible.len`, `CommitLimit = len`. Keep a small paging-buffer segment if
   dxgkrnl needs one. (If `host_visible` is None, Stage 2 can't run — fail
   honestly.) Verify caps stay coherent (Code 0) after this change — segment
   shape changes are load-bearing.

3. **`DxgkDdiCreateAllocation`** (`create_allocation.rs`, currently bookkeeping):
   - Read `pAllocationInfo[i].pPrivateDriverData` as `HeliosWddmAllocPrivate`;
     `is_valid()` (magic/version) + bounds-check `PrivateDriverDataSize`.
   - `with_virtio(|v| v.resource_create_blob(ctx_id, blob_mem, blob_flags,
     blob_id, size))` → store `resource_id` in the `AllocationContext` (alongside
     `size`, `blob_id`, `kind`, `ctx_id`). Record blob in the blob table.
   - Fill `pAllocationInfo[i]`: `Size` (page-rounded), `Flags.CpuVisible = 1`,
     `SupportedReadSegmentSet/WriteSegmentSet = bit0` (segment 1),
     `PreferredSegment`, `Alignment = PAGE`. Do NOT map yet (VidMm hasn't placed
     it). Breadcrumb `0x0C01_xxxx`.

4. **`DxgkDdiDescribeAllocation`** — fill real width/height/format/refresh for the
   allocation (needed once submits reference it). For a raw blob, report a 1-D
   buffer-like description.

5. **`DxgkDdiBuildPagingBuffer`** (`build_paging_buffer.rs`, currently null) — the
   load-bearing new code. Handle the ops VidMm drives for a CPU-visible memory
   segment, in particular **map/fill into the segment**: when VidMm assigns the
   allocation a `PhysicalAddress`/segment offset, issue
   `resource_map_blob(resource_id, offset)` so the host backs that exact window
   offset with the blob. Handle `DXGK_OPERATION_{FILL,TRANSFER,MAP_APERTURE_SEGMENT
   /UNMAP}` as the live trace shows them. Discover the exact op + where the offset
   arrives via ntoseye breakpoints + `diag::record` breadcrumbs (this is empirical,
   like Gate 1). Must not emit DMA we can't honor; must not fail in steady state.

6. **`DxgkDdiDestroyAllocation`** — UNMAP_BLOB (if mapped) + free, DETACH_RESOURCE,
   UNREF (mirror `release_blob_slot`); drop the `AllocationContext`.

## ICD changes (`vn_renderer_helios.c` + `helios_d3dkmt.h`)

1. **Shim** (`helios_d3dkmt.h`): add `D3DKMT_CREATEALLOCATION`,
   `D3DDDI_ALLOCATIONINFO2` (or `_ALLOCATIONINFO`), `D3DKMT_LOCK2`,
   `D3DKMT_DESTROYALLOCATION`, and the `D3DKMTCreateAllocation` / `D3DKMTLock2` /
   `D3DKMTUnlock2` / `D3DKMTDestroyAllocation` prototypes — copy field-for-field
   from WDK shared/d3dkmthk.h, x64, inner ptrs→void*, flags→UINT (same method as
   Stage 1's shim). These are the gdi32 thunks.
2. Mirror `HeliosWddmAllocPrivate` (48B) in C (like the escape structs).
3. `helios_shmem_create` (blob_id=0) + `helios_bo_create_from_device_memory`
   (blob_id=mem_id): build `HeliosWddmAllocPrivate`, call `D3DKMTCreateAllocation`
   with one `pAllocationInfo` carrying it; on success the allocation handle is the
   blob. `helios_bo_map`/shmem map → `D3DKMTLock2` → CPU VA. destroy →
   `D3DKMTDestroyAllocation`. Replace the fail-clean `helios_ioctl_*` calls in
   these ops; keep submit/wait on the stub until Stage 3.
4. The allocation must be created against the **device** (`D3DKMTCreateAllocation`
   takes `hDevice`), which Stage 1 already opens (`helios->device`).

## Staged test plan (vulkaninfo + offscreen helios_vk_* tests; NO vkcube — needs DWM)

- **2a**: KMD CreateAllocation creates the virtio blob. Test: a tiny D3DKMT
  harness (or the ICD shmem_create) calls `D3DKMTCreateAllocation` → breadcrumb
  `0x0C01_xxxx` + the blob `resource_create_blob` returns RESP_OK (re-verify the
  blob_id=0 question here). No Lock2 yet.
- **2b**: CPU-visible memory segment + `D3DKMTLock2` returns a VA; write a
  sentinel, read it back via a second map (or host-side) → proves the
  guest↔host-visible mapping. This is where BuildPagingBuffer + the offset
  coupling get debugged live (ntoseye on BuildPagingBuffer).
- **2c**: ICD `shmem_ops`/`bo_ops` fully on D3DKMT → `vkCreateInstance` gets its
  venus command ring shmem (it still can't finish until Stage 3 submit, but the
  ring shmem alloc+map must succeed). Confirm via the `HELIOS[gate5a]` breadcrumbs.

## Risks / watch-outs

- Segment-shape change can regress Code 0 (Gate-1 bisect showed caps are
  load-bearing). Change incrementally, verify Code 0 + dxdiag after each KMD build.
- BuildPagingBuffer is DISPATCH_LEVEL — `diag::record` is PASSIVE only; use a
  lock-free counter or ntoseye, not `diag::record`, on that path.
- The blob_id=0 shmem-ring question (above) gates everything — test it first.
- Each KMD build = devcon update = churns the (already DWM-crashing) desktop;
  the user reboots/relaunches the VM per the ownership rule.
