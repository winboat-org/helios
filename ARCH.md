# Helios Architecture v2: System-class KMDF + IOCTL + Venus

> **⚠️ SUPERSEDED (2026-07-05) — ARCHIVED REFERENCE ONLY.** This document
> describes the System-class KMDF + `DeviceIoControl` architecture, which was
> abandoned. The active project is a **WDDM 3.2 render-only miniport**
> (`kmd_render/`) binding virtio-gpu via dxgkrnl, a **DXVK-bridged D3D11 UMD**
> (`umd/`), the **Mesa-Venus ICD** (`vulkan_virtio.dll`), and an **IddCx /
> Looking Glass** display. It uses the full dxgkrnl DDI table, GpuMmu, monitored
> fences, CpuHostAperture segments, and venus-over-`D3DKMTEscape`. **CLAUDE.md
> and ROADMAP.md are authoritative for the live architecture and current state.**
> Everything below (and the 2026-06-07 reset banner) is historical.

> **DIRECTION RESET (2026-06-07):** this file is again canonical for the active project direction.
> The WDDM Display-Only Driver pivot is archived as reference only. Read
> [`SYSTEM_CLASS_REFOCUS_2026_06_07.md`](SYSTEM_CLASS_REFOCUS_2026_06_07.md) for the decision record and
> [`DISPLAY.md`](DISPLAY.md) / [`PHASE7_DISPLAY_HANDOVER.md`](PHASE7_DISPLAY_HANDOVER.md) only for historical
> DOD/display findings. The active path is System-class KMDF + DeviceIoControl + Mesa Venus.

**Status:** ARCHIVED / historical (see the superseded banner above). This was once declared canonical for the System-class direction, but the project has since shipped the WDDM render miniport; CLAUDE.md and ROADMAP.md are now the source of truth. The paragraph below records the (since-reversed) reasoning for abandoning the WDDM render miniport — kept for history: a WDDM render adapter would need a native D3D-to-venus UMD and WDDM scheduling/memory contracts that do not help the Venus renderer. The DOD/display-only work is archived, not deleted, because its dxgk bindings and VidPN findings may be useful later.

## 0. The Pivot in One Paragraph

Helios is a **System-class KMDF function driver** for the virtio-gpu PCI device (PCI\VEN_1AF4&DEV_1050), Setup class **System {4d36e97d-e325-11ce-bfc1-08002be10318}** — NOT a display/WDDM miniport. There is **no dxgkrnl, no DxgkInitialize, no DRIVER_INITIALIZATION_DATA DDI table, no GPU-VA/GpuMmu/segment/monitored-fence contract, no stub D3D user-mode driver, and therefore no Code 43 and no AddAdapter capability/version handshake.** User mode reaches the KMD via **DeviceIoControl on a device interface** (`GUID_DEVINTERFACE_HELIOS`, discovered with SetupDiGetClassDevs + CreateFile). The six existing `helios_protocol` ops (CTX_CREATE, CTX_DESTROY, SUBMIT_VENUS, ALLOC_BLOB, MAP_BLOB, WAIT_FENCE) keep their exact wire layout and simply move from the D3DKMTEscape carrier to IOCTL input/output buffers. The Vulkan ICD is a Windows port of Mesa's `venus` driver whose `vn_renderer` backend talks over those IOCTLs; it is enumerated by the Windows Vulkan loader purely through `HKLM\SOFTWARE\Khronos\Vulkan\Drivers` registry JSON (precedent: SwiftShader, Mesa lavapipe — non-WDDM ICDs that enumerate with no display adapter). The entire virtio-gpu transport, the `helios_protocol` wire crate, and the Venus host stack (virglrenderer venus + Intel ANV + egl-headless) are reused unchanged; the guest driver model is invisible to the host.

## 1. Driver Model (System-class KMDF, no WDDM)

