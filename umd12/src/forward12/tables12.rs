//! `pfnFillDDITable` — the three driver-side tables, and the 11-lane install
//! sequencer.
//!
//! ⭐ **This file is the sequencer, and S6-0 wrote all of it.** `PARALLEL.md` §5
//! originally had each lane appending one line here; that was inverted once
//! S6-0 could name all eleven lanes up front, because a file eleven agents
//! append to is a merge point, and one they never touch is not. A lane's diff
//! against this file is **empty**.
//!
//! # The fill, in four steps, and why it is four and not one
//!
//! 1. **`stub_fill_bytes(p_table, table_size, unknown_slot_noop)`** — over the
//!    **runtime's** buffer, with the **runtime's** byte count. This is the "no
//!    NULL slot, ever" guarantee, and it covers the case this driver cannot
//!    otherwise reach: a table the runtime sized *larger* than the SDK header
//!    `ddi12` was generated from. Those slots have no name here; they get a
//!    counted stub rather than whatever was in the runtime's heap.
//! 2. **Build the table as a typed local**, every slot a per-slot counting noop
//!    ([`noop12::stubbed_table`]).
//! 3. **Run the install chain** over that local. Lanes write
//!    `f.pfnCreateCommandQueue = Some(create_command_queue)` — a **typed field
//!    assignment the compiler checks against the bindgen signature**, which is
//!    the whole premise of `PARALLEL.md` §7.
//! 4. **Copy `min(table_size, size_of::<T>())` bytes** into the runtime's
//!    buffer.
//!
//! ⛔ **Step 4's count comes from the argument, never from `size_of::<T>()`.**
//! `d3d12umddi.h:2527-2528` parameterises the size explicitly and it **moves
//! with the negotiated version** — 992/600/56 at `_0110`, 768/464/56 at `_0040`
//! (`DDI_REFERENCE.md` §2.2). `ARCHITECTURE.md` §12 rule 16 / R702 is the scar:
//! 24H2 passed 576 bytes for a 592-byte `DRIVERCAPS` and the D3D11 driver wrote
//! past it. Both directions are counted, so a mismatch is visible rather than
//! merely survived.
//!
//! ⚠ Steps 1 and 3 are the only places `size_of::<T>()` appears, and both are
//! about memory **this driver owns**: step 1's `table_size` is the runtime's,
//! and step 3's local is ours.
//!
//! # ⭐ Install order is structural, not textual
//!
//! `ARCHITECTURE.md` §12 rule 9: correctness of every ≥11.1 D3D11 device once
//! rested on `install()` running before `install_11_1()`, and the wrong order
//! produced *"wrong blending for DWM, no counter, no log, only pixels."*
//! [`Filling`] carries a stage marker, each lane's `install` names the previous
//! lane's marker as its input, and the whole chain is `#[must_use]`. So:
//!
//! * a lane **cannot** run before the stub fill — there is no other way to
//!   obtain a `Filling`, and its constructor is private to this module;
//! * a lane **cannot** be dropped from the chain — the next lane's signature
//!   names its marker, and the chain's terminus is consumed by [`seal`];
//! * a lane **cannot** be reordered — the same reason, in the other direction.
//!
//! It also carries the `&mut` to the table, so the table cannot be read or
//! copied while the chain is live.

use core::ffi::c_void;
use core::marker::PhantomData;

use helios_umd_common::hr::{Hresult, E_INVALIDARG, S_OK};
use helios_umd_common::noop::stub_fill_bytes;

use super::{cmdlist, copy, descriptors, fence, misc, present12, pso, queue, resource12, rootargs};
use super::noop12;
use crate::{caps12, ddi12, log_error, note_refusal, UMD12_REFUSALS};

/// `D3D12DDI_DEVICE_FUNCS_CORE_0109` — 124 slots.
pub(crate) type DeviceCoreTable = ddi12::D3D12DDI_DEVICE_FUNCS_CORE_0109;
/// `D3D12DDI_COMMAND_LIST_FUNCS_3D_0108` — 75 slots.
pub(crate) type CommandListTable = ddi12::D3D12DDI_COMMAND_LIST_FUNCS_3D_0108;
/// `D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001` — 7 slots.
pub(crate) type CommandQueueTable = ddi12::D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001;

// ---------------------------------------------------------------------------
// The install-chain token
// ---------------------------------------------------------------------------

