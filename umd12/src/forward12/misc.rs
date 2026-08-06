//! L9 — the tail: meta-commands, state objects, RT, work graphs, VRS, mesh, scheduling groups, multi-adapter, policy.
//!
//! Owns 28 of `DEVICE_FUNCS_CORE_0109` (groups (k) 4, (l) 3, (m) 6, (n) 13,
//! (o) 2) and 16 of `COMMAND_LIST_FUNCS_3D_0108` (markers/protection 4, meta 2,
//! RT 5, VRS 2, mesh 1, work graphs 2). 44 slots, the largest lane and the
//! cheapest.
//!
//! ⭐ **Mostly refuse-and-count**, which is why `PARALLEL.md` §4 calls it the
//! natural first task for a new agent: nearly every slot here is behind a cap
//! this driver reports as unsupported, so the honest body is a named refusal —
//! and a refusal with a counter is a finished slot, not a stub.
//!
//! ⚠ The exception to watch: `DDI_REFERENCE.md` §14.1.1 splits "may a slot be
//! NULL" into three different questions — retired, optional-feature, reserved —
//! and finds the **command-list table has no opt-out mechanism at all**. So the
//! command-list half of this lane is non-NULL-or-nothing regardless of caps.
//!
//! ⚠ **S6-0: this lane has not landed, with THREE exceptions.** Everything else
//! carries the per-slot counting noops `forward12::noop12` installed, so it is
//! non-NULL and every hit is named, counted and printed by
//! `D3D12 noop DDI hits:`. `PARALLEL.md` §9.2 does not call this lane done until
//! those counters read **zero** under a real workload.
//!
//! # ⭐ The three exceptions, and why they landed before the rest of the lane
//!
//! `pfnQueryNodeMap`, `pfnGetImplicitPhysicalAdapterMask` and
//! `pfnGetDebugAllocationInfo` are all **on the device-creation path**, and a
//! counting noop answers each of them wrong in a way the runtime acts on. The
//! first two landed with L1; the third with the S6 Round-1 merge, when
//! `D12-G7`'s slot ledger showed it had been called four times per device all
//! along:
//!
//! * `pfnQueryNodeMap` is called once inside `D3D12CreateDevice`
//!   (`tmp/dx12/gates/G7/RESULT.md` counted it). Its `pMap` is `_Out_writes_`,
//!   so a noop that returns without writing leaves the runtime reading **its own
//!   uninitialised buffer** as a node remapping — and `DDI_REFERENCE.md` §11.5h
//!   quotes **three** runtime strings that fail device creation on a bad one:
//!   *"Driver specified a non-identity node remapping with more than 1
//!   API-visible node"* (strings:104), *"...duplicate API index in node
//!   remapping"* (strings:105) and *"...invalid API index in node remapping"*
//!   (strings:107). ⚠ §11.5h calls them "the four node-remapping strings"
//!   because it counts the two adjacent cross-node **sharing tier** checks
//!   (strings:106, :108) into the same group; those constrain
//!   `CrossNodeSharingTier` in `caps12`, not this slot.
//! * `pfnGetImplicitPhysicalAdapterMask` is the **other half of the same
//!   sentence** in §11.5h: *"Helios has one node: write the identity map
//!   (`pMap[0] = 0`) and `pfnGetImplicitPhysicalAdapterMask` returns `1`."* A
//!   noop returns 0, i.e. *"this device has no physical adapters"*. Landing one
//!   half of a two-part invariant and leaving the other answering zero is the
//!   silent stub CLAUDE.md rule 2 exists to forbid, so both land together.
//! * `pfnGetDebugAllocationInfo` is the same output-the-runtime-acts-on class,
//!   arriving through a second slot. Its two array counts are **`_Inout_`**
//!   (`d3d12umddi.h:3541-3548`): the runtime writes each array's *capacity* in
//!   and expects a fill count back, so a `VOID`-returning noop that writes
//!   neither leaves the capacity standing as the count and the runtime reads
//!   that many entries out of an array this driver never filled. `D12-G7` called
//!   it **four times** per `D3D12CreateDevice`. See
//!   [`get_debug_allocation_info`], whose doc records that this was first
//!   written down as `_Out_` with the wrong mechanism.
//!
//! ⚠ They are still **L9's slots**, in L9's file, under L9's ownership
//! (`PARALLEL.md` §4). The lane that takes the rest of this file inherits them
//! and does not need to write them again.
//!
//! The `install` below is not scaffolding: it is a live link in the sequencer's
//! chain (`tables12`), and the chain does not compile without it.

