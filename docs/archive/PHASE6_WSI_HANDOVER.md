# Phase 6 Handover — WSI windowed present (vkcube) + the present-perf BUG

> ⚠️ **ARCHIVED (updated 2026-06-07).** The Mesa `wsi_win32` software present remains a bad renderer-performance
> benchmark: it issues no virtio-gpu scanout flush and hits host-visible/readback latency. The DOD pivot that
> previously superseded this file is also archived. Active work is System-class KMDF + IOCTL Venus performance:
> offscreen submit/fence/blob throughput first, presentation later. Keep this file as the record of the slow
> Win32 WSI behavior and the `VN_PERF=no_fence_feedback` correctness failure.

**Status: vkcube RENDERS correctly on the spice desktop via Helios, but at <1fps. The perf
problem is REAL and needs a PROPER fix — do NOT ship `VN_PERF=no_fence_feedback` (it reaches
~15fps but CORRUPTS the image: past frames overlap/tear → broken sync; the user rejected it
twice).** Read the `wsi-present-plan`, `wsi-bringup-status`, and `fence-feedback-hack` memories,
then this. Everything here is committed at the two WSI commits on `master`
(`venus: Win32 WSI …` in the icd/mesa submodule + `icd: bump mesa (Win32 WSI) …` in the parent).

---

## 0. Prompt for the next agent

