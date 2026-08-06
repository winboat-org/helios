//! L3c — copy, resolve, barriers and queries.
//!
//! Owns 13 of `COMMAND_LIST_FUNCS_3D_0108`: copy/resolve 7, barriers 2,
//! queries/predication 4.
//!
//! # ⭐ Barriers are ONE path, and this file ships BOTH ends of it
//!
//! `DX12.md` §4.3 row 1 / `DDI_REFERENCE.md` §9.10.1, quoting
//! `D3D12EnhancedBarriers.md:539` verbatim: *"The D3D12 runtime internally
//! translates all `ResourceBarrier` calls to equivalent Enhanced Barriers at the
//! driver interface. Legacy barrier DDI's are never invoked on a driver
//! supporting enhanced barriers."* So the two generations are **mutually
//! exclusive at the driver**, selected by `EnhancedBarriersSupported`, and
//! exactly one of [`resource_barrier`] and [`barrier`] is reachable per build:
//!
//! | cap | reachable | dead |
//! |---|---|---|
//! | `EnhancedBarriersSupported = 0` | `pfnResourceBarrier` | `pfnBarrier` |
//! | `EnhancedBarriersSupported = 1` | `pfnBarrier` | `pfnResourceBarrier` |
//!
//! ⛔ **`caps12.rs:603` reports 0, so the SHIPPING configuration exercises the
//! LEGACY arm** — [`resource_barrier`] — and every `L3cEnhancedBarrier*` counter
//! below is expected to read **zero**. The cap is L1's line, outside this lane's
//! budget (`PARALLEL.md` §4), and this lane does not touch it.
//!
//! ⭐ Both slots are nevertheless real handlers, because the command-list table
//! has **no opt-out**: a NULL slot is a jump through a null pointer if the
//! runtime ever disagrees with this driver's own cap, and `tables12`'s whole
//! four-step fill exists to make "no NULL slot, ever" true. The enhanced arm is
//! also the better long-term target for a vkd3d forwarder, because
//! `D3D12_BARRIER_LAYOUT` maps far closer to `VkImageLayout` than
//! `D3D12_RESOURCE_STATES` does — which is why it is written now rather than
//! left as a refusal for the commit that raises the cap.
//!
//! # ⚠ What this lane cannot honour, and says so
//!
//! * **`pfnCopyTiles`** is behind `TiledResourcesTier`, which `caps12.rs`
//!   reports `NOT_SUPPORTED`. Refused, counted **and reported**.
//! * **`pfnAtomicCopyBufferRegion`** has an engine entry point,
//!   `ID3D12GraphicsCommandList1::AtomicCopyBufferUINT`, and that entry point is
//!   a **`FIXME` stub with an empty body** in this pinned vkd3d
//!   (`vkd3d-proton-helios/libs/vkd3d/command.c:18021-18032`). Forwarding into
//!   it would be fake success; it is refused, counted and reported instead.
//!
//! ⭐ **"Reported" means one list, not the device**, and that is what makes
//! refusing these two affordable. Both were counted-but-silent while the only
//! channel this file knew about was `pfnSetErrorCb`, whose answer is to remove
//! the whole `ID3D12Device`; [`report_error`] now routes through
//! `pfnSetCommandListErrorCb`, which quarantines the one list that recorded the
//! call and lets the application learn at `Close()`. The full argument, and the
//! false sentence it replaces, are in that function's doc.
//!
//! # ⭐ Where the copy DDI's shape came from
//!
//! `pfnCopyTextureRegion` is not the API's `D3D12_TEXTURE_COPY_LOCATION` pair.
//! Each side arrives as **two** arguments — a `D3D12DDIARG_BUFFER_PLACEMENT*`
//! carrying `{ hResource, Offset }` and a `D3D12DDIARG_PLACED_RESOURCE`
//! carrying `{ Layout, pLayout }` — and the SDK header states in a comment what
//! no public documentation does (`d3d12umddi.h:1570-1573`), which is that for
//! `D3D12DDI_RL_SELECT_SUBRESOURCE` **`pLayout` is unused and the subresource
//! index is the placement's `Offset`**:
//!
//! ```text
//! // D3D12DDI_RL_SELECT_SUBRESOURCE
//! // Use D3D12DDIARG_BUFFER_PLACEMENT::BaseAddress::UMD
//! //   D3D12DDI_HRESOURCE denotes resource
//! //   Offset specifies subresource
//! ```
//!
//! ⛔ The union discriminant is exactly the class `DDI_REFERENCE.md` §9.6.1
//! warns about, so [`texture_copy_location`] dispatches on the three named
//! layout values and refuses everything else rather than casting the DDI struct
//! at the API one.

use core::mem::ManuallyDrop;

use helios_umd_common::hr::{Hresult, E_INVALIDARG, E_NOTIMPL};
use helios_umd_common::refusals::RefusalCounter;
use helios_umd_common::throttle::LogThrottle;
// ⚠ Imported for `Interface::as_raw` (borrowing a live COM pointer into an API
// descriptor field without touching its reference count) and `Interface::cast`
// (the `QueryInterface` that reaches `ID3D12GraphicsCommandList1` and `…7`).
// Both are trait methods and therefore invisible to method resolution unless the
// trait is in scope.
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::{
    ID3D12GraphicsCommandList1, ID3D12GraphicsCommandList7, ID3D12Resource, D3D12_BARRIER_ACCESS,
    D3D12_BARRIER_ACCESS_COMMON, D3D12_BARRIER_GROUP, D3D12_BARRIER_GROUP_0, D3D12_BARRIER_LAYOUT,
    D3D12_BARRIER_LAYOUT_COPY_DEST, D3D12_BARRIER_LAYOUT_COPY_SOURCE,
    D3D12_BARRIER_LAYOUT_GENERIC_READ, D3D12_BARRIER_LAYOUT_SHADER_RESOURCE,
    D3D12_BARRIER_SUBRESOURCE_RANGE, D3D12_BARRIER_SYNC, D3D12_BARRIER_SYNC_ALL,
    D3D12_BARRIER_TYPE_BUFFER, D3D12_BARRIER_TYPE_GLOBAL, D3D12_BARRIER_TYPE_TEXTURE, D3D12_BOX,
    D3D12_BUFFER_BARRIER, D3D12_GLOBAL_BARRIER, D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    D3D12_PREDICATION_OP_EQUAL_ZERO, D3D12_PREDICATION_OP_NOT_EQUAL_ZERO, D3D12_QUERY_TYPE,
    D3D12_QUERY_TYPE_BINARY_OCCLUSION, D3D12_QUERY_TYPE_OCCLUSION,
    D3D12_QUERY_TYPE_PIPELINE_STATISTICS, D3D12_QUERY_TYPE_PIPELINE_STATISTICS1,
    D3D12_QUERY_TYPE_SO_STATISTICS_STREAM0, D3D12_QUERY_TYPE_SO_STATISTICS_STREAM1,
    D3D12_QUERY_TYPE_SO_STATISTICS_STREAM2, D3D12_QUERY_TYPE_SO_STATISTICS_STREAM3,
    D3D12_QUERY_TYPE_TIMESTAMP, D3D12_QUERY_TYPE_VIDEO_DECODE_STATISTICS, D3D12_RESOLVE_MODE,
    D3D12_RESOLVE_MODE_AVERAGE, D3D12_RESOLVE_MODE_DECODE_SAMPLER_FEEDBACK,
    D3D12_RESOLVE_MODE_DECOMPRESS, D3D12_RESOLVE_MODE_ENCODE_SAMPLER_FEEDBACK,
    D3D12_RESOLVE_MODE_MAX, D3D12_RESOLVE_MODE_MIN, D3D12_RESOURCE_ALIASING_BARRIER,
    D3D12_RESOURCE_BARRIER, D3D12_RESOURCE_BARRIER_0, D3D12_RESOURCE_BARRIER_FLAGS,
    D3D12_RESOURCE_BARRIER_FLAG_BEGIN_ONLY, D3D12_RESOURCE_BARRIER_FLAG_END_ONLY,
    D3D12_RESOURCE_BARRIER_TYPE_ALIASING, D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
    D3D12_RESOURCE_BARRIER_TYPE_UAV, D3D12_RESOURCE_STATES, D3D12_RESOURCE_TRANSITION_BARRIER,
    D3D12_RESOURCE_UAV_BARRIER, D3D12_SUBRESOURCE_FOOTPRINT, D3D12_TEXTURE_BARRIER,
    D3D12_TEXTURE_BARRIER_FLAGS, D3D12_TEXTURE_COPY_LOCATION, D3D12_TEXTURE_COPY_LOCATION_0,
    D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_BC1_TYPELESS, DXGI_FORMAT_BC5_SNORM, DXGI_FORMAT_BC6H_TYPELESS,
    DXGI_FORMAT_BC7_UNORM_SRGB,
};

use super::fence;
use super::queue::{self, CommandListState};
use super::resource12;
use super::tables12::{stage, CommandListTable, Filling};
use crate::{ddi12, device12, log_error, note_refusal, trace_line};

// ---------------------------------------------------------------------------
// ⭐ The enum identity proofs
// ---------------------------------------------------------------------------
//
// Every translation below that forwards a DDI enumerator's *value* into its
// `D3D12_*` API counterpart is only correct because the two enumerations agree
// on that value, and `ARCHITECTURE.md` §12 rule 1 forbids taking that on trust.
// Each assertion compares **two generated constants** — one out of
// `d3d12umddi.h` via bindgen, one out of the Win32 metadata via the `windows`
// crate. Neither side is transcribed, and a header revision that renumbered
// either one fails the host cross-check rather than mis-synchronising a frame.
//
// ⚠ Representative enumerators are chosen NON-zero and non-first: a table that
// agreed only at 0 would pass a first-value check and be wrong everywhere else.
// ⚠ The bitmask enumerations (`D3D12DDI_RESOURCE_STATES`,
// `D3D12DDI_BARRIER_SYNC`, `D3D12DDI_BARRIER_ACCESS`) are forwarded whole rather
// than enumerated, so their assertions pin the extremes of the bit range as well
// as a middle bit — a renumber inside the range moves at least one of them.

const _: () = assert!(
    ddi12::D3D12DDI_QUERY_TYPE_D3D12DDI_QUERY_TYPE_PIPELINE_STATISTICS1
        == D3D12_QUERY_TYPE_PIPELINE_STATISTICS1.0,
    "D3D12DDI_QUERY_TYPE must be value-identical to D3D12_QUERY_TYPE"
);
const _: () = assert!(
    ddi12::D3D12DDI_PREDICATION_OP_D3D12DDI_PREDICATION_OP_NOT_EQUAL_ZERO
        == D3D12_PREDICATION_OP_NOT_EQUAL_ZERO.0,
    "D3D12DDI_PREDICATION_OP must be value-identical to D3D12_PREDICATION_OP"
);
const _: () = assert!(
    ddi12::D3D12DDI_RESOLVE_MODE_D3D12DDI_RESOLVE_MODE_DECODE_SAMPLER_FEEDBACK_0073
        == D3D12_RESOLVE_MODE_DECODE_SAMPLER_FEEDBACK.0,
    "D3D12DDI_RESOLVE_MODE must be value-identical to D3D12_RESOLVE_MODE"
);
const _: () = assert!(
    ddi12::D3D12DDI_TILE_COPY_FLAGS_D3D12DDI_TILE_COPY_FLAG_SWIZZLED_TILED_RESOURCE_TO_LINEAR_BUFFER
        == windows::Win32::Graphics::Direct3D12::
            D3D12_TILE_COPY_FLAG_SWIZZLED_TILED_RESOURCE_TO_LINEAR_BUFFER
            .0,
    "D3D12DDI_TILE_COPY_FLAGS must be value-identical to D3D12_TILE_COPY_FLAGS"
);
const _: () = assert!(
    ddi12::D3D12DDI_RESOURCE_BARRIER_TYPE_D3D12DDI_RESOURCE_BARRIER_TYPE_UAV
        == D3D12_RESOURCE_BARRIER_TYPE_UAV.0,
    "D3D12DDI_RESOURCE_BARRIER_TYPE must be value-identical for the three shared types"
);
// ⚠ **Both** bits, not one representative: [`resource_barrier`] builds its
// forwarding mask out of these two API constants and applies it to a DDI value,
// so an identity that held for only one of them would silently drop the other.
const _: () = assert!(
    ddi12::D3D12DDI_RESOURCE_BARRIER_FLAGS_D3D12DDI_RESOURCE_BARRIER_FLAG_BEGIN_ONLY
        == D3D12_RESOURCE_BARRIER_FLAG_BEGIN_ONLY.0
        && ddi12::D3D12DDI_RESOURCE_BARRIER_FLAGS_D3D12DDI_RESOURCE_BARRIER_FLAG_END_ONLY
            == D3D12_RESOURCE_BARRIER_FLAG_END_ONLY.0,
    "D3D12DDI_RESOURCE_BARRIER_FLAGS must be value-identical for the two forwardable flags"
);
const _: () = assert!(
    ddi12::D3D12DDI_BARRIER_TYPE_D3D12DDI_BARRIER_TYPE_BUFFER == D3D12_BARRIER_TYPE_BUFFER.0,
    "D3D12DDI_BARRIER_TYPE must be value-identical for GLOBAL / TEXTURE / BUFFER"
);
const _: () = assert!(
    ddi12::D3D12DDI_TEXTURE_BARRIER_FLAGS_0088_D3D12DDI_TEXTURE_BARRIER_FLAG_DISCARD
        == windows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_BARRIER_FLAG_DISCARD.0,
    "D3D12DDI_TEXTURE_BARRIER_FLAGS_0088 must be value-identical to D3D12_TEXTURE_BARRIER_FLAGS"
);
// The three bitmasks, pinned at both ends and in the middle.
const _: () = assert!(
    ddi12::D3D12DDI_RESOURCE_STATES_D3D12DDI_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER
        == windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER.0
        && ddi12::D3D12DDI_RESOURCE_STATES_D3D12DDI_RESOURCE_STATE_RESOLVE_DEST
            == windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_RESOLVE_DEST.0
        && ddi12::D3D12DDI_RESOURCE_STATES_D3D12DDI_RESOURCE_STATE_0062_SHADING_RATE_SOURCE
            == windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_SHADING_RATE_SOURCE.0
        && ddi12::D3D12DDI_RESOURCE_STATES_D3D12DDI_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE
            == windows::Win32::Graphics::Direct3D12::
                D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE
                .0,
    "D3D12DDI_RESOURCE_STATES must be bit-identical to D3D12_RESOURCE_STATES"
);
const _: () = assert!(
    ddi12::D3D12DDI_BARRIER_SYNC_D3D12DDI_BARRIER_SYNC_DRAW
        == windows::Win32::Graphics::Direct3D12::D3D12_BARRIER_SYNC_DRAW.0
        && ddi12::D3D12DDI_BARRIER_SYNC_D3D12DDI_BARRIER_SYNC_RESOLVE
            == windows::Win32::Graphics::Direct3D12::D3D12_BARRIER_SYNC_RESOLVE.0
        && ddi12::D3D12DDI_BARRIER_SYNC_D3D12DDI_BARRIER_SYNC_SPLIT
            == windows::Win32::Graphics::Direct3D12::D3D12_BARRIER_SYNC_SPLIT.0,
    "D3D12DDI_BARRIER_SYNC must be bit-identical to D3D12_BARRIER_SYNC"
);
const _: () = assert!(
    ddi12::D3D12DDI_BARRIER_ACCESS_D3D12DDI_BARRIER_ACCESS_CONSTANT_BUFFER
        == windows::Win32::Graphics::Direct3D12::D3D12_BARRIER_ACCESS_CONSTANT_BUFFER.0
        && ddi12::D3D12DDI_BARRIER_ACCESS_D3D12DDI_BARRIER_ACCESS_RESOLVE_DEST
            == windows::Win32::Graphics::Direct3D12::D3D12_BARRIER_ACCESS_RESOLVE_DEST.0
        && ddi12::D3D12DDI_BARRIER_ACCESS_D3D12DDI_BARRIER_ACCESS_NO_ACCESS
            == windows::Win32::Graphics::Direct3D12::D3D12_BARRIER_ACCESS_NO_ACCESS.0,
    "D3D12DDI_BARRIER_ACCESS must be bit-identical to D3D12_BARRIER_ACCESS"
);
// The layout enumeration is identical over 0..=30 and at UNDEFINED; the five
// values [`api_barrier_layout`] maps by hand are the only exceptions
// (`DDI_REFERENCE.md` §9.10.1). These pin both halves of that claim.
const _: () = assert!(
    ddi12::D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_RESOLVE_DEST
        == windows::Win32::Graphics::Direct3D12::D3D12_BARRIER_LAYOUT_RESOLVE_DEST.0
        && ddi12::D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_VIDEO_QUEUE_COMMON
            == windows::Win32::Graphics::Direct3D12::D3D12_BARRIER_LAYOUT_VIDEO_QUEUE_COMMON.0
        && ddi12::D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_UNDEFINED
            == windows::Win32::Graphics::Direct3D12::D3D12_BARRIER_LAYOUT_UNDEFINED.0,
    "D3D12DDI_BARRIER_LAYOUT must be value-identical to D3D12_BARRIER_LAYOUT over the shared range"
);
// The block-compressed format ranges [`is_block_compressed`] tests. Ordering is
// what a renumber would break; the numbers themselves are never written here.
const _: () = assert!(
    DXGI_FORMAT_BC1_TYPELESS.0 < DXGI_FORMAT_BC5_SNORM.0
        && DXGI_FORMAT_BC5_SNORM.0 < DXGI_FORMAT_BC6H_TYPELESS.0
        && DXGI_FORMAT_BC6H_TYPELESS.0 < DXGI_FORMAT_BC7_UNORM_SRGB.0,
    "the DXGI block-compressed format ranges must stay ordered"
);

