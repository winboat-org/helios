//! L9 — the tail: meta-commands, state objects, RT, work graphs, VRS, mesh, scheduling groups, multi-adapter, policy.
//!
//! Owns 28 of `DEVICE_FUNCS_CORE_0109` (groups (k) 4, (l) 3, (m) 6, (n) 13,
//! (o) 2) and 16 of `COMMAND_LIST_FUNCS_3D_0108` (markers/protection 4, meta 2,
//! RT 5, VRS 2, mesh 1, work graphs 2). 44 slots, the largest lane and the
//! cheapest.
//!
//! ⚠ **The 44 is 28 + 16, and the 28 already nets out L8's slot** — re-derived
//! rather than inherited, because the near-miss is easy: `DDI_REFERENCE.md` §3.2
//! puts **five** slots in group (k), and `pfnGetPresentPrivateDriverDataSize` is
//! `present12.rs`'s. So L9's (k) share is 4, its device-core total is
//! `4 + 3 + 6 + 13 + 2 = 28`, and `PARALLEL.md` §4's device-core sum
//! (`… + 1 + 28 = 124`) counts L8's one separately. `install_core` below assigns
//! 28 fields; `install_cmdlist` assigns 16.
//!
//! ⭐ **Mostly refuse-and-count, and that is the finished state, not a stub.**
//! `PARALLEL.md` §4 calls this the natural first task for a new agent for
//! exactly that reason: nearly every slot here is behind a cap `caps12.rs`
//! reports as `NOT_SUPPORTED`, so the honest body is a named refusal. A refusal
//! whose counter says what reading is expected **is** a finished slot
//! (`PARALLEL.md` §9.1); a silent noop is not.
//!
//! ⚠ The exception to watch: `DDI_REFERENCE.md` §14.1.1 splits "may a slot be
//! NULL" into three different questions — retired, optional-feature, reserved —
//! and finds the **command-list table has no opt-out mechanism at all**. So the
//! command-list half of this lane is non-NULL-or-nothing regardless of caps.
//!
//! # ⭐ The rule this lane decides every slot by
//!
//! Exactly **one** slot here forwards into an engine that implements it
//! (`pfnWriteBufferImmediate`); every other slot in the lane refuses. The line
//! between them is not "does vkd3d have the method" — vkd3d has nearly all of
//! them — it is:
//!
//! > **Forward when the slot is self-contained and the engine implements it.
//! > Refuse when the slot is one member of a feature whose *other* members this
//! > driver does not implement.**
//!
//! Forwarding one member of a feature whose siblings refuse is the worse
//! failure: it produces a half-built feature whose behaviour depends on which
//! half the workload happened to touch, and no counter can describe that. A
//! whole feature declined at the cap, with every one of its slots counted, is a
//! state a reader can act on.
//!
//! Applied:
//!
//! * `pfnWriteBufferImmediate` — **forwarded**. It has no companion DDI slot, no
//!   PSO subobject and no tier that gates a *feature*; the only thing gating it
//!   is `WriteBufferImmediateQueueFlags`, whose own comment
//!   (`caps12.rs`, `d3d12_options`) USED to read `NONE` *"which is the honest answer
//!   while `pfnWriteBufferImmediate` is a noop"*. The cap follows the slot here,
//!   not the other way round — see [`write_buffer_immediate`] and the coherence
//!   note in this lane's REPORT.
//! * `pfnSetViewInstanceMask` — **refused**. vkd3d implements it, but view
//!   instancing's other half is the PSO's view-instancing subobject, which L6
//!   does not translate, and `caps12.rs:577` reports
//!   `ViewInstancingTier = NONE` on the strength of that. Forwarding a mask with
//!   no view-instanced pipeline to apply it to is inert either way, so the
//!   instrument is worth more than the call: see [`set_view_instance_mask`].
//! * `pfnSetMarker` — **refused, and it is not the PIX marker.** See
//!   [`set_marker`]; this is the one slot in the lane whose refusal rests on a
//!   *signature* argument rather than on a cap.
//! * `pfnSetBackgroundProcessingMode` — **refused, and the engine agrees**:
//!   vkd3d's own `d3d12_device_SetBackgroundProcessingMode` is a `FIXME(...
//!   stub!)` returning `E_NOTIMPL` (`libs/vkd3d/device.c:8941-8949`), so there is
//!   nothing behind the cap to forward to even if it were raised.
//! * Everything else — refused behind a cap, each counter naming the cap.
//!
//! # ⛔ What every refused `Create` in this file still does
//!
//! Two shapes that are not optional and whose absence is a heap bug rather than
//! a missing feature:
//!
//! 1. **Every `pfnCalcPrivate*Size` answers a non-zero size**, even though its
//!    paired create refuses. `queue.rs`'s `PRIVATE_SLOT_SIZE` records why: a `0`
//!    hands the paired `Create` a **zero-byte region** to write through, and
//!    `umd/src/device_funcs.rs:708-723` is where a transient 0 produced heap
//!    corruption surfacing as a wild call inside a 3DMark worker.
//! 2. **Every refusing `Create` nulls its handle slot first**, so the paired
//!    `Destroy` finds a defined null rather than the runtime allocator's
//!    leftovers. [`clear_refused_slot`] is that one line, once.
//!
//! # ⚠ There is deliberately no `report_error` in this lane
//!
//! `cmdlist.rs`, `pso.rs` and `descriptors.rs` each carry a `report_error` that
//! reaches `device12::set_error`. This file does not, and that is a decision
//! rather than an omission: **every** command-list slot here is either a
//! capability refusal — which `PARALLEL.md` §5 says is *counted, not reported* —
//! or a forward whose only failure mode is a malformed argument on a path the
//! caps already say is unreachable. `DDI_REFERENCE.md` §9.12: `pfnSetErrorCb`
//! **removes the device** (*"Removing device due to bad UMD error"*), so
//! escalating a feature this driver declared unsupported would take the
//! compositor down for a question the counter already answers. ⚠ The one arm
//! where that judgement is worth re-testing is named in
//! `L9WriteBufferImmediateBadArg`'s doc.
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
//! ⭐ That class — *a `VOID` slot with an `_Out_` the runtime acts on* — is the
//! one thing a refusal in this file may **never** skip, and it recurs seven more
//! times below: [`retrieve_shader_comment`], [`enumerate_meta_commands`],
//! [`enumerate_meta_command_parameters`],
//! [`get_meta_command_required_parameter_info`],
//! [`get_raytracing_acceleration_structure_prebuild_info`],
//! [`set_background_processing_mode`] and
//! [`get_work_graph_memory_requirements`]. Each writes its output before
//! refusing, and each says in its doc what the runtime would have done with the
//! uninitialised alternative.

use core::ffi::c_void;

use helios_umd_common::hr::{E_INVALIDARG, E_NOTIMPL, S_OK};
use helios_umd_common::refusals::RefusalCounter;
use helios_umd_common::slot::{Boxed, Slot};
use helios_umd_common::throttle::LogThrottle;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::{
    ID3D12GraphicsCommandList2, D3D12_WRITEBUFFERIMMEDIATE_MODE,
    D3D12_WRITEBUFFERIMMEDIATE_MODE_DEFAULT, D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_IN,
    D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT, D3D12_WRITEBUFFERIMMEDIATE_PARAMETER,
};

use super::queue;
use super::tables12::{stage, Filling};
use super::{identity12, resource12};
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

/// The private-block size every `CalcPrivate*` in this lane returns.
///
/// ⛔ **One machine word, and never 0** — the same constant, for the same
/// reason, as `queue::PRIVATE_SLOT_SIZE` and `fence::PRIVATE_SLOT_SIZE`. It is
/// restated here rather than imported because each lane's `CalcPrivate*` answers
/// for its own handles, and a cross-lane `pub(crate)` constant would make one
/// lane's decision silently govern another's.
///
/// ⚠ **Every `CalcPrivate*Size` in this file answers it even though its paired
/// `Create` refuses.** That is not generosity: a 0 makes the runtime hand the
/// paired `Create` a zero-byte region to write through, and
/// `umd/src/device_funcs.rs:708-723` records what a transient 0 cost on the
/// D3D11 side — heap corruption surfacing as a wild call inside a 3DMark worker.
/// The refusals in this file all *write* into that region (they null it, see
/// [`clear_refused_slot`]), so the region has to exist.
const PRIVATE_SLOT_SIZE: usize = core::mem::size_of::<*mut c_void>();

/// Sanity bound on `pfnWriteBufferImmediate`'s `Count`.
///
/// CLAUDE.md: *validate every runtime-supplied size before reading.* The DDI
/// declares both arrays `_In_reads_(Count)` and the runtime is the authority, so
/// this is not a semantic cap — no D3D12 rule limits how many immediates one
/// `WriteBufferImmediate` may carry. It bounds the allocation a corrupt count
/// would demand, and `L9WriteBufferImmediateBadArg` says if a real workload ever
/// approached it. Same shape and same number as
/// `queue::MAX_EXECUTE_COMMAND_LISTS`.
const MAX_WRITE_BUFFER_IMMEDIATE_PARAMS: usize = 65_536;

// ---------------------------------------------------------------------------
// Log budgets
// ---------------------------------------------------------------------------

// ⛔ **`log_error!` is unbounded by construction** (`umd_common/src/log.rs:279`)
// and T2 measured one unbounded UMD log site at ~9k mutex-serialized writes per
// second. Every site below that can repeat with the workload goes through one of
// these, and the ordinal is printed so a suppressed burst is still visible as a
// jump.
//
// ⚠ Grouped **per feature family** rather than per call site, following
// `queue.rs`'s six budgets: a burst is a property of the path that produces it
// (an app that draws with mesh shaders does it every frame), not of which of
// that path's lines fired.

/// Budget for the object-creation refusals: scheduling groups, meta-commands,
/// state objects.
static CREATE_LOG: LogThrottle = LogThrottle::new();
/// Budget for the raytracing / work-graph recording refusals.
static RT_LOG: LogThrottle = LogThrottle::new();
/// Budget for the fixed-function command-list refusals: markers, protection,
/// view instancing, VRS, mesh.
static CL_LOG: LogThrottle = LogThrottle::new();

/// Returns the occurrence ordinal (0-based) when the line should be emitted.
fn budget(t: &LogThrottle) -> Option<usize> {
    t.first_n_then_every(8, 4096)
}

// ---------------------------------------------------------------------------
// The one thing every refused Create still has to do
// ---------------------------------------------------------------------------

/// Null the handle slot of a `Create` that is about to refuse.
///
/// ⛔ **A refused create still owns its slot.** The runtime allocated the private
/// block this driver sized in the paired `CalcPrivate*Size`, and its contents are
/// whatever the runtime's allocator left there. The paired `Destroy` is called
/// regardless of what the create returned (`queue.rs`'s
/// `destroy_command_signature` counts exactly that case happening), so leaving
/// the word alone hands the destroy a garbage pointer to decide about.
///
/// ⚠ **`Boxed<()>` is a deliberate choice of the vacuous payload.** `clear` is
/// payload-agnostic — it is on `impl<P> Slot<P>` — so the tag only decides which
/// *other* operations exist on the value. `queue.rs` reaches this same operation
/// through `Slot::<Com<ID3D12GraphicsCommandList>>`, which additionally exposes
/// `load()`; a `Com<T>` tag on a slot that never held a `T` is precisely the
/// R803 shape (`ARCHITECTURE.md` §12 rule 7) where a payload chosen at the call
/// site produced a wild vtable. `Boxed<()>` exposes `get()`/`take()` over a
/// zero-sized type, which cannot name a callable function at all.
///
/// ⛔ And this file declares **no** `com_handles!`/`boxed_handles!` impl for
/// `D3D12DDI_HSCHEDULINGGROUP_0050`, `D3D12DDI_HMETACOMMAND_0052` or
/// `D3D12DDI_HSTATEOBJECT_0054`, which is why `p_drv_private` is passed raw. A
/// marker impl naming a payload would be a claim about an object this driver
/// never builds. Same call this lane's `queue.rs` neighbour makes for
/// `D3D12DDI_HCOMMANDSIGNATURE`.
///
/// # Safety
/// `p_drv_private`, when non-null, must address the machine word inside the
/// private block the paired `CalcPrivate*Size` sized — i.e. at least
/// [`PRIVATE_SLOT_SIZE`] writable bytes this driver owns.
unsafe fn clear_refused_slot(p_drv_private: *mut c_void, bad_slot: &RefusalCounter) {
    // SAFETY: the caller guarantees the slot lies in the sized private block.
    let Some(slot) = (unsafe { Slot::<Boxed<()>>::from_priv(p_drv_private) }) else {
        note_refusal(bad_slot);
        return;
    };
    // SAFETY: as above; `clear` writes one null word and touches nothing it
    // pointed at, which is correct because nothing was ever stored here.
    unsafe { slot.clear() };
}

// ---------------------------------------------------------------------------
// (k) Multi-adapter and misc — 4 of 5 (the 5th, pfnGetPresentPrivateDriverDataSize, is L8's)
// ---------------------------------------------------------------------------

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

/// `pfnRetrieveShaderComment` — **REFUSED**, `L9ShaderCommentRefused`.
///
/// PIX and the debug layer use this to pull a driver-authored disassembly
/// comment out of a pipeline state. Helios has none to give, for two independent
/// reasons and either alone would settle it:
///
/// * **There is no comment.** vkd3d compiles DXIL to SPIR-V and keeps no
///   D3D-shaped shader text; there is no string in this stack that answers the
///   question the slot asks.
/// * **This lane cannot read the handle.** `D3D12DDI_HPIPELINESTATE`'s payload is
///   declared in `pso.rs` and is **L6's** (`DECISIONS.md` D13: one declaration,
///   in the lane that owns it). L9 has no accessor budget into that file
///   (`PARALLEL.md` §4), so reaching the engine PSO from here would be the second
///   declaration.
///
/// ⭐ **The count is written to 0 before refusing**, and that is the
/// `_Out_`-the-runtime-acts-on class this module doc names. The slot's two-pass
/// idiom is *call with `pBuffer = NULL` to learn the character count including
/// the null terminator, then call again with a buffer that size* — so a caller
/// that ignores the HRESULT and reads the count finds its own uninitialised
/// stack. ⚠ **0 is chosen deliberately over 1.** A count of 1 would describe a
/// legal empty string (`L'\0'` alone) and invite a second call this driver would
/// refuse again; 0 characters *including* a null terminator describes no string
/// at all and cannot be mistaken for one.
///
/// # Safety
/// `character_count_including_null_terminator`, when non-null, must address one
/// writable `SIZE_T` the runtime owns. `p_buffer` is never dereferenced.
unsafe extern "C" fn retrieve_shader_comment(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _h_pipeline_state: ddi12::D3D12DDI_HPIPELINESTATE,
    _p_buffer: *mut ddi12::WCHAR,
    character_count_including_null_terminator: *mut ddi12::SIZE_T,
) -> ddi12::HRESULT {
    if character_count_including_null_terminator.is_null() {
        note_refusal(&L9_REFUSALS.shader_comment_bad_arg);
    } else {
        // SAFETY: non-null per the check; the DDI declares it a writable
        // `SIZE_T*` the runtime owns for the duration of the call.
        unsafe { core::ptr::write_unaligned(character_count_including_null_terminator, 0) };
    }
    note_refusal(&L9_REFUSALS.shader_comment_refused);
    E_NOTIMPL
}

