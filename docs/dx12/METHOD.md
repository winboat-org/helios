# METHOD.md — how D3D12 work is done, and why it is not done by probe

> ⛔⛔ **OWNER DIRECTIVE, 2026-08-06. This document is authoritative over the *sequencing* in every
> other file in `docs/dx12/`.** Where `GATES.md`, `PARALLEL.md`, `DX12.md` §4 or `KMD_IMPACT.md`
> §14a describe an order of work that contradicts the loop below, this file wins and the other
> document is stale. Nothing here overrides `DECISIONS.md`, which remains authoritative for
> *architecture*.
>
> Owner, verbatim: *"I think the incremental approach is not working out. What we need to do is
> implement as much of the umd correctly as possible (along with KMD changes), then throw an
> adversarial review on the changeset, rinse and repeat until we saturate static analysis fully and
> are comfortable with deploying the umd and kmd."*
>
> And on the target: *"Rendering triangle is useless (other than making me or you feel good) unless
> we can render REAL DX12 apps and benchmarks."*

---

## 1. The loop that was rejected, stated so it is recognisable

The owner's description of what this project had been doing, verbatim:

> - UMD changes done
> - Run a random probe
> - Keep repeating until the probe passes
> - Completely ignores contract violations and hacks until a new probe comes into play
> - We go back to the original code and repeat this loop

**Why it fails is not that probes are bad.** It is that *"the probe passes"* and *"the
implementation is correct"* are different predicates, and iterating against the first one optimises
for the first one. Three things follow, and all three have already happened here:

1. **A probe can pass while the defect it was written for is untouched.** The canonical case is on
   the record: with `Umd12EclDelayUs=50000`, `D12-G8` rung 0 **passed** — correct pixels — while the
   application's fence wait stayed at **0.6 µs**. The pixels arrived because a CPU sleep shifted
   everything in time; the ordering dependency the rung existed to establish did not exist.
   *A gate that reads only the exit code would have called that a fix.*
2. **Contract violations are invisible to probes, and they accumulate.** A probe exercises the path
   it exercises. `pfnSetCommandListErrorCb` sitting one field below `pfnSetErrorCb` was copied into
   49 sites and would have removed the whole `ID3D12Device` where the contract is to quarantine one
   command list — and no probe in the ladder would have failed. The 82nd session caught it by
   *reading*, not by running.
3. **It rewards the smallest change that moves the probe**, which is the definition of a hack, and
   hacks are explicitly forbidden here (`idd-code43-double-delete-rootcause` — *NO MORE HACKS*).

⛔ **And it is the wrong shape for the target.** The goal is real D3D12 applications and benchmarks,
which exercise hundreds of contract obligations at once. An implementation grown one probe at a time
is shaped like the probes, not like the contract — so the first real app finds a hundred gaps
simultaneously, in a configuration where none of them can be attributed.

---

## 2. The loop

```
  ┌─ 1. IMPLEMENT BROADLY, TO THE CONTRACT ─────────────────────────────┐
  │    A whole subsystem, UMD + KMD + ICD + engine together.            │
  │    Every obligation implemented or refused-with-a-counter.          │
  └──────────────────────────┬──────────────────────────────────────────┘
                             ▼
  ┌─ 2. ADVERSARIAL REVIEW OF THE WHOLE CHANGESET ──────────────────────┐
  │    Fan out by LENS, not by file. Every finding refuted before it    │
  │    is routed. Plus a completeness critic.                           │
  └──────────────────────────┬──────────────────────────────────────────┘
                             ▼
  ┌─ 3. REPAIR ─────────────────────────────────────────────────────────┐
  │    The author repairs; the reviewer never does.                     │
  └──────────────────────────┬──────────────────────────────────────────┘
                             ▼
                   ┌─── not saturated ───┐
                   │                     │  back to 2 with DIFFERENT lenses
                   └──── saturated ──────┘
                             ▼
  ┌─ 4. DEPLOY, AND TREAT THE FAILURE AS DIAGNOSIS ─────────────────────┐
  │    A BSOD or a dead DWM is an input, not a setback.                 │
  └─────────────────────────────────────────────────────────────────────┘
```

### Phase 1 — implement broadly, to the contract

**The unit of work is a subsystem, not a slot and never a probe.** "The fence/completion bridge",
"the present identity path", "descriptor heaps end to end". It spans crates deliberately: the D3D12
work has repeatedly turned out to need `umd12` + `kmd_render` + `icd/mesa` + the vkd3d fork *in one
piece*, and splitting it by crate is what produced changesets that could not be reasoned about.

Entry: the contract is written down. `DDI_REFERENCE.md` for the DDI surface, `KMD_IMPACT.md` for the
kernel side, the WDK headers and vkd3d's own source for everything they cover. ⛔ **Read the engine
and the headers before writing.** The 82nd session settled three items filed as open from source
alone, and one of them was an obligation vkd3d had *already discharged* — implementing it would have
issued the state twice.

