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
  `CARGO_TARGET_DIR` and **one** log path, and `robocopy /MIR`s destructively — so **concurrent
  VM builds are not safe either.**

⇒ **Authoring and compiling parallelise. Validating does not.**
⭐ And they parallelise *without touching the VM at all*: every lane's inner loop is
edit → `tools/umd12-host-check.sh` **on the Linux host** (§7). That sidesteps the
whole list above — no mirror, no adapter, no knobs, 7 seconds. Deploy, gate and benchmark go through
a **lease** (§6), and there are far fewer of those than there are edits.

⚠ "N agents, N× throughput" is still wrong. Expect the fan-out to help most through S6's long
authoring middle, and to collapse back to serial at every gate.

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

⭐ **The DDI typedefs are `extern "C"`, not `extern "system"`.** Measured by fault-injecting a wrong
signature against the host cross-check:

```
expected fn pointer `unsafe extern "C" fn(D3D12DDI_HCOMMANDLIST, u32, u32, u32, u32) -> ()`
   found fn pointer `unsafe extern "system" fn(u8) -> u8`
```

On x86_64 Windows the two are the same ABI, so this is a *type* error and not a calling-convention
bug — which is exactly why it would have been written wrong 214 times and caught by nothing until
the first compile. ⛔ Declare every handler `unsafe extern "C"`. ⚠ Note this differs from the
D3D11 side's exported entry points, which are `extern "system"`.

⛔ **`bridge_guard` stays singular.** Lanes use the shared `umd_common/bridge/bridge_guard.h`; no
lane writes a second guard template, and no lane defines `HELIOS_BRIDGE_ENGINE_CATCH` (vkd3d throws
nothing). `grep -rnE '^[[:space:]]*static_assert\(' umd/bridge umd12/bridge umd_common/bridge` must stay at **1**.

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

## 7. ⭐ Lanes compile on the LINUX HOST — no VM, no WDK, no contention

**This is what makes the fan-out work, and it is already landed and proven.**

```bash
rustup target add x86_64-pc-windows-msvc          # once, on the Linux host
tools/umd12-host-check.sh                         # 7.4 s, full DDI surface
tools/umd12-host-check.sh --message-format short  # args forward to cargo check
```

⚠ **Use the script, not a bare `cargo check`** — since S4 put `cxx` in the tree, the bare command
does not work. `cxx` pulls `link-cplusplus`, whose build script runs a `cc::Build` probe for the
*target*; cross-compiling to `x86_64-pc-windows-msvc` from Linux there is no MSVC toolchain and it
dies with ``failed to find tool "lib.exe"``, taking the whole check down. The script supplies
cargo's first-class `[target.<triple>.<links>]` build-script overrides for the two `links` keys
involved (`cplusplus` = `link-cplusplus`, `cxxbridge1` = `cxx`) so neither build script runs.
⛔ Those overrides are passed with `--config` **on the command line and never committed to
`.cargo/config.toml`**: that file is read on **both** platforms (`CLAUDE.md`), so an override there
would also skip cxx's build script on the real Windows build and silently produce a
`helios_umd12.dll` with no bridge object in it. ⛔ For the same reason the script is `check` only —
with the build scripts elided there is no `cxxbridge1` static lib to link, so a `build` here would
be a lie; the shipping build stays `tools\umd-check.ps1 -Mode release -Crate umd12` on the VM.

The obvious objection is *"the WDK is not on Linux"*, and it is correct — but **bindgen does not
need to run there.** It runs on the VM, where the WDK is, and the host only needs the *generated
Rust*. `umd12/build.rs` serves `umd12/bindgen/cached/d3d12umddi.rs` (5.4 MB, committed) into
`OUT_DIR` when the build host is not Windows, so `ddi12.rs`'s `include!` resolves and all **1 904
layout assertions plus every one of the 214 slot signatures** type-check on the host.

⛔ **The cache never builds a shipping DLL.** On Windows the bindings are regenerated from
`d3d12umddi.h` every time and the cache is only *compared* against; a mismatch emits
`cargo:warning=… is STALE …`, so drift is loud rather than silent. The SDK header remains the single
source of truth, and `PARALLEL.md` §10 still requires the integrator to re-check on the VM.

**What this buys, concretely:** eleven agents each get a **7-second compiler answer** on their own
machine-free loop, instead of writing 214 handlers blind and discovering the errors in one avalanche
at the end. On a transcription job against someone else's ABI, the compiler *is* the specification.

**What it does NOT cover** — these are the final pass's job (§11):

