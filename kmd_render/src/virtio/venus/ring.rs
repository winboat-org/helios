//! The venus ring transport: the only unchecked MMIO in the driver.
//!
//! `KernelMap` and its five raw accessors, the typed `RingMap`/`RingWord`
//! wrapper over them, the bounded `ReplyReader`, the reply-validation policy
//! and `VenusRing`'s publish/notify/wait cycle.
//!
//! Moved verbatim out of `virtio/venus.rs` by T8/R1104. `ReplyReader` moves
//! with `KernelMap` because it borrows one, so it cannot sit in an
//! adapter-free `protocol.rs`.

use super::*;

/// A kernel mapping of a guest-physical sub-range of the host-visible BAR window.
///
/// Wraps `MmMapIoSpace`/`MmUnmapIoSpace` with RAII. The KMD reads/writes the venus
/// ring and reply buffers through this VA. Created and dropped at PASSIVE_LEVEL.
pub(super) struct KernelMap {
    va: *mut u8,
    size: u64,
}

impl KernelMap {
    /// Map `[gpa, gpa+size)` into kernel VA with caching chosen from the host's
    /// `map_cache` nibble (`VIRTIO_GPU_MAP_CACHE_*`). Returns `None` on failure.
    pub(super) fn new(gpa: u64, size: u64, map_cache: u32) -> Option<Self> {
        let caching = match map_cache {
            VIRTIO_GPU_MAP_CACHE_CACHED => _MEMORY_CACHING_TYPE::MmCached,
            VIRTIO_GPU_MAP_CACHE_WC => _MEMORY_CACHING_TYPE::MmWriteCombined,
            VIRTIO_GPU_MAP_CACHE_UNCACHED => _MEMORY_CACHING_TYPE::MmNonCached,
            // Unknown / NONE: default to cached. Host-visible venus memory is WB.
            _ => _MEMORY_CACHING_TYPE::MmCached,
        };
        let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
        pa.QuadPart = gpa as i64;
        // SAFETY: PASSIVE_LEVEL. `MmMapIoSpace` gives a kernel VA for exactly
        // `size` bytes starting at `gpa`, valid until `MmUnmapIoSpace`, which
        // `Drop` calls exactly once.
        //
        // That is ALL it gives. Whether `[gpa, gpa+size)` is host-backed depends
        // on the host having honoured RESOURCE_MAP_BLOB for this window range,
        // which no Rust type can witness — and it is a separate claim from
        // "`size` is big enough for what the caller intends to put here", which
        // this constructor deliberately does not make. See [`RingMap`] for the
        // wrapper that does make it.
        let va = unsafe { MmMapIoSpace(pa, size, caching) } as *mut u8;
        if va.is_null() {
            return None;
        }
        Some(Self { va, size })
    }

    /// Zero the whole mapping (volatile, byte by byte — the region is MMIO).
    pub(super) fn zero(&self) {
        // SAFETY: `va` owns `size` mapped bytes for our lifetime.
        for i in 0..self.size {
            unsafe { core::ptr::write_volatile(self.va.add(i as usize), 0u8) };
        }
    }

    /// Volatile u32 load at byte `offset` (Acquire-ordered for ring head/status).
    ///
    /// Private to the module: the only caller is [`RingMap`], which proves
    /// `offset + 4 <= size` at construction for the closed set of [`RingWord`]
    /// offsets. Do not add a second caller without the same proof.
    fn load_u32_acquire(&self, offset: u64) -> u32 {
        // SAFETY: `offset+4 <= size`, discharged by RingMap::new checking
        // `size >= RING_SHMEM_SIZE` and by RingWord having no inhabitant above
        // RING_STATUS_OFFSET. Aligned 4-byte MMIO read.
        let p = unsafe { self.va.add(offset as usize) } as *const u32;
        let v = unsafe { core::ptr::read_volatile(p) };
        // The producer side of vn_ring loads head/status with acquire; a fence
        // after the volatile read gives the same ordering against later reads.
        fence(Ordering::Acquire);
        v
    }

