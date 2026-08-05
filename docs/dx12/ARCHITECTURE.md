# ARCHITECTURE.md — the D3D12 UMD split

**What this is:** the mechanical guide to creating the D3D12 user-mode driver as a *second* DLL
beside the shipping D3D11 one — which crates exist, what moves into a shared crate and what change
each move needs, what `umd12` may and may not export, how the cxx bridge to vkd3d-proton is shaped,
how both are built, signed, registered and deployed, how D3D12 is killed from the registry when it
breaks, and the order the whole thing lands in so the working D3D11 desktop is never at risk.

**What this is not:** the `d3d12umddi` contract (that is `DDI_REFERENCE.md`, which exists — tables,
caps, objects, fences, the minimum-viable set), the KMD-side work list (`KMD_IMPACT.md`, which also
exists), the present path (`PRESENT.md`), the vkd3d/venus substrate gap (`SUBSTRATE.md`), or the
gate commands (`GATES.md`).
⚠ **Scope note, 2026-08-05:** `DECISIONS.md` D2 removed the app-facing vkd3d arm — vkd3d is an
engine behind `helios_umd12.dll`, never an application's `d3d12.dll`, and **DXVK's `dxgi.dll` is
not part of any deliverable** (a WDDM UMD implements the DXGI DDI; MS's `dxgi.dll` is the frontend —
`umd/build.rs:238-243`). The split plan, the crate layout, the bridge and the deploy story below are
unaffected; only the "Phase 0" framing in stage S0b changed.

It is also not a re-argument of the decisions: the D-, H-, P-, K- and V-series entries are
settled in `DECISIONS.md` and cited here by id.

**Sources:** `research/R4-umd-template-and-split.md` (anatomy + split plan),
`research/R11-registration-packaging.md` (registration, packaging, rollback),
`research/R3-vkd3d-internals.md` (§1 module map, §8 Windows build, §10 fork status),
`research/R6-d3dkmt-surface.md` §2 (layer ownership), `research/R2-runtime-contract.md` §1 (object
model). **Every LOC count and `file:line` below was re-verified against the tree at branch `wddm`
on 2026-08-05** — R4's fact-checker found several off-by-a-few counts, and the corrections are
called out where they matter.

⚠ SDK header line numbers are pinned to **Windows SDK 10.0.26100.0**, staged (uncommitted) at
`tmp/dx12/sdk/`. Re-stage with the PowerShell block in `DECISIONS.md`'s preamble (before §1) if it is empty.

---

## 1. The target architecture

### 1.1 One picture

The D3D11 stack with two boxes swapped (DECISIONS D1). Everything below the bridge is shipping
today and composites the desktop.

```
   D3D11 (shipping, untouched)                    D3D12 (new)
 ┌──────────────────────────────┐        ┌──────────────────────────────────┐
 │ app / dwm                    │        │ app / dwm / 3DMark               │
 │  ID3D11Device, IDXGISwapChain│        │  ID3D12Device, IDXGISwapChain    │
 └──────────────┬───────────────┘        └──────────────┬───────────────────┘
   MS d3d11.dll + dxgi.dll                 MS d3d12.dll + D3D12Core.dll + dxgi.dll
                │ d3d10umddi                              │ d3d12umddi
                │ UserModeDriverName[2]                    │ UserModeDriverName[3]   ← the only
                ▼                                          ▼                            new slot
 ┌──────────────────────────────┐        ┌──────────────────────────────────┐
 │ helios_umd.dll   (Rust)      │        │ helios_umd12.dll  (Rust)   NEW   │
 │  umd/  + umd/bridge/*.cpp    │        │  umd12/ + umd12/bridge/*.cpp     │
 └──────────────┬───────────────┘        └──────────────┬───────────────────┘
                │ cxx bridge                             │ cxx bridge
                ▼                                        ▼ LoadLibrary + GetProcAddress
 ┌──────────────────────────────┐        ┌──────────────────────────────────┐
 │ DXVK engine, statically      │        │ helios_vkd3d.dll  (vkd3d-proton, │
 │ linked (8 × .a, zlib licence)│        │ LGPL-2.1-or-later, D4)           │
 └──────────────┬───────────────┘        └──────────────┬───────────────────┘
                └──────────────┬─────────────────────────┘
                               ▼   both crates share  helios_umd_common (rlib)   NEW
                    Mesa venus ICD (icd/mesa) — D3DKMTEscape(HELIOS_ESCAPE_SUBMIT_VENUS)
                               ▼
                    kmd_render → virtio-gpu → virglrenderer → host GPU → SET_SCANOUT_BLOB
```

