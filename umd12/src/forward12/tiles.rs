//! Native tile mapping translation and exact queue admission. Heap/resource
//! ownership and mapping snapshots continue on the engine submission worker.
use super::*;
use crate::forward12::resource12;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_TILED_RESOURCE_COORDINATE, D3D12_TILE_RANGE_FLAGS, D3D12_TILE_RANGE_FLAG_NONE,
    D3D12_TILE_RANGE_FLAG_NULL, D3D12_TILE_RANGE_FLAG_REUSE_SINGLE_TILE,
    D3D12_TILE_RANGE_FLAG_SKIP, D3D12_TILE_REGION_SIZE,
};

pub(crate) fn coordinate(
    c: ddi12::D3D12DDI_TILED_RESOURCE_COORDINATE,
) -> D3D12_TILED_RESOURCE_COORDINATE {
    D3D12_TILED_RESOURCE_COORDINATE {
        X: c.X,
        Y: c.Y,
        Z: c.Z,
        Subresource: c.Subresource,
    }
}

pub(crate) fn region(s: ddi12::D3D12DDI_TILE_REGION_SIZE) -> D3D12_TILE_REGION_SIZE {
    D3D12_TILE_REGION_SIZE {
        NumTiles: s.NumTiles,
        UseBox: (s.UseBox != 0).into(),
        Width: s.Width,
        Height: s.Height,
        Depth: s.Depth,
    }
}

/// # Safety
/// A non-null pointer addresses `count` live DDI elements. No slice survives
/// this call; the returned API array owns every translated element.
unsafe fn translate_array<A: Copy, B>(
    ptr: *const A,
    count: u32,
    mut map: impl FnMut(A) -> Result<B, i32>,
) -> Result<Option<Vec<B>>, i32> {
    if ptr.is_null() {
        return Ok(None);
    }
    let count = count as usize;
    if count
        .checked_mul(core::mem::size_of::<A>())
        .is_none_or(|n| n > isize::MAX as usize)
    {
        return Err(E_INVALIDARG);
    }
    let mut result = Vec::new();
    result
        .try_reserve_exact(count)
        .map_err(|_| helios_umd_common::hr::E_OUTOFMEMORY)?;
    for i in 0..count {
        // SAFETY: the caller guarantees count live elements, checked arithmetic
        // bounds the pointer offset, and read_unaligned accepts DDI alignment.
        result.push(map(unsafe { core::ptr::read_unaligned(ptr.add(i)) })?);
    }
    Ok(Some(result))
}

fn array_ptr<T>(array: &Option<Vec<T>>) -> usize {
    array.as_ref().map_or(0, |values| values.as_ptr() as usize)
}

fn mapping_error(queue: &QueueState, hr: i32) {
    if hr == helios_umd_common::hr::E_OUTOFMEMORY {
        // Neither first-hit summaries nor formatted logging may allocate
        // between a failed reservation and cancellation/runtime notification.
        L2_REFUSALS.tile_mappings_refused.bump();
    } else {
        note_refusal(&L2_REFUSALS.tile_mappings_refused);
        log_error!("TileMappings: failed hr={:#010x}", hr as u32);
    }
    report_ecl_submit_error(queue, hr);
}

