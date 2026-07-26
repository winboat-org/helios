# CLAUDE.md — Primary Implementor Instructions

## Project: Helios vGPU — Windows WDDM render+display driver over virtio-gpu/Venus

You are the **primary author** of this project. The human overseer has OS/driver/Rust expertise and
will review your work, but you must drive all implementation decisions, write all code, and flag
blockers proactively.

**What Helios is (2026-07):** a Windows 11 guest graphics stack for QEMU/KVM on a Linux host.
A **WDDM 3.2 render+display miniport** (`kmd_render/`, Rust) binds the virtio-gpu PCI device
(PCI\VEN_1AF4&DEV_1050) and speaks **Venus** (Vulkan serialization) to the host's virglrenderer
render server; a **D3D11 UMD** (`umd/`, Rust d3d10umddi frontend bridged via cxx to a forked
**DXVK** engine at `dxvk-helios/`) gives dwm and apps D3D11 on top of the Mesa Venus ICD
(`icd/mesa` fork). Helios owns a real VidPn source and sends DWM's shared primary through
`SET_SCANOUT_BLOB` to the in-tree **`qemu-helios/` fork**, normally displayed by
`egl-headless` + VNC. IddCx/Looking Glass and the older System-class KMDF + DeviceIoControl
driver (`kmd/`, hand-written `icd/src`) remain historical/reference paths, not the active display.

## Stage: Performance, Stability, Conformance (PSC) — since 2026-07-05

The hardware-accelerated desktop milestone is met: DWM composites the whole desktop on Helios →
Venus → host GPU → virtio-gpu scanout. The direct primary is visible through VNC on the
current KMD/QEMU stack. The stage's charter, in priority order:

1. **Stability** — direct-primary buffer rotation, resize, suspend/resume, device restart,
   cold boot, DWM recovery, and TDR contracts. No hacks; loud failure over fake success.
2. **Performance** — measure present-to-scanout and VNC delivery separately. The current DComp
   probe sustains about 63 fps, but that does not prove the idle-to-active dirty edge. KMD v142
   orders the exact DWM refresh marker on a Venus completion watermark; the UMD's bounded 10 ms
   condition-variable frame gate closes the earlier DXVK-submission-thread producer race at about
   0.48 ms steady-state average. Measure wake latency and steady-state cadence separately before
   assigning blame to the guest, frontend, or remote client.
3. **D3D11 conformance** — drive the noop-DDI hit counters to zero against real workloads,
   dxvk-tests / samples / 3DMark, DXGI format coverage, remaining 11.1 DDI plumbing.

**`ROADMAP.md` is the living stage document** — current defect list, per-workstream plans, and the
tooling inventory (registry knobs, counters, ETW AzureTriage recipe, guest probe schtasks). Update
it as items close or appear. Session-by-session state lives in the agent memory; do not create
per-session HANDOFF_*.md docs — distill into memory + ROADMAP.md.

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

**Driving the VM:** prefer the **`win` MCP server** — `win_exec`, `win_cargo` (mirrors `Z:\` to
`C:\Users\Rupansh\helios-vgpu` and sets the local target dir + `LIBCLANG_PATH`), `win_install_umd`,
`win_meson` (Mesa ICD), `win_looking_glass`/`win_looking_glass_idd`. coreutils are installed on
win11. SSH/win_exec land in **session 0** — window/desktop probes must run via scheduled tasks
(`schtasks /run /tn <name>`; see ROADMAP.md tooling). See TOOLCHAIN.md.

**VM launch ownership:** if you change the standalone VM launch command,
`tools/launch-helios-gtk.sh`, QEMU display/debug transport, or launcher environment variables,
stop after making/documenting the change and ask the user to run or restart the VM. Ask before
cold boots / guest reboots; `pnputil /restart-device` re-runs AddAdapter without one.

---

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
7. **Measure before optimizing** (this stage especially): add/read counters, ETW, or timestamps
   first; land perf changes with before/after numbers.

## Repository Structure (active paths)

```
helios-vgpu/
├── CLAUDE.md               ← You are here
├── ROADMAP.md              ← PSC stage: defects, plans, tooling (living doc)
├── ARCH.md / OVERVIEW.md   ← Architecture (some sections describe the archived
│                             System-class path; kmd_render/ is the active driver)
├── KMD.md ICD.md TRANSPORT.md HOST.md TOOLCHAIN.md
├── NTOSEYE.md              ← Windows KD (ntoseye) quirks
├── BRINGUP_QUIRKS.md       ← build/deploy/VM-control gotchas (purge-fingerprint,
│                             repackage+sign, DriverStore, QMP reset, diag ring)
├── HELIOS_DRIVER_DEPLOYMENT.md
├── docs/archive/           ← Frozen bring-up-era design/research docs (GATE*,
│                             WDDM_*, DISPLAY*, PHASE*, HANDOFF_*). Read-only
│                             history; code comments may cite them by name.
│
├── kmd_render/             ← ACTIVE: WDDM 3.2 render+display miniport (Rust, no_std)
│   └── src/ddi/            ← DDI surface (query_adapter_info = caps/segments,
│                             create_allocation, cpu_host_aperture, gdi_blit =
│                             RenderGdi executor, build_paging_buffer, escape,
│                             submit_command/scheduler, interrupt)
│   └── src/virtio/         ← virtio-gpu transport + async ctrl (C3/M3.4) + venus client
├── umd/                    ← ACTIVE: D3D11 UMD (d3d10umddi frontend, cxx bridge)
├── dxvk-helios/            ← ACTIVE: forked DXVK engine (venus import model, GDI staging)
├── icd/mesa                ← ACTIVE: Mesa fork — Venus Vulkan ICD (build via win_meson)
├── qemu-helios/            ← ACTIVE: QEMU fork — modifier metadata + native OPTIMAL readback
├── LookingGlass/           ← HISTORICAL: former IddCx capture path
├── protocol/               ← shared guest/host wire structs (builds on BOTH platforms)
├── tools/                  ← launcher, deploy scripts, guest probe .ps1 toolkit
│
├── kmd/ + icd/src…         ← ARCHIVED reference: System-class KMDF + IOCTL stack
└── host/                   ← host-side daemon experiments
```

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

## When You're Stuck

1. `ROADMAP.md` (stage state) → `BRINGUP_QUIRKS.md` (deploy/VM) → `NTOSEYE.md` (live KD).
2. dxgkrnl failure reasons in plain text: ETW `Microsoft-Windows-DxgKrnl` all-keywords trace →
   tracerpt → grep `AzureTriage` (recipe in ROADMAP.md).
3. Venus protocol ground truth: `venus-protocol/vk.xml`, `virglrenderer/src/venus/`. Host-side
   log: `/tmp/helios-qemu-stderr.log` (launcher tee); `HELIOS_VKR_DEBUG=validate` enables host
   validation layers.
4. Reference drivers: mvisor-win-vgpu-driver (System-class model), kvm-guest-drivers-windows
   viogpu (virtio init only).
5. Ask the overseer on fundamental architecture questions.

## Files Not to Touch

- `*.inx` — only with explicit instruction (active shape: WDDM render miniport INF).
- `docs/archive/**` — frozen history; do not edit, do not resurrect into the live tree.

## Code Style

```rust
// Kernel-mode code: no_std, no panics in release, wdk-sys / bindgen types directly.
// Pattern for DDI handlers:
//  - null-check args, validate every runtime-supplied length per-arm
//  - do the work; every skip/refusal increments a named counter
//  - errors -> documented NTSTATUS from the DDI's legal return set
//    (an illegal NTSTATUS is itself logged by dxgkrnl as a driver bug)
```
