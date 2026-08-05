# DECISIONS.md — the settled D3D12 decisions, and the merge that produced them

**Status:** these are the decisions the rest of `docs/dx12/` is written against. Nothing in this
directory may contradict this file. If evidence later overturns a decision here, change it *here*
first and then propagate.

**Provenance:** twelve independent research lanes (`docs/dx12/research/R1..R12`), each
adversarially fact-checked by a second reader, merged 2026-08-05. Where two lanes disagreed, §6
records the resolution and the evidence that settled it.

⚠ **Line numbers into `d3d12umddi.h` are pinned to Windows SDK 10.0.26100.0.** That header is not
committed (it is Microsoft's). Re-stage it before reading any citation:

```powershell
# win_exec, once per machine
New-Item -ItemType Directory -Force -Path Z:\tmp\dx12\sdk | Out-Null
$src = "C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0"
foreach($f in @("um\d3d12umddi.h","um\d3d12.h","um\d3dumddi.h","shared\d3dkmthk.h","shared\d3dkmdt.h","shared\d3dukmdt.h","shared\d3dkmddi.h","km\dispmprt.h")) {
  Copy-Item "$src\$f" "Z:\tmp\dx12\sdk\$(Split-Path $f -Leaf)" -Force }
```

---

## 1. The architecture

**Decision D1 — Helios ships a real D3D12 user-mode display driver, `helios_umd12.dll`, that
implements `d3d12umddi.h` and forwards into vkd3d-proton's `ID3D12*` COM objects.** This is the
D3D11 architecture with two boxes swapped:

```
              D3D11 (shipping)                          D3D12 (target)
  app / dwm                                   app / dwm
    │  IDXGISwapChain, ID3D11Device             │  IDXGISwapChain, ID3D12Device
    ▼                                           ▼
  MS d3d11.dll + dxgi.dll                     MS d3d12.dll + D3D12Core.dll + dxgi.dll
    │  d3d10umddi DDI                           │  d3d12umddi DDI
    ▼  (UserModeDriverName[2])                  ▼  (UserModeDriverName[3])
  helios_umd.dll  (Rust)                      helios_umd12.dll  (Rust)          ← NEW
    │  cxx bridge                               │  cxx bridge
    ▼                                           ▼
  DXVK engine (ID3D11Device COM)              vkd3d-proton engine (ID3D12Device COM)
    │  Vulkan                                   │  Vulkan
    ▼                                           ▼
            Mesa venus ICD  (icd/mesa)  —  D3DKMTEscape(HELIOS_ESCAPE_SUBMIT_VENUS)
                                    │
                                    ▼
            kmd_render → virtio-gpu → virglrenderer → host GPU → SET_SCANOUT_BLOB → QEMU
```

Every layer below the bridge is **already shipping and already proven**: it is the same ICD, the
same escape, the same KMD, the same scanout that composite the desktop today.

**Why a driver and not just shipping vkd3d's DLLs next to apps:** there is no supported system-wide
`d3d12.dll` replacement on Windows 11. `System32\d3d12.dll` is WRP/TrustedInstaller-protected; the
Agility SDK's `D3D12SDKVersion`/`D3D12SDKPath` mechanism is keyed off the **application's own
exports** and only swaps `D3D12Core.dll`, not the driver. App-local DLL placement reaches only apps
whose directory you control — never dwm, never Store apps, never an unmodified installer.
(`research/R10.md` Q6, `research/R11.md` §5.2.) A driver is the only shape that makes *the adapter*
D3D12-capable.

**Decision D2 — go straight for the UMD. There is no app-facing vkd3d arm.**
*(Owner directive, 2026-08-05: "I don't want to spend time on app-facing d3d12, we should aim for
the umd directly." This supersedes the earlier plan, which made app-local vkd3d DLLs a Phase 0
milestone with on-screen gates.)*

⛔ **Helios never ships, deploys, or measures vkd3d's `d3d12.dll`/`d3d12core.dll` as an
application's D3D12.** Everything app-facing goes through Microsoft's `d3d12.dll` +
`D3D12Core.dll` + `dxgi.dll` and lands on `helios_umd12.dll` via `UserModeDriverName[3]`, exactly
as D3D11 does today.

**Three things fall out of this, and they are the reason it is the right call:**

1. **DXVK's `dxgi.dll` is not needed anywhere, and never was for the shipping path.**
   `umd/build.rs:238-243` states the rule: *"a WDDM UMD sits below DXGI and implements the DXGI DDI;
   it must not depend on dxgi.dll."* The D3D11 UMD fills `DXGI_DDI_BASE_FUNCTIONS` (18 slots,
   `umd/src/forward/tables.rs:12-40`) and **Microsoft's** `dxgi.dll` is the frontend. DXVK's own
   `dxgi.dll` is not built in this tree (only `dxgi.dll.p` exists), not deployed by any script, and
   referenced by nothing. D3D12 inherits that exactly: the app's `ID3D12CommandQueue` is the
   **runtime's**, which MS DXGI understands natively, and present arrives at `pfnPresent` on the
   command-list table. ⚠ **Correction from `D12-G5`: and NOT on `D3D12DDI_TABLE_TYPE_DXGI` (=3),
   which is never requested at all** — `pfnFillDDITable` was never called with type 3, and 0 of 32
   armed DXGI thunks fired across 20 flip-model presents. A D3D12 UMD on this build needs **no DXGI
   table** (`DDI_REFERENCE.md` §2.3). vkd3d sits *behind* the DDI as an engine, so
   `IDXGIVkSwapChainFactory` is never queried and `swapchain.c` is never entered — just as DXVK's own
   swapchain factory is never used today.
2. **P-A is deleted, not mitigated.** The whole hazard (the ICD vehicle picking up an app-local DXVK
   DXGI and silently demoting frames to the software GDI blit) existed only because an app directory
   would have contained a DXVK `dxgi.dll`. No app-local DLLs ⇒ no hazard. The ICD's bare-name
   `LoadLibraryA("dxgi.dll")` is still worth hardening on its own merits, but it is no longer on any
   critical path. See H2.
3. **No dual-arm maintenance.** One present path, one conformance target, one deployment story.

⚠ **What this costs, stated plainly:** the dropped arm was the only way to answer *"does vkd3d
actually run on venus?"* **before** ~200 DDI slots are written on the assumption that it does. That
question does not go away, so D2 replaces it with two **headless** substitutes that need no DXGI, no
swapchain, no vehicle and no app-facing D3D12 at all:

- **The bridge probe (mandatory, gate `D12-G1`) — ✅ GREEN 2026-08-05.** A `tools/` program that
  `LoadLibrary`s `helios_vkd3d.dll`, calls `helios_vkd3d_create_device`, renders to an offscreen
  `ID3D12Resource` and reads it back. No `d3d12.dll`, no D3D12 runtime, no DXGI — it exercises
  *exactly* the engine path `umd12` will use, one layer below it. This is stage S4 in
  `ARCHITECTURE.md` §11 and it is the cheapest real answer available.
  **Result:** `tools/d3d12_bridge_probe.cpp`, 28 steps, 0 failures, first run — both Helios exports
  resolved, device created with no DXGI, root signature round-tripped through
  `helios_vkd3d_serialize_root_signature`, a DXIL SM 6.0 triangle drawn into a committed
  `R8G8B8A8_UNORM` target, copied to a `READBACK` heap and verified exact at five sample points
  (clear `32,96,192,255`, triangle `255,128,64,255`), fence signalled, device released to refcount 0.
  ⇒ **vkd3d runs on venus.** The ~200-slot assumption is no longer an assumption.
  (`tmp/dx12/gates/G1/bridge_probe.txt`.)
- **The headless conformance baseline (recommended, gate `D12-G2`).** `vkd3d-proton-helios/tests/`
  creates **zero** swapchains — verified, `grep -rl CreateSwapChain tests/` is empty — so the suite
  is fully headless and needs no DXGI. It does resolve `D3D12CreateDevice` from whatever `d3d12.dll`
  sits beside the test binary, so running it in vkd3d-direct mode is a **developer harness, not a
  shipping path**, and is the one narrow exception to the ⛔ above. It costs nothing extra because
  **the same binary is needed anyway** for `D12-G9` against the system `d3d12.dll`, and it converts
  "does the engine work" from a 200-slot bet into a recorded pass/fail triple. If the owner would
  rather not run it at all, `D12-G1` alone still gates the DDI work — the loss is the baseline to
  diff `D12-G9` against.

**Decision D3 — two DLLs, not one.** `helios_umd.dll` keeps slots 0–2 and is not touched;
`helios_umd12.dll` takes slot 3.

*Mechanism, proven live, not inferred:* `UserModeDriverName` is a `REG_MULTI_SZ` indexed by
`KMTUMDVERSION` (`d3dkmthk.h:1830-1839`: `DX9=0, DX10, DX11, DX12, DX12_WSA32, DX12_WSA64`).
`D3DKMTQueryAdapterInfo(KMTQAITYPE_UMDRIVERNAME, Version=3)` against the Helios adapter returns our
DriverStore `helios_umd.dll` today, and versions 4/5 return `STATUS_INVALID_PARAMETER` — so write
**four** entries, never six. (`research/R11.md` §1.3.)

*Why two:* `helios_umd.dll` is loaded **and unloaded once per D3D11 device inside dwm** (measured —
`umd/src/lib.rs:45-64`). Anything linked into it is mapped, relocated and unmapped on every dwm
device create. Linking vkd3d + dxil-spirv into the compositor's driver makes DWM pay for a feature
it never uses, and makes every D3D12 compile error a change to the binary dwm loads at boot. Two
DLLs also give the only acceptable rollback: **`UserModeDriverName[3]` is a single registry rewrite
away from disabling D3D12 entirely, with D3D11 provably untouched** (it resolves index 2). That is a
stability argument, not an aesthetic one — see D11.

