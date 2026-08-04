//! The DXGI present path, and the DXGI DDIs that ride with it.
//!
//! `finish_present` and `dxgi_present`, the runtime submission plumbing, the
//! readback and force-opaque debug instruments, resource-identity rotation,
//! Blt/Blt1, offer/reclaim, the multiplane-overlay surface and Present1.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

// --- DXGI present -----------------------------------------------------------

pub(crate) unsafe fn dxgi_device_handle(h: ddi::DXGI_DDI_HDEVICE) -> Hdevice {
    Hdevice {
        pDrvPrivate: h as *mut c_void,
    }
}

pub(crate) unsafe fn dxgi_resource_handle(h: ddi::DXGI_DDI_HRESOURCE) -> ddi::D3D10DDI_HRESOURCE {
    ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: h as *mut c_void,
    }
}

pub(crate) unsafe fn maybe_log_present_readback(h: Hdevice, src_h: ddi::D3D10DDI_HRESOURCE) {
    if !present_readback_enabled() {
        return;
    }
    let n = PRESENT_READBACK_LOG_COUNT.next();
    if n >= 8 {
        return;
    }
    let Some(device) = d3d11_device(h) else {
        log_error!("DXGI Present readback: no D3D11 device");
        return;
    };
    let Some(context) = d3d11_context(h) else {
        log_error!("DXGI Present readback: no D3D11 context");
        return;
    };
    let Some(res) = load_resource(src_h) else {
        log_error!("DXGI Present readback: source resource missing");
        return;
    };
    let Ok(tex) = (*res).cast::<ID3D11Texture2D>() else {
        log_error!("DXGI Present readback: source is not Texture2D");
        return;
    };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    if desc.Width == 0 || desc.Height == 0 || desc.SampleDesc.Count != 1 {
        log_error!(
            "DXGI Present readback: unsupported {}x{} fmt={} sample={}x{}",
            desc.Width,
            desc.Height,
            desc.Format.0,
            desc.SampleDesc.Count,
            desc.SampleDesc.Quality
        );
        return;
    }

    let mut staging_desc = desc;
    staging_desc.MipLevels = 1;
    staging_desc.ArraySize = 1;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    staging_desc.MiscFlags = 0;
    let mut staging: Option<ID3D11Texture2D> = None;
    if let Err(e) = device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) {
        log_error!("DXGI Present readback: staging create failed {e:?}");
        return;
    }
    let Some(staging) = staging else {
        log_error!("DXGI Present readback: staging create returned None");
        return;
    };
    let Ok(staging_res) = staging.cast::<ID3D11Resource>() else {
        log_error!("DXGI Present readback: staging cast failed");
        return;
    };
    context.CopyResource(&staging_res, &*res);

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    if let Err(e) = context.Map(&staging_res, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) {
        log_error!("DXGI Present readback: map failed {e:?}");
        return;
    }
    let bpp = dxgi_bytes_per_pixel(desc.Format.0 as u32).max(1) as usize;
    let row_pitch = mapped.RowPitch as usize;
    // `dxgi_bytes_per_pixel` is a PITCH-PADDING estimate, not a true bpp: its
    // 4-byte default covers the genuinely 16-bpp B5G6R5 / B5G5R5A1 / B4G4R4A4
    // formats and every block-compressed format. Over-reporting is harmless
    // where it is used to pad `linear_size`, but here it is a byte-addressing
    // stride, and `(Width - 1) * bpp` then runs past the row -- on the LAST
    // row, past the end of the mapping. `maybe_force_present_alpha_opaque`
    // already guards its own indexing with a hard `bpp != 4`; this is the
    // same refusal, expressed against the pitch the runtime actually mapped
    // so every currently-correct width/format still reads.
    let last_sample_end = (desc.Width.saturating_sub(1) as usize)
        .saturating_mul(bpp)
        .saturating_add(bpp.min(4));
    if row_pitch == 0 || last_sample_end > row_pitch {
        note_ddi_refusal(&DDI_REFUSALS.readback_stride_unsafe);
        log_error!(
            "DXGI Present readback: stride would leave the mapping, refusing \
             {}x{} fmt={} bpp={} row_pitch={} last_sample_end={}",
            desc.Width,
            desc.Height,
            desc.Format.0,
            bpp,
            row_pitch,
            last_sample_end
        );
        context.Unmap(&staging_res, 0);
        return;
    }
    let data = mapped.pData as *const u8;
    let mut sum: u64 = 0;
    let mut nonzero = 0u32;
    for y in 0..4u32 {
        for x in 0..4u32 {
            let sx = ((desc.Width - 1) as u64 * x as u64 / 3) as usize;
            let sy = ((desc.Height - 1) as u64 * y as u64 / 3) as usize;
            let p = data.add(sy * row_pitch + sx * bpp);
            let mut px = 0u32;
            for c in 0..bpp.min(4) {
                let v = *p.add(c) as u32;
                px |= v << (c * 8);
                sum += v as u64;
            }
            if px != 0 {
                nonzero += 1;
            }
        }
    }
    let cx = (desc.Width / 2) as usize;
    let cy = (desc.Height / 2) as usize;
    let cp = data.add(cy * row_pitch + cx * bpp);
    let mut center = 0u32;
    for c in 0..bpp.min(4) {
        center |= (*cp.add(c) as u32) << (c * 8);
    }
    let mut frame_sum: u64 = 0;
    let mut frame_nonzero = 0u64;
    if std::env::var_os("HELIOS_PRESENT_DUMP_DIR").is_some() {
        for y in 0..desc.Height as usize {
            for x in 0..desc.Width as usize {
                let p = data.add(y * row_pitch + x * bpp);
                let mut px = 0u32;
                for c in 0..bpp.min(4) {
                    let v = *p.add(c) as u32;
                    px |= v << (c * 8);
                    frame_sum = frame_sum.wrapping_add(v as u64);
                }
                if px != 0 {
                    frame_nonzero += 1;
                }
            }
        }
        if bpp >= 4 {
            if let Some(dir) = std::env::var_os("HELIOS_PRESENT_DUMP_DIR") {
                let _ = std::fs::create_dir_all(&dir);
                let pid = std::process::id();
                let path = std::path::PathBuf::from(dir).join(format!(
                    "present-{pid}-{:03}-{}x{}-fmt{}.bmp",
                    n + 1,
                    desc.Width,
                    desc.Height,
                    desc.Format.0
                ));
                if let Err(e) = write_bgra32_bmp(&path, data, row_pitch, desc.Width, desc.Height) {
                    log_error!("DXGI Present readback dump failed: {e}");
                } else {
                    log_error!("DXGI Present readback dump: {}", path.display());
                }
            }
        } else {
            log_error!(
                "DXGI Present readback dump skipped: bpp={} unsupported",
                bpp
            );
        }
    }
    context.Unmap(&staging_res, 0);
    log_error!(
        "DXGI Present readback #{}: {}x{} fmt={} bpp={} grid_sum={} nonzero={} center=0x{:08x} frame_sum={} frame_nonzero={}",
        n + 1,
        desc.Width,
        desc.Height,
        desc.Format.0,
        bpp,
        sum,
        nonzero,
        center,
        frame_sum,
        frame_nonzero
    );
}

pub(crate) unsafe fn write_bgra32_bmp(
    path: &std::path::Path,
    data: *const u8,
    row_pitch: usize,
    width: u32,
    height: u32,
) -> std::io::Result<()> {
    use std::io::Write;

    let row_bytes = width as usize * 4;
    let image_size = row_bytes * height as usize;
    let file_size = 14usize + 40usize + image_size;

    let mut file = std::fs::File::create(path)?;
    file.write_all(b"BM")?;
    file.write_all(&(file_size as u32).to_le_bytes())?;
    file.write_all(&0u16.to_le_bytes())?;
    file.write_all(&0u16.to_le_bytes())?;
    file.write_all(&54u32.to_le_bytes())?;

    file.write_all(&40u32.to_le_bytes())?;
    file.write_all(&(width as i32).to_le_bytes())?;
    // Negative height stores top-down rows, matching D3D's mapped row order.
    file.write_all(&(-(height as i32)).to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&32u16.to_le_bytes())?;
    file.write_all(&0u32.to_le_bytes())?;
    file.write_all(&(image_size as u32).to_le_bytes())?;
    file.write_all(&2835i32.to_le_bytes())?;
    file.write_all(&2835i32.to_le_bytes())?;
    file.write_all(&0u32.to_le_bytes())?;
    file.write_all(&0u32.to_le_bytes())?;

    for y in 0..height as usize {
        let row = std::slice::from_raw_parts(data.add(y * row_pitch), row_bytes);
        file.write_all(row)?;
    }

    Ok(())
}

