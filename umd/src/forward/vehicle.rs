//! The dcomp-vehicle present producer.
//!
//! The in-process hand-off the Mesa ICD's WSI uses: `set_present_source` parks
//! one frame's venus identity in a thread-local, the next `dxgi_present` on
//! THAT THREAD consumes it, and `wait_last_present` is the ICD's recycle gate.
//!
//! Moved verbatim out of `forward.rs` by T8/R1108. The three TLS cells are
//! declared HERE and are private to this module, so "only the vehicle entry
//! point may consume `PRESENT_SOURCE`" is module privacy rather than a comment,
//! and the same-thread contract has exactly one place to be stated.
//!
//! ⚠ NOT here, deliberately: the bounded frame gate (`run_present_frame_gate`,
//! `GateOutcome`, `PRESENT_GATE_*` and `EXT_FLIP_GATE_TIMEOUTS`). It is the
//! frozen baseline's frame-completion contract, it runs on EVERY present and
//! not only on vehicle presents, and its position relative to the flush is the
//! one thing this item must not move.

use super::*;

/// Pending vehicle present source (one per thread; same-thread contract).
#[derive(Clone, Copy)]
pub struct PresentSource {
    pub resid: u32,
    pub fence_value: u64,
    pub width: u32,
    pub height: u32,
    pub dxgi_format: u32,
    /// Creator's exact vkAllocateMemory size/type — the typed import
    /// identity (vkr's OPAQUE-fd import needs an exact-size match; importing
    /// at the opener's own requirements is the wrong-size failure mode).
    pub alloc_size: u64,
    pub memory_type_index: u32,
}

/// The dcomp-vehicle present protocol as ONE state machine.
///
/// It used to be three independent `Cell`s — a pending source, a raw
/// `HeliosDevice` pointer, and a pending (fenceId, value) — each path had to
/// remember to update in lockstep, with the ordering enforced only by comments
/// and by counters that fire after the fact. Two failure arms cleared two of
/// the three, and `dxgi_present` had an `E_FAIL` exit that cleared neither, so
/// the ICD could consume frame N's `(fenceId, value)` for frame N+1 and recycle
/// an image on a fence that had already retired.
///
/// Cross-DLL sequence, one thread, once per frame:
/// `helios_umd_set_present_source` -> `Present` ->
/// `helios_umd_get_present_result` -> optional `helios_umd_wait_last_present`.
/// Every exit now has to name a next state.
///
/// `Copy` on purpose: a `Cell` cannot panic, where a `RefCell` can double-borrow
/// — and these are reached from `extern "C"` exports under `panic = "abort"`.
#[derive(Clone, Copy)]
pub(crate) enum VehicleSlot {
    Idle,
    /// A source was armed and the vehicle `Present` has not consumed it yet.
    Armed(PresentSource),
    /// A vehicle present was MINTED on `device`, which
    /// `helios_umd_wait_last_present` then targets. R912(a) removed the
    /// `result: Option<(u32, u64)>` half: it could only ever be `None`, since
    /// its only producer was `present_sync_publish` behind a knob that
    /// defaulted off.
    Minted {
        device: usize,
    },
}

thread_local! {
    pub(crate) static VEHICLE: core::cell::Cell<VehicleSlot> =
        const { core::cell::Cell::new(VehicleSlot::Idle) };
}

/// `set_present_source` refusals (invalid geometry/resid from the ICD).
pub(crate) static EXT_SOURCE_REFUSED: AtomicUsize = AtomicUsize::new(0);
/// `wait_last_present` calls whose recorded device is no longer live.
pub(crate) static EXT_WAIT_DEAD_DEVICE: AtomicUsize = AtomicUsize::new(0);

