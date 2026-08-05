# R10 — Prior art and precedent survey (D3D12 for Helios)

**Lane:** R10. **Date:** 2026-08-05. **Scope:** what the world already knows about (a) the D3D12
UMD DDI as a third-party target, (b) D3D12-over-translation-layer implementations, (c)
vkd3d-proton on native Windows, (d) vkd3d-proton over virtualized Vulkan / venus, (e) other
virtual-GPU vendors' D3D12 story, (f) the Agility SDK redistribution mechanism, (g) public
"don't do this" evidence.

**Evidence discipline.** Every claim below is tagged with its class:
- **[HEADER]** — read out of a local SDK header in `tmp/dx12/sdk/`.
- **[SRC]** — read out of local source (`vkd3d-proton-helios/`, `icd/mesa/`).
- **[MSDOC]** — Microsoft documentation (URL or local `windows-driver-docs-research-only/` mirror).
- **[VM]** — a command actually run on the win11 dev VM, output quoted.
- **[WEB]** — third-party web source, URL given.
- **[INFER]** — my reasoning on top of the above. Never presented as fact.
- **UNVERIFIED** — could not be settled; the settling experiment is stated inline.

---

## Q1 — Is the D3D12 UMD DDI publicly documented at all?

### Q1.1 Yes, as auto-generated reference pages — in a repo we do NOT have locally

**[VM/local]** Our local docs mirror is `windows-driver-docs-research-only/windows-driver-docs-pr/`
— the **conceptual** docs repo only:

```
$ ls windows-driver-docs-research-only
ci-pipeline.yml  CONTRIBUTING.md  LICENSE  LICENSE-CODE  README.md  ThirdPartyNotices
windows-driver-docs-pr
$ find . -maxdepth 2 -type d -name "*ddi*"      # → no results
```

**[local]** In that mirror, `display/` has 460 files, of which exactly **3** have `d3d12` in the
filename (`d3d12-render-passes.md`, `video-encoding-d3d12.md`, `video-encoding-d3d12-av1.md`) and
**11** files mention any `D3D12DDI_` symbol at all:

```
$ grep -ril "D3D12DDI_" windows-driver-docs-research-only/ | wc -l
11
```
The 11: `d3d12-render-passes.md`, `direct3d-functions-implemented-by-user-mode.md`,
`generic-programs.md`, `gpu-paravirtualization.md`, `video-encoding-d3d12-av1.md`,
`what-s-new-for-prior-wddm-2-x-versions.md`, `video-encoding-d3d12.md`,
`what-s-new-for-windows-10-display-and-graphics-drivers.md`, `signaling-cpu-event-from-kmd.md`,
`work-graphs.md`, `direct3d-runtime-functions-called-by-user-mode.md`, `enhanced-barriers.md`.

**So the prompt's suspicion is half right.** The reason our mirror looks empty of D3D12 UMD DDI
pages is **not** that Microsoft documents it only via the header — it is that the DDI reference
lives in a **different repository** that we did not clone:

- **[WEB]** `MicrosoftDocs/windows-driver-docs-ddi`, path `wdk-ddi-src/content/d3d12umddi/` —
  https://github.com/MicrosoftDocs/windows-driver-docs-ddi/tree/staging/wdk-ddi-src/content/d3d12umddi
  Rendered at https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3d12umddi/
- **ACTION FOR THE IMPLEMENTER:** clone `MicrosoftDocs/windows-driver-docs-ddi` (or at least
  `wdk-ddi-src/content/d3d12umddi/`) into the research tree. It is markdown, greppable, and it is
  the only per-symbol prose that exists.

**UNVERIFIED:** the exact file count in that directory. The GitHub contents API paginates and
GitHub's HTML tree view did not render a count through WebFetch. Settling experiment:
`git clone --filter=blob:none --sparse` the repo and `ls wdk-ddi-src/content/d3d12umddi | wc -l`.
The learn.microsoft.com index page (https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3d12umddi/)
reports section headings "Callback functions" and "Structures" with, per WebFetch's read of the
truncated page, "400+" callbacks and "200+" structures — treat those as order-of-magnitude only;
**R1 owns the authoritative counts, from `tmp/dx12/sdk/d3d12umddi.h` itself.**

### Q1.2 …but the reference pages are near-contentless stubs

This is the finding that matters. **[WEB]** A representative core-DDI page,
https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3d12umddi/nc-d3d12umddi-pfnd3d12ddi_create_command_list_0040,
is verbatim, in its entirety, after the front matter:

> Pointer to the CreateCommandList function that creates a command list.
>
> ## Syntax
> ```cpp
> HRESULT Pfnd3d12ddiCreateCommandList0040(
>   D3D12DDI_HDEVICE unnamedParam1,
>   const D3D12DDIARG_CREATE_COMMAND_LIST_0040 *unnamedParam2,
>   D3D12DDI_HCOMMANDLIST unnamedParam3,
>   D3D12DDI_HRTCOMMANDLIST unnamedParam4
> )
> ```
> ## Parameters
> `unnamedParam1` — A handle to the display device (graphics context).
> `unnamedParam2` — Pointer to a D3D12DDIARG_CREATE_COMMAND_LIST_0040 structure that describes the
> parameters that the user-mode display driver uses to create a command list.
> `unnamedParam3` — A handle to the driver's data for the command list. …
> `unnamedParam4` — A handle to the command list that the driver should use, when it calls back
> into the Direct3D runtime.
> ## Return value
> Returns an HRESULT.

The page's own front matter says `word_count: 116`, `ms.date: 2018-10-19`, and **there is no
Remarks section**. "Returns an HRESULT" is the entire error contract. The parameters are literally
named `unnamedParam1..4`, i.e. the page was machine-generated from the header with no human pass.

**[INFER]** The semantics of the D3D12 UMD DDI — lifetimes, threading, which handle is valid when,
what an implementation is allowed to defer — are **not** publicly documented. They live in the
IHV-partner spec ("the WDDM hardware functional spec", referenced by HLK docs, see Q7) which is
not public. R2's lane is exactly the reconstruction of those semantics; R10's contribution is:
**do not expect a public document to answer a semantics question. There is none.**

### Q1.3 The exception: features added after ~2018 DO have real conceptual docs

**[MSDOC/local]** Where a D3D12 feature landed after Microsoft started writing display-docs for it,
there is genuine prose in the conceptual repo we DO have:
- `display/enhanced-barriers.md` (222 lines) — names `D3D12DDI_BARRIER_0088`, the full
  `D3D12DDI_BARRIER_ACCESS_*` and layout enums.
- `display/d3d12-render-passes.md` (115 lines) — `D3D12DDI_RENDER_PASS_BEGINNING_ACCESS_TYPE_0053`
  and the `_0101` PRESERVE_LOCAL additions.
- `display/work-graphs.md` (59 lines) — `D3D12DDI_DEVICE_FUNCS_CORE_0109`,
  `D3D12DDI_COMMAND_LIST_FUNCS_3D_0108`, `D3D12DDI_STATE_SUBOBJECT_TYPE_*`.
- `display/direct3d-functions-implemented-by-user-mode.md` (1061 lines) and
  `display/direct3d-runtime-functions-called-by-user-mode.md` — the DDI/callback index pages,
  covering both D3D11 and D3D12 tables (`D3D12DDI_COMMAND_LIST_FUNCS_3D_0030/0032/0033`,
  `D3D12DDI_DEVICE_FUNCS_CORE_*`, `D3D12DDI_ALLOCATION_INFO_0022`, …).

**[INFER]** The documentation is inverted relative to need: the *newest, most optional* features
are documented; the *core, mandatory* device/command-list/PSO/descriptor surface is stubs.

### Q1.4 Is there ANY public sample or open-source implementation of a **D3D12** UMD?

**No.** Searched repeatedly; found none. What exists instead is Microsoft's open-source
implementations of the **older** UMD DDIs:

