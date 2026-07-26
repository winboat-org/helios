//! helios_kmd_logic — host-testable leaf logic for the Helios WDDM miniport.
//!
//! `kmd_render` cannot host a libtest harness at all: `kmd_render/build.rs` runs
//! bindgen over `ntddk.h` / `dispmprt.h` / `d3dkmddi.h` and shells out to
//! `rc.exe`, and the crate is a `panic = "abort"` `cdylib`. So any KMD logic that
//! deserves an automated oracle has to be moved *out* of the crate rather than
//! tested in place. This crate is where it goes.
//!
//! The contract that makes that worth doing is the absent dependency edge: there
//! is no `wdk-sys`, no `wdk-build`, no `bytemuck`, and no generated `dxgk`
//! binding reachable from here, so nothing in this crate can read a `DXGKARG_*`
//! field, a `HANDLE`, or an atomic out of `AdapterContext`. Every rule below is
//! a function of its arguments and nothing else, which is exactly the property
//! that makes it testable on the host.
//!
//! Run the tests with `cargo test` inside `kmd_logic/`, the same way `protocol/`
//! is tested. Nothing else runs them.

#![no_std]

/// Page granularity for blob window offsets/sizes and for WDDM allocation sizes.
///
/// Moved verbatim from `kmd_render/src/ddi/create_allocation.rs` (`const PAGE`)
/// and `kmd_render/src/virtio/gpu.rs` (`const BLOB_PAGE`), which held two
/// byte-identical copies of the constant and of [`round_up_page`].
pub const PAGE_BYTES: u64 = 4096;

/// Cross-adapter row-major textures require a 256-byte row-pitch alignment
/// (`D3D12_TEXTURE_DATA_PITCH_ALIGNMENT`). The IddCx composition surface is a
/// cross-adapter resource (created as a standard allocation, opened on the Helios
/// render side — `rendering-on-a-discrete-gpu-using-cross-adapter-resources.md`),
/// so its backing must be linear with this pitch for the IndirectKMD adapter to
/// open the same surface. PATH-A (2026-06-22).
pub const CROSS_ADAPTER_PITCH_ALIGN: u32 = 256;

/// Round `n` up to the next [`PAGE_BYTES`] multiple (saturating).
///
/// Saturating on purpose: `u64::MAX` rounds to `0xFFFF_FFFF_FFFF_F000`, never to
/// zero. (A third, non-saturating copy still lives in
/// `kmd_render/src/virtio/venus.rs`; it is marked `DIVERGES` there and is
/// deliberately *not* unified here, because changing a Venus allocation size is a
/// behaviour change that needs its own before/after evidence.)
pub const fn round_up_page(n: u64) -> u64 {
    n.saturating_add(PAGE_BYTES - 1) & !(PAGE_BYTES - 1)
}

/// 32-bpp linear row pitch aligned to the cross-adapter requirement.
///
/// This is the stride the KMD hands the host in `SET_SCANOUT_BLOB`: a 1896-wide
/// primary must produce 7680, not `width * 4` = 7584. Reading each scanout row
/// short is invisible guest-side, which is why the vectors below are pinned.
pub const fn cross_adapter_pitch(width: u32) -> u32 {
    let raw = width.saturating_mul(4);
    raw.saturating_add(CROSS_ADAPTER_PITCH_ALIGN - 1) & !(CROSS_ADAPTER_PITCH_ALIGN - 1)
}

/// Checked sub-range of a mapped byte window: is `offset .. offset + bytes`
/// wholly inside a window of `len` bytes?
///
/// Returns the byte offset on success, `None` on any overflow or overrun. Both
/// operands of every `copy_nonoverlapping` in the paging engine have to answer
/// this question, and one of them (the MDL side of a classic TRANSFER) used to
/// answer it with a comment instead — `MdlOffset`/`TransferSize` were applied
/// raw to a pointer that carries no length, so an over-long eviction wrote
/// kernel memory past the mapped buffer (k-paging-03).
pub const fn window_range(len: u64, offset: u64, bytes: u64) -> Option<u64> {
    match offset.checked_add(bytes) {
        Some(end) if end <= len => Some(offset),
        _ => None,
    }
}

/// A physical page frame number, as supplied by `DXGK_PTE::PageAddress` or
/// derived from `MmGetPhysicalAddress`.
///
/// Exists so the PFN-to-address conversion is written once, checked. The
/// paging engine guarded it with `checked_shl(12)`, which is not an overflow
/// guard at all: `u64::checked_shl` returns `None` only when the SHIFT COUNT is
/// >= 64, so with a constant 12 it can never fail and the `BAR_ERR_VIRTUAL`
/// counter documented for that failure was unreachable (k-paging-10).
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pfn(pub u64);