pub(crate) unsafe fn maybe_force_present_alpha_opaque(h: Hdevice, src_h: ddi::D3D10DDI_HRESOURCE) {
    if !present_force_opaque_enabled() {
        return;
    }

    let n = PRESENT_FORCE_OPAQUE_LOG_COUNT.next();
    let Some(device) = d3d11_device(h) else {
        if n < 8 {
            log_error!("DXGI Present force-opaque: no D3D11 device");
        }
        return;
    };
    let Some(context) = d3d11_context(h) else {
        if n < 8 {
            log_error!("DXGI Present force-opaque: no D3D11 context");
        }
        return;
    };
    let Some(res) = load_resource(src_h) else {
        if n < 8 {
            log_error!("DXGI Present force-opaque: source resource missing");
        }
        return;
    };
    let Ok(tex) = (*res).cast::<ID3D11Texture2D>() else {
        if n < 8 {
            log_error!("DXGI Present force-opaque: source is not Texture2D");
        }
        return;
    };

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    let bpp = dxgi_bytes_per_pixel(desc.Format.0 as u32);
    if desc.Width == 0 || desc.Height == 0 || desc.SampleDesc.Count != 1 || bpp != 4 {
        if n < 8 {
            log_error!(
                "DXGI Present force-opaque: unsupported {}x{} fmt={} bpp={} sample={}x{}",
                desc.Width,
                desc.Height,
                desc.Format.0,
                bpp,
                desc.SampleDesc.Count,
                desc.SampleDesc.Quality
            );
        }
        return;
    }

    let mut staging_desc = desc;
    staging_desc.MipLevels = 1;
    staging_desc.ArraySize = 1;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32;
    staging_desc.MiscFlags = 0;

    let mut staging: Option<ID3D11Texture2D> = None;
    if let Err(e) = device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) {
        if n < 8 {
            log_error!("DXGI Present force-opaque: staging create failed {e:?}");
        }
        return;
    }
    let Some(staging) = staging else {
        if n < 8 {
            log_error!("DXGI Present force-opaque: staging create returned None");
        }
        return;
    };
    let Ok(staging_res) = staging.cast::<ID3D11Resource>() else {
        if n < 8 {
            log_error!("DXGI Present force-opaque: staging cast failed");
        }
        return;
    };

    context.CopyResource(&staging_res, &*res);

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    if let Err(e) = context.Map(&staging_res, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut mapped)) {
        if n < 8 {
            log_error!("DXGI Present force-opaque: map failed {e:?}");
        }
        return;
    }

    let row_pitch = mapped.RowPitch as usize;
    let data = mapped.pData as *mut u8;
    let mut alpha_zero = 0u64;
    let mut alpha_non_opaque = 0u64;
    for y in 0..desc.Height as usize {
        for x in 0..desc.Width as usize {
            let alpha = data.add(y * row_pitch + x * 4 + 3);
            let old = *alpha;
            if old == 0 {
                alpha_zero += 1;
            }
            if old != 0xff {
                alpha_non_opaque += 1;
                *alpha = 0xff;
            }
        }
    }
    context.Unmap(&staging_res, 0);
    context.CopyResource(&*res, &staging_res);
    context.Flush();

    if n < 8 || (n + 1) % 512 == 0 {
        log_error!(
            "DXGI Present force-opaque #{}: {}x{} fmt={} alpha_zero={} alpha_non_opaque={}",
            n + 1,
            desc.Width,
            desc.Height,
            desc.Format.0,
            alpha_zero,
            alpha_non_opaque
        );
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RuntimePresentDependencies {
    pub(crate) source: core::num::NonZeroU32,
    pub(crate) destination: Option<core::num::NonZeroU32>,
}

impl RuntimePresentDependencies {
    pub(crate) fn new(source: ddi::D3DKMT_HANDLE, destination: ddi::D3DKMT_HANDLE) -> Option<Self> {
        Some(Self {
            source: core::num::NonZeroU32::new(source)?,
            destination: core::num::NonZeroU32::new(destination),
        })
    }

    pub(crate) fn count(self) -> u32 {
        1 + u32::from(self.destination.is_some())
    }

    /// Populate the runtime-owned legacy allocation list used by pfnRenderCb.
    ///
    /// # Safety
    /// The list pointer and capacity came from pfnCreateContextCb or the
    /// preceding successful pfnRenderCb. This method validates both before
    /// writing exactly `count()` initialized entries.
    pub(crate) unsafe fn write_to(
        self,
        ctx: &crate::device_funcs::RuntimeContext,
    ) -> Result<u32, i32> {
        let required = self.count();
        // Pointer and capacity arrive together, so the `<` comparison cannot be
        // made against a capacity that describes a different pointer. The
        // comparison itself, and `required`, are unchanged from pre-R808.
        let window = ctx.allocations.get();
        let list = window.map_or(core::ptr::null_mut(), |w| w.ptr.as_ptr());
        let capacity = window.map_or(0, |w| w.capacity);
        if list.is_null() || capacity < required {
            log_error!(
                "DXGI Present: runtime allocation list unavailable ptr={:p} capacity={} required={}",
                list,
                capacity,
                required
            );
            return Err(E_FAIL);
        }

        let mut source = ddi::D3DDDI_ALLOCATIONLIST::default();
        source.hAllocation = self.source.get();
        // Value bit 0 is WriteOperation. The present source is read-only.
        source.__bindgen_anon_1.Value = 0;
        list.write(source);

        if let Some(destination) = self.destination {
            let mut entry = ddi::D3DDDI_ALLOCATIONLIST::default();
            entry.hAllocation = destination.get();
            // The present destination is written by the copy operation.
            entry.__bindgen_anon_1.Value = 1;
            list.add(1).write(entry);
        }

        Ok(required)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum RuntimeSubmission {
    /// A DXGI present carrying the scan-out identity, written as a
    /// `HeliosPresentRenderCmd`. The typed dependency value makes a
    /// source-allocation-free present submission unrepresentable.
    TypedPresent {
        dependencies: RuntimePresentDependencies,
        private: HeliosPresentPrivateData,
        correlation: PresentStreamCorrelation,
    },
    /// A DXGI present with no identity to carry, written as a
    /// `HeliosPresentRefreshCmd`. It still submits the present's allocation
    /// dependencies -- which is what distinguished it from the allocation-free
    /// `Refresh` variant, retired with the LINEAR copy path in R910: that
    /// marker's only trigger was `publish_dwm_composition` succeeding, and the
    /// KMD issues its own `HeliosPresentRefreshCmd` for the direct primary
    /// (`display.rs`), so the frozen refresh-marker ordering is unaffected.
    MarkerPresent {
        dependencies: RuntimePresentDependencies,
        correlation: PresentStreamCorrelation,
    },
}

/// The DXGI entry which led to a runtime present callback.
///
/// This is deliberately observation-only.  In particular, `Present1Single`
/// still takes the ordinary Present implementation after the DDI argument is
/// translated; retaining the tag is what makes that delegation visible in the
/// per-process probe summary.
#[derive(Clone, Copy)]
pub(crate) enum PresentBoundaryEntry {
    Present,
    Present1Single,
    Present1Multi,
    Mpo,
}

impl PresentBoundaryEntry {
    const fn name(self) -> &'static str {
        match self {
            Self::Present => "Present",
            Self::Present1Single => "Present1-single",
            Self::Present1Multi => "Present1-multi",
            Self::Mpo => "MPO",
        }
    }

    fn entry_counter(self) -> &'static AtomicUsize {
        match self {
            Self::Present => &PRESENT_BOUNDARY_PRESENT,
            Self::Present1Single => &PRESENT_BOUNDARY_PRESENT1_SINGLE,
            Self::Present1Multi => &PRESENT_BOUNDARY_PRESENT1_MULTI,
            Self::Mpo => &PRESENT_BOUNDARY_MPO,
        }
    }
}

// These are deliberately process-global: DXGI owns the DDI entry lifetime,
// while a process may create and destroy more than one UMD device. The probe
// uses relaxed counters only; allocation/string formatting happens in the
// first-N diagnostic lines or the PASSIVE DestroyDevice summary, never in the
// steady-state callback path.
static PRESENT_BOUNDARY_PRESENT: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_PRESENT1_SINGLE: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_PRESENT1_MULTI: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_MPO: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_RENDER_ATTEMPTED: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_RENDER_SUCCESS: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_RENDER_FAILURE: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_RENDER_MISSING: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_PRESENT_ATTEMPTED: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_PRESENT_SUCCESS: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_PRESENT_FAILURE: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_PRESENT_MISSING: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_MPO_ATTEMPTED: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_MPO_SUCCESS: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_MPO_FAILURE: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_MPO_MISSING: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_EARLY_REFUSAL: AtomicUsize = AtomicUsize::new(0);
static PRESENT_BOUNDARY_ENTRY_LOG_COUNT: LogThrottle = LogThrottle::new();
static PRESENT_BOUNDARY_CALLBACK_LOG_COUNT: LogThrottle = LogThrottle::new();
static PRESENT_BOUNDARY_REFUSAL_LOG_COUNT: LogThrottle = LogThrottle::new();

const PRESENT_BOUNDARY_LOG_FIRST_N: usize = 16;

fn probe_entry_attempt(entry: PresentBoundaryEntry) {
    entry.entry_counter().fetch_add(1, Ordering::Relaxed);
}

unsafe fn probe_present_entry(
    entry: PresentBoundaryEntry,
    a: &ddi::DXGI_DDI_ARG_PRESENT,
    src_alloc: ddi::D3DKMT_HANDLE,
    dst_alloc: ddi::D3DKMT_HANDLE,
) {
    let Some(n) = PRESENT_BOUNDARY_ENTRY_LOG_COUNT.first_n(PRESENT_BOUNDARY_LOG_FIRST_N) else {
        return;
    };
    let flags = *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32);
    log_error!(
        "present-boundary entry #{} {}: callback_src=0 hSrc={:p}/{} srcAlloc=0x{:x} \
         hDst={:p}/{} dstAlloc=0x{:x} flags=0x{:x} dirty=0 multiplicity=1 dxgiCtx={:p}",
        n + 1,
        entry.name(),
        a.hSurfaceToPresent as *mut c_void,
        a.SrcSubResourceIndex,
        src_alloc,
        a.hDstResource as *mut c_void,
        a.DstSubResourceIndex,
        dst_alloc,
        flags,
        a.pDXGIContext,
    );
}

unsafe fn probe_present1_multi_entry(
    a: &ddi::DXGI_DDI_ARG_PRESENT1,
    callback_source_index: usize,
    source_handle: usize,
    source_subresource: u32,
    src_alloc: ddi::D3DKMT_HANDLE,
    dst_alloc: ddi::D3DKMT_HANDLE,
) {
    let Some(n) = PRESENT_BOUNDARY_ENTRY_LOG_COUNT.first_n(PRESENT_BOUNDARY_LOG_FIRST_N) else {
        return;
    };
    let flags = *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32);
    log_error!(
        "present-boundary entry #{} Present1-multi: surfaces={} callback_src={} hSrc={:p}/{} \
         srcAlloc=0x{:x} hDst={:p}/{} dstAlloc=0x{:x} flags=0x{:x} dirty={} multiplicity={} dxgiCtx={:p}",
        n + 1,
        a.SurfacesToPresent,
        callback_source_index,
        source_handle as *mut c_void,
        source_subresource,
        src_alloc,
        a.hDstResource as *mut c_void,
        a.DstSubResourceIndex,
        dst_alloc,
        flags,
        a.DirtyRects,
        a.BackBufferMultiplicity,
        a.pDXGIContext,
    );
}

fn probe_early_refusal(entry: PresentBoundaryEntry, reason: &'static str) {
    PRESENT_BOUNDARY_EARLY_REFUSAL.fetch_add(1, Ordering::Relaxed);
    if let Some(n) = PRESENT_BOUNDARY_REFUSAL_LOG_COUNT.first_n(PRESENT_BOUNDARY_LOG_FIRST_N) {
        log_error!(
            "present-boundary refusal #{} {}: {}",
            n + 1,
            entry.name(),
            reason
        );
    }
}

