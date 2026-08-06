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

/// Fixed-phase scheduling for the synthetic 60 Hz CRTC heartbeat.
///
/// `DXGK_VIDPN_SOURCE_MODE` advertises 60/1 Hz, so a 16 ms recurring timer is
/// not an approximation we may expose as the same mode: it runs at 62.5 Hz.
/// The KMD uses a one-shot timer and advances this absolute interrupt-time
/// deadline instead. Advancing from the preceding deadline (rather than from
/// `now`) prevents ordinary DPC latency from accumulating as phase drift; a
/// genuinely late DPC skips directly to the first future deadline instead of
/// generating a catch-up burst.
pub mod vsync_deadline {
    /// 16.6667 ms in the kernel timer's 100 ns units. This is 60 Hz to within
    /// 0.0002%, versus the old 16 ms period's 4.17% error.
    pub const PERIOD_100NS: u64 = 166_667;

    /// Return the first fixed-phase deadline strictly after `now`.
    ///
    /// `previous` is the deadline which just fired (or `now` when arming the
    /// first tick). `None` is terminal: the interrupt-time representation is
    /// exhausted, so the KMD must leave the one-shot timer unarmed rather than
    /// turn a saturated deadline into an immediate rearm storm.
    pub const fn next(previous: u64, now: u64) -> Option<u64> {
        let Some(candidate) = previous.checked_add(PERIOD_100NS) else {
            return None;
        };
        if candidate > now {
            return Some(candidate);
        }

        let late = now.saturating_sub(candidate);
        let intervals = late / PERIOD_100NS + 1;
        let Some(advance) = intervals.checked_mul(PERIOD_100NS) else {
            return None;
        };
        candidate.checked_add(advance)
    }

    /// Convert a future interrupt-time deadline to a negative relative
    /// `LARGE_INTEGER` due time. A raced/equal deadline is rearmed one 100 ns
    /// unit ahead, never as zero (which has absolute-time semantics).
    pub const fn relative_due(deadline: u64, now: u64) -> i64 {
        let delta = deadline.saturating_sub(now);
        let nonzero = if delta == 0 { 1 } else { delta };
        let bounded = if nonzero > i64::MAX as u64 {
            i64::MAX as u64
        } else {
            nonzero
        };
        -(bounded as i64)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn initial_arm_is_one_sixtieth_second() {
            assert_eq!(next(10_000_000, 10_000_000), Some(10_166_667));
        }

        #[test]
        fn ordinary_lateness_does_not_accumulate_phase_drift() {
            let fired = 1_166_667;
            assert_eq!(next(fired, fired + 4_000), Some(1_333_334));
        }

        #[test]
        fn missed_ticks_skip_to_one_future_deadline_without_burst() {
            let fired = 1_166_667;
            let now = fired + PERIOD_100NS * 4 + 7;
            let Some(deadline) = next(fired, now) else {
                panic!("a representable deadline must remain armable");
            };
            assert!(deadline > now);
            assert_eq!(deadline, fired + PERIOD_100NS * 5);
        }

        #[test]
        fn relative_due_is_always_negative_and_exact() {
            assert_eq!(relative_due(166_667, 0), -166_667);
            assert_eq!(relative_due(42, 42), -1);
            assert_eq!(relative_due(41, 42), -1);
        }

        #[test]
        fn deadline_overflow_is_terminal_not_an_immediate_rearm() {
            assert_eq!(next(u64::MAX - PERIOD_100NS + 1, 0), None);
            assert_eq!(next(u64::MAX - PERIOD_100NS, u64::MAX), None);
        }
    }
}

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

/// Smallest scan-out extent Helios will adopt from the host's `GET_DISPLAY_INFO`.
///
/// One named constant for what used to be two bare literals re-evaluated on
/// every `display_mode()` call.
pub const MIN_DISPLAY_WIDTH: u32 = 320;
pub const MIN_DISPLAY_HEIGHT: u32 = 240;

/// A scan-out extent that has passed the minimum-size check exactly once.
///
/// The mode used to be two unvalidated `u32`s whose validation re-ran on every
/// read through bare literals. Making it a constructed value means there is no
/// way to observe an unvalidated `(0, 0)` mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayMode {
    width: core::num::NonZeroU32,
    height: core::num::NonZeroU32,
}

impl DisplayMode {
    /// Adopt the host-reported extent, or `None` if it is unusable.
    pub const fn from_host(width: u32, height: u32) -> Option<Self> {
        if width < MIN_DISPLAY_WIDTH || height < MIN_DISPLAY_HEIGHT {
            return None;
        }
        match (
            core::num::NonZeroU32::new(width),
            core::num::NonZeroU32::new(height),
        ) {
            (Some(width), Some(height)) => Some(Self { width, height }),
            // Unreachable: both are >= the minimums above. Written as a match
            // rather than `unwrap` because a panic in a DDI is a silent graphics
            // deadlock.
            _ => None,
        }
    }

    pub const fn width(self) -> u32 {
        self.width.get()
    }

    pub const fn height(self) -> u32 {
        self.height.get()
    }

    /// The `(w << 16) | h` form the `DspMd` breadcrumb reports.
    pub const fn packed(self) -> u32 {
        (self.width.get() << 16) | (self.height.get() & 0xFFFF)
    }

    /// The extent used when the host reports nothing usable.
    ///
    /// Written as a total `match` rather than an `unwrap` or a const `panic!`:
    /// a panicking expression has no place in a crate the kernel driver links,
    /// even one that could only fire at compile time. The `None` arms are
    /// unreachable for the nonzero constants below, and
    /// `display_mode_fallback_is_the_documented_extent` asserts that — so an
    /// edit that broke it fails a host test instead of silently degrading the
    /// fallback to the minimum extent.
    pub const FALLBACK: Self = Self {
        width: match core::num::NonZeroU32::new(FALLBACK_DISPLAY_WIDTH) {
            Some(w) => w,
            None => core::num::NonZeroU32::MIN,
        },
        height: match core::num::NonZeroU32::new(FALLBACK_DISPLAY_HEIGHT) {
            Some(h) => h,
            None => core::num::NonZeroU32::MIN,
        },
    };
}

/// The mode Helios advertises when the host's `GET_DISPLAY_INFO` reported no
/// usable scanout-0 size. Mirrored by `ddi::vidpn::DEFAULT_MODE_*`.
pub const FALLBACK_DISPLAY_WIDTH: u32 = 1920;
pub const FALLBACK_DISPLAY_HEIGHT: u32 = 1080;

impl From<DisplayMode> for (u32, u32) {
    fn from(mode: DisplayMode) -> Self {
        (mode.width(), mode.height())
    }
}

/// Pack the VidPn programming gate: generation in the high 32 bits, the active
/// flag in the low 32.
///
/// The gate used to be a bare `AtomicU32` flag, so nothing identified WHICH
/// programming interval a completion belonged to. Because the DIRQL half of
/// `SetVidPnSourceAddress` takes no lock, a second call can raise the gate for
/// interval N+1 while copy N is still outstanding; copy N's completion then
/// cleared the gate belonging to N+1. Pairing the flag with its generation in
/// ONE word makes raise and "clear only my interval" single atomic operations.
///
/// The low half keeps the exact 0/1 meaning the flag had, so every existing
/// breadcrumb that reports the gate still reports 0 or 1.
pub const fn gate_pack(seq: u32, active: bool) -> u64 {
    ((seq as u64) << 32) | (active as u64)
}

/// The generation half of a packed gate word.
pub const fn gate_seq(word: u64) -> u32 {
    (word >> 32) as u32
}

/// The active half of a packed gate word — what the VSync DPC tests.
pub const fn gate_active(word: u64) -> bool {
    (word & 0xFFFF_FFFF) != 0
}

/// The scan-out surface formats Helios can put on virtio-gpu scanout 0.
///
/// This is the one place the DXGI-to-wire mapping is written down. It used to be
/// spelled four times with bare integers — `display.rs`'s `virtio_scanout_format`
/// match, a function-local `const DXGI_FORMAT_B8G8R8A8_UNORM: u32 = 87` used
/// twice, the `matches!(dxgi_format, 28 | 87 | 88)` direct-scan-out allowlist,
/// and three more local consts plus a three-way `!=` chain gating the copy path
/// in `create_allocation.rs`. A format added to three of those four surfaces as
/// an opaque `ScSet=0xE3` or `ScFmt` trace.
///
/// The virtio values are literals rather than a `helios_protocol` import because
/// this crate's whole contract is that it has no dependency edge (see the
/// Cargo.toml comment); `kmd_render` pins them to `helios_protocol` with a
/// compile-time assertion, so a drift is a build failure, not a runtime bug.
///
/// NOT merged in, on purpose, because they answer different questions:
/// `create_allocation::resolved_dxgi_format` (D3DDDIFORMAT -> DXGI),
/// `venus::PresentPixelFormat::from_dxgi` (the wider render-side set), and
/// `scanout_diag::scanout_format` (keyed on the *diag mode*, returning a virtio
/// constant directly — it is not a fourth DXGI mapping).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanoutFormat {
    /// DXGI 88 `B8G8R8X8_UNORM` -> virtio 2.
    Bgrx8,
    /// DXGI 87 `B8G8R8A8_UNORM` -> virtio 1.
    Bgra8,
    /// DXGI 28 `R8G8B8A8_UNORM` -> virtio 67.
    Rgba8,
}

impl ScanoutFormat {
    /// The DXGI formats a scan-out surface may *declare*.
    ///
    /// This is the strict set — exactly the three the direct-scan-out validator
    /// and the copy-path gate accept today. DXGI 0 is deliberately NOT here; see
    /// [`Self::from_dxgi_or_legacy_zero`].
    pub const fn from_dxgi(dxgi: u32) -> Option<Self> {
        match dxgi {
            28 => Some(Self::Rgba8),
            87 => Some(Self::Bgra8),
            88 => Some(Self::Bgrx8),
            _ => None,
        }
    }

    /// [`Self::from_dxgi`] plus the legacy `0 -> Bgrx8` arm.
    ///
    /// The wire-format converter has always accepted DXGI 0 while both
    /// validators reject it, so collapsing all four sites onto one acceptance
    /// set would have *changed which formats are accepted*. That divergence is
    /// preserved verbatim here and named, so it is greppable instead of latent:
    /// only the converter calls this, and its `0` arm has no reachable producer
    /// today (a direct-scan-out primary always carries the UMD's exact DXGI
    /// format from the same private-data record, and the LINEAR arm's format is
    /// [`Self::Bgra8`] by construction). Deleting the arm is a separate,
    /// counter-backed commit.
    pub const fn from_dxgi_or_legacy_zero(dxgi: u32) -> Option<Self> {
        if dxgi == 0 {
            Some(Self::Bgrx8)
        } else {
            Self::from_dxgi(dxgi)
        }
    }

    /// The `VIRTIO_GPU_FORMAT_*` value for `SET_SCANOUT_BLOB`.
    pub const fn virtio(self) -> u32 {
        match self {
            Self::Bgra8 => 1,  // VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM
            Self::Bgrx8 => 2,  // VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM
            Self::Rgba8 => 67, // VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM
        }
    }

    /// The canonical DXGI format value.
    ///
    /// Note this is not a round trip for the legacy zero arm:
    /// `from_dxgi_or_legacy_zero(0).unwrap().dxgi() == 88`. That is the existing
    /// behaviour — the converter maps 0 and 88 onto the same wire format.
    pub const fn dxgi(self) -> u32 {
        match self {
            Self::Bgrx8 => 88,
            Self::Bgra8 => 87,
            Self::Rgba8 => 28,
        }
    }
}

/// Maximum venus command stream built for any single direct/ring command.
///
/// The largest encoder in the KMD is `vkCreateDevice` at its `EXT_FULL` tier,
/// which is **332 bytes**: 144 bytes of struct plus a 188-byte extension block
/// (five 8-byte string lengths plus 24+28+28+32+36 bytes of NUL-terminated,
/// 4-byte-padded names). That leaves 180 bytes of slack — enough for one more
/// extension string, not for four, and not for any encoder that grows by a
/// `pNext` chain, a multi-region `vkCmdCopyImage` or a queue-family index array.
///
/// The number is asserted by `writer_ext_full_create_device_is_332_bytes` below
/// rather than by a comment: the previous comment claimed "the largest is
/// `vkCreateDevice` (~120 bytes)" and was wrong by 212 bytes, which mattered
/// because until [`Writer`] gained a checked API, overrunning this buffer was a
/// slice-index panic — i.e. a `KeBugCheck` inside a DDI.
pub const MAX_CMD_BYTES: usize = 512;

/// Fixed-capacity little-endian writer for venus command streams.
///
/// All venus scalars are 4-byte aligned in the stream; `size_t` / `VkDeviceSize`
/// / handle / array_size are 8 bytes, and `u32` / `VkResult` / `VkStructureType`
/// / `VkFlags` / `VkCommandTypeEXT` are 4 bytes.
///
/// Overflow is **sticky and non-panicking**: a write that would exceed
/// [`MAX_CMD_BYTES`] is dropped, the writer is poisoned, and [`Writer::finished`]
/// returns `None` forever after. The caller turns that into a refusal with a
/// named counter. Every write method is infallible so the ~40 encoder bodies
/// stay linear; the single fallible point is where the bytes are handed out.
pub struct Writer {
    buf: [u8; MAX_CMD_BYTES],
    len: usize,
    overflow: bool,
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl Writer {
    pub const fn new() -> Self {
        Self {
            buf: [0u8; MAX_CMD_BYTES],
            len: 0,
            overflow: false,
        }
    }

    /// Reserve `n` bytes, or poison the writer and report that there is no room.
    fn reserve(&mut self, n: usize) -> bool {
        if self.overflow || self.len + n > MAX_CMD_BYTES {
            self.overflow = true;
            return false;
        }
        true
    }

    pub fn u32(&mut self, v: u32) {
        if !self.reserve(4) {
            return;
        }
        self.buf[self.len..self.len + 4].copy_from_slice(&v.to_le_bytes());
        self.len += 4;
    }

    pub fn i32(&mut self, v: i32) {
        self.u32(v as u32);
    }

    pub fn u64(&mut self, v: u64) {
        if !self.reserve(8) {
            return;
        }
        self.buf[self.len..self.len + 8].copy_from_slice(&v.to_le_bytes());
        self.len += 8;
    }

    /// A f32 priority value (encoded as its IEEE-754 bits).
    pub fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }

    /// `vn_encode_simple_pointer` / `vn_encode_array_size`: a u64 count (1
    /// present, 0 absent / empty array).
    pub fn count(&mut self, present: bool) {
        self.u64(if present { 1 } else { 0 });
    }

    /// Copy `bytes` and zero-fill to the next 4-byte boundary.
    pub fn bytes_padded(&mut self, bytes: &[u8]) {
        let padded = (bytes.len() + 3) & !3;
        if !self.reserve(padded) {
            return;
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        for i in bytes.len()..padded {
            self.buf[self.len + i] = 0;
        }
        self.len += padded;
    }

    /// A Vulkan object handle. Identical bytes to [`Writer::u64`]; the separate
    /// name exists so the KMD's per-class handle newtypes can be written without
    /// spelling out a conversion at every encoder, and so a reader can see at a
    /// glance which 8-byte words in a stream are handles.
    pub fn handle<H: Into<u64>>(&mut self, h: H) {
        self.u64(h.into());
    }

    /// The command header: `VkCommandTypeEXT | VkCommandFlagsEXT`.
    pub fn header(&mut self, cmd_type: u32, flags: u32) {
        self.u32(cmd_type);
        self.u32(flags);
    }

    /// Bytes written so far. Meaningless once [`Writer::overflowed`] is set.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// True once any write has been refused for want of room.
    pub fn overflowed(&self) -> bool {
        self.overflow
    }

    /// The `VkCommandTypeEXT` this stream opened with — stream word 0, read back
    /// so an overflow refusal can name the command that caused it. Reads 0 if the
    /// header was never written.
    pub fn cmd_type(&self) -> u32 {
        if self.len < 4 {
            return 0;
        }
        u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
    }

    /// The encoded stream, or `None` if any write overflowed.
    ///
    /// This is the only way to get the bytes out, so a stream that did not fit
    /// cannot be submitted: the `Option` replaces what used to be a slice-index
    /// panic on the first over-long write.
    pub fn finished(&self) -> Option<&[u8]> {
        (!self.overflow).then(|| &self.buf[..self.len])
    }
}

// ── Venus VkImageCreateInfo / VkMemoryAllocateInfo encoding ───────────────────
//
// R1002. Three image encoders and five memory encoders each re-emitted the whole
// struct body inline — about 25 `Writer` calls for an image — differing only in
// the pNext chain and a handful of scalars. Two of the three image bodies wrote
// `w.u32(0); // VK_IMAGE_TILING_OPTIMAL` as a BARE LITERAL while the third used
// the named `IMAGE_TILING_LINEAR`: the exact shape of the 39th-session defect (a
// tiling scalar hand-written in one of several copies of one struct), only
// inverted. That session's root cause was `IMAGE_TILING_LINEAR` being defined as
// 0, i.e. OPTIMAL, and it painted black.
//
// The pNext encoding is order-sensitive in a way no reader can infer from a call
// site: for the export-plus-dedicated allocation, the DEDICATED struct's fields
// are written inline first and the EXPORT struct's own `handleTypes` field comes
// AFTER them, because export's pNext points at dedicated. Getting that backwards
// produces a stream the host decodes as a different allocation.
//
// These live in `kmd_logic`, not in `venus.rs`, for one reason: they can then be
// pinned by golden-byte tests on the Linux host. The literals in those tests were
// produced by compiling the PRE-CHANGE inline sequences and printing their
// output, so they are an equivalence proof against the old code rather than a
// restatement of the new.

/// `VkCommandTypeEXT` for `vkAllocateMemory`.
pub const CMD_ALLOCATE_MEMORY: u32 = 21;
/// `VkCommandTypeEXT` for `vkCreateImage`.
pub const CMD_CREATE_IMAGE: u32 = 54;
/// `VK_COMMAND_GENERATE_REPLY_BIT_EXT`.
pub const CMD_FLAG_GENERATE_REPLY: u32 = 0x1;

pub const ST_MEMORY_ALLOCATE_INFO: i32 = 5;
pub const ST_IMAGE_CREATE_INFO: i32 = 14;
pub const ST_EXTERNAL_MEMORY_IMAGE_CREATE_INFO: i32 = 1000072001;
pub const ST_EXPORT_MEMORY_ALLOCATE_INFO: i32 = 1000072002;
pub const ST_MEMORY_DEDICATED_ALLOCATE_INFO: i32 = 1000127001;
pub const ST_IMPORT_MEMORY_RESOURCE_INFO_MESA: i32 = 1000384002;

pub const IMAGE_TYPE_2D: u32 = 1;
pub const SAMPLE_COUNT_1: u32 = 0x0000_0001;
pub const SHARING_MODE_EXCLUSIVE: u32 = 0;

/// `VK_IMAGE_TILING_OPTIMAL`.
///
/// ⚠ Introducing this is half the point of R1002. It was a bare `w.u32(0)` with
/// a trailing comment at two of the three image encoders, while its sibling
/// `IMAGE_TILING_LINEAR` was a named constant — the 39th session's defect shape
/// inverted. There are now no bare tiling literals anywhere.
pub const IMAGE_TILING_OPTIMAL: u32 = 0;
/// `VK_IMAGE_TILING_LINEAR`.
///
/// ⚠ THE 39TH SESSION'S ROOT CAUSE. This was defined as 0 — which is OPTIMAL —
/// and the host built a tiled image the display importer read as linear: a black
/// screen with no error anywhere. It is 1. Do not "simplify" it.
pub const IMAGE_TILING_LINEAR: u32 = 1;

/// Which pNext chain a `VkImageCreateInfo` carries.
///
/// An exhaustive enum rather than an ad-hoc `count(true)/i32(ST_…)` sequence, so
/// the nesting order lives in exactly one place and an unrepresentable chain
/// cannot be encoded.
///
/// The `ExternalMemoryWithModifierList` variant the review specifies is NOT
/// here: `ST_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO`, `DRM_FORMAT_MOD_LINEAR`
/// and `IMAGE_TILING_DRM_FORMAT_MODIFIER` all went with T6/R906 when the modifier
/// path was deleted, so it would have zero users.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ImagePNext {
    /// An internal image with no external-memory contract.
    None,
    /// `VkExternalMemoryImageCreateInfo` with the given `VkExternalMemoryHandleTypeFlags`.
    ExternalMemory { handle_type: u32 },
}

/// Everything the three image creates differ by. The rest of
/// `VkImageCreateInfo` — 2D, depth 1, one mip, one layer, 1 sample, exclusive
/// sharing, no queue families — is fixed by [`encode_image_create`].
#[derive(Clone, Copy)]
pub struct ImageCreateSpec {
    pub pnext: ImagePNext,
    pub flags: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub tiling: u32,
    pub usage: u32,
    pub initial_layout: u32,
}

/// Encode one `vkCreateImage` command stream.
pub fn encode_image_create(device_id: u64, image_id: u64, spec: &ImageCreateSpec) -> Writer {
    let mut w = Writer::new();
    w.header(CMD_CREATE_IMAGE, CMD_FLAG_GENERATE_REPLY);
    w.u64(device_id);
    w.count(true);
    w.i32(ST_IMAGE_CREATE_INFO);
    match spec.pnext {
        ImagePNext::None => w.count(false),
        ImagePNext::ExternalMemory { handle_type } => {
            w.count(true);
            w.i32(ST_EXTERNAL_MEMORY_IMAGE_CREATE_INFO);
            w.count(false);
            w.u32(handle_type);
        }
    }
    w.u32(spec.flags);
    w.u32(IMAGE_TYPE_2D);
    w.u32(spec.format);
    w.u32(spec.width);
    w.u32(spec.height);
    w.u32(1); // depth
    w.u32(1); // mipLevels
    w.u32(1); // arrayLayers
    w.u32(SAMPLE_COUNT_1);
    w.u32(spec.tiling);
    w.u32(spec.usage);
    w.u32(SHARING_MODE_EXCLUSIVE);
    w.u32(0); // queueFamilyIndexCount
    w.count(false); // pQueueFamilyIndices
    w.u32(spec.initial_layout);
    w.count(false); // pAllocator
    w.count(true);
    w.u64(image_id);
    w
}

/// Which pNext chain a `VkMemoryAllocateInfo` carries.
///
/// ⚠ `ExportDedicated` is the order-sensitive one: the dedicated struct is
/// nested INSIDE the export struct's pNext, so its `image`/`buffer` fields are
/// written before the export struct's own `handleTypes`. That ordering is why
/// this is an enum and not four hand-written sequences.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MemoryPNext {
    /// Plain host-visible allocation.
    None,
    /// `VkExportMemoryAllocateInfo`.
    Export { handle_type: u32 },
    /// `VkMemoryDedicatedAllocateInfo` for an image.
    Dedicated { image: u64 },
    /// `VkExportMemoryAllocateInfo` -> `VkMemoryDedicatedAllocateInfo`.
    ExportDedicated { handle_type: u32, image: u64 },
    /// `VkImportMemoryResourceInfoMESA` — adopt an existing virtio resource.
    ImportResource { resource_id: u32 },
}

/// Everything the five memory allocations differ by.
#[derive(Clone, Copy)]
pub struct MemoryAllocateSpec {
    pub pnext: MemoryPNext,
    pub size: u64,
    pub memory_type_index: u32,
}

/// Encode one `vkAllocateMemory` command stream.
pub fn encode_memory_allocate(device_id: u64, memory_id: u64, spec: &MemoryAllocateSpec) -> Writer {
    let mut w = Writer::new();
    w.header(CMD_ALLOCATE_MEMORY, CMD_FLAG_GENERATE_REPLY);
    w.u64(device_id);
    w.count(true);
    w.i32(ST_MEMORY_ALLOCATE_INFO);
    match spec.pnext {
        MemoryPNext::None => w.count(false),
        MemoryPNext::Export { handle_type } => {
            w.count(true);
            w.i32(ST_EXPORT_MEMORY_ALLOCATE_INFO);
            w.count(false);
            w.u32(handle_type);
        }
        MemoryPNext::Dedicated { image } => {
            w.count(true);
            w.i32(ST_MEMORY_DEDICATED_ALLOCATE_INFO);
            w.count(false);
            w.u64(image);
            w.u64(0); // buffer
        }
        MemoryPNext::ExportDedicated { handle_type, image } => {
            w.count(true);
            w.i32(ST_EXPORT_MEMORY_ALLOCATE_INFO);
            w.count(true);
            w.i32(ST_MEMORY_DEDICATED_ALLOCATE_INFO);
            w.count(false);
            w.u64(image);
            w.u64(0); // buffer
                      // The EXPORT struct's own field, after the nested dedicated one.
            w.u32(handle_type);
        }
        MemoryPNext::ImportResource { resource_id } => {
            w.count(true);
            w.i32(ST_IMPORT_MEMORY_RESOURCE_INFO_MESA);
            w.count(false);
            w.u32(resource_id);
        }
    }
    w.u64(spec.size);
    w.u32(spec.memory_type_index);
    w.count(false); // pAllocator
    w.count(true);
    w.u64(memory_id);
    w
}