    /// Volatile u32 store at byte `offset` with a full (SeqCst) fence first — the
    /// vn_ring tail-store contract (a full mfence so the host's acquire load is
    /// ordered after our buffer writes).
    fn store_u32_seqcst(&self, offset: u64, val: u32) {
        // Full barrier: all prior buffer writes are visible before the tail store.
        fence(Ordering::SeqCst);
        // SAFETY: `offset+4 <= size`. Two callers, each discharging it its own
        // way: RingMap, where RingWord has no inhabitant above
        // RING_STATUS_OFFSET and the map is >= RING_SHMEM_SIZE by construction;
        // and the reply-poison write at offset 0, which any nonzero-size
        // mapping admits.
        let p = unsafe { self.va.add(offset as usize) } as *mut u32;
        unsafe { core::ptr::write_volatile(p, val) };
        fence(Ordering::SeqCst);
    }

    /// Copy `src` into the ring buffer at free-running counter `cur`, splitting at
    /// the power-of-two wrap. `cur` and the buffer mask follow vn_ring exactly.
    ///
    /// Reached only through [`RingMap::write_buffer`]; the caller's own bound
    /// (`write_to_ring` refuses `src.len() > RING_BUFFER_SIZE`) plus the mask
    /// keep both halves inside the buffer window.
    fn write_ring_buffer(&self, cur: u32, src: &[u8]) {
        let mask = RING_BUFFER_SIZE - 1;
        let offset = (cur & mask) as u64;
        let first = core::cmp::min(src.len() as u64, RING_BUFFER_SIZE as u64 - offset);
        // SAFETY: buffer base + in-range offsets; first + second == src.len() and
        // both halves are within [RING_BUFFER_OFFSET, RING_EXTRA_OFFSET), which
        // RingMap::new proved is inside the mapping (size >= RING_SHMEM_SIZE).
        unsafe {
            let base = self.va.add(RING_BUFFER_OFFSET as usize);
            for i in 0..first {
                core::ptr::write_volatile(base.add((offset + i) as usize), src[i as usize]);
            }
            let rest = src.len() as u64 - first;
            for i in 0..rest {
                core::ptr::write_volatile(base.add(i as usize), src[(first + i) as usize]);
            }
        }
        // Ensure the buffer bytes land before the caller publishes the tail.
        compiler_fence(Ordering::Release);
    }

    /// Volatile byte load at `offset` (for reply decoding out of the reply
    /// shmem), or `None` if `offset` is outside the mapping.
    ///
    /// The check used to be a comment saying "caller bounds-checks
    /// `offset < size`". `ReplyReader` does; `probe_present_destination`'s four
    /// sample reads only argued about it. Making it an `Option` costs one
    /// compare per byte on paths that are already byte-at-a-time volatile MMIO.
    pub(super) fn read_byte(&self, offset: u64) -> Option<u8> {
        if offset >= self.size {
            return None;
        }
        // SAFETY: `offset < size`, just checked; `va` owns `size` mapped bytes.
        Some(unsafe { core::ptr::read_volatile(self.va.add(offset as usize)) })
    }
}

/// One of the three ring header words. A closed set with no other inhabitants,
/// so an out-of-range ring header offset is not expressible.
#[derive(Clone, Copy)]
enum RingWord {
    /// Host-owned consumer position.
    Head,
    /// Guest-owned producer position.
    Tail,
    /// Host-owned status bits (`RING_STATUS_FATAL`, idle).
    Status,
}

impl RingWord {
    const fn offset(self) -> u64 {
        match self {
            Self::Head => RING_HEAD_OFFSET,
            Self::Tail => RING_TAIL_OFFSET,
            Self::Status => RING_STATUS_OFFSET,
        }
    }
}

/// A [`KernelMap`] proven at construction to be at least [`RING_SHMEM_SIZE`]
/// bytes, so every ring accessor's bounds obligation is a constructor
/// postcondition rather than a per-call caller obligation.
///
/// `KernelMap::new` maps whatever length it is handed, and the ring accessors
/// never consulted `self.size` at all — four **safe** fns dereferencing
/// `va + offset` under SAFETY comments that asserted a different invariant than
/// the one they needed. Nothing in the type system distinguished a ring-sized
/// mapping from an arbitrary one, so a second ring mapping site, a re-map
/// through `blob_map_begin`'s `Mapped` arm (which returns the previously
/// recorded `map_len` verbatim), or a host reporting a short `map_len` yielded
/// an undersized map whose writers then ran up to `RING_BUFFER_OFFSET + 131072`
/// bytes past the end.
///
/// This does NOT remove `unsafe` — it constrains which offsets can reach it. The
/// one surviving unsafe precondition is `MmMapIoSpace`'s, which is the point.
pub(super) struct RingMap(KernelMap);