pub(crate) static EXT_PRESENTS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static EXT_IMPORT_FAILS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static EXT_COPY_FAILS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static EXT_GEOM_MISMATCH: AtomicUsize = AtomicUsize::new(0);
pub(crate) static EXT_OVERWRITES: AtomicUsize = AtomicUsize::new(0);
pub(crate) static EXT_NO_DEVICE: AtomicUsize = AtomicUsize::new(0);

/// Backing for the `helios_umd_set_present_source` C export.
pub fn set_present_source(
    resid: u32,
    fence_value: u64,
    width: u32,
    height: u32,
    dxgi_format: u32,
    alloc_size: u64,
    memory_type_index: u32,
) -> i32 {
    if resid == 0 || width == 0 || height == 0 || dxgi_format == 0 {
        EXT_SOURCE_REFUSED.fetch_add(1, Ordering::Relaxed);
        log_error!(
            "set_present_source REFUSED: resid={} {}x{} fmt={}",
            resid,
            width,
            height,
            dxgi_format
        );
        return -1;
    }
    let prev = VEHICLE.with(|c| {
        c.replace(VehicleSlot::Armed(PresentSource {
            resid,
            fence_value,
            width,
            height,
            dxgi_format,
            alloc_size,
            memory_type_index,
        }))
    });
    match prev {
        VehicleSlot::Armed(_) => {
            // A pending source nobody consumed: a Present() that never reached
            // our DDI, or a same-thread-contract violation. Count loudly; the
            // new source replaces it.
            let n = EXT_OVERWRITES.fetch_add(1, Ordering::Relaxed);
            if n < 16 || n % 512 == 0 {
                log_error!(
                    "set_present_source: overwrote a pending source (x{})",
                    n + 1
                );
            }
            1
        }
        VehicleSlot::Idle | VehicleSlot::Minted { .. } => 0,
    }
}

/// Backing for the `helios_umd_wait_last_present` C export: bounded wait for
/// the last vehicle present's submission (frame copy included) to complete
/// on the GPU. 0 = complete, 1 = timeout, -1 = no vehicle present recorded
/// on this thread.
pub fn wait_last_present(timeout_us: u32) -> i32 {
    let dev_ptr = match VEHICLE.with(|c| c.get()) {
        VehicleSlot::Minted { device, .. } => device,
        VehicleSlot::Idle | VehicleSlot::Armed(_) => return -1,
    };
    if dev_ptr == 0 {
        return -1;
    }
    if !device_is_live(dev_ptr) {
        // The recorded device was destroyed without this slot being cleared —
        // dxgkrnl may already have reused its private block. Refuse rather than
        // dereference it, and drop the slot so the refusal is not repeated.
        VEHICLE.with(|c| c.set(VehicleSlot::Idle));
        let n = EXT_WAIT_DEAD_DEVICE.fetch_add(1, Ordering::Relaxed);
        if n < 16 || n % 512 == 0 {
            log_error!(
                "wait_last_present REFUSED: recorded device 0x{dev_ptr:x} is no longer live (x{})",
                n + 1
            );
        }
        return -1;
    }
    // SAFETY: same-thread contract — the ICD calls this immediately after
    // the vehicle Present() returned on this thread, so the device the
    // present ran on is still alive (the ICD holds the vehicle D3D11
    // device reference) — now backed by the liveness check above rather than
    // by that contract alone.
    let dev = unsafe { &*(dev_ptr as *const HeliosDevice) };
    // COMPLETE, always: this is the ICD's image-RECYCLE guard, so
    // the question it asks is "has the vehicle's copy finished reading the
    // frame", which only GPU completion answers. The present-ordering handshake
    // that the ordinary present path answers is a different question with a
    // different answer -- which is why deleting the `PresentOrder` knob (owner
    // directive, 2026-07-29) does not touch this call.
    if dev.dxvk.present_frame_gate(timeout_us, PRESENT_ORDER_COMPLETE) {
        0
    } else {
        1
    }
}