**Decision D3b — two DLLs must not mean two copies of anything. Every line that is not
D3D-version-specific moves to a shared `umd_common` crate, and it moves *before* `umd12` is
written.**

The two-DLL split (D3) buys blast-radius isolation. It must not buy code duplication. The rule:

> ⛔ **If a D3D12 file would contain code that also exists in `umd/`, that code moves to
> `umd_common` first and both crates depend on it. Copy-paste between `umd` and `umd12` is a
> defect, not a shortcut.**

`umd_common` is an **`rlib` path dependency** of both cdylibs (no cargo workspace — that would
change `CARGO_TARGET_DIR` semantics for `kmd_render`, which has its own `Cargo.make.toml` and WDK
metadata; path deps already work, exactly as `helios_protocol` is consumed today at
`umd/Cargo.toml:12`). Each cdylib gets its own copy of the *machine code* at link time — that is
unavoidable and correct for two independently-deployable drivers — but there is exactly **one copy
of the source, one place to fix a bug, and one place to review**.

What moves, and it is ~1 500 lines with essentially no behaviour change:

| `umd_common` module | From | Why it is version-agnostic |
|---|---|---|
| `hr` | `umd/src/hr.rs` (93 L) | HRESULT constants + the 11 compile-time severity asserts. Its own doc already names *"the D3D10/11 UMD DDI, the D3D12 UMD DDI and the DXGI DDI"* as its audience (`:1-2`) — it was written for this. |
| `log` | `umd/src/log.rs` (244 L) | `trace_line!`/`log_error!`, the `#[deprecated]` + `#![deny(deprecated)]` compile-error guard, `close_at_detach()` under the loader lock, `log_self_module_path()`, `log_knob_inventory()`. **One change:** the log basename becomes an `init(basename)` argument so D3D11 keeps `umd-<pid>.log` and D3D12 writes `umd12-<pid>.log`. |
| `knobs` | `umd/src/knobs.rs:58-153` | `reg_dword` (the single audited advapi32 FFI site), `DwordKnob`, `BoolKnob`, and the inventory mechanism. The **knob values** stay per-crate — `umd12` declares its own set including `UmdD3D12` (D11). |
| `format` | `umd/src/format.rs` (449 L) | The DXGI format table. Already deliberately free of `windows`/WDK types so `tools/format-table-check.rs` can `include!` it and run on Linux — that property must survive the move. |
| `throttle` | `umd/src/forward.rs:119-162` | `LogThrottle`, whose budget is a call argument rather than baked into the static. |
| `refusals` | `umd/src/forward.rs:322-448` | The refusal-counter *mechanism* — `note(&AtomicUsize)` plus first-hit-emits-summary. The eleven D3D11 counter fields stay in `umd`; `umd12` declares its own set. Generalise to `RefusalCounter { count, name }`. |
| `noop` | `umd/src/device_funcs.rs:676-754` | The counting noop-DDI idiom, `UniformFn`, `log_backtrace` (`RtlCaptureStackBackTrace` on first hit), and the `stub_fill_device_table<T>` size-derived table stubber. `ddi_calc_size`'s 256-byte answer does **not** move — that is a D3D11 claim. |
| `slot` | `umd/src/forward/handles.rs:177-333` | `Slot<P>`, `Com<T>`, `Boxed<S>` and the three traits — already generic and already free of `ddi::` types. The `com_handles!`/`boxed_handles!` macros become `#[macro_export]` with `$crate`-qualified paths so each cdylib invokes them over its **own** `ddi` module. ⚠ The `Slot<Boxed<S>>::get() -> &'static S` soundness argument (`:294-301`) rests on the D3D11 runtime's `CUseCountedObject` ordering; **it must be re-derived for D3D12, not assumed**. |
| `window` | `umd/src/device_funcs.rs:127-161` | `Window<T>` — a pointer and its capacity as one value. |
| C++ side | `umd/bridge/bridge_common.h`, plus `PeriodicStat`, `qpc_elapsed_us`, `ComRelease<T>` from `dxvk_bridge.cpp:134-202` | Engine-agnostic. Becomes a shared header both bridges include. ⚠ The `bridge_guard` template — including its `static_assert` (`ead692e`) — belongs here too; a second guard template written from scratch is how that bug comes back. |

`umd_common` must build on **Linux as well as Windows** (put `windows` behind
`[target.'cfg(windows)'.dependencies]` and `#[cfg(windows)]`-gate `slot`/`log`/`knobs`) so
`tools/format-table-check.rs` and any future host-side test keep working. It has **no `build.rs`**
and **no WDK dependency** — the bindgen'd DDI types stay in their own crates, because D3D11 and
D3D12 generate different headers.

**Ordering is load-bearing:** the `umd_common` extraction is stages S1–S2 and lands *before* the
`umd12` crate exists (S3). Extracting from one caller is a mechanical, provable refactor —
`log_knob_inventory()`'s output must come out byte-identical, which is its own validation
instrument. Extracting after a second caller exists is a merge.

**Decision D3c — ⚠ the two-DLL split has a cost D3 did not price: cross-DDI resource interchange is a
*stated Microsoft requirement*, and Helios' two DLLs hold two different engines.** *(Raised
2026-08-05 from `ResourceHeaps.md`, DirectX-Specs @ `2bd58ca5`. This is **not** a decision to reverse
D3 — it is the constraint D3 must be implemented against.)*

`ResourceHeaps.md:1254-1256`, verbatim:

> Shared resources created in D3D11 must be able to be opened in D3D12. This behavior must be supported until all previous D3Ds can be hoisted on top of D3D12.
>
> Similar scenarios must be supported in reverse, as well. D3D12-created resources must be able to be opened by DWM, using D3D11 and the 11 DDI. Such resources will be created as both resource & heap with compatible D3D11 descriptions.

and `:198`, on what that costs the driver:

> The necessary requirements to support all these scenarios are also imposed on the driver. The driver must construct private data that is consumable by their D3D11 driver and will be interpreted as a shared tile pool.

**Why this bites Helios specifically.** D3 puts D3D11 behind `helios_umd.dll` → **DXVK** and D3D12
behind `helios_umd12.dll` → **vkd3d**. A resource created through one is not the same driver object as
one created through the other, and the two engines have independent resource representations. The spec
names **DWM opening D3D12-created resources through the 11 DDI** as the scenario — and DWM compositing
the whole desktop on Helios is this project's stated goal, so a D3D12 application's swapchain back
buffer walks straight into it at **P4**, the first-pixels gate.

⚠ **State the limits of the evidence honestly.** These are requirements on *a* D3D12 driver. Whether
the D3D12 runtime *enforces* them at device or resource creation, or whether they only bite when DWM
actually opens a shared handle, is **not** established by these quotes — and this is a 2014-2015-era
design document (`SPECS.md` §6 trap 1). ⛔ Nothing here is a reason to pre-emptively merge the two DLLs;
D3's rollback and blast-radius arguments are unaffected.

**What follows for the plan, concretely:**

- `ResourceHeaps.md:187` narrows the surface usefully: *"Only committed resources and certain heaps can
  be shared with earlier runtimes"*, and only `L1` heaps on discrete / `L0` on UMA, both with **no CPU
  access**. So the exposure is committed resources and a narrow heap class, not everything.
- The settling experiment is **not** an 11on12 test. `TranslationLayerResourceInterop.md` covers only
  Microsoft's own 9on12/11on12 layers, where by construction there is exactly **one** driver-side
  engine, and it never addresses two independently-authored UMDs on one adapter. The real test is:
  create a shared texture through `helios_umd.dll`/DXVK (`D3D11 CreateSharedHandle`), open it through
  `helios_umd12.dll`/vkd3d (`pfnOpenHeapAndResource`), and the reverse — recorded as a new question in
  `GATES.md` §7.
- It raises the priority of `pfnOpenHeapAndResource`, which `DDI_REFERENCE.md` §9.7 already records as
  one of the runtime's nine hard NULL-checks, from "must be non-NULL" to "must actually work".

**Decision D4 — ⛔ SUPERSEDED 2026-08-05. `helios_umd12.dll` STATICALLY LINKS vkd3d, exactly as
`helios_umd.dll` statically links DXVK. Read "✅ D4 IS DECIDED — STATIC" below before this
paragraph.** What follows is the original DLL-and-two-exports decision, retained because the **two
Helios entry points it introduced are unchanged and still the interface** — only their delivery
changed, from an exported DLL to a static archive. Every *rationale* below is dead; see the status
table in the decided section.

