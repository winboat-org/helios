//! Queries, predication, multisample quality levels and performance counters.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

// --- Queries / counters -----------------------------------------------------

pub(crate) unsafe extern "C" fn calc_size_query(
    _h: Hdevice,
    _a: *const ddi::D3D10DDIARG_CREATEQUERY,
) -> u64 {
    8
}

pub(crate) unsafe extern "C" fn create_query(
    h: Hdevice,
    arg: *const ddi::D3D10DDIARG_CREATEQUERY,
    h_query: ddi::D3D10DDI_HQUERY,
    _hrt: ddi::D3D10DDI_HRTQUERY,
) {
    clear_handle(h_query);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY(a.Query),
        MiscFlags: a.MiscFlags,
    };
    let mut q: Option<ID3D11Query> = None;
    match device.CreateQuery(&desc, Some(&mut q)) {
        Ok(()) => {
            if let Some(query) = q {
                store_com(h_query, query);
            }
        }
        Err(e) => log_error!("DDI create_query failed: {e:?}"),
    }
}

pub(crate) unsafe extern "C" fn destroy_query(_h: Hdevice, h_query: ddi::D3D10DDI_HQUERY) {
    release_com(h_query);
}

pub(crate) unsafe extern "C" fn query_begin(h: Hdevice, h_query: ddi::D3D10DDI_HQUERY) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(q) = load_com::<ID3D11Query>(h_query) else {
        return;
    };
    if let Ok(async_) = (*q).cast::<ID3D11Asynchronous>() {
        context.Begin(&async_);
    }
}

pub(crate) unsafe extern "C" fn query_end(h: Hdevice, h_query: ddi::D3D10DDI_HQUERY) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(q) = load_com::<ID3D11Query>(h_query) else {
        return;
    };
    if let Ok(async_) = (*q).cast::<ID3D11Asynchronous>() {
        context.End(&async_);
    }
}

pub(crate) unsafe extern "C" fn query_get_data(
    h: Hdevice,
    h_query: ddi::D3D10DDI_HQUERY,
    data: *mut c_void,
    data_size: u32,
    flags: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(q) = load_com::<ID3D11Query>(h_query) else {
        return;
    };
    if let Ok(async_) = (*q).cast::<ID3D11Asynchronous>() {
        // `windows` exposes ID3D11DeviceContext::GetData as `Result<()>`.
        // That is unsuitable here: HRESULT::ok() maps both S_OK and S_FALSE
        // to Ok and therefore discards the query's pending state. The DDI
        // callback returns void, so silently dropping S_FALSE makes the D3D
        // runtime report S_OK with an unchanged output buffer (zero Frequency
        // for a timestamp-disjoint query, for example). Preserve the raw
        // HRESULT and translate it through the core-layer callback exactly as
        // PFND3D10DDI_QUERYGETDATA requires.
        let hr = (Interface::vtable(&*context).GetData)(
            Interface::as_raw(&*context),
            Interface::as_raw(&async_),
            data,
            data_size,
            flags,
        )
        .0;
        if hr == crate::hr::S_FALSE {
            set_runtime_error(h, crate::hr::DXGI_DDI_ERR_WASSTILLDRAWING);
        } else if hr < 0 {
            set_runtime_error(h, hr);
        }
    }
}

pub(crate) unsafe extern "C" fn set_predication(
    h: Hdevice,
    h_query: ddi::D3D10DDI_HQUERY,
    predicate_value: i32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let predicate =
        load_com::<ID3D11Query>(h_query).and_then(|q| (*q).cast::<ID3D11Predicate>().ok());
    context.SetPredication(predicate.as_ref(), predicate_value != 0);
}

/// Shared rate cap for the three MSAA log sites (R829).
pub(crate) static MSAA_LOG_COUNT: LogThrottle = LogThrottle::new();