impl Pfn {
    /// Byte address of this page frame, or `None` if it cannot be represented.
    ///
    /// Two rules, both load-bearing for the caller: the multiply must not
    /// overflow, and the result must fit a POSITIVE `i64`, because it is
    /// assigned to `PHYSICAL_ADDRESS.QuadPart` (an `i64`) and handed to
    /// `MmMapIoSpace` — an address with bit 63 set would arrive negative.
    pub const fn physical_address(self) -> Option<u64> {
        match self.0.checked_mul(PAGE_BYTES) {
            Some(address) if address <= i64::MAX as u64 => Some(address),
            _ => None,
        }
    }
}

/// The private-data trailer layouts this driver actually accepts.
///
/// The trailer is guest-supplied (`D3DKMTCreateAllocation` private data), and
/// the length test used to be a max-union bound — "at least 24, copy up to 48" —
/// so any length in 25..=47 was accepted and copied into the MIDDLE of a field,
/// zero-extending the remainder. A 30-byte trailer yielded
/// `venus_alloc_size = real & 0x0000_FFFF_FFFF_FFFF` and `plane_offset = 0`: a
/// plausible-looking but wrong exact import size, which is the undersize-import
/// class that previously produced host Xid 31 FAULT_PTE. Per-arm validation,
/// not max-union (k-alloc-03).
///
/// The byte counts are duplicated from `helios_protocol` because this crate
/// deliberately has no dependency edge to it; `kmd_render` pins them together
/// with a `const` assertion at the use site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MetaLayout {
    /// Geometry + bind/misc only, no venus identity fields. Exactly the first
    /// 24 bytes of the full layout, so it parses into a zero-extended meta and
    /// an allocation created by a pre-identity driver instance can still be
    /// opened after a component update without a reboot.
    Legacy24,
    /// The full trailer. Longer buffers are accepted and the excess ignored —
    /// that is how a future writer adds fields without breaking this one.
    Full48,
}

impl MetaLayout {
    pub const LEGACY_BYTES: usize = 24;
    pub const FULL_BYTES: usize = 48;

    /// Classify a trailer length, or `None` if it is not one of the two real
    /// layouts. `None` must produce a refusal, never a partial read.
    pub const fn from_trailer_len(len: usize) -> Option<Self> {
        if len == Self::LEGACY_BYTES {
            Some(Self::Legacy24)
        } else if len >= Self::FULL_BYTES {
            Some(Self::Full48)
        } else {
            None
        }
    }

    /// How many bytes to copy. Comes from the layout, never from arithmetic on
    /// the caller-supplied size.
    pub const fn copy_bytes(self) -> usize {
        match self {
            Self::Legacy24 => Self::LEGACY_BYTES,
            Self::Full48 => Self::FULL_BYTES,
        }
    }
}

impl TryFrom<usize> for MetaLayout {
    type Error = ();

    fn try_from(len: usize) -> Result<Self, Self::Error> {
        Self::from_trailer_len(len).ok_or(())
    }
}

/// Verdict of one seqlock read attempt over a published descriptor.
///
/// The primary-scanout descriptor is published field by field and was read the
/// same way, with the resource id loaded FIRST and everything else `Relaxed`:
/// the publisher's store order defends a first publish, but a REPUBLISH landing
/// between the reader's loads yields resource_id from generation N combined with
/// pitch/format/plane_offset from N+1, and the consumer cannot detect it
/// (k-capsescape-11).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SeqRead {
    /// The snapshot is coherent.
    Stable,
    /// A publish was in flight (odd sequence) or landed mid-read — read again.
    Retry,
}

/// Classify a seqlock read from the sequence value before and after the fields.
///
/// Odd `before` means a writer held the descriptor when the read started; a
/// changed value means one landed during it. Both are retries; nothing else is.
pub const fn seq_read(before: u32, after: u32) -> SeqRead {
    if before % 2 != 0 || before != after {
        SeqRead::Retry
    } else {
        SeqRead::Stable
    }
}

/// Bound on seqlock read attempts before the reader gives up and reports "no
/// coherent value" instead of spinning.
///
/// The reader is a PASSIVE escape but the publishers can run at raised IRQL on
/// the VidPn path, so an unbounded spin here would be a new wedge class: the
/// escape thread could hold the CPU while the publisher is descheduled.
pub const SEQ_READ_ATTEMPTS: u32 = 8;

#[cfg(test)]
mod tests {
    use super::*;