// ---------------------------------------------------------------------------
// Log budgets
// ---------------------------------------------------------------------------

// ⛔ **`log_error!` is unbounded by construction** (`umd_common/src/log.rs:279`)
// and every site in this file is on a per-command path: a copy or a barrier
// happens tens to thousands of times per frame. T2 measured one unbudgeted UMD
// log site at ~9k mutex-serialized writes per second. Same discipline and the
// same shape as `queue.rs`'s six budgets: the first 8, then every 4096th.

/// Budget for the copy / resolve lines.
static COPY_LOG: LogThrottle = LogThrottle::new();
/// Budget for the barrier lines, both generations.
static BARRIER_LOG: LogThrottle = LogThrottle::new();
/// Budget for the query / predication lines.
static QUERY_LOG: LogThrottle = LogThrottle::new();

/// The one budget shape this lane uses: the first 8, then every 4096th.
///
/// Returns the occurrence ordinal (0-based) when the line should be emitted.
fn budget(t: &LogThrottle) -> Option<usize> {
    t.first_n_then_every(8, 4096)
}

/// Sanity bound on the barrier `Count` / `NumBarriers` both barrier slots take.
///
/// CLAUDE.md: *validate every runtime-supplied size before reading.* Both DDIs
/// declare their array `_In_reads_(Count)` and the runtime is the authority, so
/// this is not a semantic cap — no D3D12 rule limits how many barriers one call
/// may carry. It bounds the allocation a corrupt count would demand, and its
/// counter says if a real workload ever approached it. Same shape and the same
/// value as `queue::MAX_EXECUTE_COMMAND_LISTS`.
const MAX_BARRIERS: usize = 65_536;

// ---------------------------------------------------------------------------
// The error channel
// ---------------------------------------------------------------------------

/// Report a **command-list**-scope failure from a recording slot, counting the
/// case where there is no way to hear it.
///
/// # ⛔ This is `pfnSetCommandListErrorCb`, and the difference from
/// `pfnSetErrorCb` is a compositor
///
/// An earlier revision of this function asserted that *"all 75 of its slots take
/// `D3D12DDI_HCOMMANDLIST` and nothing else and 74 of the 75 return `VOID`, so a
/// recording failure can only be reported through the device-scoped
/// `pfnSetErrorCb`"*. The premise is true; the conclusion was **false**, and the
/// `PARALLEL.md` §10 review settled it out of the header this tree ships.
/// `pfnSetCommandListErrorCb` sits **one field below** `pfnSetErrorCb` in
/// `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062` — the same struct
/// `queue::set_command_list_ddi_table` already reads `pfnSetCommandListDDITableCb`
/// out of — and it takes the **runtime's** list handle,
/// `D3D12DDI_HRTCOMMANDLIST`, which is why `queue::CommandListState` carries one
/// beside `h_device`. `device12::set_command_list_error` holds the full
/// argument and the callback's three-HRESULT contract.
///
/// What the choice buys, and what every policy decision in this file now rests
/// on:
///
/// | callback | the runtime's response |
/// |---|---|
/// | `pfnSetErrorCb` | *"Removing device due to bad UMD error"* — the whole `ID3D12Device`, with every list, queue, PSO, heap and resource on it, and the compositor if the device is DWM's |
/// | `pfnSetCommandListErrorCb` | *"the runtime will drop all calls into the driver which record commands on the specified command list"* (`tmp/dx12/specs/d3d/CPUEfficiency.md:2143-2158`) — one list is quarantined and the application learns at `Close()` |
///
/// ⇒ the second is D3D12's **existing** recording-error contract, not a stricter
/// reading of the first. Answering a bad copy argument by removing the device
/// was never the conservative choice; it was a different and much worse one.
///
/// ⚠ `device12::command_list_error_code` narrows the HRESULT to one of the three
/// the callback accepts, so the `E_INVALIDARG` and `E_NOTIMPL` this file passes
/// both arrive as `D3DDDIERR_APPLICATIONERROR`. The distinguishing detail is
/// therefore **only** in the budgeted log line at the call site, which is why
/// every reporting arm here has one.
///
/// ⚠ That last clause was FALSE when first written — `pfnCopyTiles` reported
/// with no line at all, so its `D3DDDIERR_APPLICATIONERROR` reached the runtime
/// carrying nothing a reader could attribute to a slot. It has one now. ⛔ The
/// rule the near-miss establishes: an invariant asserted in a doc that nothing
/// checks is a claim with a half-life, so a reporting arm added to this file
/// must add its line in the same edit.
///
/// ⚠ Its counters are **this lane's**. `cmdlist.rs::report_error`,
/// `pso.rs::set_error_if_possible` and `descriptors.rs::report_error` are the
/// same function against those lanes' sets, which is the established per-lane
/// pattern (`PARALLEL.md` §9.1) rather than duplication to be folded away.
///
/// ⚠ **The threshold moved with the channel.** `pfnCopyTiles` and
/// `pfnAtomicCopyBufferRegion` were counted-but-silent on the reasoning that
/// *"removing the device would not fix a capability gap"* — true, and no longer
/// the alternative. Both now report: a recording call that provably did not
/// happen is exactly what a quarantined list is for, and silence there is the
/// fake success CLAUDE.md forbids. See each slot's doc.
///
/// # Safety
/// `state` must be the live list state the caller resolved for this DDI call.
unsafe fn report_error(state: &CommandListState, hr: Hresult) {
    // SAFETY: `h_device()` is the handle `create_command_list` recorded for this
    // list, and the runtime cannot destroy a device while one of its lists is
    // inside a recording DDI. The borrow does not outlive this call.
    let Some(dev) = (unsafe { device12::device(state.h_device()) }) else {
        note_refusal(&L3C_REFUSALS.set_error_no_device);
        return;
    };
    if !device12::set_command_list_error(dev, state.h_rt_list(), hr) {
        note_refusal(&L3C_REFUSALS.set_error_cb_absent);
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Borrow a live `ID3D12Resource` into the `ManuallyDrop<Option<…>>` an API
/// descriptor field wants, **without touching its reference count**.
///
/// ⚠ The result must not outlive `resource`. That holds by construction at every
/// call site: it is only ever written into a descriptor local that is built,
/// passed to one engine call and dropped inside the same function. Same shape,
/// and the same reasoning, as `pso.rs`'s `pRootSignature` field.
fn borrowed(resource: &ID3D12Resource) -> ManuallyDrop<Option<ID3D12Resource>> {
    // SAFETY: `as_raw` hands back the live, non-null pointer the caller's shared
    // borrow names; `from_raw` adopts it without an `AddRef`; `ManuallyDrop` is
    // what stops the descriptor from issuing the matching `Release` for a
    // reference it never took. That pairing is `bridge12.rs`'s owned-vs-borrowed
    // rule applied to a struct field.
    ManuallyDrop::new(Some(unsafe { ID3D12Resource::from_raw(resource.as_raw()) }))
}

/// Whether `format` is one of the DXGI block-compressed formats, i.e. one whose
/// texel dimensions and block dimensions differ by a factor of four.
///
/// ⚠ Two ranges rather than 21 named constants, and the assumption that says so
/// is *contiguity inside each range* — BC1..BC5 and BC6H..BC7 are each a solid
/// block in `DXGI_FORMAT`, unchanged since D3D11. The endpoints come from the
/// `windows` crate's generated constants and their ordering is asserted above,
/// so a renumber is a compile error rather than a wrong answer.
fn is_block_compressed(format: ddi12::DXGI_FORMAT) -> bool {
    (DXGI_FORMAT_BC1_TYPELESS.0..=DXGI_FORMAT_BC5_SNORM.0).contains(&format)
        || (DXGI_FORMAT_BC6H_TYPELESS.0..=DXGI_FORMAT_BC7_UNORM_SRGB.0).contains(&format)
}

/// The factor between a dimension stated in **blocks** and the same dimension in
/// **texels**, for `format`.
///
/// ⛔ `(4, 4, 1)` for the whole BC family and `(1, 1, 1)` for everything else,
/// and that is exact rather than a convention. vkd3d's own format table gives a
/// `block_width` / `block_height` of 4 to exactly **21** entries — BC1..BC7, the
/// same 21 [`is_block_compressed`] classifies — and 1 to all 75 others
/// (`vkd3d-proton-helios/libs/vkd3d/utils.c`, the `vkd3d_formats[]` rows). No
/// DXGI format has a block depth above 1, which is why the third element is
/// always 1 and the `Depth` field needs no scaling at all.
fn block_extent(format: ddi12::DXGI_FORMAT) -> (u32, u32, u32) {
    if is_block_compressed(format) {
        (4, 4, 1)
    } else {
        (1, 1, 1)
    }
}

/// Whether a pitched layout's `SlicePitch` survives the translation to
/// `D3D12_SUBRESOURCE_FOOTPRINT`.
///
/// ⛔ **Both DDI pitched layouts carry a `SlicePitch` and the API footprint has
/// nowhere to put it.** `D3D12DDIARG_PHYSICAL_SUBRESOURCE_PITCHED_LAYOUT` ends
/// `{ Pitch, SlicePitch }` (bindgen offsets 16 and 20) and
/// `D3D12DDIARG_VIRTUAL_SUBRESOURCE_PITCHED_LAYOUT` likewise (28 and 32), while
/// `D3D12_SUBRESOURCE_FOOTPRINT` is `{ Format, Width, Height, Depth, RowPitch }`
/// and nothing else. So the field is **necessarily** dropped by
/// [`texture_copy_location`], and D3D12 derives the distance between depth
/// slices instead. This is the test that says whether that derivation can
/// reproduce what the runtime actually sent.
///
/// ⭐ The comparison is `SlicePitch == Pitch * PhysicalHeight`, and it is
/// **format-independent by construction**: all three are memory quantities —
/// bytes per block row, block rows in memory, bytes per slice. It is also
/// exactly the engine's own derivation, which is the reason it is the right
/// comparison rather than a plausible one: `vk_buffer_image_copy_from_d3d12`
/// computes `row_count = Footprint.Height / block_height` and strides by
/// `row_count * RowPitch` (`libs/vkd3d/command.c:9631-9645`), and
/// [`texture_copy_location`] writes `Footprint.Height = PhysicalHeight *
/// block_height`, so `row_count` is `PhysicalHeight` for **every** format, BC or
/// not. Both structs carry `PhysicalHeight` / `PhysicalDepth`, including the
/// virtual one, which is why one helper serves both arms.
///
/// ⚠ `PhysicalDepth <= 1` is representable by definition: with a single slice
/// there is no second offset for a slice pitch to move, so the field is unused
/// rather than lost. That is what keeps this from firing on the 2-D traffic that
/// is nearly all of it.
///
/// ⚠ **A `false` now refuses the copy**, where an earlier revision counted it
/// and forwarded anyway. That revision's stated reason was that *"removing the
/// device on an unmeasured case would be the guess"* — which described
/// `pfnSetErrorCb` and is no longer the alternative ([`report_error`]). The
/// principle that separates this from the block-dimension question next door is
/// **expressibility**: block dimensions are wrong but *computable*, so
/// [`texture_copy_location`] corrects them; a divergent slice pitch has no field
/// in `D3D12_SUBRESOURCE_FOOTPRINT` to carry it at all, so the only honest
/// answers are refuse or corrupt slices >= 1. ⚠ The condition should be
/// unreachable — the public `D3D12_PLACED_SUBRESOURCE_FOOTPRINT` the runtime
/// itself computes has no slice pitch, so its layouts can only ever be
/// `RowPitch * rows` — which is what makes refusing cheap; see
/// [`L3cRefusals::copy_slice_pitch_unrepresentable`].
fn slice_pitch_representable(
    pitch: u32,
    physical_height: u32,
    physical_depth: u32,
    slice: u32,
) -> bool {
    if physical_depth <= 1 {
        return true;
    }
    // ⚠ `u64` on both sides: `Pitch * PhysicalHeight` overflows `u32` at a 64 KB
    // row and a 64 Ki-row surface, and a wrapped product would compare equal to
    // the wrong thing. No `as`, no panic.
    u64::from(slice) == u64::from(pitch) * u64::from(physical_height)
}

/// Resolve a `D3D12DDI_HRESOURCE` that the DDI requires to be present.
///
/// `None` covers both a null private word and a live-looking handle whose slot
/// is empty; neither can be forwarded, and neither is distinguishable here.
/// Callers that have a legal null case test `pDrvPrivate` themselves first.
///
/// # Safety
/// As `resource12::engine_resource`: `h` must be a resource handle this driver
/// created, and the returned reference must not outlive the DDI call.
unsafe fn resource_of<'a>(h: ddi12::D3D12DDI_HRESOURCE) -> Option<&'a ID3D12Resource> {
    // SAFETY: forwarded unchanged; the caller's guarantee is this function's.
    unsafe { resource12::engine_resource(h) }
}

/// Translate a `D3D12DDI_BOX` into the API's `D3D12_BOX`.
///
/// ⛔ **Not a cast.** The DDI's six members are `LONG` (signed) where the API's
/// are `UINT`, so a negative coordinate — which the API cannot express and this
/// driver must not launder into a 4-billion-pixel extent — is refused here
/// rather than reinterpreted.
fn api_box(b: &ddi12::D3D12DDI_BOX) -> Option<D3D12_BOX> {
    let fields = [b.Left, b.Top, b.Front, b.Right, b.Bottom, b.Back];
    if fields.iter().any(|v| *v < 0) {
        return None;
    }
    Some(D3D12_BOX {
        left: b.Left as u32,
        top: b.Top as u32,
        front: b.Front as u32,
        right: b.Right as u32,
        bottom: b.Bottom as u32,
        back: b.Back as u32,
    })
}

/// Why a `D3D12DDIARG_PLACED_RESOURCE` could not become an API copy location.
enum LocationRefusal {
    /// The `D3D12DDI_HRESOURCE` did not resolve to an engine resource.
    NoResource,
    /// A pitched layout arm arrived with a null `pLayout`.
    NullLayout,
    /// `Layout` was `RL_UNDEFINED` — which `d3d12umddi.h:1541` states it *cannot
    /// be* — or a value this header does not name.
    UnknownLayout(ddi12::D3D12DDI_RESOURCE_LAYOUT),
    /// `RL_SELECT_SUBRESOURCE` carried a subresource index above `UINT_MAX`.
    SubresourceOutOfRange(u64),
    /// A `Physical*` block dimension times its block extent does not fit in the
    /// `UINT` texel count `D3D12_SUBRESOURCE_FOOTPRINT` wants. ⚠ Unreachable for
    /// any real texture — D3D12's maximum dimension is 16384 texels — so this
    /// exists to keep the conversion total without a wrapping `as`.
    BlockDimensionsOverflow,
    /// A pitched layout's `SlicePitch` is not the `Pitch * PhysicalHeight` the
    /// API footprint can express. Carries every number the log line needs,
    /// because nothing downstream can recover them.
    SlicePitchUnrepresentable {
        arm: &'static str,
        pitch: u32,
        physical_height: u32,
        physical_depth: u32,
        slice: u32,
    },
}

/// Build one API `D3D12_TEXTURE_COPY_LOCATION` out of the DDI's two arguments.
///
/// See the module doc for where the `RL_SELECT_SUBRESOURCE` rule comes from.
///
/// # ⛔ `RL_PLACED_PHYSICAL_SUBRESOURCE_PITCHED` converts BLOCKS to TEXELS
///
/// An earlier revision forwarded `Physical{Width,Height,Depth}` into
/// `D3D12_SUBRESOURCE_FOOTPRINT::{Width,Height,Depth}` unchanged and called the
/// units unsettleable on the Linux host. **The SDK header in this tree settles
/// them, against that choice.** `D3D12DDIARG_VIRTUAL_SUBRESOURCE_PITCHED_LAYOUT`
/// carries **both** triples in one struct (`d3d12umddi.h:1556-1568`) with
/// `// Block dimensions` on the `Physical*` one only — so `Physical*` cannot be
/// texels without making `Virtual*` a duplicate field — while
/// `D3D12_SUBRESOURCE_FOOTPRINT` is in texels. The VIRTUAL arm below already
/// acted on that reading; the two arms could not both be right.
///
/// The engine confirms it independently: `vk_buffer_image_copy_from_d3d12`
/// divides `Footprint.Height` by `src_format->block_height`
/// (`libs/vkd3d/command.c:9631-9645`) and clamps `imageExtent` against
/// `Footprint.{Width,Height,Depth}` as texel extents. A 64x64 BC7 subresource
/// arriving as 16x16 blocks therefore became `row_count = 4` against a 16-texel
/// extent — a quarter of the rows copied into a sixteenth of the area, success
/// returned, frame wrong.
///
/// ⭐ So the three fields are multiplied by [`block_extent`] — 4x4x1 for every BC
/// format, 1x1x1 otherwise. ⚠ For a texture whose texel size is not a multiple
/// of the block size the product **over-states** the extent (a 65-texel-wide BC7
/// mip is 17 blocks, i.e. 68), and that is harmless rather than tolerated: the
/// engine takes `min(subresource extent, Footprint.Width)`, so the true 65 wins,
/// while the row arithmetic that actually needs the number — `row_count =
/// 68 / 4 = 17` — gets exactly the 17 block rows the memory holds. The
/// unmultiplied value got neither right.
///
/// ⚠ **`SlicePitch` is dropped by both pitched arms**, and that one is forced
/// rather than chosen: `D3D12_SUBRESOURCE_FOOTPRINT` has no member for it, so
/// D3D12 derives the distance between depth slices from `RowPitch`. Where the
/// runtime's value and that derivation disagree the layout is **not
/// expressible**, and the location is refused rather than forwarded — see
/// [`slice_pitch_representable`] for why that is the opposite answer from the
/// block-dimension one and why both are right.
///
/// # Safety
/// `placement` must be non-null and point at a live `D3D12DDIARG_BUFFER_PLACEMENT`,
/// and `placed.pLayout`, when the layout names one, must point at the layout
/// struct that layout value selects.
unsafe fn texture_copy_location(
    placement: *const ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
    placed: &ddi12::D3D12DDIARG_PLACED_RESOURCE,
) -> Result<D3D12_TEXTURE_COPY_LOCATION, LocationRefusal> {
    // SAFETY: non-null per this function's contract; the DDI declares it
    // `_In_ CONST`.
    let base = unsafe { (*placement).BaseAddress.UMD };
    // SAFETY: the runtime handed this handle in this call, so it is live.
    let Some(resource) = (unsafe { resource_of(base.hResource) }) else {
        return Err(LocationRefusal::NoResource);
    };

    match placed.Layout {
        ddi12::D3D12DDI_RESOURCE_LAYOUT_D3D12DDI_RL_SELECT_SUBRESOURCE => {
            // ⭐ `pLayout` is unused here and `Offset` IS the subresource index.
            let Ok(index) = u32::try_from(base.Offset) else {
                return Err(LocationRefusal::SubresourceOutOfRange(base.Offset));
            };
            Ok(D3D12_TEXTURE_COPY_LOCATION {
                pResource: borrowed(resource),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: index,
                },
            })
        }
        ddi12::D3D12DDI_RESOURCE_LAYOUT_D3D12DDI_RL_PLACED_PHYSICAL_SUBRESOURCE_PITCHED => {
            if placed.pLayout.is_null() {
                return Err(LocationRefusal::NullLayout);
            }
            // SAFETY: non-null per the check, and the layout discriminant says
            // it points at this struct.
            let l = unsafe {
                &*placed
                    .pLayout
                    .cast::<ddi12::D3D12DDIARG_PHYSICAL_SUBRESOURCE_PITCHED_LAYOUT>()
            };
            // ⛔ Blocks -> texels; see this function's doc for the header
            // pairing that settles the units and the engine arithmetic that
            // confirms them. ⚠ `Pitch` is NOT scaled: it is bytes per block row
            // on both sides, which is what `D3D12_SUBRESOURCE_FOOTPRINT::RowPitch`
            // means for a BC format.
            let (bw, bh, bd) = block_extent(l.Format);
            // ⚠ Counted off the extent rather than off `is_block_compressed`, so
            // the counter means exactly "a scaling happened here" and cannot
            // drift from the arithmetic below it.
            if (bw, bh, bd) != (1, 1, 1) {
                L3C_REFUSALS.copy_texture_block_physical_layout.bump();
            }
            // ⚠ `checked_mul`, not `as` and not `saturating_mul`: a saturated
            // extent would be a silently wrong footprint, which is the class of
            // bug this whole arm was just repaired for.
            let (Some(width), Some(height), Some(depth)) = (
                l.PhysicalWidth.checked_mul(bw),
                l.PhysicalHeight.checked_mul(bh),
                l.PhysicalDepth.checked_mul(bd),
            ) else {
                return Err(LocationRefusal::BlockDimensionsOverflow);
            };
            if !slice_pitch_representable(l.Pitch, l.PhysicalHeight, l.PhysicalDepth, l.SlicePitch)
            {
                return Err(LocationRefusal::SlicePitchUnrepresentable {
                    arm: "PHYSICAL",
                    pitch: l.Pitch,
                    physical_height: l.PhysicalHeight,
                    physical_depth: l.PhysicalDepth,
                    slice: l.SlicePitch,
                });
            }
            Ok(D3D12_TEXTURE_COPY_LOCATION {
                pResource: borrowed(resource),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: base.Offset,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT(l.Format),
                            Width: width,
                            Height: height,
                            Depth: depth,
                            RowPitch: l.Pitch,
                        },
                    },
                },
            })
        }
        ddi12::D3D12DDI_RESOURCE_LAYOUT_D3D12DDI_RL_PLACED_VIRTUAL_SUBRESOURCE_PITCHED => {
            if placed.pLayout.is_null() {
                return Err(LocationRefusal::NullLayout);
            }
            // SAFETY: non-null per the check, and the layout discriminant says
            // it points at this struct.
            let l = unsafe {
                &*placed
                    .pLayout
                    .cast::<ddi12::D3D12DDIARG_VIRTUAL_SUBRESOURCE_PITCHED_LAYOUT>()
            };
            // ⭐ The virtual dimensions, not the physical ones: the API's
            // footprint is in texels, and this struct carrying BOTH triples —
            // `Virtual*` unlabelled, `Physical*` labelled "Block dimensions"
            // (`d3d12umddi.h:1556-1568`) — is precisely what says which is
            // which. That same pairing is what the PHYSICAL arm above now
            // converts on; no scaling is needed here because `Virtual*` is
            // already the texel triple.
            // ⚠ The `Physical*` triple is therefore dropped on purpose, and the
            // only piece of it that can move data is its interaction with
            // `SlicePitch`, which is what the test below settles.
            if !slice_pitch_representable(l.Pitch, l.PhysicalHeight, l.PhysicalDepth, l.SlicePitch)
            {
                return Err(LocationRefusal::SlicePitchUnrepresentable {
                    arm: "VIRTUAL",
                    pitch: l.Pitch,
                    physical_height: l.PhysicalHeight,
                    physical_depth: l.PhysicalDepth,
                    slice: l.SlicePitch,
                });
            }
            Ok(D3D12_TEXTURE_COPY_LOCATION {
                pResource: borrowed(resource),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: base.Offset,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT(l.Format),
                            Width: l.VirtualWidth,
                            Height: l.VirtualHeight,
                            Depth: l.VirtualDepth,
                            RowPitch: l.Pitch,
                        },
                    },
                },
            })
        }
        other => Err(LocationRefusal::UnknownLayout(other)),
    }
}

