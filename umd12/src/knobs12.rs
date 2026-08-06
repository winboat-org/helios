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

/// ⛔⛔ **A DIAGNOSTIC ARM WITH A QUESTION ATTACHED. Delete it with the commit
/// that lands the WDDM submission** (`FENCE-BRIDGE-DESIGN.md`'s design **C**).
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
/// ⭐ **The architecture is decided: a real `pfnRenderCb` WDDM submission on the
/// queue's context during `pfnExecuteCommandLists`, with the KMD work it needs**
/// — so that the runtime's own kernel fence signal queues *behind* work the KMD
/// already withholds `DXGK_INTERRUPT_DMA_COMPLETED` for. No stopgap, no
/// producer-side stall in the shipping driver.
///
/// ⛔ **But that design has a precondition nobody has measured: that the runtime
/// queues its fence signal on OUR context at all, rather than CPU-signalling it
/// independently of this driver.** If it does, the submission has to be in place
/// before the DDI the runtime gates on returns — and *which* DDI that is decides
/// where the submission goes and what it must already cover. This knob and
/// [`UMD12_ECL_DELAY_US`] are the experiment that reads it, one DDI at a time.
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
/// is a measurement that runs for one probe and is then deleted.
///
/// Clamped to [`MAX_DIAGNOSTIC_DELAY_US`]; each firing bumps
/// `FenceSignalDelayed`, so an arm that was set and never reached is
/// distinguishable from one that was never set.
pub(crate) static UMD12_FENCE_SIGNAL_DELAY_US: DwordKnob =
    DwordKnob::new(c"Umd12FenceSignalDelayUs", 0);

/// ⛔⛔ **A DIAGNOSTIC ARM WITH A QUESTION ATTACHED. Delete it with the commit
/// that lands the WDDM submission.** The `pfnExecuteCommandLists` half of the
/// experiment [`UMD12_FENCE_SIGNAL_DELAY_US`] documents — read that doc for the
/// question, the reading table and the deletion obligation; everything there
/// applies here with `pfnExecuteCommandLists` substituted for `pfnSignalFence`.
///
/// ⭐ This is the arm that matters most to the chosen design: the `pfnRenderCb`
/// submission goes at the end of this DDI, so a delay here is the closest
/// available stand-in for "the submission is in place before ECL returns".
///
/// ⚠ The two arms are run **separately**, never together: their whole purpose is
/// to attribute the runtime's fence advance to one DDI or the other, and a run
/// with both set cannot tell which delay the number came from.
///
/// Absent = `0` = no delay. Clamped to [`MAX_DIAGNOSTIC_DELAY_US`]; each firing
/// bumps `EclDelayed`.
pub(crate) static UMD12_ECL_DELAY_US: DwordKnob = DwordKnob::new(c"Umd12EclDelayUs", 0);

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
pub(crate) fn resolved_inventory() -> [(&'static str, u32); 5] {
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
    ]
}
