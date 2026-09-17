# AGENTS.md — Primary Implementor Instructions

## Project: Helios vGPU — Windows WDDM render+display driver over virtio-gpu/Venus

You are the **primary author** of this project. The human overseer has OS/driver/Rust expertise and
will review your work, but you must drive all implementation decisions, write all code, and flag
blockers proactively.

**What Helios is (2026-08):** a Windows 11 guest graphics stack for QEMU/KVM on a Linux host.
A **WDDM render+display miniport** (`kmd_render/`, Rust) binds the virtio-gpu PCI device
(PCI\VEN_1AF4&DEV_1050) and speaks **Venus** (Vulkan serialization) to the host's virglrenderer
render server; a **D3D11 UMD** (`umd/`, Rust d3d10umddi frontend bridged via cxx to a forked
**DXVK** engine at `dxvk-helios/`) gives dwm and apps D3D11 on top of the Mesa Venus ICD
(`icd/mesa` fork). Helios owns a real VidPn source and sends DWM's shared primary through
`SET_SCANOUT_BLOB` to the in-tree **`qemu-helios/` fork**, normally displayed by
`egl-headless` + VNC. IddCx/Looking Glass and the older System-class KMDF + DeviceIoControl
driver (`kmd/`) remain historical/reference paths, not the active display.

⚠ The driver declares `WddmSurface::Wddm2_1GpuMmu`, not 3.2 — see
`kmd_render/src/ddi/wddm_surface.rs`, which records that 3.2 fails DWM at `E_NOTIMPL`. Older
docs and comments that say "WDDM 3.2" are describing the intent, not the surface.

## Stage: Correctness and D3D12 — since 2026-08-05

The hardware-accelerated desktop milestone is met and the performance push is **paused, not
abandoned**: DWM composites the whole desktop on Helios → Venus → host GPU → virtio-gpu scanout,
and Fire Strike runs at GT1 ≈ 221 / GT2 ≈ 208 / Graphics ≈ 49k with the present-queue stall
root-caused and fixed (ROADMAP WS2). The remaining performance limit is **named and measured** —
the frame's own producer completion on the host, at a producer floor of ~3.7 ms/frame — so more
perf work needs a new lever, not another sweep. The charter is now, in priority order:

1. **D3D11 correctness / conformance** — `CONFORMANCE.md` is the charter. Drive the UMD's
   `DDI refusals:` counters and the noop-DDI hit counters to zero against real workloads, turn
   the ~40 ad-hoc probes in `tools/` into a runnable suite with pass criteria, close DXGI format
   coverage and the remaining 11.1 DDI plumbing.
