//! L4's second file — the resource → **kernel-allocation-identity** table.
//!
//! `KMD_IMPACT.md` §14a.3 UP-4. This is the bookkeeping the D3D12 present path
//! needs and nothing else in this driver has: for a resource that may reach the
//! scanout, the chain
//!
//! ```text
//! HRESOURCE -> ResourceState -> ID3D12Resource* -> VkDeviceMemory
//!           -> helios_venus_memory_res_id -> pfnAllocateCb{adopt_resource_id}
//!           -> D3DKMT_HANDLE
//! ```
//!
//! has to be walkable *after* the create that built it. Every link but the
//! fourth arrow exists on all three sides already (§14a.3); what does not exist
//! is anywhere to *keep* the answer, because [`super::resource12::ResourceState`]
//! is written once inside `pfnCreateHeapAndResource` and read by handle, while
//! `pfnPresent`, `pfnCheckResourceAllocationHandle` and the unwind path all need
//! the same record.
//!
//! ⚠ **No longer inert, and the entry condition changed with it.** At UP-4 nothing
//! consumed this table and every entry's `vk_memory`/`venus_res_id` halves were 0
//! and counted. UP-5 landed the `pfnAllocateCb` call, and the rule is now stronger
//! than "filled in": **an entry exists if and only if this driver owns a WDDM
//! allocation for that resource**. [`record`] is called only after the callback
//! returned a handle, so
//!
//! * `h_allocation != 0` on every recorded entry — a partially-resolved identity is
//!   no longer representable in the table, it is a refusal at the create;
//! * `pfnDestroyHeapAndResource` must `pfnDeallocateCb` exactly the entries
//!   [`take`] hands back, which makes the deallocate unconditional rather than
//!   gated on a second flag that could disagree with the table;
//! * `IdentityRecorded − IdentityRemoved` is therefore both the live-entry count
//!   **and** the live WDDM-allocation count, so the 54th session's leak class is
//!   one subtraction away rather than needing its own instrument.
//!
//! # ⛔ D13: this table is NOT shared private data
//!
//! `DECISIONS.md` D13's refined form: private data that crosses a module
//! boundary is declared once in `helios_protocol`; private data that does not
//! stays in the crate that owns it. **Nothing outside `helios_umd12.dll` reads a
//! byte of this table.** It is process-local bookkeeping whose *outputs* become
//! `HeliosWddmAllocPrivate` + `HeliosWddmAllocMeta` at the moment UP-5 calls
//! `pfnAllocateCb` — and those two records come from `helios_protocol`, byte for
//! byte, exactly as `umd/src/forward/resource.rs:263-324` writes them. So the
//! table is `umd12`-local by D13's own rule, and the record it feeds is not.
//!
//! # ⭐ Why a table and not a field on `ResourceState`
//!
//! A field was the first design and it is the better one for *lifetime* — it
//! cannot leak and needs no bound. It was rejected for two reasons that are
//! properties of the D3D12 DDI rather than preferences:
//!
//! 1. **`ResourceState` is written exactly once and never mutated**, and that
//!    single-writer/single-taker property is the whole of
//!    `resource12::heap_state`'s re-derived soundness argument (D13 shares
//!    declarations, not `CUseCountedObject` claims). UP-5 must write back a
//!    `D3DKMT_HANDLE` that only exists *after* `pfnAllocateCb` returns, and UP-6
//!    must read it from `pfnCheckResourceAllocationHandle`; making that field
//!    mutable would put a lock inside the block whose soundness rests on nobody
//!    needing one.
//! 2. **The consumers do not all hold a `D3D12DDI_HRESOURCE`.** `pfnPresent`'s
//!    `_0110` shape was measured handing `hDstResource = NULL` on the WARP
//!    control arm (`KMD_IMPACT.md` §16 U3), and the present identity is built
//!    from an `ID3D12Resource*` the queue lane already has. A table keyed on the
//!    engine pointer answers for both callers; a field answers only for the one
//!    that came in through a handle.
//!
//! # ⛔ The key is an ADDRESS, and it is never dereferenced
//!
//! [`PresentableIdentity::engine_resource`] is `ID3D12Resource::as_raw() as
//! usize`. This module holds **no** COM reference and performs **no** load
//! through that value: it is an identity token compared with `==`, nothing more.
//! The owning reference lives in `ResourceState`, whose box outlives every entry
//! by construction — [`take`] is called from `pfnDestroyHeapAndResource` while
//! that box is still alive, before it is dropped.
//!
//! ⚠ **The hazard an address key has, and how it is closed.** COM object
//! addresses are recycled: the allocator is free to hand a later
//! `ID3D12Resource` the address a released one had. An entry that outlived its
//! resource would then be matched by a *different* resource and would describe
//! the wrong memory — a silent wrong answer, the class this project treats as
//! worse than a failure. Two things close it, and both are counted:
//!
//! * [`take`] on every `pfnDestroyHeapAndResource` whose resource block
//!   resolved, so the normal path leaves nothing behind. `IdentityRecorded`
//!   minus `IdentityRemoved` is the live-entry count and therefore a leak
//!   detector that needs no extra instrument.
//! * [`record`] **overwrites** a colliding key and reports
//!   [`RecordOutcome::Replaced`], which the caller counts as
//!   `IdentityReplaced`. A collision means a destroy failed to remove — so the
//!   stale entry is replaced by the live one (correct) *and* the failure is
//!   visible (loud), instead of the new resource inheriting the old memory.
//!
//! # ⛔ The second collision, and it is not the same one
//!
//! [`RecordOutcome::ResIdShared`] is a *refusal*, not a replacement, and it is what
//! keeps buffer rotation honest. Two live D3D12 resources sharing one
//! `venus_res_id` means the engine suballocated them out of one `VkDeviceMemory`;
//! every WDDM allocation would then name the same host resource and an N-buffered
//! swapchain would present one surface. That is the 56th session's *"scanout pinned
//! to ONE resource"* class by a new route, and the table is the only place in the
//! driver that can see all the live ids at once — so the check lives here and the
//! caller unwinds. `VKD3D_HEAP_FLAG_HELIOS_VENUS_EXPORT`'s dedicated allocation is
//! what makes the refusal unreachable; this is the assertion that it worked.
//!
//! # `ctx_id`, and why the field that *was* absent is now present
//!
//! ⚠ This block used to say `ctx_id` was *"deliberately absent"* because the only
//! value `umd12` could obtain was `HeliosVkd3dDevice::venus_context_id()`, which
//! reads the ICD's **process-global** `helios_current_ctx_id` — and
//! `bridge_icd_anchor.h` says of it: *"evidence only. Never stamp an identity with
//! this value"*, because it is last-writer-wins across instances. That reasoning
//! was right and its conclusion has been discharged rather than overturned: the
//! bridge now also captures `helios_venus_instance_ctx_id` at device create, on the
//! creating thread, and **that** is the value stored here. The forbidden one is
//! still not stamped; both are read so that a disagreement between them is a log
//! line rather than a silent wrong id.
//!
//! ⚠ A `ctx_id` of 0 is legal and counted. The KMD's adopt path never reads it —
//! `helios_protocol::classify` reaches `AdoptedUmdResource` from
//! `adopt_resource_id` alone — so it travels only into
//! `HeliosWddmOpenIdentity::ctx_id`, which that record's own doc calls *"diagnostic
//! only"*. Refusing a create over a diagnostic would be the wrong severity.

