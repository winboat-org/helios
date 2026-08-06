//! The D3D12 UMD's `HKLM\SOFTWARE\Helios` registry knobs.
//!
//! ⛔ **The knob VALUES are per-crate and that is D3b's explicit instruction**
//! (`helios_umd_common::knobs` says the same at its own head): only the
//! *mechanism* — the single audited `advapi32!RegGetValueA` site, [`BoolKnob`]
//! and `DwordKnob` — is shared. Sharing the *table* would mean one driver's A/B
//! lever silently applying to the other, and `UserModeDriverName[3]` is supposed
//! to be the only coupling between `helios_umd.dll` and `helios_umd12.dll`.
//!
//! ⚠ Which is why the trace knob here is `Umd12Trace` and **not** `UmdTrace`.
//! They are different registry values in the *same* hive: the hive is shared on
//! purpose (the owner types these by hand and a second subkey would be a second
//! thing to remember), the value names are not. `UmdTrace=1` must not turn on
//! per-op chatter in the D3D12 driver, because the two logs are read separately
//! (`umd-<pid>.log` vs `umd12-<pid>.log`) and a shared name would make "trace
//! was on" ambiguous across two files.
//!
//! | Value | Type | Absent |
//! |---|---|---|
//! | `Umd12Trace` | DWORD | `false` (explicit non-zero enables) |
//! | `UmdD3D12` | DWORD | `false` — **the D3D12 kill switch** (D11) |
//! | `Umd12FormatCaps` | DWORD | `0` — `pfnCheckFormatSupport`'s encoding, as an A/B |
//! | `Umd12FenceSignalDelayUs` | DWORD | `0` — **diagnostic**, the F1 delay probe on `pfnSignalFence` |
//! | `Umd12EclDelayUs` | DWORD | `0` — **diagnostic**, the F1 delay probe on `pfnExecuteCommandLists` |
//! | `Umd12EclSubmit` | DWORD | **`1` — ON.** K-F1's `pfnRenderCb` WDDM submission during `pfnExecuteCommandLists` |
//! | `Umd12EclSubmitStrict` | DWORD | `0` — **OFF during bring-up.** Whether a refused `pfnRenderCb` removes the `ID3D12Device` |
//!
//! ⭐ **`UmdD3D12` lands here at S5, and not one commit earlier.** A kill switch
//! for a driver that cannot be reached kills nothing, so declaring it before
//! slot 3 existed would have put a knob in the inventory line whose value
//! provably had no effect — the `DECISIONS.md` §7.1 / R908 failure mode in
//! miniature. It arrives in the same commit that registers
//! `UserModeDriverName[3]`, deletes `umd`'s duplicate `OpenAdapter12` export and
//! makes `adapter12::OpenAdapter12`'s body reachable.

use helios_umd_common::knobs::{BoolKnob, DwordKnob};