/// Commit one complete mapping operation under the same lock as ECL/Present.
/// # Safety
/// The queue and closure's borrowed engine objects/arrays live through the call.
unsafe fn admit_mapping(
    queue: &QueueState,
    commit: impl FnOnce(usize) -> Result<(u32, u32, u64), i32>,
) {
    let _execution = queue
        .execution
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: queue creation retained its live native device handle.
    let Some(dev) = (unsafe { device12::device(queue.h_device) }) else {
        mapping_error(queue, E_FAIL);
        return;
    };
    // SAFETY: unnamed manual-reset event, initially unsignaled and privately owned.
    let event =
        match unsafe { windows::Win32::System::Threading::CreateEventW(None, true, false, None) } {
            Ok(event) => AdmissionEvent(event),
            Err(error) => {
                mapping_error(queue, error.code().0);
                return;
            }
        };
    let boundary = match commit(event.0 .0 as usize) {
        Ok(boundary) => boundary,
        Err(hr) => {
            mapping_error(queue, hr);
            return;
        }
    };
    L2_REFUSALS.tile_mappings_forwarded.bump();
    // SAFETY: runtime submission callbacks stay on the entering DDI thread and
    // the exact context used for this operation's engine completion stream.
    // One boundary per producer, like the other two (queue.rs). The tile-mapping
    // path is the sparse one this stage already fixed, so it must carry the fence
    // too: a zero here would leave exactly this producer at worker completion.
    // SAFETY: `engine_queue` is the live engine queue this state owns.
    let gpu_wire_fence =
        unsafe { crate::bridge12::queue_gpu_fence(queue.engine_queue.as_raw() as usize) };
    match unsafe {
        submit_wddm_render(dev, queue, &ecl_submit_command(boundary, gpu_wire_fence), "TileMappings")
    } {
        WddmSubmit::Submitted => {
            // SAFETY: SignalAtSubmission follows Render on the same context;
            // this grants execution permission, not GPU completion.
            match unsafe { enqueue_runtime_admission(dev, queue, &event) } {
                Ok(()) => L2_REFUSALS.tile_mappings_admitted.bump(),
                Err(hr) => mapping_error(queue, hr),
            }
        }
        WddmSubmit::Refused(hr) => mapping_error(queue, hr),
        WddmSubmit::Unavailable => mapping_error(queue, E_NOTIMPL),
    }
}

/// # Safety
/// Live native queue/resource/optional heap and count-sized optional DDI arrays.
pub(super) unsafe extern "system" fn update_tile_mappings(
    h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    h_resource: ddi12::D3D12DDI_HRESOURCE,
    num_regions: ddi12::UINT,
    coords: *const ddi12::D3D12DDI_TILED_RESOURCE_COORDINATE,
    sizes: *const ddi12::D3D12DDI_TILE_REGION_SIZE,
    h_heap: ddi12::D3D12DDI_HHEAP,
    num_ranges: ddi12::UINT,
    range_flags: *const ddi12::D3D12DDI_TILE_RANGE_FLAGS,
    offsets: *const ddi12::UINT,
    counts: *const ddi12::UINT,
    flags: ddi12::D3D12DDI_TILE_MAPPING_FLAGS,
) {
    // SAFETY: live private queue block provided by the runtime.
    let Some(queue) = (unsafe { queue_state(h_queue) }) else {
        note_refusal(&L2_REFUSALS.tile_mappings_refused);
        return;
    };
    // SAFETY: live DDI resource/heap handles; borrowed until this call returns.
    let (resource, heap) = unsafe {
        (
            resource12::engine_resource(h_resource),
            resource12::engine_heap(h_heap),
        )
    };
    let Some(resource) = resource else {
        mapping_error(queue, E_INVALIDARG);
        return;
    };
    if (!h_heap.pDrvPrivate.is_null() && heap.is_none())
        || flags & !ddi12::D3D12DDI_TILE_MAPPING_FLAGS_D3D12DDI_TILE_MAPPING_FLAG_NO_HAZARD != 0
    {
        mapping_error(queue, E_INVALIDARG);
        return;
    }
    // SAFETY: optional runtime arrays have their declared counts; scalar fields
    // are copied by value into explicitly API-typed owned storage.
    let translated = unsafe {
        (|| {
            Ok::<_, i32>((
                translate_array(coords, num_regions, |c| Ok(coordinate(c)))?,
                translate_array(sizes, num_regions, |s| Ok(region(s)))?,
                translate_array(range_flags, num_ranges, |flag| match flag {
                    ddi12::D3D12DDI_TILE_RANGE_FLAGS_D3D12DDI_TILE_RANGE_FLAG_NONE => {
                        Ok(D3D12_TILE_RANGE_FLAG_NONE)
                    }
                    ddi12::D3D12DDI_TILE_RANGE_FLAGS_D3D12DDI_TILE_RANGE_FLAG_NULL => {
                        Ok(D3D12_TILE_RANGE_FLAG_NULL)
                    }
                    ddi12::D3D12DDI_TILE_RANGE_FLAGS_D3D12DDI_TILE_RANGE_FLAG_SKIP => {
                        Ok(D3D12_TILE_RANGE_FLAG_SKIP)
                    }
                    ddi12::D3D12DDI_TILE_RANGE_FLAGS_D3D12DDI_TILE_RANGE_FLAG_REUSE_SINGLE_TILE => {
                        Ok(D3D12_TILE_RANGE_FLAG_REUSE_SINGLE_TILE)
                    }
                    _ => Err(E_INVALIDARG),
                })?,
            ))
        })()
    };
    let (coords, sizes, range_flags): (_, _, Option<Vec<D3D12_TILE_RANGE_FLAGS>>) = match translated
    {
        Ok(arrays) => arrays,
        Err(hr) => {
            mapping_error(queue, hr);
            return;
        }
    };
    let update = crate::bridge12::TileUpdate {
        resource: resource.as_raw() as usize,
        region_count: num_regions,
        coords: array_ptr(&coords),
        sizes: array_ptr(&sizes),
        heap: heap.map_or(0, |heap| heap.as_raw() as usize),
        range_count: num_ranges,
        range_flags: array_ptr(&range_flags),
        offsets: offsets as usize,
        counts: counts as usize,
        flags,
    };
    // SAFETY: all arrays and borrowed objects remain live; engine copies and
    // retains dependencies before committing the operation to its worker.
    unsafe {
        admit_mapping(queue, |event| {
            crate::bridge12::update_tiles(queue.engine_queue.as_raw() as usize, &update, event)
        })
    };
}

