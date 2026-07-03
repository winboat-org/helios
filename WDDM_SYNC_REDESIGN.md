# Helios WDDM Cross-Process GPU Sync Redesign

> **⛔ SUPERSEDED (2026-06-26) — sync is NOT the blocker. Read `FABLE5_HANDOFF.md` instead.**
> M1 (the WDDM 3.2 + GpuMmu raise, §4/§3-PIVOT) is real and kept (KMD v22.22.33.0, Code 0, monitored
> fences accepted, keyed-mutex probe passes). Everything else here (monitored-fence completion signaling,
> host VkSemaphore, `VK_KHR_external_semaphore_win32` emulation, M1–M5 / M2'–M4') is a DEAD END:
> the IddCx swapchain's `0x887A0026` is the swapchain being *MarkAbandoned* by IDD-adapter removal —
> no keyed-mutex acquire ever fires — and the real blocker is the **IDD failing its PnP post-start
> (Code 43) on the render-only-Helios pairing**. Kept only for the M1 record + historical reasoning.

Status: DESIGN (2026-06-25) — SUPERSEDED, see banner above. Author: primary implementor. User-approved
approach = "proper sync redesign" (a real monitored fence shared by NT handle to both processes,
wired to a host VkSemaphore reflecting venus completion).

## 1. Problem

Goal (locked): DWM composites the whole Windows desktop **on the Helios WDDM render
adapter** (venus → host GPU); the Looking Glass IDD reads the composed frames and
displays them. The OS hands the composed frame to the IDD through an **IddCx swapchain**
whose surfaces are shared (producer = OS compositor; consumer = IDD) and synchronized.

**Current hard blocker:** the IDD swapchain processor cannot start. `IddCxSwapChainSetDevice`
fails `0x887A0026` ("keyed mutex abandoned") on the first attempt after a clean reboot, so
no frames flow at all (LG client loops "waiting for the host to restart").

## 2. Root cause (from a full 3-layer code read, 2026-06-25)

The entire synchronization stack is **decorative** — nothing reflects real host venus
completion, and the object model is wrong end-to-end:

### KMD (`kmd_render`)
- **No synchronization-object DDIs registered at all** (no `DxgkDdiCreateSynchronizationObject`,
  no monitored-fence/native-fence callbacks). `kmd_render/src/lib.rs` DriverEntry registration
  has zero sync entries.
- **Null engine:** `ddi/submit_command.rs` signals `DXGK_INTERRUPT_DMA_COMPLETED` for the
  submission fence **immediately at submit time** via `signal_dma_completed`, never waiting for
  the host venus stream to actually retire. The venus render stream is not even forwarded yet
  from `SubmitCommand` (deferred to UMD bring-up).
- **`HELIOS_ESCAPE_WAIT_FENCE` is a no-op stub** (`ddi/escape.rs`): validates shape, returns
  `STATUS_SUCCESS` without waiting. Comment: "the real KEVENT-backed wait arrives with async
  submission in M3.4" — not yet implemented.
- Shared surfaces are aliased purely by **venus resource id** in `HeliosWddmAllocPrivate._pad`
  (`protocol/src/wddm.rs`); there is **no fence handle, no keyed-mutex handle** in the private
  data and no NT-handle translation in OpenAllocation.
- Escape verbs: `SUBMIT_VENUS / CTX_* / ALLOC_BLOB / MAP_BLOB / WAIT_FENCE / PRESENT_BLOB /
  RELEASE_BLOB / ATTACH_RESOURCE` — **no fence export/share verb**.

### ICD (`icd/mesa` venus, `vn_renderer_helios.c`)
- `helios_wddm_sync_create` creates a **legacy `D3DDDI_FENCE`**, not a monitored fence, with
  the explicit comment that dxgkrnl **rejects `D3DDDI_MONITORED_FENCE` for this adapter
  (STATUS_INVALID_PARAMETER)**. Legacy fences have **no CPU VA** (`*out_cpu_va = NULL`).
- `helios_wddm_sync_signal`/`_wait` try the CPU-direct monitored-fence verbs
  (`D3DKMTSignal/WaitForSynchronizationObjectFromCpu`) first — which need a monitored fence —
  then fall back to context-bound `...Object2` verbs that don't provide cross-process CPU ordering.
- **`VkImportSemaphoreResourceInfoMESA.resourceId` is hardcoded 0** at every call site in
  `vn_queue.c` → the host semaphore is never bound to a shared resource → the host GPU never
  serializes DWM's render before the IDD's read.
- The submit path records a **client-side monotonic `fence_id`** and tracks pending values, but
  there is **no host VkSemaphore on the primary ring** reflecting real completion.

