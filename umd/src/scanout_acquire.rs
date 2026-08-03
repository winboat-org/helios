//! D4a scanout-read acquire — the UMD half (FIX-DESIGN-d4a.md §4).
//!
//! The KMD keeps a per-resource READ LEDGER (one nonpaged page: `issued` /
//! `retired` host-readback counters per venus resource id) and signals a
//! registered auto-reset event on every retirement. This module is the UMD's
//! plumbing for that contract:
//!
//!   * probe the capability once per process (`HELIOS_ESCAPE_MAP_READ_LEDGER`
//!     op PROBE — an old KMD fails the escape from its unknown-verb arm, and
//!     the whole feature latches OFF with one log line);
//!   * map the ledger page read-only into this process, per device (op MAP;
//!     the KMD tracks the mapping owner-keyed, so a per-device view is the one
//!     shape that is correct under every owner-reclaim implementation);
//!   * create + register one auto-reset event per device
//!     (`HELIOS_ESCAPE_SCANOUT_EVENT` op REGISTER) and hand it to the DXVK
//!     engine's per-device signaler thread through the bridge;
//!   * export the reader surface (`helios_scanout_*`) the statically linked
//!     DXVK engine resolves BY NAME from this DLL (dxvk-helios/src/dxvk/
//!     dxvk_helios_scanout_acquire.cpp) — by-name rather than a link-time
//!     reference so the fork's standalone d3d11.dll target keeps linking.
//!
//! Escape transport: `pfnEscapeCb` from the bindgen'd `D3DDDI_DEVICECALLBACKS`
//! — previously unused. Per the WDK's own signature the first argument is the
//! **runtime adapter handle** (`PFND3DDDI_ESCAPECB(hAdapter, ...)`), captured
//! at `OpenAdapter*`; `D3DDDICB_ESCAPE.hDevice` carries the runtime **device**
//! handle, which dxgkrnl resolves to the KM device handle the KMD mints its
//! `DeviceOwner` from (kmd_render ddi/escape.rs). Flags stay all-zero:
//! `HardwareAccess = 0` is the 26th-session owner directive (a HardwareAccess
//! escape serializes on the dxgkrnl adapter CORE resource), and these verbs
//! are PASSIVE + non-blocking by contract. `hContext` stays null — both verbs
//! are device-scoped.
//!
//! Concurrency model: one process-global registry (`Mutex<Vec<DeviceEntry>>`).
//! Every ledger read — the DXVK exports below — happens UNDER that mutex and
//! only through a currently-registered mapping, and teardown removes the entry
//! under the same mutex before it unmaps, so a reader can never touch a VA
//! whose device is mid-destroy. The reads are a handful of `u32` loads per
//! flush; the mutex is uncontended at that rate. The one lock-free fast path
//! is [`helios_scanout_acquire_enabled`], a single Acquire load, which is what
//! keeps the knob-off / probe-failed path bit-identical to a build without the
//! mechanism.

use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;

use helios_protocol::{
    HeliosEscapeHeader, HeliosEscapeMapReadLedger, HeliosEscapeScanoutEvent, HeliosReadLedgerPage,
    HeliosReadLedgerSlot, HELIOS_ESCAPE_MAP_READ_LEDGER, HELIOS_ESCAPE_SCANOUT_EVENT,
    HELIOS_READ_LEDGER_MAGIC, HELIOS_READ_LEDGER_SLOTS, HELIOS_READ_LEDGER_VERSION,
    HELIOS_SCANOUT_ACQ_OK, HELIOS_SCANOUT_ACQ_OP_MAP, HELIOS_SCANOUT_ACQ_OP_PROBE,
    HELIOS_SCANOUT_ACQ_OP_REGISTER, HELIOS_SCANOUT_ACQ_OP_UNMAP,
    HELIOS_SCANOUT_ACQ_OP_UNREGISTER, HELIOS_SCANOUT_ACQ_PROBE_ACK,
    HELIOS_SCANOUT_ACQ_TABLE_FULL, HELIOS_SCANOUT_CAP_ASYNC_PRESENT_STREAM,
    HELIOS_SCANOUT_CAP_SNAPSHOT_BIND,
};