- **Class/ClassGUID:** System / {4d36e97d-e325-11ce-bfc1-08002be10318}.
- **Framework:** KMDF (WDF) via `wdk-sys`, driver-type `KMDF`, KMDF 1.33 (10.0.26100 WDK / Win11 24H2). The WDF function table is auto-wired; calls go through `wdk_sys::call_unsafe_wdf_function_binding!`.
- **Role:** PnP function driver (FDO) bound to PCI\VEN_1AF4&DEV_1050.
- **Lifecycle callbacks (replace the entire DxgkDdi table):**
  - `DriverEntry(driver, registry_path)` → build `WDF_DRIVER_CONFIG { EvtDriverDeviceAdd: Some(evt_device_add), .. }`, `WdfDriverCreate(...)`.
  - `evt_device_add(driver, device_init)` → `WDF_PNPPOWER_EVENT_CALLBACKS_INIT` (set PrepareHardware/ReleaseHardware/D0Entry/D0Exit), `WdfDeviceInitSetPnpPowerEventCallbacks`, `WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(AdapterContext)`, `WdfDeviceCreate`, `WdfDeviceCreateDeviceInterface(device, &GUID_DEVINTERFACE_HELIOS, NULL)`, then `WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(WdfIoQueueDispatchParallel)` with `EvtIoDeviceControl = evt_io_device_control` and `WdfIoQueueCreate`.
  - `evt_device_prepare_hardware(device, raw, translated)` → iterate the translated `CM_PARTIAL_RESOURCE_DESCRIPTOR` list (`WdfCmResourceListGetCount`/`GetDescriptor`), map each `CmResourceTypeMemory` BAR with `MmMapIoSpaceEx`; obtain `BUS_INTERFACE_STANDARD` via `WdfFdoQueryForInterface(device, &GUID_BUS_INTERFACE_STANDARD, ...)`; call `VirtioGpu::init(bus_interface)`; store via `AdapterContext::set_virtio`. Also create the WDF interrupt object here (or in DeviceAdd) with `WdfInterruptCreate` (EvtInterruptIsr + EvtInterruptDpc).
  - `evt_device_release_hardware` → `set_virtio(None)`, `MmUnmapIoSpace` each BAR.
- **No user-mode driver registration of any kind** — no UserModeDriverName, no InstalledDisplayDrivers, no OpenAdapter handshake. The KMD exposes only the device interface; the Vulkan ICD is independent (registry JSON).
- **AdapterContext** lives in the WDF device-object context (`WdfObjectGetTypedContext`), not a Box returned as `MiniportDeviceContext`. It keeps `virtio_lock` (KSPIN_LOCK) + `with_virtio`/`set_virtio`; it **drops** the `dxgkrnl: Option<DXGKRNL_INTERFACE>` field and the `pdo`/`dxgkrnl()` accessor (no Dxgkrnl exists). Add a fence table: `fence_id -> KEVENT` (for the async WAIT_FENCE path).

## 2. Device Interface

- `GUID_DEVINTERFACE_HELIOS`: a freshly minted GUID, defined **once** in `helios_protocol` so KMD and ICD share the constant.
- KMD registers it in `evt_device_add` via `WdfDeviceCreateDeviceInterface`; it auto-enables when the device enters D0.
- ICD opens it: `SetupDiGetClassDevs(&GUID, NULL, NULL, DIGCF_DEVICEINTERFACE|DIGCF_PRESENT)` → `SetupDiEnumDeviceInterfaces` → `SetupDiGetDeviceInterfaceDetail` (device path) → `CreateFileW(path, GENERIC_READ|GENERIC_WRITE, FILE_SHARE_READ|FILE_SHARE_WRITE, OPEN_EXISTING, ...)` → `DeviceIoControl`.

## 3. IOCTL Protocol

`CTL_CODE(DeviceType, Function, Method, Access) = (DeviceType<<16)|(Access<<14)|(Function<<2)|Method`. DeviceType = `FILE_DEVICE_UNKNOWN` (0x22), Access = `FILE_READ_DATA|FILE_WRITE_DATA` (0b11 = 3), Function base 0x900 (>=0x800 vendor range). Methods: BUFFERED=0, IN_DIRECT=1, OUT_DIRECT=2, NEITHER=3. The single source of truth for these values is `protocol/src/ioctl.rs` (the constants there are asserted at compile time).

**RequiredAccess = read+write, not FILE_ANY_ACCESS:** every op transfers data and is a privileged GPU operation, so the codes specify `FILE_READ_DATA|FILE_WRITE_DATA` per the WDK "Security Issues for I/O Control Codes" guidance (the I/O manager then refuses the IOCTL on a handle lacking that access; the ICD opens with `GENERIC_READ|GENERIC_WRITE`). This puts `0b11` in the Access bits (14–15), so the codes are `0x0022_E4xx` (not the `0x0022_24xx` an Access=0 would give).