/// Count, log and report one [`LocationRefusal`], once per copy.
///
/// # Safety
/// `state` must be the live list state the caller resolved for this DDI call.
unsafe fn refuse_location(state: &CommandListState, role: &str, why: &LocationRefusal) {
    let (counter, detail) = match why {
        LocationRefusal::NoResource => (
            &L3C_REFUSALS.copy_resource_missing,
            "its D3D12DDI_HRESOURCE carries no engine resource".to_string(),
        ),
        LocationRefusal::NullLayout => (
            &L3C_REFUSALS.copy_bad_arg,
            "a pitched layout arrived with a null pLayout".to_string(),
        ),
        LocationRefusal::UnknownLayout(l) => (
            &L3C_REFUSALS.copy_layout_unknown,
            format!("D3D12DDI_RESOURCE_LAYOUT {l} is not one this header names"),
        ),
        LocationRefusal::SubresourceOutOfRange(o) => (
            &L3C_REFUSALS.copy_bad_arg,
            format!("RL_SELECT_SUBRESOURCE carries Offset {o}, which is not a subresource index"),
        ),
        LocationRefusal::BlockDimensionsOverflow => (
            &L3C_REFUSALS.copy_bad_arg,
            "a PHYSICAL pitched layout's block dimensions do not fit in a UINT texel count \
             once multiplied by the format's block extent"
                .to_string(),
        ),
        LocationRefusal::SlicePitchUnrepresentable {
            arm,
            pitch,
            physical_height,
            physical_depth,
            slice,
        } => (
            &L3C_REFUSALS.copy_slice_pitch_unrepresentable,
            format!(
                "the {arm} pitched layout carries SlicePitch={slice} but Pitch={pitch} * \
                 PhysicalHeight={physical_height} is {}, and D3D12_SUBRESOURCE_FOOTPRINT has no \
                 slice-pitch member to carry the difference -- every one of the {physical_depth} \
                 depth slices after the first would be read at the derived offset",
                u64::from(*pitch) * u64::from(*physical_height),
            ),
        ),
    };
    counter.bump();
    if let Some(n) = budget(&COPY_LOG) {
        log_error!(
            "CopyTextureRegion: the {role} location is unusable -- {detail} (x{})",
            n + 1
        );
    }
    // SAFETY: `state` is the live list state the caller resolved for this call.
    unsafe { report_error(state, E_INVALIDARG) };
}

// ---------------------------------------------------------------------------
// Copy / resolve — 7 slots
// ---------------------------------------------------------------------------

/// `pfnCopyTextureRegion` -> `ID3D12GraphicsCommandList::CopyTextureRegion`.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`; both
/// placement pointers, when non-null, must point at live
/// `D3D12DDIARG_BUFFER_PLACEMENT`s; `src_box`, when non-null, at a live
/// `D3D12DDI_BOX`; and each `D3D12DDIARG_PLACED_RESOURCE::pLayout` at the struct
/// its `Layout` selects.
unsafe extern "C" fn copy_texture_region(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    dst_placement: *const ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
    dst_resource: ddi12::D3D12DDIARG_PLACED_RESOURCE,
    dst_x: ddi12::UINT,
    dst_y: ddi12::UINT,
    dst_z: ddi12::UINT,
    src_placement: *const ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
    src_resource: ddi12::D3D12DDIARG_PLACED_RESOURCE,
    src_box: *const ddi12::D3D12DDI_BOX,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3C_REFUSALS.command_list_missing);
        return;
    };
    // ⛔ Validate the runtime-supplied pointers BEFORE reading through them, per
    // arm. The DDI declares both `_In_ CONST` and never optional.
    if dst_placement.is_null() || src_placement.is_null() {
        L3C_REFUSALS.copy_bad_arg.bump();
        if let Some(n) = budget(&COPY_LOG) {
            log_error!(
                "CopyTextureRegion: pDstPlacement={dst_placement:p} pSrcPlacement={src_placement:p} \
                 -- the DDI declares both _In_ CONST (x{})",
                n + 1,
            );
        }
        // SAFETY: `state` is the live list state this call resolved.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    }

    // SAFETY: both pointers are non-null per the check and the caller guarantees
    // the layout pointers match their discriminants.
    let dst = match unsafe { texture_copy_location(dst_placement, &dst_resource) } {
        Ok(l) => l,
        Err(why) => {
            // SAFETY: `state` is the live list state resolved above.
            unsafe { refuse_location(state, "destination", &why) };
            return;
        }
    };
    // SAFETY: as above.
    let src = match unsafe { texture_copy_location(src_placement, &src_resource) } {
        Ok(l) => l,
        Err(why) => {
            // SAFETY: as above.
            unsafe { refuse_location(state, "source", &why) };
            return;
        }
    };

    let api_src_box = if src_box.is_null() {
        None
    } else {
        // SAFETY: non-null per the check; the DDI declares it `_In_opt_ CONST`.
        match api_box(unsafe { &*src_box }) {
            Some(b) => Some(b),
            None => {
                L3C_REFUSALS.copy_box_negative.bump();
                if let Some(n) = budget(&COPY_LOG) {
                    log_error!(
                        "CopyTextureRegion: pSrcBox carries a negative coordinate, which \
                         D3D12_BOX cannot express (x{})",
                        n + 1,
                    );
                }
                // SAFETY: `state` is the live list state resolved above.
                unsafe { report_error(state, E_INVALIDARG) };
                return;
            }
        }
    };

    trace_line!(
        "CopyTextureRegion: dstLayout={} srcLayout={} dst=({dst_x},{dst_y},{dst_z}) box={}",
        dst_resource.Layout,
        src_resource.Layout,
        u8::from(api_src_box.is_some()),
    );

    // SAFETY: `engine()` borrows the list this box owns; both locations are live
    // locals whose resource fields borrow references the resource states own for
    // longer than this call, and `api_src_box` is a live local or `None`.
    unsafe {
        state.engine().CopyTextureRegion(
            &dst,
            dst_x,
            dst_y,
            dst_z,
            &src,
            api_src_box.as_ref().map(|b| b as *const D3D12_BOX),
        );
    }
}

/// `pfnResourceCopy` -> `ID3D12GraphicsCommandList::CopyResource`.
///
/// # Safety
/// `h_list` must be a live command-list handle and both resource handles live
/// handles this driver created.
unsafe extern "C" fn resource_copy(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_dst: ddi12::D3D12DDI_HRESOURCE,
    h_src: ddi12::D3D12DDI_HRESOURCE,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3C_REFUSALS.command_list_missing);
        return;
    };
    // SAFETY: the runtime handed both handles in this call, so both are live.
    let (Some(dst), Some(src)) = (unsafe { resource_of(h_dst) }, unsafe { resource_of(h_src) })
    else {
        L3C_REFUSALS.copy_resource_missing.bump();
        if let Some(n) = budget(&COPY_LOG) {
            log_error!(
                "ResourceCopy: dst={:p} src={:p} -- one of them carries no engine resource (x{})",
                h_dst.pDrvPrivate,
                h_src.pDrvPrivate,
                n + 1,
            );
        }
        // SAFETY: `state` is the live list state this call resolved.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    trace_line!(
        "ResourceCopy: dst={:p} src={:p}",
        h_dst.pDrvPrivate,
        h_src.pDrvPrivate,
    );
    // SAFETY: `engine()` borrows the list this box owns; both arguments are
    // borrowed references the resource states own for longer than this call.
    unsafe { state.engine().CopyResource(dst, src) };
}