// ── Vulkan memory-type selection ──────────────────────────────────────────────

/// `VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT`.
pub const MEMORY_PROPERTY_DEVICE_LOCAL: u32 = 0x1;
/// `VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT`.
pub const MEMORY_PROPERTY_HOST_VISIBLE: u32 = 0x2;
/// `VK_MEMORY_PROPERTY_HOST_COHERENT_BIT`.
pub const MEMORY_PROPERTY_HOST_COHERENT: u32 = 0x4;
/// `VK_MAX_MEMORY_TYPES` — the fixed array length the host encodes in
/// `vkGetPhysicalDeviceMemoryProperties`.
pub const VK_MAX_MEMORY_TYPES: u32 = 32;

/// Which memory type a selector settled on, and whether it is the one that was
/// actually asked for.
///
/// Both selectors fall back rather than fail, which is the right policy — a
/// downgraded allocation usually still works. What was wrong is that they
/// returned a bare `Option<u32>`, so every caller read `Some(i)` as "the
/// requested property was satisfied" and a downgrade left no trace anywhere.
/// That is the ScanoutDiag=16 / SdgErr=2 defect class: a memory-type choice that
/// looked fine and was not.
///
/// `#[must_use]` plus two arms means a caller has to say what it does about the
/// downgrade. It does not prevent choosing a downgraded type — it prevents doing
/// so silently.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTypeChoice {
    /// Every requested property is present on this type.
    Exact(u32),
    /// The type is allowed by `memory_type_bits` but is missing at least one
    /// requested property.
    Downgraded(u32),
}

impl MemoryTypeChoice {
    /// The chosen `memoryTypeIndex`, whichever arm it came from.
    pub fn index(self) -> u32 {
        match self {
            Self::Exact(i) | Self::Downgraded(i) => i,
        }
    }
}

/// Pick a HOST_VISIBLE memory type, preferring one that is also HOST_COHERENT.
///
/// `Downgraded` means HOST_VISIBLE but NOT HOST_COHERENT, which for a MAPPABLE
/// scanout blob means the guest's writes need explicit flushes that nothing
/// issues.
pub fn choose_host_visible_memory_type(
    memory_type_flags: &[u32],
    memory_type_count: u32,
    memory_type_bits: u32,
) -> Option<MemoryTypeChoice> {
    let mut fallback = None;
    let mut i = 0;
    while i < memory_type_count && i < VK_MAX_MEMORY_TYPES && (i as usize) < memory_type_flags.len()
    {
        if (memory_type_bits & (1u32 << i)) != 0 {
            let flags = memory_type_flags[i as usize];
            if (flags & MEMORY_PROPERTY_HOST_VISIBLE) != 0 {
                if fallback.is_none() {
                    fallback = Some(i);
                }
                if (flags & MEMORY_PROPERTY_HOST_COHERENT) != 0 {
                    return Some(MemoryTypeChoice::Exact(i));
                }
            }
        }
        i += 1;
    }
    fallback.map(MemoryTypeChoice::Downgraded)
}

/// Pick a DEVICE_LOCAL memory type, in three tiers.
///
/// The tier order is load-bearing and must not be reordered:
/// 1. device-local and NOT host-visible — the real VRAM type;
/// 2. device-local (host-visible too, i.e. a BAR/ReBAR type);
/// 3. the first allowed type at all, device-local or not.
///
/// Tiers 1 and 2 are both `Exact`: the requested property is DEVICE_LOCAL and
/// both have it, so tier 1 is a preference inside the same answer, not a
/// downgrade. Tier 3 is the downgrade, and it is just the lowest set bit of
/// `memory_type_bits`: on a host whose `memoryTypeBits` for an OPTIMAL GDI image
/// contains only a host-visible type, the old signature reported success and the
/// "device-local dedicated memory" contract in the caller's own doc comment was
/// silently false.
pub fn choose_device_local_memory_type(
    memory_type_flags: &[u32],
    memory_type_count: u32,
    memory_type_bits: u32,
) -> Option<MemoryTypeChoice> {
    let limit = |i: u32| {
        i < memory_type_count && i < VK_MAX_MEMORY_TYPES && (i as usize) < memory_type_flags.len()
    };
    let mut fallback = None;
    let mut i = 0;
    while limit(i) {
        if (memory_type_bits & (1u32 << i)) != 0 {
            let flags = memory_type_flags[i as usize];
            if fallback.is_none() {
                fallback = Some(i);
            }
            if (flags & MEMORY_PROPERTY_DEVICE_LOCAL) != 0
                && (flags & MEMORY_PROPERTY_HOST_VISIBLE) == 0
            {
                return Some(MemoryTypeChoice::Exact(i));
            }
        }
        i += 1;
    }
    i = 0;
    while limit(i) {
        if (memory_type_bits & (1u32 << i)) != 0 {
            let flags = memory_type_flags[i as usize];
            if (flags & MEMORY_PROPERTY_DEVICE_LOCAL) != 0 {
                return Some(MemoryTypeChoice::Exact(i));
            }
        }
        i += 1;
    }
    fallback.map(MemoryTypeChoice::Downgraded)
}

#[cfg(test)]
mod tests {

    // ── R1002 golden bytes ────────────────────────────────────────────────
    //
    // Produced by compiling the PRE-CHANGE inline encoder sequences against
    // this same `Writer` and printing their output, so these literals are an
    // equivalence proof against the old code rather than a restatement of the
    // new. Regenerating them from the current encoder would make the tests
    // circular and worthless.
    //
    // Fixed handles and geometry so the arrays are stable: device
    // 0x1111222233334444, image 0x5555666677778888, memory 0x9999AAAABBBBCCCC,
    // 1896x1030 (the live DWM primary), size 0x800000, memoryTypeIndex 7.
    const GOLD_DEVICE: u64 = 0x1111_2222_3333_4444;
    const GOLD_IMAGE: u64 = 0x5555_6666_7777_8888;
    const GOLD_MEMORY: u64 = 0x9999_AAAA_BBBB_CCCC;
    const GOLD_SIZE: u64 = 0x0080_0000;
    const GOLD_MTI: u32 = 7;

    // GOLDEN_LINEAR_SCANOUT_IMAGE (140 bytes)
    const GOLDEN_LINEAR_SCANOUT_IMAGE: &[u8] = &[
        0x36, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x44, 0x44, 0x33, 0x33, 0x22, 0x22, 0x11,
        0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x41, 0xe3, 0x9b, 0x3b, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x2c, 0x00, 0x00, 0x00, 0x68, 0x07, 0x00, 0x00, 0x06, 0x04, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x88, 0x88, 0x77,
        0x77, 0x66, 0x66, 0x55, 0x55,
    ];
    // GOLDEN_OPTIMAL_PRESENT_IMAGE_ALIAS (140 bytes)
    const GOLDEN_OPTIMAL_PRESENT_IMAGE_ALIAS: &[u8] = &[
        0x36, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x44, 0x44, 0x33, 0x33, 0x22, 0x22, 0x11,
        0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x41, 0xe3, 0x9b, 0x3b, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x2c, 0x00, 0x00, 0x00, 0x68, 0x07, 0x00, 0x00, 0x06, 0x04, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x17, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x88, 0x88, 0x77,
        0x77, 0x66, 0x66, 0x55, 0x55,
    ];
    // GOLDEN_PRESENT_CONVERSION_IMAGE (124 bytes)
    const GOLDEN_PRESENT_CONVERSION_IMAGE: &[u8] = &[
        0x36, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x44, 0x44, 0x33, 0x33, 0x22, 0x22, 0x11,
        0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x2c,
        0x00, 0x00, 0x00, 0x68, 0x07, 0x00, 0x00, 0x06, 0x04, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x88, 0x88, 0x77, 0x77,
        0x66, 0x66, 0x55, 0x55,
    ];
    // GOLDEN_MEMORY_PLAIN (72 bytes)
    const GOLDEN_MEMORY_PLAIN: &[u8] = &[
        0x15, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x44, 0x44, 0x33, 0x33, 0x22, 0x22, 0x11,
        0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0xcc, 0xcc, 0xbb, 0xbb, 0xaa, 0xaa, 0x99, 0x99,
    ];
    // GOLDEN_MEMORY_EXPORT (88 bytes)
    const GOLDEN_MEMORY_EXPORT: &[u8] = &[
        0x15, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x44, 0x44, 0x33, 0x33, 0x22, 0x22, 0x11,
        0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x42, 0xe3, 0x9b, 0x3b, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0xcc, 0xcc, 0xbb, 0xbb, 0xaa, 0xaa, 0x99, 0x99,
    ];
    // GOLDEN_MEMORY_DEDICATED (100 bytes)
    const GOLDEN_MEMORY_DEDICATED: &[u8] = &[
        0x15, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x44, 0x44, 0x33, 0x33, 0x22, 0x22, 0x11,
        0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x19, 0xba, 0x9c, 0x3b, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x88, 0x88, 0x77, 0x77, 0x66, 0x66, 0x55, 0x55, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0xcc, 0xcc, 0xbb, 0xbb, 0xaa, 0xaa, 0x99, 0x99,
    ];
    // GOLDEN_MEMORY_EXPORT_DEDICATED (116 bytes)
    const GOLDEN_MEMORY_EXPORT_DEDICATED: &[u8] = &[
        0x15, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x44, 0x44, 0x33, 0x33, 0x22, 0x22, 0x11,
        0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x42, 0xe3, 0x9b, 0x3b, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x19, 0xba, 0x9c, 0x3b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x88, 0x88, 0x77, 0x77, 0x66, 0x66, 0x55, 0x55, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0xcc, 0xcc, 0xbb, 0xbb, 0xaa, 0xaa, 0x99, 0x99,
    ];
    // GOLDEN_MEMORY_IMPORT_RESOURCE (88 bytes)
    const GOLDEN_MEMORY_IMPORT_RESOURCE: &[u8] = &[
        0x15, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x44, 0x44, 0x33, 0x33, 0x22, 0x22, 0x11,
        0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xa6, 0xa0, 0x3b, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x34, 0x12, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0xcc, 0xcc, 0xbb, 0xbb, 0xaa, 0xaa, 0x99, 0x99,
    ];

    /// The production LINEAR scan-out image. The frozen direct-primary path:
    /// wrong bytes here are a black desktop, which is how the 39th session
    /// started.
    #[test]
    fn linear_scanout_image_bytes_are_unchanged() {
        let w = encode_image_create(
            GOLD_DEVICE,
            GOLD_IMAGE,
            &ImageCreateSpec {
                pnext: ImagePNext::ExternalMemory {
                    handle_type: 0x0000_0200,
                },
                flags: 0,
                format: 44, // VK_FORMAT_B8G8R8A8_UNORM
                width: 1896,
                height: 1030,
                tiling: IMAGE_TILING_LINEAR,
                usage: 0x1 | 0x2,
                initial_layout: 8, // PREINITIALIZED
            },
        );
        assert_eq!(w.finished(), Some(GOLDEN_LINEAR_SCANOUT_IMAGE));
    }

    #[test]
    fn optimal_present_image_alias_bytes_are_unchanged() {
        let w = encode_image_create(
            GOLD_DEVICE,
            GOLD_IMAGE,
            &ImageCreateSpec {
                pnext: ImagePNext::ExternalMemory {
                    handle_type: 0x0000_0001,
                },
                flags: 0x8, // MUTABLE_FORMAT
                format: 44,
                width: 1896,
                height: 1030,
                tiling: IMAGE_TILING_OPTIMAL,
                usage: 0x1 | 0x2 | 0x4 | 0x10,
                initial_layout: 0, // UNDEFINED
            },
        );
        assert_eq!(w.finished(), Some(GOLDEN_OPTIMAL_PRESENT_IMAGE_ALIAS));
    }

    #[test]
    fn present_conversion_image_bytes_are_unchanged() {
        let w = encode_image_create(
            GOLD_DEVICE,
            GOLD_IMAGE,
            &ImageCreateSpec {
                pnext: ImagePNext::None,
                flags: 0,
                format: 44,
                width: 1896,
                height: 1030,
                tiling: IMAGE_TILING_OPTIMAL,
                usage: 0x1 | 0x2,
                initial_layout: 0,
            },
        );
        assert_eq!(w.finished(), Some(GOLDEN_PRESENT_CONVERSION_IMAGE));
    }

    #[test]
    fn plain_memory_allocate_bytes_are_unchanged() {
        let w = encode_memory_allocate(
            GOLD_DEVICE,
            GOLD_MEMORY,
            &MemoryAllocateSpec {
                pnext: MemoryPNext::None,
                size: GOLD_SIZE,
                memory_type_index: GOLD_MTI,
            },
        );
        assert_eq!(w.finished(), Some(GOLDEN_MEMORY_PLAIN));
    }

    #[test]
    fn export_memory_allocate_bytes_are_unchanged() {
        let w = encode_memory_allocate(
            GOLD_DEVICE,
            GOLD_MEMORY,
            &MemoryAllocateSpec {
                pnext: MemoryPNext::Export {
                    handle_type: 0x0000_0200,
                },
                size: GOLD_SIZE,
                memory_type_index: GOLD_MTI,
            },
        );
        assert_eq!(w.finished(), Some(GOLDEN_MEMORY_EXPORT));
    }

    #[test]
    fn dedicated_memory_allocate_bytes_are_unchanged() {
        let w = encode_memory_allocate(
            GOLD_DEVICE,
            GOLD_MEMORY,
            &MemoryAllocateSpec {
                pnext: MemoryPNext::Dedicated { image: GOLD_IMAGE },
                size: GOLD_SIZE,
                memory_type_index: GOLD_MTI,
            },
        );
        assert_eq!(w.finished(), Some(GOLDEN_MEMORY_DEDICATED));
    }

    /// The order-sensitive one: the dedicated struct's image/buffer fields come
    /// BEFORE the export struct's own handleTypes, because export's pNext points
    /// at dedicated. Swapping them still compiles and still type-checks.
    #[test]
    fn export_dedicated_memory_allocate_keeps_its_nesting_order() {
        let w = encode_memory_allocate(
            GOLD_DEVICE,
            GOLD_MEMORY,
            &MemoryAllocateSpec {
                pnext: MemoryPNext::ExportDedicated {
                    handle_type: 0x0000_0200,
                    image: GOLD_IMAGE,
                },
                size: GOLD_SIZE,
                memory_type_index: GOLD_MTI,
            },
        );
        assert_eq!(w.finished(), Some(GOLDEN_MEMORY_EXPORT_DEDICATED));
    }

    #[test]
    fn import_resource_memory_allocate_bytes_are_unchanged() {
        let w = encode_memory_allocate(
            GOLD_DEVICE,
            GOLD_MEMORY,
            &MemoryAllocateSpec {
                pnext: MemoryPNext::ImportResource {
                    resource_id: 0x1234,
                },
                size: GOLD_SIZE,
                memory_type_index: GOLD_MTI,
            },
        );
        assert_eq!(w.finished(), Some(GOLDEN_MEMORY_IMPORT_RESOURCE));
    }

    /// The 39th session, as an assertion. `IMAGE_TILING_LINEAR` was 0 — which
    /// is OPTIMAL — and the desktop painted black with no error anywhere.
    #[test]
    fn linear_and_optimal_tiling_are_not_the_same_value() {
        assert_eq!(IMAGE_TILING_OPTIMAL, 0);
        assert_eq!(IMAGE_TILING_LINEAR, 1);
    }
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

    /// The four sites `ScanoutFormat` replaces, reproduced by hand from the
    /// pre-R503 code so this test fails if the enum ever changes which formats
    /// are accepted or what they encode to.
    ///
    /// Site 1, `display.rs::virtio_scanout_format` (the wire-format converter):
    ///     0 | 88 => B8G8R8X8 (2), 28 => R8G8B8A8 (67), 87 => B8G8R8A8 (1),
    ///     _ => None
    /// Site 2, `display.rs`'s two uses of a function-local
    ///     `const DXGI_FORMAT_B8G8R8A8_UNORM: u32 = 87`.
    /// Site 3, `display.rs`'s direct-scan-out allowlist:
    ///     `matches!(dxgi_format, 28 | 87 | 88)` — note 0 is REJECTED here.
    /// Site 4, `create_allocation.rs`'s copy gate: the same {28, 87, 88} set.
    #[test]
    fn scanout_format_reproduces_all_four_pre_r503_sites() {
        fn site1_converter(dxgi: u32) -> Option<u32> {
            match dxgi {
                0 | 88 => Some(2),
                28 => Some(67),
                87 => Some(1),
                _ => None,
            }
        }
        fn site3_and_4_allowlist(dxgi: u32) -> bool {
            matches!(dxgi, 28 | 87 | 88)
        }

        // Every value the review names, plus the neighbours most likely to be
        // added by mistake.
        for dxgi in [0u32, 28, 87, 88, 10, 24, 91, 93, 1, 2, 67, 134, u32::MAX] {
            assert_eq!(
                ScanoutFormat::from_dxgi_or_legacy_zero(dxgi).map(ScanoutFormat::virtio),
                site1_converter(dxgi),
                "converter disagrees for DXGI {dxgi}"
            );
            assert_eq!(
                ScanoutFormat::from_dxgi(dxgi).is_some(),
                site3_and_4_allowlist(dxgi),
                "validator acceptance set disagrees for DXGI {dxgi}"
            );
        }

        // Site 2: the LINEAR fallback's hard-coded 87.
        assert_eq!(ScanoutFormat::Bgra8.dxgi(), 87);

        // The strict set is a subset of the converter set, so a format that
        // passes a validator always has a virtio encoding — that is the
        // "unrepresentable" half of the guarantee.
        for dxgi in 0..=256u32 {
            if ScanoutFormat::from_dxgi(dxgi).is_some() {
                assert!(ScanoutFormat::from_dxgi_or_legacy_zero(dxgi).is_some());
            }
        }

        // The legacy zero arm is deliberately not a round trip.
        assert_eq!(
            ScanoutFormat::from_dxgi_or_legacy_zero(0),
            Some(ScanoutFormat::Bgrx8)
        );
        assert_eq!(ScanoutFormat::from_dxgi(0), None);
        assert_eq!(ScanoutFormat::Bgrx8.dxgi(), 88);
    }

    /// The vectors the review names, plus the shipped mode.
    #[test]
    fn display_mode_from_host_validates_once() {
        assert_eq!(DisplayMode::from_host(0, 0), None);
        assert_eq!(DisplayMode::from_host(319, 240), None);
        assert_eq!(DisplayMode::from_host(320, 239), None);
        // The 320x240 boundary is INCLUSIVE, matching the pre-R512
        // `>= 320 && >= 240` test exactly.
        let min = DisplayMode::from_host(320, 240).expect("320x240 is the minimum");
        assert_eq!((min.width(), min.height()), (320, 240));
        let shipped = DisplayMode::from_host(1896, 1066).expect("the shipped primary");
        assert_eq!((shipped.width(), shipped.height()), (1896, 1066));
        let fallback = DisplayMode::from_host(1920, 1080).expect("the fallback mode");
        assert_eq!(<(u32, u32)>::from(fallback), (1920, 1080));
        // DspMd's packed form must not change.
        assert_eq!(shipped.packed(), (1896 << 16) | 1066);
        assert_eq!(fallback.packed(), (1920 << 16) | 1080);
        // A height whose low 16 bits would alias must still round-trip its
        // width, since DspMd masks only the height.
        let wide = DisplayMode::from_host(4096, 2160).expect("4K");
        assert_eq!(wide.packed() >> 16, 4096);
    }

    /// `DisplayMode::FALLBACK`'s `None` arms are unreachable — this is what says
    /// so, instead of a const `panic!` inside a crate the kernel driver links.
    #[test]
    fn display_mode_fallback_is_the_documented_extent() {
        assert_eq!(
            <(u32, u32)>::from(DisplayMode::FALLBACK),
            (FALLBACK_DISPLAY_WIDTH, FALLBACK_DISPLAY_HEIGHT)
        );
        assert_eq!(<(u32, u32)>::from(DisplayMode::FALLBACK), (1920, 1080));
        // If either constant were edited below the floor the const `match` would
        // silently degrade to the minimum extent; this catches that.
        assert_eq!(
            DisplayMode::from_host(FALLBACK_DISPLAY_WIDTH, FALLBACK_DISPLAY_HEIGHT),
            Some(DisplayMode::FALLBACK)
        );
    }

    #[test]
    fn gate_pack_round_trips_and_keeps_the_flag_in_the_low_half() {
        for seq in [0u32, 1, 2, 0x7FFF_FFFF, 0x8000_0000, u32::MAX] {
            for active in [false, true] {
                let word = gate_pack(seq, active);
                assert_eq!(gate_seq(word), seq, "seq lost for {seq}/{active}");
                assert_eq!(gate_active(word), active, "flag lost for {seq}/{active}");
            }
        }
        // A fresh adapter (word 0) is generation 0, inactive — the same "no
        // programming outstanding" answer the bare AtomicU32 flag gave.
        assert_eq!(gate_pack(0, false), 0);
        assert!(!gate_active(0));
        // The VSync DPC tests only the low half, so a raised gate at ANY
        // generation reads active. This is the check that would fail if a reader
        // were left comparing the packed word against 0/1.
        assert!(gate_active(gate_pack(12345, true)));
        // ...and a CLEARED gate at a nonzero generation must read inactive even
        // though the word itself is nonzero. A missed reader here would suppress
        // VSync forever.
        assert!(!gate_active(gate_pack(12345, false)));
        assert_ne!(gate_pack(12345, false), 0);
    }

    /// The three transitions the gate's correctness rests on, as the
    /// compare-exchange operands the driver actually uses.
    #[test]
    fn gate_transitions_reject_a_stale_clear() {
        // Raise N -> N+1, active.
        let empty = gate_pack(0, false);
        let n1 = gate_pack(gate_seq(empty).wrapping_add(1), true);
        assert_eq!(n1, gate_pack(1, true));

        // The owner of interval 1 clears it: CAS (1,true) -> (1,false) matches.
        assert_eq!(n1, gate_pack(1, true));

        // A SECOND raise lands first, making the gate interval 2.
        let n2 = gate_pack(gate_seq(n1).wrapping_add(1), true);
        assert_eq!(n2, gate_pack(2, true));

        // Interval 1's completion now tries its clear. Its expected operand no
        // longer matches the live word, so the CAS fails and the gate stays
        // raised for interval 2 — which is the whole point.
        assert_ne!(n2, gate_pack(1, true));

        // Generation wrap must not alias a live interval with a stale one.
        let wrapped = gate_pack(u32::MAX, true);
        assert_eq!(gate_seq(wrapped).wrapping_add(1), 0);
        assert_ne!(gate_pack(0, true), wrapped);
    }

    // ── Writer ────────────────────────────────────────────────────────────────

    /// The KMD's `EXT_FULL` extension tier, verbatim from
    /// `kmd_render/src/virtio/venus.rs`. These strings decide the size of the
    /// largest stream the driver ever encodes, so the test carries its own copy:
    /// if the driver's list grows, this test still asserts the OLD number and the
    /// [`MAX_CMD_BYTES`] headroom must be recomputed deliberately.
    const EXT_FULL: [&[u8]; 5] = [
        b"VK_KHR_external_memory\0",
        b"VK_KHR_external_memory_fd\0",
        b"VK_KHR_image_format_list\0",
        b"VK_EXT_external_memory_dma_buf\0",
        b"VK_EXT_image_drm_format_modifier\0",
    ];

    /// Re-encode `vkCreateDevice` exactly as `create_venus_device` does, for the
    /// given extension list.
    fn encode_create_device(exts: &[&[u8]]) -> Writer {
        const CMD_CREATE_DEVICE: u32 = 11;
        const CMD_FLAG_GENERATE_REPLY: u32 = 1;
        const ST_DEVICE_CREATE_INFO: i32 = 3;
        const ST_DEVICE_QUEUE_CREATE_INFO: i32 = 2;

        let mut w = Writer::new();
        w.header(CMD_CREATE_DEVICE, CMD_FLAG_GENERATE_REPLY);
        w.u64(0xDEAD_BEEF); // VkPhysicalDevice
        w.count(true); // simple_pointer(pCreateInfo)
        w.i32(ST_DEVICE_CREATE_INFO);
        w.u64(0); // pNext
        w.u32(0); // flags
        w.u32(1); // queueCreateInfoCount
        w.count(true); // array_size(1)
        w.i32(ST_DEVICE_QUEUE_CREATE_INFO);
        w.u64(0); // pNext
        w.u32(0); // flags
        w.u32(0); // queueFamilyIndex
        w.u32(1); // queueCount
        w.count(true); // array_size(1)
        w.f32(1.0); // priority
        w.u32(0); // enabledLayerCount
        w.count(false);
        if exts.is_empty() {
            w.u32(0);
            w.count(false);
        } else {
            w.u32(exts.len() as u32);
            w.u64(exts.len() as u64);
            for ext in exts {
                w.u64(ext.len() as u64);
                w.bytes_padded(ext);
            }
        }
        w.count(false); // pEnabledFeatures
        w.count(false); // pAllocator
        w.count(true); // simple_pointer(pDevice)
        w.u64(0x1234); // VkDevice handle
        w
    }