/// `pfnGetDebugAllocationInfo` — the real `D3DKMT_HANDLE` for a presentable
/// resource since UP-6, and two honest zeros for everything else.
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
/// ⛔⛔ **The whole premise of this doc's first form is now FALSE and the old text is
/// quoted so the change is not silent.** It said: *"Zero is the honest answer, not a
/// placeholder … Helios **is** that passthrough: the venus ICD mints every allocation
/// through its own D3DKMT, this driver calls no `pfnAllocateCb`, and L4's
/// `pfnCheckResourceAllocationHandle` answers `0` for the same reason … there is no
/// `D3DKMT_HANDLE` to report"*. UP-5 landed `pfnAllocateCb` for resources the runtime
/// declares `D3D12DDI_HEAP_FLAG_PRIMARY`, so this driver owns a handle for exactly
/// that class — and `DDI_REFERENCE.md` §9.7's *"kernel identity is mandatory in at
/// least three places, so pure passthrough with no `pfnAllocateCb` is not viable"*
/// (`:1735`) is discharged for the class that needs it and left standing for the class
/// that does not.
///
/// ⇒ **Two answers, and the split is the same one `pfnCheckResourceAllocationHandle`
/// makes**: one KMT info out of the [`identity12`] table for a presentable resource,
/// and zero infos — still the honest answer, never a fabricated handle — for every
/// other resource, whose memory is the venus ICD's.
///
/// ⚠ **No VA infos in either case**, and that is a *different* argument rather than
/// the same one twice: `D3D12DDI_DEBUG_VIRTUAL_ADDRESS_ALLOCATION_INFO_0012` describes
/// a range in this adapter's GPU virtual address space, this driver reserves none
/// (`pfnReserveGpuVirtualAddressCb` is called nowhere in this crate), and the address
/// `pfnCheckResourceVirtualAddress` answers with is vkd3d's own allocator's. Reporting
/// it here would attribute a host-side range to this adapter's segments.
///
/// ⚠ **Both counts are `_Inout_` and BOTH are written on every path**, which is the
/// obligation the counting noop could not honour and the one a future edit is most
/// likely to break. [`fill_kmt_allocation_info`] reads the KMT capacity before it
/// touches the array and writes the fill count back first as 0, so no early return can
/// leave the runtime's own capacity standing as a count.
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
    object: ddi12::D3D12DDI_HANDLE_AND_TYPE,
    p_num_virtual_address_infos: *mut ddi12::UINT,
    _p_virtual_address_infos: *mut ddi12::D3D12DDI_DEBUG_VIRTUAL_ADDRESS_ALLOCATION_INFO_0012,
    p_num_kmt_infos: *mut ddi12::UINT,
    p_kmt_infos: *mut ddi12::D3D12DDI_DEBUG_KMT_ALLOCATION_INFO_0014,
) {
    // ⛔ NO VIRTUAL-ADDRESS INFOS, and 0 is the answer rather than a gap. The
    // struct is `{PhysicalAdapterIndex, StartAddress, EndAddress}` — a range in the
    // *adapter's* GPU virtual address space — and this driver reserves none:
    // `pfnReserveGpuVirtualAddressCb` is not called anywhere in this crate, and the
    // address `pfnCheckResourceVirtualAddress` answers with is
    // `ID3D12Resource::GetGPUVirtualAddress()`, i.e. **vkd3d's own** VA allocator's,
    // which the host owns. Reporting one here would attribute a host-side range to
    // this adapter's segments — a fabrication with an `S_OK` on it, which is the
    // shape `query_node_map` above is also about.
    if !p_num_virtual_address_infos.is_null() {
        // SAFETY: non-null per the check; the DDI declares it a writable `_Inout_`
        // `UINT*` the runtime owns for the duration of the call.
        unsafe { core::ptr::write_unaligned(p_num_virtual_address_infos, 0) };
    }

    // ── the KMT allocation info, since UP-5/UP-9 ────────────────────────────
    let filled = unsafe { fill_kmt_allocation_info(object, p_num_kmt_infos, p_kmt_infos) };
    if !filled {
        DEBUG_ALLOCATION_INFO_EMPTY.bump();
        let n = DEBUG_ALLOCATION_INFO_EMPTY.get();
        if n <= LOG_BUDGET {
            log_error!(
                "GetDebugAllocationInfo: no kernel allocation for handle {:p} type {} -- \
                 answering 0 VA infos and 0 KMT infos (x{n})",
                object.Handle,
                object.Type,
            );
        }
    }
}

/// Fill the one `D3D12DDI_DEBUG_KMT_ALLOCATION_INFO_0014` a presentable resource
/// has, if it has one. `true` when an entry was written.
///
/// ⛔ **Both counts are `_Inout_`, and this is the obligation the noop could not
/// honour**: the runtime writes each array's *capacity* in and expects the driver to
/// write back how many entries it filled. So the capacity is **read before** the
/// array is touched, and the write-back happens on every path — including the
/// zero-entry one, which is what stops the runtime reading `capacity` entries out of
/// an array this driver never wrote.
///
/// ⚠ **Only `HT_0012_RESOURCE` is answered**, and it is the only resource handle type
/// the header has (`d3d12umddi.rs`'s `D3D12DDI_HANDLETYPE` block: one resource
/// enumerator, value 34). Every other type — heaps included — takes the zero arm:
/// `pfnCreateHeapAndResource` mints an allocation for the *resource*, and a
/// `D3D12DDI_HHEAP` handle does not resolve through `resource12`.
///
/// # Safety
/// `object.Handle`, when `object.Type` is `HT_0012_RESOURCE`, must be the
/// `pDrvPrivate` of a resource handle `pfnCreateHeapAndResource` returned `S_OK` for.
/// `p_num_kmt_infos`, when non-null, must address one readable-and-writable `UINT`.
/// `p_kmt_infos` must address at least `*p_num_kmt_infos` writable entries.
unsafe fn fill_kmt_allocation_info(
    object: ddi12::D3D12DDI_HANDLE_AND_TYPE,
    p_num_kmt_infos: *mut ddi12::UINT,
    p_kmt_infos: *mut ddi12::D3D12DDI_DEBUG_KMT_ALLOCATION_INFO_0014,
) -> bool {
    if p_num_kmt_infos.is_null() {
        // Nowhere to report a count, so nothing may be written to the array either:
        // its length is only knowable through the count this driver cannot read.
        return false;
    }
    // SAFETY: non-null per the check; `_Inout_`, so it is readable on entry.
    let capacity = unsafe { core::ptr::read_unaligned(p_num_kmt_infos) };
    // ⛔ The count is written back FIRST, as 0, so every early return below leaves the
    // runtime's own capacity overwritten rather than standing as a fill count.
    // SAFETY: as above, and `_Inout_` makes it writable.
    unsafe { core::ptr::write_unaligned(p_num_kmt_infos, 0) };

    if object.Type != ddi12::D3D12DDI_HANDLETYPE_D3D12DDI_HT_0012_RESOURCE
        || object.Handle.is_null()
        || capacity == 0
        || p_kmt_infos.is_null()
    {
        return false;
    }
    let h_resource = ddi12::D3D12DDI_HRESOURCE {
        pDrvPrivate: object.Handle,
    };
    // SAFETY: the type tag says this is a resource handle and the caller guarantees
    // it is one this driver's create returned; the borrow ends inside this function.
    let Some(engine) = (unsafe { resource12::engine_resource(h_resource) }) else {
        return false;
    };
    let Some(identity) = identity12::lookup(engine.as_raw() as usize) else {
        // ⚠ The ordinary case, not a fault: only a resource the runtime declared a
        // PRIMARY has a WDDM allocation at all. `resource12`'s
        // `pfnCheckResourceAllocationHandle` answers 0 for the same class and for the
        // same reason.
        return false;
    };
    // SAFETY: `p_kmt_infos` is non-null with a capacity of at least 1 entry per the
    // checks above, so element 0 is inside the array the runtime supplied.
    // `write_unaligned` because the DDI promises a count and not an alignment.
    unsafe {
        p_kmt_infos.write_unaligned(ddi12::D3D12DDI_DEBUG_KMT_ALLOCATION_INFO_0014 {
            // Single-node adapter: `CreationNodeMask`/`VisibleNodeMask` are 1 across
            // this driver and there is no second physical adapter to index.
            PhysicalAdapterIndex: 0,
            hAllocation: identity.h_allocation,
            // ⚠ The resource's offset **within the allocation**, which is the venus
            // `VkDeviceMemory` the allocation adopted whole — so it is
            // `memory_offset`, and UP-3's dedicated export makes it 0. Carried rather
            // than hardcoded: `adopt_presentable` refuses a non-zero one, so a
            // non-zero value reaching here would be a finding, and writing a literal
            // 0 would hide it.
            Offset: identity.memory_offset,
            // The whole `VkMemoryAllocateInfo::allocationSize`, i.e. the allocation's
            // size — **not** the resource's. `identity12::PresentableIdentity::
            // memory_size` has why the two are kept apart.
            Size: identity.memory_size,
        });
    }
    // SAFETY: as the count write above.
    unsafe { core::ptr::write_unaligned(p_num_kmt_infos, 1) };
    DEBUG_ALLOCATION_INFO_ANSWERED.bump();
    true
}

// ---------------------------------------------------------------------------
// (l) Scheduling groups — 3 slots, REFUSED, and the refusal is load-bearing
// ---------------------------------------------------------------------------
//
// ⛔ **The KMD is what decides this, not taste.** `DDI_REFERENCE.md` §9.11:
// `DxgkDdiCreateHwQueue` returns `STATUS_NOT_SUPPORTED` and records `HwQRef`
// (`kmd_render/src/ddi/scheduler.rs:180-187`), refusing *at queue creation*
// specifically to avoid the "succeed at create, fail at submit" VidSch
// `0x119`/Arg1=2 bugcheck. `caps12.rs:846-863` therefore answers
// `HARDWARE_SCHEDULING_CAPS` with `ComputeQueuesPer3DQueue = 0`, which umddi:7007
// documents as *"0 means don't use scheduling groups"*, and §9.11 requires these
// three slots to *"refuse consistently -- at the first step, never
// succeed-then-fail"* (`DECISIONS.md` §7.7).
//
// ⚠ So a hit on `L9SchedulingGroupRefused` is not a missing feature: it means the
// runtime opted into scheduling groups against a cap that said 0, and the next
// thing after it would have been the bugcheck the KMD refuses to reach.

/// `pfnCalcPrivateSchedulingGroupSize`.
///
/// Answers the ordinary one-word size even though [`create_scheduling_group`]
/// refuses — see [`PRIVATE_SLOT_SIZE`].
///
/// # Safety
/// `arg`, when non-null, must point at a live
/// `D3D12DDIARG_CREATESCHEDULINGGROUP_0050` for the duration of the call.
unsafe extern "C" fn calc_private_scheduling_group_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATESCHEDULINGGROUP_0050,
) -> ddi12::SIZE_T {
    if arg.is_null() {
        note_refusal(&L9_REFUSALS.scheduling_group_calc_bad_arg);
    }
    PRIVATE_SLOT_SIZE as ddi12::SIZE_T
}

/// `pfnCreateSchedulingGroup` — **REFUSED**, `L9SchedulingGroupRefused`.
///
/// See the section comment above: the KMD's `DxgkDdiCreateHwQueue` answers
/// `STATUS_NOT_SUPPORTED`, `caps12.rs:860` reports
/// `ComputeQueuesPer3DQueue = 0`, and refusing here is what "at the first step,
/// never succeed-then-fail" means in code.
///
/// ⚠ `hRTSchedulingGroup` is dropped rather than stored. `DDI_REFERENCE.md` §7
/// (3) says an `hRT` handed to a create *"must be stored"* because it is the
/// token every callback about that object takes — but there is no object here to
/// call back about, and storing a token for an object that was never created is
/// the succeed-then-fail shape in miniature.
///
/// # Safety
/// `h_group`'s `pDrvPrivate`, when non-null, must address the private block
/// [`calc_private_scheduling_group_size`] sized.
unsafe extern "C" fn create_scheduling_group(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _arg: *const ddi12::D3D12DDIARG_CREATESCHEDULINGGROUP_0050,
    h_group: ddi12::D3D12DDI_HSCHEDULINGGROUP_0050,
    _h_rt_group: ddi12::D3D12DDI_HRTSCHEDULINGGROUP_0050,
) -> ddi12::HRESULT {
    // SAFETY: the caller guarantees the slot lies in the sized private block.
    unsafe { clear_refused_slot(h_group.pDrvPrivate, &L9_REFUSALS.scheduling_group_bad_slot) };
    note_refusal(&L9_REFUSALS.scheduling_group_refused);
    if let Some(n) = budget(&CREATE_LOG) {
        log_error!(
            "CreateSchedulingGroup: refused -- the KMD's DxgkDdiCreateHwQueue returns \
             STATUS_NOT_SUPPORTED and this driver reports ComputeQueuesPer3DQueue=0, so hardware \
             scheduling groups must refuse at the first step (x{})",
            n + 1,
        );
    }
    E_NOTIMPL
}

/// `pfnDestroySchedulingGroup`.
///
/// Nothing was ever stored, so this only counts. A hit means the runtime
/// destroyed a group after [`create_scheduling_group`] refused it, which is
/// legal and worth knowing.
///
/// # Safety
/// `_h_group` must be a handle the runtime associated with a
/// `pfnCreateSchedulingGroup` call on this device.
unsafe extern "C" fn destroy_scheduling_group(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _h_group: ddi12::D3D12DDI_HSCHEDULINGGROUP_0050,
) {
    note_refusal(&L9_REFUSALS.scheduling_group_destroy_unexpected);
}

// ---------------------------------------------------------------------------
// (m) Meta-commands — 6 slots
// ---------------------------------------------------------------------------
//
// ⭐ **This driver enumerates ZERO meta-commands, and that is the whole design.**
// A meta-command is a vendor-specific fused operation (DirectML's convolutions
// are the canonical ones) that the driver publishes by GUID; a driver with none
// to publish is not a broken driver, it is the common case. Helios forwards into
// vkd3d, which publishes none either.
//
// ⇒ [`enumerate_meta_commands`] answers 0 and `S_OK` (an ANSWER, not a refusal),
// and every other slot in this group refuses on the ground that no GUID it could
// be handed can name anything. ⚠ That makes the group internally consistent in
// the way `DECISIONS.md` §7.7 asks for: the enumeration is the contract, and the
// creates cannot contradict it.

/// `pfnEnumerateMetaCommands` — **answers zero**, and `S_OK`.
///
/// ⭐ Not a refusal: zero meta-commands is a complete, correct answer to the
/// question asked, and the counter exists so that "the runtime asked and got
/// nothing" is readable rather than invisible.
///
/// ⛔ **`pNumMetaCommands` is written before returning.** The API idiom
/// (`ID3D12Device5::EnumerateMetaCommands`) is *call with `pDescs = NULL` to
/// learn the count, then call again with an array that size* — and when `pDescs`
/// is non-null the count is `_Inout_`, carrying the array's capacity in. Either
/// way a body that returns `S_OK` without writing leaves the caller reading its
/// own uninitialised count, and then reading that many descriptors out of an
/// array this driver never filled. `pDescs` is deliberately never touched: with a
/// count of 0 there is no element to write, and writing into it would be writing
/// past a fill count of zero.
///
/// # Safety
/// `p_num_meta_commands`, when non-null, must address one writable `UINT` the
/// runtime owns. `p_descs` is never dereferenced.
unsafe extern "C" fn enumerate_meta_commands(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    p_num_meta_commands: *mut ddi12::UINT,
    _p_descs: *mut ddi12::D3D12DDIARG_META_COMMAND_DESC,
) -> ddi12::HRESULT {
    if p_num_meta_commands.is_null() {
        note_refusal(&L9_REFUSALS.meta_command_enumerate_bad_arg);
        return E_INVALIDARG;
    }
    // SAFETY: non-null per the check; the DDI declares it a writable count the
    // runtime owns for the duration of the call.
    unsafe { core::ptr::write_unaligned(p_num_meta_commands, 0) };
    note_refusal(&L9_REFUSALS.meta_command_enumerate_empty);
    S_OK
}

/// `pfnEnumerateMetaCommandParameters` — **REFUSED**,
/// `L9MetaCommandParametersRefused`.
///
/// [`enumerate_meta_commands`] published none, so every `CommandId` that can
/// reach this slot names a meta-command this driver does not have.
/// `E_INVALIDARG` is the specific answer for that — not `E_NOTIMPL`, which would
/// say the *slot* is unimplemented when what is actually true is that the
/// argument does not name anything.
///
/// The count is still written to 0 first, for [`enumerate_meta_commands`]'
/// reason: it is the caller's array capacity on the way in and its fill count on
/// the way out, and a caller that ignores the HRESULT must not find its own
/// capacity standing as a count.
///
/// # Safety
/// `p_parameter_count`, when non-null, must address one writable `UINT` the
/// runtime owns. `p_parameter_descs` is never dereferenced.
unsafe extern "C" fn enumerate_meta_command_parameters(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _command_id: ddi12::GUID,
    _stage: ddi12::D3D12DDI_META_COMMAND_PARAMETER_STAGE,
    p_parameter_count: *mut ddi12::UINT,
    _p_parameter_descs: *mut ddi12::D3D12DDIARG_META_COMMAND_PARAMETER_DESC,
) -> ddi12::HRESULT {
    if !p_parameter_count.is_null() {
        // SAFETY: non-null per the check; the DDI declares it a writable count
        // the runtime owns for the duration of the call.
        unsafe { core::ptr::write_unaligned(p_parameter_count, 0) };
    }
    note_refusal(&L9_REFUSALS.meta_command_parameters_refused);
    E_INVALIDARG
}