unsafe fn probe_render_result(
    entry: PresentBoundaryEntry,
    hr: i32,
    allocation_count: u32,
    h_context: *mut c_void,
    command_length: u32,
) {
    PRESENT_BOUNDARY_RENDER_ATTEMPTED.fetch_add(1, Ordering::Relaxed);
    if hr >= 0 {
        PRESENT_BOUNDARY_RENDER_SUCCESS.fetch_add(1, Ordering::Relaxed);
    } else {
        PRESENT_BOUNDARY_RENDER_FAILURE.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(n) = PRESENT_BOUNDARY_CALLBACK_LOG_COUNT.first_n(PRESENT_BOUNDARY_LOG_FIRST_N) {
        log_error!(
            "present-boundary callback #{} {} pfnRenderCb: hr=0x{:08x} allocations={} hContext={:p} commandLength={}",
            n + 1,
            entry.name(),
            hr as u32,
            allocation_count,
            h_context,
            command_length,
        );
    }
}

unsafe fn probe_present_result(
    entry: PresentBoundaryEntry,
    hr: i32,
    callback_args: &ddi::DXGIDDICB_PRESENT,
    flags: u32,
) {
    PRESENT_BOUNDARY_PRESENT_ATTEMPTED.fetch_add(1, Ordering::Relaxed);
    if hr >= 0 {
        PRESENT_BOUNDARY_PRESENT_SUCCESS.fetch_add(1, Ordering::Relaxed);
    } else {
        PRESENT_BOUNDARY_PRESENT_FAILURE.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(n) = PRESENT_BOUNDARY_CALLBACK_LOG_COUNT.first_n(PRESENT_BOUNDARY_LOG_FIRST_N) {
        log_error!(
            "present-boundary callback #{} {} pfnPresentCb: hr=0x{:08x} srcAlloc=0x{:x} dstAlloc=0x{:x} \
             hContext={:p} dxgiCtx={:p} flags=0x{:x} optimize={}",
            n + 1,
            entry.name(),
            hr as u32,
            callback_args.hSrcAllocation,
            callback_args.hDstAllocation,
            callback_args.hContext,
            callback_args.pDXGIContext,
            flags,
            callback_args.bOptimizeForComposition,
        );
    }
}

unsafe fn probe_mpo_result(hr: i32, callback_args: &ddi::DXGIDDICB_PRESENT_MULTIPLANE_OVERLAY) {
    PRESENT_BOUNDARY_MPO_ATTEMPTED.fetch_add(1, Ordering::Relaxed);
    if hr >= 0 {
        PRESENT_BOUNDARY_MPO_SUCCESS.fetch_add(1, Ordering::Relaxed);
    } else {
        PRESENT_BOUNDARY_MPO_FAILURE.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(n) = PRESENT_BOUNDARY_CALLBACK_LOG_COUNT.first_n(PRESENT_BOUNDARY_LOG_FIRST_N) {
        log_error!(
            "present-boundary callback #{} MPO pfnPresentMultiplaneOverlayCb: hr=0x{:08x} allocations={} \
             hContext={:p} dxgiCtx={:p}",
            n + 1,
            hr as u32,
            callback_args.AllocationInfoCount,
            callback_args.hContext,
            callback_args.pDXGIContext,
        );
    }
}

unsafe fn probe_mpo_entry(
    a: &ddi::DXGI_DDI_ARG_PRESENTMULTIPLANEOVERLAY,
    callback_args: &ddi::DXGIDDICB_PRESENT_MULTIPLANE_OVERLAY,
) {
    let Some(n) = PRESENT_BOUNDARY_ENTRY_LOG_COUNT.first_n(PRESENT_BOUNDARY_LOG_FIRST_N) else {
        return;
    };
    log_error!(
        "present-boundary entry #{} MPO: planes={} enabled={} dxgiCtx={:p} hContext={:p}",
        n + 1,
        a.PresentPlaneCount,
        callback_args.AllocationInfoCount,
        a.pDXGIContext,
        callback_args.hContext,
    );
    let mut allocation_slot = 0usize;
    for i in 0..a.PresentPlaneCount as usize {
        let plane = &*a.pPresentPlanes.add(i);
        let allocation = if plane.Enabled == 0 {
            0
        } else {
            let allocation = callback_args.AllocationInfo[allocation_slot].PresentAllocation;
            allocation_slot += 1;
            allocation
        };
        log_error!(
            "present-boundary entry #{} MPO plane {}: enabled={} hSrc={:p}/{} srcAlloc=0x{:x} flags=0x{:x} dirty={}",
            n + 1,
            i,
            plane.Enabled,
            plane.hResource as *mut c_void,
            plane.SubResourceIndex,
            allocation,
            plane.PlaneAttributes.Flags,
            plane.PlaneAttributes.DirtyRectCount,
        );
    }
}

/// Process-global present-entry and callback totals, read at `DestroyDevice`.
pub(crate) fn present_boundary_summary() -> String {
    format!(
        "DDI PresentBoundary: entry present={} present1_single={} present1_multi={} mpo={} \
         render attempt/success/failure/missing={}/{}/{}/{} present attempt/success/failure/missing={}/{}/{}/{} \
         mpo attempt/success/failure/missing={}/{}/{}/{} early_refusal={}",
        PRESENT_BOUNDARY_PRESENT.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_PRESENT1_SINGLE.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_PRESENT1_MULTI.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_MPO.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_RENDER_ATTEMPTED.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_RENDER_SUCCESS.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_RENDER_FAILURE.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_RENDER_MISSING.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_PRESENT_ATTEMPTED.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_PRESENT_SUCCESS.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_PRESENT_FAILURE.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_PRESENT_MISSING.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_MPO_ATTEMPTED.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_MPO_SUCCESS.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_MPO_FAILURE.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_MPO_MISSING.load(Ordering::Relaxed),
        PRESENT_BOUNDARY_EARLY_REFUSAL.load(Ordering::Relaxed),
    )
}

impl RuntimeSubmission {
    /// The wire command's length and the label its log line carries.
    ///
    /// Pre-R828 the enum had two variants for three commands: `Present` with
    /// `private: Option<_>` selected the command type by an inner match, so the
    /// length and the bytes written were decided in two separate places, and
    /// BOTH present arms produced the label "Present". The labels are kept
    /// EXACTLY as they were -- TypedPresent and MarkerPresent both log
    /// "Present" -- so validation stays byte-identical.
    pub(crate) fn command_length_and_label(&self) -> (u32, &'static str) {
        match self {
            Self::TypedPresent { .. } => (
                core::mem::size_of::<HeliosPresentRenderCmd>() as u32,
                "Present",
            ),
            Self::MarkerPresent { .. } => (
                core::mem::size_of::<HeliosPresentRefreshCmd>() as u32,
                "Present",
            ),
        }
    }
}

/// The allocation-free dirty marker, built in ONE place.
///
/// Pre-R828 this literal appeared twice, byte for byte, in two arms of the same
/// match -- the `Present { private: None }` arm and the `Refresh` arm.
///
/// `source_index`/`destination_index` are RESERVED-ZERO on this path.
/// `submit_command.rs` validates only the magic and version and reads neither;
/// the KMD writes its own copy with real `DXGK_PRESENT_*_INDEX` values in
/// `display.rs`. Populating them here would be a wire-semantics change with no
/// reader.
pub(crate) fn present_refresh_cmd(
    correlation: PresentStreamCorrelation,
) -> HeliosPresentRefreshCmd {
    HeliosPresentRefreshCmd {
        magic: HELIOS_PRESENT_REFRESH_MAGIC,
        version: HELIOS_PRESENT_REFRESH_VERSION,
        source_index: 0,
        destination_index: 0,
        present_ctx_id: correlation.ctx_id,
        present_value: correlation.value32,
        present_cookie: correlation.cookie,
    }
}

/// Submit a runtime-owned WDDM command buffer.
///
/// The legacy pfnRenderCb allocation list is mandatory for a DXGI present even
/// though Helios's marker contains no guest GPU address. VidMm uses that list
/// to make the present source/destination resident and keep them live through
/// the pending operation. A standalone refresh has no pending allocation and
/// deliberately submits an empty list.
pub(crate) unsafe fn submit_runtime_submission(
    dev: &crate::device_funcs::HeliosDevice,
    submission: RuntimeSubmission,
    entry: PresentBoundaryEntry,
) -> i32 {
    static LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

    let Some(ctx) = dev.context.as_ref() else {
        probe_early_refusal(entry, "missing runtime context");
        return E_FAIL;
    };
    if dev.kt_callbacks.is_null() {
        PRESENT_BOUNDARY_RENDER_MISSING.fetch_add(1, Ordering::Relaxed);
        probe_early_refusal(entry, "kernel callback table missing");
        return E_FAIL;
    }
    let Some(render_cb) = (*dev.kt_callbacks).pfnRenderCb else {
        PRESENT_BOUNDARY_RENDER_MISSING.fetch_add(1, Ordering::Relaxed);
        probe_early_refusal(entry, "pfnRenderCb missing");
        log_error!("DXGI submission: pfnRenderCb missing");
        return E_FAIL;
    };
    let command_window = ctx.command.get();
    let command = command_window.map_or(core::ptr::null_mut(), |w| w.ptr.as_ptr());
    let (command_length, label) = submission.command_length_and_label();
    if command.is_null() || command_window.map_or(0, |w| w.capacity) < command_length {
        probe_early_refusal(entry, "runtime command buffer unavailable");
        log_error!("DXGI {label}: no runtime command buffer");
        return E_FAIL;
    }

    // Exactly one write per command type. A variant that writes the wrong
    // command is no longer representable: the length above and the bytes below
    // are both derived from the same variant.
    let allocation_count = match submission {
        RuntimeSubmission::TypedPresent {
            dependencies,
            mut private,
            correlation,
        } => {
            let count = match dependencies.write_to(ctx) {
                Ok(count) => count,
                Err(hr) => {
                    probe_early_refusal(entry, "runtime allocation list unavailable");
                    return hr;
                }
            };
            private.present_ctx_id = correlation.ctx_id;
            private.present_value = correlation.value32;
            private.present_cookie = correlation.cookie;
            (command as *mut HeliosPresentRenderCmd).write_unaligned(HeliosPresentRenderCmd {
                magic: HELIOS_PRESENT_RENDER_MAGIC,
                version: HELIOS_PRESENT_RENDER_VERSION,
                present: private,
            });
            count
        }
        RuntimeSubmission::MarkerPresent {
            dependencies,
            correlation,
        } => {
            let count = match dependencies.write_to(ctx) {
                Ok(count) => count,
                Err(hr) => {
                    probe_early_refusal(entry, "runtime allocation list unavailable");
                    return hr;
                }
            };
            (command as *mut HeliosPresentRefreshCmd)
                .write_unaligned(present_refresh_cmd(correlation));
            count
        }
    };

    let mut render = ddi::D3DDDICB_RENDER::default();
    render.CommandLength = command_length;
    render.CommandOffset = 0;
    render.NumAllocations = allocation_count;
    render.NumPatchLocations = 0;
    render.hContext = ctx.handle.as_ptr();
    let trace_start_ns = dev.dxvk.feed_trace_timestamp_ns();
    let hr = render_cb(dev.h_rt_device, &mut render);
    if trace_start_ns != 0 {
        let trace_end_ns = dev.dxvk.feed_trace_timestamp_ns();
        dev.dxvk
            .feed_trace_render_callback(trace_end_ns.saturating_sub(trace_start_ns));
    }
    probe_render_result(entry, hr, allocation_count, render.hContext, command_length);

    if hr >= 0 {
        // Each window is replaced as a unit, so a new pointer can never be
        // stored against the old capacity. The `!= 0` size guards are retained:
        // the runtime returning a pointer with a zero size means "keep what you
        // have", not "here is an empty buffer".
        if render.NewCommandBufferSize != 0 {
            if let Some(w) = crate::device_funcs::Window::new(
                render.pNewCommandBuffer,
                render.NewCommandBufferSize,
            ) {
                ctx.command.set(Some(w));
            }
        }
        if render.NewAllocationListSize != 0 {
            if let Some(w) = crate::device_funcs::Window::new(
                render.pNewAllocationList,
                render.NewAllocationListSize,
            ) {
                ctx.allocations.set(Some(w));
            }
        }
        if render.NewPatchLocationListSize != 0 {
            if let Some(w) = crate::device_funcs::Window::new(
                render.pNewPatchLocationList,
                render.NewPatchLocationListSize,
            ) {
                ctx.patches.set(Some(w));
            }
        }
    }

    let n = LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 64 || hr < 0 {
        log_error!(
            "DXGI {label}: pfnRenderCb hr=0x{:08x} allocations={} queued={} next_cmd={:p}/{}",
            hr as u32,
            allocation_count,
            render.QueuedBufferCount,
            render.pNewCommandBuffer,
            render.NewCommandBufferSize,
        );
    }
    hr
}

pub(crate) unsafe fn submit_runtime_present(
    dev: &crate::device_funcs::HeliosDevice,
    dependencies: RuntimePresentDependencies,
    private: Option<HeliosPresentPrivateData>,
    correlation: PresentStreamCorrelation,
    entry: PresentBoundaryEntry,
) -> i32 {
    submit_runtime_submission(
        dev,
        match private {
            Some(private) => RuntimeSubmission::TypedPresent {
                dependencies,
                private,
                correlation,
            },
            None => RuntimeSubmission::MarkerPresent {
                dependencies,
                correlation,
            },
        },
        entry,
    )
}

/// Submit all pending WDDM render dependencies before asking dxgkrnl to
/// present them.
///
/// The DXGI DDI requires `pfnRenderCb` to precede `pfnPresentCb`.  Keeping the
/// two callbacks in one helper makes it impossible for the ordinary Present
/// and Present1 paths to accidentally reverse that ordering again.  The typed
/// dependency value also makes a source-allocation-free present
/// unrepresentable.
pub(crate) unsafe fn submit_runtime_present_then_call(
    dev: &crate::device_funcs::HeliosDevice,
    dependencies: RuntimePresentDependencies,
    private: Option<HeliosPresentPrivateData>,
    correlation: PresentStreamCorrelation,
    callback_args: &mut ddi::DXGIDDICB_PRESENT,
    entry: PresentBoundaryEntry,
    flags: u32,
) -> i32 {
    if dev.dxgi_callbacks.is_null() {
        PRESENT_BOUNDARY_PRESENT_MISSING.fetch_add(1, Ordering::Relaxed);
        probe_early_refusal(entry, "DXGI callback table missing");
        log_error!("DXGI Present: callback table missing");
        return E_FAIL;
    }
    let Some(present_cb) = (*dev.dxgi_callbacks).pfnPresentCb else {
        PRESENT_BOUNDARY_PRESENT_MISSING.fetch_add(1, Ordering::Relaxed);
        probe_early_refusal(entry, "pfnPresentCb missing");
        log_error!("DXGI Present: pfnPresentCb missing");
        return E_FAIL;
    };

    let render_hr = submit_runtime_present(dev, dependencies, private, correlation, entry);
    if render_hr < 0 {
        return render_hr;
    }

    let trace_start_ns = dev.dxvk.feed_trace_timestamp_ns();
    let hr = present_cb(dev.h_rt_device, callback_args);
    if trace_start_ns != 0 {
        let trace_end_ns = dev.dxvk.feed_trace_timestamp_ns();
        dev.dxvk
            .feed_trace_present_callback(trace_end_ns.saturating_sub(trace_start_ns));
    }
    probe_present_result(entry, hr, callback_args, flags);
    hr
}

/// Which entry point a [`finish_present`] call is serving.
///
/// R1013. `dxgi_present` and `dxgi_present1`'s multi-surface arm duplicated
/// ~70 lines -- the `DXGIDDICB_PRESENT` construction, the private-data attach,
/// `RuntimePresentDependencies::new`, the PresentCb identity trace and
/// `submit_runtime_present_then_call` -- and the copies had DRIFTED. Every
/// fix landed in `dxgi_present` only, silently not applying to any swapchain
/// presenting through Present1's multi-surface form.
///
/// This impl is the divergence table: each surviving difference is one method
/// with both arms visible, instead of code that is present on one path and
/// absent on the other. **Every arm below reproduces today's behaviour
/// exactly**; narrowing any of them is a separate change with its own
/// evidence.
///
/// Four of the six differences the review lists are already gone, deleted by
/// T6 rather than reconciled here: the vehicle TLS slot and `PRESENT_RESULT`
/// (R912), the discarded frame-gate result -- `EXT_FLIP_GATE_TIMEOUTS` is now
/// bumped in one shared place, `run_present_frame_gate` -- the
/// `copy_to_scanout_target` asymmetry (R910), and `syncVal` (R912).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresentKind {
    Present,
    Present1Multi,
}

impl PresentKind {
    /// Prefix on the PresentCb identity trace. The two spellings are load
    /// bearing: they are how a log reader tells which entry point ran.
    pub(crate) fn identity_prefix(self) -> &'static str {
        match self {
            PresentKind::Present => "DXGI ",
            PresentKind::Present1Multi => "DXGI Present1 ",
        }
    }

    /// Prefix on this tail's error lines.
    pub(crate) fn error_tag(self) -> &'static str {
        match self {
            PresentKind::Present => "DXGI Present",
            PresentKind::Present1Multi => "DXGI Present1 multi",
        }
    }

    /// What the tail returns when it never reaches the present callback --
    /// no device, or a refused prerequisite on the fall-through path.
    ///
    /// DIVERGENT AND PRESERVED: Present1-multi initialises to `E_INVALIDARG`,
    /// so a device-less path there returns a FAILURE where Present returns
    /// success.
    pub(crate) fn initial_hr(self) -> i32 {
        match self {
            PresentKind::Present => 0,
            PresentKind::Present1Multi => E_INVALIDARG,
        }
    }

    /// What a failed `present_prerequisites` check does.
    ///
    /// DIVERGENT AND PRESERVED: Present logs (rate-capped) and FALLS THROUGH
    /// to the rest of its body with `initial_hr`; Present1-multi RETURNS
    /// `DXGI_ERROR_UNSUPPORTED` immediately, skipping its trailing log. The
    /// two also log different field sets, which is why the message is emitted
    /// per kind rather than unified.
    pub(crate) fn missing_prereq_hr(self) -> Option<i32> {
        match self {
            PresentKind::Present => None,
            PresentKind::Present1Multi => Some(DXGI_ERROR_UNSUPPORTED),
        }
    }
}