    /// The number that sizes [`MAX_CMD_BYTES`]. The comment this replaces said
    /// "the largest is vkCreateDevice (~120 bytes)" and was wrong by 212 bytes,
    /// so the buffer's whole safety margin was arithmetic performed in prose.
    #[test]
    fn writer_ext_full_create_device_is_332_bytes() {
        let w = encode_create_device(&EXT_FULL);
        assert!(!w.overflowed());
        assert_eq!(w.len(), 332);
        // 144 bytes of struct plus a 188-byte extension block. The zero-extension
        // arm encodes the same two words for count and array size, so the
        // difference is exactly the five lengths plus the five padded names.
        let bare = encode_create_device(&[]);
        assert_eq!(bare.len(), 144);
        assert_eq!(w.len() - bare.len(), 188);
    }

    /// 512 - 332 = 180 bytes of slack. A 32-character extension name costs 44
    /// bytes (an 8-byte length plus 33 bytes padded to 36), so FOUR more still
    /// fit at 508 bytes and a fifth does not.
    ///
    /// Both the original finding ("a sixth extension bugchecks the guest") and
    /// the review that corrected it ("four more EXT_FULL strings" overflow) are
    /// wrong in the same direction. This is the measured boundary.
    #[test]
    fn writer_headroom_is_four_more_extension_names() {
        const BIG: &[u8] = b"VK_EXT_image_drm_format_modifier\0";
        let plus_four: [&[u8]; 9] = [
            EXT_FULL[0],
            EXT_FULL[1],
            EXT_FULL[2],
            EXT_FULL[3],
            EXT_FULL[4],
            BIG,
            BIG,
            BIG,
            BIG,
        ];
        let w = encode_create_device(&plus_four);
        assert!(!w.overflowed(), "four more names must still fit");
        assert_eq!(w.len(), 508);

        let plus_five: [&[u8]; 10] = [
            EXT_FULL[0],
            EXT_FULL[1],
            EXT_FULL[2],
            EXT_FULL[3],
            EXT_FULL[4],
            BIG,
            BIG,
            BIG,
            BIG,
            BIG,
        ];
        let w = encode_create_device(&plus_five);
        assert!(w.overflowed(), "a fifth extra name must be refused");
        assert!(w.finished().is_none());
    }

    /// Overflow must be sticky: a refused write must not leave a shorter, valid
    /// looking stream that a later small write could complete.
    #[test]
    fn writer_overflow_is_sticky_and_withholds_the_bytes() {
        let mut w = Writer::new();
        w.header(7, 0);
        assert_eq!(w.cmd_type(), 7);
        for _ in 0..MAX_CMD_BYTES {
            w.u64(0);
        }
        assert!(w.overflowed());
        assert!(w.finished().is_none());
        // A write that WOULD fit must not un-poison it.
        w.u32(1);
        assert!(w.finished().is_none());
        // The command type stays readable so the refusal can name the command.
        assert_eq!(w.cmd_type(), 7);
    }

    /// The exact boundary: a stream that fills the buffer to the last byte is
    /// valid; one byte more is not.
    #[test]
    fn writer_accepts_exactly_max_cmd_bytes() {
        let mut w = Writer::new();
        for _ in 0..MAX_CMD_BYTES / 8 {
            w.u64(0);
        }
        assert!(!w.overflowed());
        assert_eq!(w.len(), MAX_CMD_BYTES);
        assert_eq!(w.finished().map(|s| s.len()), Some(MAX_CMD_BYTES));

        w.u32(0);
        assert!(w.overflowed());
    }

    /// `bytes_padded` zero-fills to the next 4-byte boundary, and the padding is
    /// counted against the budget — a 33-byte name costs 36.
    #[test]
    fn writer_bytes_padded_pads_with_zeroes() {
        let mut w = Writer::new();
        w.bytes_padded(b"abcde");
        assert_eq!(w.len(), 8);
        assert_eq!(w.finished(), Some(&b"abcde\0\0\0"[..]));

        let mut w = Writer::new();
        w.bytes_padded(b"VK_EXT_image_drm_format_modifier\0");
        assert_eq!(w.len(), 36);
    }

    // ── Memory-type selection ────────────────────────────────────────────────

    /// The shape this box actually reports: a pure DEVICE_LOCAL type, a
    /// HOST_VISIBLE|HOST_COHERENT type, and a device-local BAR type.
    const NVIDIA_SHAPED: [u32; 3] = [
        MEMORY_PROPERTY_DEVICE_LOCAL,
        MEMORY_PROPERTY_HOST_VISIBLE | MEMORY_PROPERTY_HOST_COHERENT,
        MEMORY_PROPERTY_DEVICE_LOCAL | MEMORY_PROPERTY_HOST_VISIBLE | MEMORY_PROPERTY_HOST_COHERENT,
    ];

    /// Today's host takes the exact arm on both selectors — which is why the
    /// guest gate for R605 is "VnMtDown is ABSENT", not "VnMtDown is 0".
    #[test]
    fn memory_type_exact_on_the_shape_this_box_reports() {
        assert_eq!(
            choose_device_local_memory_type(&NVIDIA_SHAPED, 3, 0b111),
            Some(MemoryTypeChoice::Exact(0))
        );
        assert_eq!(
            choose_host_visible_memory_type(&NVIDIA_SHAPED, 3, 0b111),
            Some(MemoryTypeChoice::Exact(1))
        );
    }

    /// The defect case: `memoryTypeBits` allows only a host-visible,
    /// non-device-local type. The old signature returned `Some(i)` and the
    /// caller's "device-local dedicated memory" contract was silently false.
    #[test]
    fn memory_type_downgrades_when_only_host_visible_is_allowed() {
        assert_eq!(
            choose_device_local_memory_type(&NVIDIA_SHAPED, 3, 0b010),
            Some(MemoryTypeChoice::Downgraded(1))
        );
    }

    /// A device-local BAR type still satisfies DEVICE_LOCAL, so tier 2 is Exact.
    /// Tier 1 is a preference inside the same answer, not a downgrade.
    #[test]
    fn memory_type_device_local_bar_type_is_exact() {
        assert_eq!(
            choose_device_local_memory_type(&NVIDIA_SHAPED, 3, 0b100),
            Some(MemoryTypeChoice::Exact(2))
        );
        // ...and tier 1 still wins when both are allowed.
        assert_eq!(
            choose_device_local_memory_type(&NVIDIA_SHAPED, 3, 0b101),
            Some(MemoryTypeChoice::Exact(0))
        );
    }

    /// HOST_VISIBLE without HOST_COHERENT is the silent half of the host-visible
    /// selector: a MAPPABLE scanout blob whose writes need flushes nobody issues.
    #[test]
    fn memory_type_host_visible_without_coherent_is_a_downgrade() {
        let flags = [MEMORY_PROPERTY_HOST_VISIBLE, MEMORY_PROPERTY_DEVICE_LOCAL];
        assert_eq!(
            choose_host_visible_memory_type(&flags, 2, 0b11),
            Some(MemoryTypeChoice::Downgraded(0))
        );
    }

    /// No allowed type has the property at all.
    #[test]
    fn memory_type_none_when_nothing_qualifies() {
        let flags = [MEMORY_PROPERTY_DEVICE_LOCAL, MEMORY_PROPERTY_DEVICE_LOCAL];
        assert_eq!(choose_host_visible_memory_type(&flags, 2, 0b11), None);
        assert_eq!(choose_device_local_memory_type(&flags, 2, 0), None);
    }

    /// `memory_type_count` and the array length both bound the scan; a host that
    /// reports a count larger than the array must not index past it.
    #[test]
    fn memory_type_scan_is_bounded_by_both_count_and_array() {
        let flags = [MEMORY_PROPERTY_DEVICE_LOCAL];
        assert_eq!(
            choose_device_local_memory_type(&flags, 32, u32::MAX),
            Some(MemoryTypeChoice::Exact(0))
        );
        // A count of 0 means the host reported nothing usable.
        assert_eq!(choose_device_local_memory_type(&flags, 0, u32::MAX), None);
    }

    /// Little-endian, and the header is two 4-byte words in encode order.
    #[test]
    fn writer_encodes_little_endian() {
        let mut w = Writer::new();
        w.header(0x1122_3344, 0x5566_7788);
        w.u64(0x0102_0304_0506_0708);
        w.f32(1.0);
        assert_eq!(
            w.finished(),
            Some(
                &[
                    0x44, 0x33, 0x22, 0x11, 0x88, 0x77, 0x66, 0x55, 0x08, 0x07, 0x06, 0x05, 0x04,
                    0x03, 0x02, 0x01, 0x00, 0x00, 0x80, 0x3F,
                ][..]
            )
        );
    }
}

// ── Sorted-by-key splice (R712) ─────────────────────────────────────────────

/// Where an ascending block of keys belongs in an already-sorted slice, and how
/// many existing entries it displaces.
///
/// # Why this is here rather than in `kmd_render`
///
/// `PagingPteShadow::update_leaf` used to `retain` and then
/// `sort_unstable_by_key` a table bounded at 65,536 entries with a spinlock held
/// — and `KeAcquireSpinLockRaiseToDpc` raises to DISPATCH_LEVEL regardless of the
/// caller's IRQL. That is O(n log n) of unbounded work in a DISPATCH path, which
/// the project rule forbids, and its cost was invisible because only the
/// resulting LENGTH was recorded (`PgVp`).
///
/// The sort was unnecessary: the table is already sorted before the update, the
/// appended block is itself ascending, and it is confined to the exact VA range
/// `retain` just cleared. So one splice at the right index reproduces the sorted
/// result. That argument is easy to get subtly wrong for a partially-overlapping
/// range — and getting it wrong makes `resolve`'s `binary_search_by_key` return
/// `None`, which breaks eviction CONTENT. Hence: pure logic, moved out, with a
/// randomized oracle test against a reference `sort`.
///
/// Returns `(start, removed)`: the entries in `[start, start + removed)` are the
/// ones whose keys fall inside `[first_key, end_key)`, and the new block belongs
/// at `start`.
///
/// `keys` must be sorted ascending.
pub fn sorted_splice_range(keys: &[u64], first_key: u64, end_key: u64) -> (usize, usize) {
    let start = partition_point(keys, |k| k < first_key);
    let end = partition_point(keys, |k| k < end_key);
    (start, end - start)
}

/// `slice::partition_point` over a key slice, spelled out so this crate stays
/// free of any dependency on slice-method stabilisation in `no_std`.
fn partition_point<F: Fn(u64) -> bool>(keys: &[u64], pred: F) -> usize {
    let mut lo = 0usize;
    let mut hi = keys.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(keys[mid]) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

#[cfg(test)]
mod sorted_splice_tests {
    extern crate alloc;
    use super::sorted_splice_range;
    use alloc::vec::Vec;

    /// Reference implementation: retain-then-sort, exactly what the KMD did
    /// before the splice replaced it.
    fn reference(
        existing: &[(u64, u64)],
        first: u64,
        end: u64,
        block: &[(u64, u64)],
    ) -> Vec<(u64, u64)> {
        let mut out: Vec<(u64, u64)> = existing
            .iter()
            .copied()
            .filter(|(k, _)| *k < first || *k >= end)
            .collect();
        out.extend_from_slice(block);
        out.sort_by_key(|(k, _)| *k);
        out
    }

    /// Splice implementation, driven by `sorted_splice_range`.
    fn spliced(
        existing: &[(u64, u64)],
        first: u64,
        end: u64,
        block: &[(u64, u64)],
    ) -> Vec<(u64, u64)> {
        let keys: Vec<u64> = existing.iter().map(|(k, _)| *k).collect();
        let (start, removed) = sorted_splice_range(&keys, first, end);
        let mut out: Vec<(u64, u64)> = Vec::new();
        out.extend_from_slice(&existing[..start]);
        out.extend_from_slice(block);
        out.extend_from_slice(&existing[start + removed..]);
        out
    }

    /// Deterministic xorshift — `Math::random` is not available and a fixed seed
    /// makes a failure reproducible.
    fn next(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    #[test]
    fn splice_matches_retain_then_sort_over_random_ranges() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for case in 0..2000u64 {
            // A sorted existing table with gaps, so ranges can partially overlap.
            let n = (next(&mut seed) % 40) as usize;
            let mut existing: Vec<(u64, u64)> = Vec::new();
            let mut key = next(&mut seed) % 8;
            for i in 0..n {
                existing.push((key, key * 1000 + i as u64));
                key += 1 + next(&mut seed) % 4;
            }

            let first = next(&mut seed) % 80;
            let len = next(&mut seed) % 12;
            let end = first + len;
            // The appended block is ascending and confined to [first, end).
            let block: Vec<(u64, u64)> = (first..end).map(|k| (k, k * 7 + case)).collect();

            let want = reference(&existing, first, end, &block);
            let got = spliced(&existing, first, end, &block);
            assert_eq!(want, got, "case {case}: first={first} end={end}");
            // The result must be sorted — that is what `resolve`'s binary search
            // depends on.
            assert!(
                got.windows(2).all(|w| w[0].0 <= w[1].0),
                "case {case} unsorted"
            );
        }
    }

    #[test]
    fn empty_block_is_pure_removal() {
        let existing = [(1u64, 10u64), (2, 20), (5, 50), (9, 90)];
        let keys: Vec<u64> = existing.iter().map(|(k, _)| *k).collect();
        let (start, removed) = sorted_splice_range(&keys, 2, 6);
        assert_eq!((start, removed), (1, 2));
    }

    #[test]
    fn range_beyond_the_end_removes_nothing() {
        let existing = [(1u64, 10u64), (2, 20)];
        let keys: Vec<u64> = existing.iter().map(|(k, _)| *k).collect();
        assert_eq!(sorted_splice_range(&keys, 100, 200), (2, 0));
    }
}

/// Scan-out presentation-epoch ownership — the display-consumer half of
/// "when may Windows have this allocation back?" (ROADMAP defect 0ab-B).
///
/// # The invariant
///
/// A Helios scan-out is **not** a continuous scan-out. The host reads the bound
/// DMA-BUF exactly once per `RESOURCE_FLUSH` and at no other time, so a
/// presented buffer must be immutable from the moment it is published to the
/// host until that read has finished. Windows knows nothing about that read: it
/// retires a flip on the driver's own completion notifications and then hands
/// the buffer back to DXGI, which hands it to the app, which clears it to
/// opaque black for its next frame — while the host has still not read it. That
/// is the entirely-black published frame.
///
/// So a presentation may be released only when BOTH halves hold:
///
/// ```text
/// reuse_safe(epoch) = producer_venus_work_retired(epoch)
///                     AND host_reader_lease_ended(epoch)
/// ```
///
/// The producer half already exists (`VirtioGpu::note_wddm_submission`'s wire-
/// fence watermark). This module is the second half.
///
/// # The state machine
///
/// * Every presentation of a buffer to the host mints a **monotonically
///   increasing epoch**. Re-presenting a still-bound buffer mints a NEW epoch:
///   the app wrote it again, so it needs to be read again. A bind generation
///   would collapse those and is therefore not enough.
/// * `bound_epoch` is the epoch whose buffer the host is bound to right now. It
///   advances only when the display worker has actually published the binding
///   (a completed `SET_SCANOUT_BLOB`, or an already-bound re-present).
/// * A `RESOURCE_FLUSH` issued while `bound_epoch == E` proves, when its
///   response returns, that the host has read the buffer of epoch `E` — and, by
///   monotonicity, of every epoch below it. That is [`LeaseTracker::issue_flush`]
///   / [`LeaseTracker::complete_flush`], and the snapshot `E` is the typed token
///   the transport carries on the in-flight entry.
/// * `read_epoch` is the watermark: every epoch `<= read_epoch` has ended its
///   lease. It only ever moves forward ([`merge_read_epoch`]), so a completion
///   that arrives out of order is inert rather than corrupting.
/// * A successful bind of a DIFFERENT resource ends every older epoch's lease
///   without a read. The virtio control queue is strictly FIFO host-side, so a
///   returned `SET_SCANOUT_BLOB` proves every earlier-enqueued flush already
///   completed, and a flush enqueued later cannot read a resource that is no
///   longer the scan-out. **A completed bind is not a completed read** — it is a
///   proof that no read remains, which is a different (and weaker) statement,
///   and it is why this is the escape hatch and not the primary terminator.
/// * Failure, cancellation and teardown end leases explicitly and loudly. There
///   is deliberately NO timeout: a lease is never released on the theory that
///   the read has probably finished by now.
///
/// # Why the shipped driver mirrors this with atomics
///
/// The three edges run under three different locks: the mint is on the
/// `DxgkDdiSubmitCommand` DISPATCH path, the bind and the flush issue are on the
/// PASSIVE display worker under `scanout_mutex`, and the flush completion is in
/// the used-ring drain under `virtio_lock` — which may not take
/// `wddm_notify_lock`, because the established order is the reverse and
/// inverting it is a DIRQL deadlock. Every transition here is monotone, so the
/// KMD implements them as `fetch_add` / `fetch_max` on `AdapterContext` atomics
/// and calls the SAME predicates below.
///
/// # ⚠ WHAT 22.22.217.0 RETIRED, and what it kept
///
/// The **withholding** half above — holding `DXGK_INTERRUPT_DMA_COMPLETED` and
/// the CRTC_VSYNC primary address until the presentation's read finished — was
/// built, shipped and MEASURED INERT against the defect: a 2×2 lease ×
/// `BindFlushMode` factorial over 46 681 frames moved whole-flush black by
/// nothing in any cell (14.5–16.6 % everywhere). The reason is structural and is
/// now proven: the app's clear of a reclaimed buffer never travels in a WDDM DMA
/// buffer, so no completion-notification policy can order it. The driver
/// therefore gates a WDDM submission on its Venus watermark alone again.
///
/// The **epochs** are kept and are load-bearing for the replacement fix: the
/// ownership gate on the flush executor ([`surplus_republish`]), which refuses to
/// re-read a binding generation that has already been published while a newer
/// presentation is outstanding. So [`next_epoch`], [`merge_read_epoch`] and
/// [`lease_satisfied`] are shipped predicates; [`LeaseTracker`]'s FIFO/pending-
/// primary machinery below is the specification of the RETIRED withholding, kept
/// as the record of what was measured — it is no longer a model of shipped code.
/// Every item that models only the retired half says so on its own doc line.
pub mod scanout_retire {
    //! Pure decisions for scanout-resource retirement while asynchronous binds
    //! share the virtio control FIFO.

    /// Whether a pure-query FIFO barrier is required before the latest
    /// host-selected resource is authoritative. A mismatch means at least one
    /// SET was issued after the last successful response; it may succeed or fail,
    /// and only a later response can distinguish those outcomes.
    pub const fn needs_fifo_barrier(wire_seq: u64, accepted_seq: u64) -> bool {
        wire_seq != accepted_seq
    }

    /// Disable scanout only when the retiring resource is still the final
    /// successful host selection. A different resource is already the lifetime
    /// barrier; queueing zero behind it would blank that newer selection.
    pub const fn needs_disable(retiring_resource: u32, final_host_resource: u32) -> bool {
        retiring_resource != 0 && retiring_resource == final_host_resource
    }
}

#[cfg(test)]
mod scanout_retire_tests {
    use super::scanout_retire::{needs_disable, needs_fifo_barrier};

    #[test]
    fn issued_newer_bind_requires_a_terminal_fifo_response() {
        assert!(needs_fifo_barrier(12, 11));
        assert!(!needs_fifo_barrier(12, 12));
    }

    #[test]
    fn successful_newer_bind_must_not_be_blankened_by_retiring_the_old_one() {
        let retiring_a = 0x121;
        let final_b = 0x128;
        assert!(!needs_disable(retiring_a, final_b));
    }

    #[test]
    fn failed_newer_bind_leaves_the_old_selection_needing_disable() {
        let retiring_a = 0x121;
        assert!(needs_disable(retiring_a, retiring_a));
        assert!(!needs_disable(retiring_a, 0));
    }
}

pub mod scanout_lease {
    /// "This submission is not gated on any host read." Used for paging
    /// buffers, render submissions, and every flip that carries no scan-out
    /// presentation (the MMIO/`FlipOnVSyncMmIo` desktop path).
    pub const NO_LEASE: u64 = 0;

    /// The first epoch a tracker mints. Epoch 0 is reserved for [`NO_LEASE`].
    pub const FIRST_EPOCH: u64 = 1;

    /// Mint the epoch after `previous`.
    ///
    /// Saturating: at `u64::MAX` the counter stops instead of wrapping to 0,
    /// because a wrap to 0 would read as [`NO_LEASE`] and silently ungate every
    /// flip. At 200 presentations per second that bound is ~2.9 billion years
    /// away, so the saturation arm exists to make the failure shape *stuck*
    /// rather than *unsafe*.
    pub const fn next_epoch(previous: u64) -> u64 {
        if previous == u64::MAX {
            u64::MAX
        } else {
            previous + 1
        }
    }

    /// Whether a submission whose display-consumer requirement is `lease` may be
    /// released to Windows.
    pub const fn lease_satisfied(read_epoch: u64, lease: u64) -> bool {
        lease == NO_LEASE || read_epoch >= lease
    }

    /// Monotone merge of a new lease-end watermark into the current one.
    pub const fn merge_read_epoch(current: u64, candidate: u64) -> u64 {
        if candidate > current {
            candidate
        } else {
            current
        }
    }

    /// THE OWNERSHIP GATE (ROADMAP defect 0ab-B, D2). Whether a `RESOURCE_FLUSH`
    /// issued right now would be a SURPLUS re-read of the current binding.
    ///
    /// True means: the generation the host is bound to has already been
    /// published (a flush token covering `bound_epoch` completed) AND a newer
    /// presentation has been minted. The successor's own bind edge owns the next
    /// publish, so re-reading now cannot show the newer frame — it can only
    /// re-read a buffer the app may already have reclaimed and cleared, which is
    /// a manufactured black frame (measured: surplus refresh flushes were ~30 %
    /// of publishes at 43.4 % black, against 2.6 % for first reads).
    ///
    /// `tracked` is the third operand and it is not optional. It says the
    /// CURRENT binding was published with an epoch at all. The MMIO /
    /// `FlipOnVSyncMmIo` desktop contract mints no presentations, so on it
    /// `present_epoch` is frozen at whatever the last DMA-flip app left behind:
    /// a stale `present_epoch > bound_epoch` would then hold forever and drop
    /// every desktop refresh — a frozen desktop, defect 0aa. With `tracked`
    /// false the gate is off and the flush path behaves exactly as it did.
    ///
    /// The three passing cases, stated so a future edit cannot lose them:
    /// * a first publish passes (`read_epoch < bound_epoch`);
    /// * an idle desktop re-publish passes (`present == bound <= read`);
    /// * a same-buffer re-present passes, because it mints AND publishes a fresh
    ///   epoch, so `read_epoch < bound_epoch` again at the marker's fire time.
    pub const fn surplus_republish(
        tracked: bool,
        read_epoch: u64,
        bound_epoch: u64,
        present_epoch: u64,
    ) -> bool {
        tracked
            && bound_epoch != NO_LEASE
            && lease_satisfied(read_epoch, bound_epoch)
            && present_epoch > bound_epoch
    }

    /// Whether `fence` is FORWARD of `last` in the WDDM `SubmissionFenceId`
    /// sequence, which wraps at `u32::MAX`.
    ///
    /// dxgkrnl treats a `SubmissionFenceId` as a watermark and requires
    /// monotonic completion, so a fence that is equal to or behind the last
    /// completed one must be dropped rather than re-signalled. Extracted from
    /// `submit_command::signal_dma_completed` so the wrap arithmetic has a host
    /// test; that function calls this.
    pub const fn fence_is_forward(last: u32, fence: u32) -> bool {
        fence != last && fence.wrapping_sub(last) < 0x8000_0000
    }

    /// Why a presentation epoch's host-reader lease ended.
    ///
    /// Every variant is counted separately by the driver: a fix that "works"
    /// because every lease ends as `Cancelled` is not working, and the only way
    /// to see that is to never merge the reasons.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum LeaseEnd {
        /// The exact `RESOURCE_FLUSH` covering this epoch returned. The healthy
        /// steady-state reason, and the only one that means "the host has the
        /// pixels".
        HostRead,
        /// A later successful `SET_SCANOUT_BLOB` bound a different resource, so
        /// no read of this epoch remains or can be queued.
        Superseded,
        /// The flush could not be enqueued, the host answered with an error, or
        /// the allocation was retired. The command has terminated; no future
        /// read from it exists.
        Cancelled,
        /// Transport failure, preemption/TDR epoch, reset or StopDevice.
        Teardown,
    }

    /// Unsampled per-reason tallies. Mirrored into the service key as the `Ls*`
    /// values.
    #[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
    pub struct LeaseCounters {
        /// Presentation epochs minted.
        pub minted: u32,
        /// `RESOURCE_FLUSH` reads queued with a lease token.
        pub read_queued: u32,
        /// Read tokens completed (success or host error).
        pub read_completed: u32,
        /// Epochs ended by [`LeaseEnd::HostRead`].
        pub ended_read: u32,
        /// Epochs ended by [`LeaseEnd::Superseded`] — a binding that never got
        /// its own read.
        pub ended_superseded: u32,
        /// Epochs ended by [`LeaseEnd::Cancelled`].
        pub ended_cancelled: u32,
        /// Epochs ended by [`LeaseEnd::Teardown`].
        pub ended_teardown: u32,
        /// Completions whose token was at or behind the watermark: coalesced,
        /// duplicated or reordered. Inert, but counted — a large value means the
        /// flush path is issuing reads nobody is waiting for.
        pub stale_completions: u32,
        /// Retirement attempts refused because the lease was still open.
        pub retire_blocked: u32,
        /// Retirement attempts allowed by a satisfied lease.
        pub retire_released: u32,
        /// Deferred primary addresses actually published to the VSync path.
        pub primary_published: u32,
        /// Bounded-state exhaustion: pending flips dropped because the FIFO was
        /// full. Practically unreachable; loud if it ever is not.
        pub overflow: u32,
    }

    /// Capacity of [`LeaseTracker`]'s model FIFO.
    ///
    /// The shipped driver's queue is `VecDeque<WddmPending>` with
    /// `MAX_WDDM_PENDING = 256`; the model uses a small fixed array so the
    /// exhaustion path is reachable in a test. What is under test is the RULE
    /// (head-of-line blocking, monotonic delivery, overflow clears the queue and
    /// releases every lease), not the number.
    pub const MODEL_FIFO_LEN: usize = 8;

    /// One pending WDDM submission in the model FIFO.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct PendingFlip {
        /// `DXGKARG_SUBMITCOMMAND::SubmissionFenceId`.
        pub fence: u32,
        /// Producer half: this submission's Venus completion watermark, already
        /// satisfied when `true`.
        pub producer_ready: bool,
        /// Display-consumer half: the presentation epoch whose host read must
        /// finish first, or [`NO_LEASE`].
        ///
        /// ⚠ RETIRED IN THE DRIVER (22.22.217.0): the shipped FIFO carries no
        /// lease any more. See the module's "what 22.22.217.0 retired" note.
        pub lease: u64,
    }

    /// What one attempt to retire the head of the FIFO produced.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum RetireStep {
        /// Nothing queued.
        Empty,
        /// The head's producer work has not retired yet.
        BlockedOnProducer,
        /// The head's producer work is done but the host has not finished
        /// reading the presentation named by `lease`.
        ///
        /// ⚠ RETIRED IN THE DRIVER (22.22.217.0) — `WddmTake` has no such arm.
        BlockedOnLease(u64),
        /// Deliver `DXGK_INTERRUPT_DMA_COMPLETED` for this fence.
        Ready(u32),
    }

    /// The executable specification of the ownership state machine.
    ///
    /// Single-threaded and lock-free by construction: the shipped driver splits
    /// these fields across three locks and reproduces each transition with a
    /// monotone atomic, calling the same free functions above.
    #[derive(Clone, Copy, Debug)]
    pub struct LeaseTracker {
        next_epoch: u64,
        bound_epoch: u64,
        bound_resource: u32,
        read_epoch: u64,
        /// The primary address armed at the bind, waiting for its epoch's lease
        /// to end before a CRTC_VSYNC may report it.
        pending_primary: Option<(u64, u64)>,
        /// The address the VSync heartbeat currently reports.
        displayed_primary: u64,
        fifo: [Option<PendingFlip>; MODEL_FIFO_LEN],
        fifo_len: usize,
        /// Last fence handed to Windows; the monotonicity check's left operand.
        last_completed_fence: u32,
        counters: LeaseCounters,
    }

    impl Default for LeaseTracker {
        fn default() -> Self {
            Self::new()
        }
    }

    impl LeaseTracker {
        pub const fn new() -> Self {
            Self {
                next_epoch: FIRST_EPOCH,
                bound_epoch: NO_LEASE,
                bound_resource: 0,
                read_epoch: NO_LEASE,
                pending_primary: None,
                displayed_primary: 0,
                fifo: [None; MODEL_FIFO_LEN],
                fifo_len: 0,
                last_completed_fence: 0,
                counters: LeaseCounters {
                    minted: 0,
                    read_queued: 0,
                    read_completed: 0,
                    ended_read: 0,
                    ended_superseded: 0,
                    ended_cancelled: 0,
                    ended_teardown: 0,
                    stale_completions: 0,
                    retire_blocked: 0,
                    retire_released: 0,
                    primary_published: 0,
                    overflow: 0,
                },
            }
        }

        pub const fn counters(&self) -> LeaseCounters {
            self.counters
        }

        pub const fn read_epoch(&self) -> u64 {
            self.read_epoch
        }

        pub const fn bound_epoch(&self) -> u64 {
            self.bound_epoch
        }

        pub const fn displayed_primary(&self) -> u64 {
            self.displayed_primary
        }

        pub const fn pending_len(&self) -> usize {
            self.fifo_len
        }

        /// Mint the epoch for one presentation. `DxgkDdiSubmitCommand`'s
        /// DMA-flip arm, before the flip's handle is published to the worker.
        pub fn mint_presentation(&mut self) -> u64 {
            let epoch = self.next_epoch;
            self.next_epoch = next_epoch(self.next_epoch);
            self.counters.minted = self.counters.minted.saturating_add(1);
            epoch
        }

        /// The display worker published `epoch`'s buffer to the host.
        ///
        /// `rebound` is true when this call issued a `SET_SCANOUT_BLOB` (the
        /// resource changed); false for a re-present of the already-bound
        /// buffer, which publishes nothing to the host but is still a new
        /// presentation that has to be read.
        pub fn publish_bind(&mut self, epoch: u64, resource: u32, rebound: bool) {
            if epoch == NO_LEASE {
                return;
            }
            if rebound && resource != self.bound_resource && self.bound_epoch != NO_LEASE {
                // Everything published before this bind can no longer be read.
                self.end_leases_through(self.bound_epoch, LeaseEnd::Superseded);
            }
            self.bound_epoch = merge_read_epoch(self.bound_epoch, epoch);
            self.bound_resource = resource;
        }

        /// Arm the primary address a later CRTC_VSYNC may report, gated on
        /// `epoch`'s lease. Publishes immediately when the lease has already
        /// ended.
        ///
        /// ⚠ RETIRED IN THE DRIVER (22.22.217.0): the bind publishes the
        /// address unconditionally now. Kept as the record of the withholding
        /// that was measured inert.
        pub fn arm_primary(&mut self, address: u64, epoch: u64) {
            if epoch == NO_LEASE || lease_satisfied(self.read_epoch, epoch) {
                self.displayed_primary = address;
                self.counters.primary_published = self.counters.primary_published.saturating_add(1);
                return;
            }
            self.pending_primary = Some((address, epoch));
        }

        /// Snapshot the epoch a `RESOURCE_FLUSH` issued now will prove was read.
        pub fn issue_flush(&mut self) -> u64 {
            self.counters.read_queued = self.counters.read_queued.saturating_add(1);
            self.bound_epoch
        }

        /// A flush token came back. `ok` is false for a host error response,
        /// which still terminates the command — it just does not mean the pixels
        /// were published.
        pub fn complete_flush(&mut self, covers: u64, ok: bool) {
            self.counters.read_completed = self.counters.read_completed.saturating_add(1);
            let reason = if ok {
                LeaseEnd::HostRead
            } else {
                LeaseEnd::Cancelled
            };
            if covers == NO_LEASE || covers <= self.read_epoch {
                self.counters.stale_completions = self.counters.stale_completions.saturating_add(1);
                return;
            }
            self.end_leases_through(covers, reason);
        }

        /// End every lease at or below `epoch`, for `reason`.
        pub fn end_leases_through(&mut self, epoch: u64, reason: LeaseEnd) {
            let merged = merge_read_epoch(self.read_epoch, epoch);
            if merged == self.read_epoch {
                return;
            }
            self.read_epoch = merged;
            let slot = match reason {
                LeaseEnd::HostRead => &mut self.counters.ended_read,
                LeaseEnd::Superseded => &mut self.counters.ended_superseded,
                LeaseEnd::Cancelled => &mut self.counters.ended_cancelled,
                LeaseEnd::Teardown => &mut self.counters.ended_teardown,
            };
            *slot = slot.saturating_add(1);
            self.publish_pending_primary();
        }

        /// End every lease that has ever been minted. Reset, StopDevice,
        /// transport failure, allocation retirement.
        pub fn release_all(&mut self, reason: LeaseEnd) {
            let highest = self.next_epoch.saturating_sub(1);
            self.end_leases_through(highest, reason);
        }

        fn publish_pending_primary(&mut self) {
            let Some((address, epoch)) = self.pending_primary else {
                return;
            };
            if !lease_satisfied(self.read_epoch, epoch) {
                return;
            }
            self.pending_primary = None;
            self.displayed_primary = address;
            self.counters.primary_published = self.counters.primary_published.saturating_add(1);
        }

        /// Queue one WDDM submission. Returns true when the caller must signal
        /// `DMA_COMPLETED` immediately (nothing gates it, or the FIFO overflowed
        /// and degraded to the immediate model).
        pub fn submit(&mut self, flip: PendingFlip) -> bool {
            if self.fifo_len == 0
                && flip.producer_ready
                && lease_satisfied(self.read_epoch, flip.lease)
            {
                self.counters.retire_released = self.counters.retire_released.saturating_add(1);
                self.last_completed_fence = flip.fence;
                return true;
            }
            if self.fifo_len == MODEL_FIFO_LEN {
                // Signalling the newest (monotonically largest) fence implicitly
                // completes the queued older ones, so drop them — and release
                // every lease with them, or the next presentation would be gated
                // on a read whose waiter no longer exists.
                self.counters.overflow = self.counters.overflow.saturating_add(1);
                self.fifo = [None; MODEL_FIFO_LEN];
                self.fifo_len = 0;
                self.release_all(LeaseEnd::Teardown);
                self.last_completed_fence = flip.fence;
                return true;
            }
            self.fifo[self.fifo_len] = Some(flip);
            self.fifo_len += 1;
            false
        }

        /// Try to retire the head of the FIFO. Strictly head-of-line: a blocked
        /// head is never bypassed, because `SubmissionFenceId`s are watermarks
        /// to dxgkrnl and must complete monotonically.
        pub fn try_retire(&mut self) -> RetireStep {
            let Some(head) = self.fifo[0] else {
                return RetireStep::Empty;
            };
            if !head.producer_ready {
                return RetireStep::BlockedOnProducer;
            }
            if !lease_satisfied(self.read_epoch, head.lease) {
                self.counters.retire_blocked = self.counters.retire_blocked.saturating_add(1);
                return RetireStep::BlockedOnLease(head.lease);
            }
            let mut i = 1;
            while i < self.fifo_len {
                self.fifo[i - 1] = self.fifo[i];
                i += 1;
            }
            self.fifo[self.fifo_len - 1] = None;
            self.fifo_len -= 1;
            self.counters.retire_released = self.counters.retire_released.saturating_add(1);
            if fence_is_forward(self.last_completed_fence, head.fence) {
                self.last_completed_fence = head.fence;
            }
            RetireStep::Ready(head.fence)
        }

        /// Mark the producer half of the submission carrying `fence` retired.
        pub fn producer_retired(&mut self, fence: u32) {
            let mut i = 0;
            while i < self.fifo_len {
                if let Some(entry) = self.fifo[i].as_mut() {
                    if entry.fence == fence {
                        entry.producer_ready = true;
                    }
                }
                i += 1;
            }
        }

        /// Drop every pending submission (preempt / ResetFromTimeout / reset)
        /// and end every lease with it.
        pub fn abandon(&mut self) -> usize {
            let dropped = self.fifo_len;
            self.fifo = [None; MODEL_FIFO_LEN];
            self.fifo_len = 0;
            self.release_all(LeaseEnd::Teardown);
            dropped
        }

        pub const fn last_completed_fence(&self) -> u32 {
            self.last_completed_fence
        }
    }
}