/// `pfnCalcPrivateMetaCommandSize`.
///
/// Answers the ordinary one-word size even though [`create_meta_command`]
/// refuses — see [`PRIVATE_SLOT_SIZE`].
///
/// ⚠ The one thing validated here is the creation-parameter blob's own
/// self-consistency: a non-zero byte count with a null pointer is never legal
/// (CLAUDE.md's per-arm validation rule), and it is counted even though this body
/// never reads the blob, because the paired create would.
///
/// # Safety
/// `p_creation_parameters`, when non-null, must address
/// `creation_parameters_data_size_in_bytes` readable bytes. Neither is
/// dereferenced here.
unsafe extern "C" fn calc_private_meta_command_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _command_id: ddi12::GUID,
    _node_mask: ddi12::UINT,
    p_creation_parameters: *const c_void,
    creation_parameters_data_size_in_bytes: ddi12::SIZE_T,
) -> ddi12::SIZE_T {
    if creation_parameters_data_size_in_bytes != 0 && p_creation_parameters.is_null() {
        note_refusal(&L9_REFUSALS.meta_command_calc_bad_arg);
    }
    PRIVATE_SLOT_SIZE as ddi12::SIZE_T
}

/// `pfnCreateMetaCommand` — **REFUSED**, `L9MetaCommandRefused`.
///
/// `E_INVALIDARG` rather than `E_NOTIMPL`, for [`enumerate_meta_command_parameters`]'
/// reason: this driver published no meta-commands, so the `CommandId` names
/// nothing.
///
/// # Safety
/// `h_meta_command`'s `pDrvPrivate`, when non-null, must address the private
/// block [`calc_private_meta_command_size`] sized.
unsafe extern "C" fn create_meta_command(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _command_id: ddi12::GUID,
    _node_mask: ddi12::UINT,
    _p_creation_parameters: *const c_void,
    _creation_parameters_data_size_in_bytes: ddi12::SIZE_T,
    h_meta_command: ddi12::D3D12DDI_HMETACOMMAND_0052,
    _h_rt_meta_command: ddi12::D3D12DDI_HRTMETACOMMAND_0052,
) -> ddi12::HRESULT {
    // SAFETY: the caller guarantees the slot lies in the sized private block.
    unsafe {
        clear_refused_slot(
            h_meta_command.pDrvPrivate,
            &L9_REFUSALS.meta_command_bad_slot,
        )
    };
    note_refusal(&L9_REFUSALS.meta_command_refused);
    if let Some(n) = budget(&CREATE_LOG) {
        log_error!(
            "CreateMetaCommand: refused -- EnumerateMetaCommands published 0 meta-commands, so no \
             CommandId can name one (x{})",
            n + 1,
        );
    }
    E_INVALIDARG
}

/// `pfnDestroyMetaCommand`.
///
/// Nothing was ever stored, so this only counts.
///
/// # Safety
/// `_h_meta_command` must be a handle the runtime associated with a
/// `pfnCreateMetaCommand` call on this device.
unsafe extern "C" fn destroy_meta_command(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _h_meta_command: ddi12::D3D12DDI_HMETACOMMAND_0052,
) {
    note_refusal(&L9_REFUSALS.meta_command_destroy_unexpected);
}

/// `pfnGetMetaCommandRequiredParameterInfo` — **writes a zero resource size**.
///
/// ⛔ A `VOID` slot with an `_Out_` the runtime acts on: the whole struct is one
/// `UINT64 ResourceSize`, and the runtime uses it to size the buffer it will hand
/// back at initialize/execute time. Returning without writing leaves the runtime
/// allocating a buffer of whatever was on its stack. 0 is the honest size for a
/// meta-command that does not exist.
///
/// ⚠ **This is the only slot in the group with no device handle**, so there is no
/// route to `pfnSetErrorCb` even if reporting were wanted — one more reason the
/// output has to be written rather than the call refused.
///
/// # Safety
/// `p_info`, when non-null, must address one writable
/// `D3D12DDIARG_META_COMMAND_REQUIRED_PARAMETER_INFO` the runtime owns.
unsafe extern "C" fn get_meta_command_required_parameter_info(
    _h_meta_command: ddi12::D3D12DDI_HMETACOMMAND_0052,
    _stage: ddi12::D3D12DDI_META_COMMAND_PARAMETER_STAGE,
    _parameter_index: ddi12::UINT,
    p_info: *mut ddi12::D3D12DDIARG_META_COMMAND_REQUIRED_PARAMETER_INFO,
) {
    if p_info.is_null() {
        note_refusal(&L9_REFUSALS.meta_command_required_parameter_bad_arg);
        return;
    }
    // SAFETY: non-null per the check; the DDI declares it a writable `_Out_`
    // struct the runtime owns for the duration of the call.
    unsafe {
        core::ptr::write_unaligned(
            p_info,
            ddi12::D3D12DDIARG_META_COMMAND_REQUIRED_PARAMETER_INFO { ResourceSize: 0 },
        );
    }
    note_refusal(&L9_REFUSALS.meta_command_required_parameter_empty);
}

// ---------------------------------------------------------------------------
// (n) State objects / raytracing / work graphs — 13 slots, REFUSED
// ---------------------------------------------------------------------------
//
// ⛔ **Two caps decide this whole group, and both read NOT_SUPPORTED:**
//
// * `caps12.rs:585` — `RaytracingTier: v::RAYTRACING_NONE`. `DDI_REFERENCE.md`
//   §9.9 already plans for it: *"State objects / DXR map to `ID3D12StateObject`
//   and are a large second tranche -- declinable at first via
//   `RaytracingTier = NOT_SUPPORTED`"*.
// * Work graphs' tier lives in `D3D12DDI_OPTIONS1_DATA_0103::WorkGraphsTier`,
//   which `caps12::get_caps` does not answer individually and therefore
//   **zero-fills** through its documented safe default (`caps12.rs:447-462`) —
//   and `D3D12DDI_WORK_GRAPHS_TIER_NOT_SUPPORTED` is 0. ⚠ That is a real answer
//   arrived at by a default rather than by a decision; it is coherent, and this
//   lane's REPORT flags it to L1 as worth stating explicitly.
//
// ⭐ **Where a refusal needs a VALUE, it is the engine's own vocabulary for "no
// such thing", read out of vkd3d rather than invented.** All four verified in
// `vkd3d-proton-helios/libs/vkd3d/raytracing_pipeline.c`:
// `GetShaderIdentifier` answers `NULL` (`:301`, `:325`), `GetShaderStackSize`
// answers `UINT32_MAX` (`:340`), `GetProgramIdentifier` answers a `memset`-zeroed
// struct (`:381`). Those are documented not-found answers the runtime already
// has to handle -- which is why returning them is safe where inventing a value
// would not be.

/// `pfnCalcPrivateStateObjectSize`.
///
/// Answers the ordinary one-word size even though [`create_state_object`]
/// refuses — see [`PRIVATE_SLOT_SIZE`].
///
/// # Safety
/// `arg`, when non-null, must point at a live
/// `D3D12DDIARG_CREATE_STATE_OBJECT_0054` for the duration of the call.
unsafe extern "C" fn calc_private_state_object_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_STATE_OBJECT_0054,
) -> ddi12::SIZE_T {
    if arg.is_null() {
        note_refusal(&L9_REFUSALS.state_object_calc_bad_arg);
    }
    PRIVATE_SLOT_SIZE as ddi12::SIZE_T
}

/// `pfnCreateStateObject` — **REFUSED**, `L9StateObjectRefused`.
///
/// `RaytracingTier = NOT_SUPPORTED` (`caps12.rs:585`) and work graphs' tier
/// zero-fills to `NOT_SUPPORTED`, so neither of the two state-object types this
/// DDI can carry is a feature this driver advertises.
///
/// ⚠ `E_NOTIMPL` here and not `E_INVALIDARG`: unlike the meta-command creates,
/// nothing about the *argument* is wrong — a well-formed DXR state object
/// description is being refused because the feature behind it is absent.
///
/// # Safety
/// `h_state_object`'s `pDrvPrivate`, when non-null, must address the private
/// block [`calc_private_state_object_size`] sized. `arg`, when non-null, must
/// point at a live `D3D12DDIARG_CREATE_STATE_OBJECT_0054`.
unsafe extern "C" fn create_state_object(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_STATE_OBJECT_0054,
    h_state_object: ddi12::D3D12DDI_HSTATEOBJECT_0054,
    _h_rt_state_object: ddi12::D3D12DDI_HRTSTATEOBJECT_0054,
) -> ddi12::HRESULT {
    // SAFETY: the caller guarantees the slot lies in the sized private block.
    unsafe {
        clear_refused_slot(
            h_state_object.pDrvPrivate,
            &L9_REFUSALS.state_object_bad_slot,
        )
    };
    note_refusal(&L9_REFUSALS.state_object_refused);
    if let Some(n) = budget(&CREATE_LOG) {
        // ⛔ Read the two scalars only after the null check, and never the
        // subobject array: `pSubobjects` is a tagged union this driver has no
        // translation for, and a log line is not a reason to walk one.
        let (kind, subobjects) = if arg.is_null() {
            (-1, 0)
        } else {
            // SAFETY: non-null per the check; the DDI declares it `_In_ CONST`.
            let a = unsafe { &*arg };
            (a.Type, a.NumSubobjects)
        };
        log_error!(
            "CreateStateObject: refused type={kind} subobjects={subobjects} -- RaytracingTier and \
             WorkGraphsTier both read NOT_SUPPORTED (x{})",
            n + 1,
        );
    }
    E_NOTIMPL
}

/// `pfnDestroyStateObject`.
///
/// Nothing was ever stored, so this only counts.
///
/// # Safety
/// `_h_state_object` must be a handle the runtime associated with a
/// `pfnCreateStateObject` or `pfnAddToStateObject` call on this device.
unsafe extern "C" fn destroy_state_object(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _h_state_object: ddi12::D3D12DDI_HSTATEOBJECT_0054,
) {
    note_refusal(&L9_REFUSALS.state_object_destroy_unexpected);
}

/// `pfnGetRaytracingAccelerationStructurePrebuildInfo` — **writes three zeros**.
///
/// ⛔ The `_Out_`-the-runtime-acts-on class again, and the most consequential
/// instance of it in this file: an application reads all three sizes and
/// allocates buffers from them. Returning without writing hands it three values
/// off its own stack to size GPU allocations with.
///
/// ⚠ **Zero is the refusal, and it is a legible one.** A result size of 0 cannot
/// build anything, so an application that ignores `RaytracingTier` and calls
/// anyway fails at its own allocation rather than at a garbage-sized one.
///
/// # Safety
/// `p_info`, when non-null, must address one writable
/// `D3D12DDI_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO_0054` the runtime
/// owns. `p_inputs` is never dereferenced.
unsafe extern "C" fn get_raytracing_acceleration_structure_prebuild_info(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _p_inputs: *const ddi12::D3D12DDI_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0054,
    p_info: *mut ddi12::D3D12DDI_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO_0054,
) {
    if p_info.is_null() {
        note_refusal(&L9_REFUSALS.rt_prebuild_info_bad_arg);
        return;
    }
    // SAFETY: non-null per the check; the DDI declares it a writable `_Out_`
    // struct the runtime owns for the duration of the call.
    unsafe {
        core::ptr::write_unaligned(
            p_info,
            ddi12::D3D12DDI_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO_0054 {
                ResultDataMaxSizeInBytes: 0,
                ScratchDataSizeInBytes: 0,
                UpdateScratchDataSizeInBytes: 0,
            },
        );
    }
    note_refusal(&L9_REFUSALS.rt_prebuild_info_empty);
}

/// `pfnCheckDriverMatchingIdentifier` — **`UNSUPPORTED_TYPE`**.
///
/// ⭐ The enumeration has a value that means exactly what this driver has to say,
/// and it is used rather than a nearby one:
/// `D3D12DDI_DRIVER_MATCHING_IDENTIFIER_UNSUPPORTED_TYPE` is *"the driver does
/// not support this serialized data type"*, and the only type the enum defines is
/// `D3D12DDI_SERIALIZED_DATA_RAYTRACING_ACCELERATION_STRUCTURE`
/// (`d3d12umddi.rs:70723`). With `RaytracingTier = NOT_SUPPORTED` that is
/// precisely true.
///
/// ⛔ **Never `COMPATIBLE_WITH_DEVICE`.** That is the value that tells an
/// application a serialized acceleration structure on disk may be deserialized
/// and used — a claim this driver cannot back, and one whose failure would be a
/// GPU fault rather than an error code.
///
/// # Safety
/// Neither pointer argument is dereferenced. Declared `unsafe` because the DDI's
/// PFN typedef is.
unsafe extern "C" fn check_driver_matching_identifier(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _data_type: ddi12::D3D12DDI_SERIALIZED_DATA_TYPE,
    _identifier: *const ddi12::D3D12DDI_SERIALIZED_DATA_DRIVER_MATCHING_IDENTIFIER_0054,
) -> ddi12::D3D12DDI_DRIVER_MATCHING_IDENTIFIER_STATUS {
    note_refusal(&L9_REFUSALS.driver_matching_identifier_refused);
    ddi12::D3D12DDI_DRIVER_MATCHING_IDENTIFIER_STATUS_D3D12DDI_DRIVER_MATCHING_IDENTIFIER_UNSUPPORTED_TYPE
}

/// `pfnGetShaderIdentifier` — **NULL**, which is the documented not-found answer.
///
/// No state object can exist ([`create_state_object`] refuses every one), so
/// there is no export table to look a name up in. ⭐ `NULL` is not a crash
/// hazard here: it is what the engine itself answers for an export it cannot
/// find (`vkd3d-proton-helios/libs/vkd3d/raytracing_pipeline.c:301` and `:325`),
/// so every caller of this DDI already has to handle it.
///
/// # Safety
/// `_p_export_name` is never dereferenced, and the returned pointer is null, so
/// no lifetime is implied. Declared `unsafe` because the DDI's PFN typedef is.
unsafe extern "C" fn get_shader_identifier(
    _h_state_object: ddi12::D3D12DDI_HSTATEOBJECT_0054,
    _p_export_name: ddi12::LPCWSTR,
) -> *mut c_void {
    note_refusal(&L9_REFUSALS.shader_identifier_absent);
    core::ptr::null_mut()
}

/// `pfnGetShaderStackSize` — **`UINT_MAX`**, the engine's own not-found value.
///
/// ⛔ **Not 0.** A stack size of 0 is a *legal* answer that says "this shader
/// needs no stack", and a caller would sum it into a pipeline stack size and
/// proceed. `UINT32_MAX` is what vkd3d returns when the export index does not
/// resolve (`libs/vkd3d/raytracing_pipeline.c:340`), and it is the value the
/// D3D12 contract reserves for "no such export".
///
/// # Safety
/// `_p_export_name` is never dereferenced. Declared `unsafe` because the DDI's
/// PFN typedef is.
unsafe extern "C" fn get_shader_stack_size(
    _h_state_object: ddi12::D3D12DDI_HSTATEOBJECT_0054,
    _p_export_name: ddi12::LPCWSTR,
) -> ddi12::UINT {
    note_refusal(&L9_REFUSALS.shader_stack_size_absent);
    ddi12::UINT::MAX
}

/// `pfnGetPipelineStackSize` — **0**.
///
/// ⚠ Unlike [`get_shader_stack_size`] there is no reserved not-found value here:
/// the API's `GetPipelineStackSize` has no failure encoding at all, and vkd3d
/// simply returns the object's stored size (`libs/vkd3d/raytracing_pipeline.c:359`).
/// With no state object in existence, 0 is the only number that is not a
/// fabrication, and the counter is what stops it reading as a measured answer.
///
/// # Safety
/// The handle is not dereferenced. Declared `unsafe` because the DDI's PFN
/// typedef is.
unsafe extern "C" fn get_pipeline_stack_size(
    _h_state_object: ddi12::D3D12DDI_HSTATEOBJECT_0054,
) -> ddi12::UINT {
    note_refusal(&L9_REFUSALS.pipeline_stack_size_absent);
    0
}

