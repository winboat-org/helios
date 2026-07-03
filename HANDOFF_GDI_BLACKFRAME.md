# Helios vGPU — Handoff: GDI black-frame fix (2026-07-02)

> **UPDATE (2026-07-03 session, Fable 5): §3's load blocker is SOLVED and §1's research is ANSWERED —
> see §6 (appended at the bottom) for the new state: KMD .37 deployed Code 0, executor LIVE
> (GdiE grows, GdiS=0 after the blob-tracking fix), first real desktop content (taskbar + text)
> visible in the LG client frames, research verdict = BasicRender turns the GDI cap OFF, and the
> remaining gap is the CPU↔blob coherence design. §§1–5 below are the prior session's text.**

**This supersedes the remaining open items of `FABLE5_HANDOFF.md`** (whose §1 "one question" is
ANSWERED — see banner there). Memory: `idd-code43-double-delete-rootcause` has the full evidence
chain. Everything below was verified this session, mostly debugger-free.

## 0. Where things stand (all deployed, all working right now)

- **Frames flow end-to-end**: DWM composes the desktop on Helios → OS indirect swapchain → blt
  queue → LGIdd acquires → KVMFR → Looking Glass client shows 1920x1080 BGRA. **Content is BLACK**
  (see §2). The old Code 43 is fully explained and fixed-around: it was the **IddCx watchdog**
  killing WUDFHost after ~1–2 min of no frame progress (both prior "verifier failure" theories were
  this), stacked on an LGIdd double-delete (ownership fix landed: on an AssignSwapChain failure
  return, IddCx owns/deletes the swapchain — MS-docs-confirmed).
- Deployed: KMD **22.22.33.0** (devcon-restored, Code 0, store `1e1c0ddccb992c30`), that store's
  **UMD carries today's input-layout fix** (real bug, keep), LGIdd **20.23.27.300** (ownership fix +
  strict-fail→ABANDON).
- **Recipe to (re)start frames after any boot**: `pnputil /restart-device 'ROOT\DISPLAY\0000'` once
  the system is settled. (Boot's FIRST swapchain is stillborn — render/indirect pairing-instance
  LUID churn — and the OS never re-offers before the watchdog kills the host. Boot-time
  self-convergence is still an open, separate item.)

## 1. THE TASK: make GDI window content real — THE PROPER WAY

**Root cause of black frames (evidence-complete):** all GDI-drawn window content (explorer, shell,
LogonUI — i.e. everything) arrives at `DxgkDdiRenderGdi` as `DXGK_RENDERKM_COMMAND` ops, which the
KMD records-and-discards (null engine). Window surfaces stay zero; DWM *correctly* composes a black
desktop. Proof chain: `tools/helios_clear_test.cpp` (clear+staging readback) **PASSES** on the
current stack (venus GPU readback works → zero-readings are real); DWM's 1920x1080 draws target
exactly the allocations it presents; UMD input layouts were broken and are now FIXED
(`CreateInputLayout ok=7`, was 0) — and the composition is *still* zero because its **sources** are
empty.

**★ OWNER DIRECTIVE (2026-07-02):** the session's first attempt — a CPU software rasterizer in the
KMD (`kmd_render/src/ddi/gdi_blit.rs`, written, builds, unproven) while *advertising GDI hardware
acceleration* — is **very hacky and probably not the right architecture. GDI on Windows is
primarily software-rendered; modern GPUs do not expose GDI raster-op hardware, so real modern
drivers cannot be hand-rastering in the miniport either. There must be a proper mechanism — find
it before shipping the blitter.** Research leads, in rough order:

1. **`DXGK_PRESENTATIONCAPS` per-op decline bits** (`NoSameBitmapAlphaBlend`,
   `NoSameBitmapStretchBlt`, `NoSameBitmapTransparentBlt`, `NoScreenToScreenBlt`, etc., and the
   semantics of `SupportKernelModeCommandBuffer` itself): read the MS Learn "GDI Hardware
   Acceleration" + `DXGK_PRESENTATIONCAPS` + `DxgkDdiRenderGdi` pages carefully. The win7-era GDI
   HW-accel design has a **per-surface/per-op software fallback**: GDI can render on the CPU into
   the GDI surface's CPU-visible backing (Lock/aperture path) when the driver declines the op.
   If Helios can decline everything (or report GDI surfaces CPU-accessible in the right way), GDI
   itself does the rendering and the KMD only needs its existing coherent host-visible backing —
   zero KMD raster code. Figure out what combination of caps/allocation flags triggers that path
   on a modern (WDDM 3.2, 24H2) dxgkrnl.
2. **How real open-source virtual WDDM render drivers handle this**: viogpu3d (Red Hat, closest
   analog), VirtualBox `VBoxWddm`, qxl-wddm-dod, VMware SVGA. Check what they set in
   PresentationCaps and what their `DxgkDdiRenderKm`/`RenderGdi` do (execute? decline? convert to
   GPU blits?). Real vendor drivers lower these ops to GPU blits via their DMA engine — Helios's
   analog would be venus/Vulkan copies, which is heavyweight; the decline/CPU path is likely
   correct for a paravirt adapter.