/// # Safety
/// Live native queue/resources and pointers to individual coordinate/size inputs.
pub(super) unsafe extern "system" fn copy_tile_mappings(
    h_queue: ddi12::D3D12DDI_HCOMMANDQUEUE,
    h_dst: ddi12::D3D12DDI_HRESOURCE,
    dst_coord: *const ddi12::D3D12DDI_TILED_RESOURCE_COORDINATE,
    h_src: ddi12::D3D12DDI_HRESOURCE,
    src_coord: *const ddi12::D3D12DDI_TILED_RESOURCE_COORDINATE,
    size: *const ddi12::D3D12DDI_TILE_REGION_SIZE,
    flags: ddi12::D3D12DDI_TILE_MAPPING_FLAGS,
) {
    // SAFETY: live private queue block from this driver's creation.
    let Some(queue) = (unsafe { queue_state(h_queue) }) else {
        note_refusal(&L2_REFUSALS.tile_mappings_refused);
        return;
    };
    if dst_coord.is_null()
        || src_coord.is_null()
        || size.is_null()
        || flags & !ddi12::D3D12DDI_TILE_MAPPING_FLAGS_D3D12DDI_TILE_MAPPING_FLAG_NO_HAZARD != 0
    {
        mapping_error(queue, E_INVALIDARG);
        return;
    }
    // SAFETY: runtime supplied live resource private blocks for this call.
    let (Some(dst), Some(src)) = (unsafe { resource12::engine_resource(h_dst) }, unsafe {
        resource12::engine_resource(h_src)
    }) else {
        mapping_error(queue, E_INVALIDARG);
        return;
    };
    // SAFETY: non-null pointers to individual live inputs, read without alignment assumptions.
    let (dst_coord, src_coord, size) = unsafe {
        (
            coordinate(core::ptr::read_unaligned(dst_coord)),
            coordinate(core::ptr::read_unaligned(src_coord)),
            region(core::ptr::read_unaligned(size)),
        )
    };
    let copy = crate::bridge12::TileCopy {
        dst: dst.as_raw() as usize,
        dst_coord: core::ptr::from_ref(&dst_coord) as usize,
        src: src.as_raw() as usize,
        src_coord: core::ptr::from_ref(&src_coord) as usize,
        size: core::ptr::from_ref(&size) as usize,
        flags,
    };
    // SAFETY: all owned scalar descriptions and borrowed objects remain live
    // until the engine has copied and retained the committed operation.
    unsafe {
        admit_mapping(queue, |event| {
            crate::bridge12::copy_tiles(queue.engine_queue.as_raw() as usize, &copy, event)
        })
    };
}
