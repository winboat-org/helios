//! The UMD's `HKLM\SOFTWARE\Helios` registry knobs, with their defaults as data.
//!
//! Four accessors used to be four literal copies of one ~33-line body: the same
//! `advapi32!RegGetValueA` redeclaration, the same `HKEY_LOCAL_MACHINE` /
//! `RRF_RT_REG_DWORD` constants, the same `SOFTWARE\Helios` subkey, the same
//! `OnceLock`, differing only in the value name and in an unlabelled tail
//! expression that decided what "absent" meant. That is policy-in-boilerplate:
//! a knob copy-pasted with the wrong tail is silently the wrong value on every
//! machine that never wrote the value, with no counter and no log.
//!
//! Here the default is a constructor argument, so it is impossible to write a
//! knob without stating what an absent value means, and the FFI call exists at
//! one audited site instead of four.
//!
//! **The registry value names, the hive and the `RRF` flag are the owner's
//! debugging ABI and are unchanged.** So are the four defaults:
//!
//! | Value | Type | Absent |
//! |---|---|---|
//! | `UmdTrace` | DWORD | `false` (explicit non-zero enables) |
//! | `FeatureLevel11` | DWORD | `1` |
//! | `PresentGateUs` | DWORD | `10000` |
//! | `VehicleFlipGateUs` | DWORD | `32000` |
//!
//! Two policies survive, not four: `BoolKnob` ("absent = off, non-zero = on")
//! and `DwordKnob` ("absent = this default, else the stored value"). The
//! absent-means-ON policy (`rc != 0 || value != 0`) belonged to
//! `VehicleKernelFlipWait` and the second bool to `PresentSyncPublish`; both
//! knobs went with T6/R912 when the kwait subsystem was retired.
//!
//! **Not covered here:** the environment-variable knobs, which are process
//! environment rather than registry state and have their own `OnceLock`s —
//! `HELIOS_DXGI_NO_REDIRECTION` (`lib.rs`), and `HELIOS_PRESENT_READBACK` /
//! `HELIOS_PRESENT_FORCE_OPAQUE` / `HELIOS_PRESENT_OPTIMIZE_COMPOSITION` /
//! `HELIOS_PRESENT_DUMP_DIR` (`forward.rs`). They are listed here so the knob
//! inventory is readable in one place even though the reader is not shared.

use core::ffi::{c_void, CStr};
use std::sync::OnceLock;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueA(
        hkey: usize,
        sub_key: *const u8,
        value: *const u8,
        flags: u32,
        type_out: *mut u32,
        data: *mut c_void,
        data_len: *mut u32,
    ) -> i32;
}

const HKEY_LOCAL_MACHINE: usize = 0x8000_0002;
const RRF_RT_REG_DWORD: u32 = 0x10;
const SUBKEY: &CStr = c"SOFTWARE\\Helios";

/// Read one REG_DWORD from `HKLM\SOFTWARE\Helios`, or `None` if it is absent,
/// unreadable or not a DWORD.
///
/// The single audited FFI site. Every knob below funnels through it.
fn reg_dword(name: &CStr) -> Option<u32> {
    let mut value: u32 = 0;
    let mut len: u32 = 4;
    // SAFETY: both names are NUL-terminated `CStr`s that outlive the call;
    // `value`/`len` are stack locals borrowed only for its duration, and
    // RRF_RT_REG_DWORD makes advapi32 refuse to write more than the 4 bytes
    // `len` advertises.
    let rc = unsafe {
        RegGetValueA(
            HKEY_LOCAL_MACHINE,
            SUBKEY.as_ptr().cast(),
            name.as_ptr().cast(),
            RRF_RT_REG_DWORD,
            core::ptr::null_mut(),
            (&mut value as *mut u32).cast(),
            &mut len,
        )
    };
    if rc == 0 {
        Some(value)
    } else {
        None
    }
}

/// A REG_DWORD knob read once per process, with its absent-value default
/// written at the definition site.
pub(crate) struct DwordKnob {
    name: &'static CStr,
    default: u32,
    cell: OnceLock<u32>,
}

impl DwordKnob {
    pub(crate) const fn new(name: &'static CStr, default: u32) -> Self {
        Self {
            name,
            default,
            cell: OnceLock::new(),
        }
    }

    pub(crate) fn get(&self) -> u32 {
        *self
            .cell
            .get_or_init(|| reg_dword(self.name).unwrap_or(self.default))
    }
}

/// A REG_DWORD knob interpreted as a flag: absent means `default`, present
/// means `value != 0`.
pub(crate) struct BoolKnob {
    name: &'static CStr,
    default: bool,
    cell: OnceLock<bool>,
}

impl BoolKnob {
    pub(crate) const fn new(name: &'static CStr, default: bool) -> Self {
        Self {
            name,
            default,
            cell: OnceLock::new(),
        }
    }

    pub(crate) fn get(&self) -> bool {
        *self
            .cell
            .get_or_init(|| reg_dword(self.name).map_or(self.default, |v| v != 0))
    }
}

// --- The knob set ----------------------------------------------------------
//
// Every registry knob the UMD reads is declared here. Adding one anywhere else
// is the drift this module exists to stop.

/// Per-frame/per-op DDI chatter (`trace_line!`). Absent = OFF.
pub(crate) static UMD_TRACE: BoolKnob = BoolKnob::new(c"UmdTrace", false);

/// Feature-level profile selector. Absent = 1 (the full FL11 profile).
pub(crate) static FEATURE_LEVEL_11: DwordKnob = DwordKnob::new(c"FeatureLevel11", 1);

/// Present-path frame-completion gate cap, microseconds. Absent = 10 ms.
pub(crate) static PRESENT_GATE_US: DwordKnob = DwordKnob::new(c"PresentGateUs", 10_000);

/// Dcomp-vehicle flip-ordering gate cap, microseconds. Absent = 32 ms.
pub(crate) static VEHICLE_FLIP_GATE_US: DwordKnob = DwordKnob::new(c"VehicleFlipGateUs", 32_000);

/// The knob inventory, so the set is enumerable instead of grep-discoverable.
///
/// Each entry is `(value name, resolved value as text)`. Resolving forces every
/// `OnceLock`, which is why this is not called on any hot path — it exists for
/// a one-shot dump at load, and for anyone asking "what knobs are there".
pub(crate) fn resolved_inventory() -> [(&'static str, u32); 4] {
    [
        ("UmdTrace", UMD_TRACE.get() as u32),
        ("FeatureLevel11", FEATURE_LEVEL_11.get()),
        ("PresentGateUs", PRESENT_GATE_US.get()),
        ("VehicleFlipGateUs", VEHICLE_FLIP_GATE_US.get()),
    ]
}
