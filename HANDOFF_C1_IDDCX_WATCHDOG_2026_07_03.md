# Handoff — C1 identity landed + the FAILED_POST_START killer root-caused and fixed (2026-07-03, third session)

Continues `HANDOFF_FIRST_VISIBLE_FRAMES_2026_07_03.md` (§§0a–0c) and
`HELIOS_FIRST_PRINCIPLES_AUDIT.md`. Everything here is committed: parent
`0758924` + this doc, submodules mesa `5db3cea4290`, dxvk-helios `1a68b10b`,
LookingGlass `4328e17d`.

## 1. C1 allocation identity — implemented, deployed, working end-to-end

**Root cause of the boot-#3 `invalid res_id 45` dwm kill (host-log corroborated,
deeper than the handoff's "no CTX_ATTACH_RESOURCE" reading):**

1. Adopted venus blobs kept their escape-owner `BlobSlot`, so
   `DxgkDdiDestroyDevice`'s `release_blobs_for_owner` sweep unref'd host
   resources that live shared WDDM allocations still referenced. The
   ownership "transfer" (`helios_venus_memory_transfer_resource_ownership`)
   only flipped an ICD-side flag — the KMD's per-owner reclaim table never
   learned.
2. QEMU/virglrenderer's `virgl_renderer_ctx_attach_resource` is **void** and
   silently no-ops on a missing resource (and the proxy silently drops
   attaches of non-dma-buf fd types — `proxy_context_attach_resource`,
   virglrenderer 1.3.0), so the guest's ATTACH escape "succeeded" while dwm's
   render-server worker never received the resource → `vkr: failed to import
   resource: invalid res_id` → CS error → fatal ring.
3. The nine `virgl_cmd_resource_unref: resource does not exist` (34…49) at
   dwm teardown were `destroy_allocation_ctx`'s **unguarded adopted-arm
   double-unref** of the already-swept resids.

**Fix (KMD 22.22.41.0, UMD b89f9918, protocol structs):**

- `adopt_blob_for_allocation`: adopting re-owns the BlobSlot to the
  allocation; DestroyDevice sweeps can no longer kill shared resources.
  Adopting a dead resid fails CreateAllocation loudly.
- One `take_live_resource`-guarded teardown path for created AND adopted
  resources (double-unref class closed).
- The KMD's live-resource table is the authoritative liveness oracle (the
  KMD owns the resid namespace — every create and unref goes through it;
  the host attach path cannot be trusted to fail, see #2 above). Liveness is
  validated at adopt, at `HELIOS_ESCAPE_ATTACH_RESOURCE`, and at
  `DxgkDdiOpenAllocation` — a dead-resource open FAILS
  (`STATUS_INVALID_PARAMETER`) instead of handing consumers a dead resid.
- **Versioned open-identity ABI**: `HeliosWddmOpenIdentity` ('HIDN', 48 B,
  protocol/src/wddm.rs) written into the open-time private data at
  OpenAllocation — replaces the `_pad` smuggling. The shared
  `HeliosWddmAllocMeta` trailer now carries the creator's exact
  `vkAllocateMemory` size + memoryTypeIndex (KMD kernel-client values
  written back at create for standard allocations; UMD values — via the new
  ICD export `helios_venus_memory_alloc_info` — for adopted DXVK textures).
- UMD `open_resource`: parses the identity (no heuristics), imports through
  a **typed** bridge path (`D3D11_HELIOS_IMPORT_INFO` →
  `DxvkSharedHandleInfo.heliosResourceId/AllocSize/MemoryTypeIndex` →
  allocator `importSizeOverride`/`importMemoryTypeIndex` — vkr's OPAQUE-fd
  import requires exact size; no more HANDLE punning), and **fails loudly
  via the corelayer `pfnSetErrorCb` (E_FAIL). The metadata-texture fallback
  is DELETED** (audit U-B2/C1.3).

**Verified after a full reboot:** host venus log clean the whole boot (zero
invalid-res_id, zero CS errors, zero does-not-exist unrefs); dwm stable
50+ min through heavy device churn; dwm log shows
`open_resource identity: res_id=… alloc_size=… mem_type=…` +
`ddi-shared ok` for cross-process opens — real imports, no fallbacks.

**Honest new signal (P2/C6 lead):** DXVK now warns
`import size override X < image requirement Y` and `import memory type 2 not
in image type mask 3` for opens of KMD-standard GDI surfaces — the creator's
surface is linear/host-visible, the opener creates an OPTIMAL-tiled image
with bigger requirements. This mismatch class (previously silent black) is
exactly the C6/linear-aliasing work: the opener must create a compatible
(linear, exact-size) image, or C6's RESOURCE_MAP_BLOB design supersedes it.

## 2. ICD failure honesty (I-A4/I-A6) — implemented

`vn_ring_wait_seqno` returns bool and bails on a FATAL/torn ring (checked
BEFORE `vn_ring_get_seqno_status`, whose torn-ring "already retired" lie must
not be mistaken for a written reply); `vn_ring_submit_command` drops the
never-written reply so the generated `vn_call_*` wrappers return a clean
error instead of decoding garbage. A host CS error can no longer stall a sync
call until mesa's watchdog `abort()`s the process (the boot-#3 dwm death
mode). I-A5 (fence-value verification in `helios_wait`) is deferred to the C3
real-fence work — the transport is synchronous per verb today.

## 3. THE FAILED_POST_START KILLER — root-caused with a dump, fixed, verified

