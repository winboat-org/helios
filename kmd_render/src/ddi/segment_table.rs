//! The reported segment table, built once and validated by construction.
//!
//! # The rule this type exists to own
//!
//! **A `SupportsCpuHostAperture` segment must be the LAST reported segment.**
//! ETW-proven 2026-07-05: any segment reported after a cpu-host segment fails
//! AddAdapter with "Invalid flags specified for segment #2" and the device lands
//! in Code 43. Before this type the rule held only because the production
//! topology happened to place the BAR last, and it was checked by nothing.
//!
//! # Why the check reads the SPEC and not the writer
//!
//! Two different writers produce a cpu-host segment.
//! `write_cpu_host_memory_descriptor` sets `SupportsCpuHostAperture` +
//! `SupportsCachedCpuHostAperture` unconditionally, but the BAR descriptor is
//! knob-driven: `BarSegFlags` bit 3 is `SupportsCpuHostAperture`, and the default
//! 0x1C sets it. So "is this segment cpu-host" is a property of the flag word a
//! registry DWORD chose, not of which function writes it, and
//! [`SegmentSpec::is_cpu_host`] is derived accordingly. Keying the rule on writer
//! identity would silently stop enforcing it the moment someone set
//! `BarSegFlags=0x04`.
//!
//! # What this does NOT do
//!
//! Encoding the position rule as a type (`Last<CpuHostSegment>`) is possible and
//! would be cosmetic with two slots — a validating constructor with a named
//! refusal counter is the honest shape at this size. What it buys is that a
//! rejected configuration becomes a counted refusal that still binds, instead of
//! a silent Code 43 with nothing in the diag ring naming the rule.

use crate::adapter::AdapterKnobs;

/// `BarSegFlags` bit 3 — `SupportsCpuHostAperture`. Documented at
/// `write_bar_knob_descriptor`; named here because the must-be-last rule keys
/// off it.
pub(crate) const BAR_SEG_FLAG_SUPPORTS_CPU_HOST_APERTURE: u32 = 0x08;

/// One reported segment, as the DESCRIPTOR it will become — not as the function
/// that writes it.
#[derive(Clone, Copy)]
pub(crate) enum SegmentSpec {
    /// The viogpu3d-style linear aperture. Always index 0: `InitDmaPools`
    /// validates `segdesc[0]` for paging-buffer-host capability, which a
    /// CPU-visible memory segment never has.
    Aperture,
    /// The contiguous-RAM cpu-host segment, reported under
    /// `BarSegTopology::Disabled` as the recovery baseline.
    RamCpuHost { base: u64, size: u64 },
    /// The host-visible BAR window head, knob-shaped by `BarSegFlags`.
    Bar { gpa: u64, size: u64, cpu_host: bool },
}

impl SegmentSpec {
    /// Whether the emitted descriptor will carry `SupportsCpuHostAperture`.
    pub(crate) const fn is_cpu_host(&self) -> bool {
        match self {
            Self::Aperture => false,
            Self::RamCpuHost { .. } => true,
            Self::Bar { cpu_host, .. } => *cpu_host,
        }
    }

    /// Build the BAR spec, reading its cpu-host-ness out of the knob snapshot.
    pub(crate) const fn bar(gpa: u64, size: u64, knobs: &AdapterKnobs) -> Self {
        Self::Bar {
            gpa,
            size,
            cpu_host: knobs.bar_seg_flags & BAR_SEG_FLAG_SUPPORTS_CPU_HOST_APERTURE != 0,
        }
    }
}

/// Why [`SegmentTable::new`] refused a proposed table.
#[derive(Clone, Copy)]
pub(crate) enum SegmentRuleViolation {
    /// A cpu-host segment is followed by another segment — the AddAdapter rule.
    CpuHostNotLast,
    /// More entries than the reported topology can hold.
    TooMany,
}

impl SegmentRuleViolation {
    /// Value recorded in `SegRule`, so the refusal names itself.
    pub(crate) const fn code(self) -> u32 {
        match self {
            Self::CpuHostNotLast => 1,
            Self::TooMany => 2,
        }
    }
}

/// The validated segment table. Immutable once built.
///
/// Capacity is 2 because the two surviving topologies are `[Aperture, Bar]` and
/// `[Aperture, RamCpuHost]` — the three-segment shapes were all documented as
/// AddAdapter-rejected and were deleted (R904).
#[derive(Clone, Copy)]
pub(crate) struct SegmentTable {
    entries: [Option<SegmentSpec>; Self::MAX],
    len: u32,
}

impl SegmentTable {
    pub(crate) const MAX: usize = 2;

    /// The aperture-only shape: `NbSegment` = 1, aperture at index 0.
    ///
    /// The known-good fallback, and the same shape the no-BAR baseline emits.
    /// `NbSegment` = 0 is a shape this driver has never booted, so a refusal
    /// falls back to this rather than to an empty table.
    pub(crate) const APERTURE_ONLY: Self = Self {
        entries: [Some(SegmentSpec::Aperture), None],
        len: 1,
    };

    /// Validate a proposed table and assign segment ids from position.
    ///
    /// Segment ids are positional — index 0 is id 1 — which is a dxgkrnl
    /// convention, not a choice. Enforcing it here is what stops the id and the
    /// slot from being maintained separately in two files.
    pub(crate) fn new(specs: &[SegmentSpec]) -> Result<Self, SegmentRuleViolation> {
        if specs.len() > Self::MAX {
            return Err(SegmentRuleViolation::TooMany);
        }
        // THE RULE: a cpu-host segment may only be the final entry. Checked over
        // every entry but the last, so both "cpu-host in the middle" and "two
        // cpu-host segments" (which implies the first is not last) are refused.
        let mut i = 0;
        while i + 1 < specs.len() {
            if specs[i].is_cpu_host() {
                return Err(SegmentRuleViolation::CpuHostNotLast);
            }
            i += 1;
        }
        let mut entries = [None; Self::MAX];
        let mut i = 0;
        while i < specs.len() {
            entries[i] = Some(specs[i]);
            i += 1;
        }
        Ok(Self {
            entries,
            len: specs.len() as u32,
        })
    }

    /// `NbSegment`. The count and the descriptor write loop read this same
    /// immutable value, which is the premise the old SAFETY comment asserted
    /// without ever establishing.
    pub(crate) const fn len(&self) -> u32 {
        self.len
    }

    /// `(segment id, spec)` in report order. Ids are positional: index 0 = id 1.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u32, SegmentSpec)> + '_ {
        self.entries
            .iter()
            .flatten()
            .enumerate()
            .map(|(idx, spec)| (idx as u32 + 1, *spec))
    }

    /// The id the BAR segment was reported as, if it is in the table.
    ///
    /// This is what makes the internal and reported views one fact: StartDevice
    /// clears `bar_segment` when this returns `None`, so no consumer can place
    /// an allocation against a segment id dxgkrnl was never told about.
    pub(crate) fn bar_seg_id(&self) -> Option<u32> {
        self.iter()
            .find(|(_, spec)| matches!(spec, SegmentSpec::Bar { .. }))
            .map(|(id, _)| id)
    }
}