| Op | IOCTL constant | Value | Method | In buffer | Out buffer |
|----|----------------|-------|--------|-----------|-----------|
| CTX_CREATE | IOCTL_HELIOS_CTX_CREATE | 0x0022E400 | BUFFERED | HeliosCtxCreate (capset_id) | HeliosCtxCreate (out_ctx_id) |
| CTX_DESTROY | IOCTL_HELIOS_CTX_DESTROY | 0x0022E404 | BUFFERED | HeliosCtxDestroy | — |
| SUBMIT_VENUS | IOCTL_HELIOS_SUBMIT_VENUS | 0x0022E409 | IN_DIRECT | small header (ctx_id/fence_id/buffer_size) buffered + Venus blob via MDL | — |
| ALLOC_BLOB | IOCTL_HELIOS_ALLOC_BLOB | 0x0022E40C | BUFFERED | HeliosAllocBlob (size/flags/mem/ctx) | HeliosAllocBlob (out_resource_id) |
| MAP_BLOB | IOCTL_HELIOS_MAP_BLOB | 0x0022E412 | OUT_DIRECT | HeliosMapBlob (resource_id) | HeliosMapBlob (out_user_va) |
| WAIT_FENCE | IOCTL_HELIOS_WAIT_FENCE | 0x0022E414 | BUFFERED | HeliosWaitFence (fence_id/timeout_ns) | — |

(Each value = `(0x22<<16) | (3<<14) | (fn<<2) | method`; functions 0x900–0x905; method 0/1/2/3.)