/// The vehicle present body: cached alias-import of the ICD frame, image copy
/// into the backbuffer. On error the caller must FAIL the present (no
/// pfnPresentCb) so the ICD latches its sw fallback instead of flipping a
/// stale backbuffer.
///
/// It used to also publish the backbuffer slot with this device's fence and
/// return `(sync_value, fence_id)`; R912(a) retired that producer, so there is
/// nothing left to hand back.
pub(crate) unsafe fn vehicle_present_prepare(
    h: Hdevice,
    backbuffer_h: ddi::D3D10DDI_HRESOURCE,
    info: &PresentSource,
) -> Result<(), i32> {
    let Some(dev) = helios_device(h) else {
        EXT_NO_DEVICE.fetch_add(1, Ordering::Relaxed);
        log_error!("vehicle present FAILED: no Helios device");
        return Err(E_FAIL);
    };
    let backbuffer_raw = resource_com_raw(backbuffer_h);
    if backbuffer_raw == 0 {
        EXT_NO_DEVICE.fetch_add(1, Ordering::Relaxed);
        log_error!("vehicle present FAILED: backbuffer has no COM resource");
        return Err(E_FAIL);
    }

    // Cached alias-import by resid; geometry/format change invalidates the
    // entry (swapchain recreates give new resids, so also cap the cache).
    let mut imported_raw = {
        let mut cache = dev.owned.present_src_cache.borrow_mut();
        match cache.iter().position(|e| e.resid == info.resid) {
            Some(pos)
                if cache[pos].width == info.width
                    && cache[pos].height == info.height
                    && cache[pos].dxgi_format == info.dxgi_format =>
            {
                cache[pos].resource_raw
            }
            Some(pos) => {
                cache.remove(pos); // drop releases the stale import
                0
            }
            None => 0,
        }
    };
    if imported_raw == 0 {
        let opened = dev.dxvk.open_texture2d(
            info.width,
            info.height,
            info.dxgi_format,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            0,
            // `global` is log-only in the bridge but must be nonzero; there
            // is no KMT handle on this in-process path — carry the resid.
            info.resid,
            info.resid,
            info.alloc_size,
            info.memory_type_index,
            // Not the DWM scan-out primary import; keep the plain OPTIMAL path.
            false,
            false,
            false,
        );
        let Some(imported) = opened else {
            let n = EXT_IMPORT_FAILS.fetch_add(1, Ordering::Relaxed);
            if n < 16 || n % 512 == 0 {
                log_error!(
                    "vehicle present FAILED: import resid={} {}x{} fmt={} alloc={} type={} (x{})",
                    info.resid,
                    info.width,
                    info.height,
                    info.dxgi_format,
                    info.alloc_size,
                    info.memory_type_index,
                    n + 1
                );
            }
            return Err(E_FAIL);
        };
        // PresentSrcEntry owns the reference from here; into_raw hands it over
        // so the adopted wrapper does not release it on drop.
        let raw = imported.into_raw() as usize;
        let mut cache = dev.owned.present_src_cache.borrow_mut();
        if cache.len() >= 16 {
            cache.remove(0);
        }
        cache.push(crate::device_funcs::PresentSrcEntry {
            resid: info.resid,
            width: info.width,
            height: info.height,
            dxgi_format: info.dxgi_format,
            resource_raw: raw,
        });
        imported_raw = raw;
    }

    match dev
        .dxvk
        .present_vehicle_copy(DstRes(backbuffer_raw), SrcRes(imported_raw))
    {
        0 => {}
        1 => {
            EXT_GEOM_MISMATCH.fetch_add(1, Ordering::Relaxed);
        }
        rc => {
            let n = EXT_COPY_FAILS.fetch_add(1, Ordering::Relaxed);
            if n < 16 || n % 512 == 0 {
                log_error!(
                    "vehicle present FAILED: copy rc={} resid={} (x{})",
                    rc,
                    info.resid,
                    n + 1
                );
            }
            return Err(E_FAIL);
        }
    }

    Ok(())
}
