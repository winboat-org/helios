# PARALLEL.md — how P3's 214 DDI slots are split across concurrent agents

**Status:** work-decomposition doc. It contains **no decisions** — `DECISIONS.md` is authoritative
and nothing here may contradict it. Slot counts come from `DECISIONS.md` §4.1 (the canonical count
table); group boundaries come from `DDI_REFERENCE.md` §3.2 and §4.2; file boundaries come from
`ARCHITECTURE.md` §5, which already specified the `umd12` module split for exactly this reason.

⚠ Read `ARCHITECTURE.md` §11 first for stage order. This document only says **who does which part of
S6, and how they avoid each other.**

---

## 1. Why P3 is not one job

| table | slots |
|---|---:|
| `D3D12DDI_ADAPTERFUNCS_0109` | 8 |
| `D3D12DDI_DEVICE_FUNCS_CORE_0109` | 124 |
| `D3D12DDI_COMMAND_LIST_FUNCS_3D_0108` | 75 |
| `D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001` | 7 (2 are `pfnUnused`) |
| **total driver-side slots** | **214** |

Plus 43 `D3D12DDICAPS_TYPE` enumerators behind one `pfnGetCaps`, each with cross-tier coherence
rules the runtime enforces (H4).

That is the bulk of a graphics driver. It is also, unusually, **well-suited to fan-out**: the DDI is
a flat table of independent function pointers over one engine object, so two slots in different
functional groups almost never touch the same state.

## 2. ⛔ The binding constraint nobody should discover the hard way

**There is one VM.** `win11` is a single machine with a single Helios adapter, and:

- `win_install_umd` **disables and re-enables the PCI device**. Two agents deploying concurrently
  will interleave a disable with the other's benchmark and produce a fake regression.
- Fire Strike, `helios_paintcap`, `pnputil /restart-device` and reboots are all **exclusive**.
- `HKLM\SOFTWARE\Helios` knobs are **process-global to the machine**. One agent's A/B arm silently
  applies to another agent's measurement.
- `tools/umd-check.ps1` hardcodes **one** mirror (`C:\Users\Rupansh\helios-vgpu`), **one**
  `CARGO_TARGET_DIR` and **one** log path. Concurrent runs `robocopy /MIR` over each other's
  sources mid-build.

⇒ **Authoring and compiling parallelise. Validating does not.** Every lane's inner loop is
edit → `cargo check`, which needs only build isolation (§7, Lane 0). Deploy, gate and benchmark go
through a **lease** (§6).

⚠ This is why "N agents, N× throughput" is wrong here. Expect the fan-out to help most during S6's
long authoring middle, and to collapse back to serial at every gate.

## 3. The serial spine — none of this parallelises, and all of it comes first

| stage | what | why serial |
|---|---|---|
| **S4** | `bridge/vkd3d_bridge.{h,cpp}` + `src/bridge12.rs`: `helios_vkd3d_create_device` only, returning a live `ID3D12Device*` | every lane forwards through it; lanes cannot be written against a bridge that does not exist |
| **S4b** | `helios_icd_anchor_v1` in **both** DLLs + `IcdAnchorMismatch` | must land before the first two-engine run (UNVERIFIED-4) |
| **S5** | INF + slot 3 + `UmdD3D12` knob + `OpenAdapter12` becomes reachable — **one commit** | `DECISIONS.md` §7.1 makes atomicity non-negotiable (R908) |
| **S6-0** | ⭐ **the stubbed table** — see below | it is the thing that makes fan-out safe |

### ⭐ S6-0 is the keystone: stub all 214 slots *before* any lane starts

Land `adapter12.rs`, `device12.rs` and `forward12/tables12.rs` with **every one of the 214 slots
filled by a counting noop**, plus an empty `install_<lane>()` per lane returning its `Filled*` token.

This converts the whole problem from *additive* to *substitutive*, and that is worth four things:

