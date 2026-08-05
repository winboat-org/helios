//! The registry-knob **mechanism**: one audited `advapi32` FFI site and two
//! knob policies.
//!
//! Moved from `umd/src/knobs.rs:58-153` (`DECISIONS.md` D3b, stage S2).
//!
//! ⛔ **Only the mechanism moved. The knob VALUES stay per-crate**, and that is
//! D3b's explicit instruction: `umd` keeps its ten D3D11 knobs and `umd12`
//! declares its own set, including `UmdD3D12` (D11, the D3D12 kill switch).
//! Sharing the *table* would mean one driver's A/B lever silently applying to
//! the other, and `UserModeDriverName[3]` is supposed to be the only coupling
//! between them.
//!
//! # What the original module bought, restated because it survives the move
//!
//! Four accessors used to be four literal copies of one ~33-line body: the same
//! `advapi32!RegGetValueA` redeclaration, the same `HKEY_LOCAL_MACHINE` /
//! `RRF_RT_REG_DWORD` constants, the same `SOFTWARE\Helios` subkey, the same
//! `OnceLock`, differing only in the value name and in an unlabelled tail
//! expression that decided what "absent" meant. That is policy-in-boilerplate:
//! a knob copy-pasted with the wrong tail is silently the wrong value on every
//! machine that never wrote the value, with no counter and no log. Here the
//! default is a constructor argument, so it is impossible to write a knob
//! without stating what an absent value means, and the FFI call exists at one
//! audited site instead of four — now one site for **both** drivers.
//!
//! **The registry value names, the hive and the `RRF` flag are the owner's
//! debugging ABI.** `HKLM\SOFTWARE\Helios`, REG_DWORD, read once per process.
//!
//! Two policies, not four: [`BoolKnob`] ("absent = this default, non-zero = on")
//! and [`DwordKnob`] ("absent = this default, else the stored value"). The
//! absent-means-ON policy (`rc != 0 || value != 0`) belonged to
//! `VehicleKernelFlipWait` and a second bool to `PresentSyncPublish`; both knobs
//! went with T6/R912 when the kwait subsystem was retired.

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

/// The one hive both drivers read. ⚠ Shared on purpose: the owner types these
/// values by hand, and a second subkey would be a second thing to remember.
const SUBKEY: &CStr = c"SOFTWARE\\Helios";

/// Read one REG_DWORD from `HKLM\SOFTWARE\Helios`, or `None` if it is absent,
/// unreadable or not a DWORD.
///
/// The single audited FFI site. Every knob in every Helios UMD funnels through
/// it.
pub fn reg_dword(name: &CStr) -> Option<u32> {
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
pub struct DwordKnob {
    name: &'static CStr,
    default: u32,
    cell: OnceLock<u32>,
}

impl DwordKnob {
    pub const fn new(name: &'static CStr, default: u32) -> Self {
        Self {
            name,
            default,
            cell: OnceLock::new(),
        }
    }

    pub fn get(&self) -> u32 {
        *self
            .cell
            .get_or_init(|| reg_dword(self.name).unwrap_or(self.default))
    }
}

/// A REG_DWORD knob interpreted as a flag: absent means `default`, present
/// means `value != 0`.
pub struct BoolKnob {
    name: &'static CStr,
    default: bool,
    cell: OnceLock<bool>,
}

impl BoolKnob {
    pub const fn new(name: &'static CStr, default: bool) -> Self {
        Self {
            name,
            default,
            cell: OnceLock::new(),
        }
    }

    pub fn get(&self) -> bool {
        *self
            .cell
            .get_or_init(|| reg_dword(self.name).map_or(self.default, |v| v != 0))
    }
}
