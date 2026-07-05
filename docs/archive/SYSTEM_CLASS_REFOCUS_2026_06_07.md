# Helios Direction Reset — System KMDF + IOCTL Venus First

**Date:** 2026-06-07

**Decision:** return the primary architecture to the **System-class KMDF function driver + DeviceIoControl + Mesa Venus ICD** path. The WDDM Display-Only Driver (DOD) work is no longer the active path to performance. Keep the DOD/dxgk binding work as reference material only.

## Why

The project goal is high-performance hardware-accelerated Vulkan in a Windows guest through Venus. The code path that serves that goal is:

`Windows app/DXVK/VKD3D -> Mesa venus ICD -> Helios IOCTL transport -> virtio-gpu Venus -> virglrenderer -> host GPU`

The DOD path serves a different goal: replacing the Windows display adapter and owning scanout. It does not make Venus command execution faster, and it does not make DWM GPU-composited. Without a WDDM render adapter plus a native D3D UMD, Windows desktop composition remains WARP/CPU. A DOD can display a desktop and can potentially expose a fullscreen `SET_SCANOUT_BLOB` path, but it is not required for performant Venus rendering.

## Current Evidence

- The old System-class KMDF + IOCTL path reached real Venus execution:
  - `vulkaninfo` enumerated Mesa Venus.
  - `vkCreateDevice` worked.
  - host-visible `vkAllocateMemory`/`vkMapMemory` worked.
  - `vkCmdFillBuffer` + submit + fence + readback produced expected data.
- The current System-class path now exposes cached `HOST_COHERENT` Venus memory correctly on Windows:
  - Mesa tracks cached coherent mapped allocations and flushes/invalidate them around queue submission and wait completion.
  - feedback slots are cache-flushed/invalidated on Windows, so fence-feedback correctness checks are no longer masked.
  - `vkcube` renders normally on the System-class Venus ICD after the coherency and Win32 WSI fixes.
  - teardown handles clients that exit without unmapping/freeing all cached coherent memory; the ICD no longer trips the internal device-list or mutex assertions on close.
- The remaining visible frame drops while `vkcube` runs are a display transport/composition issue, not evidence that Venus command execution is broken. The VM display is still QXL/SPICE and Windows desktop composition is not accelerated by the System-class Vulkan ICD.
- The DOD path consumed large effort in VidPN/visibility/Code 43 debugging without directly improving Venus rendering.
- Recent DOD testing showed virtio 2D transfer/flush can paint the host surface, but dxgkrnl VidPN negotiation remains fragile:
  - allowing all unpinned transforms avoids hidden source but reintroduces `EnumVidPnCofuncModality <-> IsSupportedVidPn` loops and Code 43;
  - stricter post-active rejection avoids Code 43 but causes `SetVidPnSourceVisibility(FALSE)` and a hidden/black source;
  - VioGpuDod-style rotation support appears load-bearing, but a coherent port requires present-side rotation and full current-mode state, making this a display-driver project rather than a Venus-performance project.

## Active Architecture

Active primary path:

1. **KMD:** System-class KMDF function driver for `PCI\VEN_1AF4&DEV_1050`.
2. **User channel:** `GUID_DEVINTERFACE_HELIOS` opened with SetupDi/CreateFile.
3. **Protocol carrier:** vendor IOCTLs:
   - `CTX_CREATE`
   - `CTX_DESTROY`
   - `SUBMIT_VENUS`
   - `ALLOC_BLOB`
   - `MAP_BLOB`
   - `WAIT_FENCE`
4. **ICD:** Mesa Venus Windows backend `vn_renderer_helios` over DeviceIoControl.
5. **Host:** QEMU virtio-gpu-gl / Venus / blob / hostmem with virglrenderer and host Vulkan driver.

## Immediate Engineering Plan

1. Keep the System-class KMDF build/install/test loop as the renderer baseline.
2. Continue benchmarking Venus without making QXL/SPICE display behavior the primary metric:
   - `vulkaninfo`
   - device creation
   - `vkCmdFillBuffer`
   - render-to-image
   - fence latency
   - submit throughput
3. Fix performance in the Venus transport first:
   - async submit path;
   - interrupt/DPC fence completion;
   - no long spin waits under driver locks;
   - persistent host-visible blob mappings;
   - no avoidable CPU copies for command streams or mapped memory.
4. Treat display present separately from renderer correctness:
   - QXL/SPICE can bottleneck the whole desktop while a Vulkan window updates.
   - a future display path must be chosen explicitly: continue with QXL/SPICE, add a separate display device, build an IDD, or revive a DOD only for scanout ownership.

## Presentation Strategy

Windowed Win32 WSI now works well enough for `vkcube`, but it still presents into a normal Windows desktop that is not GPU-composited by Helios. With QXL/SPICE as the display path, high-frequency updates can still drop frames across the desktop even when Venus rendering itself is healthy.

Do not spend more time on DOD until these are true:

- offscreen Venus is fast enough to prove the renderer path;
- a specific presentation target is chosen;
- the expected tradeoff is explicit: windowed desktop integration, fullscreen-only scanout, or separate display device.

The `SET_SCANOUT_BLOB` idea is still useful, but it should be tested as a small, isolated experiment on the System-class driver or a controlled display setup before another driver-model pivot.

## What To Keep

Keep, but do not treat as active:

- `DISPLAY.md`
- `PHASE7_DISPLAY_HANDOVER.md`
- `CODE43_HANDOFF_FOR_CODEX.md`
- `.dod-vidpn-types.md`
- `kmd/src/dxgk.rs` and DOD-scoped dxgk/dispmprt binding knowledge, if present
- DOD VidPN/rotation breadcrumbs and notes

These are reference material for a future display-driver attempt.

## What To Avoid

- Do not revive the WDDM render miniport path. It still requires a native Windows D3D UMD and WDDM scheduling/memory contracts that do not help Venus.
- Do not make DOD bring-up a prerequisite for Venus performance work.
- Do not use `VN_PERF=no_fence_feedback` as a correctness fix; it was observed to improve apparent frame rate while corrupting output.
- Do not assume Ubuntu virtio-gpu performance implies Windows DOD is required. Ubuntu has a mature DRM/KMS/WSI stack; the closest Windows analogue for the current project is System KMDF Venus for rendering plus a separately designed present path.

## Documentation Status

- `ARCH.md`, `OVERVIEW.md`, `KMD.md`, `ICD.md`, `TOOLCHAIN.md`, and `CLAUDE.md` should describe System-class KMDF + IOCTL Venus as canonical.
- `DISPLAY.md`, `PHASE7_DISPLAY_HANDOVER.md`, and `CODE43_HANDOFF_FOR_CODEX.md` are archived DOD/display-pivot records.
- References to project "memory" names from prior agents should be treated as historical breadcrumbs, not active instructions, unless they match this direction reset.
