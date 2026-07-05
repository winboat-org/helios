//! TEMPORARY post-start bring-up tracer (remove once Code 43 / AddAdapter clears).
//!
//! dxgkrnl's StartAdapter→AddAdapter sequence drives a series of our DDIs and can
//! fail internally (e.g. `STATUS_OBJECT_NAME_NOT_FOUND`) with no NTSTATUS we get
//! to see. To find which DDI dxgkrnl is calling (and which we answer how) right
//! before it gives up, each instrumented PASSIVE-level DDI calls [`record`],
//! which appends a `REG_DWORD` breadcrumb as values `S0`, `S1`, `S2`, … under
//! `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`. After a repro read them in
//! order (`reg query` / `Get-ItemProperty`); the last few before the failure
//! point at the culprit.
//!
//! IRQL: `RtlWriteRegistryValue` requires PASSIVE_LEVEL — only call [`record`]
//! from PASSIVE DDIs (never the DPC/ISR or DISPATCH paging paths).
//!
//! Breadcrumb code encoding (high byte = which DDI, low bytes = detail):
//!   0x01_00_0000 | type     QueryAdapterInfo entry (DXGK_QUERYADAPTERINFOTYPE)
//!   0x02_00_0000 | type     QueryAdapterInfo answered STATUS_NOT_SUPPORTED (type)
//!   0x03_00_0000 | ordinal  GetNodeMetadata entry
//!   0x04_00_0000            QueryInterface entry (followed by the GUID Data1)
//!   0x05_00_0000            GetRootPageTableSize entry
//!   0x06_00_0000            CreateProcess entry
//!   raw value               an interface GUID Data1 logged after a 0x04 marker

use core::sync::atomic::{AtomicU32, Ordering};

use wdk_sys::ntddk::RtlWriteRegistryValue;

/// `RTL_REGISTRY_SERVICES` — Path is relative to
/// `\Registry\Machine\System\CurrentControlSet\Services`.
const RTL_REGISTRY_SERVICES: u32 = 1;
/// `REG_DWORD`.
const REG_DWORD: u32 = 4;
/// Cap on breadcrumbs so a chatty steady state can't grow the key unbounded.
const MAX_STEPS: u32 = 3000;

static STEP: AtomicU32 = AtomicU32::new(0);

/// `"helios_kmd_render\0"` as UTF-16 — the service subkey under Services.
static SERVICE_NAME: [u16; 18] = [
    b'h' as u16,
    b'e' as u16,
    b'l' as u16,
    b'i' as u16,
    b'o' as u16,
    b's' as u16,
    b'_' as u16,
    b'k' as u16,
    b'm' as u16,
    b'd' as u16,
    b'_' as u16,
    b'r' as u16,
    b'e' as u16,
    b'n' as u16,
    b'd' as u16,
    b'e' as u16,
    b'r' as u16,
    0,
];

/// Write a DWORD breadcrumb to a FIXED value name (not the `S<idx>` ring). The
/// `S*` ring is overwritten within ~1s by steady-state QueryAdapterInfo polling,
/// so it is useless for one-shot tracing of a rare DDI (e.g. Present). A fixed
/// name persists until the next write, so it can be read live from the registry.
/// `name` must be a NUL-terminated UTF-16 value name. PASSIVE_LEVEL only.
pub fn record_named(name: &[u16], mut code: u32) {
    // SAFETY: PASSIVE_LEVEL (see module note). `name` is a caller-provided
    // NUL-terminated UTF-16 value name; ValueData points to a 4-byte DWORD that
    // RtlWriteRegistryValue copies before returning.
    unsafe {
        let _ = RtlWriteRegistryValue(
            RTL_REGISTRY_SERVICES,
            SERVICE_NAME.as_ptr(),
            name.as_ptr(),
            REG_DWORD,
            (&mut code as *mut u32).cast::<core::ffi::c_void>(),
            4,
        );
    }
}

/// `record_named` convenience: build the UTF-16 value name from an ASCII byte
/// slice (≤14 chars). PASSIVE_LEVEL only.
pub fn record_named_bytes(name: &[u8], value: u32) {
    let mut buf = [0u16; 16];
    let n = name.len().min(14);
    let mut i = 0;
    while i < n {
        buf[i] = name[i] as u16;
        i += 1;
    }
    buf[n] = 0;
    record_named(&buf[..=n], value);
}