3. **`D3DKMDT_STANDARDALLOCATION_GDISURFACE` semantics** (`GetStandardAllocationDriverData`):
   the GDISURFACE type has fields controlling CPU visibility / `ExistingSysMemVirtualAddress`-like
   behavior. Maybe the proper fix is reporting GDI surfaces so that GDI's software rendering lands
   directly in the (already host-coherent) host-visible blob — pitch/CPU-visible flags — instead of
   expecting the driver to blit.
4. **Caveat on my failed experiment:** dropping `SupportKernelModeCommandBuffer` (v22.22.34.0)
   produced `CM_PROB_FAILED_ADD` — but see §3: that result is **confounded** and must be re-run
   after the load-failure mystery is solved. Do not trust "the cap is load-mandatory" yet.

## 2. Ready-made pieces from this session (keep or discard per §1's outcome)

- `kmd_render/src/ddi/gdi_blit.rs` — complete CPU executor (BITBLT incl. overlap-safe scrolls +
  XOR/AND/OR rops, COLORFILL, ALPHABLEND, STRETCHBLT-nearest+mirror, TRANSPARENTBLT, approx
  CLEARTYPEBLEND), panic-free, bounds-checked, diag counters `GdiE`/`GdiS`. Wired into
  `dxgkddi_render_gdi` (submit_command.rs) via context→device→adapter. Plumbing that is useful
  regardless: `VirtioGpu::blob_kernel_range` (gpu.rs — kernel-side map of any blob into the
  host-visible window) and `cross_adapter_pitch` now pub(crate).
- **UMD input-layout fix (DEPLOYED, KEEP)**: `umd/src/forward.rs` — `build_layout_signature_blob`
  synthesizes a DXBC container with a fabricated `TEXCOORD<register>` ISGN so
  `CreateInputLayout` works against raw-token DDI shaders (DXVK location=register matches
  dxbc-spv's container-less compile). Also present-ordinal forensics in `dxgi_present`.
  Known UMD bug left: `deallocate_resource` returns 0x80070057.

## 3. BLOCKER TO SOLVE FIRST: v22.22.34/35 KMD builds fail to LOAD (`CM_PROB_FAILED_ADD`)

Both today's KMD builds fail identically: service registered but never started, zero events, no
CodeIntegrity complaints, imports identical to .33, package/store hashes consistent, fresh devnode
enumeration also fails — while the same devnode on the same boot accepts .33 instantly
(`devcon update <store-inf> <hwid>`). **Because .35 (cap restored) failed exactly like .34
(cap dropped), the "cap is load-mandatory" bisect conclusion is UNRELIABLE — both may share an
unrelated cause.** Suspects:
- **Accumulated uncommitted tree changes**: .33 was built 2026-06-25 from the then-tree; today's
  builds picked up everything since (whatever the 06-25/26 sessions edited in kmd_render but never
  test-built). Bisect: revert ONLY today's KMD edits (`gdi_blit.rs` new file; submit_command.rs
  hook + import; gpu.rs `blob_kernel_range` + import; query_adapter_info.rs
  `ADVERTISE_GDI_HW_ACCELERATION` refactor; version bumps in build.rs/Cargo.make.toml) → build
  .36 → if it STILL fails, the regression predates today; walk further back.
- The two installer instances that raced at one point (my mistake) — always reinstall cleanly once
  before concluding anything.
- The diag ring in the registry is STALE across boots — clear the `S*` values before a test boot so
  breadcrumb evidence is unambiguous (a `DriverEntry` breadcrumb proves the image loaded).

## 4. Ops notes (hard-won today)

- **KD attached freezes the guest at `dxgkrnl!DpUnmapMemory+0x702` during EVERY driver
  install/device restart** (benign break; bugcheck tool returns null). Installers "hanging" and
  `win_exec` "No route to host" = pump `ntoseye resume`/`wait_for_stop` — not a crash. Same for the
  boot flood and per-WUDFHost-launch 0x80000003 stops.
- devcon: use the **x64** binary explicitly (`...\Tools\10.0.26100.0\x64\devcon.exe`) —
  a recursive search finds arm64 first, which exits 0 silently doing nothing.
  `devcon update <driverstore-inf> <hwid>` force-installs a specific version and recovers a
  FAILED_ADD devnode **without reboot**.
- DriverStore writes need `takeown /f <dir> /a` + `icacls <dir> /grant Administrators:(OI)(CI)F`;
  UMD rename-aside then works. Killing `dwm.exe` reloads the UMD without reboot but abandons the
  IDD swapchain (LGIdd queues a replug that can stall) → follow with the §0 device-restart kick.
