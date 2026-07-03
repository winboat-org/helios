# Handoff — implement the async/interrupt-driven venus transport (C3/M3.4): the synchronous Escape model is the structural root of IDD unreliability (2026-07-04, ninth session)

Read `HANDOFF_XID109_TDR_CONTRACT_2026_07_04.md` first for this session's fix chain. **Owner
verdict at session end: frames appear occasionally, freeze after a while; the IDD is not
reliable — and the owner's standing hypothesis, now adopted as the plan, is that we have put
the interrupts implementation off for too long.** The evidence agrees; stop patching the
consumers and fix the transport.

## Why the synchronous transport is the root (evidence chain, this session)

Every venus round-trip today is `ctrl_queue_bounded_roundtrip` (kmd_render
`src/virtio/gpu.rs`): add → notify → **busy-poll the used ring (~1 s bound,
`CTRL_POLL_SPINS`) under the device spinlock** → pop. Fenced SUBMIT_3D completions are
awaited inline the same way ("host-visible-complete by return"). Consequences, all observed
live and dump-verified:

1. **Latency amplification**: with `HELIOS_VKR_DEBUG=validate`, host-side processing is
   seconds-slow; every waiter serializes under the device spinlock AND the dxgkrnl adapter
   lock above it. A WUDFHost device-create Escape was caught blocked **30+ seconds** inside
   `NtGdiDdDDIEscape` (dump `WudfHost_ext__1580.mdmp`, thread 014:
   `helios_submit → helios_ioctl_submit_cs → helios_escape → NtGdiDdDDIEscape`).
2. **Deadline collisions**: anything with a watchdog dies while queued behind that convoy —
   IddCx's ~10 s held-frame deadline killed WUDFHost twice
   (`ReportBugcheckForSwapChainTimeoutDriverDidNotReleaseFrame`; thread 013 of the same dump
   shows the predecessor's d3d11 teardown blocked on the runtime lock held by the stuck
   create). dwm's own stalls produced the earlier Xid-109 freezes.
3. **No wakeups, only polls**: the ISR/DPC path exists as a stub
   (`kmd_render/src/ddi/interrupt.rs`, 88 lines — "DxgkCbNotifyInterrupt for each completed
   fence" is a comment, not code). One second of spin per round-trip is both too long (a
   DISPATCH-level spin) and too short (validate-slow hosts legitimately exceed it —
   `CTRL_TIMEOUT_COUNT` poisons the transport when they do).
4. Bounded-spin timeouts poison the transport permanently (`VirtioGpu::failed`) — correct
   for a wedged host, wrong for a merely-slow one.

## What to build (the C3/M3.4 model, long specified, never landed)

- **KMD**: stop waiting inline. SUBMIT_3D returns as soon as the descriptor is queued.
  Wire the virtio interrupt for real: `EvtInterruptIsr` acks + queues DPC; the DPC drains
  the used ring, completes fence ids against a **fence table** (fence_id → KEVENT / waiter
  list in `AdapterContext`), and re-arms. `HELIOS_ESCAPE_WAIT_FENCE` becomes a real
  KEVENT wait with the caller's `timeout_ns` (it is a validated no-op today —
  `escape_wait_fence` in `ddi/escape.rs` returns SUCCESS unconditionally because inline
  waits made it moot). The ctrl-queue scratch/descriptor lifetime notes in
  `ctrl_queue_bounded_roundtrip`'s comment describe the residual risk this removes.
  Ordering invariant (CLAUDE.md): venus commands must be flushed before the fence signals.
- **ICD** (`vn_renderer_helios.c`): submits stop being synchronous; fence waits go through
  WAIT_FENCE (KEVENT-backed, real timeouts) instead of relying on "submit returned ⇒ done".
  The ring's seqno polling stays user-mode (that part is fine); what changes is that
  fenced submissions no longer hold a kernel spinlock for their whole host round-trip.
  ⚠ The "accidentally coherent" WDDM fence model (memory: venus-over-Escape synchronous →
  immediate fence) breaks the moment submits go async — the KMD's SubmitCommand/fence
  reporting to dxgkrnl must be driven by the SAME DPC completions (this is also the
  long-planned "real venus-driven fence" from WDDM_FAKE_VIDMM_RESEARCH §C).
- **Keep** the bounded-spin path only for early bring-up (GET_DISPLAY_INFO smoke) where no
  interrupt is armed yet.
- The IddCx swapchain processor and dwm then block in *waitable* kernel waits with real
  timeouts instead of convoying on spinlocks — the whole deadline-collision class dissolves.