/// `RTL_QUERY_REGISTRY_DIRECT` — store the value straight into EntryContext
/// (for REG_DWORD data that fits a ULONG). No callback routine.
const RTL_QUERY_REGISTRY_DIRECT: u32 = 0x20;

/// Read a REG_DWORD config value from the service key (the same key the
/// breadcrumbs live under), or `default` if absent/unreadable. The value name
/// is ASCII (≤14 chars). PASSIVE_LEVEL only. Bring-up experiment knobs: lets
/// AddAdapter-shape experiments iterate via `reg add` + `devcon restart`
/// instead of a rebuild+reboot per variant.
///
/// The value MUST be REG_DWORD (RTL_QUERY_REGISTRY_DIRECT without TYPECHECK
/// interprets string data as a UNICODE_STRING buffer — only this driver's own
/// documented knobs are read here).
pub fn read_config_dword(name: &[u8], default: u32) -> u32 {
    let mut name_buf = [0u16; 16];
    let n = name.len().min(14);
    let mut i = 0;
    while i < n {
        name_buf[i] = name[i] as u16;
        i += 1;
    }
    name_buf[n] = 0;

    let mut value: u32 = default;
    // SAFETY: zeroed RTL_QUERY_REGISTRY_TABLE entries are valid; the second,
    // all-zero entry terminates the table (Name == NULL, QueryRoutine == NULL).
    let mut table: [wdk_sys::RTL_QUERY_REGISTRY_TABLE; 2] = unsafe { core::mem::zeroed() };
    table[0].Flags = RTL_QUERY_REGISTRY_DIRECT;
    table[0].Name = name_buf.as_ptr() as *mut u16;
    table[0].EntryContext = (&mut value as *mut u32).cast();
    // DefaultType/DefaultData stay zero (REG_NONE): an absent value leaves
    // `value` at `default`.
    // SAFETY: PASSIVE_LEVEL; Path is the NUL-terminated service subkey relative
    // to RTL_REGISTRY_SERVICES; the table is NUL-entry-terminated; EntryContext
    // points at a live ULONG for the duration of the call.
    unsafe {
        let _ = wdk_sys::ntddk::RtlQueryRegistryValues(
            RTL_REGISTRY_SERVICES,
            SERVICE_NAME.as_ptr(),
            table.as_mut_ptr(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        );
    }
    value
}

/// Append one DWORD breadcrumb. Cheap and lossy by design (best-effort tracing).
pub fn record(mut code: u32) {
    let idx = STEP.fetch_add(1, Ordering::Relaxed);
    if idx >= MAX_STEPS {
        return;
    }
    // Build the value name "S<idx>\0" as UTF-16. `idx` is a u32 (up to 10 digits);
    // MAX_STEPS lets it exceed 999, so size both buffers for the full u32 range —
    // `digits[d]` previously overflowed `[0u8; 3]` once idx reached 1000, panicking
    // (→ the no_std loop{} handler hangs the thread under dxgkrnl's adapter lock and
    // deadlocks the whole graphics stack). 'S' + up to 10 digits + NUL = 12.
    let mut name = [0u16; 12];
    name[0] = b'S' as u16;
    let mut digits = [0u8; 10];
    let mut n = idx;
    let mut d = 0usize;
    if n == 0 {
        digits[0] = b'0';
        d = 1;
    } else {
        while n > 0 {
            digits[d] = b'0' + (n % 10) as u8;
            n /= 10;
            d += 1;
        }
    }
    let mut i = 0;
    while i < d {
        name[1 + i] = digits[d - 1 - i] as u16;
        i += 1;
    }
    name[1 + d] = 0;

    // SAFETY: PASSIVE_LEVEL (see module note). Path/ValueName are NUL-terminated
    // UTF-16; ValueData points to a 4-byte DWORD. RtlWriteRegistryValue copies the
    // value, so `code`'s lifetime ending after the call is fine.
    unsafe {
        let _ = RtlWriteRegistryValue(
            RTL_REGISTRY_SERVICES,
            SERVICE_NAME.as_ptr(),
            name.as_ptr(),
            REG_DWORD,
            (&mut code as *mut u32).cast::<core::ffi::c_void>(),
            4,
        );
    }
}
