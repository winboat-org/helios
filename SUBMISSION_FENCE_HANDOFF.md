# SUBMISSION_FENCE_HANDOFF.md — make DWM's composition on Helios actually render (§4 coherence)

**Date:** 2026-06-22. **Read first:** this doc, then `WDDM_FAKE_VIDMM_RESEARCH.md` §C (fence/submission),
the `path-a-crossadapter-rendergdi` + `step2-gpummu-implemented` memories, `BRINGUP_QUIRKS.md`, `NTOSEYE.md`.
Primary reference implementation: `virtio-research-only-3d/viogpu/viogpu3d/viogpu_command.cpp` (the
SubmitCommand → worker → virtio used-ring ISR/DPC → fence-complete shape).

## The one-line goal
Make DWM's composition **on Helios** produce real frames so it stops failing `0x889800b0`, the OS builds
the IDD display path, IddCx assigns a swapchain, and Looking Glass shows the desktop. This is the
**§4 coherence** layer: real venus-driven submission + a fence that signals only on real host completion.

## Where we are (verified live 2026-06-22 — all GOOD news)
- **Helios is a STABLE WDDM render adapter at Code 0** and the cross-adapter composition path now RUNS on it.
  A 3-layer crash chain was fixed this session (all uncommitted in `kmd_render`, deployed as the live
  `e0bd` DriverStore `.sys`, 83872 B):
  1. Declared `DXGK_VIDMMCAPS::CrossAdapterResource` (bit 4), gated `DECLARE_CROSS_ADAPTER_RESOURCE=true`
     in `ddi/query_adapter_info.rs`. Tier-1 only (NOT Scanout bit16 → no CHECKMULTIPLANEOVERLAYSUPPORT3
     obligation). Composition surfaces (ap.kind==STANDARD) get 256-B linear pitch + aperture-eligible.
  2. Implemented + registered **`DxgkDdiRenderGdi`** (`ddi/submit_command.rs`, `lib.rs`, `ddi/mod.rs`) —
     it was a never-registered DISTINCT field; dxgkrnl's GDI-HW-accel path called the null slot →
     `0xC0000005`. Also made `DxgkDdiRenderKm` return SUCCESS.
  3. Implemented **`DxgkDdiSubmitCommandVirtual`** (`ddi/submit_command.rs`) — GpuMmu contexts submit
     virtually, not via SubmitCommand; the `STATUS_NOT_SUPPORTED` stub caused bugcheck `0x119`
     VIDEO_SCHEDULER_INTERNAL_ERROR Arg1=2.
- **Diag ring confirms the engine now runs**: `GetStandardAllocationDriverData` (GDISURFACE/SHADOW),
  `CreateAllocation` blobs, and `Render`(0x0F09)/`SubmitCommand`(0x0F06) atoms NONZERO (all were 0 before).

## THE PROBLEM TO SOLVE
DWM now **adopts the IDD (INDIRECTKMD) as primary and composites it on Helios**, but the composition is
**not viable** → DWM fails `0x889800b0` and the `dwminit` watchdog crash-loops it (Application log). So:
`SetDisplayConfig(SDC_APPLY|SDC_TOPOLOGY_EXTEND)` → ERROR_GEN_FAILURE; IddCx `AssignSwapChain` never fires.
(NB: read display state in the **console session (1)**, not the SSH session-0 — session 0 sees 0 paths
falsely. Use a scheduled task `/ru Rupansh /it` to run in session 1, as this session did.)

**Why (root cause, high confidence):** `ddi/submit_command.rs` `dxgkddi_submit_command` AND
`dxgkddi_submit_command_virtual` are **null engines** — they call `signal_dma_completed` (DMA_COMPLETED
via `DxgkCbSynchronizeExecution`→DIRQL `DxgkCbNotifyInterrupt` + `DxgkCbQueueDpc`) **immediately**, before
any host work, and **never forward the venus render stream to the host**. So DWM's render targets on Helios
contain nothing real and the WDDM fence lies; DWM detects the failure and dies.

## What to build (§4, in order)
1. **Forward the render DMA buffer's venus stream to the host on submit.** The UMD command buffer begins
   with a `HeliosWddmCmdBuf` header followed by the opaque venus byte stream (`protocol/src/wddm.rs`);
   `dxgkddi_render`/`dxgkddi_render_gdi` already copy `pCommand`→`pDmaBuffer`. On `SubmitCommand` /
   `SubmitCommandVirtual`, parse the header and hand the venus bytes to the host via the in-kernel venus
   client (`kmd_render/src/virtio/venus.rs`, which already does ring + encode + reply over virtio — it
   self-allocates host-visible blobs today). **GOTCHA:** `DXGKARG_SUBMITCOMMAND` has no CPU VA — the DMA
   buffer is addressed by physical/GPU-VA; `DXGKARG_SUBMITCOMMANDVIRTUAL` carries `DmaBufferVirtualAddress`.
   You must resolve the DMA buffer's CPU-accessible mapping (it lives in the BAR memory segment /
   `MAP_BLOB` window) to read the venus bytes. Confirm how the buffer is mapped before wiring.
