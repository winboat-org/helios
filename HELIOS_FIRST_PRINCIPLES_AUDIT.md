# Helios vGPU — First-Principles Audit: every hack, the contract it violates, and the real fix

**Date:** 2026-07-03. **Written at the overseer's direction** after repeated instability
("stop implementing hacks; there's something fundamentally wrong; the driver must be
implemented properly, no matter the effort"). This document supersedes the *approach* of the
incremental handoffs (`HANDOFF_GDI_BLACKFRAME.md` §6a–6g remain the factual session record).

The goal is unchanged (`IDD_HELIOS_RENDER_PLAN.md`, memory `wddm-hwaccel-desktop-is-the-goal`):
**DWM composites the whole desktop ON the Helios WDDM render adapter → venus → host GPU; the
Looking Glass IDD captures via the standard IddCx swapchain; zero manual intervention.**

---

## 0. Why the system currently needs rituals (the one-paragraph diagnosis)

Every manual intervention we perform today (IDD `pnputil /restart-device` kicks, dwm/shell
mass-kills, Helios device restarts, reboot recipes) is compensation for one of seven broken
contracts listed below. The system *works* when the rituals happen to re-sequence the broken
lifecycles into a lucky order; it *fails* (black surfaces, ring poison, dwm crash-loops,
stillborn swapchains, guest wedges) when they don't. None of the rituals are fixes. The fix is
to make each contract hold by construction, at which point the boot sequence
`adapter starts → dwm starts → IDD arrives → OS offers swapchain → frames` completes with no
external help — the way every real WDDM+IDD stack on real hardware behaves.

---

## 1. The seven contracts (first principles)

### C1 — One storage identity per surface (currently: partial, with silent lies)

**Principle.** A WDDM allocation, the virtio-gpu resource behind it, and the `VkDeviceMemory`
bound in *every* consuming process's venus context must denote the same bytes. The mapping
WDDM-hAllocation → venus resid → per-context VkDeviceMemory must be a **total function**,
established at allocation/open time, never guessed later.

**Today.** The resid rides in UMD private data (dxgkrnl snapshots it before the KMD's
create-time write-back, so the KMD *mutates the open-time buffers* in `DxgkDdiOpenAllocation`
— `patch_alloc_resid`), the UMD *heuristically prefers* "whichever private-data buffer carries
a nonzero resid", and per-process materialization is an *import that can fail* — on failure the
UMD silently substitutes a **metadata texture** (a fake surface: draws "succeed", content is
black). Every black window in the capture is this contract failing somewhere.

**Real fix.**
1. The KMD is the single authority for identity: at `CreateAllocation` it records
   `{resid, blob_size, memory_type_index, exact venus allocationSize}` in the KMD-owned
   allocation state; at `OpenAllocation` it (a) writes the full identity record into the
   open-time private data (the current patching, but as a *defined ABI struct*, versioned) and
   (b) **attaches the resource to the opener's venus context** (CTX_ATTACH_RESOURCE via the
   opener's ctx id) so the subsequent import cannot fail with "resource not in context".
2. The ICD import (`vn_device_memory_import_resource_id`) must use the *recorded* venus
   allocation size (vkr's OPAQUE-fd import requires exact-size match) and the recorded memory
   type — not the WDDM-side size.
3. **Remove the metadata-texture fallback** for opens of identified resources. An import
   failure of a correctly-identified resource is a bug; fail the open loudly
   (`E_FAIL` + log) so it is found, instead of rendering fake content forever.

### C2 — No dependent command may follow an unconfirmed allocation (currently: two of four paths fixed)

**Principle.** Upstream venus submits `vkAllocateMemory` asynchronously and *assumes host
allocation cannot fail*. On this stack it can (external-handle rules, udmabuf limits, NVIDIA
driver behavior changing across releases). Any async-alloc failure lets the guest emit
`vkBindImageMemory2`/`vkFreeMemory`/blob-create against a host object that doesn't exist —
and vkr treats phantom-object commands as **fatal decoder state**, destroying the entire venus
context of that process (this was the dwm 4-minute crash-loop; a fresh
`vkr: failed to look up object 233 of type 8 → vkBindImageMemory2 CS error` was observed
2026-07-03 05:5x, proving a path is still open).

**Today.** `vn_device_memory_import_resource_id` (sync, fixed) and
`vn_device_memory_alloc_export` (sync, fixed this session) are safe.
**Still async:** `vn_device_memory_alloc_simple` — used by *plain* and *host-visible*
allocations. Host-visible allocs are exactly the ones vkr rewrites (force-export /
udmabuf / gbm import branches), i.e. the ones most likely to fail host-side. This is the
remaining object-233-class hole.

**Real fix.** On this transport, **all** `vkAllocateMemory` submissions are synchronous
(`vn_call_vkAllocateMemory`), matching upstream's own `VN_PERF=no_async_mem_alloc` semantics.
The wire round-trip already exists; the cost is one ring round-trip per allocation — noise
compared to a context kill. Delete the async path from the Windows build rather than gating it,
so it cannot regress. (Longer term, if alloc latency ever matters: failure-fencing — track
unconfirmed allocs and stall dependent commands — but do not build this until profiling
demands it.)

### C3 — Fence signals must mean "host GPU finished" (currently: accidentally coherent)

**Principle.** The WDDM scheduler contract: a monitored-fence signal for a submission means the
GPU completed it. DWM's frame pacing, the IDD's acquire loop, and TDR all consume this.

**Today.** Venus-over-Escape is synchronous per verb, so the KMD can signal fences immediately
and everything is *accidentally* coherent — while hiding real GPU latency inside Escape calls
(DWM's render thread blocks inside D3DKMTEscape for host GPU work) and making TDR watchdogs
see a driver that never has outstanding work. Any move to async submission breaks the world.

**Real fix.** `DxgkDdiSubmitCommand(Virtual)` forwards the venus command stream
asynchronously; the KMD's virtio ISR/DPC (which already exists for the System-class path)
signals the monitored fence when the host fence for that submission completes. This is the
"§4 real venus-driven SubmitCommand fence" from `WDDM_FAKE_VIDMM_RESEARCH.md` — designed,
never implemented.

### C4 — The DDI surface must be contract-legal in every path (currently: dxgkrnl sees a liar)

**Principle.** dxgkrnl audits driver returns. Invalid NTSTATUS values and NOT_IMPLEMENTED in
mandatory-during-TDR paths mark the driver as misbehaving; dxgkrnl responds with adapter
resets/TDR — and **adapter resets during boot are exactly the pairing churn that stillbirths
the first IDD swapchain** (the 0x887A0026 chain). The 2026-07-02 ETW capture recorded
**197× C0000001, 48× C00000BB, 6× C0000002 "Driver returned an invalid NTSTATUS"** during the
active swapchain window, plus `DxgkDdiCollectDbgInfo` STATUS_NOT_IMPLEMENTED firing *during
TDR dump collection*.

**Real fix.** Audit every DDI return path in `kmd_render` (agent inventory below): implement
`CollectDbgInfo` for real (fill the buffer per contract), make `QueryAdapterInfo` return
`STATUS_SUCCESS`+zeroed/defaulted data or the *documented* not-supported status per info type,
find and eliminate the C0000001 source (it is per-frame — a hot path), and re-run the ETW
AzureTriage check until it is **zero events**. This is the leading suspect for "why does the
pairing churn at boot at all".

### C5 — The IDD must self-converge (currently: kick-driven)

**Principle.** IddCx drivers own their monitor lifecycle. The OS is free to abandon/re-create
swapchains and re-pair render adapters at any time; a correct driver converges from every such
event with no external help. "We should not have to restart the IDD device at all" — correct,
and the IddCx contract agrees.

**Today, three concrete lifecycle bugs in LGIdd:**
1. **The replug state machine stalls.** `ReplugMonitor()` issues `IddCxMonitorDeparture` and
   sets `m_replugMonitor`; re-arrival happens only in `OnUnassignedSwapChain`. If no swapchain
   was assigned at departure time (the common stuck state!), UnassignSwapChain never fires →
   FinishInit never re-runs → the IDD sits monitor-less forever. (Observed live 2026-07-03
   00:04–00:07: `ReplugMonitor` logged, then silence.)
2. **No offer-timeout policy.** After MonitorArrival + CommitModes(paths=1), if AssignSwapChain
   doesn't arrive within N seconds (boot-time budget exhaustion, stale pairing after a dwm
   restart), nothing ever retries. A replug from the LGMP timer would re-negotiate. Similarly,
   a swapchain that acquires zero frames for N seconds *after a topology change* is a stale
   binding (dwm feeds the new pairing instance) and must be abandoned/replugged.
3. **`IddCxAdapterSetRenderAdapter` is issued on every FinishInit** (i.e., every replug),
   re-triggering pairing re-creation each cycle. Issue it once per adapter lifetime — or not at
   all when the OS default pick is already Helios.

**Real fix.** A monitor-lifecycle state machine in `CIndirectDeviceContext` driven from the
existing LGMP timer: states {NoMonitor, Arrived, SwapChainBound, Stale}; timers for
offer-timeout and acquire-stall; a single replug primitive that always completes
(departure → *unconditionally* schedule FinishInit re-run, not gated on UnassignSwapChain);
SetRenderAdapter latched once.

### C6 — CPU-visible bytes and venus bytes must be the same bytes (§6e — designed, unimplemented)

**Principle.** GDI/CPU writes (VidMm CPU views, `Lock`, the KMD software GDI executor) and the
venus blob the host samples must be one storage.

**Today.** The GDI executor CPU-rasterizes into mapped blob windows (correct direction), but
VidMm's own CPU mappings of aperture-backed allocations are plain RAM pages unrelated to the
venus blob — any path where Windows itself touches allocation memory (GDI redirection
surfaces, `D3DKMTLock`, cursor shapes) diverges silently.

**Real fix (already specced, `WDDM_FAKE_VIDMM_RESEARCH.md` §C + handoff §6e):** back
allocations with the CpuVisible BAR **memory segment** and issue `RESOURCE_MAP_BLOB` at the
VidMm-assigned segment offset, so every CPU view *is* the blob window. Implement, then delete
the executor's private mapping table.

### C7 — DXVK must run on queried capabilities, not hardcodes and retry ladders (currently: heuristic soup)

**Principle.** The guest driver stack must derive external-memory behavior from the ICD's
capability queries. Hardcodes rot: the NVIDIA 610.x host driver started supporting dma_buf
export, vkr's probe flipped `renderer_handle_type` from OPAQUE_FD to DMA_BUF, and every
hardcoded OPAQUE_FD assumption and skipped capability check downstream silently changed
meaning (this session's discovery).

**Today.** dxvk-helios hardcodes `heliosRendererHandleType = OPAQUE_FD`; `canShareImage`
returns blind-true in KMT mode (the capability query it *should* consult was broken until this
session — now fixed: `vn_GetPhysicalDeviceImageFormatProperties2` no longer rejects
optimal-tiling external images on Windows, and `VK_KHR_external_memory_fd` is exposed so
`vn_device_fix_create_info` equips the **host** device with the fd/dma_buf extensions vkr's
export/import machinery calls). The UMD *invents* `MISC_SHARED` from the DDI PRESENT bind flag
and walks a 0x2 → 0x802 retry ladder. And DXVK's allocator hands out **buffer-less
allocations** when a memory type has no global buffer (venus memory-type topology: buffers
only live in types 0/1, so host-visible types 2/3/4 have no global buffer) — callers deref
`m_buffer` → the `DxvkBuffer::assignStorage` AV that crashed dwm three times on 2026-07-03
(05:34/05:41/05:44, faulting module = the UMD DLL, full stack in `C:\HeliosDumps`).

**Real fix.**
1. `DxvkMemoryAllocator`: a memory type without a global buffer must never satisfy a buffer
   suballocation — fall through to the dedicated-buffer path (the code already has the
   fall-through; the "keep the allocation around for now" branch returns the buffer-less
   allocation instead). Fix upstream-style; add the same null-storage discipline to
   `DxvkBuffer` as was added to `DxvkImage`.
2. Replace the KMT-mode hardcode with the queried handle type / the now-working
   `getFormatLimits` check in `canShareImage`.
3. Delete the UMD misc-flag inventions and retry ladder once C1 makes shared-surface creation
   deterministic.

---

## 2. Component-by-component hack inventory

> Populated from four parallel code audits (KMD, UMD+bridge, ICD/mesa port, dxvk-helios+LGIdd)
> — see §2.1–2.4. Each entry: location — what it does — contract violated — disposition
> (**REPLACE** = needs the real implementation, **KEEP** = legitimate platform shim,
> **DELETE** = obsolete once its contract fix lands).

### 2.1 KMD (`kmd_render/`) — audited in full

**A-class (destabilizes dxgkrnl / adapter lifecycle):**

| # | Location | Finding | Contract | Disposition |
|---|----------|---------|----------|-------------|
| K-A1 | `ddi/create_allocation.rs:375,397` | **THE 197× C0000001 SOURCE FOUND**: `create_one` returns `STATUS_UNSUCCESSFUL` (=0xC0000001, not in the DDI's legal return set) when the host rejects the backing blob — fires during swapchain-surface creation | C4 | **REPLACE** (map to `STATUS_NO_MEMORY`; and with C2 fixed host rejects become rare) |
| K-A2 | `ddi/submit_command.rs:197-247` | Fences signaled immediately on submit; render DMA content never read/forwarded ("forwarding is DEFERRED until a UMD actually drives the render path"). `last_completed_fence` not monotonic-guarded | C3 | **REPLACE** (the §C.1 real submission path; fence from used-ring completion) |
| K-A3 | `virtio/gpu.rs:564,1066,1114`, `virtio/venus.rs:105,433-479`, `adapter.rs:250-266` | Synchronous virtio round-trips **busy-poll at DISPATCH_LEVEL under the device spinlock with no timeout** (`RING_POLL_SPINS=100_000_000`); `allocate_memory_blob` reached from CreateAllocation under the lock. Wedged host → 0x101/0x133 bugcheck or hard hang (observed: the guest wedge of 2026-07-03 04:0x) | C4/C2 | **REPLACE** (bounded waits + passive-level requeue; move venus allocs out from under the spinlock) |
| K-A4 | `ddi/blob_map.rs:69-113` | `MmMapLockedPagesSpecifyCache(UserMode)` **raises on failure; no SEH in no_std → bugcheck**, reachable from `D3DKMTEscape` MAP_BLOB by any process | C4 | **REPLACE** (C `__try/__except` shim — the file's own TODO) |
| K-A5 | `ddi/submit_command.rs:536-541` | `DxgkDdiCollectDbgInfo` → `STATUS_NOT_IMPLEMENTED`, fires during TDR dump collection | C4 | **REPLACE** (fill buffer per contract, return SUCCESS) |
| K-A6 | `ddi/create_allocation.rs:212-227,663-676` | `patch_alloc_resid`: mutates the runtime-owned OpenAllocation private-data buffers to smuggle the venus resid to the UMD (create-time write-back never reaches the UMD copy) | C1 | **REPLACE** (versioned identity-record ABI written at open, as C1 design; keep the open-time write point, formalize it) |

**B-class (content-losing):**

| # | Location | Finding | Contract | Disposition |
|---|----------|---------|----------|-------------|
| K-B1 | `ddi/build_paging_buffer.rs:66-84` | ALL paging ops no-ops incl. `TRANSFER`/eviction (bytes never copied on evict/restore) | C6 | KEEP-for-now, document; revisit with C6 (over-size segment avoids paging by design) |
| K-B2 | `ddi/display.rs:62-210` | `DxgkDdiPresent` writes a 4-DWORD `"HEPR"` nop DMA — composed pixels never blitted to the destination | C3 | **REPLACE** with the real submission path (K-A2) |
| K-B3 | `ddi/gdi_blit.rs` | GDI executor: 32bpp only, ROP3→SRCCOPY, COLORFILL→PATCOPY, ClearType approx, unresolved surfaces **silently skipped** (GdF counters) | C6/C1 | **REPLACE** incrementally (correct ROPs; unresolved surface = loud error once C1 lands) |
| K-B4 | `ddi/escape.rs:164-175` | `escape_wait_fence` always-SUCCESS stub (valid only while submission is fully synchronous) | C3 | **REPLACE** with real KEVENT wait when async submission lands |
| K-B5 | `ddi/create_allocation.rs:824-827` | `GetStandardAllocationDriverData` rejects unhandled standard types with NOT_SUPPORTED | C4 | KEEP (only 4 types observed), add loud diag |

**C-class (benign/deliberate, document-only):** `QueryAdapterInfo` NOT_SUPPORTED-for-unknown
(`query_adapter_info.rs:89-96` — deliberate: answering PHYSICALADAPTERCAPS bugchecks dxgmms2
0x3B on a null engine; revisit with K-A2's real engine), interrupt/scheduler/VidPN stubs
(render-only adapter), handle-only contexts (`device.rs:114-149`) with the load-bearing
`DmaBufferSegmentSet=1` workaround, `venus::Writer` unchecked 512-byte buffer (panic site now
reachable from CreateAllocation — add a bounds check), pervasive `diag::record` bring-up
tracers ("TEMPORARY … remove once Code 43 clears"), `hal.rs` dangling-pointer failure paths
(init-only), stale `DECLARE_CROSS_ADAPTER_RESOURCE=false` vs comment drift, decorative-GpuMmu
no-ops (deliberate design per `gpummu.rs:1-9`; valid only while the host owns all memory by
resource id).

### 2.2 UMD (`umd/src`, `umd/bridge`) — audited in full

**A-class (crash/corruption):**

| # | Location | Finding | Contract | Disposition |
|---|----------|---------|----------|-------------|
| U-A1 | `forward.rs:462-574` (`sample_present_source`, called from `dxgi_present:5002`, `resolve_shared_resource:1280,1315`) | **THE dwm AV SITE (corrected attribution)**: a per-present *diagnostic* CPU sampler creates a fresh STAGING texture + CopyResource + Flush + Map **every present** (1-in-120 forever after the first 32) purely to log a hash — forces DXVK's `CreateMappedBuffer` → `DxvkBuffer::assignStorage` null-deref, plus a per-frame GPU→CPU stall | C7 (and "no instrumentation on hot paths") | **DELETE** (move behind an off-by-default env/registry gate if ever needed) |
| U-A2 | `forward.rs:388-425` | `release_resource` calls `DeallocateCb` for allocations it never allocated (opened/shared) AND passes the illegal `hResource`+`HandleList` combination → the known `0x80070057`, runtime-side allocation leak | C4/C1 | **REPLACE** (track origin: opened vs created; deallocate accordingly) |
| U-A3 | `forward.rs:1002-1017`, bridge `:568-589` | Venus resource ownership transferred to the KMD allocation and KMT handles re-stamped in two un-synchronized steps while the image may be live; transfer failure logged-and-ignored | C1 | **REPLACE** (single atomic identity handshake at create) |
| U-A4 | bridge `dxvk_bridge.cpp:627-629` | A venus resid (small u32) cast to a Win32 `HANDLE` and fed to DXVK's shared-open ctor; `MISC_SHARED` force-added | C1/C7 | **REPLACE** with a typed import path (no HANDLE punning) |

**B-class (content-losing):**

| # | Location | Finding | Disposition |
|---|----------|---------|-------------|
| U-B1 | `forward.rs:598-635` | `api_misc_flags` invents `MISC_SHARED` from the DDI PRESENT bind bit and unconditionally promotes DDI SHARED → API `SHARED\|NTHANDLE` (0x802); opener side masks back down to plain SHARED — producer/opener share-mode mismatch by construction | **REPLACE** (translate 1:1; present-ability via C1 identity, not invented flags) |
| U-B2 | `forward.rs:1088-1244` | **Metadata-texture fallback**: import failure → fresh blank texture stamped with the real KMT handles as if it aliased the shared surface → silent black content (incl. 1×1 fabricated "meta" when private data absent; invented bind flags in the legacy parse) | **DELETE** per C1 (fail the open loudly) |
| U-B3 | bridge `:519-566` + `forward.rs:979-992` | "memory not importable" heuristic (`memory <= u32::MAX && offset == 0`) silently creates a NON-aliasing standard blob → KMD/compositor see empty memory while DXVK renders elsewhere | **REPLACE** per C1 |
| U-B4 | `forward.rs:2226-2260` | Geometry-shader stream-output declarations dropped ("Create a plain GS for now") | **REPLACE** (real SO decl translation) |
| U-B5 | `forward.rs:5083-5092, 4977-5057, 5149-5162` | `dxgi_set_display_mode` = Flush-only no-op; `dxgi_present` full-subresource copy only when both handles present (no scaling/dirty rects); `dxgi_blt` ignores source rect | **REPLACE** with C3's real present path |

**C-class (protocol-shape heuristics → real contracts):** PRESENT/CAPTURE bind bits computed
then discarded (`forward.rs:594`); the `upgrade` closure "prefer whichever private-data buffer
has a nonzero resid" (`:1090-1127`); **the venus resid smuggled in a field literally named
`_pad`** (`protocol/src/wddm.rs:60`, written `forward.rs:744-746`) — replace with the C1
versioned identity struct; synthetic `TEXCOORD<r>` ISGN blobs + hand-built DXBC containers
(`forward.rs:4605-4673`, bridge `:410-461` — KEEP until dxbc-spv offers a location contract,
but pin with a test); FL 10_1 advertisement to steer runtime to FL10_0 (`lib.rs:757-767`);
cap-clamping in CheckFormatSupport/MSAA to dodge runtime rejection (`forward.rs:3749-3906`);
residency always-FULLY_RESIDENT, gamma/priority shape stubs (`:5064-5113`); process-wide
`_putenv_s` from inside the UMD + walk-all-modules venus export discovery + adapter-0 LUID
fallback (bridge `:806-822`); `local||global` KMT-handle conflation (`forward.rs:240`).

**D — the silent-no-op DDI table (`device_funcs.rs`):** all 152 slots pre-filled with a
transmuted `return 0` stub (S_OK for HRESULT DDIs, outputs untouched — already corrupted
`D3D10DDI_COUNTER_INFO` once, `forward.rs:3787`), `CalcPrivate*Size` stubs return a flat 256.
Every DDI not in the install lists silently succeeds-and-does-nothing; the self-test only
checks non-null slots. **REPLACE the default stub with a loud fail** (log + `E_NOTIMPL`-style
error via the runtime error callback, per-entry counters kept) so unimplemented surface is
visible, and implement the entries DWM actually exercises. Also: `create_*` errors on
VOID-returning DDIs leave null handles the runtime will use ("dropped for now" TODO,
`forward.rs:9`) — must report via `pfnSetErrorCb`.

### 2.3 ICD (`icd/mesa src/virtio/vulkan/`) — audited in full (19 commits + working tree; `vn_renderer_helios.c` = 2386 lines)

**A-class (ring poison / context death):**

| # | Location | Finding | Contract | Disposition |
|---|----------|---------|----------|-------------|
| I-A1 | `vn_device_memory.c:143-164` via `:479-512` | **THE OBJECT-233 HOLE CONFIRMED**: `vn_device_memory_alloc_simple` (plain + host-visible non-export allocs — the most common path) is still ASYNC; a host alloc failure → guest holds a phantom `VkDeviceMemory` → next async `vkBind*Memory2` → CS error → fatal ring. Import/export paths already fixed sync | C2 | **REPLACE** (sync, delete the async branch on Windows) |
| I-A2 | `vn_device_memory.c:289-330` | `import_dma_buf` ends in the async alloc too (latent — path unwired today) | C2 | **REPLACE** with I-A1 |
| I-A3 | `vn_renderer_helios.c:1720-1746` | Blob-create "optimistically signals batch syncs" with literal `fence_id=0` — sync reported complete before the submit retires, resting on an UNVERIFIED "the KMD quiesces in-flight submits before ALLOC_BLOB" assumption | C3 | **REPLACE** (real fence value from a KMD out-field; document/enforce the quiesce in the KMD or drop the assumption) |
| I-A4 | `vn_renderer_helios.c:1412-1455` | `helios_submit` surfaces only escape-transport failures; host decoder/CS errors are invisible until the ring goes fatal | C2/C3 | **REPLACE** (submit status out-field; poll ring status after submit) |
| I-A5 | `vn_renderer_helios.c:1457-1601, 1067-1075` | `helios_wait`: infinite-timeout wait on an unsignalable sync returns `VK_TIMEOUT`; WAIT_FENCE escape return treated as completion without verifying the fence value | C3 | **REPLACE** (KMD returns the reached fence value; verify) |
| I-A6 | `vn_ring.c:241-243, 511-513, 698-721` | Torn/fatal ring guards return "already retired"/success — poisoned ring reported as progress (crash→silent wrong results) | C2 | **REPLACE** (propagate `VK_ERROR_DEVICE_LOST` end-to-end) |
| I-A7 | `vn_renderer_helios.c:2149-2191` | Hardcoded renderer info: `vk_extension_mask` all-zero ("everything supported"), no version negotiation; stale "each submit blocks" comment contradicts the now-async submit; `ring_idx` multi-queue fencing unverified | C4-analog | **REPLACE** (real capset negotiation via a KMD GET_CAPSET escape) |
| I-A8 | `vn_common.c:220-225` | Spin-before-abort budget raised ~16× — masks wedges for minutes instead of failing | C2 | **DELETE** once I-A1..A6 land |

**B-class (spec violations tolerated by specific drivers — fragile across driver updates):**
`vn_Bind{Image,Buffer}Memory2` strip the entire `pNext` chain (drops
`VkBindImagePlaneMemoryInfo` → planar/disjoint images silently mis-bound;
`vn_image.c:748-758`, `vn_buffer.c:480-497`) — **REPLACE** with selective filtering.
Force-injected external-memory info to match vkr's force-export
(`vn_buffer.c:375-392`; images only LINEAR+UNDEFINED — PREINITIALIZED images knowingly keep
the VUID-02728 mismatch "because Doom does not use them") — **REPLACE** by negotiating the
force-export policy explicitly instead of mirroring it per-driver. The `renderer_handle_type`
probe reimplements vkr's private fallback ladder bit-for-bit (`vn_physical_device.c:~1043-1110`)
— this session proved the fragility (NVIDIA 610.x flipping dma_buf support silently changed
its meaning); same **REPLACE** direction. `KHR_external_memory_fd` exposed as a wire-handle
descriptor (this session; KEEP with the documented caveat that guest `vkGetMemoryFdKHR` is
nonsense). DMA_BUF/optimal-tiling query guard skipped on Windows (this session; KEEP — host
accepts optimal external images; contingent on host driver). Win32 external SEMAPHORES
emulated ICD-side over D3DKMT sync objects with CPU-side timeline sync (real desync hazard,
audit §B7) while external FENCES are entirely absent — flag for the C3 rework.
Video-queue bits masked; WSI sw-path fake-signaled acquire sync (deliberate, correct for
CPU-synchronous present).

**C-class:** feedback cache-op elision (mapping-model-dependent, OK), extensive env-gated
diagnostics incl. a per-submit `malloc` on the hot path and a vectored AV handler, vtn link
stubs, CTX_CREATE-probe adapter discovery (robust), synchronous DestroyDevice ordering fix
(good — keep).

### 2.4 dxvk-helios + LGIdd — audited in full

**A-class (crash/corruption):**

| # | Location | Finding | Contract | Disposition |
|---|----------|---------|----------|-------------|
| D-A1 | `dxvk_buffer.h:303-324`, `dxvk_buffer.cpp:34`; caller `d3d11_texture.cpp:866-907,219` | **THE dwm AV, precise mechanism**: `DxvkBuffer` ctor does `assignStorage(allocateStorage())` with NO null guard (the DxvkImage guards were never mirrored); `createBufferResource` can return null (`dxvk_memory.cpp:1015-1022,1095-1098`); a null-deref is not a C++ exception so the ubiquitous `catch(DxvkError)`→`E_INVALIDARG` net (C7 sites) cannot catch it → process AV. (The "without global buffer" logged path itself falls through and self-repairs — earlier attribution refined.) Exercised per-present by the UMD's diagnostic sampler (U-A1) | C7 | **REPLACE** (throw in ctor + null-guard assignStorage, mirroring DxvkImage — converts the crash into the already-handled E_INVALIDARG) |
| D-A2 | `dxvk_image.cpp:368-369,386,402-405,411,697-706,249-251` | KMT-shared mode: hardcoded `OPAQUE_FD` renderer handle type (now stale — renderer is DMA_BUF); `HANDLE`→`uint32_t resourceId` truncation; `canShareImage` blind-true bypassing the capability check; `forceDedicated` defeating suballocation | C7/C1 | **REPLACE** (use the now-working capability query; typed resid, no HANDLE punning — pairs with U-A4) |
| D-A3 | `CIndirectMonitorContext.cpp:39,73-78` | Unbounded `goto reInit` on `CD3D12Device::RETRY` — can spin forever inside the AssignSwapChain callback | C5 | **REPLACE** (retry cap + ABANDON) |
| D-A4 | `CSwapChainProcessor.cpp:50-118` | Swapchain-handle single-owner invariant hand-maintained (thread deletes on exit; DisownSwapChain on failure); destructor `WaitForSingleObject(INFINITE)` can deadlock teardown | C5 | KEEP short-term, assert the invariant; revisit in the C5 state machine |

**B-class (convergence-blocking / content-losing):**

| # | Location | Finding | Disposition |
|---|----------|---------|-------------|
| D-B1 | `CIndirectDeviceContext.cpp:311-331,344-354` + `Device.cpp:207-213` | **THE REPLUG STALL, exact hole confirmed**: re-arrival only happens in `OnUnassignedSwapChain`, which the OS only calls if a swapchain WAS assigned; departure with no assigned swapchain → `m_replugMonitor` latched true forever → monitor never re-arrives, all future replugs no-op → permanent black, no self-recovery | **REPLACE** (C5 state machine: replug primitive that always completes + watchdog) |
| D-B2 | (absence) | No offer-timeout policy (arrival OK → OS silent → idle forever) and no acquire-stall policy (bound-but-never-fed swapchain pends forever after dwm restarts) | **REPLACE** (C5 timers) |
| D-B3 | `CSwapChainProcessor.cpp:170-180` | SetDevice failure on the thread → "continuing without a bound device" → acquire loop spins frameless with no escalation | **REPLACE** (escalate to replug) |
| D-B4 | `CSwapChainProcessor.cpp:148-167` | SetDevice 5×100 ms fixed retry window, then ABANDON — hand-tuned against boot LUID churn | KEEP until C4/C5 land, then re-evaluate |
| D-B5 | `CIndirectDeviceContext.cpp:267,248` | `SelectRenderAdapter`/`IddCxAdapterSetRenderAdapter` re-issued on EVERY FinishInit/replug — suspected positive-feedback churn loop (replug → SetRenderAdapter → pairing churn → ACCESS_LOST → replug) | **REPLACE** (latch once per adapter) |
| D-B6 | `CIndirectDeviceContext.cpp:95-97` | FP16/HDR force-disabled while HDR target caps still advertised | KEEP (documented), align caps |
| D-B7 | `CSettings.cpp:35-105` | Registry-persisted ExtraMode 1896×1030 becomes the EDID-preferred mode forever | KEEP (by design for LG), document |

**C-class:** DXVK log-path default (this session, benign — ensure dir exists),
`khrExternalMemoryFd` enable (this session — note: nothing gates D-A2's path on the feature
bit), sparse/tiled force-disable (deliberate: venus sparse encoder not conformant),
KeyedMutex/D3DKMT sync modernization (sound), ExportImageInfo no-storage guard (good),
KMT-mode clean-error returns in CreateSharedHandle/OpenSharedResource (good),
`heliosKmtOnlySharedResources()` env-var read duplicated across 5 TUs on hot paths (cache it),
LGIdd lifecycle hardening + helios_d3d11_static build target (benign).

---

## 3. Priority plan

- **P0 — stop the bleeding (stability):**
  C2 complete (make `vn_device_memory_alloc_simple` sync — closes the object-233 bind-poison
  class; I-A1/I-A2) · **DELETE the UMD per-present diagnostic sampler** (U-A1 — a live crash
  *and* a per-frame GPU→CPU stall) · DxvkBuffer null-storage guards mirroring DxvkImage
  (D-A1 — converts any residual alloc failure into the handled E_INVALIDARG path) · C4 DDI
  status audit (K-A1 `STATUS_UNSUCCESSFUL` from CreateAllocation **is the 197× C0000001**;
  K-A5 CollectDbgInfo; verify by ETW AzureTriage = zero events) · K-A4 SEH shim for MAP_BLOB
  (bugcheck reachable from any process) · U-A2 deallocate-only-what-you-allocated.
- **P1 — self-convergence (no rituals):**
  C5 (LGIdd lifecycle state machine + SetRenderAdapter-once). Acceptance test: cold boot →
  frames in the LG client with **zero** manual actions, ten boots in a row.
- **P2 — content correctness:**
  C1 (allocation identity end-to-end; delete metadata-texture fallback) then C6 (CPU↔blob
  coherence). Acceptance: GDI apps (notepad/cmd) legible in the capture; no black windows.
- **P3 — architectural honesty:**
  C3 (real fences) · C7.2/7.3 (capability-driven sharing; delete the retry ladder).

Rationale for the order: P0 items are small, sharply-scoped, and remove the crash/poison noise
that makes every other investigation ambiguous. P1 removes the human from the loop so P2/P3 can
be validated by unattended reboots. P2 is the user-visible payoff. P3 unlocks performance and
future-proofing but changes the most code.

---

## 4. Evidence status — what is actually verified vs. NOT

**⚠️ Correction (owner review): "frames being displayed" has NEVER been achieved.** The
2026-07-03 "frames" evidence was: single `FRAME_TYPE_BGRA 1896x1030` header lines in the LG
client log, short acquire bursts in the IDD log during restart-churn windows, and a KVMFR
memory dump at a *guessed* offset that appeared to contain taskbar pixels. The owner sees
**nothing** in the LG client. Treat all prior "frames flowed / content verified" statements
as unproven. The only acceptance criterion that counts: **the owner watches the live desktop
(moving cursor, opening windows) in the Looking Glass client, sustained, on an untouched cold
boot.** Nothing less closes any milestone.

**Owner's refinement (correct, adopt as the working model): every frame ever observed was a
restart transient.** All acquire bursts in the IDD log sit within seconds of a Helios/IDD
device restart — they are the OS's topology-transition redraws, not steady-state composition.
The discriminating datum: with a freshly bound swapchain, injected input advanced dwm's
present count (3→5) while IDD acquires stayed at exactly **zero** — the bound swapchain is
never fed once the transition settles. So the open question for C5/P1 is sharper than "why no
re-offer": **why does DWM stop presenting into the IDD's swapchain the moment steady state is
reached** — is the IDD display ever actually the active desktop target outside the transition
window? Next session: with a bound swapchain, read the session-1 CCD active topology and
correlate every historical acquire timestamp against the device-restart timestamps to confirm
the transient-only model, then chase where DWM's steady-state presents go (which
pairing/swapchain instance) instead of assuming a static desktop.

Genuinely verified (instrument-level, still true):
- Venus rendering end-to-end on NVIDIA for offscreen work (UMD draw self-test rc=0,
  helios_clear_test).
- The phantom-free/EPERM chain for **import and export** allocs is closed (sync alloc, ICD
  252e89a9+): host log clean of those two fatal signatures since. The *plain/host-visible*
  async path remains open (object-233 event) — P0.1.
- Capability layer after this session's ICD fixes: `VK_KHR_external_memory_fd` exposed,
  external-image queries return SUCCESS/EXPORTABLE, device create with the extension works,
  every export-alloc shape succeeds host-side (probe matrix in `tools/vk_export_alloc_probe.cpp`).
- `IddCxSwapChainSetDevice` can succeed and the acquire loop can run — observed only inside
  churn windows; never a steady state.

## 4b. Implementation status (2026-07-03, second session — P0 complete + P1 implemented)

**P0 — all five items implemented, built, and DEPLOYED** (ICD hash via install-helios-icd.ps1,
KMD 22.22.40.0 devcon-installed + verified active, UMD f9609819 ProgramData hotplug +
device restart, dxvk relinked into the UMD):

1. `vn_device_memory_alloc_simple` is now unconditionally synchronous (async branch DELETED —
   also closes I-A2's `import_dma_buf` latent path, which funnels through it). Host log clean of
   phantom-object signatures since deploy (short window — needs soak).
2. `sample_present_source` DELETED (function + 3 call sites + both sample counters).
3. `DxvkBuffer` ctor throws `DxvkError` on null storage (both ctors), `assignStorage` refuses
   null; **plus a latent-bug fix in both DxvkBuffer and DxvkImage: unregisterResource before a
   ctor throw** (a throwing ctor skips the dtor → dangling pointer in the allocator resource
   map). The allocator's "without global buffer" path was re-verified: it self-repairs via the
   dedicated-buffer fall-through (audit's refined attribution stands; no allocator change).
4. KMD: `create_one` → STATUS_NO_MEMORY (both arms); `DxgkDdiCollectDbgInfo` implemented
   (any-IRQL-safe, 13-DWORD 'HDBG' report from the DISPATCH-safe atomics incl. a new
   CTRL_TIMEOUT_COUNT); MAP_BLOB UserMode map now goes through a C `__try/__except` shim
   (`kmd_render/src/seh_shim.c`, cc build-dep, `/Zl /GS-`, verified linked via the .map);
   ALL four `add_notify_wait_pop` sites replaced with a bounded poll (CTRL_POLL_SPINS=100M)
   + **poison latch** (`VirtioGpu::failed`) so one timeout fails everything fast instead of
   re-spinning at DISPATCH per call; `VenusClient` got the same `fatal` latch and the
   registry-writing `diag()` calls were removed from its DISPATCH-reachable loops (latent IRQL
   bug). **ETW verification: steady-state capture = 0 invalid-NTSTATUS events; the 194
   (153×C0000001 + 41×C00000BB) still visible are a FROZEN pre-fix triage buffer replayed on
   adapter teardown (identical counts across two captures with a device restart between —
   zero growth = zero new emissions; clears on reboot).**
5. `release_resource`: origin tracking (`owns_allocation`), legal D3DDDICB_DEALLOCATE shapes
   only (hResource-only for runtime-associated resources, HandleList-only for owned standalone
   allocs, never both, never deallocating opened handles by list).

**P1 — C5 state machine implemented + DEPLOYED (LGIdd 6.53.8.135 via devcon):**
states {NoMonitor, Arrived, SwapChainBound, ReplugPending} with a dedicated 500 ms watchdog
WDF timer alive from adapter init (NOT the LGMP timer — that only exists after the first
swapchain); replug primitive always completes (unassign fast-path OR forced FinishInit after
1 s); offer-timeout 10 s; first-frame stale-binding timeout 10 s; SetRenderAdapter latched
once per adapter; `goto reInit` capped at 3 → ABANDON; swapchain-thread self-exits (incl.
SetDevice failure and cursor-thread topology loss) escalate to a queued replug; monitor
wrapper context now freed via EvtCleanupCallback (leaked per replug before).

**Live behavior after deploy (instrument-level, NOT user-visible-verified):** first swapchain
of the driver start arrived stillborn (0x887A0026 ×5) → ABANDON → OS offered a second →
SetDevice OK → **3 frames acquired 1896×1030** (client attached, got the format header) →
E_PENDING ever since. The prior driver instance's log tail showed the exact D-B1 stall live
(ReplugMonitor logged 00:15, then 68 min of silence). dwm stable 20+ min, zero new dumps
(the 5 dumps 06:32–06:40 are all adapter-restart collateral: AV inside the ICD when the
device is yanked under live venus mappings — new finding, argues for adapter-loss hardening
in the ICD and reinforces "no restarts").

**NEW discriminating experiment (repeatable on demand):** with the swapchain bound and idle,
`schtasks /IT` notepad → dwm presents on Helios advanced #2→#12 while IDD acquires stayed 0.
The §4 stale-binding model is now reproducible without waiting for restart transients. The
IDD alone cannot distinguish "display static" from "binding fed elsewhere" — the 3 transition
frames disarm any first-frame watchdog by design. Next-session leads: (a) chase which
pairing/swapchain instance dwm's steady-state presents feed (ntoseye on dxgkrnl's blt queue /
indirect swapchain objects); (b) consider a cross-component staleness signal (the UMD sees
dwm's presents on Helios; the IDD sees acquires — presents advancing with acquires pinned at
0 for N s IS decidable with UMD→IDD plumbing, e.g. via the existing pipe server).

**⚠️ Cold-boot addendum (same day, after the owner's hard reboot): the visible frames were
TRANSIENT — they do not survive a cold boot.** The owner sees only the client placeholder
after a hard reboot. Evidence (boot 07:09): dwm stable (P0 held), Helios Code 0, but the IDD
devnode is Code 43 `CM_PROB_FAILED_POST_START` because **LGIdd.dll failfasted WUDFHost — a
UMDF verifier bugcheck** (`FxVerifierDriverReportedBugcheck`, error `050100040000010f`) in
the ~7 s window right after the boot-time first-swapchain ABANDON, *before* the new watchdog
ever ran (per-line-flushed log ends at the abandon; the +10 s watchdog line never printed).
This is the pre-existing June-26 cold-boot failure mode, now bracketed to a crash class and
window. The boot swapchain again arrived paired to an OLDER LUID than the live Helios
adapter (churn), abandon behaved per contract — but at boot the OS never re-offers; the
process dies instead. WUDFHost LocalDumps now enabled (C:\HeliosDumps, full ×3): the next
cold boot yields the stack. **C5's watchdog cannot fix this — no user-mode state machine
survives its own host process being terminated; the failfast itself must be root-caused
and fixed.** See `HANDOFF_FIRST_VISIBLE_FRAMES_2026_07_03.md` §0.

## 5. Handoff prompt (copy-paste for the next session)

> You are continuing the Helios vGPU project in /home/rupansh/helios-vgpu. **Read
> `HELIOS_FIRST_PRINCIPLES_AUDIT.md` in full before touching anything** — it defines seven
> broken contracts (C1–C7), the complete hack inventory across KMD/UMD/ICD/DXVK/LGIdd, and a
> priority plan. The overseer's standing directive: **no hacks, no workarounds, no
> kick/restart rituals — implement the real fixes, however much effort they take.** Do not
> add fallbacks that hide failures (no metadata-texture substitutions, no silent retries, no
> "keep going on error"); a loud failure is preferred over fake success.
>
> **Evidence discipline (the overseer has been burned by hacked proof):** nothing counts as
> working unless the overseer can see it live in the Looking Glass client on an untouched
> boot. Do not claim success from log lines, single-frame captures, memory dumps at guessed
> offsets, or states reached through restart rituals. When a milestone is reached, state
> exactly what was observed, how, and what was NOT verified.
>
> Work the priority plan in order and do not skip ahead:
>
> **P0 (stability):**
> 1. `icd/mesa src/virtio/vulkan/vn_device_memory.c`: make **every** `vkAllocateMemory`
>    submission synchronous on this transport (delete the async path from
>    `vn_device_memory_alloc_simple` for the Windows build — import/export are already sync).
>    This closes the last ring-poison window (phantom-object `vkBindImageMemory2` — observed
>    as "failed to look up object 233 of type 8" even after the import/export fixes).
> 2. `umd/src/forward.rs`: **DELETE `sample_present_source`** (lines ~462-574, called from
>    `dxgi_present` and the resolve paths) — it is pure diagnostics that creates a STAGING
>    texture + CopyResource + Flush + Map on the present hot path forever (1-in-120), and it
>    is what exercises the crash below. Gate behind an off-by-default env var if ever needed.
> 3. `dxvk-helios src/dxvk/dxvk_buffer.{h,cpp}`: give `DxvkBuffer` the DxvkImage null-storage
>    contract — throw `DxvkError` in the ctor when `allocateStorage()` returns null, and
>    null-guard `assignStorage` (dxvk_buffer.h:303-324, dxvk_buffer.cpp:34). A null here is
>    NOT an exception today, so it escapes the `catch(DxvkError)`→`E_INVALIDARG` net and
>    AVs the process (dwm dumps in `C:\HeliosDumps`).
> 4. `kmd_render`: fix `create_one` returning `STATUS_UNSUCCESSFUL` (0xC0000001 — **this is
>    the 197× invalid-NTSTATUS dxgkrnl logs**; `ddi/create_allocation.rs:375,397` → return
>    `STATUS_NO_MEMORY`), implement `DxgkDdiCollectDbgInfo` (submit_command.rs:536), add the
>    SEH shim for MAP_BLOB user mapping (blob_map.rs:69 — raise → bugcheck, reachable from
>    any process via D3DKMTEscape), bound the DISPATCH-level virtio spins (gpu.rs
>    `add_notify_wait_pop`, venus.rs `RING_POLL_SPINS` — the 2026-07-03 guest wedge). Verify
>    with the ETW AzureTriage recipe until zero invalid-status events.
> 5. `umd/src/forward.rs:388-425`: `release_resource` must deallocate only allocations it
>    created (opened resources leak + return 0x80070057 today) and never pass
>    `hResource`+`HandleList` together.
>
> **P1 (self-convergence — the "no more IDD restarts" fix):** rewrite LGIdd's monitor
> lifecycle per audit §C5 + §2.4. The exact confirmed stall (D-B1): re-arrival only happens in
> `OnUnassignedSwapChain`, which the OS only calls if a swapchain WAS assigned — a departure
> with no assigned swapchain latches `m_replugMonitor` forever and the monitor never returns.
> Build: replug primitive that always completes (schedule FinishInit from the LGMP timer
> unconditionally after departure), offer-timeout + acquire-stall watchdogs (D-B2/D-B3),
> `IddCxAdapterSetRenderAdapter` latched once per adapter (D-B5 — it currently re-fires on
> every replug, a suspected churn feedback loop), retry cap on the AssignSwapChain
> `goto reInit` (D-A3). Acceptance: **ten consecutive cold boots, frames in the LG client,
> zero manual actions.** Build with `win_looking_glass_idd`, deploy via devcon (never
> in-place DriverStore copy).
>
> **P2 (content):** C1 allocation-identity (KMD attaches resources to the opener's venus
> context at OpenAllocation; identity record as a versioned ABI struct in private data; ICD
> imports with the recorded exact size; DELETE the metadata-texture fallback), then C6
> CPU↔blob coherence (RESOURCE_MAP_BLOB at VidMm segment offset per
> `WDDM_FAKE_VIDMM_RESEARCH.md` §C).
>
> **P3:** real venus-driven fences (C3), capability-driven sharing in DXVK (C7.2), delete the
> UMD misc-flag retry ladder (C7.3).
>
> Ops constants: build/deploy recipes and gotchas are in `HANDOFF_GDI_BLACKFRAME.md` §6f-2 +
> §6g (UMD = hotplug-helios-umd.ps1 hash-named ProgramData DLL, needs `-RestartDevice`
> because dxgkrnl caches the UMD path; ICD = win_meson + install-helios-icd.ps1; dxvk = copy
> to C:\Users\Rupansh\dxvk-helios then meson compile C:\Users\Rupansh\dxvk-build then purge
> UMD fingerprint + rebuild). dwm's DXVK log: `C:\ProgramData\Helios\dwm_helios_umd_dxvk.log`;
> UMD per-pid logs beside it; host venus errors `/tmp/helios-qemu-stderr.log`. Do NOT
> `taskkill dwm` and restart the Helios PCI device in the same breath (hard-wedged the guest
> once). QMP is at `/tmp/helios-tpm/mon.sock`; ntoseye works KD-less via
> `ntoseye -b memory mcp --http 127.0.0.1:8080`. Ask the user before rebooting or relaunching
> the VM. The tree is uncommitted by design — do not commit.
