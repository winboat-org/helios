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
//! ⛔ **Inert at UP-4, deliberately.** Nothing consumes this table. UP-5 is the
//! commit that calls `pfnAllocateCb`, and `DECISIONS.md` §7.1's standing rule is
//! that a refusal stops refusing in the same commit that makes its body
//! reachable — so the allocation path is not written here, and the two integer
//! halves this table has no way to fill yet are filled with zero **and counted**
//! rather than guessed (see *What is unresolved*, below).
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
//! by construction — [`remove`] is called from `pfnDestroyHeapAndResource` while
//! that box is still alive, before it is dropped.
//!
//! ⚠ **The hazard an address key has, and how it is closed.** COM object
//! addresses are recycled: the allocator is free to hand a later
//! `ID3D12Resource` the address a released one had. An entry that outlived its
//! resource would then be matched by a *different* resource and would describe
//! the wrong memory — a silent wrong answer, the class this project treats as
//! worse than a failure. Two things close it, and both are counted:
//!
//! * [`remove`] on every `pfnDestroyHeapAndResource` whose resource block
//!   resolved, so the normal path leaves nothing behind. `IdentityRecorded`
//!   minus `IdentityRemoved` is the live-entry count and therefore a leak
//!   detector that needs no extra instrument.
//! * [`record`] **overwrites** a colliding key and reports
//!   [`RecordOutcome::Replaced`], which the caller counts as
//!   `IdentityReplaced`. A collision means a destroy failed to remove — so the
//!   stale entry is replaced by the live one (correct) *and* the failure is
//!   visible (loud), instead of the new resource inheriting the old memory.
//!
//! # What is unresolved at UP-4, and why it is zero rather than wrong
//!
//! | field group | needs | status |
//! |---|---|---|
//! | `vk_memory` / `memory_offset` / `memory_size` / `memory_type_index` | `ID3D12DXVKInteropDevice4::GetVulkanResourceMemoryInfo`, reached through a new `bridge12` entry point | **0** — the vkd3d method exists (UP-2) but no C++ bridge accessor calls it yet |
//! | `venus_res_id` / `venus_alloc_size` | the venus ICD's `helios_venus_memory_res_id` / `helios_venus_memory_alloc_info` exports, reached through the process ICD anchor | **0** — `umd12`'s bridge resolves the anchor for `venus_context_id()` alone (`umd_common/bridge/bridge_icd_anchor.h`); it exposes no memory-identity accessor |
//!
//! ⛔ Neither is stubbed silently: `record`'s caller bumps
//! `IdentityVkMemoryUnresolved` and `IdentityVenusUnresolved` for every entry
//! whose respective half is zero, so "the table is populated" and "the table is
//! *usefully* populated" are different, readable numbers. While either counter
//! equals `IdentityRecorded`, UP-5 cannot be reached at all — an
//! `adopt_resource_id` of 0 is precisely what makes the KMD treat an allocation
//! as *create*, not *adopt* (`protocol/src/wddm.rs:131-138`).
//!
//! ⚠ **`ctx_id` is deliberately absent**, and it is not an oversight to fix by
//! adding a field. `HeliosWddmAllocPrivate` needs one, and the only value
//! `umd12` can currently obtain is `HeliosVkd3dDevice::venus_context_id()`,
//! which reads the ICD's **process-global** `helios_current_ctx_id`.
//! `bridge_icd_anchor.h` states the rule outright: *"evidence only. Never stamp
//! an identity with this value"* — it is last-writer-wins across instances, so a
//! concurrent instance create can replace it. UP-5 has to obtain a real
//! `VkInstance`-scoped id (`helios_venus_instance_ctx_id`) before it may stamp
//! one, and putting the wrong one in this table now is how it would end up
//! stamped by accident.

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
/// ⛔ **No row pitch.** `HeliosWddmAllocMeta::pitch` needs one and the create
/// DDI carries none: a D3D12 resource's layout is the engine's, obtainable only
/// through `GetCopyableFootprints` (which this lane already calls, from
/// `check_subresource_info`). UP-5 is where that is resolved; recording a
/// computed guess here would be a second, unchecked derivation of a number the
/// engine already owns.
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
    pub(crate) geometry: IdentityGeometry,
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
}

/// How many presentable identities the table holds.
///
/// ⛔ Bounded on purpose (CLAUDE.md rule 2: loud failure over silent
/// truncation). The number is derived, not picked: `DXGI_MAX_SWAP_CHAIN_BUFFERS`
/// is 16, so 64 is four fully-buffered swapchains live in one process at once.
/// At `size_of::<PresentableIdentity>()` ≈ 80 bytes that is ~5 KiB of process
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

    // ⛔ The collision scan comes FIRST and completes before any insertion. A
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

/// Drop the entry for `engine_resource`, if there is one.
///
/// Returns whether an entry was removed, which is how the caller distinguishes
/// "this destroy retired an identity" from the ordinary case of destroying a
/// resource that was never presentable.
pub(crate) fn remove(engine_resource: usize) -> bool {
    let mut table = identities();
    for slot in table.iter_mut() {
        if slot.is_some_and(|existing| existing.engine_resource == engine_resource) {
            *slot = None;
            return true;
        }
    }
    false
}
