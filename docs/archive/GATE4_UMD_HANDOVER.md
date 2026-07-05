# Gate 4 — UMD Handshake Handover

Status: handover brief, 2026-06-18. Start here for the **WDDM render-only D3D UMD** work
(`WDDM_RENDER_ONLY_3_2.md` §7 / §10 Gate 4). Read `WDDM_RENDER_ONLY_3_2.md` and
`WDDM_RENDER_ONLY_DDI_CHECKLIST.md` first — they are the canonical spec/contract; this brief
is the entry point and current-state snapshot.

Gate 3 (real `DxgkDdiSubmitCommand`/fences/TDR) is intentionally **deferred** — the KMD's
submit path is a null engine that completes fences immediately, which is fine for the Gate 4
handshake. Come back to Gate 3 before real rendering throughput matters.

---

## 1. Mission (Gate 4, minimal + honest)

Get the Direct3D runtime to **load the Helios UMD and create a device honestly** — i.e. real
`OpenAdapter` → `GetCaps`/`GetSupportedVersions` → `CalcPrivateDeviceSize` → `CreateDevice`
that returns a **valid (even if minimal) D3D device function table**, OR fails earlier with a
precisely documented capability reason. No rendering yet. No translation yet beyond honest
no-op validation if the runtime permits it.

**Gate 4 is NOT full rendering.** Wiring DXVK (D3D11) and VKD3D-Proton (D3D12) translation
internals behind the UMD DDI, and routing Vulkan/venus through the WDDM KMD, is Gate 5/6.

**Exit criteria (from the spec):** the runtime loads the Helios UMD and a device is created or
refused *honestly*; no app sees fake feature levels; `dxdiag`/DXGI enumeration stays coherent;
Explorer/DWM stay healthy.

### ⚠️ The load-bearing lesson — do NOT repeat `.52/.53`

Versions `.52/.53` added a **fake `CreateDevice` function-table scaffold** (returned success
with a half-filled device funcs table). Result: **Explorer/DWM heap corruption**. The current
UMD was reverted to honest `CreateDevice → E_NOTIMPL` (`umd/src/lib.rs:189`). The
non-negotiable (`WDDM_RENDER_ONLY_3_2.md` §2): **no success-returning stubs; no fake caps.**
A `CreateDevice` that returns success MUST fill a device funcs table whose entries either do
something real or fail honestly when called — and the device/allocation/submission contracts
underneath must be coherent. Build it incrementally and watch Explorer/DWM after every install.

Mitigation already in place: the Looking Glass IDD pins **Microsoft Basic Render Driver** as
its render adapter (`IddCxAdapterSetRenderAdapter`), so DWM/Explorer do **not** route through
the Helios UMD. That makes a `CreateDevice` bring-up far less likely to take down the desktop,
but it is not a license to fake success.

---

## 2. Current verified state (as of `22.22.32.58`, live)

> **UPDATE (`.62`, 2026-06-18): `GetSupportedVersions` now returns an EMPTY list**
> so the runtime no longer reaches `CreateDevice` at all — this fixed a DWM
> `dwmcore.dll 0x889800b0` crash loop caused by the advertise-render-caps-but-
> fail-`CreateDevice` inconsistency (see `GATE2_3_CAPS_BACKING.md`). The handshake
> mapping below is still the accurate record of the `CreateDevice` DDI contract;
> re-add the single correct version (D3D11_0 = `0x000b000a`) when the device funcs
> table is implemented (Gate 5b). The instrumented `create_device` stays as-is.

### Gate 4 handshake — demonstrated end-to-end (`.58`, 2026-06-18)

The full D3D11 device-create handshake now reaches `CreateDevice` and fails
*honestly*, proven with a deliberate device-create probe (the only thing that
drives the path — see below). This satisfies the Gate-4 exit criterion "runtime
loads the Helios UMD and fails or succeeds honestly; no app sees fake feature
levels":