/// Identity-only cadence rule for the direct scanout marker path.
///
/// A PRESENT marker records the completion boundary for the resource the app
/// just rendered.  It may request a host read immediately only when that exact
/// resource is already bound, because only then can a `RESOURCE_FLUSH` name
/// the marked frame.  A marker for another resource is retained for that
/// resource's bind edge; treating it as a generic dirty edge can overwrite the
/// currently bound resource's pending refresh and strand the real bind edge.
pub mod scanout_cadence {
    /// What the PRESENT-marker edge may do after recording its exact frame
    /// watermark.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum PresentMarkerAction {
        /// The marker names the active resource, or is the intentionally
        /// identity-free HERF edge (`resource_id == 0`).
        QueueImmediate,
        /// A future buffer's marker. Its own bind edge owns scheduling.
        DeferToBind,
    }

    /// Classify one PRESENT marker from exact resource identity only.
    ///
    /// No handle order, frame number, timing, or buffer-count inference is
    /// admissible here: a nonzero marker may queue only when it names the
    /// resource the host is currently bound to.
    pub const fn present_marker_action(
        resource_id: u32,
        active_resource_id: u32,
    ) -> PresentMarkerAction {
        if resource_id == 0 || resource_id == active_resource_id {
            PresentMarkerAction::QueueImmediate
        } else {
            PresentMarkerAction::DeferToBind
        }
    }

    /// Tiny host-test model of the identity rule. The production driver owns
    /// the actual completion-watermark table; this model deliberately carries
    /// only identity, pending-edge, and bind ownership so cadence regressions
    /// can be tested without KMD atomics or transport state.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct State {
        active_resource_id: u32,
        pending_resource_id: u32,
        refresh_queued: bool,
        marked_resources: [u32; 3],
    }

    impl State {
        pub const fn new() -> Self {
            Self {
                active_resource_id: 0,
                pending_resource_id: 0,
                refresh_queued: false,
                marked_resources: [0; 3],
            }
        }

        /// Record one marker, queuing only when [`present_marker_action`]
        /// permits it. The fixed table is sufficient for the A/B/C cadence
        /// model and deliberately has no allocation.
        pub fn present(&mut self, resource_id: u32) -> PresentMarkerAction {
            if resource_id != 0 {
                self.record_marker(resource_id);
            }
            let action = present_marker_action(resource_id, self.active_resource_id);
            if action == PresentMarkerAction::QueueImmediate {
                self.queue(resource_id);
            }
            action
        }

        /// Publish a bind and let only that resource consume its recorded
        /// marker. Returns whether the bind produced a refresh edge.
        pub fn bind(&mut self, resource_id: u32) -> bool {
            self.active_resource_id = resource_id;
            if resource_id != 0 && self.take_marker(resource_id) {
                self.queue(resource_id);
                true
            } else {
                false
            }
        }

        pub const fn active_resource_id(self) -> u32 {
            self.active_resource_id
        }

        pub const fn pending_resource_id(self) -> u32 {
            self.pending_resource_id
        }

        pub const fn refresh_queued(self) -> bool {
            self.refresh_queued
        }

        fn queue(&mut self, resource_id: u32) {
            self.pending_resource_id = resource_id;
            self.refresh_queued = true;
        }

        fn record_marker(&mut self, resource_id: u32) {
            for slot in &mut self.marked_resources {
                if *slot == resource_id {
                    return;
                }
            }
            for slot in &mut self.marked_resources {
                if *slot == 0 {
                    *slot = resource_id;
                    return;
                }
            }
            // The model has exactly the three rotating scanout identities used
            // by its tests. Replace the oldest model slot when a fourth is
            // introduced, matching bounded production bookkeeping.
            self.marked_resources[0] = resource_id;
        }

        fn take_marker(&mut self, resource_id: u32) -> bool {
            for slot in &mut self.marked_resources {
                if *slot == resource_id {
                    *slot = 0;
                    return true;
                }
            }
            false
        }
    }

    impl Default for State {
        fn default() -> Self {
            Self::new()
        }
    }
}

/// Pure decision for the PASSIVE worker after `VirtioGpu` checked one exact
/// direct presentation's producer boundary.
///
/// The host and Windows primary both wait for a D4b snapshot's exact producer
/// boundary. The KMD maps its transport-facing dispatch into [`Dispatch`] and
/// consumes this result without making the testable rule depend on a WDK type.
pub mod scanout_worker_bind {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Dispatch {
        Ready,
        Waiting,
        Abandoned,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Action {
        Deferred,
        AlreadyBound,
        IssueSyncSet,
        RejectAbandoned,
    }

    /// Whether this worker's direct-primary epoch may publish a SET or a
    /// primary address.  This is deliberately separate from [`Dispatch`]: the
    /// producer boundary says when a request is ready; it cannot make an older
    /// or identity-conflicting request become the host selection again.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum EpochPolicy {
        /// A newer presentation owns the binding and heartbeat already.
        Superseded,
        /// The same epoch names a different active identity.  It is terminal:
        /// guessing which identity wins would rebind behind an already-applied
        /// host selection.
        EqualConflict,
        /// The exact epoch/identity is already bound; preserve the ordinary
        /// already-bound path without another SET.
        AlreadyBound,
        /// This is a newer (or untracked) presentation and may stage normally.
        Newer,
    }

    /// An already-bound target cannot bypass [`Dispatch::Waiting`]. Once it is
    /// ready, it avoids a duplicate synchronous SET; an unbound target owns
    /// that SET instead.
    pub const fn decide(already_bound: bool, dispatch: Dispatch) -> Action {
        match dispatch {
            Dispatch::Waiting => Action::Deferred,
            Dispatch::Abandoned => Action::RejectAbandoned,
            Dispatch::Ready if already_bound => Action::AlreadyBound,
            Dispatch::Ready => Action::IssueSyncSet,
        }
    }

    /// Decide the direct-primary epoch before staging a worker SET.  `tracked`
    /// is false for the MMIO/desktop path, whose epoch is intentionally
    /// `NO_LEASE`; it must retain its existing identity-only behavior.
    pub const fn decide_epoch(
        tracked: bool,
        present_epoch: u64,
        bound_epoch: u64,
        same_active_identity: bool,
    ) -> EpochPolicy {
        if !tracked || present_epoch == 0 || bound_epoch == 0 {
            return EpochPolicy::Newer;
        }
        if present_epoch < bound_epoch {
            EpochPolicy::Superseded
        } else if present_epoch == bound_epoch {
            if same_active_identity {
                EpochPolicy::AlreadyBound
            } else {
                EpochPolicy::EqualConflict
            }
        } else {
            EpochPolicy::Newer
        }
    }
}

/// Descriptor-acceptance admission for direct presentation epochs.
///
/// Producer readiness and host-reader retirement determine WHEN a request may
/// bind; this smaller rule determines whether a request can ever travel
/// backward once a newer presentation SET has been accepted by the control
/// queue.  The floor advances at descriptor acceptance, not at host response,
/// so host errors and explicit retire paths cannot reopen an old epoch.
pub mod scanout_presentation_epoch {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Admission {
        /// No direct presentation epoch is attached (fallback/SET(0)).
        Untracked,
        /// Strictly newer than the highest accepted presentation SET epoch.
        Newer,
        /// Equal to or older than the accepted floor; must never reach wire.
        Superseded,
    }

    /// Decide whether a presentation epoch may be admitted. `0` is the
    /// untracked sentinel used by adapter-owned fallback/disable SETs.
    pub const fn decide(floor: u64, present_epoch: u64) -> Admission {
        if present_epoch == 0 {
            Admission::Untracked
        } else if present_epoch > floor {
            Admission::Newer
        } else {
            Admission::Superseded
        }
    }

    /// Advance only for an already-accepted tracked presentation descriptor.
    /// This is monotonic even when the host later rejects that descriptor.
    pub const fn advance_after_accept(floor: u64, present_epoch: u64) -> u64 {
        if present_epoch > floor {
            present_epoch
        } else {
            floor
        }
    }
}

/// Fixed publication-transaction decisions for a host-visible scanout SET.
///
/// One SET is not terminal when it succeeds: the matching `RESOURCE_FLUSH`
/// is the host-reader completion that makes the next presentation SET safe.
/// The KMD stores the full `ScanoutBindRequest` beside this state; this module
/// keeps the scalar matching and phase rules independently testable.
pub mod scanout_publish_txn {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct Key {
        pub resource_id: u32,
        pub present_epoch: u64,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Phase {
        /// The SET descriptor has reached the wire but has no terminal reply.
        SetInFlight,
        /// The SET succeeded and must be applied/armed before the flush exists.
        SetSucceeded,
        /// The exact resource/epoch `RESOURCE_FLUSH` is in flight.
        FlushInFlight,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct Transaction {
        pub key: Key,
        pub seq: u64,
        pub phase: Phase,
    }

    /// A fixed, value-only transaction slot.  `None` is the only state that
    /// permits another presentation SET; a timeout deliberately leaves the
    /// transaction intact because the host can still complete that descriptor.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct State {
        active: Option<Transaction>,
    }

    impl State {
        pub const fn new() -> Self {
            Self { active: None }
        }

        pub const fn active(&self) -> Option<Transaction> {
            self.active
        }

        /// Claim exactly one accepted SET. Returns false rather than replacing
        /// an active transaction; replacing it would permit a later wire SET
        /// before the earlier host reader has terminated.
        pub fn claim(&mut self, key: Key, seq: u64) -> bool {
            if self.active.is_some() || seq == 0 {
                return false;
            }
            self.active = Some(Transaction {
                key,
                seq,
                phase: Phase::SetInFlight,
            });
            true
        }

        /// A terminal SET response. Error clears only this exact transaction;
        /// success remains active until the matching flush terminal response.
        pub fn complete_set(&mut self, key: Key, seq: u64, ok: bool) -> bool {
            let Some(mut transaction) = self.active else {
                return false;
            };
            if transaction.key != key || transaction.seq != seq {
                return false;
            }
            if ok {
                transaction.phase = Phase::SetSucceeded;
                self.active = Some(transaction);
            } else {
                self.active = None;
            }
            true
        }

        /// The application path successfully placed the exact flush on wire.
        pub fn arm_flush(&mut self, key: Key) -> bool {
            let Some(mut transaction) = self.active else {
                return false;
            };
            if transaction.key != key || transaction.phase != Phase::SetSucceeded {
                return false;
            }
            transaction.phase = Phase::FlushInFlight;
            self.active = Some(transaction);
            true
        }

        /// Undo an arm that happened before the descriptor was accepted.  No
        /// host read exists in this transition, so the exact SET remains ready
        /// for a later flush attempt.
        pub fn rollback_flush(&mut self, key: Key) -> bool {
            let Some(mut transaction) = self.active else {
                return false;
            };
            if transaction.key != key || transaction.phase != Phase::FlushInFlight {
                return false;
            }
            transaction.phase = Phase::SetSucceeded;
            self.active = Some(transaction);
            true
        }

        /// A flush response clears only the exact resource/epoch it covered.
        pub fn complete_flush(&mut self, key: Key) -> bool {
            let Some(transaction) = self.active else {
                return false;
            };
            if transaction.key != key || transaction.phase != Phase::FlushInFlight {
                return false;
            }
            self.active = None;
            true
        }

        /// Exact no-flush termination: stale apply, host-unbound/dead refresh,
        /// enqueue failure, cancellation, or a completed unbind barrier.
        pub fn cancel_exact(&mut self, key: Key) -> bool {
            if self
                .active
                .is_some_and(|transaction| transaction.key == key)
            {
                self.active = None;
                true
            } else {
                false
            }
        }
    }
}

/// Fixed-storage state for a completion-ordered fast scanout bind.
///
/// The producer-completion predicate is supplied by the KMD because it reads
/// transport state. This module owns the bounded coalescing and cancellation
/// rule: an unready request occupies an oldest liveness frontier plus one
/// coalesced latest slot, and a destroyed resource can never be promoted later.
pub mod scanout_fast_bind {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct Request {
        pub resource_id: u32,
        pub boundary: u64,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Step {
        /// The exact producer boundary is ready, so issue this bind now.
        Issue(Request),
        /// Retained in bounded fixed storage for a later completion edge.
        Deferred,
        /// No deferred request is present, or its resource was cancelled.
        Empty,
    }

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct State {
        earliest: Option<Request>,
        latest: Option<Request>,
    }

