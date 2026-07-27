//! Registry-gated scanout-plane diagnostic.
//!
//! This is not a presentation path. `ScanoutDiag=1` creates one KMD-owned,
//! CPU-filled Venus blob and binds it to virtio scanout 0 once so we can isolate
//! whether QEMU/virgl displays any blob from this miniport at all. `ScanoutDiag=2`
//! keeps that blob pinned after later OS scanout attempts. `ScanoutDiag=4` uses a
//! DRM-modifier linear Vulkan image instead of raw `VkDeviceMemory`. Production
//! boots leave it off.

use core::ffi::c_void;
use core::sync::atomic::{fence, Ordering};

use wdk_sys::ntddk::{MmMapIoSpace, MmUnmapIoSpace};
use wdk_sys::{_MEMORY_CACHING_TYPE, PHYSICAL_ADDRESS};

use crate::adapter::AdapterContext;

fn diag_mode() -> u32 {
    crate::diag::read_config_dword(b"ScanoutDiag", 0)
}

fn uses_cpu_filled_cross_device_blob(mode: u32) -> bool {
    mode == 9 || mode == 11
}

fn scanout_format(mode: u32) -> u32 {
    if mode == 11 || mode == 16 {
        helios_protocol::VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM
    } else {
        helios_protocol::VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM
    }
}

unsafe fn fill_bgra_bars(
    va: *mut u8,
    len: usize,
    width: u32,
    height: u32,
    pitch: u32,
    offset: u32,
) -> bool {
    let row_bytes = width.saturating_mul(4) as usize;
    let pitch = pitch as usize;
    let height = height as usize;
    let offset = offset as usize;
    let bytes = match pitch
        .saturating_mul(height.saturating_sub(1))
        .checked_add(row_bytes)
        .and_then(|used| offset.checked_add(used))
    {
        Some(bytes) => bytes,
        None => return false,
    };
    if va.is_null() || row_bytes == 0 || pitch < row_bytes || bytes > len {
        return false;
    }

    let mut y = 0usize;
    while y < height {
        // SAFETY: validated pitch*height <= len, so this row starts in the map.
        let row = unsafe { va.add(offset + y * pitch) };
        let mut x = 0usize;
        while x < width as usize {
            let bar = (x * 4) / (width as usize).max(1);
            let checker = ((x >> 5) ^ (y >> 5)) & 1;
            let color = match bar {
                0 => 0xFFFF_00FFu32, // magenta
                1 => 0xFF00_FF00u32, // green
                2 => 0xFFFF_FFFFu32, // white
                _ => {
                    if checker == 0 {
                        0xFF00_0000u32
                    } else {
                        0xFF40_4040u32
                    }
                }
            };
            // SAFETY: x < width and pitch >= width*4 keep this u32 inside row.
            unsafe { core::ptr::write_volatile(row.add(x * 4) as *mut u32, color) };
            x += 1;
        }
        y += 1;
    }
    fence(Ordering::SeqCst);
    true
}

unsafe fn sample_bgra(va: *mut u8, len: usize, pitch: u32, offset: u32, width: u32, height: u32) {
    let pitch = pitch as usize;
    let offset = offset as usize;
    let width = width as usize;
    let height = height as usize;
    if va.is_null() || width == 0 || height == 0 || pitch < width.saturating_mul(4) {
        return;
    }
    let p0_end = offset.saturating_add(4);
    let center = offset
        .saturating_add((height / 2).saturating_mul(pitch))
        .saturating_add((width / 2).saturating_mul(4));
    if p0_end <= len {
        let p0 = unsafe { core::ptr::read_volatile(va.add(offset) as *const u32) };
        crate::diag::record_named_bytes(b"SdgP0", p0);
    }
    if center.saturating_add(4) <= len {
        let ctr = unsafe { core::ptr::read_volatile(va.add(center) as *const u32) };
        crate::diag::record_named_bytes(b"SdgCtr", ctr);
    }
}