**Getting the dump (the WUDF LogFiles route never fired):**
`HKLM\…\Image File Execution Options\WUDFHost.exe` `GlobalFlag=0x200` +
`HKLM\…\SilentProcessExit\WUDFHost.exe` `ReportingMode=2`,
`LocalDumpFolder=C:\HeliosDumps`, `DumpType=2` (and WER LocalDumps for
WUDFHost.exe as belt-and-braces; make sure C:\HeliosDumps grants LocalService
write). Full dumps landed on the next two kills.

**The stack (dump `WUDFHost.exe-(PID-1576)`, cdb + MS symbols):**

```
IddCx!IddWatchdog::WatchdogThread
→ IddCx!IddAdapter::ReportBugcheckWapper → ReportBugcheck
→ WUDFx02000!imp_WdfCxVerifierKeBugCheck        (= FxVerifierDriverReportedBugcheck 050100040000010f)
→ WUDFPlatform!CPlatform::InnerDriverStop → KERNELBASE!TerminateProcess
```

blocked victim thread:
```
LGIdd!LGIddMonitorUnassignSwapChain → ~CSwapChainProcessor
→ WaitForSingleObjectEx   (INFINITE join on the swapchain thread…)
```
which was stuck in:
```
LGIdd!CSwapChainProcessor::InitDevices → CD3D11Device::Init → d3d11!D3D11CreateDevice
```

**IddCx runs its own watchdog and TERMINATES WUDFHost when any LGIdd DDI
callback overruns.** Every kill of the taxonomy was this: boots #1/#2/#4 and
the warm deploy kills blocked inside AssignSwapChain (D3D11CreateDevice /
IddCxSwapChainSetDevice single calls observed blocking 6–22+ s during pairing
churn); after moving bring-up to the thread, the Unassign callback inherited
the block via the destructor's INFINITE join. The "+25 s deadline" was the
watchdog period, not a post-start property. A second, related crash class:
deleting a still-ASSIGNED swapchain object (thread-exit delete after a
bring-up failure) corrupts IddCx's lists —
`FAST_FAIL_CORRUPT_LIST_ENTRY` (0xc0000409/7) at `iddcx.dll+0xa631` (WER
AppCrash 08:42).

**Fix (LookingGlass `4328e17d`, driver 9.16.6.931) — "no blocking work in
IddCx DDI callbacks":** AssignSwapChain spawns the processor thread and
returns; all bring-up (D3D11/D3D12, pools, SetupLGMP, SetDevice) runs on the
thread; `CSwapChainProcessor` is shared_ptr-owned with each thread holding
its own reference so `Stop()` (the unassign path) is **signal-only** — no
callback ever joins; the destructor (join-free by construction, runs at
last-reference) deletes the swapchain object only once nothing references
it; replug defers while an assign/bind is in flight; the born-abandoned
first swapchain converges through thread-self-exit → queued replug instead
of ABANDON.

**Verified:** 11+ minutes of continuous replug/bind cycles including deploy
restarts with ZERO host kills (previously ~one kill per cycle). IDD Code 0.

## 4. What is NOT fixed (the current frontier, in order)

1. **§3 steady-state feed (stale binding)**: bindings now succeed
   (`IddCxSwapChainSetDevice` OK) but acquire **zero frames**; the
   stale-binding watchdog replugs every ~10 s. This is the pre-existing
   "which pairing/swapchain instance do dwm's presents feed" question — now
   crash-free and cheap to iterate (leads in
   `HANDOFF_FIRST_VISIBLE_FRAMES_2026_07_03.md` §3: poke test while
   watching, ntoseye into dxgkrnl's blt queue, UMD→IDD present/acquire
   correlation). NOTE: consider whether the 10 s stale-binding replug loop
   itself prevents convergence (boot #3 converged with the swapchain LEFT
   BOUND while dwm restarted; an aggressive replug may keep destroying
   bindings dwm was about to feed). Re-examine the timeout/policy with the
   crash class gone.
2. **P2/C6 content**: the DXVK size/type-mismatch warnings for GDI-surface
   opens (see §1) — linear-vs-optimal aliasing.
3. **P1 acceptance**: ten cold boots (needs the owner).
4. Housekeeping: dwm LocalDumps still FULL; WUDFHost SilentProcessExit/IFEO
   GlobalFlag + WER keys still armed (C:\HeliosDumps now has LocalService
   write ACLs); two 150 MB WUDFHost dumps + two 220 MB dwm dumps in
   C:\HeliosDumps to clean when done.

## 5. Ops learned this session

- `hotplug-helios-umd.ps1` defaults to `target\debug` — ALWAYS pass
  `-UmdDll C:\Users\Rupansh\helios-vgpu\umd\target\release\helios_umd.dll`.
- The KMD package (`install-helios-kmd.ps1`) carries its own UMD copy into
  the DriverStore; deploy order UMD-hotplug → KMD-install leaves the
  DriverStore copy stale — run the UMD hotplug (which also updates the
  DriverStore copy) AFTER the KMD install, with `-RestartDevice`.
- cdb symbolizes WUDFHost dumps fine:
  `cdb -z <dmp> -y 'srv*C:\symbols*https://msdl.microsoft.com/download/symbols;<LGIdd pdb dir>' -c '~*kc 25; q'`.
- virglrenderer 1.3.0 source (host behavior ground truth for attach/import)
  — clone `gitlab.freedesktop.org/virgl/virglrenderer` tag
  `virglrenderer-1.3.0`; the silent-attach hazards live in
  `src/proxy/proxy_context.c` + `src/virglrenderer.c`.