impl RingMap {
    /// Map the ring shmem, refusing a mapping too small to hold it.
    ///
    /// Records `VnRingSz` = the offered size in KiB on refusal, so a short host
    /// `map_len` is named rather than inferred from a later ring desync.
    pub(super) fn new(gpa: u64, size: u64, map_cache: u32) -> Option<Self> {
        if size < RING_SHMEM_SIZE {
            crate::diag::record_named_bytes(b"VnRingSz", (size / 1024) as u32);
            return None;
        }
        KernelMap::new(gpa, size, map_cache).map(Self)
    }

    pub(super) fn zero(&self) {
        self.0.zero();
    }

    fn load_acquire(&self, word: RingWord) -> u32 {
        self.0.load_u32_acquire(word.offset())
    }

    fn store_seqcst(&self, word: RingWord, val: u32) {
        self.0.store_u32_seqcst(word.offset(), val);
    }

    /// Copy `src` into the ring buffer window at free-running counter `cur`.
    fn write_buffer(&self, cur: u32, src: &[u8]) {
        self.0.write_ring_buffer(cur, src);
    }
}

impl Drop for KernelMap {
    fn drop(&mut self) {
        if !self.va.is_null() {
            // SAFETY: `va` came from `MmMapIoSpace` in `new`; unmapped once here.
            unsafe { MmUnmapIoSpace(self.va as *mut _, self.size) };
        }
    }
}

// ── Handing the encoded bytes out ───────────────────────────────────

/// The kernel-side half of [`Writer`]: turn a finished stream into bytes, or
/// refuse loudly.
///
/// `helios_kmd_logic` has no `VirtioError` and no `diag` (that is the point of
/// its absent dependency edge), so it reports overflow as `None`. Naming the
/// refusal and picking the error code is this crate's job.
pub(super) trait EncodedStream {
    /// The encoded bytes, or `DeviceError` if any write overflowed.
    ///
    /// Records `VnEncOvf` = the `VkCommandTypeEXT` that overflowed.
    /// `record_named_bytes` is NOT `DiagLevel`-gated (unlike [`diag`]), so the
    /// refusal is visible in a `reg query` on a production boot. It writes the
    /// registry, so it must run at PASSIVE — which every venus encoder does:
    /// `ring_wait_until` sleeps via `ctrl::sleep_ms`, so the whole ring API is
    /// PASSIVE-only.
    fn as_slice(&self) -> Result<&[u8], VirtioError>;
}

impl EncodedStream for Writer {
    fn as_slice(&self) -> Result<&[u8], VirtioError> {
        match self.finished() {
            Some(bytes) => Ok(bytes),
            None => {
                crate::diag::record_named_bytes(b"VnEncOvf", self.cmd_type());
                Err(VirtioError::DeviceError)
            }
        }
    }
}

// ── A bounds-checked LE reader over the reply shmem ───────────────────────────

/// The reply reader, in its own module so its constructor can be PRIVATE.
///
/// R1001. The shmem is reused at offset 0 for every command, so a decode
/// without a command-id check reads the PREVIOUS command's reply as its own and
/// produces a plausible-looking success. Twenty-eight sites hand-wrote that
/// check; nothing made them.
///
/// Inside this module `new` is private and [`ReplyReader::open`] is the only way
/// out, so a reader positioned at offset 0 with an UNCHECKED command id is not
/// constructible from anywhere in `venus.rs`. That is a real compile-time
/// property, not a convention.
///
/// The other half of the finding -- "decode a reply without having issued the
/// matching request" -- is NOT expressible this way, because `open` still takes
/// a `&KernelMap` that any code in this file could reach. It is enforced
/// instead by there being exactly one caller: `VenusRing::ring_command_expect`,
/// which publishes and waits first and hands the reader back. Said plainly
/// rather than claimed as a type-level guarantee.
pub(super) mod reply {
    use super::{KernelMap, VirtioError};