### DXVK (`dxvk-helios`)
- `DxvkKeyedMutex` (`dxvk_image.cpp`) acquires/releases via `D3DKMTAcquire/ReleaseKeyedMutex2`
  on `m_kmtLocal`, and only arms its Vulkan timeline-semaphore ordering
  `if (hasVulkanSyncObject())` = `m_fence->kmtLocal() != 0`.
- `DxvkFence::initKmtHandles` (`dxvk_fence.cpp`) **early-returns leaving `m_kmtLocal == 0`** when
  `vkGetSemaphoreWin32HandleKHR` is unavailable or the D3DKMT open fails → GPU ordering silently
  dropped; only the CPU keyed-mutex bookkeeping remains (which does not order the GPU).
- The published shared handle is `kmtLocal()` (per-process) when `heliosKmtOnlySharedResources()`
  is set (`d3d11_texture.cpp:751`) → a consumer that opens it gets a mismatched object.
- No `WAIT_ABANDONED` handling: all KMT failures collapse to `DXGI_ERROR_INVALID_CALL`.

**Net:** because the KMD has no real sync objects and signals completion at submit, and the ICD
falls back to a legacy fence with no CPU VA bound to no host semaphore, there is no point in the
stack where "the producer's GPU work is actually done" is represented. The keyed mutex is the
OS's bridge for that gap, and with the producer-side ordering broken/teardown-prone, the kernel
marks it abandoned → `IddCxSwapChainSetDevice` fails.

## 3. Target architecture

> **★ PIVOT (2026-06-25) — the monitored-fence / host-semaphore machinery below is SUPERSEDED.**
> The original plan assumed we had to reconstruct host-GPU ordering by hand. We don't — venus
> already gives it to us, and we control DXVK + the ICD + the KMD, so emulating
> `VK_KHR_external_semaphore_win32` (export a Vulkan semaphore → D3DKMT sync object) is pointless
> impedance-matching. The clean design (now authoritative — see `WDDM_SYNC_M3_M4_HANDOFF.md`):
>
> - **Keyed mutex (D3DKMT) = the only OS-facing sync object.** It's a real dxgkrnl object IddCx
>   requires; it works post-M1 (WDDM 3.2). Pure CPU mutual-exclusion + handoff.
> - **Host-GPU ordering is already correct, for free.** Confirmed from the code: `submit_venus`
>   (`kmd_render/src/virtio/gpu.rs`) blocks until the work is *host-visible-complete*; a venus
>   *wait* command blocks on the host's real `VkFence` (GPU-accurate — games + `vkQueueWaitIdle`
>   prove it). DXVK's `D3D11DXGIKeyedMutex::ReleaseSync` (`d3d11_resource.cpp`) already does
>   `WaitForResource(SynchronizeAll)` — a real GPU-completion wait — **before** releasing the keyed
>   mutex. So the producer cannot hand off the surface until its GPU render is actually done. No
>   host-to-host VkSemaphore, no monitored fence, no Win32 handle export.
> - **DELETE** the `VK_KHR_external_semaphore_win32` emulation: `DxvkFence::initKmtHandles`
>   (`dxvk_fence.cpp`) and the `DxvkKeyedMutex` Vulkan-semaphore arm (`vkSignalSemaphore` /
>   `vkWaitSemaphores`, gated by `hasVulkanSyncObject()`, `dxvk_image.cpp`). With the arm gone the
>   keyed mutex is pure CPU exclusion + the existing CPU-ordered release.
> - The ICD monitored-fence change (M2) becomes unnecessary for this path — harmless/dormant; can
>   stay or be reverted.
> - **The real remaining problem is NOT sync — it's DELIVERY** (the original "frames but black"):
>   does the IDD's acquired swapchain surface resolve to the same venus resource DWM composes into?
>   That's path-(a) surface unification, addressed in the handoff.
> - **Fallback (only if frames tear):** if the IddCx *producer* (the OS compositor) releases the
>   keyed mutex without going through DXVK's `ReleaseSync` GPU-wait, add a host `VkSemaphore` bound
>   to the shared resource **by venus resource id** (`VkImportSemaphoreResourceInfoMESA.resourceId`,
>   no Win32 handle) — the "use our impl directly" path. Test CPU-ordered first.

The original (superseded) target follows for historical context: a **venus-backed monitored fence**,
shared cross-process by NT handle, whose value advances only when the **host VkSemaphore** (bound to
the shared composition resource) actually signals on the host GPU.