- **Normal DXGI/dxdiag enumeration never reaches `CreateDevice`.** It only drives
  `OpenAdapter10_2 → GetCaps → GetSupportedVersions → CloseAdapter`. The doc's
  earlier "reaches `CreateDevice → E_NOTIMPL`" needed a deliberate
  `D3D11CreateDevice` against the Helios adapter to exercise it.
- **Probe:** `tools/d3d11_devicecreate_probe.cpp` enumerates DXGI adapters,
  selects "Helios vGPU Render Adapter" (it enumerates twice — render adapters
  appear per Direct3D's logical-output pairing — alongside two Microsoft Basic
  Render Driver entries), and calls `D3D11CreateDevice(adapter, DRIVER_TYPE_UNKNOWN,
  …)`. Build with `vcvars64` + `cl /EHsc /W4 … /link dxgi.lib d3d11.lib`.
- **Result:** `D3D11CreateDevice` returns `hr=0x80004001` (E_NOTIMPL); the honest
  failure surfaces to the app as a clean D3D error, **no crash**. `explorer.exe`
  survives; DWM does its normal adapter-change restart (NOT the `.52/.53` heap
  corruption — that killed Explorer).
- **Negotiation captured (`C:\Windows\Temp\helios_umd.log`):** the runtime calls
  `CreateDevice` with `Interface=0x000a0009`, `Version=0x177a` (6010, the D3D
  runtime build version, not a DDI constant), `Flags=0x0`/`0x1`
  (`DISABLE_EXTRA_THREAD_CREATION`). All four pointers are valid: `pDeviceFuncs`
  (table to fill), `pKTCallbacks`, `pUMCallbacks`, `pDXGIBaseFuncs` (the
  `DXGI_DDI_BASE_ARGS` in/out base-function table — a real `CreateDevice` MUST
  fill it). `create_device` (`umd/src/lib.rs`) now decodes the real
  `D3D10DDIARG_CREATEDEVICE` and logs these fields.
- **Finding — fix the advertised version list before building the real table:**
  the runtime selected `0x000a0009` = our advertised `ddi_supported(10,9,0)`,
  which is **not a documented D3D DDI version** (D3D11_0 = `0x000b000a` → fills
  `p11DeviceFuncs`; D3D11_1 = `0x000b000f`). The runtime just echoes the highest
  entry from our (somewhat arbitrary) `SUPPORTED_DDI_VERSIONS`. When the device
  funcs table is implemented, `GetSupportedVersions` must advertise exactly the
  interface(s) whose `D3D11DDI_DEVICEFUNCS`/`D3D11_1DDI_DEVICEFUNCS` table we
  actually fill — no more.

### Earlier verified state (as of `22.22.32.57`, live)

- **KMD (`kmd_render`, service `helios_kmd_render`) binds at Code 0** — WDDM 3.2 render-only,
  dxdiag "No Problem", virtio-gpu transport up, Explorer/DWM healthy. Gate 1 complete.
- **Caps hardened + verified** against WDK 10.0.26100 `d3dkmddi.h` (`.55`): the mandatory
  render-only caps (WDDM 3.2, preemption, per-engine TDR, FlipOnVSyncMmIo,
  SupportKernelModeCommandBuffer) are present and correct. Coherence debt: those are advertised
  but not yet backed by real impl (Gate 2/3).
- **UMD (`helios_umd.dll`)** loads and answers honestly: `OpenAdapter10`/`OpenAdapter10_2`/
  `OpenAdapter12` fill minimal adapter funcs, `GetCaps`/`GetSupportedVersions`/
  `CalcPrivateDeviceSize→0` respond, `CreateDevice → E_NOTIMPL`. Logs to
  `C:\Windows\Temp\helios_umd.log`. Registered via `InstalledDisplayDrivers`/`UserModeDriverName`.