/// D3D11 multisample-quality caps, keyed off the active feature-level profile.
///
/// The Microsoft runtime validates `CheckFormatSupport` and
/// `CheckMultisampleQualityLevels` as a coherent feature-level contract during
/// `CDevice::LLOCompleteLayerConstruction`. The FL10.0 profile expresses a
/// no-multisample device (1x only) coherently with `check_format_support`
/// stripping the multisample bits. The FL11_0 profile advertises 1x, 4x, 8x and
/// the optional standard patterns (2x/16x) for EVERY output-capable format. The
/// runtime rejects arbitrary non-power-of-two sample counts.
///
/// R829 (OWNER DECISION): this doc previously claimed the D3D11.3 §19.2.5
/// exception -- 8x only for output formats *below* 128 bits/sample -- which the
/// code has never implemented. The decision was to correct the DOC, not the
/// code. §19.2.5 is a FLOOR, not a ceiling: a driver may advertise above it,
/// and the caps/quality pair stays internally coherent either way because
/// `check_format_support` uses the SAME
/// `dxgi_msaa_bits_per_sample(fmt, caps).is_some()` predicate.
///
/// What made this a decision rather than a cleanup, and worth knowing before
/// revisiting it: `dxgi_msaa_bits_per_sample` resolves to a static format table
/// plus the DXVK caps word. It never asks whether that SAMPLE COUNT is
/// supported, so today's "8x on a 128-bit format" is a table assertion, not a
/// capability probe. Implementing the floor would narrow the claim; probing
/// DXVK would make it true. Neither is done here -- both are behaviour changes
/// on the default-live FL11 caps path, which this tranche freezes.
pub(crate) unsafe fn helios_multisample_quality_levels(
    h: Hdevice,
    fmt: ddi::DXGI_FORMAT,
    sample_count: u32,
) -> u32 {
    if crate::caps::feature_profile().msaa == crate::caps::MsaaPolicy::SingleSampleOnly {
        return if sample_count == 1 { 1 } else { 0 };
    }
    if sample_count == 0 {
        return 0;
    }
    let Some(device) = d3d11_device(h) else {
        // DXVK unreachable: fall back to the conservative single-sample answer.
        return if sample_count == 1 { 1 } else { 0 };
    };
    let caps = device
        .CheckFormatSupport(DXGI_FORMAT(fmt as i32))
        .unwrap_or(0);
    let output_bits = dxgi_msaa_bits_per_sample(fmt as u32, caps);
    // The two arms were identical -- (1|2|4|16, Some(_)) and (8, Some(_)) both
    // yielding true -- which is what made the doc's 128-bit exception look
    // implemented. Collapsed; `output_bits` is still bound for the log, which
    // is the only thing that ever consumed it.
    let required = matches!((sample_count, output_bits), (1 | 2 | 4 | 8 | 16, Some(_)));
    let val = if required { 1 } else { 0 };
    // DECLARED diagnostic-volume change (R829): this site fired whenever
    // `required || sample_count <= 8`, i.e. on essentially every query, and the
    // two public wrappers below logged unconditionally with no cap at all. All
    // three now share one throttle.
    if (required || sample_count <= 8) && MSAA_LOG_COUNT.first_n_then_every(256, 4096).is_some() {
        trace_line!(
            "MSAA q fmt={fmt} c={sample_count} output_bits={output_bits:?} required={required} -> {val}"
        );
    }
    val
}

pub(crate) unsafe extern "C" fn check_multisample_quality_levels(
    h: Hdevice,
    fmt: ddi::DXGI_FORMAT,
    sample_count: u32,
    out: *mut u32,
) {
    if !out.is_null() {
        let val = helios_multisample_quality_levels(h, fmt, sample_count);
        *out = val;
        if MSAA_LOG_COUNT.first_n_then_every(256, 4096).is_some() {
            trace_line!("MSAA out fmt={fmt} c={sample_count} flags=legacy out={out:p} val={val}");
        }
    }
}

pub(crate) unsafe extern "C" fn check_multisample_quality_levels_wddm1_3(
    h: Hdevice,
    fmt: ddi::DXGI_FORMAT,
    sample_count: u32,
    _flags: u32,
    out: *mut u32,
) {
    if !out.is_null() {
        let val = helios_multisample_quality_levels(h, fmt, sample_count);
        *out = val;
        if MSAA_LOG_COUNT.first_n_then_every(256, 4096).is_some() {
            trace_line!(
                "MSAA out fmt={fmt} c={sample_count} flags=0x{_flags:x} out={out:p} val={val}"
            );
        }
    }
}

/// `pfnCheckCounterInfo` — report the device's performance-counter capabilities.
/// Previously an unimplemented noop that left the out struct unwritten, so the
/// D3D11 runtime read whatever it had pre-set for `NumSimultaneousCounters` /
/// `NumDetectableParallelUnits` (potentially garbage → over-allocation/validation
/// failure during `LLOCompleteLayerConstruction`). We expose no device-dependent
/// counters: zero the struct (LastDeviceDependentCounter = 0, 0 simultaneous
/// counters) and report a single detectable parallel unit. PATH-A (2026-06-22).
pub(crate) unsafe extern "C" fn check_counter_info(
    _h: Hdevice,
    info: *mut ddi::D3D10DDI_COUNTER_INFO,
) {
    if !info.is_null() {
        core::ptr::write_bytes(
            info as *mut u8,
            0,
            core::mem::size_of::<ddi::D3D10DDI_COUNTER_INFO>(),
        );
        (*info).NumDetectableParallelUnits = 1;
    }
}

pub(crate) unsafe extern "C" fn check_counter(
    _h: Hdevice,
    _query: ddi::D3D10DDI_QUERY,
    counter_type: *mut ddi::D3D10DDI_COUNTER_TYPE,
    active_counters: *mut u32,
    _name: ddi::LPSTR,
    name_len: *mut u32,
    _units: ddi::LPSTR,
    units_len: *mut u32,
    _description: ddi::LPSTR,
    description_len: *mut u32,
) {
    if !counter_type.is_null() {
        *counter_type = ddi::D3D10DDI_COUNTER_TYPE_D3D10DDI_COUNTER_TYPE_UINT64;
    }
    if !active_counters.is_null() {
        *active_counters = 0;
    }
    if !name_len.is_null() {
        *name_len = 0;
    }
    if !units_len.is_null() {
        *units_len = 0;
    }
    if !description_len.is_null() {
        *description_len = 0;
    }
}