use super::tables12::{stage, Filling};
use super::tables12::{CommandListTable, DeviceCoreTable};
use crate::{ddi12, log_error, note_refusal, UMD12_REFUSALS};
use helios_umd_common::refusals::RefusalCounter;

/// How many physical adapters this driver's node map describes.
///
/// ⭐ ONE, and it is asserted rather than inferred. The guest is single-GPU, the
/// KMD binds exactly one virtio-gpu PCI function, and every cross-node cap this
/// driver reports says so — `CrossNodeSharingTier = NOT_SUPPORTED` in
/// `caps12::d3d12_options`. ⛔ `ARCHITECTURE.md` §13 UNVERIFIED-11 is the
/// standing note that none of this is right on a multi-adapter guest; this
/// constant is where that assumption is written down instead of being spread
/// across two handlers, and `device12.rs`'s `create_device` cites the same item
/// for the LUID-matching half of it.
const HELIOS_PHYSICAL_ADAPTER_COUNT: ddi12::UINT = 1;

/// The implicit physical adapter mask: bit *i* set means physical adapter *i*
/// belongs to this device.
///
/// One adapter ⇒ bit 0 ⇒ `1`, which is literally the value `DDI_REFERENCE.md`
/// §11.5h names. ⛔ Never 0: that is what the counting noop answered, and it
/// says the device has no physical adapter at all.
const HELIOS_PHYSICAL_ADAPTER_MASK: ddi12::UINT = (1 << HELIOS_PHYSICAL_ADAPTER_COUNT) - 1;
const _: () = assert!(HELIOS_PHYSICAL_ADAPTER_MASK == 1);

/// How many times a bounded evidence line may repeat, per site.
const LOG_BUDGET: usize = 8;

/// `pfnGetImplicitPhysicalAdapterMask` — the mask of physical adapters behind
/// this device.
///
/// # Safety
/// `h_device` is not dereferenced: the answer is a property of the adapter, not
/// of any device state, and is the same for every device this driver creates.
/// Declared `unsafe` because the DDI's PFN typedef is.
unsafe extern "C" fn get_implicit_physical_adapter_mask(
    _h_device: ddi12::D3D12DDI_HDEVICE,
) -> ddi12::UINT {
    HELIOS_PHYSICAL_ADAPTER_MASK
}

/// `pfnQueryNodeMap` — the identity map, and nothing else is legal here.
///
/// `pMap[i] = i` for every entry the runtime asked for. ⛔ **Never
/// `D3D12DDI_NODE_MAP_HIDE_NODE` (`0xffffffff`)**: it is the "hide this node"
/// sentinel and hiding the only node this driver has would leave the device with
/// no API-visible node at all.
///
/// ⚠ The byte count comes from `num_physical_adapters`, which is the runtime's
/// own `_Out_writes_` count — the same rule as `pfnFillDDITable`'s `SIZE_T`
/// (`ARCHITECTURE.md` §12 rule 16 / R702). Writing **fewer** entries than it
/// asked for is not the safe direction here: those entries stay uninitialised
/// and the runtime reads them as node indices. So every requested entry is
/// written, and a count this driver did not expect is counted rather than
/// truncated.
///
/// # Safety
/// `p_map` must address `num_physical_adapters` writable `UINT`s the runtime
/// owns, as the DDI's `_Out_writes_(NumPhysicalAdapters)` declares.
unsafe extern "C" fn query_node_map(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    num_physical_adapters: ddi12::UINT,
    p_map: *mut ddi12::UINT,
) {
    if p_map.is_null() {
        note_refusal(&UMD12_REFUSALS.node_map_bad_arg);
        return;
    }
    if num_physical_adapters != HELIOS_PHYSICAL_ADAPTER_COUNT {
        // ⚠ `bump` and not `note_refusal`: R911 — this arm logs its own line.
        UMD12_REFUSALS.node_map_unexpected_adapter_count.bump();
        let n = UMD12_REFUSALS.node_map_unexpected_adapter_count.get();
        if n <= LOG_BUDGET {
            log_error!(
                "QueryNodeMap: runtime asked for {num_physical_adapters} physical adapters, this \
                 driver has {HELIOS_PHYSICAL_ADAPTER_COUNT} -- writing the identity map for all \
                 of them (x{n})"
            );
        }
    }
    for index in 0..num_physical_adapters {
        // SAFETY: the caller guarantees `num_physical_adapters` writable `UINT`s
        // behind `p_map`, and `index` is strictly below that count.
        unsafe { core::ptr::write_unaligned(p_map.add(index as usize), index) };
    }
}