- IDD swapchain retries are budget-limited per boot; the kick works when the system is settled.
- LG client log: `/tmp/helios-looking-glass-client.log`; IDD log:
  `C:\ProgramData\Looking Glass (IDD)\looking-glass-idd.txt`; UMD/ICD per-pid logs:
  `C:\ProgramData\Helios\umd-<pid>.log`.

## 5. Uncommitted changes today (do NOT commit; tree already carried ~3000 lines before)

- `LookingGlass/idd/LGIdd/{CIndirectMonitorContext,CSwapChainProcessor,CIndirectDeviceContext}.{cpp,h}`
  — ownership fix, DisownSwapChain, strict-fail Start(), SelectRenderAdapter moved to FinishInit
  (deployed 20.23.27.300, oem104).
- `umd/src/forward.rs` — input-layout synthesis + present forensics (deployed via rename-aside into
  store `1e1c0ddccb992c30`).
- `kmd_render/` — gdi_blit.rs (new), submit_command.rs, gpu.rs, query_adapter_info.rs, version
  22.22.35.0 in build.rs/Cargo.make.toml (BUILT but NOT loadable, see §3; deployed-and-active is
  the old .33 package).
- `tools/helios_clear_test.cpp` — multi-adapter + FL-fallback fixes. `tools/attach_idd.ps1` — cdb
  attach helper (untested). Docs: this file + FABLE5_HANDOFF.md banner.

## 6. 2026-07-03 session results (supersedes §1's open questions and §3)

### 6a. §3 load blocker SOLVED — it was the packaging, not the code

v22.22.36.0 = the identical tree (gdi_blit et al.) built with the §BRINGUP_QUIRKS-§1/§2 discipline
(purge fingerprint → `win_cargo make` → **manual repackage**: copy fresh `deps\*.dll` → package
`.sys` → sign → **delete `.cat`** → x86 `Inf2Cat` standalone → sign cat → `signtool verify /pa /c`)
installs and runs **Code 0** first try. Root cause of .34/.35 `CM_PROB_FAILED_ADD`: cargo-make
packaged/signed a **stale `.sys`** (caught in the act this session: package `.sys` 120320 B vs
fresh dll 113152 B) — likely plus the corrupt-`.cat` re-inf2cat quirk — compounded by the racing
installers. ⇒ **the ".34 cap-drop → FAILED_ADD" bisect datum is void.** The cap-off experiment has
NOT been re-run yet (see 6d).

### 6b. Executor was skipping 100% — blob-tracking gap FIXED (v22.22.37.0, deployed, Code 0)

`GdiS=28/GdiE=0` root cause: `create_one` creates standard-allocation blobs via
`resource_create_blob` directly, which never enters the `VirtioGpu::blobs` tracking table (only
`alloc_blob`/`note_blob_size` do) → `blob_kernel_range` found nothing → every op skipped. Fix:
`create_one` now `note_blob_size(rid, size)`s KMD-created blobs; `destroy_allocation_ctx` removes
the slot symmetrically via new `VirtioGpu::forget_allocation_blob` (unmaps + frees the window
range if the executor had host-mapped it; also finally performs the 06-23 "adopted owner-0 entry"
cleanup that `forget_unmapped_blob_for_owner` was written for but never called). Skip-reason
counters added (`GdFa` alloc-resolve / `GdFg` geometry / `GdFs` slot / `GdFb` blob-lookup /
`GdFm` MmMapIoSpace / `GdFr` last-failed resid) next to `GdiE`/`GdiS`. **Result: GdiE grows,
GdiS=0, all failure counters 0.**

### 6c. FIRST REAL CONTENT END-TO-END (visual proof)

Clean boot (no KD) + one settled `pnputil /restart-device ROOT\DISPLAY\0000` → IDD Code 0, frames
advance, and the KVMFR frame (dumped host-side from `/dev/kvmfr0`, BGRA stride 1920) shows the
**Windows 11 taskbar rendered correctly: Start button, search pill, tray icons, "IN" locale
indicator, readable "02-07-2026" date text**. Everything else is still black. Interpretation:
the taskbar is XAML/DirectComposition → renders through UMD/DXVK/venus (that whole path is good);
only ~18 GDI ops arrived all boot, so classic-GDI content (wallpaper blit, GDI window interiors)
is either not yet drawn or draws from **CPU-written `STAGING_CPUVISIBLE` surfaces whose bytes the
executor cannot see** (see 6e). KVMFR frame dump recipe: mmap `/dev/kvmfr0`, frame data ≈
`0x2aed000`, pitch 7680.

### 6d. Research verdict (two deep-dives, MS docs + open-source drivers — full reports in the
session transcript; key facts):

- **MS docs**: GDI HW accel is "Mandatory" for render-only drivers **only in the WHQL feature
  table** ("WDDM Driver and Feature Caps"); once `SupportKernelModeCommandBuffer=1` there is **no
  caps combination that declines the six base ops** (only same-bitmap/overlap variants + ROP3 via
  `SupportAllBltRops=0`). GDI's own CPU rendering lands in `D3DKMDT_GDISURFACE_STAGING_CPUVISIBLE`
  / `EXISTINGSYSMEM` allocations (linear, cache-coherent, driver-returned pitch);
  `GDISURFACE_TEXTURE` is *never* CPU-visible to GDI; `TEXTURE_CPUVISIBLE` is "reserved for system
  use". RenderGdi completion is observed by dxgkrnl **only through the ordinary DMA fence**.
