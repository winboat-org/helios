# ROADMAP — Stage: Correctness and D3D12 (since 2026-08-05)

*The desktop first rendered end-to-end on 2026-07-05. The active architecture
changed on 2026-07-09: Helios is now a WDDM render+display adapter and owns the
virtio-gpu scanout; IddCx/Looking Glass is no longer the active display path.*

## Stage pivot, 2026-08-05

The **Performance, Stability, Conformance (PSC)** stage is closed as a *stage*;
its stability contracts remain permanently in force and its performance record
is kept below as WS2 — read it before opening any new perf work, because it is
mostly a list of things that have already been tried and measured.

**Why now.** The present-queue stall was root-caused and fixed (WS2, `PresentWmk`,
KMD 22.22.244.0), and the remaining limit is named rather than suspected: the WDDM
FIFO head now blocks on `stream_ready` — the frame's own producer completion on
the host — at `WfBStrm`/`WfBWire` ≈ 15220/161, against a render-thread producer
floor of ~3.7 ms/frame. There is no further sweep to run; the next perf gain needs
a new causal hypothesis, not another arm.

**The new order of business:**

1. **D3D11 correctness / conformance** — charter in `CONFORMANCE.md`, plan in WS3.
2. **D3D12** — charter in `DX12.md`, detail in `docs/dx12/`. **The strategy question is
   CLOSED as of 2026-08-05**: Helios ships a real D3D12 UMD, `helios_umd12.dll`,
   implementing `d3d12umddi` and forwarding into vkd3d-proton's `ID3D12*` COM
   objects — the D3D11 architecture with DXVK swapped for vkd3d and
   `UserModeDriverName[2]` swapped for `[3]`. The app-local vkd3d arm is Phase 0
   of that plan, not an alternative: it proves the whole lower half (vkd3d +
   dxil-spirv + venus + KMD + present) with zero Helios code. Decisions and the
   twelve-lane evidence merge: `docs/dx12/DECISIONS.md`. Checkpoints:
   `docs/dx12/GATES.md` (`D12-G0 … D12-G11`). Today `OpenAdapter12` still
   refuses, and must keep refusing until the commit that makes its body
   reachable.
   *Measured up front:* the guest satisfies vkd3d-proton's
   `VP_D3D12_FL_12_2_baseline` in full (zero feature/extension misses), and the
   KMD work list is empty for Phase 0 / three small items for the DDI arm.
3. **Stability** — WS1, unchanged and non-negotiable.
4. **Performance** — WS2, PAUSED. Do not reopen without a new hypothesis.

**Also landed with the pivot (2026-08-05), because a stage change is the right
time to stop shipping something nobody measured:**

- **Sane values are now the defaults.** Three knobs whose code default was OFF
  had been ON in the test VM's registry since 2026-08-03, so every accepted
  score was measured on a configuration no fresh install produced. A fresh
  install got the runtime's *emulated* command-list path — GT1 ≈ 184,
  Graphics ≈ 43.5k — instead of the measured GT1 221-227 / Graphics 49-52k.
  Flipped to ON, each with the evidence in the comment at its read site:
  `HELIOS_DXVK_CL_RETAIN_SAMPLER_REFS` (isolated same-boot A/B, GT1
  **53.609 → 181.938**), `UmdCommandLists`, `HELIOS_DXVK_CL_INLINE_REPLAY`.
  `VidMmVramMB` likewise went 0 → 4096, the configuration the VidMm work
  actually validated, re-confirmed on 22.22.251.0 before the flip.
  `HELIOS_DXVK_KMT_SHARED` was forced to "1" by the UMD in every process it
  ever created, so it was not a tunable at all; the engine now defaults it ON
  and the `_putenv_s` is gone. **Verified**: with `HKLM\SOFTWARE\Helios`
  completely empty and no service-key overrides, KMD 22.22.252.0 runs GT1
  **222.857**.
- **Retired**: the `probe/` and `host/` crates (orphans — no workspace, no CI,
  no build, cited only by already-archived docs); the write-only
  `TransportGeneration::page_table_window` the tree itself scheduled for
  deletion at R510; the duplicate unread `AdapterKnobs::dma_gpu_fence`;
  `tools/kmd-force-reject-sweep.ps1` (its knob was retired in T6),
  `tools/attach_idd.ps1` (IddCx-only), and the two completed one-shot DXVK
  source patchers.
- **Gates that could only pass are gone or fixed.** `kmd-gate-surface.ps1` and
  `kmd-counter-snapshot.ps1` were watching four counter/knob names the driver
  no longer writes; `umd-gate-surface.ps1` had three log patterns that could
  never match the emitted text. A gate that cannot fail is worse than no gate.
- **Four silent failure counters were surfaced** as `WdSigF` / `DmaNtfF` /
  `TxGone` / `RclBadH`. Each was incremented on a real refusal path and loaded
  by nobody, which is CLAUDE.md's "every refused path gets a named counter"
  rule being violated invisibly. **All four must read 0 on a healthy session.**
- **Docs archived**: `ARCH.md`, `OVERVIEW.md`, `KMD.md`, `ICD.md`,
  `WINDOWED_BLT_DESIGN.md`, `SCANOUT_DRM_MODIFIER_DESIGN.md` → `docs/archive/`.
  `TRANSPORT.md` deliberately stayed at root: its §1/§2 wire format is still
  ground truth and six `protocol/` comments cite it by section; its banner now
  says which sections are live and which are archived.
- **One real bug fell out of the audit**: `tools/escape_owner_probe.c` defined
  `HELIOS_ESCAPE_QUERY_SCANOUT` as `0x000B`, which is
  `HELIOS_ESCAPE_REGISTER_FENCE_EVENT`. The probe had been aiming a
  query-scanout buffer at the fence-event registrar. Fixed to `0x000D`;
  every other escape constant in that file was checked against
  `protocol/src/escape.rs` and is correct.

## RDP desktop lag — root-caused and CLOSED (2026-08-05)

**Symptom (owner):** over RDP, anything that *changes* the desktop is slow —
opening the Start menu, dragging an Explorer window, a closing window whose
frame lingers — while a static desktop, and the dragged window itself, stay
fluid and interactive. A second tester additionally reported frame tearing.

**Helios IS in the RDP path**, which is the fact the whole diagnosis rests on:
the RDP session's `dwm` renders the desktop on Helios, and RDP's indirect
display driver (`RDPIDD`, `SWD\REMOTEDISPLAYENUM\...&SESSIONID_nnnn`, hosted in
`WUDFHost`) is *also* a Helios D3D11 client. Its UMD log shows it creates one
resource of its own — `1920x1080 fmt=87 usage=3 cpu=0x30000`
(BGRA / `USAGE_STAGING` / `CPU_ACCESS_READ|WRITE`) — and `OpenResource`s DWM's
swapchain buffers. So every captured frame is
`CopyResource -> Map(READ) -> memcpy 8.3 MB -> Unmap`.

**Two independent causes. The first was ours; the second was not, and was the
dominant one.** Fixing only the first left the symptom essentially intact —
recorded here because the first fix's numbers look conclusive in isolation and
are not.

### Cause 1 (ours, FIXED) — HOST_CACHED memory was mapped write-combined

`vn_device_memory.c` set `prefer_cached_map` **only** for the WSI blit
destination, so every other host-visible allocation was mapped WC — including
ones DXVK makes on the `HOST_VISIBLE|HOST_COHERENT|HOST_CACHED` type precisely
to get fast CPU reads (`d3d11_texture.cpp:1015`, every `USAGE_STAGING`
resource). The host already reports `CACHED` for that type; `effective_map_cache()`
honours the ICD's request over the host's, so the ICD's own override was the
entire cause. Fixed: honour `HOST_CACHED`.