use std::sync::{Mutex, MutexGuard};

/// The resource geometry the create DDI supplies, verbatim.
///
/// ⚠ Every field is taken from `D3D12DDIARG_CREATERESOURCE_0109` as the runtime
/// gave it (`umd12/bindgen/cached/d3d12umddi.rs:87456-87473`), in the DDI's own
/// widths — not from the `D3D12_RESOURCE_DESC1` this lane *builds* for the
/// engine. The two agree today, and recording the input rather than the
/// translation means a future translation bug shows up as a disagreement instead
/// of being reproduced identically on both sides.
///
/// ⛔ **No row pitch, and that is a statement about THIS struct rather than
/// about the table.** The create DDI carries none — a D3D12 resource's layout is
/// the engine's, obtainable only through `GetCopyableFootprints` — so a pitch
/// here would be a second, unchecked derivation of a number the engine already
/// owns. UP-5 asks the engine instead, and UP-9 needs the answer for
/// `HeliosPresentPrivateData::pitch`, so it is kept as
/// [`PresentableIdentity::pitch`]: outside this struct precisely because it is
/// not one of the DDI's own fields.
#[derive(Clone, Copy)]
pub(crate) struct IdentityGeometry {
    /// `Width` — `UINT64` at the DDI, even for a texture.
    pub(crate) width: u64,
    pub(crate) height: u32,
    pub(crate) depth_or_array_size: u16,
    pub(crate) mip_levels: u16,
    /// `SampleDesc.Count`. A primary is single-sampled, so anything else here on
    /// a recorded entry is itself a finding.
    pub(crate) sample_count: u32,
    /// The creator's exact `DXGI_FORMAT`, which is what
    /// `HeliosWddmAllocMeta::dxgi_format` carries and what a cross-process
    /// opener must rebuild with — the lossy `D3DDDIFORMAT` collapses every
    /// non-BGRA surface to BGRA (`protocol/src/wddm.rs:254-263`).
    pub(crate) dxgi_format: u32,
}