/// ⭐ **`pfnCheckFormatSupport`'s ENCODING, as an A/B rather than an assumption.**
///
/// `d3d12umddi.h` defines a 20-bit `D3D12DDI_FORMAT_SUPPORT` enum immediately
/// beside `PFND3D12DDI_CHECKFORMATSUPPORT`, whose values are byte-for-byte the
/// D3D10 DDI's. That is strong evidence the DDI has its own small encoding — but
/// it is *evidence*, not a measurement, and the D3D11 side of this project holds
/// the opposite result in as many words:
///
/// > "The D3D11 DDI `pfnCheckFormatSupport` returns API-style
/// > `D3D11_FORMAT_SUPPORT` flags (D3D11 harmonized the DDI with the API enum;
/// > the small `D3D10_DDI_FORMAT_SUPPORT` enum is only for the legacy D3D10
/// > DDI). So pass DXVK's value through unchanged -- translating to the D3D10
/// > DDI layout regresses even a plain `D3D11CreateDevice` to
/// > `DXGI_ERROR_UNSUPPORTED`."  -- `umd/src/forward/format_caps.rs:15-19`
///
/// If D3D12 inherited that harmonization, translating is the same mistake one
/// API generation later, and it would present exactly as `D12-G7` does: a
/// device-creation failure whose ETW reason moves every time the answer changes.
/// ⛔ This knob exists so that question is settled by a measurement instead of a
/// third guess, and so the losing arm stays reachable afterwards (CLAUDE.md
/// rule 8's other half).
///
/// | value | meaning |
/// |---:|---|
/// | 0 | **`D3D12DDI_FORMAT_SUPPORT`** -- translate the engine's API bits into the DDI enum and narrow them to this driver's caps. The default. |
/// | 1 | **API passthrough** -- hand the engine's `D3D12_FORMAT_SUPPORT1` back unchanged, exactly as the D3D11 driver does with DXVK's. ⛔ **MEASURED AND LOSING** (2026-08-06): it truncates the runtime's format sweep at 12 formats / 271 multisample queries, against 23 / 600 for arm 0. So the D3D12 DDI is **not** harmonized with the API enum the way D3D11's is, and arm 0 is right. Kept reachable as rule 8 requires. |
///
/// ⚠ Two further arms existed briefly (2: no multisample bits anywhere; 3:
/// multisample bits only alongside `RENDERTARGET`) purely to bisect which bit
/// the runtime was rejecting. **They are gone**, because the rule they were
/// hunting turned out to be written down already — `msaa_ineligible` in
/// `umd_common/src/format.rs`, which the D3D11 driver paid for. A diagnostic
/// arm that outlives its question is scaffolding.
///
/// ⚠ The default is 0 and stays 0 until an arm is measured green; flipping it
/// requires the evidence written at the read site in `caps12`.
pub(crate) static UMD12_FORMAT_CAPS: DwordKnob = DwordKnob::new(c"Umd12FormatCaps", 0);

/// The `pfnCheckFormatSupport` encoding mode. See [`UMD12_FORMAT_CAPS`].
pub(crate) fn umd12_format_caps() -> u32 {
    UMD12_FORMAT_CAPS.get()
}

/// Per-op/per-frame DDI chatter (`trace_line!`) for the D3D12 driver.
/// Absent = OFF.
pub(crate) static UMD12_TRACE: BoolKnob = BoolKnob::new(c"Umd12Trace", false);

/// **The D3D12 kill switch** (`DECISIONS.md` D11). Absent = OFF.
///
/// Read once per process at the top of `adapter12::OpenAdapter12`, above every
/// other check including the null test. Absent ⇒ `DXGI_ERROR_UNSUPPORTED`, i.e.
/// **bit-identical behaviour to a build with no D3D12 path at all**: nothing is
/// dereferenced, no table is written, and the only trace is the
/// `OpenAdapter12` refusal counter ticking.
///
/// ⚠ **The default is a decision** (CLAUDE.md rule 8), and this one is OFF
/// because `dwm.exe` already calls `OpenAdapter12` on the Helios adapter in
/// production (`DECISIONS.md` §7.13). The first boot with `UmdD3D12=1` is a
/// change to the compositor's behaviour, not a change to a test app's.
///
/// ⛔ **Flipping this default to ON requires the evidence in a comment right
/// here** — the D12-G7…G11 ladder, a cold boot with zero `helios_umd12.dll`
/// entries in the id-1000 Application log, and a Fire Strike 3-run median at
/// D3D11 parity. Until then the opposite value stays reachable as the A/B
/// disable, which is the other half of that rule.
///
/// ⚠ Read once per process, deliberately: a running `dwm` keeps whatever
/// behaviour it started with while newly created processes pick the change up.
/// `HKLM\SOFTWARE\Helios` is writable over SSH with the desktop down, so the
/// switch is usable in exactly the situation it exists for.
pub(crate) static UMD_D3D12: BoolKnob = BoolKnob::new(c"UmdD3D12", false);

