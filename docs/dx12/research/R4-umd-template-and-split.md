# R4 — The live Helios D3D11 UMD as the architectural template, and how to split it for D3D12

**Lane:** R4. **Date:** 2026-08-05. **Repo state:** branch `wddm`, HEAD `4739649`, tree clean.
**Method:** every claim below is read out of the tree at the cited `path:line`, or is the output of a
command quoted verbatim. Nothing is recalled from memory. Where I could not verify a thing on this
box it is marked **UNVERIFIED** with the exact read or experiment that settles it.

Scope note: lanes R1 (d3d12umddi surface), R2 (runtime↔UMD contract), R3 (vkd3d internals), R11
(INF/registration), R12 (Vulkan feature gap) own their material. I cite the minimum of theirs that
the split plan cannot be written without, and I say when I am doing it.

---

## 0. Executive summary — the five things that decide the shape

1. **The D3D11 UMD is a `cdylib` that statically links a whole C++ engine and forwards a bindgen'd
   DDI table into it.** `umd/Cargo.toml:8` (`crate-type = ["cdylib"]`), `umd/build.rs:221-236`
   (eight DXVK `.a` archives passed as `rustc-link-arg-cdylib`), `umd/src/forward/tables.rs`
   (195 `f.pfn…` assignments across six installers). The D3D12 UMD wants the same shape with vkd3d
   in place of DXVK — but vkd3d-proton is **C, not C++**, and is not exposed as a static archive
   today (`vkd3d-proton-helios/libs/d3d12core/meson.build:16` builds a `shared_library`).

2. **`helios_umd.dll` is loaded and unloaded ONCE PER D3D11 DEVICE inside dwm**, measured, not
   assumed — `umd/src/lib.rs:45-64` and `umd/src/log.rs:95-116`. That single fact is the strongest
   argument in the one-DLL-vs-two decision: every byte linked into the D3D11 DLL is paid on every
   dwm device create/destroy. Linking vkd3d + dxil-spirv into it makes DWM pay for D3D12.

3. **Two independent by-name export surfaces cross module boundaries, and they resolve differently.**
   The Mesa ICD's vehicle walks **all loaded modules** and takes the first that exports
   `helios_umd_set_present_source` (`icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:705-732`), while
   DXVK's scanout-acquire resolves from **its own containing module**
   (`dxvk-helios/src/dxvk/dxvk_helios_scanout_acquire.cpp:41-51`). A second DLL exporting the same
   `helios_umd_*` names makes the first lookup nondeterministic. This is a hard constraint on the
   split, not a preference.

