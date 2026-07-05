# Helios DOD Code 43 Findings — Archived

> **ARCHIVED (2026-06-07):** this was the handoff for the WDDM Display-Only Driver bring-up. The active project
> direction has switched back to System-class KMDF + DeviceIoControl + Mesa Venus. Read
> [`SYSTEM_CLASS_REFOCUS_2026_06_07.md`](SYSTEM_CLASS_REFOCUS_2026_06_07.md) first. Keep this file only as a
> record of DOD/VidPN/Code 43 findings.

Original title: Helios DOD — Phase 7.1b "Code 43 after present" — Handoff for ChatGPT Codex

**Goal:** make the Helios WDDM **Display-Only Driver (DOD)** bring a real desktop up on
the virtio-gpu scanout and KEEP it up (device stays at Problem Code 0). Today the desktop
*comes up* (commit + present fire, the adapter is reported as the primary 1024×768 display)
but **dxgkrnl tears the adapter down right after the first present → device Code 43**
(`CM_PROB_FAILED_POST_START`), silently (no event-log entry, no TDR, no bugcheck, no
restart). This doc is a complete, self-contained brief so you can continue without the
prior chat history.

> **Authoritative reference: VioGpuDod** — the inbox virtio-gpu Display-Only Driver from
> `virtio-win/kvm-guest-drivers-windows` (`viogpu/viogpudo/viogpudo.cpp` + `.h`). It is
> purpose-built for *this exact device* and works. Where our port (originally based on the
> generic Microsoft **KMDOD** framebuffer sample) diverges from VioGpuDod, **follow
> VioGpuDod.** Fetch it:
> `curl -fsSL https://raw.githubusercontent.com/virtio-win/kvm-guest-drivers-windows/master/viogpu/viogpudo/viogpudo.cpp`
> (and `viogpudo.h`). The whole `viogpu/viogpudo/` dir matters; `viogpudo.cpp` is ~4045 lines.

---

## 0. TL;DR of the current state (build oem27 / DriverVer 16.9.16.648, live on the standalone)

- Device binds, loads, **commits a VidPN and presents** at 1024×768; `[Screen]::AllScreens`
  shows our adapter as **`WinDisc 1024x768 primary=True`**.
- Then **Code 43**. Breadcrumb dump of the *current* build:
  ```
  HeliosCommit = 0x09000000   (CommitVidPn fired)
  HeliosVis    = 0x0C000001   (SetVidPnSourceVisibility, Visible=TRUE)
  HeliosPresent= 0x0D000000   (PresentDisplayOnly fired)
  HeliosTAdd   = 0x00000000   (pfnAddMode(target) OK, pivot enum)
  HeliosTAnp   = 0x00000000   (pfnAddMode(target) OK, no-pivot enum)
  HeliosSeq    = 0x87878CCC   (last 8 DDIs: …Enum(8),IsSupp(7),Enum(8),IsSupp(7),Enum(8),
                               SetVis(C),SetVis(C),SetVis(C))
  HeliosPost   = 0x0C000001   (last non-teardown DDI = SetVidPnSourceVisibility(Visible))
  HeliosStep   = 0x110000FF   (StopDevice ran to completion → then dxgkrnl Code 43)
  HeliosPiv    = 0xAB4D9AB4   (pivot cycle: TGT,SCALING,ROT,NOPIVOT,SRC,TGT,SCALING,ROT)
  ```
- So the teardown happens **after** Present + SetVidPnSourceVisibility(TRUE). In an
  *earlier* build the tail was an `EnumCofuncModality↔IsSupportedVidPn` loop
  (`HeliosSeq=0x87878787`); aligning the enum to VioGpuDod moved the tail to the
  visibility/present stage but did not stop the teardown.

---

## 0b. Environment, toolchain & how to build (the part the `win` MCP abstracted)