/// The per-call values the shared present tail needs that are not derivable
/// from [`PresentKind`]. Both entry points read them from their own (different)
/// DDI argument struct.
pub(crate) struct PresentRequest {
    pub(crate) kind: PresentKind,
    pub(crate) boundary: PresentBoundaryEntry,
    /// `DXGI_DDI_ARG_PRESENT{,1}::pDXGIContext`, passed straight through.
    pub(crate) dxgi_context: *mut c_void,
    /// The raw `Flags` word. TRACE ONLY -- nothing branches on it here.
    pub(crate) flags: u32,
}

/// The shared present tail: build `DXGIDDICB_PRESENT`, attach the direct-primary
/// private data, resolve the runtime dependencies, trace the callback identity,
/// and submit.
///
/// Everything from the prerequisite check through
/// `submit_runtime_present_then_call`, written once. The scanout-publish
/// decision, the src->dst copy and the frame gate stay at the call sites: those
/// genuinely differ, and Present1-multi performs none of them.
///
/// `Ok(hr)` means "carry on with the rest of your body"; `Err(hr)` means
/// "return this from the DDI entry point NOW, running nothing else". That
/// distinction is load-bearing rather than stylistic: both entry points used
/// a bare `return` for the lost-invariant and (for Present1) missing-callback
/// cases, which skipped the vehicle mint, `EXT_PRESENTS` and the trailing
/// per-present log. Collapsing those into a plain HRESULT would have started
/// minting vehicle slots on a path that never did.
///
/// `snapshot` is the D4b substitution plan and is value-threaded on purpose:
/// only `dxgi_present`'s direct-flip arm, which just RECORDED the snapshot
/// blit this present, can produce a `Some` — there is no device-latched copy
/// that could go stale if the blit arm was skipped, so a descriptor pointing
/// at an unwritten snapshot (a stale-frame display) is unrepresentable.
/// Present1-multi performs no publish and always passes `None`.
pub(crate) unsafe fn finish_present(
    h: Hdevice,
    src_h: ddi::D3D10DDI_HRESOURCE,
    dst_h: ddi::D3D10DDI_HRESOURCE,
    src_alloc: u32,
    dst_alloc: u32,
    req: PresentRequest,
    snapshot: Option<SnapshotPlan>,
    correlation: PresentStreamCorrelation,
) -> Result<i32, i32> {
    let no_callback_hr = req.kind.initial_hr();
    let Some(dev) = helios_device(h) else {
        probe_early_refusal(req.boundary, "device handle not live");
        return Ok(no_callback_hr);
    };

    let ready = match present_prerequisites(dev, src_alloc) {
        Ok(ready) => ready,
        Err(_skip) => {
            if dev.dxgi_callbacks.is_null() {
                PRESENT_BOUNDARY_PRESENT_MISSING.fetch_add(1, Ordering::Relaxed);
            }
            probe_early_refusal(req.boundary, "present prerequisites refused");
            // Which of the three preconditions failed lives in
            // PRESENT_SKIP_NO_CALLBACKS / _NO_CONTEXT / _NO_SRC_ALLOC.
            match req.kind {
                PresentKind::Present => {
                    // Rate cap: same message text and field set, fewer lines.
                    if PRESENT_SKIP_LOG_COUNT
                        .first_n_then_every_from_one(64, 512)
                        .is_some()
                    {
                        log_error!(
                            "DXGI Present: skip PresentCb callbacks={} src=0x{:x} hContext={:p}",
                            dev.dxgi_callbacks.is_null(),
                            src_alloc,
                            dev.context
                                .as_ref()
                                .map_or(core::ptr::null_mut(), |c| c.handle.as_ptr())
                        );
                    }
                }
                PresentKind::Present1Multi => {
                    log_error!(
                        "DXGI Present1 multi: missing callback table/context callbacks={} hContext={:p}",
                        dev.dxgi_callbacks.is_null(),
                        dev.context
                            .as_ref()
                            .map_or(core::ptr::null_mut(), |c| c.handle.as_ptr())
                    );
                }
            }
            return match req.kind.missing_prereq_hr() {
                Some(hr) => Err(hr),
                None => Ok(no_callback_hr),
            };
        }
    };

    let mut cb = ddi::DXGIDDICB_PRESENT::default();
    let mut present_private = presented_primary_private(h, src_h);
    // D4b: substitute the snapshot descriptor into the OWNED LOCAL above —
    // before BOTH consumers (`pPrivateDriverData` below and the
    // `HeliosPresentRenderCmd` inside `submit_runtime_present_then_call`).
    // The override re-validates the private's geometry against the plan and
    // refuses loudly on any divergence; `ResourceState::present_private` and
    // `direct_scanout_allocations` (rotation-coupled) are never touched.
    if let Some(ref plan) = snapshot {
        apply_snapshot_override(&mut present_private, plan);
    }
    // `ready` carries the same two values both entry points used to spell out
    // by hand -- `src_alloc` proved non-zero, and the context handle proved
    // present -- so this is the checked form of what Present1-multi was
    // re-deriving from `dev.context` after the check had already run.
    cb.hSrcAllocation = ready.src_alloc.get();
    cb.hDstAllocation = dst_alloc;
    cb.pDXGIContext = req.dxgi_context;
    cb.hContext = ready.h_context.as_ptr();
    cb.BroadcastContextCount = 0;
    // RETIRED (0ab-C close-out): this used to also ship `present_private` via
    // `cb.pPrivateDriverData`, but dxgkrnl never forwarded it to
    // `DxgkDdiPresent` on the DMA-flip path (the KMD's `PBIdOk` read
    // "no payload" across three driver generations). The ONE per-present
    // channel to the KMD's flip arm is the inline `HeliosPresentRenderCmd`
    // inside `submit_runtime_present_then_call` below — which is where the
    // (possibly snapshot-substituted) `present_private` still goes.
    cb.PrivateDriverDataSize = 0;
    cb.pPrivateDriverData = core::ptr::null_mut();
    cb.bOptimizeForComposition = if present_optimize_composition_enabled() {
        1
    } else {
        0
    };
    let Some(dependencies) = RuntimePresentDependencies::new(src_alloc, dst_alloc) else {
        probe_early_refusal(req.boundary, "source allocation identity lost");
        log_error!(
            "{}: nonzero source allocation invariant lost",
            req.kind.error_tag()
        );
        return Err(E_FAIL);
    };
    if let Some(cb_n) = PRESENT_CB_LOG_COUNT.first_n_then_every_from_one(128, 512) {
        let (src_rt, src_km) = resource_parent_handles(src_h);
        let (dst_rt, dst_km) = resource_parent_handles(dst_h);
        trace_line!(
            "{}PresentCb identity: #{} src_alloc=0x{:x} dst_alloc=0x{:x} \
             src_hDrv={:p} src_hRT={:p} src_hKM=0x{:x} dst_hDrv={:p} \
             dst_hRT={:p} dst_hKM=0x{:x} hContext={:p} dxgi_context={:p} \
             flags=0x{:x} broadcast={} private={:p}/{} optimize={}",
            req.kind.identity_prefix(),
            cb_n,
            cb.hSrcAllocation,
            cb.hDstAllocation,
            src_h.pDrvPrivate,
            src_rt,
            src_km,
            dst_h.pDrvPrivate,
            dst_rt,
            dst_km,
            cb.hContext,
            cb.pDXGIContext,
            req.flags,
            cb.BroadcastContextCount,
            cb.pPrivateDriverData,
            cb.PrivateDriverDataSize,
            cb.bOptimizeForComposition,
        );
    }
    Ok(submit_runtime_present_then_call(
        dev,
        dependencies,
        present_private,
        correlation,
        &mut cb,
        req.boundary,
        req.flags,
    ))
}

/// DXGI `pfnPresent`: copy the source resource to the destination resource when
/// DXGI provides both handles, then flush submitted GPU work.
pub(crate) unsafe extern "C" fn dxgi_present(arg: *mut ddi::DXGI_DDI_ARG_PRESENT) -> i32 {
    probe_entry_attempt(PresentBoundaryEntry::Present);
    dxgi_present_impl(arg, PresentBoundaryEntry::Present)
}

