use crate::forward;

/// Dcomp present vehicle (road 4 unit 2), in-process export for the ICD's
/// WSI: hand over the frame to present — venus resid + WS1 #4 fence value +
/// geometry + the creator's exact allocation identity — immediately before
/// calling Present() on the vehicle swapchain ON THE SAME THREAD. The next
/// dxgi_present on this thread consumes the slot (see
/// forward::set_present_source / vehicle_present_prepare).
/// Returns 0 = stored, 1 = stored but overwrote a pending source (counted
/// contract violation), -1 = refused (zero resid/geometry).
#[no_mangle]
pub extern "system" fn helios_umd_set_present_source_v2(
    resid: u32,
    fence_value: u64,
    width: u32,
    height: u32,
    dxgi_format: u32,
    alloc_size: u64,
    memory_type_index: u32,
    semaphore_handle: usize,
) -> i32 {
    forward::set_present_source(
        resid,
        fence_value,
        width,
        height,
        dxgi_format,
        alloc_size,
        memory_type_index,
        semaphore_handle,
        0, // Legacy v2 has no exact image-template contract.
        false,
    )
}

/// Exact dedicated-image vehicle import. All source and semaphore borrows
/// end when the immediately following same-thread Present returns.
///
/// # Safety
/// `source_image_create_info` is a live VkImageCreateInfo, including its pNext
/// chain and pointed-to arrays, through that Present. The ICD owns the chain
/// throughout the call; native texture creation deep-copies supported metadata.
#[no_mangle]
pub unsafe extern "system" fn helios_umd_set_present_source_v3(
    resid: u32,
    fence_value: u64,
    width: u32,
    height: u32,
    dxgi_format: u32,
    alloc_size: u64,
    memory_type_index: u32,
    semaphore_handle: usize,
    source_image_create_info: usize,
) -> i32 {
    if source_image_create_info == 0 {
        forward::EXT_SOURCE_REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        crate::log_error!("set_present_source_v3 REFUSED: null image create-info");
        return -1;
    }
    forward::set_present_source(
        resid,
        fence_value,
        width,
        height,
        dxgi_format,
        alloc_size,
        memory_type_index,
        semaphore_handle,
        source_image_create_info,
        false,
    )
}

/// v3's exact image template plus matched EXTERNAL source ownership. The
/// producer signal must follow the source's GENERAL-layout release; the ICD
/// must not recycle it until the helper's fixed copy completion is proven.
///
/// # Safety
/// The template and nested pointers are live through same-thread Present,
/// and the source satisfies the external ownership contract above.
#[no_mangle]
pub unsafe extern "system" fn helios_umd_set_present_source_v4(
    resid: u32,
    fence_value: u64,
    width: u32,
    height: u32,
    dxgi_format: u32,
    alloc_size: u64,
    memory_type_index: u32,
    semaphore_handle: usize,
    source_image_create_info: usize,
) -> i32 {
    if source_image_create_info == 0 {
        forward::EXT_SOURCE_REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        crate::log_error!("set_present_source_v4 REFUSED: null image create-info");
        return -1;
    }
    forward::set_present_source(
        resid,
        fence_value,
        width,
        height,
        dxgi_format,
        alloc_size,
        memory_type_index,
        semaphore_handle,
        source_image_create_info,
        true,
    )
}

/// Companion export: bounded wait (µs) until the last vehicle present on
/// THIS thread — the frame copy included — completed on the GPU. The ICD's
/// present worker calls this after Present() returns and only then recycles
/// the frame image, closing the copy-vs-rerender race. Returns 0 =
/// complete, 1 = timeout (counted caller-side), -1 = no vehicle present
/// recorded on this thread.
#[no_mangle]
pub extern "system" fn helios_umd_wait_last_present(timeout_us: u32) -> i32 {
    forward::wait_last_present(timeout_us)
}

/// Fixed-copy completion protocol. The helper device stays retained by WSI
/// throughout the same-thread set/Present/clear/wait sequence. 0 = complete,
/// 1 = pending (safe to retry the SAME captured submission), negative = error.
/// This separate export prevents new ICDs retrying the older ambiguous wait.
#[no_mangle]
pub extern "system" fn helios_umd_wait_present_copy_v2(timeout_us: u32) -> i32 {
    forward::wait_last_present(timeout_us)
}

/// Ends the same-thread borrowed handle scope. See forward::clear_present_source.
#[no_mangle]
pub extern "system" fn helios_umd_clear_present_source_v2() -> i32 {
    forward::clear_present_source()
}