/// One presentable resource's identity, as far as it is known.
///
/// `Copy`, and every field is a plain integer: this record holds no COM
/// reference, no pointer it may dereference and nothing with a destructor, which
/// is what makes a fixed array of `Option<Self>` a `const`-initialisable
/// `static` and what makes reading an entry out under the lock free of any
/// ownership question.
#[derive(Clone, Copy)]
pub(crate) struct PresentableIdentity {
    /// `ID3D12Resource::as_raw() as usize` — the table key. **Never
    /// dereferenced**; see the module doc.
    pub(crate) engine_resource: usize,
    /// The `VkDeviceMemory` the engine bound the resource's image or buffer to,
    /// as a 64-bit handle. **0 = unresolved** (see the module doc's table).
    pub(crate) vk_memory: u64,
    /// The resource's byte offset within `vk_memory`. ⚠ Non-zero means vkd3d
    /// suballocated, which UP-3 exists to prevent for a primary: D3D11's adopt
    /// path requires `memory_offset == 0`
    /// (`umd/src/forward/resource.rs:488-490`), because one venus resid covering
    /// several D3D12 resources breaks the one-resource-one-allocation rule.
    pub(crate) memory_offset: u64,
    /// The size of the whole `vk_memory` object — its
    /// `VkMemoryAllocateInfo::allocationSize`, *not* the resource's size. This
    /// is what `HeliosWddmAllocPrivate::size` and
    /// `HeliosWddmAllocMeta::venus_alloc_size` mean, and an importer that
    /// guesses it gets its OPAQUE-fd import rejected for exact-size mismatch.
    pub(crate) memory_size: u64,
    /// The venus resource id backing `vk_memory`, i.e. the value that becomes
    /// `HeliosWddmAllocPrivate::adopt_resource_id`. **0 = unresolved**, and 0 is
    /// also the value that makes the KMD *create* instead of *adopt*, so UP-5
    /// must refuse rather than pass a zero through.
    pub(crate) venus_res_id: u32,
    /// The creating `vkAllocateMemory`'s exact `allocationSize` as the ICD
    /// recorded it. Expected to equal `memory_size` once both halves resolve —
    /// two independent sources for one number, which is why both are kept: a
    /// disagreement is a finding, and a single field could not show one.
    pub(crate) venus_alloc_size: u64,
    /// The `memoryTypeIndex` of `vk_memory`, which a cross-process opener must
    /// import with (`protocol/src/wddm.rs:233-236`).
    pub(crate) memory_type_index: u32,
    /// The `D3DKMT_HANDLE` `pfnAllocateCb` minted for this resource (UP-5).
    ///
    /// ⭐ **This is the field the whole table exists to hold**, and it is what
    /// `pfnCheckResourceAllocationHandle` (UP-6) and `pfnPresent`'s
    /// `BroadcastSrcAllocation[0]` (UP-7) answer with. Never 0 on a recorded
    /// entry: [`record`] is called only after the callback succeeded, so an entry
    /// exists **iff** this driver owns a WDDM allocation for the resource — which
    /// is also what makes `pfnDeallocateCb` on the destroy path unconditional
    /// rather than conditional on a second flag.
    pub(crate) h_allocation: u32,
    /// The `hKMResource` the same callback returned, or 0.
    ///
    /// ⚠ Recorded but **not** used to deallocate. The wire contract is *either*
    /// `hResource` *or* `NumAllocations`+`HandleList`, never both
    /// (`umd/src/forward/state.rs:90-105`: both together is `E_INVALIDARG`, and
    /// that is a measured 0x80070057 rather than a reading of the header), and
    /// this driver deallocates by handle list. It is kept because it is the only
    /// evidence that dxgkrnl created a kernel *resource* object for the
    /// allocation, which a cross-process opener needs and a shared surface's
    /// absence of would explain.
    pub(crate) h_km_resource: u32,
    /// The runtime's `D3D12DDI_HRTRESOURCE::handle` for this resource, as an
    /// integer.
    ///
    /// ⛔ **An identity token, never dereferenced**, exactly like
    /// [`Self::engine_resource`]. It is kept for one reason: `pfnDeallocateCb`'s
    /// legal `hResource` form needs it at destroy time, and
    /// `pfnDestroyHeapAndResource` is handed only the *driver* handles. Storing it
    /// here rather than in `ResourceState` keeps that block write-once, and it lives
    /// exactly as long as the allocation it releases.
    pub(crate) h_rt_resource: usize,
    /// The venus context id stamped into `HeliosWddmAllocPrivate::ctx_id`.
    ///
    /// ⛔ The **instance-scoped** one (`helios_venus_instance_ctx_id`), never the
    /// process-global `helios_venus_current_ctx_id` — see
    /// `bridge12::BridgeDevice12::venus_instance_context_id`. 0 is legal and
    /// counted: the KMD's adopt path never reads it.
    pub(crate) ctx_id: u32,
    pub(crate) geometry: IdentityGeometry,
    /// The engine's row pitch for subresource 0, as `GetCopyableFootprints`
    /// answered it at create time — the value UP-5 put in
    /// `HeliosWddmAllocMeta::pitch`, kept so UP-9's
    /// `HeliosPresentPrivateData::pitch` is the **same** number rather than a
    /// second derivation.
    ///
    /// ⚠ **0 is legal and means the engine declined**, not "one byte per row":
    /// `check_subresource_info`'s `FOOTPRINT_UNANSWERED_U32` sentinel collapses to
    /// 0 here. Nothing on the windowed path reads it — DWM imports an OPTIMAL
    /// device-local image by venus resource id, not by stride — so it is carried
    /// rather than validated. ⛔ Whoever makes a *fullscreen* flip read it must
    /// treat a 0 as a refusal: the primary's stride is a frozen agreement with the
    /// host (`align(width*bpp, 256)`), and presenting with a disagreeing stride
    /// turns a hard failure into a sheared picture (`PENDING.md` §S-3 item 7).
    pub(crate) pitch: u32,
    /// The raw `D3D12DDI_HEAP_FLAGS` word the create arrived with.
    ///
    /// ⭐ This is §14a.3's `is_primary` field, kept in its unreduced form. The
    /// table's admission predicate *is* `HEAP_FLAG_PRIMARY`, so a `bool` here
    /// would be a field that is always `true`; the raw word instead says which
    /// other bits the primary carried, which is what UP-5 needs to map onto
    /// `HeliosWddmAllocMeta::bind_flags` / `misc_flags`.
    pub(crate) heap_flags: u32,
}