Continue Helios. Windowed `vkcube` now renders on the win11 spice desktop through the Helios
venus ICD (Win32 WSI, Mesa `wsi_win32` software present path). **Your job: make present fast
AND correct** (it is currently <1fps; `no_fence_feedback` is fast but glitchy and is NOT
acceptable). Build via the `win` MCP (`win_meson compile -C C:\Users\Rupansh\helios-mesa-mingw`
for the ICD; `win_cargo kmd make --makefile Cargo.make.toml` + devcon rebind for the KMD).
**GUI apps cannot present from the session-0 SSH** — launch them into the interactive console
(session 1) with a scheduled task:
```
schtasks /create /tn HeliosVkcube /tr "C:\Users\Rupansh\run_vkcube.bat" /sc once /st 00:00 /ru rupansh /it /f
schtasks /run /tn HeliosVkcube     # /it + /ru <console user> => runs in their interactive session
```
(`run_vkcube.bat` runs `"C:\VulkanSDK\1.4.350.0\Bin\vkcube.exe" --c 300 --gpu_number 0 > log 2>&1`).
The USER must visually confirm (you can't see the spice display); you can check the log + exit +
whether the process keeps running (vkcube aborts on any present error). Headless API-path test
that runs over SSH: `C:\Users\Rupansh\helios_vk_wsi.exe` (source `icd/win-build/helios_vk_wsi.c`,
build `gcc -O2 -o C:\Users\Rupansh\helios_vk_wsi.exe Z:\icd\win-build\helios_vk_wsi.c
-IZ:\icd\mesa\include -lgdi32 -luser32`) — it does everything except the final StretchBlt, which
needs an interactive desktop (so over SSH it ends at present = -5 = `VK_ERROR_MEMORY_MAP_FAILED`,
which is EXPECTED, not a bug). Repeated mid-venus crashes leak host contexts → `vkCreateInstance`
starts failing -1 → recover with `devcon restart "PCI\VEN_1AF4&DEV_1050"`.

---

## 1. What works (committed)

- `VK_KHR_win32_surface` advertised in venus (`vn_instance.c`; venus never had it). vulkaninfo
  shows it + "Surface type = VK_KHR_win32_surface" on the Intel-ARL venus device.
- Surface → swapchain (B8G8R8A8, 2 images) → acquire → render → submit → present's internal
  submit → WaitForFences → StretchBlt: ALL work. `vn_AcquireNextImage2KHR` sw-path crash fixed
  (it ran a Linux dma-buf/sync_fd export that NULL-derefs on Windows; gated off `wsi_device.sw`).
- Present path = Mesa `wsi_win32` SOFTWARE (the chosen path; windowed zero-copy is impossible on
  virtio-gpu — no overlay planes; see `wsi-present-plan`). Pixels: host-visible swapchain image
  → CPU `memcpy` to a DIB → `StretchBlt` to the HWND (`wsi_common_win32.cpp:812-826`).
- vkcube selects "Virtio-GPU Venus (Intel(R) Graphics (ARL))", runs, renders. CORRECT image
  (with fence feedback on, the default).

---

## 2. THE PERF BUG (needs a PROPER fix — this is the task)

**Symptom:** vkcube present is **<1fps** with the default (fence feedback ON). The image is
correct. With `VN_PERF=no_fence_feedback`: ~15fps (300 frames/20s) BUT **visually corrupt**
(overlapping/torn frames) → unacceptable.

**Measured:** `helios_vk_wsi.c` per-frame: render `vkWaitForFences` ~15ms; the cost is in the
PRESENT.

**Root cause (high confidence) — the SAME host-side propagation lag as the fence-feedback bug
(`fence-feedback-hack` memory), now per-frame in WSI:**
- The WSI sw present (`wsi_common.c:wsi_common_queue_present`) does, per frame: an internal
  EMPTY `vkQueueSubmit2` (wait the app's render sem, signal `swapchain->fences[i]`, 0 cmd
  buffers) then `WaitForFences(swapchain->fences[i])` (`wsi_common.c:2504-2507`), then the CPU
  blit. `swapchain->fences[i]` is created by `wsi->CreateFence` → a venus **feedback** fence
  (we only stripped feedback from venus's INTERNAL idle-wait fence in `vn_QueueWaitIdle`).
- The empty-submit fence-feedback (ffb `vkCmdFillBuffer` into the host-visible feedback slot)
  has the host-GPU-write→guest-visible **propagation lag** (slot lands late; the guest reads it
  through the hostmem PCI-BAR window). So `vn_get_fence_status` polls the slot, never sees it,
  `vn_relax`es to the first warn (~3.5s, FENCE profile warn_order=10) and only then the
  trust-the-roundtrip path (our `vn_queue.c` fix) returns SUCCESS. **~3.5s per present → <1fps.**
- `no_fence_feedback` removes the feedback fence → `WaitForFences` returns fast (sync
  vkGetFenceStatus) → BUT the rendered pixels in the swapchain image have the SAME propagation
  lag and aren't yet visible to the guest CPU blit → it blits stale/partial pixels → **glitch**.
  So the feedback path is accidentally "correct but 3.5s slow" (the lag masks itself); removing
  it is "fast but wrong". **Both are the same underlying lag.**

**So the PROPER fix must make host-GPU writes to host-visible blobs promptly + coherently
visible to the guest** (then feedback works fast AND the blit sees fresh pixels). Candidates,
in rough priority:

1. **HOST-SIDE (most likely the real fix; was DEFERRED earlier).** The win11 VM uses
   `-display egl-headless,rendernode=/dev/dri/renderD129`; the working Ubuntu venus reference
   uses `-display gtk,gl=on` (a continuously-pumped host GL loop). Hypothesis: egl-headless
   doesn't pump the iGPU, so GPU writes to host-visible blobs flush/propagate lazily. **Test:
   change the standalone launcher display to `gtk,gl=on` (or `sdl,gl=on`), restart the VM, rebind,
   re-run vkcube WITH feedback (default).** If the lag vanishes (vkcube fast AND correct), the
   fix is the host display backend — a real, non-hacky fix. NEEDS the user (VM reconfig+restart;
   they declined earlier, but the per-frame WSI impact now justifies it). Also try whether the
   blob `dmabuf_fd` / `udmabuf` path needs `hugepage` off, or a virglrenderer flush flag.
2. **GUEST-SIDE coherency for the swapchain image read.** For the sw present, before the CPU
   blit, ensure the GPU render is visible: the present already does WaitForFences; the problem is
   the MEMORY visibility, not the fence. Investigate whether a guest-side cache invalidate / a
   different MAP_BLOB cache attribute for the swapchain image helps (NOTE: write-combined was
   already tried for the feedback slot and did NOT help — the bytes weren't in the page yet, so
   it's host-side propagation, not guest cache. Re-confirm for the image.).
3. **Avoid the empty-submit-ffb specifically.** Make venus NOT use fence feedback for EMPTY
   submits (0 command buffers) — strip the fence's feedback when the submit has no real work
   (same pattern as the idle-wait fence strip in `vn_QueueWaitIdle`, but generalized; the
   present's internal submit is empty). This fixes the <1fps WaitForFences (→ sync path, fast)
   BUT does NOT fix the image-visibility glitch (the blit still races the pixel propagation) —
   so it must be combined with #1 or #2. On its own it would reproduce the no_fence_feedback
   glitch. Do not ship alone.
4. **Zero-copy present (longer term).** `SET_SCANOUT_BLOB` of the venus-rendered blob, fullscreen
   only (see `wsi-present-plan`) — sidesteps the CPU blit entirely but needs scanout arbitration
   with VioGpuDod. Bigger effort.

**Recommended next step:** do the host-side display-backend test (#1) FIRST with the user — it
likely fixes the root lag for both feedback and WSI in one change, and is the only candidate that
makes BOTH fast and correct without new code. If it works, much of the `fence-feedback-hack`
+ `vn_queue.c` trust-roundtrip workaround may even become unnecessary.

---

## 3. Other known gaps (lower priority)

- **ctx_destroy hangup / leak (the `kek` note).** A crashed/abnormally-exiting app does not send
  CTX_DESTROY, and the KMD's `EvtFileCleanup` unmaps blobs but does NOT CTX_DESTROY either, so
  host venus contexts + resources leak; after enough leaks `vkCreateInstance` fails -1 until a
  `devcon restart`. HARDENING: have the KMD CTX_DESTROY (and free blob resources) on
  `EvtFileCleanup` / handle close. (There may also be a hang in ctx_destroy under some path —
  investigate "ctx_destroy -> process hangup".)
- Async submit (Phase 4e) + the vkQueueWaitIdle fence-feedback fix are committed and working; do
  not regress them. The WSI changes did not regress the `helios_vk_exec` gate
  (`vkQueueWaitIdle => 0`, PASS).

---

## 4. Test/commit discipline

Keep the tree green: KMD `infverif VALID` + devcon rebind; the non-WSI gate is `helios_vk_exec`
`vkQueueWaitIdle => 0` + smoke/dev/vulkaninfo. WSI gate = vkcube on the spice desktop: fast AND
visually correct. Commit ICD (submodule) + bump the `icd/mesa` gitlink as scoped commits. Do not
commit `VN_PERF=no_fence_feedback` as any kind of default.