/// `pfnGetDebugAllocationInfo` — this driver owns no kernel allocations, and
/// says so by **writing two zeros** rather than by returning without writing.
///
/// ⭐ **The third slot to land ahead of L9, for the same reason as the other two
/// and with a sharper edge.** `D12-G7`'s passing run called it **four times**
/// inside `D3D12CreateDevice`, and the counting noop it reached returns `0` —
/// which for a `VOID` slot means it returns having written **nothing**.
///
/// ⛔ **Both counts are `_Inout_`, and that is what makes the noop dangerous**:
///
/// ```text
/// _Inout_ UINT* pNumVirtualAddressInfos,
/// _Out_writes_to_opt_(*pNumVirtualAddressInfos, *pNumVirtualAddressInfos) ...* pVirtualAddressInfos,
/// ```
/// (`d3d12umddi.h:3541-3548`)
///
/// So the runtime writes each array's **capacity** in and expects the driver to
/// write back how many entries it actually filled. A body that returns without
/// touching them leaves the runtime's own capacity standing as the fill count,
/// and the runtime then reads `capacity` entries out of an array this driver
/// never wrote. Same family as the `query_node_map` above — an output the
/// runtime acts on that the noop never produced — arriving through a second slot
/// on the same device-creation path. ⚠ **Corrected after the fact:** this
/// comment first said `_Out_` and "the runtime reads its own uninitialised
/// stack". The shipped body was right either way, but the mechanism was not, and
/// it is the mechanism a future real body has to honour.
///
/// ⛔ **Zero is the honest answer, not a placeholder.** `DDI_REFERENCE.md` §9.12
/// says this slot *"must map any `D3D12DDI_HANDLE_AND_TYPE` to
/// `{ VA infos, KMT allocation infos }`"*, and **§9.7** (`:1735`) records that
/// kernel identity is *"mandatory in at least three places, so pure passthrough
/// with no `pfnAllocateCb` is not viable"*. Helios **is** that passthrough: the
/// venus ICD mints every allocation through its own D3DKMT, this driver calls no
/// `pfnAllocateCb`, and L4's `pfnCheckResourceAllocationHandle` answers `0` for
/// the same reason (`forward12/resource12.rs`, which cites §9.7 correctly). So
/// there is no `D3DKMT_HANDLE` to report and reporting a fabricated one would be
/// worse than reporting none. The counter is what stops the zero reading as
/// *"the debug layer looked and the resource was fine"*.
///
/// ⚠ **Deliberately does NOT reach `pfnSetErrorCb`.** A debug-layer query that
/// finds nothing is not a driver error, and `DDI_REFERENCE.md` §9.12's own
/// warning is that `pfnSetErrorCb` removes the device — *"Removing device due to
/// bad UMD error"*. This is the one Round-1 slot where the difference between
/// "counted" and "reported" is the difference between a diagnostic and a dead
/// compositor.
///
/// # Safety
/// `p_num_virtual_address_infos` and `p_num_kmt_infos`, when non-null, must each
/// address one `UINT` the runtime owns that is **readable and writable**: the
/// header declares them `_Inout_`, so each holds an array capacity on entry and
/// takes a fill count on exit. This body reads neither — it overwrites both with
/// `0` — but a future real body must read them before writing either array, and
/// that obligation belongs in this contract rather than in a comment. The two
/// array pointers are not dereferenced at all, because both counts are `0`.
unsafe extern "C" fn get_debug_allocation_info(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _object: ddi12::D3D12DDI_HANDLE_AND_TYPE,
    p_num_virtual_address_infos: *mut ddi12::UINT,
    _p_virtual_address_infos: *mut ddi12::D3D12DDI_DEBUG_VIRTUAL_ADDRESS_ALLOCATION_INFO_0012,
    p_num_kmt_infos: *mut ddi12::UINT,
    _p_kmt_infos: *mut ddi12::D3D12DDI_DEBUG_KMT_ALLOCATION_INFO_0014,
) {
    // ⚠ `bump` and not `note_refusal`: R911 -- the arm below logs its own line.
    DEBUG_ALLOCATION_INFO_EMPTY.bump();
    let n = DEBUG_ALLOCATION_INFO_EMPTY.get();
    if n <= LOG_BUDGET {
        log_error!(
            "GetDebugAllocationInfo: this driver owns no kernel allocations (the venus ICD mints \
             them); answering 0 VA infos and 0 KMT infos (x{n})"
        );
    }
    if !p_num_virtual_address_infos.is_null() {
        // SAFETY: non-null per the check; the DDI declares it a writable `_Out_`
        // `UINT*` the runtime owns for the duration of the call.
        unsafe { core::ptr::write_unaligned(p_num_virtual_address_infos, 0) };
    }
    if !p_num_kmt_infos.is_null() {
        // SAFETY: as above.
        unsafe { core::ptr::write_unaligned(p_num_kmt_infos, 0) };
    }
}