/// The common implementation for an ordinary Present and a one-surface
/// Present1 delegation. The caller chooses only the observation tag; rendering,
/// snapshot and callback behavior remains the ordinary path.
unsafe fn dxgi_present_impl(
    arg: *mut ddi::DXGI_DDI_ARG_PRESENT,
    boundary: PresentBoundaryEntry,
) -> i32 {
    if arg.is_null() {
        probe_early_refusal(boundary, "null Present argument");
        return 0;
    }
    let a = &*arg;
    // `DXGI_DDI_PRESENT_FLAGS` is a bindgen bitfield wrapper. Read its
    // documented Blt bit through the raw initialized representation rather
    // than depending on an operator implementation for that wrapper.
    let present_flags = *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32);
    // DXGI_DDI_HDEVICE is a UINT_PTR carrying the driver device handle, the same
    // private pointer stored in D3D10DDI_HDEVICE.pDrvPrivate.
    let h = dxgi_device_handle(a.hDevice);
    let context = d3d11_context(h);
    let src_h = dxgi_resource_handle(a.hSurfaceToPresent);
    let dst_h = dxgi_resource_handle(a.hDstResource);
    let src_alloc = resource_allocation(src_h);
    let dst_alloc = resource_allocation(dst_h);
    probe_present_entry(boundary, a, src_alloc, dst_alloc);
    let mut copied = false;
    // D4b snapshot plan: `Some` iff the direct-flip arm below RECORDED the
    // ordered blit this present. Value-threaded into `finish_present`, never
    // latched on the device — see `snapshot_for_present`.
    let mut snapshot: Option<SnapshotPlan> = None;

    // Dcomp present vehicle (road 4): a pending TLS source means THIS
    // present is the vehicle carrying an ICD frame — replace the normal
    // src->dst copy, publish and gate with the vehicle body; a vehicle
    // failure FAILS the present (no token minted) so the ICD latches its sw
    // fallback instead of flipping a stale backbuffer.
    let ext_source = VEHICLE.with(|c| match c.get() {
        VehicleSlot::Armed(source) => {
            // Consuming the arm returns the slot to Idle until this present
            // either mints or fails. A non-vehicle present must NOT touch a
            // pending Minted result, so only this arm writes.
            c.set(VehicleSlot::Idle);
            Some(source)
        }
        _ => None,
    });
    let is_vehicle_present = ext_source.is_some();
    // This only selects the ordinary fast path. The diagnostics below change
    // command-stream ordering themselves, and a vehicle publishes its frame
    // from the ICD side, so all three retain the historical post-flush
    // producer publication path exactly.
    let batch_fold_present_order = !is_vehicle_present
        && crate::knobs::present_batch_fold()
        && !present_force_opaque_enabled()
        && !present_readback_enabled();
    let mut present_order_folded = false;
    // Only the early, folded producer publication can name the exact queue
    // submission that this existing Flush dispatches.  Every post-Flush,
    // debug, vehicle and Present1 path deliberately carries this all-zero.
    let mut present_stream_correlation = PresentStreamCorrelation::default();

    if let Some(context) = &context {
        if let Some(ref src_info) = ext_source {
            match vehicle_present_prepare(h, src_h, src_info) {
                Ok(()) => {
                    copied = true;
                }
                Err(hr) => {
                    VEHICLE.with(|c| c.set(VehicleSlot::Idle));
                    return hr;
                }
            }
            context.Flush();
        } else {
            // A direct primary already is the scanout backing. Do not copy it
            // through the adapter-owned LINEAR target; Present will publish its
            // rotated resource id after flushing DWM's rendering.
            //
            // The private data is read PER PRESENT: it rotates with the
            // allocation (`presented_primary_private` resolves by the
            // allocation Windows is actually presenting), so caching it per
            // resource would hand the snapshot arm a stale geometry/identity.
            let direct_private = presented_primary_private(h, src_h);
            let published_to_scanout = direct_private.is_some();
            let copy_pair = if published_to_scanout {
                None
            } else {
                match (load_resource(dst_h), load_resource(src_h)) {
                    (Some(dst), Some(src)) => Some((dst, src)),
                    _ => None,
                }
            };
            if let Some((dst, src)) = copy_pair {
                context.CopySubresourceRegion(&*dst, 0, 0, 0, 0, &*src, 0, None);
                copied = true;
            }
            // D4b, direct-flip arm ONLY: record the ordered snapshot blit
            // S_i <- presented primary BEFORE the Flush below, so the blit is
            // inside frame N's command list — the same list the frame gate's
            // `HeliosWaitFrameSubmitted` later covers, which is what keeps
            // the KMD's completion watermark valid for the substituted resid.
            // Every refusal inside returns None (present exactly as today).
            if let Some(ref private) = direct_private {
                snapshot = snapshot_for_present(
                    h,
                    src_h,
                    private.width,
                    private.height,
                    private.dxgi_format,
                    SnapshotPurpose::DirectFlip,
                );
            } else if present_flags & 1 != 0 {
                if let Some(source) = resource_snapshot_source(src_h) {
                    // Windowed BLT: DXGI's exact source is copied into a fresh
                    // ICD-owned snapshot BEFORE Flush, then the typed Render
                    // record makes that immutable snapshot the KMD BLT source.
                    // No direct-primary private selector participates here.
                    snapshot = snapshot_for_present(
                        h,
                        src_h,
                        source.width,
                        source.height,
                        source.dxgi_format,
                        SnapshotPurpose::WindowedBlt,
                    );
                }
            }

            // Fold the ordinary producer signal into this present's actual
            // frame batch. The copy and direct-primary snapshot above have
            // both been recorded, so the signal reaches the timeline after
            // every producer write that the consumer must observe. Recording
            // it before this existing Flush avoids creating a separate tiny
            // batch solely to publish the present fence.
            if batch_fold_present_order {
                if let Some(dev) = helios_device(h) {
                    let consumed = if copied {
                        resource_com_raw(dst_h)
                    } else {
                        resource_com_raw(src_h)
                    };
                    if consumed != 0 {
                        // Only suppress the historical post-Flush path when
                        // publication actually succeeded. A failed early
                        // attempt must retain that path as its recovery try;
                        // otherwise a transient slot failure would leave the
                        // consumer unordered without even the old fallback.
                        let (published, correlation) = dev.dxvk.publish_present_order(consumed);
                        present_order_folded = published;
                        if published {
                            present_stream_correlation = correlation;
                        }
                    }
                }
            }
            context.Flush();
        }
    } else if is_vehicle_present {
        // No immediate context = nothing was copied or published.
        VEHICLE.with(|c| c.set(VehicleSlot::Idle));
        EXT_NO_DEVICE.fetch_add(1, Ordering::Relaxed);
        return E_FAIL;
    }

    maybe_force_present_alpha_opaque(h, src_h);
    maybe_log_present_readback(h, src_h);

    // Cross-process present ordering, PRODUCER half. Record a signal on this
    // device's named present timeline for the frame just flushed and publish
    // (resid -> pid, fenceId, value). A consumer compositing this surface --
    // dwm, sampling it as an SRV -- turns that into a GPU-side wait on its own
    // submission, so the ordering costs neither side a blocked CPU thread.
    //
    // This is what makes the submission-ordered present CORRECT rather than
    // merely fast — it is the GPU-timeline half of the ordering that the
    // deleted `PresentOrder`/`PresentGateUs` CPU gate was standing in for. It
    // must run before the gate: the gate's flush is what pushes the signal to
    // the wire, and the publish has to name a value this device has already
    // committed to reaching.
    //
    // Deliberately NOT applied to a vehicle present: that path's source is the
    // ICD's frame, which publishes its own slot from the ICD side.
    //
    // WHICH resource: the one the CONSUMER imports, which is not always the
    // source. On the flip path (`dst_h` empty) dwm reads the presented
    // backbuffer itself. On the windowed/BLT path we have just copied
    // src -> dst, and `dst` is win32k's redirection surface -- the one dwm
    // composes -- so publishing `src` there would key the slot on a resource no
    // consumer ever looks up, and the wait would silently not happen.
    if !is_vehicle_present && !present_order_folded {
        if let Some(dev) = helios_device(h) {
            let consumed = if copied {
                resource_com_raw(dst_h)
            } else {
                resource_com_raw(src_h)
            };
            if consumed != 0 {
                let _ = dev.dxvk.publish_present_order(consumed);
            }
        }
    }

    // Ordering gate BEFORE the kernel present becomes visible. The
    // direct-primary KMD marker can order Venus commands which have reached the
    // transport, but `context.Flush()` may return while matching work is still
    // queued on DXVK's CS and submission threads. Closing that future-work gap
    // before dxgkrnl publishes the primary is the whole job.
    //
    // The ordinary path waits for the frame's `vkQueueSubmit` — exactly what
    // the KMD watermark needs, and it costs no GPU time. It is UNCONDITIONAL:
    // there is no knob to select a completion wait and none to bound or disable
    // it, because a producer-side CPU stall is a hack that hides ordering
    // defects rather than fixing them, and owner testing on 2026-07-29 showed
    // it does not even hide this one (an unexpirable 200 ms completion gate
    // still flashed black). See the ⛔ note in `crate::knobs`.
    //
    // The VEHICLE path is the exception and keeps its bounded COMPLETE wait
    // under its own `VehicleFlipGateUs`, whose 0 still disables it: that
    // backbuffer scans out on a direct/independent flip ordered only on a
    // DECODE-domain DMA fence, so host-GPU completion genuinely is the
    // requirement there. Timeout = proceed loudly (a stale frame beats a
    // wedged worker). Cost telemetry: `present-gate:` lines.
    //
    // Until R912(a) a kernel-enforced alternative sat above this: a dxgkrnl
    // GPU-side WAIT queued on a flip fence ahead of the present packet, which
    // set `kernel_wait_armed` and skipped the gate. It required a non-zero
    // `sync_value`, whose only producer was `present_sync_publish` behind a
    // knob that defaulted OFF -- so it never armed. Measured before deleting:
    // `kwait_armed = 0` over 1536 vkcube vehicle presents (ROADMAP 7g(d)).
    // `gate_us` is meaningful only for the vehicle's COMPLETE wait; the
    // SUBMITTED arm ignores it (see `HeliosDxvkDevice::present_frame_gate`).
    // The `!= 0` disable therefore applies to the vehicle alone — applying it
    // to the ordinary path would be a knob that skips the ordering entirely.
    let gate_us = if is_vehicle_present {
        crate::vehicle_flip_gate_us()
    } else {
        0
    };
    // The registration tag makes the KMD's existing Render marker wait for the
    // exact fold batch.  It is safe to omit the submitted gate only for that
    // one ordinary form: early publication succeeded, its complete correlation
    // is nonzero, the probe latched KMD support, and the explicit A/B switch is
    // still on.  `context.Flush()` above remains the dispatch guarantee.
    let async_stream_eligible = !is_vehicle_present
        && present_order_folded
        && crate::umd_async_present_stream()
        && crate::scanout_acquire::async_present_stream_capable()
        && present_stream_correlation.is_complete();
    // The appended marker tail changes the KMD from its legacy current-wire
    // watermark to the registered stream boundary. It must therefore be zero
    // for every knob/capability/correlation fallback, even though this local
    // correlation remains useful for trace evidence below.
    let marker_correlation = if async_stream_eligible {
        present_stream_correlation
    } else {
        PresentStreamCorrelation::default()
    };
    // A WindowedBlt snapshot is safe only when this exact producer stream
    // marker is carried with it. The snapshot copy may already be in the
    // batch, but without a registered marker it is intentionally unused: the
    // legacy KMD BLT path remains the truthful fallback rather than inventing
    // a current-wire boundary.
    if snapshot
        .as_ref()
        .is_some_and(|plan| plan.purpose == SnapshotPurpose::WindowedBlt)
        && !async_stream_eligible
    {
        snapshot = None;
    }
    if async_stream_eligible {
        // Timed-path evidence is trace-gated: no logging or atomic RMW when
        // UmdTrace is off. Registration/unavailable totals are one-time C++
        // counters beside the only REGISTER escape.
        trace_line!(
            "present-stream: async gate skip ctx={} value={} cookie={}",
            present_stream_correlation.ctx_id,
            present_stream_correlation.value32,
            present_stream_correlation.cookie,
        );
    } else if !is_vehicle_present && present_order_folded {
        trace_line!(
            "present-stream: fallback gate folded={} corr={} cap={} knob={}",
            present_order_folded as u32,
            present_stream_correlation.is_complete() as u32,
            crate::scanout_acquire::async_present_stream_capable() as u32,
            crate::umd_async_present_stream() as u32,
        );
    }
    if !async_stream_eligible && (!is_vehicle_present || gate_us != 0) {
        if let Some(dev) = helios_device(h) {
            let _outcome = run_present_frame_gate(dev, gate_us, is_vehicle_present);
        }
    }

    let present_hr = match finish_present(
        h,
        src_h,
        dst_h,
        src_alloc,
        dst_alloc,
        PresentRequest {
            kind: PresentKind::Present,
            boundary,
            dxgi_context: a.pDXGIContext,
            flags: *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32),
        },
        snapshot,
        marker_correlation,
    ) {
        Ok(hr) => hr,
        // Abandons the vehicle mint, EXT_PRESENTS and the ordinal log below,
        // exactly as the bare `return E_FAIL` did.
        Err(hr) => return hr,
    };

    if is_vehicle_present {
        // `wait_last_present` targets the device recorded here. The
        // `result: Option<(fenceId, value)>` this slot used to carry went with
        // R912(a) -- it could only ever be None.
        VEHICLE.with(|c| {
            c.set(VehicleSlot::Minted {
                device: h.pDrvPrivate as usize,
            })
        });
        let n = EXT_PRESENTS.fetch_add(1, Ordering::Relaxed);
        if n < 4 || (n + 1) % 512 == 0 {
            log_error!(
                "vehicle present #{}: imports_failed={} copies_failed={} geom_mismatch={} \
                 overwrites={}",
                n + 1,
                EXT_IMPORT_FAILS.load(Ordering::Relaxed),
                EXT_COPY_FAILS.load(Ordering::Relaxed),
                EXT_GEOM_MISMATCH.load(Ordering::Relaxed),
                EXT_OVERWRITES.load(Ordering::Relaxed),
            );
        }
    }

    // Forensics for the DWM indirect-swapchain flip-present failure (3 OK then
    // 0x80070057): log the rotating runtime resource handle vs our collapsed
    // allocation handle, subresource indices, raw flags and flip interval, and
    // a per-process present ordinal so cycles can be told apart.
    static PRESENT_ORDINAL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let ordinal = PRESENT_ORDINAL.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if ordinal < 64 || (ordinal + 1) % 512 == 0 {
        log_error!(
            "DXGI Present: #{} src=0x{:x} dst=0x{:x} copied={} flags=0x{:x} opt_comp={} presentCb=0x{:08x} \
             hSurf={:p} srcSub={} hDstRes={:p} dstSub={} flipInterval={} dxgiCtx={:p} hContext={:p} \
             skips={}/{}/{} gate_nc={}",
            ordinal,
            src_alloc,
            dst_alloc,
            copied,
            *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32),
            present_optimize_composition_enabled() as u32,
            present_hr as u32,
            src_h.pDrvPrivate,
            a.SrcSubResourceIndex,
            dst_h.pDrvPrivate,
            a.DstSubResourceIndex,
            a.FlipInterval,
            a.pDXGIContext,
            dev_context_for_log(h),
            PRESENT_SKIP_NO_CALLBACKS.load(Ordering::Relaxed),
            PRESENT_SKIP_NO_CONTEXT.load(Ordering::Relaxed),
            PRESENT_SKIP_NO_SRC_ALLOC.load(Ordering::Relaxed),
            PRESENT_GATE_TIMEOUTS.load(Ordering::Relaxed),
        );
    }
    present_hr
}