use crate::ddi;
use crate::device_funcs::HeliosDevice;
use crate::log_error;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(
        security_attributes: *mut core::ffi::c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> *mut core::ffi::c_void;
    fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
}

/// Probe latch. UNKNOWN until the first device init runs the probe; then OK or
/// OFF for the rest of the process — "never retry per present" is the §4
/// contract, and per-device init reads the latch instead of re-probing.
const PROBE_UNKNOWN: u32 = 0;
const PROBE_OK: u32 = 1;
const PROBE_OFF: u32 = 2;
static PROBE_STATE: AtomicU32 = AtomicU32::new(PROBE_UNKNOWN);

/// The PROBE reply's `out_size` capability bitmask
/// (`HELIOS_SCANOUT_CAP_*`), latched beside [`PROBE_STATE`] when the ACK
/// arrives. A pre-capability KMD (<= 22.22.222.0) ACKs with `out_size == 0`
/// — no bits, so features gated on a bit stay OFF (skew-safe in both
/// directions). Only meaningful once `PROBE_STATE == PROBE_OK`.
static PROBE_CAPS: AtomicU32 = AtomicU32::new(0);

/// D4b: whether the KMD advertised `HELIOS_SCANOUT_CAP_SNAPSHOT_BIND` in the
/// probe reply, i.e. it honors `HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT` on the
/// DMA-flip bind path. Two relaxed loads — cheap enough for the per-present
/// substitution gate. `false` until the probe has ACK'd, which also covers
/// the `ScanoutAcquire=0` case (the probe never runs, so the capability can
/// never latch; `init_for_device` logs that coupling once per device).
pub(crate) fn scanout_snapshot_capable() -> bool {
    // Acquire on the state pairs with the Release store in `init_for_device`:
    // seeing PROBE_OK guarantees the caps store that preceded it is visible.
    PROBE_STATE.load(Ordering::Acquire) == PROBE_OK
        && PROBE_CAPS.load(Ordering::Relaxed) & HELIOS_SCANOUT_CAP_SNAPSHOT_BIND != 0
}

/// Whether the KMD probe explicitly advertised registered monotonic
/// present-stream markers.  The acquire probe is the shared capability latch;
/// an old KMD, a probe failure, or `ScanoutAcquire=0` stays false and forces
/// the historical present gate.
pub(crate) fn async_present_stream_capable() -> bool {
    PROBE_STATE.load(Ordering::Acquire) == PROBE_OK
        && PROBE_CAPS.load(Ordering::Relaxed) & HELIOS_SCANOUT_CAP_ASYNC_PRESENT_STREAM != 0
}

/// The lock-free fast-path flag the DXVK export reads once per flush:
/// knob ON && probe OK && at least one live ledger mapping. Maintained under
/// the registry mutex, read anywhere.
static ENABLED: AtomicU32 = AtomicU32::new(0);

/// The runtime ADAPTER handle from the most recent `OpenAdapter*`, i.e. the
/// first argument `pfnEscapeCb` documents (`hAdapter`). Every open in this
/// process targets the same Helios adapter, so any live runtime adapter
/// object reaches the same kernel adapter; the value is refreshed on every
/// open so it always names the newest (longest-lived) object.
static LAST_RT_ADAPTER: AtomicUsize = AtomicUsize::new(0);

