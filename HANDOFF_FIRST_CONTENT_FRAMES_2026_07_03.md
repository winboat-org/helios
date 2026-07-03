# Handoff — FIRST REAL CONTENT IN THE IddCx SWAPCHAIN (root cause: ICD buffer-requirements inconsistency), plus the stale-DriverStore-UMD boot gap and the ClearView-rects bug (2026-07-03, seventh session, cold-booted NVIDIA)

Supersedes the *conclusions* of `HANDOFF_UMD_CONTENT_BUGS_2026_07_03.md` (its four fixes
remain valid and deployed; its "remaining blocker" is now SOLVED). Read that doc for the
blend/typed-signature/rotation/discard fixes; read THIS doc first.

## ⚡⚡ THE MILESTONE (instrument-verified, this session)

After the ICD fix below, on the live cold-booted NVIDIA guest:

- dwm write-side (the registry-gated rotation sampler):
  `rotate-sample: slot=1/3 1896x1030 nonzero=510/510 center=0xff00000c` — a fully
  composed frame sitting in the flip ring.
- IDD read-side (LGIdd's per-30-frames sampler):
  `frame 1: … first=0xff003655 center=0xff0b0c1a sampleNonZero=357/357
  sampleHash=0x94af4df69ca3e3ae` — the acquired IddCx buffer carries REAL desktop
  pixels (Windows-wallpaper blues), first time ever at steady state.

Acceptance ladder: (1) sampler nonzero ✅ REACHED. (2) owner watches the live desktop in
the LG client, sustained — NOT yet verified (owner AFK; and see "stale binding" below:
the pairing stalled again after two good frames on this churn-saturated boot, so a clean
cold boot is the right stage for owner eyes).

## ⚡ ROOT CAUSE of the all-zero composition (the week's blocker)

**One inconsistency inside the ICD made every DXVK dynamic/upload BUFFER lose its data.**

Chain, each link instrument-confirmed:

1. `vn_CreateBuffer` (icd/mesa `vn_buffer.c`) injects the renderer external handle type
   into EVERY buffer create (`vn_buffer_fix_create_info`) — required because vkr
   force-exports HOST_VISIBLE memory and un-matched binds are VUID-02726 UB (the old
   NVIDIA white-screen fix). External buffers get a NARROWED `memoryTypeBits` from the
   host (NVIDIA excludes some host-visible types for external buffers).
2. `vn_GetDeviceBufferMemoryRequirements` — the maintenance4 query DXVK uses to compute
   its global buffer-memory mask up front — did NOT apply the same fix-up, so it
   reported the PLAIN-buffer mask (`0x1f`, all five types).
3. DXVK trusted the wide mask, placed buffer chunk allocations in memory type 4, and
   every per-chunk global-buffer create then failed the narrowed-requirements check in
   `DxvkMemoryAllocator::allocateDeviceMemory`
   (`requirements.memoryTypeBits & (1u << type.index)` — dxvk_memory.cpp:1466) →
   `warn: Failed to create global buffer: … type: 4` +
   `err: Got allocation from memory type 4 without global buffer` (both visible in the
   failing probe's own DXVK log).
4. DXVK's suballocator then handed out **buffer-less allocations** (the audit C7.1
   "keep the allocation around for now" branch — the audit's later "self-repairs via
   dedicated fall-through" refinement was WRONG). CPU writes through `Map(WRITE_DISCARD/
   NO_OVERWRITE)` and GPU writes via `UpdateSubresource` landed in memory **no VkBuffer
   aliases** → every draw consuming dynamic vertex data or updated constant buffers read
   ZEROS → zero transforms → degenerate geometry → zero fragments → all-black
   composition. dwm rides exactly these two paths (a 240 KB DYNAMIC vertex buffer via
   `Map(NO_OVERWRITE)`, DEFAULT constant buffers via `UpdateSubresource`); probes had
   always used IMMUTABLE+init-data buffers, which bypass both — why every probe passed
   while dwm stayed black.

**The fix (icd/mesa `vn_buffer.c`, committed):** make the requirements query describe the
same create info the real create will use:

```c
VKAPI_ATTR void VKAPI_CALL
vn_GetDeviceBufferMemoryRequirements(..., const VkDeviceBufferMemoryRequirements *pInfo, ...)
{
   ...
   /* Helios: vn_CreateBuffer injects the renderer external handle type into
    * every buffer create (vn_buffer_fix_create_info). The requirements
    * reported HERE must describe the same fixed-up create info, or callers
    * compute memory-type masks the real (external) buffer cannot satisfy. */
   const VkExternalMemoryHandleTypeFlagBits renderer_handle_type =
      dev->physical_device->external_memory.renderer_handle_type;
   const VkExternalMemoryBufferCreateInfo *external_info =
      vk_find_struct_const(pInfo->pCreateInfo->pNext, EXTERNAL_MEMORY_BUFFER_CREATE_INFO);
   struct vn_buffer_create_info local_info;
   VkDeviceBufferMemoryRequirements fixed_info;
   if (renderer_handle_type &&
       (!external_info || !external_info->handleTypes ||
        external_info->handleTypes != renderer_handle_type)) {
      fixed_info = *pInfo;
      fixed_info.pCreateInfo = vn_buffer_fix_create_info(
         pInfo->pCreateInfo, renderer_handle_type, &local_info);
      pInfo = &fixed_info;
   }
   ... /* existing cache + host-call body */
}
```

Notes: the injected pNext makes the entry non-cacheable in `vn_buffer_get_cache_index`
("other pNext structs are not cacheable"), so this query now always round-trips — the
buffer-reqs cache is effectively dead on this stack (correctness > caching; a follow-up
could teach the cache key about EXTERNAL_MEMORY_BUFFER_CREATE_INFO since it is now
uniform). Verification: the dwm-shaped probe passes (below), DXVK's "Memory type mask for
buffer resources" shrinks to the truthful set, no more "without global buffer" errors,
and dwm composes.

**Minimal repro (kept, `tools/d3d11_shared_draw_probe.cpp`, task `HeliosDrawProbe`):**
the probe's SINT quad streams vertices through a DYNAMIC buffer `Map(WRITE_DISCARD)`
and the textured quad reads a DEFAULT CB filled by `UpdateSubresource` — dwm's exact
data paths. Pre-fix: both draw NOTHING (readbacks stay at the clear color). Post-fix:
magenta/orange quads render and propagate cross-device. (The probe's final
"propagate=PASS/FAIL" verdict string compares stale expected values — read the raw hex
lines, not the verdict.)

## ⚡ Second confirmed root cause: STALE DriverStore UMD at every cold boot

Cold-boot forensics (dwm pid 1912): its FIRST two devices (incl. the long-lived
composition device) called the OLD untyped shader handlers while devices 3/4 (created
after pairing churn) used the new typed ones — same process, one loaded DLL at
inspection time, interface 0x000b000f for all four, identical fill code. Explanation
(confirmed): the ACTIVE DriverStore package
(`FileRepository\helios_kmd_render.inf_amd64_7bf334c168c67d1d\helios_umd.dll`) still
held a 15:12 build (pre-ALL-fixes). **At cold boot dxgkrnl's first UMD resolution loads
the DriverStore copy; the ProgramData registry override only takes effect for later
device creates.** So every cold boot ran dwm's composition on an ancient UMD regardless
of hotplug deploys. Fixes:
- The DriverStore copy was synced by hand this session, and
- `tools/hotplug-helios-umd.ps1 -Mode ProgramData` now ALSO syncs the active DriverStore
  package copy (takeown/icacls + Copy-Item; committed). Every future deploy keeps boot
  and hotplug paths identical. Device-2's funcs table being freed (`Bad virtual address`
  reading it via ntoseye) plus the perfect old/new per-device split was the discriminating
  evidence.

## ⚡ Third fix: ClearView must honor rects (twin of the Discard bug)

`clear_view_11_1` forwarded rect-limited clears as WHOLE-view `ClearRenderTargetView` —
dwm clears exactly the damaged region each frame, so the accumulated desktop was wiped
to transparent black every frame. Unlike Discard (a hint — partial discards may be
dropped), a rect clear MUST clear exactly the rects: now forwarded through
`ID3D11DeviceContext1::ClearView(view, color, pRects)` (D3D10_DDI_RECT is
layout-identical to RECT; DXVK implements ClearView incl. rects). Committed in
forward.rs. Non-RTV view types log loudly and drop (extend on evidence).

## What is now KNOWN-GOOD end to end (all on cold-booted NVIDIA, instrument level)
- Full pipeline draws with: float/SINT typed inputs, indexed + instanced-free paths,
  CBs (whole-buffer binds; dwm passes null first/count pointers), textures + samplers +
  src-over blending, DYNAMIC buffers via Map(DISCARD/NO_OVERWRITE), DEFAULT buffers via
  UpdateSubresource — render correctly and propagate cross-device, cross-process
  (`d3d11_xproc_draw_probe.cpp`), and in session 0 (task `HeliosDrawProbeSys`).
- Flip-model identity rotation cycles the 3-buffer ring (presents cycle allocations;
  runtime calls the DDI every flip present).
- dwm composes real frames into the ring; the IDD acquires and samples them; content
  reaches LGIdd's KVMFR copy path.

## Remaining work, in priority order

1. **Owner-visible acceptance on a clean cold boot.** This boot ate ~12 device restarts
   (deploy churn): repeated WUDFHost verifier kills + two dwm collateral crashes (all in
   restart windows; buckets `ucrtbase!abort` / `ucrtbase!common_exit` — known classes).
   With the DriverStore sync in place, a fresh cold boot runs the fixed stack from the
   first dwm device. Expected: desktop in the LG client. If the C1 boot hole or pairing
   lottery bites, one `devcon restart @ROOT\DISPLAY\0000` recovers.
2. **LGIdd mid-stream acquire-stall watchdog (C5/D-B2 gap, observed live).** After the
   first two content frames, a churn-window WUDFHost kill left dwm presenting (20
   presents) while acquires pinned at 2 — the FIRST-frame watchdog was already disarmed
   and nothing watches for mid-stream stalls. Implement: if presents-era signals exist
   (or simply: bound swapchain + zero acquires for N seconds while the LGMP timer runs)
   → replug. Same CIndirectDeviceContext state machine as the existing watchdogs.
3. **Rotation cost (C3-adjacent):** `rotate_resource_backings` fully syncs the device
   per flip present (event-query spin) before swapping storages. Fine at bring-up
   cadence; revisit with the real async fence work (C3) — the proper form is a CS-timeline
   storage swap (what DXVK relocation does) instead of a full sync.
4. **Instrument hygiene:** `HKLM\SOFTWARE\Helios!RotateSample` (DWORD, currently 16)
   gates the write-side sampler — set 0/delete for production runs;
   `!ShaderDumpPath` (REG_SZ) routes DXVK shader dumps into any UMD process (currently
   deleted). Consider promoting both into HELIOS docs. The probe verdict-line cleanup.
5. **Buffer-reqs cache** is now always-miss (see above) — optional perf follow-up.
6. Previous backlog unchanged: §5 residual C1 boot hole, P2/C6 GDI import sizes, KMD
   diag-tracer strip, DXVK null-descriptor VUIDs, imageLayout-00344.

## Corrections to the audit (`HELIOS_FIRST_PRINCIPLES_AUDIT.md`)
- **D-A1 refined attribution ("the without-global-buffer path self-repairs") is WRONG**
  — that path hands out buffer-less allocations and was THE content killer (C7.1's
  original wording was right). The DxvkBuffer null-storage guards from P0 masked the
  crash but not the data loss.
- **"The DDI signature entries lack types" (C-class, forward.rs:4605) is WRONG for
  >=11.1** — `D3D11_1DDIARG_SIGNATURE_ENTRY2.RegisterComponentType` carries them; the
  typed-signature fix uses them.
- **K-B2 ("DxgkDdiPresent nop is the pixel-mover") is falsified** — DxgkDdiPresent is
  never called on this stack; flip presents hand dwm's buffers to IddCx directly with
  no blit. The 'HEPR' nop DMA is dead code for the composition path.

## Ops constants (delta from previous handoffs)
- Deploy now = hotplug script (it also syncs DriverStore). KMD/LGIdd/ICD recipes
  unchanged. ICD rebuild: `win_meson` (defaults compile the standard build dir) then
  `tools/install-helios-icd.ps1`, then restart UMD users (hotplug does it).
- ntoseye after a guest reboot: restart the server (`kill` + `ntoseye -b memory mcp
  --http 127.0.0.1:8080`) or `coherent:false` persists; drive it via raw JSON-RPC
  (initialize → notifications/initialized → tools/call with the Mcp-Session-Id header).
- `helios_icd_submit.log` needed `icacls … /grant Everyone:(M)` for session-0 writers.
- dwm's venus submits are all 24-byte `vkNotifyRingMESA` escapes — the Vulkan stream
  rides the ring, so submit-log sizes tell you nothing about workload.