    /// Reads little-endian scalars out of the host-written reply buffer. The
    /// reply is **untrusted** input: every read is bounds-checked against the
    /// mapping size and returns `VirtioError::DeviceError` on overrun, so a
    /// malformed reply can never index out of bounds.
    pub(in crate::virtio::venus) struct ReplyReader<'a> {
        map: &'a KernelMap,
        /// Absolute byte offset within the mapping (reply lives at offset 0).
        pos: u64,
        /// Hard cap = reply mapping size.
        end: u64,
    }

    /// The reply's first word was not the command that was issued.
    ///
    /// Deliberately NOT a `VirtioError`: the caller has a per-site diag code
    /// and breadcrumb to record before converting it, and a bare
    /// `VirtioError::DeviceError` would let a site forget.
    pub(in crate::virtio::venus) struct CommandMismatch;

    impl<'a> ReplyReader<'a> {
        fn new(map: &'a KernelMap) -> Self {
            Self {
                map,
                pos: 0,
                end: map.size,
            }
        }

        /// The ONLY constructor reachable outside this module: read word 0 and
        /// require it to be `expected` before handing back a reader positioned
        /// after it.
        ///
        /// A short reply (word 0 not even readable) is `Err(Err(DeviceError))`,
        /// matching what the hand-written `r.read_i32()?` did.
        #[allow(clippy::result_unit_err)]
        pub(in crate::virtio::venus) fn open(
            map: &'a KernelMap,
            expected: u32,
        ) -> Result<Result<Self, CommandMismatch>, VirtioError> {
            let mut r = Self::new(map);
            let cmd = r.read_i32()?;
            if cmd as u32 != expected {
                return Ok(Err(CommandMismatch));
            }
            Ok(Ok(r))
        }

        pub(in crate::virtio::venus) fn read_u32(&mut self) -> Result<u32, VirtioError> {
            if self.pos + 4 > self.end {
                return Err(VirtioError::DeviceError);
            }
            let mut b = [0u8; 4];
            for (i, slot) in b.iter_mut().enumerate() {
                *slot = self
                    .map
                    .read_byte(self.pos + i as u64)
                    .ok_or(VirtioError::DeviceError)?;
            }
            self.pos += 4;
            Ok(u32::from_le_bytes(b))
        }

        pub(in crate::virtio::venus) fn read_i32(&mut self) -> Result<i32, VirtioError> {
            Ok(self.read_u32()? as i32)
        }

        pub(in crate::virtio::venus) fn read_u64(&mut self) -> Result<u64, VirtioError> {
            if self.pos + 8 > self.end {
                return Err(VirtioError::DeviceError);
            }
            let mut b = [0u8; 8];
            for (i, slot) in b.iter_mut().enumerate() {
                *slot = self
                    .map
                    .read_byte(self.pos + i as u64)
                    .ok_or(VirtioError::DeviceError)?;
            }
            self.pos += 8;
            Ok(u64::from_le_bytes(b))
        }

        /// Skip `n` bytes, bounds-checked.
        ///
        /// Unused today -- every current reply is parsed field by field. Kept
        /// because it is the ONLY bounds-checked way to advance this reader,
        /// and a future reply with a variable-length prefix that has to
        /// hand-roll `self.pos += n` is exactly the unchecked-advance bug this
        /// type exists to prevent. Pre-dates T6. R906.
        #[allow(dead_code)]
        pub(in crate::virtio::venus) fn skip(&mut self, n: u64) -> Result<(), VirtioError> {
            if self.pos + n > self.end {
                return Err(VirtioError::DeviceError);
            }
            self.pos += n;
            Ok(())
        }
    }
}

pub(super) use reply::ReplyReader;

/// What a reply-issuing site wants checked, and what to record when it fails.
///
/// R1001. Every one of the twenty-eight decode sites hand-wrote "read word 0,
/// compare it to my command, record my diag code" and nineteen of them also
/// hand-wrote the `VkResult` check. The per-site *values* are real -- each diag
/// code and breadcrumb name identifies which command failed and is owner
/// debugging ABI -- but the *shape* was copied. This carries the values and
/// [`VenusRing::ring_command_expect`] performs the shape.
#[derive(Clone, Copy)]
pub(super) struct ReplyCheck {
    expected: u32,
    /// `diag` code recorded when word 0 is not `expected`.
    mismatch_diag: Option<u32>,
    /// A named breadcrumb recorded as `0xE0` on a command mismatch.
    mismatch_mark: Option<&'static [u8]>,
    result: ResultPolicy,
}