Exit, per obligation in the subsystem's contract — every one, not the ones a probe reaches:

* implemented and forwarding, **or**
* refused with a named counter and a documented error code (CLAUDE.md rule 2), **or**
* explicitly recorded as unreachable, with the argument for why.

⛔ **"Implemented but never exercised" is not done, and must never be reported as done.** It is a
distinct third state and it must be named as such in the report.

### Phase 2 — adversarial review of the whole changeset

The protocol is `PARALLEL.md` §10, which is promoted here from *"the final pass for P3"* to *the
standing review of every changeset*. Its essentials, restated because they are the load-bearing part:

* **Mechanical checks split by file; semantic checks split by LENS, each reading the whole diff.** A
  reviewer holding the author's slice shares the author's blind spot, and the defects that matter in
  a flat DDI table are cross-file.
* **Script what is scriptable** before spending an agent on it (`§10 A1`'s table: `unsafe`/`SAFETY`,
  no panic paths on runtime data, `-D warnings` through `umd12-host-check.sh`, slot coverage, the
  ASCII-log check, append-only shared files).
* ⛔ **Every finding is adversarially verified before it is routed.** Each surviving finding gets a
  skeptic asked to **refute** it, defaulting to refuted when uncertain. Precedent:
  `refactor-review-phase1-40th` — 300 findings, and the ones that mattered were the ones that
  survived being argued with; and the 81st's §10 pass — 27 raw, **21 rejected**, 6 real including a
  blocker.
* **A finding without a failure scenario is a suggestion.** Concrete inputs → wrong behaviour, or it
  is a nit.
* ⛔ **No reviewer fixes what it finds, and no author reviews its own files.**

**Lenses.** `PARALLEL.md` §10's seven — ABI & tables, handles & lifetimes, loud failure,
concurrency, cross-lane seams, engine contract, claim integrity — plus two this project's history
demands:

| lens | looking for | the scar |
|---|---|---|
| **Contract completeness** | an obligation in `DDI_REFERENCE.md` / the WDK header that the changeset neither implements nor refuses. Reads the *contract* against the diff, not the diff against itself | the probe-driven loop's defining failure: nothing looks for what no probe reached |
| **Instrument attribution** | a counter or measurement read as evidence for something it cannot attribute | three in one family in one session: `WfBWire`, `RING_SUBMIT_COUNT` and `RENDER_COUNT` were each named as *the* test for a D3D12-specific question, and all three are adapter-global with DWM always running |

### Phase 3 — repair, by the author

Routed by file ownership. The reviewer does not fix. A repair that changes a claim must change the
claim's *documentation* in the same commit — the stale-claim failure has recurred in four
consecutive sessions.

### Phase 4 — deploy, and treat the failure as diagnosis

⭐ **Owner directive: these are non-issues.**

> *"KMD changes cause BSOD → No problem, its a dev box, we reboot without virtio gpu, diagnose, fix,
> and continue. UMD crashes DWM → No problem, its a dev box, we diagnose, fix and continue."*

**Consequences, which are not merely permissions but requirements:**

1. ⛔ **Fear of a crash may not shape the implementation.** A knob whose default was chosen to keep a
   run alive rather than to be correct is a hack wearing a knob's clothes. Concretely: a refused
   `pfnRenderCb` is a device-removing error because the contract says so, and it ships that way —
   not gated OFF "so the first run still produces a reading".
2. **Loud failure beats a survivable lie, at every severity.** This was always CLAUDE.md rule 2; the
   directive removes the last excuse for softening it.
3. ⛔ **But a crash is still not a substitute for reading.** "Deploy and see" is the rejected loop
   with a bigger blast radius. Phase 4 happens *after* saturation, and a crash that static analysis
   should have caught is a finding **about the review**, to be fed back as a new lens.

**What survives from `GATES.md` §1 unchanged, because it is about reading evidence and not about
sequencing:** session 0 fakes driver regressions; registry counters persist across boots so verify a
counter *moved this boot*; only owner-visible desktop state is rendering evidence; never
`VKD3D_FEATURE_LEVEL` in a pass criterion; a frozen benchmark is a defect to root-cause, never a
retry; refuse to read an exit code alone; never blame the host without host-side evidence.

---

## 3. What "saturated" means, operationally

This is the criterion the loop turns on, so it is stated as a test and not as a feeling.

**A review round is DRY when every finding it produced is either refuted, or already recorded as an
accepted decision.** Zero *new real* findings.

**Static analysis is SATURATED when all of the following hold:**