/// Install L9's 28 device-core slots.
///
/// Chain position: `PresentSlots` -> `MiscSlots` on the device-core table.
pub(crate) fn install_core(
    mut filling: Filling<'_, DeviceCoreTable, stage::PresentSlots>,
) -> Filling<'_, DeviceCoreTable, stage::MiscSlots> {
    let table = filling.table();
    // ⚠ 3 of this lane's 28. The other 25 keep their counting noops; see the
    // module doc for why these could not wait for the rest of the lane.
    table.pfnGetImplicitPhysicalAdapterMask = Some(get_implicit_physical_adapter_mask);
    table.pfnQueryNodeMap = Some(query_node_map);
    table.pfnGetDebugAllocationInfo = Some(get_debug_allocation_info);
    filling.advance()
}

/// Install L9's 16 command-list slots.
///
/// Chain position: `PresentSlots` -> `MiscSlots` on the command-list table.
pub(crate) fn install_cmdlist(
    mut filling: Filling<'_, CommandListTable, stage::PresentSlots>,
) -> Filling<'_, CommandListTable, stage::MiscSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfn... = Some(handler);`, each checked by the compiler
    // against the bindgen signature (`PARALLEL.md` §7).
    let _table = filling.table();
    filling.advance()
}

/// L9's refusal counters, printed by `crate::log_refusal_summary` at this
/// lane's position in `lib.rs`'s `UMD12_REFUSAL_SETS`.
///
/// ⭐ **Declared here rather than in `lib.rs` so this lane's diff against the
/// crate root is empty.** Every one of the eleven S6 lanes needs counters
/// (`PARALLEL.md` §9.1: *every skipped or refused path gets a named counter*),
/// and one flat array in `lib.rs` would have been the split's hottest merge
/// point — §5's shared-file table does not even list `lib.rs`. Same move
/// `forward12::tables12` makes for the 206 slots: name all eleven up front and
/// the lanes become substitutive instead of additive.
///
/// ⛔ **Append only.** Counter order inside a set, and set order in
/// `UMD12_REFUSAL_SETS`, are both the evidence contract: `D3D12 DDI refusals:`
/// lines get diffed across builds.
///
/// ⚠ Empty until this lane lands. That is a readable state and not a dead
/// one — the array is iterated on every summary, so the day L9
/// (the tail: meta-commands, state objects, VRS, mesh, work graphs) lands, its counters appear at
/// exactly this position.
///
/// ⚠ **Empty even though this file already has two live slots.**
/// `pfnQueryNodeMap` and `pfnGetImplicitPhysicalAdapterMask` landed with L1
/// because the caps sweep needs them, so their two counters
/// (`NodeMapBadArg`, `NodeMapUnexpectedAdapterCount`) are in the spine's set,
/// where they were declared and where the evidence contract has already
/// printed them. ⛔ They are not moved here: a counter that changes position
/// in `D3D12 DDI refusals:` breaks the diff that set order exists to protect.
/// The rest of L9's counters go here.
///
/// ⚠ So L9's counters are **split across two sets**, and that is the intended
/// behaviour of the scheme rather than a wart: position stability inside
/// `D3D12 DDI refusals:` outranks tidiness, because those lines get diffed across
/// builds and a counter that moves breaks the diff. The two node-map counters
/// stay where they were first printed; everything from here on is L9's own.
pub(crate) static REFUSALS: &[&RefusalCounter] = &[&DEBUG_ALLOCATION_INFO_EMPTY];

/// `pfnGetDebugAllocationInfo` answered "no VA infos, no KMT infos".
///
/// ⚠ **Expected non-zero — `D12-G7` measured four calls inside one
/// `D3D12CreateDevice`** — and it is an instrument, not a fault. Helios owns no
/// kernel allocations: the venus ICD mints every one through its own D3DKMT and
/// this driver never calls `pfnAllocateCb`, so there is no `D3DKMT_HANDLE` to
/// report (the same reason L4's `pfnCheckResourceAllocationHandle` answers 0).
/// ⛔ A **zero** reading on a run that created a device is the finding: it would
/// mean the slot stopped being reached, i.e. that the readout below is measuring
/// a different build than the one deployed.
static DEBUG_ALLOCATION_INFO_EMPTY: RefusalCounter =
    RefusalCounter::new("DebugAllocationInfoEmpty");