/// Per-device acquire state. `key` is the `HeliosDevice` pointer value —
/// identity only, never dereferenced here.
struct DeviceEntry {
    key: usize,
    /// Runtime device handle for `D3DDDICB_ESCAPE.hDevice` (owner identity).
    h_rt_device: usize,
    /// Runtime adapter handle this device's escapes go through.
    rt_adapter: usize,
    /// The device's `D3DDDI_DEVICECALLBACKS` table — runtime-owned, valid for
    /// the device DDI lifetime (teardown runs inside `DestroyDevice`).
    kt_callbacks: usize,
    /// User VA of this device's read-only [`HeliosReadLedgerPage`] view.
    /// 0 = mapping failed; the entry then contributes nothing to lookups.
    ledger_va: usize,
    /// Owned auto-reset event handle (0 = creation failed).
    event: usize,
    /// Whether the KMD accepted the REGISTER (TABLE_FULL leaves the event
    /// alive but unsignaled — the DXVK signaler then runs on its 10 ms
    /// level-triggered timeout alone, which is correct, just slower).
    event_registered: bool,
}

static REGISTRY: Mutex<Vec<DeviceEntry>> = Mutex::new(Vec::new());

/// Record the runtime adapter handle at `OpenAdapter*` time. Called from
/// `open_adapter_common`.
pub(crate) fn note_runtime_adapter(h_rt_adapter: *mut core::ffi::c_void) {
    if !h_rt_adapter.is_null() {
        LAST_RT_ADAPTER.store(h_rt_adapter as usize, Ordering::Release);
    }
}

/// Build and issue one Helios escape through `pfnEscapeCb`.
///
/// Returns the callback's HRESULT (negative = failed). The payload is read and
/// written in place.
///
/// # Safety
/// `kt_callbacks` must point at the device's live callback table and
/// `payload`/`payload_size` must describe a writable escape struct.
unsafe fn call_escape(
    kt_callbacks: *const ddi::D3DDDI_DEVICECALLBACKS,
    rt_adapter: usize,
    h_rt_device: usize,
    payload: *mut core::ffi::c_void,
    payload_size: u32,
) -> i32 {
    // SAFETY: caller guarantees the table pointer; a None slot is answered
    // with a failure HRESULT instead of a call through null.
    let Some(escape_cb) = (unsafe { &*kt_callbacks }).pfnEscapeCb else {
        return i32::MIN; // distinct “no callback” failure; logged by the caller
    };
    let mut esc = ddi::D3DDDICB_ESCAPE::default();
    // Flags stay zero-initialized: HardwareAccess = 0 (owner directive; these
    // verbs are PASSIVE and non-blocking), no DeviceStatusQuery, nothing else.
    esc.hDevice = h_rt_device as ddi::HANDLE;
    esc.pPrivateDriverData = payload;
    esc.PrivateDriverDataSize = payload_size;
    // hContext stays null: MAP_READ_LEDGER / SCANOUT_EVENT are device-scoped.
    // SAFETY: the callback contract (PFND3DDDI_ESCAPECB) takes the runtime
    // adapter handle and a fully-initialized D3DDDICB_ESCAPE whose buffer
    // outlives the call; both hold here.
    unsafe { escape_cb(rt_adapter as ddi::HANDLE, &esc) }
}

/// One `HELIOS_ESCAPE_MAP_READ_LEDGER` round-trip.
///
/// # Safety
/// Same contract as [`call_escape`].
unsafe fn escape_map_ledger(
    kt_callbacks: *const ddi::D3DDDI_DEVICECALLBACKS,
    rt_adapter: usize,
    h_rt_device: usize,
    op: u32,
) -> Result<HeliosEscapeMapReadLedger, i32> {
    let mut payload = HeliosEscapeMapReadLedger {
        hdr: HeliosEscapeHeader::new(
            HELIOS_ESCAPE_MAP_READ_LEDGER,
            size_of::<HeliosEscapeMapReadLedger>() as u32,
        ),
        out_user_va: 0,
        op,
        out_size: 0,
        out_state: !0, // never a legal out_state: a KMD that answers must overwrite it
        _pad: 0,
    };
    // SAFETY: `payload` is a live stack struct of exactly the advertised size.
    let hr = unsafe {
        call_escape(
            kt_callbacks,
            rt_adapter,
            h_rt_device,
            (&mut payload as *mut HeliosEscapeMapReadLedger).cast(),
            size_of::<HeliosEscapeMapReadLedger>() as u32,
        )
    };
    if hr < 0 {
        Err(hr)
    } else {
        Ok(payload)
    }
}