| covered on the host | only on the VM |
|---|---|
| every Rust type and DDI signature | the cxx bridge's **C++** compilation |
| the 1 904 layout assertions | linking, and the vkd3d archive link set |
| trait/borrow/lifetime errors | bindgen regeneration from the real header |
| `clippy`, `rustfmt`, dead-code | anything that runs: G7…G11, Fire Strike, the desktop |

⚠ **Refresh discipline.** When the SDK pin moves, regenerate on the VM and copy
`$OUT_DIR/d3d12umddi.rs` over the cache in the same commit. Until then every host check is measuring
a different ABI than the one being shipped — which is precisely the failure the `STALE` warning
exists to make impossible to miss.

⚠ Agents should still run with `isolation: "worktree"` so their edits do not collide in the source
tree. They do **not** need a VM mirror, a `CARGO_TARGET_DIR` of their own on Windows, or `-Lane`
plumbing in `umd-check.ps1` — that whole prerequisite dissolved.

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

## 10. ⭐ The final pass — fanned out too, on two axes

Host cross-checking (§7) removes the type errors before merge. What it cannot remove is everything
that is not a Rust type. That review is itself large — 214 handlers across a dozen files — so it
**also fans out**. Only **B** and **C** need the VM and a single holder.

### ⛔ Split reviewers by LENS, not by file

The obvious split — one reviewer per file — is the weak one. A reviewer holding the same slice the
author held **shares the author's blind spot**, and in a flat DDI table the defects that matter are
cross-file: a handle stored in `resource12.rs` and misread in `descriptors.rs`, two lanes' fields
colliding in `device12.rs`, an `install_*` ordering hazard visible only in `tables12.rs`. Eleven
file-scoped reviewers also re-derive the same context eleven times and each miss the seams.

So: **mechanical checks split by file; semantic checks split by lens, each reading the whole diff.**

### A1 — mechanical sweep, split by file, cheap and high-volume

⭐ **Script what is scriptable before spending an agent on it.** These are near-syntactic; a
reviewer adds nothing a `grep` does not, and an agent's attention is better spent on A2.

| invariant | scar |
|---|---|
| every `unsafe` has a `// SAFETY:` | `CLAUDE.md` rule 4 |
| no `panic!` / `todo!` / `unimplemented!` / `.unwrap()` / `.expect()` on runtime data | a panic in any DDI is a **silent graphics deadlock**; `panic = "abort"` makes it a dead compositor |
| no `#[allow(...)]` on a hand-written line | generated code may be allowed, hand-written code may not — R908 |
| `grep -rnE '^[[:space:]]*static_assert\(' umd/bridge umd12/bridge umd_common/bridge` → **1** | `ead692e`. ⚠ the **anchor** is what works — both the bare word and the trailing-paren form count the comments that quote them, and reported 3. ⛔ never `git grep`: it skips untracked files, so a new `umd12/bridge/` reads 0 |
| `tools/umd12-host-check.sh --clippy -- -D warnings` | 214 hand-written handlers is where `missing_safety_doc` earns its keep. ⚠ **Through the script, not a bare `cargo clippy`** — the bare form dies in `link-cplusplus`'s build script with an error naming `lib.exe` and nothing about clippy (§7), so a lane reads it as a broken tree and drops the row. It caught a real one the moment it was wired up: `umd12`'s `OpenAdapter12` had no `# Safety` section |
| `git diff` on the four shared files is append-only | §5 |

Agents take the residue — the judgement calls a grep flags but cannot settle (*is this `.unwrap()`
on runtime data or on a compile-time constant?*).

### A2 — semantic review, split by lens, each agent reads the WHOLE merged diff