4. **The infrastructure worth reusing is already factored and mostly D3D-version-agnostic**:
   `hr.rs` (93 lines, explicitly says "the D3D10/11 UMD DDI, **the D3D12 UMD DDI** and the DXGI
   DDI" — `umd/src/hr.rs:1-2`), `knobs.rs` (408), `log.rs` (244), `format.rs` (449),
   `forward/handles.rs` (333, fully generic `Slot<Com<T>>`/`Slot<Boxed<S>>`), `LogThrottle`
   (`umd/src/forward.rs:119-162`), `DdiRefusals` (`umd/src/forward.rs:322-448`). Roughly
   **1 500 lines** move to a shared crate essentially unchanged.

5. **The last D3D12 attempt in this tree was deleted for being unreachable and hand-transcribed**
   (`umd/src/adapter.rs:104-109`, DX12.md §1.2, commit `e315d03`). The revival rule is written down
   already: bindgen from the WDK header with `layout_tests(true)`, and `OpenAdapter12` stops
   refusing **in the same commit** that makes the body reachable.

---

## 1. Anatomy — every file in `umd/src` and `umd/bridge`

LOC from `wc -l` (run 2026-08-05). Classification:
**(a)** D3D11/d3d10umddi-specific · **(b)** generic UMD infrastructure · **(c)** Helios-platform glue
(venus/KMD/scanout/ICD contract, D3D-version-independent but Helios-specific).

### 1.1 `umd/src/*.rs` — 5 781 lines

| File | LOC | Class | Purpose | D3D12 disposition |
|---|---:|:--:|---|---|
| `lib.rs` | 91 | b+a | Module list, `DllMain` (closes the log handle at `DLL_PROCESS_DETACH`, `:65-76`), `#[cfg(not(windows))] compile_error!` (`:25-29`), `#![deny(deprecated)]` log guard (`:16`), crate re-exports | **Template.** `umd12/src/lib.rs` is a near-copy; the `DllMain` body and the deny-guard are verbatim |
| `adapter.rs` | 608 | a | `OpenAdapter10`/`OpenAdapter10_2`/`OpenAdapter12`, `NegotiatedInterface`, `SUPPORTED_DDI_VERSIONS`, `create_device` + `DeviceUnderConstruction` unwind guard, `close_adapter`, `get_supported_versions`, `AdapterToken` | **Split.** `OpenAdapter12` (`:177-189`) moves to `umd12`; the *pattern* (closed-set version enum, guard, token) is copied not shared |
| `caps.rs` | 266 | a | `FeatureProfile` (FL10_0 / FL11_0 / FL11_PIPELINE_ONLY), `get_caps` dispatch over 8 `D3D10_2DDICAPS_TYPE_*` | **Pattern only.** D3D12 caps are a different, much larger enum; copy the *discipline* (one profile struct, compile-time coherence asserts), not the code |
| `ddi.rs` | 80 | a | `include!(OUT_DIR/d3d10umddi.rs)` + 12 hand-pinned `D3D10DDIARG_CREATEDEVICE` offset asserts (`:54-79`) | **Template.** `umd12/src/ddi12.rs` does the same for `d3d12umddi.rs` |
| `device_funcs.rs` | 1318 | a+c | `HeliosDevice` (`:362-399`), `HeliosDeferredContext` (`:408-427`), `BridgeOwned`, `RuntimeContext`/`RuntimePagingQueue`/`Window<T>`, `threading_caps()`, `stub_fill_device_table<T>` (`:1168-1175`), `install_calc_and_lifecycle`, the six `fill_*` entry points, `ddi_noop_device`/`ddi_noop_dxgi`/`ddi_calc_size` | **Mixed.** `stub_fill_device_table<T>`, `Window<T>`, the noop-counter idiom and `RuntimePagingQueue` generalise; the tables and `HeliosDevice`'s D3D11 fields do not |
| `bridge.rs` | 791 | c | The `#[cxx::bridge]` block (`:29-249`, 36 declared C++ methods) + the `BridgeDevice` sealed newtype (`:446-791`) + `SrcRes`/`DstRes` transposition-proof newtypes (`:407-427`) | **Template.** `umd12/src/bridge12.rs` is a second, independent `#[cxx::bridge]` |
| `forward.rs` | 598 | a+b | Module list + shared import surface (`:17-99`), `LogThrottle` (`:119-162`), 15 named `LogThrottle` statics, `DdiRefusals` + `ddi_refusal_summary()` + `note_ddi_refusal()` (`:322-448`), present-skip counters, `run_present_frame_gate` | **Split.** `LogThrottle` and the refusal-registry mechanism → shared crate; the rest is D3D11 |
| `format.rs` | 449 | b | One `FormatInfo` row per DXGI format 0..=115 + 8 readers + `to_d3dddi`/`from_d3dddi` | **Move verbatim** to the shared crate. Already free of WDK/`windows` types by design (`:25-27`) so `tools/format-table-check.rs` can `include!` it on Linux |
| `hr.rs` | 93 | b | 8 HRESULT constants + 11 compile-time value/severity asserts | **Move verbatim.** Its own doc already names the D3D12 DDI as an audience (`:1-2`) |
| `knobs.rs` | 408 | b+a | `reg_dword` (the one audited advapi32 FFI site, `:82-105`), `DwordKnob`/`BoolKnob`, 10 knob statics, `resolved_inventory()`, 9 typed accessors | **Split.** `reg_dword`/`DwordKnob`/`BoolKnob`/inventory → shared; the 10 D3D11 knob *values* stay |
| `log.rs` | 244 | b | `umd_log_path()` (`C:\ProgramData\Helios\umd-<pid>.log`), `log_line` (deprecated marker), `trace_line!`, `log_error!`, `close_at_detach()`, `log_self_module_path()`, `log_knob_inventory()` | **Move to shared** with the file basename parameterised (see §6.2) |
| `scanout_acquire.rs` | 744 | c | D4a read-ledger: escape PROBE/MAP/REGISTER via `pfnEscapeCb`, per-device ledger mapping, and the three `helios_scanout_*` exports DXVK resolves by name (`:610`, `:619`, `:682`) | **Stays in the D3D11 DLL** (its consumer is the statically-linked DXVK). See §6.3 |
| `vehicle_exports.rs` | 84 | c | The three `helios_umd_*` exports the Mesa ICD WSI resolves by name (`:24`, `:51`, `:72`) | **MUST stay in exactly one DLL.** See §6.3 |

### 1.2 `umd/src/forward/*.rs` — 13 276 lines, 20 modules

All are class **(a)** D3D11-specific unless noted. They exist because `forward.rs` was 10 744 lines
before T8/R1107 split it (commit `70a0438`: "forward.rs 10744 -> 827").

| File | LOC | Purpose (first doc line, verbatim) | Note for D3D12 |
|---|---:|---|---|
| `tables.rs` | 307 | "The four device-funcs table writers" | **The structural template.** `Filled11_0`/`Filled11_1`/`FilledWddm1_3` `#[must_use]` tokens make install ORDER a compile error (`:58-70`) |
| `handles.rs` | 333 | "One payload type per DDI handle slot" | **Class (b) — move to shared.** `Slot<P>`, `Com<T>`, `Boxed<S>`, `DdiHandle`, `ComHandle`, `BoxedHandle` are fully generic |
| `resource.rs` | 1602 | "Resource create / open / destroy / resolve, and WDDM allocation" | D3D11 + Helios WDDM alloc glue |
| `present.rs` | 2528 | "The DXGI present path, and the DXGI DDIs that ride with it" | D3D12 presents through DXGI too — R7's lane |
| `state.rs` | 1266 | "Per-object state behind `pDrvPrivate`" | `ResidentAllocation`'s eviction `Drop` is a reusable idea |
| `shaders.rs` | 1070 | "Shader creation, and the DXBC signature flattening the >=11.1 DDI needs" | DXIL replaces DXBC — R8's lane |
| `views.rs` | 1000 | "View creation and the four view-descriptor translators" | D3D12 uses descriptor heaps; not portable |
| `deferred.rs` | 873 | "Native deferred contexts + command lists (Phase C)" | **Closest existing analogue to D3D12 command lists.** Read before designing `umd12`'s command-list path |
| `layout.rs` | 698 | "Input layouts and the vertex-shader input variant cache" | D3D12 folds IA into PSO |
| `transfer.rs` | 574 | "Copy, resolve, map/unmap, flush, discard, clear-view and UpdateSubresource" | |
| `pipeline.rs` | 533 | "Pipeline binding and the draw/dispatch entry points" | |
| `snapshot.rs` | 483 | "D4b ordered-snapshot substitution" | Class (c) Helios glue |
| `bindings.rs` | 470 | "Binding-array collection and the per-stage setter families" | |
| `views`, `state_objects.rs` | 315 | "The immutable pipeline state objects" | |
| `tiles.rs` | 308 | "Tiled resources: the WDDM1.3 tile-mapping … DDIs" | |
| `vehicle.rs` | 297 | "The dcomp-vehicle present producer" | Class (c) |
| `format_caps.rs` | 272 | "`CheckFormatSupport`: the per-format capability answer" | |
| `queries.rs` | 247 | "Queries, predication, multisample quality levels and performance counters" | |
| `alloc.rs` | 107 | "Validated descriptors for the WDDM allocation path" | Class (c) — reusable shape |

**Verified slot counts** (`awk` over each installer body, `grep -c 'f.pfn'`):
`install()` = **144**, `install_11_1()` = **23**, `install_wddm1_3()` = **10**,
`install_dxgi()` = **7**, `install_dxgi_1_1()` = **1**, `install_dxgi_1_3()` = **10**.
Total 195 `f.pfn…` assignments in `tables.rs`.

### 1.3 `umd/bridge/*` — 3 112 lines C++ + `umd/bindgen` + `umd/build-support`

| File | LOC | Class | Purpose |
|---|---:|:--:|---|
| `dxvk_bridge.cpp` | 1773 | c | The engine bridge: `HeliosDxvkDeviceImpl` (`:357-393`), `bridge_guard` (`:304-325`), `create_shader_impl` (`:407-…`), `HeliosStubAdapter` (`:206-233`), `umd_log` (`:257-269`), `helios_dxvk_create_device` (`:1666-1773`), `Logger::s_instance` definition (`:67-71`) |
| `dxvk_bridge.h` | 216 | c | The pimpl'd `HeliosDxvkDevice` complete type cxx needs, 30 methods |
| `bridge_icd_exports.cpp` | 537 | c | Venus ICD discovery: `TH32CS_SNAPMODULE` walk anchored on `helios_venus_memory_alloc_info` (`:38-67`), Vulkan ICD manifest discovery (env `VK_DRIVER_FILES`/`VK_ICD_FILENAMES` + `HKLM/HKCU\SOFTWARE\Khronos\Vulkan\Drivers` + a hardcoded fallback path, `:234-242`), hand-rolled `library_path` JSON parser (`:124-155`), the 8-entry coherent export table (`:296-330`) |
| `bridge_icd_exports.h` | 44 | c | The 8 published readers; carries the **include-order rule** (`:8-13`): DXVK's `vulkan_loader.h` must be the first Vulkan header a TU sees |
| `bridge_dxbc.cpp` | 406 | a | DXBC container synthesis; deliberately compiled **without** any DXVK/COM/device header so "the signature encoder cannot touch the device" is a link-time fact (`:1-13`) |
| `bridge_dxbc.h` | 107 | a | `ShaderBytecode` move-only borrowed/owned blob (`:36-71`), `kSigEntryWords = 5`, the three container factories |
| `bridge_common.h` | 29 | b | `umd_log(const char*)` + `bridge_log_budget()`. Deliberately free of DXVK/COM/Vulkan/WDK includes (`:8-11`) |
| `bindgen/d3d10umddi_wrapper.h` | 20 | a | `#include <windows.h>` + a local `NTSTATUS` typedef + `#include <d3d10umddi.h>` |
| `build-support/dxvk_c_compat.h` | 7 | c | `/FI` shim mapping POSIX `ssize_t` → `SSIZE_T` for libdisplay-info under clang-cl |

---

## 2. The load path — `OpenAdapter` to a draw

### 2.1 The call order

Microsoft's doc for the order (docs corpus, not my inference):
`windows-driver-docs-pr/display/initializing-communication-with-the-direct3d-version-11-ddi.md:17`
— *"The Direct3D runtime next calls the user-mode display driver's **OpenAdapter10_2** function
through the DLL's export table… The **OpenAdapter10_2** function is the DLL's only exported
function."* and `:29` — *"the driver must explicitly list the DDI versions it supports. The Direct3D
runtime calls the user-mode display driver's **GetSupportedVersions** function."*

The code's own order, all in `umd/src/adapter.rs`:

```
1. loader resolves OpenAdapter10_2 by name                    adapter.rs:159
2. open_adapter_common(open_data, with_10_2 = true)           adapter.rs:191
     - null-check open_data and pAdapterFuncs (union read at offset 0)   :195-211
     - log_self_module_path()   ← WHICH DLL is running        :218  (log.rs:187)
     - log_knob_inventory()     ← every knob + resolved value :219  (log.rs:235)
     - scanout_acquire::note_runtime_adapter(open.hRTAdapter.handle)     :225
         ^^ THE ONLY PLACE the runtime ADAPTER handle appears; pfnEscapeCb needs it
     - open.hAdapter = { pDrvPrivate: &ADAPTER_TOKEN }         :227-229
     - install 5 adapter funcs (10_2) or 3 (10.0)              :235-249
     - return S_OK                                             :251
3. runtime calls pfnGetSupportedVersions                      adapter.rs:578
     advertises 3 u64s from SUPPORTED_DDI_VERSIONS             :29-33
       ddi_supported(11,16,1) = D3DWDDM1_3   interface 0x000b_0010
       ddi_supported(11,15,0) = D3D11_1      interface 0x000b_000f
       ddi_supported(11,10,2) = D3D11_0      interface 0x000b_000a
4. runtime calls pfnGetCaps (0..n times)                      caps.rs:142
5. runtime calls pfnCalcPrivateDeviceSize                     adapter.rs:262
     -> device_funcs::device_private_size() = size_of::<HeliosDevice>()   device_funcs.rs:672
6. runtime calls pfnCreateDevice                              adapter.rs:273
```

### 2.2 Inside `create_device` — the exact eight steps

`umd/src/adapter.rs:273-522`:

0. `adapter_ok(h_adapter)` — **report-only**, counts `ADAPTER_UNRECOGNISED` (`:133-148`).
1. Optional raw-word dump of the arg struct, bounded by the *negotiated interface* (10 or 11 words,
   never a literal) — `:310-336`. The bound exists because a 12-word dump reads past a 80-byte
   runtime object at a page end.
2. **Validate before constructing** (`:350-364`): null `hDrvDevice` → `E_FAIL`; null `pDeviceFuncs`
   → `E_FAIL`. Both used to run *after* construction and leaked a whole DXVK/Vulkan device, a
   kernel context and a paging queue per failed attempt.
3. `NegotiatedInterface::from_interface(create.Interface)` — a **closed set**; anything else is
   `E_NOTIMPL` (`:368-377`). See §7 trap 2.
4. `bridge::BridgeDevice::create(0, 0)` (`:382`) → the C++ `helios_dxvk_create_device`.
5. `core::ptr::write(hDrvDevice.pDrvPrivate as *mut HeliosDevice, HeliosDevice { … })` (`:389-424`)
   — in-place construction into the **runtime-allocated** private block.
6. `DeviceUnderConstruction` guard armed (`:428-430`); every later early return tears down through
   `Drop` (`:544-570`), which calls `owned.release()` → `destroy_runtime_objects()` →
   `drop_in_place()` in that order.
7. `create_runtime_context(dev)` then `create_runtime_paging_queue(dev)` (`:434`, `:442`); either
   failing fails the device.
8. Fill the tables, matched arm-by-arm to the negotiated interface (`:465-488`):
   - `Wddm1_3` → `fill_wddm1_3_device_funcs(pWDDM1_3DeviceFuncs)` + `fill_dxgi_1_3_base_funcs`
   - `D3D11_1` → `fill_d3d11_1_device_funcs(p11_1DeviceFuncs)` + `fill_dxgi_1_1_base_funcs`
   - `D3D11_0` → `fill_d3d11_device_funcs(p11DeviceFuncs)` + `fill_dxgi_base_funcs`
9. `guard.defuse()` (`:492`), `forward::register_live_device(...)` (`:495`),
   `scanout_acquire::init_for_device(dev)` → `dev.dxvk.set_scanout_acquire_event(event)`
   (`:503-511`) — deliberately **after** defuse so every acquire failure degrades to
   "feature off for this device" rather than unwinding device creation.

### 2.3 How a table gets filled (the two-phase discipline)

`umd/src/device_funcs.rs:1234-1267`:

```rust
pub unsafe fn fill_wddm1_3_device_funcs(funcs: *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS) {
    let f = &mut *stub_fill_device_table(funcs);   // every slot -> ddi_noop_device
    install_calc_and_lifecycle(f);                 // 15 Calc* + 4 deferred-ctx + DestroyDevice
    (*funcs).pfnRelocateDeviceFuncs = Some(ddi_relocate_device_funcs_wddm1_3);
    let base = crate::forward::install(f);                                    // -> Filled11_0
    let l1   = crate::forward::install_11_1(base, funcs as *mut _);           // -> Filled11_1
    let _l13 = crate::forward::install_wddm1_3(l1, funcs);                    // -> FilledWddm1_3
    audit_wddm1_3_device_funcs("FillDeviceFuncs", funcs);
}
```

`stub_fill_device_table<T>` derives the slot count from `size_of::<T>() / size_of::<usize>()`
(`device_funcs.rs:1168-1175`) so a wrong hand-written length cannot under-stub a table.
The `Filled*` tokens (`tables.rs:58-70`) make `install_11_1(f); install(f);` **not compile** —
correctness of every ≥11.1 device previously rested on textual call order (R1009, commit `12c5097`).

### 2.4 How DXVK is instantiated behind it

`umd/bridge/dxvk_bridge.cpp:1666-1773`, in order:

1. `std::call_once` writes `DXVK_FILTER_DEVICE_NAME=Virtio-GPU Venus` via `_putenv_s` (`:1680-1708`).
   Once, not per-CreateDevice — one process (dwm) makes several devices and `_putenv_s` is not safe
   against concurrent `getenv`.
2. `d.instance = new dxvk::DxvkInstance(dxvk::DxvkInstanceFlags())` (`:1717`).
3. `findAdapterByLuid` if a LUID was given, else `enumAdapters(0)` (`:1719-1734`).
4. `d.device = d.adapter->createDevice()` (`:1736`).
5. `d.venus_ctx_id = read_instance_venus_context_id(d.instance->handle())` (`:1741`) — through
   `bridge_icd_exports`.
6. `new dxvk::D3D11DXGIDevice(stubAdapter.get(), nullptr, nullptr, d.instance, d.adapter, d.device,
   D3D_FEATURE_LEVEL_11_0, 0)` (`:1754-1757`) — DXVK's **full D3D11 COM device**, constructed
   directly rather than through `d3d11.dll`'s exported entry points.
7. `QueryInterface(ID3D11Device)` then `GetImmediateContext` (`:1760-1769`).

This is the crux of the architecture: **Helios never calls DXVK's public DLL entry point.** It links
`libhelios_d3d11_static.a` — a Helios-added meson target
(`dxvk-helios/src/d3d11/meson.build:96-105`: *"Helios: static archive of the full D3D11 COM
implementation so the Helios WDDM UMD can instantiate D3D11DXGIDevice from a DxvkDevice"*) — and
constructs the COM object itself. `umd/build.rs:240-243` states the reason:

> deliberately NOT linking system dxgi. A WDDM UMD sits below DXGI and implements the DXGI DDI; it
> must not depend on dxgi.dll. DXVK's only dxgi.dll call (`CreateDXGIFactory1`) is in
> `d3d11_main.cpp`'s exported d3d11.dll entry points, which we never reference … so that object is
> never pulled out of the static archive.

**The vkd3d parallel is exact and it is verified.** `vkd3d-proton-helios/libs/d3d12core/main.c:383`
and `:406` call `CreateDXGIFactory1` inside `D3D12CreateDevice`'s adapter resolution, and
`libs/d3d12core/meson.build:8` lists `lib_dxgi` as a dependency. A Helios D3D12 bridge must
therefore bypass `d3d12core/main.c` entirely and call
`vkd3d_create_device(const struct vkd3d_device_create_info*, REFIID, void**)`
(`vkd3d-proton-helios/include/vkd3d.h:110`, defined in `libs/vkd3d/vkd3d_main.c:24`) — the same
"construct the COM object from the engine, never through the DLL entry point" move.

---

## 3. The cxx bridge contract

### 3.1 What cxx generates and how it is compiled

`umd/build.rs:183-215`:

```rust
let mut build = cxx_build::bridge("src/bridge.rs");
build.file("bridge/dxvk_bridge.cpp")
     .file("bridge/bridge_dxbc.cpp")
     .file("bridge/bridge_icd_exports.cpp")
     .compiler(&clang_cl)          // C:\Program Files\LLVM\bin\clang-cl.exe
     .archiver(&archiver)          // C:\Program Files\LLVM\bin\llvm-lib.exe
     .std("c++17")
     .flag("/EHsc")                // cxx-build disables exceptions by default; DXVK needs them
     .include("bridge") … 7 more DXVK include dirs …
     .define("_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH", None)
     .define("NOMINMAX", None) .define("WIN32_LEAN_AND_MEAN", None)
     .define("_WIN32_WINNT", "0x0A00") .define("_CRT_SECURE_NO_WARNINGS", None);
build.compile("helios_dxvk_bridge");
```

**Toolchain-coherence rule** (`umd/build.rs:9-13`): DXVK, the cxx shim and the Rust crate must all
use the **MSVC C++ ABI with the dynamic CRT (`/MD`)**. DXVK is built with clang-cl + `-Db_vscrt=md`;
the shim uses the same clang-cl so the objects link. `build.rs:160-173` declares
`HELIOS_CLANG_CL` / `HELIOS_MSVC_LIB` as `rerun-if-env-changed` inputs because *"`cc` and
`cxx-build` do not add a rerun edge for a compiler supplied via `.compiler()`"* — a swapped compiler
otherwise relinked a stale shim against fresh DXVK archives, i.e. mismatched `std::string`/
`std::mutex` layouts inside one DLL. The comment names the remaining hole honestly: it does **not**
catch an in-place LLVM upgrade.

`require_path()` (`build.rs:63-70`) hard-fails at the point a path is chosen, naming the env var
that overrides it — four absolute paths are baked into that script.

### 3.2 The pimpl + opacity contract

`umd/bridge/dxvk_bridge.h:1-10`: cxx's generated glue manages `std::unique_ptr<HeliosDxvkDevice>`
and therefore needs `HeliosDxvkDevice` to be a **complete type** in that header. DXVK headers are
kept out of the header (and out of the generated glue) by pimpl: `HeliosDxvkDevice` is a thin shell
holding `std::unique_ptr<HeliosDxvkDeviceImpl>`, whose destructor is declared in the header and
defined out-of-line in the .cpp where `Impl` is complete.

`HeliosDxvkDeviceImpl` (`dxvk_bridge.cpp:357-393`) owns
`Rc<DxvkInstance> / Rc<DxvkAdapter> / Rc<DxvkDevice>`, the raw `ID3D11Device*` + `ID3D11DeviceContext*`
(released in its destructor, `:389-392`), the venus context id, and the present-order timeline state
(mutex + condvar + `Rc<DxvkFence>`).

### 3.3 How COM objects are held and released across the boundary

The C++ side returns COM pointers as bare `std::size_t`. `umd/src/bridge.rs:251-270` records the
discipline:

> Thirteen bridge methods return a COM pointer as a bare `usize`. **Two are BORROWED** — the bridge
> keeps the owning reference — **and eleven are OWNED** and the Rust side must `Release`.
> … adopting a borrowed pointer → a double release … wrapping an owned pointer in `ManuallyDrop` →
> a leak. Each surfaces as a much later crash in dwm.

Three layers make the wrong adoption unreachable:

- `adopt_resource(raw) -> Option<ID3D11Resource>` (`bridge.rs:284-286`) — the **single**
  `from_raw` for every owning entry point.
- borrowed getters return `ManuallyDrop<T>` (`:294-307`).
- **`BridgeDevice`** (`:446-448`) is a newtype with **no `Deref`** and a private `inner`, because
  *"cxx generates the raw methods as INHERENT methods on the public opaque type, and inherent methods
  of a re-exported public type stay callable regardless of module visibility"* (`:391-405`). This is
  R815, and it is the only encoding that actually seals the raw surface.
- `SrcRes`/`DstRes` newtypes (`:407-427`) make a `present_vehicle_copy(dst, src)` transposition a
  **type error** — the two neighbouring calls took their operands in opposite orders and a
  transposition compiled cleanly on both sides of the FFI.

### 3.4 How errors cross

Two mechanisms, and one of them is a trap.

**`bridge_guard`** (`dxvk_bridge.cpp:304-325`) wraps every fallible bridge body:

```cpp
template <typename R, typename Fn>
R bridge_guard(const char* what, R on_error, Fn&& fn) noexcept {
  static_assert(std::is_same_v<R, decltype(fn())>,
                "bridge_guard's error value must have the guarded body's exact "
                "return type; otherwise the success path is converted too");
  try { return fn(); }
  catch (const dxvk::DxvkError&) { char msg[160]; std::snprintf(msg, sizeof(msg), "%s: DxvkError", what); umd_log(msg); }
  catch (const std::exception& e) { char msg[256]; std::snprintf(msg, sizeof(msg), "%s: exception: %s", what, e.what()); umd_log(msg); }
  catch (...)                     { char msg[160]; std::snprintf(msg, sizeof(msg), "%s: unknown exception", what); umd_log(msg); }
  return on_error;
}
```

Why it must exist at all (`dxvk_bridge.cpp:271-280`): *"cxx emits EVERY generated C++ shim
`noexcept` (verified verbatim in the checked-in generated artifact, `bridge.rs.cc`), so an exception
escaping a bridge method is `std::terminate` — dwm.exe dies instead of the DDI returning a failure."*
The catch arms must not allocate — a `std::string` built inside a `std::bad_alloc` handler can throw
again — hence fixed `char[]` + `snprintf`, and hence `DxvkError::message()` (returns `std::string`)
is deliberately not called.

**THE RECORDED BUG.** Commit `ead692e`, *"umd/bridge: BUG — bridge_guard truncated every returned
pointer to 32 bits"*, 2026-07-28. Verbatim from the commit message:

> `R` is deduced from `on_error` ALONE — the guarded body's return type is not a deduction context.
> Four call sites passed the bare literal `0` against a body returning `std::size_t`, so `R` deduced
> to `int` and `return fn();` narrowed every **SUCCESS** value, not only the error value. Those four
> bodies all return a `reinterpret_cast<std::size_t>` of a live COM pointer:
> `create_shader_sig`, `create_tess_shader_sig`, `open_ddi_texture2d`, `create_ddi_scanout_texture2d`.

Symptom: dwm.exe and LogonUI.exe crash-looping at cold boot with `0xc0000005` at a constant
`Fault offset: 0x8068c`, resolving to
`dxvk::ComObject<ID3D11VertexShader>::AddRefPrivate` under `D3D11CommonContext::VSSetShader`.
Evidence from the box's own logs:

```
T6 (renders):  create_vertex_shader_11_1 ok: raw=0x1cd520fc300 len=4600
T7 (crashes):  create_vertex_shader_11_1 ok: raw=0x7bdb1800   len=4600
```

`0x7bdb2200` is the low 32 bits of a Win64 heap pointer. Nothing warned:
`-Wconversion`/`-Wshorten-64-to-32` are off, and a UMD build already emits ~115 clang warnings.
The fix is the `static_assert`, not the four `std::size_t(0)`s — fault-injected on the host,
restoring one bare `0` fails with `"'int' is not the same as 'long unsigned int'"`.

**Rule for the vkd3d bridge: any `bridge_guard`-equivalent must carry the same `static_assert`, and
the D3D12 bridge must not invent a second guard template without it.**

Second error mechanism: **`E_NOTIMPL`/`E_FAIL` and the `Hresult` set** (`umd/src/hr.rs`) — see §5.1.

### 3.5 `bridge_icd_exports.cpp` — how the bridge reaches the Vulkan ICD

Not through the Vulkan loader. It resolves **private ICD DLL exports by name**, from one
deliberately-selected module:

- `find_helios_icd_in_loaded_modules()` (`:41-67`) — `CreateToolhelp32Snapshot(TH32CS_SNAPMODULE)`,
  then `GetProcAddress(module, "helios_venus_memory_alloc_info")` as the anchor. On a hit it takes
  one reference with `GetModuleHandleExA` — *"Looking up each export independently can mix two ICD
  versions and call a function with a foreign `VkDeviceMemory`/`VkInstance` handle"* (`:49-51`).
- fallback `load_helios_icd_from_manifests()` (`:243-265`): `VK_DRIVER_FILES` / `VK_ICD_FILENAMES`
  → `HKLM\SOFTWARE\Khronos\Vulkan\Drivers` → `HKCU\…` → the hardcoded
  `C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json`, each parsed by a hand-rolled
  `"library_path"` scanner (`:124-155`).
- `resolve_helios_icd_module()` (`:267-289`) caches the **module** in an atomic and — critically —
  *only after success*: "A `std::call_once` or a magic static over a failed resolution would latch an
  early nullptr — the Mesa ICD is not loaded until `new dxvk::DxvkInstance`" (`:293-299`).
- 8 exports in one table (`HeliosIcdExport` enum, `:296-330`):
  `helios_venus_current_ctx_id`, `helios_venus_instance_ctx_id`, `helios_venus_memory_id`,
  `helios_venus_memory_res_id`, `helios_venus_memory_transfer_resource_ownership`,
  `helios_venus_memory_alloc_info`, `helios_venus_memory_vidmm_tracked`,
  `helios_venus_register_present_stream`.

**Include-order landmine** (`bridge_icd_exports.h:8-13`): the header names `VkInstance`/
`VkDeviceMemory` but includes no Vulkan header, because DXVK's own loader shim
(`src/vulkan/vulkan_loader.h`) must be the first Vulkan header a TU sees — pulling
`<vulkan/vulkan.h>` ahead of it breaks `dxvk_device_info.h` and `dxvk_presenter.h` with nine hard
errors. **A vkd3d TU has the opposite constraint** — `vkd3d-proton-helios/include/vkd3d.h:47`
includes `<vulkan/vulkan.h>` directly unless `VKD3D_NO_VULKAN_H` is defined. Two TUs in one DLL with
contradictory first-Vulkan-header rules is a real (and avoidable) hazard: see §6.4.

### 3.6 `bridge_dxbc.cpp` — what it does and why it is separate

Builds a **DXBC container** around the raw SM4/SM5 token stream the D3D11 DDI hands the driver, so
`dxbc-spirv` can consume it. Three factories (`bridge_dxbc.h:85-98`):
`prepare_shader_bytecode` (code chunk only), `prepare_shader_bytecode_with_sigs` (+ real ISGN/OSGN
chunks from the ≥11.1 typed `D3D11_1DDIARG_SIGNATURE_ENTRY2`), and
`prepare_shader_bytecode_with_tess_sigs` (+ a PCSG patch-constant chunk).
`ShaderBytecode` is **move-only** and derives its range from the owner rather than storing a `data`
pointer beside it (`bridge_dxbc.h:16-35`) — the copyable version left a copy's `data` pointing at the
original vector's heap buffer, i.e. a dangling shader blob handed to `CreateVertexShader`.

It is a separate TU on purpose (`bridge_dxbc.cpp:1-13`): compiled **without** `dxvk_instance.h`,
`dxvk_device.h`, `d3d11_device.h`, `d3d11_context_imm.h`, which makes "the signature encoder cannot
touch the DXVK device or the immediate context" a link-time fact and *"makes an off-VM test of
`append_signature_chunk` possible for the first time."*

**D3D12 has no analogue**: DXIL arrives as a DXIL container already (R8's lane). The reusable lesson
is the *firewall* pattern, not the code.

---

## 4. Build and deploy

### 4.1 Build

- **Crate:** `helios_umd`, `crate-type = ["cdylib"]`, edition 2021 (`umd/Cargo.toml`).
  Deps: `cxx = "1"`, `helios_protocol = { path = "../protocol" }`, `windows = "0.58"` with exactly
  four features. `Win32_Graphics_Direct3D_Fxc` is **deliberately absent** (`:13-16`): it pulls
  `D3DCompile` — an HLSL runtime compiler — into a shipped display-driver DLL.
- **Profiles:** both `panic = "abort"`; dev is `opt-level = 1`, release is `lto = "thin"`,
  `opt-level = 2`, `debug = "full"` (needed for the dump_syms + minidump-stackwalk workflow).
- **There is no cargo workspace.** Verified: no root `Cargo.toml`; `umd`, `kmd_render`, `kmd_logic`,
  `protocol` and `tools/win-mcp` are each standalone `[package]`s. Cross-crate reuse today is a
  plain path dependency (`umd/Cargo.toml:12`).
- **bindgen** (`umd/build.rs:76-137`): header `bindgen/d3d10umddi_wrapper.h`, clang args
  `-target x86_64-pc-windows-msvc` + the WDK 10.0.26100.0 `um`/`shared`/`ucrt`/`winrt` include dirs
  (env `HELIOS_WDK_INCLUDE`) + the highest-versioned MSVC include dir (env `HELIOS_MSVC_INCLUDE`).
  Allowlists: types `D3D1[012].*`, `D3DWDDM2.*`, `D3DDDI.*`, `DXGI_?DDI.*`, `PFND3D1.*`; vars
  `D3D1[012].*_DDI_.*`, `D3DWDDM.*`. **`layout_tests(true)`** — the comment records the exact
  counts: *"currently 817 size, 815 alignment and 4704 field offsets across 818 types"*, emitted by
  bindgen 0.70 as `const _: () = { ["Offset of field: X::y"][offset_of!(X, y) - N]; };` so a
  mismatch is an **E0080 during an ordinary `cargo build`**, not a `#[test]` (verified by
  deliberately corrupting one offset). Cost: ~1.1 MB / 43k generated lines.
- **Links:** eight prebuilt DXVK archives by full path via `rustc-link-arg-cdylib`
  (`build.rs:221-236`) — `libhelios_d3d11_static.a` **first** so its engine references resolve
  against `libdxvk.a`, then `libdxvk.a`, `libdxbc_spv.a`, `libspirv.a`, `libutil.a`, `libwsi.a`,
  `libvkcommon.a`, `libdisplay-info.a`. Plus nine system libs
  (`setupapi gdi32 user32 ole32 oleaut32 version advapi32 shell32 cfgmgr32`) — **no dxgi**.
- **Env / target dirs.** `CARGO_TARGET_DIR` must be a **local C: path** on Windows, never `Z:\`
  (CLAUDE.md; `tools/win-mcp/src/main.rs:36-38` cites windows-drivers-rs#481 / OS error 87).
  `LIBCLANG_PATH = C:\Program Files\LLVM\bin` (`main.rs:40`, set by `win_cargo` at `:565`).
- **Command:** `win_cargo crate_dir:"umd" args:["build","--release"]`. It robocopy-mirrors
  `Z:\` → `C:\Users\Rupansh\helios-vgpu` excluding `target/`, all `.git`, and `icd/mesa`
  (`main.rs:561`).
- **Order matters:** engine first, then UMD. `win_dxvk` mirrors `Z:\dxvk-helios` →
  `C:\Users\Rupansh\dxvk-helios` and builds into `C:\Users\Rupansh\dxvk-build`, prepending LLVM to
  `PATH` **before** vcvars64 — the reverse order silently drops MSVC's `lib.exe` because
  *"cmd expands %PATH% at PARSE time"* (`main.rs:734`).

### 4.2 Deploy

`win_install_umd` (`main.rs:779-822`) runs
`powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\hotplug-helios-umd.ps1`
— the Bypass is required because the machine ExecutionPolicy is Restricted, so any other invocation
silently no-ops. Default artifact is `umd\target\release\helios_umd.dll` (release, because debug is
opt-level 1 with no LTO and silently invalidates timing numbers), and both artifact paths are echoed
as the first output lines.

The script (`tools/hotplug-helios-umd.ps1`):

- copies to `C:\ProgramData\HeliosUmd\helios_umd_<first-16-hex-of-sha256>.dll` (`:51`) — a
  **content-addressed name**, which is why the ICD's export lookup walks modules instead of naming
  the DLL (`wsi_common_win32.cpp:705-708`);
- rewrites the display software key with **four identical entries**:
  ```powershell
  $umdNames = @("helios_umd", "helios_umd", "helios_umd", "helios_umd")
  $umdPaths = @($programDataDll, $programDataDll, $programDataDll, $programDataDll)
  New-ItemProperty … -Name "UserModeDriverName"      -PropertyType MultiString -Value $umdPaths -Force
  New-ItemProperty … -Name "InstalledDisplayDrivers" -PropertyType MultiString -Value $umdNames -Force
  ```
  (`:100-103`) — mirroring the INF, `kmd_render/helios_kmd_render.inx:81-82`;
- **also syncs the active DriverStore copy** (`:105-132`), because *"at COLD BOOT dxgkrnl's first
  UMD-path resolution loads the package's `helios_umd.dll` (before the registry override takes
  effect for later device creates), so a stale DriverStore copy means dwm's first — composition —
  device runs an old UMD every boot (proven 2026-07-03: two different handler generations in one dwm
  process)"*;
- verifies the destination SHA256 and **throws** on mismatch (`:158-162`).
- `-KillUmdUsers -RestartDevice -NoProbe` is the flag set that makes the new DLL load immediately;
  without `-RestartDevice` dxgkrnl keeps the path it cached at device start.

### 4.3 Logs

- Rust side: `C:\ProgramData\Helios\umd-<pid>.log`, one handle per DLL instance, unbuffered
  (`umd/src/log.rs:21-30`, `:83-93`). Chosen over `C:\Windows\Temp` because the restricted IddCx
  host process cannot write there.
- C++ bridge side: the **same** file, prefix `[dxvk-bridge] `, opened with
  `_fsopen(..., "a", _SH_DENYNO)` per line (`dxvk_bridge.cpp:245-269`). The sharing mode is
  load-bearing: `fopen_s` opens `_SH_SECURE` and then fails on **every** call because the Rust side
  holds a persistent handle — found the 18th session, when the DriverStore UMD had the strings but
  the logs had no `[dxvk-bridge]` lines.
- DXVK engine: `helios_umd_dxvk.log` via `Logger Logger::s_instance("helios_umd_dxvk.log")`
  (`dxvk_bridge.cpp:67-71`), a frontend-provided global the engine links against (normally defined
  in `d3d11_main.cpp`, which Helios does not build).

---

## 5. Infrastructure worth reusing verbatim, with its exact API

### 5.1 `umd/src/hr.rs` — HRESULTs

```rust
pub type Hresult = i32;                       // deliberately not windows::core::HRESULT
pub const S_OK: Hresult = 0;
pub const E_FAIL: Hresult = 0x8000_4005u32 as Hresult;
pub const E_NOTIMPL: Hresult = 0x8000_4001u32 as Hresult;
pub const E_INVALIDARG: Hresult = 0x8007_0057u32 as Hresult;
pub const E_OUTOFMEMORY: Hresult = 0x8007_000Eu32 as Hresult;
pub const DXGI_ERROR_UNSUPPORTED: Hresult = 0x887A_0004u32 as Hresult;
pub const DXGI_ERROR_DRIVER_INTERNAL_ERROR: Hresult = 0x887A_0020u32 as Hresult;
pub const DXGI_STATUS_NO_REDIRECTION: Hresult = 0x087A_0004u32 as Hresult;
```

Plus 11 `const _: () = assert!(…)` value/severity checks (`:76-93`). **The rule it encodes**
(`:51-67`): declining an unimplemented interface is `DXGI_ERROR_UNSUPPORTED` (0x887A0004), *never*
`DXGI_ERROR_DRIVER_INTERNAL_ERROR` (0x887A0020) — the latter is recorded by the runtime and ETW as a
**driver fault**. Two divergent constant blocks used the same *name* for both values and both printed
as the string "DXGI_ERROR_UNSUPPORTED" in the log, so the divergence was invisible to a triage grep
(R801). `umd/src/adapter.rs:182-188` records that `OpenAdapter12` returned the wrong one until R801.

Day-one D3D12 use: `use helios_umd_common::hr::*;` — nothing changes.

### 5.2 `umd/src/log.rs` — logging

```rust
pub(crate) fn umd_log_path() -> &'static std::path::Path;   // C:\ProgramData\Helios\umd-<pid>.log
#[deprecated] pub(crate) fn log_line(message: &str);        // the unconditional writer
macro_rules! trace_line { … }                               // UmdTrace-gated; args NOT evaluated when off
macro_rules! log_error  { … }                               // always written: errors/one-shots/refusals ONLY
pub(crate) fn close_at_detach();                            // DllMain(DLL_PROCESS_DETACH, lpReserved==NULL)
pub(crate) static LOG_CLOSE_CONTENDED: AtomicUsize;
pub(crate) fn log_self_module_path();                       // once per process; WHICH DLL is this
pub(crate) fn log_knob_inventory();                         // once per process; every knob + value
```

The `#[deprecated]` marker plus `#![deny(deprecated)]` in `lib.rs:16` is a **compile error**, not a
warning: a new per-op site cannot reach the unconditional writer by accident. Verified by fault
injection (`lib.rs:8-15`). Both macros wrap the call in `#[allow(deprecated)]`.

`close_at_detach` uses `try_lock`, not `lock` — DllMain runs under the loader lock — and counts the
refusal in `LOG_CLOSE_CONTENDED` rather than waiting (`log.rs:117-141`). It reads the `OnceLock` with
`get()`, never `get_or_init()`, so it cannot be the call that *creates* the handle it exists to close
(`:143-151`).

### 5.3 `umd/src/knobs.rs` — registry knobs

```rust
fn reg_dword(name: &CStr) -> Option<u32>;                      // the ONE audited advapi32 FFI site
pub(crate) struct DwordKnob { … }  impl DwordKnob { const fn new(&'static CStr, u32); fn get(&self)->u32 }
pub(crate) struct BoolKnob  { … }  impl BoolKnob  { const fn new(&'static CStr, bool); fn get(&self)->bool }
pub(crate) fn resolved_inventory() -> [(&'static str, u32); 10];
```

Hive `HKLM\SOFTWARE\Helios`, flag `RRF_RT_REG_DWORD`, one `OnceLock` per knob (read once per
process). The design point (`:1-14`): **the absent-value default is a constructor argument**, so a
knob cannot be written without stating what "absent" means. `resolved_inventory()` makes the set
enumerable rather than grep-discoverable, and `log_knob_inventory()` puts the resolved values in the
log next to the module path.

⛔ Standing prohibition recorded at `:31-43`: **`PresentGateUs` and `PresentOrder` were DELETED
2026-07-29 by owner directive and must not come back** — never reintroduce a producer-side CPU
present stall. A D3D12 present path is subject to the same directive.

### 5.4 The "DDI refusals" counters

```rust
struct DdiRefusals { /* 11 AtomicUsize fields, each with a doc comment naming the exact refusal */ }
static DDI_REFUSALS: DdiRefusals = …;
pub(crate) fn ddi_refusal_summary() -> String;         // one line carrying all eleven
fn note_ddi_refusal(counter: &AtomicUsize);            // bump; emit the summary on FIRST hit
```

`umd/src/forward.rs:322-448`. Two design facts a D3D12 frontend must copy:

- `note_ddi_refusal` takes `&AtomicUsize`, not a field name, *"keeps the call sites one line and
  makes 'increment without a readout' — the defect this whole item exists to close — impossible to
  write by accident"* (`:439-443`).
- The summary is emitted at `DestroyDevice` **and on the first hit of each counter**, because
  *"an instrument nothing can read is not an instrument"* — three of four earlier scan-out counters
  were process-global atomics that nothing ever loaded (`:401-415`). It is **not** on a per-present
  path.

### 5.5 The noop-DDI hit counters

`umd/src/device_funcs.rs:709-754`:

```rust
static DEVICE_NOOP_LOG_COUNT: AtomicUsize;   static DXGI_NOOP_LOG_COUNT: AtomicUsize;
unsafe extern "C" fn ddi_noop_device(_a: usize) -> usize;   // counter unconditional; I/O trace-gated
unsafe extern "C" fn ddi_noop_dxgi(_a: usize) -> usize;
unsafe extern "C" fn ddi_calc_size(_a: usize) -> usize { 256 }
type UniformFn = unsafe extern "C" fn(usize) -> usize;
```

*"The counter is the WS3 'drive noop-DDI hit counts to zero' metric and stays unconditional. Only the
I/O is gated"* (`:709-716`); the first hit captures 32 frames via `RtlCaptureStackBackTrace`
(`log_backtrace`, `:694-707`) so an unexpected slot is attributable.

### 5.6 `umd/src/format.rs` — the DXGI format table

```rust
pub(crate) fn bytes_per_pixel(format: u32) -> u32;
pub(crate) fn bits_per_sample(format: u32) -> Option<u32>;
pub(crate) fn output_family_bits(format: u32) -> Option<u32>;
pub(crate) fn msaa_ineligible(format: u32) -> bool;
pub(crate) fn resolve_required(format: u32) -> bool;
pub(crate) fn color_typeless_parent(format: u32) -> bool;
pub(crate) fn integer_typed(format: u32) -> bool;
pub(crate) fn to_d3dddi(format: u32) -> u32;     // DXGI -> legacy D3DDDIFORMAT, 0 = UNKNOWN
pub(crate) fn from_d3dddi(format: u32) -> u32;   // total, defaults to BGRA
```

One `FormatInfo` row per format 0..=115 plus a documented out-of-table default row. Deliberately free
of `windows`-crate and WDK types so `tools/format-table-check.rs` can `include!` it and run the
equivalence test **on the Linux host in seconds** (`:25-27`) — `helios_umd` is a `panic="abort"`
cdylib with no test harness. The rows were *generated* by compiling the original eight `match`
bodies and printing their answers, not transcribed (`:19-22`).

### 5.7 `umd/src/forward/handles.rs` — typed DDI handle slots

```rust
pub(crate) trait DdiHandle: Copy { fn drv_private(self) -> *mut c_void; }
pub(crate) trait ComHandle: DdiHandle {}
pub(super)  trait BoxedHandle: DdiHandle { type State; }
pub(crate) struct Com<T: Interface>(PhantomData<fn() -> T>);
pub(crate) struct Boxed<S>(PhantomData<fn() -> S>);
pub(crate) struct Slot<P> { cell: NonNull<*mut c_void>, _payload: PhantomData<fn() -> P> }

impl<P> Slot<P> { unsafe fn from_priv(*mut c_void) -> Option<Self>; unsafe fn word(self) -> usize; unsafe fn clear(self); }
impl<T: Interface> Slot<Com<T>> { unsafe fn store(T); store_raw(usize); load() -> Option<ManuallyDrop<T>>; take() -> Option<T>; release(); }
impl<S> Slot<Boxed<S>> { unsafe fn store(S); get() -> Option<&'static S>; ptr() -> *mut S; take() -> Option<Box<S>>; }
```

The `*mut c_void` → payload cast exists **in this module and nowhere else** (`:47-48`) — the
precondition T8's split of `forward.rs` depended on. `Slot<Com<T>>` has no way to reach a `Box`;
`Slot<Boxed<S>>` has no `load`/`release` that would reinterpret a box pointer as a vtable. The named
invalid sequence it kills: `load_com::<ID3D11RenderTargetView>(h_rtv.pDrvPrivate)` compiled and
yielded a `ManuallyDrop` whose vtable pointer was `RtvState::com_raw` — a wild call on first use
(`:17-25`).

The soundness argument for `Slot<Boxed<S>>::get() -> &'static S` under FREETHREADED is written out at
`:294-301` and depends on the runtime's `CUseCountedObject` first-created/last-destroyed ordering.
**D3D12's object model is different; that argument must be re-derived, not assumed.**

### 5.8 `LogThrottle`

`umd/src/forward.rs:119-162`:

```rust
pub(super) struct LogThrottle { count: AtomicUsize }
impl LogThrottle {
    pub(super) const fn new() -> Self;
    pub(super) fn next(&self) -> usize;                                        // bump, no rate decision
    pub(super) fn peek(&self) -> usize;                                        // read without bumping
    pub(super) fn first_n(&self, first: usize) -> Option<usize>;
    pub(super) fn first_n_then_every(&self, first: usize, every: usize) -> Option<usize>;
    pub(super) fn first_n_then_every_from_one(&self, first: usize, every: usize) -> Option<usize>;
}
```

The budget is a **call argument**, not baked into the static, because eleven statics are shared by
sites with different budgets and giving each its own counter would change every cadence (`:110-118`).

### 5.9 C++-side reusables

`bridge_common.h`: `void umd_log(const char*)` and
`bool bridge_log_budget(std::atomic<uint32_t>&, uint32_t first, uint32_t every)`.
`dxvk_bridge.cpp:134-170`: `class PeriodicStat` (`explicit constexpr PeriodicStat(uint32_t period)`,
period must be a power of two; `std::optional<Sample> record(uint64_t us)`), and
`qpc_elapsed_us(freq, t0, t1)` at `:174-179`. `ComRelease<T>` at `:186-202`.
All are D3D-version-agnostic and belong in the shared bridge TU.

---

## 6. THE SPLIT PLAN

### 6.1 One DLL or two — the decision, argued for this project

**Recommendation: TWO DLLs — keep `helios_umd.dll` (D3D11) untouched, add `helios_umd12.dll`.**

The arguments, project-specific, strongest first.

**(1) Load-time cost inside dwm is real and measured, and it is per-device.**
`helios_umd.dll` is loaded and unloaded **once per D3D11 device** — measured by
`tools/helios_handle_types.cpp` reading `GetModuleHandleW` as NO / yes / NO across one
`D3D11CreateDevice` + `Release`, and corroborated by the once-per-process `UMD module:` line
appearing once per device in the log (`umd/src/lib.rs:45-56`, `umd/src/log.rs:95-106`). dwm creates
several devices. Whatever is linked into that DLL is mapped, relocated and unmapped on every one of
them. Adding vkd3d + dxil-spirv (a whole second shader compiler) to the module dwm's compositor
loads per device is a cost paid by the *working* path for a feature it never uses. A separate DLL is
loaded only by processes that actually create a D3D12 device.

**(2) Risk to the working D3D11 path.** The D3D11 stack is the shipping product: DWM composites the
whole desktop on it, Fire Strike runs. One DLL means every `umd12` compile error, every static-init
order change, every symbol collision and every link-order change is a change to the binary dwm loads
at boot. This project has already lost a session to exactly that class (`ead692e`: a *refactor* of
the C++ bridge crash-looped dwm and LogonUI at cold boot). Two DLLs make the blast radius of D3D12
work exactly zero for the compositor until the INF names the second DLL.

**(3) Independent iteration and rollback.** `tools/hotplug-helios-umd.ps1` already installs by
content hash and rewrites the registry (`:51`, `:100-103`). Two DLLs means
`win_install_umd12` can hot-swap the D3D12 driver **without touching the D3D11 registry entries**,
and rollback is "point slot 4 back at `helios_umd.dll`". With one DLL, every D3D12 iteration
redeploys the compositor's driver and needs `-RestartDevice`, which restarts the display adapter.

**(4) Driver-store deployment cost.** Two DLLs add one `SourceDisksFiles` line, one `CopyFiles`
entry and one changed `UserModeDriverName` element to `kmd_render/helios_kmd_render.inx` (§6.6).
The DriverStore-staleness trap (`hotplug-helios-umd.ps1:105-132`) applies identically to both — it
is not made worse by a second file, it is made *clearer*, because the two files' hashes are
independently checkable.

**(5) Binary size / LTO.** `umd` release is `lto = "thin"` with `debug = "full"`. One DLL means
DXVK + dxbc-spirv + vkd3d + dxil-spirv + SPIRV in a single thin-LTO unit with full debug info, in a
DLL that is mapped and unmapped per dwm device.

**Counter-arguments, stated honestly:**

- *Duplication.* Both DLLs statically link a Vulkan-consuming engine and each carries its own copy of
  the shared crate's code. That is real, and it is the price. The alternative (a third shared DLL)
  adds a load-order and versioning problem this project does not need.
- *Two ICD module-resolution passes per process.* A process using both D3D11 and D3D12 on Helios
  resolves the venus ICD twice (once per DLL's `resolve_helios_icd_module` static). Harmless — the
  cache is per-module and the module is refcounted (`bridge_icd_exports.cpp:52-59`).
- *If the INF cannot name a different DLL in the D3D12 slot*, the two-DLL plan needs a different
  entry mechanism. See below.

**Coordination with R11 — both branches costed.**

- **If `UserModeDriverName` CAN name a different DLL for the D3D12 slot** (my expectation; see
  §6.6): the plan above stands unchanged. `UserModeDriverName` element 4 →
  `%13%\helios_umd12.dll`, `InstalledDisplayDrivers` element 4 → `helios_umd12`, and
  `hotplug-helios-umd.ps1:100-101` becomes
  `@($programDataDll, $programDataDll, $programDataDll, $programData12Dll)`.
- **If it CANNOT** — i.e. all D3D versions must live in the DLL named by the *last* (or a single)
  entry — then the one-DLL plan is forced and the mitigation is:
  1. Everything in §6.2/§6.3 still holds; `umd12` becomes a **library crate** (`rlib`) that `umd`
     depends on and whose `OpenAdapter12` `umd/src/adapter.rs` re-exports with `#[no_mangle]`.
  2. The vkd3d bridge becomes a second `#[cxx::bridge]` **module in the same crate** (cxx supports
     multiple bridges; `cxx_build::bridges([..])`), compiled as extra TUs on the same `cc::Build`
     — the pattern `umd/build.rs:186-189` already uses for `bridge_dxbc.cpp` and
     `bridge_icd_exports.cpp` (*"extra TUs inherit every include and define from this same
     cc::Build, so there is no flag duplication to drift"*).
  3. The link risk in §6.4 becomes load-bearing and must be resolved before any D3D12 code ships,
     because a link failure there breaks the compositor's driver.
  4. Add a `HeliosD3D12` `BoolKnob` (default **OFF**) read in `OpenAdapter12` so the D3D12 surface
     can be killed from the registry without redeploying dwm's DLL. Under two DLLs that knob is
     nice-to-have; under one DLL it is mandatory.

### 6.2 Shared-crate factoring — `umd_common`

New crate `umd_common/` (package `helios_umd_common`, `crate-type = ["rlib"]`, no `build.rs`,
**no WDK dependency**). Path-dependency from both `umd` and `umd12` — no workspace needed, exactly
how `helios_protocol` is consumed today (`umd/Cargo.toml:12`).

| Module | From | Change required |
|---|---|---|
| `umd_common::hr` | `umd/src/hr.rs` (93 L) | none — `pub` instead of `pub const` in a private module |
| `umd_common::log` | `umd/src/log.rs` (244 L) | **one**: `umd_log_path()` takes the file basename. `pub fn init(basename: &'static str)` called once from each DLL's `OpenAdapter*`, so D3D11 keeps `umd-<pid>.log` and D3D12 writes `umd12-<pid>.log`. The `#[deprecated]` marker + macros are re-exported with `#[macro_export]`; each cdylib keeps its own `#![deny(deprecated)]` |
| `umd_common::knobs` | `umd/src/knobs.rs:58-153` (the reader half) | `reg_dword`, `DwordKnob`, `BoolKnob` become `pub`. The **knob set** (`:155-290`) and the 9 typed accessors stay in `umd`; `umd12` declares its own set + its own `resolved_inventory()`. `log_knob_inventory` takes `&[(&str, u32)]` |
| `umd_common::format` | `umd/src/format.rs` (449 L) | none. Keeps its "no `windows`/WDK types" property so `tools/format-table-check.rs` still works |
| `umd_common::throttle` | `umd/src/forward.rs:103-162` | `LogThrottle` becomes `pub` |
| `umd_common::refusals` | `umd/src/forward.rs:439-448` | Generalise: `pub struct RefusalCounter { count: AtomicUsize, name: &'static str }` + `pub fn note(&self, summary: impl Fn() -> String)`. The **eleven D3D11 fields** stay in `umd`; `umd12` declares its own set with the same first-hit-emits-summary rule |
| `umd_common::slot` | `umd/src/forward/handles.rs:177-333` | The generic half (`Slot<P>`, `Com<T>`, `Boxed<S>`) is already free of `ddi::` types — moves verbatim. The three traits stay generic; the two macros `com_handles!`/`boxed_handles!` become `#[macro_export]` with a `$crate`-qualified path so each cdylib invokes them over its **own** `ddi` module |
| `umd_common::noop` | `umd/src/device_funcs.rs:676-754` | `UniformFn`, `log_backtrace`, and a `pub struct NoopTable { count: AtomicUsize, tag: &'static str }` with `stub_fill<T>(*mut T, fn)`. `ddi_calc_size` **does not move** — its 256-byte answer is a D3D11-specific claim |
| `umd_common::window` | `umd/src/device_funcs.rs:127-161` | `Window<T>` (pointer + capacity as one value) — reusable for D3D12's runtime-owned buffers |

Estimated move: **~1 500 lines** with essentially no behaviour change.

**Stays in `umd` (D3D11):** `adapter.rs`, `caps.rs`, `ddi.rs`, `device_funcs.rs`, `bridge.rs`,
all 20 `forward/*` modules, `scanout_acquire.rs`, `vehicle_exports.rs`, the ten D3D11 knob statics,
the eleven `DdiRefusals` fields, and `bridge/*.{h,cpp}`. Net: `umd/src` goes from 5 781 → ~4 300
lines plus the unchanged 13 276 in `forward/`.

**New in `umd12`:**

```
umd12/
├── Cargo.toml                       # cdylib; deps: cxx, helios_protocol, helios_umd_common, windows
├── build.rs                         # bindgen d3d12umddi + cxx_build for the vkd3d bridge
├── bindgen/d3d12umddi_wrapper.h     # windows.h + NTSTATUS typedef + <d3d12umddi.h>
├── bridge/
│   ├── vkd3d_bridge.h               # pimpl'd HeliosVkd3dDevice, complete type for cxx
│   ├── vkd3d_bridge.cpp             # helios_vkd3d_create_device + the guarded methods
│   └── bridge_common12.h            # umd_log/bridge_log_budget for this DLL's log file
└── src/
    ├── lib.rs                       # DllMain, module list, deny(deprecated)
    ├── ddi12.rs                     # include!(OUT_DIR/d3d12umddi.rs) + pinned offset asserts
    ├── adapter12.rs                 # OpenAdapter12, CalcPrivateDeviceSize, CreateDevice, CloseAdapter,
    │                                #   GetSupportedVersions, GetCaps, GetOptionalDDITables, FillDDITable
    ├── caps12.rs                    # the D3D12 caps profile (one struct, coherence asserts)
    ├── device12.rs                  # HeliosD3D12Device + the table fills
    ├── bridge12.rs                  # #[cxx::bridge] + a sealed BridgeDevice12 newtype
    ├── knobs12.rs                   # the umd12 knob set + inventory
    └── forward12/                   # the DDI forwarders, split from day one (see §7 rule 8)
```

### 6.3 The export-surface constraint (this is the part that bites)

Three by-name export surfaces exist today. They resolve **differently**, and the difference decides
what `umd12` may export.

| Export set | Who resolves it | How | Consequence for `umd12` |
|---|---|---|---|
| `helios_umd_set_present_source`, + 2 more (`umd/src/vehicle_exports.rs:24,51,72`) | Mesa ICD WSI | `K32EnumProcessModules` over **all** loaded modules, first hit wins (`icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:711-732`) | **`umd12` MUST NOT export these names.** Two exporters = whichever module the loader enumerates first, silently |
| `helios_scanout_acquire_enabled`, `helios_scanout_ledger_lookup_v2`, `helios_scanout_ledger_snapshot_v2` (`umd/src/scanout_acquire.rs:610,619,682`) | DXVK, statically linked | `GetModuleHandleExW(FROM_ADDRESS \| UNCHANGED_REFCOUNT, &umdExports)` — **its own** module (`dxvk-helios/src/dxvk/dxvk_helios_scanout_acquire.cpp:41-51`) | Safe to duplicate. vkd3d has no consumer, so `umd12` simply omits them (the DXVK side is all-or-nothing: *"a partial surface is a build skew, not a mode"*) |
| `OpenAdapter10`, `OpenAdapter10_2`, `OpenAdapter12` (`umd/src/adapter.rs:150,158,177`) | the D3D runtime loader | by name from the DLL named in `UserModeDriverName` | `umd12` exports `OpenAdapter12` only; `umd` **drops** its `OpenAdapter12` in the same commit the INF points slot 4 elsewhere |

⚠ The `vehicle_exports.rs` file header already says it: *"⚠ ALL THREE MUST KEEP EXISTING …
A UMD-only deploy that drops one kills the vehicle."* Under a two-DLL split the rule becomes:
**exactly one DLL exports them, and it is `helios_umd.dll`.**

### 6.4 The C++ side — one bridge or two, and what breaks if both engines share a DLL

**Two bridges, two crates, no shared C++ TU.** `umd12/bridge/vkd3d_bridge.cpp` is a sibling of
`umd/bridge/dxvk_bridge.cpp`, not an addition to it. Reasons, each verified:

1. **Contradictory Vulkan include-order rules.** `umd/bridge/bridge_icd_exports.h:8-13` states DXVK's
   `src/vulkan/vulkan_loader.h` must be the **first** Vulkan header a TU sees, and that pulling
   `<vulkan/vulkan.h>` ahead of it breaks `dxvk_device_info.h` and `dxvk_presenter.h` with nine hard
   errors. `vkd3d-proton-helios/include/vkd3d.h:43-49` includes `<vulkan/vulkan.h>` directly unless
   `VKD3D_NO_VULKAN_H` is defined. In one TU these fight; in separate TUs they do not.
2. **C vs C++ and different vendored Vulkan headers.** vkd3d-proton vendors
   `khronos/Vulkan-Headers` and `khronos/SPIRV-Headers` as submodules
   (`vkd3d-proton-helios/.gitmodules`); DXVK vendors its own under
   `dxvk-helios/include/vulkan/include` and `include/spirv/include` (named in `umd/build.rs:200-202`).
   Two different `vulkan_core.h` versions in one TU is a version-skew bug generator.
3. **Two shader compilers.** DXVK links `libdxbc_spv.a` + `libspirv.a`
   (`umd/build.rs:226-227`); vkd3d links `dxil-spirv` (`vkd3d-proton-helios/meson.build:177`) and
   `libvkd3d-shader.a` (`libs/vkd3d-shader/meson.build:9`). Both are C++ SPIR-V producers built over
   SPIRV-Headers. **UNVERIFIED** whether they collide at link (see §8), but the risk is concrete and
   the cheap way to not find out is to not link them together.
4. **CRT / ABI.** vkd3d-proton's meson explicitly recognises clang-cl
   (`vkd3d-proton-helios/meson.build:9`: `vkd3d_is_msvc = get_id() == 'msvc' or get_id() == 'clang-cl'`)
   and already uses `c_std=c11, cpp_std=c++17` (`:3`) — the **same** C++ standard the Helios bridge
   compiles with (`umd/build.rs:187`). So the `win_dxvk` toolchain recipe transfers directly. The
   `/MD` requirement is the same and must be stated in `umd12/build.rs`'s doc comment the way
   `umd/build.rs:9-13` states it.
5. **Static-init order.** `dxvk_bridge.cpp:67-71` defines `dxvk::Logger Logger::s_instance` — a
   namespace-scope object with a dynamic initialiser, in the DLL. vkd3d has its own logging
   (`libs/vkd3d-common`). Two engines' global initialisers in one DLL is exactly the kind of
   ordering the T7 crash class lives in. Separate DLLs eliminate the question.

**What the vkd3d bridge must do** (the DXVK bridge's shape, translated):

```cpp
struct HeliosVkd3dDeviceImpl {
  struct vkd3d_instance* instance = nullptr;   // vkd3d_create_instance
  VkPhysicalDevice       physical = VK_NULL_HANDLE;
  ID3D12Device*          d3d12    = nullptr;   // vkd3d_create_device(&info, IID_ID3D12Device, &out)
  std::uint32_t          venus_ctx_id = 0;     // read_instance_venus_context_id(vkd3d_instance_get_vk_instance(...))
  ~HeliosVkd3dDeviceImpl() { if (d3d12) d3d12->Release(); if (instance) vkd3d_instance_decref(instance); }
};
std::unique_ptr<HeliosVkd3dDevice> helios_vkd3d_create_device(std::uint32_t luid_low, std::int32_t luid_high);
```

The public API it needs is all in `vkd3d-proton-helios/include/vkd3d.h`:
`vkd3d_create_instance` (`:104`), `vkd3d_create_device` (`:110`),
`vkd3d_instance_get_vk_instance` (`:107`), `vkd3d_get_vk_device` (`:112`),
`vkd3d_instance_decref` (`:106`), and — for the LUID and adapter identity —
`struct vkd3d_device_create_info { … VkPhysicalDevice vk_physical_device; IUnknown *parent; LUID adapter_luid; … }`
(`:74-94`).

**Two changes are needed in the vkd3d-proton fork itself**, and they are the direct analogue of the
`helios_d3d11_static` target DXVK already carries:

1. Add `helios_d3d12_static = static_library('helios_d3d12_static', …, dependencies: [vkd3d_dep, …])`
   beside `d3d12core_lib` in `libs/d3d12core/meson.build` — **without** `libs/d3d12core/main.c`,
   whose `D3D12CreateDevice` calls `CreateDXGIFactory1` (`main.c:383`, `:406`). The Helios bridge
   calls `vkd3d_create_device` directly, so the dxgi-touching object is never pulled out of the
   archive. This is *exactly* what `umd/build.rs:240-243` documents for DXVK.
2. Initialise the three nested submodules (`subprojects/dxil-spirv`, `khronos/Vulkan-Headers`,
   `khronos/SPIRV-Headers`) — **verified empty today**:
   `ls -la vkd3d-proton-helios/subprojects/dxil-spirv/` → `total 0`, only `.` and `..`.
   **vkd3d-proton cannot be built from this checkout as it stands.**

### 6.5 Cargo / build.rs / bindgen changes

- `umd12/Cargo.toml`: mirror `umd/Cargo.toml` exactly — `crate-type = ["cdylib"]`,
  `panic = "abort"` on both profiles, `lto = "thin"` + `debug = "full"` on release. Keep
  `Win32_Graphics_Direct3D_Fxc` **absent** for the same reason (`umd/Cargo.toml:13-16`); D3D12 must
  not ship `D3DCompile` either.
- `umd12/bindgen/d3d12umddi_wrapper.h`: copy `umd/bindgen/d3d10umddi_wrapper.h` verbatim and change
  the last line to `#include <d3d12umddi.h>`. The local `NTSTATUS` typedef guarded by `#ifndef _NTDEF_`
  is still needed (`d3dkmddi.h` uses `NTSTATUS`, and the um `windows.h` does not define it).
- `umd12/build.rs`: same structure as `umd/build.rs` — `require_path` for every absolute default,
  `rerun-if-env-changed` for `HELIOS_CLANG_CL`/`HELIOS_MSVC_LIB`, `layout_tests(true)`,
  `derive_default(true)`, `generate_comments(false)`. Allowlists become
  `.allowlist_type("D3D12DDI.*")`, `.allowlist_type("PFND3D12DDI.*")`, `.allowlist_type("D3DDDI.*")`,
  `.allowlist_var("D3D12DDI.*")`. **Size warning:** the d3d10umddi generation is ~1.1 MB / 43k lines
  from 818 types; `d3d12umddi.h` is **19 031 lines** with **518** `typedef struct D3D12DDI_*` and
  **5 770** `PFND3D12DDI_` references (measured), so expect a substantially larger generated module
  and a slower cold build. Narrow the allowlist to the DDI versions actually implemented if it hurts.
- New crate `umd_common/Cargo.toml`: `crate-type = ["rlib"]`, deps `windows` (for
  `windows::core::Interface` in `slot`) only. It must build on **Linux** as well as Windows so
  `tools/format-table-check.rs` and any future host-side tests keep working — put the `windows`
  dep behind `[target.'cfg(windows)'.dependencies]` and `#[cfg(windows)]`-gate `slot`/`log`/`knobs`.
- **No workspace.** Adding one would change `CARGO_TARGET_DIR` semantics for `kmd_render` (which has
  its own `Cargo.make.toml` + WDK metadata) and is not needed: path deps already work.

### 6.6 INF and win-mcp tooling changes

INF (`kmd_render/helios_kmd_render.inx`) — **do not edit without explicit instruction**
(CLAUDE.md "Files Not to Touch"). The change, when authorised, is three lines:

```inf
[SourceDisksFiles]             ; lines 20-22 today: helios_kmd_render.sys, helios_umd.dll
helios_umd12.dll = 1,,         ; NEW

[Helios_CopyFiles]             ; lines 42-44 today: helios_kmd_render.sys, helios_umd.dll
helios_umd12.dll               ; NEW

[Helios_DeviceSettings]        ; lines 81-82 today
HKR,, UserModeDriverName,      %REG_MULTI_SZ%, %13%\helios_umd.dll,%13%\helios_umd.dll,%13%\helios_umd.dll,%13%\helios_umd12.dll
HKR,, InstalledDisplayDrivers, %REG_MULTI_SZ%, helios_umd,helios_umd,helios_umd,helios_umd12
```

The slot ordering that makes element 4 the D3D12 one:
`windows-driver-docs-pr/display/enabling-support-for-the-direct3d-version-11-ddi.md:27` shows
`umd9.dll, umd10.dll, umd11.dll` — element 1 = D3D9, 2 = D3D10, 3 = D3D11. Element 4 = D3D12 is the
natural extension **and matches what this INF already writes** (four identical entries,
`:81-82`), but the docs corpus in this repo contains **no** D3D12 example — see §8, UNVERIFIED-1.
This is R11's call.

`tools/hotplug-helios-umd.ps1` — add a `-Umd12Dll` parameter and change `:100-101` to
`@("helios_umd","helios_umd","helios_umd","helios_umd12")` /
`@($programDataDll,$programDataDll,$programDataDll,$programData12Dll)`, with the same
`Copy-HeliosFileVerified` + SHA256 check + DriverStore sync for the second file. Keep the
content-addressed name (`helios_umd12_<hash16>.dll`).

`tools/win-mcp/src/main.rs` — three additions, each modelled on an existing tool:

| New tool | Model | Constants to add |
|---|---|---|
| `win_vkd3d` | `win_dxvk` (`:734-777`) — LLVM on PATH **before** vcvars64 | `VKD3D_SRC = "Z:\\vkd3d-proton-helios"`, `VKD3D_MIRROR = "C:\\Users\\Rupansh\\vkd3d-proton-helios"`, `VKD3D_BUILD = "C:\\Users\\Rupansh\\vkd3d-build"` |
| `win_install_umd12` | `win_install_umd` (`:779-822`) — explicit artifact paths echoed in the output, `-ExecutionPolicy Bypass` | `DEFAULT_UMD12_DLL = "umd12\\target\\release\\helios_umd12.dll"` |
| (extend `win_cargo`) | already generic over `crate_dir` (`:563`) — `crate_dir:"umd12"` works with **no change**, but its `/XD` exclusion list (`:573`, `:840`) names a bare `vkd3d-proton` from an older layout and should be corrected to `vkd3d-proton-helios` | — |

### 6.7 Staged migration order — the working D3D11 path is never broken

Each stage is independently buildable, deployable and revertible. Stages 1–2 do not touch any shipped
binary's behaviour.

| Stage | Content | Proof it is safe |
|---|---|---|
| **S0** | Init the three nested submodules in `vkd3d-proton-helios`; add `helios_d3d12_static` to `libs/d3d12core/meson.build`; add `win_vkd3d`; build it | Touches no Helios binary. Success criterion: `libhelios_d3d12_static.a` exists and does **not** contain `d3d12core/main.c`'s object (`llvm-nm` shows no `CreateDXGIFactory1` import) |
| **S1** | Create `umd_common`; move `hr`, `format`, `throttle`, `slot`, `window` (the five with **zero** behaviour change). `umd` gains a path dep and `use` statements | The 11 `hr` asserts and the `format` equivalence test still pass; `tools/format-table-check.rs` still runs on Linux. Deploy + one Fire Strike run to confirm no regression |
| **S2** | Move `log`, `knobs` (reader half), the refusal/noop mechanisms. `log::init(basename)` added, called from `open_adapter_common` | `log_knob_inventory()` output in `umd-<pid>.log` must be **byte-identical** to before — that line is R1008's own validation instrument (`log.rs:226-234`) |
| **S3** | New `umd12` crate: `build.rs` + bindgen only. `OpenAdapter12` in `umd12` **still refuses** with `DXGI_ERROR_UNSUPPORTED`, exactly like `umd/src/adapter.rs:177-189` does today. `umd` keeps its own refusing `OpenAdapter12`. Nothing deployed | The bindgen layout assertions are the deliverable: if `d3d12umddi.rs` compiles, the ABI is machine-checked |
| **S4** | `vkd3d_bridge.{h,cpp}` + `bridge12.rs`: `helios_vkd3d_create_device` only, returning a live `ID3D12Device*`. A `tools/` probe loads `helios_umd12.dll` directly and calls the bridge (no runtime, no INF change) | First real evidence that vkd3d runs on venus. R12's Vulkan-gap answers land here |
| **S5** | INF + hotplug script name `helios_umd12.dll` in slot 4; `umd` **drops** its `OpenAdapter12` export in the **same commit**; `umd12`'s `OpenAdapter12` becomes reachable in the **same commit** it stops refusing (DX12.md §5 risk 1). Add the `HeliosD3D12` kill-switch knob | Rollback = revert the INF element. D3D11 binary unchanged apart from one deleted export |
| **S6+** | The D3D12 DDI surface, split into `forward12/*` from day one | — |

**Non-negotiable ordering rule at S5:** `OpenAdapter12` must stop refusing in the same commit that
makes its body reachable, or the body must not be written yet. That is DX12.md §5 risk 1, and it is
the whole reason R908 deleted ~230 lines.

---

## 7. What NOT to repeat — one rule per trap, each with its citation

1. **Never hand-transcribe a DDI ABI struct.** Every one comes from the WDK header through bindgen
   with `layout_tests(true)`. `umd/build.rs:104-122`; DX12.md §5 risk 4.
   *(The deleted D3D12 body had five hand-written `D3d12Ddi*` structs and seven hand-transcribed
   `D3D12DDICAPS_TYPE_*` values — `umd/src/adapter.rs:104-109`.)*

2. **Never let an unknown interface fall into an `else` that fills the largest table.**
   `umd/src/adapter.rs:36-45`: the old `if/else-if/else` treated "unknown or older interface" as
   D3D11.0 and bulk-filled 150 pointer slots into a table the runtime had sized for 101 or 103 —
   *"a 376..392 byte out-of-bounds write into the runtime's heap."* The fix is a **closed enum** with
   an exhaustive match and a `const _` assert tying it to the advertised version table
   (`adapter.rs:90-95`).

3. **Never leave a DDI body behind an unconditional early return with `#[allow(unreachable_code)]`.**
   `umd/src/adapter.rs:104-109` / DX12.md §1.2 / commit `e315d03`. If D3D12 code exists, it is
   reachable in the commit that adds it.

4. **`bridge_guard`-style templates must `static_assert` the sentinel's type against the body's.**
   Commit `ead692e`; `umd/bridge/dxvk_bridge.cpp:297-308`. A bare `0` against a `std::size_t` body
   deduced `R = int` and truncated **every success value** — dwm and LogonUI crash-looped at cold
   boot. `-Wconversion` is off and a UMD build already emits ~115 warnings, so nothing warned.

5. **Any never-freed process-lifetime handle in the UMD is a per-device leak.**
   `helios_umd.dll` is loaded/unloaded once per D3D11 device; Rust `static`s are never dropped and
   the loader closes nothing a module opened. `umd/src/lib.rs:45-64`, `umd/src/log.rs:95-116`.
   Every such resource needs a `DllMain(DLL_PROCESS_DETACH, lpReserved==NULL)` release, using
   `try_lock` (loader lock) and counting the refusal.

6. **A DriverStore copy that is stale by one build runs dwm's first (composition) device.**
   `tools/hotplug-helios-umd.ps1:105-132`: *"proven 2026-07-03: two different handler generations in
   one dwm process, early devices on the stale DLL."* Any `win_install_umd12` must sync the
   DriverStore copy too, and verify by hash.

7. **A slot's payload type must be derived from the handle type, never chosen at the call site.**
   `umd/src/forward/handles.rs:17-25`: `load_com::<ID3D11RenderTargetView>(h_rtv.pDrvPrivate)`
   compiled and produced a `ManuallyDrop` whose vtable pointer was a struct field — a wild call.

8. **Never let one forwarder file grow past a few hundred lines.** `forward.rs` reached
   **10 744** lines before T8/R1107 split it into 20 modules (commit `70a0438`). `umd12` starts
   split.

9. **Install order must be structural, not textual.** `umd/src/forward/tables.rs:44-70`: correctness
   of every ≥11.1 device rested on `install()` being called before `install_11_1()`, and a wrong
   order produced *"wrong blending for DWM, no counter, no log, only pixels."* The `#[must_use]`
   `Filled*` tokens make the wrong order not compile.

10. **`RelocateDeviceFuncs` is a NOTIFICATION — never refill a live table.**
    `umd/src/device_funcs.rs:756-770` (commit `fa1d75b`): under command lists the runtime relocates
    **twice per `pfnCommandListExecute`** (measured 1 585 160 calls in one Fire Strike run) on the
    render thread while FREETHREADED workers read the same table; the old refill made a concurrent
    `CalcPrivate*Size` transiently return 0 → zero-byte private region → heap corruption.

11. **Declining an unsupported interface is `DXGI_ERROR_UNSUPPORTED`, never
    `DXGI_ERROR_DRIVER_INTERNAL_ERROR`.** `umd/src/hr.rs:51-67`; `umd/src/adapter.rs:182-188`
    records that `OpenAdapter12` got this wrong until R801, so a normal negotiation was recorded by
    the runtime and ETW as a **driver fault**.

12. **A pointer and its capacity are one value.** `umd/src/device_funcs.rs:127-141`: six independent
    `Cell`s let a pointer be updated without its size; `Window<T>` makes that unrepresentable.
    Same idea as `RuntimeContext`/`RuntimePagingQueue` (`:115-182`).

13. **Never reintroduce a producer-side CPU present gate.** `umd/src/knobs.rs:31-43`, owner directive
    2026-07-29. Ordering belongs on the GPU timeline, not on a blocked CPU thread.

14. **Do not ship an HLSL compiler in the driver DLL.** `umd/Cargo.toml:13-16` — the
    `Win32_Graphics_Direct3D_Fxc` feature is deliberately absent, and re-adding it must be justified
    in the commit that does it.

15. **The UMD must not link `dxgi`.** `umd/build.rs:240-243`. The vkd3d equivalent is: do not pull
    `libs/d3d12core/main.c` into the static archive — its `D3D12CreateDevice` calls
    `CreateDXGIFactory1` (`vkd3d-proton-helios/libs/d3d12core/main.c:383`, `:406`).

---

## 8. UNVERIFIED — and the exact experiment that settles each

**UNVERIFIED-1 — that `UserModeDriverName` element 4 is the D3D12 slot.**
The docs corpus in this repo documents elements 1/2/3 = D3D9/D3D10/D3D11
(`windows-driver-docs-pr/display/enabling-support-for-the-direct3d-version-11-ddi.md:27`) and
contains **no** D3D12 example — `grep -rn "OpenAdapter12" windows-driver-docs-research-only/…/display/`
returns nothing. The Helios INF already writes four identical entries
(`kmd_render/helios_kmd_render.inx:81-82`), which is consistent with but does not prove the mapping.
**Settling experiment (R11's lane):** set element 4 to a *distinct* DLL that logs on load and exports
only `OpenAdapter12`, restart the device, run any D3D12 app, and check whether that DLL's
`log_self_module_path()` line appears; or read `Microsoft-Windows-DxgKrnl` ETW for the UMD path
dxgkrnl resolves for the D3D12 slot.

**UNVERIFIED-2 — that DXVK's SPIR-V/DXBC libraries and vkd3d's dxil-spirv/vkd3d-shader can coexist in
one DLL.** They are separate C++ codebases both built over SPIRV-Headers. I did not link them.
**Settling experiment:** after S0 builds `libhelios_d3d12_static.a`, run
`llvm-nm --defined-only --extern-only` over it and over
`C:\Users\Rupansh\dxvk-build\subprojects\dxbc-spirv\libdxbc_spv.a` + `src\spirv\libspirv.a`, and diff
the symbol sets. A non-empty intersection of *defined external* symbols is the answer. (Only needed
if the one-DLL branch of §6.1 is forced.)

**UNVERIFIED-3 — that vkd3d-proton builds under clang-cl at all on this box.**
`vkd3d-proton-helios/meson.build:9` recognises `clang-cl`, and `:3` sets `c_std=c11, cpp_std=c++17`
— the same C++ standard the Helios bridge uses. But the project's own build docs are
`build-win32.txt` / `build-win64.txt` (MinGW), and **its three nested submodules are empty on this
checkout** (`ls -la vkd3d-proton-helios/subprojects/dxil-spirv/` → `total 0`).
**Settling experiment:** `git submodule update --init --recursive` in `vkd3d-proton-helios`, then
`win_vkd3d` with the `icd/win-build/clang-cl-native.ini`-style cross file; a clean
`meson setup` + `ninja` is the answer. **This is the single hardest gate in the plan and it should be
S0, before any Helios code is written.**

**UNVERIFIED-4 — the D3D12 runtime's exact call order and which `D3D12DDI_DEVICE_FUNCS_CORE_*`
version it negotiates.** I read `D3D12DDIARG_OPENADAPTER` / `D3D12DDI_ADAPTERFUNCS`
(`tmp/dx12/sdk/d3d12umddi.h:2674-2694`, 8 adapter funcs including `pfnGetOptionalDDITables` and
`pfnFillDDITable` which have **no** D3D11 analogue), and counted 518
`typedef struct D3D12DDI_*` with at least eleven `D3D12DDI_DEVICE_FUNCS_CORE_00xx` versions
(0003, 0010, 0012, 0013, 0014, 0021, 0022, 0023, 0025, 0026, …). **This is R1/R2's lane** — I did not
work out the negotiation. The split plan is insensitive to the answer.

**UNVERIFIED-5 — whether `helios_umd.dll`'s once-per-device load/unload also holds for a D3D12
device.** The measurement (`tools/helios_handle_types.cpp`) was taken across
`D3D11CreateDevice` + `Release`. **Settling experiment:** the same probe around
`D3D12CreateDevice` + `Release` once S5 lands, reading `GetModuleHandleW("helios_umd12.dll")`
before/during/after.

**UNVERIFIED-6 — whether a second cdylib in the same DriverStore package needs any change to the
`cargo make` KMD packaging flow.** The UMD is not built by `Cargo.make.toml`; it is copied in by
`hotplug-helios-umd.ps1`, and `WINDOWS_CI_PACKAGE.md` / `packaging/windows/Install-Helios.ps1` were
not read for this lane. **Settling experiment:** grep `packaging/windows/Install-Helios.ps1` and
`ci/windows/` for `helios_umd.dll` and enumerate every site that would need a second entry.

---

## 9. Load-bearing facts other lanes must not contradict

1. `helios_umd.dll` is `crate-type = ["cdylib"]`, `panic = "abort"` on both profiles, and links
   **eight** DXVK static archives plus nine system libs, **not** `dxgi`.
   (`umd/Cargo.toml:8,28-38`; `umd/build.rs:221-249`)
2. The D3D11 DDI structs are **bindgen-generated with `layout_tests(true)`** — 817 size / 815
   alignment / 4704 offset assertions over 818 types, checked at **build** time, not test time.
   (`umd/build.rs:104-122`)
3. Helios **never** calls DXVK's exported DLL entry points; it links a Helios-added static archive
   (`libhelios_d3d11_static.a`, `dxvk-helios/src/d3d11/meson.build:96-105`) and constructs
   `dxvk::D3D11DXGIDevice` directly (`umd/bridge/dxvk_bridge.cpp:1754-1757`). The vkd3d analogue must
   call `vkd3d_create_device` (`vkd3d-proton-helios/include/vkd3d.h:110`), never
   `D3D12CreateDevice` in `libs/d3d12core/main.c`.
4. The UMD DLL is loaded and **unloaded once per D3D11 device**. (`umd/src/lib.rs:45-56`)
5. The Mesa ICD resolves `helios_umd_*` by walking **all** loaded modules, first hit wins
   (`icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:711-732`); DXVK resolves `helios_scanout_*` from
   **its own** module (`dxvk-helios/src/dxvk/dxvk_helios_scanout_acquire.cpp:41-51`).
6. There is **no cargo workspace**; cross-crate sharing is a path dependency.
7. `vkd3d-proton-helios`'s three nested submodules (`subprojects/dxil-spirv`,
   `khronos/Vulkan-Headers`, `khronos/SPIRV-Headers`) are **empty** on this checkout — it cannot be
   built as it stands.
8. `vkd3d-proton` builds `vkd3d`, `vkd3d-shader` and `vkd3d-common` as **static** libraries and
   `d3d12`/`d3d12core` as **shared** libraries (`libs/*/meson.build`). Only the shared half touches
   dxgi.