/// One `HELIOS_ESCAPE_SCANOUT_EVENT` round-trip.
///
/// # Safety
/// Same contract as [`call_escape`].
unsafe fn escape_scanout_event(
    kt_callbacks: *const ddi::D3DDDI_DEVICECALLBACKS,
    rt_adapter: usize,
    h_rt_device: usize,
    op: u32,
    event: usize,
) -> Result<u32, i32> {
    let mut payload = HeliosEscapeScanoutEvent {
        hdr: HeliosEscapeHeader::new(
            HELIOS_ESCAPE_SCANOUT_EVENT,
            size_of::<HeliosEscapeScanoutEvent>() as u32,
        ),
        event_handle: event as u64,
        op,
        out_state: !0,
    };
    // SAFETY: `payload` is a live stack struct of exactly the advertised size.
    let hr = unsafe {
        call_escape(
            kt_callbacks,
            rt_adapter,
            h_rt_device,
            (&mut payload as *mut HeliosEscapeScanoutEvent).cast(),
            size_of::<HeliosEscapeScanoutEvent>() as u32,
        )
    };
    if hr < 0 {
        Err(hr)
    } else {
        Ok(payload.out_state)
    }
}

/// Acquire-load one `u32` field of the mapped ledger page. The KMD writes the
/// counters with Release stores; Acquire loads are the reader half of that
/// contract (protocol doc on [`HeliosReadLedgerSlot`]).
///
/// # Safety
/// `va + offset` must lie inside a currently-mapped ledger page — guaranteed
/// by only calling this under the registry mutex against a registered entry.
#[inline]
unsafe fn ledger_load(va: usize, offset: usize) -> u32 {
    // SAFETY: caller guarantees the address is inside the live 4 KiB mapping;
    // all page fields are naturally aligned u32s, so the AtomicU32 view is
    // valid, and atomics are the correct way to read KMD-shared memory.
    unsafe { (*((va + offset) as *const AtomicU32)).load(Ordering::Acquire) }
}

const LEDGER_MAGIC_OFF: usize = offset_of!(HeliosReadLedgerPage, magic);
const LEDGER_VERSION_OFF: usize = offset_of!(HeliosReadLedgerPage, version);
const LEDGER_SLOT_COUNT_OFF: usize = offset_of!(HeliosReadLedgerPage, slot_count);
const LEDGER_SLOTS_OFF: usize = offset_of!(HeliosReadLedgerPage, slots);
const SLOT_SIZE: usize = size_of::<HeliosReadLedgerSlot>();
const SLOT_RESID_OFF: usize = offset_of!(HeliosReadLedgerSlot, resid);
const SLOT_ISSUED_OFF: usize = offset_of!(HeliosReadLedgerSlot, issued);
const SLOT_RETIRED_OFF: usize = offset_of!(HeliosReadLedgerSlot, retired);

/// Validate a freshly-mapped ledger page's magic/version. A mismatch means a
/// driver skew and the feature must treat the page as absent.
///
/// # Safety
/// `va` must be a live mapping of at least `size_of::<HeliosReadLedgerPage>()`.
unsafe fn ledger_page_valid(va: usize) -> bool {
    // SAFETY: caller guarantees the mapping covers the page header.
    let magic = unsafe { ledger_load(va, LEDGER_MAGIC_OFF) };
    // SAFETY: as above.
    let version = unsafe { ledger_load(va, LEDGER_VERSION_OFF) };
    magic == HELIOS_READ_LEDGER_MAGIC && version == HELIOS_READ_LEDGER_VERSION
}