/// `pfnCopyTiles` — **REFUSED**, `L3cCopyTilesRefused`.
///
/// ⛔ This driver reports `TiledResourcesTier = NOT_SUPPORTED`
/// (`caps12.rs:278`), so no tiled resource can exist for this DDI to copy
/// through. Refusing is the coherent answer.
///
/// ⭐ **And it is now reported, which it was not.** The earlier revision counted
/// this silently, reasoning that *"a hit means a caps inconsistency somewhere
/// else and removing the device would not fix it"*. The first half stands; the
/// second described `pfnSetErrorCb`, which is no longer the channel — see
/// [`report_error`]. Whoever's fault the call is, the tile copy **did not
/// happen**, and a `VOID` return that says nothing is the fake success CLAUDE.md
/// forbids. Quarantining the one list that recorded it is the proportionate
/// answer and hands the application a failing `Close()`.
///
/// ⚠ Still no log line: the counter plus the summary `note_refusal` emits on its
/// first hit is the readout, and this DDI is per-region traffic that a budgeted
/// line would only half cover.
///
/// ⚠ **`DX12.md` §4.4 makes `TiledResourcesTier >= 2` a feature-level 12_1
/// floor**, and it lands with this slot plus `pfnUpdateTileMappings`,
/// `pfnCopyTileMappings`, `pfnGetMipPacking` and the reserved-resource arm of
/// `pfnCreateHeapAndResource`. ⛔ The tier is **UMD-only** — Vulkan sparse
/// binding, which the guest supports end to end (`DECISIONS.md` §2) — and **not**
/// a KMD dependency. That claim was made twice and falsified twice; do not cost
/// the feature level as if the KMD were on its critical path.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`; the
/// remaining arguments are the runtime's and this body reads none of them.
unsafe extern "C" fn copy_tiles(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    _h_resource: ddi12::D3D12DDI_HRESOURCE,
    _region_start_coord: *const ddi12::D3D12DDI_TILED_RESOURCE_COORDINATE,
    _region_size: *const ddi12::D3D12DDI_TILE_REGION_SIZE,
    _h_buffer: ddi12::D3D12DDI_HRESOURCE,
    _buffer_start_offset_in_bytes: ddi12::UINT64,
    _flags: ddi12::D3D12DDI_TILE_COPY_FLAGS,
) {
    L3C_REFUSALS.copy_tiles_refused.bump();
    // ⛔ A budgeted line, and it is not decoration: `report_error` narrows every
    // HRESULT this file passes to `D3DDDIERR_APPLICATIONERROR`, so the log line
    // at the call site is the ONLY place the reason survives. This slot was the
    // one reporting arm in the file without one, which made `report_error`'s own
    // doc — *"every reporting arm here has one"* — false.
    //
    // ⚠ `bump`, not `note_refusal`, now that the line exists: R911, an
    // already-loud arm must not also print the whole refusal set.
    if let Some(n) = budget(&COPY_LOG) {
        log_error!(
            "CopyTiles: refused -- this driver reports TiledResourcesTier = NOT_SUPPORTED, so no \
             tiled resource can exist for this DDI to copy (x{})",
            n + 1,
        );
    }
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3C_REFUSALS.command_list_missing);
        return;
    };
    // SAFETY: `state` is the live list state resolved above.
    unsafe { report_error(state, E_INVALIDARG) };
}

/// The two buffer-region slots' shared body: resolve both placements, then hand
/// the pieces to `op`.
///
/// # Safety
/// `h_list` must be a live command-list handle and both placements' resource
/// handles live handles this driver created.
unsafe fn buffer_region(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    name: &str,
    dst: ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
    src: ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
    op: impl FnOnce(&CommandListState, &ID3D12Resource, u64, &ID3D12Resource, u64),
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3C_REFUSALS.command_list_missing);
        return;
    };
    // SAFETY: `BaseAddress` is a union with exactly one member, so reading `UMD`
    // is the only possible read and needs no discriminant.
    let (dst_base, src_base) = unsafe { (dst.BaseAddress.UMD, src.BaseAddress.UMD) };
    // SAFETY: the runtime handed both handles in this call, so both are live.
    let (Some(dst_res), Some(src_res)) = (unsafe { resource_of(dst_base.hResource) }, unsafe {
        resource_of(src_base.hResource)
    }) else {
        L3C_REFUSALS.copy_resource_missing.bump();
        if let Some(n) = budget(&COPY_LOG) {
            log_error!(
                "{name}: dst={:p} src={:p} -- one of them carries no engine resource (x{})",
                dst_base.hResource.pDrvPrivate,
                src_base.hResource.pDrvPrivate,
                n + 1,
            );
        }
        // SAFETY: `state` is the live list state this call resolved.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    op(state, dst_res, dst_base.Offset, src_res, src_base.Offset);
}

/// `pfnCopyBufferRegion` -> `ID3D12GraphicsCommandList::CopyBufferRegion`.
///
/// ⚠ The DDI's `SrcBytes` is the API's `NumBytes`; the two offsets ride inside
/// the placements rather than being separate arguments.
///
/// # Safety
/// As [`buffer_region`].
unsafe extern "C" fn copy_buffer_region(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    dst: ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
    src: ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
    src_bytes: ddi12::UINT64,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is `buffer_region`'s.
    unsafe {
        buffer_region(
            h_list,
            "CopyBufferRegion",
            dst,
            src,
            |state, dst_res, dst_off, src_res, src_off| {
                trace_line!(
                    "CopyBufferRegion: dstOffset={dst_off} srcOffset={src_off} bytes={src_bytes}"
                );
                // SAFETY: `engine()` borrows the list this box owns and both
                // resources are borrowed references live for this call. The
                // enclosing `unsafe` block is this closure's, so no second one
                // is written here.
                state
                    .engine()
                    .CopyBufferRegion(dst_res, dst_off, src_res, src_off, src_bytes);
            },
        );
    }
}

/// `pfnAtomicCopyBufferRegion` — **REFUSED**, `L3cAtomicCopyRefused`.
///
/// # ⛔ It shares a typedef with `pfnCopyBufferRegion` and is not the same
/// operation
///
/// Both slots are `PFND3D12DDI_COPYBUFFERREGION_0003` (`d3d12umddi.h:13361` vs
/// `:13319`), which makes forwarding this one to `CopyBufferRegion` a one-line
/// change and a silent correctness bug. The API counterpart is
/// `ID3D12GraphicsCommandList1::AtomicCopyBufferUINT` /
/// `AtomicCopyBufferUINT64` — the cross-adapter atomic copy, whose whole content
/// is the *atomicity* and the dependent-resource list, and whose DDI partner is
/// the `D3D12DDI_RESOURCE_BARRIER_FLAG_0022_ATOMIC_COPY` barrier flag. A plain
/// `CopyBufferRegion` provides neither.
///
/// ⛔ **And the engine does not implement it.** Both API methods are `FIXME`
/// stubs with empty bodies in this pinned vkd3d
/// (`vkd3d-proton-helios/libs/vkd3d/command.c:18021-18032` and `:18034`), so
/// forwarding would be fake success in the most literal sense — a call that
/// returns having copied nothing. ⚠ Two further pieces are missing even if the
/// engine grew a body: the DDI carries no `Dependencies` array, and it carries
/// no UINT-vs-UINT64 selector (only `SrcBytes`, which would have to be *read as*
/// one).
///
/// ⭐ **Counted, logged AND reported.** The earlier revision stopped at counted,
/// on the reasoning that *"this is a capability this stack does not have rather
/// than a malformed call, and removing the device would not fix it"* — which
/// described `pfnSetErrorCb`, no longer the channel ([`report_error`]). This is
/// the sharpest case in the file for reporting: the engine's body is empty, so
/// the atomic copy provably did **not** happen, and a silent `VOID` return
/// leaves the application reading whatever was in the destination before. One
/// quarantined list and a failing `Close()` is the honest answer.
///
/// # Safety
/// `h_list` must be a live handle from `queue::create_command_list`; the
/// remaining arguments are the runtime's and this body reads none of them.
unsafe extern "C" fn atomic_copy_buffer_region(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    _dst: ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
    _src: ddi12::D3D12DDIARG_BUFFER_PLACEMENT,
    _src_bytes: ddi12::UINT64,
) {
    // ⚠ `bump` plus a budgeted line, never `note_refusal`: R911's rule is that an
    // arm which already logs must not also print the whole set summary, or one
    // event produces two records.
    L3C_REFUSALS.atomic_copy_refused.bump();
    if let Some(n) = budget(&COPY_LOG) {
        log_error!(
            "AtomicCopyBufferRegion: REFUSED -- vkd3d's AtomicCopyBufferUINT/UINT64 are empty \
             FIXME stubs (libs/vkd3d/command.c:18021), so forwarding would report success \
             having copied nothing; the DDI also carries no dependency list and no \
             UINT-vs-UINT64 selector (x{})",
            n + 1,
        );
    }
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3C_REFUSALS.command_list_missing);
        return;
    };
    // SAFETY: `state` is the live list state resolved above.
    unsafe { report_error(state, E_INVALIDARG) };
}

/// `pfnResourceResolveSubresource` -> `ID3D12GraphicsCommandList::ResolveSubresource`.
///
/// # Safety
/// `h_list` must be a live command-list handle and both resource handles live
/// handles this driver created.
unsafe extern "C" fn resource_resolve_subresource(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_dst: ddi12::D3D12DDI_HRESOURCE,
    dst_subresource: ddi12::UINT,
    h_src: ddi12::D3D12DDI_HRESOURCE,
    src_subresource: ddi12::UINT,
    format: ddi12::DXGI_FORMAT,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3C_REFUSALS.command_list_missing);
        return;
    };
    // SAFETY: the runtime handed both handles in this call, so both are live.
    let (Some(dst), Some(src)) = (unsafe { resource_of(h_dst) }, unsafe { resource_of(h_src) })
    else {
        L3C_REFUSALS.copy_resource_missing.bump();
        if let Some(n) = budget(&COPY_LOG) {
            log_error!(
                "ResourceResolveSubresource: dst={:p} src={:p} -- one of them carries no engine \
                 resource (x{})",
                h_dst.pDrvPrivate,
                h_src.pDrvPrivate,
                n + 1,
            );
        }
        // SAFETY: `state` is the live list state this call resolved.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    trace_line!(
        "ResourceResolveSubresource: dstSub={dst_subresource} srcSub={src_subresource} \
         format={format}"
    );
    // SAFETY: `engine()` borrows the list this box owns; both resources are
    // borrowed references live for this call.
    unsafe {
        state.engine().ResolveSubresource(
            dst,
            dst_subresource,
            src,
            src_subresource,
            DXGI_FORMAT(format),
        );
    }
}

/// Translate `D3D12DDI_RESOLVE_MODE` into the API enum.
///
/// ⛔ A match on named constants, never a cast: `DDI_REFERENCE.md` §9.6.1's scar
/// is a DDI enumerator and an API enumerator that collided on a value with
/// different meanings while the member types kept the compiler silent. The
/// value identity IS proved above; what the match adds is that a value neither
/// header names is refused instead of forwarded.
fn api_resolve_mode(m: ddi12::D3D12DDI_RESOLVE_MODE) -> Option<D3D12_RESOLVE_MODE> {
    Some(match m {
        ddi12::D3D12DDI_RESOLVE_MODE_D3D12DDI_RESOLVE_MODE_DECOMPRESS => {
            D3D12_RESOLVE_MODE_DECOMPRESS
        }
        ddi12::D3D12DDI_RESOLVE_MODE_D3D12DDI_RESOLVE_MODE_MIN => D3D12_RESOLVE_MODE_MIN,
        ddi12::D3D12DDI_RESOLVE_MODE_D3D12DDI_RESOLVE_MODE_MAX => D3D12_RESOLVE_MODE_MAX,
        ddi12::D3D12DDI_RESOLVE_MODE_D3D12DDI_RESOLVE_MODE_AVERAGE => D3D12_RESOLVE_MODE_AVERAGE,
        ddi12::D3D12DDI_RESOLVE_MODE_D3D12DDI_RESOLVE_MODE_ENCODE_SAMPLER_FEEDBACK_0073 => {
            D3D12_RESOLVE_MODE_ENCODE_SAMPLER_FEEDBACK
        }
        ddi12::D3D12DDI_RESOLVE_MODE_D3D12DDI_RESOLVE_MODE_DECODE_SAMPLER_FEEDBACK_0073 => {
            D3D12_RESOLVE_MODE_DECODE_SAMPLER_FEEDBACK
        }
        _ => return None,
    })
}