*Original text:* the vkd3d engine is reached through a Helios-added export on vkd3d's own DLL, not
by statically linking `libvkd3d` into the UMD. The UMD `LoadLibrary`s a Helios-built
`helios_vkd3d.dll` (vkd3d's `d3d12core` target, renamed, with two added exports) and calls

```c
HRESULT helios_vkd3d_create_device(LUID adapter_luid, REFIID iid, void **device);   /* NEW export */
```

which calls `vkd3d_create_instance` + `vkd3d_create_device` (`include/vkd3d.h:104,110`) **directly**,
skipping `libs/d3d12core/main.c`'s `D3D12CreateDevice`.

⚠ **Two exports, not one.** The second is a root-signature serializer. `d3d12umddi` delivers root
signatures to the driver **already parsed** (`D3D12DDI_ROOT_SIGNATURE`), while vkd3d's
`ID3D12Device::CreateRootSignature` wants a serialized DXBC `RTS0` blob, so the UMD must
re-serialize (H3). `vkd3d_serialize_root_signature` (`include/vkd3d.h:129`) exists but is **not
exported from any vkd3d DLL** — `libs/d3d12core/d3d12core.def` exports only `D3D12GetInterface` and
the `D3D12SDKVersion` data symbol. So:

```c
HRESULT helios_vkd3d_create_device(LUID adapter_luid, REFIID iid, void **device);
HRESULT helios_vkd3d_serialize_root_signature(const D3D12_ROOT_SIGNATURE_DESC *desc,
                                              D3D_ROOT_SIGNATURE_VERSION version,
                                              ID3DBlob **blob, ID3DBlob **error_blob);
```

Three reasons, in order:

1. **`d3d12core`'s device path calls `CreateDXGIFactory1`.** ⚠ Precisely: the exported
   `D3D12CreateDevice` lives in `libs/d3d12/main.c:143` (the separate thin `d3d12.dll` target, which
   Helios does not use at all). Inside `d3d12core.dll` the DXGI-touching path is
   `d3d12core_CreateDeviceFromFactory` (`libs/d3d12core/main.c:643`), reachable only through
   `D3D12GetInterface`, and it calls `CreateDXGIFactory1` at `:383` and `:406` to resolve the
   adapter. A WDDM UMD sits *below* DXGI and must not depend on `dxgi.dll` —
   `umd/build.rs:240-243` states this rule for the D3D11 side and the D3D12 side inherits it
   verbatim, with an added hazard: a UMD that loads dxgi during device creation risks re-entering
   adapter enumeration that loads the UMD. The new exports exist precisely to bypass that path.

   ⛔⛔ **MEASURED 2026-08-05: the built `helios_vkd3d.dll` DOES NOT satisfy this, and the export
   alone cannot make it.** `objdump -p` on
   `tmp/dx12/build/vkd3d-win64/libs/d3d12core/helios_vkd3d.dll` shows, under *The Import Tables*
   (not the delay-import directory, which is empty):

   ```
   DLL Name: dxgi.dll
   vma:     Ordinal  Hint  Member-Name  Bound-To
   005e6898  <none>  0004  CreateDXGIFactory1
   ```

   **It is a normal static import.** So `LoadLibrary("helios_vkd3d.dll")` makes the loader map
   `dxgi.dll` into the process and bind `CreateDXGIFactory1` **before a single line of Helios code
   runs**, whether or not `helios_vkd3d_create_device` ever calls it. Bypassing the *call* was never
   enough — the object carrying it (`libs/d3d12core/main.c`) is still linked into the target, and an
   import is resolved at load, not at call.

   ⇒ **Reason 1 is currently ASPIRATIONAL, not achieved.** Two candidate fixes, and the choice is
   P3's: (a) build a `helios_vkd3d` target that **excludes `libs/d3d12core/main.c`** so the import is
   never generated — the same object-exclusion the static fallback below already specifies, applied
   to the DLL arm; or (b) accept the import and prove the hazard is not live, i.e. that dxgi being
   loaded (as opposed to *called*) during `pfnCreateDevice` cannot re-enter adapter enumeration.
   ⚠ (b) is a claim about the loader and the D3D12 runtime together and would need a measurement,
   not an argument. **New question: `GATES.md` §7.28.**
2. ~~**Licence.**~~ ⛔ **NOT A CONSTRAINT — owner directive, 2026-08-05: "I don't care about UMD
   License."** Recorded so it is not re-litigated. For the record, the fact is that vkd3d-proton is
   **LGPL-2.1-or-later** (`vkd3d-proton-helios/COPYING`) while DXVK is zlib, so static-linking
   `libvkd3d` into `helios_umd12.dll` would engage LGPL §6 relinking obligations that a DLL boundary
   does not — but that is **not** a reason to choose between them here. **D4 stands on reasons 1 and
   3 alone**, which are purely technical, and static linking is an equally legitimate option to be
   decided on those grounds.
3. **It sidesteps the two-SPIR-V-compilers question entirely.** DXVK links `libdxbc_spv.a` +
   `libspirv.a`; vkd3d links `dxil-spirv` + `libvkd3d-shader.a`. Both are C++ SPIR-V producers over
   SPIRV-Headers, and whether their *defined external* symbol sets intersect is unverified
   (`research/R4.md` UNVERIFIED-2). Separate modules make the question moot.

### ✅ D4 IS DECIDED — STATIC, 2026-08-05 (owner)

> *"D4 is an open choice, the choice is we are going to statically link vkd3d-proton and not mess
> with dynamic dlls."*

**`helios_umd12.dll` statically links vkd3d-proton, exactly as `helios_umd.dll` statically links
DXVK.** There is no `helios_vkd3d.dll` on the shipping path and no `LoadLibrary` of an engine.

**Built and verified the same day**, so this is a measured decision rather than a planned one:

| check | result |
|---|---|
| vkd3d builds under clang-cl / MSVC ABI | ✅ **143/143 targets, exit 0** — compiler id `clang-cl` 17.0.6, linker `lld-link`, `b_vscrt=md`, byte-for-byte the toolchain the DXVK build already uses |
| the static arm has no DXGI dependency | ✅ **0** undefined `CreateDXGIFactory` refs in `libhelios_d3d12_static.a`, `libvkd3d-proton.a`, `libdxil-spirv.a` — against **1** dxgi import in the retired `helios_vkd3d.dll` |
| the Helios entry points survive | ✅ `T helios_vkd3d_create_device`, `T helios_vkd3d_serialize_root_signature` |
| ⭐ **the engine actually renders** | ✅ **`D12-G1` PASS against the static archive, 2026-08-05** — 28 steps, 0 failures, DXIL SM 6.0 triangle exact at five sample points, `dumpbin /IMPORTS` shows **no `dxgi.dll`**. Caps identical to the mingw arm. Until this run the clang-cl archives had produced nothing and only the mingw DLL had ever drawn. `tmp/dx12/gates/G1-static/RESULT.md` |
| ⭐ **measured minimal link set** | **`libhelios_d3d12_static.a` + `gdi32.lib`. One archive, one system library** — this is what `umd12/build.rs` hard-codes at S4. |
| archive set | ⚠ The six sibling archives (`libvkd3d-proton.a`, `libvkd3d_common.a`, `libvkd3d-shader.a`, `libdxil-spirv.a`, `libdxbc_spv_module.a`, `libdxbc_spv.a`; 30.6 MB with the union) are **redundant on the link line**, not required: `libhelios_d3d12_static.a` is a *union* archive that meson hands every one of their objects (`build.ninja:1146`, `STATIC_LINKER_RSP`). Passing them anyway is harmless. |

**The fork change:** `libs/d3d12core/meson.build` gains `helios_d3d12_static` — the same Helios entry
points as the DLL target, as a `static_library`, **without `main.c` and without `lib_dxgi`**. `main.c`
is the only object in the engine that references `CreateDXGIFactory1`; a `shared_library` force-links
every object it is handed, an archive member is pulled only when referenced, so omitting it makes the
import unrepresentable. ⇒ **the static arm delivers what D4 reason 1 always wanted and the DLL arm
never did.**

⛔ **AMENDED 2026-08-05 — "self-contained" was wrong, and the way it was wrong is the lesson.**
The line above previously called `libhelios_d3d12_static.a` self-contained *because it already defines
`vkd3d_create_device`*. Linking it alone into the `D12-G1` probe produced **19 unresolved externals**.
D4 had verified *"0 undefined `CreateDXGIFactory` references"* and never asked about any **other**
undefined symbol — a search for one name is not a link.

- 14 were `__imp_D3DKMT*`: `libs/vkd3d/d3dkmt.c` imports 12 kernel entry points and `vkd3d_dep` does
  not carry `lib_gdi32`. That is the **consumer's** job and is why `gdi32` is in the minimal set above.
- 5 were `vkd3d_debug_control_*` — predicates `libvkd3d`'s `device.c`, `command.c`, `resource.c` and
  `state.c` call **unconditionally**, and which lived in `main.c`, **the one object the static target
  omits**. Dropping main.c for its dxgi import silently dropped these with it; the DLL arm never
  noticed because a `shared_library` force-links everything.

**Fix:** fork commit `8ee4440b` moves the facility verbatim from `main.c:840-1045` into
`libs/d3d12core/debug_control.c` (+ `.h`), added to **both** `d3d12core_src` and the static target, so
all three targets carry **one** implementation. Restating the five in the Helios target was rejected:
they read state that only the COM vtbl writes, so two copies would mean the conformance arm's
`IVKD3DDebugControlInterface` not reaching the shipping arm's statics — a behavioural difference
between the arms, which is exactly what `D12-G1` exists to rule out. `debug_control.c` references no
DXGI, so the zero-dxgi property is untouched.

⛔ **One required build flag, and it is a risk acknowledgement rather than a fix.**
`-D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH` on both `c_args` and `cpp_args`: MSVC 14.44's
`<yvals_core.h>` hard-asserts *"expected Clang 19.0.0 or newer"* and the installed clang-cl is
**17.0.6**, so dxbc-spirv does not compile without it. This is the **same define `umd/build.rs`
already applies to the DXVK bridge shim**, carrying the same caveat recorded there: the ABI still
rests on the objects agreeing, which nothing here can prove. Raising clang to 19+ retires it.

**Build it with `win_vkd3d`** (`tools/win-mcp`), which mirrors `Z:\vkd3d-proton-helios` to a local
`C:` checkout and runs the canonical setup + compile — the same mirror-then-build shape as `win_dxvk`,
including the LLVM-before-vcvars PATH ordering that cmd's parse-time `%PATH%` expansion demands.

**Why the three original reasons are gone, kept so the argument is not re-run from the stale version:**

| reason | status |
|---|---|
| 1 — engine must not depend on `dxgi.dll` | ✅ **Achieved by static linking, and only by it.** Measured above. |
| 2 — licence | Retracted by owner directive; never a factor. |
| 3 — two SPIR-V compilers in one DLL | Subsumed by **D3**. Two DLLs already means one SPIR-V compiler per module, so it says nothing about static vs dynamic. |
| *(the C++-ABI objection briefly recorded here)* | ⛔ **Was wrong, and is retracted.** It generalised the mingw/Itanium artifacts into a property of vkd3d. `vkd3d-proton/meson.build:9` already accepts `clang-cl`; the mingw cross was a G0 convenience because the Linux host had that toolchain. Now disproven by a working clang-cl build. |
| *(the "blast radius in dwm" objection)* | ⛔ **Measured away.** `helios_umd.dll` with DXVK statically linked is **6 115 328** bytes; `helios_vkd3d.dll` stripped is **6 193 166**. 1.3 % apart. The 20 MB figure was 10.6 MB of embedded DWARF — mingw links debug info into the image, MSVC emits a separate PDB. dwm already maps a statically-linked engine of that size every D3D11 device create. |

⚠ **The one real consequence, and it is a sequencing note:** at 105 KB the old `LoadLibrary` shape let
`OpenAdapter12` refuse without mapping any engine. Statically linked, `helios_umd12.dll` becomes ≈ 6 MB
and dwm maps all of it to receive an immediate `DXGI_ERROR_UNSUPPORTED` — until D3D12 is actually
enabled, at which point a real device create maps the engine either way. This is already governed by
the `UmdD3D12` kill switch (D11) and by not registering `UserModeDriverName[3]` until **S5**. Keep that
ordering; it is the reason the switch is cheap.

⚠ **The mingw cross-build stays**, as the **conformance** arm: `D12-G0`/`G2` build the vkd3d test suite
on the Linux host, and upstream's README says outright it does *"not stress test"* MSVC builds. Two
build recipes, one shipping arm — the MSVC/clang-cl archives ship, the mingw suite validates.

⛔ The retired `helios_vkd3d` shared target is kept in the fork. It was originally kept *only* so
`D12-G1` stayed reproducible, to be deleted once G1 had been re-run against the static arm — **that
re-run happened on 2026-08-05 and PASSED, and the target still stays.** It is now the comparison
control: `tmp/dx12/gates/G1-static/arm-diff.txt` is the whole difference between the two arms, and a
future engine regression is far cheaper to localise when the pre-D4 binary can still be built and run
from the same probe source. It costs one `shared_library` stanza and is on no shipping path.

**Decision D5 — the KMD is not on the critical path.** For Phase 0 the KMD work list is **empty**;
for the DDI arm it is three items, none required for the first triangle:

| # | Item | Why | Size | Required for first frame? |
|---|---|---|---|---|
| K1 | Validate `NodeOrdinal`/`EngineAffinity` in `DxgkDdiCreateContext` and count refusals (`CtxNode`) | Today a context for a node that does not exist is accepted silently; `DxgkDdiCreateHwContext` already checks (`scheduler.rs:135-137`). CLAUDE.md rule 2. | S | No |
| K2 | Set `ContextInfo.Caps.NoPatchingRequired` and shrink `AllocationListSize`/`PatchLocationListSize` for `VirtualAddressing` contexts | The documented shape for a GPU-VA context; Helios asks 256+256 on every context and no-ops `DxgkDdiPatch`. Wasteful, not wrong. | S | No — and it touches the Present allocation list, so knob + paired A/B |
| K3 | Revisit `ApertureSegmentCommitLimit` (64 MiB) | Only if D3D12 residency budgets read too small. Needs a measurement first. | S | No |

Everything else people expect to be needed is **not**: no extra engine nodes (D3D12's
`D3D12DDIARG_CREATECOMMANDQUEUE` carries no node ordinal — the UMD picks it in
`D3DDDICB_CREATECONTEXTVIRTUAL`); no hardware queues (HWS is WDDM 2.6+ and has never been mandatory
for D3D12); no `DxgkDdiSignalMonitoredFence` or native fences (optional, and the software path is
proven live on this adapter); no real page tables; no residency DDIs (none exist in this WDK).
(`research/R5.md` §10.)

**One correction to the received picture:** of the 103 unset `DxgkDdi*` slots, only **31 are
reachable** at the declared `WDDM2_1` level — the other 72 sit in higher-version blocks of
`DRIVER_INITIALIZATION_DATA` and cannot be called. **None of the 31 is required for a baseline
D3D12 device.** (`research/R5.md` §0.2, §1.2.)

---

## 2. The substrate is proven, and that is the biggest de-risking result

**Decision D6 — treat the Vulkan substrate as solved and stop re-litigating it.**

Measured live on the running guest this session (`research/R12.md` §9, raw capture in
`docs/dx12/research/guest-vulkaninfo-full.txt`), parsed against vkd3d-proton's own
`VP_D3D12_VKD3D_PROTON_profile.json`:

| Profile capability set | feature misses | extension misses |
|---|---|---|
| `baseline_features` | **0** | **0** |
| `fl_11_1_features` / `fl_12_0` / `fl_12_1` / `fl_12_1_rov` / `fl_12_2` | **0** | **0** |
| `shader_model_60`, `shader_model_66` | **0** | **0** |
| `optimal_performance` | 7 | 7 |

**The live Helios guest satisfies `VP_D3D12_FL_12_2_baseline` in full.** All nine of vkd3d's
hard-fail device-creation gates (`libs/vkd3d/device.c:3243-3489` — vertex attribute divisor,
transform-feedback queries, single-texel alignment, `samplerMirrorClampToEdge`, robustness2 ×2,
`shaderDrawParameters`, push descriptors, `maintenance5`+`maintenance6`) pass. Sparse binding is
supported end to end. Raytracing reaches **DXR tier 1.1**.

Known substrate ceilings, all optional: DXR 1.2 (no `VK_KHR_opacity_micromap` — at pin `2c7ba22c`
vkd3d gates on the KHR form, `libs/vkd3d/device.c:98`, not the older EXT one),
`OPTIONS14.AdvancedTextureOpsSupported` (no `VK_KHR_maintenance8`), and the descriptor-model
optimisations (`VK_EXT_descriptor_buffer` / `descriptor_heap`).

**Two substrate items are real work, and both are cheap to state:**

- **V1 — `VK_KHR_external_memory_win32` is absent and vkd3d does not check for it.** On `_WIN32`,
  `libs/vkd3d/resource.c:4405-4429` unconditionally chains `VkExportMemoryAllocateInfo` for any
  `D3D12_HEAP_FLAG_SHARED` allocation and then calls `vkGetMemoryWin32HandleKHR` — a **NULL function
  pointer** when the extension was never enabled. So shared heaps are not degraded, they are
  *hazardous*. The Mesa fork implements the semaphore twin natively already
  (`vn_physical_device.c:1273-1279`, assignment at `:1277`); the memory half is the analogous work.
- **V2 — no 32-bit (WOW64) Vulkan ICD is registered.** `HKLM\SOFTWARE\WOW6432Node\Khronos\Vulkan\Drivers`
  does not exist on the guest. A 32-bit D3D12 client finds zero physical devices. Either ship a
  32-bit venus ICD or declare 64-bit-only in writing. (Note `3DMarkNightRaid.exe` ships a Win32
  build, which would otherwise be a free WOW64 arm.)

Free wins worth taking on day one: `VN_DEBUG=mem_budget` (enables `VK_EXT_memory_budget`, improving
`QueryVideoMemoryInfo`); `VKD3D_CONFIG=single_queue` and `nodxr` as the two first stability A/Bs.

---

## 3. What is genuinely hard

Ranked. These are where the plan should spend its risk budget.

**H1 — the D3D12 UMD DDI is undocumented *as a contract*, but it is not undocumented everywhere.**
There has never been a public D3D12 UMD, open or closed (D3D9On12 and D3D11On12 were both
open-sourced; a D3D12 one never was), so strategy D1 is original engineering, not porting.
(`research/R10.md` Q1.)

⚠ **Corrected 2026-08-05.** The earlier phrasing — *"~600 auto-generated reference stubs with no
Remarks and zero conceptual articles"* — is accurate for the **driver-docs** corpus
(`learn.microsoft.com/windows-hardware/drivers/ddi/`) and `OpenAdapter12` does appear nowhere in it.
It was **wrong about `microsoft.github.io/DirectX-Specs`**, which the doc set had touched for two
pages. Measured at pin `2bd58ca5` (2026-07-28): **90 documents, 44 with an explicit `DDI` section
heading, and 123 of the header's 399 `PFND3D12DDI_*` typedefs (31 %) named in prose** — with the three
hardest areas of `DDI_REFERENCE.md` (resource binding, resources/heaps, barriers) among the
best-covered. `docs/dx12/SPECS.md` is the triage and the 235-finding register.

⭐ **The corroboration cuts both ways, and that is the reassuring half.** `OpenAdapter12`,
`pfnSetCommandListDDITableCb` and `pfnGetPresentPrivateDriverDataSize` appear **nowhere in either
corpus**. Those are exactly the three things `D12-G5` had to measure, so the spy was not redundant —
it covered the part that genuinely has no documentation.

⛔ **This does not demote the measurement.** 173 of the 296 `PFND3D12DDI_*` the specs name are absent
from SDK 26100 entirely, and several specs publish struct shapes the shipping header contradicts. The
arbiters remain, in order: the `D12-G5` log → the staged `d3d12umddi.h` → `D3D12Core.dll`'s strings →
the spec.

*Mitigation, and it is unusually good:* **`C:\Windows\System32\d3d10warp.dll` exports
`OpenAdapter12`** (verified by `dumpbin /exports`). A shim DLL that forwards to WARP and logs every
`pfnGetCaps(Type, DataSize)`, `pfnFillDDITable(TableType, TableSize, …)`,
`pfnGetSupportedVersions` result and every table-slot call turns undocumented contract questions into
one log file, with no driver change. Second source: the D3D12 runtime's own validation strings,
extracted from `D3D12Core.dll` (`docs/dx12/research/d3d12core-driverstrings.txt`, 270 lines) — the
runtime telling you in English what the driver must do. Both are first-class tools, not curiosities.

✅ **BUILT AND RUN — `D12-G5`, 2026-08-05, `tools/d3d12_spy/`, results in
`tmp/dx12/gates/G5/answers.md`.** Of `DDI_REFERENCE.md` §15.1's eighteen questions: **8 answered
outright, 6 moved forward, 4 re-marked UNVERIFIED with the reason stated** (three of those four are
structurally out of the spy's reach — WARP is a software rasterizer and never enters the kernel).
The four results that change the plan are in `DX12.md` §4.2; the two largest are that **this Windows
build accepts `D3D12DDI_SUPPORTED_0040`** (169 baseline slots instead of 214, and a triangle presents
on it) and that **the runtime hands `pfnCreateShader` a raw DXIL stream, never a DXBC container**,
converting SM 5.1 DXBC on the way. H1 is no longer "original engineering with no reference": there
is now a recorded call trace of a working D3D12 UMD on this exact Windows build.

⚠ The strings file remains the second source, but with a measured caveat: `D12-G5` showed that the
fifteen `Driver filled out an invalid value in …::<Tier>` strings are **not** retail device-creation
gates — an out-of-range tier is clamped silently. The cross-cap consistency strings *are* enforced at
retail. `DDI_REFERENCE.md` §11.5.0 separates the two.

**H2 — presentation, but not for the reason the old charter said.**

*Correction of record:* `ROADMAP.md:2385` ("only the VULKAN client class lacks a HW present") and
the version of `DX12.md` that inherited it are **stale**. The Helios Mesa ICD implements
`VK_KHR_win32_surface` + `VK_KHR_swapchain` and has had a hardware flip present — the **dcomp
vehicle**, default **ON** — since the 28th session. A Vulkan client on Helios already gets a
flip-model, DWM-composited present whose pixels move GPU-side through venus, re-entering
`helios_umd!dxgi_present`. (`research/R7.md` §4.)

The real present issues are three, all named:

- **P-A — ✅ CLOSED by D2, not mitigated.** The hazard was: vkd3d as an app's `d3d12.dll` needs
  DXVK's `dxgi.dll` (MS DXGI cannot make a swapchain for a foreign `ID3D12CommandQueue` —
  `demos/demo_win32.h:248-263` is the call that fails), and once a DXVK `dxgi.dll` sits in an app
  directory the ICD's present vehicle picks it up through its bare-name `LoadLibraryA("dxgi.dll")`
  (`wsi_common_win32.cpp:485-487`) and DXVK's `CreateSwapChainForComposition` returns `E_NOTIMPL`
  by default (`dxvk-helios/src/dxgi/dxgi_factory.cpp:282-298`, `dxgi_options.cpp:178`) — silently
  demoting every frame to the software GDI blit while the picture still looks correct.
  **D2 removes the precondition:** no app-local DLLs, so no DXVK DXGI anywhere, so no hazard.
  DXVK's `dxgi.dll` is not built, not deployed and not referenced in this tree, and the shipping
  D3D12 path uses Microsoft's DXGI exactly as D3D11 does.

  ⚠ **Two things to keep from it anyway.** (a) The ICD's bare-name `LoadLibraryA` of `dxgi.dll` /
  `d3d11.dll` / `dcomp.dll` is still a latent hijack: *any* process that ships its own DXGI — a
  game with a DXVK/ReShade drop-in, an overlay — hands the vehicle a foreign compositor stack. The
  hardening is unchanged and cheap: load by explicit full System32 path **and verify with
  `GetModuleFileNameW`**, refusing with a named counter (neither the path nor
  `LOAD_LIBRARY_SEARCH_SYSTEM32` suffices alone, because the loader's already-loaded check matches
  on base name). It is now ordinary stability work, not a D3D12 blocker. (b) The failure *shape* —
  a correct-looking picture served by a path you did not intend — is the reason `D12-G8`'s pass
  criterion must confirm which path served the frame, not merely that a frame appeared.

- **P-B — ✅ off the critical path.** `helios_umd_get_present_result` returning `-1` unconditionally
  since R912(a) forces every *vehicle* present through the worker-serial `wait_last_present`
  fallback (measured avg **5.57 ms/frame**), and the vehicle path costs two-to-three frame copies.
  ⚠ None of that is on the D3D12 present path under D2: a D3D12 frame goes MS DXGI → runtime →
  `pfnPresent` → `DxgkDdiPresent`, the same machinery D3D11 uses, and never enters the ICD's WSI
  vehicle. It remains a real cost for **native Vulkan clients** and should be fixed on its own
  merits; it is not a D3D12 number.

- **P-C (the DDI arm's present design) — narrower than it first looked.** D3D12's `pfnPresent`
  **does** reach the driver: it is on the *command-list* table (`PFND3D12DDI_PRESENT_0051`,
  `d3d12umddi.h:7250`), takes `D3D12DDIARG_PRESENT_0001` (essentially `DXGI_DDI_ARG_PRESENT`), and
  **outputs** the src/dst `D3DKMT_HANDLE`s and the context. There is no `pfnPresentCb` and no
  `pfnRenderCb` **declared in `d3d12umddi.h` itself** — but that is not the whole surface.
  ✅ **`D3D12DDIARG_CREATEDEVICE_0109.pKTCallbacks` (`d3d12umddi.h:13623`) is a
  `CONST D3DDDI_DEVICECALLBACKS*` — the same 65-entry kernel thunk table the D3D11 UMD already
  drives, and it contains both `pfnRenderCb` and `pfnPresentCb`** (verified:
  `D3DDDI_DEVICECALLBACKS` at `tmp/dx12/sdk/d3dumddi.h:4499`, 65 `pfn` members, both present).
  **Consequence:** the existing identity channel transfers — the D3D12 UMD can write a
  `HeliosPresentRenderCmd` and call `pfnRenderCb` exactly as `umd/src/forward/present.rs:795`
  does, landing in the KMD's **PASSIVE** `dxgkddi_render` path and its per-context stash, with no
  KMD change at all. ⛔ **Do not design a new `DxgkDdiSubmitCommandVirtual` decode for this** — that
  DDI runs at DISPATCH_LEVEL (`kmd_render/src/ddi/submit_command.rs:723-724`), where the stash
  machinery's `diag::record*` calls are illegal, and it would add a fourth KMD item that D5 does not
  have. Everything from `dxgkrnl` down — the flip arm, `PresentFlipPrivate`, `set_scanout_blob` —
  is reused unchanged.
  ⚠ **UNVERIFIED:** that the D3D12 runtime tolerates the driver calling `pfnRenderCb` around
  `pfnPresent`. Settling experiment: `pfnRenderCb` + a counting `DxgkDdiRender` on the D3D12 path
  at G7, before G8 depends on it.

**H3 — the object graph, not the command stream, is where the DDI cost lives.** The command
recording surface forwards almost 1:1 into `ID3D12GraphicsCommandList`. What does not:

- **Root signatures arrive parsed**, as `D3D12DDI_ROOT_SIGNATURE` — vkd3d's
  `CreateRootSignature` wants a serialized `RTS0` blob, so the UMD must **re-serialize**
  (`vkd3d_serialize_root_signature`, `include/vkd3d.h:129`, exists but is not exported today).
- **PSOs arrive as handle bundles** — blend / rasterizer / depth-stencil / element-layout are
  separate driver objects referenced by handle; the UMD must retain each one's desc and reassemble a
  `D3D12_GRAPHICS_PIPELINE_STATE_DESC`.
- **Heap and resource creation are fused** into one `pfnCreateHeapAndResource` whose two argument
  pointers are independently nullable (committed / placed / heap-only).
- **Descriptor heaps are entirely driver-owned** — and this is the *good* surprise: both
  `D3D12DDI_CPU_DESCRIPTOR_HANDLE{SIZE_T ptr}` and `D3D12DDI_GPU_DESCRIPTOR_HANDLE{UINT64 ptr}` are
  opaque driver-chosen scalars, and `pfnGetDescriptorSizeInBytes` lets the driver choose the stride.
  A forwarder can create a matching `ID3D12DescriptorHeap` on the vkd3d device and **return vkd3d's
  own handle values and stride verbatim**, so runtime/app descriptor arithmetic lands on vkd3d's own
  arithmetic. No shadow table at all.
  ⚠ One ABI hazard: the DDI returns those handle structs **by value**, while vkd3d's C
  implementation returns via hidden pointer. That is exactly the `bridge_guard` truncation class
  (commit `ead692e`) and must be handled explicitly in the bridge.

**H4 — the caps gauntlet is a hard gate with ~60 runtime-enforced consistency rules.**
`D3D12DDICAPS_TYPE` has **43** enumerators (§4.1); `D3D12Core.dll`'s own strings enumerate the
failures:
`"Driver did not respond to D3D12DDICAPS_TYPE_D3D12_OPTIONS caps query."`,
`"Driver did not report any supported shader models…"`,
`"Driver did not set valid WaveLaneCountMin/Max or TotalLaneCount…"`, ~12 distinct
`"Driver filled out an invalid value in D3D12DDI_D3D12_OPTIONS_DATA::<Tier>"`, and cross-checks such
as `"Drivers that support raytracing must expose shader model 6.3."` **Every tier is a contract the
runtime validates, and D3D12's tiered caps are exactly the shape of the `SupportDirectFlip` /
`FlipImmediateMmIo` landmine.**

**H5 — ✅ CLOSED 2026-08-05: the swizzle fires, and the ceiling is SM 6.8 / FL 12_2.** *(Was: shader
model may cap at 6.0 unless one probe says otherwise.)* vkd3d gates SM 6.2 (and the whole ladder
above it) on FP32 denorm control, exempting only `VK_DRIVER_ID_NVIDIA_PROPRIETARY`. The guest reports
`driverID = MESA_VENUS` with both denorm properties `false`. **But** vkd3d handles layered
implementations: with `VK_KHR_maintenance7` it reads the *underlying* driver's
`VkPhysicalDeviceDriverProperties` and **swizzles `driverID` to the real one**
(`device.c:2657-2664`) — and that runs at `device.c:4129`, well before shader-model caps init at
`:11599`.

`tools/vk_layered_driverid_probe.cpp` chained the nested struct and printed
**`NESTED driverID = 4 (NVIDIA_PROPRIETARY) driverName=NVIDIA`** (`maintenance7` PRESENT,
`layeredApiCount = 1`, `layerVendorID = 0x10de`) — the bit that was proven by source ordering but
never observed. **Confirmed end to end at `D12-G1`**, which is the stronger evidence because it is a
real vkd3d device on the live guest rather than a prediction of one: `VKD3D_DEBUG=info` printed
`Enabling support for SM 6.6.` → `6.7.` → `6.8.` and `DX Ultimate supported!`, and
`CheckFeatureSupport` answered `HighestShaderModel = 6.8`, `MaxSupportedFeatureLevel = 12_2`.

⚠ **The canonical phrasing changes: "SM 6.8, and FL 12_2 is live", not "plan for 6.0".** §5 and
`SUBSTRATE.md` §7 are updated to match. The one place to still be careful is that **6.8 is what the
*engine* reports**; what the *DDI arm* advertises is a separate decision made at P3 against the caps
gauntlet (H4), and it must not exceed what is backed.

---

## 4. Scale, stated honestly

### 4.1 ⚠ The canonical counts — settle every disagreement here

Re-derived from `tmp/dx12/sdk/d3d12umddi.h` (SDK 10.0.26100.0, **19 031** lines) by parsing struct
and enum bodies, 2026-08-05. **Any other number anywhere in this directory is wrong; fix it to
match this table.** Several were miscounted independently by more than one research lane, so do not
trust a figure that is not here.

| Thing | Count | Where | Miscount to watch for |
|---|---:|---|---|
| `D3D12DDI_ADAPTERFUNCS_0109` | **8** | `:13640-13650` | — |
| `D3D12DDI_DEVICE_FUNCS_CORE_0109` | **124** | `:13451-13616` | — |
| `D3D12DDI_COMMAND_LIST_FUNCS_3D_0108` | **75** | `:13303-13388` | — |
| `D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001` | **7** (2 are `pfnUnused`) | `:2729-2738` | — |
| **baseline driver-side slots** | **214** | 8+124+75+7 | — |
| the command-queue triple in `CORE_0109` | members **27, 28, 29** | `:13488-13490` | ⛔ not "slots 38-40" — that was a `sed` line offset misread as a member index |
| `D3D12DDICAPS_TYPE` | **43** enumerators | `:94-150` | 40 carry the `D3D12DDICAPS_TYPE_` prefix; the other 3 are `D3D12DDI_FEATURE_D3D12_PREDICATION_106`, `..._PLACED_RESOURCE_SUPPORT_INFO_106`, `..._HARDWARE_COPY_106`. There are **no** versioned additions elsewhere. Neither 40 nor 42 is right |
| `D3D12DDI_TABLE_TYPE` | **25** enumerators | `:2488-2516` | ⛔ not 27 — 27 is the highest assigned *value*; the value space has gaps at 5, 6, 18 |
| `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062` | **18** live members | `:8606-8647` | 28 lines declare members, but ten are `#else` `void* pfnReserved…` alternates at the same offsets. Live count under WDDM2_5+ gates is 18. Earlier revisions: `_0003` 12, `_0022` 14, `_0050` 17 |
| `D3DDDI_DEVICECALLBACKS` via `pKTCallbacks` | **65** `pfn` members | `d3dumddi.h:4499` | includes `pfnRenderCb`, `pfnPresentCb`, `pfnSubmitCommandCb`, `pfnEscapeCb`, `pfnAllocateCb` — see P-C |
| `D3DDDI_ADAPTERCALLBACKS` | 3 | — | — |
| distinct `PFND3D12DDI_*` typedefs | 399 | — | — |
| `typedef struct` in the header | 683 (517 `D3D12DDI_*` + 133 `D3D12DDIARG*`) | — | — |

Reproduce with the script in `DDI_REFERENCE.md`'s appendix; do not eyeball a `grep -c`, because
`#else` arms and value gaps defeat it.

### 4.2 The comparison

| | D3D11 (shipping) | D3D12 (target) |
|---|---|---|
| Driver-side table slots that must be non-NULL | ~175 (157 device + 18 DXGI) | **214** |
| Slots needing a real body for a triangle on screen | — | ~86–99 — `DDI_REFERENCE.md` §14 owns the authoritative list; quote it, do not re-estimate |
| Runtime→driver callbacks in | 1 table | 3 tables: `D3DDDI_ADAPTERCALLBACKS` 3, `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062` 18, `D3DDDI_DEVICECALLBACKS` 65 |
| Caps types to answer | 8 | **43** |
| Rust today | 5 774 lines in `umd/src/*.rs` + 13 283 in `umd/src/forward/` (19 modules) | — |

**~1.2× the D3D11 UMD in slot count**, and more than that in difficulty because of H3/H4. But the
kernel-facing half is already built, the engine already exists, and the substrate is measured green.

---

## 5. ✅ The one experiment that should run before anything else — RUN, 2026-08-05

**H5 is settled with a ~40-line read-only Vulkan probe** (`tools/vk_layered_driverid_probe.cpp`):
chain `VkPhysicalDeviceLayeredApiPropertiesListKHR` → `VkPhysicalDeviceLayeredApiVulkanPropertiesKHR`
→ `VkPhysicalDeviceDriverProperties` on the guest and print `driverID`. Verbatim output, from
`tmp/dx12/gates/H5/driverid-probe.txt`:

```
pd[0] Virtio-GPU Venus (NVIDIA RTX PRO 6000 Blackwell Workstation Edition)
  VK_KHR_maintenance7 = PRESENT   (vkd3d builds the layered chain only when present)
  top    driverID = 22 (MESA_VENUS) driverName=venus
  layeredApiCount = 1  layerVendorID=0x10de layerDeviceID=0x2bb1 layerName=NVIDIA RTX PRO 6000 …
  NESTED driverID = 4 (NVIDIA_PROPRIETARY) driverName=NVIDIA
  ==> vkd3d WILL swizzle driverID  ==>  SM 6.6+ (FL 12_2)
```

⇒ **The swizzle fires. SM 6.8 and FL 12_2, both confirmed on a live vkd3d device at `D12-G1`.**
`SUBSTRATE.md` §7.4's three candidate fixes are all **moot**: the ICD's layered chain already tells
the truth, and the vkd3d fork does **not** need the `device.c:10699` patch. The fork's first content
turned out to be D4's two exports instead (`ARCHITECTURE.md` §7.4), which is a better answer to
"what was the fork for" than a workaround would have been.

⚠ **The probe deliberately leaves `vk_layered_props.properties.sType` zeroed**, because vkd3d does
(`device.c:2318-2321` memsets and `:2338-2342` sets only the outer sType). Keep it that way: a probe
that sets the field could succeed where the engine fails, which is the worst possible outcome for a
probe whose whole job is to predict the engine.

---

## 6. Cross-lane conflicts and how they were resolved

| Conflict | Resolution | Evidence |
|---|---|---|
| **R3:** "the D3D12 UMD DDI has no queue object at all — the runtime owns submission." **R1/R2:** the DDI has a full queue object. | **R1/R2 are right; R3 is wrong.** `D3D12DDI_DEVICE_FUNCS_CORE_0109` contains `pfnCalcPrivateCommandQueueSize` / `pfnCreateCommandQueue` / `pfnDestroyCommandQueue`, and `D3D12DDI_TABLE_TYPE_COMMAND_QUEUE_3D` (=2) is filled from `D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001` with `pfnExecuteCommandLists`. | `d3d12umddi.h:13488-13490` (members 27-29 of `CORE_0109`) and `:2729-2738`. |
| **R2:** "does a monitored fence advance with no GPU-side write?" — marked HIGH risk, strategy-deciding. **R5:** it is documented and already proven on this adapter. | **Risk downgraded HIGH → MEDIUM.** Microsoft documents the exact fallback: *"If a GPU engine isn't capable of writing to a monitored fence using its virtual address, the UMD uses the SignalSynchronizationObjectFromGpuCb callback to queue a software signal packet"*, and *"Dxgkrnl updates the fence memory location"* on CPU signal. `tools/vehicle_flipwait_probe.c` proves the queued-wait-before-queued-signal primitive live on this software-scheduled adapter with **zero KMD changes**. Residual: confirm the CPU-visible value advances for a *D3D12-shaped* fence — one probe (G-fence). | `windows-driver-docs-pr/display/context-monitoring.md`, `native-gpu-fence-objects.md`; `ROADMAP.md:2616-2621`. |
| **R8:** vkd3d caps Helios at SM 6.0 → FL 12_1. **R12:** the `maintenance7` layered-driverID swizzle probably lifts it to SM 6.6 → FL 12_2. | ✅ **R12 is right, and it is now observed, not inferred.** The probe printed nested `driverID = 4 (NVIDIA_PROPRIETARY)` and a live vkd3d device reports **SM 6.8 / FL 12_2** (`D12-G1`). R8's SM 6.0 reading is dead. | `tmp/dx12/gates/H5/driverid-probe.txt`; `tmp/dx12/gates/G1/{bridge_probe.txt,vkd3d.log}`. |
| **R3:** driving vkd3d's core from a DDI frontend needs ~5 surgeries (de-`static` 200 methods, ops tables, rewrite `bundle.c`…). **R2:** forwarding is straightforward with shadow state. | **They answer different questions.** R3 costed *replacing* vkd3d's COM layer with a non-COM ops table. D1 does not do that — it **calls vkd3d's public `ID3D12*` COM interfaces**, exactly as the D3D11 UMD calls DXVK's `ID3D11Device`. Zero vkd3d surgery beyond exporting a device-creation entry point (D4). R3's five surgeries are **not on the plan**. | `research/R2.md` §6 verdict table; the D3D11 precedent at `umd/bridge/dxvk_bridge.cpp:1754-1757`. |
| **R2/DX12.md:** the "load-bearing unknown" is presentation for Vulkan clients. **R7:** that statement is stale. | **R7 is right.** The dcomp vehicle shipped in the 28th session and is default-ON. The load-bearing present issues are P-A/P-B/P-C in §3-H2, not the hand-off. | `icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:361-376`, `:2067-2258`. |
| **DX12.md §3.2:** `DXGK_VIDMMCAPS.DriverManagesResidency` is not set. | **Field misattributed.** `DriverManagesResidency` is a **`DXGK_CONTEXTINFO_CAPS`** bit (per-context, `d3dkmddi.h:1550-1563`), not a `DXGK_VIDMMCAPS` bit. The conclusion (residency is VidMm's job) is unchanged; Helios never writes `ContextInfo.Caps` at all. | `research/R5.md` §11. |
| **DX12.md §2(b):** "requires from the UMD: nothing; `umd/` is not in this path." | **Wrong.** The vehicle runs `D3D11CreateDevice` on the Helios adapter inside the vkd3d process, and every vkd3d frame goes through `helios_umd!dxgi_present`; the three `helios_umd_*` exports are load-bearing. | `umd/src/vehicle_exports.rs:7-11`; `research/R7.md` §8. |

---

### 6.1 Resolutions issued after the first doc pass (2026-08-05, verification round)

| Question | Resolution |
|---|---|
| **Shader-model ceiling: 6.6 or 6.7?** | ⛔ **Both answers were low. Measured: 6.8.** The ladder does not stop at 6.7 — `device.c:10817-10820` adds an unconditional 6.7→6.8 step, which `SUBSTRATE.md` §7.2's walk did not enumerate. The live guest logs `Enabling support for SM 6.6.` → `6.7.` → `6.8.` and `CheckFeatureSupport(SHADER_MODEL)` answers **6.8** (`D12-G1`). **Canonical: SM 6.8.** The "plan for 6.0, treat above as upside" hedge is retired — H5 is closed. |
| **G0 build: Linux mingw cross, or native MSVC on the VM?** | **Linux mingw cross is the primary**, because the host already has the whole toolchain installed — `x86_64-w64-mingw32-gcc`, `widl`, `glslangValidator`, `meson`, `ninja` are all on `PATH` today (verified). Zero installation, and it matches vkd3d-proton's own shipping build (`artifacts.yml`). Native MSVC x64 on the VM (upstream's `test-build-windows.yml`: choco strawberryperl + glslangValidator + meson + VS2022, built to a **local C:** path) is the **fallback, taken when a Windows debugger is wanted**. `ARCHITECTURE.md` §8.3 and `GATES.md` G0 must both say this. |
| **`InstalledDisplayDrivers`: 2 entries or 4?** | **2** — `helios_umd,helios_umd12`. It is a flat list of distinct package binaries, not index-parallel to `UserModeDriverName`. The live four-times value is semantically wrong today; fixing it is part of the INF change, not a separate item. |
| **PRESENT's `HELIOS_WSI_INSURANCE_BLIT` "numbers never landed"** | **Wrong** — the A/B landed: `ROADMAP.md:2919-2926` and `:2948-2950` record an owner Doom verdict run with `insurance=0` showing no fps change. Copy #3 is **measured inert at Doom resolution**. Re-measure at D3D12 resolutions before claiming it costs anything. |
| **PRESENT's "no post-fix fullscreen vehicle measurement exists"** | **Wrong** — `ROADMAP.md:2919-2931` is that measurement (the fullscreen 1896×1030 chain went VEHICLE, READY+LIVE on the same hwnd, after the target-registry fix). Narrow the open item to: those numbers were taken with `VehicleKernelFlipWait=1`, which R912(a) has since retired, so re-measure on the shipping gate path. |
| **Venus-level host logging lever** | `HELIOS_VKR_DEBUG=validate` (owner-gated relaunch), **not** `VIRGL_LOG_LEVEL=debug` — `ROADMAP.md:1901-1903`. `GATES.md` §5.2 must be corrected. |

## 7. Standing constraints the D3D12 work inherits

These are not new; they are the ones most likely to be violated by a D3D12 implementer.

1. **`OpenAdapter12` must stop refusing in the same commit that makes its body reachable — or the
   body must not be written yet.** R908 deleted ~230 lines of unreachable D3D12 scaffolding hidden
   behind `#[allow(unreachable_code)]`; that is the standing proof of the cost.
2. **Every DDI ABI struct comes from the WDK header through bindgen with `layout_tests(true)`.**
   Never hand-transcribed. `adapter.rs:36-45` records a 376..392-byte out-of-bounds write into the
   runtime's heap from a hand-written table.
3. **Honour `pfnFillDDITable`'s `SIZE_T` argument.** Never write `size_of::<T>()` bytes. This is the
   R702 class (24H2 passing 576 B for a 592-byte `DRIVERCAPS`), and D3D12 parameterises it
   explicitly.
4. **Unknown interface/version → a closed enum with an exhaustive match**, never an `else` that
   fills the largest table.
5. **Declining an unimplemented interface is `DXGI_ERROR_UNSUPPORTED` (0x887A0004), never
   `DXGI_ERROR_DRIVER_INTERNAL_ERROR` (0x887A0020)** — the latter is recorded by the runtime and ETW
   as a driver fault.
6. **No `panic!`/`todo!`/`unwrap` on runtime data.** A panic in any DDI is a silent graphics
   deadlock. Many D3D12 DDIs return `VOID`; errors go out through `pfnSetErrorCb` /
   `pfnSetCommandListErrorCb`.
7. **Every skipped or refused path gets a named counter**, with a readout. Refuse at the *first*
   step, never succeed-then-fail (the `CreateHwQueue`/`HwQRef` model).
8. **Advertising a capability that is not backed is a lie the OS acts on.** D3D12's 43 caps types
   and 16 tiered enums are the densest version of this hazard the project has faced.
9. **Never reintroduce a producer-side CPU present gate** (owner directive, 2026-07-29).
10. **`RelocateDeviceFuncs`-style callbacks are notifications, never a signal to refill a live
    table.**
11. **Only owner-visible desktop state counts as rendering evidence** (`helios_paintcap` →
    `Z:\tmp\screen_copy.png`). Log lines are not frames. Registry counters persist across boots —
    verify a counter *moves this boot*. Anything with a window runs in **session 1** via a cloned
    scheduled task.
12. **A frozen benchmark is a defect to root-cause, never a retry.**
13. ⚠ **dwm.exe already calls our `OpenAdapter12` in production.** Enabling D3D12 is a change to the
    compositor's behaviour on the next boot. Hence D11.

**Decision D11 — D3D12 ships behind an off-by-default kill switch.**
`HKLM\SOFTWARE\Helios!UmdD3D12` (`BoolKnob::new(c"UmdD3D12", false)`), read once per process at the
top of `OpenAdapter12`. Absent ⇒ `DXGI_ERROR_UNSUPPORTED`, i.e. bit-identical to a build without the
D3D12 path. `HKLM\SOFTWARE\Helios` is writable over SSH with the desktop down; the knob is read once
per process so a running dwm keeps its behaviour while new processes pick up the change. The flip to
default-ON requires the evidence in the comment at the read site (CLAUDE.md rule 8).

**Decision D12 — the DDI version is `D3D12DDI_SUPPORTED_0110`, advertised as a set of exactly ONE
token, with the `_0109`-generation tables. Decided 2026-08-06, before the S6 fan-out.**

`PARALLEL.md` §8 lists this as the one remaining not-parallelisable choice and requires it be made
*before* lanes start, because the lane split, every slot count in §4.1 and `DDI_REFERENCE.md` §3.2 /
§4.2's group boundaries are all derived from the chosen revision. It is decided here, once.

| | `_0110` — **chosen** | `_0040` — rejected |
|---|---|---|
| baseline driver-side slots | 214 (8 + 124 + 75 + 7) | 169 (8 + 96 + 58 + 7) |
| object model | pool + recorder | the retired command **allocator** family (`CORE_0033` and older) |
| `pfnFillDDITable` `TableSize` this runtime passes | 992 / 600 / 56 | 768 / 464 / 56 |
| caps gauntlet | 43 types, ~60 cross-tier rules | **identical** — an older token softens nothing |
| `VulkanOn12` obligations | thirteen, no cap, cannot be declined (`SUBSTRATE.md` §4.5) | none |

Reasons, in order:

1. ✅ **Measured: `_0110` is what this runtime asks for first.** `D12-G5` logged WARP's 77-token list
   and the runtime picking `_0110` out of it (`DDI_REFERENCE.md` §1.5). `_0040` is *accepted* and a
   triangle presents on it (§15.4), so the trade was real — but taking it means every count in §4.1,
   every group boundary in `DDI_REFERENCE.md` §3.2/§4.2 and the whole `PARALLEL.md` §4 lane table
   would have to be re-derived against `CORE_0040`/`_0040`-generation command lists, for which this
   directory holds **no** counts at all. That is a doc re-derivation with its own miscount risk
   (§4.1's own warning: "several were miscounted independently by more than one research lane") in
   exchange for 45 stubbed slots.
2. **The 45 slots `_0040` saves are the cheap ones.** They are state objects, mesh shaders, work
   graphs, enhanced barriers and VRS — `PARALLEL.md`'s L9, *"mostly refuse-and-count"*. The
   expensive surface (caps, queue, recording, descriptors, PSO) is present in both.
3. **`_0040`'s saving is paid back immediately in the object model.** It predates the pool + recorder
   split and carries `pfnCalcPrivateCommandAllocatorSize` / `pfnCreateCommandAllocator` /
   `pfnDestroyCommandAllocator` / `pfnResetCommandAllocator` (`d3d12umddi.h:1740-1743`), a shape
   vkd3d does not model and which nothing else in this plan is written against.

⚠ **What choosing `_0110` costs, stated rather than discovered later.** The thirteen `VulkanOn12`
obligations carry no cap and cannot be declined. `SUBSTRATE.md` §4.5 names the four that bite a
Vulkan-backed forwarder — triangle fans (0097+), mismatched RT/DS sizes (0102+), dynamic-state PSO
flags being **hints** and not the Vulkan "baked value is ignored" semantics, and non-normalized
sampler coordinates (0100+) — plus the `DepthBias` `INT`→`FLOAT` reinterpretation at 0099 and the
`D3D12DDI_RASTERIZER_DESC_0102` re-rev. **Each one that Helios cannot honour gets a named refusal
counter in its owning lane, not silence.** They are lane obligations now, not an open question.

⛔ **Advertise exactly one token.** `pfnGetSupportedVersions` reports a one-element set, so the
runtime either negotiates `_0110` or fails the handshake with its own string (*"Failed to find
matching DDI versions"*). That is what makes `ARCHITECTURE.md` §12 trap 2's closed enum trivially
exhaustive: there is one legal `(Interface, Version)` pair, every other pair is a counted refusal,
and there is no revision under which a `_0109`-shaped table could be written into a buffer sized for
something else. ⛔ Adding a second token is a behaviour change that needs its own gate — it makes a
second table shape reachable, which is the R702/§12-trap-2 surface this decision closes by
construction.

⚠ **`_0110` adds no table struct of its own.** It reuses `D3D12DDI_ADAPTERFUNCS_0109`,
`D3D12DDI_DEVICE_FUNCS_CORE_0109`, `D3D12DDI_COMMAND_LIST_FUNCS_3D_0108` and
`D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001` — so "negotiate `_0110`, fill the `_0109` generation" is one
decision, not two. The token itself is **composed from bindgen'd constants**
(`D3D12DDI_INTERFACE_VERSION_R8`, `D3D12DDI_BUILD_VERSION_0110`), never transcribed: bindgen does not
emit `D3D12DDI_SUPPORTED_0110` at all, because the macro casts through `(UINT64)`.

**Decision D13 — ⛔ OWNER REQUIREMENT, 2026-08-06, then REFINED the same day once its own source was
read: private data that CROSSES a module boundary is declared exactly once, in `helios_protocol`.
Private data that does not cross one stays in the crate that owns it.**

> *"if we must use any private data, make sure its part of the `umd_common` crate, its a hard
> requirement"* … *"my argument for shared private data stems from §4.3 of the `DX12.md` doc, if
> there's an alternative, feel free to pivot"* — owner.

**The source, `DX12.md` §4.3 row 6, resolves to D3c above and to `ResourceHeaps.md:198`:**

> The driver must construct private data that is **consumable by their D3D11 driver** and will be
> interpreted as a shared tile pool.

⭐ **That sentence is about the private data attached to a shared *allocation*, and Helios already has
it — in `protocol/src/wddm.rs`, not in `umd_common`.** `HeliosWddmAllocPrivate` (magic `'HWDM'`,
version 1) is what `helios_umd.dll` writes into
`D3DKMTCreateAllocation`'s `pAllocationInfo[i].pPrivateDriverData`
(`umd/src/forward/resource.rs:351-369`), and `HeliosWddmOpenIdentity` (magic `'HIDN'`, version 1) is
the record the **KMD** stamps back at `DxgkDdiOpenAllocation` after validating the venus resource is
live, which the opening driver reads (`resource.rs:1303-1322`). It is `#[repr(C)]`, `Pod`/`Zeroable`,
padding-free, magic- and version-tagged. **It is already the cross-driver contract the spec
describes, and it already has three readers: `umd`, `kmd_render` and the Mesa ICD.**

⇒ **The rule.** Every byte of private data that one module writes and another reads is declared once,
in `helios_protocol`, and `helios_umd12.dll` reuses those declarations byte for byte. It never
re-declares them, never writes a D3D12-flavoured variant, and never invents a second magic. A D3D12
resource opened by DWM through the D3D11 DDI is then readable **by construction** rather than by two
declarations happening to agree.

⛔ **`umd_common` is the wrong home for it, and the reason is load-bearing:** `kmd_render` is a
`no_std` cdylib that reads these same structs and does **not** depend on `umd_common` (which is
`std`, Windows-user-mode, and pulls `windows`). `protocol` is the crate that already builds on both
platforms *and* in the kernel. Moving the allocation private data into `umd_common` would put it
somewhere the KMD cannot see.

### What is NOT covered, and why the broad reading was rejected

The broad reading — *"every `CalcPrivate*Size` payload struct moves to a shared crate"* — was written
first and then withdrawn. `CORE_0109` has 26 `CalcPrivate*` slots, and those blocks are:

* **runtime-allocated, per-object, per-process, and never read by another module.** `pDrvPrivate`
  memory belongs to one driver inside one device. Sharing its *Rust type* buys nothing for D3c,
  because nothing on the other side of the boundary ever reads it.
* **the one place the compiler can type a payload against the handle it belongs to.**
  `ARCHITECTURE.md` §12 rule 7 is the scar: `load_com::<ID3D11RenderTargetView>(h_rtv)` compiled and
  produced a `ManuallyDrop` whose vtable pointer was a struct field. `umd_common::slot`'s
  `BoxedHandle::State` fixes that by naming the payload from the *handle type*, and `slot.rs:94-97`
  records that the associated type *"may be a type private to the implementing crate. That is
  deliberate."*
* **hostile to the fan-out.** A single shared payload file would be the hottest merge point in an
  11-lane split — the exact contention `PARALLEL.md` §5 exists to remove.
* **forced to lose type safety.** `umd_common` must keep building on Linux and must not grow a
  `build.rs` (D3b), so it cannot name a `ddi12` type; every payload field would degrade to a bare
  integer at precisely the sites that most need typing.

⇒ Per-object `pDrvPrivate` payloads stay in the crate that owns them, typed against `ddi12`.

### What this obliges the S6 lanes to do

1. **`umd12` takes the `helios_protocol` dependency** (`ARCHITECTURE.md` §5's `Cargo.toml` sketch
   already lists it; the crate as built at S5 does not have it yet).
2. **L4 (`forward12/resource12.rs`) writes `HeliosWddmAllocPrivate` and reads
   `HeliosWddmOpenIdentity` exactly as `umd/src/forward/resource.rs` does** — same magic, same
   version, same `kind`, same meta trailer. ⛔ Not "a compatible layout": the same struct from the
   same crate. That is D3c's requirement discharged in code rather than deferred to `GATES.md` §7.23.
3. **L8 (`forward12/present12.rs`) reuses `HeliosPresentRenderCmd` / `HeliosPresentPrivateData`**,
   for the same reason and with an extra one: the KMD decodes them (`kmd_render/src/device.rs:46`),
   and the 64th memory records that Present private data never reaches `DxgkDdiPresent` on DMA flips
   — it rides the Render command. A second D3D12 spelling of that channel would be a second thing the
   KMD has to recognise.
4. **Any genuinely new cross-module record is a new `protocol` struct**, with its own magic and
   version, added there and nowhere else.

⚠ D13 shares declarations, not claims: `Slot<Boxed<S>>::get()`'s soundness argument is still D3D11's
`CUseCountedObject` one and is **not** established for D3D12 (`slot.rs:304-322`, `PARALLEL.md` §9.4).

---

## 8. Deliverable map

| Document | Answers |
|---|---|
| `DX12.md` (repo root) | the charter: decision, phases, checkpoints, current status |
| `docs/dx12/DECISIONS.md` | **this file** — what was decided and why; conflict resolutions |
| `docs/dx12/ARCHITECTURE.md` | the UMD split: crates, DLLs, bridge, INF, build, deploy, rollback |
| `docs/dx12/DDI_REFERENCE.md` | the `d3d12umddi` contract: tables, negotiation, caps, objects, fences, minimum-viable set, undocumented questions |
| `docs/dx12/PRESENT.md` | how a D3D12 frame reaches the scanout, both arms |
| `docs/dx12/SUBSTRATE.md` | vkd3d-proton + venus: build, requirements, measured gap, knobs, licensing |
| `docs/dx12/GATES.md` | `D12-G0 … D12-G11`, exact commands and pass criteria |
| `docs/dx12/SPECS.md` | the DirectX-Specs corpus triaged (90 docs) + the 235-finding register, pinned at `2bd58ca5` |
| `docs/dx12/research/R1..R12` | the raw evidence dossiers |
