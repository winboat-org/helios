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
//!
//! ⭐ **`UmdD3D12` lands here at S5, and not one commit earlier.** A kill switch
//! for a driver that cannot be reached kills nothing, so declaring it before
//! slot 3 existed would have put a knob in the inventory line whose value
//! provably had no effect — the `DECISIONS.md` §7.1 / R908 failure mode in
//! miniature. It arrives in the same commit that registers
//! `UserModeDriverName[3]`, deletes `umd`'s duplicate `OpenAdapter12` export and
//! makes `adapter12::OpenAdapter12`'s body reachable.

use helios_umd_common::knobs::BoolKnob;

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
pub(crate) fn resolved_inventory() -> [(&'static str, u32); 2] {
    [
        ("Umd12Trace", UMD12_TRACE.get() as u32),
        ("UmdD3D12", UMD_D3D12.get() as u32),
    ]
}