    impl State {
        pub const fn new() -> Self {
            Self {
                earliest: None,
                latest: None,
            }
        }

        /// Submit a request. `ready` and `live` must describe this exact
        /// request's producer boundary and resource identity.
        pub fn submit(&mut self, request: Request, ready: bool, live: bool) -> Step {
            if !live {
                return Step::Empty;
            }
            if ready {
                self.earliest = None;
                self.latest = None;
                Step::Issue(request)
            } else {
                if self.earliest.is_none() {
                    self.earliest = Some(request);
                } else {
                    self.latest = Some(request);
                }
                Step::Deferred
            }
        }

        /// Re-evaluate the retained request after a producer completion.
        pub fn on_completion(
            &mut self,
            earliest_ready: bool,
            earliest_live: bool,
            _latest_ready: bool,
            latest_live: bool,
        ) -> Step {
            let Some(earliest) = self.earliest else {
                return Step::Empty;
            };
            if !earliest_live {
                self.earliest = self.latest.take();
                Step::Empty
            } else if earliest_ready {
                let latest = self.latest.take();
                // A completion first advances the oldest liveness frontier.
                // Keep any live successor even when it is already ready: the
                // SET issued below creates the next completion pass, which can
                // then issue that exact successor without silently losing it.
                self.earliest = latest.filter(|_| latest_live);
                Step::Issue(earliest)
            } else {
                Step::Deferred
            }
        }

        /// Exact DestroyAllocation cancellation. A different resource must not
        /// erase the retained request merely because it happened to retire.
        pub fn cancel_resource(&mut self, resource_id: u32) -> bool {
            let mut cancelled = false;
            if self
                .earliest
                .is_some_and(|request| request.resource_id == resource_id)
            {
                self.earliest = self.latest.take();
                cancelled = true;
            }
            if self
                .latest
                .is_some_and(|request| request.resource_id == resource_id)
            {
                self.latest = None;
                cancelled = true;
            }
            cancelled
        }

        pub const fn deferred(self) -> Option<Request> {
            self.earliest
        }
    }
}

/// Pure, bounded state transitions for completion-ordered scanout refreshes.
///
/// The KMD supplies producer readiness and tagged-stream liveness because both
/// depend on transport state. This module owns only the state machine, so its
/// liveness and coalescing rules are executable in the host test crate.
pub mod scanout_refresh {
    /// One exact dirty edge. Resource identity and producer boundary are an
    /// inseparable pair: coalescing must never attach one resource to another
    /// frame's completion.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct Marker {
        resource_id: u32,
        boundary: u64,
    }

    impl Marker {
        pub const fn new(resource_id: u32, boundary: u64) -> Self {
            Self {
                resource_id,
                boundary,
            }
        }

        pub const fn resource_id(self) -> u32 {
            self.resource_id
        }

        pub const fn boundary(self) -> u64 {
            self.boundary
        }
    }

    /// Two fixed marker slots: an oldest liveness frontier and one coalesced
    /// latest marker. This is deliberately not an unbounded queue and contains
    /// no allocation or synchronization.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct State {
        earliest: Option<Marker>,
        latest: Option<Marker>,
    }

    impl State {
        pub const fn new() -> Self {
            Self {
                earliest: None,
                latest: None,
            }
        }

        pub fn clear(&mut self) {
            self.earliest = None;
            self.latest = None;
        }

        /// Record one exact dirty edge. An already-ready current marker is an
        /// immediate refresh request and supersedes outstanding deferred work;
        /// retaining the older frontier would strand the ready edge if the
        /// used-ring becomes idle.
        pub fn note(&mut self, marker: Marker, ready: bool) -> bool {
            if ready {
                self.clear();
                true
            } else {
                self.note_pending(marker);
                false
            }
        }

        /// Record an unready marker without testing transport readiness.
        pub fn note_pending(&mut self, marker: Marker) {
            if self.earliest.is_none() {
                self.earliest = Some(marker);
            } else {
                self.latest = Some(marker);
            }
        }

        pub const fn earliest(self) -> Option<Marker> {
            self.earliest
        }

        pub const fn latest(self) -> Option<Marker> {
            self.latest
        }

        /// Consume one read-safe marker. If the coalesced latest marker is
        /// ready too it supersedes the frontier; otherwise the frontier is
        /// returned and the latest becomes the next frontier.
        pub fn take_ready(&mut self, earliest_ready: bool, latest_ready: bool) -> Option<Marker> {
            let earliest = self.earliest?;
            if !earliest_ready {
                return None;
            }
            match self.latest {
                Some(latest) if latest_ready => {
                    self.clear();
                    Some(latest)
                }
                Some(latest) => {
                    self.earliest = Some(latest);
                    self.latest = None;
                    Some(earliest)
                }
                None => {
                    self.earliest = None;
                    Some(earliest)
                }
            }
        }

        /// Drop explicitly cancelled markers, then promote a live latest
        /// marker only after both slots have been filtered. `marker_live` must
        /// return true for ordinary wire boundaries and for live tagged streams.
        pub fn discard_cancelled<F>(&mut self, marker_live: F)
        where
            F: Fn(Marker) -> bool,
        {
            let earliest = self.earliest.filter(|marker| marker_live(*marker));
            let latest = self.latest.filter(|marker| marker_live(*marker));
            if earliest.is_some() {
                self.earliest = earliest;
                self.latest = latest;
            } else {
                self.earliest = latest;
                self.latest = None;
            }
        }
    }

    impl Default for State {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(test)]
mod scanout_cadence_tests {
    use super::scanout_cadence::{PresentMarkerAction, State};

    #[test]
    fn present_before_bind_defers_exact_future_resource_to_its_bind() {
        let mut state = State::new();
        assert!(!state.bind(10));

        assert_eq!(state.present(20), PresentMarkerAction::DeferToBind);
        assert!(!state.refresh_queued());

        assert!(state.bind(20));
        assert_eq!(state.pending_resource_id(), 20);
    }

    #[test]
    fn bind_before_present_queues_the_now_active_resource() {
        let mut state = State::new();
        assert!(!state.bind(10));

        assert_eq!(state.present(10), PresentMarkerAction::QueueImmediate);
        assert!(state.refresh_queued());
        assert_eq!(state.pending_resource_id(), 10);
    }

    #[test]
    fn already_bound_represent_is_an_exact_immediate_refresh() {
        let mut state = State::new();
        assert!(!state.bind(10));
        assert_eq!(state.present(10), PresentMarkerAction::QueueImmediate);
        assert_eq!(state.active_resource_id(), 10);
        assert_eq!(state.pending_resource_id(), 10);
    }

    #[test]
    fn identity_free_herf_queues_whatever_is_bound() {
        let mut state = State::new();
        assert!(!state.bind(10));

        assert_eq!(state.present(0), PresentMarkerAction::QueueImmediate);
        assert!(state.refresh_queued());
        assert_eq!(state.pending_resource_id(), 0);
    }

    #[test]
    fn rotating_abc_markers_cannot_starve_each_resources_bind_edge() {
        let mut state = State::new();
        assert!(!state.bind(1));

        // B and C are future buffers. Neither may overwrite A's current
        // refresh, and each remains available to exactly its own bind.
        assert_eq!(state.present(2), PresentMarkerAction::DeferToBind);
        assert_eq!(state.present(3), PresentMarkerAction::DeferToBind);
        assert_eq!(state.present(1), PresentMarkerAction::QueueImmediate);
        assert_eq!(state.pending_resource_id(), 1);

        assert!(state.bind(2));
        assert_eq!(state.pending_resource_id(), 2);
        assert!(state.bind(3));
        assert_eq!(state.pending_resource_id(), 3);
    }
}

#[cfg(test)]
mod scanout_worker_bind_tests {
    use super::scanout_worker_bind::{decide, decide_epoch, Action, Dispatch, EpochPolicy};

    #[test]
    fn already_bound_target_still_defers_while_its_producer_is_unready() {
        assert_eq!(decide(true, Dispatch::Waiting), Action::Deferred);
    }

    #[test]
    fn ready_already_bound_target_does_not_issue_a_duplicate_sync_set() {
        assert_eq!(decide(true, Dispatch::Ready), Action::AlreadyBound);
    }

    #[test]
    fn ready_unbound_target_owns_the_sync_set() {
        assert_eq!(decide(false, Dispatch::Ready), Action::IssueSyncSet);
    }

    #[test]
    fn abandoned_producer_stays_terminal() {
        assert_eq!(decide(true, Dispatch::Abandoned), Action::RejectAbandoned);
    }

    #[test]
    fn older_worker_epoch_is_terminal_and_cannot_rebind() {
        assert_eq!(decide_epoch(true, 41, 42, false), EpochPolicy::Superseded);
    }

    #[test]
    fn equal_epoch_with_conflicting_identity_is_terminal() {
        assert_eq!(
            decide_epoch(true, 42, 42, false),
            EpochPolicy::EqualConflict
        );
    }

    #[test]
    fn equal_epoch_with_same_identity_uses_already_bound_path() {
        assert_eq!(decide_epoch(true, 42, 42, true), EpochPolicy::AlreadyBound);
    }

    #[test]
    fn newer_worker_epoch_may_stage_normally() {
        assert_eq!(decide_epoch(true, 43, 42, false), EpochPolicy::Newer);
    }
}

#[cfg(test)]
mod scanout_presentation_epoch_tests {
    use super::scanout_presentation_epoch::{advance_after_accept, decide, Admission};

    #[test]
    fn newer_epoch_is_admitted_and_advances_only_after_acceptance() {
        assert_eq!(decide(1056, 1057), Admission::Newer);
        assert_eq!(advance_after_accept(1056, 1057), 1057);
    }

    #[test]
    fn equal_or_older_epoch_is_terminally_superseded() {
        assert_eq!(decide(1056, 1056), Admission::Superseded);
        assert_eq!(decide(1056, 1055), Admission::Superseded);
        assert_eq!(advance_after_accept(1056, 1055), 1056);
    }

    #[test]
    fn accepted_1056_rejects_later_deferred_1055() {
        let floor = advance_after_accept(0, 1056);
        assert_eq!(floor, 1056);
        assert_eq!(decide(floor, 1055), Admission::Superseded);
    }

    #[test]
    fn untracked_fallback_does_not_participate_in_the_floor() {
        assert_eq!(decide(1056, 0), Admission::Untracked);
        assert_eq!(advance_after_accept(1056, 0), 1056);
    }
}

#[cfg(test)]
mod scanout_publish_txn_tests {
    use super::scanout_publish_txn::{Key, Phase, State, Transaction};

    const A: Key = Key {
        resource_id: 0x121,
        present_epoch: 41,
    };
    const B: Key = Key {
        resource_id: 0x128,
        present_epoch: 42,
    };

    #[test]
    fn active_transaction_blocks_a_ready_second_set() {
        let mut state = State::new();
        assert!(state.claim(A, 7));
        assert!(!state.claim(B, 8));
        assert_eq!(
            state.active(),
            Some(Transaction {
                key: A,
                seq: 7,
                phase: Phase::SetInFlight,
            })
        );
    }

    #[test]
    fn exact_flush_completion_clears_the_transaction() {
        let mut state = State::new();
        assert!(state.claim(A, 7));
        assert!(state.complete_set(A, 7, true));
        assert!(state.arm_flush(A));
        assert!(state.complete_flush(A));
        assert_eq!(state.active(), None);
    }

    #[test]
    fn wrong_resource_or_epoch_cannot_clear_the_transaction() {
        let mut state = State::new();
        assert!(state.claim(A, 7));
        assert!(state.complete_set(A, 7, true));
        assert!(state.arm_flush(A));
        assert!(!state.complete_flush(Key {
            resource_id: B.resource_id,
            present_epoch: A.present_epoch,
        }));
        assert!(!state.complete_flush(Key {
            resource_id: A.resource_id,
            present_epoch: B.present_epoch,
        }));
        assert_eq!(
            state.active(),
            Some(Transaction {
                key: A,
                seq: 7,
                phase: Phase::FlushInFlight,
            })
        );
    }

    #[test]
    fn set_error_is_an_exact_terminal_path() {
        let mut state = State::new();
        assert!(state.claim(A, 7));
        assert!(state.complete_set(A, 7, false));
        assert_eq!(state.active(), None);
    }

    #[test]
    fn timeout_keeps_late_success_transaction_until_its_flush() {
        let mut state = State::new();
        assert!(state.claim(A, 7));
        // Timeout detaches only the caller. It is intentionally no transition.
        assert_eq!(
            state.active(),
            Some(Transaction {
                key: A,
                seq: 7,
                phase: Phase::SetInFlight,
            })
        );
        assert!(state.complete_set(A, 7, true));
        assert!(state.arm_flush(A));
        assert!(state.complete_flush(A));
        assert_eq!(state.active(), None);
    }

    #[test]
    fn prepublication_flush_refusal_rolls_back_exactly() {
        let mut state = State::new();
        assert!(state.claim(A, 7));
        assert!(state.complete_set(A, 7, true));
        assert!(state.arm_flush(A));
        assert!(state.rollback_flush(A));
        assert_eq!(
            state.active(),
            Some(Transaction {
                key: A,
                seq: 7,
                phase: Phase::SetSucceeded,
            })
        );
        assert!(!state.rollback_flush(A));
        assert!(state.arm_flush(A));
        assert!(state.complete_flush(A));
    }
}

#[cfg(test)]
mod scanout_fast_bind_tests {
    use super::scanout_fast_bind::{Request, State, Step};

    const A: Request = Request {
        resource_id: 10,
        boundary: 100,
    };
    const B: Request = Request {
        resource_id: 20,
        boundary: 200,
    };

    #[test]
    fn unready_bind_waits_for_its_exact_producer_completion() {
        let mut state = State::new();
        assert_eq!(state.submit(A, false, true), Step::Deferred);
        assert_eq!(
            state.on_completion(false, true, false, true),
            Step::Deferred
        );
        assert_eq!(state.on_completion(true, true, false, true), Step::Issue(A));
        assert_eq!(state.deferred(), None);
    }

    #[test]
    fn already_retired_boundary_issues_without_consuming_storage() {
        let mut state = State::new();
        assert_eq!(state.submit(A, true, true), Step::Issue(A));
        assert_eq!(state.deferred(), None);
    }

    #[test]
    fn ready_successor_waits_behind_the_oldest_frontier() {
        let mut state = State::new();
        assert_eq!(state.submit(A, false, true), Step::Deferred);
        assert_eq!(state.submit(B, false, true), Step::Deferred);
        assert_eq!(state.deferred(), Some(A));
        assert_eq!(state.on_completion(true, true, true, true), Step::Issue(A));
        assert_eq!(state.deferred(), Some(B));
        // The SET for A causes the next completion pass. B was already ready,
        // but it remains exact and is issued there rather than being discarded.
        assert_eq!(state.on_completion(true, true, true, true), Step::Issue(B));
        assert_eq!(state.deferred(), None);
    }

    #[test]
    fn continuous_submits_cannot_starve_the_oldest_producer() {
        let mut state = State::new();
        assert_eq!(state.submit(A, false, true), Step::Deferred);
        // Every newer frame arrives before A is ready; only the trailing slot
        // changes, so A remains a liveness frontier.
        for boundary in [201, 202, 203, 204] {
            assert_eq!(
                state.submit(
                    Request {
                        resource_id: 20,
                        boundary,
                    },
                    false,
                    true,
                ),
                Step::Deferred
            );
            assert_eq!(state.deferred(), Some(A));
        }
        // The oldest completion always progresses first. The bounded trailing
        // slot remains the newest coalesced request and advances on the SET's
        // following completion pass.
        assert_eq!(state.on_completion(true, true, true, true), Step::Issue(A));
        let newest = Request {
            resource_id: 20,
            boundary: 204,
        };
        assert_eq!(state.deferred(), Some(newest));
        assert_eq!(
            state.on_completion(true, true, true, true),
            Step::Issue(newest)
        );
        assert_eq!(state.deferred(), None);
    }

    #[test]
    fn ready_frontier_does_not_issue_an_unready_latest() {
        let mut state = State::new();
        assert_eq!(state.submit(A, false, true), Step::Deferred);
        assert_eq!(state.submit(B, false, true), Step::Deferred);
        assert_eq!(state.on_completion(true, true, false, true), Step::Issue(A));
        assert_eq!(state.deferred(), Some(B));
    }

    #[test]
    fn ready_submit_explicitly_supersedes_retained_frontier() {
        let mut state = State::new();
        assert_eq!(state.submit(A, false, true), Step::Deferred);
        // A currently-ready request is an explicit newer selection, distinct
        // from completion service. The production path accounts for this
        // supersession in its timeline/counter before issuing B.
        assert_eq!(state.submit(B, true, true), Step::Issue(B));
        assert_eq!(state.deferred(), None);
    }

    #[test]
    fn cancelled_successor_cannot_issue_after_oldest_progresses() {
        let mut state = State::new();
        assert_eq!(state.submit(A, false, true), Step::Deferred);
        assert_eq!(state.submit(B, false, true), Step::Deferred);
        assert_eq!(state.on_completion(true, true, true, true), Step::Issue(A));
        assert_eq!(state.deferred(), Some(B));
        assert!(state.cancel_resource(B.resource_id));
        assert_eq!(state.on_completion(true, true, true, true), Step::Empty);
    }

    #[test]
    fn exact_destroy_cancels_before_a_late_completion_can_issue() {
        let mut state = State::new();
        assert_eq!(state.submit(A, false, true), Step::Deferred);
        assert!(!state.cancel_resource(B.resource_id));
        assert!(state.cancel_resource(A.resource_id));
        assert_eq!(state.on_completion(true, true, true, true), Step::Empty);
    }

    #[test]
    fn dead_producer_is_dropped_not_reinterpreted_as_ready() {
        let mut state = State::new();
        assert_eq!(state.submit(A, false, true), Step::Deferred);
        assert_eq!(state.on_completion(false, false, false, false), Step::Empty);
        assert_eq!(state.deferred(), None);
    }
}

#[cfg(test)]
mod scanout_refresh_tests {
    use super::scanout_refresh::{Marker, State};

    #[test]
    fn frontier_is_not_overwritten_by_a_hot_present_stream() {
        let mut state = State::new();
        state.note_pending(Marker::new(11, 100));
        state.note_pending(Marker::new(22, 200));
        state.note_pending(Marker::new(33, 300));

        assert_eq!(state.earliest(), Some(Marker::new(11, 100)));
        assert_eq!(state.latest(), Some(Marker::new(33, 300)));
        assert_eq!(state.take_ready(false, false), None);
    }

    #[test]
    fn refused_old_frontier_cannot_strand_promoted_newer_marker() {
        let mut state = State::new();
        state.note_pending(Marker::new(11, 100));
        state.note_pending(Marker::new(22, 200));

        assert_eq!(state.take_ready(true, false), Some(Marker::new(11, 100)));
        assert_eq!(state.earliest(), Some(Marker::new(22, 200)));
        assert_eq!(state.latest(), None);
        // The KMD executor may reject 11 after a different accepted bind. Its
        // refusal cannot erase the promoted marker; a later DPC still returns
        // the exact resource/boundary pair for 22.
        assert_eq!(state.take_ready(true, false), Some(Marker::new(22, 200)));
        assert_eq!(state.earliest(), None);
    }

    #[test]
    fn ready_latest_supersedes_frontier_without_crossing_identity() {
        let mut state = State::new();
        state.note_pending(Marker::new(11, 100));
        state.note_pending(Marker::new(22, 200));

        assert_eq!(state.take_ready(true, true), Some(Marker::new(22, 200)));
        assert_eq!(state.earliest(), None);
        assert_eq!(state.latest(), None);
    }

    #[test]
    fn ready_current_marker_supersedes_waiting_frontier_without_a_future_dpc() {
        let mut state = State::new();
        state.note_pending(Marker::new(11, 100));

        assert!(state.note(Marker::new(22, 200), true));
        assert_eq!(state.earliest(), None);
        assert_eq!(state.latest(), None);
    }

    #[test]
    fn bind_boundary_precedes_same_resource_represent_boundary() {
        let mut state = State::new();

        // The KMD publishes an accepted bind and inserts its carried W1 under
        // the same notification lock that a re-present uses to classify the
        // resource and insert W2.  W1 must therefore remain the frontier and
        // W2 the coalesced successor, even when both name the same resource.
        assert!(!state.note(Marker::new(11, 100), false));
        assert!(!state.note(Marker::new(11, 200), false));

        assert_eq!(state.take_ready(true, false), Some(Marker::new(11, 100)));
        assert_eq!(state.earliest(), Some(Marker::new(11, 200)));
        assert_eq!(state.latest(), None);
    }

    #[test]
    fn cancelled_frontier_promotes_live_latest() {
        let mut state = State::new();
        state.note_pending(Marker::new(11, 100));
        state.note_pending(Marker::new(22, 200));

        state.discard_cancelled(|marker| marker.boundary() == 200);
        assert_eq!(state.earliest(), Some(Marker::new(22, 200)));
        assert_eq!(state.latest(), None);
    }

    #[test]
    fn two_cancelled_markers_leave_no_frontier() {
        let mut state = State::new();
        state.note_pending(Marker::new(11, 100));
        state.note_pending(Marker::new(22, 200));

        state.discard_cancelled(|_| false);
        assert_eq!(state.earliest(), None);
        assert_eq!(state.latest(), None);
    }
}

#[cfg(test)]
mod scanout_lease_tests {
    use super::scanout_lease::*;

    /// Drive one steady-state presentation: mint, bind, flush, response.
    fn present(tracker: &mut LeaseTracker, resource: u32, rebound: bool, fence: u32) -> u64 {
        let epoch = tracker.mint_presentation();
        tracker.submit(PendingFlip {
            fence,
            producer_ready: false,
            lease: epoch,
        });
        tracker.publish_bind(epoch, resource, rebound);
        epoch
    }

    // 1. normal bind -> flush queued -> response -> reuse
    #[test]
    fn normal_bind_flush_response_releases_the_flip() {
        let mut t = LeaseTracker::new();
        let epoch = present(&mut t, 191, true, 10);
        t.producer_retired(10);
        assert_eq!(t.try_retire(), RetireStep::BlockedOnLease(epoch));

        let token = t.issue_flush();
        assert_eq!(token, epoch);
        t.complete_flush(token, true);
        assert_eq!(t.try_retire(), RetireStep::Ready(10));
        assert_eq!(t.counters().ended_read, 1);
        assert_eq!(t.counters().ended_superseded, 0);
    }

    // 2. producer completion before reader completion
    #[test]
    fn producer_first_still_waits_for_the_reader() {
        let mut t = LeaseTracker::new();
        let epoch = present(&mut t, 191, true, 10);
        t.producer_retired(10);
        for _ in 0..4 {
            assert_eq!(t.try_retire(), RetireStep::BlockedOnLease(epoch));
        }
        let token = t.issue_flush();
        t.complete_flush(token, true);
        assert_eq!(t.try_retire(), RetireStep::Ready(10));
        assert_eq!(t.counters().retire_blocked, 4);
    }

    // 3. reader completion before producer completion
    #[test]
    fn reader_first_still_waits_for_the_producer() {
        let mut t = LeaseTracker::new();
        present(&mut t, 191, true, 10);
        let token = t.issue_flush();
        t.complete_flush(token, true);
        assert_eq!(t.try_retire(), RetireStep::BlockedOnProducer);
        t.producer_retired(10);
        assert_eq!(t.try_retire(), RetireStep::Ready(10));
    }

    // 4. later bind supersedes an epoch that never got a read
    #[test]
    fn a_later_bind_supersedes_an_unread_epoch() {
        let mut t = LeaseTracker::new();
        let first = present(&mut t, 191, true, 10);
        t.producer_retired(10);
        assert_eq!(t.try_retire(), RetireStep::BlockedOnLease(first));

        // No flush ever named `first`; the next flip binds the other buffer.
        let second = present(&mut t, 195, true, 11);
        t.producer_retired(11);
        assert_eq!(t.try_retire(), RetireStep::Ready(10));
        assert_eq!(t.try_retire(), RetireStep::BlockedOnLease(second));
        assert_eq!(t.counters().ended_superseded, 1);
        assert_eq!(t.counters().ended_read, 0);
    }

