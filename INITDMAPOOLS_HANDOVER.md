# INITDMAPOOLS_HANDOVER.md — next-session brief for the Helios WDDM Code-43 fix

**Date:** 2026-06-19. **Status:** root cause PINNED via live KD; exact fix is one
`DXGK_SEGMENTFLAGS` change pending one reads-only decode. Read this first, then
`step2-gpummu-implemented` memory, `BRINGUP_QUIRKS.md`, `NTOSEYE.md`,
`WDDM_FAKE_VIDMM_RESEARCH.md`, and `wddm-hwaccel-desktop-is-the-goal` memory.

---

## ★ UPDATE 2026-06-22 — 0x10E:0x49 / CDD DMA-pool crash FIXED → Code 0

The later `0x10E:0x49` investigation found a second DMA-pool contract issue, not a
page-table backing issue. The aperture-first segment shape correctly gets past
`InitDmaPools`, but dxgmms2 then creates the CDD/system context and asks VidMm for a
privileged DMA pool. If `DXGKARG_CREATECONTEXT.ContextInfo.DmaBufferSegmentSet` is left
zero, VidMm uses contiguous nonpaged system memory for that pool, skips the normal VidMm
allocation object, then later dereferences the null allocation during DMA-pool/page-table
initialization.

**Fix:** `DxgkDdiCreateContext` now sets `DmaBufferSegmentSet = 1`, matching the aperture
segment used for paging buffers. The DMA buffer pool is then backed through the normal
aperture allocation path.

Validated live with gpu-gl attached:
- Helios loads and reaches **Code 0**: `Status=OK`, `Problem=CM_PROB_NONE`, class
  `Display`, driver `22.22.32.72`.
- No `0x10E` bugcheck after bring-up.
- The StartDevice debugger gate used for the decode has been removed for normal boots.

## ★ UPDATE 2026-06-21 — InitDmaPools DECODED + FIX VALIDATED → now PAST it (new gate 0x10E)

The reads-only decode below was completed and the fix landed. Summary (full detail in
the `step2-gpummu-implemented` memory):

- **The validation is deeper than "one `DXGK_SEGMENTFLAGS` bit".** Live decode of
  `VIDMM_DMA_POOL::Initialize` (RVA 0xb9100): the simple mask check
  `(pool_attr & ~[segdesc+0x3c])` PASSES; the REAL gate is a per-bit loop — for each
  set bit `b` in `[pool+0x20]` it dereferences `attr=[[segdesc+0x670]+b*8]` and requires
  `[attr+0x68] & 1` ("this segment can host a paging buffer"). Our CPU-visible **BAR
  memory** segment's attribute objects never had that bit; an **aperture** segment does.
- `VIDMM_GLOBAL +0x34/+0x74/+0x174` decode to our `PagingBufferSegmentId / PagingBufferSize
  / PagingBufferPrivateDataSize` verbatim. The pool validates the **FIRST reported
  segment** (`segdesc[0]`), regardless of `PagingBufferSegmentId`.
- VidMm **drops a system-RAM-backed memory segment** (segdesc array had only the BAR;
  index 1 = NULL) — so the prior "RAM paging segment id 2" never registered.
- **FIX (shipped, uncommitted):** report an **aperture** segment FIRST (id 1, viogpu3d
  shape: `Aperture+CacheCoherent+DirectFlip`, `CpuVisible=0`, base `0xC0000000`, 1 GiB) =
  `PagingBufferSegmentId=1`; the BAR CpuVisible **memory** segment SECOND (id 2) = page
  tables (`MEMORY_SEGMENT_ID`, `PageTableUpdateMode=GPU_PHYSICAL`) + render targets.
  `kmd_render/src/ddi/{query_adapter_info.rs::query_segments, gpummu.rs}`.
- **RESULT = PROGRESS, NEW GATE.** PnP replay → **bugcheck `0x0000010E`
  VIDEO_MEMORY_MANAGEMENT_INTERNAL, arg1=`0x49`** (VidMm unrecoverable internal). A clean
  status-return Code-43 reject became a deep VidMm CRASH ⇒ we are **past InitDmaPools**,
  failing LATER: the null engine can't actually back the aperture paging buffer
  (`BuildPagingBuffer` MAP_APERTURE_SEGMENT with real MDLs, viogpu3d's `viogpu_command.cpp`
  model) and/or the BAR page-table segment is unbacked when VidMm touches it.