/// Resolve `HKLM\SOFTWARE\Helios!Umd12Trace` (REG_DWORD) != 0, forcing its
/// `OnceLock`. Read once per process.
///
/// ⚠ This is the KNOB. The GATE that `trace_line!` consults is
/// `helios_umd_common::log::trace_enabled()`, which caches this answer in a
/// relaxed `AtomicBool` when `log::init` runs — see `crate::init_once`. The
/// split is the same one `umd/src/knobs.rs:222-236` documents: the gate must be
/// one relaxed load, not a `OnceLock` walk through a knob table the shared crate
/// cannot see.
pub(crate) fn umd12_trace() -> bool {
    UMD12_TRACE.get()
}

/// Resolve `HKLM\SOFTWARE\Helios!UmdD3D12` (REG_DWORD) != 0, forcing its
/// `OnceLock`. Read once per process, at the top of
/// `adapter12::OpenAdapter12`.
pub(crate) fn umd_d3d12() -> bool {
    UMD_D3D12.get()
}

/// The largest delay either diagnostic arm below will honour, in microseconds.
///
/// ⚠ 2 s, and the number is not arbitrary: it is the `--settle 2000` window that
/// demonstrated the pixels arriving late in `tmp/dx12/gates/G8-r0-settle/`. A
/// value above it is clamped rather than refused, because a mistyped registry
/// DWORD must not be able to hang a DDI for the rest of the boot — the arm is a
/// measurement, and a measurement that wedges the machine produces nothing.
const MAX_DIAGNOSTIC_DELAY_US: u32 = 2_000_000;