/// `pfnResourceResolveSubresourceRegion` ->
/// `ID3D12GraphicsCommandList1::ResolveSubresourceRegion`.
///
/// ⚠ The one copy slot that needs a `QueryInterface`, because the API put the
/// region form on `ID3D12GraphicsCommandList1`. The cast is per call rather than
/// cached, for the same reason `resource12::engine_device10`'s is: caching it
/// would mean appending a field to `queue::CommandListState`, a file this lane
/// does not own (`PARALLEL.md` §4), for a GUID compare against an already-live
/// object on a DDI a workload reaches once per resolve.
///
/// # Safety
/// `h_list` must be a live command-list handle, both resource handles live
/// handles this driver created, and `src_rect`, when non-null, must point at a
/// live `D3D12DDI_RECT`.
unsafe extern "C" fn resource_resolve_subresource_region(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_dst: ddi12::D3D12DDI_HRESOURCE,
    dst_subresource: ddi12::UINT,
    dst_x: ddi12::UINT,
    dst_y: ddi12::UINT,
    h_src: ddi12::D3D12DDI_HRESOURCE,
    src_subresource: ddi12::UINT,
    src_rect: *mut ddi12::D3D12DDI_RECT,
    format: ddi12::DXGI_FORMAT,
    resolve_mode: ddi12::D3D12DDI_RESOLVE_MODE,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3C_REFUSALS.command_list_missing);
        return;
    };
    let Some(mode) = api_resolve_mode(resolve_mode) else {
        L3C_REFUSALS.resolve_mode_unsupported.bump();
        if let Some(n) = budget(&COPY_LOG) {
            log_error!(
                "ResourceResolveSubresourceRegion: D3D12DDI_RESOLVE_MODE {resolve_mode} is not \
                 named by this header (x{})",
                n + 1,
            );
        }
        // SAFETY: `state` is the live list state this call resolved.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    // SAFETY: the runtime handed both handles in this call, so both are live.
    let (Some(dst), Some(src)) = (unsafe { resource_of(h_dst) }, unsafe { resource_of(h_src) })
    else {
        L3C_REFUSALS.copy_resource_missing.bump();
        if let Some(n) = budget(&COPY_LOG) {
            log_error!(
                "ResourceResolveSubresourceRegion: dst={:p} src={:p} -- one of them carries no \
                 engine resource (x{})",
                h_dst.pDrvPrivate,
                h_src.pDrvPrivate,
                n + 1,
            );
        }
        // SAFETY: `state` is the live list state resolved above.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };

    let rect = if src_rect.is_null() {
        None
    } else {
        // SAFETY: non-null per the check; `D3D12DDI_RECT` is `RECT`, and the two
        // crates' `RECT`s are distinct Rust types over the same four `LONG`s, so
        // this rebuilds rather than transmutes.
        let r = unsafe { &*src_rect };
        Some(RECT {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        })
    };

    // `cast` is a `QueryInterface` on the live list this box owns and yields an
    // owned reference this function drops. Safe: the borrow is `engine()`'s.
    let list1 = match state.engine().cast::<ID3D12GraphicsCommandList1>() {
        Ok(l) => l,
        Err(err) => {
            L3C_REFUSALS.command_list1_unavailable.bump();
            if let Some(n) = budget(&COPY_LOG) {
                log_error!(
                    "ResourceResolveSubresourceRegion: QueryInterface for \
                     ID3D12GraphicsCommandList1 failed hr={:#010x} (x{})",
                    err.code().0 as u32,
                    n + 1,
                );
            }
            // SAFETY: `state` is the live list state resolved above.
            unsafe { report_error(state, E_NOTIMPL) };
            return;
        }
    };

    trace_line!(
        "ResourceResolveSubresourceRegion: dstSub={dst_subresource} dst=({dst_x},{dst_y}) \
         srcSub={src_subresource} format={format} mode={resolve_mode} rect={}",
        u8::from(rect.is_some()),
    );

    // SAFETY: `list1` is a live owned reference to the same command list; both
    // resources are borrowed references live for this call; `rect` is a live
    // local or `None`.
    unsafe {
        list1.ResolveSubresourceRegion(
            dst,
            dst_subresource,
            dst_x,
            dst_y,
            src,
            src_subresource,
            rect.as_ref().map(|r| r as *const RECT),
            DXGI_FORMAT(format),
            mode,
        );
    }
}

// ---------------------------------------------------------------------------
// Barriers — 2 slots, of which exactly ONE is reachable per build
// ---------------------------------------------------------------------------

/// Validate a barrier array's runtime-supplied count and pointer, per arm.
///
/// Returns the element count when the array may be read, and `None` when it may
/// not — after counting, logging and reporting.
///
/// # Safety
/// `state` must be the live list state the caller resolved for this DDI call.
unsafe fn barrier_array_len(
    state: &CommandListState,
    name: &str,
    count: u32,
    ptr: *const core::ffi::c_void,
    counter: &RefusalCounter,
) -> Option<usize> {
    let n = count as usize;
    if n == 0 {
        // Legal and degenerate: nothing to lower, nothing to report.
        return None;
    }
    // ⛔ Validate the runtime-supplied count and pointer BEFORE reading the
    // array. CLAUDE.md's rule; the bound is `MAX_BARRIERS`.
    if ptr.is_null() || n > MAX_BARRIERS {
        counter.bump();
        if let Some(k) = budget(&BARRIER_LOG) {
            log_error!(
                "{name}: Count={count} pBarriers={ptr:p} -- refused (x{})",
                k + 1
            );
        }
        // SAFETY: `state` is the live list state this call resolved.
        unsafe { report_error(state, E_INVALIDARG) };
        return None;
    }
    Some(n)
}

/// `pfnResourceBarrier` -> `ID3D12GraphicsCommandList::ResourceBarrier`.
///
/// ⭐ **This is the arm the shipping build exercises**, because `caps12.rs:603`
/// reports `EnhancedBarriersSupported = 0`. See the module doc.
///
/// # ⛔ Three of the four DDI barrier types have an API twin; the fourth is
/// widened rather than dropped
///
/// | DDI `Type` | lowered to | why |
/// |---|---|---|
/// | `TRANSITION` | `D3D12_RESOURCE_BARRIER_TYPE_TRANSITION` | field for field |
/// | `UAV` | `..._TYPE_UAV` | field for field; a null `hResource` is the legal "all UAVs" form |
/// | `0022_RANGED` | `..._TYPE_ALIASING`, **both resources NULL** | see below |
/// | `ALIASING` (deprecated) | `..._TYPE_ALIASING`, **both resources NULL** | the `_0022` union has no aliasing payload to read |
///
/// `D3D12DDIARG_RESOURCE_BARRIER_0022`'s union is `{ Transition, Ranged, UAV }`
/// (`d3d12umddi.h:4802-4813`) — the aliasing member was **removed** when
/// `TYPE_0022_RANGED` deprecated `TYPE_ALIASING`, so a driver at this DDI
/// version has no `hResourceBefore`/`hResourceAfter` to forward even when the
/// runtime sends `TYPE_ALIASING`. And `D3D12_RESOURCE_BARRIER` has no ranged
/// form at all: the API's aliasing barrier names two resources or none, and it
/// carries no byte range.
///
/// ⭐ Both therefore lower to the API's **both-NULL** aliasing barrier, which
/// D3D12 defines as covering any aliased resource and which vkd3d implements as
/// a global memory barrier. That is not an inference:
/// `libs/vkd3d/command.c:4332-4336` is
/// `d3d12_resource_may_alias_other_resources`, whose first two lines are
/// *"Treat a NULL resource as 'all' resources."* followed by `if (!resource)
/// return true;` — and `:13798-13801` then raises `ALL_COMMANDS` /
/// `MEMORY_WRITE` on the batch. ⛔ That is a **widening**: it synchronises a
/// superset of what the DDI asked for, so it can cost parallelism and can never
/// lose an ordering edge. Dropping the barrier instead would be the opposite
/// trade and is the failure `DDI_REFERENCE.md` §9.10 names — *"a venus-side
/// write/read race with no guest-visible error"*.
///
/// ⚠ `FLAG_0022_ATOMIC_COPY` and `FLAG_0022_ALIASING` are **masked off** before
/// forwarding, because `D3D12_RESOURCE_BARRIER_FLAGS` names neither and passing
/// 0x4 or 0x8 into it would be a value no API header defines. `BEGIN_ONLY` and
/// `END_ONLY` forward unchanged (asserted value-identical above); vkd3d answers
/// a split barrier by ignoring the BEGIN half and warning, `:13545`.
///
/// # ⛔ A dropped barrier is issued-then-reported, never dropped silently
///
/// An element this lane cannot lower — an unresolvable `D3D12DDI_HRESOURCE`, or
/// a `Type` this header does not name — is skipped so the **rest of the call
/// still reaches the engine**, and then a command-list error is raised **once,
/// after** the surviving barriers are recorded. Both halves are load-bearing:
///
/// * Reporting does not abort anything. `report_error` returns `()` and only
///   calls `device12::set_command_list_error`, so "issue the survivors" and
///   "tell the runtime" were never alternatives — an earlier revision of this
///   file justified staying silent with that trade, and the trade does not
///   exist. ⚠ *After* also matters on its own terms now: the runtime drops
///   further **recording DDIs** on a list once it has been told, so a report
///   raised mid-loop would be a report against a list the runtime is entitled
///   to stop feeding.
/// * The condition is the same one every other slot in this file answers with
///   `E_INVALIDARG` (`resource_copy`, `buffer_region`, both resolves,
///   `resolve_query_data`, `set_predication`). A barrier is the **more**
///   dangerous of the two to lose: a dropped copy is a visibly wrong frame,
///   while a dropped barrier is a venus-side write/read race with no
///   guest-visible error at all (`DDI_REFERENCE.md` §9.10). Answering the
///   quieter way here would have made the app's fate depend on which DDI it
///   happened to call first.
///
/// # Safety
/// `h_list` must be a live command-list handle and `barriers`, when `count` is
/// non-zero, must point at `count` live `D3D12DDIARG_RESOURCE_BARRIER_0022`s.
unsafe extern "C" fn resource_barrier(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    count: ddi12::UINT,
    barriers: *const ddi12::D3D12DDIARG_RESOURCE_BARRIER_0022,
) {
    // ⚠ Counted before anything can fail; see [`barrier`]'s twin. The pair is
    // what says WHICH arm the shipping build actually exercises, and neither
    // reading alone does: `EnhancedBarrierCalled = 0` on its own is equally
    // consistent with "the legacy arm ran" and "nothing issued a barrier".
    L3C_REFUSALS.legacy_barrier_called.bump();
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3C_REFUSALS.command_list_missing);
        return;
    };
    // SAFETY: `state` is the live list state resolved immediately above.
    let Some(n) = (unsafe {
        barrier_array_len(
            state,
            "ResourceBarrier",
            count,
            barriers.cast(),
            &L3C_REFUSALS.legacy_barrier_bad_arg,
        )
    }) else {
        return;
    };

    // ⚠ One allocation per call, sized from the runtime's own bounded count. The
    // API wrapper takes a slice and the DDI array cannot be reinterpreted as
    // one: `D3D12DDIARG_RESOURCE_BARRIER_0022` is 40 bytes against
    // `D3D12_RESOURCE_BARRIER`'s 32, and the two unions do not agree member for
    // member. Same trade `queue::execute_command_lists` documents.
    let mut api: Vec<D3D12_RESOURCE_BARRIER> = Vec::with_capacity(n);
    // ⛔ How many elements this call could not lower. Reported once, after the
    // survivors are issued; see the slot doc for why both halves are needed.
    let mut dropped = 0usize;
    for i in 0..n {
        // SAFETY: `barriers` is non-null and `i < n <= count`, so this element is
        // inside the array the DDI declares `_In_reads_(Count)`.
        let b = unsafe { &*barriers.add(i) };

        // ⛔ The two DDI-only flags cannot be forwarded; see the doc above.
        let ddi_only = ddi12::D3D12DDI_RESOURCE_BARRIER_FLAGS_D3D12DDI_RESOURCE_BARRIER_FLAG_0022_ATOMIC_COPY
            | ddi12::D3D12DDI_RESOURCE_BARRIER_FLAGS_D3D12DDI_RESOURCE_BARRIER_FLAG_0022_ALIASING;
        if b.Flags & ddi_only != 0 {
            note_refusal(&L3C_REFUSALS.legacy_barrier_flags_dropped);
        }
        let flags = D3D12_RESOURCE_BARRIER_FLAGS(
            b.Flags
                & (D3D12_RESOURCE_BARRIER_FLAG_BEGIN_ONLY.0
                    | D3D12_RESOURCE_BARRIER_FLAG_END_ONLY.0),
        );

        match b.Type {
            ddi12::D3D12DDI_RESOURCE_BARRIER_TYPE_D3D12DDI_RESOURCE_BARRIER_TYPE_TRANSITION => {
                // SAFETY: `Type` says the union holds a transition.
                let t = unsafe { b.__bindgen_anon_1.Transition };
                // SAFETY: the runtime handed this handle in this call.
                let Some(resource) = (unsafe { resource_of(t.hResource) }) else {
                    L3C_REFUSALS.legacy_barrier_resource_missing.bump();
                    if let Some(k) = budget(&BARRIER_LOG) {
                        log_error!(
                            "ResourceBarrier: transition {i} of {count} names resource {:p}, \
                             which carries no engine resource -- barrier DROPPED (x{})",
                            t.hResource.pDrvPrivate,
                            k + 1,
                        );
                    }
                    dropped += 1;
                    continue;
                };
                api.push(D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: flags,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                            pResource: borrowed(resource),
                            Subresource: t.Subresource,
                            StateBefore: D3D12_RESOURCE_STATES(t.StateBefore),
                            StateAfter: D3D12_RESOURCE_STATES(t.StateAfter),
                        }),
                    },
                });
            }
            ddi12::D3D12DDI_RESOURCE_BARRIER_TYPE_D3D12DDI_RESOURCE_BARRIER_TYPE_UAV => {
                // SAFETY: `Type` says the union holds a UAV barrier.
                let u = unsafe { b.__bindgen_anon_1.UAV };
                // ⚠ A null `hResource` is the legal "every UAV access" form and
                // is NOT a missing resource. Tested on the handle, because
                // `resource_of` cannot tell the two apart.
                let resource = if u.hResource.pDrvPrivate.is_null() {
                    None
                } else {
                    // SAFETY: the runtime handed this handle in this call.
                    match unsafe { resource_of(u.hResource) } {
                        Some(r) => Some(r),
                        None => {
                            L3C_REFUSALS.legacy_barrier_resource_missing.bump();
                            if let Some(k) = budget(&BARRIER_LOG) {
                                log_error!(
                                    "ResourceBarrier: UAV barrier {i} of {count} names resource \
                                     {:p}, which carries no engine resource -- barrier DROPPED \
                                     (x{})",
                                    u.hResource.pDrvPrivate,
                                    k + 1,
                                );
                            }
                            dropped += 1;
                            continue;
                        }
                    }
                };
                api.push(D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                    Flags: flags,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                            pResource: match resource {
                                Some(r) => borrowed(r),
                                None => ManuallyDrop::new(None),
                            },
                        }),
                    },
                });
            }
            ddi12::D3D12DDI_RESOURCE_BARRIER_TYPE_D3D12DDI_RESOURCE_BARRIER_TYPE_0022_RANGED
            | ddi12::D3D12DDI_RESOURCE_BARRIER_TYPE_D3D12DDI_RESOURCE_BARRIER_TYPE_ALIASING => {
                L3C_REFUSALS.legacy_barrier_ranged_widened.bump();
                api.push(D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
                    Flags: flags,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Aliasing: ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                            pResourceBefore: ManuallyDrop::new(None),
                            pResourceAfter: ManuallyDrop::new(None),
                        }),
                    },
                });
            }
            other => {
                L3C_REFUSALS.legacy_barrier_type_unknown.bump();
                if let Some(k) = budget(&BARRIER_LOG) {
                    log_error!(
                        "ResourceBarrier: D3D12DDI_RESOURCE_BARRIER_TYPE {other} at {i} of \
                         {count} is not named by this header -- barrier DROPPED (x{})",
                        k + 1,
                    );
                }
                dropped += 1;
            }
        }
    }

    if !api.is_empty() {
        trace_line!("ResourceBarrier: count={count} lowered={}", api.len());
        // SAFETY: `engine()` borrows the list this box owns, and `api` is a live
        // local whose resource fields borrow references the resource states own
        // for longer than this call.
        unsafe { state.engine().ResourceBarrier(&api) };
    }
    if dropped != 0 {
        // ⛔ After the survivors, once for the call. See the slot doc.
        // SAFETY: `state` is the live list state this call resolved.
        unsafe { report_error(state, E_INVALIDARG) };
    }
}

/// Translate `D3D12DDI_BARRIER_LAYOUT` into the API's `D3D12_BARRIER_LAYOUT`.
///
/// ⛔ **Five values need a mapping and the rest are the identity.**
/// `DDI_REFERENCE.md` §9.10.1, cross-checking `d3d12umddi.h:10595-10636` against
/// `d3d12.h:22121-22156`: the runtime re-encodes the legacy model's
/// promotion/decay ambiguity as **DDI-only layout values that never appear in
/// the public API** — the spec calls them *"internal-only and not exposed in
/// public headers"* — and vkd3d cannot accept a value its own API header does
/// not define.
///
/// Each mapping below is a **widening** to the API layout that is legal
/// wherever the DDI one was:
///
/// | DDI-only value | API layout | why it is safe |
/// |---|---|---|
/// | `LEGACY_COPY_SOURCE` | `COPY_SOURCE` | same layout, minus the promotion licence |
/// | `LEGACY_COPY_DEST` | `COPY_DEST` | as above |
/// | `LEGACY_SHADER_RESOURCE` | `SHADER_RESOURCE` | as above |
/// | `LEGACY_PIXEL_SHADER_RESOURCE` | `SHADER_RESOURCE` | all stages, not just PS |
/// | `LEGACY_DIRECT_QUEUE_GENERIC_READ_COMPUTE_QUEUE_ACCESSIBLE` | `GENERIC_READ` | the queue-agnostic read layout, which is what "accessible from the compute queue too" means |
fn api_barrier_layout(l: ddi12::D3D12DDI_BARRIER_LAYOUT) -> (D3D12_BARRIER_LAYOUT, bool) {
    match l {
        ddi12::D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_LEGACY_COPY_SOURCE => {
            (D3D12_BARRIER_LAYOUT_COPY_SOURCE, true)
        }
        ddi12::D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_LEGACY_COPY_DEST => {
            (D3D12_BARRIER_LAYOUT_COPY_DEST, true)
        }
        ddi12::D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_LEGACY_SHADER_RESOURCE
        | ddi12::D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_LEGACY_PIXEL_SHADER_RESOURCE => {
            (D3D12_BARRIER_LAYOUT_SHADER_RESOURCE, true)
        }
        ddi12::D3D12DDI_BARRIER_LAYOUT_D3D12DDI_BARRIER_LAYOUT_LEGACY_DIRECT_QUEUE_GENERIC_READ_COMPUTE_QUEUE_ACCESSIBLE => {
            (D3D12_BARRIER_LAYOUT_GENERIC_READ, true)
        }
        other => (D3D12_BARRIER_LAYOUT(other), false),
    }
}