2. **Signal the WDDM fence only on REAL host completion.** Replace the immediate `signal_dma_completed`:
   queue the submission, return STATUS_SUCCESS, and complete the fence from the **virtio used-ring
   interrupt** path. `ddi/interrupt.rs` (`dxgkddi_interrupt_routine`/`dxgkddi_dpc_routine`) — verify the
   current real-ISR state (a read-to-clear ISR-status register was added per the step2 memory); wire the
   DPC to drain the virtio used ring and, per completed fence, fill `DXGKARGCB_NOTIFY_INTERRUPT_DATA`
   {DMA_COMPLETED, SubmissionFenceId} and call `DxgkCbNotifyInterrupt` (DIRQL) + `DxgkCbQueueDpc`. This is
   exactly `viogpu_command.cpp`'s model. Keep paging/null buffers (Flags.Paging) on the immediate path.
3. **Composed surface coherence for the IDD.** The surface DWM composes / the IDD reads must be the
   host-visible venus blob (`MAP_BLOB` BAR) with correct cache coherency, so the IDD's D3D device (on the
   Helios render LUID) reads the venus-rendered pixels. `CreateAllocation` already makes HOST3D blobs +
   marks STANDARD surfaces CpuVisible/pinned; verify `BuildPagingBuffer`/`RESOURCE_MAP_BLOB` maps them
   into the window where the IDD reads.
4. **Then** the OS should build the IDD path + `AssignSwapChain` fires → switch the IDD copy from D3D12 to
   D3D11 (Helios has no D3D12) or use the CHeliosSink Vulkan import (`IDD_HELIOS_RENDER_PLAN.md` §3).

## Diagnostics that work (proven this session)
- Watch the crash class: kernel bugcheck → `C:\Windows\Minidump` + `cdb -z <dmp> -c "!analyze -v;q"` (cdb at
  `C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe`). DWM user-mode fail → Application log
  `Dwminit` exit code; the crash is deterministically triggerable by SetDisplayConfig EXTEND in session 1.
- ntoseye HAS full **dwmcore + ntdll + dxgkrnl public symbols** (no helios symbols — base+RVA via the `.map`
  at `…\target\debug\deps\helios_kmd_render.map`). DWM is killed EXTERNALLY by the dwminit watchdog (not its
  own RtlExitUserProcess/RtlFailFast2), so to catch the failing op break **inside dwmcore composition** or on
  the failing D3D/dxgkrnl call, not dwm's exit. dwm pid churns ~2min.
- Diag ring legend: `BRINGUP_QUIRKS.md` §6. Engine atoms `0x0F06`(SubmitCommand)/`0x0F09`(Render)/
  `0x0F0C`(InterruptRoutine) dumped at DestroyDevice. Watch the fence (`0x0F07`) track real completion.
- Display state: run QueryDisplayConfig/EnumDisplayDevices via a scheduled task in **session 1**.

## Deploy / VM mechanics (BRINGUP_QUIRKS §2–5, all reconfirmed this session)
Build host IS the win11 VM over SSH (win_cargo/win_exec) — so when Helios crash-loops, **SSH/build are
blocked**; the USER must boot **gpu-gl-out** (Helios phantom → SSH up, e0bd `.sys` unlocked) so you can
build+deploy, then the USER re-adds gpu-gl to test (keep ntoseye attached to catch faults vs bugcheck-loop).
Deploy: `win_cargo build` (cargo make's `cargo test` step AVs — ignore) → copy `deps/helios_kmd_render.dll`
→ `package\helios_kmd_render.sys`, sign (WDRLocalTestCert / WDRTestCertStore, **x64** signtool) → delete cat
→ inf2cat (**x86** path, standalone) → sign + `signtool verify /pa /c` cat → in-place overwrite the e0bd
DriverStore dir (`…inf_amd64_e0bd070459ad7ca4`, = oem123.inf) → `Write-VolumeCache C` → clear diag ring
(`HKLM\SYSTEM\CCS\Services\helios_kmd_render` `Sxx`). New KMD code needs a full reboot (QMP `system_reset`
on `/tmp/helios-tpm/mon.sock`); disable→enable does NOT reload code. `win_exec` swallows object-table output
— emit strings. VM launch / gpu-gl device changes are USER-driven (CLAUDE.md).

## Files
`kmd_render/src/ddi/submit_command.rs` (submit/render/fence — the main work), `ddi/interrupt.rs` (ISR/DPC),
`virtio/venus.rs` (in-kernel venus client to reuse), `ddi/create_allocation.rs` (alloc→blob + standard
surfaces), `ddi/build_paging_buffer.rs` (MAP_BLOB/paging), `protocol/src/wddm.rs` (`HeliosWddmCmdBuf`),
`adapter.rs` (`last_completed_fence`, dxgkrnl interface). All crash-chain fixes are **uncommitted** — commit
as a milestone if desired before starting.