/// `pfnSetPipelineStackSize` — **dropped and counted**.
///
/// A `VOID` slot with no output and no device handle: there is nothing to write
/// and no channel to complain through. With no state object in existence there is
/// nothing to store the size on either.
///
/// # Safety
/// The handle is not dereferenced. Declared `unsafe` because the DDI's PFN
/// typedef is.
unsafe extern "C" fn set_pipeline_stack_size(
    _h_state_object: ddi12::D3D12DDI_HSTATEOBJECT_0054,
    _stack_size: ddi12::UINT,
) {
    note_refusal(&L9_REFUSALS.set_pipeline_stack_size_dropped);
}

/// `pfnCalcPrivateAddToStateObjectSize`.
///
/// Answers the ordinary one-word size even though [`add_to_state_object`]
/// refuses — see [`PRIVATE_SLOT_SIZE`].
///
/// # Safety
/// `arg`, when non-null, must point at a live
/// `D3D12DDIARG_ADD_TO_STATE_OBJECT_0072` for the duration of the call.
unsafe extern "C" fn calc_private_add_to_state_object_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_ADD_TO_STATE_OBJECT_0072,
) -> ddi12::SIZE_T {
    if arg.is_null() {
        note_refusal(&L9_REFUSALS.add_to_state_object_calc_bad_arg);
    }
    PRIVATE_SLOT_SIZE as ddi12::SIZE_T
}

/// `pfnAddToStateObject` — **REFUSED**, `L9AddToStateObjectRefused`.
///
/// Its `StateObjectToGrowFrom` can only ever be a handle
/// [`create_state_object`] refused, so this cannot be reached with anything to
/// grow. Refused on the same cap.
///
/// # Safety
/// `h_state_object`'s `pDrvPrivate`, when non-null, must address the private
/// block [`calc_private_add_to_state_object_size`] sized.
unsafe extern "C" fn add_to_state_object(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _arg: *const ddi12::D3D12DDIARG_ADD_TO_STATE_OBJECT_0072,
    h_state_object: ddi12::D3D12DDI_HSTATEOBJECT_0054,
    _h_rt_state_object: ddi12::D3D12DDI_HRTSTATEOBJECT_0054,
) -> ddi12::HRESULT {
    // SAFETY: the caller guarantees the slot lies in the sized private block.
    unsafe {
        clear_refused_slot(
            h_state_object.pDrvPrivate,
            &L9_REFUSALS.add_to_state_object_bad_slot,
        )
    };
    note_refusal(&L9_REFUSALS.add_to_state_object_refused);
    E_NOTIMPL
}

/// `pfnGetProgramIdentifier` — **a zeroed identifier**, which is the engine's own
/// answer.
///
/// Work graphs' tier reads `NOT_SUPPORTED`, so no program exists to identify.
/// ⭐ The value is not invented: vkd3d's own `GetProgramIdentifier` `memset`s the
/// out-struct to zero unconditionally
/// (`vkd3d-proton-helios/libs/vkd3d/raytracing_pipeline.c:376-383`), so a
/// zeroed `D3D12DDI_PROGRAM_IDENTIFIER_0108` is a shape the D3D12 runtime already
/// receives from a working engine.
///
/// ⚠ Returned **by value** (four `UINT64`s), so there is no out-pointer to
/// null-check and no way to return "nothing" — writing the struct is the only
/// available body, which is why this is an answer rather than a silent skip.
///
/// # Safety
/// Neither the handle nor `_p_program_name` is dereferenced. Declared `unsafe`
/// because the DDI's PFN typedef is.
unsafe extern "C" fn get_program_identifier(
    _h_state_object: ddi12::D3D12DDI_HSTATEOBJECT_0054,
    _p_program_name: ddi12::LPCWSTR,
) -> ddi12::D3D12DDI_PROGRAM_IDENTIFIER_0108 {
    note_refusal(&L9_REFUSALS.program_identifier_absent);
    ddi12::D3D12DDI_PROGRAM_IDENTIFIER_0108 { OpaqueData: [0; 4] }
}

/// `pfnGetWorkGraphMemoryRequirements` — **writes zeros**.
///
/// The `_Out_`-the-runtime-acts-on class once more: an application sizes its
/// backing-store allocation from `MinSizeInBytes`/`MaxSizeInBytes` and aligns it
/// to `SizeGranularityInBytes`. Returning without writing gives it three stack
/// values to allocate from, and a granularity of 0 would divide by zero in any
/// caller that rounds up.
///
/// # Safety
/// `p_requirements`, when non-null, must address one writable
/// `D3D12DDI_WORK_GRAPH_MEMORY_REQUIREMENTS_0108` the runtime owns.
/// `_p_program_name` is never dereferenced.
unsafe extern "C" fn get_work_graph_memory_requirements(
    _h_state_object: ddi12::D3D12DDI_HSTATEOBJECT_0054,
    _p_program_name: ddi12::LPCWSTR,
    p_requirements: *mut ddi12::D3D12DDI_WORK_GRAPH_MEMORY_REQUIREMENTS_0108,
) {
    if p_requirements.is_null() {
        note_refusal(&L9_REFUSALS.work_graph_memory_bad_arg);
        return;
    }
    // SAFETY: non-null per the check; the DDI declares it a writable `_Out_`
    // struct the runtime owns for the duration of the call.
    unsafe {
        core::ptr::write_unaligned(
            p_requirements,
            ddi12::D3D12DDI_WORK_GRAPH_MEMORY_REQUIREMENTS_0108 {
                MinSizeInBytes: 0,
                MaxSizeInBytes: 0,
                SizeGranularityInBytes: 0,
            },
        );
    }
    note_refusal(&L9_REFUSALS.work_graph_memory_absent);
}

// ---------------------------------------------------------------------------
// (o) Device policy — 2 slots
// ---------------------------------------------------------------------------

/// `pfnSetBackgroundProcessingMode` — **REFUSED, and the engine agrees.**
///
/// Two independent grounds, and this is the one slot in the lane where the
/// substrate settles it as firmly as the cap does:
///
/// * `caps12.rs:592` reports `BackgroundProcessingSupported = 0`;
/// * vkd3d's `d3d12_device_SetBackgroundProcessingMode` is a
///   `FIXME("... stub!")` returning `E_NOTIMPL`
///   (`vkd3d-proton-helios/libs/vkd3d/device.c:8941-8949`). ⇒ Raising the cap
///   without changing the engine would forward into a stub, so this refusal
///   cannot be closed by a caps edit alone.
///
/// ⛔ **`pbFurtherMeasurementsDesired` is written FALSE before returning, and
/// this is the highest-consequence output in the file.** It is the loop
/// condition: an application driving `SetBackgroundProcessingMode` in
/// measurement mode re-runs its workload *while the driver keeps asking for more
/// measurements*. A `VOID` slot that leaves it uninitialised leaves a non-zero
/// stack byte standing as TRUE, and the application loops forever. FALSE means
/// "I have measured all I am going to", which for a driver that measures nothing
/// is exactly right.
///
/// # Safety
/// `p_further_measurements_desired`, when non-null, must address one writable
/// `BOOL` the runtime owns.
unsafe extern "C" fn set_background_processing_mode(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    mode: ddi12::D3D12DDI_BACKGROUND_PROCESSING_MODE_0062,
    measurements_action: ddi12::D3D12DDI_MEASUREMENTS_ACTION_0062,
    p_further_measurements_desired: *mut ddi12::BOOL,
) {
    if !p_further_measurements_desired.is_null() {
        // SAFETY: non-null per the check; the DDI declares it a writable `_Out_`
        // `BOOL*` the runtime owns for the duration of the call.
        unsafe { core::ptr::write_unaligned(p_further_measurements_desired, 0) };
    }
    note_refusal(&L9_REFUSALS.background_processing_refused);
    if let Some(n) = budget(&CREATE_LOG) {
        log_error!(
            "SetBackgroundProcessingMode: refused mode={mode} action={measurements_action} -- \
             BackgroundProcessingSupported=0 and the engine's own entry point is a stub returning \
             E_NOTIMPL; answered FALSE for further measurements (x{})",
            n + 1,
        );
    }
}

/// `pfnImplicitShaderCacheControl` — **REFUSED**, `L9ImplicitShaderCacheRefused`.
///
/// ⭐ This is `DDI_REFERENCE.md` §14.1.1's worked example of an **OPTIONAL
/// FEATURE** slot — one of the three kinds of NULL it distinguishes — and its
/// prescription is followed exactly: *"counting stub, and report no
/// driver-managed cache"*. `caps12.rs:595` is the second half:
/// `DriverManagedShaderCachePresent = 0`.
///
/// §14.1.1 also records why it can be declined at all: `ShaderCache.md:219` says
/// the runtime calls it only for the `DRIVER_MANAGED` implicit-cache kind, and
/// *"this API will only be supported in developer mode"*.
///
/// ⚠ Helios does manage a shader cache — vkd3d's — but not one the D3D12 runtime
/// can enumerate, invalidate or clear through this DDI, and claiming otherwise
/// would make `CLEAR` a request this driver silently ignores.
///
/// # Safety
/// Nothing is dereferenced. Declared `unsafe` because the DDI's PFN typedef is.
unsafe extern "C" fn implicit_shader_cache_control(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _flags: ddi12::D3D12DDI_IMPLICIT_SHADER_CACHE_CONTROL_FLAGS_0080,
) {
    note_refusal(&L9_REFUSALS.implicit_shader_cache_refused);
}

// ---------------------------------------------------------------------------
// ⭐ Enum identity proofs for the one forwarded command-list slot
// ---------------------------------------------------------------------------
//
// `ARCHITECTURE.md` §12 rule 1 forbids taking a DDI-to-API enum correspondence on
// trust, and `DDI_REFERENCE.md` §9.6.1 is the standing evidence that a DDI enum
// and its API twin can collide on a value with *different meanings* while the
// member types keep the compiler silent. Each assertion below compares **two
// generated constants** — one out of `d3d12umddi.h` via bindgen, one out of the
// Win32 metadata via the `windows` crate. Neither side is transcribed.
//
// ⚠ Non-zero representatives are used where the enum has any: an enumeration
// that agreed only at 0 would pass a first-value check and be wrong everywhere
// else.

const _: () = assert!(
    ddi12::D3D12DDI_WRITEBUFFERIMMEDIATE_MODE_0032_D3D12DDI_WRITEBUFFERIMMEDIATE_MODE_MARKER_IN
        == D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_IN.0
        && ddi12::D3D12DDI_WRITEBUFFERIMMEDIATE_MODE_0032_D3D12DDI_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT
            == D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT.0,
    "D3D12DDI_WRITEBUFFERIMMEDIATE_MODE_0032 must be value-identical to \
     D3D12_WRITEBUFFERIMMEDIATE_MODE"
);

// ⛔ The layout proof the parameter translation below rests on. It is a *field*
// proof and not a `size_of` proof on purpose: the two structs could be the same
// size with the fields swapped, and `Dst`/`Dest` differ only in their last
// character.
const _: () = assert!(
    core::mem::offset_of!(ddi12::D3D12DDI_WRITEBUFFERIMMEDIATE_PARAMETER_0032, Dst)
        == core::mem::offset_of!(D3D12_WRITEBUFFERIMMEDIATE_PARAMETER, Dest)
        && core::mem::offset_of!(ddi12::D3D12DDI_WRITEBUFFERIMMEDIATE_PARAMETER_0032, Value)
            == core::mem::offset_of!(D3D12_WRITEBUFFERIMMEDIATE_PARAMETER, Value),
    "D3D12DDI_WRITEBUFFERIMMEDIATE_PARAMETER_0032 and D3D12_WRITEBUFFERIMMEDIATE_PARAMETER must \
     agree field-for-field"
);

// ---------------------------------------------------------------------------
// Command list: markers / protection / immediates / view instancing — 4 slots
// ---------------------------------------------------------------------------

/// `pfnSetMarker` — **dropped and counted**, and ⛔ **deliberately NOT forwarded
/// to `ID3D12GraphicsCommandList::SetMarker`.**
///
/// # ⭐ This is not the PIX marker, and the signature is the proof
///
/// The DDI takes `(D3D12DDI_HCOMMANDLIST, UINT64)`. The API's
/// `ID3D12GraphicsCommandList::SetMarker(Metadata, pData, Size)` takes a
/// *variable-length blob* — it cannot be lowered into one `UINT64`, so the two
/// are not the same operation and a forward would have to fabricate a
/// `(metadata, data, size)` triple out of a scalar.
///
/// The corpus says the same thing from the other side: `SPECS.md:161` records
/// `D3D12PIXMarkers.md` as a **proposal to add a new optional DDI table**,
/// `D3D12DDI_USER_DEFINED_ANNOTATION_FUNCS_0121`, precisely so that PIX
/// `BeginEvent`/`EndEvent`/`SetMarker` can reach the driver — at DDI revision
/// **121**, eleven above SDK 26100's ceiling of 0110, with **0 of its 13 named
/// PFN typedefs present in the header**. ⇒ If PIX markers still need a new table
/// at 0121, the slot that has existed since the first command-list table cannot
/// already be carrying them.
///
/// What it *is* is the WDDM command-stream marker — the same family as D3D11's
/// WDDM 1.3 `pfnSetMarker`, which the shipping D3D11 driver also answers with a
/// budgeted log line and nothing else (`umd/src/forward/tiles.rs:288-293`).
/// Helios has nowhere to put it: `DDI_REFERENCE.md` §8.2 records that there is
/// **no DMA buffer in the D3D12 DDI**, the engine records straight into Vulkan
/// command buffers, and the guest never sees the command stream a marker would
/// be written into.
///
/// # Safety
/// The handle is not dereferenced. Declared `unsafe` because the DDI's PFN
/// typedef is.
unsafe extern "C" fn set_marker(_h_list: ddi12::D3D12DDI_HCOMMANDLIST, _marker: ddi12::UINT64) {
    note_refusal(&L9_REFUSALS.marker_dropped);
}

/// `pfnSetProtectedResourceSession` — **counted, in two arms.**
///
/// A **null** handle is the ordinary "no protected session on this list" state
/// and carries no exposure; it gets its own counter so that the arm which does
/// carry one is not buried under it.
///
/// A **non-null** handle is a finding rather than a refusal, and ⛔ **a sharper
/// one than it first looks.** Two facts, stated precisely because a looser
/// version of the first one is wrong:
///
/// * `pfnCreateProtectedResourceSession` is **not** in the device-core table —
///   it lives in `D3D12DDI_DEVICE_FUNCS_CONTENT_PROTECTION_RESOURCES_0030`
///   (`d3d12umddi.rs:88660-88662`), a **separate optional table**. So the create
///   does exist in this DDI generation; what does not exist is a Helios handler
///   for it. `tables12::fill` serves exactly three table types with typed
///   handlers and gives every other one uniform counting stubs.
/// * `caps12::get_caps` does not answer
///   `D3D12DDICAPS_TYPE_0030_PROTECTED_RESOURCE_SESSION_SUPPORT`, so it zero-fills
///   to `NONE` through the documented safe default (`caps12.rs:447-462`), and the
///   runtime should therefore never ask for that table at all.
///
/// ⛔ **Which is exactly why this counter matters.** `unknown_slot_noop` returns
/// `0`, and `0` read as an `HRESULT` is `S_OK`. If the runtime ever *did* request
/// the content-protection table, the stub would answer "session created" to a
/// create whose paired `CalcPrivate*Size` also stubbed to 0 — a fake success over
/// a zero-byte private block. A non-null handle arriving here is the first place
/// that becomes visible from inside this driver.
///
/// # Safety
/// Neither handle is dereferenced; only the session handle's `pDrvPrivate` word
/// is compared against null. Declared `unsafe` because the DDI's PFN typedef is.
unsafe extern "C" fn set_protected_resource_session(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_session: ddi12::D3D12DDI_HPROTECTEDRESOURCESESSION_0030,
) {
    if h_session.pDrvPrivate.is_null() {
        note_refusal(&L9_REFUSALS.protected_resource_session_none);
        return;
    }
    note_refusal(&L9_REFUSALS.protected_resource_session_refused);
    if let Some(n) = budget(&CL_LOG) {
        log_error!(
            "SetProtectedResourceSession: a non-null session handle arrived, but this driver \
             fills no content-protection table and reports no protected-resource support -- so \
             the session was minted by a counting stub answering S_OK; dropping it (x{})",
            n + 1,
        );
    }
}