/// What [`record`] did, so the caller can count it.
///
/// `#[must_use]`: an ignored outcome is a dropped identity nobody counted, which
/// is the silent-failure shape CLAUDE.md rule 2 forbids.
#[must_use = "every outcome has a named counter; dropping it makes a full table silent"]
pub(crate) enum RecordOutcome {
    /// A free slot took the entry.
    Inserted,
    /// An entry for the same `engine_resource` already existed and was
    /// overwritten. ⛔ Means a destroy did not remove one.
    Replaced,
    /// The table is full and the identity was **dropped**.
    TableFull,
    /// ⛔⛔ **A DIFFERENT live resource already claims this `venus_res_id`**, so the
    /// engine suballocated two D3D12 resources out of one `VkDeviceMemory` and the
    /// identity was **refused**.
    ///
    /// ⭐ This is the rotation-collapse detector, and it is the reason the check
    /// lives in the table rather than at the call site: buffer rotation is *free*
    /// on this design — each `GetBuffer(i)` is a distinct `ID3D12Resource`, so N
    /// back buffers are N allocations — but only while N venus resource ids are N
    /// different numbers. If they shared one, every WDDM allocation would name the
    /// same host resource and the whole swapchain would present one surface: the
    /// 56th session's *"scanout pinned to ONE resource"* class, reached by a new
    /// route. `VKD3D_HEAP_FLAG_HELIOS_VENUS_EXPORT`'s dedicated allocation is what
    /// prevents it; this is the assertion that it worked, in the one place that can
    /// see all the live ids at once.
    ResIdShared {
        /// The engine resource that already holds the id.
        holder: usize,
    },
}