/// ⛔⛔ **A DIAGNOSTIC ARM WITH A QUESTION ATTACHED. It is INERT BY DEFAULT and
/// it is NOT a fix.**
///
/// ⛔ **SUPERSEDED INSTRUCTION, recorded so nobody re-derives the old one.** This
/// doc used to end its first line with *"Delete it with the commit that lands the
/// WDDM submission"*, and [`UMD12_ECL_DELAY_US`] inherited that obligation by
/// reference. **The commit that landed the WDDM submission (K-F1) deliberately
/// kept both arms**, because deleting them would have removed the only lever that
/// separates the two readings the submission still has to be attributed against:
///
/// | reading | what it means once K-F1 is in |
/// |---|---|
/// | the app's fence wait grows with **this** delay | the runtime's fence advance is downstream of a DDI *returning* |
/// | it grows only with a delay the **KMD** imposes on the DMA packet | the advance is downstream of our packet *retiring* — which is what K-F1 is for |
///
/// Nothing in K-F1 answers that; it is `KMD_IMPACT.md` §14a.1's UV1, whose
/// settling experiment is the K-F0 scoped hold. ⇒ these arms outlive the
/// submission and retire with UV1, not with the callback.
///
/// # The question: where does the runtime's fence advance become downstream of this driver?
///
/// `D12-G8` rung 0 fails because the application's `ID3D12Fence` completes with
/// no causal dependency on the engine's Vulkan work: the probe's
/// `WaitForSingleObject` returns in 0.8–1.1 µs (against WARP's 561 µs) and the
/// readback surface is 0/65536 exact at T+0 and 65536/65536 exact at +2000 ms
/// through the *same still-live mapping* (`tmp/dx12/gates/G8-r0-settle/`). The
/// work lands; only the ordering is wrong.
///
/// ⭐ **The architecture is decided, and as of K-F1 the UMD half is LANDED**
/// ([`UMD12_ECL_SUBMIT`]): a real `pfnRenderCb` WDDM submission on the queue's
/// context during `pfnExecuteCommandLists`, so that the runtime's own kernel fence
/// signal queues *behind* work the KMD already withholds
/// `DXGK_INTERRUPT_DMA_COMPLETED` for. No stopgap, no producer-side stall in the
/// shipping driver.
///
/// ⛔ **But that design has a precondition nobody has measured: that the runtime
/// queues its fence signal on OUR context at all, rather than CPU-signalling it
/// independently of this driver.** If it does, the submission has to be in place
/// before the DDI the runtime gates on returns — and *which* DDI that is decides
/// where the submission goes and what it must already cover. This knob and
/// [`UMD12_ECL_DELAY_US`] are the experiment that reads it, one DDI at a time.
/// ⚠ K-F1 landing does not settle it: a submission that exists is not evidence
/// that dxgkrnl orders anything behind it. That is UV1.
///
/// ⚠ **A fixed delay, never a drain** (`FENCE-BRIDGE-DESIGN.md` §5 step 2): a
/// drain that fixes the pixels is consistent with every mechanism and settles
/// nothing, while a fixed delay isolates the causal link and cannot be mistaken
/// for a fix.
///
/// # What reading says what
///
/// Set this to `50000` (50 ms), run `clear.exe --sentinel --settle 2000` and
/// read one number — the probe's own `WaitForSingleObject signalled in N us`:
///
/// | this knob = 50000 | [`UMD12_ECL_DELAY_US`] = 50000 | what it says about the submission |
/// |---|---|---|
/// | N >= 50 000 µs | — | the advance is downstream of **`pfnSignalFence` returning**; whatever is submitted must be in place before that DDI returns |
/// | N ~ 1 µs | N >= 50 000 µs | it gates on **`pfnExecuteCommandLists` returning** — exactly where the `pfnRenderCb` submission goes. The best case. |
/// | N ~ 1 µs | N ~ 1 µs | ⛔ the runtime advances the fence independently of **both** DDIs, so the precondition is in doubt and must be settled directly — submit a DMA packet the KMD deliberately holds and see whether the app's fence wait grows |
///
/// ⚠ Read `FenceSignalForwarded` alongside it. A **zero** there means the
/// runtime never enters `pfnSignalFence` at all — a fact the submission design
/// has to accommodate, and one that makes this arm's own reading unobservable
/// rather than negative.
///
/// # Why this is inert by default and stays that way
///
/// Absent = `0` = **no delay**, so a machine with no registry value behaves
/// byte-identically to the build that has never heard of this knob (CLAUDE.md
/// rule 8 is satisfied trivially: the shipping default is the measured one,
/// because every accepted measurement was taken with the value absent). The
/// non-zero arm is a producer-side CPU stall of exactly the kind
/// `umd/src/knobs.rs:31-43` forbids as a *fix*; it is legal here only because it
/// is a **measurement**, run deliberately for one probe at a time and never
/// shipped on. ⛔ It is not, and never becomes, the answer to the ordering defect
/// — that is [`UMD12_ECL_SUBMIT`]'s job, and a delay that "fixes" the pixels is
/// consistent with every mechanism and therefore evidence for none.
///
/// Clamped to [`MAX_DIAGNOSTIC_DELAY_US`]; each firing bumps
/// `FenceSignalDelayed`, so an arm that was set and never reached is
/// distinguishable from one that was never set.
pub(crate) static UMD12_FENCE_SIGNAL_DELAY_US: DwordKnob =
    DwordKnob::new(c"Umd12FenceSignalDelayUs", 0);

