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
            EXT_FULL[0], EXT_FULL[1], EXT_FULL[2], EXT_FULL[3], EXT_FULL[4], BIG, BIG, BIG, BIG,
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
        MEMORY_PROPERTY_DEVICE_LOCAL
            | MEMORY_PROPERTY_HOST_VISIBLE
            | MEMORY_PROPERTY_HOST_COHERENT,
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
    fn reference(existing: &[(u64, u64)], first: u64, end: u64, block: &[(u64, u64)]) -> Vec<(u64, u64)> {
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
    fn spliced(existing: &[(u64, u64)], first: u64, end: u64, block: &[(u64, u64)]) -> Vec<(u64, u64)> {
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
            assert!(got.windows(2).all(|w| w[0].0 <= w[1].0), "case {case} unsorted");
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