    // 5. later bind after a read was already queued for the previous epoch
    #[test]
    fn a_queued_read_and_a_later_bind_agree() {
        let mut t = LeaseTracker::new();
        let first = present(&mut t, 191, true, 10);
        let token = t.issue_flush();
        assert_eq!(token, first);

        let second = present(&mut t, 195, true, 11);
        // The bind superseded `first` before its response came back...
        assert!(lease_satisfied(t.read_epoch(), first));
        // ...and the late response is then inert, not a backwards step.
        t.complete_flush(token, true);
        assert_eq!(t.counters().stale_completions, 1);
        assert!(!lease_satisfied(t.read_epoch(), second));
    }

    // 6. repeated presentations of the same still-bound resource
    #[test]
    fn repeated_presents_of_one_buffer_get_distinct_epochs() {
        let mut t = LeaseTracker::new();
        let a = present(&mut t, 191, true, 10);
        let b = present(&mut t, 191, false, 11);
        let c = present(&mut t, 191, false, 12);
        assert!(a < b && b < c);
        t.producer_retired(10);
        t.producer_retired(11);
        t.producer_retired(12);

        // A read taken while `c` is bound covers all three; a read taken when
        // only `a` had been published covers only `a`.
        let mut t2 = LeaseTracker::new();
        let a2 = present(&mut t2, 191, true, 10);
        let token = t2.issue_flush();
        let b2 = present(&mut t2, 191, false, 11);
        t2.producer_retired(10);
        t2.producer_retired(11);
        t2.complete_flush(token, true);
        assert_eq!(t2.try_retire(), RetireStep::Ready(10));
        assert_eq!(t2.try_retire(), RetireStep::BlockedOnLease(b2));
        assert!(a2 < b2);

        // No supersede happened: the resource never changed.
        assert_eq!(t2.counters().ended_superseded, 0);
    }

    // 7. two resources alternating faster than the host reads
    #[test]
    fn two_resources_alternating_never_release_an_unread_epoch() {
        let mut t = LeaseTracker::new();
        let mut fence = 100u32;
        let mut leases = [0u64; 6];
        for (i, lease) in leases.iter_mut().enumerate() {
            let resource = if i % 2 == 0 { 191 } else { 195 };
            *lease = present(&mut t, resource, true, fence);
            t.producer_retired(fence);
            fence += 1;
        }
        // Every retire is either delivered because a later bind proved no read
        // remains, or blocked. Nothing is delivered while its own epoch is both
        // unread and still the bound one.
        let mut delivered = 0;
        loop {
            match t.try_retire() {
                RetireStep::Ready(_) => delivered += 1,
                _ => break,
            }
        }
        // The last presentation is still bound and unread, so it must be held.
        assert_eq!(delivered, leases.len() - 1);
        assert_eq!(t.try_retire(), RetireStep::BlockedOnLease(leases[5]));
        assert!(!lease_satisfied(t.read_epoch(), leases[5]));
    }

    // 8. coalesced refreshes
    #[test]
    fn one_read_covers_every_epoch_published_before_it() {
        let mut t = LeaseTracker::new();
        let e1 = present(&mut t, 191, true, 10);
        let e2 = present(&mut t, 191, false, 11);
        let e3 = present(&mut t, 191, false, 12);
        t.producer_retired(10);
        t.producer_retired(11);
        t.producer_retired(12);
        let token = t.issue_flush();
        assert_eq!(token, e3);
        t.complete_flush(token, true);
        assert!(lease_satisfied(t.read_epoch(), e1));
        assert!(lease_satisfied(t.read_epoch(), e2));
        assert!(lease_satisfied(t.read_epoch(), e3));
        assert_eq!(t.try_retire(), RetireStep::Ready(10));
        assert_eq!(t.try_retire(), RetireStep::Ready(11));
        assert_eq!(t.try_retire(), RetireStep::Ready(12));
        assert_eq!(t.counters().read_queued, 1);
    }

    // 9. stale completion token
    #[test]
    fn a_stale_token_never_moves_the_watermark_backwards() {
        let mut t = LeaseTracker::new();
        let e1 = present(&mut t, 191, true, 10);
        let stale = t.issue_flush();
        let e2 = present(&mut t, 195, true, 11);
        t.publish_bind(e2, 195, true);
        let fresh = t.issue_flush();
        t.complete_flush(fresh, true);
        let after = t.read_epoch();
        t.complete_flush(stale, true);
        assert_eq!(t.read_epoch(), after);
        assert!(lease_satisfied(t.read_epoch(), e1));
        assert_eq!(t.counters().stale_completions, 1);
        // Zero is never a valid token either.
        t.complete_flush(NO_LEASE, true);
        assert_eq!(t.read_epoch(), after);
        assert_eq!(t.counters().stale_completions, 2);
    }

    // 10. response error and enqueue failure
    #[test]
    fn an_error_response_terminates_the_lease_without_claiming_a_publish() {
        let mut t = LeaseTracker::new();
        let epoch = present(&mut t, 191, true, 10);
        t.producer_retired(10);
        let token = t.issue_flush();
        t.complete_flush(token, false);
        assert_eq!(t.try_retire(), RetireStep::Ready(10));
        assert_eq!(t.counters().ended_cancelled, 1);
        assert_eq!(t.counters().ended_read, 0);
        assert!(lease_satisfied(t.read_epoch(), epoch));
    }

    #[test]
    fn an_enqueue_failure_cancels_exactly_the_epoch_it_named() {
        let mut t = LeaseTracker::new();
        let first = present(&mut t, 191, true, 10);
        t.producer_retired(10);
        // The flush could not be enqueued: no host read exists for `first`.
        let token = t.issue_flush();
        t.end_leases_through(token, LeaseEnd::Cancelled);
        assert_eq!(t.try_retire(), RetireStep::Ready(10));

        let second = present(&mut t, 195, true, 11);
        t.producer_retired(11);
        // The cancellation released `first` and NOTHING beyond it.
        assert_eq!(t.try_retire(), RetireStep::BlockedOnLease(second));
        assert!(first < second);
        assert_eq!(t.counters().ended_cancelled, 1);
    }

    // 11. reset / teardown with outstanding leases
    #[test]
    fn teardown_releases_every_outstanding_lease() {
        let mut t = LeaseTracker::new();
        present(&mut t, 191, true, 10);
        present(&mut t, 195, true, 11);
        t.producer_retired(10);
        t.producer_retired(11);
        let dropped = t.abandon();
        assert_eq!(dropped, 2);
        assert_eq!(t.try_retire(), RetireStep::Empty);

        // A fresh presentation after teardown is gated again, not pre-released.
        let epoch = present(&mut t, 191, true, 12);
        t.producer_retired(12);
        assert_eq!(t.try_retire(), RetireStep::BlockedOnLease(epoch));
        assert_eq!(t.counters().ended_teardown, 1);
    }

    // 12. WDDM FIFO monotonicity and fence wrap
    #[test]
    fn a_blocked_head_is_never_bypassed() {
        let mut t = LeaseTracker::new();
        let first = present(&mut t, 191, true, 10);
        let second = present(&mut t, 191, false, 11);
        t.producer_retired(11);
        // The younger fence is fully ready, but the head is not: no bypass.
        assert_eq!(t.try_retire(), RetireStep::BlockedOnProducer);
        t.producer_retired(10);
        // Still the HEAD's lease that is reported, not the ready younger one.
        assert_eq!(t.try_retire(), RetireStep::BlockedOnLease(first));
        assert!(first < second);
        assert_eq!(t.last_completed_fence(), 0);

        // Releasing the head's lease releases both, in submission order.
        let token = t.issue_flush();
        t.complete_flush(token, true);
        assert_eq!(t.try_retire(), RetireStep::Ready(10));
        assert_eq!(t.try_retire(), RetireStep::Ready(11));
        assert_eq!(t.last_completed_fence(), 11);
    }

    #[test]
    fn fence_forwardness_survives_the_u32_wrap() {
        assert!(fence_is_forward(0, 1));
        assert!(!fence_is_forward(1, 1));
        assert!(!fence_is_forward(2, 1));
        // Wrap: 0xFFFF_FFFF -> 0 is forward by one.
        assert!(fence_is_forward(u32::MAX, 0));
        assert!(fence_is_forward(u32::MAX - 1, 3));
        // Half the space away is NOT forward.
        assert!(!fence_is_forward(0, 0x8000_0000));
        assert!(fence_is_forward(0, 0x7FFF_FFFF));
    }

    #[test]
    fn epoch_minting_saturates_instead_of_wrapping_to_no_lease() {
        assert_eq!(next_epoch(0), FIRST_EPOCH);
        assert_eq!(next_epoch(u64::MAX - 1), u64::MAX);
        assert_eq!(next_epoch(u64::MAX), u64::MAX);
        assert!(!lease_satisfied(u64::MAX - 1, u64::MAX));
        assert!(lease_satisfied(u64::MAX, u64::MAX));
    }

    // 13. bounded-state exhaustion
    #[test]
    fn fifo_exhaustion_degrades_loudly_and_releases_every_lease() {
        let mut t = LeaseTracker::new();
        let mut fence = 200u32;
        for _ in 0..MODEL_FIFO_LEN {
            present(&mut t, 191, false, fence);
            fence += 1;
        }
        assert_eq!(t.pending_len(), MODEL_FIFO_LEN);
        // One more overflows.
        let epoch = t.mint_presentation();
        let signal_now = t.submit(PendingFlip {
            fence,
            producer_ready: false,
            lease: epoch,
        });
        assert!(signal_now);
        assert_eq!(t.pending_len(), 0);
        assert_eq!(t.counters().overflow, 1);
        assert!(lease_satisfied(t.read_epoch(), epoch));
        assert_eq!(t.try_retire(), RetireStep::Empty);
    }

    // The CRTC_VSYNC edge: the address a VSync may report is gated by the same
    // lease that gates DMA_COMPLETED.
    #[test]
    fn the_displayed_primary_address_waits_for_the_same_lease() {
        let mut t = LeaseTracker::new();
        let epoch = present(&mut t, 191, true, 10);
        t.arm_primary(0xDEAD_0000, epoch);
        assert_eq!(t.displayed_primary(), 0);

        let token = t.issue_flush();
        t.complete_flush(token, true);
        assert_eq!(t.displayed_primary(), 0xDEAD_0000);
        assert_eq!(t.counters().primary_published, 1);
    }

    // ── The ownership gate (D2). These are the SHIPPED decisions: the driver's
    // flush executor calls `surplus_republish` with its three atomics, so a
    // regression here is a black frame or a frozen desktop, not a model bug.

    #[test]
    fn the_first_read_of_a_binding_is_never_surplus() {
        // Bound generation 7 published, nothing read yet.
        assert!(!surplus_republish(true, 6, 7, 7));
        assert!(!surplus_republish(true, 6, 7, 9));
        assert!(!surplus_republish(true, NO_LEASE, 1, 1));
    }

    #[test]
    fn a_reread_with_a_newer_presentation_outstanding_is_surplus() {
        // Generation 7 was read (read >= bound) and flip 8 has been minted:
        // publishing 7 again cannot show 8, and 7 may already be reclaimed.
        assert!(surplus_republish(true, 7, 7, 8));
        assert!(surplus_republish(true, 9, 7, 8));
    }

    #[test]
    fn an_idle_desktop_republish_is_never_surplus() {
        // Nothing newer was presented: this flush IS the freshness edge.
        assert!(!surplus_republish(true, 7, 7, 7));
        assert!(!surplus_republish(true, 9, 7, 7));
    }

    #[test]
    fn a_same_buffer_represent_passes_because_it_publishes_a_fresh_epoch() {
        let mut t = LeaseTracker::new();
        let first = present(&mut t, 191, true, 10);
        let token = t.issue_flush();
        t.complete_flush(token, true);
        // Read caught up with the binding, nothing newer minted: passes.
        assert!(!surplus_republish(
            true,
            t.read_epoch(),
            t.bound_epoch(),
            first
        ));
        // DWM re-presents the SAME buffer: the epoch is minted AND published,
        // so the next marker's flush is a first read again.
        let second = present(&mut t, 191, false, 11);
        assert_eq!(t.bound_epoch(), second);
        assert!(!surplus_republish(
            true,
            t.read_epoch(),
            t.bound_epoch(),
            second
        ));
    }

    /// The MMIO/desktop contract mints no presentations, so `present_epoch` is
    /// frozen at whatever the last DMA-flip app left behind. Without `tracked`
    /// the gate would then drop every desktop refresh forever — a frozen
    /// desktop, which is defect 0aa.
    #[test]
    fn an_untracked_binding_disables_the_gate_entirely() {
        assert!(!surplus_republish(false, 7, 7, 8));
        assert!(!surplus_republish(false, u64::MAX, 1, u64::MAX));
        // A binding that never published an epoch cannot be gated either.
        assert!(!surplus_republish(true, 7, NO_LEASE, 8));
    }

    #[test]
    fn an_ungated_primary_publishes_immediately() {
        let mut t = LeaseTracker::new();
        t.arm_primary(0xBEEF_0000, NO_LEASE);
        assert_eq!(t.displayed_primary(), 0xBEEF_0000);
    }

    #[test]
    fn a_superseded_epoch_still_publishes_its_address() {
        let mut t = LeaseTracker::new();
        let first = present(&mut t, 191, true, 10);
        t.arm_primary(0x1000, first);
        assert_eq!(t.displayed_primary(), 0);
        present(&mut t, 195, true, 11);
        // The supersede ended `first`'s lease, so its address is authoritative
        // now — the alternative is a heartbeat frozen on the previous address.
        assert_eq!(t.displayed_primary(), 0x1000);
    }
}

pub mod scanout_read_ledger {
    //! Generation-qualified v2 READ LEDGER state machine.
    //!
    //! A page has 65 slots: every legal WindowedBlt reader plus the one direct
    //! flush reader. A slot may be recycled at quiescence even if its allocation
    //! stays live. Its nonzero generation makes that safe: an old token must
    //! never retire a replacement claim with the same slot or resource id.

    /// Duplicated from `helios_protocol` to keep this executable model
    /// dependency-free. `kmd_render` pins it to the wire ABI and to the
    /// WindowedBlt bound at the actual KMD use site.
    pub const SLOT_COUNT: usize = 65;
    pub const NO_SLOT: u8 = 0xFF;
    pub const FREE_RESID: u32 = 0;

    /// Exact KMD-side identity carried by every direct-flush and WindowedBlt
    /// reader: a slot number alone is deliberately insufficient after recycle.
    #[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
    pub struct LedgerTicket {
        pub slot: u8,
        pub resid: u32,
        pub generation: u64,
    }

    impl LedgerTicket {
        pub const NONE: Self = Self {
            slot: NO_SLOT,
            resid: FREE_RESID,
            generation: 0,
        };

        pub const fn is_claimed(self) -> bool {
            self.slot != NO_SLOT && self.resid != FREE_RESID && self.generation != 0
        }
    }

    /// Reader verdict for a stable v2 sample. A generation change is a
    /// same-resid re-claim, so the UMD retries its full scan rather than using
    /// this predicate to turn it into a false no-wait.
    pub const fn reader_in_flight(
        resid_probe: u32,
        generation_probe: u64,
        issued: u64,
        retired: u64,
        generation_reread: u64,
        resid_reread: u32,
    ) -> bool {
        resid_probe != FREE_RESID
            && generation_probe != 0
            && resid_probe == resid_reread
            && generation_probe == generation_reread
            && issued > retired
    }

    /// One model slot. The driver's counterpart is four mapped atomics
    /// (`resid`, `generation`, `issued`, `retired`) plus a KMD-private
    /// retire-wanted bit, serialized by the ledger's mutation leaf lock.
    #[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
    pub struct SlotModel {
        pub resid: u32,
        pub generation: u64,
        pub issued: u64,
        pub retired: u64,
        pub wanted: bool,
    }

    /// Unsampled tallies, mirrored into the service key as the `Rd*` values.
    /// NOT zeroed by [`LedgerModel::reset`]: the page is per-transport-
    /// generation state, the counters are per-boot (StartDevice) state, and an
    /// orphaned retirement (a token outliving a reset) balances `retired`
    /// against an `issued` from before the reset ONLY because the two are
    /// decoupled.
    #[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
    pub struct LedgerCounters {
        /// Ledger issues (`RdIss`).
        pub issued: u32,
        /// Ledger retirements, any outcome (`RdRet`).
        pub retired: u32,
        /// Claims refused because all slots were live (`RdOvf`).
        pub overflow: u32,
        /// Retirements performed by the token's `Drop` rather than an explicit
        /// completion (`RdDrp`) — enqueue failure or transport teardown.
        pub drop_retired: u32,
        /// Retirements whose slot no longer named the token's resid (`RdOrp`).
        /// Reachable ONLY when a reset zeroed the ledger with the read in
        /// flight; any other movement is a bug.
        pub orphaned: u32,
    }

    /// The executable specification of the ledger state machine.
    ///
    /// Single-threaded executable form of the driver's mutation leaf lock.
    #[derive(Clone, Copy, Debug)]
    pub struct LedgerModel {
        pub slots: [SlotModel; SLOT_COUNT],
        /// The page's own `slot_overflow` word (reader-visible loud-failure
        /// signal). Zeroed by reset with the slots, unlike the counters.
        pub page_overflow: u32,
        pub counters: LedgerCounters,
        /// KMD-private global source. Transport reset clears the page, never
        /// this source, so a token from before reset cannot alias a new claim.
        pub next_generation: u64,
    }

    impl Default for LedgerModel {
        fn default() -> Self {
            Self::new()
        }
    }

    impl LedgerModel {
        pub const fn new() -> Self {
            Self {
                slots: [SlotModel {
                    resid: FREE_RESID,
                    generation: 0,
                    issued: 0,
                    retired: 0,
                    wanted: false,
                }; SLOT_COUNT],
                page_overflow: 0,
                counters: LedgerCounters {
                    issued: 0,
                    retired: 0,
                    overflow: 0,
                    drop_retired: 0,
                    orphaned: 0,
                },
                next_generation: 1,
            }
        }

        /// Find the slot currently claimed for `resid`, if any.
        pub fn slot_of(&self, resid: u32) -> Option<usize> {
            if resid == FREE_RESID {
                return None;
            }
            self.slots.iter().position(|s| s.resid == resid)
        }

        /// Find, claim, or (only when necessary) recycle one quiescent slot.
        pub fn issue(&mut self, resid: u32) -> LedgerTicket {
            if resid == FREE_RESID {
                return LedgerTicket::NONE;
            }
            if let Some(i) = self.slot_of(resid) {
                self.slots[i].issued += 1;
                self.counters.issued += 1;
                return Self::ticket(i, self.slots[i]);
            }
            if let Some(i) = self.slots.iter().position(|s| s.resid == FREE_RESID) {
                return self.claim_fresh(i, resid);
            }
            // Allocation liveness is not reader liveness. Once every free slot
            // is occupied, reuse any claim with no active reader.
            if let Some(i) = self.slots.iter().position(|s| s.issued == s.retired) {
                return self.claim_fresh(i, resid);
            }
            self.page_overflow += 1;
            self.counters.overflow += 1;
            LedgerTicket::NONE
        }

        /// One token retirement. A stale `{slot,resid,generation}` is counted
        /// orphaned and cannot mutate a replacement claim.
        pub fn token_retire(&mut self, ticket: LedgerTicket, via_drop: bool) {
            if !ticket.is_claimed() {
                return;
            }
            let i = ticket.slot as usize;
            self.counters.retired += 1;
            if via_drop {
                self.counters.drop_retired += 1;
            }
            if i >= SLOT_COUNT
                || self.slots[i].resid != ticket.resid
                || self.slots[i].generation != ticket.generation
            {
                self.counters.orphaned += 1;
                return;
            }
            self.slots[i].retired += 1;
            if self.slots[i].wanted && self.slots[i].issued == self.slots[i].retired {
                self.reclaim(i);
            }
        }

        /// The backing allocation for `resid` is retiring: reclaim its slot
        /// now if no read is in flight, else mark retire-wanted and let the
        /// equalizing [`Self::token_retire`] complete the reclaim (§3.1).
        pub fn alloc_retire(&mut self, resid: u32) {
            let Some(i) = self.slot_of(resid) else {
                return;
            };
            self.slots[i].wanted = true;
            if self.slots[i].issued == self.slots[i].retired {
                self.reclaim(i);
            }
        }

        /// Transport reset orphans old tickets but deliberately preserves the
        /// global generation source.
        pub fn reset(&mut self) {
            for slot in &mut self.slots {
                *slot = SlotModel::default();
            }
            self.page_overflow = 0;
        }

        fn reclaim(&mut self, i: usize) {
            self.slots[i] = SlotModel::default();
        }

        /// Reads in flight for `resid` — what the mapped reader computes.
        pub fn in_flight(&self, resid: u32) -> u64 {
            self.slot_of(resid)
                .map(|i| self.slots[i].issued - self.slots[i].retired)
                .unwrap_or(0)
        }

        fn claim_fresh(&mut self, i: usize, resid: u32) -> LedgerTicket {
            let generation = match self.mint_generation() {
                Some(generation) => generation,
                None => return LedgerTicket::NONE,
            };
            self.slots[i] = SlotModel {
                resid,
                generation,
                issued: 1,
                retired: 0,
                wanted: false,
            };
            self.counters.issued += 1;
            Self::ticket(i, self.slots[i])
        }

        fn mint_generation(&mut self) -> Option<u64> {
            if self.next_generation == 0 {
                return None;
            }
            let generation = self.next_generation;
            self.next_generation = generation.checked_add(1).unwrap_or(0);
            Some(generation)
        }

        fn ticket(i: usize, slot: SlotModel) -> LedgerTicket {
            LedgerTicket {
                slot: i as u8,
                resid: slot.resid,
                generation: slot.generation,
            }
        }
    }
}

#[cfg(test)]
mod scanout_read_ledger_tests {
    use super::scanout_read_ledger::*;

    /// The rules the model must uphold after EVERY operation — checked from
    /// the outside because nothing in the crate itself may panic.
    fn check_invariants(m: &LedgerModel) {
        for (i, s) in m.slots.iter().enumerate() {
            assert!(s.issued >= s.retired, "slot {i}: retired ran ahead");
            if s.resid == FREE_RESID {
                assert_eq!((s.issued, s.retired), (0, 0), "slot {i}: dirty free slot");
                assert_eq!(s.generation, 0, "slot {i}: free slot has generation");
                assert!(!s.wanted, "slot {i}: free slot still wanted");
            } else {
                assert_ne!(s.generation, 0, "slot {i}: claim has zero generation");
            }
        }
    }

    /// Liveness matrix rows 1/2 (FIX-DESIGN-d4a.md §6): a completed read
    /// retires, and the identity `issued == retired` holds at quiescence.
    #[test]
    fn issue_then_retire_balances_and_reuses_the_slot() {
        let mut m = LedgerModel::new();
        let ticket = m.issue(191);
        assert!(ticket.is_claimed());
        assert_eq!(m.in_flight(191), 1);
        m.token_retire(ticket, false);
        assert_eq!(m.in_flight(191), 0);
        let same_claim = m.issue(191);
        assert_eq!(same_claim, ticket);
        m.token_retire(same_claim, false);
        assert_eq!(m.counters.issued, 2);
        assert_eq!(m.counters.retired, 2);
        assert_eq!(m.slots[ticket.slot as usize].issued, 2);
        assert_eq!(m.slots[ticket.slot as usize].retired, 2);
        check_invariants(&m);
    }

    /// A resid of 0 would alias the free-slot sentinel; the model refuses it
    /// exactly as the driver's issue path does.
    #[test]
    fn free_resid_cannot_claim() {
        let mut m = LedgerModel::new();
        assert_eq!(m.issue(FREE_RESID), LedgerTicket::NONE);
        assert_eq!(m.counters.issued, 0);
        check_invariants(&m);
    }

    /// Rows 4/5: a token whose read never completed retires via `Drop` — the
    /// ledger cannot tell the difference, only the census can.
    #[test]
    fn drop_retirement_balances_and_is_counted() {
        let mut m = LedgerModel::new();
        let ticket = m.issue(191);
        m.token_retire(ticket, true);
        assert_eq!(m.counters.issued, m.counters.retired);
        assert_eq!(m.counters.drop_retired, 1);
        assert_eq!(m.in_flight(191), 0);
    }

    /// The v2 capacity is exactly 64 WindowedBlt readers plus one direct
    /// flush. All 65 active claims fit without a refusal.
    #[test]
    fn sixty_five_active_distinct_claims_do_not_overflow() {
        let mut m = LedgerModel::new();
        for resid in 1..=SLOT_COUNT as u32 {
            assert!(m.issue(resid).is_claimed());
        }
        assert_eq!(m.page_overflow, 0);
        assert_eq!(m.counters.overflow, 0);
        check_invariants(&m);
    }

