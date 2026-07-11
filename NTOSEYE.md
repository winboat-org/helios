# NTOSEYE.md — Kernel-debugging Helios with ntoseye

ntoseye is a **Windows KD** (kernel debugger) that talks the Windows KD protocol over a
serial transport to the live win11 guest, exposed to Claude Code as an MCP server. This doc
records how to use it for Helios bring-up debugging and the **quirks that will waste hours**
if you don't know them. Pairs with [BRINGUP_QUIRKS.md](BRINGUP_QUIRKS.md) (build/deploy/VM
mechanics) — read both before a debug session.

> **Architecture note (2026-07-11):** the current KMD is a live render+display
> adapter. Dated examples below that call it render-only or assume an IddCx
> output describe earlier bring-up configurations; do not use those claims to
> justify present-day disable/restart operations.

> Hard-won during the Step-2 GpuMmu bring-up (2026-06-18/19), where it root-caused the Code-43
> failure to `dxgmms2!VidSchTerminateAdapter`. See the `step2-gpummu-implemented` memory.

---

## 1. What it is / how it connects

- ntoseye attaches as a **Windows kernel debugger** over the QEMU serial socket
  `/tmp/ntoseye-kd.sock` (the guest must be booted with KD enabled in the standalone launcher).
  The user starts it with `ntoseye mcp --http 127.0.0.1:8080`
  and it serves Streamable HTTP at `http://127.0.0.1:8080/mcp`.
- **The user owns the MCP lifecycle.** If a tool call returns `HTTP 404` / `ConnectionRefused`,
  the MCP connection dropped (ntoseye was restarted) — ask the user to reconnect it (`/mcp`).
  You cannot reconnect it from a tool.
- It is NOT a gdbstub. A bare `int3` / `DbgBreakPoint()` in driver code still **BSODs** the
  guest unless the guest is configured to route it to the KD — do **not** put int3 in the
  driver to "break into ntoseye"; it bugchecks. Use the spin-gate technique (§5) instead.

## 2. Run-control model

The guest runs free by default. Split run-control:
- `status` — where am I now (running/halted, rip, coherent). Read-only.
- `interrupt` — halt the VCPU (needed before `registers`/`step`/`set_breakpoint`).
- `resume` — go (non-blocking).
- `wait_for_stop(timeout_ms)` — poll for the next stop WITHOUT resuming; returns
  `{stop:"breakpoint"|"step"|"bugcheck"|"target_reloaded"|"running"|"halted"}`. Poll by
  calling again (max 20 s/call). It does NOT resume.
- `step` / `step_over` / `step_out` — require the VM halted.

**While the VM is halted, the guest network is frozen** — `win_exec` (SSH) will fail with
"No route to host" until you `resume`. If SSH suddenly can't connect mid-session, you probably
left ntoseye halted; `resume`.

## 3. ⚠️ Schema-broken tools (DO NOT rely on them)

In the version used this session, these returned an array/null that the MCP client rejected
with an `invalid_type` / `expected record` error — **unusable**:

- `backtrace`  ← the big one; you cannot get a call stack the easy way
- `disassemble`
- `bugcheck`
- `list_breakpoints`

**Working** (record-returning): `status`, `registers`, `read_memory`, `read_struct`,
`step`/`step_over`/`step_out`, `set_breakpoint`/`clear_breakpoint`, `resume`/`interrupt`/
`wait_for_stop`, `kernel_modules`, `search_symbols`, `closest_symbol`, `describe_address`,
`write_memory`.

Work around the missing `backtrace` by **manual stack walking**: `registers` → read `rsp`,
`read_memory` a few hundred bytes of stack, pick out return-address-looking 8-byte LE values
(kernel code lives at `0xfffff8...`), and `closest_symbol` each. Work around `disassemble` by
`read_memory` + decoding the bytes by hand (x86-64).

Climbing frames with repeated `step_out` works but **re-enters your driver's DDIs** if you're
inside a teardown path (each DestroyContext/DestroyDevice call hits your bps) — clear those
bps first, or accept the interruptions.

## 4. Symbols & breakpoints

- Microsoft symbols for `nt` / `dxgkrnl` / `dxgmms2` resolve (decorated C++ names too, e.g.
  `dxgkrnl!?DdiCreateContext@ADAPTER_RENDER@@...`). `search_symbols "dxgmms2!VidSch"` etc.