/// Recompute the fast-path flag from the registry. Call under the mutex.
fn recompute_enabled(entries: &[DeviceEntry]) {
    let on = crate::scanout_acquire_knob()
        && PROBE_STATE.load(Ordering::Relaxed) == PROBE_OK
        && entries.iter().any(|e| e.ledger_va != 0);
    ENABLED.store(on as u32, Ordering::Release);
}

/// Wire one freshly-created device into the acquire machinery. Called from
/// `create_device` after the device is committed (post-defuse), so failure
/// here never has to unwind device creation — every failure arm degrades to
/// "feature off for this device", counted and logged once.
///
/// Returns the registered auto-reset event handle to hand to the DXVK device's
/// signaler (0 = none; the signaler then polls on its 10 ms timeout).
pub(crate) fn init_for_device(dev: &HeliosDevice) -> usize {
    if !crate::scanout_acquire_knob() {
        // Kill switch: no escapes, no event, no mapping — bit-identical off.
        // COUPLING, stated loudly: the D4b snapshot capability rides THIS
        // probe, so ScanoutAcquire=0 also leaves `scanout_snapshot_capable`
        // false and the snapshot substitution off, whatever ScanoutSnapshot
        // says. One line per device create, not per present.
        if crate::scanout_snapshot_knob() {
            log_error!(
                "scanout-acquire: ScanoutAcquire=0 skips the capability probe — \
                 D4b snapshot substitution stays OFF for this process"
            );
        }
        return 0;
    }
    if dev.kt_callbacks.is_null() {
        return 0;
    }
    let rt_adapter = LAST_RT_ADAPTER.load(Ordering::Acquire);
    if rt_adapter == 0 {
        log_error!("scanout-acquire: no runtime adapter handle captured; feature off for this device");
        return 0;
    }
    let key = dev as *const HeliosDevice as usize;
    let h_rt_device = dev.h_rt_device as usize;
    let kt_callbacks = dev.kt_callbacks;

    let Ok(mut reg) = REGISTRY.lock() else {
        return 0;
    };

    // Probe once per process. An old KMD answers STATUS_NOT_IMPLEMENTED from
    // its unknown-verb arm — the capability signal — and the feature latches
    // OFF for the process: one log line, never retried per present (§4).
    match PROBE_STATE.load(Ordering::Relaxed) {
        PROBE_OK => {}
        PROBE_OFF => return 0,
        _ => {
            // SAFETY: `kt_callbacks` is the device's live callback table and
            // the payload is a stack struct sized to its own advertisement.
            let probed = unsafe {
                escape_map_ledger(kt_callbacks, rt_adapter, h_rt_device, HELIOS_SCANOUT_ACQ_OP_PROBE)
            };
            match probed {
                Ok(p) if p.out_state == HELIOS_SCANOUT_ACQ_PROBE_ACK => {
                    // The ACK's out_size is the capability bitmask
                    // (HELIOS_SCANOUT_CAP_*; 0 from a pre-capability KMD).
                    // Caps first, then the Release store of OK: an Acquire
                    // reader that sees OK (`scanout_snapshot_capable`) is
                    // guaranteed to see the caps that came with it.
                    PROBE_CAPS.store(p.out_size, Ordering::Relaxed);
                    PROBE_STATE.store(PROBE_OK, Ordering::Release);
                    log_error!(
                        "scanout-acquire: KMD probe ACK — read-ledger capability present, caps=0x{:x}",
                        p.out_size
                    );
                }
                Ok(p) => {
                    PROBE_STATE.store(PROBE_OFF, Ordering::Relaxed);
                    log_error!(
                        "scanout-acquire: probe answered out_state={} (not ACK) — feature OFF",
                        p.out_state
                    );
                    return 0;
                }
                Err(hr) => {
                    PROBE_STATE.store(PROBE_OFF, Ordering::Relaxed);
                    log_error!(
                        "scanout-acquire: probe escape failed hr=0x{:08x} (old KMD?) — feature OFF",
                        hr as u32
                    );
                    return 0;
                }
            }
        }
    }

    // Map this device's read-only view of the ledger page.
    // SAFETY: as for the probe above.
    let ledger_va = match unsafe {
        escape_map_ledger(kt_callbacks, rt_adapter, h_rt_device, HELIOS_SCANOUT_ACQ_OP_MAP)
    } {
        Ok(m)
            if m.out_state == HELIOS_SCANOUT_ACQ_OK
                && m.out_user_va != 0
                && (m.out_size as usize) >= size_of::<HeliosReadLedgerPage>() =>
        {
            let va = m.out_user_va as usize;
            // SAFETY: the KMD just mapped `out_size >= page` bytes at `va` for
            // this process; the header reads are inside that range.
            if unsafe { ledger_page_valid(va) } {
                va
            } else {
                log_error!(
                    "scanout-acquire: mapped ledger page failed magic/version check — unmapping, feature off for this device"
                );
                // Best effort: return the bogus mapping rather than leak it.
                // SAFETY: as for the map call.
                let _ = unsafe {
                    escape_map_ledger(kt_callbacks, rt_adapter, h_rt_device, HELIOS_SCANOUT_ACQ_OP_UNMAP)
                };
                0
            }
        }
        Ok(m) => {
            log_error!(
                "scanout-acquire: MAP refused out_state={} va=0x{:x} size={} — feature off for this device",
                m.out_state,
                m.out_user_va,
                m.out_size
            );
            0
        }
        Err(hr) => {
            log_error!(
                "scanout-acquire: MAP escape failed hr=0x{:08x} — feature off for this device",
                hr as u32
            );
            0
        }
    };

    // Create + register the per-device retirement event (auto-reset: the
    // signaler is level-triggered, every wake re-reads the ledger, so a lost
    // or coalesced signal is a bounded hiccup, never a hang).
    // SAFETY: plain kernel32 call; all-null/0 arguments are the documented
    // anonymous auto-reset unsignaled shape.
    let event = unsafe { CreateEventW(core::ptr::null_mut(), 0, 0, core::ptr::null()) } as usize;
    let mut event_registered = false;
    if event != 0 {
        // SAFETY: as for the map call; `event` is a live handle in this process.
        match unsafe {
            escape_scanout_event(
                kt_callbacks,
                rt_adapter,
                h_rt_device,
                HELIOS_SCANOUT_ACQ_OP_REGISTER,
                event,
            )
        } {
            Ok(HELIOS_SCANOUT_ACQ_OK) => event_registered = true,
            Ok(HELIOS_SCANOUT_ACQ_TABLE_FULL) => {
                // Counted KMD-side (AqRgF). The signaler runs timeout-only —
                // correct, just up to 10 ms later per retirement.
                log_error!(
                    "scanout-acquire: event table FULL — signaler falls back to 10 ms polling"
                );
            }
            Ok(state) => {
                log_error!("scanout-acquire: event REGISTER answered out_state={state}");
            }
            Err(hr) => {
                log_error!(
                    "scanout-acquire: event REGISTER escape failed hr=0x{:08x}",
                    hr as u32
                );
            }
        }
    } else {
        log_error!("scanout-acquire: CreateEventW failed — signaler falls back to 10 ms polling");
    }

    reg.push(DeviceEntry {
        key,
        h_rt_device,
        rt_adapter,
        kt_callbacks: kt_callbacks as usize,
        ledger_va,
        event,
        event_registered,
    });
    recompute_enabled(&reg);
    log_error!(
        "scanout-acquire: device wired (ledger={} event={} registered={} devices={})",
        (ledger_va != 0) as u32,
        (event != 0) as u32,
        event_registered as u32,
        reg.len()
    );

    event
}