    /// One live desktop ring and two concurrent four-slot WindowedBlt rings
    /// are ordinary legal pressure, not an overflow condition. This is the
    /// minimal mixed population that used to exhaust the v1 eight-slot page.
    #[test]
    fn desktop_ring_plus_two_windowed_blt_rings_fit_without_overflow() {
        let mut m = LedgerModel::new();
        for resid in [6, 821, 822, 823] {
            assert!(m.issue(resid).is_claimed());
        }
        for resid in [992, 993, 994, 995, 1017, 1018, 1019, 1020] {
            assert!(m.issue(resid).is_claimed());
        }
        assert_eq!(m.page_overflow, 0);
        assert_eq!(m.counters.overflow, 0);
        check_invariants(&m);
    }

    #[test]
    fn sixty_sixth_active_distinct_claim_overflows_loudly() {
        let mut m = LedgerModel::new();
        for resid in 1..=SLOT_COUNT as u32 {
            assert!(m.issue(resid).is_claimed());
        }
        let ticket = m.issue(SLOT_COUNT as u32 + 1);
        assert_eq!(ticket, LedgerTicket::NONE);
        assert_eq!(m.page_overflow, 1);
        assert_eq!(m.counters.overflow, 1);
        m.token_retire(ticket, false);
        assert_eq!(m.counters.issued, SLOT_COUNT as u32);
        assert_eq!(m.counters.retired, 0);
    }

    /// Row 6, quiescent half: an allocation retiring with no read in flight
    /// reclaims immediately, and the freed slot is claimable with 0/0.
    #[test]
    fn alloc_retire_without_inflight_read_reclaims_immediately() {
        let mut m = LedgerModel::new();
        let ticket = m.issue(191);
        m.token_retire(ticket, false);
        m.alloc_retire(191);
        assert_eq!(m.slot_of(191), None);
        let reused = m.issue(400);
        assert_eq!(reused.slot, ticket.slot);
        assert_ne!(reused.generation, ticket.generation);
        assert_eq!(m.slots[ticket.slot as usize].issued, 1);
        assert_eq!(m.slots[ticket.slot as usize].retired, 0);
    }

    /// Row 6, pinned half: a live token pins its slot across the allocation
    /// retire, and the equalizing retirement completes the reclaim — so a
    /// token can NEVER retire into a recycled slot.
    #[test]
    fn live_token_pins_the_slot_until_its_retirement_reclaims() {
        let mut m = LedgerModel::new();
        let ticket = m.issue(191);
        m.alloc_retire(191);
        // Pinned: still claimed, retire-wanted, and the reader still sees the
        // in-flight read (correct — the host read has not terminated).
        assert_eq!(m.slot_of(191), Some(ticket.slot as usize));
        assert!(m.slots[ticket.slot as usize].wanted);
        assert_eq!(m.in_flight(191), 1);
        m.token_retire(ticket, false);
        // The retirement won the wanted token and reclaimed.
        assert_eq!(m.slot_of(191), None);
        assert!(!m.slots[ticket.slot as usize].wanted);
        assert_eq!(m.counters.issued, m.counters.retired);
        check_invariants(&m);
    }

    /// A quiescent claim can be recycled without allocation teardown once all
    /// slots are occupied; allocation liveness is not reader liveness.
    #[test]
    fn quiescent_live_claim_recycles_when_no_free_slot_exists() {
        let mut m = LedgerModel::new();
        let old = m.issue(191);
        m.token_retire(old, false);
        for resid in 2..=SLOT_COUNT as u32 {
            assert!(m.issue(resid).is_claimed());
        }
        let replacement = m.issue(500);
        assert_eq!(replacement.slot, old.slot);
        assert_ne!(replacement.generation, old.generation);
        assert_eq!(m.slot_of(191), None);
        check_invariants(&m);
    }

    #[test]
    fn same_resid_reclaim_gets_a_new_generation() {
        let mut m = LedgerModel::new();
        let old = m.issue(191);
        m.token_retire(old, false);
        m.alloc_retire(191);
        let replacement = m.issue(191);
        assert_eq!(replacement.slot, old.slot);
        assert_ne!(replacement.generation, old.generation);
        assert_eq!(replacement.resid, old.resid);
    }

    #[test]
    fn stale_ticket_cannot_retire_a_new_claim() {
        let mut m = LedgerModel::new();
        let old = m.issue(191);
        m.token_retire(old, false);
        m.alloc_retire(191);
        let replacement = m.issue(400);
        m.token_retire(old, true);
        assert_eq!(m.counters.orphaned, 1);
        assert_eq!(m.slots[replacement.slot as usize].retired, 0);
        assert_eq!(m.in_flight(400), 1);
    }

    #[test]
    fn reset_generation_never_aliases_old_ticket() {
        let mut m = LedgerModel::new();
        let old = m.issue(191);
        m.reset();
        assert_eq!(m.page_overflow, 0);
        let replacement = m.issue(191);
        assert_eq!(replacement.slot, old.slot);
        assert_ne!(replacement.generation, old.generation);
        m.token_retire(old, true);
        assert_eq!(m.counters.orphaned, 1);
        assert_eq!(m.in_flight(191), 1);
        m.token_retire(replacement, false);
        assert_eq!(m.counters.issued, m.counters.retired);
        check_invariants(&m);
    }

    #[test]
    fn issued_equals_retired_at_quiescence() {
        let mut m = LedgerModel::new();
        let mut tickets = [LedgerTicket::NONE; SLOT_COUNT];
        for (index, ticket) in tickets.iter_mut().enumerate() {
            *ticket = m.issue(index as u32 + 1);
        }
        for ticket in tickets {
            m.token_retire(ticket, false);
        }
        assert_eq!(m.counters.issued, m.counters.retired);
        for slot in m.slots {
            assert_eq!(slot.issued, slot.retired);
        }
    }

    /// The reader protocol (§3.1): in-flight requires a stable resid and
    /// `issued > retired`; a mid-read reclaim (resid changed) is a no-wait.
    #[test]
    fn reader_protocol_verdicts() {
        assert!(reader_in_flight(191, 7, 5, 4, 7, 191));
        assert!(!reader_in_flight(191, 7, 5, 5, 7, 191));
        assert!(!reader_in_flight(191, 7, 5, 4, 8, 191));
        assert!(!reader_in_flight(191, 7, 5, 4, 7, 0));
        assert!(!reader_in_flight(FREE_RESID, 0, 1, 0, 0, FREE_RESID));
    }
}

pub mod snapshot_bind {
    //! D4b snapshot bind: the Present-time descriptor gate
    //! (FIX-DESIGN-d4b-snapshot.md §4).
    //!
    //! When `HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT` is set, the present private
    //! data describes a UMD-owned SNAPSHOT image (filled by a venus-queue-
    //! ordered copy of the presented primary) that the KMD binds and flushes on
    //! the DMA-flip path INSTEAD of the flipped allocation. The descriptor is
    //! guest-supplied, so every field is validated before it may substitute the
    //! bind target; a descriptor that fails ANY check falls back to the flipped
    //! allocation (today's behaviour), counted as `SnFbk`.
    //!
    //! [`validate_layout`] is the undersize guard — the Xid-31 protection —
    //! reproduced byte-for-byte (saturating arithmetic included) from
    //! `ScanoutTarget::from_direct_primary` in
    //! `kmd_render/src/ddi/create_allocation.rs`. It must NEVER be relaxed: an
    //! `alloc_size` smaller than `plane_offset + pitch*height` lets QEMU read
    //! past the blob.

    use crate::ScanoutFormat;

    /// The validated snapshot bind target, carried BY VALUE from the Present
    /// DDI through the flip record to both bind paths. No pointer and no
    /// allocation-table lookup ever resolves the snapshot, so its
    /// `AllocationContext` lifetime cannot be involved; liveness is enforced
    /// where it already lives (the flush executor's `resource_is_live` arm).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct SnapshotDescriptor {
        /// Venus resource id of the snapshot image. Never 0 in a valid
        /// descriptor; 0 is the "no substitution" sentinel on the carriers.
        pub resource_id: u32,
        pub width: u32,
        pub height: u32,
        /// Row pitch in bytes (the stride `SET_SCANOUT_BLOB` uses).
        pub pitch: u32,
        /// Exact DXGI format; must resolve via [`ScanoutFormat::from_dxgi`].
        pub dxgi_format: u32,
        /// Memory-plane-0 byte offset within the backing allocation.
        pub plane_offset: u64,
        /// Total venus blob size backing `resource_id` — the undersize guard's
        /// right-hand side.
        pub venus_alloc_size: u64,
        /// Exact Vulkan memory type for a typed WindowedBlt import.  Zero is a
        /// valid type; its presence is proved by the complete v2 command tail
        /// and the WindowedBlt purpose, not by a sentinel value.
        pub memory_type_index: u32,
        /// 0 = direct bind snapshot, 1 = WindowedBlt source snapshot.
        pub purpose: u32,
    }

    /// Why a snapshot descriptor was refused. Every arm is one `SnFbk` and a
    /// fall-back to binding the flipped allocation; none is an error return.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum SnapshotReject {
        /// `resource_id == 0` — the no-substitution sentinel arrived flagged.
        ZeroResource,
        /// Descriptor extent differs from the allocation-list source's extent
        /// (a fullscreen transition / geometry change mid-flight).
        ExtentMismatch,
        /// Pitch/offset/size failed the direct-scan-out layout rules — the
        /// undersize guard.
        Layout,
        /// No virtio scan-out encoding for `dxgi_format`.
        Format,
        /// Descriptor format differs from the DXGI allocation-list source.
        /// Both can be individually supported but are not interchangeable.
        SourceFormatMismatch,
        /// A WindowedBlt path received a direct-bind descriptor (or vice
        /// versa). These are different consumers and must never be inferred.
        Purpose,
    }

    /// The full Present-time gate: identity, extent, then layout. The caller
    /// has already established `FLAG_SNAPSHOT` + 48-byte coverage; this
    /// validates everything the wire bytes claim. `source_width`/
    /// `source_height` are the allocation-list source's extent — the identity
    /// Windows placed in the Present call, which the descriptor must agree
    /// with before it may substitute for it.
    pub const fn validate(
        d: &SnapshotDescriptor,
        source_width: u32,
        source_height: u32,
    ) -> Result<(), SnapshotReject> {
        if d.resource_id == 0 {
            return Err(SnapshotReject::ZeroResource);
        }
        if d.width != source_width || d.height != source_height {
            return Err(SnapshotReject::ExtentMismatch);
        }
        validate_layout(d)
    }

    /// The layout half, shared with `ScanoutTarget::from_snapshot_descriptor`
    /// so the constructor cannot restate the arithmetic in a weakened form.
    ///
    /// ⚠ Byte-for-byte from `ScanoutTarget::from_direct_primary`, saturating
    /// arithmetic included. Do not "simplify"; do not relax.
    pub const fn validate_layout(d: &SnapshotDescriptor) -> Result<(), SnapshotReject> {
        let min_size = d
            .plane_offset
            .saturating_add((d.pitch as u64).saturating_mul(d.height as u64));
        let layout_ok = d.pitch >= d.width.saturating_mul(4)
            && d.pitch & 3 == 0
            && d.plane_offset <= u32::MAX as u64
            && d.venus_alloc_size >= min_size;
        if !layout_ok {
            return Err(SnapshotReject::Layout);
        }
        if ScanoutFormat::from_dxgi(d.dxgi_format).is_none() {
            return Err(SnapshotReject::Format);
        }
        Ok(())
    }

    /// Typed WindowedBlt source gate. It is deliberately distinct from
    /// [`validate`]: this snapshot is imported into the KMD Venus context and
    /// copied into DXGI's destination; it is never a `SET_SCANOUT_BLOB` target.
    pub const fn validate_windowed_blt(
        d: &SnapshotDescriptor,
        source_width: u32,
        source_height: u32,
        source_dxgi_format: u32,
    ) -> Result<(), SnapshotReject> {
        if d.purpose != 1 {
            return Err(SnapshotReject::Purpose);
        }
        if d.dxgi_format != source_dxgi_format {
            return Err(SnapshotReject::SourceFormatMismatch);
        }
        validate(d, source_width, source_height)
    }
}

/// Pure exact-membership rule for WindowedBlt WDDM retirement. Tokens are
/// monotonically minted only as local request identities; they are not an
/// ordering relation across generation-qualified present streams.
pub mod windowed_blt_token {
    /// Returns true only for the exact terminal `(token, stream_boundary)`
    /// pair. A greater token on another stream proves nothing about this one.
    pub fn terminal_contains(terminal: &[(u64, u64)], token: u64, stream_boundary: u64) -> bool {
        terminal.iter().any(|&(known_token, known_stream)| {
            known_token == token && known_stream == stream_boundary
        })
    }

    /// Does `candidate` belong to the merged terminal prefix ending at
    /// `(max_token, max_boundary)`? The boundary encoding reserves bit 63 and
    /// carries a generation-qualified stream handle in bits 32..62. Token
    /// order is meaningful only *inside* that exact stream.
    pub const fn prefix_contains(
        max_token: u64,
        max_boundary: u64,
        candidate_token: u64,
        candidate_boundary: u64,
    ) -> bool {
        const TAG: u64 = 1 << 63;
        const HANDLE_MASK: u64 = 0x7fff_ffff;
        max_token != 0
            && max_boundary & TAG != 0
            && candidate_boundary & TAG != 0
            && candidate_token <= max_token
            && ((max_boundary >> 32) & HANDLE_MASK) == ((candidate_boundary >> 32) & HANDLE_MASK)
    }

    /// Whether an exact terminal/pending pair may attach to a newly replayed
    /// SubmitCommand. A preempted buffer can legitimately reach a terminal
    /// before its DMA private data is resubmitted; an already queued WDDM
    /// prefix owns that terminal and must reject a duplicate replay.
    pub const fn can_attach_dependency(
        exact_pending: bool,
        exact_terminal: bool,
        already_owned_by_wddm: bool,
    ) -> bool {
        !already_owned_by_wddm && (exact_pending || exact_terminal)
    }

    /// Number of terminal identities left after successfully consuming one
    /// same-stream WDDM prefix. This makes bounded-capacity behavior a host
    /// testable pure rule rather than a property of the KMD container.
    pub fn count_after_prefix_consume(
        terminal: &[(u64, u64)],
        max_token: u64,
        max_boundary: u64,
    ) -> usize {
        terminal
            .iter()
            .filter(|&&(token, boundary)| {
                !prefix_contains(max_token, max_boundary, token, boundary)
            })
            .count()
    }

    /// Scheduler/lifecycle cancellation may only reclaim an undispatched
    /// request that belongs to the dead same-stream prefix.
    pub const fn prefix_cancel_eligible(
        dispatched: bool,
        max_token: u64,
        max_boundary: u64,
        token: u64,
        boundary: u64,
    ) -> bool {
        !dispatched && prefix_contains(max_token, max_boundary, token, boundary)
    }
}

#[cfg(test)]
mod windowed_blt_token_tests {
    use super::windowed_blt_token::{
        can_attach_dependency, count_after_prefix_consume, prefix_cancel_eligible, prefix_contains,
        terminal_contains,
    };

    #[test]
    fn interleaved_stream_terminal_tokens_do_not_cross_admit() {
        let stream_a = 0x8000_0001_0000_0009u64;
        let stream_b = 0x8000_0002_0000_0007u64;
        // Stream B's numerically later token completed first. It cannot retire
        // stream A's older token just because a global counter would be past it.
        let terminal = [(3u64, stream_b)];
        assert!(!terminal_contains(&terminal, 2, stream_a));
        assert!(terminal_contains(&terminal, 3, stream_b));
    }

    #[test]
    fn merged_prefix_consumes_only_its_own_stream_and_releases_capacity() {
        let stream_a_7 = 0x8000_0011_0000_0007u64;
        let stream_a_9 = 0x8000_0011_0000_0009u64;
        let stream_b_3 = 0x8000_0022_0000_0003u64;
        let terminal = [(7, stream_a_7), (3, stream_b_3), (9, stream_a_9)];
        assert!(prefix_contains(9, stream_a_9, 7, stream_a_7));
        assert!(!prefix_contains(9, stream_a_9, 3, stream_b_3));
        assert_eq!(count_after_prefix_consume(&terminal, 9, stream_a_9), 1);

        let full = [(1, stream_a_7); 64];
        assert_eq!(count_after_prefix_consume(&full, 64, stream_a_9), 0);
    }

    #[test]
    fn dead_prefix_cancels_only_undispatched_same_stream_requests() {
        let stream_a_7 = 0x8000_0011_0000_0007u64;
        let stream_a_9 = 0x8000_0011_0000_0009u64;
        let stream_b_3 = 0x8000_0022_0000_0003u64;
        assert!(prefix_cancel_eligible(false, 9, stream_a_9, 7, stream_a_7));
        assert!(prefix_cancel_eligible(false, 9, stream_a_9, 9, stream_a_9));
        assert!(!prefix_cancel_eligible(false, 9, stream_a_9, 3, stream_b_3));
        assert!(!prefix_cancel_eligible(true, 9, stream_a_9, 7, stream_a_7));
    }

    #[test]
    fn preempted_terminal_can_replay_once_but_stale_or_owned_private_cannot() {
        assert!(can_attach_dependency(false, true, false));
        assert!(!can_attach_dependency(false, false, false));
        assert!(!can_attach_dependency(false, true, true));
    }
}

#[cfg(test)]
mod snapshot_bind_tests {
    use super::snapshot_bind::*;

    /// A descriptor shaped like the real S-ring slot: 1920×1080 BGRA, the
    /// UMD's 256-aligned pitch, plane data at 0, exact-size blob.
    fn good() -> SnapshotDescriptor {
        SnapshotDescriptor {
            resource_id: 0x131,
            width: 1920,
            height: 1080,
            pitch: 7680,
            dxgi_format: 87,
            plane_offset: 0,
            venus_alloc_size: 7680 * 1080,
            memory_type_index: 0,
            purpose: 0,
        }
    }

    #[test]
    fn a_healthy_descriptor_validates() {
        assert_eq!(validate(&good(), 1920, 1080), Ok(()));
    }

    #[test]
    fn zero_resource_is_the_sentinel_and_never_substitutes() {
        let mut d = good();
        d.resource_id = 0;
        assert_eq!(validate(&d, 1920, 1080), Err(SnapshotReject::ZeroResource));
    }

    /// Extent must equal the ALLOCATION-LIST source's, not merely be
    /// self-consistent — the fullscreen-transition fallback row of the §6
    /// matrix.
    #[test]
    fn extent_mismatch_falls_back() {
        assert_eq!(
            validate(&good(), 1896, 1030),
            Err(SnapshotReject::ExtentMismatch)
        );
        let mut d = good();
        d.height = 1030;
        assert_eq!(
            validate(&d, 1920, 1080),
            Err(SnapshotReject::ExtentMismatch)
        );
    }

    /// The undersize guard: `alloc_size >= plane_offset + pitch*height`,
    /// exact at the boundary.
    #[test]
    fn undersize_guard_is_exact_and_never_relaxed() {
        let mut d = good();
        d.venus_alloc_size = 7680 * 1080 - 1;
        assert_eq!(validate(&d, 1920, 1080), Err(SnapshotReject::Layout));
        d.venus_alloc_size = 7680 * 1080;
        assert_eq!(validate(&d, 1920, 1080), Ok(()));
        // The plane offset shifts the requirement by exactly itself.
        d.plane_offset = 4096;
        assert_eq!(validate(&d, 1920, 1080), Err(SnapshotReject::Layout));
        d.venus_alloc_size = 4096 + 7680 * 1080;
        assert_eq!(validate(&d, 1920, 1080), Ok(()));
    }

    /// The widened-u64 arithmetic: the worst representable pitch*height
    /// product (~2^64 - 2^33) must still be demanded IN FULL from
    /// `venus_alloc_size` — a narrower or wrapping formulation would compute a
    /// small `min_size` a tiny blob satisfies. (True u64 saturation is
    /// unreachable from two u32 inputs; `saturating_*` is defense-in-depth,
    /// kept byte-for-byte with `from_direct_primary`.)
    #[test]
    fn near_max_products_demand_the_full_size() {
        let mut d = good();
        d.plane_offset = 0;
        d.pitch = u32::MAX & !3; // 4-aligned, enormous
        d.height = u32::MAX;
        let min_size = (d.pitch as u64) * (d.height as u64);
        d.venus_alloc_size = min_size - 1;
        assert_eq!(validate_layout(&d), Err(SnapshotReject::Layout));
        d.venus_alloc_size = min_size;
        assert_eq!(validate_layout(&d), Ok(()));
    }

    #[test]
    fn pitch_rules_match_the_direct_primary_validator() {
        // pitch < width*4
        let mut d = good();
        d.pitch = 1920 * 4 - 4;
        assert_eq!(validate(&d, 1920, 1080), Err(SnapshotReject::Layout));
        // pitch % 4 != 0
        let mut d = good();
        d.pitch = 7682;
        d.venus_alloc_size = 7682 * 1080;
        assert_eq!(validate(&d, 1920, 1080), Err(SnapshotReject::Layout));
        // width*4 exactly (no 256-alignment REQUIREMENT here, same as the
        // direct-primary validator).
        let mut d = good();
        d.pitch = 1920 * 4;
        d.venus_alloc_size = 1920 * 4 * 1080;
        assert_eq!(validate(&d, 1920, 1080), Ok(()));
    }

    #[test]
    fn plane_offset_must_fit_u32() {
        let mut d = good();
        d.plane_offset = u32::MAX as u64 + 1;
        d.venus_alloc_size = u64::MAX;
        assert_eq!(validate(&d, 1920, 1080), Err(SnapshotReject::Layout));
    }

    /// The strict DXGI set (28/87/88) and NOT the legacy-zero arm: a snapshot
    /// is a freshly created UMD image that always carries its exact format.
    #[test]
    fn format_uses_the_strict_dxgi_set() {
        for (fmt, ok) in [
            (28u32, true),
            (87, true),
            (88, true),
            (0, false),
            (24, false),
        ] {
            let mut d = good();
            d.dxgi_format = fmt;
            assert_eq!(validate(&d, 1920, 1080).is_ok(), ok, "dxgi {fmt}");
        }
    }

    #[test]
    fn windowed_blt_requires_the_exact_dxgi_source_format() {
        let mut d = good();
        d.purpose = 1;
        assert_eq!(validate_windowed_blt(&d, 1920, 1080, 87), Ok(()));
        // RGBA and BGRA are both supported scanout encodings, but importing a
        // BGRA snapshot for a DXGI RGBA source would silently swizzle DWM.
        assert_eq!(
            validate_windowed_blt(&d, 1920, 1080, 28),
            Err(SnapshotReject::SourceFormatMismatch)
        );
        d.purpose = 0;
        assert_eq!(
            validate_windowed_blt(&d, 1920, 1080, 87),
            Err(SnapshotReject::Purpose)
        );
    }
}

/// The registered present-stream value relation, as pure arithmetic.
///
/// One rule lives here rather than inline in the KMD for a specific reason:
/// `kmd_render` is built with `debug-assertions = on` in the profile that
/// actually ships (46th session), so a plain `value - submitted_value` on `u32`
/// is a PANIC — a silent graphics deadlock — for exactly the input the
/// instrument above it exists to observe. Saturating it makes that input
/// unrepresentable, and puts a host-run test under the claim.
/// The consumer-side liveness bound on the WDDM FIFO head (`WddmHeadMs`;
/// `KMD_IMPACT.md` §14a.2 K-F2 and `docs/dx12/PENDING.md` §1 A5).
///
/// Two rules live here because both are places an operator value or a clock edge
/// could silently break correctness, and neither is testable inside `kmd_render`.
pub mod wddm_head_bound {
    /// Clamp an operator-supplied bound, in ms.
    ///
    /// ⛔ CLAMPED IN BOTH DIRECTIONS, and the two directions protect different
    /// things: too LARGE reinstates the unbounded head and therefore the
    /// adapter-wide TDR the bound exists to prevent, too SMALL turns a last-resort
    /// release into a continuous early-fence generator (the 0ab-B stale-frame
    /// class). 0 is preserved exactly — it is the A/B disable and must stay
    /// reachable (CLAUDE.md rule 8).
    pub const fn clamp_bound_ms(raw: u32, min: u32, max: u32) -> u32 {
        if raw == 0 {
            return 0;
        }
        if raw < min {
            return min;
        }
        if raw > max {
            return max;
        }
        raw
    }