1. **No lane ever adds a slot** — it replaces a stub in its own file. Merge conflicts drop to one
   line per lane in `tables12.rs`.
2. **`D12-G7` (device creates, every slot non-NULL) is reachable before any lane lands**, so lanes
   start against a device that already works.
3. The noop hit counters are **already the metric**: `CONFORMANCE.md`'s charter is *drive the
   noop-DDI hit counters to zero*. Each lane's progress is measurable the day it starts.
4. It reuses `helios_umd_common::noop` (S2), so there is one stub idiom across both drivers.

⛔ **Use `stub_fill_bytes`, not `stub_fill_sized_table<T>`.** `d3d12umddi`'s `pfnFillDDITable`
passes a `SIZE_T` (`:2527-2528`) and the count must come from **that argument**, never from
`size_of::<T>()`. `ARCHITECTURE.md` §12 rule 16 / R702: 24H2 passed 576 bytes for a 592-byte
`DRIVERCAPS`. This is the single highest-consequence line in S6-0.

## 4. The lanes

Each lane owns its files exclusively. **No lane edits another lane's files.**

| # | Lane | Owns (under `umd12/src/`) | Device-core groups (§3.2) | CL groups (§4.2) | slots |
|---|---|---|---|---:|---:|
| **L1** | Caps | `caps12.rs` | (a) format/MSAA queries 3 | — | 3 + **43 caps types** |
| **L2** | Queue · pool · recorder · list lifetime | `forward12/queue.rs` | (d) 17 | — | 17 + **7 queue table** |
| **L3a** | Recording: draw + fixed-function + IA/SO/OM | `forward12/cmdlist.rs` | — | list lifetime 2, draw 3, FF state 11, IA/SO/OM 5, indirect/bundles 2 | 23 |
| **L3b** | Root arguments + descriptor binding + clears | `forward12/rootargs.rs` | — | root args 16, clears/discard 5 | 21 |
| **L3c** | Copy · resolve · barriers · queries | `forward12/copy.rs` | — | copy/resolve 7, barriers 2, queries/predication 4 | 13 |
| **L4** | Resources · heaps · residency · introspection | `forward12/resource12.rs` | (g) 11, (h) 5 | — | 16 |
| **L5** | Descriptor heaps + views | `forward12/descriptors.rs` | (f) 15 | — | 15 |
| **L6** | PSO · root sigs · shaders · sub-state | `forward12/pso.rs`, `shaders.rs` | (b) 12, (c) 14, (e) 12 | — | 38 |
| **L7** | Fences + query heaps | `forward12/fence.rs` | (i) 3, (j) 3 | — | 6 |
| **L8** | Present | `forward12/present12.rs` | 1 of (k): `pfnGetPresentPrivateDriverDataSize` | present/blt 2 | 3 |
| **L9** | The tail: meta-commands, state objects/RT/work graphs, VRS, mesh, scheduling groups, multi-adapter, policy | `forward12/misc.rs` | (k) 4, (l) 3, (m) 6, (n) 13, (o) 2 | markers/protection 4, meta 2, RT 5, VRS 2, mesh 1, work graphs 2 | 44 |
| — | *spine* | `adapter12.rs`, `device12.rs`, `tables12.rs` | — | — | **8 adapter** |

**Sum check** — device core `3+17+16+15+38+6+1+28 = 124` ✅ · command list
`23+21+13+2+16 = 75` ✅ · queue `7` ✅ · adapter `8` ✅ · **total 214** ✅
⚠ If you re-cut a lane, re-sum. `DDI_REFERENCE.md` §3.2 records a revision where two group counts
were both wrong and the errors cancelled.

### Suggested ordering when you have fewer agents than lanes

`L1` and `L2` first — caps decides whether the device is created at all (H4), and the queue is
where the WDDM context is minted. Then `L6`, `L5`, `L4` (a triangle needs a PSO, descriptors and a
resource), then `L3a`/`L3b`, then `L8`. `L3c`, `L7` and `L9` can trail; **`L9` is mostly
refuse-and-count** and is the natural first task for a new agent.