/// ⛔⛔ **A DIAGNOSTIC ARM WITH A QUESTION ATTACHED. Inert by default, and NOT a
/// fix.** The `pfnExecuteCommandLists` half of the experiment
/// [`UMD12_FENCE_SIGNAL_DELAY_US`] documents — read that doc for the question and
/// the reading table; everything there applies here with `pfnExecuteCommandLists`
/// substituted for `pfnSignalFence`.
///
/// ⛔ **KEPT BY K-F1, and its earlier "delete me" instruction is SUPERSEDED.**
/// This doc used to say *"Delete it with the commit that lands the WDDM
/// submission"*, and the same sentence sat at the read site in
/// `forward12::queue::execute_command_lists`. Both are wrong now, for a reason
/// that only became visible when the submission was written:
///
/// **what this arm measures now** — it delays the DDI's *return*, and the
/// `pfnRenderCb` submission happens *before* that return. So with
/// [`UMD12_ECL_SUBMIT`] ON, a fence wait that grows with this delay says the
/// runtime's advance is downstream of **the DDI returning**; a fence wait that
/// does *not* grow, while a KMD-imposed hold on the DMA packet *does* move it,
/// says the advance is downstream of **our packet retiring**. Only the second is
/// the ordering K-F1 is built on, and no other instrument in the driver can tell
/// the two apart. Deleting this arm in the same commit would have shipped the
/// submission with no way to attribute its effect — `KMD_IMPACT.md` §14a.1's
/// UV1, unsettled.
///
/// ⇒ it retires with **UV1**, not with the callback. ⚠ And it must never be read
/// as the fix for the ordering: it is a CPU stall, `umd/src/knobs.rs:31-43`
/// forbids that as a fix, and [`UMD12_ECL_SUBMIT`] is what actually closes the
/// defect.
///
/// ⚠ The two delay arms are run **separately**, never together: their whole
/// purpose is to attribute the runtime's fence advance to one DDI or the other,
/// and a run with both set cannot tell which delay the number came from.
///
/// Absent = `0` = no delay. Clamped to [`MAX_DIAGNOSTIC_DELAY_US`]; each firing
/// bumps `EclDelayed`.
pub(crate) static UMD12_ECL_DELAY_US: DwordKnob = DwordKnob::new(c"Umd12EclDelayUs", 0);