| # | criterion | why it is not redundant |
|---|---|---|
| 1 | **Two consecutive dry rounds with *different* lens compositions** | two identical dry rounds prove the lenses are exhausted, not the code. Rotate at least two lenses between rounds |
| 2 | **The completeness critic returns nothing**: no contract obligation unaddressed, no claim unverified, no file unread by any lens | a round can be dry because nobody looked |
| 3 | **Every mechanical check passes as an exit code**, not as a human's reading | `§10 A1`'s table. A grep check has twice counted its own documentation here — a check that is not an exit code is an opinion |
| 4 | **Every counter the changeset adds is readable and graded**, and every grading was re-checked at the END of the merge | a grading is a claim and goes stale; one went stale *inside the merge that corrected it* |
| 5 | **No claim in the diff's comments is inherited** — every number and `file:line` re-derived against the merged tree | citations here drift 20–60 lines under concurrent edit, and a wrong citation is worse than none |
| 6 | **The changeset's own report distinguishes implemented / refused / unreachable / *implemented-but-never-exercised*** | the fourth category is where a probe-shaped implementation hides |

⚠ **Saturation is not a proof of correctness.** It is the point at which further static analysis has
stopped paying, and the cheapest remaining information is on the VM. Deploy then — and expect to
learn something, because criteria 1–6 cannot see dxgkrnl's or the host GPU's behaviour.

### What static analysis provably cannot settle here, and therefore must not be iterated on

Naming these prevents a round burning on a question no amount of reading answers:

* **dxgkrnl's internal behaviour** — whether it orders the runtime's monitored-fence signal behind
  our DMA packets (**UV1**), whether it truncates `DRIVER_INITIALIZATION_DATA` at the declared
  version (U1), whether it accepts a `NumAllocations = 0` render on a legacy context. Closed source.
  These need one deliberate experiment each, designed to be *unambiguous* — which is why the
  `WddmHoldMs` hold exists and why the bare submission's flat reading was rejected as an answer.
* **The runtime's tolerance of caps and addresses it never issued** (U2, U4, U7).
* **Real host GPU timing.** Never inferred, always measured, and never blamed without host evidence.

Everything else — the DDI contract, the engine's obligations, the wire ABI, the transport's
retirement domains, the KMD's own invariants — **is readable, and reading it has out-performed
running it in every session that tried both.** UV3 was answered from source this session after being
filed as needing a run; three items in the 82nd were settled the same way.

---

## 4. How this changes the other documents

| document | status under this method |
|---|---|
| `DECISIONS.md` | **unchanged and still authoritative** for architecture |
| `PARALLEL.md` §10 | **promoted**: no longer "P3's final pass", it is the Phase 2 protocol for every changeset. §9's per-lane definition of done becomes the Phase 1 exit criterion |
| `GATES.md` | **demoted from ladder to acceptance suite.** Its §1 evidence rules stand verbatim. Its gates are no longer the order of work and no longer drive implementation: a gate is run *after* saturation, to accept a subsystem, and **a gate passing is not evidence that the code is right** — see the `Umd12EclDelayUs` case in §1 above |
| `DX12.md` §4 | its phase table is gate-indexed and therefore describes the rejected order. The phases survive as *scope*; the sequencing is this file |
| `KMD_IMPACT.md` §14a.1 | ⛔ its *"Do not write anything below the experiment until the experiment has run"* is exactly the rejected loop, and its own three-reading table has since been shown unsound. Implement the subsystem; the experiment is a diagnostic carried alongside, not a gate in front |
| `CONFORMANCE.md` | unaffected — it is a charter of obligations, which is what Phase 1 consumes |

---

## 5. Anti-patterns, each with its scar

* ⛔ **Iterating until a probe passes.** `Umd12EclDelayUs=50000` made rung 0 pass with the defect
  fully intact.
* ⛔ **A stopgap "for now".** Design A in `tmp/dx12/FENCE-BRIDGE-DESIGN.md` was designed, costed and
  rejected by the owner: *"stop gaps are not acceptable … do it right the first time."*
* ⛔ **Eliminating a suspect without stating the inference.** The `--sentinel` round proved a mapping
  self-consistent and was read as proving the GPU had written it. *An elimination is an argument, and
  arguments have premises.*
* ⛔ **Trusting a zero.** *"The Signal path ran"* rested on three counters reading 0; instrumented,
  `pfnSignalFence` turned out never to be called at all.
* ⛔ **Reading an adapter-global counter as client-specific.** See the instrument-attribution lens.
* ⛔ **Believing a doc claim because it is written down.** Four consecutive sessions have paid for a
  claim that was true when written. In this session alone: an unrunnable free experiment, an
  experiment whose table could not decide its own question, a prescribed guard that would have caused
  a regression, an ICD accessor that would have ordered nothing, and a *"nothing builds"* line that
  had stopped a lane from verifying anything in the vkd3d fork.
* ⛔ **A knob default chosen for survivability.** See §2 Phase 4 consequence 1.
* ⛔ **Reporting a subsystem done when its slots are implemented but never exercised.** Saturation
  criterion 6.
