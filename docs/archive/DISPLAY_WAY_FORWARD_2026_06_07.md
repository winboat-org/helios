# Display Way Forward — after System-class Venus coherency

**Date:** 2026-06-07

> **Update (2026-06-08):** the IDD experiment below was pursued through the vendored Looking Glass IDD. The direct
> IDD-to-Helios/Venus scanout path was retired after black/grey output despite nonzero test-pattern frames. The
> active desktop-output path is now the normal Looking Glass IDD producer with KVMFR/ivshmem transport. The IDD
> capture path drops frames instead of blocking when D3D12 copy queues are saturated, and the IddCx 1.10 HDR/WCG
> path now prefers 10 bpc. See [`LOOKING_GLASS_HELIOS_2026_06_08.md`](LOOKING_GLASS_HELIOS_2026_06_08.md).

The System-class KMDF + Mesa Venus path is now the renderer baseline: `vkcube` renders normally, cached
`HOST_VISIBLE|HOST_COHERENT|HOST_CACHED` memory is handled explicitly, and close-time ICD teardown assertions are
fixed. The remaining visible frame drops while a Vulkan window updates are a display/composition problem, not a
reason to replace the Venus renderer transport.

## Facts that constrain the decision

- QXL/SPICE is a 2D remote display path. It can bottleneck the whole Windows desktop when a frequently-updated
  window is present.
- The current Helios Vulkan ICD is not a WDDM render adapter. DWM and normal Windows desktop composition cannot
  use it as a compositor GPU.
- A WDDM render adapter is not just a kernel miniport. Microsoft's WDDM architecture expects a Direct3D runtime,
  a vendor user-mode display driver (UMD), and a kernel display miniport (KMD) to cooperate through Dxgkrnl,
  VidMm, VidSch, DXGI, and D3DKMT.
- Microsoft's Indirect Display Driver model is a UMDF display model for remote/virtual monitors. IddCx gives the
  driver desktop images in DirectX surfaces and explicitly says an IDD should not use GDI, windowing APIs,
  OpenGL, or Vulkan inside the driver.
- A Display-Only Driver (DOD/KMDOD) is the WDDM model for scanout-only devices. It can own a Windows display
  output without implementing 3D, but it still does not make DWM GPU-compose through Venus.
- QEMU's accelerated virtio-gpu path is the host-side mechanism that matters for Venus/virgl: `virtio-gpu-gl`
  with blob/hostmem/venus. QEMU also has vhost-user-gpu and rutabaga/gfxstream options, but those do not remove
  the need for a Windows guest display driver if the target is the Windows desktop.

Primary references:

- Microsoft WDDM overview: https://learn.microsoft.com/en-us/windows-hardware/drivers/display/windows-vista-display-driver-model-design-guide
- Microsoft WDDM architecture: https://learn.microsoft.com/en-us/windows-hardware/drivers/display/windows-vista-and-later-display-driver-model-architecture
- Microsoft IDD overview: https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview
- Microsoft IDD sample: https://learn.microsoft.com/en-us/samples/microsoft/windows-driver-samples/indirect-display-driver-sample/
- Microsoft KMDOD sample: https://learn.microsoft.com/en-us/samples/microsoft/windows-driver-samples/kernel-mode-display-only-miniport-driver-kmdod-sample/
- QEMU virtio-gpu docs: https://www.qemu.org/docs/master/system/devices/virtio/virtio-gpu.html

## Option assessment

### 1. Full WDDM render adapter

Do not pivot here now.

This is the only path that could make the Windows desktop and normal D3D/DXGI applications see Helios as a real
GPU. It is also the largest project: a WDDM KMD, a native Direct3D UMD, memory residency, scheduling, allocations,
DXGI interop, synchronization, paging, TDR behavior, and enough Direct3D DDI coverage to satisfy DWM and apps.
DXVK/VKD3D do not solve this because they are app-level translators, not WDDM UMDs loaded by the Direct3D runtime.

### 2. IDD virtual display

Good candidate for the next display experiment, but not a zero-copy Venus/display solution by itself.

Pros:

- UMDF, not kernel display miniport bring-up.
- Designed for virtual/remote displays.
- Avoids the DOD VidPN/Code 43 surface that caused the previous reset.
- Can coexist with the current System-class Helios renderer driver instead of replacing it.

Cons:

- IddCx hands the driver desktop frames as DirectX surfaces rendered by the system's render adapter. With no Helios
  WDDM render adapter, that usually means WARP/Microsoft Basic Render for the desktop.
- The IDD still needs an output path. It does not automatically plug into QEMU's native display window like a PCI
  display adapter does. We would need a host receiver or a bridge from the IDD process into the Helios/virtio-gpu
  scanout path.
- The IDD model is not meant to call Vulkan inside the driver. Any Venus use should stay outside the IDD or cross
  a controlled private channel.

Best use: prototype a separate "Helios virtual monitor" that receives desktop frames and transports them over a
simple host channel. Treat it as a display-quality experiment, not as renderer acceleration.

### 3. DOD / virtio-gpu scanout owner

Best fit if the goal is "QEMU/GTK/SPICE shows the Windows desktop through virtio-gpu scanout instead of QXL."

Pros:

- Directly owns the display path QEMU understands: scanout, dirty rect present, resource flush, and eventually
  `SET_SCANOUT_BLOB`.
- Can replace QXL for desktop output.
- Has a strong virtio reference in `VioGpuDod` and a Microsoft KMDOD sample for WDDM display-only mechanics.
- Enables a possible fullscreen/special-present path where Venus image content becomes the scanout blob.

Cons:

- Re-enters the dxgkrnl VidPN/display miniport surface that previously burned time.
- Still not a WDDM render adapter. Windowed desktop composition remains CPU/WARP.
- If built on the same `PCI\VEN_1AF4&DEV_1050` function as the System driver, it collides with the current
  Helios FDO. A cleaner experiment should use a separate virtio-gpu PCI function/device instance for display.

Best use: after an IDD or isolated scanout proof, revive DOD only as a display-only scanout owner, preferably on a
second virtio-gpu device, not as a replacement for the current Venus render driver.

### 4. Keep QXL/SPICE and tune around it

Useful only as a short-term baseline.

This keeps development focused on Venus performance, but it will not make high-frequency windowed presentation
pleasant. It is acceptable for smoke tests and driver installation, not for judging interactive graphics quality.

## Recommendation

Do not move focus to a full WDDM render driver.

Current recommendation after the 2026-06-08 Looking Glass work:

1. Keep System-class Helios as the Vulkan renderer path and measure it with offscreen, WSI, and DXVK/vkd3d
   workloads.
2. Use Looking Glass IDD + KVMFR/ivshmem as the desktop-output path. Treat IDD capture drops separately from
   Venus/KMD stalls when diagnosing visible hitches.
3. Keep the direct IDD-to-Helios scanout bridge disabled unless it is deliberately revived as a new experiment.
4. Only revive DOD if the product goal specifically becomes QEMU-native virtio-gpu scanout through GTK/SPICE. If
   DOD comes back, scope it narrowly: display-only, ideally on a second virtio-gpu PCI function, with Venus kept on
   the existing System-class driver until the scanout path proves itself.