/// What to do with the reply's `VkResult` word.
#[derive(Clone, Copy)]
enum ResultPolicy {
    /// Do not read it. Either the reply carries none (the three
    /// `Get*Requirements`/`GetDeviceQueue2`/memory-properties shapes, whose
    /// second word is a simple-pointer, not a result) or the caller
    /// interprets it itself (`VK_INCOMPLETE` is acceptable to
    /// `vkEnumeratePhysicalDevices`; the ext-ladder reads it to choose a tier).
    Deferred,
    /// Read it and refuse a non-zero value.
    Refuse {
        diag: Option<u32>,
        /// A named breadcrumb recorded as the RAW `VkResult` before refusing.
        /// The six host-VkResult breadcrumbs (`SdgLImg`, `CpImgVr`, `PBBufVr`,
        /// `SdgLMem`, `CpMemVr`, `SdgDevR`) are the owner's first look at a
        /// host-side rejection.
        mark: Option<&'static [u8]>,
    },
}

impl ReplyCheck {
    /// Check only that the reply is for `expected`; record nothing, and leave
    /// the `VkResult` word to the caller.
    pub(super) const fn new(expected: u32) -> Self {
        Self {
            expected,
            mismatch_diag: None,
            mismatch_mark: None,
            result: ResultPolicy::Deferred,
        }
    }

    pub(super) const fn mismatch(mut self, code: u32) -> Self {
        self.mismatch_diag = Some(code);
        self
    }

    /// Record `name = 0xE0` when the command id does not match.
    pub(super) const fn mismatch_marks(mut self, name: &'static [u8]) -> Self {
        self.mismatch_mark = Some(name);
        self
    }

    /// Read the `VkResult` word and refuse a non-zero value, recording `code`.
    pub(super) const fn refuse_result(mut self, code: u32) -> Self {
        self.result = ResultPolicy::Refuse {
            diag: Some(code),
            mark: None,
        };
        self
    }

    /// As [`Self::refuse_result`], but the site records no diag code.
    pub(super) const fn refuse_result_undiagnosed(mut self) -> Self {
        self.result = ResultPolicy::Refuse {
            diag: None,
            mark: None,
        };
        self
    }

    /// Record `name = <raw VkResult>` before refusing. Only meaningful after
    /// one of the `refuse_result*` builders.
    pub(super) const fn result_marks(mut self, name: &'static [u8]) -> Self {
        if let ResultPolicy::Refuse { diag, .. } = self.result {
            self.result = ResultPolicy::Refuse {
                diag,
                mark: Some(name),
            };
        }
        self
    }
}

/// Bring-up stage 1: the venus ring exists and is registered with the host, but
/// no Vulkan object does.
///
/// Splitting this out is the point of the typestate. `VenusClient` used to be
/// constructed complete-looking at stage 2 and then mutated through seven more
/// ordered stages; between them it was a perfectly valid `VenusClient` whose
/// `device_id`/`queue_id`/`memory_type_index` were 0, and every one of its ~40
/// methods would happily encode `VkDevice 0` into the wire stream — which the
/// host answers by poisoning the ring. The ordering was enforced only by the
/// linear layout of one 400-line function, so hoisting any helper above its
/// stage (a capability probe before the CreateDevice ladder, say) compiled.
///
/// Now the stages are types and each transition consumes the previous value, so
/// no stage can be skipped or reordered and no later stage's method is even
/// nameable earlier.
pub(super) struct VenusRing {
    /// Guest-assigned ring handle token (reused everywhere the ring is named).
    pub(super) ring_id: u64,
    /// Ring shmem blob resource id.
    ///
    /// ⚠ NOT consumed by any teardown. The doc used to say "for unref on
    /// teardown" and no unref exists -- T6/R916 chose to correct the claim
    /// rather than delete the field, because the id is the only in-driver
    /// record of which host resource backs the ring and a future teardown
    /// needs it. It is kept as documentation-only state, and saying so is the
    /// point: a field that promises a lifecycle it does not implement is worse
    /// than one that admits it.
    #[allow(dead_code)]
    pub(super) ring_res_id: u32,
    /// Reply shmem blob resource id. Unlike `ring_res_id` this one IS read --
    /// `VkCommandStreamDescriptionMESA.resourceId` names it on every submit.
    pub(super) reply_res_id: u32,
    /// Kernel mapping of the ring shmem.
    pub(super) ring_map: RingMap,
    /// Kernel mapping of the reply shmem.
    pub(super) reply_map: KernelMap,
    /// Free-running ring producer counter (vn_ring `ring->cur`).
    pub(super) cur: u32,
    /// Monotonic notify seqno.
    pub(super) notify_seqno: u32,
    /// Monotonic virtqueue roundtrip seqno (for the reply-shmem warm-up).
    /// Next guest-assigned Vulkan handle id. `NonZeroU64` because 0 is
    /// `VK_NULL_HANDLE`: it is the value the handle newtypes exist to keep out
    /// of the wire stream, so the counter must not be able to produce it.
    pub(super) next_handle: NonZeroU64,
    /// The persistent venus 3D context id all commands ride.
    pub(super) ctx_id: u32,
    /// The PASSIVE proof (R614) every ring/client `ctrl::` call rides.
    ///
    /// Minted once, by StartDevice, and handed to [`VenusRing::bring_up`] as a
    /// parameter. It stands in for the caller's token on the ~89 `VenusClient`
    /// methods that take none, and that substitution is sound ONLY because
    /// `AdapterContext::with_venus_client` — the single gateway to a
    /// `&mut VenusClient` — requires its caller's own token. Read that gateway's
    /// doc before adding any other way to reach a client.
    ///
    /// A ZST, so this field costs the struct nothing.
    pub(super) passive: PassiveLevel,
    /// Poison latch: set when a ring wait exhausts [`RING_WAIT_TIMEOUT_MS`] or
    /// the host reports RING_STATUS_FATAL. A wedged/fatal ring never recovers,
    /// and without the latch every subsequent call re-burned the full wait
    /// budget, which is the 2026-07-03 guest wedge. Once set, every ring command
    /// fails fast with `DeviceError`.
    ///
    /// Write it ONLY through [`VenusRing::latch_fatal`], which names the reason
    /// in the registry. These waits are PASSIVE-only (`ring_wait_until` sleeps
    /// via `ctrl::sleep_ms`), which is what makes that registry write legal.
    pub(super) fatal: bool,
}