```
DWM (producer, Helios UMD/DXVK)                 IDD (consumer, IddCx host / DXVK)
  render composition into venus resource R   ─┐
  ReleaseSync / signal monitored fence F ─────┼─ host: vkSignalSemaphore(S) after R's
                                              │   render retires on the real GPU
                                              │
  monitored fence F (CPU VA + GPU)            │   AcquireSync / wait monitored fence F
  shared to IDD via NT handle  ───────────────┘   → blocks until F.value >= target,
                                                    which == host S signaled == R is done
```

Layers:

1. **KMD — real monitored-fence sync objects + venus completion.**
   - Advertise the caps so dxgkrnl accepts `D3DDDI_MONITORED_FENCE` (the #1 unknown — see Risks).
   - Register `DxgkDdiCreateSynchronizationObject` (+ Destroy/Open/Signal/Wait as required) for
     monitored fences with a CPU-visible fence value page.
   - Make completion **real**: when a venus submission retires on the host (used-ring fence ack
     correlated to the host VkSemaphore), advance the monitored fence value (and the existing
     `DMA_COMPLETED`), instead of signaling at submit time. Replace the `WAIT_FENCE` stub with a
     real KEVENT/fence-page wait.
   - Allow a fence to be **shared by NT handle** across processes (sync-object share path).

2. **ICD — use monitored fences + bind host semaphore to a resource.**
   - `helios_wddm_sync_create`: create `D3DDDI_MONITORED_FENCE` (now accepted), capture the
     **CPU VA**; signal/wait via the CPU-direct verbs (no context fallback).
   - Set `VkImportSemaphoreResourceInfoMESA.resourceId` to the **shared composition resource id**
     so the host VkSemaphore is bound to R and orders the host GPU. Wire export/import to the
     KMD's NT-handle share verb.

3. **DXVK — keep the GPU-ordering arm alive + publish the correct handle.**
   - Ensure `initKmtHandles` succeeds (or restructure so `hasVulkanSyncObject()` doesn't gate
     incorrectly), so `vkSignalSemaphore`/`vkWaitSemaphores` always run.
   - Publish the **shared/global (NT)** handle to the consumer, not the per-process `kmtLocal()`.
   - Tolerate `WAIT_ABANDONED` defensively (recover rather than fail) during the transition.

## 4. Milestones (each independently testable)

- **M0 (done):** IDD-process logging visible (`C:\ProgramData\Helios\…`); confirmed
  `open_ddi_texture2d` import works; root-caused the swapchain-start failure.
- **M1 — KMD monitored-fence acceptance — ✅ DONE + VERIFIED (2026-06-25).** Raised the adapter
  to WDDM 3.2 + GpuMmu (gated by `RAISE_WDDM_3_2_GPUMMU`, KMD v22.22.33.0). Boot-loop risk did NOT
  hit: adapter binds Code 0, `D3D11CreateDevice`=S_OK (no revision mismatch), and
  `tools/d3dkmt_sync_probe.cpp` confirms `D3DDDI_MONITORED_FENCE` now succeeds (non-NULL CPU VA +
  GPU VA) for private + **shared-NT** forms. shared-KMT monitored fences are rejected (`0xc000000d`)
  — expected, monitored fences are NT-share-only, so M4's cross-process share MUST use NT handles
  (not the KMT-global path DXVK/ICD currently use). Helios now enumerates once in DXGI (was twice)
  — recheck IDD pairing in M4. Original requirement analysis kept below for reference.

- **M1 (original analysis) — Make dxgkrnl accept `D3DDDI_MONITORED_FENCE` from Helios.**
  **CORRECTED entry point (spike, 2026-06-25) — it is a CAP/VERSION bump, not a new DDI:**
  - The modern WDK 26100 `km\dispmprt.h` `DRIVER_INITIALIZATION_DATA` has **NO legacy
    sync-object DDIs at all** — `DxgkDdiCreateSynchronizationObject` /
    `Destroy/Wait/Signal/OpenSynchronizationObject` are GONE. The sync model is now **native
    fences**: `DxgkDdiCreateNativeFence`, `DxgkDdiOpenNativeFence`, `DxgkDdiSignalMonitoredFence`,
    `DxgkDdiUpdateMonitoredValues`, `DxgkDdiQueryCurrentFence`. So there is no Create DDI to add.
  - The real gate is **`query_adapter_info.rs::query_driver_caps` advertises
    `caps.WDDMVersion = DXGKDDI_WDDMv1_3`** (line ~124) and **deliberately withholds the WDDM 2.0+
    GPU virtual-addressing/GpuMmu cap bits** (line ~160) to keep the 1.3 surface self-consistent.
    `D3DDDI_MONITORED_FENCE` requires **WDDM 2.0+** (GPU scheduling + GPU VA). That is exactly why
    the ICD sees `STATUS_INVALID_PARAMETER` and falls back to a legacy fence.
  - Therefore M1 = **bump the advertised adapter to WDDM 2.x with a consistent GpuMmu/GPU-VA +
    scheduling cap surface** (DRIVERCAPS.WDDMVersion + the GpuMmu/VA bits + WDDMDEVICECAPS +
    GPUMMUCAPS already partly present), so dxgkrnl enables the monitored-fence path. GpuMmu itself
    is already implemented at Code 0 (see `step2-gpummu` memory / `INITDMAPOOLS_HANDOVER.md`); the
    missing piece is advertising the matching WDDM 2.x version/caps without tripping the
    AddAdapter consistency check.
  - **RISK (the #1 unknown, now concrete):** this reopens the WDDM-2.0 cap-consistency area that
    historically caused Code-43 at AddAdapter and the `0x10E:0x49` VidMm crash / boot-loops. A bad
    cap surface = adapter won't load = `gpu-gl-out` boot recovery needed. Do this with the kernel
    debugger ready and the boot-recovery path staged; bisect caps as in `GATE2_3_CAPS_BACKING.md`.
  *Test:* with the bumped caps the adapter still binds Code 0, then the ICD's
  `helios_wddm_sync_create(D3DDDI_MONITORED_FENCE)` returns success (no STATUS_INVALID_PARAMETER)
  with a non-NULL CPU VA. (Add a temporary ICD probe.)
  **Lower-risk alternative to weigh first:** the `0x887A0026` keyed-mutex abandonment may be
  fixable under the current WDDM 1.3 surface by correcting the keyed-mutex object-model bugs
  (publish the shared/global handle not per-process `kmtLocal()`; keep the GPU-ordering arm armed)
  — cheaper and non-boot-crash-prone, though it does not deliver the full host-completion ordering
  the redesign targets. Consider as M1' if the WDDM 2.0 bump proves too destabilizing.
- **M2 — real venus completion fence — ⏸ CODE-COMPLETE + DEPLOYED, DORMANT pending M4.** ICD
  `helios_wddm_sync_create` now makes a `D3DDDI_MONITORED_FENCE` (CPU VA captured) and the existing
  `helios_sync_retire_locked`→`helios_wddm_sync_signal` infra advances it on host used-ring ack.
  But it is only reached via the venus semaphore-export path, which DXVK's dropped GPU-ordering arm
  never triggers — so it does not run live until M4 arms it. M1 separately fixed the keyed-mutex
  *kernel object* (the `d3d11_keyed_mutex_probe` passes), which is what actually unblocked
  cross-process acquire. See `WDDM_SYNC_M3_M4_HANDOFF.md`.
- **M3 — host semaphore bound to the shared resource.** `resourceId != 0` import; host
  `vkSignalSemaphore` after R retires. *Test:* host validation shows the semaphore bound; the
  consumer's wait correlates to real render completion.
- **M4 — cross-process NT-handle fence share + DXVK publishing fixes.** *Test:*
  `IddCxSwapChainSetDevice` SUCCEEDS (no `0x887A0026`); swapchain processor starts; WUDFHost loads
  the UMD; IDD acquires frames.
- **M5 — deliver pixels.** With the swapchain running, confirm the IDD reads the composed surface
  non-zero (path (a) unification, already partly validated since `open_ddi_texture2d` works).
  *Test:* `looking-glass-idd` `sampleNonZero > 0`; LG client shows the desktop.

## 5. Risks / unknowns

- **#1: Will dxgkrnl accept a monitored fence from this adapter?** The ICD comment says it
  currently rejects it because "Helios advertises a WDDM 1.x-style scheduler path." Monitored
  fences require the right scheduler/sync caps (and GpuMmu, which we have, decoratively). M1 must
  determine exactly which cap/DDI set unlocks acceptance, or whether a deeper scheduler-cap change
  is needed. This gates everything; spike it first.
- Any panic in a Helios DDI = silent graphics deadlock (no_std loop handler spins under the
  dxgkrnl ERESOURCE). Every new DDI must be panic-free and bounds-checked.
- KMD changes need rebuild + reboot (~3-4 min, fragile VM); minimize churn — batch M1+M2 KMD work.
- Whether the IddCx swapchain keyed mutex is fully governed by our DxvkKeyedMutex (producer side)
  vs handled internally by IddCx/dxgkrnl — M4 will confirm; if internal, M4 focuses on not leaving
  the producer mutex abandoned (clean release + abandoned-recovery).

## 6. Current deployment state (this session)

- UMD + ICD logging redirected to `C:\ProgramData\Helios\` (per-pid); deployed reboot-free via
  DriverStore / ProgramData **rename-aside** (mapped DLLs can't be overwritten in place).
- KMD unchanged this session. Tree remains uncommitted (~3000+ lines); do not commit.