- **Every open/in-box render driver turns the cap OFF.** Disassembled **BasicRender.sys**
  (10.0.28000.2336 + public PDB): `WarpKMQueryAdapterInfo` does
  `PresentationCaps = (caps & ~0x00090004) | 0x1006C003` → `SupportKernelModeCommandBuffer=0`
  **explicitly cleared**, `NoScreenToScreenBlt=1, NoOverlapScreenBlt=1, SupportSoftwareDeviceBitmaps=1
  (bit 28, despite "reserved" docs), MaxTextureWidth/HeightShift=3`; `RenderKm` is an ICF-folded
  no-op stub; one segment: **Aperture|CpuVisible|CacheCoherent** (flags 0x15, base 0xC0000000);
  `GetStandardAllocationDriverData` fully implements GDISURFACE incl. cross-adapter types 7/8
  (128 B pitch / 4-row align) and EXISTINGSYSMEM. The **ROS sample** ships the identical caps with
  the comment "MaxTextureShift caps size redirection device bitmap". **viogpu3d** (max8rr8 fork):
  no RenderKm/RenderGdi, GDISURFACE→STATUS_NOT_SUPPORTED, GDI content = CPU into coherent
  shadow/staging + `TRANSFER_TO_HOST` + host `RESOURCE_COPY_REGION` at Present. **VBoxWddm**:
  cap off, GDISURFACE case literally `//# error port to Win7 DDI` commented out; present = kernel
  `memcpy` in `vboxVdmaGgDmaBltPerform` + VBVA dirty rects. **qxl-wddm-dod**: display-only, N/A.
  The only RenderGdi executor that ever existed = closed, deprecated **RemoteFX rdvgkm.sys**
  (`DmaEngine::CmdBitBlt(_VGPUBITBLTGDI)` — lowered ops into its paravirt DMA stream host-side).
- ⇒ **The proper mechanism = BasicRender's shape**: cap OFF, `SupportSoftwareDeviceBitmaps=1`,
  CPU-visible cache-coherent placement for GDI surfaces, GDI rasterizes everything itself; the
  driver only moves/synchronizes pixels. The KMD CPU rasterizer has **no precedent** in any
  shipping driver.

### 6e. The remaining architecture gap (why neither path is done): CPU↔blob coherence

Both paths converge on one requirement: **VidMm's CPU view of a Helios allocation must alias the
venus blob bytes.** Today it does not: allocations live in the aperture segment, so GDI/CDD CPU
writes (`STAGING_CPUVISIBLE`, and with cap-off ALL GDI rendering) land in VidMm's system-RAM
aperture backing, while the host GPU + the gdi_blit executor read the venus HOST3D blob — two
different backings. Consequences today: executor BITBLTs that *source* CPU-written staging copy
zeros (suspected cause of missing wallpaper/text; **unconfirmed** — needs the 6f discriminator);
with cap-off, GDI's CPU-rendered window content would be invisible to the host the same way.
Candidate designs (in rough preference order):
1. **Blob-backed CPU visibility**: place allocations in the CpuVisible BAR **memory segment**
   (id 2) so VidMm's CPU mappings hit BAR bytes, and `RESOURCE_MAP_BLOB` each allocation's blob at
   its VidMm-assigned segment offset (we choose the window offset at map time — make it equal).
   This is the §4/"fake-but-coherent" completion and makes the BasicRender recipe fully correct.
   Needs: segment-set change in `create_one`, BuildPagingBuffer transfer support, map-at-offset
   bookkeeping. Old "VidMm rejects CpuVisible memory segment" lore predates the GpuMmu model —
   re-verify.
2. **Hybrid (keep cap ON + executor)**: executor reads CPU-written staging via the allocation's
   aperture backing pages (PFNs are visible in the UpdatePageTable/paging stream) and writes
   results into the destination's blob. Contained, but keeps the no-precedent KMD rasterizer and
   needs per-surface-type source selection.
3. **Guest-RAM-backed blobs** (`BLOB_MEM_GUEST` + host udmabuf import) for GDI surfaces so the
   aperture RAM *is* the venus memory. Host support exists (render server + udmabuf); guest KMD
   path is new.

### 6f-PRE. LATE-SESSION BREAKTHROUGH (2026-07-03, after the owner's clean reboot): the black
### COMPOSITION root cause — substitute textures + venus-poisoning imports

Chain, each link evidence-backed (dwm UMD log + host `/tmp/helios-qemu-stderr.log`):

