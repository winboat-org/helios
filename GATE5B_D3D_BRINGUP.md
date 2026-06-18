# Gate 5b — a WDDM D3D11 UMD on Helios (reusing DXVK's engine)

> **★ IMPLEMENTATION UPDATE (2026-06-18) — UMD is Rust + cxx, NOT a C++ `umd11.dll`.**
> Per the owner's direction the UMD is the **existing Rust `umd/` crate
> (`helios_umd.dll`)** bridged to DXVK's C++ engine via
> [cxx](https://github.com/dtolnay/cxx) — not a separately-written C++ `umd11.dll`.
> The "link not fork" / "reuse `src/dxvk` + dxbc-spirv under our DDI frontend"
> design below is unchanged; only the frontend language (Rust) and the C++ interop
> mechanism (cxx) differ. **VALIDATED LIVE:** DXVK builds under clang-cl (MSVC ABI,
> `/MD`), links into the Rust DLL, and a temp `helios_umd_selftest` export brings up
> a `DxvkDevice` on the venus adapter (D3DKMTEscape `CTX_CREATE` fires). The exact
> build recipe + cxx gotchas (pimpl, `/EHsc`, `Logger::s_instance`, the
> `ssize_t`/libdisplay-info shim, MS-COFF `.a` archives) are in the
> `gate5b-dxvk-cxx-bridge` memory. Read that for current state; §6 below is the
> original plan. Files: `umd/build.rs`, `umd/src/bridge.rs`,
> `umd/bridge/dxvk_bridge.{h,cpp}`, `umd/build-support/dxvk_c_compat.h`.

> Status: design/research, 2026-06-18. Prereq: **Gate 5a DONE** — the venus Vulkan
> stack works on the `kmd_render` WDDM render adapter over `D3DKMTEscape`
> (`vulkaninfo` enumerates `Virtio-GPU Venus (NVIDIA RTX PRO 6000 Blackwell)`,
> `driverID=MESA_VENUS`). See the `gate5a-venus-d3dkmt` memory.
>
> This doc exists so the next session does **not** re-architect from scratch. Read it,
> then the `dwm-crash-vrd-pairing-rootcause` memory, then §6 (the build plan).

---

## 0. The goal (one sentence)

Ship a **real WDDM Direct3D 11 user-mode driver (UMD)** for the Helios adapter so that
`D3D11CreateDevice(Helios)` **succeeds system-wide** — which makes **DWM composite on
Helios**, brings the Windows **desktop + every D3D app + games** up, and renders them
on the host GPU through venus. The UMD is **written by us** but **reuses DXVK's
Vulkan-translation engine and DXBC shader compiler** as its backend; it does not *be*
DXVK.

### Why this, and why NOT per-app DXVK drop-in
Per-app DXVK/VKD3D drop-in (a `d3d11.dll` next to a game's `.exe`) was **already
possible with the old System-class driver** — it needs no WDDM adapter at all. **That
is not the goal and we will not spend any time on it.** The whole reason Helios is a
WDDM adapter is to provide the thing a drop-in can never provide: a driver the **OS
D3D runtime and DWM load themselves**, so D3D works everywhere without touching each
app. That requires a UMD. This doc is exclusively about building it.

---

## 1. Context — the DWM crash this fixes (recap)

From `dwm-crash-vrd-pairing-rootcause` (ntoseye + crash-dump, authoritative): Windows
pairs the render-only Helios adapter with the display-only Looking Glass IDD (WDDM
"Virtual Render Device" pairing → *"all UI rendering occurs with the VRD adapter"*), so
**DWM composites on Helios**. DWM calls `D3D11CreateDevice(Helios)`, gets
`0x887a0004 DXGI_ERROR_UNSUPPORTED` (Helios exposes **no D3D UMD** — its
`GetSupportedVersions` is empty), and fail-fasts on its chosen composition adapter.
`GpuVirtualizationFlags` (unpair) did **not** help (Helios isn't a GPU-PV VRD).
**A successful `D3D11CreateDevice` on Helios is the fix** — i.e. a UMD.

---

## 2. Where a UMD sits (the altitude that makes DXVK-drop-in the wrong layer)

```
   App / DWM
      │  ID3D11Device / ID3D11DeviceContext (COM)
      ▼
   OS d3d11.dll  (the Direct3D 11 RUNTIME — Microsoft's, always loaded)
      │  d3d10umddi DDI  (flat C pfn tables: CreateResource, Draw, Map, …; DXBC shaders)
      ▼
   ►► OUR UMD (umd11.dll)  ◄◄  ← we write this; backend = DXVK core + DXBC compiler
      │  D3DKMTEscape / Render  (the Gate-5a venus transport)
      ▼
   kmd_render (KMD) ──► venus ──► host GPU
```

- The OS `d3d11.dll` runtime sits **above** the UMD and has already lowered the app's
  COM calls into the **`d3d10umddi` DDI** (a different, lower-level interface than the
  COM API). The UMD implements that DDI.
- DXVK's `d3d11.dll` is a **replacement for the OS runtime** (the wrong altitude — it
  would have to *be* `d3d11.dll`, which only works as a per-app drop-in). So we don't
  reuse DXVK's frontend; we reuse its **engine** under our own DDI frontend.
- Registration: the Helios INF gets
  `HKR,, UserModeDriverName, %REG_MULTI_SZ%, umd11.dll` (+ `UserModeDriverNameWoW` for
  32-bit). The runtime then loads `umd11.dll` for the Helios adapter automatically.

This architecture is proven in shape by Mesa's `gallium/frontends/d3d10umd` (DDI →
gallium → zink → Vulkan). We do the same shape with DXVK's engine instead of gallium.

---

## 3. The reuse map — what comes from DXVK vs. what we write

| DXVK component | Role | Reuse? |
|---|---|---|
| `src/d3d11` | COM API frontend (= `d3d11.dll`) | **No** — wrong altitude. **Use as the reference** for how D3D11 semantics map to the engine. |
| `src/dxgi` | DXGI adapters/swapchains | **No** — DXGI/present is the OS runtime + dxgkrnl + our KMD, not the UMD. |
| `src/dxvk` | **Core engine**: `DxvkDevice`/`DxvkContext`, command lists, memory allocator, barrier & state tracking, pipeline/PSO management, Vulkan submission | **Yes — this is the prize.** |
| `src/dxbc` (+ `src/spirv`) | **DXBC → SPIR-V** shader compiler | **Yes, directly** — the D3D11 UMD DDI delivers shaders as DXBC, exactly DXVK's input. |
| backend Vulkan device | what the engine targets | **Our venus ICD** (`vulkan_virtio.dll`) — force-select `Virtio-GPU Venus`. |

So the UMD = **`d3d10umddi` frontend (new) → `src/dxvk` core (reused) + `src/dxbc`
(reused) → venus Vulkan ICD.** Conceptually it mirrors `src/d3d11`, reshaped from
"COM methods the app calls" into "DDI callbacks the runtime calls."

---

## 4. What we have to write (the honest scope)

### 4.1 The `d3d10umddi` DDI frontend (`umd11.dll`)
- Export `OpenAdapter10` / `OpenAdapter10_2`; fill `GetSupportedVersions` **non-empty**
  (the empty one is literally why DWM gets `UNSUPPORTED`) and `GetCaps`.
- Implement the device pfn tables (`D3D11DDI_DEVICEFUNCS`, `D3D11_1DDI_…`,
  `D3DWDDM2_xDDI_…` for the version slice we claim): `CalcPrivate*Size` / `CreateDevice`
  / `CreateResource` / `OpenResource` / `DestroyResource` / `ResourceMap`/`Unmap` /
  `Create*View` / `Create*Shader` (DXBC in) / the state setters / `Draw*` / `Dispatch` /
  `Clear*` / `Flush`, etc. Each maps onto a `DxvkContext`/`DxvkDevice` operation.
- **Inverted ownership model (the main reshaping work):** the runtime calls
  `pfnCalcPrivate{Device,Resource,…}Size`, allocates that storage itself, and the
  create-DDI constructs the driver object **in place** at the runtime-owned pointer.
  DXVK's frontend instead owns lifetimes via COM refcounting + its own allocation.
  Simplest bridge: the runtime-owned private data holds a **pointer/handle to a
  heap-allocated DXVK-core object** the UMD owns; the in-place shell stays thin.
- Map handles ↔ DXVK objects; translate D3D11 enums/state to DXVK/Vulkan.

### 4.2 WDDM resources, sharing, and **presentation** (the hard back-half)
This is what actually makes **DWM composite and the desktop appear**, and it is hard
**regardless of backend**:
- **Cross-process shared surfaces.** DWM composes surfaces produced by *other*
  processes. The UMD + KMD must support shareable allocations (NT-handle / KMT shared
  resources) and `OpenResource` of a peer's surface.
- **Present / blt DDIs** + the **KMD present path.** `kmd_render`'s `dxgkddi_present`
  is currently a **stub**; presenting DWM's composed frame needs a real present/blt and
  fences in the KMD, plus the cross-adapter handoff to the **display-only LG IDD** for
  scanout (Helios renders; the IDD scans out).
- ⚠️ **Cross-adapter export overlaps the old Phase-7 pain.** Getting a venus-rendered
  image to the LG IDD means exporting it (shared/dmabuf), which is exactly where the
  earlier `SET_SCANOUT_BLOB` / dmabuf experiments went **BLACK** (see `phase7-gate-status`
  / `DISPLAY.md`). Expect this sub-problem to be its own investigation; the venus image
  must be created **exportable / LINEAR** with the right modifier.

### 4.3 KMD (`kmd_render`) work this requires
- Real `DxgkDdiPresent` (+ patch/submit for the render queue) — today stubbed.
- Shareable cross-process allocations (extend the Gate-5a blob/alloc path).
- Present fences / sync with the IDD scanout.
- (The Gate-5a venus transport — ctx/blob/submit/wait over Escape — is reused as-is for
  the rendering itself.)

### 4.4 The DXVK-core extraction caveat
`src/dxvk` is **not packaged as a clean third-party `libdxvk`** — it is somewhat
coupled to `src/d3d11`. Carving the engine out to drive it from a DDI frontend is
itself real work. **First task: read `dxvk-helios/src/dxvk` public surface** and decide
what to link vs. fork. The in-tree `dxvk-helios/` fork (`github.com/rupansh/dxvk`) is
where that surgery lives.

---

## 5. Milestone ladder (each is independently observable)

1. **UMD loads + `D3D11CreateDevice(Helios)` returns S_OK** with a near-empty device
   (non-empty `GetSupportedVersions`, minimal `CreateDevice`). *Validation:* the
   `d3d11_devicecreate_probe` / `dxgi_enum` tools (already in `tools/`) show S_OK on
   Helios, and **DWM stops fail-fasting** (the crash-loop ends). This alone is the
   headline fix even before anything renders.
2. **Clear + present a solid color** → DWM shows a (blank) composed desktop via LG.
   Exercises the present + shared-surface + IDD-scanout path (§4.2) — the real risk.
3. **Resources + DXBC shaders + a draw** (reuse `src/dxbc`, `src/dxvk`) → a triangle in
   a window, composed by DWM, visible in LG.
4. **Real D3D11 desktop apps**, then **D3D11 games** (no per-app anything — they just
   work because the OS loads our UMD).
5. **D3D12** later via the same pattern (a `d3d12umddi` UMD reusing VKD3D-Proton's core,
   from `vkd3d-proton-helios/`) — separate, after D3D11 is solid.

Note milestone 1 (CreateDevice S_OK → DWM unblocked) is decoupled from the hard present
work (milestone 2). Land 1 first; it's the proof the UMD path is real.

---

## 6. Recommended first steps for the next session

1. Read `dxvk-helios/src/dxvk` — map the engine's usable surface; decide link-vs-fork.
   Read `dxvk-helios/src/d3d11` as the **semantic-mapping reference**.
2. Stand up a **minimal `umd11.dll`**: `OpenAdapter10_2` + non-empty
   `GetSupportedVersions` + a `CreateDevice` that constructs a DXVK `DxvkDevice` on the
   venus ICD (`DXVK_FILTER_DEVICE_NAME="Virtio-GPU Venus"` / `VK_DRIVER_FILES` to the
   Helios manifest). Register it in the Helios INF (`UserModeDriverName`).
3. Get **milestone 1** (CreateDevice S_OK, DWM crash-loop ends) — the first hard proof.
4. Then attack §4.2 present/shared-surface/IDD-scanout (the genuine risk), reusing the
   Phase-7 dmabuf findings.

Builds on the validated Gate-5a substrate unchanged: KMD `.72` (Code 0), ICD at
`C:\ProgramData\HeliosVulkan\vulkan_virtio-9e1534dc4ffc.dll`.

---

## 7. Alternatives considered (and why the DXVK-core UMD wins)

- **Mesa `gallium/frontends/d3d10umd` + `zink`** — same DDI→Vulkan shape and it already
  *has* the DDI frontend + Windows WDDM packaging, so less new code; but the engine is
  gallium/zink (less battle-tested for D3D11 than DXVK) and its d3d10umd frontend may be
  immature. Worth a look as a head-start on the DDI-frontend boilerplate, but DXVK's
  engine is the stronger D3D11 backend. **Keep as a fallback reference for the DDI shell.**
- **Adapt D3D11On12 → Vulkan** — D3D11On12 is an API-layer shim, not a UMD shell; you'd
  still write the DDI frontend, and you'd add a D3D12 layer in the middle. Worse.
- **Sidestep DWM (keep it off Helios / direct-present to IDD)** — narrow, app-specific,
  doesn't give a general visible desktop; the unpair lever already failed. **Rejected.**

---

## 8. Risks / open questions

- **DXVK-core extraction** — coupling to `src/d3d11`; how clean a boundary exists.
- **Presentation + cross-adapter scanout to the LG IDD** — the §4.2/Phase-7 black-screen
  risk; the venus image must be exportable/LINEAR. This is the biggest unknown.
- **DDI version/caps coherence** — claim a slice the runtime + DWM accept (DWM is picky;
  internally inconsistent caps get rejected, cf. the KMD cap-bisect history).
- **DDI ownership/threading model** vs. DXVK's assumptions (runtime-owned object memory,
  the runtime's threading/deferred-context model).
- **Effort** — this is the largest remaining task in the project; milestone 1 is a
  reachable early win, milestone 2 (present) is where the real depth is.

---

## 9. References

- D3D11 UMD DDI: "Enabling Support for the Direct3D Version 11 DDI"
  <https://learn.microsoft.com/windows-hardware/drivers/display/enabling-support-for-the-direct3d-version-11-ddi>;
  `D3D11DDI_DEVICEFUNCS` / `D3D11_1DDI_DEVICEFUNCS` (d3d10umddi.h)
  <https://learn.microsoft.com/windows-hardware/drivers/ddi/d3d10umddi/ns-d3d10umddi-d3d11ddi_devicefuncs>.
- DXVK (engine + DXBC compiler to reuse): <https://github.com/doitsujin/dxvk>
  (`src/dxvk`, `src/dxbc`, `src/d3d11` as reference). In-tree fork: `dxvk-helios/`.
  "DXVK as a UMD" upstream request, closed *not planned* (so we do it downstream; the
  requester's use case was VM 3D via VirGL/Venus = ours):
  <https://github.com/doitsujin/dxvk/issues/3345>.
- Mesa d3d10umd reference shape: <https://docs.mesa3d.org/> (gallium d3d10umd frontend),
  `vkd3d-proton-helios/` for the eventual D3D12 UMD.
- VRD pairing / DWM: repo
  `windows-driver-docs-research-only/.../display/gpu-paravirtualization.md`;
  memory `dwm-crash-vrd-pairing-rootcause`. Cross-adapter scanout pain: `DISPLAY.md`,
  memory `phase7-gate-status`.
- venus: <https://docs.mesa3d.org/drivers/venus.html>.

---

## 10. Build / registration quick reference (win11)

- **Gate-5a substrate (reused, installed):** ICD `vulkan_virtio.dll` →
  `C:\ProgramData\HeliosVulkan\vulkan_virtio-9e1534dc4ffc.dll`; KMD `.72` Code 0.
- **UMD build:** new `umd11.dll` linking DXVK core from `dxvk-helios/` (mingw/meson, the
  same toolchain family as the Mesa ICD). Target the venus ICD via
  `DXVK_FILTER_DEVICE_NAME=Virtio-GPU Venus` (+ `VK_DRIVER_FILES` to a Helios-only
  manifest if any other Vulkan driver is present).
- **Register:** add `UserModeDriverName`(+`WoW`) = `umd11.dll` to the Helios INF, bump
  version, `win_cargo kmd_render make`, `devcon update`, reboot/relaunch as needed.
- **Validate milestone 1:** `tools/d3d11_devicecreate_probe.cpp` + `tools/dxgi_enum.cpp`
  show `D3D11CreateDevice(Helios)=S_OK`; watch the DWM crash-loop stop.
- NO vkcube; NO per-app DXVK drop-in (that's the rejected Track A).