/// `pfnBarrier` -> `ID3D12GraphicsCommandList7::Barrier`.
///
/// ⛔ **Unreachable in the shipping configuration**, because `caps12.rs:603`
/// reports `EnhancedBarriersSupported = 0` and the runtime then calls
/// `pfnResourceBarrier` instead — *"Legacy barrier DDI's are never invoked on a
/// driver supporting enhanced barriers"*, read in the other direction. Every
/// `L3cEnhancedBarrier*` counter is therefore expected to read **zero**, and a
/// non-zero reading with the cap still 0 is a finding about the runtime rather
/// than about this lane. The slot is a real handler anyway; the module doc says
/// why a NULL is not an option.
///
/// # ⭐ The shape change the API forces: a tagged array becomes typed groups
///
/// `D3D12DDIARG_BARRIER_0094` is a **per-barrier tagged union**
/// (`d3d12umddi.h:11285-11296`), while `ID3D12GraphicsCommandList7::Barrier`
/// takes `D3D12_BARRIER_GROUP`s, each of which is one *type* plus an array of
/// that type. So the array is partitioned by type into at most three groups.
///
/// ⛔ **Partitioning is order-preserving in effect, not merely in practice.**
/// D3D12 defines every barrier in one `Barrier()` call as a set rather than a
/// sequence, and vkd3d implements exactly that: `barrier_batch_init` runs before
/// the group loop and `barrier_batch_end` after it
/// (`libs/vkd3d/command.c:21812` and `:21852`), so all groups accumulate into
/// one `vkCmdPipelineBarrier2`. Regrouping cannot change the result.
///
/// ⚠ `D3D12DDI_BARRIER_TYPE_RANGED` has no API counterpart — the API's
/// `D3D12_BARRIER_TYPE` is GLOBAL / TEXTURE / BUFFER only — and its DDI payload
/// carries **no sync and no access at all**, just flags and a byte range. It is
/// lowered to a maximally conservative GLOBAL barrier (`SYNC_ALL` both ways,
/// `ACCESS_COMMON` both ways, which vkd3d turns into
/// `MEMORY_READ|MEMORY_WRITE`, `libs/vkd3d/command.c:21327-21328`) and counted.
/// That is strictly stronger than any ranged barrier could be.
///
/// ⚠ The DDI buffer barrier has no `Offset` and no `Size` — the runtime strips
/// the API's two `UINT64`s before the driver sees them — so a buffer barrier is
/// always whole-resource here and the API fields take the whole-buffer form
/// `{ 0, UINT64_MAX }`.
///
/// # ⛔ `FLAG_DISCARD`: the obligation is OPEN, and the engine does not close it
///
/// `D3D12DDI_TEXTURE_BARRIER_FLAG_DISCARD` forwards raw — the two flags are both
/// `0x1` and that identity is asserted above — but forwarding is **not** the
/// obligation. `DDI_REFERENCE.md:2121-2124` states it in bold: a texture barrier
/// with `FLAG_DISCARD` on an `UNDEFINED`-to-compressible transition makes
/// compression-metadata initialisation a *driver* obligation, and *"a forwarder
/// must satisfy the obligation, not merely pass the flag"*.
///
/// ⛔ This pinned engine reads the flag in exactly **one** place — a `grep` over
/// `libs/` returns `command.c:21738` and nothing else — and it reads it the
/// opposite way round from a fixup. The condition is `LayoutBefore ==
/// D3D12_BARRIER_LAYOUT_UNDEFINED && !(Flags & D3D12_TEXTURE_BARRIER_FLAG_DISCARD)`
/// and its whole body is a `FIXME_ONCE` reading *"Transitioning away from
/// UNDEFINED, but there is no DISCARD flag. Uncertain what is expected here.
/// vkd3d-proton will force a discard."* So vkd3d **force-discards either way**
/// and merely warns when the flag is absent; nothing is gated on the flag being
/// set, and there is no metadata-initialisation path for this driver to have
/// delegated.
///
/// ⚠ The exposure is therefore the *preserve* case, not the discard case: an app
/// transitioning away from `UNDEFINED` **without** `DISCARD`, expecting its
/// contents kept, gets them discarded. It is invisible from the guest — the
/// `FIXME_ONCE` lands once in the host's `/tmp/helios-qemu-stderr.log`, no
/// counter in this file moves, and no reading here can detect it. It is also
/// unreachable in the shipping build, because this whole slot is dead while
/// `EnhancedBarriersSupported = 0`. Recorded as an UNVERIFIED for the commit
/// that raises the cap rather than as a solved problem.
///
/// # Safety
/// `h_list` must be a live command-list handle and `barriers`, when
/// `num_barriers` is non-zero, must point at `num_barriers` live
/// `D3D12DDIARG_BARRIER_0094`s.
unsafe extern "C" fn barrier(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    num_barriers: ddi12::UINT32,
    barriers: *const ddi12::D3D12DDIARG_BARRIER_0094,
) {
    // ⚠ Counted BEFORE anything can fail, because the reading that matters is
    // "did the runtime choose this arm", not "did this arm succeed".
    L3C_REFUSALS.enhanced_barrier_called.bump();
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3C_REFUSALS.command_list_missing);
        return;
    };
    // SAFETY: `state` is the live list state resolved immediately above.
    let Some(n) = (unsafe {
        barrier_array_len(
            state,
            "Barrier",
            num_barriers,
            barriers.cast(),
            &L3C_REFUSALS.enhanced_barrier_bad_arg,
        )
    }) else {
        return;
    };

    let mut globals: Vec<D3D12_GLOBAL_BARRIER> = Vec::new();
    let mut textures: Vec<D3D12_TEXTURE_BARRIER> = Vec::new();
    let mut buffers: Vec<D3D12_BUFFER_BARRIER> = Vec::new();
    // ⛔ As [`resource_barrier`]: elements this lane could not lower, reported
    // once after the survivors reach the engine. ⚠ The buffer arm never adds to
    // it — an unresolvable buffer resource is still forwarded, so nothing is
    // lost there and there is nothing to report.
    let mut dropped = 0usize;

    for i in 0..n {
        // SAFETY: `barriers` is non-null and `i < n <= num_barriers`, so this
        // element is inside the array the DDI declares `_In_reads_(NumBarriers)`.
        let b = unsafe { &*barriers.add(i) };
        match b.Type {
            ddi12::D3D12DDI_BARRIER_TYPE_D3D12DDI_BARRIER_TYPE_GLOBAL => {
                // SAFETY: `Type` says the union holds a global barrier.
                let g = unsafe { b.__bindgen_anon_1.GlobalBarrier };
                globals.push(D3D12_GLOBAL_BARRIER {
                    SyncBefore: D3D12_BARRIER_SYNC(g.SyncBefore),
                    SyncAfter: D3D12_BARRIER_SYNC(g.SyncAfter),
                    AccessBefore: D3D12_BARRIER_ACCESS(g.AccessBefore),
                    AccessAfter: D3D12_BARRIER_ACCESS(g.AccessAfter),
                });
            }
            ddi12::D3D12DDI_BARRIER_TYPE_D3D12DDI_BARRIER_TYPE_TEXTURE => {
                // SAFETY: `Type` says the union holds a texture barrier.
                let t = unsafe { b.__bindgen_anon_1.TextureBarrier };
                // SAFETY: the runtime handed this handle in this call.
                let Some(resource) = (unsafe { resource_of(t.hResource) }) else {
                    L3C_REFUSALS.enhanced_barrier_resource_missing.bump();
                    if let Some(k) = budget(&BARRIER_LOG) {
                        log_error!(
                            "Barrier: texture barrier {i} of {num_barriers} names resource {:p}, \
                             which carries no engine resource -- barrier DROPPED (x{})",
                            t.hResource.pDrvPrivate,
                            k + 1,
                        );
                    }
                    dropped += 1;
                    continue;
                };
                let (layout_before, mapped_before) = api_barrier_layout(t.LayoutBefore);
                let (layout_after, mapped_after) = api_barrier_layout(t.LayoutAfter);
                if mapped_before || mapped_after {
                    L3C_REFUSALS.enhanced_barrier_layout_legacy_mapped.bump();
                    if let Some(k) = budget(&BARRIER_LOG) {
                        log_error!(
                            "Barrier: texture barrier {i} carries DDI-only layouts \
                             before={} after={}, mapped to API {}/{} (x{})",
                            t.LayoutBefore,
                            t.LayoutAfter,
                            layout_before.0,
                            layout_after.0,
                            k + 1,
                        );
                    }
                }
                textures.push(D3D12_TEXTURE_BARRIER {
                    SyncBefore: D3D12_BARRIER_SYNC(t.SyncBefore),
                    SyncAfter: D3D12_BARRIER_SYNC(t.SyncAfter),
                    AccessBefore: D3D12_BARRIER_ACCESS(t.AccessBefore),
                    AccessAfter: D3D12_BARRIER_ACCESS(t.AccessAfter),
                    LayoutBefore: layout_before,
                    LayoutAfter: layout_after,
                    pResource: borrowed(resource),
                    Subresources: D3D12_BARRIER_SUBRESOURCE_RANGE {
                        IndexOrFirstMipLevel: t.Subresources.IndexOrFirstMipLevel,
                        NumMipLevels: t.Subresources.NumMipLevels,
                        FirstArraySlice: t.Subresources.FirstArraySlice,
                        NumArraySlices: t.Subresources.NumArraySlices,
                        FirstPlane: t.Subresources.FirstPlane,
                        NumPlanes: t.Subresources.NumPlanes,
                    },
                    Flags: D3D12_TEXTURE_BARRIER_FLAGS(t.Flags),
                });
            }
            ddi12::D3D12DDI_BARRIER_TYPE_D3D12DDI_BARRIER_TYPE_BUFFER => {
                // SAFETY: `Type` says the union holds a buffer barrier.
                let f = unsafe { b.__bindgen_anon_1.BufferBarrier };
                // SAFETY: the runtime handed this handle in this call.
                let resource = unsafe { resource_of(f.hResource) };
                if resource.is_none() {
                    L3C_REFUSALS.enhanced_barrier_resource_missing.bump();
                    if let Some(k) = budget(&BARRIER_LOG) {
                        log_error!(
                            "Barrier: buffer barrier {i} of {num_barriers} names resource {:p}, \
                             which carries no engine resource -- forwarding the sync/access edge \
                             anyway (x{})",
                            f.hResource.pDrvPrivate,
                            k + 1,
                        );
                    }
                }
                // ⛔ **Forwarded even without the resource, unlike the texture
                // arm, and that asymmetry is the engine's rather than a
                // shortcut.** vkd3d lowers a buffer barrier to a plain
                // `D3D12_GLOBAL_BARRIER` built from the four sync/access fields
                // and reads `pResource`, `Offset` and `Size` not at all
                // (`libs/vkd3d/command.c:21569-21578`, *"We get very little out
                // of buffer barriers since the stage flags are more granular
                // now."*). A texture barrier is a layout transition on a
                // specific image and has nothing left without its resource; a
                // buffer barrier's whole content is the ordering edge, and
                // dropping it would be a self-inflicted synchronisation loss.
                buffers.push(D3D12_BUFFER_BARRIER {
                    SyncBefore: D3D12_BARRIER_SYNC(f.SyncBefore),
                    SyncAfter: D3D12_BARRIER_SYNC(f.SyncAfter),
                    AccessBefore: D3D12_BARRIER_ACCESS(f.AccessBefore),
                    AccessAfter: D3D12_BARRIER_ACCESS(f.AccessAfter),
                    pResource: match resource {
                        Some(r) => borrowed(r),
                        None => ManuallyDrop::new(None),
                    },
                    // ⚠ The whole-buffer form: the DDI struct has no offset and
                    // no size — the runtime strips the API's two `UINT64`s
                    // before the driver sees them — so there is no narrower
                    // range to state. Inert on this engine either way.
                    Offset: 0,
                    Size: u64::MAX,
                });
            }
            ddi12::D3D12DDI_BARRIER_TYPE_D3D12DDI_BARRIER_TYPE_RANGED => {
                L3C_REFUSALS.enhanced_barrier_ranged_widened.bump();
                globals.push(D3D12_GLOBAL_BARRIER {
                    SyncBefore: D3D12_BARRIER_SYNC_ALL,
                    SyncAfter: D3D12_BARRIER_SYNC_ALL,
                    AccessBefore: D3D12_BARRIER_ACCESS_COMMON,
                    AccessAfter: D3D12_BARRIER_ACCESS_COMMON,
                });
            }
            other => {
                L3C_REFUSALS.enhanced_barrier_type_unknown.bump();
                if let Some(k) = budget(&BARRIER_LOG) {
                    log_error!(
                        "Barrier: D3D12DDI_BARRIER_TYPE {other} at {i} of {num_barriers} is not \
                         named by this header -- barrier DROPPED (x{})",
                        k + 1,
                    );
                }
                dropped += 1;
            }
        }
    }

    // ⚠ Built after every push, so no group points at a reallocated buffer.
    let mut groups: Vec<D3D12_BARRIER_GROUP> = Vec::with_capacity(3);
    if !globals.is_empty() {
        groups.push(D3D12_BARRIER_GROUP {
            Type: D3D12_BARRIER_TYPE_GLOBAL,
            NumBarriers: globals.len() as u32,
            Anonymous: D3D12_BARRIER_GROUP_0 {
                pGlobalBarriers: globals.as_ptr(),
            },
        });
    }
    if !textures.is_empty() {
        groups.push(D3D12_BARRIER_GROUP {
            Type: D3D12_BARRIER_TYPE_TEXTURE,
            NumBarriers: textures.len() as u32,
            Anonymous: D3D12_BARRIER_GROUP_0 {
                pTextureBarriers: textures.as_ptr(),
            },
        });
    }
    if !buffers.is_empty() {
        groups.push(D3D12_BARRIER_GROUP {
            Type: D3D12_BARRIER_TYPE_BUFFER,
            NumBarriers: buffers.len() as u32,
            Anonymous: D3D12_BARRIER_GROUP_0 {
                pBufferBarriers: buffers.as_ptr(),
            },
        });
    }
    if !groups.is_empty() {
        // `cast` is a `QueryInterface` on the live list this box owns and yields
        // an owned reference this function drops. Safe: the borrow is
        // `engine()`'s.
        let list7 = match state.engine().cast::<ID3D12GraphicsCommandList7>() {
            Ok(l) => l,
            Err(err) => {
                L3C_REFUSALS.command_list7_unavailable.bump();
                if let Some(k) = budget(&BARRIER_LOG) {
                    log_error!(
                        "Barrier: QueryInterface for ID3D12GraphicsCommandList7 failed \
                         hr={:#010x} -- {} barrier groups DROPPED (x{})",
                        err.code().0 as u32,
                        groups.len(),
                        k + 1,
                    );
                }
                // ⚠ Returns here rather than falling through to the `dropped`
                // report below: this list is already being quarantined for this
                // call, and a second `pfnSetCommandListErrorCb` on the same
                // handle would only double-count one event.
                // SAFETY: `state` is the live list state this call resolved.
                unsafe { report_error(state, E_NOTIMPL) };
                return;
            }
        };

        trace_line!(
            "Barrier: count={num_barriers} global={} texture={} buffer={}",
            globals.len(),
            textures.len(),
            buffers.len(),
        );
        // SAFETY: `list7` is a live owned reference to the same command list, and
        // every group points into a vector that is still alive on this frame and
        // is not resized again before the call returns.
        unsafe { list7.Barrier(&groups) };
    }
    if dropped != 0 {
        // ⛔ After the survivors, once for the call. See [`resource_barrier`].
        // SAFETY: `state` is the live list state this call resolved.
        unsafe { report_error(state, E_INVALIDARG) };
    }
}

// ---------------------------------------------------------------------------
// Queries and predication — 4 slots
// ---------------------------------------------------------------------------

/// Translate `D3D12DDI_QUERY_TYPE` into the API enum.
///
/// ⛔ A match on named constants, never a cast — see [`api_resolve_mode`].
fn api_query_type(t: ddi12::D3D12DDI_QUERY_TYPE) -> Option<D3D12_QUERY_TYPE> {
    Some(match t {
        ddi12::D3D12DDI_QUERY_TYPE_D3D12DDI_QUERY_TYPE_OCCLUSION => D3D12_QUERY_TYPE_OCCLUSION,
        ddi12::D3D12DDI_QUERY_TYPE_D3D12DDI_QUERY_TYPE_BINARY_OCCLUSION => {
            D3D12_QUERY_TYPE_BINARY_OCCLUSION
        }
        ddi12::D3D12DDI_QUERY_TYPE_D3D12DDI_QUERY_TYPE_TIMESTAMP => D3D12_QUERY_TYPE_TIMESTAMP,
        ddi12::D3D12DDI_QUERY_TYPE_D3D12DDI_QUERY_TYPE_PIPELINE_STATISTICS => {
            D3D12_QUERY_TYPE_PIPELINE_STATISTICS
        }
        ddi12::D3D12DDI_QUERY_TYPE_D3D12DDI_QUERY_TYPE_SO_STATISTICS_STREAM0 => {
            D3D12_QUERY_TYPE_SO_STATISTICS_STREAM0
        }
        ddi12::D3D12DDI_QUERY_TYPE_D3D12DDI_QUERY_TYPE_SO_STATISTICS_STREAM1 => {
            D3D12_QUERY_TYPE_SO_STATISTICS_STREAM1
        }
        ddi12::D3D12DDI_QUERY_TYPE_D3D12DDI_QUERY_TYPE_SO_STATISTICS_STREAM2 => {
            D3D12_QUERY_TYPE_SO_STATISTICS_STREAM2
        }
        ddi12::D3D12DDI_QUERY_TYPE_D3D12DDI_QUERY_TYPE_SO_STATISTICS_STREAM3 => {
            D3D12_QUERY_TYPE_SO_STATISTICS_STREAM3
        }
        ddi12::D3D12DDI_QUERY_TYPE_D3D12DDI_QUERY_TYPE_0020_VIDEO_DECODE_STATISTICS => {
            D3D12_QUERY_TYPE_VIDEO_DECODE_STATISTICS
        }
        ddi12::D3D12DDI_QUERY_TYPE_D3D12DDI_QUERY_TYPE_PIPELINE_STATISTICS1 => {
            D3D12_QUERY_TYPE_PIPELINE_STATISTICS1
        }
        _ => return None,
    })
}

/// Which end of a query the shared body is recording.
#[derive(Clone, Copy)]
enum QueryEdge {
    Begin,
    End,
}

impl QueryEdge {
    fn name(self) -> &'static str {
        match self {
            QueryEdge::Begin => "BeginQuery",
            QueryEdge::End => "EndQuery",
        }
    }
}