1. **DWM composed into textures that aren't the real surfaces.** Its present-time sample reads
   `nonzero=0/357` while 536 DrawIndexed calls "succeed" and the full UMD draw self-test battery
   passes (`helios_umd_selftest` rc=0 — triangle renders; run it via P/Invoke on the DriverStore
   DLL). Cause: `open_resource` got `renderer_res=0` for KMD-created standard allocations
   (indirect-swapchain backbuffers 1896x1030, GDI redirection texture 1896x48) → fell back to a
   non-aliased "metadata texture" substitute.
2. **Why `_pad`/resid was 0 at open**: dxgkrnl snapshots the UMD-visible copy of standard-alloc
   private data BEFORE the KMD's create-time resid write-back; only the KMD-visible copy gets it.
   TWO fixes deployed: (a) UMD `open_resource` now prefers whichever private-data buffer carries a
   nonzero resid (`upgrade` closure, forward.rs); (b) **KMD .38**: `patch_alloc_resid` at
   `DxgkDdiOpenAllocation` writes the resid into the open-time buffers — and dxgkrnl DOES
   propagate that to the UMD in the same OpenResource call. **VERIFIED: `ddi-shared ok:
   1896x1030` / `1896x48`** on the post-fix boot.
3. **NEW WALL (the current blocker): importing a KMD standard-allocation blob into Vulkan
   fatally poisons the venus ring.** KMD standard allocations are raw `blob_id=0` HOST3D shmem
   blobs — no host `VkDeviceMemory` behind them. When DWM's ICD imports one by resource id
   (`VkImportMemoryResourceInfoMESA`) and DXVK records `vkBindImageMemory2`, the host fails:
   `vkr: failed to look up object N of type 8` → `vkBindImageMemory2 CS error` → **`fatal decoder
   state` → `destroying context (dwm.exe)`** — dwm's whole venus context dies. Downstream
   symptoms: the CreateTexture2D retry/hang loops, `VK_ERROR_OUT_OF_HOST_MEMORY` on other opens,
   a **dwm crash at `dxvk::DxvkImage::assignStorageWithUsage+0x284`** (null `m_storage` deref on
   the failure path — helios_umd.dll fault offset 0x1169b4, symbolized via cdb `-z` + the deps
   PDB), and repeated DWM restarts. The `ddi-shared ok` logs print before the async ring poison
   lands.

### 6f-2. 2026-07-03 late-night implementation round (all deployed; tree uncommitted)

- **KMD .39 (Code 0)**: standard allocations now backed by REAL venus `VkDeviceMemory` via the
  kernel venus client — `create_one` STANDARD branch → new `with_virtio_and_venus_locked`
  (spinlocked, DISPATCH-safe) → `allocate_memory_blob(v, size, mappable=true)`;
  destroy → detach/unref → new `VenusClient::free_memory_blob` (vkFreeMemory, cmd 22).
  Ring diag: 17 creates, 0 failures. The bind-time type-8 poison for KMD blobs is GONE.
- **ICD (deployed via install-helios-icd.ps1)**: `vn_device_memory_import_resource_id` now calls
  `vn_call_vkAllocateMemory` (SYNC) — host-side import failure surfaces as a clean VkResult
  instead of the guest binding a phantom object (the poison's enabler was async
  `vn_submit_vkAllocateMemory`). Mesa submodule uncommitted.
- **DXVK (dxvk-helios, rebuilt + relinked into the UMD)**: three guards — (1)
  `DxvkImage` ctor THROWS when `allocateStorage()` returns null (root fix:
  `createImageResource` RETURNS NULL on memory-alloc failure — a storage-less image AV'd at
  `assignStorageWithUsage+0x284`, `ExportImageInfo+0x338`, `initImage+0x17c` in turn);
  (2) `ExportImageInfo` early-out on null storage; (3) `assignStorageWithUsage` refuses null.
  Result: failed shared-texture imports now log `DxvkImage: failed to allocate backing
  storage` + fall back to a metadata texture; DWM SURVIVES (no more crash-loop; first
  nonzero Present samples ever observed: `first/center=0xffffffff nonzero=50/192`).
- **DEPLOY GOTCHAS (cost an hour)**: DWM loads the UMD from the **DriverStore of the newest
  KMD install** (`5bc705c6c57b9f62`), NOT the ProgramData hotplug override — deploy the UMD to
  BOTH (takeown/icacls + rename-aside into the store). dxvk rebuild: sources at
  `C:\Users\Rupansh\dxvk-helios` (LOCAL clone — copy edited files from Z:\dxvk-helios; .bak-fable
  backups left), build `meson compile -C C:\Users\Rupansh\dxvk-build` with
  `C:\Program Files\LLVM\bin` AND MSVC `...\Hostx64\x64` (lib.exe) on PATH, then purge UMD
  fingerprint + rebuild + redeploy. Crash→symbol recipe unchanged (cdb -z + deps PDB).