// Best-effort context handle for present logging (null when unavailable).
pub(crate) fn dev_context_for_log(h: ddi::D3D10DDI_HDEVICE) -> *mut core::ffi::c_void {
    unsafe {
        helios_device(h).map_or(core::ptr::null_mut(), |d| {
            d.context
                .as_ref()
                .map_or(core::ptr::null_mut(), |c| c.handle.as_ptr())
        })
    }
}

pub(crate) unsafe extern "C" fn dxgi_get_gamma_caps(
    arg: *mut ddi::DXGI_DDI_ARG_GET_GAMMA_CONTROL_CAPS,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let caps = (*arg).pGammaCapabilities;
    if !caps.is_null() {
        core::ptr::write_bytes(
            caps as *mut u8,
            0,
            core::mem::size_of::<ddi::DXGI_GAMMA_CONTROL_CAPABILITIES>(),
        );
        (*caps).MaxConvertedValue = 1.0;
        (*caps).MinConvertedValue = 0.0;
    }
    0
}

pub(crate) unsafe extern "C" fn dxgi_set_display_mode(
    arg: *mut ddi::DXGI_DDI_ARG_SETDISPLAYMODE,
) -> i32 {
    if arg.is_null() {
        return E_INVALIDARG;
    }
    let a = &*arg;
    let h = dxgi_device_handle(a.hDevice);
    let Some(dev) = helios_device(h) else {
        log_error!("DXGI SetDisplayMode: missing device");
        return E_INVALIDARG;
    };
    if dev.kt_callbacks.is_null() {
        log_error!("DXGI SetDisplayMode: missing runtime callbacks");
        return E_FAIL;
    }
    let Some(set_display_mode_cb) = (*dev.kt_callbacks).pfnSetDisplayModeCb else {
        log_error!("DXGI SetDisplayMode: pfnSetDisplayModeCb missing");
        return E_FAIL;
    };

    // Windows supplies the authoritative primary resource and subresource for
    // the fullscreen transition. Translate that exact runtime resource to the
    // allocation created for it; pfnSetDisplayModeCb then asks dxgkrnl to make
    // that allocation the scan-out primary and initiates the VidPn commit.
    let resource = dxgi_resource_handle(a.hResource);
    let allocation = resource_allocation(resource);
    if allocation == 0 {
        log_error!(
            "DXGI SetDisplayMode: resource=0x{:x} sub={} has no WDDM allocation",
            a.hResource,
            a.SubResourceIndex
        );
        return E_INVALIDARG;
    }

    if let Some(context) = d3d11_context(h) {
        context.Flush();
    }
    let mut callback = ddi::D3DDDICB_SETDISPLAYMODE {
        hPrimaryAllocation: allocation,
        PrivateDriverFormatAttribute: 0,
    };
    let hr = set_display_mode_cb(dev.h_rt_device, &mut callback);
    log_error!(
        "DXGI SetDisplayMode: resource=0x{:x} sub={} allocation=0x{:x} hr=0x{:08x} private_format=0x{:x}",
        a.hResource,
        a.SubResourceIndex,
        allocation,
        hr as u32,
        callback.PrivateDriverFormatAttribute
    );
    hr
}

pub(crate) unsafe extern "C" fn dxgi_set_resource_priority(
    _arg: *mut ddi::DXGI_DDI_ARG_SETRESOURCEPRIORITY,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn dxgi_query_resource_residency(
    arg: *mut ddi::DXGI_DDI_ARG_QUERYRESOURCERESIDENCY,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    if !a.pStatus.is_null() {
        for i in 0..a.Resources as usize {
            *a.pStatus.add(i) = ddi::DXGI_DDI_RESIDENCY_DXGI_DDI_RESIDENCY_FULLY_RESIDENT;
        }
    }
    0
}

/// DXGI flip-model identity rotation. The runtime calls this after each flip
/// present so the app's fixed buffer objects walk the swapchain's allocation
/// ring: resource[i] takes resource[i+1]'s identity, the last takes the
/// first's. Two coordinated moves keep the world consistent:
///   1. the DXVK storages (venus memory + VkImage + KMT handles) rotate in
///      the bridge, so draws through existing views land in the allocation
///      the runtime now associates with the buffer;
///   2. our per-resource WDDM {allocation, km} records rotate here, so the
///      next present reports the rotated hSrcAllocation to dxgkrnl.
/// The old Flush-only stub pinned dwm's composition to ONE allocation while
/// dxgkrnl/IddCx walked all three swapchain buffers — two of every three
/// acquired frames were buffers dwm never rendered (black IDD output).
/// Outcome of one swapchain identity rotation. Five exits used to `return 0`,
/// which the DXGI DDI reads as success; this names them instead.
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) enum RotationOutcome {
    Rotated,
    /// `rotate_resource_backings` returned false — an entry with no DXVK image
    /// storage, or a `DxvkError`/unknown exception swallowed into false.
    BridgeRefused,
    /// No Helios device behind the DXGI device handle.
    NoDevice,
}

/// The DXVK backing rotation refused it.
pub(crate) static ROTATE_REFUSED: AtomicUsize = AtomicUsize::new(0);
/// A null resource handle or an untracked resource in the ring.
pub(crate) static ROTATE_UNTRACKED: AtomicUsize = AtomicUsize::new(0);
/// No Helios device behind the DXGI device handle.
pub(crate) static ROTATE_NO_DEVICE: AtomicUsize = AtomicUsize::new(0);
/// `Resources < 2` or a null array — the exit that had no log at all.
pub(crate) static ROTATE_SKIPPED: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn rotate_counter_summary() -> String {
    format!(
        "refused={} untracked={} no_device={} skipped={}",
        ROTATE_REFUSED.load(Ordering::Relaxed),
        ROTATE_UNTRACKED.load(Ordering::Relaxed),
        ROTATE_NO_DEVICE.load(Ordering::Relaxed),
        ROTATE_SKIPPED.load(Ordering::Relaxed),
    )
}

/// Both rotation phases, with NO return path between them.
///
/// The bridge rotation and the WDDM record rotation used to be two statements
/// held in the right order purely by statement order. If the bridge refuses
/// after the records moved — or vice versa — dwm composites into an allocation
/// dxgkrnl no longer scans out, which is the historical black-IDD bug this
/// DDI's own doc comment describes.
///
/// `states` is a slice of INDEPENDENT raw pointers, so a `&mut [ResourceState]`
/// cannot be formed from it and this stays `unsafe`. Panic-free: no indexing,
/// no `unwrap` — `first`/`last`/`windows` only.
pub(crate) unsafe fn rotate_ring(
    dev: &crate::device_funcs::HeliosDevice,
    states: &[*mut ResourceState],
) -> RotationOutcome {
    let (Some(&first), Some(&last)) = (states.first(), states.last()) else {
        // Unreachable: the caller validated len >= 2.
        ROTATE_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return RotationOutcome::BridgeRefused;
    };

    let ptrs: Vec<usize> = states.iter().map(|s| (**s).com_raw).collect();
    if !dev.dxvk.rotate_resource_backings(ptrs.as_ptr(), ptrs.len()) {
        ROTATE_REFUSED.fetch_add(1, Ordering::Relaxed);
        return RotationOutcome::BridgeRefused;
    }

    // Rotate the WDDM identity records in lockstep with the storages.
    let first_allocation = (*first).allocation.take();
    let first_km_resource = (*first).km_resource;
    // `ownership` rotates with the allocation it describes; `rt_resource`
    // deliberately does NOT rotate and is not touched here. That asymmetry is
    // why R804 keeps the discriminant a separate field rather than bundling the
    // runtime handle into the ownership enum -- a variant carrying the handle
    // would change what RotateResourceIdentities moves.
    let first_ownership = (*first).ownership;
    let first_present_private = (*first).present_private;
    let first_snapshot_source = (*first).snapshot_source;
    for pair in states.windows(2) {
        let (Some(&cur), Some(&next)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        (*cur).allocation = (*next).allocation.take();
        (*cur).km_resource = (*next).km_resource;
        (*cur).ownership = (*next).ownership;
        // Present private data identifies the backing allocation (Venus
        // resource id, layout and extent), not the stable D3D resource object.
        // DXGI rotates that backing identity together with the allocation and
        // DXVK storage. Leaving this behind makes a flip scan out the previous
        // resource's memory after the first RotateResourceIdentities call.
        (*cur).present_private = (*next).present_private;
        (*cur).snapshot_source = (*next).snapshot_source;
    }
    (*last).allocation = first_allocation;
    (*last).km_resource = first_km_resource;
    (*last).ownership = first_ownership;
    (*last).present_private = first_present_private;
    (*last).snapshot_source = first_snapshot_source;

    RotationOutcome::Rotated
}

pub(crate) unsafe extern "C" fn dxgi_rotate_resource_identities(
    arg: *mut ddi::DXGI_DDI_ARG_ROTATE_RESOURCE_IDENTITIES,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    let h = dxgi_device_handle(a.hDevice);
    let n = a.Resources as usize;
    if n < 2 || a.pResources.is_null() {
        let c = ROTATE_SKIPPED.fetch_add(1, Ordering::Relaxed);
        if c < 16 || c % 512 == 0 {
            log_error!(
                "DXGI RotateResourceIdentities: skipped resources={} null_array={} ({})",
                n,
                a.pResources.is_null(),
                rotate_counter_summary()
            );
        }
        return 0;
    }

    // Collect the per-resource state pointers; all entries must be tracked
    // resources or the rotation is refused whole (a partial rotation would
    // permanently corrupt the swapchain mapping).
    let mut states: Vec<*mut ResourceState> = Vec::with_capacity(n);
    for i in 0..n {
        let hr = dxgi_resource_handle(*a.pResources.add(i));
        if hr.pDrvPrivate.is_null() {
            ROTATE_UNTRACKED.fetch_add(1, Ordering::Relaxed);
            log_error!(
                "DXGI RotateResourceIdentities: null resource handle ({})",
                rotate_counter_summary()
            );
            return 0;
        }
        let state = match boxed_slot(hr) {
            Some(slot) => slot.ptr(),
            None => core::ptr::null_mut(),
        };
        if state.is_null() {
            ROTATE_UNTRACKED.fetch_add(1, Ordering::Relaxed);
            log_error!(
                "DXGI RotateResourceIdentities: untracked resource ({})",
                rotate_counter_summary()
            );
            return 0;
        }
        states.push(state);
    }

    let outcome = match helios_device(h) {
        Some(dev) => rotate_ring(dev, &states),
        None => {
            ROTATE_NO_DEVICE.fetch_add(1, Ordering::Relaxed);
            RotationOutcome::NoDevice
        }
    };
    if outcome != RotationOutcome::Rotated {
        log_error!(
            "DXGI RotateResourceIdentities: backing rotation FAILED ({})",
            rotate_counter_summary()
        );
        return 0;
    }

    if ROTATE_LOG_COUNT.first_n(64).is_some() {
        let (first_handle, first_resource_id) = match states.first() {
            Some(&first) => (
                (*first)
                    .allocation
                    .as_ref()
                    .map(ResidentAllocation::handle)
                    .unwrap_or(0),
                (*first).present_private.resource_id,
            ),
            None => (0, 0),
        };
        trace_line!(
            "DXGI RotateResourceIdentities: rotated {} resources, alloc[0]=0x{:x} scanout_res[0]={}",
            n,
            first_handle,
            first_resource_id
        );
    }
    // HRESULT unchanged: every path returned 0 before and every path returns 0
    // now. Making a refused rotation FAIL the DDI is a separate decision with
    // its own blast radius.
    0
}

pub(crate) static ROTATE_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(crate) static BLT_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(crate) static BLT1_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(crate) static RESIDENCY_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(crate) static MPO_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(crate) static PRESENT1_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(crate) static DXGI13_RESERVED_LOG_COUNT: LogThrottle = LogThrottle::new();
pub(crate) const DXGI_MPO_MAX_PLANES: u32 = 16;

pub(crate) unsafe extern "C" fn dxgi_blt(arg: *mut ddi::DXGI_DDI_ARG_BLT) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    let Some(context) = d3d11_context(dxgi_device_handle(a.hDevice)) else {
        return 0;
    };
    let dst_h = dxgi_resource_handle(a.hDstResource);
    let src_h = dxgi_resource_handle(a.hSrcResource);
    let (Some(dst), Some(src)) = (load_resource(dst_h), load_resource(src_h)) else {
        log_error!(
            "DXGI Blt: missing resource dst=0x{:x} src=0x{:x}",
            a.hDstResource,
            a.hSrcResource
        );
        return 0;
    };

    if let Some(n) = BLT_LOG_COUNT.first_n_then_every_from_one(128, 512) {
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        let mut dst_desc = D3D11_TEXTURE2D_DESC::default();
        let src_tex = (*src).cast::<ID3D11Texture2D>().ok();
        let dst_tex = (*dst).cast::<ID3D11Texture2D>().ok();
        if let Some(tex) = &src_tex {
            tex.GetDesc(&mut src_desc);
        }
        if let Some(tex) = &dst_tex {
            tex.GetDesc(&mut dst_desc);
        }
        trace_line!(
            "DXGI Blt: #{} src={:p}/{} alloc=0x{:x} {}x{} fmt={} -> \
             dst={:p}/{} alloc=0x{:x} {}x{} fmt={} flags=0x{:x} rotate={}",
            n,
            src_h.pDrvPrivate,
            a.SrcSubresource,
            resource_allocation(src_h),
            src_desc.Width,
            src_desc.Height,
            src_desc.Format.0,
            dst_h.pDrvPrivate,
            a.DstSubresource,
            resource_allocation(dst_h),
            dst_desc.Width,
            dst_desc.Height,
            dst_desc.Format.0,
            a.Flags.__bindgen_anon_1.Value,
            a.Rotate,
        );
    }

    // The DXGI 1.0 blit DDI has no source rectangle. For DWM/windowed present
    // setup the runtime uses it to move between compatible proxy/front-buffer
    // surfaces, so a full subresource copy is the safest baseline.
    context.CopySubresourceRegion(
        &*dst,
        a.DstSubresource,
        a.DstLeft,
        a.DstTop,
        0,
        &*src,
        a.SrcSubresource,
        None,
    );
    context.Flush();
    0
}

