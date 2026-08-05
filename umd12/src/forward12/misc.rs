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
//! ⚠ **S6-0: this lane has not landed, with TWO exceptions.** Everything else
//! carries the per-slot counting noops `forward12::noop12` installed, so it is
//! non-NULL and every hit is named, counted and printed by
//! `D3D12 noop DDI hits:`. `PARALLEL.md` §9.2 does not call this lane done until
//! those counters read **zero** under a real workload.
//!
//! # ⭐ The two exceptions, and why they landed before the rest of the lane
//!
//! `pfnQueryNodeMap` and `pfnGetImplicitPhysicalAdapterMask` are **on the
//! device-creation path**, and a counting noop answers both of them wrong in a
//! way the runtime acts on:
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

/// Install L9's 28 device-core slots.
///
/// Chain position: `PresentSlots` -> `MiscSlots` on the device-core table.
pub(crate) fn install_core(
    mut filling: Filling<'_, DeviceCoreTable, stage::PresentSlots>,
) -> Filling<'_, DeviceCoreTable, stage::MiscSlots> {
    let table = filling.table();
    // ⚠ 2 of this lane's 28. The other 26 keep their counting noops; see the
    // module doc for why these two could not wait for the rest of the lane.
    table.pfnGetImplicitPhysicalAdapterMask = Some(get_implicit_physical_adapter_mask);
    table.pfnQueryNodeMap = Some(query_node_map);
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