/// The body behind `pfnBeginQuery` and `pfnEndQuery`.
///
/// ⚠ The two slots share `PFND3D12DDI_BEGIN_END_QUERY_0003` and differ only in
/// which engine method they reach, so they share this body and name themselves
/// through [`QueryEdge`] — the shape `queue::fence_operation` takes for the same
/// reason. ⛔ They still install as **two distinct functions**, because a shared
/// installed handler could not say which slot the runtime called.
///
/// # Safety
/// `h_list` must be a live command-list handle and `h_heap` a live handle from
/// `fence::create_query_heap`.
unsafe fn query_edge(
    edge: QueryEdge,
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_heap: ddi12::D3D12DDI_HQUERYHEAP,
    query_type: ddi12::D3D12DDI_QUERY_TYPE,
    index: ddi12::UINT,
) {
    let name = edge.name();
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3C_REFUSALS.command_list_missing);
        return;
    };
    // SAFETY: the caller guarantees a live handle from `create_query_heap`.
    let Some(heap) = (unsafe { fence::engine_query_heap(h_heap) }) else {
        L3C_REFUSALS.query_heap_missing.bump();
        if let Some(n) = budget(&QUERY_LOG) {
            log_error!(
                "{name}: heap={:p} carries no ID3D12QueryHeap (x{})",
                h_heap.pDrvPrivate,
                n + 1,
            );
        }
        // SAFETY: `state` is the live list state this call resolved.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    let Some(api_type) = api_query_type(query_type) else {
        L3C_REFUSALS.query_type_unsupported.bump();
        if let Some(n) = budget(&QUERY_LOG) {
            log_error!(
                "{name}: D3D12DDI_QUERY_TYPE {query_type} is not named by this header (x{})",
                n + 1,
            );
        }
        // SAFETY: `state` is the live list state resolved above.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    trace_line!("{name}: type={query_type} index={index}");
    // SAFETY: `engine()` borrows the list this box owns, and `heap` borrows the
    // reference the query-heap slot owns — `ManuallyDrop` is why dropping it
    // issues no `Release`.
    unsafe {
        match edge {
            QueryEdge::Begin => state.engine().BeginQuery(&*heap, api_type, index),
            QueryEdge::End => state.engine().EndQuery(&*heap, api_type, index),
        }
    }
}

/// `pfnBeginQuery` -> `ID3D12GraphicsCommandList::BeginQuery`.
///
/// # Safety
/// As [`query_edge`].
unsafe extern "C" fn begin_query(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_heap: ddi12::D3D12DDI_HQUERYHEAP,
    query_type: ddi12::D3D12DDI_QUERY_TYPE,
    index: ddi12::UINT,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is `query_edge`'s.
    unsafe { query_edge(QueryEdge::Begin, h_list, h_heap, query_type, index) };
}

/// `pfnEndQuery` -> `ID3D12GraphicsCommandList::EndQuery`.
///
/// ⚠ A separate function from [`begin_query`], not a shared install: see
/// [`query_edge`].
///
/// # Safety
/// As [`query_edge`].
unsafe extern "C" fn end_query(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_heap: ddi12::D3D12DDI_HQUERYHEAP,
    query_type: ddi12::D3D12DDI_QUERY_TYPE,
    index: ddi12::UINT,
) {
    // SAFETY: forwarded unchanged; the caller's guarantee is `query_edge`'s.
    unsafe { query_edge(QueryEdge::End, h_list, h_heap, query_type, index) };
}

/// `pfnResolveQueryData` -> `ID3D12GraphicsCommandList::ResolveQueryData`.
///
/// # Safety
/// `h_list` must be a live command-list handle, `h_heap` a live handle from
/// `fence::create_query_heap`, and `h_destination_buffer` a live resource handle.
unsafe extern "C" fn resolve_query_data(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_heap: ddi12::D3D12DDI_HQUERYHEAP,
    query_type: ddi12::D3D12DDI_QUERY_TYPE,
    start_element: ddi12::UINT,
    element_count: ddi12::UINT,
    h_destination_buffer: ddi12::D3D12DDI_HRESOURCE,
    destination_offset: ddi12::UINT64,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3C_REFUSALS.command_list_missing);
        return;
    };
    // SAFETY: the caller guarantees a live handle from `create_query_heap`.
    let Some(heap) = (unsafe { fence::engine_query_heap(h_heap) }) else {
        L3C_REFUSALS.query_heap_missing.bump();
        if let Some(n) = budget(&QUERY_LOG) {
            log_error!(
                "ResolveQueryData: heap={:p} carries no ID3D12QueryHeap (x{})",
                h_heap.pDrvPrivate,
                n + 1,
            );
        }
        // SAFETY: `state` is the live list state this call resolved.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    let Some(api_type) = api_query_type(query_type) else {
        L3C_REFUSALS.query_type_unsupported.bump();
        if let Some(n) = budget(&QUERY_LOG) {
            log_error!(
                "ResolveQueryData: D3D12DDI_QUERY_TYPE {query_type} is not named by this header \
                 (x{})",
                n + 1,
            );
        }
        // SAFETY: `state` is the live list state resolved above.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    // SAFETY: the runtime handed this handle in this call, so it is live.
    let Some(dest) = (unsafe { resource_of(h_destination_buffer) }) else {
        L3C_REFUSALS.resolve_query_dest_missing.bump();
        if let Some(n) = budget(&QUERY_LOG) {
            log_error!(
                "ResolveQueryData: destination={:p} carries no engine resource (x{})",
                h_destination_buffer.pDrvPrivate,
                n + 1,
            );
        }
        // SAFETY: `state` is the live list state resolved above.
        unsafe { report_error(state, E_INVALIDARG) };
        return;
    };
    trace_line!(
        "ResolveQueryData: type={query_type} start={start_element} count={element_count} \
         offset={destination_offset}"
    );
    // SAFETY: `engine()` borrows the list this box owns, `heap` borrows the
    // query-heap slot's reference, and `dest` borrows the resource state's.
    unsafe {
        state.engine().ResolveQueryData(
            &*heap,
            api_type,
            start_element,
            element_count,
            dest,
            destination_offset,
        );
    }
}

/// `pfnSetPredication` -> `ID3D12GraphicsCommandList::SetPredication`.
///
/// ⭐ **The DDI and API predication ops ARE value-identical, and it is proved
/// rather than assumed** — the `const _` above compares
/// `D3D12DDI_PREDICATION_OP_NOT_EQUAL_ZERO` against
/// `D3D12_PREDICATION_OP_NOT_EQUAL_ZERO.0`, two generated constants. The
/// translation is still a **match on named values** rather than a cast, because
/// identity over the two named enumerators says nothing about a third value:
/// `DDI_REFERENCE.md` §9.6.1's scar is precisely a DDI value the API gives a
/// different meaning, and refusing an unnamed op is what stops this driver
/// inventing one.
///
/// ⚠ A null `hBuffer` is the legal "stop predicating" form and is forwarded as
/// `None`, not treated as a missing resource. `resource_of` cannot tell a null
/// handle from an empty slot, so the handle is tested first.
///
/// # Safety
/// `h_list` must be a live command-list handle and `h_buffer`, when its private
/// word is non-null, a live resource handle this driver created.
unsafe extern "C" fn set_predication(
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    h_buffer: ddi12::D3D12DDI_HRESOURCE,
    aligned_buffer_offset: ddi12::UINT64,
    op: ddi12::D3D12DDI_PREDICATION_OP,
) {
    // SAFETY: the caller guarantees a live handle from `create_command_list`.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        note_refusal(&L3C_REFUSALS.command_list_missing);
        return;
    };
    let api_op = match op {
        ddi12::D3D12DDI_PREDICATION_OP_D3D12DDI_PREDICATION_OP_EQUAL_ZERO => {
            D3D12_PREDICATION_OP_EQUAL_ZERO
        }
        ddi12::D3D12DDI_PREDICATION_OP_D3D12DDI_PREDICATION_OP_NOT_EQUAL_ZERO => {
            D3D12_PREDICATION_OP_NOT_EQUAL_ZERO
        }
        other => {
            L3C_REFUSALS.predication_op_unsupported.bump();
            if let Some(n) = budget(&QUERY_LOG) {
                log_error!(
                    "SetPredication: D3D12DDI_PREDICATION_OP {other} is not named by this header \
                     (x{})",
                    n + 1,
                );
            }
            // SAFETY: `state` is the live list state this call resolved.
            unsafe { report_error(state, E_INVALIDARG) };
            return;
        }
    };

    let buffer: Option<&ID3D12Resource> = if h_buffer.pDrvPrivate.is_null() {
        None
    } else {
        // SAFETY: the runtime handed this handle in this call, so it is live.
        match unsafe { resource_of(h_buffer) } {
            Some(r) => Some(r),
            None => {
                L3C_REFUSALS.predication_resource_missing.bump();
                if let Some(n) = budget(&QUERY_LOG) {
                    log_error!(
                        "SetPredication: buffer={:p} carries no engine resource (x{})",
                        h_buffer.pDrvPrivate,
                        n + 1,
                    );
                }
                // SAFETY: `state` is the live list state resolved above.
                unsafe { report_error(state, E_INVALIDARG) };
                return;
            }
        }
    };

    trace_line!(
        "SetPredication: buffer={:p} offset={aligned_buffer_offset} op={op}",
        h_buffer.pDrvPrivate,
    );
    // SAFETY: `engine()` borrows the list this box owns, and `buffer` is either
    // `None` (the API's "stop predicating") or a borrowed reference the resource
    // state owns for longer than this call.
    unsafe {
        state
            .engine()
            .SetPredication(buffer, aligned_buffer_offset, api_op);
    }
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Install L3c's 13 command-list slots.
///
/// Chain position: `RootArgSlots` -> `CopySlots` on the command-list table.
pub(crate) fn install(
    mut filling: Filling<'_, CommandListTable, stage::RootArgSlots>,
) -> Filling<'_, CommandListTable, stage::CopySlots> {
    let table = filling.table();
    // Copy / resolve — 7.
    table.pfnCopyTextureRegion = Some(copy_texture_region);
    table.pfnResourceCopy = Some(resource_copy);
    table.pfnCopyTiles = Some(copy_tiles);
    table.pfnCopyBufferRegion = Some(copy_buffer_region);
    table.pfnResourceResolveSubresource = Some(resource_resolve_subresource);
    table.pfnAtomicCopyBufferRegion = Some(atomic_copy_buffer_region);
    table.pfnResourceResolveSubresourceRegion = Some(resource_resolve_subresource_region);
    // Barriers — 2, of which exactly one is reachable per build. See the module
    // doc: at `EnhancedBarriersSupported = 0` the runtime calls
    // `pfnResourceBarrier` and never `pfnBarrier`, and the table has no opt-out
    // that would let the dead one stay NULL.
    table.pfnResourceBarrier = Some(resource_barrier);
    table.pfnBarrier = Some(barrier);
    // Queries / predication — 4.
    table.pfnBeginQuery = Some(begin_query);
    table.pfnEndQuery = Some(end_query);
    table.pfnResolveQueryData = Some(resolve_query_data);
    table.pfnSetPredication = Some(set_predication);
    filling.advance()
}

// ---------------------------------------------------------------------------
// Refusal counters
// ---------------------------------------------------------------------------