pub(crate) unsafe extern "C" fn dxgi_blt1(arg: *mut ddi::DXGI_DDI_ARG_BLT1) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    let Some(context) = d3d11_context(dxgi_device_handle(a.hDevice)) else {
        return 0;
    };
    let dst_h = dxgi_resource_handle(a.hDstResource);
    let src_h = dxgi_resource_handle(a.hSrcResource);
    let (Some(dst), Some(src)) = (load_resource(dst_h), load_resource(src_h)) else {
        log_error!(
            "DXGI Blt1: missing resource dst=0x{:x} src=0x{:x}",
            a.hDstResource,
            a.hSrcResource
        );
        return E_INVALIDARG;
    };

    const BLT_RESOLVE: u32 = 0x1;
    const BLT_CONVERT: u32 = 0x2;
    const BLT_STRETCH: u32 = 0x4;
    let flags = a.Flags.__bindgen_anon_1.Value;
    if flags & BLT_CONVERT != 0 {
        log_error!("DXGI Blt1: convert unsupported flags=0x{flags:x}");
        return DXGI_ERROR_UNSUPPORTED;
    }

    let src_w = a.SrcRight.saturating_sub(a.SrcLeft);
    let src_h_px = a.SrcBottom.saturating_sub(a.SrcTop);
    let dst_w = a.DstRight.saturating_sub(a.DstLeft);
    let dst_h_px = a.DstBottom.saturating_sub(a.DstTop);

    if flags & BLT_RESOLVE != 0 {
        let format = resource_dxgi_format(dst_h);
        if format.0 == 0 {
            log_error!("DXGI Blt1: resolve has unknown destination format");
            return E_INVALIDARG;
        }
        context.ResolveSubresource(&*dst, a.DstSubresource, &*src, a.SrcSubresource, format);
        context.Flush();
        return 0;
    }

    if flags & BLT_STRETCH != 0
        || (src_w != 0 && dst_w != 0 && (src_w != dst_w || src_h_px != dst_h_px))
    {
        log_error!(
            "DXGI Blt1: stretch unsupported src={}x{} dst={}x{} flags=0x{flags:x}",
            src_w,
            src_h_px,
            dst_w,
            dst_h_px
        );
        return DXGI_ERROR_UNSUPPORTED;
    }

    let bx;
    let bx_ptr = if a.SrcRight > a.SrcLeft && a.SrcBottom > a.SrcTop {
        bx = D3D11_BOX {
            left: a.SrcLeft,
            top: a.SrcTop,
            front: 0,
            right: a.SrcRight,
            bottom: a.SrcBottom,
            back: 1,
        };
        Some(&bx as *const D3D11_BOX)
    } else {
        None
    };

    if BLT1_LOG_COUNT.first_n(32).is_some() {
        trace_line!(
            "DXGI Blt1: copy src={}x{} dst={}x{} flags=0x{flags:x}",
            src_w,
            src_h_px,
            dst_w,
            dst_h_px
        );
    }

    context.CopySubresourceRegion(
        &*dst,
        a.DstSubresource,
        a.DstLeft,
        a.DstTop,
        0,
        &*src,
        a.SrcSubresource,
        bx_ptr,
    );
    context.Flush();
    0
}

pub(crate) unsafe extern "C" fn dxgi_offer_resources(
    arg: *mut ddi::DXGI_DDI_ARG_OFFERRESOURCES,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    if RESIDENCY_LOG_COUNT.first_n(32).is_some() {
        log_error!(
            "DXGI OfferResources: resources={} priority={} (kept resident)",
            a.Resources,
            a.Priority
        );
    }
    0
}

pub(crate) unsafe extern "C" fn dxgi_reclaim_resources(
    arg: *mut ddi::DXGI_DDI_ARG_RECLAIMRESOURCES,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    if !a.pDiscarded.is_null() {
        for i in 0..a.Resources as usize {
            *a.pDiscarded.add(i) = 0;
        }
    }
    if RESIDENCY_LOG_COUNT.first_n(32).is_some() {
        log_error!(
            "DXGI ReclaimResources: resources={} discarded=FALSE",
            a.Resources
        );
    }
    0
}

// ---------------------------------------------------------------------------
// R830 (OWNER DECISION): name the literals, DO NOT change the values.
// ---------------------------------------------------------------------------
//
// Helios advertises MaxPlanes = 16, 16x stretch and shrink, and BILINEAR
// filtering, while the KMD deliberately does not register the MPO3 interface
// (query_adapter_info.rs pins the display surface to WDDM 2.1) and dxgi_blt1
// rejects any stretch with DXGI_ERROR_UNSUPPORTED. So these are caps with no
// kernel overlay path behind them.
//
// The review's own correction stands and is worth keeping visible: the plane
// count IS already a named constant (DXGI_MPO_MAX_PLANES), and
// `dxgi_present_mpo` forwarding only (allocation, subresource) is CORRECT --
// DXGIDDICB_PRESENT_MULTIPLANE_OVERLAY has no geometry fields at all. Plane
// attributes reach the kernel through dxgkrnl's MPO VidPn DDIs, which is
// exactly where Helios has nothing. The unjustified literals were the two 16.0
// factors, BILINEAR and NumCapabilityGroups: 1 -- named below.
//
// Reducing the advertised caps is behaviour-affecting: DWM picks its
// composition strategy from them, and the direct-primary scanout path is this
// tranche's frozen baseline. DEFERRED pending same-boot evidence on whether DWM
// queries MPO at all (zero GetMultiplaneOverlayCaps / MPO-plane lines appear in
// any UMD log on this box, but those logs predate the tranche by three weeks --
// re-sample at the gate). See the ROADMAP T5 entry.
/// The four MPO feature-cap bits Helios advertises. Hoisted to module scope by
/// R830 so `HELIOS_MPO_OVERLAY_CAPS` below can be the single composition.
pub(crate) const RGB: u32 =
    ddi::DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_RGB
        as u32;
pub(crate) const BILINEAR: u32 = ddi::DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_BILINEAR_FILTER
    as u32;
pub(crate) const SHARED: u32 =
    ddi::DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_SHARED
        as u32;
pub(crate) const IMMEDIATE: u32 =
    ddi::DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_DXGI_DDI_MULTIPLANE_OVERLAY_FEATURE_CAPS_IMMEDIATE
        as u32;

/// Maximum stretch the caps advertise. NOT implemented: `dxgi_blt1` refuses any
/// stretch with DXGI_ERROR_UNSUPPORTED.
pub(crate) const HELIOS_MPO_MAX_STRETCH: f32 = 16.0;
/// Maximum shrink the caps advertise. Same caveat as the stretch factor.
pub(crate) const HELIOS_MPO_MAX_SHRINK: f32 = 16.0;
/// One capability group, covering all planes.
pub(crate) const HELIOS_MPO_GROUPS: u32 = 1;
/// The advertised overlay feature caps. BILINEAR is the questionable member --
/// there is no filter path behind it.
pub(crate) const HELIOS_MPO_OVERLAY_CAPS: u32 = RGB | BILINEAR | SHARED | IMMEDIATE;

pub(crate) unsafe extern "C" fn dxgi_get_mpo_caps(
    arg: *mut ddi::DXGI_DDI_ARG_GETMULTIPLANEOVERLAYCAPS,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &mut *arg;
    a.MultiplaneOverlayCaps = ddi::DXGI_DDI_MULTIPLANE_OVERLAY_CAPS {
        MaxPlanes: DXGI_MPO_MAX_PLANES,
        NumCapabilityGroups: HELIOS_MPO_GROUPS,
    };
    if MPO_LOG_COUNT.first_n(16).is_some() {
        log_error!(
            "DXGI GetMultiplaneOverlayCaps: MaxPlanes={} groups=1",
            DXGI_MPO_MAX_PLANES
        );
    }
    0
}

pub(crate) unsafe extern "C" fn dxgi_get_mpo_group_caps(
    arg: *mut ddi::DXGI_DDI_ARG_GETMULTIPLANEOVERLAYGROUPCAPS,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &mut *arg;
    a.MultiplaneOverlayGroupCaps = if a.GroupIndex == 0 {
        ddi::DXGI_DDI_MULTIPLANE_OVERLAY_GROUP_CAPS {
            NumPlanes: DXGI_MPO_MAX_PLANES,
            MaxStretchFactor: HELIOS_MPO_MAX_STRETCH,
            MaxShrinkFactor: HELIOS_MPO_MAX_SHRINK,
            OverlayCaps: HELIOS_MPO_OVERLAY_CAPS,
            StereoCaps: 0,
        }
    } else {
        ddi::DXGI_DDI_MULTIPLANE_OVERLAY_GROUP_CAPS::default()
    };
    if MPO_LOG_COUNT.first_n(16).is_some() {
        log_error!(
            "DXGI GetMultiplaneOverlayGroupCaps: group={} planes={} caps=0x{:x}",
            a.GroupIndex,
            a.MultiplaneOverlayGroupCaps.NumPlanes,
            a.MultiplaneOverlayGroupCaps.OverlayCaps
        );
    }
    0
}

