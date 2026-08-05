# R11 — Driver registration, DLL-split mechanics, and deployment for a D3D12 UMD

**Lane:** R11. **Date:** 2026-08-05. **Scope:** the *mechanical* questions that decide the shape of a
D3D12 UMD split — which registry slot D3D12 reads, how the runtime resolves and loads the DLL, the
exact INF diff, signing/packaging, shipping vkd3d-proton binaries, two-engines-in-one-process
hazards, and the rollback mechanism.

**Evidence classes used below:** `HEADER` (quoted from the staged SDK header), `MSDOC` (Microsoft
docs, in-tree markdown mirror or learn.microsoft.com), `CODE` (this repo, file:line), `LIVE` (a
command I ran against the win11 VM this session, with its output), `INFER` (my reasoning, labelled).
Anything I could not settle is marked **UNVERIFIED** with the experiment that settles it.

⚠ Nothing in this dossier was built, installed, or written to the guest. The only guest-side actions
were read-only registry/file reads, `dumpbin /exports` on an already-deployed DLL, and two
`D3DKMTQueryAdapterInfo` read-only probes via `Add-Type` P/Invoke (temp script removed afterwards).

---

## 0. TL;DR — the eight load-bearing answers

1. `UserModeDriverName` is a `REG_MULTI_SZ` **indexed by `KMTUMDVERSION`**: `[0]=DX9, [1]=DX10,
   [2]=DX11, [3]=DX12`. **The D3D12 slot can name a different DLL than the D3D11 slot.** (§1)
2. **Proven live, not inferred:** `D3DKMTQueryAdapterInfo(KMTQAITYPE_UMDRIVERNAME, Version=3)`
   against the Helios adapter returns our DriverStore `helios_umd.dll`, and the D3D12 runtime is
   *already* calling our `OpenAdapter12` export in production processes — **including `dwm.exe`**. (§1.3, §2.2)
3. `UserModeDriverNameWow` is **live on this VM and STALE and WRONG**: a 32-bit (WOW64) caller gets a
   path into a 27-July DriverStore folder naming the **64-bit** `helios_umd.dll`. Nothing in this
   repository writes that value. (§1.5) — a previously unrecorded defect.
4. The UMD is loaded **into every D3D client process** (2374 `umd-<pid>.log` files on the VM), and
   `helios_umd.dll` is **loaded and unloaded once per D3D11 device**. (§2.1)
5. **The DLL-path is latched at DEVICE start, not at process start.** Three different DriverStore
   folders were serving three different live processes in the same boot while the registry named a
   fourth. (§2.3)
6. `helios_umd.dll` **has no version resource at all** while the WDDM 2.1 doc requires INF/SYS/DLL
   file versions to match. A second UMD DLL inherits the same gap. (§4.4)
7. vkd3d-proton is **LGPL-2.1-or-later**. Shipping its `d3d12.dll`/`d3d12core.dll` as separate DLLs
   is the low-friction path; *statically linking* `libvkd3d` into `helios_umd12.dll` drags the whole
   UMD into LGPL relinking obligations. (§5.1)
8. Rollback must be reachable **without a working desktop**, because dwm probes D3D12 today. The
   concrete proposal: an `HKLM\SOFTWARE\Helios!UmdD3D12` `BoolKnob` defaulting **OFF**, read inside
   `OpenAdapter12`, plus a separate DLL so the INF/registry can be pointed away entirely. (§7)

---

## 1. `UserModeDriverName` semantics — AUTHORITATIVE

### 1.1 The enum (HEADER)

`tmp/dx12/sdk/d3dkmthk.h:1830-1845`, verbatim:

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

typedef struct _D3DKMT_UMDFILENAMEINFO
{
    KMTUMDVERSION       Version;                // In: UMD version
    WCHAR               UmdFileName[MAX_PATH];  // Out: UMD file name
} D3DKMT_UMDFILENAMEINFO;
```

So `D3DKMT_UMDFILENAMEINFO` is 4 + 260·2 = **524 bytes**, and the query type is
`KMTQAITYPE_UMDRIVERNAME = 1` (`tmp/dx12/sdk/d3dkmthk.h:2364`), passed through
`D3DKMT_QUERYADAPTERINFO` (`:2476-2482`):

```c
typedef struct _D3DKMT_QUERYADAPTERINFO
{
    D3DKMT_HANDLE           hAdapter;
    KMTQUERYADAPTERINFOTYPE Type;
    D3DKMT_PTR(VOID*,       pPrivateDriverData);
    UINT                    PrivateDriverDataSize;
} D3DKMT_QUERYADAPTERINFO;
```

MS Learn documents the per-value meanings (fetched 2026-08-05,
<https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmthk/ne-d3dkmthk-_kmtumdversion>):
`KMTUMDVERSION_DX12` = "DirectX 12"; `KMTUMDVERSION_DX12_WSA32` / `_WSA64` = "DirectX 12 Windows
Subsystem for Android (32 bit / 64 bit)".

### 1.2 The index→DDI-version mapping (MSDOC)

Microsoft states the positional contract twice, once per DDI generation:

- `windows-driver-docs-research-only/windows-driver-docs-pr/display/enabling-support-for-the-direct3d-version-10-ddi.md:18`
  — *"you must specify the name of the DLL that contains the version 10 DDI as the **second entry**
  in the list of user-mode display driver names"*, with the example at `:18`
  `HKR,, UserModeDriverName, %REG_MULTI_SZ%, umd9.dll, umd10.dll`.
- `.../enabling-support-for-the-direct3d-version-11-ddi.md:18` — *"you must specify the name of the
  DLL that contains the version 11 DDI as the **third entry** in the list of user-mode display driver
  names **even if the version 11 DDI exists in the same DLL as the version 9 and 10 DDIs**"*, example
  at `:27` `HKR,, UserModeDriverName, %REG_MULTI_SZ%, umd9.dll, umd10.dll, umd11.dll`.

The same doc at `:20` states explicitly that *"You can use the same user-mode display driver DLL
name in multiple locations to unify your driver implementation"* — i.e. **the slots are independent
by design; naming the same DLL in all of them is the convenience case, not the contract.**

Microsoft has **not** published an equivalent "…as the fourth entry" page for D3D12 (grep of the
whole in-tree docs mirror for `OpenAdapter12` and for a D3D12 UMD-registration article: zero hits).
The four-entry shape is however shown verbatim in the WDDM 2.1 run-from-driver-store sample,
`.../wddm-2-1-features.md:210-211`:

```inf
[regAdd]
HKR,,UserModeDriverName,%REG_MULTI_SZ%,%13%\myUMD64.dll, %13%\myUMD64.dll, %13%\myUMD64.dll, %13%\myUMD64.dll
HKR,,UserModeDriverNameWoW,%REG_MULTI_SZ%, %13%\myUMD32.dll, %13%\myUMD32.dll, %13%\myUMD32.dll, %13%\myUMD32.dll
```

Four entries, matching `KMTUMDVERSION` positions 0..3.

### 1.3 LIVE PROOF that index 3 is the D3D12 slot and that it is read

Command run on win11 (64-bit PowerShell, `Add-Type` P/Invoke of
`D3DKMTEnumAdapters2` + `D3DKMTQueryAdapterInfo(Type=1)` with `D3DKMT_UMDFILENAMEINFO.Version = v`):

```
adapter[0] h=0x40000000 luid=0:30217 sources=0          <- Helios
   KMTUMDVERSION=0 status=0x00000000 name=C:\WINDOWS\System32\DriverStore\FileRepository\helios_kmd_render.inf_amd64_3383a0e561ea9ca2\helios_umd.dll
   KMTUMDVERSION=1 status=0x00000000 name=...\helios_umd.dll
   KMTUMDVERSION=2 status=0x00000000 name=...\helios_umd.dll
   KMTUMDVERSION=3 status=0x00000000 name=...\helios_umd.dll
   KMTUMDVERSION=4 status=0xC000000D name=
   KMTUMDVERSION=5 status=0xC000000D name=