/// Tear one device out of the acquire machinery. Called from
/// `ddi_destroy_device` AFTER the bridge device dropped — i.e. after the DXVK
/// half's §5.3 sequence (stop arming → signal every gate to max → join the
/// signaler) has completed inside `~DxvkDevice` — so the event handle closed
/// here has no waiter left and the VA unmapped here has no reader left (all
/// remaining readers go through the registry mutex and can no longer pick this
/// entry once it is removed).
pub(crate) fn teardown_for_device(key: usize) {
    let entry = {
        let Ok(mut reg) = REGISTRY.lock() else {
            return;
        };
        let Some(index) = reg.iter().position(|e| e.key == key) else {
            return; // knob off / probe off / never wired — nothing to do
        };
        let entry = reg.swap_remove(index);
        recompute_enabled(&reg);
        entry
    };
    // Escapes + CloseHandle outside the lock: no reader can reach the entry
    // any more, and the KMD side is keyed by our h_rt_device owner identity.
    let kt_callbacks = entry.kt_callbacks as *const ddi::D3DDDI_DEVICECALLBACKS;
    if entry.event != 0 {
        if entry.event_registered {
            // SAFETY: the callback table stays valid for the whole
            // DestroyDevice DDI, which is where this runs.
            let unregistered = unsafe {
                escape_scanout_event(
                    kt_callbacks,
                    entry.rt_adapter,
                    entry.h_rt_device,
                    HELIOS_SCANOUT_ACQ_OP_UNREGISTER,
                    entry.event,
                )
            };
            if let Err(hr) = unregistered {
                // The KMD's owner-keyed reclaim in destroy_device is the
                // backstop; the failure is logged, not fatal.
                log_error!(
                    "scanout-acquire: event UNREGISTER failed hr=0x{:08x} (KMD owner reclaim is the backstop)",
                    hr as u32
                );
            }
        }
        // SAFETY: `entry.event` is the handle this module created and owns;
        // the DXVK signaler that waited on it joined before the bridge drop
        // returned, so no waiter survives.
        unsafe { CloseHandle(entry.event as *mut core::ffi::c_void) };
    }
    if entry.ledger_va != 0 {
        // SAFETY: as for the unregister escape above.
        let unmapped = unsafe {
            escape_map_ledger(
                kt_callbacks,
                entry.rt_adapter,
                entry.h_rt_device,
                HELIOS_SCANOUT_ACQ_OP_UNMAP,
            )
        };
        if let Err(hr) = unmapped {
            log_error!(
                "scanout-acquire: ledger UNMAP failed hr=0x{:08x} (KMD owner reclaim is the backstop)",
                hr as u32
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The reader surface the DXVK engine resolves by name (helios_scanout_*).
//
// Exported with the C ABI so dxvk_helios_scanout_acquire.cpp can GetProcAddress
// them out of this DLL. bool return/parameters are the one-byte C `_Bool` on
// both sides of the seam. None of these can panic: with panic = "abort" in
// both profiles an unwind here would kill dwm.
// ---------------------------------------------------------------------------

/// Fast-path flag: knob ON, probe OK, at least one live ledger mapping. The
/// single check DXVK's flush path performs before doing anything else.
#[no_mangle]
pub extern "C" fn helios_scanout_acquire_enabled() -> bool {
    ENABLED.load(Ordering::Acquire) != 0
}

/// Reader protocol from the protocol doc ([`HeliosReadLedgerSlot`]): find the
/// slot by resid; Acquire-read `issued`, `retired`; RE-READ `resid` — if it
/// changed, a mid-read reclaim happened and every read of this resource has
/// retired, so answer "no slot" (= no wait). `false` = no slot / feature off.
#[no_mangle]
pub extern "C" fn helios_scanout_ledger_lookup(
    resid: u32,
    out_issued: *mut u32,
    out_retired: *mut u32,
) -> bool {
    if resid == 0 || out_issued.is_null() || out_retired.is_null() {
        return false;
    }
    if ENABLED.load(Ordering::Acquire) == 0 {
        return false;
    }
    let Ok(reg) = REGISTRY.lock() else {
        return false;
    };
    let Some(va) = reg.iter().find(|e| e.ledger_va != 0).map(|e| e.ledger_va) else {
        return false;
    };
    // SAFETY: `va` belongs to a registered entry and the registry mutex is
    // held for the whole read, so the mapping cannot be torn down under us.
    let slot_count = unsafe { ledger_load(va, LEDGER_SLOT_COUNT_OFF) } as usize;
    let slots = slot_count.min(HELIOS_READ_LEDGER_SLOTS);
    for i in 0..slots {
        let slot = va + LEDGER_SLOTS_OFF + i * SLOT_SIZE;
        // SAFETY: as above; `slot` stays inside the page for i < 8.
        if unsafe { ledger_load(slot, SLOT_RESID_OFF) } != resid {
            continue;
        }
        // SAFETY: as above.
        let issued = unsafe { ledger_load(slot, SLOT_ISSUED_OFF) };
        // SAFETY: as above.
        let retired = unsafe { ledger_load(slot, SLOT_RETIRED_OFF) };
        // SAFETY: as above — the re-read that closes the reclaim race.
        if unsafe { ledger_load(slot, SLOT_RESID_OFF) } != resid {
            return false;
        }
        // SAFETY: out pointers were null-checked; the caller owns them.
        unsafe {
            *out_issued = issued;
            *out_retired = retired;
        }
        return true;
    }
    false
}

/// Snapshot up to `max_slots` ledger slots as (resid, issued, retired) `u32`
/// triples for the DXVK signaler's level-triggered pass. A slot that fails its
/// resid re-read is reported with resid = 0 (mid-read reclaim = fully
/// retired). Returns the number of triples written (0 = feature off).
#[no_mangle]
pub extern "C" fn helios_scanout_ledger_snapshot(
    out_triples: *mut u32,
    max_slots: u32,
) -> u32 {
    if out_triples.is_null() || max_slots == 0 {
        return 0;
    }
    if ENABLED.load(Ordering::Acquire) == 0 {
        return 0;
    }
    let Ok(reg) = REGISTRY.lock() else {
        return 0;
    };
    let Some(va) = reg.iter().find(|e| e.ledger_va != 0).map(|e| e.ledger_va) else {
        return 0;
    };
    // SAFETY: see helios_scanout_ledger_lookup — same mutex-held contract.
    let slot_count = unsafe { ledger_load(va, LEDGER_SLOT_COUNT_OFF) } as usize;
    let slots = slot_count
        .min(HELIOS_READ_LEDGER_SLOTS)
        .min(max_slots as usize);
    for i in 0..slots {
        let slot = va + LEDGER_SLOTS_OFF + i * SLOT_SIZE;
        // SAFETY: as above.
        let resid = unsafe { ledger_load(slot, SLOT_RESID_OFF) };
        // SAFETY: as above.
        let issued = unsafe { ledger_load(slot, SLOT_ISSUED_OFF) };
        // SAFETY: as above.
        let retired = unsafe { ledger_load(slot, SLOT_RETIRED_OFF) };
        // SAFETY: as above — mid-read reclaim reports as a free slot.
        let resid = if unsafe { ledger_load(slot, SLOT_RESID_OFF) } == resid {
            resid
        } else {
            0
        };
        // SAFETY: the caller promises `max_slots` triples of capacity; `i` is
        // bounded by `slots <= max_slots`.
        unsafe {
            *out_triples.add(i * 3) = resid;
            *out_triples.add(i * 3 + 1) = issued;
            *out_triples.add(i * 3 + 2) = retired;
        }
    }
    slots as u32
}