pub(crate) unsafe extern "C" fn dxgi_present_mpo(
    arg: *mut ddi::DXGI_DDI_ARG_PRESENTMULTIPLANEOVERLAY,
) -> i32 {
    probe_entry_attempt(PresentBoundaryEntry::Mpo);
    if arg.is_null() {
        probe_early_refusal(PresentBoundaryEntry::Mpo, "null MPO argument");
        return E_INVALIDARG;
    }
    let a = &*arg;
    if a.PresentPlaneCount == 0 || a.pPresentPlanes.is_null() {
        probe_early_refusal(PresentBoundaryEntry::Mpo, "no MPO planes");
        log_error!("DXGI PresentMultiplaneOverlay: no present planes");
        return E_INVALIDARG;
    }
    if a.PresentPlaneCount > DXGI_MPO_MAX_PLANES {
        probe_early_refusal(PresentBoundaryEntry::Mpo, "too many MPO planes");
        log_error!(
            "DXGI PresentMultiplaneOverlay: too many planes {}",
            a.PresentPlaneCount
        );
        return E_INVALIDARG;
    }

    let h = dxgi_device_handle(a.hDevice);
    let Some(dev) = helios_device(h) else {
        probe_early_refusal(PresentBoundaryEntry::Mpo, "MPO device handle not live");
        return E_INVALIDARG;
    };
    let (false, Some(ctx)) = (dev.dxgi_callbacks.is_null(), dev.context.as_ref()) else {
        if dev.dxgi_callbacks.is_null() {
            PRESENT_BOUNDARY_MPO_MISSING.fetch_add(1, Ordering::Relaxed);
        }
        probe_early_refusal(
            PresentBoundaryEntry::Mpo,
            "MPO callback table or context missing",
        );
        log_error!("DXGI PresentMultiplaneOverlay: no DXGI callbacks/context");
        return DXGI_ERROR_UNSUPPORTED;
    };
    let Some(present_cb) = (*dev.dxgi_callbacks).pfnPresentMultiplaneOverlayCb else {
        PRESENT_BOUNDARY_MPO_MISSING.fetch_add(1, Ordering::Relaxed);
        probe_early_refusal(
            PresentBoundaryEntry::Mpo,
            "pfnPresentMultiplaneOverlayCb missing",
        );
        log_error!("DXGI PresentMultiplaneOverlay: pfnPresentMultiplaneOverlayCb missing");
        return DXGI_ERROR_UNSUPPORTED;
    };

    let mut cb = ddi::DXGIDDICB_PRESENT_MULTIPLANE_OVERLAY::default();
    cb.pDXGIContext = a.pDXGIContext;
    cb.hContext = ctx.handle.as_ptr();
    cb.BroadcastContextCount = 0;

    for i in 0..a.PresentPlaneCount as usize {
        let plane = &*a.pPresentPlanes.add(i);
        let attrs = &plane.PlaneAttributes;
        if MPO_LOG_COUNT.first_n(128).is_some() {
            trace_line!(
                "DXGI MPO plane {}: enabled={} hRes=0x{:x} sub={} flags=0x{:x} \
                 src=({},{}-{}, {}) dst=({},{}-{}, {}) clip=({},{}-{}, {}) rot={} blend={} \
                 dirty={} ycbcr=0x{:x} stretch={}",
                i,
                plane.Enabled,
                plane.hResource,
                plane.SubResourceIndex,
                attrs.Flags,
                attrs.SrcRect.left,
                attrs.SrcRect.top,
                attrs.SrcRect.right,
                attrs.SrcRect.bottom,
                attrs.DstRect.left,
                attrs.DstRect.top,
                attrs.DstRect.right,
                attrs.DstRect.bottom,
                attrs.ClipRect.left,
                attrs.ClipRect.top,
                attrs.ClipRect.right,
                attrs.ClipRect.bottom,
                attrs.Rotation,
                attrs.Blend,
                attrs.DirtyRectCount,
                attrs.YCbCrFlags,
                attrs.StretchQuality
            );
        }
        if plane.Enabled == 0 {
            continue;
        }
        if cb.AllocationInfoCount as usize >= cb.AllocationInfo.len() {
            probe_early_refusal(
                PresentBoundaryEntry::Mpo,
                "MPO allocation list capacity exceeded",
            );
            return E_INVALIDARG;
        }
        let resource = dxgi_resource_handle(plane.hResource);
        let alloc = resource_allocation(resource);
        if alloc == 0 {
            probe_early_refusal(
                PresentBoundaryEntry::Mpo,
                "MPO plane source allocation missing",
            );
            log_error!(
                "DXGI PresentMultiplaneOverlay: plane {} has no allocation hResource=0x{:x}",
                i,
                plane.hResource
            );
            return E_INVALIDARG;
        }
        let slot = cb.AllocationInfoCount as usize;
        cb.AllocationInfo[slot].PresentAllocation = alloc;
        cb.AllocationInfo[slot].SubResourceIndex = plane.SubResourceIndex;
        if MPO_LOG_COUNT.first_n(128).is_some() {
            trace_line!(
                "DXGI MPO plane {} -> allocation=0x{:x} slot={}",
                i,
                alloc,
                slot
            );
        }
        cb.AllocationInfoCount += 1;
    }

    if cb.AllocationInfoCount == 0 {
        probe_early_refusal(PresentBoundaryEntry::Mpo, "no enabled MPO planes");
        log_error!("DXGI PresentMultiplaneOverlay: no enabled planes");
        return E_INVALIDARG;
    }

    probe_mpo_entry(a, &cb);

    if let Some(context) = d3d11_context(h) {
        context.Flush();
    }

    let hr = present_cb(dev.h_rt_device, &cb);
    probe_mpo_result(hr, &cb);
    if MPO_LOG_COUNT.first_n(64).is_some() {
        trace_line!(
            "DXGI PresentMultiplaneOverlay: planes={} enabled={} presentCb=0x{:08x} ctx={:p}",
            a.PresentPlaneCount,
            cb.AllocationInfoCount,
            hr as u32,
            ctx.handle.as_ptr()
        );
    }
    hr
}

pub(crate) unsafe extern "C" fn dxgi_reserved_unsupported(_arg: *mut c_void) -> i32 {
    if DXGI13_RESERVED_LOG_COUNT.first_n(16).is_some() {
        log_error!("DXGI reserved callback -> DXGI_ERROR_UNSUPPORTED");
    }
    DXGI_ERROR_UNSUPPORTED
}

pub(crate) unsafe extern "C" fn dxgi_present1(arg: *mut ddi::DXGI_DDI_ARG_PRESENT1) -> i32 {
    if arg.is_null() {
        probe_early_refusal(
            PresentBoundaryEntry::Present1Multi,
            "null Present1 argument",
        );
        return E_INVALIDARG;
    }
    let a = &*arg;
    if a.SurfacesToPresent == 0 || a.phSurfacesToPresent.is_null() {
        probe_early_refusal(
            PresentBoundaryEntry::Present1Multi,
            "Present1 has no source surfaces",
        );
        log_error!("DXGI Present1: no source surfaces");
        return E_INVALIDARG;
    }

    if a.SurfacesToPresent == 1 {
        probe_entry_attempt(PresentBoundaryEntry::Present1Single);
        let source = *a.phSurfacesToPresent;
        let mut present = ddi::DXGI_DDI_ARG_PRESENT {
            hDevice: a.hDevice,
            hSurfaceToPresent: source.hSurface,
            SrcSubResourceIndex: source.SubResourceIndex,
            hDstResource: a.hDstResource,
            DstSubResourceIndex: a.DstSubResourceIndex,
            pDXGIContext: a.pDXGIContext,
            Flags: a.Flags,
            FlipInterval: a.FlipInterval,
        };
        return dxgi_present_impl(&mut present, PresentBoundaryEntry::Present1Single);
    }

    // WDDM 1.3 Present1's surface array is not an old single-source Present.
    // Earlier entries are part of the DXGI display/release list; the documented
    // callback contract for a many-resource present is specifically to translate
    // only the last source handle into DXGIDDICB_PRESENT. Dirty rects are hints
    // and must never be a failure reason.
    let source_index = a.SurfacesToPresent as usize - 1;
    let source = *a.phSurfacesToPresent.add(source_index);
    let h = dxgi_device_handle(a.hDevice);
    let src_h = dxgi_resource_handle(source.hSurface);
    let dst_h = dxgi_resource_handle(a.hDstResource);
    let src_alloc = resource_allocation(src_h);
    let dst_alloc = resource_allocation(dst_h);
    probe_entry_attempt(PresentBoundaryEntry::Present1Multi);
    probe_present1_multi_entry(
        a,
        source_index,
        source.hSurface as usize,
        source.SubResourceIndex,
        src_alloc,
        dst_alloc,
    );
    if PRESENT1_LOG_COUNT.first_n(64).is_some() {
        trace_line!(
            "DXGI Present1 multi: surfaces={} callback_src={} src={:p}/{} alloc=0x{:x} \
             dst={:p}/{} dstAlloc=0x{:x} dirty={} multiplicity={} flags=0x{:x}",
            a.SurfacesToPresent,
            source_index,
            source.hSurface as *mut c_void,
            source.SubResourceIndex,
            src_alloc,
            a.hDstResource as *mut c_void,
            a.DstSubResourceIndex,
            dst_alloc,
            a.DirtyRects,
            a.BackBufferMultiplicity,
            *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32),
        );
    }

    if src_alloc == 0 {
        probe_early_refusal(
            PresentBoundaryEntry::Present1Multi,
            "Present1 callback source allocation missing",
        );
        log_error!(
            "DXGI Present1 multi: callback source has no allocation hResource=0x{:x}",
            source.hSurface
        );
        return E_INVALIDARG;
    }

    if let Some(context) = d3d11_context(h) {
        context.Flush();
    }

    if let Some(dev) = helios_device(h) {
        // Present1-multi discarded this boolean entirely; #[must_use] on
        // GateOutcome makes that a compiler warning rather than a silence.
        // Unconditional and unbounded-by-nothing: the SUBMITTED wait is on two
        // guest CPU threads and has no slow-GPU case to bound, so it takes the
        // `0` the vehicle-only timeout would have carried.
        let _outcome = run_present_frame_gate(dev, 0, false);
    }

    let present_hr = match finish_present(
        h,
        src_h,
        dst_h,
        src_alloc,
        dst_alloc,
        PresentRequest {
            kind: PresentKind::Present1Multi,
            boundary: PresentBoundaryEntry::Present1Multi,
            dxgi_context: a.pDXGIContext,
            flags: *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32),
        },
        // Present1-multi performs no scanout publish and records no blit —
        // no substitution here, ever.
        None,
        PresentStreamCorrelation::default(),
    ) {
        Ok(hr) => hr,
        // Skips the trailing PRESENT1_LOG_COUNT line, exactly as the bare
        // `return DXGI_ERROR_UNSUPPORTED` / `return E_FAIL` did.
        Err(hr) => return hr,
    };

    if PRESENT1_LOG_COUNT.first_n(64).is_some() {
        trace_line!(
            "DXGI Present1 multi: presentCb=0x{:08x} srcAlloc=0x{:x} dstAlloc=0x{:x} opt_comp={} \
             dxgiCtx={:p} hContext={:p}",
            present_hr as u32,
            src_alloc,
            dst_alloc,
            present_optimize_composition_enabled() as u32,
            a.pDXGIContext,
            dev_context_for_log(h)
        );
    }
    present_hr
}

pub(crate) unsafe extern "C" fn dxgi_check_present_duration_support(
    arg: *mut ddi::DXGI_DDI_ARG_CHECKPRESENTDURATIONSUPPORT,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &mut *arg;
    a.ClosestSmallerDuration = 0;
    a.ClosestLargerDuration = 0;
    if PRESENT1_LOG_COUNT.first_n(16).is_some() {
        log_error!(
            "DXGI CheckPresentDurationSupport: desired={} smaller=0 larger=0",
            a.DesiredPresentDuration
        );
    }
    0
}