pub(crate) fn maybe_run(adapter: &AdapterContext) {
    // SAFETY: PLACEHOLDER (R614) — the audited mint for this path arrives with
    // its caller-group commit (StartDevice).
    let passive = unsafe { crate::irql::PassiveLevel::assume() };
    let mode = diag_mode();
    crate::diag::record_named_bytes(b"SdgM", mode);
    crate::diag::record_named_bytes(b"SdgErr", 0);
    crate::diag::record_named_bytes(b"SdgGpu", 0);
    crate::diag::record_named_bytes(b"SdgGpF", 0);
    crate::diag::record_named_bytes(b"SdgGpW", 0);
    crate::diag::record_named_bytes(b"SdgBFl", 0);
    crate::diag::record_named_bytes(b"SdgUuid", 0);
    crate::diag::record_named_bytes(b"SdgFmt", 0);
    crate::diag::record_named_bytes(b"SdgStg", 0);
    crate::diag::record_named_bytes(b"SdgRid", 0);
    crate::diag::record_named_bytes(b"SdgBid", 0);
    crate::diag::record_named_bytes(b"SdgPch", 0);
    crate::diag::record_named_bytes(b"SdgOff", 0);
    crate::diag::record_named_bytes(b"SdgMap", 0);
    crate::diag::record_named_bytes(b"SdgFill", 0);
    crate::diag::record_named_bytes(b"SdgSet", 0);
    crate::diag::record_named_bytes(b"SdgFlu", 0);
    // Linear-scanout (mode 16) per-stage breadcrumbs — cleared each run so a
    // fresh boot never inherits a stale stage/VkResult from a prior boot
    // (registry value names persist; trust same-boot values only).
    crate::diag::record_named_bytes(b"SdgLStg", 0);
    crate::diag::record_named_bytes(b"SdgLReq", 0);
    crate::diag::record_named_bytes(b"SdgLBit", 0);
    crate::diag::record_named_bytes(b"SdgLTyc", 0);
    crate::diag::record_named_bytes(b"SdgLImg", 0);
    crate::diag::record_named_bytes(b"SdgLMem", 0);
    crate::diag::record_named_bytes(b"SdgLPch", 0);
    crate::diag::record_named_bytes(b"SdgLOff", 0);
    if mode == 0 || !adapter.display_half() {
        return;
    }

    let (width, height) = adapter.display_mode();
    if mode == 7 {
        match crate::virtio::ctrl::diagnostic_2d_scanout(adapter, width, height) {
            Ok(resource_id) => {
                crate::diag::record_named_bytes(b"S2dRid", resource_id);
                crate::diag::record_named_bytes(b"S2dSet", 1);
                adapter
                    .diag_scanout_wh
                    .store(((width as u64) << 32) | height as u64, Ordering::Release);
                adapter
                    .diag_scanout_resource
                    .store(resource_id, Ordering::Release);
                adapter.remember_scanout_blob(resource_id, width, height);
            }
            Err(_) => {
                crate::diag::record_named_bytes(b"S2dSet", 0xE);
                crate::diag::record_named_bytes(b"SdgErr", 7);
            }
        }
        return;
    }
    if mode == 12 || mode == 13 {
        let format = scanout_format(mode);
        let blob_mem = if mode == 13 {
            helios_protocol::VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST
        } else {
            helios_protocol::VIRTIO_GPU_BLOB_MEM_GUEST
        };
        crate::diag::record_named_bytes(b"SdgFmt", format);
        crate::diag::record_named_bytes(b"SdgMt", blob_mem);
        crate::diag::record_named_bytes(
            b"SdgBFl",
            helios_protocol::VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE,
        );
        match crate::virtio::ctrl::diagnostic_guest_blob_scanout(
            adapter, width, height, format, blob_mem,
        ) {
            Ok((resource_id, pitch)) => {
                crate::diag::record_named_bytes(b"SdgRid", resource_id);
                crate::diag::record_named_bytes(b"SdgBid", 0);
                crate::diag::record_named_bytes(b"SdgPch", pitch);
                crate::diag::record_named_bytes(b"SdgOff", 0);
                crate::diag::record_named_bytes(b"SdgMap", 3);
                crate::diag::record_named_bytes(b"SdgFill", 1);
                crate::diag::record_named_bytes(b"SdgSet", 1);
                crate::diag::record_named_bytes(b"SdgFlu", 1);
                adapter
                    .diag_scanout_wh
                    .store(((width as u64) << 32) | height as u64, Ordering::Release);
                adapter
                    .diag_scanout_layout
                    .store((pitch as u64) << 32, Ordering::Release);
                adapter
                    .diag_scanout_resource
                    .store(resource_id, Ordering::Release);
                adapter.remember_scanout_blob(resource_id, width, height);
            }
            Err(_) => {
                crate::diag::record_named_bytes(b"SdgErr", mode);
                crate::diag::record_named_bytes(b"SdgSet", 0xE);
            }
        }
        return;
    }
    if mode == 14 {
        let format = scanout_format(mode);
        let blob_flags = helios_protocol::VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
        crate::diag::record_named_bytes(b"SdgFmt", format);
        crate::diag::record_named_bytes(b"SdgMt", helios_protocol::VIRTIO_GPU_BLOB_MEM_HOST3D);
        crate::diag::record_named_bytes(b"SdgBFl", blob_flags);
        crate::diag::record_named_bytes(b"SdgStg", 1);
        let (resource_id, blob_id, pitch) =
            match crate::virtio::ctrl::diagnostic_virgl_host3d_blob(adapter, width, height, format)
            {
                Ok(result) => result,
                Err(_) => {
                    crate::diag::record_named_bytes(b"SdgErr", mode);
                    crate::diag::record_named_bytes(b"SdgSet", 0xE);
                    return;
                }
            };
        crate::diag::record_named_bytes(b"SdgStg", 2);
        crate::diag::record_named_bytes(b"SdgRid", resource_id);
        crate::diag::record_named_bytes(b"SdgBid", blob_id as u32);
        crate::diag::record_named_bytes(b"SdgPch", pitch);
        crate::diag::record_named_bytes(b"SdgOff", 0);

        crate::diag::record_named_bytes(b"SdgStg", 3);
        let prep = match crate::virtio::ctrl::map_blob_prepare(
            passive,
            adapter,
            crate::virtio::gpu::OwnerFilter::Any,
            resource_id,
        ) {
            Ok(prep) => prep,
            Err(_) => {
                crate::diag::record_named_bytes(b"SdgMap", 0xE);
                return;
            }
        };
        crate::diag::record_named_bytes(b"SdgMap", 1);
        crate::diag::record_named_bytes(b"SdgStg", 4);

        // SAFETY: a zeroed PHYSICAL_ADDRESS is the standard way to initialize this
        // WDK integer union before assigning QuadPart.
        let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
        pa.QuadPart = prep.gpa as i64;
        crate::diag::record_named_bytes(b"SdgMc", prep.map_cache);
        let caching = _MEMORY_CACHING_TYPE::MmWriteCombined;
        // SAFETY: RESOURCE_MAP_BLOB made [prep.gpa, prep.gpa+prep.size) a valid
        // host-visible BAR range; this PASSIVE_LEVEL diagnostic unmaps it below.
        let va = unsafe { MmMapIoSpace(pa, prep.size, caching) } as *mut u8;
        if va.is_null() {
            crate::diag::record_named_bytes(b"SdgFill", 0xE);
            return;
        }
        // SAFETY: `va` maps `prep.size` bytes from the blob; fill_bgra_bars validates
        // the requested width/height/pitch/offset against that mapped length before writes.
        let filled = unsafe { fill_bgra_bars(va, prep.size as usize, width, height, pitch, 0) };
        unsafe { sample_bgra(va, prep.size as usize, pitch, 0, width, height) };
        // SAFETY: `va` came from MmMapIoSpace above and is unmapped once here.
        unsafe { MmUnmapIoSpace(va as *mut c_void, prep.size) };
        crate::diag::record_named_bytes(b"SdgFill", if filled { 1 } else { 0xE });
        if !filled {
            return;
        }
        crate::diag::record_named_bytes(b"SdgStg", 5);

        let set = crate::virtio::ctrl::set_scanout_blob(
            adapter,
            resource_id,
            width,
            height,
            format,
            pitch,
            0,
        );
        crate::diag::record_named_bytes(b"SdgSet", if set.is_ok() { 1 } else { 0xE });
        if set.is_err() {
            crate::diag::record_named_bytes(b"SdgErr", mode);
            return;
        }
        crate::diag::record_named_bytes(b"SdgStg", 6);
        let flush = crate::virtio::ctrl::resource_flush(adapter, resource_id, width, height);
        crate::diag::record_named_bytes(b"SdgFlu", if flush.is_ok() { 1 } else { 0xE });
        crate::diag::record_named_bytes(b"SdgStg", 7);
        adapter
            .diag_scanout_wh
            .store(((width as u64) << 32) | height as u64, Ordering::Release);
        adapter
            .diag_scanout_layout
            .store((pitch as u64) << 32, Ordering::Release);
        adapter
            .diag_scanout_resource
            .store(resource_id, Ordering::Release);
        adapter.remember_scanout_blob(resource_id, width, height);
        return;
    }
    if mode == 15 {
        let format = scanout_format(mode);
        let blob_flags = helios_protocol::VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
        crate::diag::record_named_bytes(b"SdgFmt", format);
        crate::diag::record_named_bytes(
            b"SdgMt",
            helios_protocol::VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST,
        );
        crate::diag::record_named_bytes(b"SdgBFl", blob_flags);
        crate::diag::record_named_bytes(b"SdgStg", 1);
        let (resource_id, blob_id, pitch) =
            match crate::virtio::ctrl::diagnostic_virgl_host3d_guest_scanout(
                adapter, width, height, format,
            ) {
                Ok(result) => result,
                Err(_) => {
                    crate::diag::record_named_bytes(b"SdgErr", mode);
                    crate::diag::record_named_bytes(b"SdgSet", 0xE);
                    return;
                }
            };
        crate::diag::record_named_bytes(b"SdgStg", 2);
        crate::diag::record_named_bytes(b"SdgRid", resource_id);
        crate::diag::record_named_bytes(b"SdgBid", blob_id as u32);
        crate::diag::record_named_bytes(b"SdgPch", pitch);
        crate::diag::record_named_bytes(b"SdgOff", 0);
        crate::diag::record_named_bytes(b"SdgFill", 1);

        let set = crate::virtio::ctrl::set_scanout_blob(
            adapter,
            resource_id,
            width,
            height,
            format,
            pitch,
            0,
        );
        crate::diag::record_named_bytes(b"SdgSet", if set.is_ok() { 1 } else { 0xE });
        if set.is_err() {
            crate::diag::record_named_bytes(b"SdgErr", mode);
            crate::diag::record_named_bytes(b"SdgStg", 3);
            return;
        }
        crate::diag::record_named_bytes(b"SdgStg", 4);
        let flush = crate::virtio::ctrl::resource_flush(adapter, resource_id, width, height);
        crate::diag::record_named_bytes(b"SdgFlu", if flush.is_ok() { 1 } else { 0xE });
        crate::diag::record_named_bytes(b"SdgStg", if flush.is_ok() { 5 } else { 0xE });
        adapter
            .diag_scanout_wh
            .store(((width as u64) << 32) | height as u64, Ordering::Release);
        adapter
            .diag_scanout_layout
            .store((pitch as u64) << 32, Ordering::Release);
        adapter
            .diag_scanout_resource
            .store(resource_id, Ordering::Release);
        adapter.remember_scanout_blob(resource_id, width, height);
        return;
    }

    let raw_pitch = crate::ddi::create_allocation::cross_adapter_pitch(width);
    let raw_size = (raw_pitch as u64).saturating_mul(height as u64);
    if width == 0 || height == 0 || raw_size == 0 {
        crate::diag::record_named_bytes(b"SdgErr", 1);
        return;
    }

    let (blob, image_id, pitch, offset) = match adapter.with_venus_client(passive, |client| {
        if mode == 16 {
            client
                .allocate_linear_scanout_image_blob(adapter, width, height)
                .map(|scanout| {
                    (
                        scanout.blob,
                        scanout.image_id.get(),
                        scanout.row_pitch,
                        scanout.plane_offset,
                    )
                })
        } else if mode >= 4 {
            client
                .allocate_scanout_image_blob(adapter, width, height)
                .map(|scanout| {
                    (
                        scanout.blob,
                        scanout.image_id.get(),
                        scanout.row_pitch,
                        scanout.plane_offset,
                    )
                })
        } else {
            client
                .allocate_memory_blob(adapter, raw_size, true, mode >= 3)
                .map(|blob| (blob, 0, raw_pitch, 0))
        }
    }) {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            crate::diag::record_named_bytes(b"SdgErr", 2);
            return;
        }
        Err(_) => {
            crate::diag::record_named_bytes(b"SdgErr", 3);
            return;
        }
    };
    crate::diag::record_named_bytes(b"SdgRid", blob.res_id);
    crate::diag::record_named_bytes(b"SdgBid", blob.blob_id as u32);
    crate::diag::record_named_bytes(b"SdgPch", pitch);
    crate::diag::record_named_bytes(b"SdgOff", offset);

    if mode >= 5 && mode != 16 && !uses_cpu_filled_cross_device_blob(mode) {
        crate::diag::record_named_bytes(b"SdgMap", 2);
        crate::diag::record_named_bytes(b"SdgFill", 2);
    } else {
        let prep = match crate::virtio::ctrl::map_blob_prepare(
            passive,
            adapter,
            crate::virtio::gpu::OwnerFilter::Any,
            blob.res_id,
        ) {
            Ok(prep) => prep,
            Err(_) => {
                crate::diag::record_named_bytes(b"SdgMap", 0xE);
                return;
            }
        };
        crate::diag::record_named_bytes(b"SdgMap", 1);

        // SAFETY: a zeroed PHYSICAL_ADDRESS is the standard way to initialize this
        // WDK integer union before assigning QuadPart.
        let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
        pa.QuadPart = prep.gpa as i64;
        crate::diag::record_named_bytes(b"SdgMc", prep.map_cache);
        let caching = _MEMORY_CACHING_TYPE::MmWriteCombined;
        // SAFETY: RESOURCE_MAP_BLOB made [prep.gpa, prep.gpa+prep.size) a valid
        // host-visible BAR range; this PASSIVE_LEVEL diagnostic unmaps it below.
        let va = unsafe { MmMapIoSpace(pa, prep.size, caching) } as *mut u8;
        if va.is_null() {
            crate::diag::record_named_bytes(b"SdgFill", 0xE);
            return;
        }
        // SAFETY: `va` maps `prep.size` bytes from the blob; fill_bgra_bars validates
        // the requested width/height/pitch/offset against that mapped length before writes.
        let filled =
            unsafe { fill_bgra_bars(va, prep.size as usize, width, height, pitch, offset) };
        unsafe { sample_bgra(va, prep.size as usize, pitch, offset, width, height) };
        // SAFETY: `va` came from MmMapIoSpace above and is unmapped once here.
        unsafe { MmUnmapIoSpace(va as *mut c_void, prep.size) };
        crate::diag::record_named_bytes(b"SdgFill", if filled { 1 } else { 0xE });
        if !filled {
            return;
        }
    }

    if mode >= 4 && mode != 16 && !uses_cpu_filled_cross_device_blob(mode) {
        let gpu_clear =
            adapter.with_venus_client(passive, |client| {
                client.gpu_clear_scanout_image(adapter, image_id)
            });
        let ok = matches!(gpu_clear, Ok(Ok(())));
        crate::diag::record_named_bytes(b"SdgGpu", if ok { 1 } else { 0xE });
        if !ok {
            crate::diag::record_named_bytes(b"SdgErr", 4);
        }
    }

    let format = scanout_format(mode);
    crate::diag::record_named_bytes(b"SdgFmt", format);
    let set = crate::virtio::ctrl::set_scanout_blob(
        adapter,
        blob.res_id,
        width,
        height,
        format,
        pitch,
        offset,
    );
    crate::diag::record_named_bytes(b"SdgSet", if set.is_ok() { 1 } else { 0xE });
    if set.is_err() {
        return;
    }
    adapter.remember_diag_scanout_blob(blob.res_id, width, height, pitch, offset);
    adapter.remember_scanout_blob(blob.res_id, width, height);
    let flush = crate::virtio::ctrl::resource_flush(adapter, blob.res_id, width, height);
    crate::diag::record_named_bytes(b"SdgFlu", if flush.is_ok() { 1 } else { 0xE });
}