/// ⭐⭐ **K-F1: the `pfnRenderCb` WDDM submission during `pfnExecuteCommandLists`.
/// DEFAULT ON.**
///
/// # Why the default is ON, and what evidence that rests on (CLAUDE.md rule 8)
///
/// The default is a decision, and this one is **decision D5a**
/// (`docs/dx12/DECISIONS.md`), the owner's: *"stop gaps are not acceptable, we
/// must do the correct, expected and performant implementation … doesn't matter
/// if its complex or if changes are needed to be done in KMD."* An OFF default
/// would ship the measured-broken configuration as the driver's behaviour, which
/// is the exact inversion rule 8 exists to stop.
///
/// What "measured-broken" means, precisely (`tmp/dx12/gates/G8-r0-settle/`):
/// with **no** kernel submission, `D12-G8` rung 0's `WaitForSingleObject` returns
/// in **0.8–1.1 µs** against WARP's **561 µs**, `GetDeviceRemovedReason` is 0
/// throughout, vkd3d logs no errors — and the readback surface is **0/65536 exact
/// at T+0 and 65536/65536 exact at +2000 ms** through the same still-live
/// mapping. The GPU work lands; the application's fence completes with no causal
/// dependency on it. `EclNoWddmSubmission = 1` was the defect, not a standing gap
/// (`KMD_IMPACT.md` §14a.0).
///
/// # ⭐ What the ON arm SETTLES: the plumbing, and nothing grander
///
/// ⛔ Read this before quoting a number from a run with this knob on. What the arm
/// establishes is that the **submission path exists and is accepted**, which
/// nothing before it had shown:
///
/// * dxgkrnl accepts `pfnRenderCb` on a D3D12 queue's **legacy** context at all —
///   the class was chosen inside `pfnCreateCommandQueue` against a doc set that
///   contradicted itself, and this is the first evidence either way;
/// * the callback returns success and hands back the three context windows, which
///   `EclWddmSubmitted` and the re-latched `next_cmd=` trace line report together;
/// * nothing bugchecks, and `pfnRenderCb`'s HRESULT is a real reading rather than
///   an assumption (`EclSubmitRenderFailed`).
///
/// ⭐ All three instruments are **this driver's own** — per-process, D3D12-only, in
/// `umd12-<pid>.log` — and that is load-bearing rather than convenient.
///
/// ⛔ **It does NOT settle `PRESENT.md` §12's P7 — whether `DxgkDdiRender` fires on
/// the D3D12 path.** `KMD_IMPACT.md` §14a.4 item 3 named `RENDER_COUNT` moving as
/// "the whole test" and that instrument is **confounded**: it is adapter-global,
/// incremented from three sites, and DWM's own D3D11 present path calls
/// `pfnRenderCb` every frame (`umd/src/forward/present.rs:860`), so it moves with
/// no D3D12 client in existence. §14a.4 now records it as the third member of a
/// family (`WfBWire`, `RING_SUBMIT_COUNT`, `RENDER_COUNT`) with one cause: every
/// KMD counter there is adapter-global and DWM is always running. P7 needs the
/// D3D12-specific record-seen counter on the KMD's decode of
/// `HeliosD3D12SubmitCmd` — a separate lane, reached by the packet this arm sends.
///
/// # ⛔⛔ What it does NOT settle — and why a FLAT reading is not evidence against it
///
/// It does **not** make the application's `ID3D12Fence` truthful on its own, and a
/// fence wait that stays at ~1 µs with the knob ON does **not** mean dxgkrnl
/// refused to order behind our packets.
///
/// ⛔ `KMD_IMPACT.md` §14a.1's table says a flat reading implies **UV1 ✗**. That
/// inference is **known false**, and the reason is in the ICD: the venus shared
/// ring emits *no virtio submission at all* while it is busier than 1 ms. The
/// command stream is written straight into the shared ring with no virtio traffic
/// (`icd/mesa/src/virtio/vulkan/vn_ring.c:630-636`), and the doorbell escape is
/// sent only when the host ring advertises IDLE — and then only past a 1 ms rate
/// limiter (`vn_ring.c:672-690`, `VN_RING_IDLE_TIMEOUT_NS` at `:22`). So through a
/// D3D12 frame `next_wire_fence` is typically **frozen**, and the KMD's
/// `async_retired_up_to(watermark, IncludingGpu)` for an unheld packet is satisfied
/// instantly — for a reason that has nothing to do with what dxgkrnl would have
/// done with a packet that was actually outstanding.
///
/// ⇒ **UV1 needs a deliberate KMD-side hold scoped to the D3D12 path**, which is a
/// separate lane. What this arm contributes to that is the packet the hold attaches
/// to: `HeliosD3D12SubmitCmd`'s *presence* is how the KMD recognises a D3D12 ECL
/// submission, so the hold can find those packets instead of stalling DWM through
/// the adapter-global WDDM FIFO.
///
/// # ⛔ Why the OFF arm must stay reachable
///
/// It is the control arm of that plumbing comparison: `Umd12EclSubmit=0` is
/// byte-for-byte the pre-K-F1 driver, and it is what a run is compared *against*
/// when `RENDER_COUNT`, `EclWddmSubmitted` or a fence wait is read. Rule 8's other
/// half, and the only honest way to attribute a change to this commit.
///
/// ⛔ A run whose arm is not recorded is not a measurement. The knob is in
/// [`resolved_inventory`], so every `UMD knob:` capture states which arm produced
/// the numbers next to it.
///
/// ⚠ Read once per process, like every other knob here. A running `dwm` keeps the
/// arm it started with; `HKLM\SOFTWARE\Helios` is writable over SSH with the
/// desktop down, and `pnputil /restart-device` re-runs the load without a reboot.
///
/// ⚠ Absent = ON. So a machine that has never heard of this value gets the
/// submission — which is the point: the *shipping* configuration must be the one
/// under test, and the disable is what an operator adds to reproduce the old
/// behaviour, never what the code assumes.
pub(crate) static UMD12_ECL_SUBMIT: BoolKnob = BoolKnob::new(c"Umd12EclSubmit", true);