/// `pfnWriteBufferImmediate` -> `ID3D12GraphicsCommandList2::WriteBufferImmediate`.
///
/// # ⭐ The one slot in this lane that forwards, and why it is not the cap's twin
///
/// `caps12.rs` (`d3d12_options`) USED to report
/// `WriteBufferImmediateQueueFlags = NONE` with the comment *"NONE = 'no queue
/// supports WriteBufferImmediate', which is the honest answer while
/// `pfnWriteBufferImmediate` is a noop."* ⇒ **the cap follows this slot, not the
/// other way round.** It is not a feature tier gating a family of DDIs; it is a
/// statement about this one body, written when this body was a counting noop.
///
/// ⭐ **RESOLVED: implementing the body made that statement stale, and the cap was
/// raised to `3D|COMPUTE|COPY` in the same changeset (`3c29677`).** This lane's
/// REPORT routed it to L1 (`PARALLEL.md` §4) and L1 acted. ⛔ The line citation
/// that stood here was `caps12.rs:574-576` and had drifted +118 by the time
/// anyone read it — symbols, not lines, into a file being edited concurrently.
///
/// The forward is honest on the substrate side: vkd3d implements it fully —
/// `d3d12_command_list_WriteBufferImmediate`,
/// `vkd3d-proton-helios/libs/vkd3d/command.c:19168`, which resolves each `Dest`
/// GPU virtual address through its own VA map, batches the writes and honours all
/// three modes.
///
/// ⛔ **A noop here is silent data corruption, not a missing feature.** The whole
/// point of the call is that a *buffer* takes a value; an application (or DRED's
/// breadcrumb path, which is what the `MARKER_IN`/`MARKER_OUT` modes exist for)
/// then polls that buffer. A driver that drops the write leaves the old contents
/// standing and the poller waits forever. That asymmetry — a dropped write hangs,
/// an unnecessary forward costs nothing — is what decides this slot.
///
/// ⚠ **Each parameter is translated, not reinterpreted.** The two structs are
/// proven field-for-field identical above, and the two mode enumerations
/// value-identical, so a `cast` of the arrays would compile and work — but
/// `PARALLEL.md` §3 rule 8 forbids it, and the proof is what makes the
/// element-wise copy cheap rather than what makes the cast safe.
///
/// ⚠ **`pModes` is legitimately NULL**, meaning `DEFAULT` for every entry; that
/// is the API's own contract and vkd3d implements it (`command.c:19197`). This
/// body forwards `None` rather than synthesising an array of defaults, so the
/// engine's own default path is the one that runs.
///
/// ⚠ **The `cast` to `ID3D12GraphicsCommandList2` is a real `QueryInterface` per
/// call**, i.e. an AddRef/Release pair. `queue::CommandListState` stores the base
/// `ID3D12GraphicsCommandList` and widening it is L2's file, not this lane's
/// (`PARALLEL.md` §4), so the cast stays here. It is cheap relative to the call
/// and this slot is not on a per-draw path; if it ever becomes one, the fix is
/// one field in `CommandListState`.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`. `p_params`,
/// when `count` is non-zero, must address `count` readable
/// `D3D12DDI_WRITEBUFFERIMMEDIATE_PARAMETER_0032`s; `p_modes`, when non-null,
/// must address `count` readable `D3D12DDI_WRITEBUFFERIMMEDIATE_MODE_0032`s. Both
/// are what the DDI declares `_In_reads_(Count)`.
unsafe extern "C" fn write_buffer_immediate(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    count: ddi12::UINT,
    p_params: *const ddi12::D3D12DDI_WRITEBUFFERIMMEDIATE_PARAMETER_0032,
    p_modes: *const ddi12::D3D12DDI_WRITEBUFFERIMMEDIATE_MODE_0032,
) {
    // ⚠ The instrument the caps coherence needs: this slot should be unreachable
    // while `WriteBufferImmediateQueueFlags` reads NONE. Counted first, so that
    // it counts even the degenerate and refused arms below.
    L9_REFUSALS.write_buffer_immediate_calls.bump();

    let n = count as usize;
    if n == 0 {
        // Legal and degenerate: nothing to write, nothing to report.
        return;
    }
    // ⛔ Validate the runtime-supplied count and pointer BEFORE reading the
    // array. CLAUDE.md's rule; the bound is `MAX_WRITE_BUFFER_IMMEDIATE_PARAMS`.
    if p_params.is_null() || n > MAX_WRITE_BUFFER_IMMEDIATE_PARAMS {
        note_refusal(&L9_REFUSALS.write_buffer_immediate_bad_arg);
        if let Some(k) = budget(&CL_LOG) {
            log_error!(
                "WriteBufferImmediate: Count={count} pParams={p_params:p} -- refused (x{})",
                k + 1,
            );
        }
        return;
    }

    // SAFETY: the caller guarantees a live handle from `create_command_list`, and
    // the borrow does not outlive this DDI call -- which is the accessor's second
    // stated precondition. ⚠ It reaches `Slot::ptr()` and never
    // `Slot<Boxed<S>>::get()`, so `PARALLEL.md` §9.4's re-derivation obligation is
    // discharged where the handle is owned (`queue.rs:490-516`) rather than
    // inherited here.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L9_REFUSALS.command_list_missing);
        return;
    };
    // ⚠ `cast` is a real `QueryInterface`, so the result is an OWNED reference
    // released when it drops at the end of this function. The borrowed engine
    // list it came from is untouched.
    let Ok(list2) = state.engine().cast::<ID3D12GraphicsCommandList2>() else {
        note_refusal(&L9_REFUSALS.write_buffer_immediate_engine_missing);
        if let Some(k) = budget(&CL_LOG) {
            log_error!(
                "WriteBufferImmediate: engine does not expose ID3D12GraphicsCommandList2 -- {count} \
                 immediate writes dropped (x{})",
                k + 1,
            );
        }
        return;
    };

    let mut params: Vec<D3D12_WRITEBUFFERIMMEDIATE_PARAMETER> = Vec::with_capacity(n);
    for i in 0..n {
        // SAFETY: `p_params` is non-null and `i < n <= count`, so this element is
        // inside the array the DDI declares `_In_reads_(Count)`.
        let p = unsafe { core::ptr::read_unaligned(p_params.add(i)) };
        params.push(D3D12_WRITEBUFFERIMMEDIATE_PARAMETER {
            Dest: p.Dst,
            Value: p.Value,
        });
    }

    // ⛔ A mode outside the three the header defines is translated to nothing:
    // `PARALLEL.md` §3 rule 7 — never cast an enum blind. An unknown mode makes
    // the whole call refuse rather than half of it proceed, because a partial
    // batch is the shape that is impossible to attribute afterwards.
    let modes: Option<Vec<D3D12_WRITEBUFFERIMMEDIATE_MODE>> = if p_modes.is_null() {
        None
    } else {
        let mut out: Vec<D3D12_WRITEBUFFERIMMEDIATE_MODE> = Vec::with_capacity(n);
        for i in 0..n {
            // SAFETY: `p_modes` is non-null and `i < n <= count`, so this element
            // is inside the array the DDI declares `_In_reads_(Count)`.
            let m = unsafe { core::ptr::read_unaligned(p_modes.add(i)) };
            let translated = match m {
                ddi12::D3D12DDI_WRITEBUFFERIMMEDIATE_MODE_0032_D3D12DDI_WRITEBUFFERIMMEDIATE_MODE_DEFAULT => {
                    D3D12_WRITEBUFFERIMMEDIATE_MODE_DEFAULT
                }
                ddi12::D3D12DDI_WRITEBUFFERIMMEDIATE_MODE_0032_D3D12DDI_WRITEBUFFERIMMEDIATE_MODE_MARKER_IN => {
                    D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_IN
                }
                ddi12::D3D12DDI_WRITEBUFFERIMMEDIATE_MODE_0032_D3D12DDI_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT => {
                    D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT
                }
                other => {
                    note_refusal(&L9_REFUSALS.write_buffer_immediate_mode_unknown);
                    if let Some(k) = budget(&CL_LOG) {
                        log_error!(
                            "WriteBufferImmediate: mode {other} at index {i} is not one this \
                             header defines -- refusing all {count} writes (x{})",
                            k + 1,
                        );
                    }
                    return;
                }
            };
            out.push(translated);
        }
        Some(out)
    };

    // SAFETY: `list2` is an owned reference live for this call; `params` and
    // `modes` are local arrays of exactly `n` elements each, matching the `count`
    // argument, and neither outlives the call. `None` is the DDI's own "every
    // entry is DEFAULT", which the engine implements.
    unsafe {
        list2.WriteBufferImmediate(count, params.as_ptr(), modes.as_ref().map(|m| m.as_ptr()));
    }
}

/// `pfnSetViewInstanceMask` — **counted, in two arms, and deliberately NOT
/// forwarded.**
///
/// ⚠ **This one was decided against forwarding, and the reasoning is the lane's
/// rule (module doc) applied to a slot the engine does implement.** vkd3d's
/// `d3d12_command_list_SetViewInstanceMask`
/// (`vkd3d-proton-helios/libs/vkd3d/command.c:19127-19145`) is real: it masks to
/// the low four bits, stores the mask, and invalidates the pipeline when the
/// bound PSO declares `graphics.multiview.dynamic_mask`.
///
/// But view instancing's *other* half is the PSO's view-instancing subobject,
/// which L6 does not translate, and `caps12.rs:577` reports
/// `ViewInstancingTier = NONE` on the strength of that. ⇒ No PSO with
/// `multiview.dynamic_mask` can exist, so the stored mask is read by nothing and
/// the forward is provably inert. Given that, the counter is worth more than the
/// call: it says whether the runtime reaches this slot at all under a
/// `NOT_SUPPORTED` tier — which `DDI_REFERENCE.md` §14.1.1 explicitly leaves open
/// (*"a `NOT_SUPPORTED` tier does suppress its slots -- but the mechanism differs
/// per feature, so there is no general rule"*).
///
/// ⭐ **The two arms are the instrument.** With one view instance the only mask
/// consistent with the advertised tier is `1`; anything else is a workload
/// asserting view instancing this driver said it does not have. The day L1 raises
/// `ViewInstancingTier`, `L9ViewInstanceMaskRefused` already says whether
/// anything needed it, and the fix is a `cast` to `ID3D12GraphicsCommandList1`
/// and one call.
///
/// # Safety
/// The handle is not dereferenced. Declared `unsafe` because the DDI's PFN
/// typedef is.
unsafe extern "C" fn set_view_instance_mask(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    mask: ddi12::UINT,
) {
    /// The only mask a single-view-instance device can be given: view instance 0
    /// enabled, nothing else.
    const SINGLE_VIEW_INSTANCE_MASK: ddi12::UINT = 1;

    if mask == SINGLE_VIEW_INSTANCE_MASK {
        note_refusal(&L9_REFUSALS.view_instance_mask_identity_dropped);
        return;
    }
    note_refusal(&L9_REFUSALS.view_instance_mask_refused);
    if let Some(n) = budget(&CL_LOG) {
        log_error!(
            "SetViewInstanceMask: mask={mask:#x} but this driver reports ViewInstancingTier=NONE, \
             so only mask=1 is consistent -- dropped (x{})",
            n + 1,
        );
    }
}

// ---------------------------------------------------------------------------
// Command list: meta-commands — 2 slots
// ---------------------------------------------------------------------------

/// `pfnInitializeMetaCommand` — **counted.**
///
/// `pfnCreateMetaCommand` refuses every create, so the handle can only be one
/// that was never built. Nothing to initialize and nothing to write.
///
/// # Safety
/// No pointer is dereferenced. Declared `unsafe` because the DDI's PFN typedef
/// is.
unsafe extern "C" fn initialize_meta_command(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    _h_meta_command: ddi12::D3D12DDI_HMETACOMMAND_0052,
    _p_parameters: *const c_void,
    _parameters_size: ddi12::SIZE_T,
) {
    note_refusal(&L9_REFUSALS.meta_command_initialize_refused);
}

/// `pfnExecuteMetaCommand` — **counted.** Same ground as
/// [`initialize_meta_command`].
///
/// # Safety
/// No pointer is dereferenced. Declared `unsafe` because the DDI's PFN typedef
/// is.
unsafe extern "C" fn execute_meta_command(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    _h_meta_command: ddi12::D3D12DDI_HMETACOMMAND_0052,
    _p_parameters: *const c_void,
    _parameters_size: ddi12::SIZE_T,
) {
    note_refusal(&L9_REFUSALS.meta_command_execute_refused);
}

// ---------------------------------------------------------------------------
// Command list: raytracing — 5 slots, REFUSED on RaytracingTier = NONE
// ---------------------------------------------------------------------------

/// `pfnBuildRaytracingAccelerationStructure` — **counted.**
///
/// ⚠ Its destination is a GPU virtual address, not a CPU out-parameter, so there
/// is nothing to write: the acceleration structure simply never appears, and any
/// later trace against it reads whatever the app's own buffer held. That is why
/// this arm logs as well as counting — it is the first point at which a workload
/// that ignored `RaytracingTier` becomes visible.
///
/// # Safety
/// `_arg` is never dereferenced. Declared `unsafe` because the DDI's PFN typedef
/// is.
unsafe extern "C" fn build_raytracing_acceleration_structure(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    _arg: *const ddi12::D3D12DDIARG_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_0054,
) {
    note_refusal(&L9_REFUSALS.rt_build_refused);
    if let Some(n) = budget(&RT_LOG) {
        log_error!(
            "BuildRaytracingAccelerationStructure: refused -- this driver reports \
             RaytracingTier=NOT_SUPPORTED (x{})",
            n + 1,
        );
    }
}

/// `pfnEmitRaytracingAccelerationStructurePostbuildInfo` — **counted.**
///
/// Postbuild info is written to a GPU virtual address by the *build* this driver
/// never performed, so there is no CPU output to zero here either.
///
/// # Safety
/// `_arg` is never dereferenced. Declared `unsafe` because the DDI's PFN typedef
/// is.
unsafe extern "C" fn emit_raytracing_acceleration_structure_postbuild_info(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    _arg: *const ddi12::D3D12DDIARG_EMIT_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_0054,
) {
    note_refusal(&L9_REFUSALS.rt_postbuild_info_refused);
}

/// `pfnCopyRaytracingAccelerationStructure` — **counted.**
///
/// # Safety
/// `_arg` is never dereferenced. Declared `unsafe` because the DDI's PFN typedef
/// is.
unsafe extern "C" fn copy_raytracing_acceleration_structure(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    _arg: *const ddi12::D3D12DDIARG_COPY_RAYTRACING_ACCELERATION_STRUCTURE_0054,
) {
    note_refusal(&L9_REFUSALS.rt_copy_refused);
}

/// `pfnSetPipelineState1` — **counted.**
///
/// The DXR/work-graph counterpart of `pfnSetPipelineState`: it binds an
/// `ID3D12StateObject`. [`create_state_object`] refuses every one, so the handle
/// names nothing.
///
/// ⚠ **Do not confuse it with L3a's `pfnSetPipelineState`.** They are separate
/// slots with separate handle types; this one binds a state object and the other
/// binds a PSO, and only this one is L9's.
///
/// # Safety
/// The handle is not dereferenced. Declared `unsafe` because the DDI's PFN
/// typedef is.
unsafe extern "C" fn set_pipeline_state1(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    _h_state_object: ddi12::D3D12DDI_HSTATEOBJECT_0054,
) {
    note_refusal(&L9_REFUSALS.set_pipeline_state1_refused);
}

/// `pfnDispatchRays` — **counted, and logged.**
///
/// With no state object bound there is no shader table to dispatch against, and
/// the visible symptom is an empty render target rather than an error — which is
/// exactly the class the counters exist for.
///
/// # Safety
/// `_arg` is never dereferenced. Declared `unsafe` because the DDI's PFN typedef
/// is.
unsafe extern "C" fn dispatch_rays(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    _arg: *const ddi12::D3D12DDIARG_DISPATCH_RAYS_0054,
) {
    note_refusal(&L9_REFUSALS.dispatch_rays_refused);
    if let Some(n) = budget(&RT_LOG) {
        log_error!(
            "DispatchRays: refused -- this driver reports RaytracingTier=NOT_SUPPORTED, so no \
             state object was ever created to dispatch against (x{})",
            n + 1,
        );
    }
}