- **Venus 3D-context lifecycle validated standalone** (`.57`): `ctx_create(VENUS)` →
  `ctx_destroy` succeeds on the live device (KMD `VirtioGpu::self_test_venus_context`,
  breadcrumb `0x0B000010 → 0x1100`).
- **Venus resource transport primitives ready** in `kmd_render/src/virtio/gpu.rs`:
  `resource_create_blob(ctx_id, blob_mem, blob_flags, blob_id, size)` (= create_blob +
  `ctx_attach_resource`) and `resource_unref`, mirroring the proven System-class
  `kmd::alloc_blob`.

### The finding that shapes Gate 4/5

A **venus-backed (HOST3D mappable) allocation cannot be created standalone in the KMD.** A bare
`RESOURCE_CREATE_BLOB(HOST3D, blob_id=0)` is rejected by the host with `RESP_ERR_UNSPEC`
(verified live, `.56`). A HOST3D blob must reference a **venus device-memory id produced by the
UMD's `vkAllocateMemory` venus stream**, then `ctx_attach`. See `kmd/src/virtio/gpu.rs:771-774`
and the `phase4-blob-plan` memory.

**Consequence:** the venus render path is fundamentally **UMD-driven**. The UMD allocates venus
memory (via the venus encoder) → obtains a blob/mem id → the KMD creates the virtio blob
referencing it. This is the Gate 4→5 integration seam.

---

## 3. Architecture (the decision the gate must make concrete)

Target stack (`WDDM_RENDER_ONLY_3_2.md` §1):

```
D3D app → Windows DXGI + D3D runtime
        → Helios D3D11/D3D12 UMD (DDI provider, NOT a d3d11.dll/d3d12.dll replacement)
            → DXVK-derived D3D11 internals      (dxvk-helios submodule — SOURCE now available)
            → VKD3D/VKD3D-Proton D3D12 internals (vkd3d-proton-helios submodule — SOURCE available)
            → Vulkan command stream → Helios venus backend
        → Helios WDDM render-only KMD (allocations, submission, fences)
        → virtio-gpu Venus → QEMU/virglrenderer → host Vulkan
```

**Key constraint (do not violate):** the venus stream must flow through the **WDDM KMD's
allocation + submission path** (DxgkDdiCreateAllocation / DxgkDdiRender / SubmitCommand →
virtio), **not** the System-class `DeviceIoControl` IOCTL. Launcher-level DLL injection of
DXVK/VKD3D beside a game, or routing through the System-class venus ICD, is the explicit
anti-pattern (`§4`). The System-class path (`kmd/`, `icd/mesa` over IOCTL) stays as the
**reference + the working Vulkan/venus encoder to mine**, not the production transport.

This implies a likely large sub-task for Gate 5 (flag it now): the Mesa venus `vn_renderer`
backend currently has a System-class IOCTL implementation (`icd/mesa` `vn_renderer_helios.c`).
The WDDM UMD needs a `vn_renderer` (or equivalent venus transport) over the **D3DKMT/WDDM**
surface — `D3DKMTCreateAllocation` for venus memory, `D3DKMTRender`/`D3DKMTSubmitCommand` for
the command stream — mirroring the existing IOCTL backend but over WDDM. Gate 4 does not need
this yet, but the UMD's structure should anticipate it.

### UMD packaging decision (from §7)

Two options; **separate D3D11 and D3D12 UMD DLLs sharing a common Vulkan/venus backend** is the
lower-risk choice. Bring up D3D11 first (DXVK is more mature for this), keep the shared backend
small and explicit.

---

## 4. Building blocks & references