| lens | looking for | the scar that justifies it |
|---|---|---|
| **ABI & tables** | slot **index** vs member index; table size from the runtime's `SIZE_T` not `size_of::<T>()`; `extern "C"` not `extern "system"`; no hand-transcribed struct | `DECISIONS.md` §4.1's "slots 38-40" was a `sed` line offset misread as a member index. R702: 24H2 passed 576 B for a 592 B struct. R908 |
| **Handles & lifetimes** | payload type **derived** from the handle type, never chosen at the call site; every `Slot<Boxed<S>>::get()` carrying a **re-derived** D3D12 argument | §12 rule 7 — `load_com::<ID3D11RenderTargetView>(h_rtv)` compiled and produced a `ManuallyDrop` whose vtable pointer was a struct field: a wild call on first use |
| **Loud failure** | every refusal counted **and readable** — a counter that appears in no summary is not an instrument | T5: three of four scan-out counters were atomics **nothing ever loaded**, so ROADMAP's own instruction to read them was not executable |
| **Concurrency** | state touched from create/destroy DDIs under FREETHREADED; anything that refills a live table | §12 rule 10 — `RelocateDeviceFuncs` is a **NOTIFICATION**; the old refill made a concurrent `CalcPrivate*Size` return 0 → zero-byte private region → heap corruption |
| **Cross-lane seams** | duplicate/renamed `install_*`, colliding `device12.rs` fields, install **order** correctness, knob-inventory order | §12 rule 9 — install order once rested on textual sequence and the wrong order gave *"wrong blending for DWM, no counter, no log, only pixels"* |
| **Engine contract** | ownership across the cxx boundary; every bridge entry through `bridge_guard`; owned-vs-borrowed COM | R815 — cxx generates raw methods as **inherent** methods; module privacy does not seal them |
| **Claim integrity** | every number in a comment or doc re-derived, not inherited | ⭐ this session: D4's "self-contained" survived because a symbol search was mistaken for a link, and a `git grep` check counted its own documentation |

Seven lenses is a good width. With fewer agents, merge lenses rather than dropping them — **ABI &
tables** and **Loud failure** are the two that must always run.

### The finding protocol — without it, N reviewers cost more than they save

1. **Dedup before routing.** Seven whole-diff reviewers *will* report the same defect. The
   integrator merges by (file, line, claim), keeping the clearest statement.
2. ⛔ **Adversarially verify before routing.** A plausible-but-wrong finding costs a lane owner real
   time and teaches them to discount the next one. Each surviving finding gets a skeptic asked to
   **refute** it; default to refuted when uncertain. `refactor-review-phase1-40th` is the precedent —
   300 findings, and the ones that mattered were the ones that survived being argued with.
3. **Route to the owning lane, by file ownership (§4).** The reviewer does not fix.
4. **A finding without a failure scenario is a suggestion.** State the inputs and the wrong
   behaviour, or file it as a nit.

⚠ The owner can also run `/code-review ultra` on the branch — a multi-agent cloud review — but that
is **user-triggered and billed**; agents cannot launch it and must not try.

### B. Compile / build analysis — VM, lease held, ONE holder

| check | why |
|---|---|
| `umd-check.ps1 -Mode check -Crate both` → 0 errors | the **C++** bridge TUs compile here and nowhere else |
| `umd` warning count **unchanged** | a D3D12 lane that perturbed the D3D11 driver has broken the split |
| no `… is STALE …` warning from `umd12/build.rs` | the committed bindings cache still matches the SDK header ⇒ every host check the lanes ran was against the shipping ABI |
| `-Mode release -Crate both`, then `dumpbin /IMPORTS` | ⛔ **no `dxgi.dll`**, ever. `umd/build.rs:239-243` |
| the link set is still `libhelios_d3d12_static.a` + `gdi32` | anything else appearing means a lane pulled in an object it should not have |
| cold `cargo check` wall time and generated-file size, tracked | UNVERIFIED-2's numbers are the baseline; a sudden jump means an allowlist widened |

### C. Runtime — VM, lease held, and only after A and B are clean

`D12-G7` → `G8` → the id-1000 cold boot → Fire Strike 3-run median for the **D3D11** parity check.
⚠ Report what happened, including partial failures. A lane whose slots still hit the noop counters
is **not done** (§9.2), and saying so is the whole value of this pass.

⛔ **No reviewer fixes what it finds, and no lane reviews its own files.** An agent that both
writes and audits its own slots is the review-your-own-homework failure the whole pass exists to
prevent — which is also the second reason A2 splits by lens: a lens crosses every lane, so no
reviewer can be scoped to the code it wrote.

⚠ **A and C have very different costs.** A1+A2 are free and parallel — run them on every merge. B is
cheap but serial. **C is the expensive serial tail**: if lanes land in a batch, C runs once, not
once per lane.

## 11. Integrator's checklist per merge

- `grep -rnE '^[[:space:]]*static_assert\(' umd/bridge umd12/bridge umd_common/bridge` → **1**
- `umd-check.ps1 -Mode check -Crate both` → 0 errors, and `umd`'s warning count **unchanged**
- the knob inventory still byte-identical for **`umd`** (S2's instrument,
  `tools/capture-knob-inventory.ps1`) — a lane that perturbs the D3D11 driver has broken the split
- `OpenAdapter12` refuses until S5; after S5, `UmdD3D12` defaults OFF
- Fire Strike 3-run median at parity — ⚠ D3D12 work must not regress D3D11, and the two DLLs share
  the ICD