- **REMAINING (the actual frontier)**: (a) WHY do some export/import memory allocs fail
  host-side — `virgl_cmd_resource_create_blob: Operation not permitted` (ctrl 0x10c EPERM) for
  some ICD export blobs + import-alloc failures (e.g. the 1896x48 resid-1020 open) — suspect
  vkr export-handle requirements on the NVIDIA render server (OPAQUE_FD vs dma_buf, per-worker
  processes); (b) DXVK rejects DWM's plain `misc=0x2` (MISC_SHARED) CreateTexture2D with
  0x80070057 (the UMD retries 0x802 — find why plain SHARED fails); (c) one leftover phantom
  vkFreeMemory poison signature pre-guards — re-verify gone; (d) boot-time self-convergence +
  the 6e coherence design unchanged.

### 6f. NEXT (concrete, updated 2026-07-03)

0. **Back KMD standard allocations with real venus VkDeviceMemory** (the actual fix): in
   `create_one`, for `HELIOS_WDDM_ALLOC_KIND_STANDARD` with no UMD-supplied resid, allocate via
   the kernel venus client's `allocate_memory_blob(gpu, size, mappable=true)`
   (virtio/venus.rs — real `CMD_ALLOCATE_MEMORY` + blob with `blob_id = memory_id`) instead of
   raw `resource_create_blob(blob_id=0)`. Then cross-context import→bind works host-side (the
   UMD↔DWM path proves it). Mind: venus-client access/locking from CreateAllocation, freeing the
   venus memory object at destroy (CMD_FREE_MEMORY), memory_type_index must be HOST_VISIBLE
   (executor + IDD readback need mappable), and keep the tracking-table symmetry from 6b.
   Hardening pair: ICD must fail vkAllocateMemory cleanly when the import fails (not proceed to
   bind a bogus object id → ring poison — Mesa fix), and DXVK's import-failure path must not
   null-deref (`assignStorageWithUsage`).

1. After #0: clean boot → settle → one IDD kick → check `AssignSwapChain`/frames (this boot never
   got AssignSwapChain — likely collateral of the dwm venus-context deaths; re-evaluate once
   stable), then DWM `Present sample` nonzero, then LG client content. GDI-app discriminator
   (notepad/cmd via the client) still pending for the 6e staging-gap question.
2. **Re-run the cap-off experiment cleanly** (unblocked): `SupportKernelModeCommandBuffer=0`
   + `SupportSoftwareDeviceBitmaps=1` + `NoScreenToScreenBlt/NoOverlapScreenBlt=1` +
   `MaxTextureWidth/HeightShift=3` (BasicRender values). Watch: loads? DWM composes? GDISURFACE
   create mix changes? GdiE stays 0? Content? This decides Path B feasibility on 24H2 for real.
   Restore = `devcon update` the previous store INF.