- **ntoseye has NO PDB symbols for `helios_kmd_render`.** You cannot `set_breakpoint
  "helios_kmd_render!dxgkddi_create_context"` and you cannot defer a bp on the unloaded module.
  Set Helios bps by **absolute address = module base + RVA**:
  - get the load base from `kernel_modules(filter:"helios")` (only valid while loaded — see §5),
  - get the RVA from the package `.map`: a code symbol line is
    `0001:<off>  _RNvNt…<fn>  <preferredVA>  f  …`; **RVA = preferredVA − 0x180000000**.
  - RVAs **shift every build** — re-read the `.map` after each rebuild.
- `set_breakpoint`/`clear_breakpoint`/`step`/`set_register` require the VM **halted**
  (`interrupt` first). `write_memory` works live.
- **Breakpoint conditions are NOT evaluated** (at least `poi(rdx)==0xN` was ignored — the bp
  fired unconditionally on a non-matching call). Don't trust conditions; filter manually after
  the hit (read the args and `resume` if it's not the one you want).
- A bp on a **paged-out** function is refused ("target page is non-executable"). The page is
  resident while that code is executing — set the bp when the subsystem is active, or pick a
  hot/resident function on the path.

## 5. Catching a driver that loads only transiently (the spin-gate)

A WDDM render miniport that fails post-start (Code 43) is **unloaded at steady state** and only
mapped for a sub-second window during bring-up — too fast to catch by polling `kernel_modules`,
and ntoseye can't defer a bp by name. Solution: a **temporary spin-gate** in the driver that
holds the module loaded so you can arm bps, then release it.

1. Add at the very top of `DxgkDdiStartDevice` (PASSIVE, module loaded, before the
   QueryAdapterInfo storm / CreateDevice / CreateContext):
   ```rust
   pub static START_SPIN: AtomicU32 = AtomicU32::new(1);
   // ... in StartDevice:
   { let mut g: u64 = 0;
     while START_SPIN.load(Ordering::Acquire) != 0 {
         core::hint::spin_loop(); g = g.wrapping_add(1);
         if g > 200_000_000_000 { break; } }  // guard auto-releases ~minutes so a
   }                                          // no-debugger boot is never bricked
   ```
2. Build/deploy (see BRINGUP_QUIRKS.md), **QMP `system_reset`** (reliable bring-up replay).
3. After `target_reloaded`, `resume` and poll `status` until `coherent:true`, then
   `kernel_modules(filter:"helios")` — now it appears and **stays** (spinning). Note the base.
4. `interrupt`; set your DDI bps by base+RVA.
5. **Release the gate.** ⚠️ The compiler **const-folds** the `pub static AtomicU32` load to its
   init value (it sees no in-crate writer), so the loop becomes a *pure guard countdown* and
   **writing `START_SPIN=0` does nothing**. Instead patch the loop's backward `jne` to NOPs:
   `read_memory` the StartDevice prologue, find the `add rax,-8` (`48 83 c0 f8`) followed by
   `75 ea` (the `jne` back-edge), and `write_memory` `90 90` over the `75 ea`. The spinning
   thread falls through on its next pass.
6. `resume` → bring-up proceeds → your DDI bps fire.

Always REMOVE the spin-gate and redeploy a clean signed build before pausing the session.

## 6. Reboot / target reload

A guest reboot (QMP `system_reset`, or a Windows-side reboot) is reported as
`wait_for_stop → {stop:"target_reloaded", coherent:false}` at the earliest pre-init stop: only
`nt` symbols exist, module/process enumeration is unavailable, and **every prior address is
invalid** (KASLR re-randomizes `nt`/`dxgkrnl`/`dxgmms2` bases — re-resolve symbols, recompute
base+RVA). From there: `resume`, then poll `wait_for_stop`/`status` until `coherent:true` before
enumerating. ntoseye survives the reboot (it's attached to the VM, not the guest OS).

`nt!DebugService2` is where `DbgPrint`/`DbgBreakPoint` route; with the KD attached you may get
spurious breaks there during boot (drivers printing). Just `resume` past them.

## 7. A worked example (what root-caused the GpuMmu Code 43)

```
spin-gate StartDevice → QMP system_reset → poll coherent → kernel_modules(helios) base=0x…670000
→ interrupt → bp create_context/destroy_context/set_root_pt/get_root_pt/build_paging/destroy_device
(base+RVA) → patch jne → resume
→ create_context HITS (args: NodeOrdinal/EngineAffinity/Flags all 0 = plain context)
→ step_out → caller = dxgkrnl!ADAPTER_RENDER::DdiCreateContext+0xda
→ resume → NEXT bp = destroy_context (NOT any page-table DDI!)  ⇒ bail is before page tables
→ step_out × N: our DestroyContext ← dxgkrnl DdiDestroyContext ← dxgmms2 DdiDestroyContext
   ← dxgmms2!VidSchTerminateContext ← dxgmms2!VidSchTerminateAdapter (+VidSchFlushDevice)
⇒ ROOT CAUSE: the Video Scheduler terminates the whole adapter during init, right after the
  system context is created, before any page-table DDI — i.e. it's a scheduler/submission
  problem, not a page-table-model problem.
```
The teardown runs on a VidSch **worker thread** (different stack from the bring-up thread), so
the teardown stack does NOT contain the failing init call; to pin the exact failing sub-step,
trace the **bring-up thread** forward from `DdiCreateContext+0xda` (set_current_thread + step),
or bp `dxgmms2!VidSchTerminateAdapter` entry and read its caller.

## 8. Reboot-free bring-up replay (PnP disable→enable) — USE THIS for KD loops

A full `system_reset` drops ntoseye (404 → user must reconnect) and the KD-attached boot breaks
repeatedly on `DbgPrint`. To re-run Helios bring-up **without a reboot, keeping the KD attached**,
PnP-cycle the device over SSH (`win_exec`):
```
Disable-PnpDevice -InstanceId <helios> -Confirm:$false
poll until (Get-PnpDeviceProperty <helios> DEVPKEY_Device_ProblemCode).Data == 22   # CM_PROB_DISABLED
(clear the registry diag ring here)
Enable-PnpDevice  -InstanceId <helios> -Confirm:$false
poll until the ring repopulates
```
Reliable **only if you wait for the disable to settle (ProblemCode 22) before enabling.** Safe for
Helios specifically because it's render-only at Code 43 (NOT the live display adapter — disabling
the gpu-gl/IDD display adapter still deadlocks DWM). It re-runs everything except the boot-only
`DpiFdoStartAdapter*` path. **`Enable-PnpDevice` BLOCKS until the device starts or fails**, so when
a KD breakpoint halts the guest mid-bring-up the `win_exec` enable hits its timeout and is killed —
that is EXPECTED, the guest stays halted at your bp; give enable a short `timeout_secs`, then drive
ntoseye (`status`/`wait_for_stop`).

## 9. ⚠️ Stepping in VidMm paging-init reboots the guest — prefer reads-only static analysis

Single-stepping / many breakpoints inside `dxgmms2!VidMmInitializePagingProcess` / `InitDmaPools`
destabilized the guest into a **reboot** mid-trace. For deep dxgmms2 work prefer **reads-only**
static analysis: `read_memory` the function bytes and decode (calls = `e8 rel32`; the failing call
is the one whose return status is `test eax,eax; js/jns`-checked). Guest memory reads work while
running and carry no reboot risk. Reserve bps/stepping for confirming one specific spot.

## 10. Worked example #2 — root-causing the GpuMmu Code 43 to `InitDmaPools` (2026-06-19)

Climbing the nested-call stack by `step_out` + reading `rax` at each return (each level SUCCEEDED
until the one that returns the error):
```
our DxgkDdiCreateContext (rax=0) → DdiCreateContext (rax=0) → VidSchiCreateContextInternal (ctx ptr)
→ VidSchCreateSystemDevices (rax=0)  ⟵ all succeed
→ VidMmInitializePagingProcess: step_out → rax=0xC000000D  ⇒ THIS frame returns the error
→ inside it, the 3 checked calls are InitDmaPools(+0xec), CreatePagingFenceObjects(+0x11c),
  InitPagingProcessVaSpace(+0x15c loop); bp the 3 return sites, check rax: InitDmaPools FAILS first
⇒ ROOT CAUSE: dxgmms2!VIDMM_GLOBAL::InitDmaPools → STATUS_INVALID_PARAMETER (VidMm paging DMA-pool
  init rejects a segment-attribute mask our DXGK_SEGMENTFLAGS don't permit). See INITDMAPOOLS_HANDOVER.md.
```
Technique notes: to identify a caller, `step_out` lands one frame up with `rax` = the callee's
return — read it to bisect which nested call fails. To catch the *Helios* invocation amid
multi-adapter noise (OpenAdapter runs for WARP/Basic/IDD too), bp **our** `CreateContext`
(Helios base+RVA) — only Helios has it — then `step_out` to land inside the dxgkrnl/dxgmms2 caller.