    /// Vectors taken from the production comments so they double as
    /// documentation. `1896 -> 7680` is the case `ddi/display.rs` states by name:
    /// it is the shipped `.117`-era pitch defect, where the host read every
    /// scanout row short.
    #[test]
    fn cross_adapter_pitch_vectors() {
        assert_eq!(cross_adapter_pitch(0), 0);
        assert_eq!(cross_adapter_pitch(1), 256);
        assert_eq!(cross_adapter_pitch(64), 256);
        assert_eq!(cross_adapter_pitch(65), 512);
        assert_eq!(cross_adapter_pitch(1896), 7680);
        assert_eq!(cross_adapter_pitch(1920), 7680);
        assert_eq!(cross_adapter_pitch(1952), 7936);
        assert_eq!(cross_adapter_pitch(u32::MAX), 0xFFFF_FF00);
    }

    /// A 256-byte-aligned width is already a multiple of the alignment, so the
    /// function must be idempotent rather than adding another 256 bytes.
    #[test]
    fn cross_adapter_pitch_is_idempotent_on_aligned_widths() {
        for width in [64u32, 128, 320, 1920, 3840] {
            let pitch = cross_adapter_pitch(width);
            assert_eq!(pitch % CROSS_ADAPTER_PITCH_ALIGN, 0, "width {width}");
            assert!(pitch >= width * 4, "width {width} pitch {pitch}");
            assert!(
                pitch - width * 4 < CROSS_ADAPTER_PITCH_ALIGN,
                "width {width}"
            );
        }
    }

    #[test]
    fn round_up_page_vectors() {
        assert_eq!(round_up_page(0), 0);
        assert_eq!(round_up_page(1), 4096);
        assert_eq!(round_up_page(4096), 4096);
        assert_eq!(round_up_page(4097), 8192);
        assert_eq!(round_up_page(u64::MAX), 0xFFFF_FFFF_FFFF_F000);
    }

    /// The saturating add is the whole point of this copy: the near-`u64::MAX`
    /// input must clamp downward, never wrap to 0 and hand a zero-byte mapping
    /// to `MmMapLockedPagesSpecifyCache`.
    #[test]
    fn round_up_page_saturates_instead_of_wrapping() {
        for n in [u64::MAX, u64::MAX - 1, u64::MAX - 4094, u64::MAX - 4095] {
            assert_eq!(round_up_page(n), 0xFFFF_FFFF_FFFF_F000, "n {n:#x}");
        }
        // One page below the clamp still rounds normally.
        assert_eq!(round_up_page(u64::MAX - 4096), 0xFFFF_FFFF_FFFF_F000);
        assert_eq!(round_up_page(0xFFFF_FFFF_FFFF_EFFF), 0xFFFF_FFFF_FFFF_F000);
    }

    /// The exact boundary the MDL bound rests on: a transfer that ends on the
    /// last mapped byte is legal, one byte more is not.
    #[test]
    fn window_range_accepts_exact_fit_and_rejects_one_past() {
        assert_eq!(window_range(4096, 0, 4096), Some(0));
        assert_eq!(window_range(4096, 4095, 1), Some(4095));
        assert_eq!(window_range(4096, 0, 4097), None);
        assert_eq!(window_range(4096, 4096, 1), None);
        assert_eq!(window_range(4096, 1, 4096), None);
    }

    /// A zero-byte op inside the window is fine; a zero-byte op at the far end
    /// is still bounded by `len`.
    #[test]
    fn window_range_handles_empty_ranges() {
        assert_eq!(window_range(4096, 0, 0), Some(0));
        assert_eq!(window_range(4096, 4096, 0), Some(4096));
        assert_eq!(window_range(4096, 4097, 0), None);
        assert_eq!(window_range(0, 0, 0), Some(0));
        assert_eq!(window_range(0, 0, 1), None);
    }

    /// The overflow case is the whole reason this is checked arithmetic:
    /// `MdlOffset << 12` is a guest/VidMm-supplied quantity, so `offset + bytes`
    /// must not be allowed to wrap and land back inside the window.
    #[test]
    fn window_range_rejects_wrapping_offsets() {
        assert_eq!(window_range(4096, u64::MAX, 1), None);
        assert_eq!(window_range(4096, u64::MAX - 4095, 4096), None);
        assert_eq!(window_range(u64::MAX, u64::MAX, 1), None);
    }

    #[test]
    fn pfn_physical_address_vectors() {
        assert_eq!(Pfn(0).physical_address(), Some(0));
        assert_eq!(Pfn(1).physical_address(), Some(4096));
        assert_eq!(Pfn(0x1234).physical_address(), Some(0x1234 * 4096));
        // Largest PFN whose address still has bit 63 clear.
        assert_eq!(
            Pfn((1 << 51) - 1).physical_address(),
            Some(0x7FFF_FFFF_FFFF_F000)
        );
    }