Plan docs that already cover parts of this: `KMD.md` Phase 2/4 (ISR→DPC→KEVENT design),
`WDDM_SYNC_REDESIGN.md`, `SUBMISSION_FENCE_HANDOFF.md`, `WDDM_FAKE_VIDMM_RESEARCH.md` §C.
Reference: mvisor-win-vgpu-driver's interrupt path; viogpu_queue.cpp for virtio ISR shape.

## State at handoff (all committed)

- This session's landed fixes (all verified individually): Input-08733 layout-driven VS
  variants (d8a2d97), stray boot-path UMD deleted + module-path logging (2ee27f5), ICD image
  memreqs consistency (icd/mesa 3671722a8f2), KMD standard-alloc GOB padding (d8bded7),
  dxvk-helios undersized-import refusal (3d27c1af), ICD sem deadline 8 s (icd/mesa
  40c9af15465), LGIdd processor-lifecycle serialization (LookingGlass fbc5ae98) + watchdog
  fixes (7c0dd842), TDR contract (e732850).
- ⚠ **Deploy state is UNCERTAIN**: the last deploy (ICD-8s install → IDD update+restart →
  UMD hotplug) lost SSH mid-hotplug ("Connection timed out" after the IDD restart) — the
  final UMD hotplug (kill users + Helios/IDD restart) may not have completed. On session
  start: check `Get-Process`/`umd-*.log` "UMD module:" lines and the active hashes, rerun
  `tools/hotplug-helios-umd.ps1 -Mode ProgramData -KillUmdUsers -RestartDevice -NoProbe`
  if in doubt, and note the guest may need a moment (or a reboot) if the SSH loss was a
  guest-side stall. The IDD may sit at Code 43 with UMDF's 5-crash ban — ONE
  `devcon restart '@ROOT\DISPLAY\0000'` clears it after the deploy is confirmed
  (devcon at `C:\Program Files (x86)\Windows Kits\10\Tools\10.0.26100.0\x64\devcon.exe`).
- Remaining validation classes (validate boots): CB-lifecycle violations in churn windows
  (teardown-under-load — likely ALSO dissolved by real fences, since guest-side "done"
  will finally mean host-side done), memoryTypeBits 0x3-vs-type-2 import UB (functional),
  layout-00344 (known backlog). `RotateSample` still 16 — set 0 for production runs.

## Diagnostic arsenal (all proven this session)

- Host: `journalctl -k | grep -i xid` FIRST; `/tmp/helios-qemu-stderr.log` full-file with
  validate on (dedupe = 10 per VUID per render-server process — count budget before
  trusting silence).
- WUDF crash dumps: `C:\ProgramData\Microsoft\WDF\WudfHost_ext__*.mdmp` — analyze with
  `C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe -z <dump> -c ".symfix;
  .reload; kc 30; q"` (or `!findstack LGIdd 2` for all threads). This is how both IddCx
  kills were root-caused.
- Guest stacks without halting: ntoseye memory backend → `threads(pid)` → KTHREAD+0x90
  TrapFrame → user RSP → scan qwords against `modules` (scripts in the session scratchpad:
  `sweep.py`, `usweep.py`, `stackwalk.py`, `nto.py`).
- MinGW ICD DLL carries DWARF: `llvm-addr2line -e vulkan_virtio-<hash>.dll <ImageBase+RVA>`.
- KMD health: `blob_capacity_probe.exe` prints QUERY_STATS incl. `ctrl-timeouts`
  (nonzero = transport poisoned).
- DXVK rebuild (cargo can't see it): sync `Z:\dxvk-helios\src` →
  `C:\Users\Rupansh\dxvk-helios\src`; `ninja -C C:\Users\Rupansh\dxvk-build` with PATH =
  MSVC `Hostx64\x64` (lib.exe) + `LLVM\bin` (clang-cl); touch `umd\build.rs`; `cargo build`;
  grep the DLL for a new string to confirm the relink took.

## Acceptance for the interrupts milestone

1. A validate cold boot where device creation no longer produces >10 s Escape queuing
   (no WUDFHost verifier kills across 3 boots).
2. Owner-visible desktop that survives login transition + 15 minutes of interaction.
3. `ctrl-timeouts` stays 0 AND no transport poison under validate (slow ≠ dead once waits
   are event-driven).
4. The 8 s ICD deadline never fires in steady state (it is the tripwire, not the mechanism).