## 5. The merge protocol — how lanes stay out of each other's diffs

Four shared files exist. Each has an **append-only** discipline:

| shared file | a lane may add | never |
|---|---|---|
| `forward12/tables12.rs` | one line: `let t = queue::install(t);` | edit another lane's line, or reorder |
| `umd12/build.rs` | one `.file("bridge/vkd3d_bridge_<lane>.cpp")` and one `bridges([...])` entry | change flags, includes or the link set |
| `src/device12.rs` | one field on `HeliosD3D12Device`, at the **end** | reorder or repurpose existing fields |
| `src/knobs12.rs` | one knob + one `resolved_inventory()` entry, at the **end** | reorder (the inventory order is the evidence contract — S2) |

**Install order is structural, not textual.** Use the `#[must_use] Filled*` token pattern from
`umd/src/forward/tables.rs:44-70` (`ARCHITECTURE.md` §12 rule 9): correctness of every ≥11.1 device
once rested on `install()` running before `install_11_1()`, and the wrong order produced *"wrong
blending for DWM, no counter, no log, only pixels"*. Tokens make the wrong order **not compile** —
which is exactly what you want when eleven agents are appending to one sequencer.

**Each lane gets its own cxx bridge module.** `cxx_build::bridges([...])` accepts several, so a lane
that needs new engine calls adds `src/bridge12_<lane>.rs` with its own `#[cxx::bridge] mod ffi` and
`bridge/vkd3d_bridge_<lane>.cpp`. ⛔ Do **not** funnel every lane through one `bridge12.rs` — that
single file would otherwise be the contention point that serialises the whole fan-out. Precedent:
`umd/build.rs` already compiles `bridge_dxbc.cpp` and `bridge_icd_exports.cpp` as extra TUs off one
`cc::Build`.

⛔ **`bridge_guard` stays singular.** Lanes use the shared `umd_common/bridge/bridge_guard.h`; no
lane writes a second guard template, and no lane defines `HELIOS_BRIDGE_ENGINE_CATCH` (vkd3d throws
nothing). `grep -rn 'static_assert(' umd/bridge umd12/bridge umd_common/bridge` must stay at **1**.

## 6. The VM lease

One holder at a time, for: `win_install_umd`, `pnputil`, reboots, `schtasks` runs, Fire Strike, any
registry knob write, and `-Mode release`.

- **Claim** by writing `Z:\tmp\dx12\lanes\VM-LEASE` with lane name + UTC timestamp + intent.
- **Hold** for the shortest useful span; release immediately after.
- ⚠ **A stale lease is not permission.** If one is older than ~30 min, say so before breaking it —
  the holder may be mid-benchmark, and *a frozen benchmark is a defect to root-cause, never a
  retry*.
- **Gates are the integrator's, not a lane's.** `D12-G7`…`G11` are run once, by whoever holds the
  lease, against merged code. A lane reporting "my gate passed" on unmerged code has measured
  something nobody will ship.

⚠ Benchmarks run in **session 1** via a cloned scheduled task. A `win_exec` launch lands in session
0 and fakes a driver regression.

## 7. ⛔ Lane 0 — the tooling prerequisite. Fan-out is unsafe until this lands.

`tools/umd-check.ps1` today hardcodes:

```powershell
$mirror = 'C:\Users\Rupansh\helios-vgpu'                 # :28   one mirror
$env:CARGO_TARGET_DIR = "$mirror\$CrateDir\target"       # :83   one target dir
$log = "Z:\tmp\$CrateDir-$Mode.log"                      # :80   one log
robocopy "Z:\$sub" "$mirror\$sub" /MIR                   # :56   destructive
```

Two concurrent agents `/MIR` over each other's sources mid-build, share one target dir, and
overwrite one another's diagnostics. **The first symptom is a lane compiling code it did not
write.**

