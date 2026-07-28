//! The BAR memory segment and the reported segment table.
//!
//! Moved verbatim out of `ddi/start_device.rs` by T8/R1102. Named
//! `bar_segment` rather than `segments` so it cannot be confused with
//! `adapter/segments.rs` (R1101), which owns the descriptors this builds.

/// Size cap for the VidMm-owned head partition of the host-visible window (the
/// CPU-visible BAR memory segment). The window is 8 GiB on the current
/// QEMU config (`hostmem=8G`); 1 GiB comfortably holds the CPU-rasterized
/// GDI/shadow/staging/shared-primary standard allocations (a full-screen
/// surface is ~8 MiB) while leaving the rest to the KMD/ICD blob allocator.
const BAR_SEGMENT_MAX_BYTES: u64 = 1 << 30;

/// The segment topology this adapter may report. **Two shapes, not five.**
///
/// The `BarSegMode` registry DWORD (service key; read once per StartDevice, so
/// experiments iterate via `reg add` + `devcon restart` — AddAdapter re-runs
/// without a rebuild/reboot) once selected among five, because BOTH initial
/// shapes (classic CpuVisible 22.22.45, CpuHostAperture 22.22.46) were rejected
/// at AddAdapter right after the segment queries and each blind retry cost an
/// owner reboot. Four of those arms were annotated REJECTED or historic in this
/// file's own table and yet stayed reachable from a DWORD, and any unrecognised
/// value fell through to a shape the same table documented as rejected:
///
///   1  = 3 segments (aperture/RAM/BAR id 3) — REJECTED by dxgmms: a
///        SupportsCpuHostAperture segment must be the LAST segment, so ANY
///        segment after the RAM cpu-host segment fails AddAdapter with
///        "Invalid flags specified for segment #2" (ETW AzureTriage, 2026-07-05)
///   2  = 3 segments, BAR id 3, 64 MiB   (historic size-bisect arm; rejected)
///   5  = 3 segments, RAM probe id 3     (historic backing-bisect arm; rejected)
///   11 = 3 segments swapped: aperture + BAR id 2 + RAM id 3 (rejected —
///        confirms the must-be-last rule; the BAR cpu-host seg isn't last)
///
/// A VM left with `BarSegMode=5` from an old bisect booted into an adapter that
/// reported a segment no allocation could use, which is indistinguishable from a
/// code regression; 1, 2, 11 and any unknown value reached Code 43. They are gone.
/// An unrecognised value is now COERCED to the production shape and counted in
/// `BarMCo`, so a stale knob is loud rather than fatal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BarSegTopology {
    /// `BarSegMode=0` — no BAR segment. The recovery baseline that always binds:
    /// aperture (id 1) plus, if the contiguous allocation succeeded, the
    /// paging-RAM cpu-host segment (id 2).
    Disabled,
    /// `BarSegMode=10` — **the production shape**. Two segments: aperture (id 1)
    /// plus the BAR as id 2, the vestigial paging-RAM segment dropped (page
    /// tables live in system segment 0, paging buffers in the aperture). With
    /// GDI surfaces in this device segment win32k routes their rasterization
    /// through DxgkDdiRenderGdi instead of CPU raster into aperture pages: the
    /// two-memory-split fix. First full desktop 2026-07-05 20:53.
    ApertureAndBar,
}

impl TryFrom<u32> for BarSegTopology {
    /// The unrecognised value, for the caller to count.
    type Error = u32;

    fn try_from(mode: u32) -> Result<Self, u32> {
        match mode {
            0 => Ok(Self::Disabled),
            10 => Ok(Self::ApertureAndBar),
            other => Err(other),
        }
    }
}