    /// The multiply overflows at 2^52 pages (2^52 * 4096 == 2^64), which is the
    /// case `checked_shl(12)` could never catch.
    #[test]
    fn pfn_physical_address_rejects_overflow() {
        assert_eq!(Pfn(1 << 52).physical_address(), None);
        assert_eq!(Pfn(u64::MAX).physical_address(), None);
    }

    /// Deliberate deviation from the review's wording, which asked for both
    /// "2^52 - 1 returns the right address" AND "the sign bit is rejected":
    /// (2^52 - 1) * 4096 == 0xFFFF_FFFF_FFFF_F000, whose bit 63 IS set, so the
    /// two rules cannot both hold for that input. The sign rule wins, because
    /// the value's only consumer is an i64 QuadPart handed to MmMapIoSpace.
    #[test]
    fn pfn_physical_address_rejects_the_sign_bit() {
        assert_eq!(Pfn((1 << 52) - 1).physical_address(), None);
        assert_eq!(Pfn(1 << 51).physical_address(), None);
    }

    #[test]
    fn meta_layout_accepts_only_the_two_real_layouts() {
        assert_eq!(MetaLayout::try_from(24), Ok(MetaLayout::Legacy24));
        assert_eq!(MetaLayout::try_from(48), Ok(MetaLayout::Full48));
        // 96 is what both live writers emit (48 prefix + 48 trailer).
        assert_eq!(MetaLayout::try_from(96), Ok(MetaLayout::Full48));
        assert_eq!(MetaLayout::try_from(usize::MAX), Ok(MetaLayout::Full48));
    }

    /// The whole point: a length that lands mid-field is refused, not truncated
    /// into a plausible-looking wrong value.
    #[test]
    fn meta_layout_rejects_partial_trailers() {
        for len in [0usize, 1, 8, 16, 23, 25, 30, 32, 40, 47] {
            assert_eq!(MetaLayout::try_from(len), Err(()), "len {len}");
        }
    }

    #[test]
    fn meta_layout_copy_length_comes_from_the_layout() {
        assert_eq!(MetaLayout::Legacy24.copy_bytes(), 24);
        assert_eq!(MetaLayout::Full48.copy_bytes(), 48);
        // A longer buffer copies the full layout, never `len`.
        assert_eq!(
            MetaLayout::from_trailer_len(96).unwrap().copy_bytes(),
            MetaLayout::FULL_BYTES
        );
    }

    #[test]
    fn seq_read_accepts_only_an_even_unchanged_sequence() {
        assert_eq!(seq_read(0, 0), SeqRead::Stable);
        assert_eq!(seq_read(2, 2), SeqRead::Stable);
        assert_eq!(seq_read(u32::MAX - 1, u32::MAX - 1), SeqRead::Stable);
    }

    #[test]
    fn seq_read_retries_on_an_in_flight_or_landed_publish() {
        // Writer held the descriptor when the read started.
        assert_eq!(seq_read(1, 1), SeqRead::Retry);
        assert_eq!(seq_read(3, 4), SeqRead::Retry);
        // A publish landed during the read (the republish tear this fixes).
        assert_eq!(seq_read(2, 4), SeqRead::Retry);
        assert_eq!(seq_read(2, 3), SeqRead::Retry);
        // Wrap is still a change.
        assert_eq!(seq_read(u32::MAX - 1, 0), SeqRead::Retry);
    }

    /// A simulated writer that never stops must exhaust the bound rather than
    /// spin: the reader is PASSIVE, the publishers can be at raised IRQL.
    #[test]
    fn seq_read_bound_terminates_against_a_live_writer() {
        let mut attempts = 0;
        let mut settled = false;
        while attempts < SEQ_READ_ATTEMPTS {
            attempts += 1;
            // Always odd => always Retry.
            if seq_read(2 * attempts + 1, 2 * attempts + 1) == SeqRead::Stable {
                settled = true;
                break;
            }
        }
        assert!(!settled);
        assert_eq!(attempts, SEQ_READ_ATTEMPTS);
    }

    /// Both moved functions are `const fn`, so a future edit that reaches for
    /// runtime state stops compiling here rather than in `kmd_render`.
    #[test]
    fn both_helpers_are_const_evaluable() {
        const PITCH: u32 = cross_adapter_pitch(1896);
        const PAGES: u64 = round_up_page(4097);
        assert_eq!(PITCH, 7680);
        assert_eq!(PAGES, 8192);
    }
}