| Thing | Where | Use |
|---|---|---|
| WDDM UMD spec + gates | `WDDM_RENDER_ONLY_3_2.md`, `WDDM_RENDER_ONLY_DDI_CHECKLIST.md` | Canonical contract |
| Current UMD | `umd/src/lib.rs` | OpenAdapter*/GetCaps/CreateDevice→E_NOTIMPL; extend here |
| D3D11 translation source | `dxvk-helios/` (submodule) | Adapt `src/d3d11` + `src/dxvk` internals behind the UMD DDI |
| D3D12 translation source | `vkd3d-proton-helios/` (submodule) | Gate 6 |
| Venus encoder (working) | `icd/mesa` (Mesa venus ICD, System-class) | The byte-correct venus encoder; mine its `vn_renderer` + Gallium `d3d10umd` is the closest UMD-DDI-table reference |
| Proven venus blob sequence | `kmd/src/virtio/gpu.rs::alloc_blob` (`:764`) | create_blob → ctx_attach with a venus mem id |
| KMD venus transport primitives | `kmd_render/src/virtio/gpu.rs` | `resource_create_blob`, `ctx_attach_resource`, `resource_unref`, `ctx_create`/`ctx_destroy`, `submit_venus` |
| KMD allocation DDI | `kmd_render/src/ddi/create_allocation.rs` | bookkeeping-only today; wire venus blobs here using the UMD-supplied mem id |
| D3DKMT harness model | `probe/src/main.rs` | SetupDi/CreateFile pattern (System-class); for WDDM use D3DKMT* from gdi32 |
| DX test samples | `dx-samples-research-only/` | end-goal validation (needs Gate 5/6) |
| Mesa→Windows venus port plan | `icd/PHASE5_HANDOVER.md`, `mesa-venus-icd-port` memory | how the venus ICD was ported; the `vn_renderer` vtable map |

---

## 5. Concrete first steps (incremental, honest, testable)

Do these in order; build + install + watch Explorer/DWM after each (recovery model in §6).

1. **Map the D3D11 UMD DDI handshake precisely.** From the WDK headers (`d3d10umddi.h`):
   after `OpenAdapter10_2` the runtime calls `GetSupportedVersions`, `GetCaps`,
   `CalcPrivateDeviceSize`, then `CreateDevice` with a `D3D10DDIARG_CREATEDEVICE` carrying the
   `pDeviceFuncs` table to fill + the runtime callback table (`pKTCallbacks` / `pUMCallbacks`)
   + `hRTDevice`. Enumerate exactly which `pfnXxx` entries are mandatory for the lowest feature
   level you intend to advertise. Document required vs optional vs unsupported (mirror the
   `WDDM_RENDER_ONLY_DDI_CHECKLIST.md` style).
2. **Decide the minimal honest `CreateDevice`.** Either (a) fill a device funcs table where the
   handful of entries the runtime calls before first draw are real no-op-correct and everything
   else is a logged-unsupported stub that fails when actually invoked, returning `S_OK`; or
   (b) keep `E_NOTIMPL` until DXVK internals are wired. Pick the smallest thing that lets the
   runtime accept the device without faking a capability it then calls into and crashes on.
   **If you fill the table, the device/adapter caps you advertise must be the ones the runtime
   actually exercises — no more.**