/// Configure the BAR memory segment per [`BarSegTopology`].
///
/// Takes the knob snapshot rather than reading the registry: StartDevice has
/// already read `BarSegMode` once and mirrored it to `BarM`.
fn setup_bar_segment(
    adapter: &crate::adapter::AdapterContext,
    knobs: &crate::adapter::AdapterKnobs,
) -> Option<crate::adapter::BarSegment> {
    let topo = match BarSegTopology::try_from(knobs.bar_seg_mode) {
        Ok(topo) => topo,
        Err(stale) => {
            // A deleted bisect arm, or a typo. Bind the production shape and say
            // which value was coerced — silently honouring it would reproduce
            // exactly the "reports a segment nothing may use" failure this
            // reduction exists to remove.
            crate::diag::fault(crate::diag::FaultCounter::BarMCo, stale);
            BarSegTopology::ApertureAndBar
        }
    };
    if topo == BarSegTopology::Disabled {
        return None;
    }
    let window = adapter.with_virtio(|v| v.host_visible()).ok().flatten()?;
    let size = (window.len / 2).min(BAR_SEGMENT_MAX_BYTES) & !4095;
    if size < (16 << 20) || size > window.len {
        crate::diag::record(0x0B00_00E8);
        crate::diag::fault(crate::diag::FaultCounter::StBar, (size >> 20) as u32);
        return None;
    }
    // The KMD blob-window allocator must never hand out offsets inside the
    // aperture region (dxgkrnl's CPU-host-aperture allocator owns them).
    // Refused (and counted) if any window offset has already been issued —
    // moving the VidMm partition out from under live mappings would strand
    // every offset below the new mark with no way to recycle it. Nothing has
    // been mapped at this point in StartDevice; a false here is a real defect.
    let _ = adapter.with_virtio(|v| v.configure_window_reserve(size));
    crate::diag::record(0x0B00_0008);
    crate::diag::record(((size >> 20) & 0xFFFF_FFFF) as u32);
    Some(crate::adapter::BarSegment {
        gpa: window.base,
        size,
        // Positional: the aperture is always index 0 (id 1), so the BAR is index
        // 1. This used to be computed (`if mode == 10 || mode == 11 { 2 } else
        // { 3 }`) purely because the deleted topologies moved it to id 3.
        seg_id: crate::ddi::gpummu::MEMORY_SEGMENT_ID,
    })
}

/// Build the reported segment table, and make the driver's own view agree with it.
///
/// The single construction site. `query_segments` used to synthesize a table of
/// its own from live adapter state on every call while `adapter.bar_segment`
/// carried an independent view of the same BAR, and the two could disagree —
/// three subsystems then placed allocations against a segment id dxgkrnl was
/// never told about (k-capsescape-04).
///
/// `bar_segment` is taken by `&mut` precisely so that cannot happen: if the BAR
/// does not make it into the table, it is cleared here, in the same step, before
/// the transport generation that publishes it is installed. `SegDiv` counts that
/// — a divergence PREVENTED, not observed.
///
/// A rule violation is refused rather than reported: binding with the
/// aperture-only shape and a `SegRule` breadcrumb beats AddAdapter failing with
/// Code 43 and nothing in the ring naming the rule. The aperture-only shape is
/// the same one the no-BAR baseline emits, so it is a known-good fallback.
fn build_segment_table(
    bar_segment: &mut Option<crate::adapter::BarSegment>,
    paging_ram: Option<(u64, u64)>,
    knobs: &crate::adapter::AdapterKnobs,
) -> crate::ddi::segment_table::SegmentTable {
    use crate::ddi::segment_table::{SegmentSpec, SegmentTable};

    // The aperture is ALWAYS first: InitDmaPools validates segdesc[0] for
    // paging-buffer-host capability, which a CPU-visible memory segment never has.
    let mut specs = [SegmentSpec::Aperture; SegmentTable::MAX];
    let mut len = 1;
    match (bar_segment.as_ref(), paging_ram) {
        // ApertureAndBar (production). The paging-RAM segment is dropped; it was
        // vestigial (page tables live in system segment 0, paging buffers in the
        // aperture).
        (Some(bar), _) => {
            specs[1] = SegmentSpec::bar(bar.gpa, bar.size, knobs);
            len = 2;
        }
        // Disabled: the recovery baseline.
        (None, Some((base, size))) => {
            specs[1] = SegmentSpec::RamCpuHost { base, size };
            len = 2;
        }
        (None, None) => {}
    }

    match SegmentTable::new(&specs[..len]) {
        Ok(table) => {
            if bar_segment.is_some() && table.bar_seg_id().is_none() {
                // Unreachable with the arms above, which only omit the BAR when
                // it is already absent. Counted anyway: this is the exact
                // divergence the item is about, and a future arm could
                // reintroduce it.
                crate::diag::fault(crate::diag::FaultCounter::SegDiv, 1);
                *bar_segment = None;
            }
            table
        }
        Err(violation) => {
            crate::diag::fault(crate::diag::FaultCounter::SegRule, violation.code());
            *bar_segment = None;
            SegmentTable::APERTURE_ONLY
        }
    }
}