impl VenusRing {
    /// Mint the next raw handle. Private: every caller goes through one of the
    /// typed constructors below, so a handle cannot be created without deciding
    /// what class of object it names.
    pub(super) fn next_raw(&mut self) -> NonZeroU64 {
        let h = self.next_handle;
        // Handles are never recycled, and must never wrap into the zero that
        // means "absent". At one handle per Present the bound is unreachable;
        // saturating is nevertheless the right failure mode, because reusing 1
        // would collide with a live host object.
        self.next_handle = self.next_handle.checked_add(1).unwrap_or(self.next_handle);
        h
    }

    /// Latch the ring poison, naming the reason in the registry.
    ///
    /// This is the ONLY writer of `self.fatal`; `grep -n 'self.fatal = true'`
    /// returning just this body is the invariant. Before it, a wedged ring left
    /// zero evidence on a production boot: the three latch sites recorded
    /// nothing, `diag::record` is `DiagLevel`-gated off by default, and the DDI
    /// returned a bare `DeviceError` indistinguishable from every other one —
    /// destroying the post-mortem for exactly the failure the 30 s budget exists
    /// to survive.
    ///
    /// `record_named_bytes` writes the registry and so must run at PASSIVE. All
    /// three callers are on the PASSIVE ring path: `ring_wait_until` sleeps via
    /// `ctrl::sleep_ms` (`KeDelayExecutionThread`), which is only legal there.
    pub(super) fn latch_fatal(&mut self, reason: FatalReason) {
        self.fatal = true;
        match reason {
            FatalReason::HostStatusFatal => crate::diag::record_named_bytes(b"VnRingFt", 1),
            FatalReason::HeadWaitTimeout { elapsed_ms } => {
                crate::diag::record_named_bytes(b"VnRingWd", elapsed_ms as u32)
            }
        }
    }

    /// Send a direct (non-ring) venus command via `VIRTIO_GPU_CMD_SUBMIT_3D`. Used
    /// for the ring-bootstrap commands (`vkCreateRingMESA`, `vkNotifyRingMESA`,
    /// `vkSubmit/WaitVirtqueueSeqnoMESA`) which must reach the host before / around
    /// the ring being usable. Blocks at PASSIVE (virtio::ctrl KEVENT wait) until
    /// the device acks the command.
    pub(super) fn submit_direct(
        &self,
        adapter: &AdapterContext,
        stream: &[u8],
    ) -> Result<(), VirtioError> {
        ctrl::submit_venus_sync(self.passive, adapter, self.ctx_id, stream)
    }