- **NEXT = the `0x10E:0x49` gate.** Implement real `BuildPagingBuffer` aperture backing +
  page-table backing (or decode the 0x10E:0x49 site reads-only for the exact paging op
  VidMm couldn't complete). The deployed `.sys` is the **crashing** build → re-adding
  gpu-gl boot-loops (Helios brings up at boot → DWM opens it → 0x10E). Recover with the
  gpu-gl-OUT boot (BRINGUP_QUIRKS §5). `.sys.old` is the prior Code-43 build but its
  matching `.cat` was overwritten — rebuild from reverted source for a clean safe baseline.
- **Mechanics gotchas found:** `inf2cat.exe` is in `…\10\bin\10.0.26100.0\**x86**\`, NOT
  `x64` (BRINGUP_QUIRKS §2's x64 path is wrong — `signtool` IS x64). `win_exec` swallows
  `Select-Object`/table output — emit explicit strings.

## ★ UPDATE 2026-06-21 (later) — Option A implemented (CPU_VIRTUAL + RAM page-table seg), safe build deployed

Decided fix for the `0x10E:0x49` gate = **Option A**: `PageTableUpdateMode = CPU_VIRTUAL`
(no execution engine in the page-table path — the null engine is exactly why
GPU_PHYSICAL bugchecked) + a **real, CPU-writable, registered** page-table segment.

Two ways to get a real, CPU-writable, VidMm-accepted page-table backing:
- **System RAM (`AdapterContext::paging_ram`, `MmAllocateContiguousMemory`, 16 MiB)** — the
  KMD already allocates this. The earlier live decode saw VidMm DROP a system-RAM segment,
  **but** that was behind a competing CpuVisible **BAR** memory segment; the new aperture-
  first shape makes the RAM segment the **only CpuVisible memory segment** (segment 0 is the
  aperture, which holds no bits), which may change VidMm's acceptance. Cheap to test (built).
- **Host-visible venus memory (BAR-backed), self-allocated by the KMD.** Correction to an
  earlier note: the KMD *can* issue `vkAllocateMemory` itself — it is NOT limited to a UMD-
  provided memory id (the `gpu.rs:437` ".56 ERR_UNSPEC" only means a `blob_id=0` HOST3D blob
  with NO venus memory behind it fails). But the KMD currently has only the virtio-gpu
  *control* path + opaque `submit_venus`; driving `vkAllocateMemory` needs a **minimal venus
  client in the KMD**: a `blob_id=0` HOST3D ring shmem, a hand-encoded venus command sequence
  (`vkCreateInstance`→`vkEnumeratePhysicalDevices`→`vkGetPhysicalDeviceMemoryProperties`→
  `vkCreateDevice`→`vkAllocateMemory`), and **ring reply parsing** (to read back the memory
  id — `submit_venus` only returns a virtio fence ack, not venus return values). Then
  `RESOURCE_CREATE_BLOB(blob_id=mem_id, HOST3D)` + `RESOURCE_MAP_BLOB` → a BAR-backed,
  CPU-coherent region VidMm accepts as device memory. This is the robust fix if RAM is
  rejected; it is a meaningful chunk of careful, byte-exact venus work.

**Implemented (uncommitted), behind the `REPORT_APERTURE_PAGING_SEGMENT` toggle in
`query_adapter_info.rs`:**
- `false` = **SAFE** single-BAR shape → clean Code-43 reject (no crash). **This is what is
  currently DEPLOYED**, so the VM boots normally once gpu-gl is re-attached.
- `true` = **Option A** shape: segment 0 = aperture (PagingBufferSegmentId, passes
  InitDmaPools); segment 1 = real-RAM CpuVisible memory (`paging_ram`) = `MEMORY_SEGMENT_ID`
  = page tables, written CPU-direct via `CPU_VIRTUAL`.
- `gpummu.rs`: `PageTableUpdateMode = CPU_VIRTUAL`.

**Next session (user re-attaches gpu-gl + KD, then):** pre-arm ntoseye → flip the toggle to
`true`, build (purge first), deploy, replay. **First check: does VidMm REGISTER the RAM
page-table segment?** Read the segment array `[[VIDMM_GLOBAL+0x9d28]+1*8]` at the
InitDmaPools bp — if non-NULL, VidMm accepted the RAM segment (the aperture-first shape
worked); then see if `CPU_VIRTUAL` page-table init reaches Code 0 (or read `0x0F01..0x0F05`
atomics). If the RAM segment is STILL dropped, system-RAM is categorically rejected as a
memory segment → the page-table backing must come from venus host-visible memory the UMD
provides (defer page-table-segment registration until a UMD maps venus memory), or switch to
the viogpu3d aperture + real `MAP_APERTURE_SEGMENT` MDL-backed model (Option B).

## ★ UPDATE 2026-06-21 (later-2) — KMD venus client WORKS; vkAllocateMemory from the kernel

Built a minimal in-kernel venus client (`kmd_render/src/virtio/venus.rs`; full protocol in
`VENUS_KMD_ALLOC_SPEC.md`) so the KMD self-allocates a HOST_VISIBLE `VkDeviceMemory` over
venus and maps it as a BAR-backed page-table window (VidMm drops system-RAM segments; it
accepts device-BAR memory). **It works end-to-end** — diag `0x0D00_0006..000C` = CreateInstance,
EnumeratePhysicalDevices, GetPhysicalDeviceMemoryProperties, CreateDevice, vkAllocateMemory,
blob-create, blob-map all succeed; `0x0B00_0007` = venus OK; `page_table_window` populated.

Fixes that got it working (all from host `virgl_render_server` logs the user fed back — gold):
- **NotifyRing must be UNCONDITIONAL** after each ring publish (gating on the IDLE status bit
  is racy; the host idles after ~1ms) — this made the host start consuming the ring.
- **`vkWaitVirtqueueSeqnoMESA` must be ring-dispatched, not direct** ("must be called on ring
  dispatch") — removed the direct Submit/Wait reply-warm-up (host maps the reply shmem when it
  processes the ring's `vkSetReplyCommandStreamMESA`).
- **ALL venus handles are GUEST-assigned, incl. `VkPhysicalDevice`** — pass a non-zero
  `alloc_handle()` id in the enumerate array slot, not a 0 placeholder ("invalid object id 0").
- `RING_POLL_SPINS` = **100M** (2M was too short → premature E4 before the host answered).
- **Code 39 = corrupt `.cat`** root cause: QMP `system_reset` issued before the copied catalog
  flushed to disk → torn cat on reboot. FIX: `Write-VolumeCache C` (+1s) after the DriverStore
  copy, before reset; re-verify the LIVE cat (`signtool verify /pa /c`), not just the package.
- venus alloc gated behind `VENUS_ALLOC_ENABLED` (start_device.rs) — on for these runs.

**THE Option-A test (next): flip `REPORT_APERTURE_PAGING_SEGMENT = true`** so `query_segments`
reports aperture(id1) + the **venus-backed** memory segment(id2, `CpuTranslatedAddress =
page_table_window.gpa`) as the page-table segment. Question: does VidMm REGISTER it and clear
the `0x10E:0x49` PTE-path bugcheck? Success → Code 0 (then DWM-compositing crash-loop is the
next, separate problem); failure → new bugcheck/boot-loop (recover via gpu-gl-OUT). Run with
ntoseye attached.

The original 2026-06-19 brief (now historical) follows.

---

## The goal (unchanged, locked)
A **fake-but-coherent WDDM GpuMmu render adapter** (`kmd_render`) backed by venus, so
VidMm accepts Helios at **device Code 0** and DWM composites the whole desktop on it →
venus → host GPU, captured into Looking Glass via the IDD. The host owns the real MMU;
guest page tables are decorative. This is NOT per-app venus (the old System-class driver
already did that). Do not pivot away from compositable-WDDM.

## THE ROOT CAUSE (pinned by KD this session)
Helios loads at **Code 43**. Live stack-walk + step trace proved the teardown chain:
```
DXGPROCESS::OpenAdapter (+0x161)
  → DXGPROCESS_RENDER_ADAPTER_INFO::Initialize   (+0x115 call, status saved to rdi)
    → dxgmms2!VidMmInitializePagingProcess
      → dxgmms2!VIDMM_GLOBAL::InitDmaPools  ⟵ RETURNS STATUS_INVALID_PARAMETER (0xC000000D)
  → (rdi<0) DXGADAPTER::Destroy → ADAPTER_RENDER::Destroy → VidSchTerminateAdapter → Code 43
```
Confirmed by stepping that **everything up to and including our `DxgkDdiCreateContext`,
`VidSchCreateSystemDevices`, `VidSchiCreateContextInternal` SUCCEEDS (rax=0)**. The
failure is purely VidMm's **paging DMA-pool init**, which runs *before* any page-table
DDI (which is why GetRootPageTableSize/BuildPagingBuffer atomics are always 0, and why
the 5 memory-model hypotheses below never moved it).

### The exact validation (decoded statically)
Inside the per-DMA-pool init (`InitDmaPools` loops over `[VIDMM_GLOBAL+0x1b20]` pools;
per pool: alloc obj → construct (call at `InitDmaPools+0x9c`) → init (call at `+0xba`,
status checked `js`)). The init does:
```
r10d = [pool+0x20]                              ; pool's requested attribute/segment mask
r11  = [[parent+0x9d28] + [pool+0x18]*8]        ; the segment descriptor this pool binds to
eax  = [r11+0x3c]; not eax; test r10d, eax; jne FAIL   ; (r10d & ~allowedMask) != 0 → INVALID_PARAMETER
```
So **a DMA pool requests a segment-attribute bit that our segment's `[segdesc+0x3c]`
allowed-mask (derived from the `DXGK_SEGMENTFLAGS` WE report in `query_segments`) does
not permit.** `[pool+0x20]` and `[pool+0x18]` are built by the pool constructor
(`InitDmaPools+0x9c`) from per-index arrays at `VIDMM_GLOBAL+0x34` (byte→`1<<(al-1)`),
`+0x74` (dword), `+0x174` (dword), populated during adapter init from our reported
segments/caps.

## THE FIX (do this)
A targeted **`DXGK_SEGMENTFLAGS`** change on our declared segment(s) in
`kmd_render/src/ddi/query_adapter_info.rs::query_segments`. The exact flag is the last
unknown — get it with a **reads-only** decode (SAFE: memory reads only, no stepping →
no reboot; stepping in VidMm paging-init rebooted the guest once this session):
1. Re-resolve symbols at the live base (after any reboot, ntoseye symbols may not
   re-resolve; compute via RVA — old dxgmms2 base was `0xfffff80143600000`, so
   InitDmaPools RVA = `0x8f678`, its constructor-call target RVA ≈ `0x10484b`,
   VidSchTerminateAdapter RVA = `0xf95e0`. Verify by reading base+RVA and matching the
   known InitDmaPools prologue `48 89 5c 24 08 48 89 6c 24 10 48 89 74 24 18 57`).
2. Decode the pool **constructor** (`InitDmaPools+0x9c` target) to see which input
   ([this+rbx*4+0x74] or +0x174 or the `1<<(al-1)` size) becomes `[pool+0x20]`, and
   what `[segdesc+0x3c]` is in `DXGK_SEGMENTFLAGS` terms.
3. Find what populates `VIDMM_GLOBAL+0x34/+0x74/+0x174` from our config (these arrays
   are set when VidMm ingests our `QUERYSEGMENT4` descriptors / DMA caps).
4. Set the missing flag on our segment(s) (current flags: seg1 BAR = CpuVisible+
   CacheCoherent; seg2 RAM = CpuVisible+CacheCoherent). Build → deploy → disable→enable
   → read ring: success = page-table DDI atomics (`0x0F02/0x0F04`) become non-zero and
   ProblemCode → 0.

### DEAD ENDS — do NOT repeat (5 hypotheses ruled out)
1. Coherent async **submission engine** — SubmitCommand/Render/page-table DDIs never
   called before teardown (all 0). Engine is downstream of the blocker.
2. **Interrupt storm** — was real (INTx, MSISupported=0, ISR fired 10000×/cycle
   unclaimed); FIXED (real ISR reads-to-clears the virtio ISR-status register, count
   10000→0) but Code 43 unchanged. Storm was NOT the cause.
3. **RAM-backed paging segment** (added seg2 real RAM for page tables/paging buffer) —
   no effect on Code 43.
4. **PageTableUpdateMode** GPU_PHYSICAL→CPU_VIRTUAL — no effect.
5. **Aperture segment** (bare Aperture=1 seg3) — REGRESSED: VidMm rejects at
   QUERYSEGMENT (earlier than before), reverted. A malformed segment descriptor is
   rejected at CreateDevice stage. Whatever flag the fix needs, it must keep the
   descriptor valid to VidMm's strict segment validation.

## Code changes shipped this session (uncommitted, all in `kmd_render/src/`)
All CORRECT (and the storm fix is a genuine latent bug eliminated), even though none is
the Code-43 fix:
- `ddi/interrupt.rs` + `virtio/gpu.rs` (`map_isr_status_register`, ISR-status VA) +
  `adapter.rs` (`isr_status` atomic, `paging_ram`) + `ddi/start_device.rs`: **real ISR**
  (read-to-clear INTx) + clear-once at init. Storm gone.
- `adapter.rs` (`PagingRam`, 16 MiB MmAllocateContiguousMemory, freed in Drop) +
  `ddi/query_adapter_info.rs` (`query_segments` reports seg2 RAM) + `ddi/gpummu.rs`
  (`page_table_segment_id`, PageTableUpdateMode = **CPU_VIRTUAL**): real-RAM page-table
  segment 2, paging routed to it.
- `ddi/submit_command.rs`: completion-notify via `DxgkCbSynchronizeExecution` (correct
  DIRQL, was a contract violation) + DISPATCH-safe engine instrumentation atomics
  (diag `0x0F06..0x0F0E`) + real `Render` (copy) + `Patch` (no-op SUCCESS) + DestroyDevice
  dumps the atomics.
- `device.rs`: DestroyDevice dumps engine atomics.
- The aperture seg3 was added then REVERTED — source is back to the 2-segment shape.

Decide whether to commit these (storm/IRQL/instrumentation are keepers) before the fix.

## VM / debug mechanics (all hard-won — in BRINGUP_QUIRKS.md §1-6 + NTOSEYE.md)
- Build host IS the win11 VM. Drive via win MCP (`win_cargo`/`win_exec`). ALWAYS purge
  the fingerprint before building (§1). cargo-make signs a STALE .sys — repackage by hand (§2).
- **Deploy = in-place DriverStore overwrite** at `…\helios_kmd_render.inf_amd64_e0bd070459ad7ca4\`.
  **Catalog gotcha:** re-running inf2cat over a signed .cat corrupts it → 0xC000026C /
  Code 39 / empty ring. DELETE the .cat first, run inf2cat STANDALONE (not chained), sign,
  and VERIFY with `signtool verify /pa /c <cat> <sys>` (Get-AuthenticodeSignature does
  NOT check coverage). Confirm deployed .sys hash == package .sys hash.
- **★ Reboot-free bring-up replay (use this for KD loops — keeps ntoseye attached):**
  `Disable-PnpDevice` → poll until `DEVPKEY_Device_ProblemCode == 22` → clear ring →
  `Enable-PnpDevice` → poll until ring repopulates. Helios is render-only/Code-43 so
  disabling it does NOT hang DWM. `Enable-PnpDevice` BLOCKS until start/fail, so if a KD
  bp halts the guest mid-bring-up the win_exec enable times out + is killed (EXPECTED) —
  use a short timeout_secs then drive ntoseye.
- ntoseye: schema-broken `backtrace/disassemble/bugcheck/list_breakpoints` — walk the
  stack manually (read_memory [rsp] + closest_symbol on return addresses). `interrupt`
  parks a CPU at DbgBreakPointWithStatus (that's the KD break-in, not a hang — 15/16 CPUs
  idle = waiting). KD-attached boot breaks on DbgPrints (resume past). After reboot,
  symbols may not re-resolve → use base+RVA.
- Helios instance id `PCI\VEN_1AF4&DEV_1050&SUBSYS_11001AF4&REV_01\4&27FF4EC&0&0017`;
  device id `ua-heliosgpu`. Diag ring = registry `S0..Sn` REG_DWORD under the service key
  (codes in BRINGUP_QUIRKS.md §6). QMP `system_reset` socket `/tmp/helios-tpm/mon.sock`.

## Current VM state at handover
2-segment CPU_VIRTUAL + RAM-paging-segment + real-ISR build deployed, signed + `verify
/pa /c` PASS, **Code 43** (fails at InitDmaPools — the furthest-progress baseline).
ntoseye attached/coherent. Source uncommitted.
