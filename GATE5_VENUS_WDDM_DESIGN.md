# Gate 5 — Venus-over-D3DKMT Backend Design

> **PROGRESS (2026-06-18) — Stage 1 DONE + Stage 2a DONE + Stage 2b first light, all LIVE.**
> Stage 1 (open + context over D3DKMT, §6/§8) validated: `vulkaninfo` →
> `CTX_CREATE(VENUS) over D3DKMTEscape OK`. **Escape REQUIRES `hDevice`** (§8.3 was
> wrong that adapter-only suffices). The D3DKMT headers are the REAL vendored WDK
> headers (`icd/win-build/wdk-include`), not a shim. Stage 2a (allocations, §4/§6)
> DONE: `D3DKMTCreateAllocation`/`OpenAllocation`/`DestroyAllocation` succeed for a
> venus HOST3D blob; `blob_id=0` works; the missing piece was `DxgkDdiOpenAllocation`.
> Stage 2b: `D3DKMTLock2` works but maps system memory — next is the host-visible
> segment reshape. Authoritative running notes: the `gate5a-venus-d3dkmt` memory +
> `GATE5_STAGE2_ALLOC_DESIGN.md` + `HANDOFF_NEXT_SESSION.md`.

Status: design, 2026-06-18. Grounded in live exploration of `icd/mesa`,
`kmd_render/src`, `protocol/`, and WDK 10.0.26100 headers. This is the canonical
plan for the "venus `vn_renderer` over WDDM/D3DKMT" sub-task that
`GATE4_UMD_HANDOVER.md` §5 step 4 / §3 names as the dominant Gate-5 effort.
Read `WDDM_RENDER_ONLY_3_2.md` (non-negotiables §2) and
`WDDM_RENDER_ONLY_DDI_CHECKLIST.md` first.

---

## 1. The key insight — decouple from the D3D11 UMD

The venus backend is **not** in the Rust `umd/` crate. It is the **Mesa Venus
ICD** (`icd/mesa`, a C/meson Vulkan driver). Today its `vn_renderer_helios.c`
backend reaches the System-class KMD over `DeviceIoControl` (IOCTL on
`GUID_DEVINTERFACE_HELIOS`). The Gate-5 work is to **port that backend's
transport to WDDM D3DKMT** so it reaches the `kmd_render` WDDM adapter instead.

Two Windows user-mode components, brought up independently:

1. **Mesa Venus ICD** (`vulkan_helios` DLL) — a full Vulkan driver. Reaches the
   KMD via **D3DKMT thunks** (`D3DKMTCreateDevice`, `D3DKMTCreateContext`,
   `D3DKMTCreateAllocation`, `D3DKMTRender`, `D3DKMTEscape`, `D3DKMTLock2`,
   `D3DKMTWaitForSynchronizationObjectFromCpu`). These are **global `gdi32`
   exports callable from any process** — they need NO D3D11 runtime and NO UMD
   device-funcs table.
2. **D3D11 UMD** (`helios_umd.dll`, Rust) — the WDDM D3D11 DDI provider the D3D
   runtime loads (Gate 4 handshake done; device-funcs table = Gate 5b, via DXVK).
   It will translate D3D11 → Vulkan and call **into component 1**.

**Consequence — the test strategy that de-risks everything:** the existing
IOCTL ICD already renders `vulkaninfo`, `vkcube`, and Doom 2016 (per the
`nvidia-white-screen-investigation` memory). Porting only the transport lets us
validate the **entire** venus-over-WDDM KMD path (allocation + submit + fence
DDIs) with those same proven Vulkan workloads — **before** touching DXVK/D3D11.
The D3D11 UMD's CreateDevice → device-funcs-table problem is orthogonal and
deferred; it consumes this Vulkan backend once it works.

```
D3D app → D3D runtime → helios_umd.dll (DXVK d3d11 — Gate 5b)
                              │
                       Vulkan API
                              ▼
                   Mesa Venus ICD (vn_renderer over D3DKMT) ← Gate 5a (THIS doc)
                              │
            D3DKMTCreateAllocation / D3DKMTRender / D3DKMTEscape
                              ▼
              kmd_render WDDM KMD (real CreateAllocation / Render / SubmitCommand)
                              ▼
                       virtio-gpu Venus → host
```