    /// Publish the ring buffer up to `self.cur` (SeqCst tail store), then nudge the
    /// host if the ring reports idle. Returns the seqno of the just-written command
    /// (== the post-write `cur`). Aborts on a fatal ring status.
    pub(super) fn publish_and_notify(
        &mut self,
        adapter: &AdapterContext,
    ) -> Result<u32, VirtioError> {
        if self.fatal {
            return Err(VirtioError::DeviceError);
        }
        let seqno = self.cur;
        self.ring_map.store_seqcst(RingWord::Tail, seqno);

        let status = self.ring_map.load_acquire(RingWord::Status);
        if status & RING_STATUS_FATAL != 0 {
            self.latch_fatal(FatalReason::HostStatusFatal);
            return Err(VirtioError::DeviceError);
        }
        // vkNotifyRingMESA(ring, seqno, flags=0): wake the host ring dispatch.
        // Sent UNCONDITIONALLY (not gated on the IDLE status bit): the host idles
        // after a 1 ms timeout and the guest's read of the IDLE bit from the ring
        // shmem is racy/coherency-sensitive — always nudging is correct and cheap.
        // vkNotifyRingMESA is a valid DIRECT command (unlike
        // vkWaitVirtqueueSeqnoMESA, which the host rejects off-ring).
        self.notify_seqno = self.notify_seqno.wrapping_add(1);
        let mut w = Writer::new();
        w.header(CMD_NOTIFY_RING_MESA, 0);
        w.u64(self.ring_id);
        w.u32(self.notify_seqno);
        w.u32(0); // VkRingNotifyFlagsMESA
        self.submit_direct(adapter, w.as_slice()?)?;
        Ok(seqno)
    }