3. **Bridge `QueryAdapterInfo`/private device size to the KMD** so UMD caps come from one source
   of truth (the KMD's `DxgkDdiQueryAdapterInfo`), not hardcoded twice.
4. **Stand up the venus backend inside the UMD** (begin the Gate 5 seam): create a venus
   `VkInstance`/`VkPhysicalDevice`/`VkDevice` for the device, with the venus `vn_renderer` going
   through **D3DKMT** (allocations via `D3DKMTCreateAllocation`, submit via `D3DKMTRender`),
   reusing `icd/mesa`'s encoder. This is where the §2 finding pays off: `vkAllocateMemory` →
   venus mem id → KMD `resource_create_blob`. Large; can be staged after the bare handshake.
5. **Wire `DxgkDdiCreateAllocation` to real venus blobs** using the mem id the UMD supplies
   (parse it from the allocation private driver data), via the transport primitives in
   `kmd_render/src/virtio/gpu.rs`. Until then, system-memory allocations can be exercised with a
   D3DKMT harness (the deferred standalone half of Gate 2).
6. **Validate with a real D3D11 device-create test** (`D3D11CreateDevice` selecting the Helios
   adapter, or a DXGI enum + create), then the simplest `dx-samples-research-only` D3D11 sample.

---

## 6. Build / test / recovery workflow

- **Build:** `win_cargo` with `crate_dir: "kmd_render"` and args
  `["make","--makefile","Cargo.make.toml"]` for the KMD (test-signs `.sys`+`.cat`, builds +
  packages `helios_umd.dll`, stamps the version from `Cargo.make.toml -v` + `build.rs`).
  Bump the version every iteration (currently `.57`) so packages stay distinct + rollback-able.
- **Install/bind (the device prefers Red Hat viogpudo):** `PCI\VEN_1AF4&DEV_1050&SUBSYS_11001AF4&REV_01`
  is matched by both Helios and the WHQL-signed `viogpudo` (`oem6`), which out-ranks our
  test-signed driver, so a fresh boot auto-binds viogpudo. Force Helios:
  `devcon update <pkg>\helios_kmd_render.inf "PCI\VEN_1AF4&DEV_1050&SUBSYS_11001AF4&REV_01"`
  (devcon at `C:\Program Files (x86)\Windows Kits\10\Tools\10.0.26100.0\x64\devcon.exe`).
  `devcon restart <hwid>` re-runs StartDevice. Prune stale `helios_kmd_render` oem packages with
  `pnputil /delete-driver oemNNN.inf /uninstall /force` (do NOT touch the other Red Hat virtio
  packages — they're in use).
- **Rollback (no snapshot):** known-good packages `.55`/`oem106` and `.54`/`oem135` stay staged;
  `devcon update` back to one of them, or boot with `HELIOS_DISABLE_VIRTIO_GPU=1` (recovery, no
  virtio-gpu) per `tools/launch-helios-gtk.sh`. UMD bring-up corruption affects user mode, so
  worst case is killing the test app; a KMD bugcheck needs the recovery boot.
- **Debug readout:** ntoseye `debug_log` is EMPTY on this setup (KD DbgPrint stream not
  surfaced). Use the **KMD `diag` registry-breadcrumb tracer** (`HKLM\SYSTEM\CurrentControlSet\
  Services\helios_kmd_render`, REG_DWORDs `S0,S1,…`; codes in `kmd_render/src/diag.rs`). The UMD
  logs to `C:\Windows\Temp\helios_umd.log`. ntoseye MCP for live kernel inspection
  (`status`/`resume`/`wait_for_stop`; symbols for dxgkrnl/dxgmms2 are loaded).
- **VM launch ownership:** if you change `tools/launch-helios-gtk.sh` or any
  launcher env/transport, make the change then **ask the user to (re)start the VM** — do not
  launch from automation (`WDDM_RENDER_ONLY_3_2.md` §2.1).

---

## 7. Non-negotiables (repeat of §2 spec, because they bite)

- No success-returning `CreateDevice`/DDI stubs; no fake caps/feature levels. Honest failure or
  real implementation only.
- DXGI stays Windows DXGI — DXVK's `src/dxgi` is research only.
- Venus/D3D work flows through WDDM DDIs (D3DKMT), not the System-class IOCTL and not a
  `d3d11.dll` drop-in.
- Watch Explorer/DWM after every install (the `.52/.53` corruption signature).

---

## 8. Open questions / risks to resolve early

- Exact mandatory `pDeviceFuncs` subset for the minimal feature level (WDK `d3d10umddi.h`).
- Whether to advertise a D3D feature level at all before DXVK is wired (likely not — keep
  `E_NOTIMPL` until the device funcs are real).
- The venus-over-D3DKMT `vn_renderer` backend is the dominant Gate 5 effort; scope it once the
  bare handshake works.
- How the UMD passes the venus mem id to `DxgkDdiCreateAllocation` (allocation private driver
  data format — define it as the shared `helios_protocol` contract, like the existing escape/
  IOCTL structs).