Gate 5a (this doc) = the bottom two boxes. Gate 5b = wiring DXVK behind the UMD.

---

## 2. Non-negotiable: escape is NOT the rendering ABI

`WDDM_RENDER_ONLY_3_2.md` §2: *"No D3DKMTEscape render submission bypass.
Escapes are acceptable for diagnostics or narrow private controls, not as the
primary rendering ABI."*

Mapping each `vn_renderer` need to its WDDM carrier accordingly:

| Need | Carrier | Rationale |
|---|---|---|
| Adapter/device/context open | `D3DKMTOpenAdapterFromLuid` + `D3DKMTCreateDevice` + `D3DKMTCreateContext` | Standard WDDM lifecycle |
| Venus context create/destroy | `D3DKMTEscape` (`HELIOS_ESCAPE_CTX_CREATE`/`DESTROY`) | Narrow private control — already wired in KMD `DxgkDdiEscape` |
| Capset / renderer_info | hardcoded in ICD (no GET_CAPSET) or `D3DKMTEscape` | Narrow control; ICD already hardcodes it |
| Shmem ring alloc + device-memory BO | **`D3DKMTCreateAllocation`** | Real VidMm allocation path (NOT escape) |
| CPU map of a blob | **`D3DKMTLock2`** / CPU-visible segment | Real VidMm map path |
| Venus command submit | **`D3DKMTRender`** → DMA buffer | Real rendering ABI (NOT escape) |
| Fence wait | **monitored fence** (`D3DKMTWaitForSynchronizationObjectFromCpu`) | Real WDDM sync (interim: escape WAIT_FENCE) |

Escape stays only for context lifecycle + capset (genuinely narrow controls).
Allocation and submission MUST be `CreateAllocation` / `Render`.

---

## 3. The `vn_renderer` ops to re-route (from `icd/mesa/src/virtio/vulkan/vn_renderer.h`)

The Helios backend (`vn_renderer_helios.c`) implements these; the port keeps the
same op set and the non-blocking submit/wait model, swapping the transport call
inside each:

- `ops`: `destroy`, `submit` (non-blocking → records fence_id on syncs), `wait`
  (collects pending fence_ids, waits each).
- `shmem_ops`: `create` (HOST3D mappable blob, `blob_id=0`), `destroy` (cached).
- `bo_ops`: `create_from_device_memory` (HOST3D blob, `blob_id=venus mem_id`),
  `destroy`, `release_resource`, `map`, `flush`, `invalidate`.
- `sync_ops`: `create`, `destroy`, `reset`, `read`, `write` (timeline counter).

IOCTL→D3DKMT replacement per op (current IOCTL in parens):

| op | current (IOCTL) | ported (D3DKMT) |
|---|---|---|
| init/open | SetupDi+CreateFile | `D3DKMTOpenAdapterFromLuid`+`CreateDevice`+`CreateContext` |
| ctx create/destroy | `IOCTL_*_CTX_CREATE/DESTROY` | `D3DKMTEscape(CTX_CREATE/DESTROY)` |
| `shmem.create` | `ALLOC_BLOB(blob_id=0)`+`MAP_BLOB` | `D3DKMTCreateAllocation`(shmem priv data)+`Lock2` |
| `bo.create_from_device_memory` | `ALLOC_BLOB(blob_id=mem_id)` | `D3DKMTCreateAllocation`(bo priv data) |
| `bo.map` | `MAP_BLOB` | `D3DKMTLock2` |
| `bo/shmem.destroy` | `RELEASE_BLOB` | `D3DKMTDestroyAllocation` |
| `ops.submit` | `SUBMIT_VENUS` | `D3DKMTRender`(venus DMA buffer) |
| `ops.wait` | `WAIT_FENCE` | monitored fence wait |

---

## 4. KMD work (`kmd_render`) — make the null engine real