    /// What a blocked look at the FIFO head should do about the bound.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Action {
        /// The bound is disabled (`WddmHeadMs = 0`): historical unbounded head.
        Disabled,
        /// Nothing was armed yet; arm the deadline at this absolute time and keep
        /// waiting. The FIRST blocked look at this entry as head does this.
        Arm(u64),
        /// Armed and not yet expired: keep waiting.
        Wait,
        /// Expired: rebase this head's tagged dependencies onto the conservative
        /// wire watermark.
        Rebase,
        /// Already rebased once. A second rebase could only install a NEWER
        /// `next_wire_fence`, i.e. a STRICTER dependency, so it is forbidden rather
        /// than merely pointless.
        AlreadyRebased,
    }

    /// Whether the periodic heartbeat should prompt a completion DPC.
    ///
    /// ⚠ THE ZERO IS THE COMMON CASE AND IT MUST STAY CHEAP. Nothing is armed on
    /// an ordinary desktop, and the caller reads a single relaxed atomic to learn
    /// that — it must not reach for a clock, and it must NOT test the knob instead:
    /// `WddmHeadMs` defaults to 250, so a knob test is true on every shipping boot
    /// and would turn this into a permanent 60 DPC/s tax on the compositor path.
    /// Armed-ness is STATE; the bound is CONFIGURATION.
    pub const fn heartbeat_due(deadline_100ns: u64, now_100ns: u64) -> bool {
        deadline_100ns != 0 && now_100ns >= deadline_100ns
    }

    /// Decide one blocked look. `deadline_100ns == 0` means "not armed".
    pub const fn look(bound_ms: u32, now_100ns: u64, deadline_100ns: u64, rebased: bool) -> Action {
        if bound_ms == 0 {
            return Action::Disabled;
        }
        if rebased {
            return Action::AlreadyRebased;
        }
        if deadline_100ns == 0 {
            // 100 ns units. A saturating add keeps an exhausted interrupt-time
            // representation from wrapping into a deadline already in the past —
            // which would rebase the very next look.
            return Action::Arm(now_100ns.saturating_add(bound_ms as u64 * 10_000));
        }
        if now_100ns < deadline_100ns {
            return Action::Wait;
        }
        Action::Rebase
    }
}

#[cfg(test)]
mod wddm_head_bound_tests {
    use super::wddm_head_bound::{Action, clamp_bound_ms, heartbeat_due, look};

    const MIN: u32 = 100;
    const MAX: u32 = 1000;

    #[test]
    fn zero_survives_the_clamp_because_it_is_the_ab_disable() {
        assert_eq!(clamp_bound_ms(0, MIN, MAX), 0);
    }

    #[test]
    fn an_operator_typo_cannot_reinstate_the_tdr() {
        // The failure this guards: WddmHeadMs=100000 would be 100 s, far past the
        // 2 s default TdrDelay, i.e. the bound never acts.
        assert_eq!(clamp_bound_ms(100_000, MIN, MAX), MAX);
        assert_eq!(clamp_bound_ms(u32::MAX, MIN, MAX), MAX);
    }

    #[test]
    fn an_operator_typo_cannot_turn_the_last_resort_into_a_policy() {
        // 1 ms would rebase healthy frames continuously, which is 0ab-B on purpose.
        assert_eq!(clamp_bound_ms(1, MIN, MAX), MIN);
        assert_eq!(clamp_bound_ms(MIN - 1, MIN, MAX), MIN);
        assert_eq!(clamp_bound_ms(MIN, MIN, MAX), MIN);
        assert_eq!(clamp_bound_ms(250, MIN, MAX), 250);
    }

    #[test]
    fn the_first_blocked_look_arms_and_does_not_release() {
        assert_eq!(look(250, 1_000, 0, false), Action::Arm(1_000 + 2_500_000));
    }

    #[test]
    fn an_armed_head_waits_until_the_deadline_and_then_rebases_once() {
        let deadline = 1_000 + 2_500_000;
        assert_eq!(look(250, deadline - 1, deadline, false), Action::Wait);
        assert_eq!(look(250, deadline, deadline, false), Action::Rebase);
        // ...and never again, however long it stays blocked afterwards.
        assert_eq!(
            look(250, deadline + 10_000_000, deadline, true),
            Action::AlreadyRebased
        );
    }

    #[test]
    fn the_disabled_bound_short_circuits_every_other_state() {
        assert_eq!(look(0, 0, 0, false), Action::Disabled);
        assert_eq!(look(0, u64::MAX, 1, false), Action::Disabled);
        assert_eq!(look(0, u64::MAX, 1, true), Action::Disabled);
    }

    #[test]
    fn an_unarmed_head_never_asks_the_heartbeat_for_anything() {
        // THE COMMON CASE, and the one that must stay false: an ordinary desktop
        // has nothing armed. A caller that tested the KNOB instead of this state
        // would be true on every shipping boot (WddmHeadMs defaults to 250) and
        // would prompt a DPC on all 60 ticks a second, forever.
        assert!(!heartbeat_due(0, 0));
        assert!(!heartbeat_due(0, u64::MAX));
    }

    #[test]
    fn the_heartbeat_fires_only_at_or_after_an_armed_deadline() {
        let deadline = 2_500_000;
        assert!(!heartbeat_due(deadline, deadline - 1));
        assert!(heartbeat_due(deadline, deadline));
        assert!(heartbeat_due(deadline, deadline + 1));
    }

    #[test]
    fn an_exhausted_interrupt_clock_does_not_arm_a_deadline_in_the_past() {
        // Without the saturating add the arm would wrap to a small number and the
        // NEXT look would rebase immediately — a released fence caused by a clock,
        // not by a stuck producer.
        let Action::Arm(deadline) = look(250, u64::MAX, 0, false) else {
            panic!("expected Arm");
        };
        assert_eq!(deadline, u64::MAX);
        assert_eq!(look(250, u64::MAX - 1, deadline, false), Action::Wait);
    }
}

/// How a WDDM DMA fence's wire-fence dependency is chosen from the boundary a
/// guest submission named. Defects A4 and A6 of `docs/dx12/PENDING.md` §1 are both
/// decisions in this one table, which is why the table is here and testable rather
/// than an `if` chain inside the transport.
pub mod wddm_boundary {
    /// How the resulting watermark is compared against the in-flight wire fences.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Kind {
        /// EXCLUSIVE PREFIX: every async fence strictly below the watermark must
        /// have retired — every ring, every process. Conservative, always
        /// eventually satisfied, never a lie, and the fallback for any boundary
        /// that cannot be trusted.
        Prefix,
        /// The watermark IS one wire fence, and only that fence must have retired.
        /// The frame's own boundary.
        Exact,
    }

    /// Why a named boundary was replaced, if it was.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Rejection {
        /// The named boundary was used.
        Accepted,
        /// Zero, or at/beyond the ids this transport generation has assigned — a
        /// malformed or stale marker inside the live generation.
        OutOfRange,
        /// Below this transport generation's first id: a fence from a PREVIOUS
        /// StartDevice, surviving in a client that outlived a device restart.
        ///
        /// ⛔ It is a separate variant because an upper bound cannot see it. Wire
        /// fence ranges STRIDE UP at every StartDevice, so such an id is billions
        /// below the live range, satisfies `< next_wire_fence` trivially, matches
        /// nothing in flight, and would make the dependency complete instantly —
        /// a DMA fence reporting completion before its work exists.
        ForeignGeneration,
    }

    /// The chosen dependency.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Selection {
        pub watermark: u64,
        pub kind: Kind,
        pub rejection: Rejection,
    }

    /// Choose the wire-fence dependency for a submission that named
    /// `gpu_fence_id`.
    ///
    /// `wire_fence_base` is the FIRST id this transport generation may hand out and
    /// `next_wire_fence` the next it will; every id in `base..next` was genuinely
    /// assigned and enqueued, so `[base, next)` is exactly "issued by this
    /// generation".
    ///
    /// `d3d12` says the submission carried a `HeliosD3D12SubmitCmd` record — an
    /// IDENTITY, never a boundary. It selects [`Kind::Exact`] because a D3D12 ECL
    /// packet carries no GPU commands of its own: the only thing its DMA completion
    /// can truthfully report is that the batch the UMD named has finished, so
    /// waiting on the prefix below that fence is waiting on other processes'
    /// frames. The legacy Present writer of the same field keeps [`Kind::Prefix`]
    /// because that is the shipping, measured desktop configuration.
    pub const fn select(
        gpu_fence_id: u64,
        wire_fence_base: u64,
        next_wire_fence: u64,
        d3d12: bool,
    ) -> Selection {
        if gpu_fence_id == 0 || gpu_fence_id >= next_wire_fence {
            return Selection {
                watermark: next_wire_fence,
                kind: Kind::Prefix,
                rejection: Rejection::OutOfRange,
            };
        }
        if gpu_fence_id < wire_fence_base {
            return Selection {
                watermark: next_wire_fence,
                kind: Kind::Prefix,
                rejection: Rejection::ForeignGeneration,
            };
        }
        if d3d12 {
            return Selection {
                watermark: gpu_fence_id,
                kind: Kind::Exact,
                rejection: Rejection::Accepted,
            };
        }
        Selection {
            // The prefix bound is EXCLUSIVE, so naming fence N means waiting for
            // everything below N + 1. `saturating_add` because a u64 fence id at
            // the representation's ceiling must not wrap to 0, which is the "no
            // dependency" watermark.
            watermark: gpu_fence_id.saturating_add(1),
            kind: Kind::Prefix,
            rejection: Rejection::Accepted,
        }
    }
}

#[cfg(test)]
mod wddm_boundary_tests {
    use super::wddm_boundary::{Kind, Rejection, select};

    /// A representative live generation: base 1 + 3·2^32, 40 ids issued.
    const BASE: u64 = 1 + (3u64 << 32);
    const NEXT: u64 = BASE + 40;

    #[test]
    fn a_d3d12_boundary_is_exact_and_names_the_fence_itself() {
        let s = select(BASE + 12, BASE, NEXT, true);
        assert_eq!(s.kind, Kind::Exact);
        assert_eq!(s.rejection, Rejection::Accepted);
        // NOT `+ 1`: an exact test names the fence, and adding one would silently
        // turn it back into the prefix that A4 is about.
        assert_eq!(s.watermark, BASE + 12);
    }

    #[test]
    fn the_legacy_present_writer_keeps_the_exclusive_prefix() {
        let s = select(BASE + 12, BASE, NEXT, false);
        assert_eq!(s.kind, Kind::Prefix);
        assert_eq!(s.rejection, Rejection::Accepted);
        assert_eq!(s.watermark, BASE + 13);
    }

    #[test]
    fn a_foreign_generation_fence_is_rejected_however_plausible_it_looks() {
        // A6. The previous generation's ids are a whole stride below, and every
        // one of them satisfies `< next_wire_fence`.
        for id in [1u64, 42, BASE - 1, 1 + (2u64 << 32) + 7] {
            let s = select(id, BASE, NEXT, true);
            assert_eq!(s.rejection, Rejection::ForeignGeneration, "id {id}");
            // The fallback must be the conservative prefix, never the named id:
            // an exact wait on a fence this generation never issued is satisfied
            // immediately, which is the lie.
            assert_eq!(s.kind, Kind::Prefix, "id {id}");
            assert_eq!(s.watermark, NEXT, "id {id}");
        }
    }

    #[test]
    fn zero_and_beyond_the_range_stay_on_the_old_clamp_and_its_own_counter() {
        for id in [0u64, NEXT, NEXT + 1, u64::MAX] {
            let s = select(id, BASE, NEXT, true);
            assert_eq!(s.rejection, Rejection::OutOfRange, "id {id}");
            assert_eq!(s.kind, Kind::Prefix, "id {id}");
            assert_eq!(s.watermark, NEXT, "id {id}");
        }
    }

    #[test]
    fn exactly_one_rejection_can_apply_to_any_input() {
        // The two rejections must partition, or a counter pair reads as double
        // the truth. Sweep the boundaries of both conditions.
        for id in [0, 1, BASE - 1, BASE, BASE + 1, NEXT - 1, NEXT, NEXT + 1] {
            let s = select(id, BASE, NEXT, true);
            let out_of_range = id == 0 || id >= NEXT;
            let foreign = !out_of_range && id < BASE;
            assert_eq!(
                s.rejection == Rejection::OutOfRange,
                out_of_range,
                "id {id}"
            );
            assert_eq!(
                s.rejection == Rejection::ForeignGeneration,
                foreign,
                "id {id}"
            );
            assert_eq!(
                s.rejection == Rejection::Accepted,
                !out_of_range && !foreign,
                "id {id}"
            );
        }
    }

    #[test]
    fn the_first_generation_starts_at_one_and_accepts_its_own_low_ids() {
        // `NEXT_WIRE_FENCE_BASE` starts at 1, so instance 0's ids ARE small. The
        // foreign-generation test must not reject them.
        let s = select(1, 1, 5, true);
        assert_eq!(s.rejection, Rejection::Accepted);
        assert_eq!(s.watermark, 1);
        assert_eq!(select(0, 1, 5, true).rejection, Rejection::OutOfRange);
    }

    #[test]
    fn an_empty_generation_accepts_nothing() {
        // Nothing assigned yet: base == next, so every id is out of range and no
        // boundary can be honoured. Notably NOT reported as foreign — the
        // out-of-range test runs first, and it is the honest description.
        let s = select(BASE, BASE, BASE, true);
        assert_eq!(s.rejection, Rejection::OutOfRange);
        assert_eq!(s.watermark, BASE);
        assert_eq!(s.kind, Kind::Prefix);
    }

    #[test]
    fn a_prefix_watermark_never_wraps_to_the_no_dependency_sentinel() {
        // 0 is "no dependency". A `+ 1` that wrapped would turn the strongest
        // possible wait into none at all.
        let s = select(u64::MAX - 1, 0, u64::MAX, false);
        assert_eq!(s.rejection, Rejection::Accepted);
        assert_eq!(s.watermark, u64::MAX);
        assert_ne!(s.watermark, 0);
    }
}

pub mod present_stream {
    /// Capacity of the KMD's fixed, allocation-free registered-stream table.
    ///
    /// ⚠ These five constants are the ABI of the tagged boundary and of the
    /// packed handle, and they are DEFINED HERE so that the rules below and the
    /// tests that exercise them cannot drift from the driver's own numbers:
    /// `kmd_render`'s `MAX_PRESENT_STREAMS` / `PRESENT_STREAM_INDEX_BITS` /
    /// `PRESENT_STREAM_GENERATION_BITS` / `PRESENT_STREAM_GENERATION_MAX` /
    /// `PRESENT_STREAM_BOUNDARY_TAG` are aliases of these.
    pub const MAX_STREAMS: usize = 64;
    /// Low bits of a packed handle that carry the raw slot index.
    pub const INDEX_BITS: u32 = 6;
    /// Bits left for the generation inside the 31 the tagged-boundary ABI
    /// reserves for the handle (bit 63 is the namespace tag, bit 31 is unused so
    /// the handle stays positive in every signed reinterpretation).
    pub const GENERATION_BITS: u32 = 31 - INDEX_BITS;
    /// Largest generation a slot may reach before it must be retired rather than
    /// re-registered; the registration scan refuses a slot at this value.
    pub const GENERATION_MAX: u32 = (1 << GENERATION_BITS) - 1;
    /// Bit 63 distinguishes a tagged stream boundary from the legacy exclusive
    /// wire-fence namespace. The two are intentionally incomparable.
    pub const BOUNDARY_TAG: u64 = 1 << 63;

    /// Pack one slot's `(generation, index)` into the opaque handle.
    ///
    /// The generation is nonzero for a live slot, so the whole handle is nonzero
    /// without slot 63 ever carrying into the generation field — which is why
    /// the index occupies the LOW bits and is not added to a shifted generation.
    pub const fn slot_handle(generation: u32, index: usize) -> u32 {
        (generation << INDEX_BITS) | index as u32
    }

    /// Recover the slot index a handle names, or `None` when it is out of range.
    pub const fn handle_index(handle: u32) -> Option<usize> {
        let index = (handle & ((1 << INDEX_BITS) - 1)) as usize;
        if index < MAX_STREAMS {
            Some(index)
        } else {
            None
        }
    }

    /// Readiness for one decoded stream slot.
    ///
    /// A dead or generation-mismatched slot is NEVER success. Its owner must
    /// explicitly discharge any scheduler/scanout wait that still carries the
    /// old boundary before the slot is retired; accepting it here would turn a
    /// rejected producer into a false `DMA_COMPLETED` edge.
    pub const fn slot_ready(
        live: bool,
        generation: u32,
        index: usize,
        handle: u32,
        value: u32,
        retired_value: u32,
    ) -> bool {
        live && slot_handle(generation, index) == handle && retired_value >= value
    }

    /// Monotonic stream retirement: host completions may be observed out of
    /// order, and a stream value must never move backwards.
    pub const fn advance_retired(retired_value: u32, completed_value: u32) -> u32 {
        if completed_value > retired_value {
            completed_value
        } else {
            retired_value
        }
    }

    /// Encode a generation-qualified opaque present-stream boundary.
    pub const fn encode_boundary(handle: u32, value: u32) -> u64 {
        BOUNDARY_TAG | ((handle as u64) << 32) | value as u64
    }

    /// Decode a tagged boundary. Legacy wire-fence boundaries are intentionally
    /// NOT accepted here: their ordering relation is unrelated to stream values,
    /// so a numeric comparison across the two namespaces is meaningless.
    pub const fn decode_boundary(boundary: u64) -> Option<(u32, u32)> {
        if boundary & BOUNDARY_TAG == 0 {
            return None;
        }
        let handle = ((boundary >> 32) & 0x7fff_ffff) as u32;
        if handle == 0 {
            None
        } else {
            Some((handle, boundary as u32))
        }
    }

    /// A value-only lifecycle tag reaches the host-selection ledger only on a
    /// TERMINAL SUCCESSFUL response.
    ///
    /// Generic over the tag so the rule is expressible without the transport's
    /// `SyncScanoutBind`: abandoning a stack waiter detaches only the waiter,
    /// and a late `RESP_OK` must still advance lifecycle selection, while any
    /// non-OK response must not.
    pub const fn terminal_on_response_ok<T: Copy>(response_ok: bool, tag: Option<T>) -> Option<T> {
        if response_ok {
            tag
        } else {
            None
        }
    }

    /// How far a marker `value` runs AHEAD of what the producer has actually
    /// submitted on that stream; 0 when it does not run ahead at all.
    ///
    /// 0 deliberately covers both "already submitted" and "already retired":
    /// neither is a future dependency, so neither is what the instrument
    /// counts.
    pub const fn marker_lookahead(value: u32, submitted_value: u32) -> u32 {
        value.saturating_sub(submitted_value)
    }

    /// Whether the marker names producer work that has not been submitted yet.
    ///
    /// ⚠ This is an OBSERVATION, never a refusal predicate, and the difference
    /// is the whole K-F2 finding: on the shipping D3D11 present path the UMD
    /// delivers the marker BEFORE the frame's `vkQueueSubmit` on purpose (it
    /// skips its own submitted-gate precisely because the marker carries the
    /// dependency), so `true` here is the normal steady state. The tag path's
    /// direction is the opposite one — a PRODUCER advancing the stream must be
    /// strictly ahead of `submitted_value` — and conflating the two would
    /// refuse every legitimate frame.
    pub const fn marker_runs_ahead(value: u32, submitted_value: u32) -> bool {
        marker_lookahead(value, submitted_value) != 0
    }
}

#[cfg(test)]
mod present_stream_tests {
    use super::present_stream::{marker_lookahead, marker_runs_ahead};

    #[test]
    fn steady_state_marker_is_exactly_one_frame_ahead() {
        // The value is minted per Present and the tag lands on the submission
        // thread one frame behind, so this is the expected shipping reading.
        assert_eq!(marker_lookahead(41, 40), 1);
        assert!(marker_runs_ahead(41, 40));
    }

    #[test]
    fn a_fresh_stream_makes_every_marker_run_ahead() {
        // Registered, nothing tagged yet: the first frame's marker cannot be
        // behind anything, which is why "ahead" is not evidence of forgery.
        assert_eq!(marker_lookahead(1, 0), 1);
        assert!(marker_runs_ahead(1, 0));
    }

    #[test]
    fn already_submitted_or_retired_markers_do_not_run_ahead() {
        assert_eq!(marker_lookahead(40, 40), 0);
        assert!(!marker_runs_ahead(40, 40));
        assert_eq!(marker_lookahead(39, 40), 0);
        assert!(!marker_runs_ahead(39, 40));
    }

    #[test]
    fn a_forged_far_future_value_reads_as_its_own_magnitude() {
        // The whole point of the high-water: a forgery is not distinguishable
        // from a legitimate marker by direction, only by MAGNITUDE.
        assert_eq!(marker_lookahead(u32::MAX, 40), u32::MAX - 40);
        assert_eq!(marker_lookahead(1_000_000, 0), 1_000_000);
    }

    #[test]
    fn behind_by_the_full_range_saturates_instead_of_wrapping() {
        // In the shipped profile the unsaturated form would panic here, and a
        // panic in a DDI is a silent graphics deadlock.
        assert_eq!(marker_lookahead(0, u32::MAX), 0);
        assert_eq!(marker_lookahead(1, u32::MAX), 0);
        assert!(!marker_runs_ahead(0, u32::MAX));
    }
}

/// RECOVERED ASSURANCE, 2026-08-06. These six tests lived in
/// `kmd_render/src/virtio/gpu/mod.rs` as `#[cfg(test)] mod present_stream_tests`
/// — inside a `panic = "abort"` `cdylib` whose `build.rs` runs bindgen and
/// `rc.exe`, i.e. a crate that **cannot host a libtest harness at all**. They had
/// therefore never executed, on any platform, since the day they were written,
/// while covering exactly the boundary/handle helpers the D3D12 fence bridge now
/// depends on. CLAUDE.md's invariant table forbids that shape in `kmd_render`;
/// this is the violation being repaid rather than deleted.
#[cfg(test)]
mod present_stream_boundary_tests {
    use super::present_stream::{
        GENERATION_MAX, MAX_STREAMS, advance_retired, decode_boundary, encode_boundary,
        handle_index, slot_handle, slot_ready, terminal_on_response_ok,
    };

    #[test]
    fn tagged_boundary_round_trips_without_entering_legacy_namespace() {
        let boundary = encode_boundary(0x1234_5678, 77);
        assert_eq!(decode_boundary(boundary), Some((0x1234_5678, 77)));
        // 77 on its own is a legacy exclusive wire watermark, and the two
        // namespaces must never be numerically compared.
        assert_eq!(decode_boundary(77), None);
    }

    #[test]
    fn slot_63_and_new_generation_never_alias() {
        let last_index = MAX_STREAMS - 1;
        let first_handle = slot_handle(1, last_index);
        // Bit 31 stays clear: the tagged-boundary ABI reserves 31 bits.
        assert!(first_handle < (1 << 31));
        assert_ne!(first_handle, slot_handle(2, last_index));
        assert_ne!(slot_handle(1, 0), slot_handle(2, 0));
        // The index occupies the LOW bits, so the highest slot cannot carry into
        // the generation field and impersonate the next generation of slot 0.
        assert_eq!(handle_index(first_handle), Some(last_index));
        assert_eq!(handle_index(slot_handle(2, 0)), Some(0));
    }

    #[test]
    fn retirement_is_monotonic_even_when_completions_reorder() {
        let retired = advance_retired(9, 4);
        assert_eq!(retired, 9);
        assert_eq!(advance_retired(retired, 12), 12);
    }

    #[test]
    fn dead_or_generation_mismatched_stream_boundary_never_reads_as_retired() {
        // live, generation 3, slot 0, retired through 6.
        let handle = slot_handle(3, 0);
        assert!(!slot_ready(true, 3, 0, handle, 7, 6));
        assert!(slot_ready(true, 3, 0, handle, 6, 6));
        // A dead slot is not ready even for a value it has already passed.
        assert!(!slot_ready(false, 3, 0, handle, 1, 6));
        // A handle from another generation is not ready either. Flipping bit 6
        // is the lowest generation bit, i.e. the nearest possible collision.
        assert!(!slot_ready(true, 3, 0, handle ^ (1 << 6), 1, 6));
    }

    #[test]
    fn late_successful_sync_set_retains_its_lifecycle_tag() {
        // `abandon_sync` detaches only the stack waiter; the tag remains on the
        // entry and a later RESP_OK must still advance lifecycle selection.
        let bind = (41u64, 77u32);
        assert_eq!(terminal_on_response_ok(true, Some(bind)), Some(bind));
        assert_eq!(terminal_on_response_ok(false, Some(bind)), None);
        assert_eq!(terminal_on_response_ok::<(u64, u32)>(true, None), None);
    }

    #[test]
    fn generation_ceiling_stays_inside_the_reserved_handle_width() {
        // The registration scan refuses a slot whose generation has reached
        // GENERATION_MAX; the largest handle it can therefore mint must still
        // fit the 31 bits the boundary ABI reserves.
        assert!(slot_handle(GENERATION_MAX, MAX_STREAMS - 1) < (1 << 31));
    }
}