- **[WEB]** `microsoft/D3D11On12` — https://github.com/microsoft/D3D11On12 . Its README says
  verbatim it is "an implementation of the D3D11 usermode **DDI** (device driver interface)",
  shipped as `d3d11on12.dll`, entered through a customised `OpenAdapter_D3D11On12`, and that the
  repo "is largely a simple adaptor from the D3D11 DDI to the D3D12TranslationLayer library, where
  the real heavy lifting of converting to the D3D12 domain is done."
  (https://github.com/microsoft/D3D11On12/blob/master/README.md)
- **[WEB]** `microsoft/D3D9On12` — https://github.com/microsoft/D3D9On12 — the same shape for the
  D3D9 UMD DDI.

**[INFER]** Both are *consumers* of D3D12 and *implementers* of a pre-D3D12 UMD DDI. Neither gives
Helios a line of D3D12-UMD-DDI code. The asymmetry is itself the signal: Microsoft was willing to
open-source implementations of the D3D9 and D3D11 driver interfaces and has never open-sourced,
sampled, or WDK-shipped an implementation of the D3D12 one.

- **[WEB]** The only "third-party writing to d3d12umddi" artefact found is an old SDK header mirror
  (`tpn/winsdk-10`, `Include/10.0.14393.0/um/d3d12umddi.h`) — a copy of the header, not an
  implementation.

### Q1.5 One place where MS *does* explain D3D12 DDI mechanics: the runtime-bypass spec

**[WEB]** `microsoft/DirectX-Specs`, `d3d/D3D12RuntimeBypass.md` —
https://github.com/microsoft/DirectX-Specs/blob/master/d3d/D3D12RuntimeBypass.md (rendered:
https://microsoft.github.io/DirectX-Specs/d3d/D3D12RuntimeBypass.html). Quoted content:

- The optimisation "eliminate[s] the overhead of the D3D12 runtime entirely by providing a means
  for the application to take a shortcut directly to the User Mode Driver", projected to "save
  around 5% of CPU time for heavy D3D12 API usage workloads."
- The runtime's normal job is described: "retrieve the handle for the driver representation", "look
  up the matching Device Driver Interface function pointer", "call the DDI function."
- A `D3D12DDI_RUNTIME_BYPASS_HEADER` with "a pointer to the V-Table hosted in the D3D12 runtime"
  and "a pointer directly to the driver object."
- New DDI prototypes named `PFND3D12DDI_[APINAME]_0114`, e.g. `PFND3D12DDI_DRAWINSTANCED_0114`,
  `PFND3D12DDI_DISPATCH_0114`.
- On validation: "drivers do *not* have to increase their defensiveness to invalid API input"
  because "when it is determined that validation should be enabled the D3D runtime can disable the
  runtime bypass optimization."
- Bypass is optional: "not all drivers will be enlightened about the new D3D12 Object layout."

**[INFER] Two things Helios should take from this.** (1) The `_0114` suffix confirms the DDI is a
*version-stamped table negotiation*, the same shape as the D3D11 `D3D11_1DDI_DEVICEFUNCS` tables
`umd/` already installs — so the R802 bindgen discipline transfers directly. (2) It confirms the
D3D12 runtime is thin: it validates and dispatches, and the driver owns the object model. A D3D12
UMD is therefore *more* work than a D3D11 UMD for the same feature set, not less.

**[INFER]** There is also a third-party datapoint on how legible the DDI is to an outsider:
**[WEB]** https://frguthmann.github.io/posts/shimming_d3d12/ — the author, shimming `d3d12.dll`,
says of Microsoft's driver docs "It tells you which functions the driver should implement and
which runtime callbacks the driver can call", while also noting the docs make no mention of
sharing contracts. Weak evidence (the author says he has "never tried to actually implement a
driver"), included only because it is the only outside-observer commentary found.

---

## Q2 — D3D12 over a translation layer, and the WSL analogue

### Q2.1 The catalogue

| Project | Direction | Public? | Relevance to Helios |
|---|---|---|---|
| vkd3d-proton | D3D12 API → Vulkan | yes, in-tree submodule | the reference; Helios strategy (b) |
| vkd3d (WineHQ) | D3D12 API → Vulkan | yes, https://gitlab.winehq.org/wine/vkd3d | upstream of the above; vkd3d-proton's README says "Backwards compatibility with the vkd3d standalone API is not a goal of this project" (`vkd3d-proton-helios/README.md:15`) |
| Mesa `d3d12` gallium driver | OpenGL/OpenCL → **D3D12 API** | yes, https://docs.mesa3d.org/drivers/d3d12.html | *reverse* direction; the WSL consumer of libd3d12.so |
| Mesa `dzn` (Dozen) | Vulkan → **D3D12 API** | yes | *reverse* direction |
| microsoft/D3D11On12, D3D9On12 | D3D11/D3D9 **UMD DDI** → D3D12 API | yes | the only open UMD-DDI implementations, both pre-D3D12 |
| Apple D3DMetal (Game Porting Toolkit) | D3D11+D3D12 API → Metal | closed | see Q5 |
| WSL `libd3d12.so` + vendor Linux D3D12 UMDs | D3D12 API → real HW, over paravirtualized dxgkrnl | closed binaries; kernel side open | **the closest architectural analogue — see Q2.2** |

### Q2.2 WSLg / DirectX-on-Linux — the single most valuable finding of this lane

**Primary source (slides, XDC 2020, Jesse Natalie [Direct3D dev] + Steve Pronovost [Lead Windows
Graphics Kernel]):**
https://lpc.events/event/9/contributions/610/attachments/700/1295/XDC_-_WSL_Graphics_Architecture.pdf
Secondary: https://devblogs.microsoft.com/directx/directx-heart-linux/

**Verbatim slide content (read from the PDF, page numbers = slide order):**

Slide "WDDM GPU Para-Virtualization (GPU-PV)":
> Para-Virtualization
> • Level of abstraction is the WDDM interface
> • Project the compute/rendering portion of the WDDM interface in a VM so driver can interact with
>   it as if the GPU was local
> Was designed precisely for these usage scenarios: Windows Defender Application Guard for Edge,
> Windows Sandbox, Device Emulator (e.g. Hololens emulator). Extending to support Linux Guest,
> including WSL.

Slide "Dxgkrnl Linux Edition":
> • Open source — https://github.com/microsoft/WSL2-Linux-Kernel/tree/linux-msft-wsl-4.19.y/drivers/gpu/dxgkrnl
> • **Not a straight pass-through**
>   • Some WDDM API implemented locally
>   • Some a combination of local and messages to the host
>   • **Fundamentally memory manager, scheduler and GPU are on the host**
> • No data copy — Only control information exchanged over VM bus; Data in command buffers or GPU
>   surfaces shared between guest and host

Slide "How to get compute acceleration in WSL":
> Two possible approaches
> • Ask driver vendors to port ICDs for APIs apps are using
> • **Ask driver vendors to port UMD, we port D3D, we build layers to support APIs in terms of D3D**
> ICD approach means continued asks on driver vendors for new APIs (E.g. 3+ APIs across 4+ vendors)
> Mapping layer approach improves both Windows + WSL — **1 UMD per vendor, 1 mapping layer per API**

Slide "What exists today":
> D3D12 — **Requires D3D12 UMD to be ported as well** — UMDs available or in development from all
> Windows GPU vendors

Slide "What exists today - notes":
> • **Compute-only functionality — Rasterization pipeline is available, but no swapchains / window
>   integration**
> • Intention of D3D in WSL is implementation detail for GPU access — Not trying to introduce a new
>   API for apps – no SDK planned
> • **D3D stack is same code that runs on Windows** — All components involved modified to
>   dual-compile; Fixed lots of non-conformant code depending on MSVC quirks; Replaced
>   Windows-specific constructs with cross-platform code; **Wrote header shim with #defines/typedefs
>   for things that come from Windows SDK**; Clang caught several real bugs with its better warnings

Slide "WDDM 3.0":
> Seamless support in WDDM3.0+ — **User mode driver compiled for Linux included in driver package**;
> Host driver store mounted in Linux; Works out of the box.
> Integrated into the Windows Driver Certification process — IHV Partner adding WSL 2 configured
> system to their test pool; **HLK contains WSL 2 specific test validating driver**

The architecture diagram slide shows, in the Linux box: `libcuda → libdxcore`, and
`Mesa (GLon12) → libd3d12 ↔ D3D12 User Mode Driver`, both over `libdxcore (D3DKMT*)` → `/dev/dxg`
→ `drivers/gpu/dxgkrnl` → VM Bus ("WDDM Paravirtualization Protocol") → Windows-side `dxgkrnl` →
`GPU Kernel mode driver (KMD)`.

**[MSDOC]** The blog adds: `libd3d12.so` and `libdxcore.so` are "closed source, pre-compiled user
mode binaries" mounted at `/usr/lib/wsl/lib`; vendor D3D12 UMDs mount at `/usr/lib/wsl/drivers`.
(https://devblogs.microsoft.com/directx/directx-heart-linux/)

### Q2.3 The `/dev/dxg` ioctl surface — enumerated

**[WEB]** `include/uapi/misc/d3dkmthk.h` in
https://github.com/microsoft/WSL2-Linux-Kernel (branch `linux-msft-wsl-6.6.y`). File header:
"Dxgkrnl Graphics Driver — User mode WDDM interface definitions", © 2019 Microsoft, author Iouri
Tarassov, SPDX GPL-2.0 WITH Linux-syscall-note. The `LX_DX*` ioctls, with their numbers, as read:

```
0x01 LX_DXOPENADAPTERFROMLUID          0x25 LX_DXLOCK2
0x02 LX_DXCREATEDEVICE                 0x26 LX_DXMARKDEVICEASERROR
0x04 LX_DXCREATECONTEXTVIRTUAL         0x27 LX_DXOFFERALLOCATIONS
0x05 LX_DXDESTROYCONTEXT               0x2a LX_DXQUERYALLOCATIONRESIDENCY
0x06 LX_DXCREATEALLOCATION             0x2c LX_DXRECLAIMALLOCATIONS2
0x07 LX_DXCREATEPAGINGQUEUE            0x2e LX_DXSETALLOCATIONPRIORITY
0x08 LX_DXRESERVEGPUVIRTUALADDRESS     0x2f LX_DXSETCONTEXTINPROCESSSCHEDULINGPRIORITY
0x09 LX_DXQUERYADAPTERINFO             0x30 LX_DXSETCONTEXTSCHEDULINGPRIORITY
0x0a LX_DXQUERYVIDEOMEMORYINFO         0x31 LX_DXSIGNALSYNCHRONIZATIONOBJECTFROMCPU
0x0b LX_DXMAKERESIDENT                 0x32 LX_DXSIGNALSYNCHRONIZATIONOBJECTFROMGPU
0x0c LX_DXMAPGPUVIRTUALADDRESS         0x33 LX_DXSIGNALSYNCHRONIZATIONOBJECTFROMGPU2
0x0d LX_DXESCAPE                       0x34 LX_DXSUBMITCOMMANDTOHWQUEUE
0x0e LX_DXGETDEVICESTATE               0x35 LX_DXSUBMITSIGNALSYNCOBJECTSTOHWQUEUE
0x0f LX_DXSUBMITCOMMAND                0x36 LX_DXSUBMITWAITFORSYNCOBJECTSTOHWQUEUE
0x10 LX_DXCREATESYNCHRONIZATIONOBJECT  0x37 LX_DXUNLOCK2
0x11 LX_DXSIGNALSYNCHRONIZATIONOBJECT  0x38 LX_DXUPDATEALLOCPROPERTY
0x12 LX_DXWAITFORSYNCHRONIZATIONOBJECT 0x39 LX_DXUPDATEGPUVIRTUALADDRESS
0x13 LX_DXDESTROYALLOCATION2           0x3a LX_DXWAITFORSYNCHRONIZATIONOBJECTFROMCPU
0x14 LX_DXENUMADAPTERS2                0x3b LX_DXWAITFORSYNCHRONIZATIONOBJECTFROMGPU
0x15 LX_DXCLOSEADAPTER                 0x3c LX_DXGETALLOCATIONPRIORITY
0x16 LX_DXCHANGEVIDEOMEMORYRESERVATION 0x3d LX_DXQUERYCLOCKCALIBRATION
0x18 LX_DXCREATEHWQUEUE                0x3e LX_DXENUMADAPTERS3
0x19 LX_DXDESTROYDEVICE                0x3f LX_DXSHAREOBJECTS
0x1b LX_DXDESTROYHWQUEUE               0x40 LX_DXOPENSYNCOBJECTFROMNTHANDLE2
0x1c LX_DXDESTROYPAGINGQUEUE           0x41 LX_DXQUERYRESOURCEINFOFROMNTHANDLE
0x1d LX_DXDESTROYSYNCHRONIZATIONOBJECT 0x42 LX_DXOPENRESOURCEFROMNTHANDLE
0x1e LX_DXEVICT                        0x43 LX_DXQUERYSTATISTICS
0x1f LX_DXFLUSHHEAPTRANSITIONS         0x44 LX_DXSHAREOBJECTWITHHOST
0x20 LX_DXFREEGPUVIRTUALADDRESS        0x45 LX_DXCREATESYNCFILE
0x21 LX_DXGETCONTEXTINPROCESSSCHEDULINGPRIORITY  0x46 LX_DXWAITSYNCFILE
0x22 LX_DXGETCONTEXTSCHEDULINGPRIORITY 0x47 LX_DXOPENSYNCOBJECTFROMSYNCFILE
0x24 LX_DXINVALIDATECACHE              0x48 LX_DXENUMPROCESSES
                                       0x49 LX_ISFEATUREENABLED
```
~68 ioctls. **This is the practical minimum D3DKMT surface a D3D12 UMD (plus DXCore) exercises** —
Microsoft ported exactly this much and no more, and it was enough for `libd3d12.so` + vendor UMDs
+ DirectML + GLon12. **R6 should treat this list as the checklist**, with the caveat that it is
"what WSL's compute-only D3D12 needs", i.e. it deliberately excludes presentation.

Note what is IN that list and, per DX12.md §3, absent or refused in `kmd_render`:
`LX_DXCREATEHWQUEUE` / `LX_DXSUBMITCOMMANDTOHWQUEUE` (Helios `DxgkDdiCreateHwQueue` returns
`STATUS_NOT_SUPPORTED`, `kmd_render/src/ddi/scheduler.rs:180-187`), `LX_DXOFFERALLOCATIONS` /
`LX_DXRECLAIMALLOCATIONS2` (DX12.md §3.2 records there is no such miniport DDI in this WDK — those
are D3DKMT-level and land in VidMm, not the miniport), and the GPU-VA family
(`RESERVEGPUVIRTUALADDRESS` / `MAPGPUVIRTUALADDRESS` / `UPDATEGPUVIRTUALADDRESS` /
`FREEGPUVIRTUALADDRESS`) which Helios backs only decoratively (`kmd_render/src/ddi/gpummu.rs:1-14`).
**[INFER]** WSL's dxgkrnl gets away with those because "memory manager, scheduler and GPU are on
the host" — Helios does **not** get that out; in Helios, dxgkrnl/VidMm/VidSch *are* in the guest
and they are real.

### Q2.4 GPU-PV in Microsoft's own docs — and why it is a *different* model from Helios

**[MSDOC/local]** `windows-driver-docs-research-only/windows-driver-docs-pr/display/gpu-paravirtualization.md`
(1155 lines, `ms.date: 02/06/2025`), lines 47-59, verbatim:

> * The UMD in the guest VM needs to: Be aware that the communications with the host kernel-mode
>   driver (KMD) happen across the VM boundary. Use the added *Dxgkrnl* services to access registry
>   settings.
> * **There's no KMD in the guest, only UMD. The Virtual Render Device (VRD) KMD replaces the KMD.**
>   VRD's purpose is to facilitate the loading of *Dxgkrnl*.
> * **There's no video memory manager (*VidMm*) or scheduler (*VidSch*) in the guest.**
> * *Dxgkrnl* in a VM gets thunk calls and marshalls them to the host partition via VM bus channels.

Same file, lines 391-411 — the **only** D3D12-UMD-specific contract in the whole conceptual corpus,
and it names a real d3d12umddi entry point:

> * D3D12 gets the **D3D12_CROSS_NODE_SHARING_TIER** cap from UMD.
> * D3D12 gets the physical adapter count from *Dxgkrnl* by calling
>   **D3DKMTQueryAdapterInfo(KMTQAITYPE_PHYSICALADAPTERCOUNT)**.
> * D3D12 calls **pfnQueryNodeMap(PhysicalAdapterCount, &map)** … The UMD needs to set the actual
>   physical adapter index in the map or **D3D12DDI_NODE_MAP_HIDE_NODE** to disable a node.
> * … **If the state of the tier and the effective physical adapter count don't match, D3D12 fails
>   device creation.** Mismatch happens when: The tier is
>   **D3D12DDI_CROSS_NODE_SHARING_TIER_NOT_SUPPORTED** and adapter count is greater than 1. The tier
>   isn't … and adapter count is 1.

(Reference page for that callback:
https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3d12umddi/nc-d3d12umddi-pfnd3d12ddi_query_node_map)

**[INFER]** This is a concrete "D3D12 device creation fails hard on a caps inconsistency" trap, and
it is the *only* one Microsoft documented in prose. It is exactly the DX12.md §5.5 risk shape
("advertising a capability that is not backed is a lie the OS acts on"). If strategy (a) is ever
taken, `pfnQueryNodeMap` + `CROSS_NODE_SHARING_TIER_NOT_SUPPORTED` + adapter count 1 is a
day-one requirement.

**[INFER] The load-bearing structural point for Helios:** Microsoft's *supported* answer to "full
D3D12 inside a VM" (GPU-PV, used by Windows Sandbox, WDAG, the HoloLens emulator and WSL) is
**"put no KMD in the guest, ship the host vendor's real UMD into the guest, marshal D3DKMT over a
bus."** Helios does the opposite: a real WDDM miniport with real guest-side VidMm/VidSch, and no
vendor UMD available to ship. **Nobody has publicly built Helios' shape with D3D12 on top.** That
is not a reason not to; it is a reason not to expect a precedent to de-risk it.

**[WEB] Anti-citation — do not trust this one.** A Microsoft Q&A answer
(https://learn.microsoft.com/en-us/answers/questions/5605686/is-it-possible-to-add-support-for-directx-12-in-hy,
dated 2025-11-01) states "At present, Hyper‑V GPU Partitioning (GPU‑P) does not expose DirectX 12
or Vulkan to guest VMs in a supported way. Only DirectX 11 and OpenGL are officially supported."
The author is **"VPHAN - Independent Advisor" — a community member, not Microsoft**, and the claim
contradicts Microsoft's own GPU-PV documentation above (the guest runs the vendor's real UMD, which
is a D3D12 UMD). **Flagging it because it is the top search hit for "GPU-P DirectX 12" and will
mislead the next person who searches.**

---

## Q3 — vkd3d-proton on native Windows

All quotes below are **[SRC]** from the pinned in-tree checkout
`vkd3d-proton-helios/` (upstream `master` @ `2c7ba22c`), which is authoritative for what Helios
would actually ship, and match upstream https://github.com/HansKristian-Work/vkd3d-proton .

### Q3.1 It is a supported configuration, but explicitly a developer one

`vkd3d-proton-helios/README.md:168-181`, verbatim:

> ## Using vkd3d-proton
>
> The intended way to use vkd3d-proton is as native Win32 DLLs (d3d12.dll and d3d12core.dll).
> These serve as a drop-in replacement for D3D12, and can be used in Wine (Proton or vanilla
> flavors), or on Windows.
>
> **vkd3d-proton does not supply the necessary DXGI components on its own.
> Instead, DXVK (2.1+) and vkd3d-proton share a DXGI implementation.**
>
> ### A note on using vkd3d-proton on Windows
>
> Native Windows use is mostly relevant for developer testing purposes.
> Do not expect games running on Windows 7 or 8.1 to magically make use of vkd3d-proton,
> as many games will only even attempt to load d3d12.dll if they are running on Windows 10.

`README.md:136-140`:
> #### Building on Windows
> NOTE: Building directly on Windows (instead of cross compiling) is only expected to be used for
> testing and development. The primary use case is to develop tests and run them against native
> drivers, not to run real applications. This requires decent debugger support, so MSVC is
> supported as a compiler, although we do not stress test these builds at all.

### Q3.2 The DLL-injection mechanics — verified on the Helios VM

**[VM]** Run on win11 via `win_exec`:
```
KnownDLLs count: 37
--- any d3d in list: 0
--- d3d12 files in System32:
D3D12.dll      146168   10.0.26100.8737
D3D12Core.dll  3505480  10.0.26100.8737
d3d12SDKLayers.dll 4820992 10.0.26100.8737
--- SysWOW64:
D3D12.dll      101856
D3D12Core.dll  2914760
d3d12SDKLayers.dll 3935232
```
**Neither `d3d12.dll` nor `d3d12core.dll` nor `dxgi.dll` is in `KnownDLLs`** on the target machine.
**[INFER]** Therefore the standard Windows DLL search order applies and an app-local
`d3d12.dll` + `d3d12core.dll` (+ DXVK `dxgi.dll`) **will** be loaded ahead of System32 — no
registry surgery required for the base case. This is the packaging model Helios should plan on.

**[WEB]** DXVK's own Windows guidance (https://github.com/doitsujin/dxvk/wiki/Windows) confirms the
shape and gives the escape hatch and the prohibition:
- "DO NOT replace Windows DLLs in `System32` or `SysWOW64` with DXVK's. This will break your
  Windows install."
- Place the DLLs "next to the game's executable"; some applications ignore this and load from
  system directories anyway.
- "it is possible to work around games loading the wrong DLL by enabling `DevOverride` in the
  registry."
- Use the 32-bit DLLs for 32-bit games; Steam/Epic/GFE overlays interfere.
- And, directly on topic: for games supporting both D3D11 and D3D12, "this will only work if
  vkd3d-proton is used as the D3D12 implementation, in addition to DXVK".

**UNVERIFIED:** the exact registry path/value name for `DevOverride`. Settling read: the DXVK wiki
page source, or Microsoft's Image File Execution Options docs; then verify on the VM with
`reg query`.

### Q3.3 The DXGI dependency is structural, not cosmetic

**[SRC]** vkd3d-proton exposes its swapchain through a private COM interface that **DXVK's
dxgi.dll** consumes:
- `vkd3d-proton-helios/include/vkd3d_swapchain_factory.idl:45` — `interface IDXGIVkSurfaceFactory`
- `…:141` — `IDXGIVkSwapChainFactory::CreateSwapChain(IDXGIVkSurfaceFactory* pSurfaceFactory, …)`
- `libs/vkd3d/command.c:25475` — `dxgi_vk_swap_chain_factory_init(queue, &queue->vk_swap_chain_factory)`
  i.e. the factory hangs off the **command queue** object.
- `libs/vkd3d/swapchain.c` — the whole `dxgi_vk_swap_chain` implementation.

**[INFER, high confidence]** Windows' own `dxgi.dll` cannot create a swapchain over a vkd3d
`ID3D12CommandQueue`: `IDXGIFactory2::CreateSwapChainForHwnd` takes "a pointer to a direct command
queue" (https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgifactory2-createswapchainforhwnd)
and the system DXGI resolves that to the *runtime's* internal object, which a vkd3d queue is not.
So **shipping vkd3d-proton on Helios requires shipping DXVK's `dxgi.dll` too** — three DLLs, not
two. **Settling experiment (cheap, and it is the D1 gate anyway):** drop only
`d3d12.dll`+`d3d12core.dll` next to a D3D12 sample from `dx-samples-research-only/Samples/Desktop/`
and observe the `CreateSwapChainForHwnd` HRESULT; then repeat with DXVK's `dxgi.dll` added.

### Q3.4 A concrete deployment landmine found in the source

**[SRC]** `vkd3d-proton-helios/libs/d3d12/main.c:111-135`:
```c
static void load_d3d12core_once(void)
{
    ret = load_d3d12core_module(SONAME_D3D12CORE);
#ifdef _WIN32
    if (!ret)
    {
        /* Fallback to loading directly from the system32 dir, to handle
         * the case where a game ships a D3D12Core.dll next to
         * their executable. */
        char buf[VKD3D_PATH_MAX];
        GetSystemDirectoryA(buf, sizeof(buf));
        vkd3d_strlcat(buf, sizeof(buf), "\\" SONAME_D3D12CORE);
        ret = load_d3d12core_module(buf);
    }
#endif
```
and `libs/d3d12/main.c:74-86`: it `dlopen`s and `dlsym`s `D3D12GetInterface`, then requires
`D3D12GetInterface(&CLSID_VKD3DCore, &IID_IVKD3DCoreInterface, …)` to succeed.

**[INFER]** On native Windows, if the target application ships its **own** Agility-SDK
`D3D12Core.dll` app-local (which many modern D3D12 titles do), vkd3d's `d3d12.dll` will `dlopen`
*that* one, fail the `CLSID_VKD3DCore` query, and then fall back to **System32's Microsoft
D3D12Core.dll**, which also fails — and the whole thing `ERR`s out with "Failed to find
vkd3d-proton d3d12core interfaces." The System32 fallback exists for the *Wine* case, where a
prefix's system32 holds vkd3d's own DLLs. **On Windows this fallback is a trap, not a safety net.**
Helios' D3D12 install/verify path must detect an app-local Microsoft `D3D12Core.dll` and handle it.

**[SRC]** Also note `libs/d3d12core/d3d12core.def`:
```
LIBRARY d3d12core.dll
EXPORTS
    D3D12GetInterface
    D3D12SDKVersion DATA PRIVATE
```
and `libs/d3d12core/main.c:1355`: `DLLEXPORT const UINT D3D12SDKVersion = D3D12_SDK_VERSION;` —
vkd3d-proton deliberately mimics the Agility redist's export shape (see Q6).

### Q3.5 Public reports of native-Windows breakage

**[WEB]** The known reported issues in this area are packaging-level, not engine-level:
- https://github.com/Frogging-Family/wine-tkg-git/issues/978 — "Latest vkd3d-proton splits into
  d3d12.dll and d3d12core.dll yet prefix generation does not copy the file", i.e. the two-DLL split
  routinely breaks installers.
- https://github.com/HansKristian-Work/vkd3d-proton/issues/2231 — "Unable to completely replace
  vkd3d with vkd3d-proton", the Wine-builtin-vkd3d vs vkd3d-proton coexistence problem (Wine-side
  only; not applicable on real Windows).
- https://forum.winehq.org/viewtopic.php?t=34370 — a game shipping its own `d3d12.dll` in its
  install directory crashing under VKD3D.
- https://github.com/HansKristian-Work/vkd3d-proton/issues/2790 — "DX12 swapchain creation fails on
  multi-GPU NVIDIA systems (duplicate LUID)". **[INFER]** Directly relevant: Helios' LUID identity
  is a known-fragile area (ROADMAP 30th-session LUID work); a duplicate/mismatched LUID between the
  DXGI adapter and the `VkPhysicalDevice` is a *documented* vkd3d-proton failure mode.

**UNVERIFIED:** I did not find a systematic "vkd3d-proton on native Windows: what works" report.
Settling experiment: it is cheaper to run it than to search for it — build the pinned submodule
with MSVC per `README.md:136-165` and run `tests/d3d12` on the VM against (i) the host GPU's real
Vulkan driver if one is reachable, and (ii) the Helios ICD, and diff the two pass lists. That diff
is worth more than any web source, because it separates "vkd3d on Windows" from "vkd3d on venus".

---

## Q4 — vkd3d-proton over virtualized / venus Vulkan

### Q4.1 Venus has been driven to VKD3D-Proton Feature Level 12_2 — upstream, deliberately

**[WEB]** Phoronix, "Venus Vulkan Driver Lands Mesh Shader Support In Mesa 26.0", Michael Larabel,
**5 December 2025** — https://www.phoronix.com/news/Venus-Vulkan-Mesh-Shader :

> Venus … can "advertise VK_EXT_mesh_shader support, permitting sufficient host Vulkan driver
> support." … the mesh shader implementation represents **"the last piece needed for getting Venus
> to VKD3D-Proton Feature Level 12_2 for the Direct3D 12 feature level atop Vulkan."**

Merge request cited by the article: https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/38739 .

**[SRC] And the Helios ICD fork already has it.** `icd/mesa` is Mesa `26.2.0-devel`
(`icd/mesa/VERSION`), HEAD `3af97415bc56f34010811dcfb1110e67e986b123` (2026-08-05), and venus
advertises the extension:
```
icd/mesa/src/virtio/vulkan/vn_physical_device.c:1554:      .EXT_mesh_shader = true,
icd/mesa/src/virtio/vulkan/vn_physical_device.c:400: VN_ADD_PNEXT_EXT(feats2, MESH_SHADER_FEATURES_EXT, …)
```
**[INFER]** So the venus *protocol layer* Helios ships is downstream of the FL 12_2 push. This is
the strongest single piece of external evidence for Helios strategy (b): the specific Vulkan
implementation Helios uses has been targeted at vkd3d-proton by its own upstream, recently, with a
named feature-level goal. It does **not** prove anything about the *host* (virglrenderer + the
NVIDIA host driver) or about Helios' Windows-guest transport — R12 owns that gap.

**UNVERIFIED:** whether a Windows-guest venus ICD (Helios' build) reaches the same feature set as
the Linux-guest venus the Mesa work targeted, and whether the Helios host (virglrenderer +
NVIDIA) exposes the required host-side extensions. Settling experiment: run `vulkaninfo` in the
guest (`tmp/dx12/guest-vulkaninfo-full.txt` is already staged) and diff against
`vkd3d-proton-helios/README.md:19-35`'s hard requirements — **R12's lane; do not duplicate.**

### Q4.2 The Vulkan bar vkd3d-proton sets

**[SRC]** `vkd3d-proton-helios/README.md:17-35`, verbatim:

> ## Drivers
> There are some hard requirements on drivers to be able to implement D3D12 in a reasonably
> performant way.
> - Vulkan 1.3
> - Descriptor indexing with at least 1000000 UpdateAfterBind descriptors for all types except
>   UniformBuffer. Essentially all features in `VkPhysicalDeviceDescriptorIndexingFeatures` must be
>   supported.
> - Further, the following device features are required:
>   - `samplerMirrorClampToEdge`
>   - `shaderDrawParameters`
> - `VK_EXT_robustness2`
> - `VK_KHR_push_descriptor`
>
> Some notable extensions that **should** be supported for optimal or correct behavior. These
> extensions will likely become mandatory later.
> - `VK_EXT_image_view_min_lod`
>
> `VK_EXT_mutable_descriptor_type` (or the vendor `VALVE` alias) and `VK_EXT_descriptor_buffer` are
> also highly recommended, but not mandatory.

Driver minimums: RADV ≥ Mesa 22.0; NVIDIA ≥ 535 series; "We have not done any testing against Intel
GPUs yet." (`README.md:37-53`).

### Q4.3 Other "vkd3d-proton on a non-native Vulkan driver" precedents

- **Winlator / Bannerlator / Cassia (Android).** Winlator runs Windows apps on Android with Wine +
  Box64 and "uses Mesa (Turnip/Zink/VirGL), DXVK, and VKD3D"; for DirectX 12 titles "VKD3D-Proton
  must be selected as the wrapper" (https://github.com/brunodev85/winlator , https://winlator.org/).
  Cassia pairs "Wine/DXVK/VKD3D-Proton/FEX" for Android
  (https://www.phoronix.com/forums/forum/software/linux-gaming/1440897-cassia-aims-to-pair-wine-dxvk-vkd3d-proton-fex-for-windows-games-on-android).
  **[INFER]** Establishes that vkd3d-proton runs on non-IHV Mesa Vulkan drivers in adverse
  environments. It does **not** establish anything about venus specifically — Winlator's Vulkan is
  usually Turnip (native Adreno), with VirGL used for GL.
- **Lavapipe (software Vulkan).** Mesa deliberately wired up lavapipe features for vkd3d-proton:
  MR https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/23996 ("lavapipe: Bringup
  vkd3d-proton"), and later "VK_EXT_fragment_shader_interlock, KHR_shader_quad_control,
  shaderResourceMinLod, VK_EXT_shader_image_atomic_int64, 64-bit image clears and 64-bit image
  operations" merged for Mesa 25.2 (https://www.phoronix.com/news/Lavapipe-VKD3D-Proton-Features,
  9 May 2025). **[INFER]** The precedent that matters: bringing a *non-IHV* Vulkan driver up to
  vkd3d-proton is a known, tractable, incremental workstream with an established shape — implement
  the feature, rerun the suite, count passes. That is exactly the D0 gate DX12.md proposes.
  (Note: gitlab.freedesktop.org is behind Anubis anti-bot and could not be fetched directly;
  the MR title/number is from search results, the feature list from Phoronix.)
- **Mesa's own vkd3d-proton CI harness exists** and is the obvious thing to copy:
  `.gitlab-ci/container/build-vkd3d-proton.sh` in mesa
  (https://gitlab.freedesktop.org/mesa/mesa/-/blob/c43d94a8536c44dcc39a11e85fc247c1d9a7fcc6/.gitlab-ci/container/build-vkd3d-proton.sh),
  plus per-driver failure trackers, e.g. https://gitlab.freedesktop.org/mesa/mesa/-/issues/5004
  ("[anv] multiple failures running the vkd3d-proton testsuite"). **R9's lane** — flagged here
  only so R9 knows the prior art exists.

### Q4.4 What I could NOT find

**UNVERIFIED — and I searched for it several ways:** *any* public report of vkd3d-proton running
inside a **Windows guest** over virtio-gpu/venus, or over any paravirtualized Windows GPU driver.
Every venus+vkd3d datapoint is a **Linux** guest running Proton. Settling experiment: there is no
document to find — Helios would be the first, and the D0/D1 gates in DX12.md §4 are the experiment.
Treat "someone has surely done this" as **false** until shown otherwise.

---

## Q5 — Other virtual-GPU vendors' D3D12 story

| Vendor / project | Highest D3D in guest | Ships a D3D12 UMD? | Source |
|---|---|---|---|
| **Hyper-V GPU-PV / WSL / Windows Sandbox** | full D3D12 | **the host IHV's UMD, shipped into the guest** — MS ships none of its own | slides + `gpu-paravirtualization.md` (Q2.2/Q2.4) |
| **VMware SVGA3D (vm3d)** | D3D11 FL11_0 | no | https://docs.mesa3d.org/drivers/svga3d.html ; no VMware announcement of D3D12 FL support found |
| **VirtualBox 7.x** | D3D11 (via DXVK on non-Windows hosts) | no | https://www.gamingonlinux.com/2022/10/virtualbox-70-is-out-with-their-directx-11-support-using-dxvk/ ; https://docs.oracle.com/en/virtualization/virtualbox/7.2/user/guestadditions.html |
| **Parallels Desktop** | D3D11 (since v15, over Metal) | no | https://kb.parallels.com/124137 ; recurring unanswered forum asks for D3D12, e.g. https://forum.parallels.com/threads/any-update-on-directx-12-and-parallels-windows-11.369471/ |
| **Apple Game Porting Toolkit / D3DMetal** | D3D11 **and D3D12** | n/a — not a driver; a Wine-side API translator to Metal | https://www.codeweavers.com/blog/mjohnson/2023/6/6/wine-comes-to-macos-apple-s-game-porting-toolkit-powered-by-crossover-source-code ; https://www.applegamingwiki.com/wiki/Game_Porting_Toolkit |
| **mvisor win-vgpu (tenclass)** — the driver CLAUDE.md cites as a reference | OpenGL 4.x only | no | https://github.com/tenclass/mvisor-win-vgpu-driver — README: OpenGL 4.x by translating to "Mesa Virgl Render Commands"; **no Direct3D or Vulkan**; UMD (`MvisorVGPUx64.dll`, `opengl32.dll`) + KMDF `vgpu.sys`, requires test-signing |
| **QEMU / virtio-gpu Windows guest (community)** | nothing shipped | no | open feature requests, e.g. https://github.com/virtio-win/kvm-guest-drivers-windows/issues/773 ("Add VirtIO-GPU full graphics driver (with DirectX support)", opened 2022-05-27, still open) and https://github.com/virtio-win/kvm-guest-drivers-windows/issues/841 |
| **NVIDIA vGPU / DDA passthrough** | full D3D12 | the real vendor UMD, unmodified | https://docs.nvidia.com/vgpu/ |

**[INFER] The pattern is unambiguous.** Every vGPU vendor that gets D3D12 in a Windows guest does it
by **passing the real vendor driver through** (GPU-PV, DDA, vGPU). Every vGPU vendor that *emulates*
a GPU with its own guest WDDM driver **stops at D3D11** — VMware, VirtualBox, Parallels, mvisor, and
every QEMU/virtio-gpu attempt. **Helios is already past that line for D3D11 with a real WDDM
render+display miniport, which is further than any of them; and none of them provides a template for
the next step.**

**[INFER]** Apple's GPTK is the closest *strategic* analogue to Helios strategy (b): Apple did not
write a D3D12 driver — they wrote an API-level translator (D3DMetal) and shipped it inside the
compatibility layer, in userspace, next to the app. That is exactly the vkd3d-proton model.

**UNVERIFIED:** the VMware FL11_0 cap. The Mesa svga3d page documents the *Linux guest* driver, and
my VMware citation is a search-result summary rather than a VMware document. Settling read: VMware
Workstation/Fusion release notes or KB for "DirectX 11 Feature Level 11_0"; or simply note it as
"no vendor announcement of D3D12 exists", which is what the search actually establishes.

---

## Q6 — Agility SDK / D3D12Core redistribution: can it be used instead of a UMD?

### Q6.1 The mechanism, exactly

**[WEB]** Spec: https://microsoft.github.io/DirectX-Specs/d3d/D3D12Redistributable.html
Getting-started: https://devblogs.microsoft.com/directx/gettingstarted-dx12agility/

The app exports two symbols from its **own exe**:
```cpp
extern "C" { __declspec(dllexport) extern const UINT D3D12SDKVersion = n; }
extern "C" { __declspec(dllexport) extern const char* D3D12SDKPath = u8".\\D3D12\\"; }
```
or via a `.def`:
```
EXPORTS
  D3D12SDKVersion DATA PRIVATE
  D3D12SDKPath    DATA PRIVATE
```
Rules, from the two sources:
- `D3D12SDKPath` is "the path to D3D12 binaries **relative to the application exe**". Absolute
  paths / env vars break deployment.
- The redist must live in an app-local **subdirectory**; "developers should avoid having Agility SDK
  components in the same directory as their application exe."
- "If the requested version is the same or older than the OS inbox D3D12, the application uses the
  inbox version" — the OS wins ties and newer.
- The runtime SDK-selection API `ID3D12SDKConfiguration::SetSDKVersion` "can only be used in
  **Windows Developer Mode**" and must be called before device creation.
- Minimum OS: "every retail build of Windows Version 1909 (19H2) and more recent" (the blog adds
  specific KB patches).

### Q6.2 What it does and does not replace

**[INFER, and this is the key answer]** The Agility SDK replaces **`D3D12Core.dll` — the D3D12
*runtime*** — and nothing below it. It does not replace, bypass, or alter the **UMD DDI**: the
redistributed runtime still calls `OpenAdapter12` in the driver's UMD and still needs a driver that
implements `d3d12umddi.h`. The DirectX-Specs redist page's own framing is that the redist preserves
"contract integrity with kernel thunks". **So the Agility mechanism cannot be used to make apps load
a Helios/vkd3d D3D12 implementation while `OpenAdapter12` still refuses.**

**[SRC]** vkd3d-proton nonetheless exports `D3D12SDKVersion` from its `d3d12core.dll`
(`libs/d3d12core/d3d12core.def`; `libs/d3d12core/main.c:1355`
`DLLEXPORT const UINT D3D12SDKVersion = D3D12_SDK_VERSION;`). **[INFER]** That is shape-compatibility
with the Microsoft split, not participation in the Agility loader: vkd3d's own `d3d12.dll` locates
its core by `dlopen("d3d12core.dll")` + a private `CLSID_VKD3DCore` query (`libs/d3d12/main.c:66-108`),
**not** by reading `D3D12SDKPath`.

### Q6.3 Is there a supported system-wide D3D12 replacement on Windows 11?

**No.** The two mechanisms are:
1. **Agility SDK** — per-application, opt-in by the *app author* (it exports the symbols), and it
   swaps only the runtime, not the driver. Not usable by a third party who does not control the exe.
2. **App-local DLL replacement** (the DXVK/vkd3d model) — per-application, opt-in by whoever
   installs the files, works because `d3d12.dll`/`d3d12core.dll`/`dxgi.dll` are **not** KnownDLLs
   (**[VM]**, Q3.2). DXVK explicitly forbids the System32 variant: "DO NOT replace Windows DLLs in
   `System32` or `SysWOW64` with DXVK's. This will break your Windows install."
   (https://github.com/doitsujin/dxvk/wiki/Windows)

**[INFER] Consequence for Helios:** strategy (b) is **inherently per-application**. DWM, Explorer,
the shell, WPF/WinUI, browsers, and anything that reaches D3D12 through the system runtime will
**never** see vkd3d — they will see Helios' `OpenAdapter12` refusing, and either fall back to D3D11
or to WARP. Only apps whose directory Helios (or the user) populates get D3D12. That is a real,
permanent product limitation of (b) and it should be written into the decision, not discovered
later. (Whether it *matters* depends on the goal: for "run a D3D12 game/benchmark" it is fine; for
"the desktop composites via D3D12" it is fatal.)

**UNVERIFIED:** whether `DevOverrideEnable` (per-exe IFEO) is a supported enough lever to widen (b)
beyond games, and whether it is per-exe only. Settling read: the DXVK wiki page + Microsoft's IFEO
documentation, then a `reg query`/`reg add` experiment on the VM against a known process.

---

## Q7 — "Don't do this": what the record says about the cost of a D3D12 driver surface

There is no single public post-mortem titled "we tried to write a D3D12 UMD". What exists is
converging circumstantial evidence, all of it citable:

1. **Microsoft, given the choice, did not write one either.** For WSL they explicitly weighed
   "ask driver vendors to port ICDs" vs "**ask driver vendors to port UMD**, we port D3D, we build
   layers" and chose the latter — "1 UMD per vendor, 1 mapping layer per API" (XDC slides, Q2.2).
   Microsoft's own contribution was the *runtime* and the *mapping layers*, never a UMD.
2. **Microsoft's open-source UMD implementations stop at D3D11.** `D3D11On12` and `D3D9On12` are
   "an implementation of the D3D11/D3D9 usermode DDI"; there is no `D3D12OnX`. The heavy lifting in
   both is delegated to `D3D12TranslationLayer`
   (https://github.com/microsoft/D3D11On12/blob/master/README.md).
3. **Every emulated-GPU vendor stops at D3D11** (Q5 table). None of them published a reason; the
   absence across five independent vendors is the datum.
4. **The public documentation of the D3D12 UMD DDI has no semantics** (Q1.2). An implementer is
   working from a header plus the HLK's behaviour, which is the most expensive possible mode.
5. **Certification is a real gate with a named D3D12 requirement.** The HLK
   (https://learn.microsoft.com/en-us/windows-hardware/test/hlk/) contains display-driver
   requirements including `Device.Graphics.AdapterRender.D3D12Core.CoreRequirement`, and HLK D3D12
   testing "enables DXGKrnl WDDM validation which ensures that drivers implemented all mandatory
   WDDM features". **[INFER]** Helios does not need WHQL to run, but the HLK is the only public
   enumeration of "mandatory" and it will surface every cap Helios advertises but does not back —
   the DX12.md §5.5 failure mode.
   **UNVERIFIED:** the exact list of D3D12 HLK requirements and whether the HLK D3D12 tests are
   runnable without a certified environment. Settling read: `learn.microsoft.com/windows-hardware/test/hlk/testref`
   for `Device.Graphics.AdapterRender.D3D12*`, and whether the HLK client is installable on win11.
6. **Even the *translation* side is brutally hard where D3D12 and Vulkan disagree.** vkd3d-proton's
   descriptor work is the documented worst case: descriptors in D3D12 are plain data with no
   independent lifetime, games "copy 10000+ descriptors per frame in many threads concurrently",
   and Death Stranding was found "spending over 80% of CPU time copying descriptors" before the
   `vkUpdateDescriptorSets`/`VkCopyDescriptorSet` rework
   (https://deepwiki.com/HansKristian-Work/vkd3d-proton/3.3-resources-and-heaps ;
   https://www.phoronix.com/forums/forum/software/linux-gaming/1636269-vkd3d-proton-merges-vulkan-descriptor-heap-support).
   **[INFER]** A from-scratch D3D12 UMD would have to solve this class of problem *again*, without
   vkd3d-proton's six years of game-specific workarounds.
7. **The D3D12 runtime is deliberately thin, so the driver carries more.** The runtime-bypass spec
   (Q1.5) exists precisely because "the overhead of the D3D12 runtime" is already only ~5% — i.e.
   virtually all of D3D12's semantics already live in the driver.

---

## What the precedent implies for Helios

Each line traceable to a source above.

1. **There is no public D3D12 UMD to copy — none, anywhere, open or closed.** Microsoft
   open-sourced D3D9On12 and D3D11On12 (both UMD-DDI implementations) and never a D3D12 one
   (Q1.4). Strategy (a) is a genuine first, and the only reference material is the header plus
   ~600 auto-generated stub pages with no Remarks (Q1.2). *Implication:* budget strategy (a) as
   original engineering, not as porting.

2. **Clone `MicrosoftDocs/windows-driver-docs-ddi` before anything else.** Our local mirror is the
   *conceptual* repo; the entire `d3d12umddi` reference lives in a repo we do not have (Q1.1).
   Cheap, one command, removes a whole class of "there is no documentation" false conclusions.

3. **Microsoft's own answer to "D3D12 in a VM" is the opposite of Helios' architecture, and it is
   available in source for the kernel half.** GPU-PV: no guest KMD, no guest VidMm/VidSch, the
   host IHV's real UMD shipped into the guest, ~68 `/dev/dxg` ioctls marshalled over VMBus
   (Q2.2-Q2.4). *Implication:* the WSL ioctl list is the best available checklist of "what a D3D12
   UMD actually asks the kernel for" (R6), **but** it is a compute-only profile — "Rasterization
   pipeline is available, but no swapchains / window integration" — so it says nothing about
   presentation, which is exactly Helios' known-hard area.

4. **The precedent for *presentation* is uniformly bad, and it is the same gap ROADMAP already
   names.** WSL's D3D12 has no swapchain at all; vkd3d-proton does not implement DXGI and routes
   presentation through **DXVK's** `dxgi.dll` via `IDXGIVkSwapChainFactory` hanging off the command
   queue (Q3.3). *Implication:* strategy (b) must ship **three** DLLs (`d3d12.dll`, `d3d12core.dll`,
   DXVK `dxgi.dll`), and the D1 gate must prove `CreateSwapChainForHwnd` works — this is the single
   riskiest step and it lands on the present path ROADMAP has repeatedly found fragile.

5. **The Vulkan substrate is the *least* worrying part, and that is new information.** Upstream
   Mesa deliberately drove **venus** to "VKD3D-Proton Feature Level 12_2", with `VK_EXT_mesh_shader`
   called out as "the last piece", landing in Mesa 26.0 (Dec 2025) — and Helios' `icd/mesa` fork is
   Mesa 26.2.0-devel with `EXT_mesh_shader = true` in venus (Q4.1). *Implication:* the DX12.md D0
   gate should be reframed from "does the substrate carry D3D12 at all?" to "does *our build, in a
   Windows guest, on our host* reach what upstream venus already reaches on Linux?" — a much
   narrower and more answerable question.

6. **Bringing a non-IHV Vulkan driver up to vkd3d-proton is a known, tractable workstream with an
   established method.** Lavapipe and venus both did it incrementally, feature by feature, measured
   by the vkd3d-proton test suite, with Mesa CI harness scripts to copy (Q4.3). *Implication:* D0's
   exit gate ("a recorded pass/fail count + a named list of missing features") is exactly the right
   shape, and R9 should copy Mesa's harness rather than invent one.

7. **Strategy (b) is permanently per-application. There is no supported system-wide D3D12
   replacement on Windows 11.** The Agility SDK swaps only `D3D12Core.dll`, is opted into by the
   *app's own exports*, and still requires a driver UMD; app-local DLL replacement works (verified:
   `d3d12.dll`/`d3d12core.dll` are not KnownDLLs on this VM) but reaches only apps whose directory
   you control; System32 replacement is explicitly forbidden (Q6). *Implication:* if the D3D12 goal
   is ever "DWM/shell/system apps use D3D12", (b) cannot deliver it and (a) is the only path. If the
   goal is "3DMark/games/samples run D3D12", (b) is sufficient. **The decision in DX12.md §D3 should
   state which goal it is choosing.**

8. **`OpenAdapter12` should keep refusing under strategy (b) — and the refusal is now defensible in
   writing.** Under (b) the D3D12 runtime is never in the path; a refusing `OpenAdapter12` is the
   honest answer for every client that *is* using the system runtime (Q6.3). This matches DX12.md
   §2(b) and R908's standing lesson.

9. **Two concrete traps to encode in the install/verify path now.** (i) If the target app ships its
   own Agility `D3D12Core.dll`, vkd3d's `d3d12.dll` will load it, fail `CLSID_VKD3DCore`, fall back
   to System32, and fail again with "Failed to find vkd3d-proton d3d12core interfaces"
   (`libs/d3d12/main.c:111-135`, Q3.4). (ii) LUID mismatch between the DXGI adapter and the
   `VkPhysicalDevice` is a *documented* vkd3d-proton swapchain failure
   (https://github.com/HansKristian-Work/vkd3d-proton/issues/2790) — and LUID identity is an area
   Helios has already had to fix once.

10. **Nobody has published "vkd3d-proton in a Windows guest over virtio-gpu/venus".** Every
    venus+vkd3d datapoint is a Linux guest running Proton (Q4.4). *Implication:* do not plan around
    a precedent that does not exist; the D0/D1 experiments in DX12.md §4 **are** the experiment, and
    their results are worth writing up because they will be the first of their kind.

11. **If strategy (a) is ever chosen, two Microsoft-documented hard gates are already known.**
    `pfnQueryNodeMap` must return one enabled node and `D3D12DDI_CROSS_NODE_SHARING_TIER_NOT_SUPPORTED`
    or **D3D12 fails device creation outright** (`gpu-paravirtualization.md:398-411`, Q2.4); and the
    DDI is a version-stamped table negotiation (`_0040`, `_0088`, `_0109`, `_0114` suffixes,
    Q1.5) whose ABI must come from the header through bindgen with layout assertions, exactly as
    R802 established for D3D11.

---

## Source list

- https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3d12umddi/
- https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3d12umddi/nc-d3d12umddi-pfnd3d12ddi_create_command_list_0040
- https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3d12umddi/nc-d3d12umddi-pfnd3d12ddi_query_node_map
- https://github.com/MicrosoftDocs/windows-driver-docs-ddi/tree/staging/wdk-ddi-src/content/d3d12umddi
- https://github.com/microsoft/DirectX-Specs/blob/master/d3d/D3D12RuntimeBypass.md
- https://microsoft.github.io/DirectX-Specs/d3d/D3D12Redistributable.html
- https://devblogs.microsoft.com/directx/gettingstarted-dx12agility/
- https://devblogs.microsoft.com/directx/directx-heart-linux/
- https://lpc.events/event/9/contributions/610/attachments/700/1295/XDC_-_WSL_Graphics_Architecture.pdf
- https://github.com/microsoft/WSL2-Linux-Kernel (`include/uapi/misc/d3dkmthk.h`, `drivers/gpu/dxgkrnl`)
- https://github.com/microsoft/D3D11On12 , https://github.com/microsoft/D3D9On12
- https://github.com/HansKristian-Work/vkd3d-proton (+ issues 2231, 2790)
- https://github.com/doitsujin/dxvk/wiki/Windows
- https://www.phoronix.com/news/Venus-Vulkan-Mesh-Shader
- https://www.phoronix.com/news/Lavapipe-VKD3D-Proton-Features
- https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/38739 , `/merge_requests/23996` , `/issues/5004`
- https://gitlab.freedesktop.org/mesa/mesa/-/blob/c43d94a8536c44dcc39a11e85fc247c1d9a7fcc6/.gitlab-ci/container/build-vkd3d-proton.sh
- https://docs.mesa3d.org/drivers/venus.html , https://docs.mesa3d.org/drivers/d3d12.html , https://docs.mesa3d.org/drivers/svga3d.html
- https://github.com/tenclass/mvisor-win-vgpu-driver
- https://github.com/virtio-win/kvm-guest-drivers-windows/issues/773 , `/issues/841`
- https://www.gamingonlinux.com/2022/10/virtualbox-70-is-out-with-their-directx-11-support-using-dxvk/
- https://kb.parallels.com/124137 , https://forum.parallels.com/threads/any-update-on-directx-12-and-parallels-windows-11.369471/
- https://www.codeweavers.com/blog/mjohnson/2023/6/6/wine-comes-to-macos-apple-s-game-porting-toolkit-powered-by-crossover-source-code
- https://www.applegamingwiki.com/wiki/Game_Porting_Toolkit
- https://github.com/brunodev85/winlator , https://winlator.org/
- https://deepwiki.com/HansKristian-Work/vkd3d-proton/3.3-resources-and-heaps
- https://learn.microsoft.com/en-us/windows-hardware/test/hlk/
- https://frguthmann.github.io/posts/shimming_d3d12/
- ⚠ **unreliable, cited only as a warning:** https://learn.microsoft.com/en-us/answers/questions/5605686/is-it-possible-to-add-support-for-directx-12-in-hy