The prior agent built via a local **`win` MCP server** (`tools/win-mcp/`, a Rust stdio MCP
server). **Codex supports MCP (https://developers.openai.com/codex/mcp) — register and use this
server; it is by far the easiest path.** It exposes three tools: **`win_exec`** (run PowerShell
on the VM), **`win_cargo`** (mirror + cargo/cargo-make build), **`win_meson`** (Mesa ICD — not
needed here).

Register it with Codex (the prebuilt binary already exists at
`/home/rupansh/helios-vgpu/target/linux/release/win-mcp`; rebuild with
`CARGO_TARGET_DIR=target/linux cargo build --release -p win-mcp` if needed):

```toml
# ~/.codex/config.toml
[mcp_servers.win]
command = "/home/rupansh/helios-vgpu/target/linux/release/win-mcp"
args = []
```
(or `codex mcp add win -- /home/rupansh/helios-vgpu/target/linux/release/win-mcp`).

It shells out to `ssh win` (the `Host win` entry in `~/.ssh/config` → 192.168.122.120), so run
Codex on **this Linux host**. Tool args:
`win_exec{command, cwd?, env?, timeout_secs?}`;
`win_cargo{crate_dir, args, timeout_secs?}` e.g. `crate_dir="kmd",
args=["make","--makefile","Cargo.make.toml"]`.

If you'd rather not use the MCP, here is exactly what those tools do so you can replicate them
with raw `ssh win` PowerShell calls:

**Topology**
- Linux host holds the repo at `/home/rupansh/helios-vgpu`. It is shared into the Windows 11
  guest as the **`Z:\` drive** (same files). Edit on the Linux side; `Z:\` reflects it live.
- Windows 11 **build/dev VM** "win11": `ssh win` → **192.168.122.120**, user `rupansh`
  (`~/.ssh/config` has `Host win`). This VM is always up; it is where you BUILD and where you
  read device state. (It is a *different* VM from the standalone test qemu — see §1.)

**Toolchain already installed on the win11 VM** (verified; see `TOOLCHAIN.md`):
- VS 2022 Build Tools, "Desktop development with C++" (MSVC v143 + Spectre x64 libs).
- **WDK + SDK 10.0.26100.0** (a *complete* matched kit — `wdk-build` picks the highest kit).
- **LLVM 17.0.6** at `C:\Program Files\LLVM\bin` → set `LIBCLANG_PATH` to it for bindgen
  (LLVM 18 has a bindgen bug; do not use it).
- Rust **nightly** + `rust-src` + target `x86_64-pc-windows-msvc`; `cargo-make`.

**`win_exec(cmd)`** = run PowerShell on the VM: `ssh win powershell -EncodedCommand <base64 of
UTF-16LE script>` (the encoding just dodges cmd.exe quoting; `ssh win 'powershell -Command "…"'`
works too). Default cwd is `Z:\`. Use this for `pnputil`, registry reads, `Get-PnpDevice`, etc.

**`win_cargo(crate_dir, args)`** = build in a LOCAL-disk mirror, because **cargo/WDK artifact
writes FAIL on the `Z:\` 9p share** (`OS error 87`). It runs, over `ssh win` PowerShell:
```
robocopy Z:\ C:\Users\Rupansh\helios-vgpu /MIR /XD target .git "Z:\icd\mesa" /NFL /NDL /NJH /NJS /NP /R:1 /W:1
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
Set-Location C:\Users\Rupansh\helios-vgpu\<crate_dir>
cargo <args>
```
So: edit sources on Linux (`Z:\`), then mirror→build on `C:\Users\Rupansh\helios-vgpu`. The
build target dir is inside that mirror (local C:), which is why the driver package lands at
`C:\Users\Rupansh\helios-vgpu\kmd\target\debug\helios_kmd_package\`.

**Build the driver:** `crate_dir = "kmd"`, `args = ["make","--makefile","Cargo.make.toml"]`
(compiles + signs with the WDRLocalTestCert test cert + `inf2cat` + `infverif`). `args =
["build"]` is compile-only (fast, for iterating on compile errors). DriverVer is timestamp-based
so the newest build always out-ranks older staged packages.

(There is also `win_meson` for the Mesa venus ICD — irrelevant to this 2D DOD Code-43 task.)

## 1. The standalone TEST device & the exact test loop

- **Linux host** runs the Windows 11 (build 26100) guest. The guest IP is `192.168.122.120`.
- The project tree is shared into the guest as `Z:\`. **Windows builds CANNOT run on the
  share** (Rust IO fails, OS error 87); a build helper mirrors `Z:\` → `C:\Users\Rupansh\helios-vgpu`
  and builds there. In the prior chat this was the `win` MCP server (`win_cargo`, `win_exec`).
  For Codex: build via that mirror; run `pnputil`/registry reads over SSH to `.120`.
- **Test device = the STANDALONE qemu only.** The user launches
  `tools/launch-helios-gtk.sh` as their normal user with
  `HELIOS_GPU=plain` → a **`virtio-gpu-pci`** device (non-VGA, 2D, software `gtk` display, no
  GL/venus needed for this 2D bring-up). It keeps the VM at `.120`. Do not use alternate VM managers
  or XML/domain definitions for this target; they do not match the hardware expected by IDD + Helios.
- **Build + sign + infverif:** `cargo make --makefile Cargo.make.toml` in `kmd/` (DriverVer is
  timestamp-based, newest wins). `["build"]` alone = compile-only (fast).
- **Stage on the running guest** (driver auto-binds via the INX specific-hwid match):
  ```
  pnputil /add-driver <pkg>\helios_kmd.inf /install
  pnputil /delete-driver oemNN.inf /uninstall     # delete the PREVIOUS helios package so only newest remains
  ```
  Package dir: `C:\Users\Rupansh\helios-vgpu\kmd\target\debug\helios_kmd_package\`.
- **Reboot to load** (in-guest `shutdown /r` does NOT fire on this standalone). Use the QMP
  monitor `/tmp/helios-tpm/mon.sock` on the Linux host (python AF_UNIX: recv greeting →
  `{"execute":"qmp_capabilities"}` → `{"execute":"system_reset"}`). This reliably reproduces
  Code 43 (== cold boot). After ~60–75 s the guest is back; poll TCP `.120:22`.
- **Read result** over SSH:
  ```
  Get-PnpDevice | ? InstanceId -like "PCI\VEN_1AF4&DEV_1050*" → DEVPKEY_Device_ProblemCode
  Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd' | Select Helios*
  ```
  Clear the `Helios*` breadcrumb values before each run.
- **The user sees the gtk screen; you cannot.** Ask them to confirm what's on-screen.

---

## 2. The DOD source map (`kmd/src/`)

- `lib.rs` — `DriverEntry` builds the `KMDDOD_INITIALIZATION_DATA` DDI table and registers it
  via `DxgkInitializeDisplayOnlyDriver`. **Register the FULL table** (see §5 — trimming it is a
  regression).
- `dod.rs` — the DDI thunks (AddDevice/StartDevice/Stop/Remove, QueryAdapterInfo,
  QueryChild*, the VidPN DDIs as thin wrappers over `vidpn.rs`, PresentDisplayOnly, pointer,
  power, etc.) + `set_desktop_mode` (allocates the scanout fb + programs it via gpu.rs).
- `vidpn.rs` — the VidPN mode-management: `MODE_TABLE`, `video_signal_info`,
  `is_supported_vidpn`, `enum_vidpn_cofunc_modality`, `commit_vidpn`, `recommend_monitor_modes`,
  `add_single_source_mode`, `add_single_target_mode`. **This is where the bug almost certainly
  is.**
- `virtio/gpu.rs` — the virtio-gpu 2D transport: scanout (`CREATE_RESOURCE_2D`, `SET_SCANOUT`,
  `TRANSFER_TO_HOST_2D`, `RESOURCE_FLUSH`), `present_desktop`, `set_desktop_mode`, bounded
  virtio polls + the ISR ack. The present currently shows BLACK on-screen (see §9).
- `diag.rs` — TEMPORARY registry-breadcrumb tracer (see §3). Strip before final commit.
- `adapter.rs` — the WDF/dxgk adapter context (saved `DXGKRNL_INTERFACE`, virtio handle,
  `isr_status_va`).
- `.dod-vidpn-types.md` (untracked) — the exact bindgen FFI shapes for the VidPN structs
  (pfn signatures, field paths, `_<TAG>::VARIANT` enum forms). Read this before touching FFI.

---

## 3. The breadcrumb diagnostic system (`diag.rs`) — decoder

No kernel debugger is available, so each instrumented DDI writes a DWORD to
`HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd\Helios*`. **High byte = DDI id:**
`01`=AddDevice `02`=StartDevice `03`=QueryAdapterInfo|type `04`=QueryChildRelations
`05`=QueryChildStatus `06`=QueryDeviceDescriptor `07`=IsSupportedVidPn
`08`=EnumVidPnCofuncModality `09`=CommitVidPn `0A`=RecommendMonitorModes
`0B`=QueryVidPnHWCapability `0C`=SetVidPnSourceVisibility(low bit=Visible)
`0D`=PresentDisplayOnly `0E`=UpdateActiveVidPnPresentPath `0F`=RecommendFunctionalVidPn
`11`=StopDevice.

Values (all sticky / last-write-wins unless noted):
- `HeliosStep` — last DDI (ends at `0x110000FF` = StopDevice).
- `HeliosPost` — last **non-teardown** DDI (StopDevice uses `record_step_only`, which skips it).
- `HeliosSeq` — rolling last-8 DDI high-nibbles (`record()` shifts one in). The pre-teardown
  sequence.
- `HeliosPiv` — rolling last-8 EnumCofuncModality nibbles: low 3 bits = EnumPivotType
  (0=uninit 1=src 2=tgt 3=scaling 4=rotation 5=nopivot), **bit3 = "this enum called
  pfnUpdatePathSupportInfo"**.
- `HeliosEnum` / `HeliosEnumP` — last no-pivot / pivot enum detail: `0x0800_0000 |
  (pivotType<<20) | flags | paths`. flags: `0x100`=src-modeset-assigned
  `0x200`=tgt-assigned `0x400`=src-already-pinned `0x800`=tgt-already-pinned
  `0x4000`=src-pivot `0x8000`=tgt-pivot.
- `HeliosTAdd` / `HeliosTAnp` — last `pfnAddMode(target)` NTSTATUS in a pivot / no-pivot
  enum (`0` = OK).
- `HeliosSrcRes` — pinned source resolution seen during enum: `(w<<16)|h`.
- `HeliosAErr` / `HeliosErr` — last `pfnAssignTargetModeSet` / create-add-assign failure.
- `HeliosCommit` / `HeliosVis` / `HeliosPresent` — sticky markers when those DDIs fire.
- `HeliosQai` — set when dxgkrnl queries `DXGKQAITYPE_DISPLAY_DRIVERCAPS_EXTENSION` (type 16).
- `HeliosCmd` — the virtio cmd that stalled/failed a present (`(phase<<24)|(wedged<<20)|cmd`).

**ALL of `Helios*` and the `record_step_only`/`HeliosSeq`/`HeliosPiv`/`HeliosTAnp`/`HeliosQai`
machinery is TEMPORARY** — strip it (and the `diag.rs` temp surface) before a real commit.

---

## 4. The exact symptom, restated

`StartDevice → … → EnumCofuncModality↔IsSupportedVidPn (a few rounds) → CommitVidPn(0x09) →
SetVidPnSourceVisibility(Visible=TRUE, 0x0C…01) → PresentDisplayOnly(0x0D, returns SUCCESS,
scanout goes active, screen BLACK ~1 s) → [more EnumCofunc↔IsSupp and/or SetVidPnSourceVisibility]
→ StopDevice(0x11, runs to completion) → dxgkrnl sets CM_PROB_FAILED_POST_START (Code 43),
NO restart, NO TDR, NO event-log entry.`

dxgkrnl itself invalidates the device (via `IoInvalidateDeviceState` + `PNP_DEVICE_FAILED`)
*after* probing one present — i.e. it decided the adapter cannot sustain the desktop. The
exact internal reason is **not visible via breadcrumbs** → a kernel debugger (WinDbg/KDNET
over the network to the guest) is the surest way to read dxgkrnl's rejection reason.

---

## 5. Tried & RULED OUT (each built + tested on hardware; Code 43 unchanged)

- **`DXGKQAITYPE_DISPLAY_DRIVERCAPS_EXTENSION` / `VirtualModeSupport=1`** — dxgkrnl *does*
  query it (`HeliosQai` set) but answering it (we have the bindgen type) changed nothing. It's
  a start-time query, fires before present. (Reverted to `STATUS_NOT_SUPPORTED`.)
- **Changing registered DDIs' `NOT_IMPLEMENTED` returns to `SUCCESS`** (NotifyAcpiEvent /
  GetScanLine / ControlInterrupt / GetChildContainerId) — no effect on Code 43. (Kept anyway:
  a registered DDI should not return NOT_IMPLEMENTED.)
- **Proper `IsSupportedVidPn` validation** (KMDOD/VioGpuDod path-count check, replacing a
  blanket accept) — correct, kept, but didn't fix it.
- **Honoring the SCALING/ROTATION pivots** + **idempotent `UpdatePathSupportInfo`** — cleaned
  the modification pattern (HeliosPiv) but the loop/teardown persisted; later removed the
  idempotency to match VioGpuDod (which always updates and still converges).
- **Offering MORE modes** (MODE_TABLE 1→6, with per-mode preference) — all modes add cleanly,
  display still primary, Code 43 unchanged.

### Tried & REGRESSED — DO NOT repeat (each made it strictly worse: NO present at all)

- **Trimming the DDI table to KMDOD's exact 28-entry set** (leaving ControlInterrupt /
  GetScanLine / GetChildContainerId / NotifyAcpiEvent / ControlEtwLogging / SetPalette /
  CollectDbgInfo / QueryInterface / NotifySurpriseRemoval / Escape NULL) — this BREAKS the
  VidPN commit: target `pfnAddMode` then fails `STATUS_GRAPHICS_INVALID_FREQUENCY (0xC01E030A)`
  in *every* enum context, so nothing commits. It only *masked* Code 43 (no present → no
  post-present teardown). **The FULL DDI table is load-bearing.** (Mechanism unknown but
  rock-solid empirically.)
- **`VOT_INTERNAL`** instead of `VOT_HD15` for the child InterfaceTechnology — dxgkrnl then
  never even enumerates the VidPN. VioGpuDod uses `VOT_INTERNAL` only for VGA devices; for
  non-VGA it uses **`VOT_HD15`** (which we now match).
- **Coupling `QueryChildStatus.Connected` to "virtio transport bound"** — dxgkrnl queries child
  status BEFORE StartDevice binds virtio → reported Connected=0 → monitor marked permanently
  disconnected → no enum. VioGpuDod uses `IsDriverActive()` (a post-StartDevice driver-state
  flag, NOT transport binding) — we currently hardcode `Connected=1` which is fine.

---

## 6. Confirmed facts / load-bearing settings (don't change these blindly)

- **FULL DDI table** required (trim breaks commit — see §5).
- **CONCRETE VESA timing** required for the target/monitor `VideoSignalInfo` on this NON-VGA
  win11-26100 device. **NOTSPECIFIED freqs (what VioGpuDod's `BuildVideoSignalInfo` uses) are
  REJECTED here by `pfnAddMode(target)` with `0xC01E030A`** — reproduced with ActiveSize both 0
  and (w,h). VioGpuDod gets away with NOTSPECIFIED because its modes come from the POST
  framebuffer on the **VGA** path (`IsVgaDevice()`); our non-VGA path has no POST mode, so we
  synthesize concrete timing. `kmd/src/vidpn.rs::video_signal_info` currently uses a
  self-consistent 60 Hz raster (PixelRate = HTotal·VTotal·VSync) and `pfnAddMode` accepts it.
- **`VOT_HD15` + `HpdAwarenessInterruptible`** for the non-VGA child (matches VioGpuDod).
- `Connected = 1` hardcoded is fine while running.
- Standalone test device = **`virtio-gpu-pci`** (non-VGA), software gtk display.

---

## 7. The most likely remaining cause — VioGpuDod's per-source state machine (PRIMARY lead)

Our `commit_vidpn` / `set_vidpn_source_visibility` / `present_display_only` are thin and do
**not** maintain VioGpuDod's `m_CurrentMode` flag state machine. VioGpuDod:

- **`CommitVidPn`** → validates `IsVidPnSourceModeFieldsValid` + `IsVidPnPathFieldsValid`
  per path, then `SetSourceModeAndPath` → `SetCurrentMode` (actually programs the scanout for
  the *committed* source mode, matched by resolution against the HW mode list) and sets
  `m_CurrentMode.Flags.FullscreenPresent = TRUE`.
- **`SetVidPnSourceVisibility`** → `Visible`: `m_CurrentMode.Flags.FullscreenPresent = TRUE`;
  not visible: `BlackOutScreen(&m_CurrentMode)`. Always sets
  `m_CurrentMode.Flags.SourceNotVisible = !Visible`.
- **`PresentDisplayOnly`** → early-returns `STATUS_SUCCESS` if `SourceNotVisible`;
  early-returns **`STATUS_UNSUCCESSFUL` if `!FrameBufferIsActive`**; otherwise blts.
- **`QueryVidPnHWCapability`** → sets `DriverRotation = 1`, `DriverColorConvert = 1` (we return
  all-zero caps). We advertise rotation *support* in the path (Identity+Rotate90) yet report
  `DriverRotation = 0` — an **inconsistency** that may be exactly what dxgkrnl rejects
  post-present. **Try this first — it is a one-line, low-risk change.**

Concretely, the `FrameBufferIsActive` flag: in VioGpuDod the present is **conditional on the
commit having realized the source** (`CreateFrameBufferObj` / `SetCurrentMode` sets the flag).
Our present always blts off a StartDevice-time scanout. dxgkrnl's post-present validation may
require this per-source realize-on-commit contract.

### Ranked next steps for Codex

0. **TRIED, did NOT fix Code 43:** `QueryVidPnHWCapability` set to `DriverRotation=1` +
   `DriverColorConvert=1` (kept — it's correct/VioGpuDod-aligned). So the cap inconsistency was
   not the trigger; the lead is now #1 below.
1. **Port VioGpuDod's `m_CurrentMode` state machine (PRIMARY lead)**: add `FrameBufferIsActive` /
   `SourceNotVisible` / `FullscreenPresent`; make `CommitVidPn` validate the path/source fields
   (`IsVidPnPathFieldsValid` / `IsVidPnSourceModeFieldsValid` — already ported as
   `is_supported_vidpn`'s would-be helpers; VioGpuDod's exact checks are in §"validators"
   below) and *realize the source* (program the scanout for the committed mode) + set
   `FrameBufferIsActive=TRUE`; gate `PresentDisplayOnly` on those flags;
   `SetVidPnSourceVisibility` updates `SourceNotVisible` + `BlackOutScreen`. This is the
   biggest structural gap vs VioGpuDod and the most likely fix.
2. **Kernel debugger (WinDbg/KDNET over the network)** — attach to the guest, break on the
   dxgkrnl path that sets `PNP_DEVICE_FAILED` after the present, and read the actual rejection
   reason. Breadcrumbs are exhausted; this is the definitive route.
3. **The present shows BLACK** — verify `virtio/gpu.rs::present_desktop`
   (`TRANSFER_TO_HOST_2D` rect + `RESOURCE_FLUSH` + the scanout binding) actually pushes the
   blt'd content. If dxgkrnl/compositor sees no valid output it may re-negotiate then give up.

**VioGpuDod's field validators (port verbatim into CommitVidPn):**
- `IsVidPnPathFieldsValid`: `VidPnSourceId < MAX_VIEWS`; `VidPnTargetId < MAX_CHILDREN`;
  `GammaRamp.Type == D3DDDI_GAMMARAMP_DEFAULT`; Scaling ∈ {IDENTITY, CENTERED, NOTSPECIFIED,
  UNINITIALIZED}; Rotation ∈ {IDENTITY, ROTATE90, NOTSPECIFIED, UNINITIALIZED};
  `VidPnTargetColorBasis ∈ {CB_SCRGB, CB_UNINITIALIZED}`.
- `IsVidPnSourceModeFieldsValid`: `Type == RMT_GRAPHICS`; `ColorBasis ∈ {CB_SCRGB,
  CB_UNINITIALIZED}`; `PixelValueAccessMode == PVAM_DIRECT`; `PixelFormat == A8R8G8B8`.

---

## 8. Open mystery worth resolving

VioGpuDod uses NOTSPECIFIED freqs and ships/works on virtio-gpu, but NOTSPECIFIED is rejected
on our **non-VGA** path (`0xC01E030A`). The working path for Helios remains the standalone
launcher and its non-VGA virtio-gpu device. Concrete timing side-steps the rejection and lets
the desktop commit; revisit the exact timing contract only if the display path regresses.

---

## 9. Current working-tree state (uncommitted; build oem27 / 16.9.16.648)

Touched (vs HEAD `b9b1e81`): `kmd/src/{lib,dod,vidpn,diag,adapter,virtio/gpu}.rs`,
`protocol/src/features.rs`, `kmd/helios_kmd.inx`, `tools/launch-helios-gtk.sh`; untracked
`.dod-vidpn-types.md`. The display **comes up** (commit+present, primary 1024×768) then Code
43. This is the best functional state reached. The `diag.rs` breadcrumb surface
(HeliosSeq/HeliosPiv/HeliosPost/HeliosTAnp/HeliosQai + `record_step_only`) is TEMPORARY and
should be stripped once Code 43 is fixed. The relevant project memory is
`~/.claude/projects/-home-rupansh-helios-vgpu/memory/phase71b-commit-fixed.md` (entries
LATEST-4..6 + this VioGpuDod session).