pub(crate) fn rebind_if_forced(adapter: &AdapterContext, via: u32) -> bool {
    if diag_mode() < 2 || !adapter.display_half() {
        return false;
    }
    let resource_id = adapter.diag_scanout_resource.load(Ordering::Acquire);
    let wh = adapter.diag_scanout_wh.load(Ordering::Acquire);
    let width = (wh >> 32) as u32;
    let height = wh as u32;
    if resource_id == 0 || width == 0 || height == 0 {
        crate::diag::record_named_bytes(b"SdgReb", 0xE0 | (via & 0xF));
        return false;
    }

    let layout = adapter.diag_scanout_layout.load(Ordering::Acquire);
    let pitch = (layout >> 32) as u32;
    let offset = layout as u32;
    let pitch = if pitch != 0 {
        pitch
    } else {
        crate::ddi::create_allocation::cross_adapter_pitch(width)
    };
    crate::diag::record_named_bytes(b"SdgRv", via);
    crate::diag::record_named_bytes(b"SdgRRid", resource_id);
    crate::diag::record_named_bytes(b"SdgRPc", pitch);
    crate::diag::record_named_bytes(b"SdgROf", offset);
    if diag_mode() == 7 {
        let set = crate::virtio::ctrl::set_scanout_2d(adapter, resource_id, width, height);
        crate::diag::record_named_bytes(b"SdgRSet", if set.is_ok() { 1 } else { 0xE });
        if set.is_err() {
            return true;
        }
        adapter.remember_scanout_blob(resource_id, width, height);
        let flush = crate::virtio::ctrl::resource_flush(adapter, resource_id, width, height);
        crate::diag::record_named_bytes(b"SdgRFlu", if flush.is_ok() { 1 } else { 0xE });
        return true;
    }
    let format = scanout_format(diag_mode());
    crate::diag::record_named_bytes(b"SdgFmt", format);
    let set = crate::virtio::ctrl::set_scanout_blob(
        adapter,
        resource_id,
        width,
        height,
        format,
        pitch,
        offset,
    );
    crate::diag::record_named_bytes(b"SdgRSet", if set.is_ok() { 1 } else { 0xE });
    if set.is_err() {
        return true;
    }
    adapter.remember_scanout_blob(resource_id, width, height);
    let flush = crate::virtio::ctrl::resource_flush(adapter, resource_id, width, height);
    crate::diag::record_named_bytes(b"SdgRFlu", if flush.is_ok() { 1 } else { 0xE });
    true
}