3. Then commit to 6e design (likely #1) and spec it against `WDDM_FAKE_VIDMM_RESEARCH.md` §C.
4. Ops (this session's additions): without KD attached no freeze-pumping — installs/restarts are
   clean. Per-boot IDD kick: settle ~60 s, one `pnputil /restart-device`. The KMD install script's
   "requires reboot to activate" throw is spuriously conservative — the devcon rebind DOES
   activate (verify `DEVPKEY_Device_DriverVersion`). The IDD log resets per WUDFHost start.
   KMD-crash forensics recipe: WER event → fault offset → `cdb -z <deps dll> -y <deps dir>
   -c "ln helios_umd+0x<off>; q"`. Host venus errors: `/tmp/helios-qemu-stderr.log`. Run the UMD
   self-test battery headless: P/Invoke `helios_umd_selftest` from the DriverStore DLL (rc=0 =
   clear+triangle+CB+CB-triangle all pass). DWM reloads the UMD only on process restart
   (`taskkill /F /IM dwm.exe`, then re-kick the IDD).

### 6g. 2026-07-03 session (Fable 5, late): phantom-vkFreeMemory poison ROOT-CAUSED + FIXED
### (sync export alloc); first end-to-end 1896x1030 frames to the LG client; guest left degraded

**Headline results (chronological, all evidence-backed):**

1. **The ~4-min dwm crash-loop was still live** at session start (dwm 0xc0000409 BEX64/abort in
   ucrtbase at 03:05/03:10/03:14/03:18/03:22/03:26...). Host log signature per death, always the
   SAME object id within a boot: `virgl_cmd_resource_create_blob: Operation not permitted` →
   `vkr: failed to look up object 197 of type 8` (vkFreeMemory) → `fatal decoder state` →
   `destroying context (dwm.exe)`. §6f-2's item (c) — the "leftover phantom vkFreeMemory
   signature" — was NOT leftover noise; it was the active killer.

2. **ROOT CAUSE (one hole, three symptoms):** `vn_device_memory_alloc_export()` used the ASYNC
   `vn_device_memory_alloc_simple()` → host-side `vkAllocateMemory` failure is invisible; the
   guest then (a) creates the export blob → vkr's
   `vkr_context_create_resource_from_device_memory` can't find the memory object → returns false
   **silently** → QEMU prints the generic proxy **EPERM** (that's ALL the EPERM was!);
   (b) `bo_init` fails → cleanup `vn_device_memory_free_simple()` sends vkFreeMemory for an
   object the host never created → **ring-fatal** → context death → every subsequent DXVK call
   fails → dwm aborts. (The vkBindImageMemory2 type-8 poison from §6f-PRE was the same hole via
   bind-before-free.)

3. **FIX (deployed, uncommitted):** `icd/mesa src/virtio/vulkan/vn_device_memory.c` —
   `vn_device_memory_alloc_export` now does a SYNC `vn_call_vkAllocateMemory` (mirrors the
   import fix; matches upstream's `VN_PERF=no_async_mem_alloc` semantics; `bo_ring_seqno_valid`
   stays false so bo_init correctly skips the wait batch). Built via `win_meson` (default
   compile), deployed `install-helios-icd.ps1` (hash 252e89a9). **RESULT: zero venus context
   deaths from then on** (host log clean), dwm survived indefinitely, and — without any other
   change — **the OS started offering AssignSwapChain again**.

4. **FIRST FULL-RES END-TO-END FRAMES:** LG client log: `main_frameThread | Format:
   FRAME_TYPE_BGRA 1896x1030 (1896x1030) stride:1920 pitch:7680` — the whole
   DWM-composes-on-Helios → IddCx swapchain → D3D11-staging readback → KVMFR → client pipeline
   delivered desktop-resolution frames (content verification pending — session ended before a
   visual check).

5. **The vk_export_alloc_probe result matrix** (tools/vk_export_alloc_probe.cpp, build recipe in
   header; runs the exact DWM alloc shapes through the loader→ICD→host): on NVIDIA,
   **every** variant SUCCEEDS — 1896x1030 and 1896x48, external+plain image,
   export±dedicated on both DEVICE_LOCAL types. So the host was never rejecting the alloc
   *shape*; the async-invisible failures were something contextual in dwm's sequence (moot now,
   but note if EPERMs reappear). Two capability quirks found:
   `vkGetPhysicalDeviceImageFormatProperties2(external OPAQUE_FD, B8G8R8A8 optimal)` returns
   **VK_ERROR_FORMAT_NOT_SUPPORTED** (features=0x0) even though export allocs work (host-driver
   inconsistency; dxvk-helios's KMT mode skips that check, stock canShareImage would refuse);
   and the ICD does **not** expose `VK_KHR_external_memory_fd` (probe device create with it →
   -7 EXTENSION_NOT_PRESENT).

6. **DXVK Logger was writing to nowhere for dwm** (CWD=System32, unwritable) — every
   `Logger::err` from the create paths was lost. Fixed in `dxvk-helios
   src/util/log/log.cpp getFileName()`: default `DXVK_LOG_PATH` to `C:/ProgramData/Helios` →
   dwm now writes `C:\ProgramData\Helios\dwm_helios_umd_dxvk.log` (confirmed live: DXVK banner,
   device create, venus heaps, `err: Got allocation from memory type 4 without global buffer`,
   `warn: Helios KMT shared resource path: proceeding without VK_KHR_external_memory_win32`).

7. **The misc=0x2/0x802 CreateTexture2D 0x80070057 storm** (§6f-2 item b): with the poison fixed
   and the fresh adapter, the new dwm ran **0 create failures** — the storm was a
   dead-venus-context symptom, not a standing DXVK bug. Note the failures never produced a
   single DXVK Logger line, so if a storm recurs on a healthy context, look at the SILENT
   E_INVALIDARG returns in `NormalizeTextureProperties` first (the ctor-throw path Logger::err's
   and returns the same 0x80070057).

**Deploy/ops learned this session (additions to §6f-2):**

- **The UMD the OS loads is the hash-named `C:\ProgramData\HeliosUmd\helios_umd_<hash16>.dll`**
  selected by registry — deploy via `tools/hotplug-helios-umd.ps1 -Mode ProgramData
  -KillUmdUsers -NoProbe`, and **dxgkrnl caches the UMD path per adapter-start**: without
  `-RestartDevice` (or a Helios devnode restart) new dwm instances keep loading the OLD dll.
  Plain-named DriverStore/ProgramData copies are inert.
- UMD rebuild after a dxvk change: copy edited files Z:\dxvk-helios → C:\Users\Rupansh\dxvk-helios,
  `meson compile -C C:\Users\Rupansh\dxvk-build` (LLVM bin + MSVC Hostx64\x64 on PATH), purge
  `C:\Users\Rupansh\helios-vgpu\umd\target\release\.fingerprint\helios_umd-*`, `win_cargo umd
  build --release` (target dir is INSIDE the mirror: `...\helios-vgpu\umd\target\release`).
- D3D11CreateDevice(Helios) from SSH/console sessions fails 0x887a0004 without ever loading the
  UMD (session/adapter visibility) — even via schtasks /IT. The D3DKMT/Vulkan path (vk_probe)
  works from anywhere; use it for host-facing repros.
- dwm crash dumps: LocalDumps `HKLM\...\Windows Error Reporting\LocalDumps\dwm.exe` →
  C:\HeliosDumps (**left enabled with DumpType=2 FULL 215MB dumps — reduce to 1/miniDump or
  delete the key when dwm is stable**, dumps of the crash-era are in C:\HeliosDumps).
- ntoseye works WITHOUT KD via the **memory backend**: `ntoseye -b memory mcp --http
  127.0.0.1:8080` (passive /dev/kvm introspection; run in background, attach takes ~1 min). The
  session MCP client may 404 — use direct Streamable-HTTP JSON-RPC (helper:
  scratchpad ntcall.py pattern: initialize → notifications/initialized → tools/call, parse SSE
  `data:` lines, match reply by id). QMP monitor at `/tmp/helios-tpm/mon.sock` (JSON):
  query-status, screendump, human-monitor-command (`x/16i <addr>`, `info registers -a`),
  system_reset, device_del/device_add.
- `ps -o %cpu` is LIFETIME average — do not read it as "the guest is spinning". All-vCPUs at the
  same RIP can simply be the idle `sti; hlt; ret` loop (disassemble via QMP before concluding
  bugcheck/wedge; ntoseye `bugcheck` returns null on a healthy guest).

**Where the session ENDED (the bad part — READ BEFORE TOUCHING THE VM):**

- Mid-session, a `taskkill dwm` + `pnputil /restart-device <Helios PCI>` race **hard-wedged the
  guest** (real wedge: network died instantly, virtio traffic stopped). Recovered via QMP
  `system_reset` — but **all three post-reset warm boots came up WITHOUT NETWORK** (no DHCP
  request ever arrives host-side; dnsmasq/bridge/tap verified fine; NIC hot-replug via QMP
  device_del/device_add net0 did NOT recover it). ntoseye memory-backend inspection of the last
  such boot: guest fully booted and healthy (Explorer/dwm/WUDFHost/sshd/LogonUI all running, all
  vCPUs idle, NO bugcheck) — only the NIC (and the IDD session, which died ~4 s after its LGMP
  session appeared each boot, WerFault present) are down. Suspected: warm-reset leaves a
  device (virtio-net vhost? PnP tree stuck on the IDD/Helios devnodes?) in a state Windows can't
  re-init; a **full QEMU-side power cycle (launcher restart) is probably needed** — that's a
  user action per the VM-ownership rule.
- The deployed SOFTWARE stack is believed GOOD (the pre-wedge live run proved it): KMD .39
  untouched, ICD 252e89a9 (sync export alloc), UMD 117f199f (hotplug-active) + dxvk with the
  Logger fix. Uncommitted source edits this session: `icd/mesa/src/virtio/vulkan/vn_device_memory.c`,
  `dxvk-helios/src/util/log/log.cpp` (mirrored to C:\Users\Rupansh\dxvk-helios),
  `umd/src/forward.rs` (log fields only), new `tools/vk_export_alloc_probe.cpp`,
  `tools/d3d11_dwm_shared_repro.cpp`.

**NEXT (in order):**

1. User relaunches the VM (full power cycle). Then on a clean cold boot, WITHOUT touching
   anything: watch `AssignSwapChain` in the IDD log + LG client for frames. The poison fix may
   have solved boot-time convergence by itself (the OS's swapchain re-offer was plausibly gated
   on dwm stability all along).
2. If frames flow: verify CONTENT visually (taskbar was proven earlier; now full desktop), then
   the GDI-app discriminator (§6e), then re-check `dwm_helios_umd_dxvk.log` +
   `/tmp/helios-qemu-stderr.log` for the EPERM/import failures (the 1896x48 import-by-resid
   fallback still fires — its root cause is NOT fixed, just non-fatal; likely OPAQUE_FD
   allocationSize-must-match or resource-not-attached-to-context — see
   `vkr_get_fd_info_from_resource_info`, which FATALS the context on unknown res_id, vs the
   clean paths).
3. Reduce/remove the dwm LocalDumps key (C:\HeliosDumps fills at 215MB/crash).
4. If boot convergence still needs help: LGIdd self-replug on no-AssignSwapChain-within-N-sec
   (the OnSwapChainDeviceLost replug path already exists and fired correctly this session when
   the swapchain died; extend it to the never-offered case) and/or gate SelectRenderAdapter to
   once-per-adapter.

### 6h. SUPERSEDED FOR DIRECTION — read `HELIOS_FIRST_PRINCIPLES_AUDIT.md`

After §6g the overseer directed a stop to all incremental workarounds. The full
first-principles audit (seven broken contracts, complete hack inventory across
KMD/UMD/ICD/DXVK/LGIdd with file:line, priority plan, acceptance criteria, and the next
session's handoff prompt) lives in `HELIOS_FIRST_PRINCIPLES_AUDIT.md` at the repo root.
§§6a–6g above remain the factual session record; do not mine them for next steps.