Same-boot A/B, `tools/d3d11_rdp_capture_probe.cpp` (new — replicates RDPIDD's
loop on RDPIDD's exact resource desc):

| per captured frame | before | after |
|---|---|---|
| `memcpy` out of the mapping | 25.209 ms — **313.8 MB/s** | 0.613 ms — **12906 MB/s** |
| `MOVNTDQA`, same pages | 0.839 ms — 9428.1 MB/s | 0.620 ms — 12756.3 MB/s |
| total | 25.069 ms | 0.942 ms |

**The discriminator is that `memcpy` and `MOVNTDQA` measure the same
afterwards.** Streaming loads only beat `memcpy` on write-combined memory, so
"30x faster with MOVNTDQA" *is* the WC signature, and its disappearance is the
proof. Cache maintenance stays free: the type is COHERENT, guest WB over host WB
is hardware-coherent under KVM, and `helios_bo_needs_cache_ops()` already
exempts exactly this flag combination. Write-only `DYNAMIC` resources are
unaffected — they request `HOST_VISIBLE|HOST_COHERENT`, which matches the
lower-indexed uncached type first, and WC is correct for them.

### Cause 2 (NOT ours, and the dominant one) — RDP's link estimate was fiction

With capture made cheap, the guest sat at **2% total CPU across 16 vCPUs** with
the encoder idle at 1.4%, and the desktop was still slow. During a drag:

    input frames/second 70.03   output frames/second 0.94
    frames skipped/second - insufficient network resources 69.08
    current tcp bandwidth 1536.00 (mean == max)   current tcp rtt 100.00 (mean == max)
    loss rate 0.00

Real RTT to the client is **0 ms** and the link is a **10 Gbps local virtio
bridge** with zero loss. RDP believed 1536 Kbps / 100 ms — its built-in default
profile — and discarded 69 of every 70 frames DWM produced. Cause:

    HKLM\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations\RDP-Tcp
        SelectNetworkDetect = 1

Per this box's own `C:\Windows\PolicyDefinitions\TerminalServer.admx`, `1` =
**connect-time detect OFF**, so RDP never measures the link. Set to `0` (both
connect-time and steady-state detection on); takes effect on the next
connection. `DWMFRAMEINTERVAL=15` is also set on `WinStations`, so this box has
had an "RDP optimization" pass — that is the likely provenance.

**Result after both, same repro** (`tools/rdp-measure.ps1 -Mode drag`):

| drag repro | original | cause 1 fixed | **both fixed** |
|---|---|---|---|
| input -> output fps | 43.6 -> 3.3 | 63.1 -> 15.8 | **36.1 -> 35.1** |
| frames skipped/s (network) | 39.6 | 45.4 | **0.05** |
| avg encoding time | 8.05 ms | 6.20 ms | **1.25 ms** |
| `WUDFHost` (RDPIDD) CPU | **87.0%** | 5.4% | 3.9% |

Owner-confirmed by eye: "RDP is smooth, perfect."

**Instruments, all reusable** — `tools/d3d11_rdp_capture_probe.cpp` +
`tools/d3d11-rdp-capture-probe.ps1` (per-phase capture cost, with a cached
heap->heap control and the MOVNTDQA memory-type classifier);
`tools/rdp-measure.ps1` -> `tools/rdp-lag-repro.ps1` + `tools/rdp-sample.ps1`
(damage workload in the interactive session + RemoteFX Graphics/Network and
per-process CPU sampling).

⚠ **Two traps this cost a cycle each, both now guarded in the scripts.**
(1) The RDP session id is **not stable** — a reconnect (e.g. after a reboot)
moves the same user to a new session, and a sampler hardcoding the old one
silently reports 0% CPU for a session that no longer exists. `rdp-sample.ps1`
resolves it at run time from `query session`. (2) The workload must be *proved*
to have run in the RDP session, not assumed: `rdp-lag-repro.ps1` writes its own
`SessionId` to its output file. The console/SDL session is a *different*
session with its own `dwm`, and a repro landing there measures the SDL scanout
path instead.

**Still open:** the second tester's frame tearing is unretested since the frame
throttle was removed — RDP dropping 98 % of frames could plausibly have produced
partial-looking updates on its own. Re-check before opening any driver-side
tearing investigation.

## Current verified correction (2026-08-04, KMD 22.22.238.0)

- **Fullscreen presentation is not currently a broken SDL scanout path.** The
  owner corrected the viewer identity after the `.238` visible test: the
  hold/judder that looked like roughly 30–40 fps was observed through **VNC**,
  not SDL. Native QEMU SDL is owner-verified rock solid and smooth, and the
  tearing is gone. Treat the earlier claim that SDL independently reproduced
  the hold/burst defect as retracted. A VNC cadence observation is evidence
  about VNC update/encoding/client delivery only; it must not be used to blame
  KMD scanout, QEMU readback, or the D3D11 render path without a correlated
  boundary trace. Smooth SDL means smooth at the display refresh ceiling, not
  that all 150–220 rendered frames per second can be shown on a 60 Hz output.
- `.238` replaced the coarse fallback VSync timer with a high-resolution
  `ExAllocateTimer(EX_TIMER_HIGH_RESOLUTION)` source. In the targeted Combined
  trace its active VSync samples were stable at about 16.6 ms (p95 about
  17.1 ms, no gaps over 40 ms), and the owner now sees no tearing. This closes
  the fullscreen tearing/cadence symptom for SDL; VNC fluidity remains a
  separate frontend/client concern and is not a blocker for D3D11 throughput
  work.
- **Windowed 3D11 presentation remains open and is a different defect.** In
  the interactive standard Fire Strike flow, a blank titled `3DMark Workload`
  window appears and then disappears while 3DMark continues the workload and
  ultimately reports a score. The scheduled custom `FireStrikeCombinedC`
  window trace (`tmp/cadence-238-window-blt-accept.csv`) rendered successfully,
  but it does **not** validate this interactive path. Instrument the actual
  runtime entry point (ordinary Present, single/multi-surface Present1, or MPO)
  and its exact handles/allocations before changing policy. In particular,
  current `dxgi_present1` many-surface code deliberately passes no snapshot or
  stream correlation; that is a source-backed lead, not yet the proven cause.
- **The remaining Fire Strike performance gap is not a scanout-cadence
  diagnosis.** The current multithreaded command-list path recorded GT1
  221.337, GT2 220.996, Physics 125.986, and Combined 41.952 fps in
  `tmp/perf/fs-std.txt`; a later targeted Combined run reached 43.593 fps.
  Nevertheless, the owner observes only roughly 50–60% host-GPU utilization
  in Fire Strike/DX11, versus a sustained roughly 80–90% in Steel Nomad's
  Vulkan path. Use that differential to find where the D3D11-specific
  runtime/UMD/DXVK command-production pipeline fails to keep the GPU fed.
  Steel Nomad exonerates generic Vulkan throughput, but not D3D11 per-draw,
  command-list, synchronization, or submission economics. Do not spend the
  next performance session tuning scanout unless an epoch-correlated trace
  actually shows scanout back-pressure reaching rendering.

## Earlier direct-primary baseline (2026-07-23, KMD 22.22.142.0)

- `DisplayHalf=1` exposes one connected child and one VidPn source. DWM composes
  the whole desktop on Helios and `SetVidPnSourceAddress` selects the real
  primary for `SET_SCANOUT_BLOB`.
- `ScanoutDiag` is **deleted/off**. Mode 16 remains a diagnostic only and must
  never overwrite the real primary during a desktop test.
- The LINEAR diagnostic image is proven on NVIDIA. The old failure was a guest
  constant bug (`VK_IMAGE_TILING_LINEAR` was encoded as `0`; it is `1`). After
  the fix, same-boot breadcrumbs reached `SdgLStg=0x10`, host-visible/coherent
  memory was selected, and the owner saw its fill pattern in VNC.
- The real DWM primary is a dedicated, DMA_BUF-exportable Venus
  `VK_IMAGE_TILING_OPTIMAL` allocation. The UMD marks the actual
  `CDD_SHAREDPRIMARYSURFACE`; the KMD uses that allocation in
  `SetVidPnSourceAddress`. There is no heuristic selection and no guest-side
  primary-to-scanout copy.
- The QEMU fork propagates virglrenderer DMA_BUF modifier metadata and the
  existing `RESOURCE_CREATE_BLOB.size` internally, without changing the public
  virtio-gpu wire ABI. Plain OPTIMAL exports currently arrive as
  `DRM_FORMAT_MOD_INVALID`; EGL cannot describe that layout. QEMU reconstructs
  the exact producer VkImage, verifies its Vulkan memory requirement equals the
  original blob allocation size, copies image-to-staging on the host GPU, and
  publishes a CPU `DisplaySurface` to VNC. This is direct guest-primary scanout,
  but **not end-to-end zero-copy** because the host display backend reads back.
- Visible desktop output is verified. A DComp scheduled-task probe completed
  1576 Presents in 25 seconds (63.0 fps), and interaction was responsive while
  that continuous producer ran. This isolated the perceived lag to the
  idle-to-active scanout edge, not steady-state GPU throughput. The UMD now
  emits a refresh marker after the exact DWM primary operation. KMD
  `DxgkDdiRender` captures the current Venus wire-fence watermark under the
  statically witnessed notification lock; the used-ring DPC coalesces markers
  and dirties scanout only after all preceding Venus work retires. This does not
  depend on VidSch choosing `SubmitCommand` versus `SubmitCommandVirtual`.
- The v142 wake test advanced the live 16-refresh telemetry snapshot
  (`AsSub`/`AsDone` caught up, `WtOut=CtOut=QfRet=0`). Same-boot QEMU evidence
  then rebound the real 1896x1030 OPTIMAL primary and completed Vulkan readback
  in about 1.0–1.9 ms. The owner confirmed excellent idle-to-active
  responsiveness.
- The KMD watermark orders Venus commands which already exist when the marker
  reaches `DxgkDdiRender`; it cannot cover work still queued on DXVK's
  submission thread. With `PresentGateUs=0`, fast cursor motion exposed that
  producer race as stale cursor replicas. A 5 ms A/B still leaked six stale
  frames in one 128-present burst, so the direct-primary default is now a
  bounded 10 ms `HeliosWaitFrameComplete` before the kernel present callback.
  It sleeps on DXVK's submission-fence condition variable instead of polling.
  The 10 ms A/B measured 0.48 ms cumulative average after 384 presents and
  zero timeouts after its six startup expirations. The owner confirmed both
  excellent responsiveness and no cursor ghosting.
- The old synchronous KMD `RESOURCE_FLUSH` control roundtrip is gone from the
  frame path. One interrupt-completed async bind/flush is allowed in flight and
  later flips coalesce. Control DMA buffers are reaped/reused outside the
  spinlock. Mesa's Windows ring notifies an idle renderer eagerly, folds
  side-effect-free wait-only timeline submits on the guest, and reuses its
  escape staging buffer; per-submit shape logging is opt-in.
- The same exact OPTIMAL Vulkan fallback is shared by `egl-headless`, GTK EGL,
  GTK GLArea, and SDL OpenGL. `egl-headless`+VNC and SDL OpenGL on native
  Wayland are visually verified. The launcher leaves interactive EGL vendor
  selection to the compositor while pinning Venus/readback Vulkan to NVIDIA.
  GTK/Wayland still fails during the full run with repeated GDK
  `eglMakeCurrent` errors and remains unverified.

### VidMm / Task Manager validation (2026-08-04, KMD 22.22.250.0 / 22.22.254.0)

- Task Manager's 4.0 GiB dedicated capacity is now backed by the configured
  `VidMmVramMB=4096` local segment while the CPU-visible aperture remains
  separately capped. A live SDL-window check showed `0.5/4.0 GB` dedicated,
  `0.0/6.0 GB` shared and `0.5/10.0 GB` total.
- Venus `VkDeviceMemory` tracking allocations follow the Vulkan memory heap:
  device-local allocations use the local non-aperture segment without becoming
  BAR-mappable, while non-device-local allocations use the aperture/shared
  segment. Direct KMT and native-Vulkan four-by-64 MiB probes each measured
  exactly `+256.00 MiB` in the selected segment, no movement in the other
  segment, and a return to baseline after destroy.
- Exportable DXVK/Venus memory initially had two full VidMm charges: its local
  `VkDeviceMemory` tracking allocation and the WDDM allocation that adopts the
  same renderer resource in the aperture. The adopted allocation is now an
  identity-only one-page VidMm object only when the current ICD positively
  attests that the full-size tracker exists; missing exports and tracker
  failures retain the safe full-size adopted charge. Its private open identity
  and KMD context retain the exact renderer size. Eight shared 64 MiB D3D11
  render targets consequently measured exactly `+512.00 MiB` local and only
  `+0.03 MiB` aperture (eight pages), then released both. An attempted
  local-segment placement for the adopted WDDM allocation was rejected: its
  first `CreateTexture2D` device-removed the UMD, so that policy never shipped.
- The `.249` hardware gate passed 12/12 direct-KMT cycles, 12/12 native-Vulkan
  cycles and 12/12 shared-D3D11 cycles. A 40-allocation Vulkan high-water test
  charged exactly 2560 MiB locally with no aperture movement, and four
  concurrent eight-allocation processes charged exactly 2048 MiB locally;
  DWM kept the same responsive process throughout.
- The `.250` heap-aware gate passed exact local and non-local direct-KMT tests,
  exact local and non-local native-Vulkan tests, and the eight-allocation D3D11
  adoption test above. A pre-tracking ICD retained one full shared charge; an
  older tracking ICD without the attestation export retained both its exact
  local tracker and one conservative full shared charge, proving the mixed
  deployment cannot under-report. Private export lookup is pinned to one ICD
  module so a missing old export cannot fall through to a newer DLL and receive
  a foreign Vulkan handle. The UMD build now watches every compiled bridge
  source and header, preventing incremental builds from silently reusing stale
  C++ objects. The installed signed package reports `22.22.250.0`, PnP status
  is Code 0, DWM stayed responsive, and no new display/PnP/WHEA/BugCheck
  critical or error events appeared.
- The `.254` follow-up closes the cross-process lifetime boundary. Each tracker
  is now a globally shared WDDM resource, its global KMT handle travels in a
  typed private allocation flag/open identity, and an importer opens the same
  tracker before returning the shared D3D resource. The KMD shrinks the adopted
  payload to one page only when the cookie names a live tracker whose size
  matches the KMD's recorded adopted-blob size; either mixed-version direction
  therefore keeps the conservative full payload charge. If the shared tracker
  disappears during an import race, Mesa creates a full-size tracker in the
  imported memory's actual heap. If that fallback also fails, the bridge
  rejects the D3D shared-resource open.
- The `.254` cross-process gate created and cleared a 4096x4096 shared D3D11
  texture in a child, opened it in the parent, exited the creator, and retained
  exactly `+64.00 MiB` in both adapter-global and importer-process dedicated
  counters. The importer then read the expected `ffff00ff` pixel and returned
  both counters to baseline after its device was destroyed. The both-open and
  creator-exited checks then passed 50 consecutive cycles. Raw KMT shared and
  two-process probes independently retained exactly `+128.00 MiB` and
  `+64.00 MiB`, respectively, after creator handle/process teardown and
  returned to baseline after the final close.
- With the final ICD loaded in DWM, an automated interactive Task Manager smoke
  left both processes responsive and produced no DWM/Task Manager error event.
  During validation, three deliberate PnP restart cycles still reproduced
  defect 0z in the pre-existing `vn_ring_load_head` teardown path (also present
  in the pre-branch ICD); DWM recovered each time. This branch does not claim to
  fix that separate adapter-removal race.
- **Re-gated after the merge, on the version that actually ships (2026-08-05).**
  The bullets above say `.254`; `kmd_render/driver-version.env` says
  **22.22.255.0** (the branch bumped 252 -> 255 directly), so read `.254` as the
  development build and `.255` as the shipped one. The merge also joined this
  branch to the ICD `HOST_CACHED` mapping fix, two changes that had never seen
  each other — the submodule conflict resolved to `e7ad5b238ec`, which strictly
  contains the branch's own `c3262452217`. Re-gated on the merged image:
  `d3d11_xproc_lifetime_probe` **PASS** (both-open and creator-exited each
  retained exactly `+64.00 MiB` adapter and process, pixel `ffff00ff` survived
  the creator's exit, exact return to baseline); `vidmm_tracking_probe` **PASS**
  in all four modes — local, `nonlocal`, `shared` (each exactly `+256.00 MiB`
  for 4x64 MiB) and the new `crossproc` (`+64.00 MiB` retained past creator
  exit). PnP `OK`/`CM_PROB_NONE`, desktop composites (screenshot), no
  display/Dxgkrnl/WHEA/BugCheck critical or error events since boot,
  `WdSigF`/`DmaNtfF`/`TxGone`/`RclBadH` all **0**, and `umd-gate-surface.ps1`
  reports `UMD GATE SURFACE CLEAN` with its must-not-appear set `all clear`.
- The Task Manager-triggered DWM abort was a mixed-source Mesa deployment: the
  installed ICD combined the old `vn_queue.c` with only four files from the
  newer VidMm work. Deploying one coherent Mesa `1a02ba9` image restored the
  imported-Win32-timeline path; Task Manager then stayed open with a stable DWM
  process. The separate, pre-existing PnP-restart DWM fault remains defect 0z.

## Current priorities

1. **DONE (2026-07-28) — the Phase-1 quality refactor of `kmd_render` and
   `umd` is COMPLETE.** Eleven tranches (T0, T1a, T1b, T2, T3, T4a, R614, T4b,
   T5, T6, T7, T8) from `REFACTOR_REVIEW.md`'s 300 findings / 177
   recommendations, every one landed and gated on hardware. Final image:
   **KMD 22.22.190.0 + UMD `DB343F02…`**, T8 gate passed on the 2026-07-28
   15:39:45 cold boot.

   **The tranche-by-tranche record — every gate result, every scope
   correction, every dropped item and its evidence — is
   `docs/archive/REFACTOR_TRANCHES_T0_T8.md`.** The review itself, its two
   kickoff prompts and the T7-crash brief are archived beside it. Code
   comments cite the review by NAME (`REFACTOR_REVIEW.md R802`); those
   citations still resolve, the same convention the other archived design docs
   use.

   Two directives from that work stay in force for all later changes: never
   fold a `BUG` fix into a structure move, and preserve the direct primary,
   completion ordering, loud-failure contracts, registry ABI and diagnostic
   names unless a reviewed change explicitly migrates them.

   **Owed, recorded with the measurements that justify deferring them** (see
   7m/7n in the archived record):
   - **R1103's `VirtioGpu` sub-structs.** `ResourceTables` is genuinely
     field-disjoint; `CtrlQueue`+`FenceTables` needs **six** method hoists on
     the completion path, not the three the review budgeted. Needs its own
     tranche and gate.
   - **R1108's vehicle-TLS sealing** — `take_present_source()` plus the four
     `dxgi_present` call sites that touch the cell.
   - **R1015** — whether the production surface ever takes the
     QUERYSEGMENT3/legacy paths. Needs a `DiagLevel=1` boot.
   - ~~The pre-existing **6-handles-per-device teardown leak** (7d(b))~~ —
     **CLOSED 2026-07-28**, root-caused and fixed. See the WS1 entry below.
   - **WS1 defect 0z** — `pnputil /restart-device` access-violates dwm,
     Explorer, SearchHost and ApplicationFrameHost inside
     `vulkan_virtio-*.dll`. Pre-existing, reproduced on every restart.
   - ~~**WS1 defect 0aa** — fullscreen scan-out pinned to ONE resource~~ —
     **ROOT-CAUSED AND FIXED 2026-07-29** (KMD 22.22.201.0), host-verified.
   - **WS1 defect 0ab — black-frame flashes. SPLIT IN TWO 2026-07-29, one half
     FIXED, one half OPEN.** First measured directly on the displayed surface
     (VNC RFB sampler + QEMU trace, both on the host clock) instead of inferred.
     - **0ab-A — the bind-edge RESOURCE_FLUSH was submission-ordered**, firing
       ~10 ms before the frame it named finished on the host, so the host read
       the frame's clear. **FIXED, KMD 22.22.206.0**: Fire Strike Combined
       (23 fps) unfinished displayed frames **22.0 % → 0.7 %**.
     - **0ab-B — at ~165 fps (GT1 fullscreen) the flashes REMAIN**: ~15 % of
       published frames are entirely black in EVERY configuration we own. Five
       mechanisms built, deployed, falsified; then a same-boot **2×2 factorial**
       (lease × BindFlushMode, 9 runs, 46 681 frames, 2026-07-29 evening) closed
       the whole ordering family WITH data: whole-flush black is 14.5–16.6 % in
       all four cells, and the knobs only move black between populations
       (bind-triggered first reads vs surplus refresh re-reads). **The mechanism
       is now PROVEN, not inferred**: the first read of a binding — the very
       event that ends its lease — finds the buffer already cleared 13–17 % of
       the time under a live lease gate, which no WDDM release chain can permit.
       The app's clear rides venus and never enters a DMA buffer, so the
       scheduler-side allocation sync that real flip-model relies on to defer it
       DOES NOT EXIST in this stack. The one variable that predicts black is
       bind→read age (<3 ms ⇒ 0.4–5.6 %; 6–12 ms ⇒ 34–60 %).
       **FIX SHIPPED — KMD 22.22.217.0 (owner-approved D1+D2+D3, 2026-07-29
       late evening): GT1 whole-flush black 14.5–16.6 % → 2.1 / 0.7 / 2.0 %**
       (age-standardised 2.2/0.9/2.0 — not an age-mix artifact), fps 169–186
       (UP: 25–33 % fewer synchronous host readbacks), Combined 0ab-A gate
       PASS (1.3 %, completion ordering intact), desktop 1:1 binds:flushes,
       Start menu opens, windowed-app coexistence verified, `WvTorn` 0.
       The win is the OWNERSHIP GATE (D2): the 34–49 %-black 2nd-read
       population (1090–1669/run) collapsed to 9–26; the 6–12 ms bucket kept
       its flush share but went 56 % → 0.5 % black — the wrong reads stopped
       being issued, not the timing. See the build-1 subsection below +
       `tmp/handoff-0ab-b-lease/analysis/build1-results.md`.
       **OWNER-CONFIRMED BY EYE 2026-07-29 late night: GT1 visually clean,
       overall Fire Strike >25k (was ~20k). 0ab-B's main population is
       CLOSED.**
     - **0ab-C — residual black-frame stuttering in GRAPHICS TEST 2 at
       ~210 fps. CLASSIFIED 2026-07-29/30: the first-publish bind-edge margin
       race (population (a)), the exact population build 1 left open.** Two
       oracle GT2 runs on .217: whole-flush black 7.3 %/6.0 % (GT1 post-fix
       0.7–2.1 %), all first reads at 1–3 ms bind age; the ownership gate
       holds unchanged (6–12 ms bucket 0.2–0.4 %, rereads ~1 %). Guest half:
       worker bind cadence bimodal (1–3 ms vs 10–14 ms stall modes),
       `BeOvw` ×~30 GT1's rate. Minorities: 0ad's transition window
       (~12–23 %), coalesce-holds (dup 3–5 %). **Fix arc = the D1(ii)
       DISPATCH-bind family, four builds in one night**: .218 bugchecked (a
       PRE-EXISTING `wait_block` TOCTOU the new load armed — root-caused
       from dumps, fixed in .219, three clean batteries since); .219 halved
       GT2 black (4.0/3.4 %); .220/.221 closed the fast-path coverage gap to
       99 % and thereby PROVED the GT2 residual is not bind timing (x = y;
       0/439 black at 0–1 ms bind age — the venus-executed clear lands in
       the READ window). **GT1's residual was eliminated outright
       (1.9 → 0.3 %, best recorded). SHIPPING: 22.22.221.0. GT2 residual
       ~3.5–4 % needs D4 (venus acquire, owner-gated). 0ab-C = reduced, not
       closed; owner's eye pending.** Corpus:
       `tmp/handoff-0ab-c-gt2/analysis/{CLASSIFICATION,FIX-DESIGN-d1ii,BUGCHECK-0xA-218,build219-results,build220-results,build221-results}.md`.

   ⚠ **One standing gate line remains NOT OBTAINABLE on this box** and should
   not be retried as written: **suspend/resume** (`powercfg /a` reports every
   sleep state unsupported by the VM firmware — which also means the
   same-context PnP stop/start carry-over path, `StRst`/`RfUnb`, can never be
   provoked here). The other one — **same-boot QEMU scanout evidence** — is
   RESOLVED: since 2026-07-29 the VM runs `HELIOS_DISPLAY=egl-vnc` and the
   per-flush oracle (`tools/qmp_trace.py` + `tools/scanout_oracle_report.py`)
   provides it routinely; verify with `/proc/<qemu>/cmdline` before relying
   on it.


2. Continue soaking the current direct-primary path across DWM buffer rotation,
   resize, device restart and cold boot. **Suspend/resume is struck from this
   list**: `powercfg /a` on this VM reports S1, S2, S3, hibernate and S0ix all
   unsupported by the firmware, so it is untestable here until the machine type
   changes — and with it, the same-context PnP stop/start carry-over path
   (`StRst`, `RfUnb`) has no way to be provoked on this box at all.
3. Pursue true host zero-copy only with a layout contract the display importer
   can consume. An explicit DRM modifier is one possible route, but enabling the
   modifier/DMA_BUF extensions on every DXVK device is prohibited: it inflated
   ordinary shared OPTIMAL import requirements and caused valid undersized-import
   refusal, DWM failures, and NVIDIA Xid 31 when bypassed.
4. Continue D3D11 stability and conformance work now that the quality pass is done.

## Historical PSC workstreams

The dated IDD/Looking Glass investigations below explain how the display pivot
was reached. They are historical evidence, not descriptions of the active
display architecture, and are superseded by the baseline above wherever they
conflict.

1. **D3D11 windowed apps render transparent — investigate & fix.** Windowed
   D3D11 swapchains (FaceWorks, Fire Strike windowed) show a transparent/black
   client area even when placed on-screen at the right size, while the desktop
   and window frames composite fine. Established this session: the app DOES
   render correct content (`HELIOS_PRESENT_READBACK` source non-black at
   1264×681); it is NOT alpha (`HELIOS_PRESENT_FORCE_OPAQUE` no-op, owner-
   confirmed); it is NOT a two-memory split (the KMD adopt path backs the alloc
   with the DXVK venus image — an earlier "KMD zeroes the resid" reading was a
   UMD struct-layout misread, see below); and it is NOT the IDD (both the D3D11
   fallback and the dead D3D12 path capture the same single composed IddCx
   surface — D3D12 is dead only because our UMD has no D3D12). The live thread:
   **DXGI `EnumOutputs`/`GetDisplayModeList` racily return `0x887a0022`** on the
   Helios adapters, DWM never imports the app's flip backbuffer (its max
   imported resid trails the app's). **ROOT-CAUSED 2026-07-08 (34th) — see
   `WINDOWED_BLT_DESIGN.md` (full design + implementation plan) and memory
   `windowed-blt-occluded-root-34th-session`.** It is specifically the legacy
   **`DXGI_SWAP_EFFECT_DISCARD` (BLT) swap model** returning `DXGI_STATUS_OCCLUDED`;
   **flip composites fine** (proven with `tools/d3d11_triangle.cpp`). Falsified:
   alpha, IDD, two-memory-split, phantom-LUID/EnumOutputs, adapter selection, AND
   the cross-adapter cap (present is same-adapter, not cross). Real cause: a legacy
   DISCARD windowed present needs a **real active VidPn output** in the path; ours
   has none — the only monitor is the indirect IddCx one, enumerated on Helios's
   **runtime-synthesized *facade* output** (`NumOfSources=0`), and there is no real
   display adapter to cross-adapter to. FIX = give Helios a **real (virtual) VidPn
   source** (RDP/display-miniport model) so DISCARD resolves a real output; do the
   **Stage-0 WARP A/B first** (WINDOWED_BLT_DESIGN.md §6). The "two Helios adapters"
   are the Helios render adapter + the Looking Glass IddCx adapter (which inherits
   the render adapter's name) — NOT stale residue.
   **STAGE 0 DONE + STAGE 1 IMPLEMENTED (2026-07-08, 35th):** Stage 0 validated
   Option A on-screen (disable Helios → WARP presentable → BLT composites). §6.3
   resolved from MS docs — a VidPn source+target+monitor must be same-adapter, so
   Helios gets its OWN 2nd (virtual, no-scanout) monitor (owner-approved; IDD
   renders unchanged, monitor unobserved). **Built KMD v22.22.63.0** with the full
   display half behind a `DisplayHalf` REG_DWORD knob (default 0 = today's
   render-only surface): `start_device` sources/children=1 + child DDIs +
   GetChildContainerId; new `ddi/vidpn.rs` (viogpudo-style
   EnumVidPnCofuncModality/RecommendMonitorModes, single 1920x1080@60); VidPn DDIs
   in `display.rs` (IsSupportedVidPn=TRUE, RecommendFunctionalVidPn=NO_RECOMMENDED,
   Commit/SetAddr/SetVisibility=SUCCESS no-op scanout). Compiles + signed; NOT yet
   installed. **NEXT: owner install (reboot) → `DisplayHalf`=1 + `pnputil
   /restart-device` → BLT triangle un-occlusion test** (WINDOWED_BLT_DESIGN.md §9,
   memory `windowed-blt-display-half-implemented-35th`). Honest caveat: docs don't
   tie OCCLUDED to a VidPn source — the knob A/B is the arbiter.

2. **Slow first-paint on some windows — our UMD makes DWM wait.** Settings app,
   parts of Explorer on fresh open, and (easiest repro) the **UAC dimmed
   window** take several seconds to render. Suspected a UMD-side present/consumer
   wait or a per-window gate stalling DWM's first composition of these surfaces.
   Likely related to #1's consumer/import path. NEXT: measure — instrument the
   present-wait / gate-flush / consumer-wait counters against a UAC-window repro,
   find which wait blocks and why it only bites first-paint.

3. **Codebase cleanup (HIGH).** Many paths accreted across bring-up sessions add
   overhead or cause minor misbehaviours: retired diagnostic scaffolding, dead
   knobs, superseded present/staging paths, force-* diagnostics, staged-probe
   machinery, and now-falsified experiments (e.g. the `DECLARE_CROSS_ADAPTER_RESOURCE`
   line, broad-adopted-BAR remnants). Audit the UMD present path, dxvk-helios
   staging/refresh layers, and KMD segment/adopt code; delete or gate what is not
   load-bearing, with before/after behaviour verified. Do this before large new
   feature work so #1/#2/#4 land on a clean base.

4. **Performance — Fire Strike fullscreen (~100 fps @1080p, GT1).** Owner
   believes near-2× is reachable. Render path is healthy (fullscreen renders
   correctly). Measure first (venus submit/fence latency, copy/acquire gates,
   present-to-scanout) then remove known costs. See WS2 for the levers already
   mapped (feedback-shadow retire, dcomp vehicle, copy-latency).

## Fullscreen scan-out — 0aa FIXED, 0ab STILL OPEN (2026-07-29, KMD 22.22.201.0)

⚠ **0aa is fixed and host-verified; the owner still sees black-frame FLASHES
(defect 0ab).** Do not read the two fixes below as closing the visible
artifact. What they closed is measurable and closed; what remains has a
different shape (brief, frequent flashes vs a lasting stale frame) and is
recorded at the end of this section.


**Symptom.** With a fullscreen D3D11 app, the guest published ONE scan-out
resource to the host for the whole run, at the app's frame rate, with ZERO
`SET_SCANOUT_BLOB`; the desktop before and after rotated a three-buffer chain
with a blob per flip. Owner saw black frames, "some for longer". Five sessions
of inference produced four wrong mechanisms; one run of an UNSAMPLED instrument
named the cause.

**The instrument comes first.** `kmd_render/src/ddi/scanout_trace.rs` separates
accumulation (unsampled, atomics-only, legal at any IRQL, every call recorded)
from publication (one throttled PASSIVE dump). Read it with
`tmp/perf/scanout-trace.ps1`; run a workload with it via
`tmp/perf/run-gt1-trace.ps1`. Host side: `virtio_gpu_cmd_*` over QMP
(`/tmp/helios-tpm/mon.sock`) — see the recipe in the 57th-session memory.
Every `Sc*`/`Rf*`/`PB*` value is SAMPLED (1st + every 600th) and registry values
persist across boots; that is what produced the four wrong mechanisms.

**Root cause 1 — the bind was never asked to move.** `DXGK_FLIPCAPS` advertised
`FlipOnVSyncMmIo` only, which covers nonzero-interval flips. Measured: DWM
presents at `FlipInterval=1` with `pDmaBuffer == NULL` (the MMIO contract,
which dxgkrnl completes through `SetVidPnSourceAddress`); a fullscreen app
presents at `FlipInterval=0` (IMMEDIATE) with a DMA buffer — the DMA-buffer
flip contract, where the driver must program the display itself, and which
`SetVidPnSourceAddress` never follows. `DXGKARG_PRESENT.Flags` is IDENTICAL
(`Flip|FlipWithNoWait`) in both cases, so the flags are not the discriminator —
the flip interval is. 839 flips in one run, 0 MMIO, 0 blobs,
`SetVidPnSourceAddress` silent for 36 s. **Fix: also advertise
`FlipImmediateMmIo`** — honest, because a Helios flip IS a `SET_SCANOUT_BLOB`
and has no vblank to wait for. `FlipCapsX` (service key, default 0) overrides
the whole word; `FlipCapsX=2` restores the old advertisement for an A/B.

**Root cause 2 — a bind is itself a dirty edge.** `SET_SCANOUT_BLOB` changes
which resource the host reads; it does not make the host read it. The zero-copy
arm deliberately produced no dirty edge ("the matching Render marker and
used-ring retirement are the sole producers"), which was true only while the
bind never moved. Once it rotated per flip, a freshly bound buffer sat on screen
unread. QMP-measured bind→same-resource-flush latency over a full run:

| KMD | binds | p50 | p90 | max | ≥20 ms |
|---|---|---|---|---|---|
| 22.22.200.0 | 793 | 0.5 ms | 100.2 ms | 2634 ms | 265 (**33.4 %**) |
| 22.22.201.0 | 829 | 0.3 ms | 0.6 ms | 81.8 ms | 2 (0.2 %) |

**Fix: request a refresh for the exact resource after a bind that changed the
binding.** Fire Strike Combined 23.1 → 25.5.

**FALSIFIED here, with full-coverage measurements — do not re-propose.**
- *The bind races the app's rendering.* The per-buffer watermark census
  (`Bw`) is `CONTENT_TRACKED` on 425/425 binds and `CONTENT_PENDING` on ZERO.
  dxgkrnl issues the flip on the app's DMA fence, which `DmaGpuFence=1` already
  retires on host GPU completion, so the buffer is always complete at bind time.
  The `BindWait` gate that came out of this hypothesis is LANDED but inert
  (`ScNotRdy` ~1/boot, `ScBForce`=0); keep or delete deliberately.
- *A stuck programming gate / dead VSync heartbeat.* `VpGate`=0 and `VpVsN`
  advancing at ~60/s throughout the stall.
- *`SetVidPnSourceAddress` binds are coalesced to ~1.5/s* (an in-tree code
  comment). Measured at ~64/s with `VpCoal`=0 on the desktop.

### NOT a defect — the one-shot `scanout_dmabuf` import failure

`/tmp/helios-qemu-stderr.log` shows `sdl2_gl_scanout_dmabuf: failed` with
`OPTIMAL DMA-BUF shape mismatch required=8773632 fd_size=7913472`, and 229 of
229 attempts fail. It looks alarming and IS NOT A DEFECT: owner-verified
2026-07-29, it fires ONCE PER RUN at startup and every subsequent
`SET_SCANOUT_BLOB` is fine. It is a one-shot capability probe that falls back
to the working blob path. Recorded here so the 100 %-failure ratio is not
"discovered" again — the ratio is over ATTEMPTS, and there are only one or two
attempts per boot.

### Defect 0ab — 0ab-A FIXED 2026-07-29, 0ab-B STILL OPEN

**Shipping state: KMD 22.22.209.0** = 0ab-A only. 22.22.207.0 and .208.0 were
the two falsified 0ab-B attempts below and are fully reverted; .209.0 is .206.0
plus nothing.

**OWNER-CONFIRMED on .209.0, full Fire Strike suite (2026-07-29)** — and the
result is the sharpest constraint we have on what is left:

| test | presents | owner verdict |
|---|---|---|
| Graphics Test 1 (~165 fps) | high | **black-frame stutter** |
| Graphics Test 2 (high fps) | high | **black-frame stutter** |
| Physics | none (CPU) | clean |
| Combined (~23 fps) | low | **clean** |

Score 20k overall / 40k graphics / 4k combined — **unchanged**, so 0ab-A cost
nothing. This confirms both halves of the instrument's reading: 0ab-A is fixed
(Combined went 22.0 % → 0.7 % unfinished frames and the owner now sees it
clean), and **0ab-B scales with FRAME RATE, not with workload**. Physics is the
control: no presents, no artifact.

**The instrument came first, and it is the reusable part.** Five sessions
argued about 0ab from guest counters. What settled it was watching the thing
itself: `tools/vnc_frame_probe.py` samples QEMU's VNC surface at ~30/s and
stamps each frame with `time.time()`, which is the SAME CLOCK as the
`virtio_gpu_cmd_*` trace lines the QEMU `log` backend writes; and it scores each
frame with a **completeness oracle** — the mean brightness of 3DMark's fps bar,
which is present in every finished frame and absent in every unfinished one.
Whole-frame brightness cannot tell "dark scene" from "unfinished frame"; the
oracle can. `tools/vnc_scanout_correlate.py` joins the two.

⚠ Two traps in the probe, each of which cost a cycle: sending RFB
`SetPixelFormat` makes QEMU stop answering FramebufferUpdateRequests entirely
(its native format is already the one you want), and writing PNGs inside the
sample loop throttles it to 3/s — which is coarser than the artifact and
silently biases the sample.

⚠ `screendump` is NOT an alternative, under `sdl,gl=on` OR under `egl-vnc`: the
console's `scanout.kind` is DMABUF, so `qemu_console_surface()` returns NULL and
QMP answers `"no surface"` even though the VNC path is happily reading a live
surface. That is why the RFB client exists.

**0ab-A root cause: the bind-edge RESOURCE_FLUSH was submission-ordered, so the
host read the frame's CLEAR.**

`program_vidpn_source_inner` fired `request_scanout_refresh_for(target)`
immediately after every SET_SCANOUT_BLOB that changed the binding — the 0aa
"a bind is itself a dirty edge" fix. Under the DMA-buffer flip contract that is
~10 ms too early: `arm_dma_flip` runs in `DxgkDdiSubmitCommand` **with the
flip's DMA fence still outstanding** (deliberately — that is the contract), and
the app's real work never travels in that DMA buffer at all; it goes to the host
over the Venus escape channel. So at bind time the frame is SUBMITTED, not
COMPLETE. A RESOURCE_FLUSH is the host's instruction to READ, and what it read
was the frame's own clear — hence a fully black frame, never a partial one.

The `425 of 425 binds found the buffer's watermark already retired` claim the
arm shipped with was measured on the MMIO contract, where dxgkrnl retired the
flip BEFORE calling `SetVidPnSourceAddress`. It did not survive the move to the
DMA contract, and nothing re-measured it.

**Measured on what is actually DISPLAYED**, Fire Strike Combined, one QEMU
`virtio_gpu_cmd_*` trace over QMP correlated with an RFB sampler on the VNC
surface (`tools/vnc_frame_probe.py` + `tools/vnc_scanout_correlate.py`), using
3DMark's fps bar as a "did this frame finish?" oracle:

| | 22.22.205.0 | 22.22.206.0 |
|---|---|---|
| displayed frames that are UNFINISHED | **98/445 = 22.0 %** | **3/431 = 0.7 %** |
| …and entirely black | 98 of 98 | 3 of 3 |
| binds whose flush was within 2 ms of the bind | 465/475 = 98 % | 60/521 = 12 % |
| bind → first flush of that resource, p50 | **0.2 ms** | **10.4 ms** |
| Combined score | 23.06 | 23.14 |

10.4 ms is the app's own GPU frame time — the flush now lands when the frame
lands. Guest-side, the same thing reads as `BeDef` 3096 vs `BeRdy` 1478 over
4576 binds: **two thirds of binds under a fullscreen DMA-flip workload had
outstanding Venus work at bind time**, i.e. two thirds of them used to publish a
half-drawn buffer. GT1 180.19 vs 183.06 — inside the run-to-run band.

**The fix is an ordering, not a stall.** The bind edge now arms through the
Venus watermark (`AdapterContext::arm_completion_ordered_refresh` →
`VirtioGpu::note_scanout_refresh`), so the flush is issued from the completion
DPC by `take_ready_scanout_refresh`. No CPU thread waits anywhere, which is
exactly why the deleted producer-side gate was the wrong shape for this. It also
keeps everything the bind edge exists for: a bind that changed the binding is
still guaranteed a flush naming its own resource (defect 0aa stays fixed).

**Residual inside 0ab-A, named rather than hand-waved.** ~12 % of binds still
find the watermark already retired and flush immediately (`BeRdy`), and one of
those produced the single remaining black frame in a 21 s Combined window.
`note_scanout_refresh` samples `next_wire_fence` at the call, so "everything
below it has retired" is trivially true if the frame's Venus commands have not
reached the virtio ring yet. Closing that needs the watermark captured at flip
SUBMISSION and carried in `PresentFlipPrivate`, not sampled at bind time.

### 0ab-B — at ~180 fps the flashes REMAIN (OPEN)

**Do not read 0ab-A's numbers as closing the visible artifact.** The same probe
on Fire Strike **GT1 fullscreen (~180 fps)**, KMD 22.22.206.0:

| | GT1 after 0ab-A |
|---|---|
| displayed frames entirely black | **157/856 = 18.3 %** (sampler 29.5/s) |
| binds whose flush was within 2 ms | 1595/4409 = 36 % |
| bind → first flush, p50 / p90 | 0.9 ms / 10.3 ms |
| frame sampled AFTER the completion-ordered flush | 41/249 = **16 %** unfinished |

The bind-edge fix DID take effect here (98 % → 36 % immediate flushes), and the
black frames did not go away — so the early read is not what produces them at
this frame rate. The 16 % figure is the load-bearing one: the buffer is
unfinished *after* a flush that is correctly ordered on its content.

Per-run counter deltas over that GT1 run (deltas, not absolutes — registry
values persist across boots):

    VpBind +5015   VpSkip +1046 (21 % already-bound)   VpCoal +310
    MkTot  +5645   MkBound +824 = 14.6 % of presents
    BeRdy  +1875   BeDef  +3139

`MkBound` at **14.6 %** is write-while-displayed: the app finished writing the
buffer that was, at that instant, the bound scan-out. That is the previous
session's mechanism, which the DMA-flip contract drove to 0.45 % on Combined and
which is plainly back at 180 fps, alongside `VpCoal`/`VpSkip` at ~20 % of binds.
The app rotates only TWO scan-out resources in fullscreen (host trace: strict
A,B,A,B alternation, zero consecutive same-resource binds), so a bind that lags
one flip leaves the display pointed at the buffer the app has just been handed
back and cleared.

#### 0ab-B: two mechanisms BUILT, DEPLOYED, MEASURED, and FALSIFIED

Both were implemented in full, installed, and measured against the same probe.
Neither moved the artifact. **Both are reverted; do not re-propose either
without new evidence.** The code is gone, this record is the point.

| attempt | what it did | result |
|---|---|---|
| **Flip-fence ordering** (was 22.22.207.0) | Held a DMA-buffer flip's `DXGK_INTERRUPT_DMA_COMPLETED` until that flip's own SET_SCANOUT_BLOB had completed, via a monotonic ticket minted in `arm_dma_flip_programming` and released by the display worker. Premise: completing the flip fence on Venus retirement alone lets dxgkrnl recycle a still-displayed buffer. | Gate demonstrably live — **4844 of 5028 flips held** — and it did what it claimed: `VpSkip` 1046 → 274, no fps cost (GT1 162.0 vs 164.7). **Black frames unchanged: 21.2 % → 21.6 %.** |
| **Dirty-edge identity gate** (was 22.22.208.0) | Dropped a refresh whose armed resource was not the bound one, *provided the bound one had already been flushed once* (a binding epoch, so defect 0aa stays fixed). Premise: a marker armed for frame B was re-reading buffer A. | Desktop stayed healthy (61 binds / 61 flushes over 3 s of mouse movement, so the epoch qualifier did fix the 2026-07-28 delivery collapse). But the gate fired only ~128 times per run. **Black frames unchanged: 18.0 %.** |

**What the falsification actually taught us**, and it is the useful part:

* A fence census over one GT1 run (`virtio_gpu_fence_ctrl` type 0x207 →
  `fence_resp`, joined to the flushes) found the app had **NO GPU work in
  flight for 94 % of flushes**, and the dark rate was **identical (19 %)**
  whether 0, 1 or ≥2 submissions were outstanding. So the host is not reading a
  buffer mid-render: the buffer is quiescent and genuinely contains a clear.
* The dark rate is **FLAT against every timing variable** — age of the current
  binding (21/16/18/14/16/29 % across <3…≥20 ms), time to the next bind
  (18/20/16/16/17 %), time since the last flush (22/21/18/19/11/11 %). A race
  against our bind/flush cadence would show a gradient. There is none.
* The flashes are **163 isolated single-frame events in 31 s** (~5/s) plus the
  two workload-transition fades; run-length is 1 for all of them.
* Fullscreen rotates exactly **two** scan-out resources in strict A,B,A,B order
  (**zero** consecutive same-resource binds in 4418), so this is not the
  desktop primary being interleaved and not a lost rotation at our level.

**Where that leaves 0ab-B.** The remaining shape is "the guest's own presented
buffer contains a clear", which points UPSTREAM of the scan-out path — at the
UMD/DXVK swapchain (`dxgi_rotate_resource_identities`/`rotate_ring`, and DXVK's
reuse of a presented image) rather than at the KMD. Note the standing constraint
that the app's CONTENT is correct was established with 3DMark's offline frame
output, which does **not** exercise the swapchain rotation — so it does not
cover this. ⚠ Also note the sampler tops out at ~30/s while GT1 presents at
~165/s, so it cannot resolve an individual presented frame there; the next
instrument needs either a slower workload or a guest-side timestamped trace.

**Also still true and still not the cause** (kept because each cost a session):
- The host runs `-display sdl,gl=on` or `egl-vnc`; the artifact reproduced on
  BOTH, which is what ruled the backend out. Under `egl-vnc` QMP `screendump`
  still answers `"no surface"` (the console is in DMABUF scanout kind), so the
  RFB path is the only way to see the surface — hence the probe.
- `VpCoal` and the burst bind pacing are real and unexplained, and are NOT this.

**RESULT — producer-side ordering is NOT the cause, and the old ESTABLISHED #1
is now wrong.** Owner ran `PresentOrder=0` + `PresentGateUs=200000` (a
completion gate that cannot expire) on top of the 0aa fixes: **black frames
still flash, only less frequently.** The handoff recorded that config as
"visually clean"; it is not. A producer-side stall REDUCES the artifact — which
is why it read as clean at lower frequency — but does not remove it, so
whatever is producing 0ab survives the app's own GPU work being finished before
the present is published.

⛔ **The `PresentGateUs` and `PresentOrder` knobs were DELETED by owner
directive (2026-07-29) and must not be reintroduced.** They were the
producer-side CPU present gate. It is a hack in both directions: on expiry it
publishes the present with work still outstanding (the exact thing it exists to
prevent), and when it does hold it removes all CPU/GPU overlap (Fire Strike GT1
158 → 136). The measurement above is the last word on it — it does not even fix
what it was being kept for. The ordinary present path is now unconditionally
SUBMITTED-ordered with no knob; the vehicle path keeps its own
`VehicleFlipGateUs` COMPLETE wait, which answers a different question (ICD
image RECYCLE, not present ordering). See the ⛔ note in `umd/src/knobs.rs`.

**MECHANISM MEASURED 2026-07-29 (KMD 22.22.203.0).** `MkBound` counts present
markers whose resource was, at that instant, the BOUND scan-out — i.e. the app
finished writing the buffer the host was displaying. Over one Combined run:
**145 of 1245 markers (11.6 %)**, and the per-window deltas correlate EXACTLY
with dropped binds:

| window | ΔMkBound | ΔVpCoal |
|---|---|---|
| desktop 0–10.9 s | 0 | 0 |
| 11.8–16.6 s (app start) | 19, 22, 36, 18 | 2, 6, 27, 9 |
| 18.7–22.6 s | 0 | 0 |
| 26.7–29.1 s | 11, 17, 15, 5 | 10, 11, 12, 3 |
| 31.8–46.7 s | 0 | 0 |

Run totals `coalesced=80`, `alreadyBound=82` — tracking 1:1. Reading: a dropped
pending bind makes the NEXT bind find the same buffer already bound, so nothing
is issued and the display stays on a buffer the app then overwrites.

**Why binds are dropped at all is the load-bearing question.** With
`MaxQueuedFlipOnVSync = 1`, dxgkrnl should not issue a second
`SetVidPnSourceAddress` until the first flip has retired — which requires our
CRTC_VSYNC to carry its address, which requires us to have bound it. Two
pendings therefore should never coexist, yet `VpCoal` is 80. The implication is
that **dxgkrnl retires an IMMEDIATE flip on the DDI's RETURN, not on
CRTC_VSYNC** — and our DIRQL half returns STATUS_SUCCESS having only stashed the
handle for a PASSIVE worker. That makes the success return a lie for exactly the
flip class `FlipImmediateMmIo` opted us into: dxgkrnl frees the previous buffer
to the app immediately, then issues the next flip, while we have programmed
nothing.

**CONFIRMED, AND THE DMA-BUFFER FLIP CONTRACT IS IMPLEMENTED (KMD
22.22.205.0).** `FlipImmediateMmIo` is withdrawn; `ddi/present_packet.rs`'s
`PresentFlipPrivate` carries the flip's allocation + `DXGK_ALLOCATIONLIST`
physical address in the kernel-only DMA private data, and `submit_command` arms
the scan-out programming from there while the flip's DMA fence is still
outstanding. Two supporting pieces were needed:

* `create_allocation::SCANOUT_ALLOCS` — Present holds only
  `hDeviceSpecificAllocation`, the scan-out path keys on the GLOBAL
  `AllocationContext*`, and NOTHING bridges them (`DXGK_OPENALLOCATIONINFO`
  carries a `D3DKMT_HANDLE`, and the create-time private data is UMD-visible so
  a kernel pointer must not travel through it). A 32-slot registry keyed by
  venus resource id, populated for direct-scan-out allocations only, is the
  bridge. Measured before it existed: `VpDmaF=165, VpDmaA=0, VpPrF=165`.
* `PresentFlipPrivate::take` CONSUMES the record, because dxgkrnl recycles DMA
  private-data buffers and a left-behind record would re-arm a bind for a stale
  allocation.

Result on the same Combined workload:

| | binds | coalesced | alreadyBound | write-while-displayed |
|---|---|---|---|---|
| MMIO immediate (22.22.203.0) | 1034 | 80 | 82 | **145 / 1245 = 11.6 %** |
| DMA flip (22.22.205.0) | 1272 | **2** | **6** | **6 / 1327 = 0.45 %** |

`VpDmaF=946, VpDmaA=946` — every immediate flip programs the scan-out.
`ScAlcFul=0` (registry never overflowed), `VpPrF=0`, `ScSetErr=0`, `RfFail=0`.
Combined 23.06, inside the run-to-run band. **Owner visual confirmation is still
outstanding — the guest census is not the artifact.**

**The reasoning that got here, kept because it is the reusable part** (an ordered pending QUEUE fixes lost flips but not a lag whose
bound is "whenever the worker runs"; implementing the DMA-buffer flip contract
puts completion back under driver control via the DMA fence, which is what that
contract exists for). The confirming measurement is a DIRQL-side record of
whether `last_primary_address` had already advanced to the previous pending
handle's address when the next `SetVidPnSourceAddress` arrives.

**FALSIFIED WITH EVIDENCE 2026-07-29 — do not re-propose:**
- *The app's rendered CONTENT is black.* Owner ran 3DMark's frame-output (image
  quality) dump: no black frames in the output, and none visible while it ran.
  Note that frame output renders scenes offline and does NOT exercise the
  display/scan-out path, so it is a content oracle only — which is exactly what
  makes it decisive here. **The defect is strictly in what we DISPLAY.**
- *`dxgi_present1`'s multi arm fails to copy src->dst.* CORRECT as written:
  `DXGI_DDI_ARG_PRESENT1` documents that when many resources are presented
  `hDstResource` is NULL and the driver must translate only the LAST source
  handle for `pfnPresentCb`. There is no destination to populate.
- *`BltDXGI` leaves the DWM shared surface / full-screen PROXY surface empty.*
  The `dxgi-presentation-path` doc makes this the obvious suspect — both the
  windowed shared surface and the full-screen proxy are filled by `BltDXGI`,
  and our `dxgi_blt1` refuses CONVERT/STRETCH outright while the caps advertise
  16x stretch. It is nevertheless NOT the cause: **`DXGI Blt` appears 0 times in
  every UMD log.** Modern flip-model swapchains bypass the Blt path entirely.
  (The unbacked stretch/convert caps remain a separate honesty problem.)
- *A mid-run scan-out DISABLE blanks the screen.* `set_scanout_blob res 0x0`
  appears ONCE in the entire host log.
- *DWM keeps binding the scan-out during a fullscreen run, alternating with the
  app.* DWM's ids freeze the moment the workload starts (`Vs` 36/39/40 stop at
  86/85/85) and its flushes stop with them.

**Older suspects, now subordinate to the above:**
1. **Bind lag / burst pacing.** `VpCoal` ~85 per run, and binds arriving two
   3 ms apart then a 40 ms gap against a 25 fps app. A late bind can point the
   display at a buffer dxgkrnl has already recycled to the app — and with
   `gl=on` the app's clear-to-black is then on screen live.
2. **The programming gate's VSync suppression** as the cause of that burst
   pacing: `vsync_dpc_routine` early-returns while `vidpn_programming` is
   raised, so dxgkrnl cannot retire, flips queue, and the queue drains in a
   burst when the gate drops.
3. The consumer half of `publish_present_order`
   (`Global\HeliosPresentFence_<pid>_<id>`) still has no consumer — a GPU-side
   wait is the non-hack form of the ordering the deleted gate was faking.

#### The QEMU-side scan-out oracle (built 2026-07-29, needs a VM relaunch)

Every 0ab-B conclusion so far is statistical, because `tools/vnc_frame_probe.py`
samples at ~30/s while GT1 flushes at ~142/s. The owner authorised working in
the QEMU scan-out path, so the oracle now lives where the pixels are — inside
the flush itself, one line per displayed frame, no sampling:

| trace event | site | what it settles |
|---|---|---|
| `helios_scanout_blob_layout` | `virgl_cmd_set_scanout_blob` | the guest's own view: fd, blob size, `offsets[0]`, computed `fb.offset`, stride. ⚠ `virtio_gpu_create_dmabuf` builds every `QemuDmaBuf` at **offset 0** and drops `fb.offset`; a nonzero guest offset would mean the host reads the wrong bytes, so it is now visible rather than assumed away. |
| `helios_scanout_bind` | `egl_scanout_dmabuf` | per bind: resource id → DMA-BUF inode/size/backing/stride, which readback path took it (`vk-optimal` / `vk-linear` / `cpu-mmap` / `egl-texture`), and whether the readback cache reused an entry. Two resource ids sharing one inode = aliased buffers. |
| `helios_scanout_read` | `egl_scanout_flush` | per FLUSH: `bound_ino` (what the guest has bound, fstat'd now) vs `read_ino` (what the active readback actually imported), the flush rect, and a content verdict over the surface the VNC encoder is about to read — sampled every 4th pixel: `sampled`/`nonzero`/`max` plus an FNV-1a `csum` that separates new content from a re-read. |

`nonzero == 0` **is** the black-frame flash, decided on the exact pixels that go
out. Enable with `tools/qmp_trace.py on helios_scanout_read helios_scanout_bind
helios_scanout_blob_layout`; the lines land in `/tmp/helios-qemu-stderr.log`
with the same ISO8601 UTC prefix as the `virtio_gpu_cmd_*` traces. Report:
`tools/scanout_oracle_report.py /tmp/helios-qemu-stderr.log <label>`.

Zero cost when the events are off (both emitters return on
`trace_event_get_state_backends`), and the only behavioural change in QEMU is
that a `QemuDmaBuf` now carries its producer's id (`qemu_dmabuf_set_source_id`).

#### 0ab-B ANSWERED (2026-07-29, KMD 22.22.209.0, GT1 183.6 fps, 54 s, 2965 flushes)

One GT1 run through the oracle. **The host reads the right buffer; the guest's
buffer contains a clear.** Hypothesis (B) is dead, and it died on identity, not
on argument:

| oracle question | answer |
|---|---|
| are the two rotating resources distinct memory? | **yes** — res 191 ino 27926 fd 423, res 195 ino 27930 fd 303, both 4587520 B, `guest_offset` 0, `fb_offset` 0. No aliasing. |
| did any flush read a buffer other than the bound one? | **0 of 2965.** `read_ino == bound_ino` every time. |
| what fraction of PUBLISHED frames are entirely black? | **619 / 2965 = 20.9 %** (res 191 20.4 %, res 195 22.0 %) — and that independently reproduces the 30/s VNC sampler's 18–21 %, on a per-flush instrument. |

**And the artifact has a sharp timing structure the sampler could not see.** The
previous session's "no timing gradient — the dark rate is flat against binding
age" was an artifact of sampling at 30/s a thing that happens at 90 flushes/s.
Per flush, the bind→flush latency is **bimodal**, and the black frames live
entirely in the late mode:

| bind → flush | flushes | black | |
|---|---|---|---|
| 1.0–3.0 ms | 1261 | 13 | **1.0 %** |
| 3.0–6.0 ms | 26 | 6 | 23.1 % |
| 9.0–12.0 ms | 1317 | 541 | **41.1 %** |
| ≥12 ms | 301 | 58 | 19.3 % |

p50 bind→flush: **LIT 1.8 ms, BLACK 11.1 ms**. And 611 of the 619 black flushes
land **within 0.5 ms of the NEXT bind** (every other time-to-next-bind bucket is
0–5 % black). So a black frame is a flush that arrives ~2 app frames after its
own bind, at the instant the next flip is being programmed.

**Root cause, confirmed in the source.** `VirtioGpu::note_scanout_refresh` arms
the marker on `let watermark = self.next_wire_fence` — *everything submitted so
far*, sampled at the call. The bind-edge arm
(`program_vidpn_source_inner` → `arm_completion_ordered_refresh`) runs in the
PASSIVE display worker, long after the flip was submitted, and at 183 fps the
app has already pushed frame N+1 (and more) into the ring by then. So the flush
for frame N waits for frame **N+1** to complete — one whole frame too long — and
with only two rotating buffers the app has had buffer A handed back and cleared
it for N+2 by the time QEMU reads it. That is the entirely-black frame: not a
half-drawn one, a *re-cleared* one. It also explains why the artifact scales
with frame rate (at Combined's 23 fps the extra frame still fits inside the
buffer's time on screen) and why both falsified attempts missed — neither
changed *when the flush is issued*.

This is precisely the residual 0ab-A shipped with and named: *"`note_scanout_refresh`
samples `next_wire_fence` at the call… closing that needs the watermark captured
at flip SUBMISSION and carried in `PresentFlipPrivate`, not sampled at bind
time."* The oracle turned that from a footnote into the measured cause.

⚠ Also note `scanout_refresh_watermark` is a SINGLE slot: a second arm before
the first fires overwrites it. Binds 3867 vs flushes 2965 in this run — ~900
arms were dropped that way.

#### FALSIFIED — capturing the boundary at flip SUBMISSION (22.22.210.0)

Built, deployed, measured, reverted. `arm_dma_flip` captured
`next_wire_fence` in `DxgkDdiSubmitCommand` and carried it to the bind edge in
`AdapterContext::pending_flip_watermark`. **The gate was live** — `BeCar` 6504
vs `BeSmp` 1694, so 79 % of binds used the carried boundary — and **black
frames got WORSE: 20.9 % → 27.1 %** (GT1 162.3 fps, 52 s, 3689 flushes). The
bimodal split survived unchanged: 0.2 % black at 1–3 ms, **63.1 %** at 6–12 ms.

The raw event stream says why, and it is worth reading as the actual shape of
the defect — a 13 ms cycle, identical on .209.0 and .210.0:

    +1.76 ms  res 332  BIND
    +12.66 ms res 332  read BLACK     <- flushed 10.9 ms after its bind
    +12.72 ms res 335  BIND           <- the NEXT flip, 60 us later
    +13.94 ms res 335  read lit       <- flushed 1.3 ms after ITS bind
    +14.26 ms res 332  BIND

Two flips arrive as a burst ~1.5 ms apart, then nothing for 11 ms. The buffer
bound FIRST in a burst is flushed inside the same burst (1.3 ms → lit); the one
bound LAST waits for the next burst (11 ms → black), by which time the app has
cycled both buffers and re-cleared it. **The flush for frame N is released
within 60 µs of frame N+1's bind, every cycle** — i.e. the carried boundary
still covered frame N+1. dxgkrnl submits a flip's DMA buffer about a frame
after the app presented, so `next_wire_fence` at SUBMISSION is already too
late. `BeRdy` 3925 / `BeDef` 4273 matches the 58/42 early/late flush split
exactly: the black frames ARE the deferred arms.

#### RESULT — 0ab-B is NOT a flush-ordering defect. The ordering is irrelevant.

**The artifact bundle for the follow-up static-analysis session is
`tmp/handoff-perf/` — `INDEX.md` lists it, `HANDOFF.md` is the paste-able
prompt, `reports/measurements.md` is every number in one place.**

Three builds settled it, and the last one settled it by removing the ordering
entirely rather than by arguing about it.

| KMD | bind-edge ordering | GT1 fps | black flushes |
|---|---|---|---|
| 22.22.209.0 | boundary sampled at the bind | 183.6 | 20.9 % |
| 22.22.210.0 | boundary carried from flip SUBMISSION | 162.3 | **27.1 %** |
| 22.22.211.0 | boundary carried from the PRESENT MARKER, per buffer | 171.7 | 21.0 % |
| 22.22.212.0 `BindFlushMode=0` | as .211 | 184.0 | 15.1 % |
| 22.22.212.0 `BindFlushMode=1` | **none — flush AT the bind** | 173.7 | 15.8 % |

`BindFlushMode=1` was demonstrably live (`BeDef` 0, `BeRdy` 4747, every bind
flushed immediately) and it changed **nothing**. Note also the run-to-run spread
on identical logic (.211 21.0 % vs .212 mode-0 15.1 %): treat anything under
~6 points as noise.

**The measurement that ends the ordering theory.** Even with every bind flushed
immediately, the flushes that still land 6–12 ms after a bind — the
marker-driven ones — are **50.2 % black**, against 4.2 % for those landing
1–3 ms after. The black rate is a function of HOW LONG AFTER THE BIND the read
happens, not of what triggered it:

| flush lands after its bind | `BindFlushMode=0` | `BindFlushMode=1` |
|---|---|---|
| 1–3 ms | 1.3 % black | 4.2 % black |
| 6–12 ms | 35.5 % black | **50.2 %** black |

**So the bound buffer's content is destroyed ~6 ms after we bind it, and no
flush-timing change can fix that — it can only race it.** The guest-side counter
agrees to within a point: `MkBound/MkTot = 1268/5955 = 21.3 %` of present
markers named the buffer that was, at that instant, the bound scan-out.

`BeWMax=3 / BeWLst=1 / BeWRng=1` also disposes of the "something unrelated is
blocking the boundary" reading: a deferred arm waits on one to three fences,
all on the host GPU ring — the app's own recent frames.

**What it actually is: a buffer-LIFETIME defect.** A Helios scan-out is not a
continuous scan-out; the host reads the buffer only when a `RESOURCE_FLUSH`
tells it to. So the buffer must be immutable from its bind until that flush
completes. It is not: dxgkrnl retires the flip, frees the previous buffer to the
app, and the app clears it for the next frame while it is still the bound
scan-out and still unread.

Two directions, both real, neither yet built:
1. **Enforce the lifetime.** Do not retire a flip's DMA fence until the
   PREVIOUS buffer's final `RESOURCE_FLUSH` has completed on the host. ⚠ Not
   the same as the 2026-07-29 falsified attempt, which held the fence until the
   flip's own `SET_SCANOUT_BLOB` completed — that waited for the BIND, never for
   the READ. Cost: it serialises the app against our readback.
   → **BUILT AND FALSIFIED, 22.22.216.0. See the next section.**
2. **Shrink the window.** The safe zone is measured: <3 ms after the bind is
   ~1 % black. Today the bind lands ~10 ms after the flip is submitted (PASSIVE
   display worker) and binds arrive in bursts of two with an 11 ms gap. Getting
   bind+flush inside a couple of milliseconds would shrink the race to nothing
   and would help latency generally.

#### FALSIFIED — the presentation-LEASE ownership gate (22.22.213.0-216.0)

Direction 1 above was built in full, deployed, and measured against the QEMU
per-flush oracle on one boot with a same-boot control. **The gate works
mechanically and does not close the defect.** Do not rebuild it.

**What was built** (`helios_kmd_logic::scanout_lease` + `AdapterContext`'s
`scanout_{present,bound,read}_epoch`). Every DMA-buffer flip mints a
monotonically increasing PRESENTATION EPOCH in `arm_dma_flip`, stamped on the
allocation itself (`AllocationContext::vidpn_present_epoch`) so the coalescing
single-slot `pending_vidpn_allocation` cannot pair one flip's handle with
another flip's epoch. The display worker publishes `bound_epoch` after the
`SET_SCANOUT_BLOB` returns; a `RESOURCE_FLUSH` carries a typed
`ScanoutFlushToken` snapshotting `bound_epoch` at ISSUE, and its used-ring
response advances `read_epoch`. A flip's `DXGK_INTERRUPT_DMA_COMPLETED` **and**
its `last_primary_address` publication (the CRTC_VSYNC edge — both are reuse
edges, and gating only the first is what 22.22.207.0 did) are withheld until
`read_epoch >= lease`. Escapes, all loud and counted, never a timeout: a
later bind of a DIFFERENT resource supersedes (virtio FIFO proves no read
remains), and enqueue failure / host error / retire / reject / preempt / reset /
transport failure cancel.

**The gate is provably live, not inert** — the 22.22.208.0 trap was checked for
explicitly. Per GT1 run: `LsMint 5590` == `VpDmaF 5590` (one epoch per flip),
`LsRel 5590` (one release per mint), `LsBlk 20668` retirements actually blocked,
and **`LsEndR 4996` of them ended on a REAL HOST READ** vs `LsSupe 573`
superseded, `LsCanc 0`, `LsTear 0`. It also fixed the bind pipeline outright:
`VpCoal` ~500 → **21** and `VpSkip` ~500 → **23**, i.e. essentially every flip
now binds and gets its own read (the "349 of 4127 bind intervals with no read"
gap is closed).

**And it does not move the artifact.** The decisive metric is the black rate of
the FIRST read after each bind — the exact population the invariant makes
impossible, because at that instant the flip is still held:

| build | 1st-read-after-bind black | GT1 fps |
|---|---|---|
| 22.22.212.0 control, same boot | **18.3 %** (771/4203) | 170.8 |
| 22.22.216.0 lease, run a | **14.7 %** (752/5128) | 168.4 |
| 22.22.216.0 lease, run b | **12.5 %** (676/5397) | 179.4 |
| 22.22.216.0 lease, run c | **16.6 %** (874/5257) | 171.7 |

Mean 14.6 % against a 4.1-point spread on the lease side alone, i.e. inside the
documented ~6-point run-to-run band — and nowhere near the ~0 % the invariant
predicts. fps is unaffected (mean 173.2 vs 170.8; instrumentation-off control
168.4).

**What that proves, and it is the reusable part.** dxgkrnl's flip retirement is
NOT what returns the buffer to the app on this stack. The gate demonstrably
back-pressures dxgkrnl — the flip stream serialised, `VpCoal` collapsed — and
the app still clears the bound buffer before our read of it. That is consistent
with the one structural fact this driver has always had: **the app's render work
never travels through a WDDM DMA buffer at all**; it goes to the host over the
Venus escape channel, so no WDDM completion notification can order the app's
writes against our host read. Holding a flip only throttles `Present`; it cannot
stop a clear that is already queued on the host GPU.

⇒ **No KMD-side notification gating can fix 0ab-B.** The whole
"which completion notification releases the allocation" family — 207.0's
bind-hold, this lease gate, and any successor — is closed. The producing write
has to be ordered where it is issued: the UMD/DXVK swapchain, or the host.

⚠ TRAP THAT COST THREE RUNS: `3DMarkCmd` launched from `win_exec` lands in
**session 0**, which has no desktop. The workload reaches `SINGLE_INIT_BEGIN`
and then null-derefs (`0xc0000005`, `rcx=0`) inside its own module with
`helios_umd.dll` NOT EVEN LOADED — it looks exactly like a driver regression and
is not one. Launch it through a session-1 scheduled task
(`helios_lease_gt1` / `helios_trace_fs`, principal `Rupansh` Interactive
Highest). ⚠ Also: re-signing a package without bumping `DriverVer` makes the
installer refuse to bind (correctly) — bump the version for every deploy.

#### Superseded: capture at the PRESENT MARKER, keyed by buffer (22.22.211.0)

The marker is the last point at which "everything submitted so far" still means
"this frame and nothing after it" — the app records it inside its own Present.
`arm_present_marker_refresh` records `(resource → next_wire_fence)` in a
4-slot table on `AdapterContext`; `arm_bind_refresh` takes the entry for the
buffer being bound and arms against it, falling back to sampling (`BeSmp`) when
no marker named it (the MMIO/desktop path, where dxgkrnl retires the flip
before calling us, so "now" IS that frame's boundary).

Falsifiable prediction: **`BeDef` collapses toward zero** (the boundary has
already retired by bind time), the 6–12 ms flush population disappears, and the
black rate approaches the 0.2 % the early population already measures. If
`BeDef` stays near half, the whole watermark family is dead and the answer is a
lifetime contract instead — do not let dxgkrnl recycle the buffer until the
host's RESOURCE_FLUSH for it has completed.

⚠ The table lives on `AdapterContext`, NOT on `VirtioGpu`: adding 64 bytes to
`VirtioGpu` cost **2448 bytes of boot-chain frame** (17488 → 19936, over the
17936 ceiling), because that struct is built on the `DxgkDdiStartDevice` stack.
Re-measured at 17488 after the move.

#### CLOSED AS AN ORDERING QUESTION — the 2×2 factorial + metric validation (2026-07-29 evening)

Full reports: `tmp/handoff-0ab-b-lease/analysis/{factorial-runs.md,
metric-validation.md, SYNTHESIS.md}` (+ 89 raw artifacts under `analysis/logs/`).
Same boot per build, runs interleaved (A,B,A,B,A then C,D,C,D), same UMD binary
across both builds (hash-verified), oracle on for every run, per-run counter
deltas, mode latched via `pnputil /restart-device` (`BndFM` echo checked).

| | **A** .216 lease + mode 1 | **B** .216 lease + mode 0 | **C** .212 + mode 1 | **D** .212 + mode 0 |
|---|---|---|---|---|
| 1st-read-after-bind black | **2.6 %** | 13.8 % | **5.1 %** | 14.8 % |
| 2nd-read-in-binding black | 43.4 % | 5.3 % | 32.3 % | 10.5 % |
| whole-flush black | 15.4 % | 14.8 % | 14.5 % | 16.6 % |
| age-standardised | 17.5 % | 13.3 % | 15.2 % | 13.7 % |
| GT1 fps | 164.5 | 173.6 | 177.9 | 174.4 |
| `VpCoal` per run | 206–642 | **27–34** | 796–873 | 621–886 |

**No cell moves the whole-flush number** (pooled mode effect 0.66 pp, lease
effect 0.39 pp, both under the within-cell spread). `BindFlushMode=1` does not
remove black publications, it RE-LABELS them: the bind-triggered flush reads
~1 ms after the bind and is nearly clean, while the surplus refresh flushes
(187 flushes/s against 131 binds/s) re-read the same buffer several ms later at
32–43 % black. Age-standardised, mode 1 is WORSE, not better.

**Three corrections to the lease section above, from this data:**
1. *"The gate demonstrably back-pressures dxgkrnl (`VpCoal` 500 → 21)"* — the
   coalescing collapse appears in **lease × mode 0 only** (cell B). Under the
   lease at mode 1, `VpCoal` is 206–642, same order as pre-lease. It was a
   lease×latency artifact (slow mode-0 reads → longer withholding → spaced
   flips), not a lease property.
2. *"the '349 of 4127 bind intervals with no read' gap is closed"* — it is not:
   9.5–10.7 % of binding generations still get zero reads under lease+mode0
   (metric validation, `reuse`-delta-confirmed).
3. The lease-vs-control black comparison: with real n and interleaving,
   **B 14.8 % vs D 16.6 %** (raw; 13.3 vs 13.7 age-standardised) — if the lease
   helps it helps by ≲2 points. The original 18.3-vs-14.6 rested on an n=1,
   run-first control and omitted the lowest lease run (215-mode0, 10.3 %).

**The mechanism is PROVEN (upgrade from the inference above).** The first read
of binding N is the event that ends lease N — and it finds the buffer already
cleared **12.9–17.2 %** of the time across every lease run. At that instant no
WDDM edge can have returned buffer N to the app (reuse of N requires flip N+1's
completion → read N+1 → FIFO-after read N). Supersede/cancel escapes are
excluded (`LsSupe` = 0 in cell A entirely; post-supersede generations are
15–20× LESS black; `LsCanc` 0). In real flip-model the app's next clear is
deferred by the SCHEDULER via the allocation list of its render DMA buffer;
Helios's clears never enter a DMA buffer, so that primitive does not exist
here. Corollary of the metric validation: the black is always a complete
opaque clear (`nonzero==0 && max==0 && csum==0`, zero exceptions), 96–98 %
isolated single frames — one ~7–8 ms flash at a time.

**Where the fix lives (decision list for the owner; measured populations):**

black ≈ (share of publishes issued late) × (P(clear executed by then)) — two
independent terms, two owners:

- **D1 — publish once, promptly, content-ordered (KMD).** Bind-triggered
  first reads are already 2.6–5.1 % black; <3 ms-old reads are 0.4–5.6 %.
  Two sub-items: (i) the mode-0 deferred half (`BeDef` ≈ 41 % of binds even at
  1:1 bind rate) fits the **mark-overwrite window** — a bind landing >2 frame
  periods after its present takes the buffer's NEXT present's watermark
  (`record_frame_watermark` replaces same-resource entries), waiting a frame
  too long; fix = consume the recorded mark at `arm_dma_flip` time (dxgkrnl
  submits flips ~1 frame after present, always before the overwrite) and carry
  it in `PresentFlipPrivate`. Confirm with the R5 delta counter before
  building. (ii) flip→read latency: a DISPATCH-level async bind
  (fire-and-forget `SET_SCANOUT_BLOB` from the flip arm; the completion-
  ordered flush arm already fires from the drain DPC) — T6/R902 deleted
  `set_scanout_blob_async` as *unreachable dead code*, not as a falsified
  design, so this is unexplored.
- **D2 — never publish a binding the app may own (KMD, ⛔-adjacent, needs
  explicit owner sign-off).** The surplus re-publishes are 32–43 % black and
  ~30 % of all publishes at mode-1 cadence; suppressing/deferring a refresh
  whose armed identity ≠ the active binding when the active binding already
  had its first publish would take whole-flush to ≈ the first-read rate.
  This is 22.22.208.0's identity gate — inert then only because mode-0
  cadence starved its precondition — and it is ADJACENT TO THE REJECTED
  `BindFlushMode=2`. The old objections now have answers (the lease's epochs
  repair ownership; DWM's same-buffer re-presents mint fresh epochs and are
  never suppressed; a suppressed stale re-read trades a 40 %-black flash for
  a one-frame hold), but the ⛔ stands until the owner says otherwise.
- **D3 — lease disposition.** The DMA_COMPLETED/address withholding is proven
  inert against the defect (this table) and its hang-suspicion is retired
  (defect 0ac reproduced on .212); the EPOCH bookkeeping is what D2 needs.
  Keep epochs, consider retiring the withholding (or bounding it loudly).
- **D4 — the true ~0 % ceiling** is a venus-level acquire: a GPU-side wait so
  the clear executes only after the host read (the "non-hack form" already
  anticipated at the `publish_present_order` consumer note above), or the
  host-side equivalent (read-at-bind atomically in QEMU, owner-gated). Whether
  the ~1–4 % residual after D1+D2 justifies it is an owner call.

Also out of this campaign: defect **0ac** (guest bugcheck 0xD1 on 22.22.212.0,
dump preserved) and defect **0ad** (fullscreen→desktop transition drops the
host readback ~250 ms), filed in the WS1 list below.

#### BUILD 1 SHIPPED — D1(i)+D2+D3, KMD 22.22.217.0 (2026-07-29 ~22:15)

Owner approved D1+D2+D3 and deferred D4. Design:
`tmp/handoff-0ab-b-lease/analysis/FIX-DESIGN-build1.md`; implementation +
review record: `analysis/build1-implementation.md`; acceptance numbers:
`analysis/build1-results.md` (raw artifacts `analysis/logs/b1-*`).

**What landed** (all uncommitted, like the rest of the tree):
- **D2 — the ownership gate** on the flush executor
  (`scanout.rs::queue_active_scanout_refresh_locked`): identity arm (armed ≠
  active → drop, `OgIdn`) + epoch arm (`helios_kmd_logic::scanout_lease::
  surplus_republish`, 5 host tests, `OgEpo`) with a third `tracked` operand
  (`AdapterContext::scanout_epoch_tracked`, mirrored as `LsTrk`) that disarms
  the gate on the desktop's first MMIO bind — the MMIO contract mints no
  epochs, so without it a stale `present>bound` after any app run would have
  frozen the desktop (0aa). LsTrk's disarm was verified on hardware.
- **D1(i) — allocation-carried frame marks** (`AllocationContext::
  vidpn_frame_watermark`, taken at flip-arm time): LIVE (96.9 % of
  frame-boundary binds use the carried mark) but **INERT — the mark-overwrite
  mechanism (R5) is DEAD**: `BeOvw` 11/1/6 per run (0.02–0.25 %), not the ~41 %
  the hypothesis predicted, and `BeDef` stays 45–48 %. The deferred half
  defers because the frame's content genuinely has not retired at bind time —
  which is CORRECT completion ordering, and harmless now that D2 stops the
  surplus reads. Kept: it is 0ab-A-protective and the census cost nothing.
  (Sixth falsified sub-mechanism of 0ab, this time for the price of a counter
  riding a winning build.)
- **D3 — the lease's completion withholding retired, epochs kept** (they are
  D2's predicate). `LsBlk/LsRel/LsPump/LsWait/LsPubG` removed with their
  mechanisms and one-shot-zeroed at StartDevice; the liveness pump deleted;
  `WddmPending` no longer carries a lease.
- **0ac riders**: the `WvTorn` tripwire (with_virtio; the review caught its
  failure arm releasing the lock through the corrupted pointer — fixed to a
  pre-acquire hoisted address, codegen verified) and PDB archiving in the
  deploy script (fired on its first real run: sys+pdb+map in
  `HeliosDeployBackups\20260729-221346\staged`).

**Acceptance (all this-boot deltas, oracle per run):** GT1 ×3 whole-flush
black **2.1 / 0.7 / 2.0 %** (was 14.5–16.6 % in every factorial cell),
first-read 1.9/0.6/1.8 %, fps 185.7/169.2/182.0, duplicate-content
0.2–1.5 % (was 2.9–10.3 %), `OgIdn` 1057–1483 + `OgEpo` 6–13 per run
(24.5–33.4 % of would-be flushes dropped), 2nd-read population 1090–1669 →
**9–26**, 6–12 ms bucket 56 % → **0.5 %** black at unchanged flush share.
Combined ×1: **PASS** (1.3 % black, 61.5 % of binds still content-deferred =
0ab-A ordering intact, `OgEpo` 0). Desktop: binds:flushes 412:415, Start menu
opens (0w closed stays closed), windowed D3D11 + desktop coexist with
`OgIdn` 7.5 % and no starvation — the self-healing argument held, no escape
hatch needed. `WvTorn` 0, `IrqlBad` 0, no bugcheck in 4 runs (not evidence on
0ac's ~1-in-10 base rate).

**Attribution caveat, recorded honestly:** D2 and D3 shipped together, so the
build cannot A/B them — but the dropped-population accounting is
mechanism-level attribution to D2 (the reads that vanished are exactly the
population that was black), and the factorial had already measured D3's
withholding as inert on black.

**Residual ~0.7–2.1 %**: the 1–3 ms margin race at the bind edge (most of
what remains), workload transitions (12+ ms bucket, adjacent to defect 0ad),
and a 3–6 ms bucket that reads 50 % on n≈6 (noise; do not quote it). Paths
below ~1 % if ever wanted: D1(ii) (DISPATCH-level async bind — shrinks the
margin race) and D4 (the venus acquire — the true ~0 % ceiling). Neither is
scheduled; owner's call after the visual check.

**Owner visual verdict (same night): GT1 clean, overall score >25k (was
~20k) — 0ab-B's main population CLOSED. Residual black-frame stutter
observed in GT2 around ~200 fps → filed as 0ab-C**, classification plan and
levers in `tmp/handoff-0ab-c-gt2/HANDOFF.md` (next session).

### 0ab-C — CLASSIFIED 2026-07-29/30 night: the first-publish margin race at GT2's operating point

Two instrumented GT2 runs on 22.22.217.0 (a GT2-only schtask `helios_gt2`
now exists; real GT2 runtime **68–69 s**, fps **209.5/210.1** — the owner's
"~200 fps" is GT2's actual average; the T6-era ~63 fps figure is obsolete).
Full verdict: `tmp/handoff-0ab-c-gt2/analysis/CLASSIFICATION.md`; raw
artifacts `tmp/handoff-0ab-b-lease/analysis/logs/c1-gt2-*`; predictions were
registered in advance (`PREDICTIONS.md`) and scored.

- **Population (a) — the bind-edge margin race on the FIRST publish —
  dominates (~75–85 % of black), the exact population build 1 left open.**
  Whole-flush black 7.3 %/6.0 % (GT1 post-fix: 0.7–2.1 %), carried by first
  reads (7.3 %/6.2 %) in the 1–3 ms bind-age bucket (10.1 %/8.6 %); 96–98 %
  isolated single-frame flashes at ~116–120 flushes/s ⇒ ~7–8 flashes/s = the
  visible stutter. The 6–12 ms bucket stays 0.2–0.4 % and 2nd reads ~1 %
  black — **the 217.0 ownership gate holds unchanged at 210 fps**; identity
  clean (0 mismatches), 2-buffer rotation, `WvTorn` 0 both runs.
- **Guest-side half measured**: inter-bind gaps are bimodal — ~45–48 % at
  1–3 ms but 19–24 % at 10–14 ms and 10–11 % ≥20 ms (mean worker cycle ~7 ms
  vs the 4.8 ms flip cadence). `BeOvw` (bind landing after the same buffer's
  next present) 180/194 per run vs GT1's 1–11, climbing within-run with the
  black. Within-run black tracks scene PHASE (flip rate is flat 197–223/s);
  between operating points it tracks the margin 1/F − latency, which crossed
  zero between GT1's 5.9 ms and GT2's 4.8 ms period.
- Minorities: **(c)/0ad** — the undersized `res 6` transition bind fires at
  t≈63.7 s in BOTH runs (scene-end window w12, each run's worst 5-s window,
  ~12–23 % of total black; separate defect, unchanged). **(b)** duplicates
  3–5 % (GT1 0.2–1.5 %), `VpCoal` 15.5–16 % — real, minor. **(d)** absent.
- **Fix: D1(ii) — DISPATCH-level fire-and-forget SET_SCANOUT_BLOB from the
  flip arm**, design + review checklist + registered predictions in
  `tmp/handoff-0ab-c-gt2/analysis/FIX-DESIGN-d1ii.md`. Pure accelerator: the
  worker path and the DestroyAllocation cancel semantics are untouched;
  values-only in-flight entry; wire-order seq guards bookkeeping; failure =
  count + existing worker ladder.
- **22.22.218.0 (D1(ii) build 1) BUGCHECKED 0xA deterministically under GT2
  (2/2, ~50 s) — ROOT-CAUSED from two kernel dumps, and it is NOT the D1(ii)
  logic:** a pre-existing TOCTOU in the sync-wait protocol.
  `wait_block`'s lock-free `is_done()` early exit let the waiter pop its
  stack frame between the drain Sync arm's `done.store(Release)` and its
  `KeSetEvent` (an ISR or KVM vm-exit stalls the draining CPU mid-window);
  the drain then memcpy'd + signaled a popped `SyncWaitBlock` on the HPD
  worker's stack. .218 armed it by doubling ctrl traffic and lengthening
  drain holds (sync waits started outliving the 15.6 ms wait slice, so the
  poll finally ran inside the window). Dumps show the worker one call past
  its sync bind, spinning on the drain's own lock — the race photographed.
  All abandon/timeout counters zero; fast-bind machinery clean (`FpErr` 0,
  `FpSeq == FpAppSq`, desktop `Fp*` Δ0). Full record:
  `tmp/handoff-0ab-c-gt2/analysis/BUGCHECK-0xA-218.md`; forensics transcript
  `tmp/dump0xA/`; dumps preserved in `C:\HeliosDumps`.
  **Fix = 22.22.219.0: delete the `is_done()` fast path — the signal (or the
  lock-serialized timeout-abandon) becomes the only exit.** (The same
  deletion also fixed the identical latent TOCTOU on the `wait_fence` path,
  which shares `wait_block`.)
- **22.22.219.0 battery (c3-*): the TOCTOU fix HOLDS — 5/5 cells, zero
  bugchecks where .218 died 2/2.** All gates pass: GT1 black 0.7 %
  (0ab-B closed), Combined first-read 0.5 % with `BeDef`-dominant ordering
  (0ab-A closed), desktop inert (`FpBind` Δ0) and clean, `WvTorn`/`IrqlBad`/
  `FpErr` 0 throughout. **GT2: black HALVED, 7.3/6.0 % → 4.0/3.4 %
  (fps 211.4/213.0, up), but the registered ≤2.5 % target was missed.**
  Early/mid-scene windows collapsed to 0.5–1.6 % black; the residual lives
  late-scene. Per-poll attribution: the fast path's ALREADY_BOUND skip is a
  FLAT ~28–34 % coverage gap (`FsC0` 2140/2192 per run — the predicate
  compares against the *applied* active resource, 1–2 flips behind at
  2-deep pipelining), and the late-scene black climbs with `BeOvw` (worker
  bind lateness) — i.e. black ≈ flat-uncovered-fraction × climbing-worker-
  lateness. Secondary (not black): presents outrun reads 2.3:1, `OgEpo`
  416–538/run (correct surplus drops), duplicates 11 %, in-scene flush rate
  −22 % under the doubled ctrl load — display-freshness economics, a
  separate lever. Full numbers + registered next-step predictions:
  `tmp/handoff-0ab-c-gt2/analysis/build219-results.md`.
- **22.22.220.0 = D1(ii)-b** (wire-resource skip predicate): predicate
  confirmed live (`FsC0` 2140→296) but the freed flips became `FpBusy`
  (61→1776) — the SINGLETON bind command buffer was the real coverage
  bottleneck (it only returns at the guest DPC drain, which lags the host
  consume by several flip periods). Exact identity all cells:
  `FpBind + FpSkip + FpBusy = VpDmaF`. GT2 black flat; GT1 1.9 % at
  190 fps (confounded). `tmp/handoff-0ab-c-gt2/analysis/build220-results.md`
  (includes the x/y identifiability argument that motivated the last build).
- **22.22.221.0 = the bind command POOL (depth 4; `Vec`, not array — the
  inline array measured 18128 B on the StartDevice chain vs the 17936
  ceiling) — THE DISCRIMINATING RUN, and it discriminated:**
  `FpBusy` → 0, coverage → **99.1/99.3/99.9 %**, `FpBind = VpDmaF − FpSkip`
  exact — and **GT2 black did not move by one decimal (3.5 %/4.1 %)** ⟹
  x = y in the mixture model: flip-time binds go black at the worker-timed
  rate. **The bind-timing family is EXHAUSTED WITH PROOF for GT2** — 0/439
  pooled black in the 0–1 ms bind-age bucket; the loss is in the READ
  window, to the venus-executed clear no publish timing can outrun. The
  remaining GT2 lever is **D4 (venus-level acquire / host read-at-bind,
  owner-gated)**, plus the flush-freshness economics as a separate quality
  item. **The same pool FIXED GT1: 1.9 % → 0.3 % — the best GT1 black
  recorded, below the whole .217-era band** (at 178–190 fps the margin is
  wide enough for flip-time binds to win; at 210+ fps it is not).
  Three consecutive clean batteries on the TOCTOU fix (c3/c4/c5, 15
  workload cells, zero bugchecks). GT2 fps 214.1/212.5 (up), dup% improved
  (12.6→7.6 %), `OgEpo` calmed (338→83). Watch: GT1/Combined scores c5
  181.0/20.11 vs c4's 190.3/21.51 (c4 looks like the high outlier vs c3/b1;
  re-measure before calling it a cost). Full record:
  `tmp/handoff-0ab-c-gt2/analysis/build221-results.md`.
- Shipping state after the four-build night: 22.22.221.0 (`DspBnd=1`,
  `BndFM=0`; superseded by 22.22.222.0 below).
  **OWNER EYE VERDICT (2026-07-30): GT1 visually CLEAN — the GT1 half of
  0ab-C is CLOSED by the ground-truth rule. GT2 still visibly flashes,
  ~24 black frames over a full run** (≈0.5/s visible vs the oracle's
  ~3.3–3.9 black flushes/s — VNC delivery samples roughly 1-in-7 into
  displayed frames). The GT2 residual is proven out of
  KMD-publish-timing scope → **next session = the D4 family**, handoff at
  **`tmp/handoff-gt2-d4/HANDOFF.md`**. Deferred hygiene items: the fast
  path's mint-before-enqueue staleness (benign, one-line remedy documented
  in the 220 implementor report); the `FpCoal`-heavy same-res coalescing
  semantics if the pool ever deepens further.
- **D4a v1 BUILT, LIVE, and MEASURED INERT on GT2 black — the in-flight
  conditional's blind spot found (2026-07-30, KMD 22.22.222.0 + UMD/DXVK,
  battery c6)**. The full acquire chain shipped and is proven end-to-end:
  per-resid READ LEDGER page (escape 0x000E, `RdIss == RdRet` exact in
  every cell, zero overflow/orphans), persistent retirement events
  (0x000F), `ScanoutFlushToken` carries resource identity with
  Drop-guaranteed retirement, DXVK arms conditional GPU-side timeline
  waits at the reuse submission (GT2: armed ≈5 % of frames, signals
  sub-8 ms, `residMiss=0`; GT1 0.4 %; desktop ~0), knob `ScanoutAcquire`
  (default ON, free: **GT1 192.6 and Combined 22.44 — both records — with
  it on**; GT2 213.5/209.8 fps in band; 0ab-A gate 0.5 % exact; zero
  bugchecks). **GT2 black: 3.9 %/4.0 % — unchanged from .221's
  3.5/4.1 %.** The design's §8 falsifier fired with a sharper
  localization: at ~2 frames of CPU run-ahead the reuse-list's ledger
  check races the bind-edge flush ISSUE of the buffer's own last present —
  the killer read is not yet in flight when the only sound check can run,
  and the watermark orders that read only against its OWN frame's content
  (0ab-A's guarantee, which held), not against the NEXT reuse's clear.
  **The in-flight-only conditional (the −40 %-trap cost guard itself) is
  therefore structurally insufficient at GT2's operating point.** Both
  closures are owner-gated: **D4a v2** (settle-semantics wait — UMD-side
  per-resid present counter closes the race by program order; KMD signals
  settlement from the existing lease edges; cost = throttles run-ahead
  toward flip cadence, unmeasured, same family as the §3(a) trap → build
  knob-gated with a registered fps envelope) or **D4b** (host-side
  read-at-completion in qemu-helios; structural kill, slow owner-gated
  loop). Record: `tmp/handoff-gt2-d4/analysis/build222-results.md`
  (+ `FIX-DESIGN-d4a.md` with scored predictions).
- Interim shipping state 22.22.222.0 (D4a live-but-inert; superseded below).
- **D4b — THE ORDERED SNAPSHOT CHAIN: BUILT, SHIPPED, and ORACLE-CLOSES the
  GT2 residual (2026-08-02, KMD 22.22.224.0, battery c8). GT2 oracle black
  3.9–4.1 % → 0.02–0.1 %.** Owner selected D4b; the design conversation
  established that a QEMU-only fix cannot order against the venus-ring
  clear (packaged render server, content destroyed before flush arrival),
  so the snapshot/copy rides the ONLY viable insertion point — the app's
  own submission stream: at present time DXVK records a GPU blit of the
  presented primary into a 4-slot ring of DXVK-internal DirectOptimalScanout
  images (queue-ordered after frame N, before clear N+2, NO waits); the
  UMD substitutes the snapshot's full descriptor into the present private
  data (`HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT`, wire struct 40→48 B,
  capability-gated by the 0x000E PROBE caps); the KMD carries it BY VALUE
  into the flip and binds/flushes the snapshot while ALL flip bookkeeping
  stays on the real allocation. Nothing ever clears a snapshot — the race
  died structurally. QEMU/virglrenderer: zero changes. Census EXACT:
  `SnSub = VpDmaF = FpBind`, `SnFbk = 0`, `FpSkip = 0`, `BeCar` dominant
  (0ab-A preserved), `RdIss == RdRet` everywhere, zero bugchecks; GT1
  black 0.2 % with the fps↔margin correlation broken (the mechanism's
  differential signature), Combined 22.68 = record, desktop/MMIO path
  untouched. **The .223 detour's permanent lesson: dxgkrnl does NOT
  forward Present private data to DxgkDdiPresent on DMA flips (PBIdOk=2
  across three generations) — per-present data for the flip arm must ride
  the Render command; .224 stashes it per-context at DxgkDdiRender and
  takes+clears at every Present.** Records:
  `tmp/handoff-gt2-d4/analysis/{FIX-DESIGN-d4b-snapshot,build224-results}.md`
  (+ build222-results.md for the D4a-v1 falsification that motivated it).
  Residual black counts are 0ad-class transition edges. **0ab-C is
  oracle-closed; the owner's eye (baseline ~24 visible flashes/run) is the
  ground-truth close.** Note: the c8 battery ran at 1896×1030 (EDID/viewer
  geometry after a host reboot) — fps not comparable to 1280×800
  baselines; a `ScanoutSnapshot=0` A/B isolates the blit cost if wanted.
- **0ab-C: CLOSED 2026-08-02, OWNER-CONFIRMED FIXED.** Close-out build
  **22.22.225.0**: sane defaults made code defaults (`DisplayHalf` now
  defaults ON — the render+display miniport is the product; the production
  `reg add` is gone), leftover diagnostics retired (`PresentProbe` registry
  value cleared; the dead PresentCb-private channel — decode, `PBIdOk`,
  and the UMD's write — deleted outright, since dxgkrnl never forwarded it
  on flip presents), census log cadences quieted to steady-state
  (per-16384). Smoke on .225: GT2 black 1/4766 = 0.02 %, `SnSub == VpDmaF`,
  ledger exact, zero faults. The entire 0ab arc (D1(ii) family + D4a + D4b)
  is committed and pushed (main `wddm`, dxvk-helios `master`, qemu-helios
  `helios-11.0.1`).
- **SHIPPING STATE: 22.22.225.0**, all knobs at code defaults; kill
  switches `ScanoutSnapshot=0` / `ScanoutAcquire=0` / `DispatchBind=0` /
  `DisplayHalf=0`.
- **NEXT FOCUS: PERFORMANCE (WS2) — the STRUCTURAL bottleneck** — Fire
  Strike Graphics 43k → 70k+, Combined → 10k+. Handoff:
  **`tmp/handoff-perf-structural/HANDOFF.md`** (owner: one structural
  point, not micro-levers; Steel Nomad Vulkan is only ~10 % off native ⇒
  the shared ICD/venus/KMD substrate is exonerated, the gap is
  D3D11-side). Supersedes `tmp/handoff-perf-saturation/HANDOFF.md` (its
  attribution + lever outcomes stand in
  `tmp/handoff-perf-saturation/reports/p1-attribution.md`).
  **66th session (2026-08-03): the prime suspect is CONFIRMED** — at
  THREADING caps = 0 the runtime EMULATES command lists: Fire Strike's
  ~15 workers record into software deferred contexts (SWDC_* frames,
  worker sample) and the render thread replays every call through the
  immediate DDI (`SWCL_CommandList::Execute` = #1 d3d11 function by
  direct RIP; replay markers in 42.5 % of UMD samples, lower bound).
  Evidence: `tmp/handoff-perf-structural/reports/
  p0-commandlist-verification.md`. Build plan (phases A thread-safety →
  B FREETHREADED → C COMMANDLISTS_BUILD_2 → DXVK's stock
  D3D11DeferredContext, each knob-gated):
  `tmp/handoff-perf-structural/PLAN-commandlists.md`. DXVK fork and cxx
  bridge need ZERO changes — the work is entirely in `umd/src`.
  **Phase A LANDED (`ed7efe1`, thread-safe forward layer) and Phase B
  LANDED (`b47bdd4`, FREETHREADED behind `UmdFreeThreaded`, absent=ON).**
  First Phase B canonical run: GT1 190.92 / GT2 208.92 / Combined 25.25 /
  Physics 112.11 — GT2 +3.4 % on the tight metric, GT1+Combined at/above
  the historical best, all gates green. (Standard-preset scores are
  display-mode-independent — owner-confirmed; comparable to baseline
  directly. Owed: 3-run median + optional UmdFreeThreaded=0 knob A/B for
  attribution, cold-boot gate.)
  **Phase C LANDED AND MEASURED (67th session, 2026-08-03): NEGATIVE on
  GT1/GT2, POSITIVE on Combined — knob stays default OFF.** Full DC/CL
  DDI surface (tag-discriminated HDEVICE namespace, context-local handle
  copies, BUILD_2 recycle flow → DXVK's stock D3D11DeferredContext)
  landed in `2fccb5c`+`9fbeeb6`+`fa1d75b` behind `UmdCommandLists`
  (bring-up default OFF; requires UmdFreeThreaded). Same-boot A/B:
  ON = GT1 49 / GT2 144 / Combined 28.6–31.2; OFF = GT1 184.7 / GT2 210.6 /
  Combined 25.3–26.7 (= the Phase B 3-run baseline GT1 184.23 / GT2
  209.65 / Comb 25.01, so environment+ICD exonerated). The replay share
  DID move off the render thread (d3d11.dll 42.5 %→3.4 % of samples) and
  submits are EQUAL (AsSub 110.7k vs 115.1k — no flush storm); the loss
  is DXVK's constant per-CL costs on the single dxvk-cs consumer at the
  runtime's granularity (GT1 942k, GT2 2.66M FinishCommandList cycles
  per run; recorded trailing state reset in EVERY CL; double reset +
  chunk flush per execute; render thread 47 % parked on CS
  backpressure). Numbers, scored predictions and the identified next
  lever (make DXVK per-CL costs content-proportional — fork changes now
  in scope): `tmp/handoff-perf-structural/reports/p2-phase-c-outcome.md`.
  ⚠ Two host-stack robustness defects found en route (WS1):
  `tmp/xid109-evidence/INCIDENT.md` — a venus worker can wedge inside the
  NVIDIA driver and QEMU's virtio-gpu serializes behind it forever (all
  contexts starve; one instance drew an Xid-109), and worker death is not
  handled as context-loss. Hit 2× on 2026-08-03 during 3DMark
  probe/loading phases, then 2 clean runs — timing-sensitive.
  **67th session: wedge #3 captured LIVE with debuginfod symbols and the
  wedge class FIXED guest-side** — the ring thread was executing the
  guest's async `vkWaitSemaphores(UINT64_MAX)` (Mesa venus upstream's
  feedback race-closer) inside the NVIDIA driver for a channel NVRM had
  already Xid-killed; stock virglrenderer passes the guest timeout
  verbatim, so bounding it guest-side frees the ring. icd/mesa
  `f0c7bcd3465` (`VN_HELIOS_RING_WAIT_BOUND_MS`, default 8000);
  4 subsequent FS runs, zero wedges/Xids; A/B-proven perf-neutral.
  Evidence: `tmp/xid109-evidence/wedge3/WEDGE3.md`. Still open (WS1):
  QEMU treating worker death as context-loss, and the Xid trigger if it
  recurs with the bound in place.
  **Phase D attempted (68th session, 2026-08-03): dxvk-helios sweep
  elision measured INSUFFICIENT — knob stays OFF; the Xid-109 trigger is
  now the gating item for any further CL-path work.** Levers 1+2 of
  HANDOFF-PHASE-D.md landed in dxvk-helios `3daacecc` (parent `7470eda`):
  the immediate context elides redundant `ResetCommandListState` sweeps
  via CS-stream tail tracking (`m_heliosCsState`), and `FinishCommandList`
  stops recording the trailing sweep into every CL (`EndsClean=false`,
  leftover state restored by the EmitCs funnel; kill switch
  `HELIOS_DXVK_CL_FAST=0` = stock). Producer side sped up as designed
  (2.89M finishes in 44 s ≈ 65k/s vs stock ~8k/s) but scores barely
  moved: ON+fast GT1 53.8 / GT2 145.1 / Comb 33.2 vs ON-stock 49/144/29–31
  — the reset sweeps were NOT the dominant per-CL cost. Remaining CS-side
  suspects: per-chunk dispatch/wakeup overhead (EmitToCsThread → one
  chunk + one queue op per tiny CL) and the CL content itself. GT1-ON
  runs at ~15 % GPU (host trace) — still guest-CPU-bound.
  **The Xid-109 trigger characterized (WS1, still unfixed): NVRM CTX
  SWITCH TIMEOUT on the workload's venus channel, mid-GT1 only,
  native-CL path only — 2 of 3 fast-path runs (+24 s, +53 s), ~1 of 3 at
  stock Phase C rates (wedge3), never on the emulated path at 184 fps.**
  New presentation with the ring bound in place: the channel dies, the
  next `vkQueueSubmit2` dispatch fails ("resulted in CS error"), the
  guest sees it at `vkEndCommandBuffer` (dxvk-cs exception) — a
  PER-CONTEXT death with clean `pnputil /restart-device` recovery, no
  QEMU relaunch (containment proven live twice). KMD counters clean both
  times (AsSub==AsDone, no storm); healthy-run scores are tight, which
  argues against systematic garbage draws; the leading remaining theory
  is a timing-sensitive lost/misordered device-side signal (the WS1
  "never signal a wire fence before host completion" suspect —
  rate-amplified, execute-count-correlated: GT1 has 2× GT2's executes).
  **A persistent host-side evidence trap is ARMED** (`tmp/xid-trap/`,
  nohup, survives sessions): on any NVRM Xid line it captures journal
  context, nvidia-smi, QEMU stderr tail, per-thread states and gdb
  backtraces of every virgl_render_server (ptrace_scope=0). Operating
  rule going forward: NO knob-ON benchmark runs except with the trap
  armed and a specific hypothesis to discriminate — the next occurrence
  must pay for itself in stacks, not scores.
  ⛔ **STALE as of the 72nd — the GT1 half of this entry no longer
  reproduces.** Owner report, 2026-08-05: **GT1 never triggers Xid-109
  any more**; it was fixed in work that landed after `18dba5f` and this
  section was not updated. Everything above about "2 of 3 fast-path GT1
  runs", the rate-amplification model and the trap operating rule is
  history. ⚠ The `tmp/xid-trap/` trap is still armed and last captured on
  2026-08-03; there is no live GT1 Xid for it to catch.

  ⭐ **72nd: what IS live is a D3D12-only reproducer, and it is a
  different animal.** `test_uav_counter_null_behavior_dxbc` and `…_dxil` from
  vkd3d-proton's own suite fire it **on demand, ~6-8 s in, two for two**
  (dxbc start 16:02:20 → `Xid 109 … name=vkr-ring-346, channel 0x1b, CTX
  SWITCH TIMEOUT` at 16:02:26; dxil start 16:11:23 → Xid at 16:11:31, a
  different ring). Run one test headless in ~30 s instead of waiting for
  2-of-3 fast-path GT1 runs. `tests/d3d12_descriptors.c:4440` builds UAVs
  with a **null counter resource** and dispatches counter ops on them —
  the test calls it *"technically undefined, but all drivers behave
  robustly here"*, and its neighbour records *"Observed on NV: Blue screen
  of death (?!?!)"* for the analogous case, so the family hard-faults
  NVIDIA hardware rather than returning zeroes.
  ⚠ **Do not read this as evidence about the old GT1 Xid** — that one is
  fixed and gone (see above), so there is no rate-amplification theory
  left to discriminate against. This is a **self-contained D3D12
  robustness defect**: a guest application can fault the host GPU context
  with a null UAV counter descriptor, and the guest then hangs instead of
  seeing an error. It is scoped to the D3D12 workstream and it is
  excluded from routine gate runs (below), so it does not gate anything.
  ⚠ **Containment reconfirmed, and the real defect named:** the Xid kills
  ONE channel — the `D12-G1` bridge probe passed all 28 steps *while the
  wedged process was still alive* — but **nothing propagates the loss to
  the guest**: `/tmp/helios-qemu-stderr.log` has no entry at all for
  either Xid, and the guest's vkd3d fence thread sleeps in the venus ICD
  forever (6+ min, 0.17 s CPU).
  ⭐ **Why the ring bound does not cover this, and it is structural rather
  than a missed site.** That fix bounds a *ring* wait
  (`VN_HELIOS_RING_WAIT_BOUND_MS`, default 8000,
  `icd/mesa/.../vn_queue.c:2824`), and venus's own escalation ladder
  (`vn_relax`, `vn_common.c:248`) checks `VK_RING_STATUS_FATAL_BIT_MESA`
  and the `ALIVE` bit through `vn_watchdog_timeout()`. **Every one of
  those signals is ring liveness.** Xid 109 kills the GPU *channel* while
  the `vkr-ring-NNN` thread stays perfectly healthy and keeps marking
  itself alive — so the watchdog is watching the wrong thing, and the
  guest waits on a fence whose GPU work is already dead. ⛔ Do not "fix"
  this by shortening the ring bound; the signal needed is *host
  submission/fence failure*, which today reaches neither QEMU's log nor
  the guest. **A lost host context must become a guest-visible error
  (device removal / TDR), not an unbounded wait** — that is the WS1 fix,
  and it is now testable in half a minute.
  ⚠ Note for the record: the two tests exercise behaviour vkd3d itself
  calls undefined, so they are **excluded by name** from routine G2/G9
  runs (`test-runner.sh -x`, fork commit `fd205b2c`, which prints
  `EXCLUDED <name>` for every one) and kept as the dedicated repro. The
  exclusion is a scheduling decision, not a verdict: a guest application
  being able to fault the host GPU context is a real robustness defect and
  stays open here.
  Evidence: `tmp/dx12/gates/G2/hang/` (stacks + `/ma` dumps),
  `tmp/dx12/gates/G2/hangs.txt`, `docs/dx12/GATES.md` §4.3.
  **NEW WS1 watch item (67th): `ScStale` ≈ 4,000/run under FS (23 % of
  18k flips; ScUnav ~10/run) — PRE-EXISTING load signature, masked until
  now because every historical gate check followed a counter-zeroing
  device restart.** The KMD's ticketed gate-clear refusal handles it
  (display correct, scores normal), but `adapter/mod.rs`'s "interleave
  does not occur today" invariant is falsified under FS flip load. The
  kmd-gate-surface must-be-zero list needs a policy answer (expected-
  under-load vs defect) before the next KMD tranche.

## Workstream 1 — Stability

**IDD frame freeze: DIAGNOSED 2026-07-05 (17th session), live on the frozen boot** — full chain
in memory `idd-freeze-root-cause-chain`. Summary: (1) routine multi-second completion stalls
(per-present full-GPU drain in `rotate_resource_backings` + event-cadence desktop) →
(2) the 4×8 s sem-deadline latch declares CONTEXT LOST on a healthy-but-slow stack →
(3) dxvk teardown on DEVICE_LOST resets command pools with host work pending (= the
`vkResetCommandPool` VUs; symptom, not cause) → (4) post-loss, `submitCmdLists` drops cmdlists
WITHOUT `notifyObjects()` → in-use refs leak → next `Map` → `waitForResource` (no timeout, no
lost-check) wedges dwm permanently; win32k session-1 GDI hangs behind it. Falsified: the
early-fence/helios_sync theory for the steady-state stall — 0 of 251,810 submissions carry
ring≠0; the vn win32-sync signal path never fires; cross-process sync is dxvk-helios-internal.
**Status 2026-07-06 (18th session): the whole chain is now closed** — (1) the "stall" was the
sem-deadline misreading idle wait-before-signal waits (fixed, defect 1 below) plus the rotate
drain (fixed, WS2); (2) the latch no longer fires on idle desktops; (3)+(4) fixed 17th session.
Remaining: cold-boot + multi-hour soak, and the forced-loss test for the loss path (defect 2).

Open defects, roughly ordered:

0w. **CLOSED 2026-07-28 — the Start menu never opened: our own QFOT re-acquire
    claimed `DxvkAccess::Write` on a still-open command list, making
    `SyncSharedTexture`'s wait unsatisfiable.** Owner report: "clicking the
    Windows button or the search bar does not open them — it's literally not
    there, I can't interact with it either", plus "if I keep clicking the
    search bar, it sometimes appears".

    **Symptom, measured.** `tools/start_menu_repro.ps1` and
    `tools/start_invoke_probe.ps1` (session-1 tasks) press the hotkey / click
    the real Start button and report each participant's CPU delta, window tree,
    UMD-log growth and screen pixels:
    - `StartMenuExperienceHost` burned **0.000 s of CPU** across 6/6 Win-key
      presses AND a real Start-button click, owned **zero** top-level windows,
      and its UMD log grew **+0 bytes**. It never woke.
    - Controls passed: Win+R opened a Run dialog from the same probe (so the
      synthetic input was real) and `SearchHost` painted its flyout in ~150 ms.
      The desktop, dwm, composition and the taskbar were all healthy.
    - Its UMD log ended mid-call — **8 `calling DXVK CreateTexture2D` vs 7
      `returned`** — on a 704x576 `fmt=65` (A8_UNORM) `misc=0x802`
      (SHARED|SHARED_NTHANDLE) texture, i.e. a XAML glyph atlas.

    **Root cause.** `DxvkContext::acquireSharedImagesFromExternal` — the
    Helios-only queue-family-ownership-transfer re-acquire, which upstream DXVK
    does not have at all (0 occurrences upstream vs 9 in the fork) — ran at the
    START of every command list and did
    `m_cmd->track(image, DxvkAccess::Write)` for every shared image the
    previous list touched. `track(obj, access)` records access "for the purpose
    of CPU access synchronisation" (`dxvk_cmdlist.h`), which is exactly what
    `DxvkDevice::waitForResource` tests. So a brand-new command list — still
    OPEN, never submitted — held a write reference on the texture, and
    `D3D11Initializer::SyncSharedTexture`'s `waitForResource(image, Write)`
    could only be released by that list being submitted, by the very thread now
    blocked in the wait. On an idle XAML startup that is a permanent deadlock:
    the UI thread parked forever and the Start menu's CoreWindow was never
    created. A queue-ownership transfer does not write image contents, so the
    Write claim was simply wrong.

    Chain, symbolised from a live minidump (`take-minidump.ps1` + `cdb` with
    the release PDB):

    ```
    Windows_UI_Xaml → dcomp → d3d11
      → helios_umd::forward::resource::create_resource
        → dxvk::D3D11Device::CreateTexture2D → CreateTexture2DBase
          → dxvk::D3D11Initializer::SyncSharedTexture
            → dxvk::DxvkDevice::waitForResource
              → DxvkSubmissionQueue::synchronizeUntil → SleepConditionVariableSRW
    ```

    **A TIMEOUT WOULD HAVE BEEN THE WRONG FIX (owner directive), and the dump
    proved it**: all three DXVK workers were parked on their own idle condition
    variables — `dxvk-submit` in `submitCmdLists`, `dxvk-queue` in
    `finishCmdLists`, `dxvk-cs` in `threadFunc` — so every queue was drained
    while `isInUse(Write)` was still true. Nothing was in flight to wait for;
    the reference was leaked. The wedged instruction
    `mov rax,qword ptr [r14+10h]` gave `m_useCount = 0x0000010000000002`, which
    with `getIncrement = 1 << (access*20)` and `Write = 2` decodes as
    **Write 1, Read 0, refcount 2** — exactly one unreleased `acquire(Write)`.
    Both documented deliberate-leak paths were excluded: `m_lastError` is
    sticky and never reset, and the wait did not take its `DEVICE_LOST`
    bail-out, so no device loss occurred.

    **Fix (ordering/accounting, not a bound):** both QFOT sites now take a
    lifetime-only reference, `m_cmd->track(image)`. `DxvkObjectRef` holds a
    strong `Rc` released when the list retires, so the barrier still keeps the
    image alive; genuine content accesses recorded into the list continue to
    track Read/Write normally, so `waitForResource` keeps its real meaning. The
    release side was changed to match — it is not a deadlock source on its own
    (its list is submitted immediately) but an asymmetric pair invites the bug
    back.

    **Verified by A/B under a forced race.** The bug is ~1-in-6 at XAML startup
    and did **not** reproduce in 35 scripted attempts (25 quiet restarts + 10
    `restart-device` churn cycles), because `ExecuteFlush` only *injects* the
    flush chunk and the caller usually beats the CS thread to the wait. Adding
    a temporary 150 ms sleep between the flush and the wait made the caller
    lose that race deterministically:
    - **before:** wedged on iteration 0; instrumented log read
      `QFOT-ACQ res=X inUseWrite=1` → `WFR-ENTRY res=X inUse=1` →
      `UNSATISFIABLE waitForResource … submission queue fully drained`.
    - **after:** 8/8 creates returned, `wedged=0`, every `WFR-ENTRY` read
      `inUse=0`, zero unsatisfiable waits.
    Shipping build re-verified end to end: desktop composites and the Start
    menu renders in full (`Z:\tmp\start_menu_open.png`), plus a
    `restart-device` churn soak.

    **Kept as permanent instrumentation:** a loud check in `waitForResource` —
    if a resource is still in use while `DxvkSubmissionQueue::isDrainedLocked()`
    reports both stages empty, nothing pending can release the reference, so it
    logs the resource, its access bits and trackId once and counts the
    occurrence (`waitForResource STALLED`). It deliberately does not change
    behaviour; bounding the wait would return with the reference still held.
    All investigation-only logging (`QFOT-ACQ`, `WFR-ENTRY`) and the sleep knob
    were removed. To re-provoke the race for a regression test, re-add a sleep
    between `ExecuteFlush()` and `waitForResource` in `SyncSharedTexture`
    (noted in the comment there) and run
    `tools/d3d11_shared_wedge_repro.cpp --clear --watchdog-ms 25000`.

    ⚠ **Read that counter as a RATE, not as pass/fail — the check is a warning,
    not a proof of deadlock.** It fires whenever no completion is currently
    possible, which is terminal only if no other thread will submit the holding
    command list. That is what made 0w fatal (a single-threaded XAML UI
    thread), but **dwm trips it exactly once per start and recovers**, because
    another of its threads submits the list. Verified on the fixed build: dwm
    pid 8184 logged `occurrences=1` at startup, then stayed responsive with a
    live desktop and the count flat. A repeating or never-cleared occurrence is
    the one that matters; one line in dwm's log at startup is expected and is
    NOT a 0w regression.

    ⚠ **Deploy trap that cost two build cycles and invalidated two runs:** the
    default ProgramData UMD hotplug does **not** reach new processes. dxgkrnl
    caches the UMD path at **device** start, so freshly launched processes kept
    loading the previously deployed DLL; two builds' worth of instrumentation
    appeared absent because the old image was still being loaded. Confirm with
    `(Get-Process -Id N).Modules` and deploy with `-KillUmdUsers
    -RestartDevice -NoProbe` whenever the new code must actually run.

    ⚠ **Probe trap:** neither `EnumWindows` nor a `GetWindow(GW_HWNDNEXT)`
    Z-order walk enumerates the Start/Search flyout CoreWindows — nor even
    `Shell_TrayWnd`, which `FindWindowW` finds instantly. "0 top-level windows"
    is NOT proof a flyout is absent; the host process's CPU delta and the
    screen pixels are the discriminators.

    Tooling added: `tools/d3d11_shared_wedge_repro.cpp` (forced-race repro with
    watchdog), `tools/start_menu_repro.ps1`, `tools/start_invoke_probe.ps1`,
    `tools/start_menu_churn_hunt.ps1`, `tools/start_menu_poke.ps1`,
    `tools/start_menu_shot.ps1` (schtasks `helios_startrepro`,
    `helios_startinvoke`, `helios_pokestart`, `helios_startshot`).
0y. **CLOSED 2026-07-28 — the 6-handles-per-device leak, and the ~350-device
    fail-fast with it. ONE root cause for both.** Reported by the ownership
    soak since T5, constant at 5.99/device across T5/T6/T7/T8.

    **Root cause: `helios_umd.dll` and the venus ICD are loaded and UNLOADED
    once per D3D11 device, and nothing releases a module's process- or
    thread-lifetime state on unload.** A Rust `static`/`OnceLock` is never
    dropped, the loader closes no handles a module opened, and tss destructors
    run at THREAD exit — so on a thread that outlives the module they never
    run at all. Measured, not inferred: `GetModuleHandleW("helios_umd.dll")`
    reads NO / yes / NO across one `D3D11CreateDevice` + `Release`, and the
    UMD's once-per-DLL-instance `UMD module:` line appears once per device.

    The six, each named by type, module and creating stack (the module
    attribution is a `LoadLibrary`-pin bisect; the stacks are
    `tools/helios_handle_origins.cpp` + `addr2line` on the mingw DWARF):

    | # | type | site |
    |---|------|------|
    | 1 | `File` | `helios_umd` `log.rs`, the `OnceLock` log handle (`umd-<pid>.log`, access 0x00120194) |
    | 2 | `Event` | ICD `vn_renderer_helios.c:1584` `helios_fence_event_get`, the per-thread tss fence event |
    | 3,4 | `Event`+`Thread` | winpthreads registering the caller's NATIVE thread (event + duplicated thread handle) |
    | 5,6 | `Semaphore` ×2 | libgcc `emutls.c:104` `emutls_init` → `__gthread_mutex_lock` |

    Fixes: the UMD closes its log handle in `DllMain(DLL_PROCESS_DETACH)`
    (`FreeLibrary` case only, `try_lock` so it cannot deadlock under the loader
    lock, refusals counted in `LOG_CLOSE_CONTENDED`); the ICD **pins its own
    module** (`GetModuleHandleExW(..._PIN)`, refusals counted in
    `helios_module_pin_failures`), because four of its five handles are inside
    the statically linked mingw runtime and unreachable from any detach hook we
    could write, and the fifth belongs to threads the module does not own.

    **Measured, 1000 devices / 10 000 resources:** 6.00 → 5.00 (UMD fix alone)
    → **-0.00 handles/device**; working set +3,584 KiB where 300 devices alone
    used to cost +39,048 KiB; modules +0, dwm +0, failures 0/0;
    `OWNERSHIP SOAK PASS`, exit 0. **The WARP control still reads +0.00**, so
    the fix is in the driver and not in the measurement. ★ **The soak also
    completes at 1000 devices for the first time** — 7d(b) recorded that scale
    as unreachable on any build (deterministic `0xC0000409` fail-fast in
    `ucrtbase` between cycle 301 and 400), so the DLL churn was that too.

    Residue, all recorded rather than hidden:
    - The soak now fails on handle **growth**, not on any drift. With the leak
      gone it settled at a fixed −2 (identical at 300 and 1000 cycles), and the
      two are **ALPC Port** handles the Windows RPC runtime closes on its own
      idle schedule inside the window — its old exactly-equal criterion called
      that a Helios defect.
    - The soak's working-set tolerance (16 MiB) is calibrated per **1000**
      resource cycles but the default is 10 000, so the WARP control fails its
      own default scale on working set (+80,484 KiB, linear in the documented
      ~8 MiB). Read the control on its HANDLE number.
    - ⚠ **New hazard from the pin:** a process that survives an ICD redeploy
      now keeps the OLD ICD image loaded alongside the new one. The codebase
      already anticipates two live ICD images (see the
      `helios_venus_query_scanout` comment on decoding a DXVK `VkInstance`),
      but a deploy still wants a `restart-device` or a dwm restart, not just a
      manifest swap.
    - The deeper fix is upstream of both: a DXVK that shares one `VkInstance`
      across D3D11 devices would end the load/unload churn at its source and
      stop paying loader enumeration + ICD load per device. Not attempted here
      — different blast radius.

0ac. **NEW 2026-07-29 (factorial campaign, run D-2x): guest BUGCHECK `0xD1`
   DRIVER_IRQL_NOT_LESS_OR_EQUAL in `helios_kmd_render.sys` — on
   22.22.212.0, the PRE-LEASE build**, 41 s into a Fire Strike GT1 run at
   BindFlushMode=0. Read of `0x00000001'59b430d0` at DISPATCH_LEVEL from
   `helios_kmd_render+0x21076`; two frames up the stack sits the VALID kernel
   pointer `0xffffd38f'59b43050` (same region, high dword ffffd38f→00000001,
   low +0x80) — shape of a torn/truncated 64-bit pointer dereference.
   WinDbg bucket (public symbols, nearest-symbol hint only) names the
   virtio transport / `WdkHal` / `DxgkConfigAccess` region. **Evidence
   preserved**: kernel dump `C:\HeliosDumps\MEMORY-D2-212-mode0-20260729-2002.DMP`
   (1 GB), minidump `C:\Windows\Minidump\072926-6359-01.dmp`, counter polls to
   t+41 s and the oracle slice under
   `tmp/handoff-0ab-b-lease/analysis/logs/D-2-VOID-bugcheck.*`. Intermittent
   (~1 in 10 GT1 runs); the same config re-ran clean before and after.
   3DMark logged `workload did not respond` for 11 s before the crash while
   the oracle still saw 390–490 flushes/s — the graphics pipeline was live,
   the workload's IPC was not. **This retires "treat the lease gate as the
   prime suspect" for the 2026-07-29 19:08 hard hang** — the class reproduces
   on the rollback build.
   **TRIAGED 2026-07-29 (no matching PDB — resolved via `.pdata` bounds + a
   487/506-byte match against the current build's `.map`)** →
   `tmp/handoff-0ab-b-lease/analysis/bugcheck-d1.md`. The faulting function is
   `AdapterContext::with_virtio` reached from `hpd_thread_routine →
   process_deferred_vidpn_source_address → … → arm_bind_refresh`; the
   dereference is the `Option<VirtioGpu>` discriminant test at `+0x80`
   (`locks.rs:263`), first touch after `virtio_lock`. The CONTEXT record shows
   `rsi/rdi/r13/rbx/r12` correct and **`r14` alone** holding
   `0x00000001'59b43050` where `&AdapterContext` belongs — corrupted ACROSS
   `KeAcquireSpinLockRaiseToDpc` (the consecutive `mov r14,rcx` /
   `lea rsi,[rcx+0xb28]` had a good `rcx`; the real `virtio_lock` reads held).
   Ruled out with dump evidence: torn reads, in-flight reuse, DMA recycling,
   stack overflow, pool corruption, enlightened-spinlock path
   (`HvlEnlightenments=0`), lease code. Since no x86-64 instruction writes
   only a GPR's high dword, `r14` was RESTORED from a damaged image:
   **H1** a VM-exit register round-trip (PLE exits during the contended spin,
   `ple_gap=128`; contention was climbing — ISR 521→943/s in the final 10 s) /
   **H2** a 4-byte `1` over a saved-`r14` stack slot in the DIRQL interrupt
   chain (our ISR is verifiably clean; the INTx line is shared with the
   balloon, 29th-session memory) / **H3** marginal host CPU / **H4** the
   `hv-*` enlightenment set changing exit paths. Per the host-proven-good
   rule none is promotable without host-side evidence.
   **Mitigations landing in 22.22.217.0 (build 1):** the `WvTorn` tripwire in
   `with_virtio` (converts a recurrence into a counter + graceful Err instead
   of a bugcheck) and PDB archiving next to every deployed `.sys`.
   **OWNER-GATED discriminating A/B (the actual fix path — host config only):**
   ≥20-run GT1 loops under (a) baseline, (b) `kvm_intel ple_gap=0`,
   (c) `hv-spinlocks`/`hv-avic`/`hv-evmcs` dropped, (d) balloon device removed
   (shares IRQ 22); watch `dmesg -w` + `/sys/kernel/debug/kvm/*/pause_exits`
   during the loops. (b)/(c) stopping it ⇒ H1/H4; surviving all ⇒ H2/H3.

0ad. **NEW 2026-07-29 (metric validation): the fullscreen→desktop transition
   sends one undersized `SET_SCANOUT_BLOB` and the host readback goes dark for
   ~250 ms.** In every trace, at the transition, res 6 arrives with
   `blob_size 4096000` (= 1280×800×4, the LINEAR size) against the OPTIMAL
   import's `required=4587520`; the vk-optimal path refuses the shape, the
   vk-linear import is rejected for all usages, and the egl-texture path
   refuses the modifier-less reinterpretation (`glEGLImageTargetTexture2DOES`
   0x502). `egl_scanout_dmabuf` had already deactivated the previous readback
   on entry, so NO readback exists until the next successful bind ~250 ms
   later. ⚠ The refusals themselves are correct — the undersize guard is
   Xid-31 protection and must NOT be relaxed (38th-session memory); the defect
   is guest-side: whatever binds res 6 at the transition presents a
   LINEAR-sized blob to a path that needs the padded OPTIMAL size, and the
   gap in coverage is user-visible as a transition blackout.

0z. **NEW, 2026-07-27 (R614 gate): the Mesa venus ICD does not survive adapter teardown —
   `pnputil /restart-device` ACCESS-VIOLATES every process holding a venus device.** Each
   restart-device cycle logs Application-log id 1000 faults with
   `Faulting module name: vulkan_virtio-<hash>.dll, Exception code: 0xc0000005` for dwm.exe plus
   3-4 shell processes (Explorer, SearchHost, StartMenuExperienceHost, ApplicationFrameHost,
   ShellExperienceHost). The desktop self-recovers — dwm restarts and composites, which is why
   this was never noticed — but "a new dwm pid is expected, not a crash" was too generous: it IS
   a crash, just a survivable one. The same cluster appears at every clean shutdown/reboot.
   **PRE-EXISTING, NOT caused by R614, and the evidence is unambiguous:** 889 such faults in the
   Application log spanning 2026-07-07 → 2026-07-27, and **T4a's own nine-consecutive-restart-device
   soak on 22.22.184.0 produced them on every cycle** (08:56:45, 08:56:58, 08:57:12, 08:57:25,
   08:57:39 … a ~13 s cadence matching the nine cycles).
   **Why every gate so far called that soak clean:** the gates check the SYSTEM log for
   4101/dxgkrnl/LiveKernelEvent, and those stay legitimately empty — a user-mode ICD access
   violation is not a TDR. Nothing checked the APPLICATION log. `tmp/r614-tdr-check.ps1` and
   `tmp/r614-icd-fault-history.ps1` now do; fold them into the standing gate.
   Next step is a stack: the deployed UMD/ICD PDBs are present, so
   `tools/take-minidump.ps1 -ProcessId <dwm>` under a restart-device, then
   `minidump-stackwalk` on Linux, should name the ICD site directly. Most likely shape is the ICD
   touching a venus object (or its ring/reply BAR mapping) after StopDevice destroyed the host
   context — i.e. the ICD has no adapter-loss path, which is the same class as the 17th-session
   DEVICE_LOST chain but on teardown rather than on a slow present.
   **2026-07-29 factorial campaign: reproduced 6/6** — every `pnputil /restart-device`
   crashed dwm/Explorer/SearchHost/StartMenuExperienceHost/ApplicationFrameHost in
   `vulkan_virtio-*.dll` (0xc0000005); desktop recovered within ~10 s each time
   (screenshot-verified before every run). restart-device is NOT a free operation.

0a. **Ghosting — ROOT-CAUSED AND FIXED IN LAYERS (21st session, 2026-07-06).** Owner
   insight proved out: the dirty-rect *attribution* was fine; STUTTER caused the
   ghosting. Evidence chain (all same-day): dwm's composed primary is paintcap-CLEAN
   under a bouncing-window probe while the LG client trails catastrophically → the
   corruption is in the IDD→KVMFR→client delivery; 1843 consecutive IddCx acquires
   show frame-delta exactly 1 (no skipped presents) and zero move-regions; WUDFHost
   consumer waits showed timeouts=0 while trails persisted → the wait "succeeded"
   against the WRONG instant. Root cause: the WS1 #4 consumer wait ran at cmdlist
   START (refreshHeliosStagedImages), re-reading the publish slot when the list
   BEGAN — under load that predates the acquire the list's copy serves by a full
   consumer cycle, so the wait targeted an already-retired value and the copy read
   ring-stale content inside freshly-reported damage rects. Fixes landed:
   - dxvk-helios `6eab004c`: bounded present-wait ON THE CS THREAD at copy-execution
     time (copyImage/copyImageToBuffer, imported sources). The acquired buffer can't
     be re-presented while held, so the slot value at that moment IS the acquired
     present's value. Silent no-slot returns (unordered reads) + fast-path hits now
     counted in the `present-wait:` line (`fast=`, `noslot=`).
   - dxvk-helios `35fe0912`: WUDFHost.exe app profile heliosPresentWaitUs=500000 —
     the copy-time wait exposed real producer lag (dwm fence frozen 250ms+ behind
     its publishes under churn; 32ms bound timed out exactly when ordering mattered).
     dwm keeps the tight 32ms default.
   - LGIdd `13630d7f`: D3D11 readback path (the ACTIVE path — D3D12 device creation
     fails 0x887A0004 by design, the D3D12 partial-copy path is DEAD code) finishes
     the acquired frame AFTER the staging Map (was: at CopyResource submit — dwm
     could re-render the buffer before the copy executed), reuses the staging
     texture (was: CreateTexture2D per frame = venus alloc/free per frame), adds
     per-stage QPC telemetry (`D3D11 path stats:` 1 line/300 frames), and a
     `HeliosForceFullDamage` registry knob (diagnostic; owner-verified to hide
     ghosting).
   - LGIdd `37356f83`: pending-damage debt — frames dropped after their dirty rects
     were consumed (LGMP queue full fired exactly at the login→desktop transition:
     giant permanent cold-boot ghosts, owner screenshot) bank their damage for the
     next delivered frame; new-subscriber re-post now carries full damage
     (damageRectsCount=0) instead of the last frame's partial rects.
   - LGIdd `5d36c512`: IddCx MOVE REGIONS delivered as damage (DestRect per move) —
     previously never queried, silently dropped (window drags are moves+edge-dirt;
     zero moves observed from programmatic MoveWindow, so this wasn't the trail
     cause, but it was a real hole).
   - LG client `b27eb1d0`: malformed frames (zero geometry) and transient
     onFrameFormat failures skip-and-retry instead of exiting — a client exit tears
     down the whole VM via the launcher (observed live: render-server killed dwm's
     venus context → torn frame → client exit → VM shutdown).
   Remaining for 60fps IDD (owner target): the serialized IDD cycle measured
   map(wait-inclusive)+memcpy ≈ 60-95ms under churn. `AllocCached` KMD fix (22.22.53)
   targets the 36ms WC-read memcpy; producer completion lag (dwm CS submission lag +
   a RECURRING ~1.49s stall, constant across configs — hunt with QMP fence tracing)
   is the other half. See WS2. **22nd session: the ~1.49s stall is ROOT-CAUSED AND
   FIXED (our own staged-probe diagnostics — see WS2); present-wait timeouts now 0.**

0b. **NEW DEFECT — venus pipeline-layout lookup failure killed dwm's context
   (2026-07-06, host-side evidence).** virgl render server:
   `vkr: failed to look up object 626238 of type 19 (pipeline layout)` →
   `vkCreateGraphicsPipelines resulted in CS error` → fatal decoder state → context
   1613 (dwm.exe) destroyed → all subsequent blob creates refused → LG client choked
   on the torn frame and exited → launcher shut the VM down. First host-side proof
   of a guest venus protocol violation.
   **22nd session (2026-07-06): the fork-early-out suspicion is FALSIFIED as the
   cause.** Static audit: vn_pipeline.c/vn_common.c are byte-identical to upstream
   (the relaxed tail load in wait_all is upstream's own); the fork's divergence is
   confined to vn_ring.c. Evidence: the never-read ring-diag file at
   `C:\Windows\Temp\helios_icd_diag.log` (13.5 MB across 6 boots) shows the
   shared-valid early-out NEVER fired and the wait_seqno fatal-abandon fired
   exactly twice — both AFTER the host had already latched ring-FATAL
   (consequences of the CS error, not causes; on the death boot the first
   guest-side anomaly IS the post-FATAL abandon, head frozen at 141452 with
   ~1.2 KB undecoded). The sync-vs-async structure is also sound: TLS-ring
   pipeline creates are `vn_call_` (synchronous), so destroy-after-create races
   are excluded. The initiating violation left NO guest-side trace. Landed
   (mesa `a0412461012`, ICD `vulkan_virtio-7c9ddf378055` deployed):
   `vn_ring_wait_all` barrier skips/abandons now log LOUD
   (`BARRIER SKIPPED/ABANDONED in wait_all`) to the ProgramData diag log —
   either line on a healthy process is the smoking gun; ring diag rerouted
   from `C:\Windows\Temp` (unwritable for WUDFHost — its ring diags silently
   vanished) to `C:\ProgramData\Helios\helios_icd_diag.log`; post-fatal
   per-submit spam rate-limited (power-of-two); NEW env knob
   `VN_HELIOS_PIPELINE_TRACE` traces (ring, primary_tail seqno, object id)
   for layout create/destroy + the barrier + pipeline creates. NEXT on
   recurrence: read the BARRIER/PL-TRACE lines before theorizing; host-side
   `HELIOS_VKR_DEBUG=validate` relaunch (ask owner) — NOT VIRGL_LOG_LEVEL
   (HOST.md §5.1: venus runs in the render-server child; only WARN+ reaches
   the qemu stderr log).

1. **Stall — ROOT-CAUSED AND FIXED (18th session, 2026-07-06).** The strike attribution
   (mesa `f7a816f182f`: sem id, wait reason, signal queue/family/ring, signal value+age in the
   strike line) cracked it in one boot: **every observed strike had `sig_age_ms ≤ 14`** — the
   waiter had legally parked a wait-before-signal on the NEXT frame's timeline value while the
   desktop idled; the moment the next frame's burst submitted the signal, the old deadline
   (which measured time-since-counter-movement, gated only on a-signal-is-pending-NOW) fired
   against a milliseconds-old signal. Explorer striking at exactly hh:mm:00 = the taskbar clock
   tick submitting that signal. The strikes were false positives BY CONSTRUCTION on an idle
   desktop, and 4 of them during login churn are what tripped the DEVICE_LOST latch (act one
   of the 2026-07-05 freeze cascade). FIX: the deadline now measures how long a submitted
   signal has been pending with zero movement (`pending_signal_since_ns`); a genuine zombie
   (Xid-109: pending forever) still latches at strikes×deadline. RESULT: strikes went from
   ~1/min/process to ZERO across dwm+explorer+WUDFHost (fixed ICD, ~10 min incl. idle).
   The second stall contributor — the rotate drain — is fixed under WS2 (presents now track
   damage rate: ~10/s under the 10 Hz flasher vs ~6/s before). 19th-session soak data
   point (2026-07-06, warm boot): ZERO non-forced strikes and zero DEVICE_LOST over the
   whole session (~1 h incl. heavy GPU probe workloads); present-gate timeouts flat at the
   startup value; rotate-perf 1–4 µs; HELIOS_QUEUE_PERF live in dwm, submit phases all
   µs-class (submit_avg ~4 µs). Remaining: multi-hour/day soak.
2. **dxvk-helios device-loss hygiene — FIXED and FORCED-LOSS-VERIFIED (19th session,
   2026-07-06):** (a) dropped post-loss submissions now `notifyObjects()` + recycle (was:
   permanent in-use ref leak → dwm compositor wedge); (b) `waitForResource` bails on
   DEVICE_LOST (loud warn); (c) on lost, bounded 2 s grace wait then deliberate cmdlist
   leak instead of resetting a pool with host-pending buffers (was: the vkResetCommandPool
   VUs). End-to-end forced-loss test PASSED (`VN_HELIOS_SEM_DEADLINE_MS=1` +
   `_STRIKES=1` on d3d11_upload_integrity_probe): latch fired on the first genuine 1 ms
   pending window with full attribution, probe ran to completion with loud FAILs (reads
   return zeros post-loss) and exited — no wedge; all three hygiene paths logged in the
   dxvk log (`leaking command list with unretired host work`, `waitForResource aborted`,
   repeated loud submit failures); dwm untouched. Note: once the ICD latches, host
   retirement becomes unobservable, so the deliberate leak fires even on a healthy host —
   by design.
3. **dwm shared-resource creation failure — FALSIFIED as written (18th session, 2026-07-05)**:
   every `DXVK memory not importable` line (current boot AND the freeze-evidence tail) belongs
   to `misc=0x0` PRIVATE textures, where suballocation is correct. Shared/present creations
   already get dedicated importable memory (`forceDedicated`, dxvk_image.cpp:634): dwm's
   backbuffers allocate `kind=DEVICE_MEMORY` blobs (blob=venus mem id, KMD creates the wire
   resource) and the IDD imports them (resids 32/33/36, live content probe-verified). The
   `res_id=0` in the allocate log is normal for KMD-created blobs, not a failure. Log line now
   scoped: loud `SHARED RESOURCE WITHOUT IMPORTABLE BACKING` only when the resource actually
   needs one (shared/keyedmutex/present/primary); private-texture noise trace-gated. The IDD's
   per-acquire re-resolve is the alias-staging refresh design, not a missing-export symptom.
   Residual noise, harmless: dwm warns `Failed to write shared resource info` 9× (dxvk shared
   metadata runs before the UMD stamps KMT handles; WINE-escape fallback fails by design).
4. **KMD wire-fence semantics / consumer-side present ordering — MECHANISM PROVEN
   END-TO-END (19th session, 2026-07-06).** `tools/vk_ring_fence_probe.cpp` (schtasks
   `helios_ringprobe`, MEDIUM integrity — see tooling gotcha) demonstrates the full chain
   on the live stack: vkQueueSubmit2 signaling an exported win32 timeline semaphore →
   vn ring_idx≥1 fence-only SUBMIT_VENUS (`vkWaitRingSeqnoMESA` cs, orders the fence
   behind the ring-decoded queue submission) → KMD INFO_RING_IDX wire fence (the transport
   already honored ring_idx; in-flight table is per-fence token-matched, out-of-order-safe)
   → QEMU 11.0.1 → proxy → render-server vkr queue sync thread → host GPU completion
   (qemu fence trace: retire at 263 ms on a 269 ms workload; ring-0 retires in µs at
   decode) → used-ring completion → **new ICD retire thread** (mesa `4a6aa14f17b`: on
   every SHARED-sync signal, a per-renderer thread WAIT_FENCEs the wire fence and signals
   the shared WDDM monitored fence in retire order; helios_sync now refcounted; also fixed
   the `helios_sync_append_locked(fence_id=0)` blind-signal that cleared older pendings)
   → consumer `vkWaitSemaphores` on the re-imported semaphore returns at 97–98 % of
   T_gpu, never early. KMD `22.22.52` (BUILT, NOT yet installed — needs owner reboot):
   WDDM pending-FIFO watermark now counts ring-0 fences only (ring≥1 stay in flight for
   the whole GPU-work duration; counting them would couple GDI/paging DMA pacing to
   multi-ms GPU work) + RING_SUBMIT/RING_COMPLETE counters (TDR report v4).
   **PRODUCTION INTEGRATION DEPLOYED (20th session, 2026-07-06) — A/B running with
   `PresentGateUs=0`.** The design changed twice against reality:
   - **KMT is impossible**: dxgkrnl rejects a MONITORED fence with `Shared=1` and no
     `NtSecuritySharing` (0xc000000d, proven live), i.e. no global-DWORD flavor exists.
     The ICD's legacy-D3DDDI_FENCE fallback silently produced syncs whose FromCpu waits
     hang forever (wedged the probe) — fallback REMOVED, KMT semaphore caps no longer
     advertised (mesa `d5d698aaec5`). Rendezvous = **NAMED NT sharing**: standard
     `VkExportSemaphoreWin32HandleInfoKHR::name` / import-by-name
     (`D3DKMTShareObjects` w/ named OBJECT_ATTRIBUTES → `D3DKMTOpenSyncObjectNtHandleFromName`).
     dwm CAN create `Global\` names (SeCreateGlobalPrivilege verified on its token).
     `vk_ring_fence_probe named` rc=0 (98 % of T_gpu; NT regression 97 %).
   - **LGIdd needed NO changes**: its per-acquire copy runs on OUR UMD/dxvk inside
     WUDFHost (that instance imports dwm's backbuffers as alias images — resids
     probe-verified) — the consumer wait lives in dxvk-helios's
     `refreshHeliosStagedImages`, `dxvk.heliosPresentWaitUs` (default 0, WUDFHost.exe
     profile = 100000; imported-mode images only, so dwm never grows a CS-thread wait).
   Shipped shape: producer (`present_sync_publish`, umd `9d22620`+`a7f2dfa`, dxvk
   `b15bc42f`+`8c612b3d`) creates one named fence per D3D11 device
   (`Global\HeliosPresentFence_<pid>_<fenceId>` — dwm owns SEVERAL devices, per-pid
   names collided live; Everyone-DACL), records signalFence(++counter) on the frame's
   OPEN cmdlist (no present-thread wait), publishes (resid → pid, fenceId, value) in a
   HPS2 seqlock-slotted mapped FILE `C:\ProgramData\Helios\helios_present_sync_v2.bin`
   (4096 fixed 32-byte slots; 131104 bytes total; both
   principals have ProgramData rights; do NOT delete the live file — mapped views
   split-brain until the mappers restart). Consumer imports by name (cached per
   pid+fenceId), `getValue` fast-path, bounded wait, timeout → copy anyway + loud
   `present-wait:` telemetry. Kill switches: `HKLM\SOFTWARE\Helios!PresentSyncPublish=0`
   (producer), `dxvk.heliosPresentWaitUs=0` (consumer). Deployed: UMD
   `helios_umd_8301bc9779a48b99.dll`, ICD `vulkan_virtio-cf1280da750c.dll`, verified in
   dwm+WUDFHost; cross-session import proven live ("imported fence of producer pid").
   First numbers (gate=0, 10 Hz flasher stress): ~900 consumer waits, avg 5–6 ms,
   ~4 % bounded timeouts in bursts when dwm's CS runs >100 ms behind (structural:
   the consumer chases the latest published value; bounded + loud by design), ZERO
   strikes, desktop paintcap-clean, submit_avg ~4 µs unchanged.
   **Owner cold-boot report (same session): ghosting + frame drops PERSISTED — the
   WUDFHost-only scoping was WRONG.** The old gate had been ordering EVERY producer's
   present (apps included), so registry gate=0 also unordered the app-backbuffer →
   dwm-composition edge; the artifacts were in dwm's OWN composed primary (stale
   sliver + content overhang in the owner's screenshot). Fix (dxvk `90b76c5c`, UMD
   `helios_umd_f2c6f833d7d293eb.dll`): `heliosPresentWaitUs` defaults to **32000 for
   every consumer** — the wait fires only for imported surfaces with a published
   slot (dwm/IDD cross-process edges), the wait DAG is acyclic (IDD → dwm → apps),
   every edge bounded; uniform 32 ms also caps the IDD stall bursts that read as
   frame drops (was 100 ms). Verified live: dwm imports app producer fences
   ("imported fence 1 of producer pid <app>"), zero dwm-side timeouts, churn
   paintcaps coherent. Timeout warns now log the fence's current value — churn
   bursts are retire-lag (publish outruns GPU completion by a few frames).
   **Remaining:** owner eyeball via Looking Glass + multi-hour soak with gate=0;
   then retire `PresentGateUs` (flip compiled default to 0) and drop the gate from
   the hot path. Differential levers if ghosting recurs: `DXVK_CONFIG
   "dxvk.heliosPresentWaitUs = N"` per process, or registry `PresentGateUs=32000`
   restore. Known residue: old-ICD probe runs leaked 4 idle render-server workers
   host-side (virgl-63/65/95/97); clears on VM restart, did not recur with the new ICD.
5. **dxgkrnl "Driver returned an invalid NTSTATUS 0xC00000BB"** (ETW
   AzureTriage) — some query answered STATUS_NOT_SUPPORTED where that return
   is illegal. Tolerated today; find and fix the query.
6. **WUDFRd cold-boot race** ("SCM not ready", boot+23s) — LGIdd loads late;
   pairing is resilient now but the race window is still there.
7. **In-place KMD update flakiness** — CM_PROB_FAILED_POST_START limbo until
   reboot is expected, but keep the version-coherence gotcha (one site since
   2026-07-26: `kmd_render/driver-version.env`) and
   backup ladder in mind. 2026-07-06 state (19th session END, reboot-verified): ACTIVE
   driver = **oem59.inf = 22.22.52 (pkg 155b7345f9360525)** — per-ring watermark +
   counters live. `UserModeDriverName` → ProgramData `helios_umd_b3615be0ce9de13e.dll`
   (RELEASE profile), dwm ICD = **`vulkan_virtio-5535366186bd.dll`** (retire thread) —
   both verified by the fresh dwm's loaded-module list + paintcap. **NEW GOTCHA: a KMD
   `devcon update` install creates a new DriverStore dir and RESETS `UserModeDriverName`
   to the DriverStore copy, which ships the package's DEBUG-profile UMD — after EVERY
   KMD install, rerun `win_install_umd` (release dll + `-KillUmdUsers -RestartDevice
   -NoProbe`) and re-verify dwm's loaded module. The DriverStore UMD copy staying locked
   during that redeploy (script exit 1) is benign — ProgramData + registry are what
   load.** Backup: `C:\ProgramData\HeliosDeployBackups\20260706-021734`.
8. **GDI byte-path / executor retirement (REFRAMED 20th session)**: post-cold-boot,
   GDI content renders while RenderGdi (GdiE), MapCpuHostAperture (ChMn) and
   paging (Pg*) counters all stay idle — the canonical CPU path likely already
   carries the bytes. RESEARCH CONFIRMED (2026-07-06): GDI HW acceleration is
   OPTIONAL (`SupportKernelModeCommandBuffer` is a "should… only if" per
   gdi-hardware-acceleration.md); **viogpu3d** (vendored kvm-guest-drivers, the
   closest existing WDDM driver to Helios, near-identical cap set) never sets the
   bit and implements NO RenderKm/RenderGdi; WARP works render-only in the IDD
   slot. The in-tree "LOAD-MANDATORY" bisect (query_adapter_info.rs:166,
   2026-07-02 v22.22.34) predates Option A and is confounded by the then-broken
   CPU-visible story — retest under BarSegMode 10 via a new `GdiAccelMode`
   service-key knob + pnputil restart-device (AzureTriage names any FAILED_ADD).
   If the desktop renders accel-off: verify byte flow (GdXn stays 0, watch for a
   two-memory-split regression = black/stale GDI windows), verify the BAR CPU
   mapping is WB-cacheable (CPU GDI is read-modify-write), re-run the Doom
   stutter differential (its WSI BitBlt storm currently traverses the executor),
   then retire the gdi_blit executor (a guest-CPU blitter behind a DMA round
   trip — strictly worse than win32k's rasterizer, source of the 48%-drop bug
   class).
   **21st session (2026-07-06): both knobs SHIPPED in 22.22.53** (`85ad16a`):
   `GdiAccelMode` (default 1) gates SupportKernelModeCommandBuffer per the plan
   above; `AllocCached` (default 1) additionally answers the WB-cacheable
   question for ALL CpuVisible allocations — user views were mapped WC (only
   CpuVisible was set at create_allocation; the WDDM2 `Cached` flag was never
   set), measured at ~200 MB/s reads = 36 ms per 7.8 MiB IDD readback frame.
   **22nd session: GdiAccelMode=0 A/B PASSED — the 2026-07-02 "LOAD-MANDATORY"
   bisect is OVERTURNED under BarSegMode 10.** Evidence (all same-boot, via
   `pnputil /restart-device`): adapter re-adds `CM_PROB_NONE`; desktop composes;
   classic-GDI text renders (fresh cmd-console canary echoes + regedit tree text
   + desktop-listview labels, paintcap-verified); every Gd* counter FROZEN this
   boot (GdiE 339, GdXn 24 — zero executor traffic; the KMD's `GdiM` mirror
   flipped 1→0 proving the mode took); no two-memory-split regression under
   mover churn across three device restarts. **The knob is LIVE at 0 for soak**
   (service-key value set; revert = `reg delete ...\helios_kmd_render /v
   GdiAccelMode /f` + restart-device). NEXT: owner-attended Doom stutter
   differential (its WSI BitBlt storm no longer traverses the executor), then
   retire the gdi_blit executor + flip the compiled default.
   **CLOSED 2026-07-26 (T1b, 22.22.180.0)** — owner directive: the driver does
   not and should not advertise GDI acceleration, so both the advertisement and
   the executor are gone (R903/x-dup-dead-20, pulled forward from T6).
   Reachability was re-proven the same boot before deleting anything: every
   `Gd*` service value deleted, then explorer restart + maximized notepad + GDI
   canary + repaint + EnumWindows + two paintcaps, and **not one `Gd*` value
   reappeared** (the executor flushes its block on its first batch, so absence
   is proof, not throttling). `SupportKernelModeCommandBuffer` is now hard-coded
   0; `DxgkDdiRenderGdi`/`RenderKm` stay registered as record-and-advance
   (a null slot bugchecks — `DdiRenderGdi+0x140`). This also deletes T1b's
   R301/R304/R305/R306, which only hardened the deleted executor.

## Workstream 2 — Performance

- **THE PRESENT BLOCK IS ATTRIBUTED AND HALVED (2026-08-04/05, KMD
  22.22.243.0 → 22.22.244.0).** `umd_present_callback` (548–661 µs/frame, the
  single largest ours-attributable cost on the app's render thread) is not CPU
  and not our `DxgkDdiPresent` (7.9 µs mean): it is **one dxgkrnl wait**.
  A `Microsoft-Windows-DxgKrnl` ETW slice names it — `BlockThread` `Reason=2`
  on 21.1 % of presents, mean 2448 µs, **516 µs amortised = the whole callback**
  — and pins the mechanism exactly: **89 of 91 blocks began with exactly 3
  `PresentQueuePacket`s outstanding** (non-blocked presents saw 0/1/2) and
  **90 of 91 unblocks landed within 200 µs of a `PresentQueuePacket Stop`**
  (median 12.1 µs). dxgkrnl allows three outstanding present packets; the
  fourth present blocks until one retires.
  **Why the queue filled was OUR defect:** `note_wddm_submission` gated every
  non-paging WDDM fence on `async_retired_up_to(next_wire_fence, IncludingGpu)`
  — *every transport entry enqueued before the buffer* — although each
  submission already carries its exact dependency (`stream_ready` on its live
  present stream boundary, present on 95.8 % of them: `PmHit` 12452 /
  `PmLeg` 13003). The DXVK CS thread runs ahead of the presenting thread, so
  the superset routinely covered LATER frames' work. `arm_dma_flip`'s 0ab-B
  note recorded the same over-wait from the flush side in 22.22.210.0 and fixed
  it there; the fence path kept it.
  **Fix: `PresentWmk` (service key, default 1 since 22.22.244.0; `0` is the
  same-boot A/B disable).** A submission carrying a LIVE boundary is gated on
  that boundary alone. FIFO order, monotonic fence completion, the
  stale-generation cancellation path, paging, WindowedBlt admission and every
  scanout bind/flush ordering rule are untouched.
  **Measured** (same boot, `pnputil /restart-device` between cells):
  `DxgkDdiSubmitCommand`→DMA_COMPLETED 5.825→4.854 ms mean, 6.110→3.995 ms p50;
  flip packet lifetime 8.594→7.398 ms; `umd_present_callback` 626–661→359 µs;
  presents/run 6793–6892→7184. **GT1 +4.30 % / +3.74 % paired**, and on the
  canonical STANDARD preset **Graphics 45 365 / 46 500 → 47 875 (median
  45 933 → 49 405, +7.6 %)**, GT2 carrying most of it (183.8/190.2 → 196.7–211.7).
  Host-GPU envelope barely moves (mean 59.8→60.7 %, p50 64→68 %): the extra
  frames come out of the guest, and the host still has headroom.
  **The residual is now NAMED, not guessed:** `.244` adds `WfBWire` /
  `WfBStrm` / `WfBBlt` (exactly one moves per blocked look at the WDDM FIFO
  head). One GT1 gives `WfBStrm=15482`, `WfBWire=93`, `WfBBlt=0` — the
  over-wait is gone (0.6 %), and what paces retirement is `stream_ready`, i.e.
  the frame's own producer completion on the host. That is physics. Its shape
  is bursty: flip packets retire in **106.6 bursts/s, mean size 2.20,
  inter-burst gap p50 10.06 ms**, so the app issues three presents and waits
  for the next burst — **that burst cadence is the next question.**
  Frame budget under the fix: **producer 3.716 ms (p10 3.273) + kernel
  0.571 ms = 4.288 ms**; even a perfect kernel leaves ~269 fps.
  Artifacts: `tmp/perf/present-watermark-arm/` (prediction + outcome),
  `tmp/perf/flip-queue-arm/` (the REJECTED `MaxQueuedFlipOnVSync` arm and the
  ETW that replaced it), `tmp/perf/etw-slice-242/`, `tmp/perf/etw-slice-pwmk1-243/`,
  `tmp/perf/fs-std-244/`. Reusable: `tmp/perf/run-gt1-arm.ps1` +
  `launch-gt1-arm.ps1` (feed trace + counters + read ledger + scanout timeline
  around one GT1, `-Extra NAME=VALUE` for env-knob arms),
  `tmp/perf/ab-presentwmk.ps1` / `ab-env.ps1` (interleaved A/B — GT1 drifts
  across a session, so all-A-then-all-B cannot separate knob from drift),
  `tmp/perf/sample-host-gpu.sh` (host-side GPU envelope).
  **REJECTED, do not retry:** `MaxQueuedFlipOnVSync` (the `FlipQueueN` knob,
  default 1) at depth 4 with and without `FlipOnVSyncWithNoWait` — inert on
  both fps and present-callback time; and
  `HELIOS_DXVK_LOCAL_ALLOC_CACHE_FALLBACK=1` **re-tested under the fix**
  (+1.41 / −0.60 / −0.07 % paired), which also kills the "the present block was
  absorbing its CPU saving" hypothesis.

- **THE FRAME IS ATTRIBUTED (2026-08-02, 65th session, measurement-only):
  GT1 is guest render-thread bound; the host GPU idles at ~35 % busy
  (~200 W of 400 W) and NOTHING else in the pipeline is saturated** —
  venus decode thread 36 % of a core (decode-saturation KILLED), QEMU main
  loop 13 %, dxvk-queue/cs/submit 38/24/6 %, no vCPU over ~70 %. The app's
  render thread runs 88–93 % and RIP+stack sampling (no build; PDB-symbolized)
  decomposes it: ~8 % COM refcount churn (samplers alone 5.6 % — R8 is real),
  ~6.5 % DXVK CS chunk-pool mutex contention (`AllocCsChunk` parks),
  ~5.4 % DxvkMemoryAllocator on the Map-DISCARD path, ~5.1 % heap, 5.6 %
  d3d11-runtime device-critsec contention under `CUseCountedObject::Release`
  (named via MS public PDBs — msdl is curl-reachable, recipe in the report),
  3.6 % frame gate (contract), 1.2 % log.rs with tracing off. Wire path is µs-class (submit 40 µs,
  escape ~100 µs, retire 0.75 ms). Full report + ranked levers:
  `tmp/handoff-perf-saturation/reports/p1-attribution.md`; raw artifacts in
  `tmp/handoff-perf-saturation/logs/`; the samplers (reusable) in
  `tmp/handoff-perf-saturation/tools/`.
  **Lever outcomes (2026-08-03, same session; canonical config =
  owner-picked STANDARD FS preset, task `helios_fs_std`, baseline
  GT1 170.6 / GT2 202.2 / Combined 21.38 ⇒ Graphics 42.6k):**
  LEVER 1 LANDED (`e610d1c` — COM churn out of the binding DDIs; refcount
  samples 8 %→1.5 %; Combined 21.4→≥22.9 on every subsequent run, best
  25.26; GT1 likely +3–6 % but GT1 single runs swing ±5 % on identical
  code — use 3-run medians; all stability gates + GT2 oracle green).
  LEVER 2 NEGATIVE, reverted: the "chunk-pool contention" was a
  stack-scan artifact — those parks are the FRAME GATE's
  `SynchronizeCsThread` CS-drain; corrected gate cost ≈9.6 % of the
  render thread (~0.55 ms/frame; DXVK's `CsSyncCount`/`CsSyncTicks`
  already instrument it). LEVER 3 reverted: two real discoveries — the
  per-context `DxvkLocalAllocationCache` mask is ZERO on venus (no
  DEVICE_LOCAL+HOST_VISIBLE+HOST_COHERENT global-buffer type), and GT1
  runs ~11 M small (48–128 B) buffer allocations through the locked
  allocator per run (~950/frame, likely with a failed-properties attempt
  + fallback each); enabling the cache (mask sans DEVICE_LOCAL → 99.7 %
  hit rate) coincided with GT2 202→193–195, confounded by late-session
  drift — unproven either way, do not re-land without answering why a
  live cache would cost GT2 through venus. Run table + honest noise
  discussion: report §3c.

- **HISTORICAL OPEN MEASUREMENT (2026-07-26, found while taking the T0 baseline):
  DComp producer cadence ~50 fps, not the then-documented ~63; present-gate avg
  ~2.0 ms, not the then-documented ~0.48 ms.** Measured on an
  idle box (CPU ~1 %) with `helios_dcomp_probe` (25 s runs): 1236 / 1152 / 1253 frames
  **before** the T0 deploy and 1227 / 1307 **after**, i.e. 46–52 fps throughout — so this
  is NOT a T0 effect and not a debug-vs-release-UMD effect. An earlier stored run of the
  same probe on this box recorded 1576 frames (63.0 fps), which is where the documented
  figure comes from. dwm `present-gate:` reads avg 2018 µs / max 14595 µs / 30 timeouts in
  3072 presents on the release UMD. This count is a producer-side DComp observation,
  not proof of a visible fullscreen scanout defect. The 2026-08-04 owner correction
  verifies native SDL smooth and retracts the prior SDL hold/burst report; VNC cadence
  must be measured separately. Revisit this probe only when targeting DWM/desktop
  producer cadence, and correlate it through presentation before assigning blame.
  Repeatable baseline procedure: two `helios_dcomp_probe` runs, then read the last
  `present-gate:` line from the live dwm UMD log.
- **IDD delivered-frame cycle — MEASURED (21st session, `D3D11 path stats:` line,
  1/300 frames in the LGIdd log):** the swapchain thread is fully serialized, so
  stage sums ARE the delivered fps. Under a 50Hz bouncing-window probe:
  rects ~14µs | staging ~0 (reuse landed; was a venus CreateTexture2D per frame) |
  copy-submit ~25µs | map 13-59ms avg with a RECURRING ~1.5s max (suspiciously
  constant across configs/builds — a fixed timeout somewhere; hunt with QMP fence
  tracing `virtio_gpu_fence_ctrl/resp` during a mover run) | memcpy 34-38ms
  (7.8 MiB at ~200 MB/s = WC reads) | lgmp ~0. Owner target: 60fps IDD.
  **RESOLVED IN TWO STEPS (same session):** (1) `AllocCached` (22.22.53) did NOT
  move the stage — the ICD maps venus blobs through its own escape path using the
  host's per-blob map_info (the WDDM2 Cached flag only affects dxgkrnl-owned
  mappings); a one-shot BW probe (now permanent in LGIdd) split the sides:
  src strided-read 622 MB/s, memcpy 157 MB/s, IVSHMEM memset 27 GB/s — the READS
  of the WC venus mapping were the whole cost. (2) LGIdd `1d03b685`: MOVNTDQA
  streaming loads (CopyFromWC) → probe streamcpy 5.5 GB/s, frame memcpy stage
  36.5ms → **1.4ms**, delivered rate 14-18fps → **32fps = the probe's damage
  rate** (map now ~8ms avg; pipeline headroom ~100fps — a 60Hz producer should
  see 60).
- **The RECURRING ~1.5s stall — ROOT-CAUSED AND FIXED (22nd session,
  2026-07-06, dxvk-helios `bdbbc2ea`): it was OUR OWN staged-content-probe
  diagnostics.** QMP fence tracing (`virtio_gpu_fence_ctrl/resp` during a mover
  run) split it in one pass: host ctrl→resp ≤ 11.5 ms across 7891 fences (p50
  0.17 ms — host exonerated again), but ALL guest submissions went SILENT in
  ~1.49 s beats, 3 per cluster, every 600 IddCx frames, landing at tick
  600k+30 = the probe HARVEST tick (`HeliosProbeRecurTick=600`,
  `HeliosProbeHarvestTick=30` in refreshHeliosStagedImages). The harvest
  scanned the ~7.8 MiB probe readback BYTE-WISE through the WC venus mapping
  ON THE CS THREAD (~0.75 s per probe at WC single-byte rates × 2 probes per
  staged image × 3 imported dwm backbuffers = 3 beats/cluster), while WUDFHost
  held the acquired IddCx frame — starving dwm and every producer behind it.
  Corroboration: probe issue/result log lines at ticks 2400/3000/3600 match
  the stall frames exactly; the 48-probe cap matched the observed stall
  cutoff (last cluster at tick 3600, none after); this also explains the
  "constant across configs" property (the probes shipped in every build), the
  LGIdd map-max 1.47-1.54 s, the "dwm fence frozen ≥500 ms" producer-lag
  signature, AND all 10 consumer present-wait timeouts (fence exactly one
  behind = dwm's next signal parked behind the block). FIX:
  `dxvk.heliosStagedProbes` config knob, default OFF (re-enable per process
  via DXVK_CONFIG for black-surface triage); harvest now bulk-copies WC →
  cacheable before scanning (~50 ms, not ~1.5 s, if ever re-enabled).
  VERIFIED live (mover churn re-trace, UMD `helios_umd_81a033e237bef769.dll`):
  max silence 48.5 ms (was 1493 ms), submission rate flat through every
  600-frame boundary, `present-wait: timeouts=0`, desktop paintcap-clean.
- **Producer (dwm) completion lag:** publishes outrun the fence by 1-4+ presents
  under churn; vkQueueSubmit2 phases are µs-class (queue_perf), so any residual
  lag is dxvk-CS-thread backlog + venus decode/retire path, NOT the submit
  call. The dominant "frozen fence" signature was the staged-probe stall
  (above, fixed). RE-MEASURE under 60Hz content before more work here;
  if residue: instrument dwm CS latency (record→execute) and the ICD retire
  thread's WAIT_FENCE throughput; consider moving dwm's own consumer waits from
  list-start to copy/sample-time like the IDD fix.

- **Per-present full-GPU drain — FIXED (18th session, `3579ef7` + `ef6689b`):**
  `rotate_resource_backings` drained the whole device (event query + `Sleep(1)` spin —
  the timer quantization WAS the 15–25 ms) on dwm's present thread every present. Now a
  CS-side identity rotation mirroring upstream `D3D11SwapChain::RotateBackBuffers` via
  `InjectCsOrderedAfterPending` (dispatch open chunk + ordered inject, NO CS-thread wait —
  the first iteration's `SynchronizeCsThread` blocked up to **1.9 s/present** behind login-
  churn CS backlogs = the post-cold-boot "occasional framerate dips"). `rotate-perf` after:
  **1–4 µs/rotation**; presents track damage rate (~10/s under the 10 Hz flasher, was ~6/s).
  WARNING for future work: do NOT per-image `waitForResource` on the present thread — a
  bound backbuffer RTV is re-recorded into every new open cmdlist, so `isInUse` never
  clears and dwm wedges (proven with a live minidump).
- **Ghosting after the drain removal — GATED (18th session cont., `4f0a96c`):** the drain
  had been accidentally serializing dwm's GPU work against the IddCx consumer's per-acquire
  copy; with it gone, the copy occasionally read a just-presented buffer whose GPU writes
  were in flight (dwm's venus rendering produces NO dxgkrnl-visible DMA fences — nothing
  else orders the cross-process read). Fix: bounded frame-completion gate in the DXGI
  present DDI (`HeliosWaitFrameComplete`: flush, then poll the submission fence — signals
  at GPU completion) before `pfnPresentCb` makes the flip visible.
  `HKLM\SOFTWARE\Helios!PresentGateUs` (default 32000; 0 = off, the A/B lever). Measured:
  avg 3.6–5.3 ms, timeouts only during startup churn, present rate unaffected
  (`present-gate:` telemetry, 1 line/128). The ARCHITECTURAL fix stays WS1 #4: per-ring
  wire fences + cross-process win32-sync so the consumer waits GPU-side.
- **Release-profile UMD is the deploy default (18th session `32cf4a4`; made the actual
  default 2026-07-26, T0/R102):** dev profile is opt-level 1; release is opt 2 + thin LTO,
  with `debug="full"` keeping the GUID-matched PDB for minidump symbolization. Both
  `tools\install-helios-kmd.ps1` and `tools\hotplug-helios-umd.ps1` now default `-UmdDll`
  to `umd\target\release\helios_umd.dll`, and the install plan prints `UmdProfile` next to
  `UmdSource` so the deployed profile is a line in the log. Until R102 the defaults were
  `...\target\debug\...`, so a default `win_install_kmd` signed the DEBUG DLL into the
  DriverStore while reporting success — every cadence and wake-latency number taken that
  way measured the wrong binary. Build with `win_cargo crate_dir:"umd" args:["build","--release"]`;
  pass `-UmdDll ...\target\debug\helios_umd.dll` for a deliberate debug deploy. A missing
  release DLL now aborts the install (`UMD DLL not found`) instead of quietly shipping debug.
  dxvk-helios stays meson debugoptimized (-O2 already).
- **Frame-update slowness** (owner-visible): remaining suspects after the drain fix =
  dxvk-helios persistent-refresh (14th session, alias-image staging + per-frame refresh).
  Quantify with `HELIOS_QUEUE_PERF` (machine env set 18th session; reaches dwm after the
  next reboot; one aggregate line per 300 submits to
  `C:\ProgramData\Helios\helios_queue_perf.log`).
- **Diagnostics overhead — GATED 2026-07-05** (default quiet; knobs restore):
  UMD per-op log I/O was open/write/close per line on per-frame paths → now a
  persistent handle + `trace_line!` behind `HKLM\SOFTWARE\Helios!UmdTrace`
  (e88f2c6; umd log 8612→681 lines/30 s under churn). ICD submit/shmem trace
  (33 MB+1.9 MB per session, fopen per submission) behind `HELIOS_SUBMIT_TRACE`
  env (mesa 4bb43194e5d). KMD: S-ring `diag::record` off + gdi_blit's 20-value
  per-batch registry dump deferred to every 64th batch behind service-key
  `DiagLevel` (bf0ab37, **22.22.51 ACTIVE since the 2026-07-05 reboot**, pkg
  c393e58c1b189688 / oem58). Also cleared the stale
  `HKLM\SOFTWARE\Helios!RotateSample=16` debug knob (was CPU-reading back the
  whole swapchain ring every 16 rotations). GOTCHA found while measuring: pids
  get reused and `umd-<pid>.log` appends — delete logs before comparing runs.
  Present-rate deltas across dwm restarts are confounded by IDD-pairing state;
  compare within one dwm session only. 18th-session regression found+fixed
  (`a6780f1`): the persistent Rust log handle made the bridge's `fopen_s`
  (deny-sharing) fail on every call — ALL `[dxvk-bridge]` lines incl.
  rotate-perf were silently dead in post-e88f2c6 builds; bridge now uses
  `_fsopen(_SH_DENYNO)`. If a telemetry stream goes quiet, check for a
  string-in-binary vs lines-in-log mismatch before trusting it.
- **Doom present path — async WSI worker LANDED (22nd session cont.,
  mesa `808c7e4a786`, ICD `vulkan_virtio-1b44e8d36fe4` deployed): 120 → 160 fps.**
  The sw WSI present serialized the frame-fence wait (5-6 ms: GPU frame +
  venus retire) + StretchDIBits (0.65-0.75 ms) on Doom's present thread
  (`helios-doom-wsi-perf.txt` wait_avg/stretch_avg = the 120 fps ceiling).
  Now: per-swapchain worker thread does wait+invalidate+blit; acquire's
  IDLE condvar is the back-pressure (run-ahead = image_count-1); kill
  switch `HELIOS_WSI_ASYNC_PRESENT=0`. vkcube 6,600 fps; worker wait_avg
  4.3 ms now overlaps the app's next frame. **CRASH POST-MORTEM in the same
  arc: an unconditional 5-image sw-swapchain bump crashed Doom at renderer
  init ×2 (idTech sizes per-image arrays to the REQUESTED count — unhandled
  C++ FatalError, Crash.00003/00004). Spec-legal ≠ app-safe; extra depth is
  now opt-in `HELIOS_WSI_EXTRA_IMAGES=N` (default 0), for engines that
  re-query.** OWNER DECISION (2026-07-06): the sw present path is FROZEN at
  160 fps; the next present work is HW-accelerated present. The async sw
  worker remains the fallback path + kill switches. Host-side during Doom:
  p50 fence 0.05 ms; a tight ~10.7 ms class at 53/s = ring≥1 GPU-completion
  fences seeing the pipelined queue backlog (healthy).
  **D3DKMTPresent FEASIBILITY RESEARCH (same session, 26100 SDK d3dkmthk.h +
  live-counter evidence):**
  - `D3DKMT_PRESENT` itself is fully documented (hContext — gate5a already
    owns one — hWindow, hSource allocation, Flags, INLINE PresentHistoryToken
    built by the CALLER). The wall is the TOKEN: every redirected model
    (GDI/GDI_SYSMEM/BLT/FLIP/COMPOSITION) requires `hLogicalSurface`/
    `hPhysicalSurface` (win32k logical-surface handles) or dxgi-private
    rendezvous state (`dxgContext`, `hCompSurf`, `confirmationCookie`,
    `PresentLimitSemaphoreId`) minted only by the D3D runtimes through
    win32k-private (win32u NtGdiDdDDI*) calls. NO documented path lets an
    ICD mint a valid redirected token.
  - **Blt-model is DEAD on 26100 regardless**: the KMD's DxgkDdiPresent
    feasibility trace (display.rs PBcall/PBsrc/PBdst, present since .34-era)
    has NEVER fired across weeks of desktop uptime (dwm, steamwebhelper,
    Settings, taskmgr, D3D11 apps) — modern win32k drives flip/composition
    models exclusively; a real KMD present-blit would serve a path Windows
    no longer invokes. (Also: a KMD GPU blit needs venus Vulkan encoding —
    viogpu3d's equivalent rides VIRGL_CCMD_RESOURCE_COPY_REGION, a virgl
    primitive venus lacks; a kernel venus Vulkan client is weeks of work
    for that dead path.)
  - Composition-swapchain API (documented "present from Vulkan" API) needs
    a real D3D11/D3D12 device at CreatePresentationFactory → recursion veto
    (and no D3D12 UMD exists). DEAD.
  - **Viable roads, pick after owner gameplay numbers:** (1) Helios-private
    independent-flip-at-the-consumer: for an unoccluded fullscreen-sized
    Vulkan window, the ICD publishes (resid, fence, geometry) via the WS1 #4
    present-sync channel and LGIdd's per-acquire copy sources the APP's blob
    instead of dwm's composition — semantically DXGI independent flip
    implemented at the IDD; every piece exists (publisher wire format,
    LGIdd dxvk import-by-resid, bounded consumer waits); the ONE design risk
    is the foreground/occlusion contract (must provably fall back to dwm
    composition the moment the window isn't the whole visible screen).
    (2) Pinned-build flip-model RE: reverse the dxgi↔win32k rendezvous
    (win32u NtGdiDdDDI* on 26100, which Helios pins as the guest build) to
    mint real flip tokens = true zero-copy dwm flip for windowed too; high
    RE effort, build-pinned fragility. (3) sw path stays the composed-window
    contract (already async; blit 0.7 ms; the dwm-side staged upload is the
    remaining per-composite cost). **(4) NEW — the dzn/zink-style dcomp road
    (in-tree PROOF in wsi_common_win32.cpp's dxgi branch, used by dzn):
    flip-model presents WITHOUT minting tokens — DXGI + DirectComposition
    mint them: `CreateSwapChainForComposition(device_or_queue, FLIP_SEQUENTIAL,
    ALLOW_TEARING)` + `DCompositionCreateDevice(NULL)` (dcomp needs NO
    rendering device) + `CreateTargetForHwnd(hwnd)` → `visual->SetContent
    (swapchain)` → `Commit()` — all documented public API. The one real
    device required is the swapchain's presenting device; a WARP D3D11
    device satisfies it with NO helios_umd recursion (d3d10warp.dll,
    self-contained). Frame flow: venus host-visible frame (cached mapping)
    → CPU copy into the WARP backbuffer → Present(0); dwm opens the WARP
    buffer cross-adapter (proven path — this box ran a WARP-composited
    desktop pre-milestone). Throughput ≈ the sw path (the bytes still cross
    guest→host once per composite, plus one extra CPU copy vs StretchDIBits);
    the wins are flip-model pacing/damage semantics, tear control, and no
    GDI redirection surface. Zink itself is only a consumer: it rides the
    underlying ICD's VK_KHR_win32_surface via kopper — on venus that is our
    sw path; dzn is the driver that exercises the dcomp branch.**
    **OUR OWN D3D11 PRESENT MODEL (read 2026-07-06, umd forward.rs
    `dxgi_present`): windowed D3D11 is ALREADY hardware-presented end-to-end.
    The DXGI runtime solves the rendezvous FOR the UMD — `DXGI_DDI_ARG_PRESENT`
    delivers BOTH `hSurfaceToPresent` and `hDstResource` (the win32k
    redirection/destination surface as a first-class UMD resource); the UMD
    does `CopySubresourceRegion(dst←src)` = the present blit runs GPU-side
    through venus (zero CPU bytes), then WS1 #4 present-fence publish + the
    bounded frame gate + `pfnPresentCb(hSrc, hDst)` — the KERNEL mints the
    history token (allowed in kernel mode; that's why DxgkDdiPresent never
    fires — the blit already happened in the UMD). dwm's own presents are
    flip-model onto the IddCx buffers (no dst → copy no-ops). CONSEQUENCE:
    only the VULKAN client class (native VK games + vkd3d-proton D3D12) lacks
    a HW present — a Vulkan ICD has no runtime handing it the destination
    surface; that missing hand-off IS the entire gap roads (1)/(2)/(4) exist
    to fill. Gotcha: `UmdTrace` is cached at device init — toggling it needs
    a process restart to take effect.**
    **ROAD 4 ON OUR OWN ADAPTER — PROVEN LIVE (2026-07-06,
    `tools/dcomp_present_probe.cpp`, schtask `helios_dcomp_probe`):** a D3D11
    device on the HELIOS adapter (our UMD; owner-approved vehicle use — the
    nesting is bounded and dwm already runs multiple DXVK→ICD stacks per
    process) + `CreateSwapChainForComposition(FLIP_SEQUENTIAL)` + dcomp
    target/visual on the HWND: every call S_OK, 1023 flip presents, dwm
    composes the animation (paintcap ×2, gradient advancing). This upgrades
    road 4 from "WARP + CPU copy" to the REAL zero-CPU-byte design:
    the DXGI backbuffers are OUR KMD allocations = venus blobs, so the ICD
    copies its frame into the current backbuffer GPU-side with its OWN venus
    device (import-by-resid, the dwm/WUDFHost machinery), then Present() on
    the vehicle device mints the token via pfnPresentCb (flip model → the
    UMD's dst-copy no-ops).
    **DESIGN REFINED (owner, 2026-07-06): reverse the import direction and
    special-case in the UMD present DDI via D3D10DDI_HRESOURCE.** The ICD
    never learns backbuffer resids (kills the COM→DDI texture-query problem
    entirely): the ICD publishes ITS OWN frame resid + fence value (WS1 #4
    slot), hands (resid, value, geometry) to a tiny in-process UMD export
    that stores it in TLS, and calls Present() on the same thread.
    `dxgi_present` consumes the TLS slot: the vehicle's DXVK imports the ICD
    frame by resid — the battle-tested alias-import path INCLUDING the
    `6eab004c` copy-time consumer wait, so the published slot orders the
    copy against the ICD's GPU writes for free — copies into
    `hSurfaceToPresent`'s pDrvPrivate DXVK texture, and publishes with its
    OWN fence (correct: the vehicle genuinely wrote the backbuffer). The
    double-publish conflict stops existing — single writer, single
    publisher, gate on the real writing device. Same GPU copy count, zero
    CPU bytes; WSI images become device-local/tiled (no linear cpu_map
    constraint — a rendering win). Remaining engineering: (a) vehicle
    lifecycle off the ICD hot path (the residual-risk contract), (b) the
    UMD export + TLS present-source slot + dxgi_present special case,
    (c) ICD publish of its frame resid (C writer for the seqlock format),
    (d) present-mode mapping, (e) resize/teardown lifecycle. Verify item:
    the ICD's WSI images must be created as shared-importable dedicated
    blobs (wire resources) so the vehicle context can import them. The
    async sw worker stays as fallback wherever the vehicle fails.
    **ROAD 4 IMPLEMENTED END-TO-END (23rd session, 2026-07-06; mesa
    `8a4e331ea9e..737cb2309b3`, dxvk-helios `6069f323`, main `6521d93` +
    `41fa1c4`). Kill switch `HELIOS_WSI_DCOMP_PRESENT`, DEFAULT OFF —
    flip-on is owner-gated (ladder e). Shipped shape:** vehicle lifecycle
    on a dedicated worker (parks after READY and owns the COM release, so
    the nested D3D11→UMD→DXVK→ICD2 teardown never runs on an ICD1 thread);
    frame images stay OPTIMAL buffer-blit images but their dedicated memory
    exports OPAQUE_FD → USE_SHAREABLE blobs; per-chain named present-order
    timeline (`Global\HeliosPresentFence_<pid>_<0x8000_0000|n>` — ICD id
    space is the high-bit half, the UMD producer counter owns the low half)
    signaled on every pre-present submit; seqlock publish from a
    byte-compatible C writer (wsi_helios_present_sync.c); UMD exports
    `helios_umd_set_present_source`/`_wait_last_present` (TLS, same-thread
    contract); dxgi_present special case alias-imports the frame by resid
    (typed identity, per-resid cache — 3 imports per geometry for 30k+
    presents) and copies at the DXVK image level from the LIVE storage
    (staging ALIAS if present; the COM CopySubresourceRegion path would
    read the never-refreshed private image), publishes the backbuffer with
    the vehicle's own fence, no gate, pfnPresentCb as today. Ladder:
    (a) probe PASS 1082 presents; (b) vkcube READY→LIVE, ~64 fps FIFO
    vsync-paced, 30k+ presents ZERO import/copy/geometry/overwrite
    failures, copy-time wait timeouts=0 noslot=0, dwm imports the vehicle
    backbuffers + fence (val tracking live), venus ctx destroyed at exit,
    resize-recreate exercised live (800x600→1896x959); (d) mover-churn
    paintcap clean, WUDFHost timeouts=0, BARRIER=0. **Flip no-copy
    invariant CONFIRMED: every vehicle 'DXGI Present:' has dst=0x0.**
    GOTCHAS: a maximized vehicle chain gets promoted to direct/independent
    flip — correct on the display but ABSENT from GDI-based paintcaps
    (eyeball via Looking Glass; owner-confirmed live); UmdTrace was ON for
    the invariant check — it confounds fps measurements.
    **Doom (immediate, menu): 40 fps v1 → 75 fps** after two measured
    fixes (frame-latency-waitable DROP for non-FIFO — windowed Present()
    otherwise blocks at dwm's compose pace; skip the worker's 4.1 ms
    frame-fence wait while the vehicle serves — ordering is the copy-time
    wait + pre-present throttle). Remaining gap to the 160 sw baseline is
    the image-recycle guard `wait_last_present` (6.1 ms serial,
    present-gate telemetry) stacked on the copy-time wait (8.4 ms
    overlapped): both are venus fence-OBSERVATION latency (ring≥1 retire →
    retire thread → NT signal; ICD2 submission-fence poll), not GPU time
    (~0.3 ms copy). NEXT LEVERS: (1) GPU-side recycle gating — export the
    vehicle's (fenceId, value) back through the UMD, import the named
    fence in ICD1 once, and make image reuse a timeline WAIT at acquire
    instead of a worker-serial CPU wait (fully pipelined; the run-ahead
    absorbs the latency); (2) shave the retire→signal path itself (helps
    every WS1 #4 consumer). sw async worker remains the default and the
    per-frame fallback (the pre-present blit still runs in vehicle mode —
    skip it only with numbers, it is what makes fallback seamless).
    **LEVER 1 IMPLEMENTED (24th session, 2026-07-06; mesa `10f72c50104` +
    `3e5fa9eb1ce`, main `1c34671`; UMD `helios_umd_c78f1a7e01df20b4`, ICD
    `vulkan_virtio-d49cd875438d`).** dxgi_present hands the vehicle's
    (fenceId, value) back via `helios_umd_get_present_result` (TLS,
    same-thread, counted misses/overwrites); the WSI imports
    `Global\HeliosPresentFence_<pid>_<fenceId>` once per chain and gates
    image reuse at ACQUIRE (bound `HELIOS_WSI_VEHICLE_WAIT_US`, 0=off;
    `acquire-gate:` diag telemetry per 512; drops recycle ungated;
    fallback = the old serial wait, `gate_fb` counter; teardown drains max
    pending value). Measured: worker cycle 13→4.4 ms, present-gate serial
    lines GONE, gate avg 2.7 ms overlapped, timeouts 0, gate_fb 0; vkcube
    FIFO ~60 clean; ladder clean (mover, WUDFHost timeouts flat,
    BARRIER=0).
    **CRASH FOUND+FIXED en route (first gated vkcube run froze the guest
    and degraded the whole desktop): vkr resolves vkSignalSemaphore only
    for >=1.2 devices or with KHR_timeline_semaphore enabled at create and
    dispatches it with NO null check** — vn_WaitSemaphores' imported-win32
    post-wait counter sync on a Vulkan 1.0 app (vkcube) = ip=0 segfault of
    the per-context render worker (`journalctl: vkr-ring-<ctx> segfault at
    0`), which the guest NEVER sees: qemu logs only
    `virgl_renderer_context_create_fence: Operation not permitted`
    (proxy_context_submit_fence → dead worker socket → -1), the poisoned
    submission's fence never retires, the app wedges in vkWaitForFences
    and everything else crawls on the backpressure. Fix (mesa
    `3e5fa9eb1ce`): append KHR_timeline_semaphore at device create for WSI
    devices on <1.2 instances (also legitimizes the pre-existing
    present-order timeline signals on 1.0/1.1 apps) + skip the host
    counter sync when the procs cannot exist. dwm/probe never hit it (1.3
    devices). Recovery for a wedged desktop: Stop-Process dwm.
    **OPEN — Doom menu capped at exactly 60.0 fps on BOTH paths today**
    (sw AND vehicle, gate on/off identical, QMP fence-trace on/off
    identical, foreground attempt identical; drops=0, gate timeouts=0,
    worker 4.4 ms, sw wait_avg 3.7 ms — nothing visible adds to 16.6 ms).
    The 22nd/23rd 120/160/75 numbers came from the same unattended
    launcher, so this is a NEW environmental cap, not the acquire gate
    (gate-off A/B proves it). Suspects to disambiguate with the owner:
    idTech background/unfocused throttle state, Steam relaunch settings,
    LGIdd/dwm restart state vs yesterday's boot. Tool:
    `tools/read-vehicle-counters.ps1` samples live minted/drops/gate
    counters from a running process (the perf line never prints on
    pure-vehicle runs); `Z:\tmp\movewin_target.txt` + helios_movewin
    foregrounds an arbitrary window title. Owner gameplay: ~70-80 fps.
    **STALE-FRAME STUTTER FIXED (24th session, cont.; owner-confirmed
    "much better"; main `5a9d5d5`).** Root cause: direct/independent-flip
    presents (Doom's near-fullscreen window) are ordered only on the KMD's
    decode-complete DMA fence — the backbuffer scanned out before the
    venus copy landed, showing the buffer's 3-presents-old occupant. dwm's
    consumer wait protects only COMPOSED presents. Fix: vehicle presents
    now run the frame-completion gate on the WORKER before pfnPresentCb
    (`VehicleFlipGateUs`, default 32 ms, 0=off; 6.7 ms avg = the copy's
    fence-observation latency; acquire-gate wait dropped 2.7→1.7 ms).
    Pipelined follow-up: dxgkrnl WaitForSynchronizationObjectFromGpu on
    the present packet + the producer fence before the copy flush (also
    retires the copy-time CPU wait for vehicle copies). Related cleanup
    (dxvk `7255c1e6`): the redundant pre-6eab004c list-start consumer wait
    in refreshHeliosStagedImages removed for the alias variant.
    **THE 200-fps LEVER FOUND — escape-parked fence waits convoy submits
    (mesa `fbf38f4cc63` has the telemetry + stopgap).** Phase splits
    ([prep/ring/win32] on QueueSubmit2, [mutex/escape/sync] on
    helios_submit) proved: the pre-present submit's 2.5-2.9 ms is entirely
    the win32-signal SUBMIT_VENUS D3DKMTEscape, which serializes at the
    dxgkrnl escape layer behind the retire thread's blocking WAIT_FENCE
    escapes (parked up to 250 ms/slice; processes with no parked waits —
    dwm, the sw path — submit at 3-5 µs). Slice 250 ms → 2 ms bounded the
    convoy (escape 2.9 → ~1.0-1.6 ms) and CONFIRMED the mechanism; Doom
    stays ~60-70 because its OWN vkWaitForFences ride the same parked
    escapes. NEXT SESSION (the fix): **KMD-signaled usermode fence
    events** — non-blocking REGISTER_FENCE_EVENT escape (fence_id, event
    HANDLE), KMD DPC KeSetEvent at wire-fence retirement (in-flight table
    already has ISR→DPC), retire thread + helios_wait park on the EVENT
    in usermode. Kills the convoy class entirely AND collapses the
    fence-observation latency every consumer pays (app fence waits, flip
    gate, acquire gate, retire→signal chain). KMD unit: event table with
    ObReferenceObjectByHandle lifetime + process-teardown cleanup +
    version bump; ICD unit: register/park/cancel in helios_ioctl_wait_fence
    path. Deployed at session end: UMD `helios_umd_89e95497a7d6d08f`, ICD
    `vulkan_virtio-2900b457e859`, dxvk `7255c1e6`, mesa `fbf38f4cc63`.
    **USERMODE FENCE EVENTS SHIPPED (25th session, 2026-07-06; main
    `16da0eb` = KMD 22.22.54, mesa `e5f35c18bf9`; deployed ICD
    `vulkan_virtio-9384eb059a8f`, UMD unchanged `89e95497a7d6d08f`).**
    REGISTER/UNREGISTER_FENCE_EVENT escapes: PASSIVE
    ObReferenceObjectByHandle → bounded (fence_id → PKEVENT) table;
    retirement DPC KeSetEvent + ObDereferenceObjectDeferDelete; one-shot;
    already-retired = atomic check under the device spinlock (no lost
    wakeups); capability probe (fence_id=0/handle=0 → PROBE_ACK, old KMD
    → NOT_IMPLEMENTED → loud diag + blocking-escape fallback). ICD:
    per-thread tss event, register → WaitForSingleObject → cancel;
    UNREGISTER NOT_FOUND + signaled event = raced (complete), +
    unsignaled = teardown purge (loud, INCOMPLETE). Retire thread: 2 ms
    slice stopgap retired from the event path — one WFMO on {fence
    event, retire_stop_event}, 60 s deadline; slice loop survives only
    as the old-KMD fallback. All counters in QUERY_STATS v2 + a
    `fence_events` perf-summary line.
    **BEFORE/AFTER (Doom vehicle, unit-0 baseline 23:00 → post-deploy
    23:35): submit_phases escape 465-786 µs → 111-126 µs; QueueSubmit2
    [prep 11.2/ring 45.5/win32 351 µs] → [5.2/2.3/87 µs] (submit_avg
    390 → 89 µs); acquire-gate (vkcube) 2.6-2.9 ms avg max 11-21 ms →
    196-273 µs avg max <1 ms; fence_events waits=2455 imm=245 raced=0
    timeouts=0 fallbacks=0 lost=0; vkcube FIFO ~60 clean; WUDFHost/dwm
    timeouts 0, BARRIER 0.** Gates passed: escape µs-class ✓, win32
    <100 µs ✓. PENDING: owner Doom gameplay fps + stale-frame stutter
    eyeball on the new stack (baseline gameplay flip-gate max was
    20-29.7 ms = the 2-3-vblank hitch signature; event waits should
    collapse it), present/flip-gate gameplay averages, ladder e soak.
    **DOOM LEVEL-LOAD FATAL ROOT-CAUSED + FIXED (same session; main
    `aedd8ba` = KMD 22.22.55): "Cannot map buffer with usage BU_STATIC"
    = MAP_BLOB → STATUS_INSUFFICIENT_RESOURCES = the adapter-global
    user-VA MappingTable still sized to the ORIGINAL MAX_BLOBS (256)
    after blobs grew to 8192** — desktop held ~223 mappings, the level
    load's vkMapMemory burst blew the remaining ~33 (probe-proven:
    map-and-hold refused at held=33 with 0xC000009A; size sweep 1-256
    MiB all passed, falsifying the MDL-CSHORT hypothesis; failure
    signature present at 05:28 pre-change = NOT a fence-event
    regression). Fix: MAX_MAPPINGS 8192 + the three indistinguishable
    0xC000009A refusal sites individually counted (mapping-full /
    map-pages / window-alloc) in QUERY_STATS v2.
    `tools/blob_map_size_probe.c` = size sweep + concurrent-headroom +
    v2 stats reader (gcc on the VM, no vcvars needed).
    **STALE-FRAME A/B VERDICT + KERNEL-ENFORCED FLIP ORDERING PROVEN
    (25th session cont., 2026-07-07; main `77dffe2`).** Owner A/B on the
    fence-event stack: **sw path 200 fps (was 160 — the event waits
    bought sw +40 fps) with ZERO stale frames; vehicle path 120-130 fps
    with stale-frame stutter still present** → the leak is the
    vehicle/UMD flip path, pre-existing, fps-scaled. Counted leak sites
    that window: flip gate 11 timeouts + acquire gate 3 timeouts per
    ~41k presents (each "proceeds loudly" = a stale flip candidate);
    fence-event machinery itself clean (0 fallbacks/0 lost). NEXT ROAD —
    **kernel-enforced vehicle flip ordering** (replaces the bounded CPU
    worker gate): queue D3DKMTWaitForSynchronizationObjectFromGpu on the
    copy's monitored fence AHEAD of the present packet, so dxgkrnl holds
    the flip until the retire thread's CPU signal lands — no usermode
    cap to leak, and the 6.6 ms worker serial gate retires (fps win).
    `tools/vehicle_flipwait_probe.c` PROVES the primitive live on our
    software-scheduled adapter (queued signal held behind an unsatisfied
    wait, drained ~10 ms after the CPU signal; ZERO KMD changes) and the
    topology: raw cross-device sync handles are REJECTED 0xC000000D —
    the fence must be NT-shared (D3DKMTShareObjects) and reopened via
    OpenSyncObjectFromNtHandle2 on the device owning the waiting context
    (the WS1 #4 named-share consumer pattern). Implementation sketch:
    UMD opens the vehicle fence (fence_id known from the present result)
    on the device owning `dev.h_context`, queues the wait right before
    pfnPresentCb; CPU gate kept as fallback knob + wedge watchdog
    (a never-signaled fence would park the context queue). Second half
    (same primitive): producer-fence wait before the copy flush retires
    the 7 ms copy-time CPU wait. Ladder: wait-honored telemetry → flip
    gate CPU path off → owner stutter eyeball → fps before/after.
    **IMPLEMENTED + WEDGED (main `e9cdae6`, default OFF; VM registry
    VehicleKernelFlipWait=0 guards the deployed default-ON UMD
    `fb564d5072f8a58e`).** First live test (vkcube): present #1 armed +
    queued OK, but the enqueueWait signal NEVER fired (exported present
    fence counter never read ≥1 in the producer's own process — likely
    an untested vn_GetSemaphoreCounterValue/vn_WaitSemaphores
    exported-win32 path), the watchdog unwedge released the flip but the
    app never presented again (the WSI worker parks on something after
    pfnPresentCb — find it), and TerminateProcess on the wedged instance
    HUNG THE ENTIRE GUEST — no bugcheck, no dump (kernel deadlock;
    dxgkrnl/KMD teardown of a context with a parked queue + queued
    monitored-fence wait whose signal source died). QMP system_reset
    recovered. **26TH-SESSION TOP PRIORITY: root-cause + fix all three
    (see the flip-kwait-wedge memory: NMI-crash/live-KD recipe, suspect
    KMD teardown paths, retire-thread-signal design fallback).**
    **26TH SESSION (2026-07-07): DEFECT (a) ROOT-CAUSED + FIXED (mesa
    `b2f47c780d2`, deployed ICD `vulkan_virtio-e31ec528ac79`); the
    GUEST "HANG" CLASS SOLVED — it was the SERIAL KERNEL DEBUGGER.**
    (a) The producer's EXPORTED present fence was unobservable in its
    own process: `is_external` skips the feedback slot, the queue
    signal routes out-of-band onto the helios_sync/WDDM fence (retire
    thread only), and both vn_GetSemaphoreCounterValue (host ring
    round-trip: 0 forever) and vn_WaitSemaphores (win32 fast-path was
    imported-only) never read the WDDM fence. Fix: fold
    vn_renderer_sync_read into the counter read for ANY win32-backed
    payload + widen the wait fast-path (event wait); host-counter sync
    stays imported-only (exported host object has a pending GPU signal
    op). Verified: knob=1 vkcube presents #1-2 armed AND RELEASED
    (was: #1 wedged); first-rescue diag `wddm=852/1307 host=0`.
    (c) The whole-guest freeze reproduced WITHOUT any kill (knob=1
    vkcube after ONE wedge+watchdog-unwedge cycle, ~3 min fuse) —
    QMP `inject-nmi` → bugcheck 0x80 minidump `070726-4890-01.dmp`
    (copy in Z:\tmp): 15/16 vCPUs in nt!KiFreezeTargetExecution, CPU#6
    polling kdcom!READ_PORT_UCHAR — the guest had dropped into the
    SERIAL KERNEL DEBUGGER (bcdedit debug=Serial port 1, NO kdcom
    client exists — ntoseye is a gdbstub) and froze forever. With KD
    enabled, dxgkrnl asserts / the ERESOURCE deadlock detector / TDR
    BREAK instead of bugchecking — that is why neither hang ever left
    a dump. TerminateProcess was never the root cause.
    (b) Caught red-handed in the same dump (stack scavenge of the
    frozen vkcube thread): a runtime thread inside
    DxgkWaitForSynchronizationObjectFromGpu blocked MINUTES in
    nt!ExpWaitForResource on a dxgkrnl ERESOURCE (holder unknown —
    triage dump has one stack) until nt!ExpResourceTimeoutCaptureLiveDump
    → KiBreakpointTrap → KD entry. So the worker's forever-park after a
    flip-kwait stall = a dxgkrnl-internal ERESOURCE convoy seeded by
    the stalled flip, not a WSI wait (all WSI waits audited bounded).
    RESIDUAL OPEN: present #3's wire fence never retired (fence stalled
    at 2 with 3 queued; #1-2 clean) — the remaining true stall; and the
    WSI perf-counter oddity (presents=0/ready=0 while 3 vehicle
    presents + gate-ARM demonstrably ran). TOOLKIT NOW ARMED:
    CrashDumpEnabled=2 (full kernel dump next time → !locks names the
    ERESOURCE holder); PROPOSED: bcdedit /debug off so every future
    freeze self-converts to dump+reboot (testsigning + ntoseye/gdbstub
    unaffected). Registry knob back to 0; ICD fix deployed + desktop
    cold-verified healthy on it.
    **26TH SESSION CONT.: THE DEADLOCK CLASS KILLED — ERESOURCE HOLDER
    NAMED AND FIXED (mesa `2cc63d82468`, deployed ICD
    `vulkan_virtio-178211300d5f`; bcdedit /debug off applied).** The
    owner-approved second NMI (full kernel dump, CrashDumpEnabled=2)
    caught it red-handed: the exclusive holder of all 3 contended
    ERESOURCEs was the process's own next VENUS ESCAPE —
    HardwareAccess=1 escapes run dxgkrnl
    AcquireCoreResourceExclusive → DXGPROCESS::FlushAllDevice →
    VidSchWaitForCompletionEvent, i.e. they WAIT FOR EVERY CONTEXT
    QUEUE TO DRAIN while holding the core resource; with a kwait-parked
    queue whose signal comes from user mode, every
    SignalSynchronizationObjectFromCpu (dxvk waiter, watchdog — user
    dump caught the watchdog parked IN the signal syscall, so the
    25th-session "unwedge released the flip" was FALSE — and the ICD
    retire thread) convoys behind it = deadlock; wedge point #1/#2/#3
    varied by race. Fix = unit 3's HardwareAccess=0 (correctness, not
    just perf; `HELIOS_ESCAPE_HW=1` env kill switch) + the exported-sem
    fold + a signalTo monotonicity guard (main `6d41f8e`, deployed UMD
    `helios_umd_3b4a9e66394e9523`). **LADDER RESULTS (2026-07-07
    02:45-02:53): 24,064 kwait presents, 0 wedges / 0 arm fails /
    0 queue fails / 0 signal fails; mid-run kill (owner) → clean
    teardown, no zombie, guest healthy. BUT THE OWNER EYEBALL RUNG
    FAILED: vkcube showed the Doom-class STALE-FRAME STUTTER from
    ~40 s in — kernel flip ordering alone does NOT close the stale
    class. Plus a new observation: a freshly launched (unfocused)
    vkcube alternates between TWO stale frames until the window is
    clicked, then rendering progresses. DEFAULTS STAY OFF
    (VehicleKernelFlipWait code default OFF, registry knob back to 0).**
    **OWNER CONFIRMED: both issues are ABSENT on the sw present path**
    — they are properties of the dcomp VEHICLE presentation layer, not
    of fences/venus in general (matches the A/B: sw 200 fps zero
    stale). 27th-session hypotheses, vehicle-layer-first:
    (c1) DXGI_STATUS_OCCLUDED — Present() on an occluded/background
    window returns a SUCCESS status (0x087A0001) and does not display;
    wsi_win32_queue_present_vehicle checks only FAILED(hr)
    (wsi_common_win32.cpp:1985-1995) so occluded presents "succeed"
    silently → add per-present hr!=S_OK logging (cheap, likely explains
    the click-gated launch behavior); (c2) dwm/dcomp consumption pacing
    for background visuals, direct-flip vs composed transitions;
    (a) backbuffer clobber — the venus copy lands at Present-call time
    while up to frame-latency flips sit parked, so rotation may
    overwrite a buffer whose flip has not scanned out (instrument
    backbuffer ptr + flip completion pacing; consider bounding armed
    depth); (b) U:=V fires at ring-fence retirement measured at 97-98%
    of T_gpu — check the tail. First step: knob=0 vehicle A/B of the
    unfocused-launch behavior + the hr logging.
    **27TH SESSION (2026-07-07): (c1) FALSIFIED + full static analysis of
    the HW present path.** hr-transition/drop-streak instrumentation
    deployed (mesa `e3d2fdf61c1`, ICD `vulkan_virtio-1545839fc535`;
    `odd_hr` counter in the perf line; paintcap_hidden schtask = capture
    without the console occluder/focus steal): under a 45 s full-screen
    occluder the knob=0 chain presented at a steady 60 Hz, hr == S_OK
    throughout, acquire-gate timeouts 0 — composition swapchains never
    report OCCLUDED and dwm keeps consuming occluded chains; presents
    flowing says NOTHING about display. Owner clarified the property set:
    launch alternation + random self-healing stutter are both cured by ANY
    rendering activity (notepad open — no focus change!) ⇒ the lever is
    activity, not focus. Static analysis (3-leg map, agent reports in the
    session transcript): every vehicle ordering protection is a bounded
    32 ms wait that LEAKS stale on timeout (consumer copy-wait
    dxvk_context.cpp:9507 "copying anyway"; flip gate forward.rs:5902
    "flipping anyway"; acquire release-gate wsi_common_win32.cpp:1683
    proceed; dwm-side staged-refresh wait ditto) — all funnel into named-
    fence advancement by the per-process ICD retire thread, whose event
    wait is the ONE chain link with NO self-heal: KMD fence events are
    interrupt-edge-driven (drain_used from the ISR/DPC or opportunistic
    drains on the process's OWN escape traffic only; no KMD timer;
    register-vs-retire race proven closed, gpu.rs:1317/escape.rs:186), so
    a lost/deferred INTx for a mostly-IDLE process (dwm on a static
    desktop!) parks its retire thread ≤60 s with nothing to kick it.
    PRIME SUSPECT (fits all 4 properties + why vkcube-side counters were
    all clean): the stall is in DWM's process — dwm consumes the vehicle
    backbuffers through the alias-staging refresh + 32 ms consumer wait;
    a parked dwm retire thread ⇒ dwm composes stale staged copies of the
    2 ever-refreshed backbuffers (= the TWO alternating frames) while the
    app's flips rotate healthily; dwm idle ⇒ few escapes ⇒ no drains ⇒
    self-sustaining until any dirty region makes dwm submit (notepad,
    click) ⇒ drain ⇒ unstick. Secondary: retire-thread serial FIFO
    convoy (one stuck head delays every later signal = stutter burst,
    self-heals = P3/P4); vehicle named release fence created LAZILY
    inside present #1 (dxvk_bridge.cpp:1434) + consumer import-failure
    negative-cache of 256 LOOKUPS (dxvk_context.cpp:9482) = long
    unordered window at chain birth. SOLUTION CHOSEN (owner: NO HACKS):
    prove the failing link with ONE owner repro, then fix that link at
    its ROOT — (a) event registered-but-never-signaled + INT_ROUTINE
    stalled ⇒ interrupt delivery ⇒ MSI-X (MSISupported=0 is a bring-up
    leftover); (b) used entry never posted ⇒ submission/doorbell path;
    (c) event signaled but fence behind ⇒ ICD retire logic. The sliced-
    polling retire wait + KMD timer backstop are REJECTED as hacks;
    eager vehicle fence + time-based import retry are clean but staged
    AFTER the repro so they cannot mask it. Evidence kit already
    complete: dwm_helios_umd_dxvk.log present-wait lines (import lines
    prove the channel live; ZERO timeout lines in healthy runs),
    helios_icd_diag.log retire "GIVING UP ... stays UNSIGNALED" lines
    (PROVEN to fire — 2 instances 2026-07-06), QUERY_STATS v2
    FENCE_EVENT_*/INT_ROUTINE via tools/blob_map_size_probe.c,
    paintcap_hidden content diffs. REPRO IS OWNER-ONLY: schtasks
    launches always take foreground (fg=1 across 4 steal attempts), and
    automated probing IS the curing activity (self-defeating). Owner
    recipe: reproduce the alternation, hands off ~90 s (>60 s arms the
    GIVING-UP diag), note the time; then read the four channels.
    **27TH SESSION VERDICT + FIX DEPLOYED (KMD 22.22.56, main `1a07814`).**
    Owner repro delivered the discriminator: during a LIVE two-frame
    alternation (~60 s, hands-off recovery) EVERY sync in the DAG was
    green — vkcube 60 fps/acquire-gate 64 µs/0 timeouts, vehicle 29k
    copies 0 fails/flip-gate 390 µs, dwm waits 7.9 ms 0 timeouts 0
    noslot on the live chain, WUDFHost 6.3 ms 0/0 — so the fence class
    was EXONERATED (retire/interrupt fixes rejected as the wrong tree).
    Second audit pair: our flip-emulation identity rotation is provably
    self-consistent (copy dst == flip src per present; storage+identity
    rotations = same permutation; publish resid tracks rotation; no
    in-tree pairing skew), BUT the caps surface lied:
    `DXGK_DRIVERCAPS.SupportDirectFlip=1` (NO bisect provenance — never
    load-mandatory) + aperture-segment DirectFlip flags, on an adapter
    with ZERO scanout displaying through IddCx-captured COMPOSITION.
    dwm promotes the eligible dcomp vehicle visual (flip-model +
    IGNORE-alpha + unoccluded) to direct/independent flip and STOPS
    COMPOSING it — two alternating stale frames = dwm's last composed
    pair; any dirty-region recompose demotes it (taskbar clock minute
    repaint = the hands-off ~60 s recovery; console-overlapped schtasks
    launches never repro'd because occlusion kills eligibility; the
    23rd-session "maximized chains vanish from paintcaps" was the same
    promotion). The UMD already denied CheckDirectFlipSupport — the KMD
    now agrees: SupportDirectFlip + the 3 aperture DirectFlip flags are
    DENIED by default behind the `DirectFlipCaps` service knob (0 =
    deny, 1 = legacy A/B via reg add + devcon restart; state in diag
    0x01D7 bit 2, DiagLevel-gated). Display-less-adapter guidance
    (mcdm-implementation-guidelines.md) mandates 0. DEPLOYED + cold-boot
    verified: 22.22.56 CM_PROB_NONE, release UMD re-pinned
    (helios_umd_3b4a9e66394e9523; devcon reset it to debug — known
    trap; DriverStore copy sync failed file-in-use, cosmetic), desktop
    composited healthy. OWNER VERDICT: NO CHANGE — direct-flip denial
    falsified as the mechanism (kept as the truthful cap surface).
    **TRUE ROOT CAUSE FOUND + FIX DEPLOYED (dxvk `1cdf0837`, mesa
    `10cefe67c64`; UMD `helios_umd_746b0242cf664825`, ICD
    `vulkan_virtio-390d1b583dba`).** Discriminating evidence: owner
    repro with per-window counter sampling — during a LIVE dance dwm
    performed ZERO consumer reads for 20+ s while composing 60 fps
    (waits/fast/noslot all frozen), and mouse movement cured it
    INSTANTLY (both dance and stutter) — consumer freshness tracked
    dxvk COMMAND-LIST CADENCE, not producer progress: staged imports
    re-stage only at list starts, an idle dwm's CS chunks span many
    frames (~60 s to fill when idle = the hands-off recovery), so its
    sampled staged copies froze while every fence stayed green; the
    front-buffer rotation cycled 2-3 differently-aged frozen copies =
    the two-frame dance; chunk-threshold oscillation = the stutter;
    activity = per-frame flushes = cure. sw path immune because
    GdiAccelMode=0 routes GDI content via CPU redirection surfaces
    (not our staged imports). THE FIX (dxvk-helios, 3 parts):
    (1) bind-time staleness gate — staged-SRV binds arm a sticky
    per-context flag; draws/dispatches on the immediate context compare
    each bound staged image's published present-sync value against the
    value its last re-stage observed and Flush() when the producer
    advanced (≤1 flush per published value per image via a claim CAS;
    non-consumer processes pay one branch per draw); (2) zombie-refresh
    unenroll — texture dtor flags the image, the refresh loop erases it
    (was: 15k+ no-slot full-image copies per DEAD vkcube chain until
    the 3600-tick prune, Rc-pinning the corpse's venus resources);
    (3) present-sync slot recycling by PRODUCER CREATION TIME
    (reserved2 repurposed; pid liveness alone let cross-boot pid reuse
    keep 64/64 slots stale — publishes were being DROPPED table-full).
    Along the way: 366-368-vs-389-391 resid "mismatch" resolved as two
    chain generations (live-chain publish/lookup rendezvous verified
    healthy, 3 rotating slots advancing per frame). AWAITING owner
    verdict on the fix. Then: ladder rungs (Doom fps vs 120-130,
    kwait default decision, HELIOS_WSI_DCOMP_PRESENT default), and the
    deferred cleanups: per-present sw-fallback insurance blit on
    vehicle chains ("measure before removing"), in-process present-sync
    slot/named-fence round-trips (set_source carries a dead
    fence_value), eager vehicle fence at init, time-based import
    negative cache, DirectFlipCaps knob retirement decision.
    **FIX CONFIRMED (owner: "working very well"); 28TH SESSION =
    WINDOWED DOOM PERF.** Doom telemetry (pid 9444, 1880x943 windowed
    vehicle, immediate/tearing=1, ~105 fps): flip gate avg 5.57 ms
    (worker-serial) + acquire gate avg 4.06 ms (app), both timeouts=0 —
    pure fence-observation latency of the vehicle copy;
    queue_present_avg 5.96 ms. FULLSCREEN "200 fps rock-stable" = THE
    SW PATH: the fullscreen (1896x1030) chain's vehicle build FAILED at
    stage='dcomp target/visual' hr=0x88980800 and latched → sw direct
    path at ~0.85 ms/frame CPU (creates=2 fails=1 in
    helios-doom-wsi-perf.txt). NEW DEFECT: vehicle re-create for the
    same hwnd fails (one-dcomp-target-per-hwnd on resize/fullscreen;
    vkd3d likely creates a NEW VkSurface for the same hwnd → per-surface
    target cache misses → second CreateTargetForHwnd fails). dwm
    post-fix: noslot=54 (was 68k), fence_events 0 fallbacks/0 lost,
    escape 64 µs. Doom perf files: C:\Users\Rupansh\helios-doom-perf.txt
    + helios-doom-wsi-perf.txt (owner's launcher tees them).
    **28TH SESSION: the dcomp target re-create defect FIXED (mesa
    `bbf5e33f314`, ICD `vulkan_virtio-437986e5fcc4` deployed).** One
    composition target per hwnd is a Windows rule; the target/visual
    cache was per-VkSurface, so a new VkSurface for the same hwnd
    (vkd3d resize/fullscreen) failed CreateTargetForHwnd 0x88980800 and
    latched onto the sw path. Fix: process-global refcounted
    hwnd→target registry under the vehicle runtime mutex; the visual's
    content owner (current_swapchain) lives on the shared entry so a
    retired chain on the OLD surface cannot blank the new chain's
    content; `tgt_reuse` counter added to the perf line. Proven by
    tools/vk_surface_recreate_probe.cpp (schtask `helios_vk_recreate`,
    session 1, time-based phases — frame-count phases finish before the
    ~5 s async vehicle build, first attempt exercised nothing): chain A
    LIVE holding the target → chain B (new surface, SAME hwnd, A alive)
    READY+LIVE → chain+surface A destroyed under live B, B presented on
    (acquire-gate 0 timeouts). The honest fullscreen-vehicle Doom A/B
    is now unblocked (owner run — expect creates=2 fails=0 and the
    fullscreen chain LIVE instead of the 0x88980800 latch).
    **28TH SESSION cont. — kwait rung 1 GREEN + copy-side wait FALSIFIED
    + both cleanup counters live (mesa `06e27a05ea3`, dxvk `7c82271f`;
    deployed ICD `vulkan_virtio-43feb2709167`, UMD
    `helios_umd_801a8571aff69c67`, `VehicleKernelFlipWait=1` in the
    registry).** (a) Kernel flip-wait smoke: vkcube dcomp 4608/4608
    presents kwait_armed, 0 arm/queue fails, acquire-gate 66 µs avg
    0 timeouts, 40 s no wedge, content advancing in paintcap diffs —
    the 5.6 ms worker-serial flip gate is retired whenever the knob is
    on; Doom A/B vs the 105 fps baseline = OWNER RUNG (knob already
    live). (b) The 25th-session "producer-fence wait before copy flush"
    idea is FALSIFIED as a lever: Doom's 28k-present run logged ZERO
    copy-time consumer waits (no present-wait/unordered/noslot lines in
    umd-9444.log) — the copy-time CPU wait always fast-paths; the
    4.06 ms acquire gate is copy COMPLETION+OBSERVATION latency, not a
    CS-thread wait. Not built (measure-first). (c) Insurance blit:
    HELIOS_WSI_INSURANCE_BLIT=0 skips the per-present image->buffer
    fallback blit on vehicle-serving chains (insurance_skipped counter
    in the common perf line; vkcube A/B clean, 2780 skips, content
    correct) — default ON until the Doom A/B numbers land. (d)
    dwm-side staleness-gate cost now visible: gate_flushes in the
    present-wait line — ~50 flushes / 13.7k consumer reads (~0.4%) on
    a live desktop, the 27th-session fix is cheap. OWNER LADDER: Doom
    windowed (kwait live) fps + gates vs 105; fullscreen re-try
    (target-registry fix) — expect vehicle LIVE, honest fullscreen
    number vs the 200 fps sw path; optional HELIOS_WSI_INSURANCE_BLIT=0
    leg; stale class must stay dead throughout (vkcube shows no
    regression). THEN: kwait + DCOMP default decisions, residual
    stutter triage (105-vs-60 Hz judder, gate max-spikes).
    **OWNER DOOM VERDICT (same-process windowed→fullscreen, kwait=1 +
    insurance=0): no fps change; stutter still present WINDOWED, GONE
    FULLSCREEN.** Evidence (pid 6220): the fullscreen 1896x1030 chain
    went VEHICLE (READY+LIVE on the same hwnd as the windowed chain —
    the target-registry fix confirmed in the wild); kwait_armed
    6144/6144, 0 arm/queue fails, no wedge; insurance_skipped
    13176/13200; queue_present_avg 5.96→2.81 ms (flip gate retired) BUT
    acquire-gate 4.06→7.69 ms avg (max ~20 ms, timeouts 0) — the
    latency the worker gate used to absorb moved to acquire: the fps
    limiter is the vehicle copy's COMPLETION+OBSERVATION latency
    (~1 host-GPU frame at saturation), not CPU gates. Gates are now
    honest pipeline measurements, nothing left to delete guest-CPU-side.
    STUTTER LOCALIZED: same process/gates/fps, stutter only when dwm
    COMPOSES the chain (windowed); fullscreen (no dwm compose leg)
    smooth ⇒ the stutter lives in dwm's consumption leg. dwm telemetry
    during the run: gate_flushes 2480 (~60/s — the freshness gate does
    per-frame work under a game, expected) and consumer waits avg
    8.9 ms when they fire (~8% of reads) — 9 ms stalls ON DWM'S CS
    THREAD = composition hitches. HYPOTHESIS (next session): the
    staged-refresh loop re-stages ALL enrolled vehicle backbuffers at
    list start, including the just-presented one whose copy is still
    in flight — dwm waits ~9 ms for a buffer it is not composing this
    frame. Fix shape (no hacks — matches real-hw dwm semantics of
    composing the newest COMPLETE frame): skip-if-unretired in the
    refresh (keep the current staged bytes, let the bind-time gate
    force the re-stage next list) instead of the bounded 32 ms wait;
    kwait guarantees the flip itself never outruns its copy, so
    skipping cannot resurrect the stale class. DEFAULTS PROPOSAL:
    kwait code-default ON (Doom+vkcube green, deadlock class dead);
    insurance knob keep (no measurable cost either way at Doom res —
    the copy hides under GPU latency); DCOMP default AFTER the dwm
    stutter fix.
    **STUTTER FIX IMPLEMENTED + DEPLOYED (dxvk `6bcbd282`, main
    `83f9697`; UMD `helios_umd_b70f3e5b23cc5e03`): skip-if-unretired
    staged refresh for kwait-ordered publishes.** Producers advertise
    kernel-held flips in the present-sync slot (fenceId bit 30 — free
    in both id spaces; UMD sets it on vehicle publishes when
    flip_wait_setup succeeded, never on dwm→IddCx publishes; mesa
    publishes never set it); the consumer refresh keeps its current
    staged bytes for an unretired kwait value (that image cannot be
    the sampled front buffer — dxgkrnl holds its flip), re-arms the
    bind-time gate, counts (refresh_skips in the present-wait line;
    dxvk.heliosSkipUnretiredRefresh default ON = kill switch).
    heliosProducerFence factors the import cache; VehicleKernelFlipWait
    CODE DEFAULT NOW ON (registry 0 = kill switch). VERIFIED LIVE:
    dwm skips ~60/s on vkcube's 3 backbuffers with ZERO blocking waits
    since restart and content advancing (paintcap pair); WUDFHost
    refresh_skips=0, waits unchanged 5.8 ms/0 timeouts (the IddCx
    orderer untouched); vkcube kwait 4096/4096 armed via the code
    default. AWAITING owner windowed-Doom eyeball: the 8.9 ms dwm CS
    stalls are gone — stutter verdict decides the DCOMP default next.
    **OWNER VERDICT: STUTTER FIXED (fps ~same, as expected — the
    limiter is copy completion+observation, a separate lever). DCOMP
    DEFAULT FLIPPED ON (mesa `f5037d701ad`, ICD
    `vulkan_virtio-d6e68f7a3322` deployed): the vehicle now serves
    every Vulkan swapchain by default; HELIOS_WSI_DCOMP_PRESENT=0 =
    per-process kill switch. Verified: env-free vkcube goes vehicle
    LIVE, kwait 1536/1536 armed, acquire-gate 84 µs / 0 timeouts,
    content advancing (paintcap pair). The vehicle class (WS2 road 4)
    is DONE: kwait ON by default, skip-if-unretired ON by default,
    hwnd→target registry, insurance knob available. REMAINING WS2
    PERF LEVER: the vehicle copy's completion+observation latency
    (acquire gate ≈ 7.7 ms under Doom ≈ 1 host-GPU frame) — measure
    host-side scheduling before touching. Deferred cleanups: in-process
    slot round-trips (dead set_source fence_value), eager vehicle
    fence at init, WSI perf-counter oddity, cold-boot re-verify,
    GdiAccelMode retirement, DirectFlipCaps knob retirement.**
    **29TH SESSION — COPY-LATENCY ROOT CAUSE FOUND: QEMU delivers venus
    wire-fence completions by POLLING (10 ms fence_poll timer + an
    opportunistic poll on every guest ctrl-queue kick); the async
    fence-callback path is NOT active in the running config.** The whole
    stack below it is fast. Measurement chain (all landed, deployed):
    (a) `copy-lat` in the umd bridge (publish→waiter-observation of the
    vehicle copy fence; umd log, every 512): vkcube on an IDLE GPU avg
    ~13.6 ms, dominant bucket 10-20 ms — the Doom 7.69 ms acquire gate is
    just the tail of this past the app's natural slack. (b) `retire_lat`
    in the ICD (submit→wire-fence-retirement-observed, HELIOS_PERF
    summary + histogram): BIMODAL — ~half <1 ms, ~half 10-20 ms, max
    ≈11 ms (vehicle) / ≈16 ms (producer), RATE-INDEPENDENT (260 fps
    immediate-mode vkcube: slow mode persists and stacks, max 28 ms).
    (c) Host driver EXONERATED: tools/vk_fence_wake_probe.c (Linux,
    empty vkQueueSubmit+WaitForFences on the idle NVIDIA queue) =
    0.23 ms avg / 0.48 ms worst. (d) HOST-SIDE PROOF from the 07-06
    traced run in /tmp/helios-qemu-stderr.log (virtio_gpu_fence_ctrl/
    resp): a lone in-flight fence gets its response +10.5 ms; two
    fences submitted together both complete fast (the second submit's
    handle_ctrl kick polls the first through) — the exact
    poll+kick signature of hw/display/virtio-gpu-gl.c
    virtio_gpu_gl_handle_ctrl → virtio_gpu_virgl_fence_poll and the
    10 ms fence_poll timer. QEMU 11.0.1 enables async fences only when
    `qemu_egl_display` is set + virglrenderer ≥1.1.2 (both look
    satisfied: egl-headless display, virgl 1.3.0, callbacks v4) — WHY
    it is not active is the open host-side question (verify via gdb
    `print qemu_egl_display` on the live process or a traced relaunch:
    `--trace 'virtio_gpu_fence_*'`). FIX DIRECTION (owner decision, VM
    relaunch territory): get VIRGL_RENDERER_ASYNC_FENCE_CB active —
    expected win: every venus fence wait in the system (vehicle copy
    observation, dwm consumer waits avg 8.9 ms, IddCx waits 5.8 ms,
    D3D11 fence waits) drops from ~5-15 ms to sub-ms; the ~105 fps
    windowed Doom ceiling should lift substantially. Vkr map (host,
    for later levers): per-context worker PROCESSES, no cross-context
    CPU locks; per-guest-queue host VkQueue, family passed verbatim;
    ring≥1 retirement = empty marker submit + per-queue sync thread;
    VK_EXT/KHR_global_priority plumbed end-to-end; transfer-family
    queues map 1:1 (both usable if GPU-side contention ever becomes
    the limiter — it is NOT today). Waiter named: the vehicle device's
    `wait_calls fast=0 timeout≈2571` = DxvkFence::run() (dxvk_fence.cpp
    10 ms slice loop) servicing present_flip_wait_arm enqueueWaits —
    slices are benign event-backed re-loops. New tooling: schtask
    `helios_vkcube_imm` (immediate-mode cube, perf files
    vkcube_imm_*); helios_vkcube now also sets HELIOS_PERF →
    vkcube_renderer_perf.txt.
    **29TH SESSION cont. (owner-collaborative) — REFINED + WORKAROUND
    SHIPPED: feedback-shadow retire (mesa `b578e7d42a3`, ICD
    `vulkan_virtio-32e87a6fc919` deployed, device restarted, desktop
    verified).** Refinements from live host debugging (owner gdb/strace/
    sysctls): the async fence path IS configured (proxy flags 962);
    worker retires fences in 50-150 µs (strace); the io_uring fdmon
    theory was FALSIFIED (kernel.io_uring_disabled=2 + reboot: epoll
    shows kick→IRQ p50 33 µs yet guest retire_lat unchanged); vkr ring
    thread + guest KMD interrupt handling audited clean (spec + Linux
    virtgpu cross-checked by the owner; real non-10 ms inefficiencies
    noted: INTx shares IRQ 22 with the balloon → KMD MSI-X someday;
    serial retire-thread waits). Guest no-WSI probe
    (tools/vk_fence_wake_probe_win.c): the vn FEEDBACK channel is
    0.61 ms while the WIRE-fence channel carries the 10-20 ms class.
    DOOM (instrumented, owner run): copy-lat 11.4 ms avg (65% in
    10-20 ms, 0% <1 ms), acquire-gate 6.9 ms of the 9.5 ms frame, game
    device 97% of fences 6-20 ms — CONFIRMED as THE fps ceiling.
    OWNER DECISION: QEMU fix out of scope → WORKAROUND: the ICD retire
    thread now observes exported-fence completion via the semaphore's
    GPU-written vn feedback slot (slots now allocated for exported
    timelines — self-signaled only on this stack; counter VA attached to
    the sync, detached before pool recycling; poll ladder yield/Sleep to
    a 50 ms budget; wire path = fallback + kill switch).
    `HELIOS_RETIRE_FEEDBACK` DEFAULT ON, =0 restores wire behavior
    (A/B-proven). vkcube: retire_lat 5.6-9.2 ms → 0.25-0.33 ms (100%
    <1 ms, fb fast=100%); copy-lat 13.6 ms → 0.8 ms; content advancing;
    kill-switch run bimodal again. retire_fb fast/fallback/wire counters
    in the perf summary. PENDING: owner Doom re-run (expect the acquire
    gate to collapse and fps to rise toward the ~200 sw-path bound);
    HELIOS_RING_NOTIFY_EAGER diag knob (mesa `4d0c7d21514`, default off,
    falsified as a lever); host sysctls to revert when debugging ends:
    kernel.yama.ptrace_scope=0 (→1), kernel.io_uring_disabled=2 (owner's
    call — QEMU fence behavior identical either way).
- **Capture path**: IddCx frame drop policy vs D3D12 copy queue saturation;
  KVMFR bandwidth; 10 bpc default.
- Candidates list from the NVIDIA fix era lives in `docs/archive/ICD.md`.

## Workstream 3 — D3D11 Conformance  ← **PRIORITY 1 since 2026-08-05**

**The charter is `CONFORMANCE.md`** — what "conformant" means for this stack,
the refusal/no-op counter surface and how to read it, the ~40 `tools/` probes
catalogued into a suite, the open gaps, and how to add a test. Everything below
this line in WS3 is the session-by-session record that produced it; read the
charter first.

### Open items carried into the new stage

1. **The `DDI refusals:` counters must reach 0 against real workloads.** Two are
   known to move under 3DMark and each names a real gap:
   `gs_so_declaration_dropped` and `tess_sig_fallback`. Definition of done is a
   3DMark standard run plus a desktop session with every counter at 0, read
   through `tools/umd-gate-surface.ps1`.
   ⚠ Two corrections found while writing `CONFORMANCE.md`: the line carries
   **eleven** counters, not the nine this document used to claim (R1010 added
   `alloc_meta_format_unknown` and `readback_stride_unsafe`) — and **the
   noop-DDI hit counter, which CLAUDE.md names as the headline WS3 metric, is
   currently unreadable**: `DEVICE_NOOP_LOG_COUNT` is incremented and loaded by
   nobody, with no summary line and no gate pattern. Making it readable is
   backlog item C1 in `CONFORMANCE.md` and is a prerequisite for the rest of
   this item.
2. **3DMark Fire Strike reports `103 Display Mode List not found for given
   format` and `402`** on a failed 2026-07-24 run
   (`3DMark-Firestrike-FAILED-20260724221433.3dmark-result`). This was sitting
   in a scratch file at the repo root rather than in the roadmap; it is a real
   DXGI mode-enumeration conformance datapoint and belongs to this workstream.
   Not reproduced since — first job is to establish whether it still occurs.
3. **DXGI format coverage audit** — the format round-trip carrier landed; the
   coverage matrix does not exist.
4. **Remaining 11.1 DDI plumbing.** The threading/command-list surface is now
   real and on by default (see the 2026-08-05 stage-pivot note), which changes
   what "remaining" means — re-survey before planning.
5. **FL11 MSAA** — status recorded below as PARTIAL with un-deployed WIP
   (`ff14979`). Verify whether that is still true before treating it as open.
6. **`kmd_render` has five `#[test]` functions that can never run**
   (`present_stream_tests` in `src/virtio/gpu/mod.rs`): the crate is a
   `panic=abort` no_std cdylib and cannot host a libtest harness, and CI runs no
   `cargo test` at all. They are assurance that is not real. Move them, and the
   pure functions they cover, into `kmd_logic`, which exists for exactly this and
   does run on Linux.

## Workstream 4 — D3D12  ← **PRIORITY 2 since 2026-08-05**

**The charter is `DX12.md`; the implementation set is `docs/dx12/`, and
`docs/dx12/DECISIONS.md` is authoritative over both.** State, so this file is not
silent on it:

- **The strategy is CLOSED (2026-08-05):** Helios ships a real D3D12 UMD,
  `helios_umd12.dll`, implementing `d3d12umddi` and forwarding into vkd3d-proton's
  `ID3D12*` COM objects — the D3D11 architecture with DXVK swapped for vkd3d and
  `UserModeDriverName[2]` for `[3]`. ⛔ There is **no app-facing vkd3d arm**
  (owner directive, DECISIONS D2): Helios never ships or measures vkd3d's
  `d3d12.dll` as an application's D3D12.
- **P0 is complete (2026-08-05) and it was the load-bearing risk.** `D12-G0`
  (mingw cross build, `--list-tests` = 557) and `D12-G1` (the headless bridge
  probe: `helios_vkd3d_create_device` → DXIL SM 6.0 triangle → `READBACK` →
  exact pixels, 28 steps / 0 failures) both green on the first run.
  ⇒ **vkd3d demonstrably runs on venus**, which is what stood between a wrong
  assumption and ~200 DDI slots written on top of it.
- ⭐ **P2 is complete (2026-08-06): the engine is proven in its SHIPPING shape,
  and `umd_common` + `umd12` are built out to S3.** Four things landed, each with
  its evidence under `tmp/dx12/gates/`:
  - **`D12-G1` re-run against the STATIC clang-cl archives and PASSED**
    (`G1-static/RESULT.md`). The earlier pass was against the *mingw* DLL; D4's
    shipping artifacts had never drawn a pixel. Same probe source, one
    `-DHELIOS_G1_STATIC`, so the 28 steps cannot drift apart — normalised, the
    whole diff between the two arms is six lines of banner. ⭐ Plus the assertion
    the DLL arm could never pass: **the probe imports no `dxgi.dll`**.
    ⭐ **Measured minimal link set, which `umd12/build.rs` hard-codes at S4:
    `libhelios_d3d12_static.a` + `gdi32.lib`** — one archive, because it is a
    union of every vkd3d / dxil-spirv / dxbc-spirv object; ⛔ never `dxgi`.
    ⚠ It cost a fork fix: the archive was **not** self-contained as D4 claimed
    (D4 checked one symbol name, `CreateDXGIFactory`, and never asked about any
    other). Linking it alone gave 19 unresolved externals, five of them
    `vkd3d_debug_control_*` predicates `libvkd3d` calls unconditionally and which
    lived in the one object the static target omits. Fork `8ee4440b` splits them
    into `libs/d3d12core/debug_control.c`.
  - **S1 + S2 complete ⇒ `DECISIONS.md` D3b is done.** `slot`, the three shared
    C++ bridge headers, `log` (with `init(basename)`), `knobs`, `refusals` and
    `noop` all live in `umd_common`. Both stages carry the full check list —
    Fire Strike 3-run medians at parity (S1 GT1 220.10/GT2 211.85; S2 GT1
    222.90/GT2 206.07 against a ~221/208 baseline) and a **cold boot** with
    **zero id-1000 events of any kind**. S2's headline: `log_knob_inventory()`
    comes out **byte-identical**, one SHA256 across the pre-move DLL, the
    post-move DLL and the cold boot.
  - **S3: `d3d12umddi.h` is bindgen'd with `layout_tests(true)`** — 1 904
    assertion blocks, 102 874 lines, 399 `PFND3D12DDI*`, 15 s cold. Closes
    `UNVERIFIED-2`: it does not hurt, do not narrow the allowlist. ⭐
    `helios_umd12.dll` is **104 960 bytes, byte-for-byte what it was before** —
    5.4 MB of generated ABI, zero bytes shipped, because nothing references it.
  - ⛔ **`OpenAdapter12` still refuses**, in both DLLs, and must until S5.
    Nothing was deployed for D3D12: no INF, no registry, no `UserModeDriverName[3]`.
- **The substrate ceiling is measured, not predicted: FL 12_2 and SM 6.8.**
  `DECISIONS.md` H5 — whether vkd3d's `maintenance7` layered-`driverID` swizzle
  fires on venus — was the one open question that moved the ceiling, and
  `tools/vk_layered_driverid_probe.cpp` answered it: the nested
  `VkPhysicalDeviceDriverProperties` carries `NVIDIA_PROPRIETARY`. Confirmed on a
  live device at G1 (`ResourceBindingTier 3`, `TiledResourcesTier 4`,
  `ConservativeRasterizationTier 3`, `RaytracingTier 1_1`,
  `TypedUAVLoadAdditionalFormats 1`).
- `umd/src/adapter.rs` and `umd12/src/lib.rs` both export `OpenAdapter12` and both
  **still refuse**, and must keep refusing until the commit that makes the body
  reachable (R908) — that is **S5**, where `umd` drops its export, `umd12`'s
  becomes reachable, the INF registers slot 3 and the `UmdD3D12` kill switch lands,
  all in one commit. No D3D12 DDI code exists yet. The earlier scaffolding
  (hand-written `D3d12Ddi*` structs, eight `d3d12_*` handlers,
  `D3D12_SUPPORTED_DDI_VERSIONS`) was deleted by T6/R908 and this starts from zero
  rather than from a half-built surface.
- ⭐ **S4 is COMPLETE (2026-08-06): the engine has drawn a pixel from inside
  `helios_umd12.dll`.** `bridge/vkd3d_bridge.{h,cpp}` + `src/bridge12.rs` carry
  `helios_vkd3d_create_device` and `helios_vkd3d_serialize_root_signature` — both
  needed, because the probe's root signature is unbuildable without the second —
  through the shared `bridge_guard`, with `build.rs` linking the measured set
  (one archive + `gdi32`, never `dxgi`).
  - **`D12-G1` now has a third arm**, `-DHELIOS_G1_UMD12`, in the *same* probe
    source: 28 steps, 0 failures, pixels exact, `device final Release() -> refcount
    0`. `tmp/dx12/gates/G1-umd12/RESULT.md`. What it proves that the static arm
    could not: the engine works inside a Rust `cdylib` with `panic = "abort"`,
    `lto = "thin"`, cxx glue and the MSVC CRT — a different artifact from a probe
    `.exe`. `arm-diff.txt`: only steps 01–03 differ (prologue + the per-boot LUID);
    **steps 04–28 are byte-identical**.
  - ⭐ `helios_umd12.dll` (4 124 672 B) imports **no `dxgi.dll`**, and of its 154
    exports the 149 `cxxbridge1$…` are cxx's own leaked ABI (ARCHITECTURE §6.3
    predicts them) — the other five are exactly `DllMain`, `OpenAdapter12` and the
    three `helios_umd12_probe_*_v1`. **Zero `helios_umd_*` names**, so the Mesa
    ICD's first-hit-wins module walk cannot mistake this DLL for the D3D11 vehicle.
  - ⛔ Still nothing deployed: no INF, no registry, no `UserModeDriverName[3]`, and
    `OpenAdapter12` refuses in both DLLs.
  - ⚠ Two things S4 changed in the surrounding tooling because it had to:
    adding `cxx` pulls `link-cplusplus`, whose build script dies cross-compiling
    from Linux (`failed to find tool "lib.exe"`) and would have taken the host
    cross-check away from every S6 lane — fixed with cargo build-script overrides
    in `tools/umd12-host-check.sh`; and the `static_assert` invariant check had
    been counting its own documentation (reporting 3) since before P2.
- ⭐ **S4b is COMPLETE (2026-08-06): one venus ICD module per process, whichever UMD
  loads first.** Both cdylibs export `helios_icd_anchor_v1`; the module walk moved to
  `umd_common/bridge/bridge_icd_anchor.{h,cpp}` (ONE source compiled into both — that
  is the mechanism, not duplication); a mismatch **refuses** device creation rather
  than adopting the other module, and counts `IcdAnchorMismatch`.
  - **Gate: 18 steps, 0 failures, in BOTH load orders** (`tmp/dx12/gates/S4b/RESULT.md`).
    Both DLLs' logs name the same ICD; ⭐ the **publisher** changes with load order and
    the **answer** does not.
  - ⛔ **A correction to `ARCHITECTURE.md` §6.4, settled by the run.** §6.4's criterion
    *"both venus context ids non-zero and EQUAL"* is **wrong**: each engine builds its
    own `VkInstance`, the ICD mints a context per instance, and
    `helios_venus_current_ctx_id` is last-writer-wins — so `normal` order gives 23/23
    and `reverse` gives 25/27 **on one ICD module**. Equality is an artifact of
    ordering; the invariant is one ICD **module** per process. §6.4 not edited — owner
    call.
  - ⚠ The deploy this needed faulted four live Vulkan clients (Explorer, dwm,
    SearchHost, ApplicationFrameHost, `0xc0000005`) **inside the venus ICD**
    (`vulkan_virtio-*.dll`) when `-RestartDevice` removed the PCI device. Pre-existing
    ICD fragility at device removal, not a UMD regression — no id-1000 names
    `helios_umd`, and the desktop was verified composited afterwards. Worth a stability
    item of its own.
- ⭐ **S5 is COMPLETE (2026-08-06): `helios_umd12.dll` holds `UserModeDriverName[3]`
  and `OpenAdapter12` no longer refuses.** ONE commit, because `DECISIONS.md` §7.1 /
  R908 makes atomicity non-negotiable: the eight `D3D12DDI_ADAPTERFUNCS_0109` slots,
  the `UmdD3D12` kill switch (default OFF), the deletion of `umd`'s duplicate
  `OpenAdapter12` export, the four `.inx` edits and the `cargo make` staging of the
  second UMD all land together.
  - **`D12-G6` PASSES** (`tmp/dx12/gates/G6/RESULT.md`): four `UserModeDriverName`
    entries with `[3]` on the deployed `helios_umd12`, `InstalledDisplayDrivers` the
    two-entry form, `D3D12CreateDevice` → `0x887A0004` with the knob absent, zero
    `helios_umd*` id-1000s, desktop composited, and `umd`'s rustc warning count
    **15 → 15** measured by reverting `adapter.rs` alone.
  - ⭐ **`ARCHITECTURE.md` §13 UNVERIFIED-1 is CLOSED — slot 3 IS served
    independently.** The D3D12 client loaded the slot-3 DLL by its content-addressed
    name and logged to its own `umd12-<pid>.log` with `OpenAdapter12=1`, so the
    refusal the app saw is **ours**, not DXGI's generic answer.
  - ⛔ **Two things the doc set had backwards or open, corrected by the knob-ON run.**
    `pfnGetCaps` is called **BEFORE** `pfnGetSupportedVersions` (§1.2 had it the other
    way): `GetCaps(1074)`, `GetCaps(1007)`, then the two version calls. ⇒ **the caps
    answer cannot depend on a negotiated version, because there is not one yet**, and
    refusing 1074/1007 aborts device creation two calls in — which is why `D12-G7` is
    not reachable until L1 lands. And `pfnGetSupportedVersions` really is the two-call
    count-then-fill pair (`DDI_REFERENCE.md` §1.3's UNVERIFIED, closed without needing
    the §15 spy).
  - ⚠ The DriverStore package still carries no `helios_umd12.dll`: the INF and the
    packaging task are committed, but the `win_build_kmd` + `win_install_kmd` + reboot
    that publishes them is deliberately deferred until after S6-0, so one boot
    validates a device that can actually be created. A cold boot has no D3D12 UMD
    until then — harmless while the kill switch is off.
- ⭐ **`DECISIONS.md` D12 (2026-08-06): the DDI version is `_0110`, advertised as
  exactly ONE token, filling the `_0109`-generation tables.** `PARALLEL.md` §8's last
  not-parallelisable decision, made before the fan-out. One token means the runtime
  either negotiates `_0110` or fails the handshake, which makes §12 trap 2's closed
  enum exhaustive with a single legal arm and a wrong-sized table fill
  *unrepresentable*. The thirteen `VulkanOn12` obligations are accepted and become
  **lane** obligations — each one a lane cannot honour gets a named refusal counter.
- ⭐ **`DECISIONS.md` D13 (2026-08-06, owner): private data that CROSSES a module
  boundary is declared once, in `helios_protocol`.** The requirement traces to
  `DX12.md` §4.3 row 6 → D3c → `ResourceHeaps.md:198` (*"private data … consumable by
  their D3D11 driver"*), and the thing it names **already exists**:
  `HeliosWddmAllocPrivate` (`'HWDM'`) and `HeliosWddmOpenIdentity` (`'HIDN'`) in
  `protocol/src/wddm.rs`, already read by `umd`, `kmd_render` and the Mesa ICD. L4 and
  L8 reuse them **verbatim** — same struct, same magic, same version — which discharges
  D3c in code. ⛔ `umd_common` would have been the wrong home: `kmd_render` is `no_std`
  and does not depend on it. Per-object `pDrvPrivate` blocks stay local and typed.
- ⭐ **S6-0 is COMPLETE (2026-08-06): all 206 device / command-list / queue slots carry
  PER-SLOT counting noops, and the eleven-lane sequencer is written.**
  - **Per-slot, not per-table**, because `PARALLEL.md` §9.2 makes *"its noop hit
    counters read zero for its slots"* a per-lane definition of done and
    `CONFORMANCE.md` reads the same instrument. One const-generic
    `slot_noop<TABLE, SLOT>` monomorphises 206 times, and **the slot ordinal is
    `offset_of!(Table, field) / 8`** — derived from the ABI, so a mis-ordered name list
    cannot mis-attribute a hit.
  - ⭐ **The compile-time ABI-order proof is the real deliverable**: per table,
    `OFFSETS.len() == size_of::<T>()/8` and `OFFSETS[i] == i*8` for every `i`, on every
    build of either platform. That is `DECISIONS.md` §4.1's "slots 38-40" scar — a
    `sed` line offset misread as a member index — made unrepresentable.
  - ⭐ **Install order is structural**: `Filling<'a, T, Stage>` is `#[must_use]`, carries
    the `&mut`, and each lane's `install` names the previous lane's marker. S6-0 wrote
    the whole chain, so a lane's diff against `tables12.rs` is **empty** — fewer merge
    points than §5's original one-line-per-lane protocol, not more.
  - **Evidence: 40 steps, 0 failures** (`tmp/dx12/gates/S6-0/RESULT.md`). Driven by
    `tools/d3d12_fill_table_probe.cpp` through two probe exports, because the runtime
    cannot reach `pfnFillDDITable` until L1 answers caps. It poisons a buffer, asks for
    `size − 8`, and checks the **guard band is untouched** — the R702 failure a
    prefix-only test cannot see.
  - ⭐ **And the sizes came back 992 / 600 / 56**, exactly what `D12-G5` measured this
    runtime handing WARP at `_0110`: the bindgen structs are byte-identical to what the
    runtime negotiates, confirming D12 from the driver's own side.
- **S6-0b is COMPLETE (2026-08-06): `device12.rs`** — the private block,
  `pfnCalcPrivateDeviceSize` / `pfnCreateDevice` / `pfnDestroyDevice`, the engine
  device, and the `DeviceUnderConstruction` unwind guard. Validate-before-construct,
  one function of `Flags` for the size, the corelayer union arm fixed at `_0062` by
  D12's one-token set, and a per-device teardown readout that makes the block's fields
  genuinely *read* rather than merely stored (the R908 rule forces that choice).
- ⭐ **L1 (caps) is LANDED (2026-08-06), and `D3D12CreateDevice` now builds a real
  vkd3d `ID3D12Device` through the DDI.** `pfnGetCaps` answers all 43 types: the ~13
  with an explicit "device creation fails" runtime string individually, the rest by
  §11.2's measured safe default. The 43-way policy was derived and then
  **adversarially verified**, three lenses per risky answer; eight drew a refutation
  that survived and two changed the code.
  - ⛔ **The load-bearing decision is a COUPLING: this ships FEATURE LEVEL 11_0**, and
    every OPTIONS tier is legal only because of that. The level is asserted by the
    driver, never inferred, and asserting 12_0 arms cap floors that are lies on a
    driver whose descriptor/resource/recording lanes are counting noops — `D12-G5`
    measured that exact failure. Each raise belongs to the lane that earns it and must
    move the level and its floors **together**.
  - ⛔ **Four caps where the §11.2 zero-fill default is ILLEGAL**, which is the least
    obvious thing in the lane: `1002` (`IOCoherent` must be TRUE on amd64), `1004`
    (zeroed lane counts fail device creation), `1003` (zeroed alignments are four
    separate errors), `1088` (`EXECUTE_INDIRECT_TIER` has no zero enumerator, so a
    zero-fill writes an out-of-range tier the runtime clamps **silently**).
  - ⛔ ~~**`MaxSamplerDescriptorHeapSize` is 2048, not `SUBSTRATE.md` §4.5's ">= 4000".**~~
    **FALSIFIED 2026-08-06 by the runtime itself, at `D12-G7`.** ETW
    `Microsoft-Windows-Direct3D12`: `Driver's MaxSamplerDescriptorHeapSize is too small`
    (strings:113) with 2048. **§4.5 was right and this "correction" was a LAYER
    CONFUSION**, which is the part worth keeping: both arguments for 2048 —
    `D3D12_MAX_SHADER_VISIBLE_SAMPLER_HEAP_SIZE` and
    `baselines/d3d12-caps.csv:85` — are **API-level**, and the runtime is what clamps
    the DDI value down to them. `d3d12_caps_dump.cpp` reads the *post-clamp* number
    through the API, so it could not have disagreed with 4000 whatever the driver
    reported. ⇒ **an API-level capture cannot falsify a DDI-level requirement.** The
    value is now 4000, which is also exactly the guest's `maxSamplerAllocationCount`.
  - ⛔ **Refusing an unknown `pfnFillDDITable` type LOSES THE DEVICE.** The runtime asks
    for `D3D12DDI_TABLE_TYPE_0096_EXTENDED_FEATURES` (27, 32 B) on a baseline device.
    Unknown tables are now stub-filled at the runtime's own byte count and counted —
    filling selects no *shape*, which is what §7.4 actually forbids, while a refused
    table has NULL slots the runtime may still call through.
- **`D12-G7`'s FIRST failure, and the blocker it named — now CLOSED by L1's second
  half below, kept because the chain it measured is still the reference**
  (`tmp/dx12/gates/G7/RESULT.md`). The whole chain runs: `OpenAdapter12` → caps →
  versions → `CalcPrivateDeviceSize` → **`CreateDevice` building a real vkd3d device on
  venus ctx 19** → all four table fills at **992 / 600 / 56 / 32**, with the
  command-list table filled **twice** and both `hRTTable` handles (`0x3E0`, `0x638`)
  stashed → `DestroyDevice` → `CloseAdapter`. It fails at **`0x887A0020`**, which is the
  runtime rejecting an inconsistent caps **set** — not our own `0x887A0004` refusal.
  ⭐ The HRESULT moving is the result: the failure went from *"the driver said no"* to
  *"the driver said something wrong"*.
  - ⇒ **The blocker is three device-core slots still counting noops.** The runtime calls
    them **2 824 times inside `D3D12CreateDevice`**: `pfnCheckFormatSupport` 93 times
    (the 91-format sweep §11.1 predicted), `pfnCheckMultisampleQualityLevels` **2 730**,
    `pfnQueryNodeMap` once. A noop returns 0, i.e. *"no format supports anything"*.
    They need a `bridge12` entry point into `ID3D12Device::CheckFeatureSupport`
    (C++, VM-only). ⭐ `umd/src/forward/format_caps.rs` is the D3D11 precedent and its
    `D3D10_DDI_FORMAT_SUPPORT` bits are **identical** to `D3D12DDI_FORMAT_SUPPORT`'s —
    including the trap: `DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM` (89) must be refused
    with the explicit `_NOT_SUPPORTED` sentinel `0x8000_0000` and **not a bare 0**,
    which the D3D11 runtime rejected with the *same* `0x887A0020` on the same box.
- ⚠ **A UMD path change needs a device restart to take effect.** A deploy without
  `-RestartDevice` rewrote `UserModeDriverName[3]` and the next new process still loaded
  the previous content-addressed DLL: dxgkrnl caches the resolved UMD path. Cost one
  confusing gate run whose log showed the old hash and the old counter names.
- ⭐ **L1's SECOND HALF is LANDED (2026-08-06, `2c7460e`): the format/MSAA slots are
  real and `D3D12 noop DDI hits:` reads `slots=0/206`.** Every one of the 2 824 calls
  the runtime made into counting noops inside `D3D12CreateDevice` now reaches a body —
  `pfnCheckFormatSupport`, `pfnCheckMultisampleQualityLevels`, `pfnGetMipPacking`, plus
  `pfnQueryNodeMap` and `pfnGetImplicitPhysicalAdapterMask` in L9's file, landed early
  because the same sweep needs them. Full write-up: `tmp/dx12/gates/G7/RESULT.md`.
  - ⭐ **No cxx bridge was needed, and that widened the fan-out rather than narrowing
    it.** The handoff specified a C++ module into `ID3D12Device::CheckFeatureSupport`;
    `bridge12` already hands Rust a borrowed `ID3D12Device`, so the `windows` crate's
    vtable call reaches the identical slot. ⇒ the lane type-checks on the **Linux
    host** (`PARALLEL.md` §7), which the C++ route would have taken away. The added
    `Win32_Graphics_Dxgi_Common` feature is types only: `dumpbin /IMPORTS` on the
    release DLL is unchanged — **no `dxgi.dll`, no `d3d12.dll`**.
  - ⭐ **MEASURED: `pfnCheckFormatSupport` writes the small `D3D12DDI_FORMAT_SUPPORT`
    enum, NOT API-level `D3D12_FORMAT_SUPPORT1`.** This had to be settled by experiment
    rather than by reading the header, because the D3D11 side of this project holds the
    *opposite* result for its own DDI (`umd/src/forward/format_caps.rs:15-19`: "D3D11
    harmonized the DDI with the API enum … translating regresses even a plain
    `D3D11CreateDevice`"). `Umd12FormatCaps=1` (API passthrough) truncates the runtime's
    format sweep at **12 formats / 271 MSAA queries** against **23 / 600** for the DDI
    encoding. Arm 0 is the default; arm 1 stays reachable (CLAUDE.md rule 8).
- ⭐⭐ **`D12-G7` PASSES (2026-08-06, `23fbf44`): a real `ID3D12Device` exists on the
  Helios adapter, built by the D3D12 runtime through Helios' own `d3d12umddi`
  implementation on top of vkd3d on venus.** `D3D12CreateDevice` → `S_OK` at FL 11_0,
  `nodes=1`, `final Release()` → refcount 0. The runtime then builds its **own** objects
  through the DDI — a root signature, two graphics PSOs, a command pool, the
  extended-features handshake — and reports
  `UMAdapterVersion = UMDeviceVersion = 0xC0050006E0000`, D12's single `_0110` token
  negotiated end to end. Full write-up: `tmp/dx12/gates/G7/RESULT.md`.
  - ⭐ **The fix was already in this repository, in the D3D11 driver.** Six
    build/deploy/run cycles were spent bisecting the runtime's per-format contract
    before the answer turned out to be written down:
    `umd/src/forward/queries.rs:104-164` does **not** forward the engine's
    quality-level answer, it *derives* it from the same predicate that decides the
    format-support multisample bits, *"because the Microsoft runtime validates
    `CheckFormatSupport` and `CheckMultisampleQualityLevels` as a coherent
    feature-level contract"*. ⛔ **Two independent engine queries make a coherent pair
    a coincidence; one shared predicate makes disagreement unrepresentable.**
  - ⛔ And the predicate rests on `umd_common/src/format.rs`, whose `msaa_ineligible`
    field doc is the entire answer to where the sweep was stopping — the depth/stencil
    **read** views `R32_FLOAT_X8X24_TYPELESS` (21), `X32_TYPELESS_G8X24_UINT` (22),
    `R24_UNORM_X8_TYPELESS` (46), `X24_TYPELESS_G8_UINT` (47): *"WARP reports zero
    quality levels above 1x and the runtime rejects advertising them as MSAA render
    targets."* The sweep stopped at **21**, and four earlier arms each changed the
    format bits while still forwarding non-zero levels — so the one answer that works,
    *neither bits nor levels*, was never tried.
  - Two further answers the runtime named in English over ETW, one cycle each:
    `ROW_MAJOR_LAYOUT_SUB_CAPS::DepthPitchAlignment` **512 → 256** (strings:85 — the
    bound is *relative*: the identical 512 in `BaseOffsetAlignment` passes, because a
    depth pitch is `RowPitch * Height` and `RowPitch` is only `PitchAlignment`-aligned);
    and `OPTIONS_0102::MaxSamplerDescriptorHeapSize` **2048 → 4000** (strings:113).
  - **Counters, all expected-non-zero and documented as such:**
    `CapsFormatSupportCalls=93` and `CapsMsaaCalls=2730` — byte-for-byte what `D12-G5`
    measured the runtime handing WARP; `CapsTextureLayoutSetEnd=2` (the enumeration
    terminating as WARP's contract does); ⭐ `CapsFormatNotSupportedSentinel=1` — format
    89's `_NOT_SUPPORTED` trap discharged and **observed**; `CapsMsaaIneligibleFormat=124`;
    `CapsMsaaBitsDropped=4`.
  - **Gate criteria:** `HwQRef` never moved; the knob off restores `0x887A0004` exactly
    (`D12-G6` still passes); knobs deleted; zero id-1000 events naming `helios_umd`
    across ten device restarts (all 60 name the venus ICD — pre-existing); desktop
    composited afterwards with `dwm` started after the last restart; the D3D11 UMD hash
    byte-identical throughout.
- ⚠ **25 noop slots were hit on the passing run, and they ARE the fan-out's work list** —
  `DDI_REFERENCE.md` §14.0's prediction landing exactly: *"`D3D12CreateDevice` alone
  drives 27 of the 124 core slots"*, the runtime building its own internal pipelines.
  Blend / depth-stencil / rasterizer state, `pfnCalcPrivateShaderSize`,
  `pfnCreateVertexShader`, `pfnCreateComputeShader`, PSO create/destroy, root signature,
  command pool, `pfnMakeResident`, `pfnGetDebugAllocationInfo`. Those are **L6**, **L2**
  and **L4**, and driving them to zero is those lanes' definition of done
- ⭐⭐ **S6 ROUND 1 IS COMPLETE (2026-08-06): four DDI lanes landed and
  `D3D12 noop DDI hits:` reads `slots=0/206`.** Every one of the 25 slots the passing
  `D12-G7` run hit now reaches a real body. Static coverage went **5/206 -> 105/206**
  (device-core 98/124, command-queue **7/7**, command-list 0/75). Evidence:
  `tmp/dx12/gates/G7-s6r1/RESULT.md`; commits `81cf82d` -> `1ce9939`.
  - **L2+L7** (30 slots, `forward12/queue.rs` + `fence.rs`) — queues, command pools,
    recorders, command lists, command signatures, fences, query heaps. ⭐ The WDDM-context
    question is settled **decisively and not by the doc**: the runtime enforces the scoping
    itself (*"CreateContextCb or CreateContextVirtualCb called outside of queue creation"*,
    fullstrings:10597), so a lane that skips it makes the object **unobtainable for every
    later lane**, L8's `pfnPresent` included. ⭐ And grepping the D3D11 driver confirmed
    what its own context is FOR — every use of `HeliosDevice::context` is present-path
    (`umd/src/forward/present.rs:786`), never submission — so §6.4's *"cardinality, not
    kind"* is confirmed from source. ⚠ The context class is **legacy**, which forecloses
    `pfnSubmitCommandCb`; the doc-set contradiction (D5/§9.2 say VIRTUAL) is recorded at the
    site with its cost rather than left for L8.
  - **L4** (16 slots, `forward12/resource12.rs`) — committed / heap / placed creates,
    map/unmap, residency, the four introspection slots. Cross-process sharing
    (`pfnOpenHeapAndResource`) refused with named counters: it is what discharges D3c and it
    needs `helios_protocol` verbatim, which an in-process triangle does not.
  - **L5** (15 slots, `forward12/descriptors.rs`) — heaps and all six view creates. ⭐ Found
    the fifteenth slot (`pfnCreateSamplerFeedbackUnorderedAccessView`, appended late to the
    `_0109` struct far from its siblings, which is why a reader counts 14). ⭐⭐ The
    `ead692e` struct-return hazard is **CLEARED with both halves quoted**: vkd3d's
    `resource.c:9146` takes a hidden out-pointer and windows-rs 0.58's vtable
    (`Direct3D12/mod.rs:652`) declares exactly that — they agree, no shim.
  - **L6** (38 slots, `forward12/pso.rs` + `shaders.rs`) — ⭐⭐ **the DDI's shader bytecode is
    unusable by vkd3d as delivered.** It arrives as a bare `DxilProgramHeader` and three
    engine readers reject a non-`'DXBC'` tag, so the lane **synthesises a DXBC container**.
    `DDI_REFERENCE.md` §12.3 had left this open; the gate closed it — `L6ShaderDxbcContainerSeen=0`
    on both shaders the runtime built. ⭐ And the `DepthBias` `INT`/`FLOAT` trap is resolved
    **by never converting it**: the pipeline-state STREAM's `RASTERIZER2` keeps it a
    `FLOAT`, which also carries mesh/amplification shaders the legacy struct cannot express.
  - ⭐ **The fan-out shape worked, and the pre-VM adversarial pass paid for itself twice.**
    Four lanes authored concurrently in isolated worktrees, each verified by an adversarial
    refuter and repaired before merge: that caught two **blockers** (`Slot::clear()` on a
    live heap; array-ness decided from `ArraySize` alone) and a missing null-descriptor arm
    that would have met a **legal** D3D12 call with `pfnSetErrorCb`, i.e. device removal.
    None of it cost a VM lease.
  - ⭐ **The `PARALLEL.md` §10 lens review then found six more**: 7 reviewers, 27 raw
    findings, **21 rejected** by an adjudicator that re-verified each against source. The
    survivors included a **blocker** — `pfnDestroyHeapAndResource` freed a heap it did not
    own, contradicting the create site's own guard, so the first `Release()` of any placed
    resource tore down its live parent heap — and the one enum proof of 21 that compared
    against a transcribed literal inside the block whose preamble says none are.
- ⛔ **TWO counters were graded for a world that had ended, in one merge, and that is now a
  thing to check rather than a coincidence.** L5's `ViewResourceUnavailable` still said
  *"expected non-zero until L4 lands"* after L4 landed in the same batch — where every hit
  now removes the device; and `DebugAllocationInfoEmpty` said *"a zero reading is the
  finding"* and then read zero on a healthy run. **A counter's grading is a claim and it
  goes stale like any other.** The first was caught by a reviewer, the second by the gate.
- ⚠ **`pfnGetDebugAllocationInfo` went from 4 calls to 0** between the two `D12-G7` runs, on
  otherwise byte-identical device creates (`Flags=0x0` both times). Why is **not
  established** and is recorded as unknown: either the four calls were the runtime reacting
  to the noop'd path (a zero-byte PSO private block), or the slot is debug-layer traffic
  enabled some other way. A run with the debug layer deliberately on settles it.
- ⭐ **`tools/umd12-slot-coverage.sh` is new**, and it exists because a slot with **two
  owners is silent**: the install chain runs lanes in order over one table, so the later
  wins and the earlier handler is unreachable — it compiles, both files look complete, both
  lanes report the slot done. Now an exit code. ⚠ It was wrong three times in one sitting
  (counted its own documentation; missed raw-pointer slots; missed rustfmt-wrapped
  assignments) before the matcher was rebuilt to join continuation lines. Its two remaining
  blind spots both over-report and are recorded at the site.
- **Round 2, for `D12-G8` (a triangle, owner-visible):** the command-list table is **0/75**.
  It needs **L3a** (draw, fixed function, IA/SO/OM), **L3b** (root arguments, descriptor
  binding, clears), **L3c** (copy, resolve, barriers, queries) and **L8** (present — ⛔ not
  parallelisable, and it touches the `HeliosPresentRenderCmd` identity channel shared with
  the KMD and the D3D11 driver). ⚠ L6 routed one obligation to L3a:
  `pfnSetPipelineState` must re-apply the PSO's baked depth bias and strip-cut even when the
  PSO declares them dynamic — `SUBSTRATE.md` §4.5's *"precise inversion of the Vulkan mental
  model"* — and `L6PsoDynamicStateFlagForwarded` counts the exposure until it does.
- ⚠ **The deferred INF / cold-boot half of S5 is still deferred** and is now worth doing: the
  DriverStore package carries no `helios_umd12.dll`, so a cold boot has no D3D12 UMD.
  Harmless while `UmdD3D12` is off, and one reboot would validate a device that can actually
  be created.
  (`PARALLEL.md` §9.2).
- ⭐⭐ **THE FEATURE-LEVEL TARGET IS FL 12_1. FL 12_2 IS OUT OF SCOPE, AND THE BLOCKER
  IS WDDM, NOT CAPS** (owner, 2026-08-06). `DX12.md` **§4.4** is the ladder. FL 11_0 is
  what `caps12.rs` ships today and is a **staging value**.
  - ⛔ **FL 12_2 requires a WDDM 2.9 adapter; Helios declares 2.1 deliberately.**
    `kmd_render/src/ddi/wddm_surface.rs`'s module doc has the mechanism: 2.1 is *"below
    the MPO3 requirement boundary"*, and at **2.2+** DWM treats the adapter as a
    Display-Core/MPO3 presentation device — Helios registers no MPO3 KMD interface, so
    it *"fails fast with `E_NOTIMPL`"*, which is exactly why the 3.2 level is not
    deployable. ⇒ 12_2 costs a new `WddmSurface` level across **five coupled sites**,
    **plus** the MPO3 interface, **plus** re-validating the display path that currently
    composites the whole desktop. A display-stack workstream wagered against a milestone
    already met — **revisit it as its own effort**, with `WddmSurface` + MPO3 as the
    deliverables and the feature level as a consequence.
  - ⭐ **FL 12_1 needs no KMD change at all, and no lane `D12-G8` did not already need.**
    Its five floors are typed-UAV-load + `ResourceBindingTier >= 2` +
    `TiledResourcesTier >= 2` (12_0 → **L5**, **L4**, **L2**) and ROVs + conservative
    raster ≥ 1 (12_1 → **L6**) — the triangle's own L2 → L6 → L5 → L4 order. **L9 and
    L3c stay able to trail**, as `PARALLEL.md` §4 says.
  - ⭐ **The substrate is not the constraint at any level** — vkd3d logs `DX Ultimate
    supported!` and §4.4 tabulates all 23 floors against `baselines/d3d12-caps.csv`.
    None of the five 12_1 floors is marginal (binding tier 3, tiled tier 4, conservative
    raster 3 all exceed requirement).
  - ⛔ **The level and its floors move in ONE commit, by the lane that earned them.**
    `D12-G5` measured the retail failure verbatim.
  - ⚠ FL 12_1 is *expected* to be reachable at the current WDDM 2.1 surface (D3D12 needs
    only WDDM 2.0, and no 12_1 floor is a display-path feature). The commit that raises
    the level must **confirm** that — a WDDM-shaped ETW refusal at 12_0/12_1 falsifies it.
  - ⛔ **CORRECTION, kept because it cuts the other way: tiled resources are NOT a
    `kmd_render` dependency.** An earlier note called `TiledResourcesTier >= 3` "the long
    pole" and "not a UMD-only job", on `caps12.rs`'s claim that the decorative page
    tables cannot give the zero-read guarantee. Wrong twice: the hard guarantee is at
    **tier 2** (required at FL 12_0, so the 12_1 target needs it), and it is **host-side
    and already true** — tiled resources ride Vulkan **sparse binding**
    (`vkQueueBindSparse`), no guest page table is in the path, and the guest exposes
    `residencyNonResidentStrict = true` beside `sparseResidencyImage2D/3D`,
    `sparseResidencyAliased` and the standard block shapes
    (`research/guest-vulkaninfo-full.txt`). ⇒ **UMD-only**: L4 plus L2's two tile-mapping
    slots. ⚠ Backed on paper, **unexercised** — a `D12-G9` verify item.
  - ⭐ **The lesson, and it runs both ways:** *a code comment asserting a dependency is
    not evidence of one* (one grep killed the tiled-resources claim after it had spread
    to three documents) — and the dependency that IS real was also sitting in a module
    doc. **Read the KMD's own docs before costing a feature level.**
  - ⚠ SM `>= 6_5` is a **12_2** floor, so `shader_models`' short `{5.1, 6.0}` list stays
    legal all the way to 12_1; L6 raises it when the shader creates become real.
- **Next: `D12-G8`** — a triangle through the DDI, owner-visible. That needs the 25 slots
  above, i.e. the `PARALLEL.md` §4 fan-out: **L2** first (it mints the WDDM context),
  then **L6** → **L5** → **L4**, then L3a/L3b, then L8 — followed by the §10 review pass.
- **Three instruments landed with L1's second half**, each of which paid for itself:
  - `tools/d3d12_format_matrix_probe.cpp` — the probe `GATES.md` §3.2 names. It
    `LoadLibrary`s the deployed `helios_umd12.dll`, takes a borrowed `ID3D12Device` off
    `helios_umd12_probe_create_device_v1`, and dumps the **engine's** per-format
    `Support1`/`Support2` and quality levels at counts 1/2/4/8/16/32 as CSV. No adapter
    restart, no `UmdD3D12` knob, no D3D12 runtime, no `d3d12.lib`/`dxgi.lib`. It is what
    made the engine's answers checkable against the driver's at all.
  - **`trace_line!` reaches `umd12`** with `caps12`'s multisample slot as its first
    per-op consumer: all 2 730 calls, **zeros included**, gated on `Umd12Trace`. Two
    runs had been spent inferring the rejected `(format, sample count)` from call
    *counts*; the trace answered it in one.
  - The `CheckFormatSupport` evidence line now carries the **engine's raw `s1`/`s2`**
    beside the driver's answer. Without it the first failure could not be diagnosed from
    the log at all — it took an ETW capture — because the log recorded only the derived
    value and the whole question was how it was derived.
- ⭐ **S6 is the bulk — 214 driver-side slots — and it FANS OUT. The split is
  `docs/dx12/PARALLEL.md`:** 11 lanes with exclusive file ownership, an append-only
  protocol for the four shared files, and a lease on the VM. Two things gate the
  fan-out and are worth knowing before planning around it:
  - ⭐ **Lanes compile on the LINUX HOST, so VM contention never arises.**
    `tools/umd12-host-check.sh` type-checks the whole 214-slot
    surface in seconds with no WDK: bindgen runs on the VM and `umd12/build.rs`
    serves the committed `umd12/bindgen/cached/d3d12umddi.rs` into `OUT_DIR` on a
    non-Windows host. ⛔ Never used for a shipping DLL — Windows regenerates from
    the header every time and warns `… is STALE …` on drift. Proven by fault
    injection, which also settled that the DDI typedefs are **`extern "C"`, not
    `extern "system"`** — the same ABI, a different Rust type, and a mistake that
    would otherwise have been made 214 times.
  - ⛔ **Authoring parallelises; validating does not.** There is one VM and one
    adapter. `win_install_umd` disables the PCI device, benchmarks are exclusive,
    and `HKLM\SOFTWARE\Helios` knobs are machine-global — one agent's A/B arm
    silently applies to another's measurement. Gates are run once, by the
    integrator, against merged code.
  - **S6-0 is the keystone**: stub all 214 slots with counting noops *before* any
    lane starts, so every lane is substitutive rather than additive, `D12-G7` is
    green before any lane lands, and the noop hit counters become each lane's
    progress metric (which is already `CONFORMANCE.md`'s charter).
- **The fork has content now.** `vkd3d-proton-helios` is on branch `helios` at
  `github.com/rupansh/vkd3d-proton` (remote `helios`; the checkout's `origin` is
  still upstream — ⛔ push to `helios`): D4's two DXGI-free exports, plus a
  Windows-bash fix to `tests/test-runner.sh`. The "zero Helios divergence, nobody
  knows why it was vendored" note is history.
- **The KMD is not on the critical path** — three small items (`K1` NodeOrdinal
  validation, `K2` `NoPatchingRequired`, `K3` `ApertureSegmentCommitLimit`), none
  required for the first triangle, and none of the 31 reachable-but-unset
  `DxgkDdi*` slots is required for a baseline D3D12 device. The multi-engine /
  residency / tiled-resource expectations in the earlier version of this bullet
  were wrong: `docs/dx12/KMD_IMPACT.md` walks each one.
- **P1 is complete (2026-08-05) — `D12-G5`, the WARP spy proxy.**
  `tools/d3d12_spy/` is a proxy `d3d10warp.dll` that forwards to Microsoft's own
  D3D12 UMD with a counting thunk on **all 206 driver slots** (+32 armed DXGI
  slots), driven by four workloads, six caps-mutation arms, four version-floor
  arms, and one run on the real Helios adapter. Containment check passes: 23
  caps types asked, all within the 43, none of the 7 deprecated. Artifacts
  `tmp/dx12/gates/G5/{spy.log,answers.md}`; `DX12.md` §4.2 has the summary and
  `DDI_REFERENCE.md` §15.0 the merged result. Six results that change P2-P4:
  - ⭐ **`D3D12DDI_SUPPORTED_0040` is accepted by this Windows build and a
    triangle presents on it** — 96 core + 58 CL slots instead of 124 + 75, i.e.
    **169 baseline slots instead of 214**. The default the runtime negotiates is
    `_0110`, not `_0109`. The choice between them belongs to P3.
  - ⭐ **`pfnCreateShader` receives a RAW stream, never a DXBC container**
    (`dword[0]=(type<<16)|(major<<4)|minor`, `dword[1]=length in dwords`,
    `dword[2]='DXIL'`), and the runtime converts SM 5.1 DXBC to DXIL first.
  - ⭐ **The caps set is validated as ONE contract at retail** —
    `D3D12CreateDevice` fails `0x887A0020` with an English reason on ETW
    **`Microsoft-Windows-Direct3D12`** (⛔ *not* `DxgKrnl`/`AzureTriage`, which
    said nothing). But an **out-of-range tier is clamped silently** — the
    advertise-what-is-backed hazard with the loud failure removed.
  - **No DXGI table**: `D3D12DDI_TABLE_TYPE_DXGI` is never requested, across 20
    flip-model presents. Present arrives on the command-list table only.
  - **A WDDM 2.1 adapter is not a barrier** — forced `_0110` negotiated cleanly
    on the real Helios adapter, so `Wddm2_1GpuMmu` does not cap the DDI version.
  - **`pfnGetPresentPrivateDriverDataSize` is called once per present**, right
    before `pfnPresent` — a second candidate identity channel, to test at G8
    beside the `pfnRenderCb` plan rather than instead of it.
  ⛔ Traps worth keeping: `C:\ProgramData\Helios` has a junction loop, so a
  wildcard *inside* a path silently finds nothing (use `-Filter`); Route B needs
  `pnputil /restart-device` because dxgkrnl caches the UMD path at StartDevice;
  and the spy's own `UmdD3D12Spy` gate refuses everything when the knob is
  absent, which faked four "the runtime rejected this DDI version" results.
- **Next: P2 / `D12-G6`** — the `umd_common` extraction (stages S1-S2), the
  first phase that writes driver code. `OpenAdapter12` still refuses.


**30th session (2026-07-07) — 3DMark bring-up: LUID gap FIXED, FL11 ceiling
root-caused.**

- **Vulkan/DXGI LUID identity gap — FIXED, DEPLOYED, VERIFIED (mesa
  `23b10bb6d80`, main `e0b462f`; ICD `vulkan_virtio-c2919595f95d`).** The venus
  ICD reported `VkPhysicalDeviceIDProperties::deviceLUIDValid=false` ("Phase 6
  concern" stub in `helios_init_renderer_info`), so no VkPhysicalDevice carried
  the guest WDDM adapter LUID that DXGI reports. UL/3DMark Steel Nomad (Vulkan)
  selects an adapter via DXGI then matches into Vulkan by `deviceLUID` → found
  nothing → "VkPhysicalDevice with device LUID X not found". Fix: plumb
  `helios->adapter_luid` (captured from D3DKMTEnumAdapters2 at open, == the DXGI
  AdapterLuid) into `info->id` (has_luid=true, node_mask=1, luid verbatim).
  Verified same-boot: vulkaninfo `deviceLUID dfb16300-00000000` == WDDM adapter
  `luid 00000000:0063b1df`, `deviceLUIDValid=true`. (LUIDs change per
  device-restart — the fix reads adapter_luid dynamically, tracks it. dxvk's
  own findAdapterByLuid / D3DKMTOpenAdapterFromLuid also benefit.) **Owner to
  re-test Steel Nomad.**
- **Fire Strike (D3D11 FL11_0) "no GPU" — the Helios adapter is FL10_0; being
  raised gate-by-gate. It is NOT a KMD/adapter ceiling (that theory falsified)
  — it's a sequence of UMD caps bugs.** The engine is genuinely FL11 (bridge
  creates the dxvk device at `D3D_FEATURE_LEVEL_11_0`; all FL11 DDIs wired).
  Everything is behind `HKLM\SOFTWARE\Helios!FeatureLevel11`, now an integer
  MODE (0=explicit FL10 fallback, 1=full FL11 and the absent-value default as
  of 2026-07-24, 2=diagnostic pipeline-only); knob=0 = exact FL10 baseline,
  dwm-safe. **THE tool that cracked it: the
  `Microsoft-Windows-DXGI` ETW provider prints d3d11.dll's exact rejection
  string** (the debug layer / DXGI InfoQueue / DBWIN / DxgKrnl-AzureTriage all
  gave 0 messages — the failure is device-less). Recipe: `logman start
  helios_dxgi -p Microsoft-Windows-DXGI 0xFFFFFFFFFFFFFFFF 0xff -o x.etl -ets` +
  `logman update helios_dxgi -p Microsoft-Windows-Direct3D11 ... -ets`, run the
  probe, `logman stop`, `tracerpt x.etl -o x.xml -of XML -y`, read `<Data
  Name="Message">`/`Code`. Gates cleared: (1) **"Driver returned invalid
  pipeline caps"** — 3DPIPELINESUPPORT is a BITMASK
  `(1<<Level)` OR'd, not the bare enum; we wrote 11_0=2 = bit1-only = invalid;
  fixed FL11=0x7, FL10=0x1 (the old 10_1=1 worked by luck = the 10_0 bit).
  (2) **"Driver doesn't support compute on FL11"** — SHADER caps now advertise
  0x2 (compute). (3) **"MSAA quality reported to be 0"** — FL11 requires every
  render-target format to support 4x MSAA and does NOT exempt 96-bit R32G32B32;
  CheckMultisampleQualityLevels now floors RT formats to >=1 at 1/2/4/8 +
  check_format_support advertises MULTISAMPLE_RENDERTARGET for RTs — PARTIAL:
  the runtime advances past formats 5-8 but still hits the MSAA error on a later
  format/count. **NEXT (owner directive): conform to the D3D11.3 functional
  spec** (https://microsoft.github.io/DirectX-Specs/d3d/archive/D3D11_3_FunctionalSpec.htm)
  — exact per-format/per-sample-count FL11 MSAA requirements. Committed main
  `ff14979` (WIP, un-deployed source; the all-count MSAA-log build was not
  installed). Probe: `tools/d3d11_fl_probe.cpp`, schtasks `helios_flprobe`,
  session 1 (session-0 win_exec fails all levels for a context reason, not FL).
  ✅ **The owner directive's READ half is DONE, 2026-08-05** — §19.2.5 *Required
  Multisample Support*: 1x/4x/8x required with standard patterns, **4x for ALL**
  output formats (so this entry's "does NOT exempt 96-bit R32G32B32" was right),
  **8x only below 128 bits per sample**, and *"Other MSAA counts and patterns are
  optional"*. ⇒ Helios meets the first two and **over-reports two**: 8x on
  128-bit formats, and 2x/16x which are not required at all. ⛔ Not changed in
  code — the residual rejection above is still UNVERIFIED and narrowing could
  move which format/count first trips it. Full table + the decision that remains:
  `CONFORMANCE.md` §4(a) and backlog **C6**.

**31st session (2026-07-07) — Fire Strike launch blocker moved past FL11:
legacy DXGI output mode-list fails on the IddCx logical output.**

- Owner rebooted after an IDD/client wedge; desktop composition is healthy again
  (`HeliosRenderAdapter=1`, both Display devices `OK`, IDD frames flowing with
  dirty-rect D3D11 fallback copies). The latest Fire Strike result
  (`3DMark-FireStrike-FAILED-20260707155504.3dmark-result`) exits both Demo and
  GT1 during `SINGLE_INIT_BEGIN` before workload rendering:
  `IDXGIOutput::GetDisplayModeList` returns `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`
  (`0x887a0022`), then 3DMark reports "Workload produced no results".
- Repro probe on the same boot: DXGI enumerates `\\.\DISPLAY6` under a fixed
  logical Helios adapter LUID `00000000:000078c5`; every
  `IDXGIOutput::GetDisplayModeList` / `GetDisplayModeList1` call returns
  `0x887a0022` for the tested formats. `D3DKMTOpenAdapterFromGdiDisplayName` on
  that same `\\.\DISPLAY6` opens the real Helios render adapter LUID
  `00000000:0038c127`, and `D3DKMTGetDisplayModeList` succeeds with 1064 modes.
  DXGI ETW around the probe records `IDXGIOutput_GetDisplayModeList` stop events
  with `m_Ret=2289696802` (`0x887a0022`) and the output object bound to
  `\\.\DISPLAY6`, but no richer rejection string.
- Falsified: RDP/session-context cause. The workload and probes run in active
  console session 1, not an RDS session. Also falsified: missing IDD modes in
  general (CCD/KMT mode lists exist), and UMD WDDM1.3 Present1/MPO callback
  table as the immediate launch blocker (current UMD logs show the DXGI 1.3 base
  table populated and D3D11 device creation succeeds in probes).
- Current read: this is the architectural split between the IddCx/IndirectKmd
  display adapter that owns the output and the real Helios render adapter that
  owns D3D/Venus. Fire Strike's legacy fullscreen init assumes
  `IDXGIOutput::GetDisplayModeList` works on the visible output. A proper fix is
  not a UMD present-path hack; it likely requires a real display/VidPN owner for
  the visible output (or equivalent OS-supported path that makes DXGI's output
  mode-list resolve on the render/display adapter pair). If reviving KMD display
  ownership, use the archived viogpu3d/VidPN research and treat it as a full
  display-miniport implementation, not as stubbed VidPN callbacks.

**32nd session (2026-07-07) — Fire Strike/FaceWorks missing surfaces: first
real D3D11 correctness bug fixed, owner visual validation pending.**

- Owner used Windows Settings -> Display -> Detect multiple displays; there was
  no visible display change, but Fire Strike moved past the legacy
  `IDXGIOutput::GetDisplayModeList` launch blocker and now reaches fullscreen
  rendering. Current symptom: many missing surfaces. Do not spend time on
  `DxgkDdiPresent`: this is a render-only adapter path, and the desktop already
  proves the presentation/capture leg is alive.
- FaceWorks is the faster repro. Its initial dxbc-spv assertion in
  `shd_instruction.cpp` came from D3D11 DDI tessellation patch-constant sysval
  names; `umd/bridge/dxvk_bridge.cpp` now remaps the DDI tess-factor sysvals to
  DXBC `SV_TessFactor` / `SV_InsideTessFactor` signatures. The sample then
  opened a black window in owner-visible testing. A scheduled-task run is not a
  valid visual proxy yet: it exits during DXUT validation before any present.
- Concrete bug found in `umd/src/forward.rs`: DDI Texture2D views were always
  translated to non-MS D3D11 view dimensions. For multisampled Texture2D
  resources this is wrong: RTV/DSV/SRV must use
  `TEXTURE2DMS`/`TEXTURE2DMSARRAY`, not `TEXTURE2D`/`TEXTURE2DARRAY`. This is a
  real correctness hole for Fire Strike-class MSAA workloads and can make view
  creation fail or bind the wrong resource interpretation.
- Fix deployed in UMD `helios_umd_ac10566f81de7294.dll` (SHA256
  `AC10566F81DE72944B472131DF1AE8CBAD719938DCC804C9A936FC14F6643B19`):
  `rtv_desc`, `dsv_desc`, and `srv_desc` now query the underlying
  `ID3D11Texture2D::GetDesc().SampleDesc.Count` and select the MSAA view
  dimensions/unions when `Count > 1`. Adapter hotplug only; no guest reboot.
  Active registry and live DWM/explorer modules point at the new ProgramData
  UMD, Helios device is `CM_PROB_NONE`.
- Evidence: new `tools/d3d11_msaa_view_probe.cpp` passes on Helios FL11_0. It
  creates 4x `R8G8B8A8_UNORM` RT/SRV and `D24_UNORM_S8_UINT` depth resources,
  creates explicit 2DMS RTV/SRV/DSV views, clears, resolves, stages, maps, and
  reads pixel `64,127,191,255`. UMD log for pid 10120 shows
  `MSAA q fmt=28 c=4 -> 1`, successful `create_rtv`, `create_srv`, and
  `create_dsv` on `dim=3` MSAA resources. **NEXT:** owner reruns FaceWorks and
  Fire Strike against this UMD; if surfaces are still missing, inspect fresh UMD
  logs for failed Create*View, ResolveSubresource, Copy/Discard/ClearView, and
  noop-DDI counter movement.

**33rd session (2026-07-07) — FaceWorks/Fire Strike "missing surfaces" reframed:
render is FINE, the problem is windowed compositing. Two earlier theories
FALSIFIED.**

- **FaceWorks is not a render/coherence bug.** `HELIOS_PRESENT_READBACK` shows the
  present source non-black at 1264×681 (multi-pass scene: 1060 draws to the
  backbuffer, 937 to a 184×161 SSS RT, DrawIndexed to 1024×1024).
  `tools/d3d11_shared_draw_probe.cpp` proves float + SINT-indexed+CB + textured/
  blend draws propagate cross-device via OpenSharedResource1. Owner: **Fire Strike
  renders fine fullscreen** — the render pipeline is healthy.
- **FALSIFIED — "two-memory split / KMD zeroes the adopted resid."** The UMD's
  `allocate_wddm_resource` "private mutated" log (`res_id 22301→0, blob→0x3b4000`)
  is the UMD reading the KMD's NEW `HeliosWddmOpenIdentity` writeback with the OLD
  `HeliosWddmAllocPrivate` layout — offset 0 is `venus_alloc_size` (0x3b4000 =
  3883008), and the fields it prints as res_id/kind are `reserved` words. The KMD
  adopt path (`create_allocation.rs adopt_blob_for_allocation`) preserves resid
  22301 and mints no redundant blob; the allocation IS backed by the DXVK venus
  image. Do not re-chase this.
- **FALSIFIED — alpha.** Owner confirmed `HELIOS_PRESENT_FORCE_OPAQUE` changes
  nothing; DWM composites the flip swapchain opaque.
- **FALSIFIED — the IDD.** Both the live D3D11 fallback (`SwapChainNewFrameD3D11`)
  and the D3D12 path capture the same single composed IddCx surface; the D3D12
  path is dead only because our UMD implements no D3D12. The IDD faithfully
  forwards whatever DWM composited (`CopyFromScreen`/paintcap show the same black
  client area).
- **The live root cause is the render-adapter / DXGI-output topology (→ priority
  #1 above).** DXGI enumerates TWO identically-named "Helios vGPU Render Adapter"
  entries (LUIDs …fba8, …7896) that both resolve to the same physical WDDM adapter
  (a stale-LUID residue from repeated device restarts) plus 2× WARP; every
  adapter's `EnumOutputs` returns `0x887a0022` intermittently (one run showed
  `outputs=1` on …7896). CCD `QueryDisplayConfig(ONLY_ACTIVE_PATHS)` pins the live
  Looking Glass output to …fba8 (`\\.\DISPLAY2`), while `QDC_ALL_PATHS` fails
  `ERROR_GEN_FAILURE` on a broken second path (ghost QEMU/DEFAULT monitors). Apps
  that need an `IDXGIOutput` get `NOT_CURRENTLY_AVAILABLE`, fabricate a mode, and
  drop the window off-screen; and DWM never imports the app's flip backbuffer.
  Probes: `tools/{dxgi_output_modes,ccd_adapter}_probe.cpp` (session 1). Memory:
  `phantom-adapter-luid-enumoutputs-33rd`, `faceworks-black-d3dflip-twomemory-split-33rd`.
- KMD 22.22.61: `create_allocation` now writes the `HeliosWddmOpenIdentity`
  trailer for adopted allocations too (live resid for cross-process openers).

Current state (surveyed 2026-07-05):

- UMD (`umd/`, Rust d3d10umddi frontend → cxx bridge → DXVK C++ engine):
  advertises `D3D11_1_DDI_SUPPORTED` (11.15.0) + `D3D11_0` (11.10.2), fills
  `D3D11_1DDI_DEVICEFUNCS`; `forward.rs` implements ~220 DDI functions;
  unfilled slots route to counted noop handlers (`ddi_noop_device/dxgi` —
  loud, not silent). Feature level 11_0 reported to the runtime.
- dxvk-helios: ~58 files / +3.8k lines diverged from upstream DXVK (venus
  import model, GDI staging, alias-image detile, persistent refresh,
  undersized-import refusal).
- Known DDI gaps (from bring-up sessions): ClearView logging blind (log
  budget), partial Discard handling fixed for the common case, Rotate
  implemented minimally; keyed mutex path exercised only by probes.

Plan:
1. Enumerate the noop-slot hit counters after a real workload day — every
   nonzero noop is a conformance gap with a caller.
2. Run dxvk-tests / d3d11-triangle / d3d9-on-11 samples inventory
   (`dx-samples-research-only/`), then 3DMark (installed on the VM).
3. DXGI format coverage audit (the format round-trip carrier landed at
   `bfb5121`; verify beyond BGRA8).
4. Map remaining 11.1 features (deferred contexts? threading modes? UAVs at
   FL11_0) against DXVK capabilities — most exist in the engine; the work is
   the DDI plumbing.

## Tooling (keep alive; this stage depends on it)

- **What is on the SCREEN, sampled at ~30/s** — `tools/vnc_frame_probe.py` +
  `tools/vnc_scanout_correlate.py` (added 2026-07-29 for defect 0ab; needs
  numpy + pillow, host-side only, a venv is fine).
  The probe is an RFB client against QEMU's VNC server. It stamps every
  framebuffer update with `time.time()` — the SAME CLOCK as the
  `virtio_gpu_cmd_*` lines QEMU's `log` trace backend writes to
  `/tmp/helios-qemu-stderr.log` — so a displayed frame can be attributed to a
  specific `res_flush`. Enable the events over QMP first:
  `python3 qmp trace-event-set-state virtio_gpu_cmd_set_scanout_blob /
  _res_flush / _res_unref` on `/tmp/helios-tpm/mon.sock`.
  Its **completeness oracle** is what makes it decisive: `--hud x0,y0,x1,y1`
  names a rectangle that is bright in every FINISHED application frame
  (3DMark's fps bar by default), which separates "the app rendered a dark
  scene" from "we displayed a frame the app had not finished". Whole-frame
  brightness cannot do that and led two sessions astray.
  ⚠ `screendump` is not an alternative under `sdl,gl=on` OR `egl-vnc`: the
  console's scanout kind is DMABUF, so QMP answers `"no surface"`.
  ⚠ Use `--exclusive`; QEMU refuses a SHARED client while an exclusive viewer
  (most viewers) is connected, and drops it silently after ClientInit.
- **Registry knobs** (service key, active KMD reads) — this list is now the
  complete set and is checked against `kmd_render/src/diag.rs`'s `pub mod knobs`:
  `DiagLevel`, `AllocCached`, `DmaGpuFence`, `BindFlushMode`, `DispatchBind`,
  `PresentProbe`, `DisplayHalf`, `DirectFlipCaps`, `CrossAdaptCaps`,
  `BarSegFlags`, `BarSegBaseMB`, `BarSegMode`, `VidMmVramMB`, `FlipCapsX`,
  `FlipQueueN`, `PresentWmk`.
  It used to list `ScanoutDiag`, which the very next bullet says was RETIRED in
  T6/R901, and to omit six knobs that do exist. Do not add a knob here without
  adding it there, or the reverse.
  **`PresentWmk` (default 1 since 22.22.244.0)** gates a WDDM submission that
  carries a live present stream boundary on that boundary alone instead of on
  the whole `next_wire_fence` backlog; `0` restores the historical superset for
  a same-boot A/B. Advertised value mirrored in `PwExact`; the FIFO-head block
  reason in `WfBWire`/`WfBStrm`/`WfBBlt`. **`FlipQueueN` (default 1)** sets
  `DXGK_DRIVERCAPS::MaxQueuedFlipOnVSync` (mirrored in `FlipQueV`); depth 4 was
  MEASURED INERT on 2026-08-04 with and without `FlipCapsX=3`, so it exists as a
  bisect handle only. Both are read at AddAdapter/transport init, so
  `pnputil /restart-device` applies them with no reboot.
  `DisplayHalf=1` enables the render+display adapter shape. `AllocCached=0`
  is the CpuVisible cached-allocation kill switch. `DirectFlipCaps` and `CrossAdaptCaps` are
  explicit cap-advertisement probes; leave off unless bisecting.
  `BarSegFlags`/`BarSegBaseMB` bisect BAR descriptor flags/base. `DiagLevel`
  enables the generic S-ring registry breadcrumbs.
  **`BarSegMode` now has exactly TWO legal values** (T4b/R904, KMD 22.22.187.0):
  `10` (default, absent = production: aperture id 1 + BAR id 2) and `0` (the
  recovery baseline: aperture id 1 + paging-RAM cpu-host id 2, no BAR). The
  historic Code-43 bisect arms `1`, `2`, `5` and `11` are DELETED, along with the
  `probe_only` BAR segment and its 16 MiB contiguous RAM block. Any other value
  is coerced to `10` and recorded in the new `BarMCo` counter carrying the stale
  number — so a VM left set from an old bisect now binds and says so instead of
  reporting a segment no allocation may use. Nothing reports segment id 3 any more.
- **ScanoutDiag — RETIRED in T6/R901 (KMD 22.22.188.0).** The knob, its 16 modes
  and `ddi/scanout_diag.rs` are GONE from the driver; any `ScanoutDiag` value or
  `Sdg*`/`S2d*` name still in the service key is a stale leftover. What it bought
  and why it went: the lab published its colour-bar blobs through the PRODUCTION
  publish word, so at the type level a KMD-owned fill image was indistinguishable
  from the Windows-designated primary, and a leftover `ScanoutDiag >= 4` selected
  a 5-extension `VkDevice` (the 38th-session global-modifier-enable regression
  class) on the one device every render/scanout/GDI path uses. Neither is
  representable now. **`Sdg*` names that SURVIVE, written by the production
  LINEAR fallback:** `SdgLStg SdgLReq SdgLBit SdgLTyc SdgLImg SdgLMem SdgLPch
  SdgLOff` (zeroed each StartDevice by `zero_linear_scanout_breadcrumbs`), plus
  `SdgMt SdgMf SdgBFl` and `SdgDevR SdgDevX` (ext tier; numbering unchanged, 1 =
  export trio, 2 = none).
- **Scanout counters** (service key fixed names): `Sc*` =
  `SetVidPnSourceAddress` scanout, `CSc*` = create-time scanout bind attempt,
  `PSc*` = Present/HWQ diagnostic-only scanout candidate, `Sdg*` = diagnostic
  scanout allocator/bind path, `Rf*` = periodic active-scanout refresh. Values
  persist across boots; trust movement plus same-boot QEMU traces.
- **SAMPLED counters, 22.22.180.0+** (R316): the `PB*` IDENTITY values written by
  `DxgkDdiPresent` — `PBcall PBflag PBcnt PBalst PBDma PBPatch PBpdsz PBkpsz`,
  the `PBs*`/`PBd*` surface identity sets, `PBstrk`/`PBdtrk`, and the flip arm's
  `PBsrc PBsw PBsh PBsDir PBIdOk PBFlip=1` — refresh on the 1st present and
  every 600th thereafter at `DiagLevel=0`, NOT per frame. **A `PB*` identity
  value can therefore be up to ~10 s stale; do not read one as live.** Set
  `DiagLevel=1` (+ `pnputil /restart-device`) to restore the per-call cadence.
  `PBRet=STATUS_SUCCESS` follows the same first/every-600th cadence beginning
  with 22.22.240.0; every non-success `PBRet` remains immediate. UNTHROTTLED,
  always current: `PBCpy` (all arms), `PBFnc`, `PBSyWt`, `PBSyCp`, and
  `PBFlip`'s `0xE1`/`0xE2` failure arms.
- **RETIRED 22.22.180.0** (R903/x-dup-dead-20 — do not look for these; they are
  gone from the driver, and any value still in the service key is a stale
  leftover): the `GdiAccelMode` knob and the whole `Gd*` counter family —
  `GdiM`, `GdiE`, `GdiS`, `GdFa`, `GdFg`, `GdFs`, `GdFb`, `GdFm`, `GdFi`,
  `GdFr`, `GdTc`, `GdDs`, `GdCn`, `GdCr`, `GdCc`, `GdCg`, `GdBn`, `GdBr`,
  `GdBg`, `GdXn`, `GdXz`, `GdXr`. The KMD no longer advertises
  `SupportKernelModeCommandBuffer` in any configuration and no longer contains a
  GDI raster executor; GDI renders through win32k's CPU redirection path.
- **Counters** (service key): Ch* (CpuHostAperture),
  Pg* (paging engine; `PgEv` nonzero means unresolved virtual paging transfer),
  AE* (8-slot allocation create/open ring: resid, dimensions, ctx/open marker)
  — all failure counters must stay 0; S-ring breadcrumbs persist across boots
  and high indices go stale after short boots.
- **Direct-primary producer gate — DELETED, do not reintroduce.** `PresentGateUs`
  and `PresentOrder` were removed on 2026-07-29 by owner directive and this
  inventory entry described them as live for a week afterwards.
  `umd/src/knobs.rs` carries the reasoning: a producer-side CPU stall hides an
  ordering defect instead of fixing it, it costs Fire Strike GT1 158 -> 136 fps
  when it holds, and it publishes the present anyway when it expires. Ordering
  belongs on the GPU timeline (`ScanoutAcquire` + a consumer-side wait), never
  on a blocked CPU thread.
- **UMD `DDI refusals:` counters, T6/R911** — nine names on ONE bounded log
  line, read by `tools/umd-gate-surface.ps1`: `srv_raw_hazard`,
  `resource_raw_hazard`, `text_filter_size_ignored`,
  `staging_busy_assumed_free`, `discard_partial`, `clear_view_unsupported`,
  `gs_so_declaration_dropped`, `tess_sig_fallback`,
  `unhandled_resource_dimension`. Emitted at `DestroyDevice` and on each
  counter's FIRST hit — never on a per-present path (that cost is what T2
  measured and reduced). All nine should read 0 on a healthy DWM session;
  **`gs_so_declaration_dropped` and `tess_sig_fallback` are expected to MOVE
  under 3DMark** and each names a real WS3 conformance gap. The UMD still has
  no registry counter surface, so the log line is the readout — check the line
  exists, not just the `fetch_add`.
- **RETIRED in T6** — do not look for these; they are gone from the driver:
  the `ScanoutDiag` knob and the whole diagnostic `Sdg*`/`S2d*` lab (R901; the
  production `SdgL*` LINEAR ladder plus `SdgMt SdgMf SdgBFl SdgDevR SdgDevX`
  SURVIVE), `ScForceReject`/`ScFrc` (owner-approved), `RbRid`/`RbFail` (R902,
  replaced by `RfUnb`), and on the UMD side the `PresentSyncPublish` and
  `VehicleKernelFlipWait` knobs with the whole kwait subsystem (R912a).
  `helios_umd_get_present_result` REMAINS EXPORTED, returning -1 — the mesa ICD
  resolves it by name and fails the dcomp vehicle with `E_NOINTERFACE` if it is
  absent. Two verbs now have no in-tree consumer and are kept as read-only ABI:
  `HELIOS_ESCAPE_QUERY_SCANOUT` / `helios_venus_query_scanout` (R910), and the
  UMD-side `HeliosPresentRefreshCmd` sender (R910 — the KMD still issues its own
  'HERF' marker in `display.rs`, so the refresh-marker ordering is intact).
- **Launcher/display path**: `tools/launch-helios-gtk.sh` supports
  `HELIOS_DISPLAY=egl-vnc` for `-display egl-headless` + VNC, intended as the
  reliable display-output inspection path, and `HELIOS_DISPLAY=sdl` is visually
  verified on native Wayland. It uses the `qemu-helios` submodule build, whose
  egl-headless/GTK/SDL OpenGL backends share exact OPTIMAL Vulkan readback when
  EGL cannot import a modifier-less native image. Interactive modes leave EGL
  vendor selection to the compositor while NVIDIA remains selected for
  Venus/readback Vulkan; globally forcing NVIDIA EGL breaks Wayland context
  creation on the development host. GTK is still blocked by its later GDK
  `eglMakeCurrent` failure.
  `HELIOS_QEMU_RENDER_GPU=nvidia` is the current owner preference; render-node
  defaults are tracked in the script. The old force-LINEAR LD_PRELOAD shims were
  experiments, not supported display paths. `HELIOS_QEMU_TRACE` can enable
  `virtio_gpu_cmd_set_scanout_blob`, `virtio_gpu_cmd_res_flush`,
  `virtio_gpu_cmd_res_create_blob`, and `virtio_gpu_cmd_ctx_submit`; the trace
  file `/tmp/helios-qemu-stderr.log` is ground truth for scanout shape.
- **ETW**: `logman create trace -p Microsoft-Windows-DxgKrnl 0xFFFFFFFFFFFFFFFF
  0xFF` → tracerpt → grep `AzureTriage` = dxgkrnl failure reasons in plain
  text. Found the segment rule in minutes.
- **AddAdapter iteration**: `pnputil /restart-device` re-runs AddAdapter with
  the loaded image — registry-knob experiments need no reboot.
- **T3 refusal instrument `ScForceReject` — RETIRED in T6 (owner-approved).**
  ⚠ **This leaves the T3 gate line "force each of the seven deferred-programming
  exits and confirm the matching counter moved" with NO mechanism behind it.**
  The `Sc*Err` counters below are still written at their real sites; what is gone
  is the only way to provoke them. Knob names are still capped at **14 chars**
  (`diag::MAX_CONFIG_NAME`) — a build failure, previously a silent always-default.
- **T3 counters** (all must read 0; reset at StartDevice so movement is
  this-boot): `ScBadAlc ScBadExt ScBadLay ScBadFmt ScLinErr ScSetErr ScNoTgt
  ScCpyErr` (one per refusal class), `ScUnav` (HPD dropped a dirty bit),
  `ScRetry`/`ScGaveUp` (R506's bounded retry), `ScStale` (a completion tried to
  clear an interval that was not its own), `ScGateCx` (the DIRQL raise CAS
  exhausted its budget), `HpdStTo` (the HPD prologue fell back to its 500 ms
  bound instead of the real start edge).
- ⚠ **Kernel stack budget on the boot path**: `DxgkDdiStartDevice` +
  `VirtioGpu::init` are the binding chain — **17568 B of the 17936-B known-good
  ceiling as of 22.22.184.0** (8408 + 9160), i.e. 368 B of headroom in a 24 KB
  kernel stack. Overflow at boot = `0xc0000001`/Startup Repair with **no dump and
  no bugcheck event**, and it does NOT reproduce on a live `devcon` restart.
  **Run `tools/kmd-frame-sizes.ps1` on every image** — it reads the frames out of
  the built `.sys` + linker `.map` with `llvm-objdump` (no PDB, no debugger),
  handles sub-page frames, sums the declared call CHAINS rather than every symbol
  measured, and **exits 1 over the ceiling**. `-Symbols`/`-Chains` extend it.
- **Counter snapshots**: `tools/kmd-counter-snapshot.ps1 -Label <name>` dumps the
  whole service key to `Z:\tmp\kmd-counters-<name>.txt` and prints the
  transport/venus/scanout subset. Registry values PERSIST ACROSS BOOTS — take one
  before a workload and one after and diff the files; a single read proves nothing.
- **T4a counters** (22.22.184.0+; every one must read 0 or be absent on a healthy
  boot): `VnEncOvf` (venus command-stream overflow, absent), `VnRingFt`/`VnRingWd`
  (ring fatal latch / head-wait ms, absent), `VnRingSz` (undersized ring mapping,
  absent), `VnMtDown` (memory-type downgrade, absent), `CpNoDrn` (prepared-copy
  drain skipped because nothing was submitted), `PBTdErr` (partial Present-BLT
  teardown), `CtNotOurs` (sync token named another entry), `WtTbl`
  (`FENCE_WAIT_TABLE_FULL`, split out of `WtOut`), `AbnDrop` (fences discarded by
  a TDR/preempt/reset epoch), `ChSzMm`/`ChSzDl`/`ChSzPv` (aperture size-provenance
  cross-checks), `MapDup` (duplicate blob map refused at commit time),
  `PciCapOob` (PCI capability tail outside config space), `WnRcf` (window-reserve
  reconfiguration refused). CollectDbgInfo is **version 6 / `[u32; 38]`**, with
  `FENCE_WAIT_TABLE_FULL` at index 37; word 25 is still `FENCE_WAIT_TIMEOUTS`.
- **Guest probes** (schtasks, session 1; SSH lands in session 0):
  `helios_paintcap` (screenshot → `Z:\tmp\screen_copy.png`), `helios_repaint`,
  `helios_flasher`, `helios_dstate`, `helios_enum_windows`, `helios_regedit`.
  `FindWindow('Progman')` is broken on this box — EnumWindows only.
- **Handle-leak instruments** (`handle.exe` is NOT installed on this box and
  these replace it; both run from SSH, no scheduled task needed):
  `tools/helios-handle-types.ps1` answers *what* — per-type counts either side
  of a run of device cycles, each new handle's type / granted access / kernel
  object address / name, the handles that CLOSED during the run, the
  transient-module set around ONE device cycle, and TlsAlloc/FlsAlloc
  high-water. `-Pin <module-prefix|all>` holds modules loaded, which attributes
  a leak to a module with no hooking at all: the per-device rate drops by
  exactly that module's own never-released statics.
  `tools/helios-handle-origins.ps1` answers *where* — IAT-hooks the
  handle-minting kernel32 entry points (matching slots by resolved address, so
  the kernel32/KernelBase/api-ms-win-core aliasing needs no spelling list) and
  prints a stack per handle that one device leaves behind. **Two traps, each
  cost a run:** the provider modules must be excluded or `kernel32!CreateFileW`
  recurses through its own IAT into the hook (`0xC00000FD`, no output), and the
  **ANSI** spellings are not redundant — the ICD is mingw-built, so its
  `CreateSemaphore` IS `CreateSemaphoreA`. ICD frames are DWARF, which dbghelp
  cannot read: resolve `module+0xRVA` with
  `x86_64-w64-mingw32-addr2line -f -C -e <dll> $((ImageBase + RVA))` on the
  Linux side (get ImageBase from `objdump -p`).
- **User-mode stack dumps**: `tools/take-minidump.ps1 -ProcessId <pid> -Path <dmp>`
  (P/Invoke MiniDumpWriteDump; the `rundll32 comsvcs.dll,MiniDump` trick writes
  TRUNCATED dumps on this box — do not use). Analyze on Linux:
  `~/.cargo/bin/minidump-stackwalk --symbols-path <breakpad-syms> <dmp>`;
  make syms with `~/.cargo/bin/dump_syms <pdb>` (dir layout
  `syms/<name>.pdb/<GUID+age>/<name>.sym`; fix the MODULE line name if the pdb
  was renamed). The deployed UMD build's PDB must GUID-match the dump's module
  (check with `llvm-pdbutil dump --summary`).
- **KMD build/deploy**: `win_build_kmd` (bumps the three version sites with a
  coherence check, then cargo-make package build) → `win_install_kmd`
  (install script + recommended, toggleable graceful guest reboot — the only
  reliable activation path). Manual fallback: `win_cargo` +
  `tools/install-helios-kmd.ps1` (ExecutionPolicy Bypass,
  `-AllowRebootRequired`); version bump = the single `HELIOS_KMD_VERSION` line in
  `kmd_render/driver-version.env` (build.rs renders the FILEVERSION numerics and
  the version strings from it; Cargo.make stampinf reads it via `env_files`);
  backups under
  `C:\ProgramData\HeliosDeployBackups`. New tools appear after the win MCP
  server restarts (new session).
- **dxvk staged-content probes** (`dxvk.heliosStagedProbes`, default OFF since
  `bdbbc2ea` — they were the ~1.5 s stall): full-surface raw+post-copy readback
  characterization at fixed refresh ticks for black-surface triage. Re-enable
  per process via `DXVK_CONFIG "dxvk.heliosStagedProbes = True"` (no rebuild).
- **Venus pipeline object trace** (`VN_HELIOS_PIPELINE_TRACE` env, per-process,
  default off): (ring, primary_tail seqno, object id) lines in the ICD diag log
  for pipeline-layout create/destroy, the vn_get_target_ring wait_all barrier,
  and graphics-pipeline creates — the defect-0b recurrence kit. The barrier
  skip/abandon lines (`BARRIER SKIPPED/ABANDONED in wait_all`) are ALWAYS on.
- **ICD sem-deadline strike log** (`helios_icd_diag.log`, always on): each strike
  line carries `sem=` (venus object id), `reason=` (vn_relax reason),
  `sig_queue=/family=/ring=` + `sig_value=/sig_age_ms=` (the most recent submitted
  signal op for that semaphore, recorded at submission prepare) and `pending_ms=`
  (how long a signal had been pending with zero movement — the quantity the
  deadline gates on). `sig_age_ms` near 0 on a strike = wait-before-signal false
  positive (should no longer happen post-f7a816f182f); large `pending_ms` = a
  genuinely stuck host channel.
- **Queue-submit phase timing**: `HELIOS_QUEUE_PERF=1` + `HELIOS_PERF_FILE`
  machine env (live in dwm since the 2026-07-06 reboot) — one aggregate line
  per 300 vkQueueSubmit2 calls (tls/wsi-flush/cache-flush/submit/fence-wait
  phase averages) to `C:\ProgramData\Helios\helios_queue_perf.log`.
- **Ring-fence probe**: `tools/vk_ring_fence_probe.cpp` → schtasks
  `helios_ringprobe` (wrapper `C:\Users\Rupansh\helios-probe\run_ring_probe.cmd`,
  `/rl LIMITED`; `helios_ringprobe_named` runs the NAMED-import mode against the
  dev ICD build via `icd_devbuild.json`). Proves/regression-tests the WS1 #4
  chain: rc=0 + "consumer wait tracked GPU completion". Build on the VM with
  the WinLibs g++ (`g++ -O2 -o ... Z:\tools\vk_ring_fence_probe.cpp -I <VulkanSDK>\Include
  C:\Windows\System32\vulkan-1.dll`) — no clang-cl on the box. **GOTCHA (cost a diagnosis detour): the Vulkan loader
  silently ignores `VK_DRIVER_FILES`/`VK_ICD_FILENAMES` in ELEVATED processes** —
  win_exec/SSH shells are High-IL (and `runas /trustlevel:0x20000` still reads as
  elevated), so an "env-override" probe actually tests the REGISTRY ICD. Run ICD
  A/B probes through a `/rl LIMITED` scheduled task.
- **QEMU fence tracing without restart**: QMP on `/tmp/helios-tpm/mon.sock` →
  `trace-event-set-state` for `virtio_gpu_fence_ctrl`/`virtio_gpu_fence_resp`
  (output → `/tmp/helios-qemu-stderr.log`; ctrl→resp gap per fence id = decode-
  vs GPU-completion retirement; disable after use — it logs 2 lines per fence).
  NOTE: `-d guest_errors` is already on, but virglrenderer's vkr_log/proxy_log
  are INFO-level = SILENT on the release build — absence of host log lines
  proves nothing below WARNING; a real host-side bisect needs a relaunch with
  `VIRGL_LOG_LEVEL=debug`.