/// How many presentable identities the table holds.
///
/// ⛔ Bounded on purpose (CLAUDE.md rule 2: loud failure over silent
/// truncation). The number is derived, not picked: `DXGI_MAX_SWAP_CHAIN_BUFFERS`
/// is 16, so 64 is four fully-buffered swapchains live in one process at once.
/// At `size_of::<PresentableIdentity>()` ≈ 96 bytes that is ~6 KiB of process
/// lifetime data, which is not worth a heap allocation or a `HashMap` — and a
/// fixed array is what lets the table be a plain `static` with no lazy
/// initialisation on a free-threaded DDI path.
///
/// ⚠ It bounds **presentable** resources, not resources. The admission
/// predicate is the `PRIMARY` heap flag, so an application creating 50 000
/// textures records none of them; that is why 64 is a plausible bound at all,
/// and it is why `IdentityTableFull` reading non-zero would say the predicate is
/// wrong rather than that the bound is small.
const MAX_PRESENTABLE_IDENTITIES: usize = 64;

/// The table. One lock, held for the length of one array scan and nothing else.
///
/// ⚠ A `Mutex` and not a lock-free scheme: D3D12 DDIs are FREETHREADED
/// (`DDI_REFERENCE.md` §7.1), so a create on one thread and a destroy on another
/// are the expected case, and the critical section here is tens of integer
/// comparisons. `Mutex::new` is `const`, so this needs no `OnceLock` and no
/// initialisation ordering.
static IDENTITIES: Mutex<[Option<PresentableIdentity>; MAX_PRESENTABLE_IDENTITIES]> =
    Mutex::new([None; MAX_PRESENTABLE_IDENTITIES]);

/// Lock the table, ignoring poisoning.
///
/// ⚠ Poisoning cannot occur: both `umd12` profiles set `panic = "abort"`
/// (`umd12/Cargo.toml`), so no unwind can leave the guard poisoned in the first
/// place. `unwrap_or_else(PoisonError::into_inner)` rather than `unwrap()`
/// because a `panic!` in a DDI is a silent graphics deadlock (CLAUDE.md's
/// invariant table) and this crate must not contain a reachable one, even a
/// theoretically unreachable one.
fn identities() -> MutexGuard<'static, [Option<PresentableIdentity>; MAX_PRESENTABLE_IDENTITIES]> {
    IDENTITIES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record one presentable resource's identity.