Current state (from exploration): `DxgkDdiCreateAllocation` is bookkeeping-only
(stores `size`, no virtio resource); `DxgkDdiSubmitCommand` is a null engine
(immediate fence completion); `DxgkDdiRender`/`Patch` are `STATUS_NOT_IMPLEMENTED`;
`DxgkDdiEscape` already handles `CTX_CREATE/DESTROY/SUBMIT_VENUS`. Venus
primitives exist in `virtio/gpu.rs` (`ctx_create`, `resource_create_blob` =
create_blob+ctx_attach, `resource_unref`, `submit_venus`).

Required:

1. **`DxgkDdiCreateAllocation` (`ddi/create_allocation.rs`)** — read the WDDM
   allocation private driver data (the new `HeliosWddmAllocPrivate` struct, §5),
   and for a venus-backed allocation call
   `resource_create_blob(ctx_id, blob_mem, blob_flags, blob_id, size)`; store the
   resulting `resource_id` (+ blob_id, size) in `AllocationContext`. CPU-visible
   allocations get the host-visible segment + an MDL for `Lock2`. Reuse the
   `kmd::alloc_blob` sequence. (Recall the .56 finding: `blob_id=0` HOST3D needs
   the host **render server**; `blob_id=mem_id` needs the venus mem id from the
   ICD's `vkAllocateMemory` — both already satisfied by the proven host config.)
2. **`DxgkDdiDescribeAllocation` / `OpenAllocation`** — fill real metadata; needed
   once allocations are referenced by submits.
3. **`DxgkDdiRender` / `DxgkDdiPatch` (`ddi/submit_command.rs` + new)** — accept
   the venus DMA command buffer (the `HeliosWddmCmdBuf` header + venus stream +
   allocation references), validate, stage into the DMA buffer. Patch resolves
   allocation handles → resource ids.
4. **`DxgkDdiSubmitCommand`** — instead of immediate completion, parse the staged
   command buffer, resolve referenced `resource_id`s, call
   `submit_venus(ctx_id, fence_id, venus_stream)`, then complete the fence
   (interim: synchronous; later: DPC on used-ring completion). Must never return
   an error in normal operation (WDDM contract).
5. **Monitored fence** — back `ops.wait`. Interim: keep the escape `WAIT_FENCE`
   the ICD already knows, migrate to a real monitored fence object.
6. **Diag breadcrumbs** (`diag.rs`): new prefix `0x0C_xx_xxxx` for the
   venus-over-WDDM path (CreateAllocation/Render/SubmitCommand entry+result).
   NOTE: `diag::record` is PASSIVE_LEVEL only — do NOT call it from
   `DxgkDdiSubmitCommand` (DISPATCH) or DPC/ISR.

---

## 5. Shared protocol contract (new — `protocol/src/wddm.rs`)

Two new wire structs cross the UMD(ICD)↔KMD boundary via D3DKMT private driver
data (parallel to the existing `escape.rs` IOCTL structs, which stay for the
System-class path). `repr(C)`, padding-free, `Pod`/`Zeroable`.

- **`HeliosWddmAllocPrivate`** — `D3DKMT_CREATEALLOCATION.pAllocationInfo[i]
  .pPrivateDriverData`. Carries what the KMD needs to make the virtio blob:
  `magic`, `version`, `blob_id: u64` (venus mem id; 0 = scratch shmem),
  `size: u64`, `blob_mem: u32` (HOST3D), `blob_flags: u32` (USE_MAPPABLE),
  `ctx_id: u32`, `map_cache: u32`, `kind: u32` (shmem | device_memory).
- **`HeliosWddmCmdBuf`** — header at the start of the `D3DKMTRender` command
  buffer: `magic`, `version`, `ctx_id: u32`, `ring_idx: u32`,
  `venus_offset: u32`, `venus_size: u32`, fence/seq fields. Followed by the
  opaque venus byte stream. Allocation references travel in the
  `D3DDDI_ALLOCATIONLIST` / patch list, not inline.

The C ICD mirrors these (same byte layout) in a small header, exactly as it
mirrors `escape.rs` today.

---

## 6. Staged plan (each stage independently testable)

- **Stage 1 — open + context over D3DKMT.** ICD: `D3DKMTOpenAdapterFromLuid`
  (LUID from DXGI enum matching "Helios"), `D3DKMTCreateDevice`,
  `D3DKMTCreateContext`, `D3DKMTEscape(CTX_CREATE)`. KMD: confirm `DxgkDdiEscape`
  works adapter-scoped over D3DKMT (it does for the System-class path's escape
  shapes). Test: tiny harness or `vulkaninfo` reaching `vkCreateInstance`.
- **Stage 2 — allocation over `D3DKMTCreateAllocation`.** `protocol/wddm.rs`
  `HeliosWddmAllocPrivate`; KMD `DxgkDdiCreateAllocation` → `resource_create_blob`;
  `Lock2` map path. ICD `shmem_ops`/`bo_ops` route here. Test: harness allocates +
  maps a blob, reads back; then the venus command ring comes up.
- **Stage 3 — submit over `D3DKMTRender` + fences.** `HeliosWddmCmdBuf`; KMD
  `DxgkDdiRender`/`Patch`/`SubmitCommand` → `submit_venus`; monitored fence for
  `wait`. **Interim shortcut (allowed for first light only):** reuse the
  already-wired escape `SUBMIT_VENUS` to get an end-to-end venus roundtrip, then
  migrate to `D3DKMTRender` before the stage is "done" (§2 forbids escape as the
  steady-state rendering ABI). Test: a venus roundtrip (`vkCmdFillBuffer`
  readback), then `vkcube`.
- **Stage 4 — validate.** `vulkaninfo` + `vkcube` over the WDDM ICD on the
  `kmd_render` adapter. This proves Gate 5a end-to-end. Then Gate 5b (DXVK behind
  the D3D11 UMD) consumes this Vulkan backend.

## 7. Open sub-decisions (flag for the owner)

1. **Stage-3 submit path order.** Recommended: interim escape `SUBMIT_VENUS` for
   first venus roundtrip (fast, KMD path already exists), then migrate to
   `D3DKMTRender` before declaring Stage 3 done. Alternative: go straight to
   `D3DKMTRender` (more KMD work before any green light).
2. **One ICD or two transports behind a flag.** Recommended: a new
   `vn_renderer_helios_wddm.c` selected by an env var (`HELIOS_TRANSPORT=wddm`)
   so the proven IOCTL backend stays runnable side-by-side for A/B during
   bring-up. Alternative: replace the IOCTL transport in place.
3. **Build packaging.** ICD builds via `win_meson` from the share (Mesa is
   excluded from the `win_cargo` mirror). Need to confirm the D3DKMT thunks link
   (`gdi32.lib`) and the LUID-enum needs `dxgi.lib`.

---

## 8. Stage 1 implementation notes (code-grounded, ready to execute)

Decisions locked (2026-06-18): **port the transport in place** in
`icd/mesa/src/virtio/vulkan/vn_renderer_helios.c` (no separate file/flag);
**straight to `D3DKMTRender`** for submit (no escape SUBMIT_VENUS shortcut).

Exact edit points in `vn_renderer_helios.c` (line refs as of this writing):

1. **Includes/link (top, ~`:48`).** Add `#include <d3dkmthk.h>` (pulls
   `D3DKMT_*` + the `D3DKMTCreateDevice/CreateContext/CreateAllocation/Render/
   Escape/DestroyDevice/CloseAdapter` prototypes) and `#include <dxgi.h>` (LUID
   enum). `meson.build` Windows branch (~`:120`) currently links `setupapi.lib`;
   add `gdi32.lib` (D3DKMT thunks live in gdi32) and `dxgi.lib`. `setupapi` can
   stay until the SetupDi open path is removed.
2. **`struct helios` (~`:238`).** Replace `HANDLE dev;` with the D3DKMT handle
   set: `D3DKMT_HANDLE adapter; D3DKMT_HANDLE device; D3DKMT_HANDLE context;
   LUID adapter_luid;`. Keep `dev_mutex`, `ctx_id`, `next_fence_id`.
3. **New `helios_escape()` (~`:298`, replacing/alongside `helios_ioctl`).** One
   `D3DKMTEscape` round-trip: fill `D3DKMT_ESCAPE { hAdapter=adapter,
   hDevice=device, Type=D3DKMT_ESCAPE_DRIVERPRIVATE, pPrivateDriverData=buf,
   PrivateDriverDataSize=size, hContext=context }`. The KMD's `DxgkDdiEscape`
   already validates the `helios_escape_header` and dispatches CTX_CREATE/
   DESTROY/SUBMIT_VENUS — so `helios_ioctl_ctx_create`/`_ctx_destroy` just swap
   their `helios_ioctl(...)` call for `helios_escape(...)` with the SAME payload
   struct (escape is in/out, so the out fields like `out_ctx_id` come back in the
   same buffer). NOTE the KMD escape is adapter-scoped; CTX_CREATE/DESTROY do not
   need `hDevice`/`hContext`, but pass them when available.
4. **`helios_open_device` (~`:643`) → `helios_open_d3dkmt`.** (a) Enumerate DXGI
   adapters (`CreateDXGIFactory1` + `EnumAdapters1`), match
   `Description` contains `L"Helios"` (vendor `0x1af4`, device `0x1050`), capture
   `DXGI_ADAPTER_DESC1.AdapterLuid`. (b) `D3DKMTOpenAdapterFromLuid{ AdapterLuid }`
   → `adapter`. (c) `D3DKMTCreateDevice{ hAdapter=adapter }` → `device`. (d)
   `D3DKMTCreateContext{ hDevice=device, NodeOrdinal=0, EngineAffinity=0 }` (or
   `CreateContextVirtual`) → `context`. Return success; store all three + luid.
5. **`helios_init` (~`:1318`).** Replace `helios->dev = helios_open_device(...)`
   + the `INVALID_HANDLE_VALUE` check with the D3DKMT open. `helios_ioctl_ctx_
   create(VENUS)` then rides `D3DKMTEscape` unchanged.
6. **`helios_destroy` (~`:1294`).** Replace `CloseHandle(dev)` with: ctx_destroy
   escape, then `D3DKMTDestroyContext(context)`, `D3DKMTDestroyDevice(device)`,
   `D3DKMTCloseAdapter(adapter)` (guard each handle != 0).
7. **Deferred to Stage 2/3 (leave returning honest failure until then):**
   `helios_ioctl_alloc_blob`/`_map_blob`/`_release_blob` → `D3DKMTCreateAllocation`
   /`Lock2`/`DestroyAllocation`; `helios_ioctl_submit_cs` → `D3DKMTRender`;
   `helios_ioctl_wait_fence` → monitored fence. During Stage 1 these still call
   the (now-removed) IOCTL path, so they must be stubbed to fail cleanly — the
   Stage 1 test target is `vkCreateInstance` + the up-front CTX_CREATE over
   `D3DKMTEscape`, NOT a full render.

**KMD side for Stage 1: already done.** `kmd_render` `DxgkDdiEscape`
(`ddi/escape.rs`) handles `HELIOS_ESCAPE_CTX_CREATE`/`CTX_DESTROY` →
`VirtioGpu::ctx_create/ctx_destroy` (validated live by the `.57` self-test). No
KMD change needed for Stage 1; Stage 2 adds `DxgkDdiCreateAllocation` wiring,
Stage 3 adds `DxgkDdiRender`/`SubmitCommand`.

**Stage 1 test:** build ICD via `win_meson`, register its manifest, run a tiny
Vulkan app (or `vulkaninfo --summary`) → expect `vkCreateInstance` to succeed and
the venus context to come up over `D3DKMTEscape` (CTX_CREATE breadcrumb
`0x0B0000xx` in the KMD diag registry). Allocation/submit will fail until Stage
2/3 — that is expected and honest.

**Operational note:** ICD bring-up over the `kmd_render` adapter shares the
DWM-crash-loop hazard (§ the caps-coherence debt: DWM faults in `dwmcore.dll`
`0x889800b0` whenever Helios is the bound adapter — pre-existing, not Gate-5
caused). It does not block Vulkan-app testing (those don't depend on DWM), but
expect the desktop/Looking Glass to be unstable while Helios is bound.