// ---------------------------------------------------------------------------
// Command list: VRS — 2 slots, REFUSED on VariableShadingRateTier = NONE
// ---------------------------------------------------------------------------

/// `pfnRSSetShadingRate` — **counted, in two arms.**
///
/// ⛔ **The two arms exist because this slot is in the runtime's own state-reset
/// block, so a single counter would be unreadable.** `DDI_REFERENCE.md` §14.0
/// measured that `pfnResetCommandList` is followed by a fixed 15-call state
/// reset *"whether or not the application touches that state"*, and
/// `pfnRSSetShadingRate` is one of the fifteen. So a per-frame, per-list hit rate
/// is the *expected* reading and says nothing; what says something is a rate
/// that is not the default state.
///
/// * `1X1` with all-`PASSTHROUGH` (or a null combiner array, which means the
///   same) is the default the reset block restores. It changes nothing, so
///   dropping it is exact rather than approximate: `L9ShadingRateDefaultDropped`.
/// * Anything else is a workload asserting variable-rate shading against
///   `caps12.rs:586`'s `VariableShadingRateTier = VRS_NONE`:
///   `L9ShadingRateRefused`, and it logs.
///
/// ⚠ **Not forwarded**, even though vkd3d implements it
/// (`vkd3d-proton-helios/libs/vkd3d/command.c:21056`), and unlike
/// `pfnWriteBufferImmediate` this is the lane rule's *refuse* side: VRS is a
/// feature with a second slot ([`rs_set_shading_rate_image`]), a per-primitive
/// cap, a tile-size cap and a combiner cap, none of which this driver backs.
/// Forwarding one of the two would make the rendered result depend on which half
/// a workload happened to use.
///
/// ⚠ The combiner array's length is `D3D12_RS_SET_SHADING_RATE_COMBINER_COUNT`,
/// taken from the generated header rather than written as `2`.
///
/// # Safety
/// `combiners`, when non-null, must address
/// `D3D12_RS_SET_SHADING_RATE_COMBINER_COUNT` readable
/// `D3D12DDI_SHADING_RATE_COMBINER_0062`s, as the API's fixed-size array
/// declares.
unsafe extern "C" fn rs_set_shading_rate(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    shading_rate: ddi12::D3D12DDI_SHADING_RATE_0062,
    combiners: *const ddi12::D3D12DDI_SHADING_RATE_COMBINER_0062,
) {
    // ⛔ **Which of the two conditions failed is tracked separately, because the
    // rate test SHORT-CIRCUITS the combiner scan and only one of them is ever
    // established.** On a coarse rate the loop below never runs and `combiners`
    // is never read, so a log line for that arm must not name combiners: the
    // single-argument call shape `RSSetShadingRate(rate, nullptr)` is legal and
    // ordinary — `pCombiners` is optional in the API and vkd3d reads a null
    // array as KEEP for both slots
    // (`vkd3d-proton-helios/libs/vkd3d/command.c:21074-21079`) — so a message
    // asserting "non-passthrough combiners" there sends a triage reader hunting
    // a combiner defect that cannot exist. The counters are unaffected: the
    // partition between the two arms is exactly what it was.
    let rate_is_default =
        shading_rate == ddi12::D3D12DDI_SHADING_RATE_0062_D3D12DDI_SHADING_RATE_0062_1X1;
    let mut combiner_not_passthrough = false;
    if rate_is_default && !combiners.is_null() {
        for i in 0..ddi12::D3D12_RS_SET_SHADING_RATE_COMBINER_COUNT as usize {
            // SAFETY: `combiners` is non-null and `i` is below the fixed array
            // length the API declares for this argument.
            let c = unsafe { core::ptr::read_unaligned(combiners.add(i)) };
            if c != ddi12::D3D12DDI_SHADING_RATE_COMBINER_0062_D3D12DDI_SHADING_RATE_COMBINER_0062_PASSTHROUGH
            {
                combiner_not_passthrough = true;
                break;
            }
        }
    }

    if rate_is_default && !combiner_not_passthrough {
        note_refusal(&L9_REFUSALS.shading_rate_default_dropped);
        return;
    }
    note_refusal(&L9_REFUSALS.shading_rate_refused);
    if let Some(n) = budget(&CL_LOG) {
        // Two literals rather than one line with a substituted reason, so each
        // arm states only what that arm actually looked at.
        if rate_is_default {
            log_error!(
                "RSSetShadingRate: refused -- rate is 1X1 but a combiner is not PASSTHROUGH, \
                 and this driver reports VariableShadingRateTier=NOT_SUPPORTED; dropped (x{})",
                n + 1,
            );
        } else {
            log_error!(
                "RSSetShadingRate: refused -- rate={shading_rate} is not the 1X1 default \
                 (combiners not inspected), and this driver reports \
                 VariableShadingRateTier=NOT_SUPPORTED; dropped (x{})",
                n + 1,
            );
        }
    }
}

/// `pfnRSSetShadingRateImage` — **counted, in two arms.**
///
/// A **null** resource is "unbind the shading-rate image", which is the state
/// this driver is permanently in; it gets its own counter so it cannot drown the
/// other arm. A **non-null** one requires VRS tier 2, and `caps12.rs:586` reports
/// tier `NONE` with `ShadingRateImageTileSize = 0` beside it (`:591`) — a tile
/// size of 0 is the value that *cannot contradict* a tier of NONE, and it is also
/// the value an application would divide by to size such an image.
///
/// # Safety
/// The resource handle is not dereferenced; only its `pDrvPrivate` word is
/// compared against null. Declared `unsafe` because the DDI's PFN typedef is.
unsafe extern "C" fn rs_set_shading_rate_image(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_image: ddi12::D3D12DDI_HRESOURCE,
) {
    if h_image.pDrvPrivate.is_null() {
        note_refusal(&L9_REFUSALS.shading_rate_image_none);
        return;
    }
    note_refusal(&L9_REFUSALS.shading_rate_image_refused);
    if let Some(n) = budget(&CL_LOG) {
        log_error!(
            "RSSetShadingRateImage: a shading-rate image was bound, but this driver reports \
             VariableShadingRateTier=NOT_SUPPORTED and ShadingRateImageTileSize=0 -- dropped (x{})",
            n + 1,
        );
    }
}

// ---------------------------------------------------------------------------
// Command list: mesh shaders — 1 slot, REFUSED on MeshShaderTier = NONE
// ---------------------------------------------------------------------------

/// `pfnDispatchMesh` — **counted, and logged loudly.**
///
/// `caps12.rs:593` reports `MeshShaderTier = MESH_NONE`, so no amplification or
/// mesh shader can have been created and no PSO can have been built from one:
/// there is nothing for this dispatch to run.
///
/// ⛔ **The symptom of dropping it is missing geometry with no error anywhere**,
/// which is why this arm logs rather than only counting. It is the same failure
/// shape CLAUDE.md rule 2 is written against — a skipped path that looks like a
/// working one.
///
/// ⚠ **Not forwarded**, even though vkd3d implements `DispatchMesh`
/// (`vkd3d-proton-helios/libs/vkd3d/command.c:21134`): mesh shaders are a feature
/// whose other members are L6's `pfnCreateMeshShader` /
/// `pfnCreateAmplificationShader` / `pfnCalcPrivateMeshShaderSize`, and this lane
/// does not own them. Dispatching against a pipeline that was never built from a
/// mesh shader is not a partial feature, it is an engine error.
///
/// # Safety
/// The handle is not dereferenced. Declared `unsafe` because the DDI's PFN
/// typedef is.
unsafe extern "C" fn dispatch_mesh(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    x: ddi12::UINT,
    y: ddi12::UINT,
    z: ddi12::UINT,
) {
    note_refusal(&L9_REFUSALS.dispatch_mesh_refused);
    if let Some(n) = budget(&CL_LOG) {
        log_error!(
            "DispatchMesh: {x}x{y}x{z} thread groups dropped -- this driver reports \
             MeshShaderTier=NOT_SUPPORTED, so no mesh pipeline exists; expect missing geometry \
             (x{})",
            n + 1,
        );
    }
}

// ---------------------------------------------------------------------------
// Command list: work graphs — 2 slots, REFUSED on WorkGraphsTier = NOT_SUPPORTED
// ---------------------------------------------------------------------------

/// `pfnSetProgram` — **counted.**
///
/// The `_0108` generalisation of pipeline binding: it selects a *program* out of
/// a state object (generic, raytracing, or work-graph). Every state object create
/// refuses, so the descriptor names nothing.
///
/// ⚠ The descriptor's `Type` is read for the log line only, and only after the
/// null check; its union arm is deliberately never touched.
///
/// # Safety
/// `p_desc`, when non-null, must point at a live `D3D12DDI_SET_PROGRAM_DESC_0108`
/// for the duration of the call. Only its first (non-union) field is read.
unsafe extern "C" fn set_program(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    p_desc: *const ddi12::D3D12DDI_SET_PROGRAM_DESC_0108,
) {
    note_refusal(&L9_REFUSALS.set_program_refused);
    if let Some(n) = budget(&RT_LOG) {
        let kind = if p_desc.is_null() {
            -1
        } else {
            // SAFETY: non-null per the check; the DDI declares it `_In_ CONST`,
            // and `Type` is the descriptor's first field, outside its union.
            unsafe { (*p_desc).Type }
        };
        log_error!(
            "SetProgram: refused program type={kind} -- no state object exists on this device \
             (x{})",
            n + 1,
        );
    }
}

/// `pfnDispatchGraph` — **counted.**
///
/// With [`set_program`] refusing every bind there is no work graph to dispatch,
/// and — like [`dispatch_mesh`] — the symptom of dropping it is silence.
///
/// # Safety
/// `p_desc`, when non-null, must point at a live
/// `D3D12DDI_DISPATCH_GRAPH_DESC_0108` for the duration of the call. Only its
/// first (non-union) field is read.
unsafe extern "C" fn dispatch_graph(
    _h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    p_desc: *const ddi12::D3D12DDI_DISPATCH_GRAPH_DESC_0108,
) {
    note_refusal(&L9_REFUSALS.dispatch_graph_refused);
    if let Some(n) = budget(&RT_LOG) {
        let mode = if p_desc.is_null() {
            -1
        } else {
            // SAFETY: non-null per the check; the DDI declares it `_In_ CONST`,
            // and `Mode` is the descriptor's first field, outside its union.
            unsafe { (*p_desc).Mode }
        };
        log_error!(
            "DispatchGraph: refused mode={mode} -- this driver reports \
             WorkGraphsTier=NOT_SUPPORTED (x{})",
            n + 1,
        );
    }
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Install L9's 28 device-core slots.
///
/// Chain position: `PresentSlots` -> `MiscSlots` on the device-core table.
///
/// ⚠ **28, and the four groups below sum to it: 4 + 3 + 6 + 13 + 2.** Group (k)
/// has *five* slots in `DDI_REFERENCE.md` §3.2 and only four are assigned here —
/// `pfnGetPresentPrivateDriverDataSize` is **L8's**, installed by
/// `present12::install_core` one link earlier in this chain, and `PARALLEL.md`
/// §4 counts it separately (`… + 1 + 28 = 124`).
pub(crate) fn install_core(
    mut filling: Filling<'_, DeviceCoreTable, stage::PresentSlots>,
) -> Filling<'_, DeviceCoreTable, stage::MiscSlots> {
    let table = filling.table();

    // (k) multi-adapter and misc — 4
    table.pfnGetImplicitPhysicalAdapterMask = Some(get_implicit_physical_adapter_mask);
    table.pfnQueryNodeMap = Some(query_node_map);
    table.pfnRetrieveShaderComment = Some(retrieve_shader_comment);
    table.pfnGetDebugAllocationInfo = Some(get_debug_allocation_info);

    // (l) scheduling groups — 3
    table.pfnCalcPrivateSchedulingGroupSize = Some(calc_private_scheduling_group_size);
    table.pfnCreateSchedulingGroup = Some(create_scheduling_group);
    table.pfnDestroySchedulingGroup = Some(destroy_scheduling_group);

    // (m) meta-commands — 6
    table.pfnEnumerateMetaCommands = Some(enumerate_meta_commands);
    table.pfnEnumerateMetaCommandParameters = Some(enumerate_meta_command_parameters);
    table.pfnCalcPrivateMetaCommandSize = Some(calc_private_meta_command_size);
    table.pfnCreateMetaCommand = Some(create_meta_command);
    table.pfnDestroyMetaCommand = Some(destroy_meta_command);
    table.pfnGetMetaCommandRequiredParameterInfo = Some(get_meta_command_required_parameter_info);

    // (n) state objects / raytracing / work graphs — 13
    table.pfnCalcPrivateStateObjectSize = Some(calc_private_state_object_size);
    table.pfnCreateStateObject = Some(create_state_object);
    table.pfnDestroyStateObject = Some(destroy_state_object);
    table.pfnGetRaytracingAccelerationStructurePrebuildInfo =
        Some(get_raytracing_acceleration_structure_prebuild_info);
    table.pfnCheckDriverMatchingIdentifier = Some(check_driver_matching_identifier);
    table.pfnGetShaderIdentifier = Some(get_shader_identifier);
    table.pfnGetShaderStackSize = Some(get_shader_stack_size);
    table.pfnGetPipelineStackSize = Some(get_pipeline_stack_size);
    table.pfnSetPipelineStackSize = Some(set_pipeline_stack_size);
    table.pfnCalcPrivateAddToStateObjectSize = Some(calc_private_add_to_state_object_size);
    table.pfnAddToStateObject = Some(add_to_state_object);
    table.pfnGetProgramIdentifier = Some(get_program_identifier);
    table.pfnGetWorkGraphMemoryRequirements = Some(get_work_graph_memory_requirements);

    // (o) device policy — 2
    table.pfnSetBackgroundProcessingMode = Some(set_background_processing_mode);
    table.pfnImplicitShaderCacheControl = Some(implicit_shader_cache_control);

    filling.advance()
}

/// Install L9's 16 command-list slots.
///
/// Chain position: `PresentSlots` -> `MiscSlots` on the command-list table.
pub(crate) fn install_cmdlist(
    mut filling: Filling<'_, CommandListTable, stage::PresentSlots>,
) -> Filling<'_, CommandListTable, stage::MiscSlots> {
    let table = filling.table();

    // markers / protection / immediates / view instancing — 4
    table.pfnSetMarker = Some(set_marker);
    table.pfnSetProtectedResourceSession = Some(set_protected_resource_session);
    table.pfnWriteBufferImmediate = Some(write_buffer_immediate);
    table.pfnSetViewInstanceMask = Some(set_view_instance_mask);

    // meta-commands — 2
    table.pfnInitializeMetaCommand = Some(initialize_meta_command);
    table.pfnExecuteMetaCommand = Some(execute_meta_command);

    // raytracing — 5
    table.pfnBuildRaytracingAccelerationStructure = Some(build_raytracing_acceleration_structure);
    table.pfnEmitRaytracingAccelerationStructurePostbuildInfo =
        Some(emit_raytracing_acceleration_structure_postbuild_info);
    table.pfnCopyRaytracingAccelerationStructure = Some(copy_raytracing_acceleration_structure);
    table.pfnSetPipelineState1 = Some(set_pipeline_state1);
    table.pfnDispatchRays = Some(dispatch_rays);

    // VRS — 2
    table.pfnRSSetShadingRate = Some(rs_set_shading_rate);
    table.pfnRSSetShadingRateImage = Some(rs_set_shading_rate_image);

    // mesh shaders — 1
    table.pfnDispatchMesh = Some(dispatch_mesh);

    // work graphs — 2
    table.pfnSetProgram = Some(set_program);
    table.pfnDispatchGraph = Some(dispatch_graph);

    filling.advance()
}