    /// PASSIVE ring-progress wait: run `ready()` until it returns true, with a
    /// short spin burst then 1 ms sleeps, bounded by [`RING_WAIT_TIMEOUT_MS`].
    /// Checks the ring FATAL status each round. Latches `fatal` on timeout.
    fn ring_wait_until(&mut self, ready: impl Fn(&Self) -> bool) -> Result<(), VirtioError> {
        if self.fatal {
            return Err(VirtioError::DeviceError);
        }
        for _ in 0..RING_SPIN_BURST {
            if ready(self) {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        let mut slept_ms: u64 = 0;
        loop {
            if ready(self) {
                return Ok(());
            }
            let status = self.ring_map.load_acquire(RingWord::Status);
            if status & RING_STATUS_FATAL != 0 {
                // Host declared the ring fatal — it never recovers.
                self.latch_fatal(FatalReason::HostStatusFatal);
                return Err(VirtioError::DeviceError);
            }
            if slept_ms >= RING_WAIT_TIMEOUT_MS {
                self.latch_fatal(FatalReason::HeadWaitTimeout {
                    elapsed_ms: slept_ms,
                });
                return Err(VirtioError::DeviceError);
            }
            ctrl::sleep_ms(self.passive, 1);
            slept_ms += 1;
        }
    }

    /// Reserve space and copy `stream` into the ring buffer at `self.cur`,
    /// advancing `cur` (vn_ring producer). Waits (PASSIVE) until the host has
    /// consumed enough to make room (`cur + size - head <= buffer_size`).
    fn write_to_ring(&mut self, stream: &[u8]) -> Result<(), VirtioError> {
        if self.fatal {
            return Err(VirtioError::DeviceError);
        }
        let size = stream.len() as u32;
        if size as u64 > RING_BUFFER_SIZE as u64 {
            return Err(VirtioError::DeviceError);
        }
        // Wait for buffer space (u32 free-running arithmetic, wrap-preserving):
        // occupancy after this write = cur + size - head; must be <= buffer_size.
        let cur = self.cur;
        self.ring_wait_until(move |c| {
            let head = c.ring_map.load_acquire(RingWord::Head);
            cur.wrapping_add(size).wrapping_sub(head) <= RING_BUFFER_SIZE
        })?;
        self.ring_map.write_buffer(self.cur, stream);
        self.cur = self.cur.wrapping_add(size);
        Ok(())
    }

    /// Wait (PASSIVE) until the ring head reaches `seqno` (the host has consumed
    /// and completed the command). Wrap-safe `(i32)(head - seqno) >= 0` compare.
    fn wait_seqno(&mut self, seqno: u32) -> Result<(), VirtioError> {
        self.ring_wait_until(move |c| {
            let head = c.ring_map.load_acquire(RingWord::Head);
            (head.wrapping_sub(seqno)) as i32 >= 0
        })
    }

    /// Issue a command WITHOUT a reply through the ring: write → publish → wait.
    pub(super) fn ring_command_noreply(
        &mut self,
        adapter: &AdapterContext,
        stream: &[u8],
    ) -> Result<(), VirtioError> {
        self.write_to_ring(stream)?;
        let seqno = self.publish_and_notify(adapter)?;
        self.wait_seqno(seqno)
    }

    /// Issue a command WITH a reply through the ring. First points the host at the
    /// reply shmem (`vkSetReplyCommandStreamMESA`), then writes the real command
    /// with the GENERATE_REPLY flag, publishes once, and waits. The reply lands at
    /// reply-shmem offset 0.
    ///
    /// PRIVATE to [`Self::ring_command_expect`], which is its only caller. That
    /// is R1001's second half: publishing a reply-generating command and
    /// obtaining a reader for it are one operation, so a decode cannot be
    /// written against a reply nobody asked for.
    pub(super) fn ring_command_reply(
        &mut self,
        adapter: &AdapterContext,
        cmd_stream: &[u8],
    ) -> Result<(), VirtioError> {
        // vkSetReplyCommandStreamMESA: point the host at reply shmem [0, size).
        let mut set = Writer::new();
        set.header(CMD_SET_REPLY_COMMAND_STREAM_MESA, 0);
        set.count(true); // simple_pointer(pStream)
        set.u32(self.reply_res_id); // VkCommandStreamDescriptionMESA.resourceId
        set.u64(0); // .offset
        set.u64(REPLY_SHMEM_SIZE); // .size
        self.write_to_ring(set.as_slice()?)?;

        // The real command, with GENERATE_REPLY.
        self.write_to_ring(cmd_stream)?;

        // Poison the reply's command-type word before publishing.
        //
        // The reply shmem is zeroed EXACTLY ONCE, at bring-up, so a command
        // whose reply the host never writes decodes as whatever the PREVIOUS
        // reply left there — and every caller's first check is "is word 0 my
        // command type?". After a `vkCreateImage` reply, a silently-unanswered
        // second `vkCreateImage` passes that check and the caller goes on to
        // read stale handles as if they were fresh. 0xFFFF_FFFF is not a legal
        // VkCommandTypeEXT, so it fails every existing check with no new code.
        //
        // Deliberately NOT bundled: uniform strict handle validation. The three
        // adopt sites accept a host-substituted handle and tightening them is a
        // real behaviour change that could break bring-up on a host that
        // legitimately returns a different handle.
        self.reply_map.store_u32_seqcst(0, REPLY_POISON);

        let seqno = self.publish_and_notify(adapter)?;
        self.wait_seqno(seqno)?;
        Ok(())
    }

    /// Issue a command WITH a reply and hand back a reader positioned past the
    /// header words `check` consumed.
    ///
    /// R1001. This is the only way to obtain a [`ReplyReader`]: `open` is the
    /// reader's sole public constructor and it performs the command-id check
    /// itself, so a decode that skips that check does not compile. Publishing
    /// and decoding being one call is what stops a reader being built over the
    /// PREVIOUS command's reply.
    ///
    /// The returned reader borrows `self` for its lifetime, so a caller that
    /// needs `&mut self` afterwards must read what it wants into locals and
    /// drop the reader first -- which is the correct discipline anyway.
    pub(super) fn ring_command_expect(
        &mut self,
        adapter: &AdapterContext,
        cmd_stream: &[u8],
        check: ReplyCheck,
    ) -> Result<ReplyReader<'_>, VirtioError> {
        self.ring_command_reply(adapter, cmd_stream)?;

        let opened = ReplyReader::open(&self.reply_map, check.expected)?;
        let mut r = match opened {
            Ok(r) => r,
            Err(_mismatch) => {
                if let Some(name) = check.mismatch_mark {
                    crate::diag::record_named_bytes(name, 0xE0);
                }
                if let Some(code) = check.mismatch_diag {
                    diag(code);
                }
                return Err(VirtioError::DeviceError);
            }
        };

        if let ResultPolicy::Refuse { diag: code, mark } = check.result {
            let result = r.read_i32()?;
            if result != 0 {
                if let Some(name) = mark {
                    crate::diag::record_named_bytes(name, result as u32);
                }
                if let Some(code) = code {
                    diag(code);
                }
                return Err(VirtioError::DeviceError);
            }
        }

        Ok(r)
    }
}