/// L3c's refusal counters. One instance, [`L3C_REFUSALS`]; the set that prints
/// them is [`REFUSALS`].
pub(crate) struct L3cRefusals {
    /// A command-list slot could not resolve its `D3D12DDI_HCOMMANDLIST` to a
    /// live `queue::CommandListState`. **Expected 0** — the runtime only records
    /// into a list `pfnCreateCommandList` returned `S_OK` for.
    ///
    /// ⚠ Deliberately **not** reported: with no state there is neither an
    /// `h_device` to resolve the callbacks through nor an
    /// `D3D12DDI_HRTCOMMANDLIST` to name the list, so this is the one failure
    /// this table cannot escalate by either channel. Tracks `cmdlist.rs`'s
    /// counter of the same name in the other lane's set.
    command_list_missing: RefusalCounter,
    /// A copy slot arrived with a null pointer the DDI declares `_In_ CONST`,
    /// with an `RL_SELECT_SUBRESOURCE` `Offset` that is not a subresource index,
    /// or with a `Physical*` triple that does not fit a `UINT` texel count once
    /// multiplied by the format's block extent. **Expected 0**; reported,
    /// because a copy that cannot be expressed is a correctness failure rather
    /// than an unsupported capability.
    ///
    /// ⚠ The third condition is defensive only — D3D12's maximum texture
    /// dimension is 16384 texels, so no legal layout can reach it. It exists so
    /// [`block_extent`]'s multiplication is total without a wrapping `as`.
    copy_bad_arg: RefusalCounter,
    /// A `D3D12DDI_HRESOURCE` in a copy, resolve or buffer-region slot did not
    /// resolve to an `ID3D12Resource`. **Expected 0.**
    ///
    /// ⚠ Read it beside L4's `ResourceCreateEngineFailed` and
    /// `HeapSpanBufferUnavailable`: a resource whose creation failed still gets a
    /// handle, and this is where the consequence lands.
    copy_resource_missing: RefusalCounter,
    /// `D3D12DDIARG_PLACED_RESOURCE::Layout` was `RL_UNDEFINED` — which
    /// `d3d12umddi.h:1541` states it *cannot be* — or a value this header does
    /// not name. **Expected 0**; a hit is a runtime-contract finding, not an app
    /// one.
    copy_layout_unknown: RefusalCounter,
    /// `pfnCopyTextureRegion`'s `pSrcBox` carried a negative coordinate, which
    /// the API's unsigned `D3D12_BOX` cannot express. **Expected 0** — the
    /// runtime's own box is unsigned, so a negative here means the `LONG`s were
    /// not written by a `D3D12_BOX`.
    copy_box_negative: RefusalCounter,
    /// A **traffic** counter, not a refusal:
    /// `RL_PLACED_PHYSICAL_SUBRESOURCE_PITCHED` arrived carrying a
    /// block-compressed `DXGI_FORMAT`, and its `Physical*` block dimensions were
    /// converted to the texels `D3D12_SUBRESOURCE_FOOTPRINT` wants.
    ///
    /// ⚠ **Re-graded, and the grading is the whole point.** It read *"Expected
    /// 0, and a hit is work"* while this arm forwarded block dimensions raw and
    /// called the units unsettleable. The `PARALLEL.md` §10 review settled them
    /// out of `d3d12umddi.h:1556-1568`, where
    /// `D3D12DDIARG_VIRTUAL_SUBRESOURCE_PITCHED_LAYOUT` carries the `Virtual*`
    /// and `Physical*` triples side by side with `// Block dimensions` on the
    /// second only. ⇒ **Expected NON-ZERO on any workload that copies BC
    /// textures**, which is nearly all of them, and it is now the readout that
    /// says the conversion path ran rather than a defect waiting to be graded.
    ///
    /// ⛔ A **zero** on a workload known to stream BC textures is the finding
    /// now: either those copies take the VIRTUAL arm — legitimate, that arm
    /// needs no conversion — or they are not reaching this slot at all.
    /// ⚠ Every non-block format leaves the two readings equal, so a zero says
    /// nothing about whether the arm itself was taken; read it with
    /// `L3cCopyBadArg`.
    ///
    /// ⚠ Its log line was deleted with the re-grade. It existed to carry the
    /// dimensions to a VM run that would settle the units; the units are
    /// settled, and a per-BC-copy line would be pure noise.
    copy_texture_block_physical_layout: RefusalCounter,
    /// A pitched copy location arrived with `PhysicalDepth > 1` and a
    /// `SlicePitch` that is not `Pitch * PhysicalHeight`, and **the copy was
    /// refused**: `D3D12_SUBRESOURCE_FOOTPRINT` has no member that could carry
    /// the difference.
    ///
    /// **Expected 0**, and unlike its neighbour above this grading survived the
    /// review unchanged — what changed is the **answer**. It used to forward the
    /// copy anyway, and its own doc conceded that *"silent forwarding is the one
    /// answer that is only acceptable while this reads 0"*; the reason given for
    /// forwarding was that reporting would remove the device, which is no longer
    /// the channel ([`report_error`]). It now refuses and reports, quarantining
    /// one list.
    ///
    /// ⛔ Refusing is cheap precisely because the condition should be
    /// unreachable: the public `D3D12_PLACED_SUBRESOURCE_FOOTPRINT` the runtime
    /// computes for itself has no slice-pitch member, so any layout it derives
    /// can only be `RowPitch * rows`. A hit is therefore a **finding about the
    /// runtime**, and the log line at `refuse_location` prints all four numbers.
    /// ⚠ The reading is exact rather than conservative — the comparison is
    /// between three memory quantities and needs no format knowledge (see
    /// [`slice_pitch_representable`]) — so a zero really does mean the runtime
    /// never diverged on this workload.
    copy_slice_pitch_unrepresentable: RefusalCounter,
    /// `pfnCopyTiles` was called and refused. **Expected 0** while `caps12.rs`
    /// reports `TiledResourcesTier = NOT_SUPPORTED`: a hit means the runtime
    /// reached a tiled-resource path on a driver that says it has none, i.e. a
    /// caps inconsistency somewhere else. ⚠ It moves with L2's
    /// `TileMappingsRefused`, which counts the queue-side half of the same
    /// capability; a run that hits one and not the other is itself the finding.
    ///
    /// ⚠ **Re-graded consequence**: a hit now also quarantines the recording
    /// list, where it used to be silent. The expected reading is unchanged; what
    /// changed is that the application finds out, at `Close()`.
    copy_tiles_refused: RefusalCounter,
    /// `pfnAtomicCopyBufferRegion` was called and refused. **Expected 0** — the
    /// cross-adapter atomic copy needs a shared heap this stack does not
    /// advertise. ⛔ A hit is a real exposure and not merely a gap: the engine's
    /// own `AtomicCopyBufferUINT` is an empty `FIXME` stub, so *no* answer this
    /// driver can give makes the copy happen. See the slot's doc.
    ///
    /// ⚠ **Re-graded consequence**, as `CopyTilesRefused`: the list is now
    /// quarantined too, so the application learns instead of reading stale
    /// destination bytes.
    atomic_copy_refused: RefusalCounter,
    /// `pfnResourceResolveSubresourceRegion` carried a `D3D12DDI_RESOLVE_MODE`
    /// this header does not name. **Expected 0.**
    resolve_mode_unsupported: RefusalCounter,
    /// The `QueryInterface` for `ID3D12GraphicsCommandList1` failed on a live
    /// vkd3d command list. **Expected 0** — vkd3d's list vtable is built as one
    /// chain up to `ID3D12GraphicsCommandList10`
    /// (`libs/vkd3d/command.c:22049-22059`), so this cannot fail for a real
    /// list; it is the instrument that says the object was not one.
    command_list1_unavailable: RefusalCounter,
    /// `pfnResourceBarrier` was entered at all — a **traffic** counter, not a
    /// refusal, and one of exactly two in this set.
    ///
    /// ⭐ **Expected non-zero on any workload that draws**, and it is half of the
    /// answer to *which barrier arm is live*. Read it against
    /// `EnhancedBarrierCalled`: the two are mutually exclusive by construction
    /// (`DDI_REFERENCE.md` §9.10.1), so exactly one should move, and **both
    /// reading 0 means no barrier reached this driver at all** — which is a
    /// finding about the workload or the table fill rather than about barriers.
    /// A zero here on a frame that rendered would say the runtime is not using
    /// the arm this driver's cap selects. Precedent for a non-refusal counter in
    /// a refusal set: `UMD12_REFUSALS.caps_calls`.
    legacy_barrier_called: RefusalCounter,
    /// `pfnResourceBarrier` arrived with a non-zero `Count` and a null array, or
    /// a count above `MAX_BARRIERS`. **Expected 0.**
    legacy_barrier_bad_arg: RefusalCounter,
    /// A legacy transition or UAV barrier named a resource that did not resolve,
    /// and **that barrier was dropped**. ⛔ **Expected 0, and a hit is a
    /// synchronisation hazard**, not a cosmetic one: on this stack a lost
    /// barrier is a venus-side write/read race with no guest-visible error
    /// (`DDI_REFERENCE.md` §9.10).
    ///
    /// ⚠ It is counted **and reported** — the surviving barriers in the call are
    /// issued first, then one command-list error is raised. Those were never
    /// alternatives (`report_error` returns and aborts nothing), and an earlier
    /// revision of this doc claimed they were. See [`resource_barrier`].
    ///
    /// ⚠ Read it beside L4's `ResourceCreateEngineFailed` and
    /// `HeapSpanBufferUnavailable`: a resource whose creation half-failed still
    /// gets a live handle with an empty `ResourceState::resource`
    /// (`resource12.rs:498-504`), and this is where that lands.
    legacy_barrier_resource_missing: RefusalCounter,
    /// A `0022_RANGED` or deprecated `ALIASING` barrier was lowered to the API's
    /// both-NULL aliasing barrier, i.e. a global memory barrier.
    ///
    /// ⚠ **Expected non-zero on any workload that aliases placed resources**, and
    /// that is the correct reading rather than an exposure: the lowering is a
    /// *widening*, so it can only over-synchronise. It becomes a performance
    /// finding — not a correctness one — if it moves per frame, and the fix
    /// would be to carry the DDI's byte range into a narrower Vulkan barrier,
    /// which the D3D12 API cannot express and vkd3d therefore cannot be told.
    legacy_barrier_ranged_widened: RefusalCounter,
    /// A legacy barrier carried `FLAG_0022_ATOMIC_COPY` or `FLAG_0022_ALIASING`,
    /// and the flag was masked off because `D3D12_RESOURCE_BARRIER_FLAGS` names
    /// neither. ⚠ Expected non-zero exactly when a workload uses the atomic-copy
    /// pairing — in which case `AtomicCopyRefused` moves too, and the pair is
    /// the whole story. `FLAG_0022_ALIASING` accompanies a RANGED barrier this
    /// lane already widens, so it costs nothing on its own.
    legacy_barrier_flags_dropped: RefusalCounter,
    /// A legacy barrier carried a `D3D12DDI_RESOURCE_BARRIER_TYPE` this header
    /// does not name, and **the barrier was dropped**. **Expected 0**; same
    /// hazard as `LegacyBarrierResourceMissing`, and reported the same way — the
    /// call's survivors first, then one command-list error.
    legacy_barrier_type_unknown: RefusalCounter,
    /// `pfnBarrier` was entered at all — the second **traffic** counter, and
    /// `LegacyBarrierCalled`'s twin.
    ///
    /// ⛔ **Expected 0 while `caps12.rs` reports `EnhancedBarriersSupported = 0`**
    /// — at that cap the runtime lowers every `ResourceBarrier` to the legacy
    /// slot and *"legacy barrier DDI's are never invoked"* reads in the other
    /// direction. A non-zero reading with the cap still 0 is a finding about the
    /// runtime, and it is also the signal that the whole enhanced arm below is
    /// live and its counters mean something. ⚠ When L1 raises the cap this
    /// becomes the *expected non-zero* counter and `LegacyBarrier*` become the
    /// zeros; the two families swap, and neither is deleted.
    enhanced_barrier_called: RefusalCounter,
    /// `pfnBarrier` arrived with a non-zero `NumBarriers` and a null array, or a
    /// count above `MAX_BARRIERS`. **Expected 0.**
    enhanced_barrier_bad_arg: RefusalCounter,
    /// An enhanced texture or buffer barrier named a resource that did not
    /// resolve. **Expected 0**; same hazard as `LegacyBarrierResourceMissing`.
    ///
    /// ⚠ The two arms answer it differently, and the difference is the engine's:
    /// a **texture** barrier is dropped, because a layout transition with no
    /// image has nothing left; a **buffer** barrier is still forwarded with a
    /// null `pResource`, because vkd3d lowers it to a global barrier and never
    /// reads the resource (`libs/vkd3d/command.c:21569-21578`), so dropping it
    /// would lose an ordering edge for nothing.
    ///
    /// ⛔ **Only the texture arm reports.** One command-list error is raised per
    /// call, after the survivors, and exactly when something was *lost*; the
    /// buffer arm loses nothing, so a reading that moved only through buffer
    /// barriers leaves the list usable. That asymmetry is deliberate and it is
    /// the reason this counter alone cannot tell you whether the list was
    /// quarantined -- read it with `EnhancedBarrierTypeUnknown` and the log
    /// line, which names the arm.
    enhanced_barrier_resource_missing: RefusalCounter,
    /// An enhanced texture barrier carried one of the five **DDI-only** layout
    /// values and it was mapped to its API counterpart.
    ///
    /// ⚠ Expected non-zero once the cap is raised, on any workload that mixes
    /// enhanced barriers with legacy-state resources — the values exist
    /// precisely to carry the legacy model's promotion/decay ambiguity across
    /// the DDI. Every mapping is a widening (see [`api_barrier_layout`]), so a
    /// hit is information rather than an exposure; a hit while
    /// `EnhancedBarrierCalled` reads 0 would be impossible and is worth a look.
    enhanced_barrier_layout_legacy_mapped: RefusalCounter,
    /// An enhanced `RANGED` barrier was lowered to a full `SYNC_ALL` /
    /// `ACCESS_COMMON` global barrier. ⚠ Expected non-zero when a workload uses
    /// ranged barriers, and it is a widening rather than a loss — the DDI
    /// payload carries no sync and no access to narrow it with.
    enhanced_barrier_ranged_widened: RefusalCounter,
    /// An enhanced barrier carried a `D3D12DDI_BARRIER_TYPE` this header does not
    /// name, and the barrier was dropped. **Expected 0**; reported once per call
    /// after the survivors, as `LegacyBarrierTypeUnknown` is.
    enhanced_barrier_type_unknown: RefusalCounter,
    /// The `QueryInterface` for `ID3D12GraphicsCommandList7` failed on a live
    /// vkd3d command list. **Expected 0** for the same reason as
    /// `CommandList1Unavailable` — vkd3d's chain reaches
    /// `ID3D12GraphicsCommandList10`. ⛔ It is nevertheless the counter that
    /// would fire first if the cap were raised against an engine build without
    /// the enhanced-barrier entry point, which is why it is separate.
    command_list7_unavailable: RefusalCounter,
    /// A query slot could not resolve its `D3D12DDI_HQUERYHEAP` to a live
    /// `ID3D12QueryHeap`. **Expected 0** — the runtime only records against a
    /// heap `pfnCreateQueryHeap` returned `S_OK` for. ⚠ Read it beside L7's
    /// `QueryHeapEngineFailed`, which is where the creation half fails.
    query_heap_missing: RefusalCounter,
    /// A query slot carried a `D3D12DDI_QUERY_TYPE` this header does not name.
    /// **Expected 0.**
    query_type_unsupported: RefusalCounter,
    /// `pfnResolveQueryData`'s destination buffer did not resolve to an
    /// `ID3D12Resource`. **Expected 0.**
    resolve_query_dest_missing: RefusalCounter,
    /// `pfnSetPredication` carried a `D3D12DDI_PREDICATION_OP` that is neither
    /// `EQUAL_ZERO` nor `NOT_EQUAL_ZERO`. **Expected 0** — the DDI enum has
    /// exactly two enumerators.
    predication_op_unsupported: RefusalCounter,
    /// `pfnSetPredication` named a non-null buffer that did not resolve.
    /// **Expected 0.** ⚠ A *null* buffer is the legal "stop predicating" form
    /// and does not reach this counter.
    predication_resource_missing: RefusalCounter,
    /// A command-list slot needed to report and could not reach the device its
    /// list was created on, so the callback table was unreachable.
    /// **Expected 0.**
    ///
    /// ⚠ The device is resolved only to reach `um_callbacks`; the error itself
    /// is scoped to the list. Read it with `L3cCommandListMissing`, which is the
    /// same failure one handle earlier.
    set_error_no_device: RefusalCounter,
    /// A command-list slot needed `pfnSetCommandListErrorCb` and the runtime's
    /// `D3D12DDI_CORELAYER_DEVICECALLBACKS_0062` carried none.
    ///
    /// ⛔ **Expected 0**, and **re-graded**: it used to read *"needed
    /// `pfnSetErrorCb` ... the only error channel this whole table has"*, which
    /// named the wrong callback on a premise the `PARALLEL.md` §10 review
    /// disproved. The correct callback is the **second** member of that struct,
    /// one below `pfnSetErrorCb` and one above the
    /// `pfnSetCommandListDDITableCb` this driver already reads out of it
    /// (`queue::set_command_list_ddi_table`) — so a hit here means the runtime
    /// supplied a table with a hole in the middle.
    ///
    /// ⚠ The **consequence** changed with the channel, and that is the part
    /// worth re-reading. A hit no longer means "a device that never dies"; it
    /// means a recording failure the runtime never learns about, so the list is
    /// **not** quarantined, `Close()` returns success, and the application
    /// executes commands this driver knows are wrong. That is strictly worse
    /// than the old reading and is why it stays a named counter rather than a
    /// silent `else`.
    set_error_cb_absent: RefusalCounter,
}

pub(crate) static L3C_REFUSALS: L3cRefusals = L3cRefusals {
    command_list_missing: RefusalCounter::new("L3cCommandListMissing"),
    copy_bad_arg: RefusalCounter::new("L3cCopyBadArg"),
    copy_resource_missing: RefusalCounter::new("L3cCopyResourceMissing"),
    copy_layout_unknown: RefusalCounter::new("L3cCopyLayoutUnknown"),
    copy_box_negative: RefusalCounter::new("L3cCopyBoxNegative"),
    copy_texture_block_physical_layout: RefusalCounter::new("L3cCopyTextureBlockPhysicalLayout"),
    copy_slice_pitch_unrepresentable: RefusalCounter::new("L3cCopySlicePitchUnrepresentable"),
    copy_tiles_refused: RefusalCounter::new("L3cCopyTilesRefused"),
    atomic_copy_refused: RefusalCounter::new("L3cAtomicCopyRefused"),
    resolve_mode_unsupported: RefusalCounter::new("L3cResolveModeUnsupported"),
    command_list1_unavailable: RefusalCounter::new("L3cCommandList1Unavailable"),
    legacy_barrier_called: RefusalCounter::new("L3cLegacyBarrierCalled"),
    legacy_barrier_bad_arg: RefusalCounter::new("L3cLegacyBarrierBadArg"),
    legacy_barrier_resource_missing: RefusalCounter::new("L3cLegacyBarrierResourceMissing"),
    legacy_barrier_ranged_widened: RefusalCounter::new("L3cLegacyBarrierRangedWidened"),
    legacy_barrier_flags_dropped: RefusalCounter::new("L3cLegacyBarrierFlagsDropped"),
    legacy_barrier_type_unknown: RefusalCounter::new("L3cLegacyBarrierTypeUnknown"),
    enhanced_barrier_called: RefusalCounter::new("L3cEnhancedBarrierCalled"),
    enhanced_barrier_bad_arg: RefusalCounter::new("L3cEnhancedBarrierBadArg"),
    enhanced_barrier_resource_missing: RefusalCounter::new("L3cEnhancedBarrierResourceMissing"),
    enhanced_barrier_layout_legacy_mapped: RefusalCounter::new(
        "L3cEnhancedBarrierLayoutLegacyMapped",
    ),
    enhanced_barrier_ranged_widened: RefusalCounter::new("L3cEnhancedBarrierRangedWidened"),
    enhanced_barrier_type_unknown: RefusalCounter::new("L3cEnhancedBarrierTypeUnknown"),
    command_list7_unavailable: RefusalCounter::new("L3cCommandList7Unavailable"),
    query_heap_missing: RefusalCounter::new("L3cQueryHeapMissing"),
    query_type_unsupported: RefusalCounter::new("L3cQueryTypeUnsupported"),
    resolve_query_dest_missing: RefusalCounter::new("L3cResolveQueryDestMissing"),
    predication_op_unsupported: RefusalCounter::new("L3cPredicationOpUnsupported"),
    predication_resource_missing: RefusalCounter::new("L3cPredicationResourceMissing"),
    set_error_no_device: RefusalCounter::new("L3cSetErrorNoDevice"),
    set_error_cb_absent: RefusalCounter::new("L3cSetErrorCbAbsent"),
};

/// L3c's refusal counters, printed by `crate::log_refusal_summary` at this
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
pub(crate) static REFUSALS: &[&RefusalCounter] = &[
    &L3C_REFUSALS.command_list_missing,
    &L3C_REFUSALS.copy_bad_arg,
    &L3C_REFUSALS.copy_resource_missing,
    &L3C_REFUSALS.copy_layout_unknown,
    &L3C_REFUSALS.copy_box_negative,
    &L3C_REFUSALS.copy_texture_block_physical_layout,
    &L3C_REFUSALS.copy_slice_pitch_unrepresentable,
    &L3C_REFUSALS.copy_tiles_refused,
    &L3C_REFUSALS.atomic_copy_refused,
    &L3C_REFUSALS.resolve_mode_unsupported,
    &L3C_REFUSALS.command_list1_unavailable,
    &L3C_REFUSALS.legacy_barrier_called,
    &L3C_REFUSALS.legacy_barrier_bad_arg,
    &L3C_REFUSALS.legacy_barrier_resource_missing,
    &L3C_REFUSALS.legacy_barrier_ranged_widened,
    &L3C_REFUSALS.legacy_barrier_flags_dropped,
    &L3C_REFUSALS.legacy_barrier_type_unknown,
    &L3C_REFUSALS.enhanced_barrier_called,
    &L3C_REFUSALS.enhanced_barrier_bad_arg,
    &L3C_REFUSALS.enhanced_barrier_resource_missing,
    &L3C_REFUSALS.enhanced_barrier_layout_legacy_mapped,
    &L3C_REFUSALS.enhanced_barrier_ranged_widened,
    &L3C_REFUSALS.enhanced_barrier_type_unknown,
    &L3C_REFUSALS.command_list7_unavailable,
    &L3C_REFUSALS.query_heap_missing,
    &L3C_REFUSALS.query_type_unsupported,
    &L3C_REFUSALS.resolve_query_dest_missing,
    &L3C_REFUSALS.predication_op_unsupported,
    &L3C_REFUSALS.predication_resource_missing,
    &L3C_REFUSALS.set_error_no_device,
    &L3C_REFUSALS.set_error_cb_absent,
];