// ---------------------------------------------------------------------------
// Refusal counters
// ---------------------------------------------------------------------------

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
/// lines get diffed across builds. `DebugAllocationInfoEmpty` therefore stays
/// **first**, where the S6 Round-1 merge printed it; everything Round 2 adds is
/// appended after it and nothing is reordered.
///
/// ⚠ **L9's counters are split across two sets, and that is the scheme working
/// rather than a wart.** `pfnQueryNodeMap` and
/// `pfnGetImplicitPhysicalAdapterMask` landed with L1, so `NodeMapBadArg` and
/// `NodeMapUnexpectedAdapterCount` live in the spine's set where they were first
/// printed. ⛔ They are not moved here: a counter that changes position in
/// `D3D12 DDI refusals:` breaks the diff that set order exists to protect.
pub(crate) static REFUSALS: &[&RefusalCounter] = &[
    &DEBUG_ALLOCATION_INFO_EMPTY,
    // ⛔ APPENDED, UP-6's second half. See the counter's doc for why it is a pair
    // with the one above and unreadable alone.
    &DEBUG_ALLOCATION_INFO_ANSWERED,
    // -- device core -------------------------------------------------------
    &L9_REFUSALS.shader_comment_refused,
    &L9_REFUSALS.shader_comment_bad_arg,
    &L9_REFUSALS.scheduling_group_calc_bad_arg,
    &L9_REFUSALS.scheduling_group_refused,
    &L9_REFUSALS.scheduling_group_bad_slot,
    &L9_REFUSALS.scheduling_group_destroy_unexpected,
    &L9_REFUSALS.meta_command_enumerate_empty,
    &L9_REFUSALS.meta_command_enumerate_bad_arg,
    &L9_REFUSALS.meta_command_parameters_refused,
    &L9_REFUSALS.meta_command_calc_bad_arg,
    &L9_REFUSALS.meta_command_refused,
    &L9_REFUSALS.meta_command_bad_slot,
    &L9_REFUSALS.meta_command_destroy_unexpected,
    &L9_REFUSALS.meta_command_required_parameter_empty,
    &L9_REFUSALS.meta_command_required_parameter_bad_arg,
    &L9_REFUSALS.state_object_calc_bad_arg,
    &L9_REFUSALS.state_object_refused,
    &L9_REFUSALS.state_object_bad_slot,
    &L9_REFUSALS.state_object_destroy_unexpected,
    &L9_REFUSALS.rt_prebuild_info_empty,
    &L9_REFUSALS.rt_prebuild_info_bad_arg,
    &L9_REFUSALS.driver_matching_identifier_refused,
    &L9_REFUSALS.shader_identifier_absent,
    &L9_REFUSALS.shader_stack_size_absent,
    &L9_REFUSALS.pipeline_stack_size_absent,
    &L9_REFUSALS.set_pipeline_stack_size_dropped,
    &L9_REFUSALS.add_to_state_object_calc_bad_arg,
    &L9_REFUSALS.add_to_state_object_refused,
    &L9_REFUSALS.add_to_state_object_bad_slot,
    &L9_REFUSALS.program_identifier_absent,
    &L9_REFUSALS.work_graph_memory_absent,
    &L9_REFUSALS.work_graph_memory_bad_arg,
    &L9_REFUSALS.background_processing_refused,
    &L9_REFUSALS.implicit_shader_cache_refused,
    // -- command list ------------------------------------------------------
    &L9_REFUSALS.command_list_missing,
    &L9_REFUSALS.marker_dropped,
    &L9_REFUSALS.protected_resource_session_none,
    &L9_REFUSALS.protected_resource_session_refused,
    &L9_REFUSALS.write_buffer_immediate_calls,
    &L9_REFUSALS.write_buffer_immediate_bad_arg,
    &L9_REFUSALS.write_buffer_immediate_mode_unknown,
    &L9_REFUSALS.write_buffer_immediate_engine_missing,
    &L9_REFUSALS.view_instance_mask_identity_dropped,
    &L9_REFUSALS.view_instance_mask_refused,
    &L9_REFUSALS.meta_command_initialize_refused,
    &L9_REFUSALS.meta_command_execute_refused,
    &L9_REFUSALS.rt_build_refused,
    &L9_REFUSALS.rt_postbuild_info_refused,
    &L9_REFUSALS.rt_copy_refused,
    &L9_REFUSALS.set_pipeline_state1_refused,
    &L9_REFUSALS.dispatch_rays_refused,
    &L9_REFUSALS.shading_rate_default_dropped,
    &L9_REFUSALS.shading_rate_refused,
    &L9_REFUSALS.shading_rate_image_none,
    &L9_REFUSALS.shading_rate_image_refused,
    &L9_REFUSALS.dispatch_mesh_refused,
    &L9_REFUSALS.set_program_refused,
    &L9_REFUSALS.dispatch_graph_refused,
];

/// `pfnGetDebugAllocationInfo` answered "no VA infos, no KMT infos".
///
/// It is an instrument, not a fault: the queried object had no WDDM allocation of
/// this driver's, which is true of every resource the runtime did not declare a
/// primary — the same class, and the same reason, as L4's
/// `pfnCheckResourceAllocationHandle` answering 0.
///
/// ⚠⚠ **RE-GRADED by UP-6, and the widening is the whole change**: it used to mean
/// *"this driver owns no kernel allocations, so there is nothing to report"* — a
/// statement about the driver. It now means *"this particular object has no kernel
/// allocation"*, which is the ordinary answer for every resource that is not a
/// primary, plus the four argument-shaped refusals (no count pointer, zero capacity,
/// a non-resource handle type, an unresolved handle). ⇒ it can no longer be read as
/// *"the passthrough model is still in force"*; `DebugAllocationInfoAnswered` is the
/// counter that says whether a handle was ever produced.
///
/// ⚠ **Expected 0 on a retail device-creation path, and non-zero under the debug
/// layer.** ⛔ This was first graded the other way round — *"expected non-zero;
/// a zero reading is the finding"* — on the strength of `D12-G7`'s pre-fan-out
/// ledger, which counted **four** noop hits on this slot inside one
/// `D3D12CreateDevice`. The re-run with all 25 of those slots implemented
/// (`tmp/dx12/gates/G7-s6r1/`) reads **0**, on an otherwise byte-identical
/// device create: same `Flags=0x0`, same `CapsCalls=24`, same
/// `CapsMsaaCalls=2730`, same `venusCtx=19`.
///
/// ⚠ **Why it went from 4 to 0 is NOT established**, and it is written down as
/// unknown rather than guessed. Two candidates, neither confirmed:
/// the four calls were the runtime reacting to something the noop'd
/// device-creation path produced (`pfnCalcPrivatePipelineStateSize` answering 0
/// gave each PSO a **zero-byte** driver block, which is abnormal); or this slot
/// is debug-layer traffic that happened to be enabled on that run by something
/// other than `D3D12DDI_CREATE_DEVICE_FLAG_DEBUGGABLE`, which was clear in both.
/// ⛔ The one thing that would settle it is a run with the debug layer
/// deliberately on, and until then the honest grading is "0 here, non-zero
/// there" — not a claim about which mechanism produced the 4.
static DEBUG_ALLOCATION_INFO_EMPTY: RefusalCounter =
    RefusalCounter::new("DebugAllocationInfoEmpty");

/// ⭐ `pfnGetDebugAllocationInfo` **answered a real `D3DKMT_HANDLE`** for a
/// presentable resource (UP-6's second half).
///
/// ⚠ **Expected 0 in retail and non-zero only under the debug layer**, like the
/// counter above — this slot is a debug-layer query and nothing on the shipping path
/// calls it. ⛔ Its value is as a *pair* with `DebugAllocationInfoEmpty`: the two
/// together partition every entry, so `Empty > 0` with `Answered == 0` on a run with
/// `IdentityRecorded > 0` says the debug layer asked about resources that are not
/// primaries (benign), while `Answered == 0` with `Empty == 0` says the slot was never
/// entered at all. Neither reading is available from one counter.
static DEBUG_ALLOCATION_INFO_ANSWERED: RefusalCounter =
    RefusalCounter::new("DebugAllocationInfoAnswered");

/// L9's Round-2 refusal counters. One instance, [`L9_REFUSALS`]; the set that
/// prints them is [`REFUSALS`].
///
/// ⚠ Every doc below states what reading is **expected** and what reading is a
/// **finding**. `PARALLEL.md` §9.1: a counter that can never move is not an
/// instrument, and a counter whose expected reading is not written down is one
/// nobody can act on.
pub(crate) struct L9Refusals {
    // -- device core -------------------------------------------------------
    /// `pfnRetrieveShaderComment` refused. **Expected 0 in retail**, non-zero
    /// under PIX or the debug layer. There is no D3D-shaped shader text anywhere
    /// in this stack to answer with, so a hit is information about the *tool*,
    /// not about the driver.
    shader_comment_refused: RefusalCounter,
    /// `pfnRetrieveShaderComment` with a null character count. **Expected 0** —
    /// the DDI's two-pass idiom has no arm in which the count pointer is
    /// optional, so a hit is a runtime the contract capture does not describe.
    shader_comment_bad_arg: RefusalCounter,
    /// `pfnCalcPrivateSchedulingGroupSize` with a null arg. **Expected 0.**
    scheduling_group_calc_bad_arg: RefusalCounter,
    /// `pfnCreateSchedulingGroup` refused.
    ///
    /// ⛔ **Expected 0, and a hit is a real finding.** `caps12.rs:860` reports
    /// `ComputeQueuesPer3DQueue = 0`, which umddi:7007 documents as *"don't use
    /// scheduling groups"*, so the runtime should never reach this slot. A hit
    /// means it opted in anyway — and the KMD's `DxgkDdiCreateHwQueue` refusing
    /// (`kmd_render/src/ddi/scheduler.rs:180-187`) is the only thing then
    /// standing between the process and the VidSch `0x119`/Arg1=2 bugcheck that
    /// refusal exists to avoid.
    scheduling_group_refused: RefusalCounter,
    /// `pfnCreateSchedulingGroup` was handed a null private slot. **Expected 0**
    /// — the paired `CalcPrivate*Size` always answers a non-zero size.
    scheduling_group_bad_slot: RefusalCounter,
    /// `pfnDestroySchedulingGroup` on a group that was never created. ⚠ Expected
    /// to track `SchedulingGroupRefused` exactly; a destroy without a matching
    /// refused create would mean the runtime believes it owns a group this driver
    /// never returned.
    scheduling_group_destroy_unexpected: RefusalCounter,
    /// `pfnEnumerateMetaCommands` answered "zero meta-commands".
    ///
    /// ⚠ **Expected non-zero on any device that asks**, and it is an answer
    /// rather than a refusal: Helios publishes no vendor meta-commands and
    /// neither does vkd3d. It is here so that "the runtime asked" is visible; a
    /// reading of 0 across a DirectML workload would mean the runtime never
    /// asked, which is itself worth knowing.
    meta_command_enumerate_empty: RefusalCounter,
    /// `pfnEnumerateMetaCommands` with a null count pointer. **Expected 0.**
    meta_command_enumerate_bad_arg: RefusalCounter,
    /// `pfnEnumerateMetaCommandParameters` refused. **Expected 0**: the
    /// enumeration published nothing, so no caller should have a `CommandId` to
    /// ask about. A hit means a caller invented one.
    meta_command_parameters_refused: RefusalCounter,
    /// `pfnCalcPrivateMetaCommandSize` was given a non-zero creation-parameter
    /// size with a null pointer. **Expected 0** — a non-zero count with a null
    /// pointer is never legal.
    meta_command_calc_bad_arg: RefusalCounter,
    /// `pfnCreateMetaCommand` refused. **Expected 0**, for
    /// `MetaCommandParametersRefused`'s reason.
    meta_command_refused: RefusalCounter,
    /// `pfnCreateMetaCommand` was handed a null private slot. **Expected 0.**
    meta_command_bad_slot: RefusalCounter,
    /// `pfnDestroyMetaCommand` on a command that was never created. ⚠ Expected to
    /// track `MetaCommandRefused`.
    meta_command_destroy_unexpected: RefusalCounter,
    /// `pfnGetMetaCommandRequiredParameterInfo` answered a zero resource size.
    /// **Expected 0** — it can only be reached with a handle whose create
    /// refused.
    meta_command_required_parameter_empty: RefusalCounter,
    /// `pfnGetMetaCommandRequiredParameterInfo` with a null out-struct.
    /// **Expected 0.**
    meta_command_required_parameter_bad_arg: RefusalCounter,
    /// `pfnCalcPrivateStateObjectSize` with a null arg. **Expected 0.**
    state_object_calc_bad_arg: RefusalCounter,
    /// `pfnCreateStateObject` refused.
    ///
    /// ⚠ **Expected 0 while `RaytracingTier` and `WorkGraphsTier` both read
    /// `NOT_SUPPORTED`; expected non-zero the moment a DXR or work-graph workload
    /// runs anyway.** It is the single number that says whether raytracing is
    /// worth costing: nothing else in this driver distinguishes "no app asked"
    /// from "apps ask constantly and we decline".
    state_object_refused: RefusalCounter,
    /// `pfnCreateStateObject` was handed a null private slot. **Expected 0.**
    state_object_bad_slot: RefusalCounter,
    /// `pfnDestroyStateObject` on an object that was never created. ⚠ Expected to
    /// track `StateObjectRefused` plus `AddToStateObjectRefused`.
    state_object_destroy_unexpected: RefusalCounter,
    /// `pfnGetRaytracingAccelerationStructurePrebuildInfo` answered three zeros.
    ///
    /// ⚠ **Expected 0**; a hit means an application is sizing acceleration-
    /// structure buffers on a device that reports no raytracing, and it will get
    /// zero-byte allocations. Read it beside `RtBuildRefused` — this one fires
    /// *before* the build, so it is the earlier warning of the same workload.
    rt_prebuild_info_empty: RefusalCounter,
    /// `pfnGetRaytracingAccelerationStructurePrebuildInfo` with a null
    /// out-struct. **Expected 0.**
    rt_prebuild_info_bad_arg: RefusalCounter,
    /// `pfnCheckDriverMatchingIdentifier` answered `UNSUPPORTED_TYPE`.
    /// **Expected 0**; a hit means something is trying to deserialize a
    /// previously-built acceleration structure against this driver.
    driver_matching_identifier_refused: RefusalCounter,
    /// `pfnGetShaderIdentifier` answered NULL. **Expected 0** — it can only be
    /// reached with a state-object handle whose create refused.
    shader_identifier_absent: RefusalCounter,
    /// `pfnGetShaderStackSize` answered `UINT_MAX`. **Expected 0.**
    shader_stack_size_absent: RefusalCounter,
    /// `pfnGetPipelineStackSize` answered 0. **Expected 0.**
    pipeline_stack_size_absent: RefusalCounter,
    /// `pfnSetPipelineStackSize` was dropped. **Expected 0.**
    set_pipeline_stack_size_dropped: RefusalCounter,
    /// `pfnCalcPrivateAddToStateObjectSize` with a null arg. **Expected 0.**
    add_to_state_object_calc_bad_arg: RefusalCounter,
    /// `pfnAddToStateObject` refused. **Expected 0** — its
    /// `StateObjectToGrowFrom` can only name an object whose create refused.
    add_to_state_object_refused: RefusalCounter,
    /// `pfnAddToStateObject` was handed a null private slot. **Expected 0.**
    add_to_state_object_bad_slot: RefusalCounter,
    /// `pfnGetProgramIdentifier` answered a zeroed identifier. **Expected 0.**
    program_identifier_absent: RefusalCounter,
    /// `pfnGetWorkGraphMemoryRequirements` answered zeros. **Expected 0**; a hit
    /// means an application is sizing a work-graph backing store on a device that
    /// reports `WorkGraphsTier = NOT_SUPPORTED`.
    work_graph_memory_absent: RefusalCounter,
    /// `pfnGetWorkGraphMemoryRequirements` with a null out-struct. **Expected
    /// 0.**
    work_graph_memory_bad_arg: RefusalCounter,
    /// `pfnSetBackgroundProcessingMode` refused, having answered FALSE for
    /// further measurements.
    ///
    /// ⚠ **Expected 0 in retail; expected non-zero under a profiler.** ⛔ A hit is
    /// not closeable by raising `BackgroundProcessingSupported` alone: vkd3d's own
    /// entry point is a `FIXME` stub returning `E_NOTIMPL`
    /// (`libs/vkd3d/device.c:8941-8949`), so the engine would have to change
    /// first.
    background_processing_refused: RefusalCounter,
    /// `pfnImplicitShaderCacheControl` refused.
    ///
    /// ⚠ **Expected 0 outside developer mode.** `DDI_REFERENCE.md` §14.1.1 quotes
    /// `ShaderCache.md:219`: the runtime calls it only for the `DRIVER_MANAGED`
    /// implicit-cache kind, and *"this API will only be supported in developer
    /// mode"*. A hit under developer mode is expected and benign; a hit outside
    /// it means the runtime believes this driver reported a driver-managed cache,
    /// which would contradict `caps12.rs:595`.
    implicit_shader_cache_refused: RefusalCounter,