Three new artifacts, in dependency order: `umd_common/` (rlib, shared by both UMDs),
`helios_vkd3d.dll` (vkd3d's `d3d12core` target renamed, with **two** added exports — D4), `umd12/` (cdylib).

### 1.2 The exact call chain, `OpenAdapter12` → `ID3D12Device`

Named function by named function. Left column is the D3D11 site to model each step on.

| # | D3D12 step | Header / signature | Model it on |
|---|---|---|---|
| 0 | loader resolves `OpenAdapter12` by name from the DLL in `UserModeDriverName[3]` | `PFND3D12DDI_OPENADAPTER` (`d3d12umddi.h:2694`) | `umd/src/adapter.rs:177` (already exported and refusing) |
| 1 | `OpenAdapter12(D3D12DDIARG_OPENADAPTER*)` | `d3d12umddi.h:2686-2692`: `{ hRTAdapter, hAdapter, pAdapterCallbacks, pAdapterFuncs }` | `open_adapter_common`, `umd/src/adapter.rs:191-252` |
| 2 | knob gate: `UMD_D3D12.get()` false ⇒ return `DXGI_ERROR_UNSUPPORTED` | §10 | — |
| 3 | `log_self_module_path()` + `log_knob_inventory()` — WHICH DLL, WHICH knobs | `umd/src/log.rs:187`, `:235` | `adapter.rs:218-219` |
| 4 | stash `hRTAdapter.handle` for `pfnEscapeCb` | — | `adapter.rs:225` (`scanout_acquire::note_runtime_adapter`) |
| 5 | `open->hAdapter.pDrvPrivate = &ADAPTER_TOKEN` | ZST, address-taken | `adapter.rs:120-121, 227-229` |
| 6 | fill all **8** slots of `D3D12DDI_ADAPTERFUNCS` / `_0109` | `d3d12umddi.h:2674-2684` / `:13640-13650` | `adapter.rs:235-249` |
| 7 | runtime → `pfnGetSupportedVersions(D3D12DDI_HADAPTER, UINT32* puEntries, UINT64* pSupportedDDIInterfaceVersions)` | `PFND3D12DDI_GETSUPPORTEDVERSIONS`, `d3d12umddi.h:2608` | `adapter.rs:578` + `SUPPORTED_DDI_VERSIONS` `:29-33` and the `const _` lockstep assert `:90-95` |
| 8 | runtime → `pfnGetCaps` — the caps gauntlet (H4) | `D3D12DDICAPS_TYPE`, `d3d12umddi.h:94-150` — **43 enumerators** (DECISIONS §4.1 is canonical): 40 carry the `D3D12DDICAPS_TYPE_` prefix and the other 3 are `D3D12DDI_FEATURE_D3D12_PREDICATION_106`, `..._PLACED_RESOURCE_SUPPORT_INFO_106`, `..._HARDWARE_COPY_106`, which live in the *same* enum and are equally valid `pfnGetCaps` types. ⛔ There are **no** versioned `D3D12DDICAPS_TYPE_00xx_*` additions elsewhere in the header — every versioned name is inside this enum; the only other caps enum is `D3D12DDICAPS_TYPE_VIDEO_0020` (`:4327`), a different type. The enumerated working set is `DDI_REFERENCE.md` §11.1 (all 43 values), the must-answer subset is §11.2, and the three that must be pinned conservatively from commit 1 are §11.6 | `umd/src/caps.rs` — pattern only; the enum is different and 5× larger |
| 9 | runtime → `pfnGetOptionalDDITables(hAdapter, UINT32* puEntries, D3D12DDI_TABLE_REQUEST*)` — **no D3D11 analogue** | `d3d12umddi.h:2524-2525`, `D3D12DDI_TABLE_REQUEST` `:2518-2522`, `D3D12DDI_TABLE_TYPE` `:2488-2516` — **25 enumerators**, values 0-4, 7-17, 19-27 (⛔ *not* 27: 27 is the highest assigned value, `D3D12DDI_TABLE_TYPE_0096_EXTENDED_FEATURES`, and the value space has gaps at 5, 6, 18). All 25 are listed in `DDI_REFERENCE.md` §2.1; the exhaustive match DECISIONS §7.4 demands has **25 arms**, not 27 | nothing — new code |
| 10 | runtime → `pfnFillDDITable(hAdapter, D3D12DDI_TABLE_TYPE, VOID*, SIZE_T, UINT, D3D12DDI_HRTTABLE)` — **honour the `SIZE_T`** (DECISIONS §7.3) | `d3d12umddi.h:2527-2528` | `stub_fill_device_table<T>`, `umd/src/device_funcs.rs:1168-1175` — derive the slot count from the *given* size, never `size_of::<T>()` |
| 11 | runtime → `pfnCalcPrivateDeviceSize(hAdapter, D3D12DDIARG_CALCPRIVATEDEVICESIZE*)` → `SIZE_T` | `d3d12umddi.h:2604` | `calc_private_device_size`, `adapter.rs:262-271` |
| 12 | runtime → `pfnCreateDevice(hAdapter, D3D12DDIARG_CREATEDEVICE_0109*)` | `d3d12umddi.h:13618-13636` — carries `pKTCallbacks` (the *same* 65-entry `D3DDDI_DEVICECALLBACKS` D3D11 uses, R2 §1.2) and `p12UMCallbacks_0062` (**18 live** D3D12-only callbacks, `d3d12umddi.h:8606-8647`; 28 declared lines, ten of them same-offset `#else pfnReserved` alternates — DECISIONS §4.1) | `create_device`, `adapter.rs:273-522` — copy all eight steps, especially validate-before-construct (`:350-364`) and the `DeviceUnderConstruction` guard (`:428-430`, `Drop` at `:544-570`) |
| 13 | `BridgeDevice12::create(luid_low, luid_high)` | §7 | `bridge::BridgeDevice::create`, `umd/src/bridge.rs:455-458`, called at `adapter.rs:382` |
| 14 | C++ `helios_vkd3d_create_device(luid, iid, &device)` inside `umd12/bridge/vkd3d_bridge.cpp` | §7.3 | `helios_dxvk_create_device`, `umd/bridge/dxvk_bridge.cpp:1666-1773` |
| 15 | `LoadLibrary("helios_vkd3d.dll")` + `GetProcAddress` for the **two** Helios exports, `helios_vkd3d_create_device` and `helios_vkd3d_serialize_root_signature` (D4) | §7.4 | `resolve_helios_icd_module`, `umd/bridge/bridge_icd_exports.cpp:269-287` (same "cache only after success" rule) |
| 16 | inside `helios_vkd3d.dll`: `vkd3d_create_instance(&instance_ci, &instance)` then `vkd3d_create_device(&device_ci, &IID_ID3D12Device, (void**)&dev)` | `vkd3d-proton-helios/include/vkd3d.h:104` and `:110`; `struct vkd3d_device_create_info` at `:74-94` | ⛔ **not** through `d3d12core_CreateDeviceFromFactory` (`libs/d3d12core/main.c:643`), which is what `D3D12GetInterface` → `d3d12core_CreateDevice` (`:742`) reaches and which resolves the adapter through `d3d12_get_adapter` (`:375`) → `CreateDXGIFactory1` (`:383`, `:406`). There is **no** `D3D12CreateDevice` in `libs/d3d12core/main.c` at all — that export is `libs/d3d12/main.c:143`, in the separate thin `d3d12.dll` target Helios never loads |
| 17 | `core::ptr::write(hDrvDevice.pDrvPrivate as *mut HeliosD3D12Device, …)` | in-place into the runtime-allocated private block | `adapter.rs:389-424` |
| 18 | fill `D3D12DDI_DEVICE_FUNCS_CORE_0109` (124 slots, `d3d12umddi.h:13451`) via `pfnFillDDITable` | | `forward::install*` + the `#[must_use] Filled*` tokens, `umd/src/forward/tables.rs:58-70` |
| 19 | per `ID3D12CommandQueue`: `pfnCreateCommandQueue` → the UMD calls `pfnCreateContextCb(hRTCommandQueue, D3DDDICB_CREATECONTEXT*)` (`d3d12umddi.h:2556-2559`) — **one WDDM context per queue** (R2 §1.3) | | `create_runtime_context`, `umd/src/device_funcs.rs:1046-1094` — same call, different cardinality (per queue, not per device) |

⚠ `D3D12DDIARG_OPENADAPTER` has **no `Interface`/`Version` member** (compare
`D3D10DDIARG_OPENADAPTER`, whose `Interface`/`Version` `open_adapter_common` logs at
`adapter.rs:212-217`). All version negotiation is `pfnGetSupportedVersions` +
`pfnGetOptionalDDITables` + `pfnFillDDITable`. R11 §1.4.

---

## 2. Anatomy of the existing D3D11 UMD

This table is the split plan's input. **LOC re-counted with `wc -l` on 2026-08-05** — R4 §1 reported
`umd/src` = 5 781 (actual **5 774**), `forward/` = 13 276 across "20 modules" (actual **13 283**
across **19**). The C++ figure (3 112) was right.

Class: **(a)** D3D11/`d3d10umddi`-specific · **(b)** generic UMD infrastructure · **(c)** Helios
platform glue (venus/KMD/scanout/ICD contract — D3D-version-independent but Helios-specific).

### 2.1 `umd/src/*.rs` — 5 774 lines

| File | LOC | Class | What it is | D3D12 disposition |
|---|---:|:--:|---|---|
| `lib.rs` | 91 | b+a | module list; `DllMain` (`:65-76`); `#![deny(deprecated)]` (`:16`); `#[cfg(not(windows))] compile_error!` (`:25-29`); re-exports (`:78-91`, i.e. to the last line of the 91-line file) | **Template.** `umd12/src/lib.rs` is a near-copy; `DllMain` body and the deny-guard verbatim |
| `adapter.rs` | 608 | a | `OpenAdapter10`(`:150`)/`OpenAdapter10_2`(`:158`)/`OpenAdapter12`(`:177`), `NegotiatedInterface` (`:47-76`), `SUPPORTED_DDI_VERSIONS` (`:29-33`), `create_device` (`:273`), `DeviceUnderConstruction` (`:534-570`), `close_adapter` (`:572`), `get_supported_versions` (`:578`), `AdapterToken` (`:120`) | **Split.** `OpenAdapter12` moves to `umd12/src/adapter12.rs`; the *pattern* (closed-set enum, unwind guard, ZST token) is copied, not shared |
| `caps.rs` | 266 | a | `FeatureProfile` (FL10_0/FL11_0/FL11_PIPELINE_ONLY), `get_caps` over 8 `D3D10_2DDICAPS_TYPE_*` | **Pattern only** — copy the discipline (one profile struct + compile-time coherence asserts) into `caps12.rs`; D3D12's caps enum has **43** enumerators (H4, DECISIONS §4.1) |
| `ddi.rs` | 80 | a | `include!($OUT_DIR/d3d10umddi.rs)` + 12 hand-pinned `D3D10DDIARG_CREATEDEVICE` offset asserts (`:53-79`) | **Template** → `umd12/src/ddi12.rs`, same shape over `D3D12DDIARG_CREATEDEVICE_0109` |
| `device_funcs.rs` | 1318 | a+c | `RuntimePagingQueue` (`:121`), `Window<T>` (`:134`), `RuntimeContext` (`:175`), `threading_caps` (`:314`), `HeliosDevice` (`:362`), `device_private_size` (`:672`), `UniformFn` (`:677`), `log_backtrace` (`:694`), `ddi_noop_device` (`:717`), `ddi_noop_dxgi` (`:737`), `ddi_calc_size` (`:752`), the three `ddi_relocate_device_funcs*` (`:784`,`:791`,`:798`), `stub_fill_device_table<T>` (`:1168`), `install_calc_and_lifecycle` (`:1185`), `fill_d3d11_device_funcs` (`:1234`), `fill_d3d11_1_device_funcs` (`:1247`), `fill_wddm1_3_device_funcs` (`:1258`) | **Mixed.** `stub_fill_device_table`, `Window<T>`, `UniformFn`, `log_backtrace` and the noop-counter idiom generalise (→ `umd_common`); the tables and `HeliosDevice`'s D3D11 fields do not |
| `bridge.rs` | 791 | c | `#[cxx::bridge] mod ffi` (`:29-249`), owned/borrowed COM discipline (`:251-307`, `adopt_resource` `:284`), `SrcRes`/`DstRes` (`:423`,`:427`), sealed `BridgeDevice` (`:446-450`, `create` `:455`) | **Template** → `umd12/src/bridge12.rs`, a second independent `#[cxx::bridge]` in a second crate |
| `forward.rs` | 598 | a+b | module list + shared import surface, `LogThrottle` (`:119-162`), **28** named `LogThrottle` statics in the file (R4 §5.8 said "eleven"/"fifteen" — re-counted): **26** at `:164-192`, plus `PRESENT_SKIP_LOG_COUNT` (`:495`) and `PRESENT_GATE_LOG_COUNT` (`:550`). ⚠ Moving only `:164-192` leaves those two behind and the move reads complete when it is not. (A further **13** `LogThrottle` statics live under `umd/src/forward/`: `present.rs` 10, `queries.rs`/`tiles.rs`/`deferred.rs` 1 each.) `DdiRefusals` (`:331`), `DDI_REFUSALS` (`:387`), `ddi_refusal_summary`, `note_ddi_refusal` (`:444`) | **Split.** `LogThrottle` + the refusal-registry *mechanism* → `umd_common`; the 11 D3D11 counters stay |
| `format.rs` | 449 | b | one `FormatInfo` row per DXGI format `0..=115` + **7** readers (`:221`-`:249`) + `to_d3dddi` (`:268`) / `from_d3dddi` (`:279`) — **9 public fns total** (the Linux test harness pins 8 legacy predicates) | **Move verbatim** to `umd_common`. Keeps its "no `windows`/WDK types" property (`:24-27`) |
| `hr.rs` | 93 | b | 8 HRESULT constants + 11 compile-time value/severity asserts. Its own doc names *"the D3D12 UMD DDI"* as an audience (`:1-2`) | **Move verbatim** to `umd_common` |
| `knobs.rs` | 408 | b+a | `#[link(name="advapi32")] RegGetValueA` (`:60-71`), `reg_dword` (`:82`), `DwordKnob` (`:109-131`), `BoolKnob` (`:133-153`), 10 knob statics (`:155-276`), `resolved_inventory()` (`:277-290`), **10** typed accessors (`trace_enabled` `:304`, `feature_level_mode` `:334`, `vehicle_flip_gate_us` `:347`, `scanout_acquire_knob` `:356`, `scanout_snapshot_knob` `:366`, `present_batch_fold` `:374`, `umd_async_present_stream` `:381`, `umd_free_threaded` `:388`, `umd_command_lists` `:398`, `umd_deferred_diagnostics` `:406`) | **Split.** reader half → `umd_common`; the 10 D3D11 knob *values* stay; `umd12` declares its own set. ⚠ `lib.rs:78-82` re-exports only **9** of the accessors — `present_batch_fold` is consumed inside `forward/` and is the one a "move the 9 accessors" reading leaves behind |
| `log.rs` | 244 | b | `umd_log_path()` (`:21`, `C:\ProgramData\Helios\umd-<pid>.log` at `:28`), `log_line` (`:45`, `#[deprecated]`), `close_at_detach` (`:117`), `LOG_CLOSE_CONTENDED` (`:140`), `trace_line!` (`:158`), `log_error!` (`:173`), `log_self_module_path` (`:187`), `log_knob_inventory` (`:235`) | **Move to `umd_common`** with the file basename parameterised (§4.2) |
| `scanout_acquire.rs` | 744 | c | D4a read-ledger over `pfnEscapeCb`; the three `helios_scanout_*` exports at `:611`, `:620`, `:683` | **Stays in `helios_umd.dll`** — its consumer is the statically-linked DXVK (§6) |
| `vehicle_exports.rs` | 84 | c | the three `helios_umd_*` exports the Mesa ICD resolves by name | ⛔ **MUST stay in exactly one DLL, and it is `helios_umd.dll`** (§6) |

### 2.2 `umd/src/forward/*.rs` — 13 283 lines, **19** modules

All class **(a)** unless noted. They exist because `forward.rs` was **10 744** lines before
T8/R1107 split it (commit `70a0438`, *"forward.rs 10744 -> 827"*).

| File | LOC | Purpose | D3D12 note |
|---|---:|---|---|
| `present.rs` | 2528 | the DXGI present path and the DXGI DDIs riding with it | D3D12's present is `PFND3D12DDI_PRESENT_0051` on the **command-list** table (P-C) — read `PRESENT.md` first. ✅ The identity channel transfers **unchanged, with no KMD change**: `D3D12DDIARG_CREATEDEVICE_0109.pKTCallbacks` (`d3d12umddi.h:13623`) is a `CONST D3DDDI_DEVICECALLBACKS*` — the same 65-entry kernel thunk table this file already drives — and it contains `pfnRenderCb` and `pfnPresentCb` (verified, `d3dumddi.h:4499`). So `umd12` writes a `HeliosPresentRenderCmd` and calls `pfnRenderCb` exactly as `present.rs:795` does, landing in the KMD's **PASSIVE** `dxgkddi_render` and its per-context stash |
| `resource.rs` | 1602 | resource create/open/destroy/resolve + WDDM allocation | D3D12 fuses heap+resource into `pfnCreateHeapAndResource` (H3) |
| `state.rs` | 1266 | per-object state behind `pDrvPrivate` | `ResidentAllocation`'s eviction `Drop` is a reusable idea |
| `shaders.rs` | 1070 | shader create + DXBC signature flattening for ≥11.1 | DXIL arrives as a container already — **no analogue** |
| `views.rs` | 1000 | view creation + the four view-descriptor translators | D3D12 uses descriptor heaps; not portable |
| `deferred.rs` | 873 | native deferred contexts + command lists (Phase C) | **closest existing analogue to D3D12 command lists — read before designing `forward12/`** |
| `layout.rs` | 698 | input layouts + the VS input-variant cache | D3D12 folds IA into the PSO |
| `transfer.rs` | 574 | copy/resolve/map/unmap/flush/discard/clear-view/UpdateSubresource | |
| `pipeline.rs` | 533 | pipeline binding + draw/dispatch | |
| `snapshot.rs` | 483 | D4b ordered-snapshot substitution | class (c) |
| `bindings.rs` | 470 | binding-array collection + per-stage setter families | |
| `handles.rs` | 333 | **class (b)** — `DdiHandle` (`:81`), `ComHandle` (`:93`), `com_handles!` (`:96`), `BoxedHandle` (`:121`), `boxed_handles!` (`:128`), `Com<T>` (`:178`), `Boxed<S>` (`:181`), `Slot<P>` (`:188`), `impl Slot<Com<T>>` (`:238`), `impl Slot<Boxed<S>>` (`:284`) | **Move the generic half to `umd_common`.** R2 §1.1: D3D12 handles are the *same* `pDrvPrivate`-word model (`D3D12DDI_HDEVICE = D3D10DDI_HDEVICE`, `d3d12umddi.h:24`; `D3D12DDI_HRESOURCE` is the next line, `:25`) |
| `state_objects.rs` | 315 | immutable pipeline state objects | |
| `tiles.rs` | 308 | tiled resources / WDDM1.3 tile-mapping DDIs | |
| `tables.rs` | 307 | **the structural template** — the six installers and the `#[must_use]` `Filled11_0/11_1/Wddm1_3` tokens (`:58-70`) | copy the token idiom into `forward12/` |
| `vehicle.rs` | 297 | the dcomp-vehicle present producer | class (c) |
| `format_caps.rs` | 272 | `CheckFormatSupport` per-format answer | |
| `queries.rs` | 247 | queries, predication, MSAA quality, perf counters | |
| `alloc.rs` | 107 | validated descriptors for the WDDM allocation path | class (c) — reusable shape |

**Verified slot counts** (`awk` per installer body over `tables.rs`, re-run this session):
`install()` = **144**, `install_11_1()` = **23**, `install_wddm1_3()` = **10**, `install_dxgi()` =
**7**, `install_dxgi_1_1()` = **1**, `install_dxgi_1_3()` = **10**. Total **195** `pfn… = Some(…)`
assignments. (Matches R4.) ⚠ **195 assignments ≠ 195 slots.** The ≥11.1 and WDDM1.3 installers
re-assign slots the 11.0 installer already filled (that is the whole point of the `#[must_use]`
ordering tokens at `tables.rs:58-70`), so the number of *distinct filled DDI slots* is **175 — 157
device + 18 DXGI**, which is the figure DECISIONS §4.2 uses for the D3D11-vs-D3D12 comparison
against D3D12's 214. Quote 175 when comparing surfaces and 195 only when talking about this file's
assignment count.

### 2.3 `umd/bridge/*` + `umd/bindgen` + `umd/build-support` — 3 139 lines

| File | LOC | Class | What it is |
|---|---:|:--:|---|
| `dxvk_bridge.cpp` | 1773 | c | `Logger Logger::s_instance("helios_umd_dxvk.log")` (`:70`); `PeriodicStat` (`:134`); `qpc_elapsed_us` (`:174`); `ComRelease<T>` (`:187`); `umd_log` (`:257`); `bridge_guard` (`:305-324`, `static_assert` at `:306`); `HeliosDxvkDeviceImpl` (`:357-393`); `helios_dxvk_create_device` (`:1666-1773`) |
| `dxvk_bridge.h` | 216 | c | the pimpl'd `HeliosDxvkDevice` complete type cxx needs; pimpl rationale at `:1-10` |
| `bridge_icd_exports.cpp` | 537 | c | venus ICD discovery: `TH32CS_SNAPMODULE` walk anchored on `helios_venus_memory_alloc_info` (`:38-67`), manifest fallback (`:243-265`), `resolve_helios_icd_module` (`:269-287`), the 8-entry export table (`:296-330`) |
| `bridge_icd_exports.h` | 44 | c | 8 published readers + the **include-order rule** (`:8-13`) |
| `bridge_dxbc.cpp` | 406 | a | DXBC container synthesis, compiled with **no** DXVK/COM/device header (`:1-13`) |
| `bridge_dxbc.h` | 107 | a | move-only `ShaderBytecode` (`:16-71`), `kSigEntryWords = 5`, 3 container factories |
| `bridge_common.h` | 29 | b | `umd_log(const char*)` + `bridge_log_budget(...)`, free of DXVK/COM/Vulkan/WDK includes (`:8-11`) |
| `bindgen/d3d10umddi_wrapper.h` | 20 | a | `windows.h` + a `#ifndef _NTDEF_` `NTSTATUS` typedef + `<d3d10umddi.h>` |
| `build-support/dxvk_c_compat.h` | 7 | c | `/FI` shim mapping POSIX `ssize_t` → `SSIZE_T` for libdisplay-info under clang-cl |

---

## 3. The two-DLL decision

**Decision (DECISIONS D3): two DLLs.** `helios_umd.dll` keeps `UserModeDriverName` slots 0–2 and is
not touched; `helios_umd12.dll` takes slot 3.

### 3.1 The live proof that slot 3 is independently served

Not inferred — measured on the guest (R11 §1.3). `KMTUMDVERSION` is the index
(`tmp/dx12/sdk/d3dkmthk.h:1830-1839`):

```c
typedef enum _KMTUMDVERSION
{
    KMTUMDVERSION_DX9 = 0,
    KMTUMDVERSION_DX10,
    KMTUMDVERSION_DX11,
    KMTUMDVERSION_DX12,
    KMTUMDVERSION_DX12_WSA32,
    KMTUMDVERSION_DX12_WSA64,
    NUM_KMTUMDVERSIONS
} KMTUMDVERSION;
```

`D3DKMTQueryAdapterInfo(KMTQAITYPE_UMDRIVERNAME=1, D3DKMT_UMDFILENAMEINFO{Version=v})` against the
Helios adapter returned, on 26100.8737:

```
adapter[0] h=0x40000000 luid=0:30217           <- Helios
   KMTUMDVERSION=0..3  status=0x00000000  name=...\helios_kmd_render.inf_amd64_3383a0e5...\helios_umd.dll
   KMTUMDVERSION=4,5   status=0xC000000D  name=            <- STATUS_INVALID_PARAMETER
```

Two facts fall out and both are load-bearing:

1. **The kernel serves each index independently from the `REG_MULTI_SZ`.** Microsoft states the
   positional contract for slots 1 and 2 (*"as the third entry … even if the version 11 DDI exists
   in the same DLL"*, `enabling-support-for-the-direct3d-version-11-ddi.md:18`) and explicitly says
   at `:20` that naming one DLL in several slots is a *convenience*, not the contract.
2. ⛔ **Write four entries, never six.** `KMTUMDVERSION_DX12_WSA32/64` return
   `STATUS_INVALID_PARAMETER` for every adapter on this build (Helios *and* both WARP/Basic-Render
   adapters). A six-element `REG_MULTI_SZ` buys nothing.

And the D3D12 slot is *already being read in production* — from `umd-1832.log` (dwm), verbatim:

```
[pid=1832 tid=5052] OpenAdapter12
[pid=1832 tid=5052] OpenAdapter12 -> DXGI_ERROR_UNSUPPORTED (D3D12 DDI not implemented yet)
[pid=1832 tid=2216] OpenAdapter10_2
```

23 of the 40 newest `umd-*.log` files carry at least one `OpenAdapter12`. ⚠ **dwm probes D3D12 on
every boot.** That is why §10 is not optional.

### 3.2 Why two, argued for this project

**(1) Load cost inside dwm is real, measured, and per-device.** `helios_umd.dll` is loaded and
unloaded **once per D3D11 device** — `GetModuleHandleW` reads NO / yes / NO across one
`D3D11CreateDevice` + `Release` (`tools/helios_handle_types.cpp`; recorded at `umd/src/lib.rs:48-54`
and `umd/src/log.rs:97-106`). dwm creates several devices. Everything linked into that DLL is
mapped, relocated and unmapped on each one. vkd3d + dxil-spirv is a whole second SPIR-V compiler
that dwm's compositor would pay for and never use.

**(2) Blast radius.** The D3D11 stack is the shipping product. One DLL means every `umd12` compile
error, static-init reorder, symbol collision and link-order change is a change to the binary dwm
loads at boot. This project has already lost a session to exactly that: commit `ead692e` — a
*refactor* of the C++ bridge — crash-looped dwm and LogonUI at cold boot (§7.2).

**(3) Rollback is a registry edit.** With two DLLs, "the D3D12 UMD will not even load" is fixed by
rewriting `UserModeDriverName[3]`; dwm's D3D11 composition device resolves index 2 and is provably
unaffected. `tools/hotplug-helios-umd.ps1:100-103` already writes that value. This is a *stability*
argument, not an aesthetic one (§10 L2).

**(4) Iteration.** `win_install_umd12` hot-swaps the D3D12 driver without touching the D3D11
registry entries. With one DLL, every D3D12 iteration redeploys the compositor's driver and needs
`-RestartDevice`, which restarts the display adapter.

**(5) LTO/size.** `umd` release is `lto = "thin"` with `debug = "full"` (`umd/Cargo.toml:32-38`).
One DLL means DXVK + dxbc-spirv + vkd3d + dxil-spirv + two SPIRV-Headers copies in one thin-LTO unit
with full debug info, mapped and unmapped per dwm device.

### 3.3 The counter-arguments, stated honestly

- **Duplication.** Both DLLs carry their own copy of `umd_common`'s code and each statically links a
  Vulkan-consuming engine. That is real and it is the price. A third shared DLL would add a
  load-order and versioning problem this project does not need.
- **Two ICD resolutions per process.** A process using both D3D11 and D3D12 resolves the venus ICD
  twice (one `resolve_helios_icd_module` static per module). Harmless *for the module handle* — it
  is refcounted and the ICD pins itself — `helios_pin_module`'s
  `GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN, …)` at
  `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:3858-3859`, whose refusal counter
  `helios_module_pin_failures` is declared at `:522-524` — but see §6.4 for the anchor-selection
  hazard, which is **not** harmless.
- **Per-device handle leak doubles.** `bridge_icd_exports.cpp` caches the ICD `HMODULE` and never
  releases it on the success path; a second DLL has its own copy of that static, so a process using
  both leaks at twice the rate (R11 §6-H2). Re-baseline `tools/helios_ownership_soak.cpp` after the
  split.
- **`DllMain` ordering.** `DLL_PROCESS_DETACH` under the loader lock while the *other* UMD DLL is
  mid-call is a real question the split creates and one DLL does not. Mitigation: `umd12`'s
  `DllMain` obeys the identical rule list at `umd/src/lib.rs:56-61` (no allocation, no I/O, no
  `LoadLibrary`, no thread waits, no panic, nothing on the process-exit path) and uses `try_lock`,
  counting the refusal, exactly like `log::close_at_detach` (`umd/src/log.rs:117-141`).

### 3.4 If the assumption ever breaks

The assumption is UNVERIFIED-1 (§13): slot 3 has never been observed serving a *different* string
from slots 0–2, because all four entries are identical on every adapter on this box. If the settling
experiment shows the slots are not independent, or a Windows update collapses them, the fallback is
**one DLL**, and it costs:

1. `umd12` becomes an **rlib**, not a cdylib; `umd` depends on it and re-exports its
   `OpenAdapter12` with `#[no_mangle]`.
2. The vkd3d bridge becomes a second `#[cxx::bridge]` **module in the same crate** —
   `cxx_build::bridges([..])` instead of `cxx_build::bridge(..)`. Extra TUs already work this way:
   `umd/build.rs:185-189` compiles `bridge_dxbc.cpp` and `bridge_icd_exports.cpp` on the same
   `cc::Build` *"so there is no flag duplication to drift"* (`:186-187`).
3. §8.4's link-coexistence question (two SPIR-V producers in one image) stops being avoidable and
   becomes a **pre-requisite**, because a link failure there breaks the compositor's driver.
4. The `UmdD3D12` kill switch (§10 L1) goes from strongly-advised to **mandatory** — it becomes the
   only rollback.

---

## 4. The `umd_common` shared crate

New crate `umd_common/`, package name **`helios_umd_common`**, `crate-type = ["rlib"]`, **no
`build.rs`**, **no WDK dependency**. Consumed as a path dependency from both `umd` and `umd12` —
exactly how `helios_protocol` is consumed today (`umd/Cargo.toml:12`).

⛔ **Do not add a cargo workspace.** There is none today (verified: no root `Cargo.toml`; `umd`,
`kmd_render`, `kmd_logic`, `protocol` and `tools/win-mcp` are standalone packages). Adding one would
change `CARGO_TARGET_DIR` semantics for `kmd_render`, which carries its own `Cargo.make.toml` + WDK
metadata, and path deps already work.

⚠ There is an **untracked** `umd_clean/` directory on this box declaring `package.name =
"helios_umd"` with `Win32_Graphics_Direct3D_Fxc` enabled. It is in no build and in no commit
(`git ls-files umd_clean` is empty). Do not model anything on it; do not let a glob pick it up.

### 4.1 What moves

| Module | From | Lines | Change required |
|---|---|---:|---|
| `umd_common::hr` | `umd/src/hr.rs` | 93 | none beyond `pub(crate)` → `pub` |
| `umd_common::format` | `umd/src/format.rs` | 449 | none. **Property P1 below must survive** |
| `umd_common::log` | `umd/src/log.rs` | 244 | **one**: `umd_log_path()` takes the basename. **Property P2 below must survive** |
| `umd_common::knobs` | `umd/src/knobs.rs:58-153` (DECISIONS D3b's range — it starts at the `use core::ffi::{c_void, CStr}` the FFI declaration needs, not at `:60`) | ~96 | `#[link(name="advapi32")]` (`:60-71`), `reg_dword`, `DwordKnob`, `BoolKnob` become `pub`; the whole module `#[cfg(windows)]`-gated. The **knob set** (`:155-276`) and all **10** accessors stay in `umd`; `umd12` declares its own. `log_knob_inventory` takes `&[(&'static str, u32)]` instead of calling `resolved_inventory()` |
| `umd_common::throttle` | `umd/src/forward.rs:119-162` (DECISIONS D3b's range; `:119` is where `pub(super) struct LogThrottle` starts — `:108-118` is its DEVIATION doc comment, which travels with it) | ~45 | `LogThrottle` and its six methods become `pub` |
| `umd_common::refusals` | `umd/src/forward.rs:322-448` (DECISIONS D3b's range) | ~25 moved | the *mechanism* is `ddi_refusal_summary` (`:416-437`, first-hit-emits-summary) + `note_ddi_refusal` (`:439-448`); generalise to `pub struct RefusalCounter { count: AtomicUsize, name: &'static str }` + `pub fn note(&self, summary: impl Fn() -> String)`. ⛔ The `DdiRefusals` struct and its **11 D3D11 fields** (`:322-411`) stay in `umd` — they are inside the cited range but do not move; `umd12` declares its own set with the same first-hit-emits-summary rule |
| `umd_common::slot` | `umd/src/forward/handles.rs:81-333` | ~255 | the generic half (`Slot<P>`, `Com<T>`, `Boxed<S>`, the three traits) is already free of `ddi::` types and moves verbatim. `com_handles!`/`boxed_handles!` become `#[macro_export]` with `$crate`-qualified paths so each cdylib invokes them over its **own** `ddi` module. (DECISIONS D3b cites `:177-333` for the `Slot`/`Com`/`Boxed` half; the range here additionally covers the two macros at `:96` and `:128`, which D3b's prose also moves) |
| `umd_common::noop` | `umd/src/device_funcs.rs:676-754, 1160-1175` (DECISIONS D3b's range; ⛔ **not** from `:672` — `:672` is `device_private_size`, which the "does not move" column below names explicitly) | ~110 | `UniformFn`, `log_backtrace`, `stub_fill_device_table<T>`, and `pub struct NoopTable { count: AtomicUsize, tag: &'static str }`. ⛔ `ddi_calc_size` **does not move** — its 256-byte answer is a D3D11-specific claim — and neither does `device_private_size` |
| `umd_common::window` | `umd/src/device_funcs.rs:134-161` | ~30 | `Window<T>` (pointer + capacity as one value) |
| `umd_common` **C++ side** (§4.4) | `umd/bridge/bridge_common.h` (29 L) + `PeriodicStat` (`dxvk_bridge.cpp:134`), `qpc_elapsed_us` (`:174`), `ComRelease<T>` (`:187`) — DECISIONS D3b's `dxvk_bridge.cpp:134-202` — **plus `bridge_guard` and its `static_assert`** (`:305-324`) | ~120 | becomes `umd_common/bridge/bridge_common.h`, a shared header **both** bridges `#include`. It carries no DXVK/COM/Vulkan/WDK include (`bridge_common.h:8-11`), so it is engine-agnostic by construction. ⚠ `bridge_guard`'s `static_assert` is the `ead692e` fix (§7.2); a second guard template written from scratch is how that bug comes back |

**Estimated move: ~1 470 lines** — **~1 350 Rust** (the nine Rust rows sum to 1 346) plus **~120
C++** — with essentially no behaviour change. That is DECISIONS D3b's "~1 500 lines"; the earlier
"~1 350" in this section counted only the Rust rows and silently dropped the C++ one. `umd/src` goes
from 5 774 → ~4 400 plus the unchanged 13 283 in `forward/`.

**Stays in `umd`:** `adapter.rs`, `caps.rs`, `ddi.rs`, `device_funcs.rs`, `bridge.rs`, all 19
`forward/*` modules, `scanout_acquire.rs`, `vehicle_exports.rs`, the ten D3D11 knob statics, the
eleven `DdiRefusals` fields, and `bridge/*.{h,cpp}` **except** the shared header carved out by §4.4
(`bridge_common.h` and the four engine-agnostic helpers listed above, which `dxvk_bridge.cpp` then
`#include`s instead of defining).

### 4.2 The two properties that MUST survive the move

**P1 — `format.rs` stays free of `windows`-crate and WDK types.** That property is what lets
`tools/format-table-check.rs` `#[path]`-include it and run the equivalence test **on the Linux host
in seconds**, over every format number `0..=200`, against verbatim copies of the eight pre-change
`match` bodies (`format.rs:18-27`; `tools/format-table-check.rs:1-25`). `helios_umd` is a
`panic="abort"` cdylib with no test harness, so this is the only real test the format table has.

Consequences for `umd_common/Cargo.toml`:

```toml
[lib]
crate-type = ["rlib"]

# `windows` is needed ONLY by `slot` (windows::core::Interface). Gate it so the
# crate configures and builds on Linux — tools/format-table-check.rs and any
# future host-side test depend on that.
[target.'cfg(windows)'.dependencies]
windows = { version = "0.58", features = ["Win32_Foundation", "Win32_Graphics_Direct3D11"] }
```

and in `umd_common/src/lib.rs`, `#[cfg(windows)] pub mod slot;`, `#[cfg(windows)] pub mod log;`,
`#[cfg(windows)] pub mod knobs;`, `#[cfg(windows)] pub mod noop;` — `hr`, `format`, `throttle` and
`window` stay unconditional. `knobs` must be gated because it carries
`#[link(name = "advapi32")] unsafe extern "system" { fn RegGetValueA(...) }`
(`umd/src/knobs.rs:60-71`).

⚠ Verification after the move: `rustc --test --edition 2021 -o /tmp/format-table-check
tools/format-table-check.rs && /tmp/format-table-check` still passes. Update the `#[path]` in that
file to `../umd_common/src/format.rs` in the same commit. `--edition 2021` matters — without it the
captured-identifier `assert!` messages are silently not format strings
(`tools/format-table-check.rs:13-14`).

**P2 — the `log_line` compile-error guard.** `umd/src/log.rs:45` marks `log_line` `#[deprecated]`
purely as an internal marker, and `umd/src/lib.rs:16` sets `#![deny(deprecated)]`, so a new per-op
site that reaches the unconditional writer **does not compile**. Only `trace_line!` and `log_error!`
may reach it, each wrapping the call in `#[allow(deprecated)]` (`log.rs:158`, `:173`; the guarantee
is recorded at `lib.rs:8-15` and was verified by fault injection).

Preserving it across the move needs four things, all easy to get wrong:

1. `umd_common::log::log_line` stays `#[deprecated]` and becomes `#[doc(hidden)] pub` — moving from
   `pub(crate)` to `pub` widens the writer surface, and `#[doc(hidden)]` plus the deprecation is what
   keeps it from reading as API.
2. **Each cdylib keeps its own `#![deny(deprecated)]`** in its `lib.rs`. A lint level in the rlib
   does not propagate to consumers.
3. `trace_line!`/`log_error!` become `#[macro_export]` and must call `$crate::log::log_line`, still
   inside `#[allow(deprecated)]`. Because `#[macro_export]` places them at the *root* of
   `helios_umd_common`, each cdylib re-exports them (`pub(crate) use helios_umd_common::{log_error,
   trace_line};`) so the ~hundreds of existing `crate::log_error!` call sites keep resolving —
   `umd/src/lib.rs:83` already re-exports them that way.
4. **Re-run the fault injection** in the commit that moves it: add a bare `crate::log_line("x")` to
   a `forward/*` file and confirm the build fails with *"use of deprecated function"*, then remove
   it. A guard nobody re-tested after a refactor is not a guard.

The basename parameterisation (the one behavioural change):

```rust
// umd_common::log
static BASENAME: OnceLock<&'static str> = OnceLock::new();

/// Call ONCE per process from the DLL's OpenAdapter* before any log write.
/// D3D11 passes "umd", D3D12 passes "umd12" — the two DLLs must not share a
/// file handle (R11 §6-H6: `log.rs:83-93` keeps ONE process-lifetime handle,
/// and a second DLL opening the same path is a second handle to one file).
pub fn init(basename: &'static str) { let _ = BASENAME.set(basename); }

pub fn umd_log_path() -> &'static Path { /* …join(format!("{}-{}.log", BASENAME.get().unwrap_or(&"umd"), process::id())) */ }
```

⚠ `close_at_detach` must keep reading the `OnceLock` with `get()`, never `get_or_init()`, so it
cannot be the call that *creates* the handle it exists to close (`umd/src/log.rs:143-151`), and must
keep `try_lock` + `LOG_CLOSE_CONTENDED` because DllMain runs under the loader lock (`:117-141`).

### 4.3 What the move must NOT change

`log_knob_inventory()`'s output line is R1008's own validation instrument (`umd/src/log.rs:226-234`).
After the `log`/`knobs` move, the line in `umd-<pid>.log` for the **D3D11** DLL must be
**byte-identical** to before. That is the S2 pass criterion (§11).

### 4.4 The shared C++ header — `umd_common` is not Rust-only

DECISIONS D3b's rule is *"⛔ Copy-paste between `umd` and `umd12` is a defect, not a shortcut"*, and
it applies to the bridge C++ exactly as it applies to the Rust. The D3D12 bridge needs `umd_log`,
`bridge_log_budget`, `PeriodicStat`, `qpc_elapsed_us`, `ComRelease<T>` and `bridge_guard` — every
one of which already exists on the D3D11 side and none of which mentions DXVK.

Concretely:

```
umd_common/
├── Cargo.toml
├── src/            # hr, format, log, knobs, throttle, refusals, slot, noop, window
└── bridge/
    └── bridge_common.h        # MOVED from umd/bridge/bridge_common.h, then extended
```

`umd_common/bridge/bridge_common.h` contains, in this order:

1. `umd_log(const char*)` and `bridge_log_budget(...)` verbatim from `umd/bridge/bridge_common.h`
   (29 L), keeping its stated property — **no DXVK / COM / Vulkan / WDK include** (`:8-11`). The
   *implementation* of `umd_log` stays per-DLL, because each DLL logs to its own file: the header
   declares it, `umd/bridge/dxvk_bridge.cpp:257` and `umd12/bridge/vkd3d_bridge.cpp` each define it.
2. `PeriodicStat` (`dxvk_bridge.cpp:134`), `qpc_elapsed_us` (`:174`), `ComRelease<T>` (`:187`) —
   moved out of `dxvk_bridge.cpp`, which then includes them.
3. **`bridge_guard`, with its `static_assert` (§7.2)**, parameterised on the exception types it
   catches so the DXVK arm keeps `catch (const dxvk::DxvkError&)` without that type entering the
   shared header:

```cpp
// umd_common/bridge/bridge_common.h  — ONE definition, both bridges.
template <typename R, typename Fn>
R bridge_guard(const char* what, R on_error, Fn&& fn) noexcept {
  static_assert(std::is_same_v<R, decltype(fn())>,
                "bridge_guard's error value must have the guarded body's exact "
                "return type; otherwise the success path is converted too");
  try { return fn(); }
  catch (const std::exception& e) { /* fixed char[] + snprintf, never std::string */ }
  catch (...) { /* fixed char[] + snprintf */ }
  return on_error;
}
```

The DXVK-specific `catch (const dxvk::DxvkError&)` arm becomes a thin `dxvk_bridge.cpp`-local
wrapper that calls the shared template from inside its own `try` — the engine type never reaches the
shared header, and there is still exactly **one** `static_assert` in the tree.

Build wiring: `umd_common` has **no `build.rs`** (§4), so it compiles no C++ itself. Each cdylib's
`build.rs` adds `.include("../umd_common/bridge")` to its `cxx_build` invocation — one line in
`umd/build.rs` beside the existing `.include("bridge")` (`umd/build.rs:183-215`), and the same line
in `umd12/build.rs` (§8.1(b)). The header is a build input of both, so both rebuild when it changes.

⚠ This move lands in **S1** (it is zero-behaviour-change, like `hr`/`format`), and its proof is the
same as S1's: the D3D11 DLL builds, deploys, and one Fire Strike run at the standard preset shows no
regression. `git grep -n 'static_assert' umd/bridge umd12/bridge umd_common/bridge` must return
exactly one `bridge_guard` hit afterwards — that grep is the S1 check for this row.

---

## 5. The `umd12` crate layout

```
umd12/
├── Cargo.toml                       # cdylib; deps: cxx, helios_protocol, helios_umd_common, windows
├── build.rs                         # bindgen d3d12umddi + cxx_build for the vkd3d bridge  (§8.1)
├── bindgen/
│   └── d3d12umddi_wrapper.h         # windows.h + NTSTATUS typedef + <d3d12umddi.h>
├── bridge/
│   ├── vkd3d_bridge.h               # pimpl'd HeliosVkd3dDevice — the complete type cxx needs
│   └── vkd3d_bridge.cpp             # helios_vkd3d_create_device + the guarded methods;
│                                    #   DEFINES this DLL's umd_log, and #includes the SHARED
│                                    #   ../../umd_common/bridge/bridge_common.h (§4.4) for
│                                    #   bridge_log_budget / PeriodicStat / qpc_elapsed_us /
│                                    #   ComRelease<T> / bridge_guard. ⛔ No bridge_common12.h:
│                                    #   a second copy of that header is DECISIONS D3b's
│                                    #   copy-paste defect, and a second bridge_guard without
│                                    #   the static_assert is how ead692e comes back (§7.2).
└── src/
    ├── lib.rs
    ├── ddi12.rs
    ├── adapter12.rs
    ├── caps12.rs
    ├── device12.rs
    ├── bridge12.rs
    ├── knobs12.rs
    └── forward12/                   # split from day one — see §12 trap 8
        ├── mod.rs
        ├── tables12.rs
        ├── queue.rs
        ├── cmdlist.rs
        ├── resource12.rs
        ├── descriptors.rs
        ├── pso.rs
        └── fence.rs
```

| File | Contains | Modelled on | Why |
|---|---|---|---|
| `src/lib.rs` | module list, `DllMain` releasing this DLL's process-lifetime handles, `#![deny(deprecated)]`, `#[cfg(not(windows))] compile_error!`, re-exports of `helios_umd_common`'s macros | `umd/src/lib.rs` (91 L) verbatim apart from the module list | `helios_umd12.dll` is loaded/unloaded per device too (UNVERIFIED-5); a never-freed static is a per-device leak (§12 trap 5) |
| `src/ddi12.rs` | `include!($OUT_DIR/d3d12umddi.rs)` + the four `#![allow(non_*)]`, plus hand-pinned `offset_of!` asserts for **only** the structs read positionally (start with `D3D12DDIARG_CREATEDEVICE_0109` and `D3D12DDIARG_OPENADAPTER`) | `umd/src/ddi.rs` (80 L), whose `abi_offsets` doc (`:18-49`) explains why bindgen's own asserts are *not* a substitute | bindgen derives struct and assertion from the same header, so they are self-consistent by construction; hand-pinned offsets catch the ABI *moving* |
| `src/adapter12.rs` | `OpenAdapter12`, the `UmdD3D12` knob gate, `AdapterToken`, `adapter_ok`, `calc_private_device_size`, `create_device`, `close_adapter`, `get_supported_versions`, `get_optional_ddi_tables`, `fill_ddi_table`, `DeviceUnderConstruction` | `umd/src/adapter.rs` (608 L) | it is the same eight-step shape; the guard and the closed-set version enum are the two things that must not be reinvented (§12 traps 2, 3) |
| `src/caps12.rs` | one `D3D12FeatureProfile` struct + the `pfnGetCaps` dispatch over the live `D3D12DDICAPS_TYPE` set — **43 enumerators** at `d3d12umddi.h:94-150`, all of them in that one enum (§1.2 step 8; the value-by-value list is `DDI_REFERENCE.md` §11.1, the must-answer subset §11.2, the 14 tiered enums §11.4, the runtime's cross-check rules §11.5, and the three to pin conservatively from commit 1 §11.6) — with `const _: () = assert!(…)` coherence checks between tiers | `umd/src/caps.rs` (266 L) — **discipline only** | H4: `D3D12Core.dll`'s own strings enforce ~60 cross-tier consistency rules; advertising an unbacked tier is a lie the OS acts on (DECISIONS §7.8) |
| `src/device12.rs` | `HeliosD3D12Device` (the private block), `device_private_size()`, the `pfnFillDDITable` dispatch, per-queue `RuntimeContext` creation, `DestroyDevice` | `umd/src/device_funcs.rs` (1318 L) | same runtime-allocated-private-block model (R2 §1.1); context creation is per **queue** not per device (R2 §1.3) |
| `src/bridge12.rs` | `#[cxx::bridge] mod ffi` for `HeliosVkd3dDevice`, the owned/borrowed COM discipline, a sealed `BridgeDevice12` newtype with no `Deref` and a private `inner` | `umd/src/bridge.rs` (791 L), especially `:251-307` and `:386-405` | cxx generates raw methods as **inherent** methods on the public opaque type; module privacy does not seal them — only a newtype without `Deref` does (R815) |
| `src/knobs12.rs` | `UMD_D3D12` (§10) + any `umd12`-only knobs, and its own `resolved_inventory()` | `umd/src/knobs.rs:155-290` | one `OnceLock` per knob per module; the two DLLs deliberately have independent caches (R11 §6-H7) |
| `src/forward12/tables12.rs` | the installers for `D3D12DDI_DEVICE_FUNCS_CORE_*`, `D3D12DDI_COMMAND_LIST_FUNCS_3D_*`, `D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001` with `#[must_use]` `Filled*` tokens | `umd/src/forward/tables.rs:44-70` | install order must be structural, not textual (§12 trap 9) |
| `src/forward12/queue.rs` | `pfnCalcPrivateCommandQueueSize` / `pfnCreateCommandQueue` / `pfnDestroyCommandQueue` / `pfnExecuteCommandLists` | `umd/src/forward/deferred.rs` | the WDDM context is minted here (R2 §1.3). The three device-side entry points are **members 27, 28 and 29** of the 124 in `D3D12DDI_DEVICE_FUNCS_CORE_0109` (zero-based indices 26-28), at `d3d12umddi.h:13488-13490` — ⛔ **not** "slots 38-40", which was a raw line offset inside the struct misread as a member index and would make a size-derived table installer write into the wrong three slots (DECISIONS §4.1). `pfnExecuteCommandLists` is on the separate `D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001`, `:2729-2738` (7 members, 2 of them `pfnUnused`) |
| `src/forward12/cmdlist.rs` | the **75**-slot 3D command-list table (`D3D12DDI_COMMAND_LIST_FUNCS_3D_0108`, `d3d12umddi.h:13303-13388`) incl. `PFND3D12DDI_PRESENT_0051` (`:7250`) | `umd/src/forward/pipeline.rs` + `deferred.rs` | the recording surface forwards ~1:1 into `ID3D12GraphicsCommandList` (H3). ⛔ For the present identity: use `pfnRenderCb` off `pKTCallbacks` (§2.2 `present.rs` row, DECISIONS §3-H2 P-C) — **do not** design a `DxgkDdiSubmitCommandVirtual` decode for it. That DDI runs at **DISPATCH_LEVEL** (`kmd_render/src/ddi/submit_command.rs:723-724`), where the stash machinery's `diag::record*` calls are illegal, and it would add a fourth KMD item that DECISIONS D5 does not have |
| `src/forward12/resource12.rs` | `pfnCreateHeapAndResource` (both arg pointers independently nullable), GPU-VA reserve/map | `umd/src/forward/resource.rs` + `alloc.rs` | H3 |
| `src/forward12/descriptors.rs` | descriptor heaps: return **vkd3d's own handle values and stride verbatim** | — (new) | H3's good surprise: `D3D12DDI_CPU_DESCRIPTOR_HANDLE{SIZE_T}` / `_GPU_{UINT64}` are opaque driver-chosen scalars and `pfnGetDescriptorSizeInBytes` lets the driver pick the stride ⇒ **no shadow table**. ⚠ they are returned **by value** while vkd3d's C impl returns via hidden pointer — that is the `ead692e` truncation class (§7.2) |
| `src/forward12/pso.rs` | root-signature re-serialization — through the **second** Helios export, `helios_vkd3d_serialize_root_signature` (D4, §7.4), because `vkd3d_serialize_root_signature` (`include/vkd3d.h:129`) is not exported from any vkd3d DLL — and PSO handle-bundle reassembly | `umd/src/forward/state_objects.rs` + `layout.rs` | H3: root sigs arrive **parsed** as `D3D12DDI_ROOT_SIGNATURE`; PSOs arrive as handle bundles |
| `src/forward12/fence.rs` | `pfnSignalFence` / `pfnWaitForFence` over monitored fences | `umd/src/forward/present.rs` fence sites | DECISIONS §6 downgraded the monitored-fence risk to MEDIUM; residual probe is G-fence |

`umd12/Cargo.toml` mirrors `umd/Cargo.toml` exactly:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
cxx = "1"
helios_protocol   = { path = "../protocol" }
helios_umd_common = { path = "../umd_common" }
# `Win32_Graphics_Direct3D_Fxc` is deliberately ABSENT, for the same reason as
# umd/Cargo.toml:13-16: it pulls D3DCompile — an HLSL runtime compiler — into a
# shipped display-driver DLL. Re-adding it must be justified in the commit.
windows = { version = "0.58", features = ["Win32_Foundation", "Win32_Graphics_Direct3D", "Win32_Graphics_Dxgi_Common"] }

[build-dependencies]
cxx-build = "1"
bindgen = "0.70"

[profile.dev]
panic = "abort"
opt-level = 1

[profile.release]
panic = "abort"
lto = "thin"
opt-level = 2
debug = "full"   # dump_syms + minidump-stackwalk need a GUID-matched PDB
```

---

## 6. The export-surface constraint

**This is the part that bites.** Three by-name export surfaces cross module boundaries in this
process, and they resolve by three *different* rules. Getting this wrong is silent.

| Export set | Resolver | Mechanism | `umd12` |
|---|---|---|---|
| `helios_umd_set_present_source`, `helios_umd_wait_last_present`, `helios_umd_get_present_result` (`umd/src/vehicle_exports.rs:25`, `:52`, `:73` — the `#[no_mangle]` attributes are the lines above, `:24`/`:51`/`:72`; `PRESENT.md:47` cites `:25-43` for the first of them) | **Mesa ICD WSI** | `K32EnumProcessModules` over **ALL** loaded modules, **first hit wins** — `icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:711-732`, consumed at `:876-886` | ⛔ **MUST NOT export these names** |
| `helios_scanout_acquire_enabled`, `helios_scanout_ledger_lookup_v2`, `helios_scanout_ledger_snapshot_v2` (`umd/src/scanout_acquire.rs:611`, `:620`, `:683`) | **DXVK**, statically linked into `helios_umd.dll` | `GetModuleHandleExW(FROM_ADDRESS \| UNCHANGED_REFCOUNT, &umdExports, &mod)` — **its own** module — `dxvk-helios/src/dxvk/dxvk_helios_scanout_acquire.cpp:43-49` | safe to duplicate, but **omit them**: vkd3d has no consumer, and the DXVK side is all-or-nothing (*"a partial surface is a build skew, not a mode"*, same file) |
| `OpenAdapter10`, `OpenAdapter10_2`, `OpenAdapter12`, `DllMain` | **the D3D runtime loader** | by name from the DLL named in `UserModeDriverName[n]` | `umd12` exports **`OpenAdapter12` and `DllMain` only** |

### 6.1 The ICD lookup, verbatim — why it is a hard constraint

```c
/* icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:711-732 */
static void *
wsi_win32_vehicle_find_umd_export(const char *name)
{
   ...
   HMODULE modules[1024];
   DWORD needed = 0;
   if (!enum_modules(GetCurrentProcess(), modules, sizeof(modules), &needed))
      return NULL;

   DWORD count = MIN2(needed / sizeof(HMODULE), ARRAY_SIZE(modules));
   for (DWORD i = 0; i < count; i++) {
      void *fn = (void *)GetProcAddress(modules[i], name);
      if (fn)
         return fn;                    /* <- FIRST HIT WINS, module order is the loader's */
   }
   return NULL;
}
```

The lookup walks modules rather than naming a DLL because the hotplug path installs by **content
hash** — `helios_umd_<first 16 hex of sha256>.dll` (`tools/hotplug-helios-umd.ps1:51`), so there is
no stable name to ask for. If `helios_umd12.dll` also exported
`helios_umd_set_present_source`, whichever module the loader enumerated first would silently own the
vehicle's present hand-off, and the two DLLs' per-thread present state is *not* interchangeable.

⛔ **Rule: exactly one DLL exports the `helios_umd_*` names, and it is `helios_umd.dll`.** The file
header already states the failure mode: *"⚠ ALL THREE MUST KEEP EXISTING … A UMD-only deploy that
drops one kills the vehicle"* (`umd/src/vehicle_exports.rs:8-11`) — the ICD increments
`helios_vehicle_export_miss` and fails the whole dcomp vehicle with `E_NOINTERFACE`
(`wsi_common_win32.cpp:882-885`).

### 6.2 `OpenAdapter12` moves, it is not duplicated

At S5 (§11), in **one commit**: `umd/src/adapter.rs:177-189` deletes its `OpenAdapter12`, the INF
points slot 3 at `helios_umd12.dll`, and `umd12`'s `OpenAdapter12` becomes reachable and stops
refusing. Two DLLs exporting `OpenAdapter12` is not itself a conflict (the runtime resolves from the
DLL it loaded for that slot) but it makes "which one answered?" unanswerable from a log —
`log_self_module_path()` (`umd/src/log.rs:187`) is the only instrument, and running it in two
modules writing two files is exactly the confusion this rule avoids.

⚠ Pre-existing and orthogonal: `helios_umd.dll` exports **no** `OpenAdapter` (D3D9) yet slot 0 names
it, so a D3D9 client would `LoadLibrary` us and fail at `GetProcAddress` (R11 §1.4, from
`dumpbin /exports` on the deployed DLL). Do not repeat that in reverse — never name a DLL in a slot
whose entry point it does not export.

### 6.3 cxx's own leaked exports

`dumpbin /exports helios_umd.dll` shows `DllMain`, the three `OpenAdapter*`, and then *hundreds* of
`cxxbridge1$rust_vec$*` symbols — cxx's public ABI leaking out of the cdylib (R11 §1.4). Both DLLs
will carry these. They are namespaced by cxx and no external resolver asks for them, so they are
noise, not a collision — but they are why `dumpbin /exports` output on a UMD is long, and why a
"the export list looks wrong" reaction is usually wrong.

### 6.4 ⚠ The ICD anchor resolver, and why duplicating it is not free

`umd/bridge/bridge_icd_exports.cpp:38-67` finds the venus ICD by walking `TH32CS_SNAPMODULE` and
testing `GetProcAddress(module, "helios_venus_memory_alloc_info")`. Its own comment (`:49-51`):
*"Looking up each export independently can mix two ICD versions and call a function with a foreign
`VkDeviceMemory`/`VkInstance` handle."* If `helios_umd12.dll` carries its own copy of that resolver,
the two copies can independently select **different** ICD modules in one process.

**Decision, taken here rather than deferred (R11 §9.6 offered two options; this is the pick):**
option **(b) — the anchor selection becomes process-global**, and it is implemented with the
mechanism already in the tree rather than a new one.

*Rejected option (a)* — "`umd12` resolves the ICD through a C-ABI export on `helios_umd.dll` when
that module is present" — because of the "when present" clause: a pure-D3D12 process never loads
`helios_umd.dll`, so (a) needs a fallback path that is exactly the divergent per-module resolver it
was meant to remove, and the divergence then only appears in the mixed process, i.e. the case
nobody tests.

*The shape of (b), concretely.* Both DLLs export **one** new name:

```c
/* umd_common/bridge/bridge_common.h + one definition per cdylib.
 * Publishes/queries the process's single canonical venus-ICD module.
 * Returns the canonical HMODULE; `candidate` is this module's own resolution. */
extern "C" __declspec(dllexport)
void* helios_icd_anchor_v1(void* candidate);
```

Every `resolve_helios_icd_module` (`umd/bridge/bridge_icd_exports.cpp:269-287`) gains one step after
its existing walk: find `helios_icd_anchor_v1` with the **same** `K32EnumProcessModules`
first-hit-wins walk the ICD already uses (`icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:711-732`,
quoted in §6.1) and call it. Because both DLLs export the name, the first module in loader order
becomes the single publisher for the whole process, whichever UMD that is — and a lone `umd12`
finds its own export and is trivially self-consistent. No new OS primitive, no load-order
dependency between the two UMDs, and it reuses machinery that is already proven in this process.

⛔ The name is deliberately **not** in the `helios_umd_*` family: §6.1's rule is that exactly one
DLL exports those, because the ICD looks *them* up. The ICD never looks up `helios_icd_anchor_v1`,
so both DLLs exporting it is the mechanism, not a collision.

*Failure is loud.* If the published anchor is a different `HMODULE` from this module's own
candidate, the resolver **refuses device creation** (`E_FAIL` out of the bridge,
`DXGI_ERROR_UNSUPPORTED` out of `OpenAdapter12`) and increments a named counter
**`IcdAnchorMismatch`**, logged
through `log_error!` on its first hit exactly like `DdiRefusals` (§4.1 `refusals` row). Two ICD
builds in one process is a silent-wrong-answer class — foreign `VkDeviceMemory`/`VkInstance`
handles — so it must not degrade quietly.

*Scheduled:* **stage S4b** (§11), immediately after S4 and **before** the first two-engine run. Its
proof is UNVERIFIED-4's probe (§13), which becomes the S4b pass criterion rather than a detector for
a hazard nobody owns: both modules must report the same ICD path, both venus context ids must be
non-zero and equal, and `IcdAnchorMismatch` must read 0.

Related, and pre-existing in the ICD: `helios_venus_instance_ctx_id(VkInstance)` ignores its
argument and returns a `_Thread_local`
(`icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:639-644`, `:540-546`). **A vkd3d-based engine that
creates its `VkInstance` on one thread and asks for the context id on another gets 0 or the wrong
answer.** vkd3d creates its instance on the calling thread of `vkd3d_create_instance`, so the bridge
must read the ctx id on that same thread, synchronously — exactly what
`dxvk_bridge.cpp:1741` does (`read_instance_venus_context_id` immediately after
`new DxvkInstance`).

---

## 7. The vkd3d bridge

### 7.1 The cxx contract

Compilation (`umd/build.rs:183-215`, translated for `umd12`):

```rust
let mut build = cxx_build::bridge("src/bridge12.rs");
build.file("bridge/vkd3d_bridge.cpp")
     .compiler(&clang_cl)             // C:\Program Files\LLVM\bin\clang-cl.exe
     .archiver(&archiver)             // C:\Program Files\LLVM\bin\llvm-lib.exe
     .std("c++17")
     .flag("/EHsc")                   // cxx-build disables exceptions by default
     .include("bridge")
     .include("../umd_common/bridge")  // the SHARED bridge_common.h (§4.4)
     .include(format!(r"{vkd3d_src}\include"))
     .include(format!(r"{vkd3d_src}\khronos\Vulkan-Headers\include"))
     .define("NOMINMAX", None).define("WIN32_LEAN_AND_MEAN", None)
     .define("_WIN32_WINNT", "0x0A00").define("_CRT_SECURE_NO_WARNINGS", None);
build.compile("helios_vkd3d_bridge");
```

**The pimpl rule** (`umd/bridge/dxvk_bridge.h:1-10`, verbatim): cxx's generated glue manages
`std::unique_ptr<HeliosVkd3dDevice>` and therefore needs that type **complete** in the header. Keep
the engine headers out of the header — and out of the generated glue — by pimpl: a thin complete
shell holding `std::unique_ptr<HeliosVkd3dDeviceImpl>`, destructor **declared** in the header and
**defined out-of-line** in the `.cpp` where `Impl` is complete.

⚠ For vkd3d the pimpl is not merely tidy: `vkd3d-proton-helios/include/vkd3d.h:43-49` does
`#include <vulkan/vulkan.h>` (plus `private/vulkan_private_extensions.h`, and it `#define`s
`VK_USE_PLATFORM_WIN32_KHR` on `_WIN32`) unless `VKD3D_NO_VULKAN_H` is defined — `:102-109` is the
public *function*
declaration block this document cites elsewhere as `:104` and `:110`. It also pulls
`D3D12_*`/`ID3D12*` types from vkd3d's own widl-generated headers, not the Windows SDK's (R3 §8.3).
Neither belongs in a header the cxx glue compiles.

**Owned vs borrowed COM pointers.** The C++ side returns COM pointers as bare `std::size_t`.
`umd/src/bridge.rs:251-270` records the discipline that keeps that safe:

> Thirteen bridge methods return a COM pointer as a bare `usize`. **Two are BORROWED** — the bridge
> keeps the owning reference — **and eleven are OWNED** and the Rust side must `Release`. … adopting
> a borrowed pointer → a double release … wrapping an owned pointer in `ManuallyDrop` → a leak. Each
> surfaces as a much later crash in dwm.

Three layers make the wrong adoption unreachable, and `bridge12.rs` must reproduce all three:

1. **one** `from_raw` per owning entry point — `adopt_resource(raw) -> Option<T>`
   (`umd/src/bridge.rs:284-286`);
2. borrowed getters return `ManuallyDrop<T>` (`:294-307`);
3. a sealed newtype (`BridgeDevice`, `:446-450`) with **no `Deref`** and a private `inner`, because
   *"cxx generates the raw methods as INHERENT methods on the public opaque type, and inherent
   methods of a re-exported public type stay callable regardless of module visibility"*
   (`:391-405`, R815).

Plus the transposition-proof newtypes where two same-typed operands neighbour each other:
`SrcRes`/`DstRes` (`:423`, `:427`) exist because `present_vehicle_copy(dst, src)` and
`present_sync_publish(src, dst)` took their operands in opposite orders ~30 lines apart and a
transposition compiled cleanly on both sides of the FFI (R816). D3D12 has the same shape in
`CopyResource`/`CopyBufferRegion` and in every `D3D12DDI_*_DESCRIPTOR_HANDLE` pair.

### 7.2 ⛔ `bridge_guard` and the `static_assert` that must not be omitted

Every fallible bridge body is wrapped. Verbatim, `umd/bridge/dxvk_bridge.cpp:304-324`:

```cpp
  template <typename R, typename Fn>
  R bridge_guard(const char* what, R on_error, Fn&& fn) noexcept {
    static_assert(std::is_same_v<R, decltype(fn())>,
                  "bridge_guard's error value must have the guarded body's exact "
                  "return type; otherwise the success path is converted too");
    try {
      return fn();
    } catch (const dxvk::DxvkError&) {
      char msg[160];
      std::snprintf(msg, sizeof(msg), "%s: DxvkError", what);
      umd_log(msg);
    } catch (const std::exception& e) {
      char msg[256];
      std::snprintf(msg, sizeof(msg), "%s: exception: %s", what, e.what());
      umd_log(msg);
    } catch (...) {
      char msg[160];
      std::snprintf(msg, sizeof(msg), "%s: unknown exception", what);
      umd_log(msg);
    }
    return on_error;
  }
```

Why it exists at all: **cxx emits every generated C++ shim `noexcept`**, so an exception escaping a
bridge method is `std::terminate` — dwm.exe dies instead of the DDI returning a failure
(`dxvk_bridge.cpp:271-280`). The catch arms must not allocate: a `std::string` built inside a
`std::bad_alloc` handler can throw again — hence fixed `char[]` + `snprintf`, and hence
`DxvkError::message()` (returns `std::string`) is deliberately not called.

**THE RECORDED BUG — commit `ead692e`**, *"umd/bridge: BUG — bridge_guard truncated every returned
pointer to 32 bits"*, 2026-07-28:

> `R` is deduced from `on_error` ALONE — the guarded body's return type is not a deduction context.
> Four call sites passed the bare literal `0` against a body returning `std::size_t`, so `R` deduced
> to `int` and `return fn();` narrowed every **SUCCESS** value, not only the error value.

Symptom: dwm.exe and LogonUI.exe crash-looping at cold boot, `0xc0000005` at a constant
`Fault offset: 0x8068c` under `D3D11CommonContext::VSSetShader`. From the box's own logs:

```
T6 (renders):  create_vertex_shader_11_1 ok: raw=0x1cd520fc300 len=4600
T7 (crashes):  create_vertex_shader_11_1 ok: raw=0x7bdb1800   len=4600
```

Nothing warned: `-Wconversion`/`-Wshorten-64-to-32` are off and a UMD build already emits ~115 clang
warnings. The fix is the `static_assert`, not the four `std::size_t(0)`s.

⛔ **Rules for `umd12/bridge/vkd3d_bridge.cpp`:** any `bridge_guard`-equivalent carries the same
`static_assert`; do not invent a second guard template without it; and the D3D12 descriptor-handle
DDIs are the highest-risk site of this class, because they return `D3D12DDI_CPU_DESCRIPTOR_HANDLE`
(`SIZE_T`) / `D3D12DDI_GPU_DESCRIPTOR_HANDLE` (`UINT64`) **by value** while vkd3d's C implementation
returns them via a hidden pointer (H3, R2 §4.4).

### 7.3 `HeliosVkd3dDevice` — the concrete shape

`umd12/bridge/vkd3d_bridge.h`:

```cpp
#pragma once
#include <cstdint>
#include <memory>

// Owns the vkd3d instance/device + the ID3D12Device COM pointer; defined in
// vkd3d_bridge.cpp. Pimpl so no vkd3d/Vulkan header reaches cxx's glue.
struct HeliosVkd3dDeviceImpl;

struct HeliosVkd3dDevice {
  HeliosVkd3dDevice() noexcept;
  ~HeliosVkd3dDevice();
  HeliosVkd3dDevice(const HeliosVkd3dDevice&) = delete;
  HeliosVkd3dDevice& operator=(const HeliosVkd3dDevice&) = delete;

  std::unique_ptr<HeliosVkd3dDeviceImpl> impl;

  // BORROWED — the bridge keeps the owning reference. Rust wraps in ManuallyDrop.
  std::size_t   d3d12_device_ptr() const;
  std::uint32_t venus_context_id() const;
};

// Create a vkd3d instance + ID3D12Device bound to the Helios adapter LUID.
// Returns a null unique_ptr on failure. Matches the cxx signature in src/bridge12.rs.
std::unique_ptr<HeliosVkd3dDevice> helios_vkd3d_create_device(
    std::uint32_t luid_low,
    std::int32_t  luid_high);
```

`umd12/bridge/vkd3d_bridge.cpp` (the `Impl`, modelled on `dxvk_bridge.cpp:357-393`):

```cpp
struct HeliosVkd3dDeviceImpl {
  HMODULE          vkd3d      = nullptr;          // helios_vkd3d.dll, LoadLibrary'd
  ID3D12Device*    d3d12      = nullptr;          // one owned reference
  std::uint32_t    venus_ctx_id = 0;              // read on the CREATING thread (§6.4)

  ~HeliosVkd3dDeviceImpl() {
    if (d3d12) d3d12->Release();
    // ⚠ do NOT FreeLibrary here: the D3D12 UMD is unloaded per device (UNVERIFIED-5)
    // and helios_vkd3d.dll (the d3d12core target) has process-global once-init
    // state — `library_once` at libs/d3d12core/main.c:52, driven by
    // load_modules_once (:319-366), which loads the Vulkan module and any
    // OpenVR/OpenXR modules exactly once per process. (The analogous once-init in
    // the thin d3d12.dll target is libs/d3d12/main.c:52,112-141, but Helios never
    // loads that DLL.) Pin instead — GetModuleHandleExW(FROM_ADDRESS | PIN) — the
    // way the ICD's helios_pin_module does at vn_renderer_helios.c:3858-3859, and
    // count any pin refusal the way it counts helios_module_pin_failures (:522-524).
  }
};
```

and the entry point, in `bridge_guard` and mirroring
`helios_dxvk_create_device` (`dxvk_bridge.cpp:1666-1773`) step for step:

1. `std::call_once` for any process-global env configuration (`_putenv_s` is **not** safe against a
   concurrent `getenv`, and one process makes several devices — `dxvk_bridge.cpp:1680-1708`). The
   D3D12 analogues are `VKD3D_CONFIG`, `VKD3D_DEBUG`, and — mandatory before the first bring-up run
   — `VKD3D_LOG_FILE` pointed at `C:\ProgramData\Helios\umd12-<pid>-vkd3d.log`, because vkd3d
   defaults to **stderr** (`vkd3d-proton-helios/libs/vkd3d-common/debug.c:110-114`) and stderr is a
   black hole in `dwm.exe` (R11 §6-H5; the same trap `dxvk-helios/src/util/log/log.cpp:135-140`
   records).
2. resolve `helios_vkd3d.dll` (§7.4) and `GetProcAddress` **both** Helios exports —
   `helios_vkd3d_create_device` and `helios_vkd3d_serialize_root_signature` (D4). Resolve both up
   front and fail the whole create if either is missing: a device that comes up and then cannot
   serialize a root signature is the succeed-then-fail shape DECISIONS §7.7 forbids.
3. call it with `{luid_low, luid_high}` and `IID_ID3D12Device`.
4. read the venus context id on **this** thread (§6.4).
5. return the `unique_ptr`; a failure returns `nullptr` and `BridgeDevice12::create` folds the null
   check into construction so a `BridgeDevice12` that exists is always usable
   (`umd/src/bridge.rs:451-458`).

### 7.4 ✅ The change required in `vkd3d-proton-helios` — LANDED 2026-08-05 (`fc35d37d`)

**Status:** on branch `helios` of `github.com/rupansh/vkd3d-proton`, exactly as specified below:
`libs/d3d12core/helios_entry.c`, `libs/d3d12core/helios_vkd3d.def`, and the
`shared_library('helios_vkd3d', …)` target appended to `libs/d3d12core/meson.build`. Verified:

```
$ x86_64-w64-mingw32-objdump -p tmp/dx12/build/vkd3d-win64/libs/d3d12core/helios_vkd3d.dll
    [ 0] D3D12GetInterface   [ 1] D3D12SDKVersion
    [ 2] helios_vkd3d_create_device   [ 3] helios_vkd3d_serialize_root_signature
```

and both are exercised end to end by `D12-G1` (`tools/d3d12_bridge_probe.cpp`, 28 steps, 0 failures).

⛔ **One correction to the sketch below, and it is load-bearing:** `include/vkd3d.h:68` claims that a
NULL `pfn_vkGetInstanceProcAddr` makes libvkd3d load libvulkan itself. **It does not** —
`vkd3d_init_vk_global_procs` (`libs/vkd3d/device.c:461-468`) returns `E_INVALIDARG` for NULL. The
`...` in the sketch below therefore has to include the module load: `helios_entry.c` mirrors
`load_modules_once` (`libs/d3d12core/main.c:319-364`) — `LoadLibraryA("winevulkan.dll")` then
`"vulkan-1.dll"`, `GetProcAddress("vkGetInstanceProcAddr")`, behind a `pthread_once` — minus the
wineopenxr/openvr half, which only supplies VR instance extensions a display driver has no use for.
The instance/device extension lists are otherwise copied verbatim from `d3d12core`
(`main.c:574-593`, `:659-670`) so the device this export builds is configured identically to the one
vkd3d's own conformance suite builds; that equivalence is what makes G1 predictive of G9.

---

**vkd3d exports nothing usable today.** `libs/d3d12core/d3d12core.def` is exactly:

```
LIBRARY d3d12core.dll

EXPORTS
    D3D12GetInterface
    D3D12SDKVersion DATA PRIVATE
```

and `libs/vkd3d` is a **static** library (`libs/vkd3d/meson.build:119-121`,
`static_library('vkd3d-proton', …)`). The public creation API is declared in
`include/vkd3d.h` — `vkd3d_create_instance` (`:104`), `vkd3d_instance_decref` (`:106`),
`vkd3d_instance_get_vk_instance` (`:107`), `vkd3d_create_device` (`:110`), `vkd3d_get_vk_device`
(`:113`), `vkd3d_serialize_root_signature` (`:129`) — but **none of them is exported from any DLL**.

**The change (DECISIONS D4): add one Helios source file + TWO exports to the `d3d12core` target, and
add a second, distinctly-named `shared_library` target for the output.**

⚠ **Two exports, not one.** The second is the root-signature serializer, and it is not optional:
`d3d12umddi` delivers root signatures to the driver **already parsed** (`D3D12DDI_ROOT_SIGNATURE`,
H3), while vkd3d's `ID3D12Device::CreateRootSignature` wants a serialized DXBC `RTS0` blob, so the
UMD must re-serialize. `vkd3d_serialize_root_signature` (`include/vkd3d.h:129`) exists in the static
library but is exported from no DLL, and `d3d12core`'s own
`d3d12core_SerializeRootSignature` (`libs/d3d12core/main.c:757`) is reachable only as a vtable method
behind `D3D12GetInterface`, i.e. through the interface the UMD is avoiding.

Where: a new `libs/d3d12core/helios_entry.c`, added to `d3d12core_src` in
`libs/d3d12core/meson.build:1-4`, with both symbols in a new `libs/d3d12core/helios_vkd3d.def`
(see "Renaming" below — the existing `d3d12core.def` says `LIBRARY d3d12core.dll` and must not be
reused for a differently-named DLL):

```c
/* libs/d3d12core/helios_entry.c — Helios addition.
 * DXGI-free entry points. d3d12core's own device path,
 * d3d12core_CreateDeviceFromFactory (main.c:643, reached from
 * d3d12core_CreateDevice at :742 via D3D12GetInterface), resolves the adapter
 * through d3d12_get_adapter (:375) -> CreateDXGIFactory1 (:383, :406).
 * (The exported D3D12CreateDevice is NOT in this file — it is
 * libs/d3d12/main.c:143, in the thin d3d12.dll target Helios never loads.)
 * A WDDM UMD sits BELOW DXGI and must not depend on dxgi.dll
 * (umd/build.rs:239-243), and a UMD that loads dxgi during device creation
 * risks re-entering adapter enumeration that loads the UMD. */
#include "vkd3d.h"

DLLEXPORT HRESULT helios_vkd3d_create_device(LUID adapter_luid, REFIID iid, void **device)
{
    struct vkd3d_instance_create_info instance_ci = { 0 };
    struct vkd3d_device_create_info   device_ci   = { 0 };
    /* required instance extensions: VK_KHR_surface + VK_KHR_win32_surface
     * (d3d12core/main.c:574-580) */
    ...
    device_ci.minimum_feature_level = D3D_FEATURE_LEVEL_11_0;
    device_ci.instance_create_info  = &instance_ci;
    device_ci.vk_physical_device    = VK_NULL_HANDLE;   /* see the ⚠ below */
    device_ci.parent                = NULL;             /* NOT an IDXGIAdapter */
    device_ci.adapter_luid          = adapter_luid;
    return vkd3d_create_device(&device_ci, iid, device);
}

/* The H3 re-serialization export. Thin — vkd3d_serialize_root_signature is
 * already linked into this DLL from the static libvkd3d; it simply has no
 * exported name. */
DLLEXPORT HRESULT helios_vkd3d_serialize_root_signature(
        const D3D12_ROOT_SIGNATURE_DESC *desc, D3D_ROOT_SIGNATURE_VERSION version,
        ID3DBlob **blob, ID3DBlob **error_blob)
{
    return vkd3d_serialize_root_signature(desc, version, blob, error_blob);
}
```

`struct vkd3d_device_create_info`, verbatim from `include/vkd3d.h:74-94`:

```c
struct vkd3d_device_create_info
{
    D3D_FEATURE_LEVEL minimum_feature_level;
    struct vkd3d_instance *instance;
    const struct vkd3d_instance_create_info *instance_create_info;
    VkPhysicalDevice vk_physical_device;
    const char * const *device_extensions;
    uint32_t device_extension_count;
    const char * const *optional_device_extensions;
    uint32_t optional_device_extension_count;
    IUnknown *parent;
    LUID adapter_luid;
    D3D12_DEVICE_FACTORY_FLAGS device_factory_flags;
    bool independent;
};
```

⚠ **`vk_physical_device = VK_NULL_HANDLE` delegates selection to
`vkd3d_select_physical_device` (`libs/vkd3d/device.c:3491-3573`)**, which honours
`VKD3D_FILTER_DEVICE_NAME` and otherwise prefers DISCRETE > INTEGRATED > `physical_devices[0]`. On a
single-GPU guest that is correct, but it is *not* LUID matching. If a second Vulkan device ever
appears in the guest, chain `VkPhysicalDeviceIDProperties` yourself and match `deviceLUID` against
`adapter_luid` before calling — the algorithm to copy is `d3d12_find_physical_device`
(`libs/d3d12core/main.c:446-566`, LUID pass at `:498-532`). Setting `VKD3D_FILTER_DEVICE_NAME` to
the venus device name in step 1 of §7.3 is the cheap interim, mirroring
`DXVK_FILTER_DEVICE_NAME="Virtio-GPU Venus"` at `dxvk_bridge.cpp:1683`.

⛔ **`d3d12core.dll`'s own device path must not be used — and it is not called
`D3D12CreateDevice`.** ⚠ Correction of record, because getting this wrong sends an implementer
looking for a function that does not exist: `libs/d3d12core/main.c` contains **no**
`D3D12CreateDevice` at all. The only definition of that name in the whole tree is
`libs/d3d12/main.c:143` (`grep -rn D3D12CreateDevice libs/` returns exactly that one hit, plus
`libs/d3d12/d3d12.def:4`), and it belongs to the separate thin **`d3d12.dll`** target, which Helios
neither builds into `helios_vkd3d.dll` nor ever loads.

The entry point that must actually be avoided inside `d3d12core.dll` is
**`d3d12core_CreateDeviceFromFactory` (`libs/d3d12core/main.c:643`)**, reachable only through the
exported `D3D12GetInterface` (`:50`) → the `IVKD3DCoreInterfaceVtbl` (`:828`) →
`d3d12core_CreateDevice` (`:742`, which is a one-line forwarder to `:643`). It calls
`d3d12_get_adapter` (`main.c:375-444`), which calls `CreateDXGIFactory1(&IID_IDXGIFactory4, …)` at
**`:383`** and **`:406`**; and `libs/d3d12core/meson.build:8` lists `lib_dxgi` in
`d3d12core_dependencies`. So: **the UMD calls neither `D3D12GetInterface` nor anything reached
through it.** The Helios exports exist precisely so that path is never entered. This is the exact
parallel to `umd/build.rs:239-243`:

> NOTE: deliberately NOT linking system dxgi. A WDDM UMD sits below DXGI and implements the DXGI
> DDI; it must not depend on dxgi.dll. DXVK's only dxgi.dll call (CreateDXGIFactory1) is in
> `d3d11_main.cpp`'s exported d3d11.dll entry points, which we never reference (we build
> `D3D11DXGIDevice` directly), so that object is never pulled out of the static archive.

Building `helios_vkd3d.dll` as a *separate DLL* is what makes that true here: `main.c`'s dxgi calls
still exist inside it (the DLL does import `dxgi`), but they are only reached through
`D3D12GetInterface` → `d3d12core_CreateDeviceFromFactory`, which the UMD never calls. `umd12` itself
links **no dxgi**.

**Renaming — decided: add a second target beside the upstream one; do not rename the existing one.**
The output needs a distinct name so it can never be mistaken for, or shadow, the OS `d3d12core.dll`
in a `LoadLibrary` search, but rewriting `d3d12core_lib` in place makes every upstream rebase a
conflict in the one meson file Helios must keep patching. So `libs/d3d12core/meson.build` gains,
after the existing `d3d12core_lib` block (`:16-22`):

```meson
# Helios addition: the same objects and dependencies as d3d12core, plus
# helios_entry.c, emitted under a name that cannot shadow the OS d3d12core.dll.
helios_vkd3d_lib = shared_library('helios_vkd3d', d3d12core_src + ['helios_entry.c'],
  name_prefix         : '',
  dependencies        : d3d12core_dependencies,
  include_directories : vkd3d_private_includes,
  install             : true,
  objects             : d3d12core_needs_defs ? 'helios_vkd3d.def' : [],
  vs_module_defs      : 'helios_vkd3d.def')
```

⚠ **It needs its own `.def`, and this is easy to miss:** the existing `libs/d3d12core/d3d12core.def`
opens with `LIBRARY d3d12core.dll`, which names the *output* — reusing it for a target called
`helios_vkd3d` produces an import library and an export table stamped with the wrong module name.
The new `libs/d3d12core/helios_vkd3d.def` is:

```
LIBRARY helios_vkd3d.dll

EXPORTS
    D3D12GetInterface
    D3D12SDKVersion DATA PRIVATE
    helios_vkd3d_create_device
    helios_vkd3d_serialize_root_signature
```

(`D3D12GetInterface` and `D3D12SDKVersion` are kept because they cost nothing and keep the same
binary usable as a drop-in `d3d12core.dll` for the **headless conformance harness** — `D12-G2`,
where the vkd3d test binary resolves `D3D12CreateDevice` from a `d3d12.dll` beside it. ⛔ That is a
developer harness only: `DECISIONS.md` D2 removed the app-facing vkd3d arm, so nothing is ever
*shipped* under those names.) ⚠ `d3d12core_needs_defs` is
`(not vkd3d_is_msvc) and (vkd3d_platform == 'windows')` (`libs/d3d12core/meson.build:14`), which is **true** for the
primary mingw cross build (§8.3), so the `.def` is a real build input there, not decoration.

The resulting artifact path — the one `win_install_umd12` and `DEFAULT_VKD3D_DLL` (§9.3) must name —
is `<builddir>/libs/d3d12core/helios_vkd3d.dll`.

**Licence consequence, stated plainly (R11 §5.1).** `vkd3d-proton-helios/COPYING`:

> vkd3d-proton is free software; you can redistribute it and/or modify it under the terms of the
> **GNU Lesser General Public License** … either **version 2.1** … **or (at your option) any later
> version.**

`LICENSE` is the 502-line verbatim LGPL-2.1 text. ⇒ **LGPL-2.1-or-later.** Across a **DLL boundary**
(the design above) Helios distributes an LGPL library and its obligations are: carry the licence
text, state the changes, provide corresponding source for the LGPL parts. `helios_umd12.dll` stays
outside the LGPL boundary. **Static-linking `libvkd3d` into `helios_umd12.dll` instead triggers LGPL
§6 relinking obligations for the whole UMD.** DXVK is zlib/libpng-licensed, which is why the
existing static-link model carried none of this; vkd3d is the first LGPL component that would enter
a Helios binary.

⛔ **RESOLVED — owner directive, 2026-08-05: licensing is NOT a constraint on this decision**
(`DECISIONS.md` D4 reason 2). The paragraph above is retained as a statement of fact, not as a
decision input. **D4 stands on its two technical reasons alone** — keeping `dxgi.dll` out of the UMD
(§7.4) and keeping two SPIR-V compilers out of one DLL (§8.4) — and a static-link variant is an
equally legitimate option to be argued on those grounds. Do not re-open this as a licensing
question; if it is re-opened, it is because reason 1 or reason 3 changed.

✅ **The complete component list, read from the tree 2026-08-05 (UNVERIFIED-10 / R11's U5, closed).**
The earlier version of this table had four rows and three of them were unreadable. It is **seven**,
because `dxil-spirv` carries four nested submodules of its own:

| # | Component | Where | Links into | Terms |
|---|---|---|---|---|
| 1 | **vkd3d-proton** | `COPYING` + 502-line `LICENSE` | `helios_vkd3d.dll` | **LGPL-2.1-or-later** |
| 2 | `dxil-spirv` | `subprojects/dxil-spirv` → `HansKristian-Work/dxil-spirv` | `helios_vkd3d.dll` (`meson.build:177`) | **MIT** (`LICENSE.MIT`, `SPDX-License-Identifier: MIT`, Hans-Kristian Arntzen for Valve) |
| 3 | `dxbc-spirv` | `subprojects/dxil-spirv/subprojects/dxbc-spirv` → `doitsujin/dxbc-spirv` | `helios_vkd3d.dll` | **MIT** (Philip Rebohle, 2025) |
| 4 | `SPIRV-Cross` | `subprojects/dxil-spirv/third_party/SPIRV-Cross` | `helios_vkd3d.dll` | **Apache-2.0** (202-line text; the repo also carries a `LICENSES/` dir) |
| 5 | `SPIRV-Tools` | `subprojects/dxil-spirv/third_party/SPIRV-Tools` | `helios_vkd3d.dll` | **Apache-2.0** |
| 6 | `SPIRV-Headers` | `khronos/SPIRV-Headers` **and** `dxil-spirv/third_party/spirv-headers` (two different pins) | headers, compiled in | **Khronos MIT-style** ("Materials" grant, 502-line file) |
| 7 | `Vulkan-Headers` | `khronos/Vulkan-Headers` | headers, compiled in | **Apache-2.0 OR MIT**, per-file (`LICENSE.md` + a `LICENSES/` dir) |

⇒ **One LGPL-2.1-or-later component (vkd3d itself) and six permissive ones.** The DLL boundary of D4
is what keeps the LGPL obligations off `helios_umd12.dll`; the six permissive components need
attribution in whatever the package ships as its licence text, and nothing more.

⚠ Rows 2–7 became readable only because `git submodule update --init --recursive` ran — a
non-recursive init leaves rows 3–6 out of the tree entirely, which is exactly how the earlier
four-row table came to be wrong. **Re-read, do not paraphrase, if any pin moves.**

*Static-link fallback, if the export approach fails* (R4 §6.4): add
`helios_d3d12_static = static_library('helios_d3d12_static', …, dependencies: [vkd3d_dep, …])` beside
`d3d12core_lib` in `libs/d3d12core/meson.build`, **excluding `main.c`** so the dxgi-touching object
is never pulled out of the archive, and link it into `umd12` with `rustc-link-arg-cdylib` exactly as
`umd/build.rs:221-236` links the eight DXVK archives. Verify with
`llvm-nm --undefined-only libhelios_d3d12_static.a | findstr CreateDXGIFactory1` returning nothing.
**Record the licence decision in the commit that does it.**

---

## 8. Build and toolchain

### 8.1 `umd12/build.rs`

Same structure as `umd/build.rs` (265 lines), with four changes. Start by copying it.

**(a) bindgen.** Copy `umd/bindgen/d3d10umddi_wrapper.h` (20 lines) verbatim to
`umd12/bindgen/d3d12umddi_wrapper.h` and change the last line:

```c
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

/* d3dkmddi.h (pulled by d3d12umddi.h -> d3d10umddi.h) uses NTSTATUS, which the
 * um windows.h does not define. */
#ifndef _NTDEF_
typedef LONG NTSTATUS, *PNTSTATUS;
#endif

#include <d3d12umddi.h>
```

The `NTSTATUS` shim is still needed. Note `d3d12umddi.h:21` does `#include "d3d10umddi.h"`, so this
generation is a **superset** of the D3D11 one — which is why the crates cannot share a generated
module and why the allowlist matters.

Allowlists (replacing `umd/build.rs:97-103`):

```rust
.allowlist_type("D3D12DDI.*")
.allowlist_type("PFND3D12DDI.*")
.allowlist_type("D3DDDI.*")          // the 65-entry D3DDDI_DEVICECALLBACKS the UMD drives
.allowlist_var("D3D12DDI.*")
.layout_tests(true)
.derive_default(true)
.generate_comments(false)
```

**Expected size — measure it, do not guess.** The D3D11 generation is ~1.1 MB / 43k lines / 818
types, with 817 size + 815 alignment + 4 704 offset assertions (`umd/build.rs:104-122`). Measured on
`tmp/dx12/sdk/d3d12umddi.h` this session:

| Measure | d3d10umddi (shipping) | d3d12umddi |
|---|---:|---:|
| header lines | — | **19 031** |
| `^typedef struct D3D12DDI_` | — | **517** |
| `^typedef struct D3D12DDIARG` | — | **133** |
| `^typedef struct` (all) | — | **683** |
| distinct `PFND3D12DDI_*` typedefs | — | **399** |
| `PFND3D12DDI_` references | — | **5 770** |
| generated Rust | 43k lines / 1.1 MB | **UNVERIFIED-2** — expect ≥2× |

⚠ R4 §6.5 reported "518 `typedef struct D3D12DDI_*`"; the exact-prefix count is **517** and the
D3D12DDI-prefixed total (including `D3D12DDIARG`) is **681**. If the cold build hurts, narrow the
allowlist to the DDI versions actually implemented — but **never** drop `layout_tests(true)`
(DECISIONS §7.2).

**(b) cxx_build** — §7.1. Single bridge for now (`cxx_build::bridge("src/bridge12.rs")`); use
`cxx_build::bridges([..])` only if a second bridge module appears. ⚠ Include
`../umd_common/bridge` (§4.4) so the shared `bridge_common.h` — `umd_log`, `bridge_log_budget`,
`PeriodicStat`, `qpc_elapsed_us`, `ComRelease<T>`, `bridge_guard` — is the one both DLLs compile.
`umd/build.rs` gains the identical line in the same commit; a `bridge_common.h` visible to only one
of the two crates is how a second `bridge_guard` gets written.

**(c) The `/MD` + clang-cl coherence rule, and its `rerun-if-env-changed` edge.** State it in
`umd12/build.rs`'s module doc the way `umd/build.rs:9-13` does:

> Toolchain coherence (critical): vkd3d, the cxx shim, and the Rust crate must all use the MSVC C++
> ABI with the **dynamic** CRT (`/MD`).

and reproduce `umd/build.rs:158-173` verbatim:

```rust
// `cc` and `cxx-build` do not add a rerun edge for a compiler supplied via
// `.compiler()`, so swapping HELIOS_CLANG_CL or HELIOS_MSVC_LIB left the
// previously built bridge .lib — compiled against the previous MSVC STL — to be
// relinked against freshly built engine archives (which DO have
// rerun-if-changed), giving mismatched std::string / std::mutex layouts across
// the cxx boundary inside one DLL. That is heap corruption at runtime, guarded
// by prose. Declaring the identity as a build input turns a changed *selection*
// into a rebuild.
println!("cargo:rerun-if-env-changed=HELIOS_CLANG_CL");
println!("cargo:rerun-if-env-changed=HELIOS_MSVC_LIB");
```

⚠ The honest remaining hole, named in that same comment: **it does not catch an in-place LLVM
upgrade.** A generated toolchain fingerprint (resolved `clang-cl --version` + MSVC include dir, with
`rerun-if-changed` on it) is the stronger fix and is still a follow-up.

vkd3d's meson already recognises clang-cl —
`vkd3d_is_msvc = vkd3d_compiler.get_id() == 'msvc' or vkd3d_compiler.get_id() == 'clang-cl'`
(`vkd3d-proton-helios/meson.build:9`) —
and sets `c_std=c11, cpp_std=c++17` (`meson.build:2-4`), the same C++ standard the Helios bridge
compiles with (`umd/build.rs:192`). So the `win_dxvk` toolchain recipe transfers directly.

**(d) `require_path` for every absolute default** (`umd/build.rs:63-70`) — fail at the point a path
is chosen, naming the env var that overrides it. `umd12` needs `HELIOS_VKD3D_SRC` and
`HELIOS_VKD3D_BUILD`.

### 8.2 ⛔ FIRST: the three nested submodules are uninitialised

Verified this session in `vkd3d-proton-helios/`:

```
$ git submodule status
-f88a2d766840fc825af1fc065977953ba1fa4a91 khronos/SPIRV-Headers
-0e9de566b7d4051c5cc1b762e242c46565956bdf khronos/Vulkan-Headers
-cc75a0c98d34d7bcc03560527c799b52e48b4d1f subprojects/dxil-spirv
```

The leading `-` means **uninitialised**; all three directories are empty. **`vkd3d-proton` cannot be
configured, let alone built, from this checkout as it stands.** This is step one of any D3D12 build
task and it is recorded nowhere else in the repo.

On the **Linux host**:

```bash
cd /home/rupansh/helios-vgpu/vkd3d-proton-helios
git submodule update --init --recursive
git submodule status          # expect three lines with NO leading '-'
```

✅ **Done 2026-08-05** — `khronos/SPIRV-Headers f88a2d76`, `khronos/Vulkan-Headers 0e9de566`
(v1.4.351), `subprojects/dxil-spirv cc75a0c9`. ⚠ **`--recursive` is not optional and it is not
cosmetic:** `dxil-spirv` has **four** nested submodules of its own —
`subprojects/dxbc-spirv` (`doitsujin/dxbc-spirv d5b06435`), `third_party/SPIRV-Cross 4b7bcb7e`,
`third_party/SPIRV-Tools 199cb207`, `third_party/spirv-headers c63848ec` — so seven repositories,
not three, end up inside `helios_vkd3d.dll`.

⚠ `.gitmodules` in the superproject names `https://github.com/rupansh/vkd3d-proton` while the
checkout's `origin` is `HansKristian-Work/vkd3d-proton` via the `github-rupansh` SSH alias (R3 §10.2).
Any Helios change to vkd3d (§7.4) needs the fork wired as a push remote first — that wiring, and the
commit that adds `helios_entry.c` + `helios_vkd3d.def` + the `helios_vkd3d_lib` target, is **stage
S0c** (§11). It was previously specified in §7.4 and scheduled nowhere.

⚠ Initialising these submodules is also what made UNVERIFIED-10 answerable — the licence files are
not in the tree until this command runs. ✅ Read and tabulated: §7.4's seven-row table.

### 8.3 Building vkd3d — Linux mingw cross is PRIMARY; native MSVC on the VM is the fallback

⚠ **Decided (DECISIONS §6.1; `GATES.md` §4.1 `D12-G0` says the same).** An earlier revision of this
section prescribed a native MSVC/clang-cl build on the win11 VM as the only arm and made it
UNVERIFIED-7, "the single hardest gate in the plan". That was wrong on the facts: **the entire
toolchain is already installed on the Linux host**, so the cross build needs zero bring-up, and it
is the configuration vkd3d-proton itself ships (`.github/workflows/artifacts.yml:18-22` builds
through `misyltoad/arch-mingw-github-action` + `package-release.sh`). Verified on this host,
2026-08-05 — every binary the cross file names resolves:

```
x86_64-w64-mingw32-gcc   /usr/bin/x86_64-w64-mingw32-gcc
x86_64-w64-mingw32-g++   /usr/bin/x86_64-w64-mingw32-g++
widl                     /usr/bin/widl
glslangValidator         /usr/bin/glslangValidator
meson                    /usr/bin/meson
ninja                    /usr/bin/ninja
wine                     /usr/bin/wine
```

`build-win64.txt` (committed, verified) names `x86_64-w64-mingw32-{gcc,g++,ar,strip}` plus
`widl-mingw-tools-fallback = 'x86_64-w64-mingw32-widl'`. ⚠ That last binary is **not** installed on
this host — and it does not matter: `meson.build:73` does `find_program('widl', required : false)`
first and only falls back to `widl-stable`/`widl-mingw-tools-fallback` at `:76` when that misses.
Plain `/usr/bin/widl` is present, so the IDL step resolves on the first try. Do not "fix" the cross
file.

**Primary — the Linux host** (this is `GATES.md` `D12-G0` verbatim; run it there, not here):

```bash
cd /home/rupansh/helios-vgpu/vkd3d-proton-helios
git submodule update --init --recursive        # §8.2 — all three are EMPTY today
meson setup --cross-file build-win64.txt --buildtype release \
      -Denable_tests=true -Denable_extras=true \
      /home/rupansh/helios-vgpu/tmp/dx12/build/vkd3d-win64
ninja -C /home/rupansh/helios-vgpu/tmp/dx12/build/vkd3d-win64
```

Artifacts land at `tmp/dx12/build/vkd3d-win64/libs/d3d12core/{d3d12core.dll,helios_vkd3d.dll}`,
`libs/d3d12/d3d12.dll`, `tests/d3d12.exe`, `demos/{triangle,gears}.exe`. Copy to the VM with
`robocopy`/`win_exec` — nothing about the *build* needs the VM.

⚠ **`/MD` does not reach across the D4 boundary.** §8.1(c)'s toolchain-coherence rule (vkd3d, the
cxx shim and the Rust crate all on the MSVC C++ ABI with the dynamic CRT) is a rule about objects
linked into **one image**. D4's boundary is a **C ABI + `LoadLibrary`/`GetProcAddress`**: the only
types crossing it are `LUID`, `REFIID`, `void**`, `HRESULT` and COM vtable pointers. A mingw-built
`helios_vkd3d.dll` and an MSVC-built `helios_umd12.dll` therefore coexist by construction — which is
exactly why the primary arm can be a cross build at all. §8.1(c) still governs `umd12` itself
(bridge `.lib` vs Rust crate vs anything statically linked), and it becomes load-bearing again only
under the §7.4 static-link fallback, where vkd3d objects *do* enter the UMD image.

**Fallback — native MSVC x64 on the win11 VM, taken when a Windows debugger is wanted.** Upstream CI
builds it and gates on it — `.github/workflows/test-build-windows.yml`, `runs-on: windows-2022`:

```yaml
    - name: Setup widl and glslangValidator
      run: |
        choco install strawberryperl -y
        Invoke-WebRequest -Uri "https://raw.githubusercontent.com/HansKristian-Work/vkd3d-proton-ci/main/glslangValidator.exe" -OutFile "C:\Strawberry\c\bin\glslangValidator.exe"
        Write-Output "C:\Strawberry\c\bin" | Out-File -FilePath "${Env:GITHUB_PATH}" -Append
    - name: Setup Meson
      run: pip install meson
    - name: Build MSVC x64
      run: |
        ... VsDevCmd.bat -arch=x64 -host_arch=x64 ...
        meson setup -Denable_tests=True -Denable_extras=True --buildtype release --backend vs2022 build-msvc-x64
        msbuild -m build-msvc-x64/vkd3d-proton.sln
```

Dependencies (R3 §8.2): meson ≥ 0.49; MSVC 2022 **or clang-cl** (or, for the primary arm,
`x86_64-w64-mingw32-gcc`); **`widl`** (Wine IDL compiler — on Windows it ships with **Strawberry
Perl**; on the Linux host it is `/usr/bin/widl`) to compile the **18** `.idl` in `include/`
(`ls vkd3d-proton-helios/include/*.idl | wc -l` → 18; R3's "17" was one short);
**`glslangValidator`** for the **53** GLSL meta-shaders (`--target-env vulkan1.3`) — the
`vkd3d_shaders` list at `libs/vkd3d/meson.build:1-…` has 53 entries and
`libs/vkd3d/shaders/` holds 35 `.comp` + 14 `.frag` + 3 `.vert` + 1 `.geom` = 53 (plus 3 `.h`); the
three submodules. Not needed: SPIRV-Tools, DXC.

⚠ vkd3d does **not** use the Windows SDK's `d3d12.h` — it compiles its own IDLs on top of its own
`vkd3d_windows.h`/`vkd3d_win32.h` shims (R3 §8.3), so the type layouts are vkd3d's transcription of
the D3D12 ABI, not the SDK's. Any comparison against `tmp/dx12/sdk/d3d12.h` must be deliberate. This
is *not* a problem for the bridge — the bridge only ever touches `ID3D12Device*` as an opaque
pointer plus `IUnknown::Release` — but it is a trap for anyone tempted to pass an SDK
`D3D12_GRAPHICS_PIPELINE_STATE_DESC` across it.

Exact commands, mirroring `win_dxvk`'s recipe (⚠ **LLVM on PATH before `vcvars64`** — cmd expands
`%PATH%` at **parse** time, so the reverse order silently drops MSVC's `lib.exe`,
`tools/win-mcp/src/main.rs:755-762`). Build to a **local C: path**, never `Z:\`:

```powershell
# one-time, via win_exec
choco install strawberryperl -y
pip install meson
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/HansKristian-Work/vkd3d-proton-ci/main/glslangValidator.exe" `
  -OutFile "C:\Strawberry\c\bin\glslangValidator.exe"

# configure + build (this is what `win_vkd3d` should wrap — §9.3)
robocopy Z:\vkd3d-proton-helios C:\Users\Rupansh\vkd3d-proton-helios /MIR /XJ /XD .git /XF .git /NFL /NDL /NJH /NJS /NP /R:1 /W:1
cmd /c 'set "PATH=C:\Program Files\LLVM\bin;C:\Strawberry\c\bin;%PATH%" && call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat" && meson setup --buildtype release -Denable_tests=true -Denable_extras=true C:\Users\Rupansh\vkd3d-build C:\Users\Rupansh\vkd3d-proton-helios'
cmd /c 'set "PATH=C:\Program Files\LLVM\bin;C:\Strawberry\c\bin;%PATH%" && call "...\vcvars64.bat" && meson compile -C C:\Users\Rupansh\vkd3d-build'
```

`-Denable_tests=true` matters: it builds `tests/d3d12.exe` — **40 `.c` test files** which, with the
4 `.h` beside them, total **~105 k lines** (`cat tests/*.c tests/*.h | wc -l` → 105 265; the whole
`tests/` tree including `tests/shaders/` is ~167 k). ⚠ The "~152k LOC" figure this section carried
matched neither count. It is the conformance oracle for G2/G9 and can be baselined on WARP first
(R3 §13.8). `-Denable_extras=true` builds `demos/triangle.c` and `demos/gears.c` — the cheapest
first-light targets (R3 §13.9).

⚠ `robocopy /XD .git` must **not** be relied on to bring the submodules if `git submodule update`
was run only on the Linux side — it does copy the checked-out files (they are ordinary files after
init), but confirm with
`Test-Path C:\Users\Rupansh\vkd3d-proton-helios\subprojects\dxil-spirv\meson.build`.

### 8.4 UNVERIFIED: do the two SPIR-V producers coexist?

Only matters under the one-DLL fallback (§3.4). DXVK links `libdxbc_spv.a` + `libspirv.a`
(`umd/build.rs:226-227`); vkd3d links `dxil-spirv` (`meson.build:177`) + `libvkd3d-shader.a`. Both
are C++ SPIR-V producers built over SPIRV-Headers, from **different vendored copies** —
`vkd3d-proton-helios/khronos/{Vulkan,SPIRV}-Headers` versus
`dxvk-helios/include/vulkan/include` + `include/spirv/include` (named at `umd/build.rs:201-202`).
Settling experiment in §13.

Two more reasons the C++ sides stay in separate crates and separate DLLs, both verified:

- **Contradictory Vulkan include-order rules.** `umd/bridge/bridge_icd_exports.h:8-13` states DXVK's
  `src/vulkan/vulkan_loader.h` must be the **first** Vulkan header a TU sees, and that pulling
  `<vulkan/vulkan.h>` ahead of it breaks `dxvk_device_info.h` and `dxvk_presenter.h` with nine hard
  errors. `vkd3d-proton-helios/include/vkd3d.h:43-49` includes `<vulkan/vulkan.h>` directly unless
  `VKD3D_NO_VULKAN_H` is defined. In one TU these fight.
- **Static-init order.** `dxvk_bridge.cpp:70` defines `dxvk::Logger Logger::s_instance` — a
  namespace-scope object with a dynamic initialiser. vkd3d has its own logging in `libs/vkd3d-common`.
  Two engines' global initialisers in one DLL is the T7 crash class's home ground.

---

## 9. Deployment

### 9.1 The INF

⛔ **`*.inx` is only edited with explicit instruction** (CLAUDE.md, "Files Not to Touch"). What
follows is the proposal, not an applied change.

Current relevant lines of `kmd_render/helios_kmd_render.inx` (verified):

```inf
14  [DestinationDirs]
15  Helios_CopyFiles = 13  ; driver store (DIRID 13) — required for a universal INF
20  [SourceDisksFiles]
21  helios_kmd_render.sys = 1,,
22  helios_umd.dll = 1,,
42  [Helios_CopyFiles]
43  helios_kmd_render.sys
44  helios_umd.dll
80  [Helios_DeviceSettings]
81  HKR,, UserModeDriverName,       %REG_MULTI_SZ%, %13%\helios_umd.dll,%13%\helios_umd.dll,%13%\helios_umd.dll,%13%\helios_umd.dll
82  HKR,, InstalledDisplayDrivers,  %REG_MULTI_SZ%, helios_umd,helios_umd,helios_umd,helios_umd
```

**Variant A — D3D12 inside `helios_umd.dll`. Zero INF changes.** Slot 3 already names
`helios_umd.dll` and `OpenAdapter12` is already exported and already called (§3.1). This is the
cheapest handshake bring-up and the tree is already wired for it. Cost: every D3D12 bug ships inside
the binary dwm's composition device loads.

**Variant B — the separate DLL (the target).**

```diff
 [SourceDisksFiles]
 helios_kmd_render.sys = 1,,
 helios_umd.dll = 1,,
+helios_umd12.dll = 1,,

 [Helios_CopyFiles]
 helios_kmd_render.sys
 helios_umd.dll
+helios_umd12.dll

 [Helios_DeviceSettings]
-HKR,, UserModeDriverName,       %REG_MULTI_SZ%, %13%\helios_umd.dll,%13%\helios_umd.dll,%13%\helios_umd.dll,%13%\helios_umd.dll
-HKR,, InstalledDisplayDrivers,  %REG_MULTI_SZ%, helios_umd,helios_umd,helios_umd,helios_umd
+HKR,, UserModeDriverName,       %REG_MULTI_SZ%, %13%\helios_umd.dll,%13%\helios_umd.dll,%13%\helios_umd.dll,%13%\helios_umd12.dll
+HKR,, InstalledDisplayDrivers,  %REG_MULTI_SZ%, helios_umd,helios_umd12
```

Line-by-line notes:

- `[DestinationDirs]` needs **no** change — `Helios_CopyFiles = 13` already sends the section to the
  DriverStore package directory. ⛔ Do **not** add `COPYFLG_IN_USE_TRY_RENAME`: combined with
  DIRID 13, `infverif` rejects it (`HELIOS_DRIVER_DEPLOYMENT.md:29-30`).
- `[Strings]` already defines `REG_MULTI_SZ = 0x00010000` (`:98`; `:97` is `REG_DWORD`, `:99` is
  `REG_EXPAND_SZ`); nothing new.
- ⚠ **`InstalledDisplayDrivers` is NOT index-parallel.** Microsoft's shape
  (`adding-user-mode-display-driver-names-to-the-registry.md:20`) is a **flat list of the distinct
  UMD binaries in the package, extension stripped** — its stated purpose (`:39`) is that *"WHQL test
  programs use the list … to validate that the driver binaries remain unchanged over a test run"*.
  The current four-times-`helios_umd` value is harmless but semantically wrong. **The correct value
  for the split is `helios_umd,helios_umd12`** — two entries, not four (R11 §1.6).
- `Include = msdv.inf` at `:37` is copied from Microsoft's own sample and, with no matching `Needs=`,
  is inert. Leave it alone while touching this file (R11 U4).
- ⛔ vkd3d's DLLs do **not** go in the driver package. DriverStore placement does not make them
  findable by the D3D12 loader; only app-local placement does (R11 §5.2). `helios_vkd3d.dll` is
  loaded by *us*, by full path, from wherever `win_install_umd12` puts it — put it beside the UMD in
  `C:\ProgramData\HeliosUmd\`.

**Version single-site rule.** `kmd_render/driver-version.env` (`HELIOS_KMD_VERSION=22.22.252.0`) is
the only place the version is edited; its header names the three consumers (`kmd_render/build.rs`,
`kmd_render/Cargo.make.toml` `env_files`, `tools/win-mcp`'s `win_build_kmd`). ⛔ Never reintroduce a
literal — an INF `DriverVer` disagreeing with the image `FILEVERSION` is **FAILED_ADD 0xc0000182**,
discovered only after a reboot.

⚠ **Live gap, and adding a second UMD is the moment to close it:** the deployed `helios_umd.dll` has
**no version resource at all** (`(Get-Item …).VersionInfo.FileVersion` is empty), while the `.sys`
reads `22.22.252.0` and the INF `DriverVer = 08/05/2026,22.22.252.0`. `umd/build.rs` has no `rc.exe`
step (grep for `rc.exe|VERSIONINFO|resource` → zero hits; `kmd_render/build.rs` has all of them).
MSDOC `wddm-2-1-features.md:242-244`: *"The .inf, .sys and .dll file version info must match."*
Give **both** UMDs a version resource driven from `driver-version.env` (R11 §9.4).

⚠ `UserModeDriverNameWow` is live on this VM, **stale and wrong**: it points at a 27-July DriverStore
folder and names the **x64** `helios_umd.dll`, so a 32-bit D3D client `LoadLibrary`s an x64 image →
`ERROR_BAD_EXE_FORMAT`. Nothing in this repository writes it. The correct action until 32-bit
binaries exist is to `DelReg` it (R11 §3 Variant B+), not to extend it. Related: DECISIONS S2 —
`HKLM\SOFTWARE\WOW6432Node\Khronos\Vulkan\Drivers` does not exist on the guest either.

### 9.2 `hotplug-helios-umd.ps1`

The script (`tools/hotplug-helios-umd.ps1`, invoked by `win_install_umd`) does four things that a
D3D12 sibling must do identically:

1. copies to `C:\ProgramData\HeliosUmd\helios_umd_<first-16-hex-of-sha256>.dll` (`:51`) — a
   **content-addressed** name, which is *why* the ICD's export lookup walks modules (§6.1);
2. rewrites the class key with four identical entries (`:100-103`):
   ```powershell
   $umdNames = @("helios_umd", "helios_umd", "helios_umd", "helios_umd")
   $umdPaths = @($programDataDll, $programDataDll, $programDataDll, $programDataDll)
   New-ItemProperty … -Name "UserModeDriverName"      -PropertyType MultiString -Value $umdPaths -Force
   New-ItemProperty … -Name "InstalledDisplayDrivers" -PropertyType MultiString -Value $umdNames -Force
   ```
3. **also syncs the active DriverStore copy** (`:105-133`), with `-DisplaceInUse` for the routinely
   mapped package copy;
4. verifies the destination SHA256 and **throws** on mismatch (`:158-162`).

Changes, when authorised:

```powershell
param(
  ...
  [string]$Umd12Dll = "",              # empty = leave slot 3 alone
  [switch]$DisableD3D12                # point slot 3 back at $programDataDll (§10-L2)
)
...
$programData12Dll = if ($Umd12Dll) { Join-Path $ProgramDataDir ("helios_umd12_{0}.dll" -f $src12Hash.Substring(0,16).ToLowerInvariant()) } else { $programDataDll }
$umdNames = @("helios_umd", "helios_umd12")                                        # NOT index-parallel (R11 §1.6)
$umdPaths = @($programDataDll, $programDataDll, $programDataDll, $programData12Dll)
```

plus the same `Copy-HeliosFileVerified` + SHA256 check + `takeown`/`icacls` + DriverStore sync for
`helios_umd12.dll`, and the same for `helios_vkd3d.dll` (which needs no registry entry, only a
verified copy).

⚠ **The DriverStore-staleness trap applies to both files.** From the script's own comment
(`:105-111`): at **cold boot** dxgkrnl's first UMD-path resolution loads the *package's*
`helios_umd.dll` before the registry override takes effect for later device creates, so a stale
DriverStore copy means dwm's first — composition — device runs an old UMD every boot (*"proven
2026-07-03: two different handler generations in one dwm process"*). A `win_install_umd12` that
skips the DriverStore sync will produce a D3D12 driver that is one build old at every cold boot and
current afterwards, which is the worst possible debugging surface.

⚠ **The path is latched at DEVICE start, not process start.** Three concurrently-live processes were
running three *different* DriverStore copies in one boot while the registry named a fourth (R11
§2.3), with different `3DPIPELINESUPPORT` caps (`0x1` vs `0x8f`). **No D3D12 evidence is admissible
without confirming the loaded module path per process**: `(Get-Process -Id N).Modules`. Deploy with
`-KillUmdUsers -RestartDevice -NoProbe`.

⚠ `-ExecutionPolicy Bypass` is mandatory — machine policy is `Restricted`, and any other invocation
silently no-ops (`tools/win-mcp/src/main.rs:802-811`, `BRINGUP_QUIRKS.md:74-83`).

### 9.3 New `win-mcp` tools

Three additions to `tools/win-mcp/src/main.rs`, each modelled on an existing tool.

| New tool | Model | Constants to add |
|---|---|---|
| `win_vkd3d` — ⚠ **only needed for the §8.3 *fallback* arm.** The primary vkd3d build is the Linux mingw cross build, which needs no VM tool at all; the artifact reaches the VM by `robocopy`. Build this tool when a Windows debugger is first wanted, not at S0 | `win_dxvk` (`:733-776`) — mirror source, then **LLVM on PATH before `vcvars64`** (the comment is `:755-760`, the command `:761-763`) | `VKD3D_SRC = "Z:\\vkd3d-proton-helios"`, `VKD3D_MIRROR = "C:\\Users\\Rupansh\\vkd3d-proton-helios"`, `VKD3D_BUILD = "C:\\Users\\Rupansh\\vkd3d-build"`, `STRAWBERRY_BIN = "C:\\Strawberry\\c\\bin"` (widl + glslangValidator) |
| `win_install_umd12` | `win_install_umd` (`:778-822`) — explicit artifact paths echoed as the first output lines, `-ExecutionPolicy Bypass`, refuse `-UmdDll`-style duplicates in `args` | `DEFAULT_UMD12_DLL = "C:\\Users\\Rupansh\\helios-vgpu\\umd12\\target\\release\\helios_umd12.dll"`, and — because §7.4 picks the *second-target* rename, so the file is genuinely named `helios_vkd3d.dll` in `libs/d3d12core/` — `DEFAULT_VKD3D_DLL = "C:\\Users\\Rupansh\\vkd3d-build\\libs\\d3d12core\\helios_vkd3d.dll"` for the fallback arm, or `"Z:\\tmp\\dx12\\build\\vkd3d-win64\\libs\\d3d12core\\helios_vkd3d.dll"` for the primary (Linux-cross) arm. Name **one** of them the default in the tool and mark the other NON-DEFAULT when overridden, the way `win_install_umd` already does |
| (extend `win_cargo`) | already generic over `crate_dir` (`:562`) — `crate_dir:"umd12"` and `crate_dir:"umd_common"` work with **no change** | — |

⚠ **The `/XD` exclusion-list bug.** `win_cargo`'s robocopy mirror at `main.rs:576` and
`win_build_kmd`'s at `main.rs:843` both name a bare **`vkd3d-proton`** — a directory that no longer
exists; the tree has `vkd3d-proton-helios`:

```
robocopy Z:\ C:\Users\Rupansh\helios-vgpu /MIR /XJ /XD target .git "Z:\icd\mesa" dxvk
    dxvk-research-only vkd3d-proton virtio-research-only-3d windows-driver-docs-research-only ...
```

Robocopy's `/XD` name matching is documented ambiguously and is substring-ish for path forms, so
**whether `vkd3d-proton` also excludes `vkd3d-proton-helios` is UNVERIFIED-3 (§13)** — and it
decides whether every `win_cargo` call copies ~118k LOC plus three submodules to the VM or skips
them. Correct the entry to `vkd3d-proton-helios` in both places once the answer is known; if the
intent is to keep vkd3d out of the cargo mirror (it should be — `win_vkd3d` mirrors it separately,
like DXVK), the entry must name the real directory.

### 9.4 Packaging / signing sites a second DLL touches

Every list below is hand-maintained; each is a place the new DLL is silently dropped (R11 §4.2,
re-verified):

| Site | Change |
|---|---|
| `kmd_render/helios_kmd_render.inx:21-22, 43-44, 81-82` | `SourceDisksFiles`, `CopyFiles`, both registry values (§9.1) |
| `kmd_render/Cargo.make.toml:44-101` (`[tasks.copy-umd-to-package]`) | build + stage the second DLL — or better, one task over a list. It shells out to `cargo build` in `umd/` with the *same* profile and asserts the DLL exists (`:75-91`) |
| `tools/install-helios-kmd.ps1:10, 157, 300, 315, 327-329, 351, 428` | `Sync-HeliosPackageUmd` for the second DLL; add to `$copyNames` (`:327`) and to the final state hash map (`:428`) |
| `tools/hotplug-helios-umd.ps1:51, 100-133` | hashed ProgramData name, `UserModeDriverName` rewrite, **DriverStore sync of both** (§9.2) |
| `ci/windows/Build-Driver.ps1:96` | `$required = @("helios_kmd_render.inf", "helios_kmd_render.sys", "helios_umd.dll")` → add `helios_umd12.dll` |
| `ci/windows/Assemble-Package.ps1:49, 52, 116` | required-file list, optional PDB list (`helios_umd12.pdb`), `Invoke-SignTool … helios_umd12.dll` |
| `packaging/windows/Verify-Helios.ps1` | ⚠ it checks runtime-file hashes, PnP status, the Vulkan ICD registry, `OpenGLDriverName` and the OpenCL vendor key but **never looks at `UserModeDriverName` at all** (verified: zero hits for `helios_umd` or `UserModeDriverName` in that file). Add a `UserModeDriverName[3]` assertion and a `d3d12-smoke.exe` |

⛔ Catalog rules, all previously paid for (`BRINGUP_QUIRKS.md:37-67`): sign **before** `inf2cat`;
re-running `inf2cat` over a package that already has a **signed** `.cat` produces a **corrupt** `.cat`
(`CryptCATOpen → 0x0000000D` → `0xC000026C` → **Code 39**, with an empty diag ring because AddDevice
never ran) — delete the `.cat` first, run `inf2cat` standalone, then sign;
`Get-AuthenticodeSignature` on the `.cat` proves nothing about coverage — verify with
`signtool verify /pa /c <cat> helios_umd12.dll` **and** compare `Get-FileHash` of the deployed vs
package binary. `Inf2Cat.exe` ships **x86-only**. A bare `& signtool …` not on PATH fails
**silently**.

---

## 10. The kill switch and rollback

**Requirement, from evidence:** dwm probes D3D12 on every boot today (§3.1). The moment
`OpenAdapter12` returns `S_OK`, dwm becomes a potential D3D12 client, and a bug there costs the
desktop. The disable path must work (a) without a rebuild, (b) **without a working desktop**, and
(c) without a reboot for *new* processes.

**DECISIONS D11 — D3D12 ships behind an off-by-default kill switch.** Three layers, increasing blast
radius.

### L1 — the registry knob (primary)

Exact declaration, in `umd12/src/knobs12.rs`, using `umd_common`'s `BoolKnob`:

```rust
/// D3D12 DDI enable. `HKLM\SOFTWARE\Helios!UmdD3D12` (REG_DWORD), read once per
/// process. **Absent = OFF** during bring-up: `OpenAdapter12` returns
/// DXGI_ERROR_UNSUPPORTED exactly as `umd/src/adapter.rs:177-189` does today, so
/// an install with the knob unset is bit-identical to a build without the D3D12
/// path.
///
/// CLAUDE.md rule 8: flipping this default to ON requires the evidence in the
/// comment at THIS site, and OFF must stay reachable as the A/B disable.
pub(crate) static UMD_D3D12: BoolKnob = BoolKnob::new(c"UmdD3D12", false);
```

read at the very top of `OpenAdapter12`, and added to `umd12`'s `resolved_inventory()` so
`log_knob_inventory()` dumps it at every adapter open.

Why this shape:

- `BoolKnob::new(name, default)` forces the absent-value policy to be written at the definition site
  (`umd/src/knobs.rs:1-13`).
- Read **once per process** (one `OnceLock` per knob), so an already-running dwm keeps its behaviour
  while new processes pick up the change.
- `HKLM\SOFTWARE\Helios` is writable over SSH in session 0 with the desktop down.
- **No 14-character limit applies.** The UMD reads via `RegGetValueA` (`umd/src/knobs.rs:60-105`);
  the ≤14-char cap is a **KMD** constraint (`kmd_render/src/diag.rs`) because those knobs go through
  `RtlQueryRegistryValues` on the service key. Do not confuse the two hives.

Set / clear it:

```powershell
reg add    "HKLM\SOFTWARE\Helios" /v UmdD3D12 /t REG_DWORD /d 1 /f    # enable  (new processes)
reg delete "HKLM\SOFTWARE\Helios" /v UmdD3D12 /f                      # disable (new processes)
```

⛔ The disabled path returns `DXGI_ERROR_UNSUPPORTED` = `0x887A_0004`, **never**
`DXGI_ERROR_DRIVER_INTERNAL_ERROR` = `0x887A_0020`. `umd/src/adapter.rs:182-188` records that this
site returned the latter until R801, so an ordinary "this driver has no D3D12 DDI" negotiation was
recorded by the runtime and by ETW as a **driver fault**, and both constants printed as the string
`"DXGI_ERROR_UNSUPPORTED"` in our own log so the divergence was invisible to a triage grep.

### L2 — the separate DLL in slot 3 (structural)

Recovery from "`helios_umd12.dll` will not even load" (a link error, a missing
`helios_vkd3d.dll`, a static-init crash — none of which the L1 knob can reach, because the failure is
before `OpenAdapter12` runs) is a **single `REG_MULTI_SZ` rewrite**:

```powershell
$k = "HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}\0000"
$v = (Get-ItemProperty -Path "Registry::$k" -Name UserModeDriverName).UserModeDriverName
$v[3] = $v[2]            # point D3D12 back at the D3D11 DLL — or at a nonexistent path
Set-ItemProperty -Path "Registry::$k" -Name UserModeDriverName -Value $v
```

No package reinstall, no catalog, no reboot for new processes, and dwm's D3D11 composition device is
provably unaffected because it resolves index 2. This is the strongest argument for the two-DLL
shape and it is a stability argument, not an architecture one. `hotplug-helios-umd.ps1` needs only
the `-DisableD3D12` switch from §9.2.

### L3 — package rollback (last resort, owner-driven)

`tools/install-helios-kmd.ps1` backs up the active DriverStore files under
`C:\ProgramData\HeliosDeployBackups\<timestamp>` (`HELIOS_DRIVER_DEPLOYMENT.md:87`), and a recovery
boot without the `virtio-gpu-gl-pci` device (`BRINGUP_QUIRKS.md:142-153`) unlocks the live
`.sys`/`.dll` for replacement. VM device-set changes are owner-gated (CLAUDE.md).

### ⛔ What NOT to do

- Do **not** make the disable a compile-time `#[cfg]`. The whole point is that the failure is
  discovered on a machine where you cannot rebuild-and-redeploy through a dead desktop.
- Do **not** gate it on an environment variable alone: dwm's environment is not yours to set, and the
  UMD's env knobs are explicitly outside the knob inventory (`umd/src/knobs.rs:51-56`).
- Do **not** let `OpenAdapter12` do *anything* — no `LoadLibrary`, no logging beyond the two existing
  lines — before the knob is read.

---

## 11. Staged migration order

Each stage is independently buildable, deployable and revertible. **S1 and S2 touch shipped binaries
and must each be proven neutral; S0, S3 and S4 touch no shipped binary at all.** Gate ids reference
`docs/dx12/GATES.md` (`D12-G0 … D12-G11`).

⛔ **AMENDMENT 2026-08-05 — the engine is STATICALLY LINKED (`DECISIONS.md` D4, owner).** The table
below was written against the superseded "`LoadLibrary` a `helios_vkd3d.dll`" shape. Read it with
these three substitutions:

| where the table says | read |
|---|---|
| S0 builds `helios_vkd3d.dll` by **mingw cross on the Linux host** | that build **stays**, but as the **conformance** arm only — it is what `D12-G0`/`G2` need for the vkd3d test suite. The **shipping** artifacts are static archives built on the VM by **`win_vkd3d`** with clang-cl (`libhelios_d3d12_static.a` + six engine archives, ~30.6 MB), matching how DXVK is built and linked today. |
| S4's probe **`LoadLibrary`s** the engine | S4's `umd12/build.rs` **links the archives**, exactly as `umd/build.rs` links DXVK's eight. There is no engine DLL on the shipping path and no `LoadLibrary` of one. ⭐ **The link set is MEASURED, not guessed** (`D12-G1` static arm, 2026-08-05): `libhelios_d3d12_static.a` **alone**, plus `cargo:rustc-link-lib=dylib=gdi32`. One archive — it is a *union* archive carrying every vkd3d / dxil-spirv / dxbc-spirv object — and `gdi32` for the 12 `__imp_D3DKMT*` that `libs/vkd3d/d3dkmt.c` imports. ⛔ **Never `dxgi`.** `tmp/dx12/gates/G1-static/RESULT.md` |
| `helios_vkd3d.dll` as a deliverable | `helios_d3d12_static` (the fork's new `static_library` target, which omits `libs/d3d12core/main.c` so no `CreateDXGIFactory1` import can be generated — verified 0 refs). |

⚠ **What does NOT change:** the two Helios entry points (`helios_vkd3d_create_device`,
`helios_vkd3d_serialize_root_signature`) are the same functions with the same signatures — only their
delivery changed from an export to an archive symbol. S4's bridge design is unaffected.

| Stage | Content | Proof the D3D11 path is unharmed | Gates |
|---|---|---|---|
| **S0** | `git submodule update --init --recursive` in `vkd3d-proton-helios` (§8.2); **build vkd3d with the mingw cross file on the Linux host** — `meson setup --cross-file build-win64.txt --buildtype release -Denable_tests=true -Denable_extras=true tmp/dx12/build/vkd3d-win64 && ninja -C …` (§8.3, and `GATES.md` §4.1 verbatim); fix the `/XD vkd3d-proton` entry in `tools/win-mcp/src/main.rs:576` and `:843`. ⚠ **No VM toolchain bring-up and no `win_vkd3d` at this stage** — the whole cross toolchain is already installed on the host, and the artifacts reach the VM by `robocopy` | **No Helios binary is touched.** Success = `libs/d3d12core/d3d12core.dll`, `libs/d3d12/d3d12.dll`, `tests/d3d12.exe`, `demos/{triangle,gears}.exe` exist and are non-empty, hashed into `tmp/dx12/gates/G0/sha256sums.txt` | **G0** |
| **S0c** ✅ **DONE 2026-08-05** (`fc35d37d` exports, `e571d71a` the Windows-bash test-runner fix; both pushed to remote `helios`) | **The vkd3d fork change (§7.4), which nothing else assigns.** Three parts, in this order: (1) wire the fork as a push remote — `.gitmodules` names `https://github.com/rupansh/vkd3d-proton` while the checkout's `origin` is `HansKristian-Work/vkd3d-proton` via the `github-rupansh` SSH alias, so `git -C vkd3d-proton-helios remote add helios <fork>` first and push there, never to `origin` (§8.2); (2) commit `libs/d3d12core/helios_entry.c` + `libs/d3d12core/helios_vkd3d.def` + the `helios_vkd3d_lib` target in `libs/d3d12core/meson.build`; (3) rebuild and confirm the two exports. ⚠ Also read the four licence files the S0 submodule init just made readable and produce §7.4's component table (UNVERIFIED-10) | No Helios binary is touched; the change is entirely inside the pinned submodule. Success = `x86_64-w64-mingw32-objdump -p tmp/dx12/build/vkd3d-win64/libs/d3d12core/helios_vkd3d.dll \| grep -E 'helios_vkd3d_(create_device\|serialize_root_signature)'` prints **both**, and the commit is on the fork remote | **G0** |
| **S0b** | **Headless engine validation** (`DECISIONS.md` D2 — ⛔ *not* an app-local arm; that framing is retired). Write `tools/d3d12_bridge_probe.cpp`: `LoadLibrary("helios_vkd3d.dll")` → `helios_vkd3d_create_device` → clear + one triangle into an offscreen `ID3D12Resource` → copy to a `READBACK` heap → `Map` and verify the pixels. **No `d3d12.dll`, no D3D12 runtime, no DXGI, no swapchain.** Optionally also run the headless vkd3d conformance suite (zero swapchains — verified) as the `D12-G9` baseline. ⚠ P-A is **closed by construction** here, not mitigated: with no app-local DLLs the ICD vehicle cannot pick up a foreign DXGI. The vehicle's bare-name `LoadLibraryA` hardening is still worth doing, but as ordinary stability work, not as a D3D12 prerequisite | Same: no Helios binary changes. This is the engine half of the architecture proven with zero UMD code, and it is the only check standing between a wrong assumption about vkd3d-on-venus and ~200 DDI slots | **G1, G2** |
| **S1** | Create `umd_common`; move the five **zero-behaviour-change** Rust modules (`hr`, `format`, `throttle`, `slot`, `window`) **and the shared C++ header** (§4.4: `bridge_common.h` + `PeriodicStat`/`qpc_elapsed_us`/`ComRelease<T>`/`bridge_guard`). `umd` gains one path dep, `use` statements, and one `.include("../umd_common/bridge")` in its `build.rs` | **`S1-check`** (no `D12-G*` id exists for this — see the note below): (a) the 11 `hr` asserts compile; (b) `rustc --test --edition 2021 -o /tmp/format-table-check tools/format-table-check.rs && /tmp/format-table-check` passes **on Linux**, with the `#[path]` updated in the same commit; (c) `git grep -n 'static_assert' umd/bridge umd12/bridge umd_common/bridge` returns exactly one `bridge_guard` hit; (d) deploy + one Fire Strike run at the standard preset via `helios_fs_std`, 3-run median within the known ±5–6 % spread; (e) zero `helios_umd.dll` entries in the id-1000 Application log for the boot | — (see note) |
| **S2** | Move `log`, `knobs` (reader half), the refusal and noop mechanisms. `log::init(basename)` added, called from `open_adapter_common` (`adapter.rs:218`) | **`S2-check`**: (a) ⚠ `log_knob_inventory()`'s line in `umd-<pid>.log` must be **byte-identical** to before — capture it before the move and `fc /b` after; that line is R1008's own validation instrument (`umd/src/log.rs:226-234`); (b) the P2 fault injection re-run (§4.2): add a bare `crate::log_line("x")` to a `forward/*` file, confirm the build fails with *"use of deprecated function"*, remove it; (c) all ten knob accessors still resolve — `present_batch_fold` is the one a "move the 9" reading drops (§2.1); (d) the same Fire Strike + id-1000 evidence as S1 | — (see note) |
| **S3** | New `umd12` crate: `build.rs` + bindgen + `ddi12.rs` only. `OpenAdapter12` in `umd12` **still refuses** with `DXGI_ERROR_UNSUPPORTED`; `umd` keeps its own refusing `OpenAdapter12`. **Nothing deployed** | Nothing shipped changes. The bindgen layout assertions ARE the deliverable: if `d3d12umddi.rs` compiles, the ABI is machine-checked | **G0** |
| **S4** | `vkd3d_bridge.{h,cpp}` + `bridge12.rs`: `helios_vkd3d_create_device` only, returning a live `ID3D12Device*`. A `tools/` probe `LoadLibrary`s `helios_umd12.dll` directly and calls the bridge — **no runtime, no INF change, no registry change** | Still nothing shipped. First real evidence that vkd3d runs on venus *through our bridge* | **G1** |
| **S4b** | **The ICD anchor (§6.4), which must land before the first two-engine run.** Add `helios_icd_anchor_v1` to both DLLs, route both `resolve_helios_icd_module`s through it, add the `IcdAnchorMismatch` counter and its first-hit `log_error!` | Still nothing shipped in the D3D11 sense beyond one added export on `helios_umd.dll` (a superset change; no existing export moves). Proof = UNVERIFIED-4's probe (§13), promoted from detector to pass criterion: one process creates a D3D11 device *and* calls `helios_vkd3d_create_device`; both modules report the **same** ICD path, both venus context ids are non-zero and **equal**, and `IcdAnchorMismatch` reads 0 | **G1** |
| **S5** | INF + hotplug name `helios_umd12.dll` in slot 3; `umd` **drops** its `OpenAdapter12` export and `umd12`'s becomes reachable and stops refusing — **all in one commit**; the `UmdD3D12` knob lands in that same commit, default OFF | Rollback = revert `UserModeDriverName[3]` (§10-L2). The D3D11 binary changes by exactly one deleted export. Knob absent ⇒ bit-identical to a build without D3D12 | **G6** (knob absent — the split gate is the deploy that registers slot 3), then **G7** (knob ON) |
| **S6** | The D3D12 DDI surface, built out in `forward12/*` — caps first (H4), then device/queue/command-list, then descriptors, then present | Each sub-stage is knob-gated OFF by default until its gate passes | **G3** (DDI arm), **G8–G11** |

**⚠ On the two "—" cells: S1 and S2 have no `D12-G*` id, and that is a gap in the ladder, not a
judgement that they need no proof.** `GATES.md`'s ladder starts the UMD-side sequence at
`D12-G6`, whose subject is the *finished* split (`umd_common` + `umd12` both existing, both
deployed). S1 and S2 are the only two stages in this whole plan that modify **the binary dwm loads
at boot** while G6 is still several stages away, so they are the stages most in need of a
checkpoint. Until `GATES.md` adopts them, their pass criteria are the `S1-check` / `S2-check` lists
in the table above — written as commands, captured under `tmp/dx12/gates/S1/` and `tmp/dx12/gates/S2/`
with the same evidence discipline as a real gate (the byte-identical inventory line, the fault
injection, the Fire Strike medians, the id-1000 log). **Proposed follow-up for `GATES.md`: add
`D12-G5b` (S1) and `D12-G5c` (S2) between `D12-G5` and `D12-G6`**, since G5 (the WARP contract
capture) is stage-independent and leaves that slot free. That is a `GATES.md` edit, not one this
document may make.

**⛔ Non-negotiable ordering rule at S5:** `OpenAdapter12` must stop refusing **in the same commit**
that makes its body reachable, or the body must not be written yet (DECISIONS §7.1). That is what
R908 (`e315d03`) paid for: ~230 lines of unreachable D3D12 scaffolding behind
`#[allow(unreachable_code)]`, including five hand-written `D3d12Ddi*` structs and seven
hand-transcribed `D3D12DDICAPS_TYPE_*` values, deleted because the compiler had already proved they
could never run while they read as a live contract (`umd/src/adapter.rs:104-109`).

**Gate mapping, the other direction** — which stage must be complete before each gate can be
attempted:

| Gate | Meaning | Earliest stage |
|---|---|---|
| G0 | build | S0 (vkd3d, Linux mingw cross) / S0c (the two Helios exports) / S3 (`umd12` bindgen) |
| G1 | engine gate, headless (vkd3d produces correct pixels on venus through `helios_vkd3d_create_device`) | S0b the bridge probe, S4 the same path inside `umd12`, S4b for the two-engine process |
| G2 | substrate conformance (`tests/d3d12.exe`, WARP-baselined) | S0b |
| G3 | first frame | S0b (app-local) / S6 (DDI) |
| G4 | present characterisation | S0b — read with P-B in mind (`helios_umd_get_present_result` returns −1 unconditionally, so every vehicle present takes the worker-serial `wait_last_present` fallback, measured **5.57 ms/frame**) |
| G5 | contract capture (the `d3d10warp.dll` `OpenAdapter12` shim, H1) | independent of all stages — do it early |
| G6 | split gate (`umd_common` + `umd12` exist, D3D11 unregressed) | **S5** — ⚠ *not* S2+S3, which was wrong and made G6 unreachable at the stage it was assigned to. `GATES.md` §4.7 requires that `helios_umd12.dll` *"builds, is signed, installs, and is referenced by `UserModeDriverName[3]`"*, with pass criteria that include a `REG_MULTI_SZ` of **exactly four** entries and deployed-hash capture for **both** DLLs. Nothing is deployed at S3, so the earliest stage that can satisfy it is S5, run with the `UmdD3D12` knob **absent** (which is why the same GATES section also demands `D3D12CreateDevice` still fail). The *content* G6 proves inert comes from S1–S3; the *deploy* it gates on is S5 |
| G7 | DDI device | S5, with the knob ON — plus S4/S4b for the bridge it exercises |
| G8 | DDI first frame | S6 |
| G9 | DDI conformance | S6 |
| G10 | real workload | S6 |
| G11 | stability + packaging + CI | S6 + §9.4 |

---

## 12. What NOT to repeat

One rule per trap, each with its citation. All re-verified this session.

1. **Never hand-transcribe a DDI ABI struct.** Every one comes from the WDK header through bindgen
   with `layout_tests(true)` — `umd/build.rs:104-122`. The deleted D3D12 body had five hand-written
   `D3d12Ddi*` structs and seven hand-transcribed `D3D12DDICAPS_TYPE_*` values
   (`umd/src/adapter.rs:104-109`, commit `e315d03`).
2. **Never let an unknown interface fall into an `else` that fills the largest table.**
   `umd/src/adapter.rs:36-45`: the old `if/else-if/else` treated "unknown or older interface" as
   D3D11.0 and bulk-filled 150 pointer slots into a table sized for 101 or 103 — *"a 376..392 byte
   out-of-bounds write into the runtime's heap."* Fix: a **closed enum** with an exhaustive match and
   a `const _` assert tying it to the advertised version table (`:90-95`).
3. **Never leave a DDI body behind an unconditional early return with `#[allow(unreachable_code)]`.**
   `umd/src/adapter.rs:104-109`, commit `e315d03`. If D3D12 code exists, it is reachable in the
   commit that adds it.
4. **`bridge_guard`-style templates must `static_assert` the sentinel's type against the body's.**
   Commit `ead692e`; `umd/bridge/dxvk_bridge.cpp:296-324`. A bare `0` against a `std::size_t` body
   deduced `R = int` and truncated **every success value**; dwm and LogonUI crash-looped at cold
   boot and nothing warned. §7.2.
5. **Any never-freed process-lifetime handle in the UMD is a per-device leak.** The DLL is
   loaded/unloaded once per device; Rust `static`s are never dropped and the loader closes nothing a
   module opened. `umd/src/lib.rs:45-64`, `umd/src/log.rs:95-116`. Every such resource needs a
   `DllMain(DLL_PROCESS_DETACH, lpReserved == NULL)` release, using `try_lock` (loader lock) and
   counting the refusal (`LOG_CLOSE_CONTENDED`, `log.rs:140`).
6. **A DriverStore copy stale by one build runs dwm's first (composition) device.**
   `tools/hotplug-helios-umd.ps1:105-133`: *"proven 2026-07-03: two different handler generations in
   one dwm process, early devices on the stale DLL."* `win_install_umd12` must sync the DriverStore
   copy too, and verify by hash.
7. **A slot's payload type must be derived from the handle type, never chosen at the call site.**
   `umd/src/forward/handles.rs:17-25`: `load_com::<ID3D11RenderTargetView>(h_rtv.pDrvPrivate)`
   compiled and produced a `ManuallyDrop` whose vtable pointer was a struct field — a wild call on
   first use.
8. **Never let one forwarder file grow past a few hundred lines.** `forward.rs` reached **10 744**
   lines before T8/R1107 split it into 19 modules (commit `70a0438`). `umd12` starts split (§5).
9. **Install order must be structural, not textual.** `umd/src/forward/tables.rs:44-70`: correctness
   of every ≥11.1 device rested on `install()` running before `install_11_1()`, and the wrong order
   produced *"wrong blending for DWM, no counter, no log, only pixels."* The `#[must_use]` `Filled*`
   tokens make the wrong order not compile (commit `12c5097`).
10. **`RelocateDeviceFuncs` is a NOTIFICATION — never refill a live table.**
    `umd/src/device_funcs.rs:756-798` (commit `fa1d75b`): under command lists the runtime relocates
    **twice per `pfnCommandListExecute`** (1 585 160 calls in one Fire Strike run) on the render
    thread while FREETHREADED workers read the same table; the old refill made a concurrent
    `CalcPrivate*Size` transiently return 0 → zero-byte private region → heap corruption.
11. **Declining an unsupported interface is `DXGI_ERROR_UNSUPPORTED`, never
    `DXGI_ERROR_DRIVER_INTERNAL_ERROR`.** `umd/src/hr.rs:51-67`; `umd/src/adapter.rs:182-188`. §10.
12. **A pointer and its capacity are one value.** `umd/src/device_funcs.rs:127-161`: six independent
    `Cell`s let a pointer be updated without its size; `Window<T>` makes that unrepresentable.
13. **Never reintroduce a producer-side CPU present gate.** `umd/src/knobs.rs:31-43`, owner directive
    2026-07-29 — `PresentGateUs`/`PresentOrder` were **deleted** and must not come back. A D3D12
    present path is subject to the same directive (DECISIONS §7.9).
14. **Do not ship an HLSL compiler in the driver DLL.** `umd/Cargo.toml:13-16` — the
    `Win32_Graphics_Direct3D_Fxc` feature is deliberately absent; re-adding it must be justified in
    the commit that does it.
15. **The UMD must not link `dxgi`.** `umd/build.rs:239-243`. The vkd3d equivalent is: never call
    `D3D12GetInterface` or anything reached through it — `d3d12core_CreateDevice`
    (`libs/d3d12core/main.c:742`) → `d3d12core_CreateDeviceFromFactory` (`:643`) →
    `d3d12_get_adapter` (`:375`) → `CreateDXGIFactory1` (`:383`, `:406`). ⛔ Do **not** go looking
    for a `D3D12CreateDevice` in `libs/d3d12core/main.c`: there isn't one. That export is
    `libs/d3d12/main.c:143`, in the thin `d3d12.dll` target Helios never loads, and hunting for it
    in the wrong file is how the entry point that actually matters gets missed. §7.4.
16. **Honour `pfnFillDDITable`'s `SIZE_T` argument; never write `size_of::<T>()` bytes.** This is the
    R702 class (24H2 passing 576 B for a 592-byte `DRIVERCAPS`), and D3D12 parameterises the size
    explicitly (`d3d12umddi.h:2527-2528`). `stub_fill_device_table<T>` derives the slot count from
    the type today (`umd/src/device_funcs.rs:1168-1175`) — the D3D12 version must derive it from the
    **argument**.

---

## 13. UNVERIFIED, with the experiment that settles each

**UNVERIFIED-1 — that `UserModeDriverName` index 3 is served *independently*.** The enum
(`d3dkmthk.h:1830-1839`), Microsoft's "second entry / third entry" doc statements, and the fact that
`OpenAdapter12` is called all point one way, but **all four entries are identical on every adapter on
this box**, so the kernel has never been observed returning a *different* string for index 3.
*Settle:* temporarily set `UserModeDriverName` to four distinct strings (`a.dll,b.dll,c.dll,d.dll`)
and re-run the read-only `D3DKMTQueryAdapterInfo(Type=1)` probe from R11 §1.3; expect
`v=0→a.dll … v=3→d.dll`. ⚠ This is a registry write, needs owner consent, and breaks D3D until
restored. Cheaper alternative, no write: at S5, deploy a `helios_umd12.dll` whose `OpenAdapter12`
only calls `log_self_module_path()` (`umd/src/log.rs:187`) and returns `DXGI_ERROR_UNSUPPORTED`, then
check that `umd12-<pid>.log` appears for a D3D12 client.

**UNVERIFIED-2 — the size and build cost of the `d3d12umddi.h` bindgen generation.** The header is
19 031 lines with 683 `typedef struct` and 399 distinct `PFND3D12DDI_*` typedefs, versus a d3d10umddi
generation of 43k Rust lines / 1.1 MB from 818 types. *Settle:* run S3 and read
`wc -l $OUT_DIR/d3d12umddi.rs` plus the cold `cargo build` wall time. If it hurts, narrow the
allowlist to the implemented DDI versions — never drop `layout_tests(true)`.

**UNVERIFIED-3 — whether robocopy `/XD vkd3d-proton` also excludes `vkd3d-proton-helios`.**
`tools/win-mcp/src/main.rs:576` and `:843` name a bare `vkd3d-proton`, a directory that does not
exist. *Settle:* run one `win_cargo` and check
`Test-Path C:\Users\Rupansh\helios-vgpu\vkd3d-proton-helios\meson.build`. Then fix the entry to name
the real directory either way.

**UNVERIFIED-4 — whether two Helios UMD modules in one process fight over the venus ICD.** Each
module carries its own `resolve_helios_icd_module` static (`bridge_icd_exports.cpp:269-287`) and its
own anchor walk (`:38-67`), and the ICD's `helios_venus_instance_ctx_id` is `_Thread_local`
(`vn_renderer_helios.c:639-644`). Nothing has ever run two of them. *Settle:* at **S4b**, a `tools/`
probe that creates a D3D11 device (loading `helios_umd.dll`) and then calls
`helios_vkd3d_create_device` from `helios_umd12.dll` in the same process, logging both modules'
resolved ICD path and both venus context ids; they must be the same ICD and non-zero, and
`IcdAnchorMismatch` must read 0. ⚠ This is no longer only a detector: §6.4 now **picks** the
mitigation (process-global anchor via the `helios_icd_anchor_v1` export in both DLLs), S4b owns it,
and this probe is S4b's pass criterion.

**UNVERIFIED-5 — whether `helios_umd12.dll` is also loaded/unloaded once per device.** The
measurement (`tools/helios_handle_types.cpp`, `GetModuleHandleW` NO/yes/NO) was taken across
`D3D11CreateDevice` + `Release`. *Settle:* repeat it around `D3D12CreateDevice` + `Release` once S5
lands, reading `GetModuleHandleW(L"helios_umd12.dll")`. This decides whether §3.3's leak-doubling is
real and whether the `helios_vkd3d.dll` pin in §7.3 is mandatory.

**UNVERIFIED-6 — whether DXVK's and vkd3d's SPIR-V/shader libraries can coexist in one image.** Only
relevant under the one-DLL fallback (§3.4). *Settle:* after S0, run
`llvm-nm --defined-only --extern-only` over vkd3d's `libvkd3d-shader.a` + the dxil-spirv archive and
over `C:\Users\Rupansh\dxvk-build\subprojects\dxbc-spirv\libdxbc_spv.a` + `src\spirv\libspirv.a`, and
diff the symbol sets. A non-empty intersection of *defined external* symbols is the answer.

**UNVERIFIED-7 — whether a native MSVC/clang-cl vkd3d build succeeds on *this* VM.** Upstream CI
proves `windows-2022` + VS2022 + Strawberry-Perl `widl` + `glslangValidator` works; this box's exact
combination has never been tried. *Settle:* §8.3's **fallback** commands. ⚠ **Downgraded, and the
downgrade matters:** an earlier revision called this *"the single hardest gate in the plan (G0)"* and
put a VM toolchain bring-up on the critical path. It is neither hard nor on the path — the primary
G0 arm is the **Linux mingw cross build**, whose toolchain is already installed and verified
(§8.3), matching vkd3d-proton's own shipping build and `GATES.md` §4.1. This item is now only:
*if and when a Windows debugger on the vkd3d side is wanted, does the native build work here?* It
blocks nothing until then.

**UNVERIFIED-8 — whether a second cdylib needs any change to the `cargo make` KMD packaging flow.**
The UMD is not built by `Cargo.make.toml`'s core path; it is staged by `[tasks.copy-umd-to-package]`
(`:44-101`) and then **overwritten** by `install-helios-kmd.ps1`'s `Sync-HeliosPackageUmd` before the
catalog is created and signed (`Cargo.make.toml:39-43`). *Settle:* work §9.4's table top to bottom
and confirm `signtool verify /pa /c helios_kmd_render.cat helios_umd12.dll` reports
"Successfully verified" after a full `cargo make` + `install-helios-kmd.ps1`.

**UNVERIFIED-9 — who writes `UserModeDriverNameWow`.** Nothing in this repository does (R11 §1.5:
zero hits outside the docs mirror, `git log --all -S` empty, the 27-July DriverStore INF read
directly and it has only the `UserModeDriverName` line, `setupapi.dev*.log` clean). *Settle:* delete
the value, run a full `tools\install-helios-kmd.ps1`, re-read. If it comes back, SetupAPI's display
class installer synthesises it; if not, it was a one-off manual write and should be `DelReg`'d by the
INF. ⚠ Owner consent — it changes the machine.

**UNVERIFIED-10 — ✅ CLOSED 2026-08-05.** The licence terms of the components that link into
`helios_vkd3d.dll` alongside vkd3d itself are read and tabulated in §7.4. ⚠ **It was three
components in this item's own statement and it is actually six**, because `dxil-spirv` carries four
nested submodules that a non-recursive init leaves out of the tree: `dxbc-spirv`, `SPIRV-Cross`,
`SPIRV-Tools` and a second `spirv-headers` pin. Result: **one LGPL-2.1-or-later component (vkd3d)
and six permissive ones** (MIT ×2, Apache-2.0 ×2, Khronos MIT-style, Apache-2.0-OR-MIT). The DLL
boundary of D4 keeps the LGPL obligations off `helios_umd12.dll`; the six permissive components need
attribution in the package's licence text and nothing more. **The completed table is what goes to the
owner before any `helios_vkd3d.dll` is distributed** — §7.4 already escalates the licence question,
and this is what must be escalated with it.