adapter[1] luid=0:30050   -> d3d10warp.dll for v=0..3, 0xC000000D for v=4,5
adapter[2] luid=0:30111   -> d3d10warp.dll for v=0..3, 0xC000000D for v=4,5
```

Facts established:

- The kernel answers `KMTUMDVERSION_DX12` (=3) for our adapter with our UMD path. The D3D12 runtime's
  UMD lookup therefore resolves through this exact index.
- `KMTUMDVERSION_DX12_WSA32/64` (4, 5) return **`STATUS_INVALID_PARAMETER` (0xC000000D)** on
  26100.8737 — for both Helios and both WARP/Basic-Render adapters. So a 6-entry `REG_MULTI_SZ` buys
  nothing today; **write four entries, not six.**
- The `%13%` token in the INF is expanded by SetupAPI to the absolute DriverStore path before it
  reaches the registry (registry read below).

Raw registry (LIVE, `reg query HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}\0000`):

```
UserModeDriverName    REG_MULTI_SZ   ...\helios_kmd_render.inf_amd64_3383a0e561ea9ca2\helios_umd.dll  (×4)
InstalledDisplayDrivers REG_MULTI_SZ helios_umd\0helios_umd\0helios_umd\0helios_umd
UserModeDriverNameWow REG_MULTI_SZ   ...\helios_kmd_render.inf_amd64_96afd14068f4bc12\helios_umd.dll  (×4)
```

MS Learn also confirms the lookup location for `KMTQAITYPE_UMDRIVERNAME`:
`HKLM\System\CurrentControlSet\Control\Class\{Adapter GUID}\0000\`
(<https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmthk/ns-d3dkmthk-_d3dkmt_umdfilenameinfo>).

### 1.4 ⇒ ANSWER for the split plan

> **YES — the D3D12 slot (index 3) may name a different DLL than the D3D11 slot (index 2).** The
> positional contract is Microsoft's own ("even if the version 11 DDI exists in the same DLL"), the
> `KMTUMDVERSION` enum is the index, and the kernel serves each index independently from the
> `REG_MULTI_SZ`. Helios can ship `helios_umd.dll` in slots 0-2 and `helios_umd12.dll` in slot 3
> with no runtime change and no KMD change.

**Constraint that comes with it (CODE + LIVE):** whichever DLL sits in a slot must export that
slot's entry point. `dumpbin /exports` on the live deployed UMD (LIVE) shows exactly three DDI
entry points plus `DllMain`:

```
   1    0 00038240 DllMain
   2    1 00038320 OpenAdapter10
   3    2 000383A0 OpenAdapter10_2
   4    3 00038420 OpenAdapter12
   ... then ~hundreds of cxxbridge1$rust_vec$* symbols (cxx's public ABI leaking out of the cdylib)
```

Note there is **no `OpenAdapter` (D3D9) export**, yet slot 0 names this DLL: a D3D9 client would
resolve slot 0, `LoadLibrary` us, and fail at `GetProcAddress`. That is pre-existing and orthogonal,
but it is the same class of mistake the split must not repeat in reverse.

`OpenAdapter12` as the *export name* is **not documented by Microsoft** (no hit anywhere in the docs
mirror). It is established here **empirically**: our `#[no_mangle] extern "system" fn OpenAdapter12`
(`umd/src/adapter.rs:177-189`) is called by the runtime in production (§2.2). By symmetry with the
documented D3D10/11 model — *"The Direct3D runtime next calls the user-mode display driver's
`OpenAdapter10_2` function **through the DLL's export table**"*
(`.../initializing-communication-with-the-direct3d-version-11-ddi.md:17`) — a separate
`helios_umd12.dll` needs only `OpenAdapter12` (+ `DllMain`) exported.

`D3D12DDIARG_OPENADAPTER` (`tmp/dx12/sdk/d3d12umddi.h:2686-2694`) carries **no `Interface`/`Version`
members** — unlike `D3D10DDIARG_OPENADAPTER` — so version negotiation is entirely via
`pfnGetSupportedVersions` in `D3D12DDI_ADAPTERFUNCS` (`:2674-2684`). (Detail is R1/R2's lane; it
matters here only because it means the *export* signature is trivially stable.)

### 1.5 `UserModeDriverNameWow` — LIVE, STALE, and a real defect

MS doc (`.../microsoft-windows-vista-display-driver-64-bit-issues.md:15,22`):

> "To allow 32-bit applications to run on a 64-bit operating system, **a 32-bit user-mode display
> driver must be provided** in addition to the 64-bit user-mode display driver … `HKR,,
> UserModeDriverNameWow, %REG_MULTI_SZ%, Xxx.dll`"

The same 32-bit-vs-64-bit split applies to the D3D12 slot: `C:\WINDOWS\SysWOW64\d3d12.dll` and
`d3d12core.dll` both exist on this VM (LIVE, 10.0.26100.8737), so a 32-bit D3D12 app is a real client.

**LIVE PROOF that WOW64 reads the Wow value.** The same P/Invoke probe re-run under
`%WINDIR%\SysWOW64\WindowsPowerShell\v1.0\powershell.exe`:

```
ptrsize=4
adapter[0] luid=0:30217
   ver=0..3 status=0x00000000 name=C:\WINDOWS\System32\DriverStore\FileRepository\helios_kmd_render.inf_amd64_96afd14068f4bc12\helios_umd.dll
   ver=4,5  status=0xC000000D
```

That is `UserModeDriverNameWow`, not `UserModeDriverName`. Consequences, all currently true on the
dev VM:

- The Wow path points at DriverStore folder `96afd140…` (last written **27-07-2026 03:08**), while
  the 64-bit path points at `3383a0e5…` (**05-08-2026 01:02**). **The Wow slot has been stale for
  nine days and ~dozens of deploys.**
- The DLL it names is the **x64** `helios_umd.dll` (6,016,000 bytes; the package has no 32-bit
  binary at all). A 32-bit D3D client on Helios therefore `LoadLibrary`s an x64 image →
  `ERROR_BAD_EXE_FORMAT` → the runtime falls back (WARP) or fails.
- `packaging/windows/README.md:54-56` states the intent: *"This package is x64-only. Native 32-bit
  applications need separately built x86 UMD, Mesa, CLVK, and loader binaries."* The registry
  contradicts that intent by advertising a 32-bit UMD that does not exist.

**Where the value came from is UNVERIFIED.** Nothing in this repository writes it: a
case-insensitive repo-wide grep for `UserModeDriverNameWow` outside `windows-driver-docs-research-only/`
returns exactly one hit, a prose line in `docs/archive/GATE5B_D3D_BRINGUP.md:82`. `git log --all -S
UserModeDriverNameWow` returns nothing. The INF in the 27-07 DriverStore folder (`96afd140…`) was
read directly (LIVE) and contains only the `UserModeDriverName` line. `Select-String` over all six
`C:\Windows\INF\setupapi.dev*.log` files for `UserModeDriverNameWow` returns zero hits.

> **Settling experiment (do not run without owner consent — it changes the machine):** delete
> `UserModeDriverNameWow` from `…\Class\{4d36e968…}\0000`, run
> `tools\install-helios-kmd.ps1` (a full package install through `devcon update`), and re-read.
> If it comes back, SetupAPI's display class installer synthesises it from `UserModeDriverName`;
> if it does not, it was a one-off manual write from an earlier session and should simply be
> deleted (or, better, `DelReg`'d by the INF).

**Is a 32-bit UMD required for a D3D12-capable adapter?** Two different bars:

- *Functionally:* no. 64-bit D3D12 apps work with only `UserModeDriverName[3]` populated — that is
  the current state and `OpenAdapter12` is reached (§2.2).
- *For WHQL/HLK plausibility:* Microsoft's language is "must be provided" for 32-bit app support
  (doc quoted above), and HLK graphics tests ship 32-bit variants. **UNVERIFIED** whether the modern
  (Win11 / HLK for 26100) Display device playlist hard-fails an x64-only package — I could not find
  a citable HLK requirement text. Settling read: the HLK "Device.Graphics" requirements page /
  `Device.Graphics.WDDM…` test list for the target OS. For Helios today this is moot: the package is
  test-signed, not WHQL-signed (`WINDOWS_CI_PACKAGE.md:38-48`).

### 1.6 `InstalledDisplayDrivers` is NOT index-parallel

Do not model it on `UserModeDriverName`. Microsoft's shape
(`.../adding-user-mode-display-driver-names-to-the-registry.md:20`) is:

```inf
HKR,, InstalledDisplayDrivers, %REG_MULTI_SZ%, UserModeDriverName1, UserModeDriverName2, UserModeDriverNameWow1, UserModeDriverNameWow2
```

i.e. a **flat list of the distinct UMD binaries in the package, extension stripped** — its stated
purpose (`:39`) is that *"WHQL test programs use the list … to validate that the driver binaries
remain unchanged over a test run"*, and WMI consumers treat it as the package file list. Our INF
writes `helios_umd` four times (`kmd_render/helios_kmd_render.inx:82`), which is harmless but
semantically wrong. **For a split, the correct value is `helios_umd,helios_umd12`, not
`helios_umd,helios_umd,helios_umd,helios_umd12`.**

---

## 2. How the runtime resolves and loads the UMD

### 2.1 Which process loads it — every D3D client, and once per device

MSDOC (`.../loading-a-user-mode-display-driver.md:25`): *"The Direct3D runtime obtains the user-mode
display driver's DLL name from the registry in order to load the user-mode display driver **in the
runtime's process space**."*

LIVE: `C:\ProgramData\Helios\` holds **2,374** `umd-<pid>.log` files. The per-process log path is
`umd/src/log.rs:21-30`:

```rust
let dir = std::path::Path::new(r"C:\ProgramData\Helios");
let _ = std::fs::create_dir_all(dir);
dir.join(format!("umd-{}.log", std::process::id()))
```

CODE — the measured load/unload cadence, `umd/src/log.rs:97-106`:

> "`helios_umd.dll` is loaded and unloaded **ONCE PER D3D11 DEVICE** — measured directly
> (`GetModuleHandleW` reads NO / yes / NO across one `D3D11CreateDevice` + `Release` pair …)"

This is the single most under-appreciated fact for the split: a second UMD DLL is **not** a
process-lifetime module. It will be `LoadLibrary`d/`FreeLibrary`d per D3D12 device, its `DllMain`
will run under the loader lock each time, and any never-released global it owns becomes a
**per-device leak** — exactly the 54th-session defect
(`memory/handle-leak-dll-unload-54th.md:40-47`; fix is `umd/src/lib.rs:65-76` +
`umd/src/log.rs:117-136`, with the refusal counter `LOG_CLOSE_CONTENDED`).

### 2.2 D3D12 already reaches us — including dwm

LIVE, grep of the newest 25 `umd-*.log` files for `OpenAdapter12` (counts are call counts):

```
umd-1832.log  proc=dwm                       oa12=2
umd-7828.log  proc=(exited)                  oa12=24
umd-7588.log  proc=StartMenuExperienceHost   oa12=2
umd-9048.log  proc=ApplicationFrameHost      oa12=2
umd-7028.log  proc=ShellHost                 oa12=2
umd-7156.log  proc=CrossDeviceResume         oa12=2
… 23 of the 40 newest logs have ≥1 OpenAdapter12
```

And the verbatim dwm sequence (LIVE, `umd-1832.log`):

```
[pid=1832 tid=5052] OpenAdapter12
[pid=1832 tid=5052] OpenAdapter12 -> DXGI_ERROR_UNSUPPORTED (D3D12 DDI not implemented yet)
[pid=1832 tid=2216] OpenAdapter10_2
```

⇒ **`dwm.exe` probes D3D12 on the Helios adapter on every boot today.** This is the fact that makes
the rollback story (§7) non-optional: the moment `OpenAdapter12` starts returning `S_OK`, dwm becomes
a potential D3D12 client, and a bug there costs the desktop.

DX12.md §1.1's claim — *"a D3D12 runtime already loads Helios' UMD and calls `OpenAdapter12`; it is
refused at the first statement"* — is **CONFIRMED** by the above, including the dwm case which
DX12.md does not mention.

### 2.3 Path resolution and the four traps

**Resolution:** INF `%13%` → DriverStore package directory → absolute path in the class-key
`UserModeDriverName` → `D3DKMTQueryAdapterInfo(KMTQAITYPE_UMDRIVERNAME)` → `LoadLibrary` →
`GetProcAddress("OpenAdapter12")`. Run-from-DriverStore is the required modern shape
(`HELIOS_DRIVER_DEPLOYMENT.md:26`), and WoW64 file-system redirection does **not** apply to the
DriverStore (`.../wddm-2-1-features.md:174`), so a 32-bit UMD lives in the same package directory (or
an INF-created `WoW64` subfolder, `.../wddm-2-1-features.md:218-228`).

**Trap 1 — the path is latched at DEVICE start, not process start.** `memory/start-menu-wedge-syncsharedtexture-55th.md:57-64`:

> "★ THE DEPLOY TRAP THAT COST TWO BUILD CYCLES AND VOIDED TWO RUNS. The default ProgramData UMD
> hotplug does **NOT** reach new processes: dxgkrnl caches the UMD path at **DEVICE** start …
> Always confirm with `(Get-Process -Id N).Modules` and deploy `-KillUmdUsers -RestartDevice
> -NoProbe`."

Also `memory/black-desktop-devlocal-import-critique.md:225-226`. **LIVE confirmation this session:**
three concurrently-live processes were running three *different* DriverStore copies while the
registry named a fourth —

| process | module it actually loaded |
|---|---|
| `dwm` (pid 1832) | `…inf_amd64_169fe0dfef2886ab\helios_umd.dll` |
| pid 7000 | `…inf_amd64_2122745a5fdf04a4\helios_umd.dll` |
| pid 2404 | `C:\ProgramData\HeliosUmd\helios_umd_b16a79485512ae76.dll` |
| registry `UserModeDriverName` | `…inf_amd64_3383a0e561ea9ca2\helios_umd.dll` |

(and their `3DPIPELINESUPPORT` caps differ, `0x1` vs `0x8f` — i.e. genuinely different builds live in
one boot). Any D3D12 A/B measurement that does not pin the module path is measuring an unknown binary.

**Trap 2 — stale DriverStore copy at COLD BOOT.** `memory/first-content-frames-icd-buffer-reqs.md:32-36`:
at cold boot dxgkrnl loads the *DriverStore* copy for dwm's first (composition) device; the
ProgramData registry override only affects later creates. The deploy script therefore syncs both —
`tools/hotplug-helios-umd.ps1:105-133`, with the `-DisplaceInUse` rename for the routinely-mapped
package copy (`:115-127`, which records a store SHA stale by eight hours and six deploys).

**Trap 3 — ProgramData hotplug uses content-hashed names.** `tools/hotplug-helios-umd.ps1:51`:
`helios_umd_{first 16 hex of SHA256}.dll`. A D3D12 sibling needs the same treatment or the two
hotplug paths will disagree about which build is live.

**Trap 4 — `ExecutionPolicy`.** The VM's machine policy is `Restricted`; the `.ps1` scripts must be
invoked as `powershell -NoProfile -ExecutionPolicy Bypass -File …` or they silently no-op
(`tools/win-mcp/src/main.rs:779`, `BRINGUP_QUIRKS.md:74-83`).

---

## 3. The INF diff

⚠ **Project rule: `*.inx` is only edited with explicit instruction** (CLAUDE.md, "Files Not to
Touch"). What follows is a *proposal*, not an applied change.

Current relevant lines, `kmd_render/helios_kmd_render.inx`:

```inf
14  [DestinationDirs]
15  Helios_CopyFiles = 13  ; driver store (DIRID 13) — required for a universal INF
…
20  [SourceDisksFiles]
21  helios_kmd_render.sys = 1,,
22  helios_umd.dll = 1,,
…
42  [Helios_CopyFiles]
43  helios_kmd_render.sys
44  helios_umd.dll
…
80  [Helios_DeviceSettings]
81  HKR,, UserModeDriverName,       %REG_MULTI_SZ%, %13%\helios_umd.dll,%13%\helios_umd.dll,%13%\helios_umd.dll,%13%\helios_umd.dll
82  HKR,, InstalledDisplayDrivers,  %REG_MULTI_SZ%, helios_umd,helios_umd,helios_umd,helios_umd
```

### Variant A — same DLL, D3D12 in `helios_umd.dll`

**Zero INF changes required.** Slot 3 already names `helios_umd.dll` and `OpenAdapter12` is already
exported and already called (§2.2). Turning D3D12 on is a pure UMD-code change plus §7's knob. This
is the cheapest possible bring-up and it is what the tree is already wired for.

Cost: every D3D12 bug ships inside the same binary that dwm's D3D11 composition device loads, and the
UMD gains vkd3d's static footprint (and, if `libvkd3d` is linked in, its LGPL obligations, §5.1) in
the module that runs in every D3D client on the machine.

### Variant B — separate `helios_umd12.dll`

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

Notes on each line:

- `DestinationDirs` needs **no** change: `Helios_CopyFiles = 13` already sends everything in that
  section to the DriverStore package directory. Do **not** add `COPYFLG_IN_USE_TRY_RENAME` — combined
  with DIRID 13 `infverif` rejects it (`HELIOS_DRIVER_DEPLOYMENT.md:29-30`).
- The `[Strings]` block already defines `REG_MULTI_SZ = 0x00010000` (`:98`); nothing new.
- `Include = msdv.inf` at `:37` is copied verbatim from Microsoft's own sample
  (`.../adding-software-registry-settings.md:32`) and, with no matching `Needs=`, is inert. Leave it
  alone while touching this file; **UNVERIFIED** whether removing it is safe (settling experiment:
  build the package with it removed and run `infverif /w` + a full `devcon update`).
- If vkd3d-proton's `d3d12.dll`/`d3d12core.dll` are ever shipped *inside the driver package*, they get
  the same `SourceDisksFiles` + `CopyFiles` treatment — but see §5.2 for why DriverStore placement
  does **not** make them findable by the D3D12 loader.

### Variant B+ — with a 32-bit UMD (only if §1.5 is being fixed properly)

```inf
[DestinationDirs]
Helios_CopyFiles      = 13
Helios_CopyFilesWoW64 = 13,WoW64

[Helios_CopyFilesWoW64]
helios_umd32.dll
helios_umd12_32.dll

[Helios_DeviceSettings]
HKR,, UserModeDriverNameWow, %REG_MULTI_SZ%, %13%\WoW64\helios_umd32.dll,%13%\WoW64\helios_umd32.dll,%13%\WoW64\helios_umd32.dll,%13%\WoW64\helios_umd12_32.dll
```

(shape taken from `.../wddm-2-1-features.md:218-228`). Until such binaries exist, the correct action
is the opposite: **`DelReg` the bogus `UserModeDriverNameWow`** so 32-bit clients get a clean "no
driver" instead of a load failure —

```inf
[Helios_Install]
…
DelReg = Helios_RemoveDeviceSettings

[Helios_RemoveDeviceSettings]
HKR,, UserModeDriverNameWow
```

### Version single-site rule — what must move together

`kmd_render/driver-version.env` is the **only** place the version is edited (currently
`HELIOS_KMD_VERSION=22.22.252.0`). Its header comment names the three consumers:

- `kmd_render/build.rs` → renders the `.sys` `FILEVERSION`/`PRODUCTVERSION` (`build.rs:65,192-249`);
- `kmd_render/Cargo.make.toml` top-level `env_files` (`:19`, and the comment at `:13-18` warns it is
  **not** a `[config]` key) → `[tasks.stampinf]` `-v ${HELIOS_KMD_VERSION}` (`:212-223`);
- `tools/win-mcp` (`win_build_kmd`) reads and rewrites it when bumping.

> "An INF DriverVer that disagrees with the image FILEVERSION is FAILED_ADD **0xc0000182** at install,
> discovered only after a reboot." — `kmd_render/driver-version.env:11-13`

**LIVE, and this is a live gap:** the deployed `helios_umd.dll` has **no version resource at all**
(`(Get-Item …\helios_umd.dll).VersionInfo.FileVersion` is empty), while the `.sys` reads
`22.22.252.0` and the INF `DriverVer = 08/05/2026,22.22.252.0`. `umd/build.rs` contains no `rc.exe`
step (grep for `rc.exe|VERSIONINFO|resource` → zero hits; `kmd_render/build.rs` has all of them). MSDOC
(`.../wddm-2-1-features.md:242-244`): *"The driver information file (.inf), kernel-mode driver (.sys),
and user-mode driver (.dll) file version info **must match**."* Adding a second UMD DLL is the natural
moment to give both UMDs a version resource driven from the same `driver-version.env`.

---

## 4. Signing / packaging today, and what a second DLL changes

### 4.1 The build/package chain

`kmd_render/Cargo.make.toml`:

- `[tasks.copy-umd-to-package]` (`:44-101`) shells out to `cargo build` in `umd/` with the *same*
  profile (`:60-72` — one source of truth for the profile, after a bug where all three were the
  literal `"debug"`), asserts the DLL exists (`:86-91`), and copies it into
  `kmd_render/target/<profile>/helios_kmd_render_package/`.
- `[tasks.stampinf]` (`:212-223`) stamps `DriverVer` from `HELIOS_KMD_VERSION`.
- `[tasks.inf2cat]` (`:225-239`) depends on `verify-no-panics`, `copy-driver-binary-to-package`,
  `copy-inf-to-package`, `copy-umd-to-package` and runs `inf2cat /driver:<pkg> /os:10_x64 /uselocaltime`.
- Its note at `:39-43`: whatever the task stages is **overwritten** by `install-helios-kmd.ps1`'s
  `Sync-HeliosPackageUmd` before the catalog is created and signed.

`tools/install-helios-kmd.ps1`:

- `$UmdDll` defaults to `…\umd\target\release\helios_umd.dll` (`:1-10`).
- `Find-Signtool` / `Find-Inf2Cat` (`:28-48`) search both `x64` and `x86` WDK bin dirs — `Inf2Cat.exe`
  ships **x86-only** (`BRINGUP_QUIRKS.md:49-51`; calling a nonexistent x64 path fails **silently**).
- `Sync-HeliosPackageUmd` (`:157`, used at `:315` and `:351`) puts the chosen UMD into the package.
- Signing (`:139-149`): `signtool sign /v /fd SHA256 /tr … /sm /s My /sha1 <thumbprint>` with
  machine-store `CN=WDRLocalTestCert`, imported to `LocalMachine\Root` + `TrustedPublisher`
  (`:109-128`).
- File set copied to the DriverStore (`:327-328`):
  `helios_kmd_render.inf`, `.sys`, `.cat`, `helios_umd.dll`.

CI (`ci/windows/`): `Build-Driver.ps1:96` `$required = @("helios_kmd_render.inf",
"helios_kmd_render.sys", "helios_umd.dll")`; `Assemble-Package.ps1:49` the same list,
`:52` optional PDBs/map, `:116` `Invoke-SignTool … helios_umd.dll`. Signing model:
`WINDOWS_CI_PACKAGE.md:38-48` — an ephemeral non-exportable per-bundle key, private key destroyed;
Windows must boot with test-signing on (⇒ Secure Boot off).

### 4.2 What a second UMD DLL touches

Every list above is hand-maintained and each one is a place the new DLL can be silently dropped:

| Site | Change |
|---|---|
| `kmd_render/helios_kmd_render.inx:21-22, 43-44, 81-82` | `SourceDisksFiles`, `CopyFiles`, both registry values |
| `kmd_render/Cargo.make.toml:44-101` | build + stage the second DLL (or one task that stages a list) |
| `tools/install-helios-kmd.ps1:300, 315, 327-328, 351, 428` | `Sync-HeliosPackageUmd` for the second DLL, add to `$copyNames`, add to the final state hash map |
| `tools/hotplug-helios-umd.ps1:51, 100-133` | hashed ProgramData name, `UserModeDriverName` rewrite, **DriverStore sync of both** |
| `ci/windows/Build-Driver.ps1:96`, `Assemble-Package.ps1:49,52,116` | required-file list, optional PDB list, `Invoke-SignTool` |
| `packaging/windows/Verify-Helios.ps1` | no UMD check exists today (§4.5) |

### 4.3 The catalog rules that bite

From `BRINGUP_QUIRKS.md:37-67`, all previously paid for:

- `cargo make` may sign a **stale** `.sys`; repackage by hand and compare sizes.
- Re-running `inf2cat` over a package that already has a **signed** `.cat` produces a **corrupt**
  `.cat` (`CryptCATOpen → 0x0000000D`), driver fails to load with `0xC000026C` → **Code 39**, and the
  diag ring is empty because AddDevice never ran. Delete the `.cat` first, run `inf2cat` standalone
  (chaining `Remove-Item; inf2cat` in one pipeline races), then sign.
- `Get-AuthenticodeSignature` on the `.cat` proves nothing about coverage. Verify with
  `signtool verify /pa /c <cat> <sys>` ("Successfully verified") **and** compare
  `Get-FileHash` of the deployed vs package binary. **A second UMD DLL doubles the number of
  hashes the catalog must cover, and `signtool verify /pa /c <cat> helios_umd12.dll` is the check
  that must be added.**
- A bare `& signtool …` not on PATH fails **silently**, leaving `NotSigned`.

### 4.4 Test-signing constraints

Test-signing must stay on (`bcdedit /set testsigning on`, `packaging/windows/Install-Helios.ps1:26-37`)
and Secure Boot off; the installer refuses to enable test-signing while Secure Boot is on (`:30-32`)
and exits 3010 for a reboot (`:35-36`). A second UMD DLL is covered by the same catalog, so no extra
certificate work — but it *must* be signed before `inf2cat`, in the same order the existing UMD is
(`ci/windows/Assemble-Package.ps1:116`).

### 4.5 `Verify-Helios.ps1` gap

`packaging/windows/Verify-Helios.ps1` (92 lines, read in full) checks runtime-file hashes, PnP status,
driver provider, the Vulkan ICD registry, `OpenGLDriverName`, and the OpenCL vendor key — **it never
looks at `UserModeDriverName` at all**, and the smoke list (`:70-75`) is Vulkan / D3D11 / OpenGL /
OpenCL. A D3D12 landing should add (a) a `UserModeDriverName[3]` assertion, (b) a
`UserModeDriverNameWow` assertion (currently it would fail — see §1.5), and (c) a `d3d12-smoke.exe`.

---

## 5. Shipping vkd3d-proton's binaries

### 5.1 License — exact terms

- `vkd3d-proton-helios/COPYING` (16 lines, read in full):
  > "Copyright 2016-2024 the vkd3d-proton project authors (see the file AUTHORS for a complete list)
  >
  > vkd3d-proton is free software; you can redistribute it and/or modify it under the terms of the
  > **GNU Lesser General Public License** as published by the Free Software Foundation; either
  > **version 2.1** of the License, **or (at your option) any later version**."
- `vkd3d-proton-helios/LICENSE` is the 502-line verbatim **LGPL-2.1** text (`Version 2.1, February 1999`).
- Every source file carries the same header, e.g. `libs/d3d12/main.c:6-18`.

⇒ **LGPL-2.1-or-later.** Practical reading for the two designs:

- **Ship `d3d12.dll` + `d3d12core.dll` as separate, dynamically-loaded DLLs** (vkd3d's own shipping
  model): Helios distributes unmodified-or-modified LGPL libraries. Obligations: carry the license
  text, state changes, and provide corresponding source for the LGPL parts. Helios' own UMD stays
  outside the LGPL boundary because it links to them only across the DLL boundary.
- **Statically link `libvkd3d` into `helios_umd12.dll`** (the shape `umd/build.rs:218-223` already
  uses for DXVK archives — `libhelios_d3d11_static.a` / `libdxvk.a`): LGPL 2.1 §6 then requires
  shipping either the object files or a mechanism allowing the user to relink `helios_umd12.dll`
  against a modified vkd3d. **Note the precedent**: DXVK is zlib/libpng-licensed, so the existing
  static-link model carried no such obligation; vkd3d is the first LGPL component that would enter
  the UMD binary. This is a decision for the owner, not for the implementer. **UNVERIFIED**: I did
  not audit whether vkd3d-proton has additional non-LGPL vendored components with their own terms
  (`subprojects/`, `khronos/`); settling read: `vkd3d-proton-helios/subprojects/*` and
  `khronos/*` license headers.

### 5.2 DLL search / app-local override / system-wide install

vkd3d-proton on Windows is a **drop-in replacement for the OS `d3d12.dll` + `d3d12core.dll` pair**,
not a UMD. Its `d3d12.dll` is a thin loader (`libs/d3d12/main.c:66-135`):

```c
static bool load_d3d12core_module(const char *module_name) { … vkd3d_dlopen(module_name) …
    vkd3d_dlsym(d3d12core_module, "D3D12GetInterface"); … }

static void load_d3d12core_once(void) {
    ret = load_d3d12core_module(SONAME_D3D12CORE);      /* "d3d12core.dll" */
#ifdef _WIN32
    if (!ret) {                                         /* fallback: system32 */
        GetSystemDirectoryA(buf, sizeof(buf));
        vkd3d_strlcat(buf, sizeof(buf), "\\" SONAME_D3D12CORE);
        ret = load_d3d12core_module(buf); } …
```

(`SONAME_D3D12CORE` = `"d3d12core.dll"`, `vkd3d-proton-helios/include/vkd3d_sonames.h:26`.)

Exports — `libs/d3d12/d3d12.def`: `D3D12CreateDevice @101`, `D3D12GetDebugInterface @102`,
`D3D12CreateRootSignatureDeserializer`, `D3D12CreateVersionedRootSignatureDeserializer`,
`D3D12EnableExperimentalFeatures`, `D3D12SerializeRootSignature`,
`D3D12SerializeVersionedRootSignature`, `D3D12GetInterface`.
`libs/d3d12core/d3d12core.def`: `D3D12GetInterface`, `D3D12SDKVersion DATA PRIVATE`
(defined `libs/d3d12core/main.c:1355`: `DLLEXPORT const UINT D3D12SDKVersion = D3D12_SDK_VERSION;`).

**Placement rules on real Windows (not Wine):**

- **App-local is the only supported override.** Windows' `LoadLibrary` search order puts the
  executable's directory before `System32`, so `d3d12.dll` next to the `.exe` wins for an app that
  links `d3d12.dll` normally. This is what vkd3d-proton's Windows story relies on
  (vkd3d-proton README / DeepWiki "Installation and Configuration", <https://deepwiki.com/HansKristian-Work/vkd3d-proton/8.3-installation-and-configuration>).
- **A system-wide install is not available.** Replacing `C:\Windows\System32\d3d12.dll` (LIVE:
  10.0.26100.8737, 146,168 bytes) is blocked by WRP/TrustedInstaller and would be reverted by
  servicing; there is no `KnownDLLs`- or registry-based redirection for it. Wine's `WINEDLLOVERRIDES`
  equivalent does not exist on Windows. **⇒ Any vkd3d-proton-based Helios D3D12 is per-application,
  not machine-wide, unless it is reached through the UMD DDI instead.**
- **Agility SDK opt-in is orthogonal and does NOT give a vendor a system-wide hook.** The mechanism
  (<https://devblogs.microsoft.com/directx/gettingstarted-dx12agility/>) is: the *application* exports
  `D3D12SDKVersion` (UINT) and `D3D12SDKPath` (const char*, a path **relative to the exe**, e.g.
  `".\\D3D12\\"`); the system `d3d12.dll` then loads `D3D12Core.dll` from that app-relative directory
  and fails `D3D12CreateDevice` if the exported version does not match the DLL's. It falls back to
  System32's `D3D12Core.dll` otherwise. **It is an app-authored opt-in, keyed off the app's own
  exports** — Helios cannot use it to insert vkd3d under an unmodified third-party app. vkd3d-proton's
  own `D3D12SDKVersion` export exists so its `d3d12core.dll` can *stand in* for an Agility
  `D3D12Core.dll`, which only helps for apps the packager controls. (Coordinate with R10; the
  packaging mechanics above are stated independently here.)

⇒ **Packaging conclusion.** There are exactly three deliverable shapes, and only the first is
machine-wide:

| Shape | Machine-wide? | Registration | License exposure |
|---|---|---|---|
| **(i) D3D12 UMD DDI** — `helios_umd12.dll` implements `d3d12umddi.h`, engine internals may be vkd3d-derived | **Yes** — every D3D12 app on the adapter | INF slot 3 (§3) | LGPL only if vkd3d code is linked in (§5.1) |
| **(ii) app-local vkd3d `d3d12.dll`+`d3d12core.dll`** | No — per app directory | none (file placement) | LGPL, dynamic |
| **(iii) Agility-style, app exports `D3D12SDKPath`** | No — app must be rebuilt | none | LGPL, dynamic |

Shapes (ii)/(iii) are usable as an **early evidence vehicle** (they need no INF, no signing, no
reboot, and they exercise the Vulkan/venus substrate end-to-end), but they cannot be the shipping
answer for "D3D12 on Helios" — dwm, the Store apps, and 3DMark's D3D12 workloads all go through the
OS `d3d12.dll` → `UserModeDriverName[3]` path.

---

## 6. Two engines in one process — concrete hazards

Assume a process holds both a DXVK-backed D3D11 device (dwm always does; many games do) and a
vkd3d-backed D3D12 device.

**H1 — two Vulkan instances against one venus ICD, and a process-global context id.**
The Helios ICD documents this exact shape, `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:526-536`:

> "Process-global state, audited 2026-07-06 (23rd session) for the two-live-VkInstance shape the
> dcomp present vehicle introduces (a Vulkan app's own instance + the vehicle's DXVK->ICD stack in
> the SAME process): … `helios_current_ctx_id`: **last-writer-wins across instances**, and destroy
> only clears it when it still matches the dying renderer. Ambiguous with two instances — bridge
> callers must use the handle-based `helios_venus_instance_ctx_id()` below; the process-global form
> stays for single-instance probes only."

And the UMD bridge's own note, `umd/bridge/bridge_icd_exports.cpp:396-401`:

> "The process-global export is last-writer-wins across two live venus instances, so its result was
> the LEAST trustworthy of the two …"

**H1a — the "handle-based" replacement is actually THREAD-LOCAL.**
`vn_renderer_helios.c:639-644`:

```c
__declspec(dllexport) uint32_t
helios_venus_instance_ctx_id(VkInstance instance)
{
   (void)instance;                       /* opaque: loader wrapper, not a vn_instance */
   return helios_calling_thread_ctx_id;  /* _Thread_local, :546 */
}
```

with `:540-546` explaining that the bridge "asks for the context on the same thread that
synchronously created its DxvkInstance". **A vkd3d-based engine that creates its `VkInstance` on one
thread and asks for the ctx id on another gets the wrong answer (or 0).** This is the single most
likely silent-corruption vector when a second engine is added, and it is a *pre-existing* ICD
contract, not something a D3D12 UMD can fix from its own side.

**H1b — `helios_current_renderer` is "last-created wins"** with a process-wide SRWLOCK
(`vn_renderer_helios.c:547-559`) for the adapter-global scanout query. Two engines racing device
creation makes which renderer answers non-deterministic.

**H2 — ICD module refcounting.** `umd/bridge/bridge_icd_exports.cpp:245-287`: the bridge either finds
the ICD among loaded modules (holding one reference, `:48-51`) or `LoadLibraryA`s it (`:251`), caches
the `HMODULE` in a function-local `std::atomic<HMODULE>` (`:270`) and **never releases it on the
success path**. Because `helios_umd.dll` is loaded/unloaded **once per device** (§2.1), each device
adds a reference. A second UMD DLL has its *own* copy of that static ⇒ **the leak rate doubles per
process that uses both**. The recorded mitigation is that the ICD pins itself
(`GetModuleHandleExW(..._PIN)`, refusals counted in `helios_module_pin_failures`,
`vn_renderer_helios.c:522-524`; narrative in `memory/handle-leak-dll-unload-54th.md:40-47`) — pinning
makes the extra references harmless but does not make them *zero*, and the per-device handle-rate
soak (`tools/helios_ownership_soak.cpp`) must be re-baselined after any split.

**H3 — the anchor-export module search can pick the wrong ICD.**
`bridge_icd_exports.cpp:38-39` uses `helios_venus_memory_alloc_info` as the anchor and `:41-51` walks
`Module32First/Next`. Comment at `:49-51`: *"Looking up each export independently can mix two ICD
versions and call a function with a foreign `VkDeviceMemory`/`VkInstance` handle."* If a second UMD
DLL carries its own copy of this resolver, the two copies can independently select **different** ICD
modules in the same process. Mitigation for the split: **one shared resolver**, i.e. either link the
bridge into exactly one DLL and export a tiny C ABI from it, or make the anchor selection
process-global via a named handle rather than a per-module static.

**H4 — static-init order and `DllMain`.** `umd/src/lib.rs:56-61` enumerates the rules the UMD's
`DllMain` obeys (no allocation, no I/O, **no `LoadLibrary`**, no thread waits, no panic, nothing on
the process-exit path). A vkd3d-derived DLL brings a C runtime + `pthread_once`
(`libs/d3d12/main.c:52,112-139`) whose first use is at `D3D12GetInterface` time, not at attach — that
is fine, but any Helios glue added to `helios_umd12.dll`'s `DllMain` must obey the same list.
`DLL_PROCESS_DETACH` under the loader lock while the *other* UMD DLL is mid-call is a real ordering
question the split creates and the single-DLL design does not.

**H5 — log-file collisions.** Three distinct writers, all under `C:\ProgramData\Helios\`:

| writer | path | source |
|---|---|---|
| Rust UMD | `umd-<pid>.log` | `umd/src/log.rs:21-29` |
| DXVK engine | `<exeBaseName>_helios_umd_dxvk.log` | `umd/bridge/dxvk_bridge.cpp:70` (`Logger Logger::s_instance("helios_umd_dxvk.log")`) + `dxvk-helios/src/util/log/log.cpp:124-149` (default dir forced to `C:/ProgramData/Helios`, `path += exeName + "_" + base`) |
| vkd3d | stderr by default; a file only if `VKD3D_LOG_FILE` is set | `vkd3d-proton-helios/libs/vkd3d-common/debug.c:110-114` |

Two hazards: (a) `Logger::s_instance` is a **per-DLL static**, so if `helios_umd12.dll` also statically
links DXVK/its own Logger, both DLLs open the same `<exe>_helios_umd_dxvk.log` for append in one
process — interleaved, unattributable output. (b) vkd3d's default is `stderr`, which is a black hole
in `dwm.exe` — the same trap `dxvk-helios/src/util/log/log.cpp:135-140` records:

> "Helios: default the log next to the UMD's own per-pid log. Processes like dwm.exe cannot write
> their CWD (System32), which silently swallowed every `Logger::err` from the shared-surface create
> path; `C:\ProgramData\Helios` is writable by standard users."

⇒ **give vkd3d a distinct, per-pid, ProgramData-rooted default log name before the first bring-up run.**

**H6 — the UMD log handle.** `umd/src/log.rs:83-93` keeps one process-lifetime `File`; the second
DLL will open a **second handle to the same `umd-<pid>.log`** unless the two share a writer. Either
give the D3D12 UMD its own `umd12-<pid>.log`, or share via an exported function. Note
`log.rs:117-136`'s `try_lock`-under-loader-lock discipline must be replicated, not re-invented.

**H7 — knob divergence.** `umd/src/knobs.rs` reads `HKLM\SOFTWARE\Helios` REG_DWORDs **once per
process** via one audited `RegGetValueA` site (`:82-105`) with `OnceLock` caching. A second DLL gets
its own `OnceLock`s ⇒ two independent caches of the same values. That is benign for read-only knobs
but means `resolved_inventory()` (`:277-290`, 10 entries, dumped once at adapter open) will print
twice with possibly different values if the registry is edited mid-process.

---

## 7. Rollback — the concrete mechanism

**Requirement, restated from evidence:** dwm probes D3D12 today (§2.2). Any D3D12 enablement is a
change to dwm's behaviour on the next boot. The disable path must therefore work (a) without a
rebuild, (b) without a working desktop, and (c) ideally without a reboot for *new* processes.

**Proposal — three layers, in increasing blast radius:**

### L1 — a registry knob, mirroring `umd/src/knobs.rs` (primary)

```rust
/// D3D12 DDI enable. `HKLM\SOFTWARE\Helios!UmdD3D12` (REG_DWORD), read once per
/// process. **Absent = OFF** during bring-up: `OpenAdapter12` returns
/// DXGI_ERROR_UNSUPPORTED exactly as it does today, so an install with the knob
/// unset is bit-identical to a build without the D3D12 path.
pub(crate) static UMD_D3D12: BoolKnob = BoolKnob::new(c"UmdD3D12", false);
```

read at the top of `OpenAdapter12` (`umd/src/adapter.rs:177`), and added to
`resolved_inventory()` (`knobs.rs:277-290`) so it is dumped at every adapter open.

Why this is the right shape here:

- It matches the established pattern exactly — `BoolKnob::new(name, default)` forces the
  absent-value policy to be written at the definition site (`knobs.rs:1-13`), and the knob is
  read once per process so *already-running* dwm keeps its behaviour while new processes pick up
  the change (the same property `feature_level_mode()` documents at `:311-313`).
- `HKLM\SOFTWARE\Helios` is settable over SSH/session 0 with the desktop down.
- **No length limit** applies: the UMD reads via `RegGetValueA` (`knobs.rs:63-71,89-99`). (The ≤14-char
  cap is a *KMD* constraint — `kmd_render/src/diag.rs:380,468,487,517` — because those knobs go
  through `RtlQueryRegistryValues` on the service key. Do not confuse the two hives.)
- **CLAUDE.md rule:** "A knob's default is a decision, and it must match the measured configuration."
  Default OFF is correct *during bring-up*, and the flip to ON must land with the evidence in the
  comment at the read site — exactly as `UmdCommandLists` did (`knobs.rs:240-260`, ROADMAP.md:32-46).

### L2 — a separate DLL in slot 3 (structural)

With Variant B (§3), the recovery from "`helios_umd12.dll` will not even load" is a **single
`REG_MULTI_SZ` rewrite** of `UserModeDriverName[3]` back to `helios_umd.dll` (or to a nonexistent
path, which yields a clean D3D12 "no driver" while D3D11 is untouched) — no package reinstall, no
catalog, no reboot for new processes, and dwm's D3D11 composition device is provably unaffected
because it resolves index 2. `tools/hotplug-helios-umd.ps1:100-103` already writes this value; it
needs a `-D3D12Dll <path|None>` parameter, nothing more.

This is the strongest argument for Variant B and it is a *stability* argument, not an architecture
one: it converts "a broken D3D12 UMD" from a desktop-down event into a registry edit.

### L3 — package rollback (last resort)

`tools/install-helios-kmd.ps1` backs up the active DriverStore files under
`C:\ProgramData\HeliosDeployBackups\<timestamp>` (`HELIOS_DRIVER_DEPLOYMENT.md:87`), and the
recovery boot without the `virtio-gpu-gl-pci` device (`BRINGUP_QUIRKS.md:142-153`) unlocks the live
`.sys`/`.dll` for replacement. Owner-driven; VM device-set changes are owner-gated (CLAUDE.md).

### What NOT to do

- Do **not** make the disable a compile-time `#[cfg]`. The whole point is that the failure is
  discovered on a machine where you cannot rebuild-and-redeploy through a dead desktop.
- Do **not** gate it on an environment variable alone: dwm's environment is not yours to set, and the
  UMD's existing env knobs are explicitly documented as *not* covered by the knob inventory
  (`knobs.rs:51-56`).
- Do **not** return `DXGI_ERROR_DRIVER_INTERNAL_ERROR` on the disabled path. `adapter.rs:182-188`
  records why: until R801 that site returned `0x887A_0020` and the runtime + ETW logged an ordinary
  "no D3D12 DDI" negotiation as a **driver fault**. The disabled path must keep returning
  `DXGI_ERROR_UNSUPPORTED` = `0x887A_0004`.

---

## 8. UNVERIFIED items, each with its settling experiment

| # | Question | Settling experiment |
|---|---|---|
| U1 | Does index 3 *specifically* map to D3D12, or does the kernel return one string for every index because all four entries are identical? | The four entries are identical on every adapter on this box (Helios and both WARP/Basic-Render), so the mapping is proven only by the enum + the two MS "second entry / third entry" doc statements + the fact that `OpenAdapter12` is called. **Settle:** temporarily set `UserModeDriverName` to four *distinct* strings (e.g. `a.dll,b.dll,c.dll,d.dll`) and re-run the read-only `D3DKMTQueryAdapterInfo` probe from §1.3; expect `v=0→a.dll … v=3→d.dll`. This is a registry write and needs owner consent; it also breaks D3D until restored. |
| U2 | Who writes `UserModeDriverNameWow` on this machine? | Delete the value, run `tools\install-helios-kmd.ps1` (full package install), re-read. See §1.5. |
| U3 | Does a modern HLK Display playlist hard-require a 32-bit UMD? | Read the HLK `Device.Graphics` requirement list for the target OS build. Moot while the package is test-signed. |
| U4 | Is `Include = msdv.inf` (`.inx:37`) removable? | Remove, `cargo make` the package, run `infverif /w` on the produced INF, then a full `devcon update`. |
| U5 | Does vkd3d-proton's `subprojects/` / `khronos/` carry non-LGPL terms that change §5.1? | Read the license headers under `vkd3d-proton-helios/subprojects/` and `vkd3d-proton-helios/khronos/`. |
| U6 | Is the export name for a D3D12 UMD really and only `OpenAdapter12`? | Empirically yes on 26100 (the runtime calls ours). To be exhaustive: run a WPP/ETW `Microsoft-Windows-DxgKrnl`/`Microsoft-Windows-D3D12` trace across a `D3D12CreateDevice` and read the `LoadLibrary`/`GetProcAddress` sequence, or `strings` the OS `d3d12core.dll` for `OpenAdapter12`. |
| U7 | Whether a *second* UMD DLL is loaded/unloaded per-device the same way `helios_umd.dll` is, or whether the D3D12 runtime keeps it resident | Repeat the 54th-session measurement (`tools/helios_handle_types.cpp` / `GetModuleHandleW` NO/yes/NO) across a `D3D12CreateDevice` + `Release` pair once a D3D12 device can be created. This decides whether §6-H2's leak doubling is real. |
| U8 | Whether `KMTUMDVERSION_DX12_WSA32/64` ever become live (a 6-entry `REG_MULTI_SZ`) | Nothing to do: they return `STATUS_INVALID_PARAMETER` on 26100 for every adapter, and WSA is a retired product. Re-probe if the guest OS build changes. |

---

## 9. Direct recommendations for the D3D12 plan

1. **Start in Variant A (same DLL) for the DDI handshake only, then move to Variant B before any
   real D3D12 device work lands.** Variant A costs zero INF changes and the slot is already wired;
   Variant B is what makes the failure mode recoverable (§7-L2). The switch is a two-line INF diff.
2. **Land the `UmdD3D12` knob in the same commit as the first non-refusing `OpenAdapter12`**, default
   OFF, and keep `DXGI_ERROR_UNSUPPORTED` as the disabled return.
3. **Fix or delete `UserModeDriverNameWow` first.** It is wrong today, it will be wrong for D3D12
   too, and it is a two-line INF `DelReg` (§3, Variant B+).
4. **Give both UMD DLLs a version resource from `driver-version.env`** while the packaging is being
   touched (§3, §4.4) — the WDDM requirement is explicit and the current DLL has none.
5. **Do not put vkd3d's `d3d12.dll`/`d3d12core.dll` in the driver package.** DriverStore placement
   does not make them findable; app-local placement does. If they ship at all, they ship as an
   opt-in developer artifact next to a specific `.exe` (§5.2), and that is a *measurement vehicle*,
   not the product.
6. **Before the first two-engine run, do three things:** give vkd3d a per-pid ProgramData log default
   (§6-H5), decide who owns the ICD anchor/module resolver (§6-H3), and confirm the ctx-id export
   contract for a non-creating thread (§6-H1a). Each of those is a silent-wrong-answer path, not a
   crash.
7. **Every deploy of a D3D12 UMD must confirm the loaded module path per process**
   (`(Get-Process -Id N).Modules`) — three different builds were live in one boot on this VM
   (§2.3). No D3D12 evidence is admissible without it.