    // -- command list ------------------------------------------------------
    /// A command-list slot in this lane could not resolve its
    /// `D3D12DDI_HCOMMANDLIST` to a live `queue::CommandListState`. **Expected
    /// 0** — the runtime only records into a list `pfnCreateCommandList` returned
    /// `S_OK` for.
    ///
    /// ⚠ Only [`write_buffer_immediate`] can move it: it is the one slot here
    /// that needs the engine list. Every other command-list slot in this lane
    /// refuses without touching the handle, which is deliberate — a refusal has
    /// no reason to dereference anything.
    command_list_missing: RefusalCounter,
    /// `pfnSetMarker` was dropped.
    ///
    /// ⚠ **Expected non-zero under any TDR-instrumented or profiled workload, and
    /// 0 otherwise** — it is the WDDM command-stream marker, not the PIX one (see
    /// [`set_marker`]). It carries no exposure: there is no guest-visible command
    /// stream to write a marker into, so there is nothing a real body could do
    /// differently until the driver has one.
    marker_dropped: RefusalCounter,
    /// `pfnSetProtectedResourceSession` with a null session, i.e. "no protected
    /// session". ⚠ **Expected non-zero** if the runtime issues it as part of list
    /// setup, and benign either way; it exists so the non-null arm below is
    /// readable on its own.
    protected_resource_session_none: RefusalCounter,
    /// `pfnSetProtectedResourceSession` with a **non-null** session.
    ///
    /// ⛔ **Expected 0, and a hit is a genuine finding with a named mechanism.**
    /// The create lives in the optional
    /// `D3D12DDI_DEVICE_FUNCS_CONTENT_PROTECTION_RESOURCES_0030` table, which
    /// this driver fills with uniform counting stubs, and `unknown_slot_noop`
    /// returns `0` — which as an `HRESULT` is `S_OK`. So a non-null session
    /// handle here means the runtime asked for that table despite the
    /// protected-resource caps zero-filling to `NONE`, and got a fake success
    /// over a zero-byte private block. See [`set_protected_resource_session`].
    protected_resource_session_refused: RefusalCounter,
    /// `pfnWriteBufferImmediate` was called at all.
    ///
    /// ⛔⛔ **RE-GRADED AND RENAMED 2026-08-07. It used to read: "Expected 0 while
    /// `caps12.rs:576` reports `WriteBufferImmediateQueueFlags = NONE`, and a
    /// non-zero reading is the evidence L1 needs to raise that cap." Every clause
    /// of that is now false**, and it was false the moment it shipped: `3c29677`
    /// raised the cap to `3D|COMPUTE|COPY` **in the same changeset**, its cited
    /// line no longer holds the cap, and the old name asserted a condition
    /// (`UnderNoneCap`) that is untrue of every call it counts. Left as-is it
    /// could only rise, so it could never again distinguish anything, while still
    /// printing in the `D3D12 DDI refusals:` line that `CONFORMANCE.md`'s charter
    /// says to drive to zero — inviting either a chase after a non-defect or a
    /// second "raise" of an already-raised cap.
    ///
    /// ⭐ What it is now: **a plain census of `pfnWriteBufferImmediate` entries**,
    /// which is the instrument `METHOD.md` saturation criterion 6 requires to tell
    /// *implemented* from *implemented-but-never-exercised*. Expected **non-zero**
    /// on any DRED or breadcrumb-using title, and 0 elsewhere. The slot forwards
    /// into vkd3d (see [`write_buffer_immediate`]), so a hit means the write
    /// worked. ⚠ Bumped on **every** call including the degenerate `Count == 0`
    /// one, so it counts calls rather than writes; `L9WriteBufferImmediateBadArg`
    /// beside it is the one that must stay 0.
    ///
    /// ⚠ **An instrument in a set named `DDI refusals:`, and there is precedent
    /// for that** — `CapsCalls`/`CapsMsaaCalls` in the spine's set and
    /// `L6PsoDynamicStateFlagForwarded` in L6's are the same shape. It uses
    /// `bump()` rather than `note_refusal()` because a *successful forward* must
    /// not print the whole refusal summary; the summary is emitted
    /// unconditionally at adapter close and device teardown
    /// (`crate::log_refusal_summary`), so the counter is still readable.
    write_buffer_immediate_calls: RefusalCounter,
    /// `pfnWriteBufferImmediate` with a null parameter array or a count above
    /// [`MAX_WRITE_BUFFER_IMMEDIATE_PARAMS`]. **Expected 0.**
    ///
    /// ⚠ **This is the one arm in the lane where the no-`report_error` decision
    /// is worth re-testing** (module doc). The immediates are dropped, so a
    /// caller polling the destination buffer waits forever — a hang rather than
    /// an error. It is counted rather than escalated because the argument comes
    /// from the *runtime*, not the application, and `pfnSetErrorCb` would remove
    /// the device; if this ever moves, that trade should be re-made with the
    /// evidence in hand.
    write_buffer_immediate_bad_arg: RefusalCounter,
    /// `pfnWriteBufferImmediate` carried a mode outside the three this header
    /// defines, and the **whole** call was refused.
    ///
    /// **Expected 0.** ⚠ All-or-nothing on purpose: a partially-applied batch is
    /// the shape that cannot be attributed afterwards.
    write_buffer_immediate_mode_unknown: RefusalCounter,
    /// The engine command list did not expose `ID3D12GraphicsCommandList2`.
    /// **Expected 0** — vkd3d's command list QIs to every
    /// `ID3D12GraphicsCommandList1..10` off one object
    /// (`libs/vkd3d/command.c:6680-6690`).
    write_buffer_immediate_engine_missing: RefusalCounter,
    /// `pfnSetViewInstanceMask` with the only mask consistent with one view
    /// instance (`1`), dropped.
    ///
    /// ⚠ **Expected reading is the open question this counter exists to close:**
    /// `DDI_REFERENCE.md` §14.1.1 records that a `NOT_SUPPORTED` tier suppresses
    /// its slots by a *different mechanism per feature*, and view instancing is
    /// not one of the three worked examples. So 0 means the runtime filters this
    /// slot at `ViewInstancingTier = NONE`, and non-zero means it does not —
    /// either is information, neither is a fault.
    view_instance_mask_identity_dropped: RefusalCounter,
    /// `pfnSetViewInstanceMask` with a mask other than `1`.
    ///
    /// ⛔ **Expected 0, and a hit is a finding**: it is a workload asserting
    /// multiple view instances against `caps12.rs:577`'s
    /// `ViewInstancingTier = NONE`, and the geometry for the extra views will be
    /// missing. It is also the number that says whether implementing view
    /// instancing is worth L6's time.
    view_instance_mask_refused: RefusalCounter,
    /// `pfnInitializeMetaCommand` refused. **Expected 0.**
    meta_command_initialize_refused: RefusalCounter,
    /// `pfnExecuteMetaCommand` refused. **Expected 0.**
    meta_command_execute_refused: RefusalCounter,
    /// `pfnBuildRaytracingAccelerationStructure` refused. **Expected 0**; a hit
    /// means a DXR workload is running against `RaytracingTier = NOT_SUPPORTED`
    /// and its acceleration structures do not exist.
    rt_build_refused: RefusalCounter,
    /// `pfnEmitRaytracingAccelerationStructurePostbuildInfo` refused. **Expected
    /// 0**; expected to trail `RtBuildRefused` when it moves.
    rt_postbuild_info_refused: RefusalCounter,
    /// `pfnCopyRaytracingAccelerationStructure` refused. **Expected 0.**
    rt_copy_refused: RefusalCounter,
    /// `pfnSetPipelineState1` refused. **Expected 0** — it binds a state object,
    /// and every state-object create refused.
    set_pipeline_state1_refused: RefusalCounter,
    /// `pfnDispatchRays` refused. **Expected 0**; a hit is an empty render target
    /// with no error, which is why it also logs.
    dispatch_rays_refused: RefusalCounter,
    /// `pfnRSSetShadingRate` with the default `1X1` / all-`PASSTHROUGH` state,
    /// dropped.
    ///
    /// ⚠ **Expected non-zero and roughly once per command-list reset**, because
    /// `DDI_REFERENCE.md` §14.0 measured this slot inside the runtime's fixed
    /// 15-call state-reset block, issued *"whether or not the application touches
    /// that state"*. It carries **no exposure**: `1X1` with passthrough combiners
    /// is exactly the state a VRS-less device is already in. It exists so that
    /// `ShadingRateRefused` is not buried under it.
    shading_rate_default_dropped: RefusalCounter,
    /// `pfnRSSetShadingRate` with a coarse rate or a non-passthrough combiner.
    ///
    /// ⛔ **Expected 0, and a hit is a finding**: the workload will be shaded at
    /// full rate where it asked for coarse, against
    /// `caps12.rs:586`'s `VariableShadingRateTier = VRS_NONE`. Correct output,
    /// wrong performance — the failure mode a counter is the only way to see.
    shading_rate_refused: RefusalCounter,
    /// `pfnRSSetShadingRateImage` with a null resource, i.e. "unbind". ⚠ Benign
    /// and possibly frequent; it exists so the arm below is readable alone.
    shading_rate_image_none: RefusalCounter,
    /// `pfnRSSetShadingRateImage` with a real resource. ⛔ **Expected 0** — a
    /// shading-rate image needs VRS tier 2 and a non-zero
    /// `ShadingRateImageTileSize`, and this driver reports neither.
    shading_rate_image_refused: RefusalCounter,
    /// `pfnDispatchMesh` refused.
    ///
    /// ⛔ **Expected 0, and a hit is the most visible defect this lane can
    /// produce**: geometry silently missing, with `MeshShaderTier = MESH_NONE`
    /// (`caps12.rs:593`) as the only explanation and no error anywhere. Read it
    /// beside L6's mesh-shader creates: if those refuse and this moves, an
    /// application ignored the tier.
    dispatch_mesh_refused: RefusalCounter,
    /// `pfnSetProgram` refused. **Expected 0** — it selects a program out of a
    /// state object, and every state-object create refused.
    set_program_refused: RefusalCounter,
    /// `pfnDispatchGraph` refused. **Expected 0**; like `DispatchMeshRefused`,
    /// its symptom is silence.
    dispatch_graph_refused: RefusalCounter,
}

pub(crate) static L9_REFUSALS: L9Refusals = L9Refusals {
    shader_comment_refused: RefusalCounter::new("L9ShaderCommentRefused"),
    shader_comment_bad_arg: RefusalCounter::new("L9ShaderCommentBadArg"),
    scheduling_group_calc_bad_arg: RefusalCounter::new("L9SchedulingGroupCalcBadArg"),
    scheduling_group_refused: RefusalCounter::new("L9SchedulingGroupRefused"),
    scheduling_group_bad_slot: RefusalCounter::new("L9SchedulingGroupBadSlot"),
    scheduling_group_destroy_unexpected: RefusalCounter::new("L9SchedulingGroupDestroyUnexpected"),
    meta_command_enumerate_empty: RefusalCounter::new("L9MetaCommandEnumerateEmpty"),
    meta_command_enumerate_bad_arg: RefusalCounter::new("L9MetaCommandEnumerateBadArg"),
    meta_command_parameters_refused: RefusalCounter::new("L9MetaCommandParametersRefused"),
    meta_command_calc_bad_arg: RefusalCounter::new("L9MetaCommandCalcBadArg"),
    meta_command_refused: RefusalCounter::new("L9MetaCommandRefused"),
    meta_command_bad_slot: RefusalCounter::new("L9MetaCommandBadSlot"),
    meta_command_destroy_unexpected: RefusalCounter::new("L9MetaCommandDestroyUnexpected"),
    meta_command_required_parameter_empty: RefusalCounter::new(
        "L9MetaCommandRequiredParameterEmpty",
    ),
    meta_command_required_parameter_bad_arg: RefusalCounter::new(
        "L9MetaCommandRequiredParameterBadArg",
    ),
    state_object_calc_bad_arg: RefusalCounter::new("L9StateObjectCalcBadArg"),
    state_object_refused: RefusalCounter::new("L9StateObjectRefused"),
    state_object_bad_slot: RefusalCounter::new("L9StateObjectBadSlot"),
    state_object_destroy_unexpected: RefusalCounter::new("L9StateObjectDestroyUnexpected"),
    rt_prebuild_info_empty: RefusalCounter::new("L9RaytracingPrebuildInfoEmpty"),
    rt_prebuild_info_bad_arg: RefusalCounter::new("L9RaytracingPrebuildInfoBadArg"),
    driver_matching_identifier_refused: RefusalCounter::new("L9DriverMatchingIdentifierRefused"),
    shader_identifier_absent: RefusalCounter::new("L9ShaderIdentifierAbsent"),
    shader_stack_size_absent: RefusalCounter::new("L9ShaderStackSizeAbsent"),
    pipeline_stack_size_absent: RefusalCounter::new("L9PipelineStackSizeAbsent"),
    set_pipeline_stack_size_dropped: RefusalCounter::new("L9SetPipelineStackSizeDropped"),
    add_to_state_object_calc_bad_arg: RefusalCounter::new("L9AddToStateObjectCalcBadArg"),
    add_to_state_object_refused: RefusalCounter::new("L9AddToStateObjectRefused"),
    add_to_state_object_bad_slot: RefusalCounter::new("L9AddToStateObjectBadSlot"),
    program_identifier_absent: RefusalCounter::new("L9ProgramIdentifierAbsent"),
    work_graph_memory_absent: RefusalCounter::new("L9WorkGraphMemoryAbsent"),
    work_graph_memory_bad_arg: RefusalCounter::new("L9WorkGraphMemoryBadArg"),
    background_processing_refused: RefusalCounter::new("L9BackgroundProcessingRefused"),
    implicit_shader_cache_refused: RefusalCounter::new("L9ImplicitShaderCacheRefused"),
    command_list_missing: RefusalCounter::new("L9CommandListMissing"),
    marker_dropped: RefusalCounter::new("L9MarkerDropped"),
    protected_resource_session_none: RefusalCounter::new("L9ProtectedResourceSessionNone"),
    protected_resource_session_refused: RefusalCounter::new("L9ProtectedResourceSessionRefused"),
    write_buffer_immediate_calls: RefusalCounter::new(
        "L9WriteBufferImmediateCalls",
    ),
    write_buffer_immediate_bad_arg: RefusalCounter::new("L9WriteBufferImmediateBadArg"),
    write_buffer_immediate_mode_unknown: RefusalCounter::new("L9WriteBufferImmediateModeUnknown"),
    write_buffer_immediate_engine_missing: RefusalCounter::new(
        "L9WriteBufferImmediateEngineMissing",
    ),
    view_instance_mask_identity_dropped: RefusalCounter::new("L9ViewInstanceMaskIdentityDropped"),
    view_instance_mask_refused: RefusalCounter::new("L9ViewInstanceMaskRefused"),
    meta_command_initialize_refused: RefusalCounter::new("L9MetaCommandInitializeRefused"),
    meta_command_execute_refused: RefusalCounter::new("L9MetaCommandExecuteRefused"),
    rt_build_refused: RefusalCounter::new("L9RaytracingBuildRefused"),
    rt_postbuild_info_refused: RefusalCounter::new("L9RaytracingPostbuildInfoRefused"),
    rt_copy_refused: RefusalCounter::new("L9RaytracingCopyRefused"),
    set_pipeline_state1_refused: RefusalCounter::new("L9SetPipelineState1Refused"),
    dispatch_rays_refused: RefusalCounter::new("L9DispatchRaysRefused"),
    shading_rate_default_dropped: RefusalCounter::new("L9ShadingRateDefaultDropped"),
    shading_rate_refused: RefusalCounter::new("L9ShadingRateRefused"),
    shading_rate_image_none: RefusalCounter::new("L9ShadingRateImageNone"),
    shading_rate_image_refused: RefusalCounter::new("L9ShadingRateImageRefused"),
    dispatch_mesh_refused: RefusalCounter::new("L9DispatchMeshRefused"),
    set_program_refused: RefusalCounter::new("L9SetProgramRefused"),
    dispatch_graph_refused: RefusalCounter::new("L9DispatchGraphRefused"),
};