/// A DDI table under construction, tagged with how far the install chain has
/// got.
///
/// `T` is the table struct; `S` is an uninhabited stage marker. Neither is ever
/// a value: the whole type is one `&mut T` at runtime.
#[must_use = "the install chain must reach `seal`, or a lane was silently dropped"]
pub(crate) struct Filling<'a, T, S> {
    table: &'a mut T,
    _stage: PhantomData<fn() -> S>,
}

impl<'a, T, S> Filling<'a, T, S> {
    /// The table, for the lane that currently holds the token.
    pub(crate) fn table(&mut self) -> &mut T {
        self.table
    }

    /// Hand the table to the next stage.
    ///
    /// ⚠ Callable only by whoever already holds a `Filling`, i.e. by a lane the
    /// chain reached — which is what makes "cannot run before the stub fill"
    /// and "cannot be reordered" the same property.
    pub(crate) fn advance<S2>(self) -> Filling<'a, T, S2> {
        Filling {
            table: self.table,
            _stage: PhantomData,
        }
    }
}

/// Consume the last token of a chain.
///
/// Exists so the terminus is a named act rather than a `let _ =`, which would
/// silence the `#[must_use]` and with it the "a lane was dropped" signal.
fn seal<T, S>(_last: Filling<'_, T, S>) {}

/// Stage markers. Uninhabited: they are type-level names, never values.
///
/// ⚠ Deliberately suffixed `Slots` rather than named after the lane bare
/// (`Copy`, `Fence`, `Queue`): `Copy` would shadow the prelude trait in this
/// module, and one shadowed prelude name is enough to make a later
/// `#[derive(Copy)]` here fail with an error that points nowhere useful.
pub(crate) mod stage {
    /// Every slot carries its per-slot counting noop; no lane has run.
    pub(crate) enum Stubbed {}
    /// L1 — caps: format/MSAA queries.
    pub(crate) enum CapsSlots {}
    /// L2 — queue, pool, recorder, command-list lifetime.
    pub(crate) enum QueueSlots {}
    /// L4 — resources, heaps, residency, introspection.
    pub(crate) enum ResourceSlots {}
    /// L5 — descriptor heaps and views.
    pub(crate) enum DescriptorSlots {}
    /// L6a — PSO, root signatures, sub-state.
    pub(crate) enum PsoSlots {}
    /// L6b — shaders.
    pub(crate) enum ShaderSlots {}
    /// L7 — fences and query heaps.
    pub(crate) enum FenceSlots {}
    /// L8 — present.
    pub(crate) enum PresentSlots {}
    /// L9 — the tail: meta-commands, state objects, VRS, mesh, work graphs.
    pub(crate) enum MiscSlots {}
    /// L3a — recording: draw, fixed-function state, IA/SO/OM.
    pub(crate) enum RecordSlots {}
    /// L3b — root arguments, descriptor binding, clears.
    pub(crate) enum RootArgSlots {}
    /// L3c — copy, resolve, barriers, queries.
    pub(crate) enum CopySlots {}
}

// ---------------------------------------------------------------------------
// The command-list table handles
// ---------------------------------------------------------------------------

/// The runtime's per-index `D3D12DDI_HRTTABLE` for the command-list table.
///
/// ⭐ **Stashed here because there is no other way to obtain it.**
/// `DDI_REFERENCE.md` §2.2, measured by `D12-G5`: the runtime fills
/// `COMMAND_LIST_3D` **twice** during device creation, in immediate succession,
/// with the 5th `UINT` = 0 then 1 and two distinct `hRTTable` handles — and
/// those handles are exactly what the driver later hands to
/// `pfnSetCommandListDDITableCb` on every command-list create. Nothing else
/// carries them.
///
/// ⚠ Two entries because two is what was measured, and `pfnGetOptionalDDITables`
/// answers 0 (so the second table is the runtime's own doing, not something this
/// driver asked for). An index beyond this is counted and dropped rather than
/// growing the array on runtime data.
const COMMAND_LIST_TABLE_SLOTS: usize = 2;
static COMMAND_LIST_RT_TABLES: [core::sync::atomic::AtomicUsize; COMMAND_LIST_TABLE_SLOTS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; COMMAND_LIST_TABLE_SLOTS];

/// The runtime table handle for command-list table `index`, or 0.
///
/// L2 (`queue.rs`) is its real consumer, the moment `pfnCreateCommandList`
/// becomes a body: `pfnSetCommandListDDITableCb(hRTCommandList, hRTTable)` has
/// nowhere else to get its second argument (`DDI_REFERENCE.md` §2.2). Until
/// then it is read by [`log_command_list_tables`] — because a stash nothing
/// reads is the T5 anti-pattern, *an instrument nothing can read is not an
/// instrument*, and because the handles themselves are contract capture that
/// `D12-G5` needed a spy proxy in front of WARP to obtain.
pub(crate) fn command_list_rt_table(index: usize) -> usize {
    COMMAND_LIST_RT_TABLES
        .get(index)
        .map_or(0, |h| h.load(core::sync::atomic::Ordering::Relaxed))
}

/// Print the runtime table handles this adapter was handed, one line.
///
/// Called from `adapter12::close_adapter`, beside the refusal summary and the
/// noop hit counts.
pub(crate) fn log_command_list_tables() {
    let mut line = String::with_capacity(64);
    for index in 0..COMMAND_LIST_TABLE_SLOTS {
        line.push_str(&format!(" [{index}]={:#x}", command_list_rt_table(index)));
    }
    log_error!("D3D12 command-list hRTTable:{line}");
}

fn stash_command_list_rt_table(index: ddi12::UINT, handle: *mut c_void) {
    match COMMAND_LIST_RT_TABLES.get(index as usize) {
        Some(slot) => slot.store(handle as usize, core::sync::atomic::Ordering::Relaxed),
        None => note_refusal(&UMD12_REFUSALS.command_list_table_index_unbounded),
    }
}

// ---------------------------------------------------------------------------
// pfnFillDDITable
// ---------------------------------------------------------------------------

/// Fill one runtime-owned DDI table.
///
/// Called from `adapter12::fill_ddi_table`, which owns the DDI slot; this owns
/// the tables.
///
/// # Safety
/// `table` must point at `table_size` writable bytes the runtime owns, aligned
/// for function pointers. `table_size` is the runtime's own count and is the
/// only authority on how much of that buffer exists.
pub(crate) unsafe fn fill(
    table_type: ddi12::D3D12DDI_TABLE_TYPE,
    table: *mut c_void,
    table_size: ddi12::SIZE_T,
    index: ddi12::UINT,
    h_rt_table: ddi12::D3D12DDI_HRTTABLE,
) -> Hresult {
    let bytes = table_size as usize;
    if table.is_null() || bytes < core::mem::size_of::<usize>() {
        note_refusal(&UMD12_REFUSALS.fill_ddi_table_bad_arg);
        return E_INVALIDARG;
    }

    // ⛔ A closed dispatch: exactly three values reach a fill, and every other
    // value — known-but-unimplemented, retired, or one this header has never
    // heard of — refuses **without writing a byte**.
    //
    // ⚠ `ARCHITECTURE.md` §1.2 step 9 asks for "an exhaustive match with 25
    // arms". Twenty-two of those arms would be textually identical refusals, and
    // the property `DECISIONS.md` §7.4 actually demands is *that no unrecognised
    // value may select a table shape* — which this has, with 22 fewer chances to
    // paste the wrong constant into one of them. The refused type is logged, so
    // the evidence an enumerated match would have produced is produced anyway.
    match table_type {
        ddi12::D3D12DDI_TABLE_TYPE_D3D12DDI_TABLE_TYPE_DEVICE_CORE => {
            // SAFETY: forwarded unchanged; the caller's guarantee is this
            // function's, and the table type has been checked.
            unsafe { fill_device_core(table, bytes) }
        }
        ddi12::D3D12DDI_TABLE_TYPE_D3D12DDI_TABLE_TYPE_COMMAND_LIST_3D => {
            stash_command_list_rt_table(index, h_rt_table.handle);
            // SAFETY: as above.
            unsafe { fill_command_list(table, bytes) }
        }
        ddi12::D3D12DDI_TABLE_TYPE_D3D12DDI_TABLE_TYPE_COMMAND_QUEUE_3D => {
            // SAFETY: as above.
            unsafe { fill_command_queue(table, bytes) }
        }
        // ⛔ **MEASURED, and it corrects the obvious design.** The first cut
        // refused every other table type without writing a byte, reasoning that
        // a driver must never fill a shape it does not understand. `D12-G7`
        // failed on exactly that: the runtime asks for
        // `D3D12DDI_TABLE_TYPE_0096_EXTENDED_FEATURES` (**27**, 32 bytes, four
        // slots) on a baseline device — `DDI_REFERENCE.md` §2.1 already records
        // it as *"filled with no extended-features handshake, for a baseline
        // device"* — and a refusal there loses the device, with `S_OK`
        // everywhere else and nothing else refused.
        //
        // ⭐ Filling it selects no *shape*, which is what `DECISIONS.md` §7.4
        // actually forbids: the slot count comes from the runtime's own
        // `SIZE_T` and every slot gets the same uniform counting stub. That is
        // strictly safer than refusing — a refused table is a table the runtime
        // may still call through, and those slots are NULL.
        //
        // ⚠ It is also honest about what it is: nothing here is implemented.
        // `unknown_slot_noop` returns 0, which for the extended-features
        // handshake means "zero supported features" — the answer this driver
        // can defend. The counter is what stops that reading as support.
        other => {
            note_refusal(&UMD12_REFUSALS.fill_ddi_table_unknown_type);
            log_error!(
                "FillDDITable: table type {other} has no typed handler; filling {bytes} B \
                 ({} slots) with counting stubs (index={index})",
                bytes / core::mem::size_of::<usize>(),
            );
            // SAFETY: the caller guarantees `bytes` writable, pointer-aligned
            // bytes; the count is the runtime's own and is never `size_of::<T>()`.
            unsafe { stub_fill_bytes(table, bytes, noop12::unknown_slot_noop) };
            S_OK
        }
    }
}

/// The size of the table struct **this build's header** describes, or 0 for a
/// table type this driver does not serve.
///
/// Exists for `probe12::helios_umd12_probe_ddi_table_size_v1`: a size test that
/// hard-codes 992 / 600 / 56 agrees with whichever SDK it was written against,
/// not the one being built, and that is the one failure mode a size test must
/// not have.
pub(crate) fn header_table_size(table_type: ddi12::D3D12DDI_TABLE_TYPE) -> usize {
    match table_type {
        ddi12::D3D12DDI_TABLE_TYPE_D3D12DDI_TABLE_TYPE_DEVICE_CORE => {
            core::mem::size_of::<DeviceCoreTable>()
        }
        ddi12::D3D12DDI_TABLE_TYPE_D3D12DDI_TABLE_TYPE_COMMAND_LIST_3D => {
            core::mem::size_of::<CommandListTable>()
        }
        ddi12::D3D12DDI_TABLE_TYPE_D3D12DDI_TABLE_TYPE_COMMAND_QUEUE_3D => {
            core::mem::size_of::<CommandQueueTable>()
        }
        _ => 0,
    }
}

/// Count and log the two ways the runtime's table size can differ from this
/// header's struct, and return how many bytes may be copied.
fn agreed_bytes(name: &str, runtime_bytes: usize, header_bytes: usize) -> usize {
    if runtime_bytes < header_bytes {
        // The R702 direction: the runtime's buffer is SHORTER than the struct
        // this build knows. Copying `header_bytes` here is the out-of-bounds
        // write. The tail slots simply do not exist for this runtime.
        note_refusal(&UMD12_REFUSALS.fill_ddi_table_truncated);
        log_error!(
            "FillDDITable {name}: runtime table is {runtime_bytes} B, this header's struct is \
             {header_bytes} B -- filling {runtime_bytes} B and leaving the rest alone"
        );
        runtime_bytes
    } else {
        if runtime_bytes > header_bytes {
            // The other direction: a table shape newer than `ddi12`. Step 1
            // already stubbed the tail, so nothing is NULL; it is still worth a
            // line, because those slots can never do anything.
            note_refusal(&UMD12_REFUSALS.fill_ddi_table_oversized);
            log_error!(
                "FillDDITable {name}: runtime table is {runtime_bytes} B, this header's struct is \
                 only {header_bytes} B -- the tail is served by counted stubs; refresh the bindings"
            );
        }
        header_bytes
    }
}

/// Copy a driver-built table into the runtime's buffer, bounded by the
/// runtime's own byte count.
///
/// # Safety
/// `dst` must point at `runtime_bytes` writable bytes the runtime owns.
unsafe fn publish<T>(name: &str, dst: *mut c_void, runtime_bytes: usize, built: &T) {
    let n = agreed_bytes(name, runtime_bytes, core::mem::size_of::<T>());
    // SAFETY: `n <= runtime_bytes` and `n <= size_of::<T>()` by construction in
    // `agreed_bytes`, `dst` is the runtime's buffer of at least `runtime_bytes`
    // writable bytes, and `built` is a live local — the two cannot overlap.
    unsafe {
        core::ptr::copy_nonoverlapping((built as *const T).cast::<u8>(), dst.cast::<u8>(), n);
    }
}

/// # Safety
/// As [`fill`].
unsafe fn fill_device_core(table: *mut c_void, bytes: usize) -> Hresult {
    // Step 1 — every slot the RUNTIME asked for is non-NULL, including any past
    // the end of this header's struct. Byte count from the argument.
    // SAFETY: the caller guarantees `bytes` writable, pointer-aligned bytes.
    unsafe { stub_fill_bytes(table, bytes, noop12::unknown_slot_noop) };

    // Step 2 — the typed local, every slot a per-slot counting noop.
    const _: () = assert!(
        noop12::device_core::STUBS.len() * core::mem::size_of::<usize>()
            == core::mem::size_of::<DeviceCoreTable>()
    );
    // SAFETY: `DeviceCoreTable` is a `#[repr(C)]` struct of exactly
    // `STUBS.len()` pointer-sized `Option<fn>` fields — asserted immediately
    // above from `size_of`, and by the macro's per-field offset proof in
    // `noop12`.
    let mut built: DeviceCoreTable = unsafe { noop12::stubbed_table(noop12::device_core::STUBS) };

    // Step 3 — the install chain. Order is structural (see the module doc).
    let t = Filling::<DeviceCoreTable, stage::Stubbed> {
        table: &mut built,
        _stage: PhantomData,
    };
    let t = caps12::install(t);
    let t = queue::install_core(t);
    let t = resource12::install(t);
    let t = descriptors::install(t);
    let t = pso::install(t);
    let t = pso::install_shaders(t);
    let t = fence::install(t);
    let t = present12::install_core(t);
    let t = misc::install_core(t);
    seal(t);

    // Step 4 — publish, bounded by the runtime's count.
    // SAFETY: as the caller's guarantee.
    unsafe { publish("DEVICE_CORE", table, bytes, &built) };
    S_OK
}

/// # Safety
/// As [`fill`].
unsafe fn fill_command_list(table: *mut c_void, bytes: usize) -> Hresult {
    // SAFETY: as `fill_device_core`.
    unsafe { stub_fill_bytes(table, bytes, noop12::unknown_slot_noop) };

    const _: () = assert!(
        noop12::command_list::STUBS.len() * core::mem::size_of::<usize>()
            == core::mem::size_of::<CommandListTable>()
    );
    // SAFETY: as `fill_device_core`.
    let mut built: CommandListTable = unsafe { noop12::stubbed_table(noop12::command_list::STUBS) };

    let t = Filling::<CommandListTable, stage::Stubbed> {
        table: &mut built,
        _stage: PhantomData,
    };
    let t = cmdlist::install(t);
    let t = rootargs::install(t);
    let t = copy::install(t);
    let t = present12::install_cmdlist(t);
    let t = misc::install_cmdlist(t);
    seal(t);

    // SAFETY: as `fill_device_core`.
    unsafe { publish("COMMAND_LIST_3D", table, bytes, &built) };
    S_OK
}

/// # Safety
/// As [`fill`].
unsafe fn fill_command_queue(table: *mut c_void, bytes: usize) -> Hresult {
    // SAFETY: as `fill_device_core`.
    unsafe { stub_fill_bytes(table, bytes, noop12::unknown_slot_noop) };

    const _: () = assert!(
        noop12::command_queue::STUBS.len() * core::mem::size_of::<usize>()
            == core::mem::size_of::<CommandQueueTable>()
    );
    // SAFETY: as `fill_device_core`.
    let mut built: CommandQueueTable =
        unsafe { noop12::stubbed_table(noop12::command_queue::STUBS) };

    let t = Filling::<CommandQueueTable, stage::Stubbed> {
        table: &mut built,
        _stage: PhantomData,
    };
    let t = queue::install_queue(t);
    seal(t);

    // SAFETY: as `fill_device_core`.
    unsafe { publish("COMMAND_QUEUE_CORE", table, bytes, &built) };
    S_OK
}