2. **D3D12** — `DX12.md` is the charter, `docs/dx12/` the implementation set. **The strategy
   question is CLOSED (2026-08-05):** Helios ships a real D3D12 UMD, `helios_umd12.dll`,
   implementing `d3d12umddi` and forwarding into vkd3d-proton's `ID3D12*` COM objects — the D3D11
   architecture with DXVK swapped for vkd3d and `UserModeDriverName[2]` for `[3]`. The app-local
   vkd3d arm is Phase 0 of that plan, not an alternative. ⭐ **Stage S5 HAS LANDED** (the older
   entry here said `OpenAdapter12` "still refuses and must keep refusing until the commit that
   makes its body reachable" — that commit is in): the INF registers `UserModeDriverName[3]`,
   `umd`'s duplicate `OpenAdapter12` export is gone (`umd/src/adapter.rs`), and
   `adapter12::OpenAdapter12`'s body is reachable behind the `UmdD3D12` kill switch
   (`umd12/src/knobs12.rs`). **Absent = ON as of the owner's 2026-09-07 default change**;
   explicit `UmdD3D12=0` preserves the disable for new processes, including dwm.
   The enabled .270 stack passed the four native ordering cases and completed
   Time Spy/Fire Strike; broader acceptance limits remain in ROADMAP.md.
   Verified 2026-09-05 with `UmdD3D12=1`:
   `OpenAdapter12=0` refusals and a real `CreateDevice` in `umd12-<pid>.log`.
3. **Stability** — unchanged and still non-negotiable: buffer rotation, resize, suspend/resume,
   device restart, cold boot, DWM recovery, TDR. No hacks; loud failure over fake success.
4. **Performance** — paused. Do not open a perf sweep without a new causal hypothesis; ROADMAP
   WS2 lists what has already been measured and rejected, with numbers.

**`ROADMAP.md` is the living stage document** — current defect list, per-workstream plans, and the
tooling inventory (registry knobs, counters, ETW recipes, guest probe schtasks). Update
it as items close or appear. Session-by-session state lives in the agent memory; do not create
per-session HANDOFF_*.md docs — distill into memory + ROADMAP.md.

**Owner update, 2026-09-08:** the old mandatory multi-agent review loop, fixed lane ownership
and two-dry-round deployment requirement are retired. Use review and validation appropriate to
the concrete change. Historical review instructions and source-comment citations do not revive
that workflow. Preserve the architecture, synchronization invariants and evidence requirements.

**Owner update, 2026-09-09:** virglrenderer forks are authorized. The paired
`virglrenderer` and `venus-protocol` submodules plus Mesa provide native DGC;
the private vkd3d indirect emulation and `HELIOS_RETIRE_FEEDBACK` workaround are
removed. See `docs/dx12/NATIVE_DGC.md` for the new wire-completion contract,
host query discrepancy, local renderer build and owner-operated QEMU restart.
WDDM2.1, the native static UMD and async WSI remain. Guest reboots are already
authorized. Do not update memory unless explicitly requested.

---

## ⚠️ VERY IMPORTANT: `CARGO_TARGET_DIR`

The Linux host and the Windows VM (`win11`) **share the same source tree** (the Linux project dir
is the VM's `Z:\` drive) but use different toolchains and produce incompatible artifacts. Set
`CARGO_TARGET_DIR` per platform — and on Windows it MUST point at **local disk**, never the share:

- **Linux:** `CARGO_TARGET_DIR=target/linux` (native Linux fs).
- **Windows:** a **local C: path** — NOT `Z:\...`. Rust/cargo file IO **fails on the `Z:\`
  9p/virtio share**: `OS error 87` (windows-drivers-rs#481).

Set this via the environment on each cargo invocation. Do **NOT** commit `target-dir` in
`.cargo/config.toml` — that file is read on both platforms.

## ⚠ Toolchain floor: bindgen 0.72, because libclang is 22.1.8

Every bindgen in the tree is **0.72** and must stay there. Under libclang 22, bindgen 0.70/0.71
bind the FORWARD DECLARATION instead of the definition for a struct declared before it is
defined, emitting `pub _address: u8` (size 1) *beside* the real layout assertion — so a bump
backwards fails as ~300 errors in `umd` and 42 in `wdk-sys`, in two shapes at once:

```
error[E0609]: no field `pfnCalcPrivateResourceSize` on type `&mut D3D11DDI_DEVICEFUNCS`
error[E0080]: attempt to compute `1_usize - 1200_usize`, which would overflow
```

⛔ **Never "fix" that by disabling bindgen's layout tests.** Those assertions are the only reason
1-byte `_IRP` / `_DEVICE_OBJECT` / `_KDPC` were a build failure instead of a running driver —
suppressing them is exactly the fake success rule 2 forbids, in the component that bugchecks.

`kmd_render` cannot bump bindgen alone: it must match the version `wdk-build` uses, so
`Builder::wdk_default` extends the same `bindgen::Builder` type. Published `wdk-build 0.5.1` caps
at `bindgen ^0.71`, so the wdk crates are **pinned to an upstream git rev** that already carries
0.72.1. Return them to crates.io the day a `wdk-build > 0.5.1` ships.

**Driving either Windows host:** `tools/win-mcp` is both the **`win` MCP server**
(`win_exec`, `win_cargo`, `win_vkd3d`, `win_meson`, `win_build_kmd`,
`win_install_kmd`, `win_install_umd`, `win_looking_glass*`) and a CLI over the
same code — `win-mcp --cli <command>`. The CLI exists for CI, humans and
shell-only agents, and the generic `win_host_*` tools/commands work on **both**
the dev VM and the `firstheberg2-win` build slave: `status` (one-shot "what is
actually running", which is what to run first in a session), `preflight`,
`hostinfo`, `run`, `run-script`, `task` (detached work), and verified `push` /
`pull`. `purpose` (`build` / `install` / `desktop` / `system`) selects the
session and privilege rules and is part of correctness: `desktop` refuses session
0 because a GPU probe there reports plausible but fake results, `build` runs as
SYSTEM with `safe.directory` and a PATH that cannot pick up MSYS2's git, and
`install` runs as SYSTEM so a console control event cannot interrupt a display
driver swap. Read `tools/win-mcp/README.md` before writing another ssh/task
wrapper — long work belongs in `run-script --task NAME` (an ssh drop kills a
synchronous remote process) and every transfer is sha256-verified on both ends.

The VM-specific tools remain:

`win_exec`, `win_cargo` (mirrors `Z:\` to
`C:\Users\Rupansh\helios-vgpu` and sets the local target dir + `LIBCLANG_PATH`),
`win_build_kmd` + `win_install_kmd` (the KMD build/sign/deploy path), `win_install_umd`,
`win_dxvk` (the DXVK engine), `win_meson` (Mesa ICD), and the historical
`win_looking_glass`/`win_looking_glass_idd`. coreutils are installed on
win11. SSH/win_exec land in **session 0** — window/desktop probes and every benchmark must run
via scheduled tasks (`schtasks /run /tn <name>`; a 3DMark run launched from session 0 fakes a
driver regression). See TOOLCHAIN.md and ROADMAP.md tooling.

**Standing VM authorization (owner directive, 2026-09-12):** start, stop, restart,
cold-boot or reboot the test VM and build slave whenever needed for Helios work.
This includes changing and relaunching the VM, its QEMU
display/debug transport and its environment variables — which on this host means the
**WinBoat container** (see "Test environment" below); the retired
`tools/launch-helios-gtk.sh` bare-GTK launcher is dead. Do not pause for
approval or require the owner to be present; the owner is often AFK. This
authorization supersedes older approval requirements in the project docs.
Document launch changes, report disruptive restarts, and verify guest health and
loaded driver versions afterward. `pnputil /restart-device` re-runs AddAdapter
when a full guest reboot is unnecessary.

---

## Test environment

⚠ **The test VM is contributor-specific, and on this host it is a container.** Helios itself is
*part of* WinBoat upstream, but WinBoat is only one way to run the guest — other contributors
run their own QEMU/KVM VM, and the driver does not depend on WinBoat. So: environment facts
belong in this section, not in code, scripts or probe arguments, and nothing here should be
assumed to hold on someone else's machine. This snapshot is what this host is actually running
(written after an agent assumed a bare QEMU process and lost time on 2026-09-13).

| | |
|---|---|
| Container | `WinBoat`, compose project `winboat`, image `ghcr.io/winboat-org/helios-windows:6.03.8` (dockur/windows-derived Win11) |
| Compose file | `~/.local/share/winboat-app/docker-compose.yml` — **this is the launch lever** |
| Passthrough | `/dev/kvm` and `/dev/dri/renderD128` (`RENDERNODE`); `privileged: true`, `cap_add: NET_ADMIN` |
| Host stack | QEMU fork + `/opt/helios/libexec/virgl_render_server` run **inside the container** (`LD_LIBRARY_PATH=/opt/helios/lib`, image-provided, so host-stack changes need an image rebuild) |
| Volumes | repo → `/shared`; `/home/tibix/Data/WinboatGPUInstall/winboat` → `/storage` (the guest disk, `data.img`); `./oem` → `/oem` |
| Access | `ssh win` = `127.0.0.1:2222`; VNC-web `47270`; RDP `47273`; QMP `47272` (in-container `7149`) for reset/screendump |
| QEMU args | set by the `ARGUMENTS` env (e.g. `-qmp …`); `virtio-gpu-gl-pci,venus=on,blob=on,hostmem=5G,host3d_blob_limit=5G` comes from the Helios image entrypoint |
| VM knobs | compose env: `RAM_SIZE`, `CPU_CORES`, `HELIOS_HOSTMEM`, `HELIOS_BLOB_LIMIT`, `VKR_DEVICE_MEMORY_LIMIT_BYTES`, `override_vram_size`, `LOSSY` |
| Host logs | **`docker logs WinBoat`** (QEMU stderr + render-server output). `docker exec WinBoat ps aux` shows QEMU and the render servers |
| Lifecycle | `restart: no`. A guest shutdown/power-off leaves the container exited: **`docker start WinBoat`** resumes it (this is why a guest reboot can end with the VM down — `ROADMAP.md` records the same trap) |

⚠ Running `tools/launch-helios-gtk.sh` (or expecting `/tmp/helios-qemu-stderr.log`) targets
the retired bare-GTK environment and will not describe what is actually running.

## Operating Rules

1. **Read the relevant doc/spec before writing code** in a subsystem. For WDDM DDI surfaces use
   the WDK bindings (`kmd_render` bindgen) and verify struct shapes; for Venus protocol,
   `venus-protocol/vk.xml` and `virglrenderer/src/venus/` are ground truth.
2. **Never stub silently.** Mark stubs `// STUB: reason`; return documented error codes. Every
   skipped/refused path gets a named registry counter or atomic — loud failure over fake success.
3. **Prefer explicit over clever.** Kernel code has zero tolerance for bugs.
4. **All unsafe blocks carry a `// SAFETY:` comment** stating the invariant.
5. **Scoped commits** — one topic per commit.
6. **Evidence discipline:** only user-visible/screenshot desktop state counts as rendering
   evidence (`helios_paintcap` → `Z:\tmp\screen_copy.png` is ground truth); log lines are not
   frames. Registry counter values persist across boots — verify a counter *moves* this boot
   before trusting it. Never blame the host stack (proven good) without host-side evidence.
7. **Measure before optimizing:** add/read counters, ETW, or timestamps first; land perf
   changes with before/after numbers. GT1 drifts across a session, so an all-A-then-all-B
   comparison cannot separate a knob from the drift — interleave the arms
   (`tmp/perf/ab-presentwmk.ps1`, `ab-env.ps1`) and report paired deltas.
8. **A knob's default is a decision, and it must match the measured configuration.** If every
   accepted measurement was taken with a value the code does not default to, the code is
   shipping something nobody measured. Flipping a default requires the evidence in the comment
   at the read site, and the opposite value must remain reachable as the A/B disable.

## Repository Structure (active paths)

```
helios-vgpu/
├── AGENTS.md               ← You are here
├── ROADMAP.md              ← living stage doc: defects, per-workstream plans, tooling
├── CONFORMANCE.md          ← D3D11 correctness charter (priority 1)
├── DX12.md                 ← D3D12 charter (priority 2) — decision, phases, checkpoints
├── TRANSPORT.md            ← virtio-gpu + Venus wire format. §1/§2 LIVE; §3/§7 archived
├── HOST.md TOOLCHAIN.md    ← Linux host setup / cross-platform build + deploy
├── NTOSEYE.md              ← Windows KD (ntoseye) quirks
├── BRINGUP_QUIRKS.md       ← build/deploy/VM-control gotchas (purge-fingerprint,
│                             repackage+sign, DriverStore, QMP reset, diag ring)
├── HELIOS_DRIVER_DEPLOYMENT.md
├── WINDOWS_CI_PACKAGE.md   ← the GH Actions bundle + Install/Verify-Helios.ps1
├── docs/archive/           ← Frozen history. Read-only; code comments may cite by
│                             name. ⭐ ROADMAP_HISTORY_THROUGH_2026-09-05.md — the
│                             4,472-line ROADMAP verbatim, before it was rebuilt lean
│                             on 2026-09-05; every WS number and defect id still
│                             resolves there. ARCH/OVERVIEW/KMD/ICD (the System-class stack),
│                             WINDOWED_BLT_DESIGN, SCANOUT_DRM_MODIFIER_DESIGN, the
│                             GATE*/WDDM_*/DISPLAY*/PHASE*/HANDOFF_* corpus, and
│                             REFACTOR_* (the completed T0–T8 quality refactor).
├── docs/dx12/              ← D3D12 implementation doc set. DECISIONS.md
│                             remains authoritative for ARCHITECTURE (nothing may
│                             contradict it); ARCHITECTURE (the UMD split: umd_common +
│                             umd12 + the vkd3d bridge), DDI_REFERENCE (the d3d12umddi
│                             contract, reconstructed — MS does not document it), PRESENT,
│                             SUBSTRATE, KMD_IMPACT, GATES (D12-G0..G11, acceptance only),
│                             research/ (12 dossiers)
├── docs/reference/         ← Non-narrative reference data (host vulkaninfo profile)
│
├── kmd_render/             ← ACTIVE: WDDM render+display miniport (Rust, no_std)
│   └── src/ddi/            ← DDI surface (query_adapter_info = caps/segments,
│                             create_allocation, cpu_host_aperture, build_paging_buffer,
│                             escape, submit_command/scheduler, interrupt, display,
│                             vidpn, present_packet, scanout_timeline/scanout_trace)
│   └── src/virtio/         ← virtio-gpu transport + async ctrl (C3/M3.4) + venus client
├── kmd_logic/              ← ACTIVE: the KMD's testable pure logic. `kmd_render` is a
│                             no_std cdylib with panic=abort and CANNOT host a libtest
│                             harness — new KMD unit tests belong HERE, and this is the
│                             only KMD code with tests that actually run.
├── umd/                    ← ACTIVE: D3D11 UMD (d3d10umddi frontend, cxx bridge)
├── dxvk-helios/            ← ACTIVE: forked DXVK engine (venus import model, GDI staging)
├── icd/mesa                ← ACTIVE: Mesa fork — Venus Vulkan ICD (build via win_meson)
├── qemu-helios/            ← ACTIVE: QEMU fork — modifier metadata + native OPTIMAL readback
├── protocol/               ← shared guest/host wire structs (builds on BOTH platforms)
├── tools/                  ← launcher, deploy scripts, ~40 D3D11/DXGI/D3DKMT probes,
│                             gate scripts, and the `win` MCP server (tools/win-mcp)
├── packaging/windows/      ← Install-Helios.ps1 / Verify-Helios.ps1 + the four smoke
│                             probes — the payload the installer embeds
├── installer/              ← ACTIVE: the self-contained Rust Windows installer (GUI +
│                             CLI). Embeds the packaging/windows payload, appended to its
│                             own PE image by `--bundle` in ci/windows/Assemble-Package.ps1
├── ci/ + .github/workflows ← the Windows graphics+compute bundle build
├── vkd3d-proton-helios/    ← submodule, forked off upstream 2c7ba22c. ⛔⛔ **DO NOT WRITE THE
│                             CURRENT SHA HERE.** This line has named a stale SHA three times,
│                             twice in one session (2c7ba22c "zero divergence" → 8ee4440b →
│                             4c26d855 → …), and each time a lane reasoned from it. A doc line
│                             naming a moving pointer is stale by construction; read it with
│                             `git ls-files -s vkd3d-proton-helios` and `git -C
│                             vkd3d-proton-helios log --oneline 2c7ba22c..HEAD`, which cannot
│                             go stale. ⛔ NOT zero divergence. ⛔⛔ AND NEITHER IS A COUNT:
│                             this entry said "the 6th is the first that is NOT plumbing" and
│                             went stale INSIDE the changeset that wrote it — there are 7, and
│                             the 7th is not plumbing either. That is the FOURTH time this
│                             line has been stale. ⇒ do not write a count here either; run
│                             `git -C vkd3d-proton-helios log --oneline 2c7ba22c..HEAD`.
│                             The SHAPE, which does not go stale: the early commits are build
│                             plumbing (libs/d3d12core/{helios_entry.c,debug_control.c,
│                             debug_control.h,helios_vkd3d.def,meson.build,main.c},
│                             tests/test-runner.sh); after them come the real divergences —
│                             ID3D12DXVKInteropDevice4::GetVulkanResourceMemoryInfo
│                             (include/vkd3d_device_vkd3d_ext.idl, libs/vkd3d/{device.c,
│                             device_vkd3d_ext.c,vkd3d_private.h}), the resource-level
│                             sibling of GetVulkanHeapInfo that the D3D12 present path
│                             needs; and VKD3D_HEAP_FLAG_HELIOS_VENUS_EXPORT, a PRIVATE
│                             D3D12_HEAP_FLAGS bit (vkd3d_private.h) with an export-memory
│                             chain in resource.c. ⛔ That bit's value is HAND-MIRRORED in
│                             umd12/src/forward12/resource12.rs across two repositories with
│                             NO compile-time check — the highest-risk divergence in the fork
│                             and the one a lane reading only the old entry would miss.
│                             ⭐ The fork BUILDS NATIVELY ON LINUX, tests included
│                             (widl/meson/ninja/glslang all present) — so vkd3d changes are
│                             verifiable on the host with no VM and no WDK. ⚠ But its nested
│                             submodules are a WORKING-COPY property, not a repo one: they
│                             were NOT initialised in this checkout on 2026-09-05 and meson
│                             will not configure without them. Run `git submodule update
│                             --init --recursive` inside the submodule and check, rather than
│                             trusting either this line or the older "nothing builds" one.
│                             (dxvk-helios needs the same.) The D3D12 engine; see
│                             docs/dx12/SUBSTRATE.md
├── LookingGlass/           ← HISTORICAL: former IddCx capture path. Retained only
│                             because tools/win-mcp still implements win_looking_glass*
└── kmd/                    ← ARCHIVED reference: System-class KMDF + IOCTL stack. Kept
                              because active kmd_render code cites it for provenance
                              ("Ported from kmd/src/…"); it is in no build.
```

⚠ Two crates were retired on 2026-08-05 — `probe/` and `host/`. Both were orphans (no
workspace, no CI, no build, cited only by docs that had already been archived). Do not
re-create them; if you need a host-side or user-mode probe, add it under `tools/`.

## Key Invariants (never violate)

| Rule | Why |
|------|-----|
| No pageable code / diag::record (registry writes) above PASSIVE; IRQL-gate anything that round-trips | BSOD / silent deadlock |
| Never allocate or spin-wait in ISR/DPC paths; PASSIVE waits only via the async ctrl plumbing | the 0x7F / DISPATCH-spin lessons |
| Validate every runtime/guest-supplied size & offset before reading (per-arm, not max-union) | the RenderGdi ~48% drop bug |
| A panic in any DDI = silent graphics deadlock — return errors, count, never `panic!`/`todo!` in release paths | proven repeatedly |
| Blob window offsets below the VidMm/CpuHostAperture reserve belong to dxgkrnl — never recycle them in the KMD allocator | host subregion overlap |
| A SupportsCpuHostAperture segment must be the LAST reported segment; classic CpuVisible memory segments are rejected | AddAdapter Code 43 (ETW-proven 2026-07-05) |
| The KMD version lives at ONE site, `kmd_render/driver-version.env`; never reintroduce a literal into build.rs or Cargo.make stampinf | INF/FILEVERSION mismatch = FAILED_ADD 0xc0000182 |
| Venus commands flush before fence signal; never signal a wire fence before host completion | suspected root of DEVICE_LOST/freeze (stability WS1) |
| A WDDM fence may wait on the frame's OWN boundary, never on the whole `next_wire_fence` backlog | the superset delayed the fence by the pipeline depth and stalled dxgkrnl's 3-deep present queue (WS2, `PresentWmk`) |
| New KMD unit tests go in `kmd_logic`, never in `kmd_render` | `kmd_render` is a `panic=abort` no_std cdylib: a `#[cfg(test)]` module there can never run, so it is assurance that is not real |

## When You're Stuck

1. `ROADMAP.md` (stage state) → `BRINGUP_QUIRKS.md` (deploy/VM) → `NTOSEYE.md` (live KD).
2. dxgkrnl failure reasons in plain text: ETW `Microsoft-Windows-DxgKrnl` all-keywords trace →
   tracerpt → grep `AzureTriage` (recipe in ROADMAP.md). The SAME provider answers
   "what is dxgkrnl doing to my thread" — take a ~2 s circular slice mid-run and read the
   `Present` / `Flip` / `QueuePacket` / `DmaPacket` / `BlockThread` events; that is how the
   present-queue stall was found (ROADMAP WS2).
3. Venus protocol ground truth: `venus-protocol/vk.xml`, `virglrenderer/src/venus/`. ⛔
   **Host-side logs are `docker logs WinBoat`** — the QEMU fork and
   `/opt/helios/libexec/virgl_render_server` run INSIDE the WinBoat container, so there is no
   `/tmp/helios-qemu-stderr.log` in this environment (verified 2026-09-13; that path is a
   leftover of the retired bare-GTK launcher). `HELIOS_VKR_DEBUG=validate` enables host
   validation layers.
4. Reference drivers: mvisor-win-vgpu-driver (System-class model), kvm-guest-drivers-windows
   viogpu (virtio init only).
5. Ask the overseer on fundamental architecture questions.

## Files Not to Touch

- `*.inx` — only with explicit instruction (active shape: WDDM render miniport INF).
- `docs/archive/**` — frozen history; do not edit, do not resurrect into the live tree.
  ⚠ Its files still cite this document by its old name, `CLAUDE.md` (renamed to
  `AGENTS.md` on 2026-09-05). That is correct — they record what was true when frozen.
  A tree-wide rename must exclude `docs/archive/`; note that this repo's `grep` is
  `ugrep`, which does **not** prefix results with `./`, so a `^\./docs/archive/` filter
  silently matches nothing and rewrites the archive.

## Code Style

```rust
// Kernel-mode code: no_std, no panics in release, wdk-sys / bindgen types directly.
// Pattern for DDI handlers:
//  - null-check args, validate every runtime-supplied length per-arm
//  - do the work; every skip/refusal increments a named counter
//  - errors -> documented NTSTATUS from the DDI's legal return set
//    (an illegal NTSTATUS is itself logged by dxgkrnl as a driver bug)
```