**Required change** — add a `-Lane <name>` parameter deriving all four:

| | single-agent (default, unchanged) | `-Lane l5` |
|---|---|---|
| source root | `Z:\` | `Z:\.lanes\l5\` (a git worktree) |
| mirror | `C:\Users\Rupansh\helios-vgpu` | `C:\Users\Rupansh\helios-lane-l5` |
| `CARGO_TARGET_DIR` | `<mirror>\<crate>\target` | `<mirror>\<crate>\target` |
| log | `Z:\tmp\<crate>-<mode>.log` | `Z:\tmp\lanes\l5\<crate>-<mode>.log` |

⚠ Omitting `-Lane` must behave **exactly** as today; every existing recipe in `ROADMAP.md` and the
gate scripts passes no such flag.

⚠ Disk: each lane mirror is a full `umd` + `umd12` + `umd_common` + `protocol` tree plus its own
target dir. Budget for it, or cap concurrency.

Agents should run with `isolation: "worktree"` so each has its own checkout; the worktree path is
what `-Lane` points at.

## 8. What must NOT be parallelised

- **The caps table (L1) is one agent's, whole.** `D3D12Core.dll` enforces ~60 cross-tier
  consistency rules (H4) and advertising an unbacked tier is *a lie the OS acts on*
  (`DECISIONS.md` §7.8). Splitting caps across agents is how two individually-plausible tiers become
  a rejected device with an English reason on ETW.
- **S5.** Four things in one commit, by one agent.
- **The present path (L8).** It touches the `HeliosPresentRenderCmd` identity channel shared with
  the KMD and the D3D11 driver. One owner, and it lands after L3a/L3b, not beside them.
- **`DECISIONS.md`, `ARCHITECTURE.md`, `GATES.md`.** Lanes propose; the integrator edits. Eleven
  agents editing the authoritative docs is how D4's "self-contained" claim happens again.
- **The version choice (`_0040` vs `_0110`).** One decision, made once, before fan-out —
  `_0110` carries thirteen `VulkanOn12` obligations that carry no cap and cannot be declined
  (`SUBSTRATE.md` §4.5).

## 9. Per-lane definition of done

A lane is done when **all** of:

1. Its slots are implemented or **explicitly refused with a named counter** — never silently
   stubbed. `CLAUDE.md` rule 2; a refusal uses `helios_umd_common::refusals::RefusalCounter` and its
   set's summary line, exactly as D3D11's `DDI refusals:` does.
2. Its noop hit counters read **zero** for its slots under a real workload.
3. Every `unsafe` carries a `// SAFETY:`; no `panic!`/`todo!`/`unwrap` on runtime data.
4. ⛔ If it calls `Slot<Boxed<S>>::get()`, it carries a **re-derived** D3D12 soundness argument at
   the call site. `umd_common::slot` states plainly that the `CUseCountedObject` ordering is
   established for D3D11 and **not** for D3D12. Do not inherit the claim.
5. It compiles clean in its own lane build, and the integrator has merged and re-run
   `-Crate both` — a lane that only ever built alone has not been integrated.
6. It touched **no** file it does not own, and its diff against `tables12.rs`/`build.rs`/
   `device12.rs`/`knobs12.rs` is append-only.

## 10. Integrator's checklist per merge

- `grep -rn 'static_assert(' umd/bridge umd12/bridge umd_common/bridge` → **1**
- `umd-check.ps1 -Mode check -Crate both` → 0 errors, and `umd`'s warning count **unchanged**
- the knob inventory still byte-identical for **`umd`** (S2's instrument,
  `tools/capture-knob-inventory.ps1`) — a lane that perturbs the D3D11 driver has broken the split
- `OpenAdapter12` refuses until S5; after S5, `UmdD3D12` defaults OFF
- Fire Strike 3-run median at parity — ⚠ D3D12 work must not regress D3D11, and the two DLLs share
  the ICD