///
/// Overwrites any entry with the same `engine_resource` — see the module doc's
/// address-recycling argument for why that is the correct direction and not a
/// convenience.
pub(crate) fn record(identity: PresentableIdentity) -> RecordOutcome {
    let mut table = identities();

    // ⛔ THE RES-ID SCAN COMES BEFORE ANY WRITE, and it is a refusal rather than a
    // replacement. A shared `venus_res_id` is not a stale entry to overwrite -- both
    // resources are live and both would name one host resource -- so the correct
    // action is to refuse the second identity and let the caller unwind its
    // allocation. ⚠ Only a non-zero id can collide: `record` is only reached with a
    // resolved identity, but the guard is written anyway because a zero id is the
    // one value that would otherwise match every unresolved entry a future caller
    // might add.
    if identity.venus_res_id != 0 {
        for existing in table.iter().flatten() {
            if existing.venus_res_id == identity.venus_res_id
                && existing.engine_resource != identity.engine_resource
            {
                return RecordOutcome::ResIdShared {
                    holder: existing.engine_resource,
                };
            }
        }
    }

    // ⛔ The address-collision scan comes next and completes before any insertion. A
    // single pass that inserted into the first free slot it met would, on a
    // colliding key held in a *later* slot, leave two entries for one address --
    // and then `remove` would clear one and `IdentityRemoved` would under-count
    // by exactly the number of collisions, i.e. the leak detector would be
    // broken by the very case it exists to catch.
    for slot in table.iter_mut() {
        if slot.is_some_and(|existing| existing.engine_resource == identity.engine_resource) {
            *slot = Some(identity);
            return RecordOutcome::Replaced;
        }
    }
    for slot in table.iter_mut() {
        if slot.is_none() {
            *slot = Some(identity);
            return RecordOutcome::Inserted;
        }
    }
    RecordOutcome::TableFull
}

/// The recorded identity for `engine_resource`, if there is one.
///
/// ⚠ Returns a **copy**, taken under the lock and never a reference into the
/// table: the entry may be removed by a concurrent `pfnDestroyHeapAndResource` on
/// another thread the instant the lock is dropped (D3D12 DDIs are FREETHREADED),
/// so a borrow would be a use-after-free waiting for a scheduler. Every consumer
/// wants two integers anyway.
///
/// ⛔ **The copy is a snapshot, and callers must not treat it as a liveness
/// claim.** `pfnCheckResourceAllocationHandle` and `pfnPresent` are both called by
/// the runtime for a resource it is holding, so the resource cannot be destroyed
/// under them — that is the runtime's own object-lifetime guarantee, not something
/// this table provides.
pub(crate) fn lookup(engine_resource: usize) -> Option<PresentableIdentity> {
    let table = identities();
    table
        .iter()
        .flatten()
        .find(|existing| existing.engine_resource == engine_resource)
        .copied()
}

/// Take the entry for `engine_resource`, if there is one.
///
/// Returns the removed record, which is how the caller distinguishes "this destroy
/// retired an identity" from the ordinary case of destroying a resource that was
/// never presentable — **and** how it learns which `D3DKMT_HANDLE` to hand
/// `pfnDeallocateCb`.
///
/// ⭐ **Take, not read-then-remove**, and the atomicity is the point: the slot is
/// cleared under the same lock acquisition that reads it, so two concurrent
/// destroys of one resource cannot both come away with the handle and deallocate it
/// twice. A `lookup` + `remove` pair would have that race, and a double
/// `pfnDeallocateCb` is a kernel-handle double free.
pub(crate) fn take(engine_resource: usize) -> Option<PresentableIdentity> {
    let mut table = identities();
    for slot in table.iter_mut() {
        if slot.is_some_and(|existing| existing.engine_resource == engine_resource) {
            return slot.take();
        }
    }
    None
}