/// ⛔⛔ **Whether a refused `pfnRenderCb` removes the `ID3D12Device`. DEFAULT OFF —
/// and that default is a BRING-UP decision with a written flip condition, not an
/// opinion about severity.**
///
/// # The severity question, and why the answer is "not yet"
///
/// `forward12::queue::report_ecl_submit_error` has the full argument for *which*
/// channel a refused submission belongs on (`pfnSetErrorCb`, whose runtime response
/// is *"Removing device due to bad UMD error"*) and for why that is the right
/// **eventual** behaviour: a refused packet means the frame's fence ordering did not
/// happen and every later frame's will not either, and a silently untruthful fence
/// is the exact defect K-F1 exists to end. None of that is in doubt.
///
/// ⛔ **What is in doubt is whether the path works at all yet**, and that decides
/// this default. Two things about K-F1's very first deployment are unverified from
/// the Linux host: that the cxx bridge's C++ half links (nothing here compiles C++),
/// and that dxgkrnl's own `D3DDDICB_RENDER` validation accepts `NumAllocations = 0`
/// with a 16-byte command on a legacy D3D12 context. Either would arrive as
/// `EclSubmitRenderFailed` plus an `hr=` line.
///
/// With strictness ON, that first run produces a **removed device instead of a
/// measurement**: no surface survey, no fence-wait number, no trace tail — and no
/// way to tell a bridge signature error from a semantic refusal by dxgkrnl, because
/// both look like device-removed. That is self-inflicted blindness on precisely the
/// run that exists to settle the question, and it is the shape of failure this
/// project already has a rule against: a gate that reads only the exit code settles
/// nothing.
///
/// ⇒ **CLAUDE.md rule 8 is what picks OFF**, not caution. *"A knob's default is a
/// decision, and it must match the measured configuration"* — **nothing has been
/// measured on this path**, so the default must be the value under which a
/// measurement can happen. The strict arm's own doc is the record of why it is
/// right; this knob is the record of why it is not yet the default.
///
/// ⚠ **OFF does not mean quiet.** A refusal still bumps `EclSubmitRenderFailed` and
/// still writes its budgeted `pfnRenderCb(...) failed hr=...` line, and the
/// suppression itself bumps `EclSubmitErrorNotRaised` — so with strictness off the
/// counters are the *only* signal and they are deliberately undiminished. What is
/// suppressed is exactly one thing: the `pfnSetErrorCb` call.
///
/// # ⭐ THE FLIP CONDITION, stated so this is a decision and not a fork nobody
/// revisits
///
/// **Once a real guest run shows `EclWddmSubmitted > 0` with
/// `EclSubmitRenderFailed == 0`, this default becomes `true` and the evidence goes
/// in this comment** — the run, the counter values, and the build. That single
/// reading retires both unknowns above at once: a submission that succeeded proves
/// the bridge linked and that dxgkrnl accepted the packet shape, which is the whole
/// content of "the path is proven".
///
/// ⛔ Until then the opposite arm stays reachable both ways, which is rule 8's other
/// half: `Umd12EclSubmitStrict=1` is available *now*, on purpose, so that the strict
/// behaviour can be exercised deliberately once the path is up rather than waiting
/// on a rebuild.
pub(crate) static UMD12_ECL_SUBMIT_STRICT: BoolKnob = BoolKnob::new(c"Umd12EclSubmitStrict", false);

/// The `pfnSignalFence` diagnostic delay in microseconds, clamped. `0` = off.
/// See [`UMD12_FENCE_SIGNAL_DELAY_US`].
pub(crate) fn umd12_fence_signal_delay_us() -> u32 {
    UMD12_FENCE_SIGNAL_DELAY_US.get().min(MAX_DIAGNOSTIC_DELAY_US)
}