**Method rationale:** small fixed verbs use METHOD_BUFFERED (the I/O manager double-buffers; this is what mvisor's control.c uses). SUBMIT_VENUS's Venus stream can be megabytes, so METHOD_IN_DIRECT carries the variable payload via a locked MDL (`WdfRequestRetrieveInputWdmMdl`) while a small fixed header rides the buffered system buffer. MAP_BLOB returns a **user VA**, not a GPA: the kernel does `MmMapLockedPagesSpecifyCache(blobMdl, UserMode, <cache>)` and writes the resulting user VA into the OUT buffer — the page mapping is the side effect, the IOCTL only carries the 8-byte pointer. **Rename `HeliosEscapeMapBlob.out_gpa` → `out_user_va`** to reflect this.

**Dispatch:** `evt_io_device_control(queue, request, out_len, in_len, code)` switches on `code`, retrieves buffers (`WdfRequestRetrieveInputBuffer`/`OutputBuffer` for buffered; `WdfRequestRetrieveInputWdmMdl` for SUBMIT_VENUS), runs the **same body as today's `ddi/escape.rs` handlers verbatim** (`adapter.with_virtio(|v| v.ctx_create/ctx_destroy/submit_venus/...)`), then `WdfRequestCompleteWithInformation(request, status, bytes_returned)`.

**Header framing:** the 16-byte `HeliosEscapeHeader` (magic/cmd_type/version/size) becomes **redundant** — the IOCTL control code *is* the verb and WDF validates in/out lengths. Keep the header optionally as a cheap version sanity check, or drop it; the byte layout of the op structs is unchanged.

**Trust boundary (preserved):** every guest-supplied size/offset is validated against the WDF-reported in/out length before use; reads use `pod_read_unaligned`. This logic ports 1:1 from `ddi/escape.rs`.

## 4. Transport Reuse (virtio-gpu)

Reused **unchanged**: PCI cap scan (cap types 1/2/4/5), `virtio-drivers` `VirtQueue<WdkHal>`, `WdkHal` (Mm* calls — KMDF-agnostic), `virtio/gpu.rs` Venus submit (ctx_create/ctx_destroy/submit_venus + feature negotiation / virtqueue). One change and one addition:

1. **Config access:** replace `virtio/config.rs` `DxgkConfigAccess` (which uses `dxgkrnl.DxgkCbReadDeviceSpace/WriteDeviceSpace` — gone under KMDF) with `KmdfConfigAccess` wrapping a `BUS_INTERFACE_STANDARD` obtained via `WdfFdoQueryForInterface(device, &GUID_BUS_INTERFACE_STANDARD, ...)`; implement `ConfigurationAccess::read_word`/`write_word` via `busInterface.GetBusData/SetBusData(ctx, PCI_WHICHSPACE_CONFIG=0, buf, off, 4)` (callable up to DISPATCH_LEVEL). `VirtioGpu::init` signature changes from `init(&DXGKRNL_INTERFACE)` to `init(&KmdfConfigAccess)`.
2. **Shared-memory BAR:** the cap scan must **also** record `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` (cap type 8) with `shmid == VIRTIO_GPU_SHM_ID_HOST_VISIBLE`. This is the host-visible window BAR (typically BAR4, prefetchable 64-bit, size = QEMU `hostmem=`). See §6.

**Interrupt:** WDF interrupt object — `WdfInterruptCreate` with `EvtInterruptIsr` (ack via `transport.ack_interrupt()`, request DPC with `WdfInterruptQueueDpcForIsr`) and `EvtInterruptDpc` (pop the used ring under the queue lock, signal the per-fence KEVENT). All `DxgkCbNotifyInterrupt`/`DxgkCbQueueDpc`/monitored-fence callbacks are deleted.

## 5. ICD (Mesa venus port over IOCTL)

The ICD is a Windows port of Mesa's `venus` driver (`-Dvulkan-drivers=virtio`, driverName "venus"). Everything above `vn_renderer` (vn_instance/device/queue/ring, the byte-correct Venus encoder `vn_protocol_driver_*`) is reused unmodified. The port adds **one** new file — a third `vn_renderer` backend, `src/virtio/vulkan/vn_renderer_helios.c`, modeled on `vn_renderer_vtest.c` (the clean non-DRM template; NOT `vn_renderer_virtgpu.c`, which is libdrm/gem). `vn_renderer_create()` selects it on Windows (mirror the existing `#ifdef HAVE_LIBDRM` / vtest fallback structure, adding a Windows arm and building WITHOUT libdrm/the virtgpu backend).

**vn_renderer vtable → IOCTL mapping:**
- `ops.submit(vn_renderer_submit{bos[],batches[]})` → SUBMIT_VENUS (serialize each batch's `cs_data` bytes + ctx_id + fence_id; first cut collapses to one ctx/one ring, which SUBMIT_VENUS already assumes).
- `ops.wait(vn_renderer_wait{timeout,syncs,sync_values})` → WAIT_FENCE (timeout_ns).
- `ops.destroy` → close HANDLE + CTX_DESTROY.
- `shmem_ops.create(size)` (command-ring backing) → ALLOC_BLOB(HOST3D, USE_MAPPABLE, blob_id=0) + MAP_BLOB.
- `bo_ops.create_from_device_memory(size, mem_id, props, external)` → ALLOC_BLOB(HOST3D, USE_MAPPABLE iff HOST_VISIBLE_BIT, blob_id=mem_id) → out_resource_id.
- `bo_ops.map(bo)` → MAP_BLOB returning out_user_va; user-mode reads/writes that VA directly (replaces Linux `mmap(drm_fd, offset)`).
- `bo_ops.flush/invalidate` → no-op (HOST3D blobs treated host-coherent).
- `bo_ops.create_from_dma_buf`, export/import, `sync_ops.create_from_syncobj`/`export_syncobj` → NULL (no dma-buf/external sync on a single-VM IOCTL channel).
- `sync_ops.create/destroy/reset/read/write` → guest `fence_id <-> KEVENT` table in the KMD (fence_id is the sync token).
- `info` (vn_renderer_info): fill capset version fields from GET_CAPSET(VENUS); set `id.has_luid` + `id.luid` (needed for DXVK D3DKMT interop) and `pci.vendor_id`/`device_id` from the virtio-gpu device.

Device open replaces `D3DKMTOpenAdapterFromLuid`+`D3DKMTEscape` with the SetupDi/CreateFile path (§2); store the HANDLE where vtest stored its socket fd, guarded by a mutex. Build: the venus meson currently gates virtgpu behind HAVE_LIBDRM; the Windows port (like mvisor's `mesa-virgl-icd-for-windows.patch`) adds meson logic to build venus on Windows without libdrm and select the IOCTL backend.

## 6. Host-visible blob mapping (resolves the TRANSPORT.md OPEN ITEM)

The blob GPA is **not** in `VirtioGpuRespMapInfo` (that carries only the `map_info` caching byte). QEMU exposes a host-visible memory window as a prefetchable 64-bit PCI BAR (typically BAR4) advertised by a `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` cap with `shmid == VIRTIO_GPU_SHM_ID_HOST_VISIBLE`, size = device `hostmem=` (256M..8G). `VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB` makes the host inject the resource's mapping at an offset inside that BAR. So: the KMD records the SHARED_MEMORY_CFG(HOST_VISIBLE) BAR's guest-physical base during the cap scan; on MAP_BLOB it computes `gpa = host_visible_bar_base + offset`, builds an MDL over those pages, `MmMapLockedPagesSpecifyCache(mdl, UserMode, <WC or cached from map_info>)`, and returns the resulting **user VA** to the ICD. This is the value that previously would have filled WDDM's `query_segments` BaseAddress.

## 7. ICD Loader Registration

The Windows Vulkan loader scans `HKLM\SOFTWARE\Khronos\Vulkan\Drivers` (and WOW6432Node for 32-bit) for DWORD values named with the absolute path to a JSON manifest; data 0 = enabled, 1 = disabled. Required ICD exports (Mesa venus already provides): `vk_icdGetInstanceProcAddr`, `vk_icdNegotiateLoaderICDInterfaceVersion`, `vk_icdGetPhysicalDeviceProcAddr`. JSON: `{"file_format_version":"1.0.0","ICD":{"library_path":"<path to venus DLL>","api_version":"1.3.x"}}`. The KMDF universal INF cannot write absolute HKLM values, so the **ICD installer (or a postinstall script) writes the registry value** — independent of the KMD INF. **Precedent confirmed:** lavapipe (pure CPU, no GPU) and SwiftShader enumerate via exactly this mechanism with no display adapter; the loader does not require GPU/PnP/WDDM association.

## 8. Presentation

Presentation is explicitly separate from Venus command execution. The active renderer path must first be made
fast offscreen (submit/fence/blob throughput, render-to-image, readback checks). Windowed Win32 WSI via
software blit is known to be slow and is not a reliable performance metric for the renderer.

The DOD `SET_SCANOUT_BLOB` path is archived as a possible future display integration strategy, not the current
driver model. If revisited, prove the smallest possible `SET_SCANOUT_BLOB` experiment first and only then decide
whether a display miniport is worth the cost.

## 9. INF (System-class KMDF)

- `[Version]` Class=System, ClassGuid={4d36e97d-e325-11ce-bfc1-08002be10318}, PnpLockdown=1.
- `[DestinationDirs]` DefaultDestDir=13 (driver store; universal INF).
- Model line keeps `PCI\VEN_1AF4&DEV_1050` under the NTamd64.10.0...16299 decoration.
- `[Helios_Install.Services]` AddService = helios_kmd, 0x2, Helios_ServiceInstall; `[Helios_ServiceInstall]` ServiceType=1, StartType=3, ErrorControl=1, ServiceBinary=%13%\helios_kmd.sys. **REMOVE LoadOrderGroup=Video.**
- ADD KMDF directive: `[Helios_Install.NT.Wdf] KmdfService = helios_kmd, helios_wdfsect` / `[helios_wdfsect] KmdfLibraryVersion = $KMDFVERSION$` (resolves to 1.33).
- **DELETE** `[Helios_Install.SoftwareSettings]` / `[Helios_SoftwareSettings_Reg]` (InstalledDisplayDrivers/Version) — display-only.
- **NO CoInstallers section / NO WdfCoInstaller DLL** — inbox KMDF on Win10 1709+ (the same 16299 floor). The legacy CoInstallers32 pattern is forbidden in a universal INF.
- Device interface is registered in code (WdfDeviceCreateDeviceInterface), so the INF needs nothing extra for it.

## 10. Deleted vs Kept

**DELETED:**
- The entire WDDM DDI surface: `kmd/src/ddi/{add_device,start_device,query_adapter_info,create_allocation,build_paging_buffer,submit_command,patch,interrupt}.rs` and `ddi/mod.rs`'s WDDM dispatch.
- `kmd/src/dxgk.rs` (the dispmprt.h/d3dkmddi.h bindgen module) from the active build. Keep any existing file as
  archived/reference material; do not make it a dependency of the System-class build.
- `kmd/build.rs` display bindgen + `cargo:rustc-link-lib=static=displib` (collapse build.rs to `Config::from_env_auto()?.configure_binary_build()?`).
- `kmd/src/diag.rs` (the Code-43 breadcrumb tracer — obsolete with Code 43 gone).
- The entire `umd/` crate (the stub D3D UMD).
- `AdapterContext.dxgkrnl` field + `dxgkrnl()` accessor; `virtio/config.rs` `DxgkConfigAccess`.
- The display-class `.inx` SoftwareSettings/UserModeDriverName/LoadOrderGroup=Video; the Display class/ClassGUID.
- The bindgen build-dependency in kmd/Cargo.toml from the active build; the WDM driver-model.

**KEPT (unchanged or near-unchanged):**
- `protocol/` crate entirely — all six op structs (escape.rs; only `out_gpa`→`out_user_va` rename, IOCTL codes + GUID added, optional header drop) and all virtio-gpu structs/constants (VIRTIO_GPU_CAPSET_VENUS=4, blob mem/flag constants).
- `kmd/src/virtio/{gpu.rs, hal.rs, mod.rs}` — the transport (gpu.rs `init` takes `KmdfConfigAccess`; cap scan extended for SHARED_MEMORY_CFG).
- `kmd/src/error.rs` `DriverError` enum + `into_ntstatus` (model-agnostic).
- `ddi/escape.rs` body — repurposed as the IOCTL handler (logic ports 1:1).
- `host/` crate entirely (host-side, driver-model-agnostic).
- The Venus host stack, CARGO_TARGET_DIR split, win MCP build flow, kernel/Rust discipline.

## 11. Repo-structure deltas

```
kmd/src/
  lib.rs            REWRITE  DriverEntry(WdfDriverCreate) + evt_device_add + IOCTL dispatch (no DDI table)
  adapter.rs        EDIT     WDF device context; drop dxgkrnl field; add fence_id→KEVENT table
  ioctl.rs          NEW      EvtIoDeviceControl dispatch (from ddi/escape.rs)
  pnp.rs            NEW      evt_device_add / prepare_hardware / release_hardware (BAR map, device iface)
  interrupt.rs      NEW      WdfInterruptCreate + EvtInterruptIsr + EvtInterruptDpc (replaces ddi/interrupt.rs)
  device.rs         DELETE/RECAST  (WDDM device/context DDIs gone)
  error.rs          KEEP
  dxgk.rs           DELETE
  diag.rs           DELETE
  ddi/              DELETE   (whole directory; escape.rs body moves to ioctl.rs)
  virtio/
    config.rs       REWRITE  DxgkConfigAccess → KmdfConfigAccess (BUS_INTERFACE_STANDARD)
    gpu.rs          EDIT     init(&KmdfConfigAccess); add alloc_blob/map_blob; cap scan SHARED_MEMORY_CFG; pop_used/ack_interrupt for DPC
    hal.rs          KEEP
    mod.rs          EDIT     re-exports
  helios_kmd.inx    REWRITE  System class + [.Wdf] directive (one-time INF exception)
  build.rs          REWRITE  collapse to configure_binary_build()
  Cargo.toml        EDIT     driver-type KMDF + kmdf-version; drop bindgen build-dep
protocol/src/
  escape.rs         EDIT     rename out_gpa→out_user_va; add IOCTL_HELIOS_* codes + GUID_DEVINTERFACE_HELIOS; header optional
icd/                (Mesa venus port — supersedes the hand-written icd/ tree)
umd/                DELETE
```

## 12. Open implementation questions (resolve during the code pivot)

- **KMDF/wdk-sys completeness:** confirm windows-drivers-rs stamps `$KMDFVERSION$`=1.33 and that `wdk-sys` exposes `WdfDeviceCreateDeviceInterface`, `WdfFdoQueryForInterface`, `WdfInterruptCreate`, `WdfRequestRetrieveInputWdmMdl`. If any are missing, a small *virtio-scoped* bindgen (NOT the deleted display one) may be needed.
- **PCI config access:** verify `BUS_INTERFACE_STANDARD.GetBusData` reaches the full 4KB extended config space the virtio cap chain needs, and that `virtio-drivers`' `PciTransport` only needs reads we can satisfy (BAR MMIO comes from the CM resource list).
- **MAP_BLOB user-VA lifetime:** `MmMapLockedPagesSpecifyCache(UserMode)` maps into the *calling* process; tear down on handle close (EvtFileClose/EvtCleanup). The ICD must map from the same process it opened the handle in.
- **Host-visible BAR → MDL:** building an MDL over the SHARED_MEMORY_CFG BAR's I/O-space pages may need `MmMapIoSpaceEx` + a manual MDL rather than `MmBuildMdlForNonPagedPool`; confirm whether the RESOURCE_MAP_BLOB offset is guest-chosen or host-returned.
- **SUBMIT_VENUS payload:** METHOD_IN_DIRECT delivers a locked MDL; v2 leans on keeping the proven PASSIVE-level copy into a contiguous `DmaBuffer` (vs zero-copy from a fragmented user MDL) — perf tradeoff to revisit.
- **DXVK LUID interop (Phase 6):** confirm DXVK degrades gracefully when `D3DKMTOpenAdapterFromLuid` finds no WDDM adapter for the venus-reported LUID.
- **`escape.rs` naming:** the module/types are still `HeliosEscape*`; byte layout is unchanged. Decide rename (cleaner, wide churn) vs keep-with-note.

## 13. STATUS (updated 2026-06-07) — System-class KMDF + IOCTL Venus is the active path

Phases 0–5 reached end-to-end Venus rendering. The DOD/display pivot is archived in
`SYSTEM_CLASS_REFOCUS_2026_06_07.md` and should not block renderer work. The active next work is Venus
performance on the System-class path: async submit, fence completion, blob mapping, and offscreen render
throughput before revisiting presentation.

**DONE + committed (`7a5763f`):** Phases 0+1. The System-class KMDF driver builds, packages (infverif VALID, test-signed), force-installs over the inbox `VioGpuDod` (via `devcon update … "PCI\VEN_1AF4&DEV_1050"`), and **loads cleanly: device Code 0 / System class, `GUID_DEVINTERFACE_HELIOS` opens from user mode, and `VirtioGpu::init` round-trips `GET_DISPLAY_INFO`** (transport works). New module layout per §11. Detail: the `pivot-phase01-done` memory.

**✅ RESOLVED (Phase 3 IOCTL dispatch — uncommitted WIP in the working tree):** every `DeviceIoControl` used to pend forever; root cause was a hand-rolled-`_INIT` bug, **not** queue config. `wdf::io_queue_config` builds `WDF_IO_QUEUE_CONFIG` but the Rust replica of `WDF_IO_QUEUE_CONFIG_INIT` omitted `Settings.Parallel.NumberOfPresentedRequests`, so `..Default::default()` left it **0**. The real FORCEINLINE macro sets it to `(ULONG)-1` (unlimited) for a parallel queue; WDF's dispatch gate is `if (DriverIoCount < NumberOfPresentedRequests) present()`, so 0 means the queue accepts requests but presents **zero** to the driver → `EvtIoDeviceControl` never fires. **Fix:** `cfg.Settings.Parallel.NumberOfPresentedRequests = ULONG::MAX;` (kmd/src/wdf.rs). **Verified:** `helios_probe.exe` PASSES end-to-end — `IOCTL_HELIOS_CTX_CREATE` returns `out_ctx_id=1` (a real host venus context), `IOCTL_HELIOS_CTX_DESTROY` succeeds; the `diag.rs` breadcrumb now reaches stage 11 (was frozen at 6). The whole earlier "ruled-out list" (default/non-default queue, power-managed/not, `WdfDeviceConfigureRequestDispatching`, IoType, ExecutionLevel, RequiredAccess) was a dead end because every variant reused the same builder and so kept `NumberOfPresentedRequests=0`. The earlier interrupt-connect `STATUS_DEVICE_POWER_FAILURE` was unrelated (a real separate Phase-4 item). Full record: the `pivot-phase3-ioctl-blocker` memory.

**✅ Phase 4a/4b DONE (committed `ad30321`):** the `SHARED_MEMORY_CFG`/HOST_VISIBLE window cap scan (§6) and `IOCTL_HELIOS_ALLOC_BLOB` are implemented; the KMD blob mechanism + IOCTL plumbing work end-to-end (command reaches the host and is acked/nacked). Also fixed a `0x7F` kernel-stack-overflow (a ~4 KB inline blob-table array in the stack-built `VirtioGpu` → moved to a heap `Vec`). Device loads Code 0, probe opens the interface and `CTX_CREATE` round-trips. **A standalone host-visible blob is rejected by the host** (it must be backed by a venus `vkAllocateMemory`, `blob_id` = a venus mem id), so `ALLOC_BLOB`/`MAP_BLOB` are validated under the ICD, not the probe.

**✅ Phase 4c MAP_BLOB DONE + `blob_id` gap closed (uncommitted on master; built, packaged infverif-VALID, installed, regression-verified on win11 — device Code 0, no bugcheck):** `HeliosEscapeAllocBlob` gained `blob_id: u64` (→48 bytes), threaded to `VirtioGpuResourceCreateBlob.blob_id`. `IOCTL_HELIOS_MAP_BLOB` → `gpu.rs::map_blob_prepare` (bump-allocated host-visible window offset + `RESOURCE_MAP_BLOB` + `RESP_OK_MAP_INFO`) under the virtio spinlock, then `IoAllocateMdl` + manual PFN fill + `MmMapLockedPagesSpecifyCache(UserMode)` at PASSIVE outside the lock → user VA; teardown in `EvtFileCleanup` (new `kmd/src/mapping.rs` `MappingTable` in `AdapterContext`, tagged per-`WDFFILEOBJECT`). A 16-agent adversarial review caught + fixed 3 real kernel bugs: missing **`MDL_IO_SPACE`** on the BAR-page MDL, **device-wide (not per-file) cleanup** unmapping other processes, and the **uncatchable UserMode-map SEH** (mitigated by a 256 MiB cap; C `__try` shim is a hardening TODO). MAP_BLOB IOCTL moved to METHOD_BUFFERED (0x0022E410). Detail: `phase4-blob-plan` memory.

**✅ Phase 5 DONE (2026-06-05, committed — the Mesa venus ICD works end-to-end on real hardware).** `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c` (the IOCTL `vn_renderer` backend) + `vn_renderer.h`/`meson.build` edits are committed to the fork (submodule bumped). Validated against real venus on the Intel ARL iGPU: `vulkaninfo --summary` → `DRIVER_ID_MESA_VENUS`, `driverName venus` (Intel ARL + llvmpipe); `vkCreateInstance`/`vkEnumeratePhysicalDevices`/`vkCreateDevice`; host-visible `vkAllocateMemory`(ALLOC_BLOB blob_id=mem_id)/`vkMapMemory`(MAP_BLOB) with readback; and **real GPU command execution** (`vkCmdFillBuffer`+`vkQueueSubmit`+fence+readback = `0xDEADBEEF`). Test harnesses: `icd/win-build/helios_vk_{smoke,dev,exec,poll}.c`. Two bring-up bugs fixed en route: (1) the virtio-gpu **command opcodes** were miscounted (a missing `RESOURCE_CREATE_3D` shifted SUBMIT_3D/MAP_BLOB/UNMAP_BLOB) — now pinned to the **`virtio-bindings`** crate via `cargo test` (dev-dep; const values + struct size/align); (2) `info.max_timeline_count` 1→64 (each queue binds ring_idx≥1). Detail: the `phase5-ctx-attach`, `phase5-backend-status`, `mesa-venus-icd-port` memories.

**NEXT = Phase 4e (async submission) — the proper fix for the synchronous-submit deadlock class.** Today `submit_venus` blocks (spin-polls the used ring via `add_notify_wait_pop`); the one known Phase-5 gap is `vkQueueWaitIdle`/`vkDeviceWaitIdle` → `VK_ERROR_UNKNOWN` (the empty-submit fence-feedback ffb slot is never seen updated; non-fatal — ordinary fence waits work). Plan: make SUBMIT_VENUS non-blocking (KMD in-flight buffer pool + drain/`pop_used` + wire the already-built `FenceTable`), and defer the ICD's premature `sync->val` to a real `WAIT_FENCE`; **poll-first, then interrupt-driven** (the `WDFINTERRUPT` + DIRQL ISR-status-read + DPC path, gated on resolving the `STATUS_DEVICE_POWER_FAILURE`/MSI-X-vs-INTx interrupt-resource question). Then: an optimal vkcube **present** path (NOT a GDI `StretchDIBits` hack — mine the Linux virtio-gpu driver's present/flush flow and find the Windows-equivalent surface/composition API), then DXVK + VKD3D-Proton. Full design + the no_std-async-Rust evaluation: **`icd/PHASE4E_ASYNC_HANDOVER.md`** and the `phase4e-async-submit` memory.