/// The `pfnExecuteCommandLists` diagnostic delay in microseconds, clamped.
/// `0` = off. See [`UMD12_ECL_DELAY_US`].
pub(crate) fn umd12_ecl_delay_us() -> u32 {
    UMD12_ECL_DELAY_US.get().min(MAX_DIAGNOSTIC_DELAY_US)
}

/// Whether `pfnExecuteCommandLists` makes K-F1's `pfnRenderCb` WDDM submission.
/// **Absent = ON**; see [`UMD12_ECL_SUBMIT`] for the evidence behind that default
/// and for what the OFF arm is used to measure.
pub(crate) fn umd12_ecl_submit() -> bool {
    UMD12_ECL_SUBMIT.get()
}

/// Whether a refused `pfnRenderCb` is raised through `pfnSetErrorCb`, which removes
/// the `ID3D12Device`. **Absent = OFF**; see [`UMD12_ECL_SUBMIT_STRICT`] for why the
/// default is OFF during bring-up and for the written condition that flips it.
pub(crate) fn umd12_ecl_submit_strict() -> bool {
    UMD12_ECL_SUBMIT_STRICT.get()
}

/// Emit this crate's knob inventory through the shared reader, once per process.
///
/// The thin wrapper D3b's split implies: the READER is shared
/// (`helios_umd_common::log::log_knob_inventory`) because the emitted
/// `UMD knob: name=value` line is the evidence contract that
/// `tools/capture-knob-inventory.ps1` parses, while the SET is per-crate. The
/// module-path line logged just before it is what attributes these values to
/// *this* DLL when two Helios UMDs are loaded in one process.
pub(crate) fn log_knob_inventory() {
    helios_umd_common::log::log_knob_inventory(&resolved_inventory());
}

/// The knob inventory, so the set is enumerable instead of grep-discoverable.
///
/// Resolving forces every `OnceLock`, which is why this is not on any hot path:
/// it exists for the one-shot dump at driver init, and for anyone asking "what
/// knobs does the D3D12 driver have".
///
/// ⚠ **New knobs are APPENDED, never inserted.** The emitted `UMD knob:` lines
/// are the evidence contract `tools/capture-knob-inventory.ps1` parses and that
/// S2 proved the crate split byte-identical against; reordering makes two
/// captures differ for a reason that is not a behaviour change.
pub(crate) fn resolved_inventory() -> [(&'static str, u32); 7] {
    [
        ("Umd12Trace", UMD12_TRACE.get() as u32),
        ("UmdD3D12", UMD_D3D12.get() as u32),
        ("Umd12FormatCaps", UMD12_FORMAT_CAPS.get()),
        // ⚠ The two delay arms report their **clamped** value, through the same
        // accessor the DDI reads, not the raw DWORD. The inventory line is read
        // as "what will this driver do", and a mistyped 50000000 that the read
        // site silently caps at 2 000 000 would otherwise be captured as a
        // configuration the run never had.
        ("Umd12FenceSignalDelayUs", umd12_fence_signal_delay_us()),
        ("Umd12EclDelayUs", umd12_ecl_delay_us()),
        // ⭐ APPENDED (K-F1), never inserted — see the rule above. ⛔ This one is
        // load-bearing rather than informational: it is the only record of WHICH
        // ARM produced a run's numbers, and its default is ON, so a capture that
        // does not name it cannot be attributed at all.
        ("Umd12EclSubmit", umd12_ecl_submit() as u32),
        // ⭐ And this one is what says whether a `pfnRenderCb` refusal on that run
        // would have removed the device or only been counted — i.e. whether a run
        // that ended early ended for THIS reason. A capture without it cannot
        // distinguish "no refusal happened" from "a refusal happened and was
        // suppressed", which is exactly the ambiguity the OFF default introduces
        // and this line removes.
        ("Umd12EclSubmitStrict", umd12_ecl_submit_strict() as u32),
    ]
}
