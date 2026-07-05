//! d3d10umddi device-funcs → D3D11 COM forwarders (pure Rust via the windows
//! crate). Each func reads its bindgen DDI arg struct, translates to a
//! windows-crate COM call on the DXVK `ID3D11Device`/`ID3D11DeviceContext`, and
//! stores the returned COM interface in the runtime-allocated DDI handle.
//!
//! DDI Usage/BindFlags/MiscFlags mirror the D3D11 API bit values (passthrough).
//! Resource/view handles store the raw COM pointer (8 bytes) in pDrvPrivate;
//! CalcPrivate*Size returns 8. Errors on VOID-returning Create* are dropped for
//! now (TODO: report via the device error callback) — a failed create leaves a
//! null handle.

use core::ffi::c_void;
use core::mem::ManuallyDrop;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::{IUnknown, Interface, PCSTR};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D::{
    D3D11_SRV_DIMENSION_BUFFER, D3D11_SRV_DIMENSION_BUFFEREX, D3D11_SRV_DIMENSION_TEXTURE1D,
    D3D11_SRV_DIMENSION_TEXTURE1DARRAY, D3D11_SRV_DIMENSION_TEXTURE2D,
    D3D11_SRV_DIMENSION_TEXTURE2DARRAY, D3D11_SRV_DIMENSION_TEXTURE3D,
    D3D11_SRV_DIMENSION_TEXTURECUBE, D3D11_SRV_DIMENSION_TEXTURECUBEARRAY,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

use helios_protocol::{
    HeliosWddmAllocMeta, HeliosWddmAllocPrivate, HeliosWddmOpenIdentity,
    HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY, HELIOS_WDDM_ALLOC_KIND_STANDARD,
    VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE, VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE,
    VIRTIO_GPU_BLOB_MEM_HOST3D, VIRTIO_GPU_MAP_CACHE_CACHED,
};

use crate::ddi;
use crate::device_funcs::HeliosDevice;
use crate::log_line;
use crate::present_gate_us;
use crate::present_sync_publish_enabled;
use crate::trace_line;

type Hdevice = ddi::D3D10DDI_HDEVICE;

static RESOURCE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static VIEW_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static WDDM_ALLOC_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static D3D11_1_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static COPY_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static MAP_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SHADER_BIND_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static DRAW_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static OM_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

struct ResourceState {
    com_raw: usize,
    allocation: ddi::D3DKMT_HANDLE,
    km_resource: ddi::D3DKMT_HANDLE,
    rt_resource: ddi::HANDLE,
    /// True when this UMD allocated `allocation` itself (pfnAllocateCb in
    /// `create_resource`); false for handles the runtime handed us at
    /// `open_resource`. `release_resource` may only pass allocation handles it
    /// created to pfnDeallocateCb's HandleList form — deallocating opened
    /// handles that way is what returned 0x80070057 and leaked the runtime's
    /// side of the open.
    owns_allocation: bool,
}

struct RtvState {
    com_raw: usize,
    allocation: ddi::D3DKMT_HANDLE,
    width: u32,
    height: u32,
    format: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct RuntimeAllocPrivate {
    alloc: HeliosWddmAllocPrivate,
    meta: HeliosWddmAllocMeta,
}

/// Legacy 24-byte trailer (geometry + bind/misc, no venus identity) written by
/// pre-identity driver builds. Parse-only.
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct StandardAllocMetaV2 {
    width: u32,
    height: u32,
    format: u32,
    pitch: u32,
    bind_flags: u32,
    misc_flags: u32,
}

/// Oldest legacy 16-byte trailer. Parse-only.
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct StandardAllocMetaV1 {
    width: u32,
    height: u32,
    format: u32,
    pitch: u32,
}

fn dxgi_to_d3dddi_format(format: u32) -> u32 {
    // KMD DescribeAllocation consumes legacy D3DDDIFORMAT values, not DXGI_FORMAT.
    // DWM/LG composition surfaces are 32-bpp BGRA, represented as A8R8G8B8 there.
    const DXGI_FORMAT_R8G8B8A8_UNORM: u32 = 28;
    const DXGI_FORMAT_B8G8R8A8_UNORM: u32 = 87;
    const D3DDDIFMT_A8R8G8B8: u32 = 21;

    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM => D3DDDIFMT_A8R8G8B8,
        _ => 0,
    }
}

fn d3dddi_to_dxgi_format(format: u32) -> DXGI_FORMAT {
    const D3DDDIFMT_A8R8G8B8: u32 = 21;

    match format {
        D3DDDIFMT_A8R8G8B8 => windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
        _ => windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
    }
}

/// Bytes per pixel of an (uncompressed) `DXGI_FORMAT`, for computing the WDDM
/// surface pitch. Only the single-byte channel block (R8_*/A8/R1, 60..=66) and
/// the wider HDR/deep formats matter for DWM composition; everything else
/// defaults to 4 (32-bpp BGRA/RGBA — the historical assumption). Over-reporting
/// is safe (the pitch only pads `linear_size`); under-reporting an A8 mask as
/// 4bpp is what made openers size these surfaces 4x too large.
fn dxgi_bytes_per_pixel(format: u32) -> u32 {
    match format {
        1..=4 => 16,   // R32G32B32A32_*
        5..=8 => 12,   // R32G32B32_*
        9..=14 => 8,   // R16G16B16A16_*
        15..=19 => 8,  // R32G32_*, R32G8X24 depth
        60..=66 => 1,  // R8_TYPELESS/UNORM/UINT/SNORM/SINT, A8_UNORM, R1_UNORM
        _ => 4,
    }
}

/// Parse the meta trailer at `base_off` bytes into the buffer, tolerating the
/// two legacy (shorter) layouts. Returns a zero-extended [`HeliosWddmAllocMeta`].
unsafe fn read_alloc_meta(ptr: *const c_void, size: u32, base_off: usize) -> Option<HeliosWddmAllocMeta> {
    let avail = (size as usize).checked_sub(base_off)?;
    let meta_ptr = (ptr as *const u8).add(base_off);
    if avail >= core::mem::size_of::<HeliosWddmAllocMeta>() {
        return Some(core::ptr::read_unaligned(meta_ptr as *const HeliosWddmAllocMeta));
    }
    if avail >= core::mem::size_of::<StandardAllocMetaV2>() {
        let legacy = core::ptr::read_unaligned(meta_ptr as *const StandardAllocMetaV2);
        return Some(HeliosWddmAllocMeta {
            width: legacy.width,
            height: legacy.height,
            format: legacy.format,
            pitch: legacy.pitch,
            bind_flags: legacy.bind_flags,
            misc_flags: legacy.misc_flags,
            venus_alloc_size: 0,
            memory_type_index: 0,
            dxgi_format: 0,
        });
    }
    if avail >= core::mem::size_of::<StandardAllocMetaV1>() {
        let legacy = core::ptr::read_unaligned(meta_ptr as *const StandardAllocMetaV1);
        return Some(HeliosWddmAllocMeta {
            width: legacy.width,
            height: legacy.height,
            format: legacy.format,
            pitch: legacy.pitch,
            // KMD standard allocations predate these trailer fields. Use the
            // composition-surface baseline so opened resources are renderable and
            // shader-readable instead of constructing a zero-usage texture.
            bind_flags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            misc_flags: 0,
            venus_alloc_size: 0,
            memory_type_index: 0,
            dxgi_format: 0,
        });
    }
    None
}

/// Parse the OPEN-time private data: the KMD's versioned [`HeliosWddmOpenIdentity`]
/// record (written in `DxgkDdiOpenAllocation` after validating the venus resource
/// is LIVE) plus the creator's meta trailer. This replaces the `_pad`-smuggling
/// heuristics: an open-time buffer either carries a valid identity or the open is
/// not backed by an identified venus resource.
unsafe fn read_open_identity(
    ptr: *const c_void,
    size: u32,
) -> Option<(HeliosWddmOpenIdentity, Option<HeliosWddmAllocMeta>)> {
    if ptr.is_null() || (size as usize) < core::mem::size_of::<HeliosWddmOpenIdentity>() {
        return None;
    }
    let ident = core::ptr::read_unaligned(ptr as *const HeliosWddmOpenIdentity);
    if !ident.is_valid() || ident.resource_id == 0 {
        return None;
    }
    let meta = read_alloc_meta(ptr, size, core::mem::size_of::<HeliosWddmOpenIdentity>());
    Some((ident, meta))
}

// --- handle <-> COM helpers -------------------------------------------------

unsafe fn d3d11_device(h: Hdevice) -> Option<ManuallyDrop<ID3D11Device>> {
    let hd = h.pDrvPrivate as *const HeliosDevice;
    if hd.is_null() {
        return None;
    }
    let p = (*hd).dxvk.d3d11_device_ptr();
    if p == 0 {
        return None;
    }
    Some(ManuallyDrop::new(ID3D11Device::from_raw(p as *mut c_void)))
}

/// Borrow the `HeliosDevice` behind a device handle (for the deferred
/// input-assembler state). Does not take ownership.
unsafe fn helios_device<'a>(h: Hdevice) -> Option<&'a HeliosDevice> {
    let hd = h.pDrvPrivate as *const HeliosDevice;
    if hd.is_null() {
        None
    } else {
        Some(&*hd)
    }
}

const E_FAIL: i32 = 0x8000_4005u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;

/// Report an error to the D3D11 runtime for a VOID-returning DDI via the
/// corelayer `pfnSetErrorCb`. The runtime fails the API call that invoked the
/// DDI (e.g. `OpenSharedResource`), which is the contractual way for
/// `open_resource`/`create_*` to fail loudly instead of leaving a null handle
/// the runtime will dereference.
unsafe fn set_runtime_error(h: Hdevice, hr: i32) {
    let Some(dev) = helios_device(h) else {
        return;
    };
    if dev.um_callbacks.is_null() {
        log_line("set_runtime_error: no corelayer callbacks");
        return;
    }
    // pfnSetErrorCb is the first member of every D3D11DDI_CORELAYER_DEVICECALLBACKS
    // revision, so reading it through the 11.0 layout is version-independent.
    let cb = &*(dev.um_callbacks as *const ddi::D3D11DDI_CORELAYER_DEVICECALLBACKS);
    if let Some(f) = cb.pfnSetErrorCb {
        f(
            ddi::D3D10DDI_HRTCORELAYER {
                handle: dev.h_rt_core_layer,
            },
            hr,
        );
    }
}

unsafe fn d3d11_context(h: Hdevice) -> Option<ManuallyDrop<ID3D11DeviceContext>> {
    let hd = h.pDrvPrivate as *const HeliosDevice;
    if hd.is_null() {
        return None;
    }
    let p = (*hd).dxvk.d3d11_context_ptr();
    if p == 0 {
        return None;
    }
    Some(ManuallyDrop::new(ID3D11DeviceContext::from_raw(
        p as *mut c_void,
    )))
}

unsafe fn d3d11_context1(h: Hdevice) -> Option<ID3D11DeviceContext1> {
    let context = d3d11_context(h)?;
    (*context).cast::<ID3D11DeviceContext1>().ok()
}

/// Store a COM interface's raw pointer (ownership transferred) in a DDI handle.
unsafe fn store_com<T: Interface>(handle_priv: *mut c_void, obj: T) {
    *(handle_priv as *mut *mut c_void) = obj.into_raw();
}

unsafe fn store_raw_com(handle_priv: *mut c_void, raw: usize) {
    if !handle_priv.is_null() {
        *(handle_priv as *mut *mut c_void) = raw as *mut c_void;
    }
}

unsafe fn clear_handle(handle_priv: *mut c_void) {
    if !handle_priv.is_null() {
        *(handle_priv as *mut *mut c_void) = core::ptr::null_mut();
    }
}

unsafe fn store_resource(
    handle_priv: *mut c_void,
    obj: ID3D11Resource,
    allocation: ddi::D3DKMT_HANDLE,
    km_resource: ddi::D3DKMT_HANDLE,
    rt_resource: ddi::HANDLE,
    owns_allocation: bool,
) {
    if handle_priv.is_null() {
        return;
    }
    let state = Box::new(ResourceState {
        com_raw: obj.into_raw() as usize,
        allocation,
        km_resource,
        rt_resource,
        owns_allocation,
    });
    *(handle_priv as *mut *mut c_void) = Box::into_raw(state) as *mut c_void;
}

unsafe fn stamp_dxvk_resource_kmt_handles(
    h: Hdevice,
    obj: &ID3D11Resource,
    local: ddi::D3DKMT_HANDLE,
    global: ddi::D3DKMT_HANDLE,
) {
    trace_line!(
        "DDI resource KMT stamp enter: raw_local=0x{:x} raw_global=0x{:x}",
        local, global
    );
    let local = if local != 0 { local } else { global };
    if local == 0 {
        trace_line!("DDI resource KMT stamp skipped: no usable handle");
        return;
    }
    let Some(dev) = helios_device(h) else {
        log_line("DDI resource KMT stamp skipped: missing device");
        return;
    };
    if dev
        .dxvk
        .set_resource_kmt_handles(obj.as_raw() as usize, local, global)
    {
        log_line(&format!(
            "DDI resource KMT handles stamped: local=0x{:x} global=0x{:x}",
            local, global
        ));
    } else {
        log_line(&format!(
            "DDI resource KMT handle stamp failed: local=0x{:x} global=0x{:x}",
            local, global
        ));
    }
}

unsafe fn load_resource(handle_priv: *mut c_void) -> Option<ManuallyDrop<ID3D11Resource>> {
    if handle_priv.is_null() {
        return None;
    }
    let state = *(handle_priv as *const *mut ResourceState);
    if state.is_null() || (*state).com_raw == 0 {
        return None;
    }
    Some(ManuallyDrop::new(ID3D11Resource::from_raw(
        (*state).com_raw as *mut c_void,
    )))
}

/// Raw ID3D11Resource COM pointer behind a DDI resource private handle
/// (0 when absent) — for bridge calls that inspect the DXVK image without
/// taking a COM reference.
unsafe fn resource_com_raw(handle_priv: *mut c_void) -> usize {
    if handle_priv.is_null() {
        return 0;
    }
    let state = *(handle_priv as *const *mut ResourceState);
    if state.is_null() {
        return 0;
    }
    (*state).com_raw
}

unsafe fn resource_allocation(handle_priv: *mut c_void) -> ddi::D3DKMT_HANDLE {
    if handle_priv.is_null() {
        return 0;
    }
    let state = *(handle_priv as *const *mut ResourceState);
    if state.is_null() {
        0
    } else {
        (*state).allocation
    }
}

unsafe fn dxvk_resource_memory_info(h: Hdevice, obj: &ID3D11Resource) -> (u64, u64, u64, u32) {
    let Some(dev) = helios_device(h) else {
        return (0, 0, 0, 0);
    };
    let mut memory = 0u64;
    let mut size = 0u64;
    let mut offset = 0u64;
    let mut resource_id = 0u32;
    if dev.dxvk.get_resource_memory_info(
        obj.as_raw() as usize,
        &mut memory,
        &mut size,
        &mut offset,
        &mut resource_id,
    ) {
        (memory, size, offset, resource_id)
    } else {
        (0, 0, 0, 0)
    }
}

unsafe fn resource_dimensions(handle_priv: *mut c_void) -> (u32, u32) {
    let Some(res) = load_resource(handle_priv) else {
        return (0, 0);
    };
    let Ok(tex) = (*res).cast::<ID3D11Texture2D>() else {
        return (0, 0);
    };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    (desc.Width, desc.Height)
}

unsafe fn store_rtv(
    handle_priv: *mut c_void,
    obj: ID3D11RenderTargetView,
    allocation: ddi::D3DKMT_HANDLE,
    width: u32,
    height: u32,
    format: u32,
) {
    if handle_priv.is_null() {
        return;
    }
    let state = Box::new(RtvState {
        com_raw: obj.into_raw() as usize,
        allocation,
        width,
        height,
        format,
    });
    *(handle_priv as *mut *mut c_void) = Box::into_raw(state) as *mut c_void;
}

unsafe fn load_rtv(handle_priv: *mut c_void) -> Option<ManuallyDrop<ID3D11RenderTargetView>> {
    if handle_priv.is_null() {
        return None;
    }
    let state = *(handle_priv as *const *mut RtvState);
    if state.is_null() || (*state).com_raw == 0 {
        return None;
    }
    Some(ManuallyDrop::new(ID3D11RenderTargetView::from_raw(
        (*state).com_raw as *mut c_void,
    )))
}

unsafe fn rtv_info(handle_priv: *mut c_void) -> (ddi::D3DKMT_HANDLE, u32, u32, u32) {
    if handle_priv.is_null() {
        return (0, 0, 0, 0);
    }
    let state = *(handle_priv as *const *mut RtvState);
    if state.is_null() {
        (0, 0, 0, 0)
    } else {
        (
            (*state).allocation,
            (*state).width,
            (*state).height,
            (*state).format,
        )
    }
}

unsafe fn release_rtv(handle_priv: *mut c_void) {
    if handle_priv.is_null() {
        return;
    }
    let state = *(handle_priv as *mut *mut RtvState);
    if !state.is_null() {
        if (*state).com_raw != 0 {
            drop(IUnknown::from_raw((*state).com_raw as *mut c_void));
        }
        drop(Box::from_raw(state));
        *(handle_priv as *mut *mut c_void) = core::ptr::null_mut();
    }
}

unsafe fn release_resource(h: Hdevice, handle_priv: *mut c_void) {
    if handle_priv.is_null() {
        return;
    }
    let state = *(handle_priv as *mut *mut ResourceState);
    if !state.is_null() {
        if (*state).allocation != 0 || !(*state).rt_resource.is_null() {
            if let Some(dev) = helios_device(h) {
                if !dev.kt_callbacks.is_null() {
                    if let Some(deallocate_cb) = (*dev.kt_callbacks).pfnDeallocateCb {
                        // D3DDDICB_DEALLOCATE contract: EITHER hResource (the
                        // runtime releases/closes every allocation it tracks
                        // for the resource — created AND opened instances) OR
                        // NumAllocations+HandleList with hResource NULL. Both
                        // together is E_INVALIDARG (the old 0x80070057, which
                        // also leaked opened resources).
                        let mut allocation = (*state).allocation;
                        let mut dealloc = if !(*state).rt_resource.is_null() {
                            ddi::D3DDDICB_DEALLOCATE {
                                hResource: (*state).rt_resource,
                                NumAllocations: 0,
                                HandleList: core::ptr::null_mut(),
                            }
                        } else if (*state).owns_allocation {
                            // Standalone allocation this UMD created itself.
                            ddi::D3DDDICB_DEALLOCATE {
                                hResource: core::ptr::null_mut(),
                                NumAllocations: 1,
                                HandleList: &mut allocation,
                            }
                        } else {
                            // An allocation handle we did not create and no
                            // runtime resource to hand back: nothing we may
                            // legally deallocate.
                            log_line(&format!(
                                "DDI deallocate_resource skip: not owner alloc=0x{:x} km=0x{:x}",
                                (*state).allocation,
                                (*state).km_resource
                            ));
                            ddi::D3DDDICB_DEALLOCATE {
                                hResource: core::ptr::null_mut(),
                                NumAllocations: 0,
                                HandleList: core::ptr::null_mut(),
                            }
                        };
                        if !dealloc.hResource.is_null() || dealloc.NumAllocations != 0 {
                            let hr = deallocate_cb(dev.h_rt_device, &mut dealloc);
                            trace_line!(
                                "DDI deallocate_resource: hr=0x{:08x} alloc=0x{:x} km=0x{:x} rt={:p} owned={}",
                                hr as u32,
                                (*state).allocation,
                                (*state).km_resource,
                                (*state).rt_resource,
                                (*state).owns_allocation
                            );
                        }
                    }
                }
            }
        }
        if (*state).com_raw != 0 {
            drop(IUnknown::from_raw((*state).com_raw as *mut c_void));
        }
        drop(Box::from_raw(state));
        *(handle_priv as *mut *mut c_void) = core::ptr::null_mut();
    }
}

/// Borrow the COM interface stored in a DDI handle (does not take ownership).
unsafe fn load_com<T: Interface>(handle_priv: *mut c_void) -> Option<ManuallyDrop<T>> {
    if handle_priv.is_null() {
        return None;
    }
    let raw = *(handle_priv as *const *mut c_void);
    if raw.is_null() {
        return None;
    }
    Some(ManuallyDrop::new(T::from_raw(raw)))
}

/// Release the COM interface stored in a DDI handle.
unsafe fn release_com(handle_priv: *mut c_void) {
    if handle_priv.is_null() {
        return;
    }
    let raw = *(handle_priv as *const *mut c_void);
    if !raw.is_null() {
        // Reconstitute owning ref and drop it.
        drop(IUnknown::from_raw(raw));
        *(handle_priv as *mut *mut c_void) = core::ptr::null_mut();
    }
}

fn cpu_access(usage: u32) -> u32 {
    // D3D11_USAGE: DEFAULT=0, IMMUTABLE=1, DYNAMIC=2, STAGING=3.
    match usage {
        2 => D3D11_CPU_ACCESS_WRITE.0 as u32,
        3 => (D3D11_CPU_ACCESS_WRITE.0 | D3D11_CPU_ACCESS_READ.0) as u32,
        _ => 0,
    }
}

fn api_bind_flags(ddi_bind: u32) -> u32 {
    const DDI_PIPELINE_MASK: u32 = 0x0000_007f;
    const DDI_BIND_PRESENT: u32 = 0x0000_0080;
    const DDI_BIND_UAV: u32 = 0x0000_0100;
    const DDI_BIND_DECODER: u32 = 0x0000_0200;
    const DDI_BIND_VIDEO_ENCODER: u32 = 0x0000_0400;
    const DDI_BIND_CAPTURE: u32 = 0x0000_0800;

    let mut out = ddi_bind & DDI_PIPELINE_MASK;
    if ddi_bind & DDI_BIND_UAV != 0 {
        out |= D3D11_BIND_UNORDERED_ACCESS.0 as u32;
    }
    if ddi_bind & DDI_BIND_DECODER != 0 {
        out |= D3D11_BIND_DECODER.0 as u32;
    }
    if ddi_bind & DDI_BIND_VIDEO_ENCODER != 0 {
        out |= D3D11_BIND_VIDEO_ENCODER.0 as u32;
    }
    let _ = DDI_BIND_PRESENT | DDI_BIND_CAPTURE;
    out
}

fn api_misc_flags(ddi_misc: u32, ddi_bind: u32, is_buffer: bool) -> u32 {
    const DDI_MISC_AUTO_GEN_MIP_MAP: u32 = 0x0000_0001;
    const DDI_MISC_SHARED: u32 = 0x0000_0002;
    const DDI_MISC_DRAWINDIRECT_ARGS: u32 = 0x0000_0010;
    const DDI_MISC_BUFFER_ALLOW_RAW_VIEWS: u32 = 0x0000_0020;
    const DDI_MISC_BUFFER_STRUCTURED: u32 = 0x0000_0040;
    const DDI_MISC_RESOURCE_CLAMP: u32 = 0x0000_0080;
    const DDI_MISC_SHARED_KEYEDMUTEX: u32 = 0x0000_0100;
    const DDI_MISC_GDI_COMPATIBLE: u32 = 0x0000_0200;
    const DDI_BIND_PRESENT: u32 = 0x0000_0080;
    const API_MISC_SHARED_NTHANDLE: u32 = 0x0000_0800;

    if is_buffer {
        ddi_misc
            & (DDI_MISC_DRAWINDIRECT_ARGS
                | DDI_MISC_BUFFER_ALLOW_RAW_VIEWS
                | DDI_MISC_BUFFER_STRUCTURED
                | DDI_MISC_RESOURCE_CLAMP)
    } else {
        let mut out = ddi_misc
            & (DDI_MISC_AUTO_GEN_MIP_MAP
                | DDI_MISC_SHARED
                | DDI_MISC_RESOURCE_CLAMP
                | DDI_MISC_GDI_COMPATIBLE);

        // Producer-side D3D11 resources still need DXVK's NT-shareable export
        // path so native DWM can create shared handles normally. The DDI
        // open_resource path below intentionally imports the KMT resource as
        // plain shared to avoid creating a second DXVK keyed mutex.
        if ddi_misc & DDI_MISC_SHARED != 0 {
            out |= API_MISC_SHARED_NTHANDLE;
        }
        if ddi_misc & DDI_MISC_SHARED_KEYEDMUTEX != 0 || ddi_bind & DDI_BIND_PRESENT != 0 {
            out |= DDI_MISC_SHARED;
        }
        out
    }
}

// --- CalcPrivate*Size (all store one COM pointer) ---------------------------

unsafe extern "C" fn calc_size_resource(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATERESOURCE,
) -> u64 {
    8
}
unsafe extern "C" fn calc_size_rtv(
    _h: Hdevice,
    _a: *const ddi::D3D10DDIARG_CREATERENDERTARGETVIEW,
) -> u64 {
    8
}

// --- Resources --------------------------------------------------------------

const RES_BUFFER: ddi::D3D10DDIRESOURCE_TYPE = ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_BUFFER;
const RES_BUFFEREX: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D11DDIRESOURCE_BUFFEREX;
const RES_TEX2D: ddi::D3D10DDIRESOURCE_TYPE = ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURE2D;
const RES_TEX1D: ddi::D3D10DDIRESOURCE_TYPE = ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURE1D;
const RES_TEX3D: ddi::D3D10DDIRESOURCE_TYPE = ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURE3D;
const RES_TEXCUBE: ddi::D3D10DDIRESOURCE_TYPE =
    ddi::D3D10DDIRESOURCE_TYPE_D3D10DDIRESOURCE_TEXTURECUBE;

unsafe fn allocate_wddm_resource(
    h: Hdevice,
    a: &ddi::D3D11DDIARG_CREATERESOURCE,
    mip0: &ddi::D3D10DDI_MIPINFO,
    h_rt: ddi::D3D10DDI_HRTRESOURCE,
    backing_blob_id: u64,
    backing_blob_size: u64,
    backing_resource_id: u32,
    venus_alloc_size: u64,
    memory_type_index: u32,
) -> (ddi::D3DKMT_HANDLE, ddi::D3DKMT_HANDLE) {
    const DDI_BIND_PRESENT: u32 = 0x0000_0080;
    const DDI_MISC_SHARED: u32 = 0x0000_0002;
    const DDI_MISC_SHARED_KEYEDMUTEX: u32 = 0x0000_0100;

    let needs_allocation = !a.pPrimaryDesc.is_null()
        || (a.BindFlags & DDI_BIND_PRESENT) != 0
        || (a.MiscFlags & (DDI_MISC_SHARED | DDI_MISC_SHARED_KEYEDMUTEX)) != 0;
    if !needs_allocation {
        return (0, 0);
    }

    let Some(dev) = helios_device(h) else {
        return (0, 0);
    };
    if dev.kt_callbacks.is_null() {
        log_line("DDI allocate_wddm_resource: no KT callbacks");
        return (0, 0);
    }
    let Some(allocate_cb) = (*dev.kt_callbacks).pfnAllocateCb else {
        log_line("DDI allocate_wddm_resource: pfnAllocateCb missing");
        return (0, 0);
    };

    let venus_ctx_id = dev.dxvk.venus_context_id();
    if venus_ctx_id == 0 {
        log_line("DDI allocate_wddm_resource: no Venus context id");
    }

    const CROSS_ADAPTER_PITCH_ALIGN: u32 = 256;
    let raw_pitch = mip0.TexelWidth.saturating_mul(dxgi_bytes_per_pixel(a.Format as u32));
    let pitch =
        raw_pitch.saturating_add(CROSS_ADAPTER_PITCH_ALIGN - 1) & !(CROSS_ADAPTER_PITCH_ALIGN - 1);
    let linear_size = (pitch as u64)
        .saturating_mul(mip0.TexelHeight.max(1) as u64)
        .max(4096);
    let size = if backing_blob_id != 0 && backing_blob_size != 0 {
        backing_blob_size
    } else {
        linear_size
    };

    let mut private = RuntimeAllocPrivate {
        alloc: HeliosWddmAllocPrivate::new(
            if backing_blob_id != 0 {
                HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY
            } else {
                HELIOS_WDDM_ALLOC_KIND_STANDARD
            },
            venus_ctx_id,
            backing_blob_id,
            size,
            VIRTIO_GPU_BLOB_MEM_HOST3D,
            if backing_blob_id != 0 {
                // DXVK render targets are normally backed by device-local Venus
                // memory. virglrenderer rejects USE_MAPPABLE for non-host-visible
                // memory ("mem cannot support mappable blob"). They still must
                // be shareable so the host can export/import the backing memory.
                VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE
            } else {
                VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE
            },
            VIRTIO_GPU_MAP_CACHE_CACHED,
        ),
        meta: HeliosWddmAllocMeta {
            width: mip0.TexelWidth,
            height: mip0.TexelHeight,
            format: dxgi_to_d3dddi_format(a.Format as u32),
            pitch,
            bind_flags: a.BindFlags,
            misc_flags: a.MiscFlags,
            // C1 identity: the creating vkAllocateMemory's exact parameters for
            // adopted venus-backed resources (a cross-process opener must import
            // with them). Zero for KMD-backed standard allocations — the KMD
            // fills them at CreateAllocation from its kernel venus client.
            venus_alloc_size,
            memory_type_index,
            // Carry the creator's EXACT DXGI format so a cross-process opener
            // rebuilds the image with the same bpp/layout instead of a squashed
            // BGRA (the `format` field below is a lossy D3DDDIFORMAT for the
            // KMD's DescribeAllocation). `a.Format` is already a DXGI_FORMAT.
            dxgi_format: a.Format as u32,
        },
    };
    if backing_blob_id != 0 && backing_resource_id != 0 {
        private.alloc._pad = backing_resource_id;
    }

    let mut allocation_info = ddi::D3DDDI_ALLOCATIONINFO2::default();
    let private_ptr = (&mut private as *mut RuntimeAllocPrivate).cast();
    let private_size = core::mem::size_of::<RuntimeAllocPrivate>() as u32;
    allocation_info.pPrivateDriverData = private_ptr;
    allocation_info.PrivateDriverDataSize = private_size;
    let _is_present = (a.BindFlags & DDI_BIND_PRESENT) != 0;
    let is_primary_allocation = !a.pPrimaryDesc.is_null();
    allocation_info.VidPnSourceId = if !a.pPrimaryDesc.is_null() {
        (*a.pPrimaryDesc).VidPnSourceId
    } else {
        0
    };
    allocation_info.Flags.Value = if is_primary_allocation { 1 } else { 0 };
    let mut alloc = ddi::D3DDDICB_ALLOCATE::default();
    // pfnAllocateCb expects the runtime resource handle for the resource whose
    // surfaces are being allocated. Shared resources additionally return an
    // hKMResource, but the association itself is not optional for present-only
    // allocations.
    alloc.pPrivateDriverData = private_ptr;
    alloc.PrivateDriverDataSize = private_size;
    alloc.hResource = h_rt.handle;
    alloc.NumAllocations = 1;
    alloc.__bindgen_anon_1.pAllocationInfo2 = &mut allocation_info;

    let hr = allocate_cb(dev.h_rt_device, &mut alloc);
    let h_allocation = allocation_info.hAllocation;
    let n = WDDM_ALLOC_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 128 || hr != 0 {
        log_line(&format!(
            "DDI allocate_wddm_resource: hr=0x{:08x} alloc=0x{:x} km=0x{:x} rt={:p} assoc={:p} info={} rpriv={} size={} pitch={} blob=0x{:x} res_id={} ctx={} kind={} primary={} vidpn={} {}x{} fmt={} bind=0x{:x} misc=0x{:x}",
            hr as u32,
            h_allocation,
            alloc.hKMResource,
            h_rt.handle,
            alloc.hResource,
            allocation_info.PrivateDriverDataSize,
            alloc.PrivateDriverDataSize,
            size,
            pitch,
            backing_blob_id,
            private.alloc._pad,
            private.alloc.ctx_id,
            private.alloc.kind,
            is_primary_allocation,
            allocation_info.VidPnSourceId,
            mip0.TexelWidth,
            mip0.TexelHeight,
            a.Format,
            a.BindFlags,
            a.MiscFlags
        ));
    }
    if hr == 0 {
        (h_allocation, alloc.hKMResource)
    } else {
        (0, 0)
    }
}

unsafe extern "C" fn create_resource(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATERESOURCE,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    h_rt: ddi::D3D10DDI_HRTRESOURCE,
) {
    clear_handle(h_resource.pDrvPrivate);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let mip0 = if a.pMipInfoList.is_null() {
        ddi::D3D10DDI_MIPINFO {
            TexelWidth: 0,
            TexelHeight: 0,
            TexelDepth: 0,
            PhysicalWidth: 0,
            PhysicalHeight: 0,
            PhysicalDepth: 0,
        }
    } else {
        *a.pMipInfoList
    };

    // Build initial-data array if provided (one entry per subresource).
    let num_sub = (a.MipLevels.max(1) * a.ArraySize.max(1)) as usize;
    let mut init: Vec<D3D11_SUBRESOURCE_DATA> = Vec::new();
    if !a.pInitialDataUP.is_null() {
        for i in 0..num_sub {
            let up = &*a.pInitialDataUP.add(i);
            init.push(D3D11_SUBRESOURCE_DATA {
                pSysMem: up.pSysMem,
                SysMemPitch: up.SysMemPitch,
                SysMemSlicePitch: up.SysMemSlicePitch,
            });
        }
    }
    let init_ptr = if init.is_empty() {
        None
    } else {
        Some(init.as_ptr())
    };

    let cpu = cpu_access(a.Usage);

    match a.ResourceDimension {
        RES_BUFFER | RES_BUFFEREX => {
            let bind = api_bind_flags(a.BindFlags);
            let misc = api_misc_flags(a.MiscFlags, a.BindFlags, true);
            if bind != a.BindFlags || misc != a.MiscFlags || !a.pPrimaryDesc.is_null() {
                log_line(&format!(
                    "DDI create_resource(buffer): normalize bind 0x{:x}->0x{:x} misc 0x{:x}->0x{:x} primary={}",
                    a.BindFlags,
                    bind,
                    a.MiscFlags,
                    misc,
                    !a.pPrimaryDesc.is_null()
                ));
            }
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: mip0.TexelWidth,
                Usage: D3D11_USAGE(a.Usage as i32),
                BindFlags: bind,
                CPUAccessFlags: cpu,
                MiscFlags: misc,
                StructureByteStride: a.ByteStride,
            };
            let (allocation, km_resource) = allocate_wddm_resource(h, a, &mip0, h_rt, 0, 0, 0, 0, 0);
            let mut buf: Option<ID3D11Buffer> = None;
            match device.CreateBuffer(&desc, init_ptr, Some(&mut buf)) {
                Ok(()) => {
                    if let Some(b) = buf {
                        if let Ok(res) = b.cast::<ID3D11Resource>() {
                            stamp_dxvk_resource_kmt_handles(h, &res, allocation, km_resource);
                            let n = RESOURCE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                            if n < 128 {
                                log_line(&format!(
                                    "DDI create_resource(buffer) ok: bytes={} fmt={} usage={} bind=0x{:x} misc=0x{:x}",
                                    mip0.TexelWidth, a.Format, a.Usage, bind, misc
                                ));
                            }
                            store_resource(
                                h_resource.pDrvPrivate,
                                res,
                                allocation,
                                km_resource,
                                h_rt.handle,
                                true, // allocated via pfnAllocateCb above
                            );
                        }
                    }
                }
                Err(e) => log_line(&format!("DDI create_resource(buffer) failed: {e:?}")),
            }
        }
        RES_TEX2D | RES_TEXCUBE => {
            let bind = api_bind_flags(a.BindFlags);
            let mut misc = api_misc_flags(a.MiscFlags, a.BindFlags, false);
            if a.ResourceDimension == RES_TEXCUBE {
                misc |= D3D11_RESOURCE_MISC_TEXTURECUBE.0 as u32;
            }
            if a.MiscFlags != 0 {
                log_line(&format!(
                    "DDI misc translation v2: ddi_misc=0x{:x} ddi_bind=0x{:x} api_misc=0x{:x}",
                    a.MiscFlags, a.BindFlags, misc
                ));
            }
            if bind != a.BindFlags
                || (misc & !D3D11_RESOURCE_MISC_TEXTURECUBE.0 as u32) != a.MiscFlags
                || !a.pPrimaryDesc.is_null()
            {
                log_line(&format!(
                    "DDI create_resource(tex2d): {}x{} fmt={} usage={} bind 0x{:x}->0x{:x} misc 0x{:x}->0x{:x} primary={} sample={}x{}",
                    mip0.TexelWidth,
                    mip0.TexelHeight,
                    a.Format,
                    a.Usage,
                    a.BindFlags,
                    bind,
                    a.MiscFlags,
                    misc,
                    !a.pPrimaryDesc.is_null(),
                    a.SampleDesc.Count,
                    a.SampleDesc.Quality
                ));
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: mip0.TexelWidth,
                Height: mip0.TexelHeight,
                MipLevels: a.MipLevels,
                ArraySize: a.ArraySize.max(1),
                Format: DXGI_FORMAT(a.Format as i32),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: a.SampleDesc.Count,
                    Quality: a.SampleDesc.Quality,
                },
                Usage: D3D11_USAGE(a.Usage as i32),
                BindFlags: bind,
                CPUAccessFlags: cpu,
                MiscFlags: misc,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            if mip0.TexelWidth >= 1024 || mip0.TexelHeight >= 576 || misc != 0 {
                log_line(&format!(
                    "DDI create_resource(tex2d): calling DXVK CreateTexture2D {}x{} fmt={} bind=0x{:x} misc=0x{:x} init={} hrt={:p} mips={} array={} usage={} cpu=0x{:x} sample={}x{}",
                    mip0.TexelWidth,
                    mip0.TexelHeight,
                    a.Format,
                    bind,
                    misc,
                    init_ptr.is_some(),
                    h_rt.handle,
                    desc.MipLevels,
                    desc.ArraySize,
                    a.Usage,
                    cpu,
                    a.SampleDesc.Count,
                    a.SampleDesc.Quality
                ));
            }
            match device.CreateTexture2D(&desc, init_ptr, Some(&mut tex)) {
                Ok(()) => {
                    if mip0.TexelWidth >= 1024 || mip0.TexelHeight >= 576 || misc != 0 {
                        log_line(&format!(
                            "DDI create_resource(tex2d): DXVK CreateTexture2D returned S_OK tex_present={}",
                            tex.is_some()
                        ));
                    }
                    if let Some(t) = tex {
                        if let Ok(res) = t.cast::<ID3D11Resource>() {
                            if mip0.TexelWidth >= 1024 || mip0.TexelHeight >= 576 || misc != 0 {
                                log_line("DDI create_resource(tex2d): cast to ID3D11Resource OK");
                            }
                            let (memory, memory_size, memory_offset, resource_id) =
                                dxvk_resource_memory_info(h, &res);
                            let (backing_blob_id, backing_blob_size, backing_resource_id) =
                                if memory != 0 && memory_offset == 0 && memory <= u32::MAX as u64 {
                                    (memory, memory_size, resource_id)
                                } else {
                                    // Only resources that get a WDDM allocation (shared /
                                    // keyed-mutex / present / primary) NEED an importable
                                    // backing — for those a suballocated DXVK memory means
                                    // a cross-process opener sees a disconnected KMD blob
                                    // (two-memory split), so shout. Private textures are
                                    // suballocated by design; that used to log here too and
                                    // got misread as a shared-resource defect (18th session).
                                    const DDI_BIND_PRESENT: u32 = 0x0000_0080;
                                    const DDI_MISC_SHARED: u32 = 0x0000_0002;
                                    const DDI_MISC_SHARED_KEYEDMUTEX: u32 = 0x0000_0100;
                                    let needs_importable = !a.pPrimaryDesc.is_null()
                                        || (a.BindFlags & DDI_BIND_PRESENT) != 0
                                        || (a.MiscFlags
                                            & (DDI_MISC_SHARED | DDI_MISC_SHARED_KEYEDMUTEX))
                                            != 0;
                                    if memory != 0 && needs_importable {
                                        log_line(&format!(
                                            "DDI create_resource(tex2d): SHARED RESOURCE WITHOUT IMPORTABLE BACKING memory=0x{:x} res_id={} size={} offset={} bind=0x{:x} misc=0x{:x}",
                                            memory, resource_id, memory_size, memory_offset,
                                            a.BindFlags, a.MiscFlags
                                        ));
                                    } else if memory != 0 {
                                        trace_line!(
                                            "DDI create_resource(tex2d): private suballocated memory=0x{:x} size={} offset={}",
                                            memory, memory_size, memory_offset
                                        );
                                    }
                                    (0, 0, 0)
                                };
                            // C1: record the creating vkAllocateMemory's exact
                            // size + memory type into the allocation trailer so
                            // cross-process openers import with them.
                            let (mut venus_alloc_size, mut memory_type_index) = (0u64, 0u32);
                            if backing_resource_id != 0 {
                                if let Some(dev) = helios_device(h) {
                                    if !dev.dxvk.get_resource_alloc_identity(
                                        res.as_raw() as usize,
                                        &mut venus_alloc_size,
                                        &mut memory_type_index,
                                    ) {
                                        log_line(&format!(
                                            "DDI create_resource(tex2d): no venus alloc identity for res_id={}",
                                            backing_resource_id
                                        ));
                                    }
                                }
                            }
                            let (allocation, km_resource) = allocate_wddm_resource(
                                h,
                                a,
                                &mip0,
                                h_rt,
                                backing_blob_id,
                                backing_blob_size,
                                backing_resource_id,
                                venus_alloc_size,
                                memory_type_index,
                            );
                            if allocation != 0 && backing_resource_id != 0 {
                                if let Some(dev) = helios_device(h) {
                                    if !dev.dxvk.transfer_resource_ownership(res.as_raw() as usize)
                                    {
                                        log_line(&format!(
                                            "DDI create_resource(tex2d): ownership transfer failed res_id={}",
                                            backing_resource_id
                                        ));
                                    }
                                }
                            }
                            trace_line!(
                                "DDI create_resource(tex2d): before KMT stamp km=0x{:x} alloc=0x{:x} blob=0x{:x} res_id={} blob_size={}",
                                km_resource, allocation, backing_blob_id, backing_resource_id, backing_blob_size
                            );
                            stamp_dxvk_resource_kmt_handles(h, &res, allocation, km_resource);
                            let n = RESOURCE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                            if n < 128 {
                                trace_line!(
                                    "DDI create_resource(tex2d) ok after-stamp-call: {}x{} fmt={} usage={} bind=0x{:x} misc=0x{:x} sample={}x{}",
                                    mip0.TexelWidth,
                                    mip0.TexelHeight,
                                    a.Format,
                                    a.Usage,
                                    bind,
                                    misc,
                                    a.SampleDesc.Count,
                                    a.SampleDesc.Quality
                                );
                            }
                            store_resource(
                                h_resource.pDrvPrivate,
                                res,
                                allocation,
                                km_resource,
                                h_rt.handle,
                                true, // allocated via pfnAllocateCb above
                            );
                        } else if mip0.TexelWidth >= 1024 || mip0.TexelHeight >= 576 || misc != 0 {
                            log_line("DDI create_resource(tex2d): cast to ID3D11Resource failed");
                        }
                    } else if mip0.TexelWidth >= 1024 || mip0.TexelHeight >= 576 || misc != 0 {
                        log_line(
                            "DDI create_resource(tex2d): DXVK CreateTexture2D returned no texture",
                        );
                    }
                }
                Err(e) => log_line(&format!("DDI create_resource(tex2d) failed: {e:?}")),
            }
        }
        other => log_line(&format!("DDI create_resource: unhandled dimension {other}")),
    }
}

unsafe extern "C" fn destroy_resource(h: Hdevice, h_resource: ddi::D3D10DDI_HRESOURCE) {
    release_resource(h, h_resource.pDrvPrivate);
}

unsafe extern "C" fn open_resource(
    h: Hdevice,
    arg: *const ddi::D3D10DDIARG_OPENRESOURCE,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    h_rt: ddi::D3D10DDI_HRTRESOURCE,
) {
    clear_handle(h_resource.pDrvPrivate);

    if arg.is_null() {
        log_line("DDI open_resource: null args");
        set_runtime_error(h, E_INVALIDARG);
        return;
    }

    let a = &*arg;
    let info = unsafe { a.__bindgen_anon_1.pOpenAllocationInfo };
    let info2 = unsafe { a.__bindgen_anon_1.pOpenAllocationInfo2 };
    let mut detail = String::new();
    let mut allocation: ddi::D3DKMT_HANDLE = 0;
    // C1 identity ABI: the KMD wrote a versioned HeliosWddmOpenIdentity record
    // into the open-time private data in DxgkDdiOpenAllocation, after
    // validating the backing venus resource is LIVE. Prefer the per-allocation
    // buffer; fall back to the resource-level one. No `_pad` heuristics.
    let mut identity = unsafe {
        read_open_identity(a.pPrivateDriverData, a.PrivateDriverDataSize)
    };
    if !info.is_null() {
        let i = &*info;
        allocation = i.hAllocation;
        if identity.is_none() {
            identity = unsafe { read_open_identity(i.pPrivateDriverData, i.PrivateDriverDataSize) };
        }
        detail.push_str(&format!(
            " info.hAlloc=0x{:x} info.private=0x{:x}/{}",
            i.hAllocation, i.pPrivateDriverData as usize, i.PrivateDriverDataSize
        ));
    }
    if !info2.is_null() {
        let i = &*info2;
        allocation = i.hAllocation;
        if identity.is_none() {
            identity = unsafe { read_open_identity(i.pPrivateDriverData, i.PrivateDriverDataSize) };
        }
        detail.push_str(&format!(
            " info2.hAlloc=0x{:x} info2.private=0x{:x}/{} gpuva=0x{:x}",
            i.hAllocation,
            i.pPrivateDriverData as usize,
            i.PrivateDriverDataSize,
            i.GpuVirtualAddress
        ));
    }
    log_line(&format!(
        "DDI open_resource: num_alloc={} hKM={:?} private=0x{:x}/{}{}",
        a.NumAllocations,
        a.hKMResource,
        a.pPrivateDriverData as usize,
        a.PrivateDriverDataSize,
        detail
    ));

    // A shared open without a KMD identity record cannot alias the real
    // surface. The old metadata-texture fallback fabricated a blank texture
    // here and stamped it with the real KMT handles — draws "succeeded" and
    // the shared content stayed black forever (audit U-B2). Fail loudly
    // instead so the producer-side bug gets found.
    let Some((ident, meta)) = identity else {
        log_line(&format!(
            "DDI open_resource FAILED: no venus identity record (hKM={:?} alloc=0x{:x}) -> E_FAIL",
            a.hKMResource, allocation
        ));
        set_runtime_error(h, E_FAIL);
        return;
    };
    let meta = meta.unwrap_or(HeliosWddmAllocMeta {
        width: 1,
        height: 1,
        format: 21,
        pitch: 4,
        bind_flags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        misc_flags: 0,
        venus_alloc_size: 0,
        memory_type_index: 0,
        dxgi_format: 0,
    });

    if a.hKMResource.handle == 0 {
        log_line(&format!(
            "DDI open_resource FAILED: no hKMResource (res_id={}) -> E_FAIL",
            ident.resource_id
        ));
        set_runtime_error(h, E_FAIL);
        return;
    }

    let Some(dev) = helios_device(h) else {
        log_line("DDI open_resource FAILED: no Helios device -> E_FAIL");
        set_runtime_error(h, E_FAIL);
        return;
    };

    let open_bind = api_bind_flags(meta.bind_flags);
    let open_misc = meta.misc_flags
        & (D3D11_RESOURCE_MISC_SHARED.0 as u32
            | D3D11_RESOURCE_MISC_RESOURCE_CLAMP.0 as u32
            | D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32);
    // Prefer the identity's venus alloc size; the meta trailer carries the
    // creator-recorded copy for KMD standard allocations.
    let venus_alloc_size = if ident.venus_alloc_size != 0 {
        ident.venus_alloc_size
    } else {
        meta.venus_alloc_size
    };
    // Rebuild the image with the creator's EXACT DXGI format. Falls back to the
    // lossy D3DDDIFORMAT translation only when the creator did not record a DXGI
    // format (0 = legacy trailer / KMD standard allocation, which are BGRA). The
    // old unconditional `d3dddi_to_dxgi_format(meta.format)` collapsed every
    // surface to BGRA — an A8/R8 mask then rebuilt as 4bpp needed 4x the memory
    // and its (correctly sized) import was refused as "undersized".
    let open_dxgi_format = if meta.dxgi_format != 0 {
        meta.dxgi_format
    } else {
        d3dddi_to_dxgi_format(meta.format).0 as u32
    };
    log_line(&format!(
        "DDI open_resource identity: res_id={} alloc_size={} mem_type={} kind={} ctx={} meta_bind=0x{:x} meta_misc=0x{:x} open_bind=0x{:x} open_misc=0x{:x} dxgi_fmt={} d3dddi_fmt={}",
        ident.resource_id, venus_alloc_size, ident.memory_type_index, ident.kind, ident.ctx_id,
        meta.bind_flags, meta.misc_flags, open_bind, open_misc, open_dxgi_format, meta.format
    ));
    let raw = dev.dxvk.open_ddi_texture2d(
        meta.width.max(1),
        meta.height.max(1),
        open_dxgi_format,
        open_bind,
        open_misc,
        a.hKMResource.handle,
        ident.resource_id,
        venus_alloc_size,
        ident.memory_type_index,
    );
    if raw == 0 {
        // Import of a KMD-validated-live resource failed: a real bug, not a
        // condition to paper over with substitute content (audit C1.3).
        log_line(&format!(
            "DDI open_resource FAILED: ddi-shared import {}x{} d3dfmt={} alloc=0x{:x} hKM={:?} res_id={} -> E_FAIL",
            meta.width, meta.height, meta.format, allocation, a.hKMResource, ident.resource_id
        ));
        set_runtime_error(h, E_FAIL);
        return;
    }

    let res = ID3D11Resource::from_raw(raw as *mut c_void);
    stamp_dxvk_resource_kmt_handles(h, &res, allocation, a.hKMResource.handle);
    log_line(&format!(
        "DDI open_resource ddi-shared ok: {}x{} d3dfmt={} alloc=0x{:x} hKM={:?} raw=0x{:x}",
        meta.width, meta.height, meta.format, allocation, a.hKMResource, raw
    ));
    store_resource(
        h_resource.pDrvPrivate,
        res,
        allocation,
        a.hKMResource.handle,
        h_rt.handle,
        false, // opened: runtime owns these allocation handles
    );
}

unsafe extern "C" fn calc_size_opened_resource(
    _h: Hdevice,
    _arg: *const ddi::D3D10DDIARG_OPENRESOURCE,
) -> u64 {
    8
}

unsafe extern "C" fn resolve_shared_resource(
    h: ddi::HANDLE,
    arg: *const ddi::D3DDDIARG_RESOLVESHAREDRESOURCE,
) -> i32 {
    let h_resource: *mut c_void = if arg.is_null() {
        core::ptr::null_mut()
    } else {
        (*arg).hResource as *mut c_void
    };
    let resource = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: h_resource,
    };
    let alloc = resource_allocation(resource.pDrvPrivate);
    let (width, height) = resource_dimensions(resource.pDrvPrivate);
    log_line(&format!(
        "DDI ResolveSharedResource: hDevice={:p} hResource={:p} alloc=0x{:x} {}x{}",
        h, h_resource, alloc, width, height
    ));
    if let Some(context) = d3d11_context(Hdevice {
        pDrvPrivate: h as *mut c_void,
    }) {
        context.Flush();
    }
    0
}

unsafe extern "C" fn dxgi_resolve_shared_resource(
    arg: *mut ddi::DXGI_DDI_ARG_RESOLVESHAREDRESOURCE,
) -> i32 {
    let (h_device, h_resource): (usize, usize) = if arg.is_null() {
        (0, 0)
    } else {
        ((*arg).hDevice as usize, (*arg).hResource as usize)
    };
    let resource = dxgi_resource_handle(h_resource as ddi::DXGI_DDI_HRESOURCE);
    let alloc = resource_allocation(resource.pDrvPrivate);
    let (width, height) = resource_dimensions(resource.pDrvPrivate);
    trace_line!(
        "DXGI ResolveSharedResource: hDevice=0x{:x} hResource=0x{:x} alloc=0x{:x} {}x{}",
        h_device, h_resource, alloc, width, height
    );
    if let Some(context) = d3d11_context(Hdevice {
        pDrvPrivate: h_device as *mut c_void,
    }) {
        context.Flush();
    }
    0
}

// --- Render target views ----------------------------------------------------

unsafe extern "C" fn create_rtv(
    h: Hdevice,
    arg: *const ddi::D3D10DDIARG_CREATERENDERTARGETVIEW,
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
    _h_rt: ddi::D3D10DDI_HRTRENDERTARGETVIEW,
) {
    clear_handle(h_rtv.pDrvPrivate);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource.pDrvPrivate) else {
        log_line("DDI create_rtv: resource handle empty");
        return;
    };
    let Some(desc) = rtv_desc(a) else {
        log_line(&format!(
            "DDI create_rtv: unsupported resource dimension {} fmt={}",
            a.ResourceDimension, a.Format
        ));
        return;
    };
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    match device.CreateRenderTargetView(&*res, Some(&desc), Some(&mut rtv)) {
        Ok(()) => {
            if let Some(v) = rtv {
                let n = VIEW_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                let allocation = resource_allocation(a.hDrvResource.pDrvPrivate);
                let (width, height) = resource_dimensions(a.hDrvResource.pDrvPrivate);
                if n < 128 {
                    log_line(&format!(
                        "DDI create_rtv ok: dim={} fmt={} alloc=0x{:x} {}x{}",
                        a.ResourceDimension, a.Format, allocation, width, height
                    ));
                }
                store_rtv(
                    h_rtv.pDrvPrivate,
                    v,
                    allocation,
                    width,
                    height,
                    a.Format as u32,
                );
            }
        }
        Err(e) => log_line(&format!(
            "DDI create_rtv failed: dim={} fmt={} {e:?}",
            a.ResourceDimension, a.Format
        )),
    }
}

unsafe fn rtv_desc(
    a: &ddi::D3D10DDIARG_CREATERENDERTARGETVIEW,
) -> Option<D3D11_RENDER_TARGET_VIEW_DESC> {
    let format = DXGI_FORMAT(a.Format as i32);
    match a.ResourceDimension {
        RES_BUFFER | RES_BUFFEREX => {
            let b = a.__bindgen_anon_1.Buffer;
            Some(D3D11_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_RTV_DIMENSION_BUFFER,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Buffer: D3D11_BUFFER_RTV {
                        Anonymous1: D3D11_BUFFER_RTV_0 {
                            FirstElement: b.__bindgen_anon_1.FirstElement,
                        },
                        Anonymous2: D3D11_BUFFER_RTV_1 {
                            NumElements: b.__bindgen_anon_2.NumElements,
                        },
                    },
                },
            })
        }
        RES_TEX1D => {
            let t = a.__bindgen_anon_1.Tex1D;
            if t.ArraySize > 1 {
                Some(D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE1DARRAY,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture1DArray: D3D11_TEX1D_ARRAY_RTV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE1D,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture1D: D3D11_TEX1D_RTV {
                            MipSlice: t.MipSlice,
                        },
                    },
                })
            }
        }
        RES_TEX2D => {
            let t = a.__bindgen_anon_1.Tex2D;
            if t.ArraySize > 1 {
                Some(D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DARRAY,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture2DArray: D3D11_TEX2D_ARRAY_RTV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_RTV {
                            MipSlice: t.MipSlice,
                        },
                    },
                })
            }
        }
        RES_TEX3D => {
            let t = a.__bindgen_anon_1.Tex3D;
            Some(D3D11_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE3D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture3D: D3D11_TEX3D_RTV {
                        MipSlice: t.MipSlice,
                        FirstWSlice: t.FirstW,
                        WSize: t.WSize,
                    },
                },
            })
        }
        RES_TEXCUBE => {
            let t = a.__bindgen_anon_1.TexCube;
            Some(D3D11_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_RTV {
                        MipSlice: t.MipSlice,
                        FirstArraySlice: t.FirstArraySlice,
                        ArraySize: t.ArraySize,
                    },
                },
            })
        }
        _ => None,
    }
}

unsafe extern "C" fn destroy_rtv(_h: Hdevice, h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW) {
    release_rtv(h_rtv.pDrvPrivate);
}

// --- Depth-stencil views ----------------------------------------------------

unsafe extern "C" fn calc_size_dsv(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATEDEPTHSTENCILVIEW,
) -> u64 {
    8
}

unsafe extern "C" fn create_dsv(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATEDEPTHSTENCILVIEW,
    h_dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW,
    _h_rt: ddi::D3D10DDI_HRTDEPTHSTENCILVIEW,
) {
    clear_handle(h_dsv.pDrvPrivate);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource.pDrvPrivate) else {
        log_line("DDI create_dsv: resource handle empty");
        return;
    };
    let Some(desc) = dsv_desc(a) else {
        log_line(&format!(
            "DDI create_dsv: unsupported resource dimension {} fmt={}",
            a.ResourceDimension, a.Format
        ));
        return;
    };
    let mut dsv: Option<ID3D11DepthStencilView> = None;
    match device.CreateDepthStencilView(&*res, Some(&desc), Some(&mut dsv)) {
        Ok(()) => {
            if let Some(v) = dsv {
                let n = VIEW_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                if n < 128 {
                    log_line(&format!(
                        "DDI create_dsv ok: dim={} fmt={} flags=0x{:x}",
                        a.ResourceDimension, a.Format, a.Flags
                    ));
                }
                store_com(h_dsv.pDrvPrivate, v);
            }
        }
        Err(e) => log_line(&format!(
            "DDI create_dsv failed: dim={} fmt={} flags=0x{:x} {e:?}",
            a.ResourceDimension, a.Format, a.Flags
        )),
    }
}

unsafe fn dsv_desc(
    a: &ddi::D3D11DDIARG_CREATEDEPTHSTENCILVIEW,
) -> Option<D3D11_DEPTH_STENCIL_VIEW_DESC> {
    let format = DXGI_FORMAT(a.Format as i32);
    match a.ResourceDimension {
        RES_TEX1D => {
            let t = a.__bindgen_anon_1.Tex1D;
            if t.ArraySize > 1 {
                Some(D3D11_DEPTH_STENCIL_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_DSV_DIMENSION_TEXTURE1DARRAY,
                    Flags: a.Flags,
                    Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture1DArray: D3D11_TEX1D_ARRAY_DSV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_DEPTH_STENCIL_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_DSV_DIMENSION_TEXTURE1D,
                    Flags: a.Flags,
                    Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture1D: D3D11_TEX1D_DSV {
                            MipSlice: t.MipSlice,
                        },
                    },
                })
            }
        }
        RES_TEX2D => {
            let t = a.__bindgen_anon_1.Tex2D;
            if t.ArraySize > 1 {
                Some(D3D11_DEPTH_STENCIL_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2DARRAY,
                    Flags: a.Flags,
                    Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture2DArray: D3D11_TEX2D_ARRAY_DSV {
                            MipSlice: t.MipSlice,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_DEPTH_STENCIL_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2D,
                    Flags: a.Flags,
                    Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_DSV {
                            MipSlice: t.MipSlice,
                        },
                    },
                })
            }
        }
        RES_TEXCUBE => {
            let t = a.__bindgen_anon_1.TexCube;
            Some(D3D11_DEPTH_STENCIL_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2DARRAY,
                Flags: a.Flags,
                Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_DSV {
                        MipSlice: t.MipSlice,
                        FirstArraySlice: t.FirstArraySlice,
                        ArraySize: t.ArraySize,
                    },
                },
            })
        }
        _ => None,
    }
}

unsafe extern "C" fn destroy_dsv(_h: Hdevice, h_dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW) {
    release_com(h_dsv.pDrvPrivate);
}

unsafe extern "C" fn clear_rtv(
    h: Hdevice,
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
    color: *mut f32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(rtv) = load_rtv(h_rtv.pDrvPrivate) else {
        return;
    };
    let rgba: [f32; 4] = if color.is_null() {
        [0.0; 4]
    } else {
        [*color, *color.add(1), *color.add(2), *color.add(3)]
    };
    context.ClearRenderTargetView(&*rtv, &rgba);
}

unsafe extern "C" fn clear_dsv(
    h: Hdevice,
    h_dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW,
    flags: u32,
    depth: f32,
    stencil: u8,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(dsv) = load_com::<ID3D11DepthStencilView>(h_dsv.pDrvPrivate) else {
        return;
    };
    context.ClearDepthStencilView(&*dsv, flags, depth, stencil);
}

// --- Copy / Map / Flush -----------------------------------------------------

unsafe extern "C" fn resource_copy(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    h_src: ddi::D3D10DDI_HRESOURCE,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let (Some(dst), Some(src)) = (
        load_resource(h_dst.pDrvPrivate),
        load_resource(h_src.pDrvPrivate),
    ) else {
        let n = COPY_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
        if n < 256 {
            log_line(&format!(
                "DDI resource_copy missing resource dst_priv={:p} src_priv={:p}",
                h_dst.pDrvPrivate, h_src.pDrvPrivate
            ));
        }
        return;
    };
    let dst_alloc = resource_allocation(h_dst.pDrvPrivate);
    let src_alloc = resource_allocation(h_src.pDrvPrivate);
    let n = COPY_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 256 || dst_alloc != 0 || src_alloc != 0 {
        log_line(&format!(
            "DDI resource_copy dst_alloc=0x{:x} src_alloc=0x{:x}",
            dst_alloc, src_alloc
        ));
    }
    context.CopyResource(&*dst, &*src);
}

unsafe extern "C" fn resource_copy_region(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    dst_subresource: u32,
    dst_x: u32,
    dst_y: u32,
    dst_z: u32,
    h_src: ddi::D3D10DDI_HRESOURCE,
    src_subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let (Some(dst), Some(src)) = (
        load_resource(h_dst.pDrvPrivate),
        load_resource(h_src.pDrvPrivate),
    ) else {
        return;
    };
    let bx;
    let bx_ptr = if box_.is_null() {
        None
    } else {
        let b = &*box_;
        bx = D3D11_BOX {
            left: b.left as u32,
            top: b.top as u32,
            front: b.front as u32,
            right: b.right as u32,
            bottom: b.bottom as u32,
            back: b.back as u32,
        };
        Some(&bx as *const D3D11_BOX)
    };
    context.CopySubresourceRegion(
        &*dst,
        dst_subresource,
        dst_x,
        dst_y,
        dst_z,
        &*src,
        src_subresource,
        bx_ptr,
    );
}

unsafe extern "C" fn resource_copy_region_11_1(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    dst_subresource: u32,
    dst_x: u32,
    dst_y: u32,
    dst_z: u32,
    h_src: ddi::D3D10DDI_HRESOURCE,
    src_subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
    _copy_flags: u32,
) {
    resource_copy_region(
        h,
        h_dst,
        dst_subresource,
        dst_x,
        dst_y,
        dst_z,
        h_src,
        src_subresource,
        box_,
    );
}

unsafe extern "C" fn resource_resolve_subresource(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    dst_subresource: u32,
    h_src: ddi::D3D10DDI_HRESOURCE,
    src_subresource: u32,
    format: ddi::DXGI_FORMAT,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let (Some(dst), Some(src)) = (
        load_resource(h_dst.pDrvPrivate),
        load_resource(h_src.pDrvPrivate),
    ) else {
        return;
    };
    context.ResolveSubresource(
        &*dst,
        dst_subresource,
        &*src,
        src_subresource,
        DXGI_FORMAT(format as i32),
    );
}

unsafe extern "C" fn resource_is_staging_busy(
    _h: Hdevice,
    _h_resource: ddi::D3D10DDI_HRESOURCE,
) -> i32 {
    0
}

unsafe extern "C" fn resource_map(
    h: Hdevice,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    map_type: ddi::D3D10_DDI_MAP,
    _map_flags: u32,
    mapped: *mut ddi::D3D10DDI_MAPPED_SUBRESOURCE,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_resource(h_resource.pDrvPrivate) else {
        return;
    };
    let mut out = D3D11_MAPPED_SUBRESOURCE::default();
    // DDI D3D10_DDI_MAP values match D3D11_MAP (READ=1, WRITE=2, ...).
    match context.Map(
        &*res,
        subresource,
        D3D11_MAP(map_type as i32),
        0,
        Some(&mut out),
    ) {
        Ok(()) => {
            let allocation = resource_allocation(h_resource.pDrvPrivate);
            let n = MAP_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
            if n < 256 || allocation != 0 {
                trace_line!(
                    "DDI resource_map ok alloc=0x{:x} sub={} map={} rowPitch={} depthPitch={} pData={:p}",
                    allocation,
                    subresource,
                    map_type,
                    out.RowPitch,
                    out.DepthPitch,
                    out.pData
                );
            }
            if !mapped.is_null() {
                (*mapped).pData = out.pData;
                (*mapped).RowPitch = out.RowPitch;
                (*mapped).DepthPitch = out.DepthPitch;
            }
        }
        Err(e) => {
            log_line(&format!("DDI resource_map failed: {e:?}"));
            if !mapped.is_null() {
                (*mapped).pData = core::ptr::null_mut();
            }
        }
    }
}

unsafe extern "C" fn resource_unmap(
    h: Hdevice,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_resource(h_resource.pDrvPrivate) else {
        return;
    };
    context.Unmap(&*res, subresource);
}

unsafe extern "C" fn srv_read_after_write_hazard(
    _h: Hdevice,
    _srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW,
    _resource: ddi::D3D10DDI_HRESOURCE,
) {
}

unsafe extern "C" fn resource_read_after_write_hazard(
    _h: Hdevice,
    _resource: ddi::D3D10DDI_HRESOURCE,
) {
}

unsafe fn sync_token_cb(h: Hdevice, arg: *const ddi::D3DDDIARG_SYNCTOKEN, release: bool) {
    let Some(dev) = helios_device(h) else {
        log_line("DDI sync_token: missing device");
        return;
    };
    if dev.kt_callbacks.is_null() || arg.is_null() {
        log_line(&format!(
            "DDI sync_token: missing callbacks={} arg={}",
            dev.kt_callbacks.is_null(),
            arg.is_null()
        ));
        return;
    }

    let a = &*arg;
    let cb_arg = ddi::D3DDDICB_SYNCTOKEN {
        hSyncToken: a.hSyncToken,
        BroadcastContextCount: 0,
        BroadcastContextArray: core::ptr::null(),
    };
    let cb = if release {
        (*dev.kt_callbacks).pfnReleaseResourceCb
    } else {
        (*dev.kt_callbacks).pfnAcquireResourceCb
    };
    let Some(cb) = cb else {
        log_line(&format!(
            "DDI sync_token: callback missing release={} resource={:p} token={:p}",
            release, a.hResource, a.hSyncToken
        ));
        return;
    };
    let hr = cb(dev.h_rt_device, &cb_arg);
    log_line(&format!(
        "DDI sync_token: release={} resource={:p} token={:p} hr=0x{:08x}",
        release, a.hResource, a.hSyncToken, hr as u32
    ));
}

unsafe extern "C" fn acquire_resource(h: Hdevice, arg: *const ddi::D3DDDIARG_SYNCTOKEN) {
    sync_token_cb(h, arg, false);
}

unsafe extern "C" fn release_resource_sync(h: Hdevice, arg: *const ddi::D3DDDIARG_SYNCTOKEN) {
    sync_token_cb(h, arg, true);
}

unsafe fn sync_token_cb_2_1(
    h: Hdevice,
    resource: ddi::D3D10DDI_HRESOURCE,
    token: ddi::HANDLE,
    release: bool,
) {
    let arg = ddi::D3DDDIARG_SYNCTOKEN {
        hResource: resource.pDrvPrivate,
        hSyncToken: token,
    };
    sync_token_cb(h, &arg, release);
}

unsafe extern "C" fn acquire_resource_2_1(
    h: Hdevice,
    resource: ddi::D3D10DDI_HRESOURCE,
    token: ddi::HANDLE,
) {
    sync_token_cb_2_1(h, resource, token, false);
}

unsafe extern "C" fn release_resource_2_1(
    h: Hdevice,
    resource: ddi::D3D10DDI_HRESOURCE,
    token: ddi::HANDLE,
) {
    sync_token_cb_2_1(h, resource, token, true);
}

unsafe extern "C" fn flush(h: Hdevice) {
    if let Some(context) = d3d11_context(h) {
        context.Flush();
    }
}

unsafe extern "C" fn flush_11_1(h: Hdevice, _flush_flags: u32) -> ddi::BOOL {
    flush(h);
    1
}

unsafe extern "C" fn discard_11_1(
    h: Hdevice,
    handle_type: ddi::D3D11DDI_HANDLETYPE,
    handle: *mut c_void,
    _rects: *const ddi::D3D10_DDI_RECT,
    num_rects: u32,
) {
    let n = D3D11_1_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 64 {
        log_line(&format!(
            "DDI D3D11.1 Discard: type={handle_type} rects={num_rects}"
        ));
    }

    // A rect-limited discard invalidates ONLY the given sub-rects. D3D11's
    // Discard is a hint, so discarding LESS than requested is always legal —
    // discarding more is not: forwarding these as full-view discards made
    // DXVK reinitialize the whole image, wiping the undamaged 99% of dwm's
    // flip backbuffer every frame (dwm redraws only the dirty region and
    // rect-discards exactly that region on the incoming buffer). Match
    // upstream DXVK's DiscardView1 behaviour: drop partial discards.
    if num_rects != 0 {
        return;
    }

    let Some(context) = d3d11_context1(h) else {
        return;
    };
    match handle_type {
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RESOURCE => {
            if let Some(res) = load_resource(handle) {
                context.DiscardResource(&*res);
            }
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_SHADERRESOURCEVIEW => {
            if let Some(view) = load_com::<ID3D11ShaderResourceView>(handle)
                .and_then(|v| (*v).cast::<ID3D11View>().ok())
            {
                context.DiscardView(&view);
            }
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RENDERTARGETVIEW => {
            if let Some(view) = load_rtv(handle).and_then(|v| (*v).cast::<ID3D11View>().ok()) {
                context.DiscardView(&view);
            }
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_DEPTHSTENCILVIEW => {
            if let Some(view) = load_com::<ID3D11DepthStencilView>(handle)
                .and_then(|v| (*v).cast::<ID3D11View>().ok())
            {
                context.DiscardView(&view);
            }
        }
        ddi::D3D11DDI_HANDLETYPE_D3D11DDI_HT_UNORDEREDACCESSVIEW => {
            if let Some(view) = load_com::<ID3D11UnorderedAccessView>(handle)
                .and_then(|v| (*v).cast::<ID3D11View>().ok())
            {
                context.DiscardView(&view);
            }
        }
        _ => {}
    }
}

unsafe extern "C" fn check_direct_flip_support_11_1(
    _h: Hdevice,
    _resource1: ddi::D3D10DDI_HRESOURCE,
    _resource2: ddi::D3D10DDI_HRESOURCE,
    flags: u32,
    supported: *mut ddi::BOOL,
) {
    if !supported.is_null() {
        *supported = 0;
    }
    let n = D3D11_1_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 64 {
        log_line(&format!(
            "DDI D3D11.1 CheckDirectFlipSupport: flags=0x{flags:x} -> no"
        ));
    }
}

unsafe extern "C" fn clear_view_11_1(
    h: Hdevice,
    view_type: ddi::D3D11DDI_HANDLETYPE,
    view: *mut c_void,
    color: *const f32,
    rects: *const ddi::D3D10_DDI_RECT,
    num_rects: u32,
) {
    let n = D3D11_1_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 64 {
        log_line(&format!(
            "DDI D3D11.1 ClearView: type={view_type} rects={num_rects}"
        ));
    }
    if view_type != ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RENDERTARGETVIEW {
        log_line(&format!(
            "DDI D3D11.1 ClearView UNSUPPORTED view type {view_type} — clear dropped"
        ));
        return;
    }
    let Some(context) = d3d11_context1(h) else {
        return;
    };
    let Some(rtv) = load_rtv(view) else {
        return;
    };
    let Ok(view) = (*rtv).cast::<ID3D11View>() else {
        return;
    };
    let rgba = if color.is_null() {
        [0.0; 4]
    } else {
        [*color, *color.add(1), *color.add(2), *color.add(3)]
    };
    // A rect-limited ClearView clears ONLY the given sub-rects. The previous
    // ClearRenderTargetView forwarding cleared the WHOLE view: dwm's flip
    // composition issues ClearView(dirty-rect) each frame before redrawing
    // just that region, so every frame the accumulated desktop was wiped to
    // the (transparent-black) clear color and only the delta survived — the
    // all-zero presented-frame class. D3D10_DDI_RECT is layout-identical to
    // RECT; DXVK implements ID3D11DeviceContext1::ClearView incl. rects.
    if num_rects != 0 && !rects.is_null() {
        let rects = core::slice::from_raw_parts(
            rects as *const windows::Win32::Foundation::RECT,
            num_rects as usize,
        );
        context.ClearView(&view, &rgba, Some(rects));
    } else {
        context.ClearView(&view, &rgba, None);
    }
}

// --- Shaders ----------------------------------------------------------------

unsafe fn shader_code_len(code: *const u32) -> usize {
    if code.is_null() {
        return 0;
    }

    // D3D API bytecode is a DXBC container with the total size at byte offset 24.
    if *code == u32::from_le_bytes(*b"DXBC") {
        return *code.add(6) as usize;
    }

    // D3D UMD callbacks receive raw SHDR/SHEX token streams. The second DWORD
    // is the stream length in DWORDs, including the two-token shader header.
    let dwords = *code.add(1) as usize;
    if dwords < 2 || dwords > (1 << 20) {
        return 0;
    }

    dwords * core::mem::size_of::<u32>()
}

unsafe fn log_shader_code(kind: &str, code: *const u32, len: usize) {
    if code.is_null() {
        log_line(&format!("DDI {kind}: null shader code"));
        return;
    }
    let d0 = *code.add(0);
    let d1 = *code.add(1);
    let d2 = *code.add(2);
    let d3 = *code.add(3);
    let is_dxbc = d0 == u32::from_le_bytes(*b"DXBC");
    log_line(&format!(
        "DDI {kind}: shader len={} dxbc={} tokens={:08x} {:08x} {:08x} {:08x}",
        len, is_dxbc, d0, d1, d2, d3
    ));
}

unsafe extern "C" fn calc_size_shader(
    _h: Hdevice,
    _code: *const u32,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) -> u64 {
    8
}

unsafe extern "C" fn create_vertex_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    clear_handle(h_shader.pDrvPrivate);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_vertex_shader", code, len);
    if len == 0 {
        log_line("DDI create_vertex_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let Some(dxvk) = dev.dxvk.as_ref() else {
        return;
    };
    let raw = dxvk.create_vertex_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        let n = SHADER_BIND_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
        if n < 128 {
            log_line(&format!(
                "DDI create_vertex_shader ok: raw=0x{raw:x} len={len}"
            ));
        }
        store_raw_com(h_shader.pDrvPrivate, raw);
        // Keep the bytecode so input layouts can be created lazily (the ISGN
        // supplies the semantic names CreateInputLayout requires).
        dev.ia.borrow_mut().vs_bytecode.insert(raw, bytes.to_vec());
    } else {
        log_line("DDI create_vertex_shader failed");
    }
}

unsafe extern "C" fn create_pixel_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    clear_handle(h_shader.pDrvPrivate);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_pixel_shader", code, len);
    if len == 0 {
        log_line("DDI create_pixel_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let Some(dxvk) = dev.dxvk.as_ref() else {
        return;
    };
    let raw = dxvk.create_pixel_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        let n = SHADER_BIND_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
        if n < 128 {
            log_line(&format!(
                "DDI create_pixel_shader ok: raw=0x{raw:x} len={len}"
            ));
        }
        store_raw_com(h_shader.pDrvPrivate, raw);
    } else {
        log_line("DDI create_pixel_shader failed");
    }
}

/// Flatten a >=11.1 typed signature block into the bridge wire layout:
/// [n_in, n_out, (sysval, register, mask, comptype, stream) x n_in, same x
/// n_out]. The ENTRY2 arm is the one the >=11.1 runtime fills.
unsafe fn flatten_stage_io_signatures(
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
) -> Vec<u32> {
    let mut words = vec![0u32, 0u32];
    if sig.is_null() {
        return words;
    }
    let s = &*sig;
    let p_in = s.__bindgen_anon_1.pInputSignature;
    let p_out = s.__bindgen_anon_2.pOutputSignature;
    let n_in = if p_in.is_null() { 0 } else { s.NumInputSignatureEntries };
    let n_out = if p_out.is_null() { 0 } else { s.NumOutputSignatureEntries };
    words[0] = n_in;
    words[1] = n_out;
    for i in 0..n_in as usize {
        let e = &*p_in.add(i);
        words.extend_from_slice(&[
            e.SystemValue as u32,
            e.Register,
            e.Mask as u32,
            e.RegisterComponentType as u32,
            e.Stream as u32,
        ]);
    }
    for i in 0..n_out as usize {
        let e = &*p_out.add(i);
        words.extend_from_slice(&[
            e.SystemValue as u32,
            e.Register,
            e.Mask as u32,
            e.RegisterComponentType as u32,
            e.Stream as u32,
        ]);
    }
    words
}

/// Shared body for the >=11.1 typed shader creates. `kind`: 0 = vertex,
/// 1 = pixel, 2 = geometry (bridge convention). The typed signatures carry
/// the component types the raw token stream cannot express — without them
/// dxbc-spv declared every input float32 while dwm binds R16G16_SINT vertex
/// data (VUID-Input-08733 UB: garbage positions, nothing rasterized).
unsafe fn create_shader_11_1_common(
    h: Hdevice,
    kind: u32,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
    name: &str,
) {
    clear_handle(h_shader.pDrvPrivate);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code(name, code, len);
    if len == 0 {
        log_line(&format!("DDI {name} failed: unknown shader length"));
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let Some(dxvk) = dev.dxvk.as_ref() else {
        return;
    };
    let sig_words = flatten_stage_io_signatures(sig);
    {
        // Evidence line for the Input-08733 investigation: dump each input
        // entry's (register, mask, component type) — comptype 0 (UNKNOWN)
        // falls back to float32 in the bridge, which is UB against SINT
        // vertex formats.
        let n_in = sig_words[0] as usize;
        let mut dump = format!("DDI {name} sig entries:");
        for i in 0..n_in.min(8) {
            let base = 2 + i * 5;
            dump.push_str(&format!(
                " [r{} m0x{:x} t{}]",
                sig_words[base + 1],
                sig_words[base + 2],
                sig_words[base + 3]
            ));
        }
        log_line(&dump);
    }
    let raw = dxvk.create_shader_sig(
        kind,
        bytes.as_ptr(),
        bytes.len(),
        sig_words.as_ptr(),
        sig_words.len(),
    );
    if raw != 0 {
        let n = SHADER_BIND_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
        if n < 128 {
            log_line(&format!(
                "DDI {name} ok: raw=0x{raw:x} len={len} sig_in={} sig_out={}",
                sig_words[0], sig_words[1]
            ));
        }
        store_raw_com(h_shader.pDrvPrivate, raw);
        if kind == 0 {
            // Keep the bytecode so input layouts can be created lazily, and
            // the signature words so input-class variants can be recompiled
            // against the bound layout (resolve_vs_input_variant).
            let mut ia = dev.ia.borrow_mut();
            ia.vs_bytecode.insert(raw, bytes.to_vec());
            ia.vs_sig_words.insert(raw, sig_words);
        }
    } else {
        log_line(&format!("DDI {name} failed"));
    }
}

unsafe extern "C" fn create_vertex_shader_11_1(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
) {
    create_shader_11_1_common(h, 0, code, h_shader, sig, "create_vertex_shader_11_1");
}

unsafe extern "C" fn create_pixel_shader_11_1(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
) {
    create_shader_11_1_common(h, 1, code, h_shader, sig, "create_pixel_shader_11_1");
}

unsafe extern "C" fn create_geometry_shader_11_1(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
) {
    create_shader_11_1_common(h, 2, code, h_shader, sig, "create_geometry_shader_11_1");
}

unsafe extern "C" fn create_geometry_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    clear_handle(h_shader.pDrvPrivate);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_geometry_shader", code, len);
    if len == 0 {
        log_line("DDI create_geometry_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let Some(dxvk) = dev.dxvk.as_ref() else {
        return;
    };
    let raw = dxvk.create_geometry_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        store_raw_com(h_shader.pDrvPrivate, raw);
    } else {
        log_line("DDI create_geometry_shader failed");
    }
}

unsafe extern "C" fn calc_size_geometry_shader_so(
    _h: Hdevice,
    _arg: *const ddi::D3D11DDIARG_CREATEGEOMETRYSHADERWITHSTREAMOUTPUT,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) -> u64 {
    8
}

unsafe extern "C" fn create_geometry_shader_so(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATEGEOMETRYSHADERWITHSTREAMOUTPUT,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    clear_handle(h_shader.pDrvPrivate);
    if arg.is_null() {
        return;
    }
    let Some(dev) = helios_device(h) else {
        return;
    };
    let a = &*arg;
    let len = shader_code_len(a.pShaderCode);
    log_shader_code("create_geometry_shader_so", a.pShaderCode, len);
    if len == 0 {
        log_line("DDI create_geometry_shader_so failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(a.pShaderCode as *const u8, len);
    // Stream-output declarations need semantic names that are not present in the
    // compact DDI declaration. Create a plain GS for now; DWM's composition path
    // should not depend on SO capture.
    let Some(dxvk) = dev.dxvk.as_ref() else {
        return;
    };
    let raw = dxvk.create_geometry_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        store_raw_com(h_shader.pDrvPrivate, raw);
    } else {
        log_line("DDI create_geometry_shader_so failed");
    }
}

unsafe extern "C" fn calc_size_tess_shader(
    _h: Hdevice,
    _code: *const u32,
    _sig: *const ddi::D3D11DDIARG_TESSELLATION_IO_SIGNATURES,
) -> u64 {
    8
}

unsafe extern "C" fn create_hull_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D11DDIARG_TESSELLATION_IO_SIGNATURES,
) {
    clear_handle(h_shader.pDrvPrivate);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_hull_shader", code, len);
    if len == 0 {
        log_line("DDI create_hull_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let Some(dxvk) = dev.dxvk.as_ref() else {
        return;
    };
    let raw = dxvk.create_hull_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        store_raw_com(h_shader.pDrvPrivate, raw);
    } else {
        log_line("DDI create_hull_shader failed");
    }
}

unsafe extern "C" fn create_domain_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D11DDIARG_TESSELLATION_IO_SIGNATURES,
) {
    clear_handle(h_shader.pDrvPrivate);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_domain_shader", code, len);
    if len == 0 {
        log_line("DDI create_domain_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let Some(dxvk) = dev.dxvk.as_ref() else {
        return;
    };
    let raw = dxvk.create_domain_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        store_raw_com(h_shader.pDrvPrivate, raw);
    } else {
        log_line("DDI create_domain_shader failed");
    }
}

unsafe extern "C" fn create_compute_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
) {
    clear_handle(h_shader.pDrvPrivate);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_compute_shader", code, len);
    if len == 0 {
        log_line("DDI create_compute_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let Some(dxvk) = dev.dxvk.as_ref() else {
        return;
    };
    let raw = dxvk.create_compute_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        store_raw_com(h_shader.pDrvPrivate, raw);
    } else {
        log_line("DDI create_compute_shader failed");
    }
}

unsafe extern "C" fn destroy_shader(_h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    release_com(h_shader.pDrvPrivate);
}

unsafe extern "C" fn vs_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let com = if h_shader.pDrvPrivate.is_null() {
        0
    } else {
        *(h_shader.pDrvPrivate as *const usize)
    };
    if let Some(dev) = helios_device(h) {
        let mut ia = dev.ia.borrow_mut();
        ia.current_vs = com;
        ia.bound_vs_com = com;
    }
    let n = SHADER_BIND_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 128 {
        trace_line!("DDI VSSetShader raw=0x{com:x}");
    }
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11VertexShader>(h_shader.pDrvPrivate) {
        Some(s) => context.VSSetShader(&*s, None),
        None => context.VSSetShader(None, None),
    }
}

unsafe extern "C" fn ps_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let com = if h_shader.pDrvPrivate.is_null() {
        0
    } else {
        *(h_shader.pDrvPrivate as *const usize)
    };
    if let Some(dev) = helios_device(h) {
        dev.ia.borrow_mut().current_ps = com;
    }
    let n = SHADER_BIND_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 128 {
        trace_line!("DDI PSSetShader raw=0x{com:x}");
    }
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11PixelShader>(h_shader.pDrvPrivate) {
        Some(s) => context.PSSetShader(&*s, None),
        None => context.PSSetShader(None, None),
    }
}

unsafe extern "C" fn gs_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11GeometryShader>(h_shader.pDrvPrivate) {
        Some(s) => context.GSSetShader(&*s, None),
        None => context.GSSetShader(None, None),
    }
}

unsafe extern "C" fn hs_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11HullShader>(h_shader.pDrvPrivate) {
        Some(s) => context.HSSetShader(&*s, None),
        None => context.HSSetShader(None, None),
    }
}

unsafe extern "C" fn ds_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11DomainShader>(h_shader.pDrvPrivate) {
        Some(s) => context.DSSetShader(&*s, None),
        None => context.DSSetShader(None, None),
    }
}

unsafe extern "C" fn cs_set_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11ComputeShader>(h_shader.pDrvPrivate) {
        Some(s) => context.CSSetShader(&*s, None),
        None => context.CSSetShader(None, None),
    }
}

unsafe extern "C" fn ps_set_shader_with_ifaces(
    h: Hdevice,
    h_shader: ddi::D3D10DDI_HSHADER,
    _num_class_instances: u32,
    _class_instance_ids: *const u32,
    _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
) {
    ps_set_shader(h, h_shader);
}

unsafe extern "C" fn vs_set_shader_with_ifaces(
    h: Hdevice,
    h_shader: ddi::D3D10DDI_HSHADER,
    _num_class_instances: u32,
    _class_instance_ids: *const u32,
    _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
) {
    vs_set_shader(h, h_shader);
}

unsafe extern "C" fn gs_set_shader_with_ifaces(
    h: Hdevice,
    h_shader: ddi::D3D10DDI_HSHADER,
    _num_class_instances: u32,
    _class_instance_ids: *const u32,
    _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
) {
    gs_set_shader(h, h_shader);
}

unsafe extern "C" fn hs_set_shader_with_ifaces(
    h: Hdevice,
    h_shader: ddi::D3D10DDI_HSHADER,
    _num_class_instances: u32,
    _class_instance_ids: *const u32,
    _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
) {
    hs_set_shader(h, h_shader);
}

unsafe extern "C" fn ds_set_shader_with_ifaces(
    h: Hdevice,
    h_shader: ddi::D3D10DDI_HSHADER,
    _num_class_instances: u32,
    _class_instance_ids: *const u32,
    _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
) {
    ds_set_shader(h, h_shader);
}

unsafe extern "C" fn cs_set_shader_with_ifaces(
    h: Hdevice,
    h_shader: ddi::D3D10DDI_HSHADER,
    _num_class_instances: u32,
    _class_instance_ids: *const u32,
    _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
) {
    cs_set_shader(h, h_shader);
}

// --- Output-merger / rasterizer state setters -------------------------------

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn set_render_targets(
    h: Hdevice,
    rtvs: *const ddi::D3D10DDI_HRENDERTARGETVIEW,
    num_views: u32,
    _clear_slots: u32,
    dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW,
    _uavs: *const ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    _uav_counts: *const u32,
    _uav_start: u32,
    _num_uavs: u32,
    _uav_first: u32,
    _uav_count: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let mut views: Vec<Option<ID3D11RenderTargetView>> = Vec::with_capacity(num_views as usize);
    let mut rt0 = (0, 0, 0, 0);
    for i in 0..num_views as usize {
        let p = (*rtvs.add(i)).pDrvPrivate;
        if i == 0 {
            rt0 = rtv_info(p);
        }
        views.push(load_rtv(p).map(|m| (*m).clone()));
    }
    if let Some(dev) = helios_device(h) {
        let mut ia = dev.ia.borrow_mut();
        ia.current_rt0_alloc = rt0.0;
        ia.current_rt0_width = rt0.1;
        ia.current_rt0_height = rt0.2;
        ia.current_rt0_format = rt0.3;
    }
    let n = OM_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 256 || rt0.0 != 0 {
        trace_line!(
            "DDI OMSetRenderTargets num={} rt0_alloc=0x{:x} rt0={}x{} fmt={}",
            num_views, rt0.0, rt0.1, rt0.2, rt0.3
        );
    }
    let depth = load_com::<ID3D11DepthStencilView>(dsv.pDrvPrivate).map(|m| (*m).clone());
    context.OMSetRenderTargets(Some(&views), depth.as_ref());
}

unsafe extern "C" fn set_viewports(
    h: Hdevice,
    num: u32,
    _clear: u32,
    vps: *const ddi::D3D10_DDI_VIEWPORT,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let mut out: Vec<D3D11_VIEWPORT> = Vec::with_capacity(num as usize);
    for i in 0..num as usize {
        let v = &*vps.add(i);
        out.push(D3D11_VIEWPORT {
            TopLeftX: v.TopLeftX as f32,
            TopLeftY: v.TopLeftY as f32,
            Width: v.Width as f32,
            Height: v.Height as f32,
            MinDepth: v.MinDepth,
            MaxDepth: v.MaxDepth,
        });
    }
    context.RSSetViewports(Some(&out));
}

unsafe extern "C" fn set_scissor_rects(
    h: Hdevice,
    num: u32,
    _clear: u32,
    rects: *const ddi::D3D10_DDI_RECT,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let mut out: Vec<RECT> = Vec::with_capacity(num as usize);
    if !rects.is_null() {
        for i in 0..num as usize {
            let r = &*rects.add(i);
            out.push(RECT {
                left: r.left,
                top: r.top,
                right: r.right,
                bottom: r.bottom,
            });
        }
    }
    context.RSSetScissorRects(Some(&out));
}

unsafe extern "C" fn set_text_filter_size(_h: Hdevice, _w: u32, _hgt: u32) {}

unsafe extern "C" fn ia_set_topology(h: Hdevice, topo: ddi::D3D10_DDI_PRIMITIVE_TOPOLOGY) {
    if let Some(context) = d3d11_context(h) {
        context.IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(
            topo as i32,
        ));
    }
}

unsafe fn log_draw_state(
    h: Hdevice,
    kind: &str,
    count0: u32,
    start0: u32,
    count1: u32,
    start1: u32,
) {
    let n = DRAW_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n >= 512 && (n % 512) != 0 {
        return;
    }
    let Some(dev) = helios_device(h) else {
        return;
    };
    let ia = dev.ia.borrow();
    trace_line!(
        "DDI {kind}: a={} b={} c={} d={} vs=0x{:x} ps=0x{:x} rt0_alloc=0x{:x} rt0={}x{} fmt={} layout=0x{:x}",
        count0,
        start0,
        count1,
        start1,
        ia.current_vs,
        ia.current_ps,
        ia.current_rt0_alloc,
        ia.current_rt0_width,
        ia.current_rt0_height,
        ia.current_rt0_format,
        ia.current_layout
    );
}

unsafe extern "C" fn draw(h: Hdevice, vertex_count: u32, start_vertex: u32) {
    bind_input_layout(h);
    log_draw_state(h, "Draw", vertex_count, start_vertex, 0, 0);
    if let Some(context) = d3d11_context(h) {
        context.Draw(vertex_count, start_vertex);
    }
}

unsafe extern "C" fn draw_indexed(
    h: Hdevice,
    index_count: u32,
    start_index: u32,
    base_vertex: i32,
) {
    bind_input_layout(h);
    log_draw_state(
        h,
        "DrawIndexed",
        index_count,
        start_index,
        base_vertex as u32,
        0,
    );
    if let Some(context) = d3d11_context(h) {
        context.DrawIndexed(index_count, start_index, base_vertex);
    }
}

unsafe extern "C" fn draw_instanced(
    h: Hdevice,
    vertex_count_per_instance: u32,
    instance_count: u32,
    start_vertex: u32,
    start_instance: u32,
) {
    bind_input_layout(h);
    log_draw_state(
        h,
        "DrawInstanced",
        vertex_count_per_instance,
        start_vertex,
        instance_count,
        start_instance,
    );
    if let Some(context) = d3d11_context(h) {
        context.DrawInstanced(
            vertex_count_per_instance,
            instance_count,
            start_vertex,
            start_instance,
        );
    }
}

unsafe extern "C" fn draw_indexed_instanced(
    h: Hdevice,
    index_count_per_instance: u32,
    instance_count: u32,
    start_index: u32,
    base_vertex: i32,
    start_instance: u32,
) {
    bind_input_layout(h);
    log_draw_state(
        h,
        "DrawIndexedInstanced",
        index_count_per_instance,
        start_index,
        instance_count,
        start_instance,
    );
    if let Some(context) = d3d11_context(h) {
        context.DrawIndexedInstanced(
            index_count_per_instance,
            instance_count,
            start_index,
            base_vertex,
            start_instance,
        );
    }
}

unsafe extern "C" fn draw_auto(h: Hdevice) {
    bind_input_layout(h);
    if let Some(context) = d3d11_context(h) {
        context.DrawAuto();
    }
}

unsafe extern "C" fn draw_instanced_indirect(
    h: Hdevice,
    h_args: ddi::D3D10DDI_HRESOURCE,
    aligned_byte_offset: u32,
) {
    bind_input_layout(h);
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(buf) =
        load_resource(h_args.pDrvPrivate).and_then(|r| (*r).cast::<ID3D11Buffer>().ok())
    else {
        return;
    };
    context.DrawInstancedIndirect(&buf, aligned_byte_offset);
}

unsafe extern "C" fn draw_indexed_instanced_indirect(
    h: Hdevice,
    h_args: ddi::D3D10DDI_HRESOURCE,
    aligned_byte_offset: u32,
) {
    bind_input_layout(h);
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(buf) =
        load_resource(h_args.pDrvPrivate).and_then(|r| (*r).cast::<ID3D11Buffer>().ok())
    else {
        return;
    };
    context.DrawIndexedInstancedIndirect(&buf, aligned_byte_offset);
}

unsafe extern "C" fn so_set_targets(
    h: Hdevice,
    num: u32,
    _clear: u32,
    buffers: *const ddi::D3D10DDI_HRESOURCE,
    offsets: *const u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let mut out: Vec<Option<ID3D11Buffer>> = Vec::with_capacity(num as usize);
    if !buffers.is_null() {
        for i in 0..num as usize {
            let p = (*buffers.add(i)).pDrvPrivate;
            out.push(load_resource(p).and_then(|r| (*r).cast::<ID3D11Buffer>().ok()));
        }
    }
    context.SOSetTargets(num, Some(out.as_ptr()), Some(offsets));
}

// --- Rasterizer / depth-stencil state ---------------------------------------

unsafe extern "C" fn calc_size_raster(
    _h: Hdevice,
    _d: *const ddi::D3D10_DDI_RASTERIZER_DESC,
) -> u64 {
    8
}
unsafe extern "C" fn calc_size_depth(
    _h: Hdevice,
    _d: *const ddi::D3D10_DDI_DEPTH_STENCIL_DESC,
) -> u64 {
    8
}

unsafe extern "C" fn create_rasterizer_state(
    h: Hdevice,
    desc: *const ddi::D3D10_DDI_RASTERIZER_DESC,
    h_rs: ddi::D3D10DDI_HRASTERIZERSTATE,
    _hrt: ddi::D3D10DDI_HRTRASTERIZERSTATE,
) {
    clear_handle(h_rs.pDrvPrivate);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let d = &*desc;
    let rd = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_MODE(d.FillMode),
        CullMode: D3D11_CULL_MODE(d.CullMode),
        FrontCounterClockwise: windows::Win32::Foundation::BOOL(d.FrontCounterClockwise),
        DepthBias: d.DepthBias,
        DepthBiasClamp: d.DepthBiasClamp,
        SlopeScaledDepthBias: d.SlopeScaledDepthBias,
        DepthClipEnable: windows::Win32::Foundation::BOOL(d.DepthClipEnable),
        ScissorEnable: windows::Win32::Foundation::BOOL(d.ScissorEnable),
        MultisampleEnable: windows::Win32::Foundation::BOOL(d.MultisampleEnable),
        AntialiasedLineEnable: windows::Win32::Foundation::BOOL(d.AntialiasedLineEnable),
    };
    let mut rs: Option<ID3D11RasterizerState> = None;
    if device.CreateRasterizerState(&rd, Some(&mut rs)).is_ok() {
        if let Some(s) = rs {
            store_com(h_rs.pDrvPrivate, s);
        }
    }
}

unsafe extern "C" fn set_rasterizer_state(h: Hdevice, h_rs: ddi::D3D10DDI_HRASTERIZERSTATE) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11RasterizerState>(h_rs.pDrvPrivate) {
        Some(s) => context.RSSetState(&*s),
        None => context.RSSetState(None),
    }
}

unsafe fn cvt_stencilop(d: &ddi::D3D10_DDI_DEPTH_STENCILOP_DESC) -> D3D11_DEPTH_STENCILOP_DESC {
    D3D11_DEPTH_STENCILOP_DESC {
        StencilFailOp: D3D11_STENCIL_OP(d.StencilFailOp),
        StencilDepthFailOp: D3D11_STENCIL_OP(d.StencilDepthFailOp),
        StencilPassOp: D3D11_STENCIL_OP(d.StencilPassOp),
        StencilFunc: D3D11_COMPARISON_FUNC(d.StencilFunc),
    }
}

unsafe extern "C" fn create_depth_stencil_state(
    h: Hdevice,
    desc: *const ddi::D3D10_DDI_DEPTH_STENCIL_DESC,
    h_ds: ddi::D3D10DDI_HDEPTHSTENCILSTATE,
    _hrt: ddi::D3D10DDI_HRTDEPTHSTENCILSTATE,
) {
    clear_handle(h_ds.pDrvPrivate);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let d = &*desc;
    let dd = D3D11_DEPTH_STENCIL_DESC {
        DepthEnable: windows::Win32::Foundation::BOOL(d.DepthEnable),
        DepthWriteMask: D3D11_DEPTH_WRITE_MASK(d.DepthWriteMask),
        DepthFunc: D3D11_COMPARISON_FUNC(d.DepthFunc),
        StencilEnable: windows::Win32::Foundation::BOOL(d.StencilEnable),
        StencilReadMask: d.StencilReadMask,
        StencilWriteMask: d.StencilWriteMask,
        FrontFace: cvt_stencilop(&d.FrontFace),
        BackFace: cvt_stencilop(&d.BackFace),
    };
    let mut ds: Option<ID3D11DepthStencilState> = None;
    if device.CreateDepthStencilState(&dd, Some(&mut ds)).is_ok() {
        if let Some(s) = ds {
            store_com(h_ds.pDrvPrivate, s);
        }
    }
}

unsafe extern "C" fn set_depth_stencil_state(
    h: Hdevice,
    h_ds: ddi::D3D10DDI_HDEPTHSTENCILSTATE,
    stencil_ref: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    match load_com::<ID3D11DepthStencilState>(h_ds.pDrvPrivate) {
        Some(s) => context.OMSetDepthStencilState(&*s, stencil_ref),
        None => context.OMSetDepthStencilState(None, stencil_ref),
    }
}

unsafe extern "C" fn destroy_raster_state(_h: Hdevice, h_state: ddi::D3D10DDI_HRASTERIZERSTATE) {
    release_com(h_state.pDrvPrivate);
}

// --- Shader resource views, samplers, constant buffers ----------------------

unsafe extern "C" fn calc_size_srv(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATESHADERRESOURCEVIEW,
) -> u64 {
    8
}

unsafe extern "C" fn create_srv(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATESHADERRESOURCEVIEW,
    h_srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW,
    _hrt: ddi::D3D10DDI_HRTSHADERRESOURCEVIEW,
) {
    clear_handle(h_srv.pDrvPrivate);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource.pDrvPrivate) else {
        return;
    };
    let Some(desc) = srv_desc(a) else {
        log_line(&format!(
            "DDI create_srv: unsupported resource dimension {} fmt={}",
            a.ResourceDimension, a.Format
        ));
        return;
    };
    let mut srv: Option<ID3D11ShaderResourceView> = None;
    match device.CreateShaderResourceView(&*res, Some(&desc), Some(&mut srv)) {
        Ok(()) => {
            if let Some(v) = srv {
                let allocation = resource_allocation(a.hDrvResource.pDrvPrivate);
                let n = VIEW_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                if n < 256 || allocation != 0 {
                    let (width, height) = resource_dimensions(a.hDrvResource.pDrvPrivate);
                    trace_line!(
                        "DDI create_srv ok: hpriv={:p} alloc=0x{:x} dim={} fmt={} {}x{}",
                        h_srv.pDrvPrivate, allocation, a.ResourceDimension, a.Format, width, height
                    );
                }
                store_com(h_srv.pDrvPrivate, v);
            }
        }
        Err(e) => log_line(&format!(
            "DDI create_srv failed: dim={} fmt={} {e:?}",
            a.ResourceDimension, a.Format
        )),
    }
}

unsafe fn srv_desc(
    a: &ddi::D3D11DDIARG_CREATESHADERRESOURCEVIEW,
) -> Option<D3D11_SHADER_RESOURCE_VIEW_DESC> {
    let format = DXGI_FORMAT(a.Format as i32);
    match a.ResourceDimension {
        RES_BUFFER => {
            let b = a.__bindgen_anon_1.Buffer;
            Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_BUFFER,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D11_BUFFER_SRV {
                        Anonymous1: D3D11_BUFFER_SRV_0 {
                            FirstElement: b.__bindgen_anon_1.FirstElement,
                        },
                        Anonymous2: D3D11_BUFFER_SRV_1 {
                            NumElements: b.__bindgen_anon_2.NumElements,
                        },
                    },
                },
            })
        }
        RES_BUFFEREX => {
            let b = a.__bindgen_anon_1.BufferEx;
            Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_BUFFEREX,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    BufferEx: D3D11_BUFFEREX_SRV {
                        FirstElement: b.__bindgen_anon_1.FirstElement,
                        NumElements: b.__bindgen_anon_2.NumElements,
                        Flags: b.Flags,
                    },
                },
            })
        }
        RES_TEX1D => {
            let t = a.__bindgen_anon_1.Tex1D;
            if t.ArraySize > 1 {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURE1DARRAY,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture1DArray: D3D11_TEX1D_ARRAY_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURE1D,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture1D: D3D11_TEX1D_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                        },
                    },
                })
            }
        }
        RES_TEX2D => {
            let t = a.__bindgen_anon_1.Tex2D;
            if t.ArraySize > 1 {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            FirstArraySlice: t.FirstArraySlice,
                            ArraySize: t.ArraySize,
                        },
                    },
                })
            } else {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                        },
                    },
                })
            }
        }
        RES_TEX3D => {
            let t = a.__bindgen_anon_1.Tex3D;
            Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_TEXTURE3D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture3D: D3D11_TEX3D_SRV {
                        MostDetailedMip: t.MostDetailedMip,
                        MipLevels: t.MipLevels,
                    },
                },
            })
        }
        RES_TEXCUBE => {
            let t = a.__bindgen_anon_1.TexCube;
            if t.NumCubes > 1 {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURECUBEARRAY,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        TextureCubeArray: D3D11_TEXCUBE_ARRAY_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                            First2DArrayFace: t.First2DArrayFace,
                            NumCubes: t.NumCubes,
                        },
                    },
                })
            } else {
                Some(D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_SRV_DIMENSION_TEXTURECUBE,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        TextureCube: D3D11_TEXCUBE_SRV {
                            MostDetailedMip: t.MostDetailedMip,
                            MipLevels: t.MipLevels,
                        },
                    },
                })
            }
        }
        _ => None,
    }
}

unsafe extern "C" fn destroy_srv(_h: Hdevice, h_srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW) {
    release_com(h_srv.pDrvPrivate);
}

unsafe extern "C" fn gen_mips(h: Hdevice, h_srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(srv) = load_com::<ID3D11ShaderResourceView>(h_srv.pDrvPrivate) else {
        return;
    };
    context.GenerateMips(&*srv);
}

unsafe extern "C" fn calc_size_uav(
    _h: Hdevice,
    _a: *const ddi::D3D11DDIARG_CREATEUNORDEREDACCESSVIEW,
) -> u64 {
    8
}

unsafe extern "C" fn create_uav(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATEUNORDEREDACCESSVIEW,
    h_uav: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    _hrt: ddi::D3D11DDI_HRTUNORDEREDACCESSVIEW,
) {
    clear_handle(h_uav.pDrvPrivate);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource.pDrvPrivate) else {
        return;
    };
    let mut uav: Option<ID3D11UnorderedAccessView> = None;
    match device.CreateUnorderedAccessView(&*res, None, Some(&mut uav)) {
        Ok(()) => {
            if let Some(v) = uav {
                store_com(h_uav.pDrvPrivate, v);
            }
        }
        Err(e) => log_line(&format!("DDI create_uav failed: {e:?}")),
    }
}

unsafe extern "C" fn destroy_uav(_h: Hdevice, h_uav: ddi::D3D11DDI_HUNORDEREDACCESSVIEW) {
    release_com(h_uav.pDrvPrivate);
}

unsafe extern "C" fn clear_uav_uint(
    h: Hdevice,
    h_uav: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    values: *const u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(uav) = load_com::<ID3D11UnorderedAccessView>(h_uav.pDrvPrivate) else {
        return;
    };
    let v = if values.is_null() {
        [0u32; 4]
    } else {
        [*values, *values.add(1), *values.add(2), *values.add(3)]
    };
    context.ClearUnorderedAccessViewUint(&*uav, &v);
}

unsafe extern "C" fn clear_uav_float(
    h: Hdevice,
    h_uav: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    values: *const f32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(uav) = load_com::<ID3D11UnorderedAccessView>(h_uav.pDrvPrivate) else {
        return;
    };
    let v = if values.is_null() {
        [0.0f32; 4]
    } else {
        [*values, *values.add(1), *values.add(2), *values.add(3)]
    };
    context.ClearUnorderedAccessViewFloat(&*uav, &v);
}

unsafe extern "C" fn cs_set_uavs(
    h: Hdevice,
    start: u32,
    num: u32,
    uavs: *const ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    counts: *const u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let mut out: Vec<Option<ID3D11UnorderedAccessView>> = Vec::with_capacity(num as usize);
    if !uavs.is_null() {
        for i in 0..num as usize {
            let p = (*uavs.add(i)).pDrvPrivate;
            out.push(load_com::<ID3D11UnorderedAccessView>(p).map(|m| (*m).clone()));
        }
    }
    context.CSSetUnorderedAccessViews(start, num, Some(out.as_ptr()), Some(counts));
}

unsafe extern "C" fn copy_structure_count(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    dst_offset: u32,
    h_src: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(dst) = load_resource(h_dst.pDrvPrivate).and_then(|r| (*r).cast::<ID3D11Buffer>().ok())
    else {
        return;
    };
    let Some(src) = load_com::<ID3D11UnorderedAccessView>(h_src.pDrvPrivate) else {
        return;
    };
    context.CopyStructureCount(&dst, dst_offset, &*src);
}

unsafe extern "C" fn calc_size_sampler(_h: Hdevice, _d: *const ddi::D3D10_DDI_SAMPLER_DESC) -> u64 {
    8
}

unsafe extern "C" fn create_sampler(
    h: Hdevice,
    desc: *const ddi::D3D10_DDI_SAMPLER_DESC,
    h_sampler: ddi::D3D10DDI_HSAMPLER,
    _hrt: ddi::D3D10DDI_HRTSAMPLER,
) {
    clear_handle(h_sampler.pDrvPrivate);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let d = &*desc;
    let sd = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER(d.Filter),
        AddressU: D3D11_TEXTURE_ADDRESS_MODE(d.AddressU),
        AddressV: D3D11_TEXTURE_ADDRESS_MODE(d.AddressV),
        AddressW: D3D11_TEXTURE_ADDRESS_MODE(d.AddressW),
        MipLODBias: d.MipLODBias,
        MaxAnisotropy: d.MaxAnisotropy,
        ComparisonFunc: D3D11_COMPARISON_FUNC(d.ComparisonFunc),
        BorderColor: d.BorderColor,
        MinLOD: d.MinLOD,
        MaxLOD: d.MaxLOD,
    };
    let mut s: Option<ID3D11SamplerState> = None;
    if device.CreateSamplerState(&sd, Some(&mut s)).is_ok() {
        if let Some(o) = s {
            store_com(h_sampler.pDrvPrivate, o);
        }
    }
}

unsafe extern "C" fn destroy_sampler(_h: Hdevice, h_sampler: ddi::D3D10DDI_HSAMPLER) {
    release_com(h_sampler.pDrvPrivate);
}

unsafe fn collect_buffers(
    start: u32,
    num: u32,
    h: *const ddi::D3D10DDI_HRESOURCE,
) -> Vec<Option<ID3D11Buffer>> {
    let _ = start;
    let mut v = Vec::with_capacity(num as usize);
    for i in 0..num as usize {
        let p = (*h.add(i)).pDrvPrivate;
        v.push(load_resource(p).and_then(|r| (*r).cast::<ID3D11Buffer>().ok()));
    }
    v
}
unsafe fn collect_srvs(
    num: u32,
    h: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) -> Vec<Option<ID3D11ShaderResourceView>> {
    let mut v = Vec::with_capacity(num as usize);
    for i in 0..num as usize {
        let p = (*h.add(i)).pDrvPrivate;
        v.push(load_com::<ID3D11ShaderResourceView>(p).map(|m| (*m).clone()));
    }
    v
}
unsafe fn collect_samplers(
    num: u32,
    h: *const ddi::D3D10DDI_HSAMPLER,
) -> Vec<Option<ID3D11SamplerState>> {
    let mut v = Vec::with_capacity(num as usize);
    for i in 0..num as usize {
        let p = (*h.add(i)).pDrvPrivate;
        v.push(load_com::<ID3D11SamplerState>(p).map(|m| (*m).clone()));
    }
    v
}

unsafe extern "C" fn ps_set_constant_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
) {
    if let Some(c) = d3d11_context(h) {
        c.PSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}
unsafe extern "C" fn vs_set_constant_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
) {
    if let Some(c) = d3d11_context(h) {
        c.VSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}
unsafe extern "C" fn gs_set_constant_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
) {
    if let Some(c) = d3d11_context(h) {
        c.GSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}
unsafe extern "C" fn hs_set_constant_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
) {
    if let Some(c) = d3d11_context(h) {
        c.HSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}
unsafe extern "C" fn ds_set_constant_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
) {
    if let Some(c) = d3d11_context(h) {
        c.DSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}
unsafe extern "C" fn cs_set_constant_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
) {
    if let Some(c) = d3d11_context(h) {
        c.CSSetConstantBuffers(start, Some(&collect_buffers(start, num, bufs)));
    }
}

unsafe fn set_constant_buffers1_common(
    h: Hdevice,
    stage: &str,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    let Some(c) = d3d11_context1(h) else {
        return;
    };
    let buffers = collect_buffers(start, num, bufs);
    let buffers_ptr = if num == 0 {
        None
    } else {
        Some(buffers.as_ptr())
    };
    let first_ptr = if first_constants.is_null() {
        None
    } else {
        Some(first_constants)
    };
    let count_ptr = if num_constants.is_null() {
        None
    } else {
        Some(num_constants)
    };
    let n = SHADER_BIND_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 256 {
        let first0 = if first_constants.is_null() {
            0
        } else {
            *first_constants
        };
        let count0 = if num_constants.is_null() {
            0
        } else {
            *num_constants
        };
        log_line(&format!(
            "DDI {stage}SetConstantBuffers1 start={} num={} first_ptr={} count_ptr={} first0={} count0={}",
            start, num, !first_constants.is_null(), !num_constants.is_null(), first0, count0
        ));
    }
    match stage {
        "VS" => c.VSSetConstantBuffers1(start, num, buffers_ptr, first_ptr, count_ptr),
        "PS" => c.PSSetConstantBuffers1(start, num, buffers_ptr, first_ptr, count_ptr),
        "GS" => c.GSSetConstantBuffers1(start, num, buffers_ptr, first_ptr, count_ptr),
        "HS" => c.HSSetConstantBuffers1(start, num, buffers_ptr, first_ptr, count_ptr),
        "DS" => c.DSSetConstantBuffers1(start, num, buffers_ptr, first_ptr, count_ptr),
        "CS" => c.CSSetConstantBuffers1(start, num, buffers_ptr, first_ptr, count_ptr),
        _ => {}
    }
}

unsafe extern "C" fn vs_set_constant_buffers1(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    set_constant_buffers1_common(h, "VS", start, num, bufs, first_constants, num_constants);
}

unsafe extern "C" fn ps_set_constant_buffers1(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    set_constant_buffers1_common(h, "PS", start, num, bufs, first_constants, num_constants);
}

unsafe extern "C" fn gs_set_constant_buffers1(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    set_constant_buffers1_common(h, "GS", start, num, bufs, first_constants, num_constants);
}

unsafe extern "C" fn hs_set_constant_buffers1(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    set_constant_buffers1_common(h, "HS", start, num, bufs, first_constants, num_constants);
}

unsafe extern "C" fn ds_set_constant_buffers1(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    set_constant_buffers1_common(h, "DS", start, num, bufs, first_constants, num_constants);
}

unsafe extern "C" fn cs_set_constant_buffers1(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    set_constant_buffers1_common(h, "CS", start, num, bufs, first_constants, num_constants);
}

unsafe extern "C" fn ps_set_shader_resources(
    h: Hdevice,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    if let Some(c) = d3d11_context(h) {
        let n = SHADER_BIND_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
        if n < 256 && !srvs.is_null() {
            let mut nonnull = 0u32;
            let mut first_priv: *mut c_void = core::ptr::null_mut();
            let mut second_priv: *mut c_void = core::ptr::null_mut();
            for i in 0..num as usize {
                let p = (*srvs.add(i)).pDrvPrivate;
                if !p.is_null() && !(*(p as *const *mut c_void)).is_null() {
                    nonnull += 1;
                    if first_priv.is_null() {
                        first_priv = p;
                    } else if second_priv.is_null() {
                        second_priv = p;
                    }
                }
            }
            trace_line!(
                "DDI PSSetShaderResources start={} num={} nonnull={} first={:p} second={:p}",
                start, num, nonnull, first_priv, second_priv
            );
        }
        c.PSSetShaderResources(start, Some(&collect_srvs(num, srvs)));
    }
}
unsafe extern "C" fn vs_set_shader_resources(
    h: Hdevice,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    if let Some(c) = d3d11_context(h) {
        c.VSSetShaderResources(start, Some(&collect_srvs(num, srvs)));
    }
}
unsafe extern "C" fn gs_set_shader_resources(
    h: Hdevice,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    if let Some(c) = d3d11_context(h) {
        c.GSSetShaderResources(start, Some(&collect_srvs(num, srvs)));
    }
}
unsafe extern "C" fn hs_set_shader_resources(
    h: Hdevice,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    if let Some(c) = d3d11_context(h) {
        c.HSSetShaderResources(start, Some(&collect_srvs(num, srvs)));
    }
}
unsafe extern "C" fn ds_set_shader_resources(
    h: Hdevice,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    if let Some(c) = d3d11_context(h) {
        c.DSSetShaderResources(start, Some(&collect_srvs(num, srvs)));
    }
}
unsafe extern "C" fn cs_set_shader_resources(
    h: Hdevice,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    if let Some(c) = d3d11_context(h) {
        c.CSSetShaderResources(start, Some(&collect_srvs(num, srvs)));
    }
}
unsafe extern "C" fn ps_set_samplers(
    h: Hdevice,
    start: u32,
    num: u32,
    samplers: *const ddi::D3D10DDI_HSAMPLER,
) {
    if let Some(c) = d3d11_context(h) {
        c.PSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}
unsafe extern "C" fn vs_set_samplers(
    h: Hdevice,
    start: u32,
    num: u32,
    samplers: *const ddi::D3D10DDI_HSAMPLER,
) {
    if let Some(c) = d3d11_context(h) {
        c.VSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}
unsafe extern "C" fn gs_set_samplers(
    h: Hdevice,
    start: u32,
    num: u32,
    samplers: *const ddi::D3D10DDI_HSAMPLER,
) {
    if let Some(c) = d3d11_context(h) {
        c.GSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}
unsafe extern "C" fn hs_set_samplers(
    h: Hdevice,
    start: u32,
    num: u32,
    samplers: *const ddi::D3D10DDI_HSAMPLER,
) {
    if let Some(c) = d3d11_context(h) {
        c.HSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}
unsafe extern "C" fn ds_set_samplers(
    h: Hdevice,
    start: u32,
    num: u32,
    samplers: *const ddi::D3D10DDI_HSAMPLER,
) {
    if let Some(c) = d3d11_context(h) {
        c.DSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}
unsafe extern "C" fn cs_set_samplers(
    h: Hdevice,
    start: u32,
    num: u32,
    samplers: *const ddi::D3D10DDI_HSAMPLER,
) {
    if let Some(c) = d3d11_context(h) {
        c.CSSetSamplers(start, Some(&collect_samplers(num, samplers)));
    }
}

unsafe extern "C" fn resource_update_subresource(
    h: Hdevice,
    h_res: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
    data: *const c_void,
    row_pitch: u32,
    depth_pitch: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_resource(h_res.pDrvPrivate) else {
        return;
    };
    let bx;
    let bx_ptr = if box_.is_null() {
        None
    } else {
        let b = &*box_;
        bx = D3D11_BOX {
            left: b.left as u32,
            top: b.top as u32,
            front: b.front as u32,
            right: b.right as u32,
            bottom: b.bottom as u32,
            back: b.back as u32,
        };
        Some(&bx as *const D3D11_BOX)
    };
    context.UpdateSubresource(&*res, subresource, bx_ptr, data, row_pitch, depth_pitch);
}

unsafe extern "C" fn resource_update_subresource_11_1(
    h: Hdevice,
    h_res: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
    data: *const c_void,
    row_pitch: u32,
    depth_pitch: u32,
    _copy_flags: u32,
) {
    resource_update_subresource(h, h_res, subresource, box_, data, row_pitch, depth_pitch);
}

// --- Queries / counters -----------------------------------------------------

unsafe extern "C" fn calc_size_query(_h: Hdevice, _a: *const ddi::D3D10DDIARG_CREATEQUERY) -> u64 {
    8
}

unsafe extern "C" fn create_query(
    h: Hdevice,
    arg: *const ddi::D3D10DDIARG_CREATEQUERY,
    h_query: ddi::D3D10DDI_HQUERY,
    _hrt: ddi::D3D10DDI_HRTQUERY,
) {
    clear_handle(h_query.pDrvPrivate);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let a = &*arg;
    let desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY(a.Query),
        MiscFlags: a.MiscFlags,
    };
    let mut q: Option<ID3D11Query> = None;
    match device.CreateQuery(&desc, Some(&mut q)) {
        Ok(()) => {
            if let Some(query) = q {
                store_com(h_query.pDrvPrivate, query);
            }
        }
        Err(e) => log_line(&format!("DDI create_query failed: {e:?}")),
    }
}

unsafe extern "C" fn destroy_query(_h: Hdevice, h_query: ddi::D3D10DDI_HQUERY) {
    release_com(h_query.pDrvPrivate);
}

unsafe extern "C" fn query_begin(h: Hdevice, h_query: ddi::D3D10DDI_HQUERY) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(q) = load_com::<ID3D11Query>(h_query.pDrvPrivate) else {
        return;
    };
    if let Ok(async_) = (*q).cast::<ID3D11Asynchronous>() {
        context.Begin(&async_);
    }
}

unsafe extern "C" fn query_end(h: Hdevice, h_query: ddi::D3D10DDI_HQUERY) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(q) = load_com::<ID3D11Query>(h_query.pDrvPrivate) else {
        return;
    };
    if let Ok(async_) = (*q).cast::<ID3D11Asynchronous>() {
        context.End(&async_);
    }
}

unsafe extern "C" fn query_get_data(
    h: Hdevice,
    h_query: ddi::D3D10DDI_HQUERY,
    data: *mut c_void,
    data_size: u32,
    flags: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(q) = load_com::<ID3D11Query>(h_query.pDrvPrivate) else {
        return;
    };
    if let Ok(async_) = (*q).cast::<ID3D11Asynchronous>() {
        let _ = context.GetData(&async_, Some(data), data_size, flags);
    }
}

unsafe extern "C" fn set_predication(
    h: Hdevice,
    h_query: ddi::D3D10DDI_HQUERY,
    predicate_value: i32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let predicate = load_com::<ID3D11Query>(h_query.pDrvPrivate)
        .and_then(|q| (*q).cast::<ID3D11Predicate>().ok());
    context.SetPredication(predicate.as_ref(), predicate_value != 0);
}

/// Current D3D11 caps profile for Helios: expose a conservative FL10.0 device
/// with no multisample render targets. The Microsoft runtime validates
/// `CheckFormatSupport` and `CheckMultisampleQualityLevels` as a coherent
/// feature-level contract during `CDevice::LLOCompleteLayerConstruction`; forwarding
/// DXVK's host-derived MSAA caps over-reports support for some formats and makes
/// the runtime reject the adapter with `DXGI_ERROR_UNSUPPORTED`.
fn helios_multisample_quality_levels(_fmt: ddi::DXGI_FORMAT, sample_count: u32) -> u32 {
    if sample_count == 1 {
        1
    } else {
        0
    }
}

unsafe extern "C" fn check_multisample_quality_levels(
    _h: Hdevice,
    fmt: ddi::DXGI_FORMAT,
    sample_count: u32,
    out: *mut u32,
) {
    if !out.is_null() {
        *out = helios_multisample_quality_levels(fmt, sample_count);
    }
}

unsafe extern "C" fn check_multisample_quality_levels_wddm1_3(
    _h: Hdevice,
    fmt: ddi::DXGI_FORMAT,
    sample_count: u32,
    _flags: u32,
    out: *mut u32,
) {
    if !out.is_null() {
        *out = helios_multisample_quality_levels(fmt, sample_count);
    }
}

/// `pfnCheckCounterInfo` — report the device's performance-counter capabilities.
/// Previously an unimplemented noop that left the out struct unwritten, so the
/// D3D11 runtime read whatever it had pre-set for `NumSimultaneousCounters` /
/// `NumDetectableParallelUnits` (potentially garbage → over-allocation/validation
/// failure during `LLOCompleteLayerConstruction`). We expose no device-dependent
/// counters: zero the struct (LastDeviceDependentCounter = 0, 0 simultaneous
/// counters) and report a single detectable parallel unit. PATH-A (2026-06-22).
unsafe extern "C" fn check_counter_info(_h: Hdevice, info: *mut ddi::D3D10DDI_COUNTER_INFO) {
    if !info.is_null() {
        core::ptr::write_bytes(
            info as *mut u8,
            0,
            core::mem::size_of::<ddi::D3D10DDI_COUNTER_INFO>(),
        );
        (*info).NumDetectableParallelUnits = 1;
    }
}

unsafe extern "C" fn check_counter(
    _h: Hdevice,
    _query: ddi::D3D10DDI_QUERY,
    counter_type: *mut ddi::D3D10DDI_COUNTER_TYPE,
    active_counters: *mut u32,
    _name: ddi::LPSTR,
    name_len: *mut u32,
    _units: ddi::LPSTR,
    units_len: *mut u32,
    _description: ddi::LPSTR,
    description_len: *mut u32,
) {
    if !counter_type.is_null() {
        *counter_type = ddi::D3D10DDI_COUNTER_TYPE_D3D10DDI_COUNTER_TYPE_UINT64;
    }
    if !active_counters.is_null() {
        *active_counters = 0;
    }
    if !name_len.is_null() {
        *name_len = 0;
    }
    if !units_len.is_null() {
        *units_len = 0;
    }
    if !description_len.is_null() {
        *description_len = 0;
    }
}

// --- Compute ---------------------------------------------------------------

unsafe extern "C" fn dispatch(h: Hdevice, x: u32, y: u32, z: u32) {
    if let Some(context) = d3d11_context(h) {
        context.Dispatch(x, y, z);
    }
}

unsafe extern "C" fn dispatch_indirect(
    h: Hdevice,
    h_args: ddi::D3D10DDI_HRESOURCE,
    aligned_byte_offset: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(buf) =
        load_resource(h_args.pDrvPrivate).and_then(|r| (*r).cast::<ID3D11Buffer>().ok())
    else {
        return;
    };
    context.DispatchIndirect(&buf, aligned_byte_offset);
}

unsafe extern "C" fn set_resource_min_lod(
    h: Hdevice,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    min_lod: f32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_resource(h_resource.pDrvPrivate) else {
        return;
    };
    context.SetResourceMinLOD(&*res, min_lod);
}

unsafe extern "C" fn check_format_support(h: Hdevice, fmt: ddi::DXGI_FORMAT, out: *mut u32) {
    // The D3D11 DDI `pfnCheckFormatSupport` returns API-style D3D11_FORMAT_SUPPORT
    // flags (D3D11 harmonized the DDI with the API enum; the small
    // D3D10_DDI_FORMAT_SUPPORT enum is only for the legacy D3D10 DDI). So pass
    // DXVK's value through unchanged — translating to the D3D10 DDI layout
    // regresses even a plain D3D11CreateDevice to DXGI_ERROR_UNSUPPORTED.
    let mut caps: u32 = 0;
    if let Some(device) = d3d11_device(h) {
        if let Ok(c) = device.CheckFormatSupport(DXGI_FORMAT(fmt as i32)) {
            caps = c;
        }
    }
    // Keep format support coherent with the current FL10.0/no-MSAA profile.
    // API D3D11_FORMAT_SUPPORT: MULTISAMPLE_RESOLVE=0x40000,
    // MULTISAMPLE_RENDERTARGET=0x200000, MULTISAMPLE_LOAD=0x400000.
    caps &= !0x0064_0000u32;

    // The Microsoft D3D11 runtime validates some typeless/depth format families
    // as a group during CDevice::LLOCompleteLayerConstruction. DXVK reports the
    // host's raw SO_BUFFER support for the color-typed siblings (for example
    // R32_FLOAT), while the matching depth format (D32_FLOAT) reports none; that
    // mismatch is rejected with DXGI_ERROR_UNSUPPORTED. Normalize the family to
    // the stricter depth-compatible answer.
    const D3D11_FORMAT_SUPPORT_SO_BUFFER: u32 = 0x0000_0008;
    const DXGI_FORMAT_R32_TYPELESS: ddi::DXGI_FORMAT = 39;
    const DXGI_FORMAT_D32_FLOAT: ddi::DXGI_FORMAT = 40;
    const DXGI_FORMAT_R32_FLOAT: ddi::DXGI_FORMAT = 41;
    const DXGI_FORMAT_R32_UINT: ddi::DXGI_FORMAT = 42;
    const DXGI_FORMAT_R32_SINT: ddi::DXGI_FORMAT = 43;
    const DXGI_FORMAT_R24G8_TYPELESS: ddi::DXGI_FORMAT = 44;
    const DXGI_FORMAT_D24_UNORM_S8_UINT: ddi::DXGI_FORMAT = 45;
    const DXGI_FORMAT_R24_UNORM_X8_TYPELESS: ddi::DXGI_FORMAT = 46;
    const DXGI_FORMAT_X24_TYPELESS_G8_UINT: ddi::DXGI_FORMAT = 47;
    const DXGI_FORMAT_R32G8X24_TYPELESS: ddi::DXGI_FORMAT = 19;
    const DXGI_FORMAT_D32_FLOAT_S8X24_UINT: ddi::DXGI_FORMAT = 20;
    const DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS: ddi::DXGI_FORMAT = 21;
    const DXGI_FORMAT_X32_TYPELESS_G8X24_UINT: ddi::DXGI_FORMAT = 22;
    if matches!(
        fmt,
        DXGI_FORMAT_R32_TYPELESS
            | DXGI_FORMAT_D32_FLOAT
            | DXGI_FORMAT_R32_FLOAT
            | DXGI_FORMAT_R32_UINT
            | DXGI_FORMAT_R32_SINT
            | DXGI_FORMAT_R24G8_TYPELESS
            | DXGI_FORMAT_D24_UNORM_S8_UINT
            | DXGI_FORMAT_R24_UNORM_X8_TYPELESS
            | DXGI_FORMAT_X24_TYPELESS_G8_UINT
            | DXGI_FORMAT_R32G8X24_TYPELESS
            | DXGI_FORMAT_D32_FLOAT_S8X24_UINT
            | DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS
            | DXGI_FORMAT_X32_TYPELESS_G8X24_UINT
    ) {
        caps &= !D3D11_FORMAT_SUPPORT_SO_BUFFER;
    }

    // `DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM` (89) is the one format the WDDM
    // runtime validates specially during device creation: the driver MUST signal
    // lack of support with the explicit `D3D10_DDI_FORMAT_SUPPORT_NOT_SUPPORTED`
    // sentinel (0x80000000, "Set only this bit") rather than a bare 0. DXVK does
    // not implement this legacy XR format and returns 0, which the runtime treats
    // as a malformed response and fails `D3D11CreateDevice` with
    // `DXGI_ERROR_UNSUPPORTED` (0x887a0020) — the only caps=0 format, observed
    // live. That is the device-create failure DWM hits, after which
    // dwmcore!CreateD3D11Device raises the DWM error 0x889800b0 and crash-loops.
    // Map the 0 to the sentinel so the runtime accepts the (legitimately
    // unsupported) format. PATH-A (2026-06-22).
    const DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM: ddi::DXGI_FORMAT = 89;
    const DDI_FORMAT_SUPPORT_NOT_SUPPORTED: u32 = 0x8000_0000;
    if fmt == DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM && caps == 0 {
        caps = DDI_FORMAT_SUPPORT_NOT_SUPPORTED;
    }
    if !out.is_null() {
        *out = caps;
    }
}
unsafe extern "C" fn destroy_depth_state(_h: Hdevice, h_state: ddi::D3D10DDI_HDEPTHSTENCILSTATE) {
    release_com(h_state.pDrvPrivate);
}

/// In-process offscreen clear+readback through the real forwarders (no install,
/// no DXGI, no runtime): create a render-target texture, clear it, copy to a
/// staging texture, map and read back pixel 0. Returns 0 on PASS. `h` is the
/// device handle whose pDrvPrivate is a constructed HeliosDevice.
pub unsafe fn selftest_offscreen_clear(h: Hdevice) -> i32 {
    let fmt_bgra: ddi::DXGI_FORMAT = 87; // DXGI_FORMAT_B8G8R8A8_UNORM
    let mip = ddi::D3D10DDI_MIPINFO {
        TexelWidth: 64,
        TexelHeight: 64,
        TexelDepth: 1,
        PhysicalWidth: 64,
        PhysicalHeight: 64,
        PhysicalDepth: 1,
    };

    let mut rt_desc = ddi::D3D11DDIARG_CREATERESOURCE::default();
    rt_desc.pMipInfoList = &mip;
    rt_desc.ResourceDimension = RES_TEX2D;
    rt_desc.Format = fmt_bgra;
    rt_desc.Usage = 0; // DEFAULT
    rt_desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
    rt_desc.MipLevels = 1;
    rt_desc.ArraySize = 1;
    rt_desc.SampleDesc.Count = 1;

    let mut rt_priv = 0u64;
    let h_rt = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut rt_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &rt_desc, h_rt, Default::default());
    if rt_priv == 0 {
        log_line("selftest_offscreen_clear: RT create failed");
        return 1;
    }

    let mut rtv_desc = ddi::D3D10DDIARG_CREATERENDERTARGETVIEW::default();
    rtv_desc.hDrvResource = h_rt;
    rtv_desc.Format = fmt_bgra;
    rtv_desc.ResourceDimension = RES_TEX2D;
    let mut rtv_priv = 0u64;
    let h_rtv = ddi::D3D10DDI_HRENDERTARGETVIEW {
        pDrvPrivate: &mut rtv_priv as *mut u64 as *mut c_void,
    };
    create_rtv(h, &rtv_desc, h_rtv, Default::default());
    if rtv_priv == 0 {
        log_line("selftest_offscreen_clear: RTV create failed");
        return 2;
    }

    let mut color = [0.25f32, 0.5, 0.75, 1.0];
    clear_rtv(h, h_rtv, color.as_mut_ptr());

    let mut st_desc = ddi::D3D11DDIARG_CREATERESOURCE::default();
    st_desc.pMipInfoList = &mip;
    st_desc.ResourceDimension = RES_TEX2D;
    st_desc.Format = fmt_bgra;
    st_desc.Usage = 3; // STAGING
    st_desc.BindFlags = 0;
    st_desc.MipLevels = 1;
    st_desc.ArraySize = 1;
    st_desc.SampleDesc.Count = 1;
    let mut st_priv = 0u64;
    let h_st = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut st_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &st_desc, h_st, Default::default());
    if st_priv == 0 {
        log_line("selftest_offscreen_clear: staging create failed");
        return 3;
    }

    resource_copy(h, h_st, h_rt);
    flush(h);

    let mut mapped = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
    resource_map(h, h_st, 0, 1 /* D3D10_DDI_MAP_READ */, 0, &mut mapped);
    let mut result = 4;
    if !mapped.pData.is_null() {
        let px = mapped.pData as *const u8;
        let (b, g, r, a) = (*px, *px.add(1), *px.add(2), *px.add(3));
        log_line(&format!(
            "selftest_offscreen_clear: readback BGRA = {b} {g} {r} {a} (want ~191 128 64 255)"
        ));
        let ok = (b as i32 - 191).abs() <= 2
            && (g as i32 - 128).abs() <= 2
            && (r as i32 - 64).abs() <= 2
            && a == 255;
        result = if ok { 0 } else { 5 };
        resource_unmap(h, h_st, 0);
    }

    destroy_resource(h, h_st);
    destroy_rtv(h, h_rtv);
    destroy_resource(h, h_rt);
    log_line(&format!(
        "selftest_offscreen_clear: result={result} (0=PASS)"
    ));
    result
}

unsafe fn compile_hlsl(src: &[u8], entry: &[u8], target: &[u8]) -> Option<ID3DBlob> {
    let mut blob: Option<ID3DBlob> = None;
    let mut errs: Option<ID3DBlob> = None;
    let hr = D3DCompile(
        src.as_ptr() as *const c_void,
        src.len(),
        PCSTR::null(),
        None,
        None,
        PCSTR(entry.as_ptr()),
        PCSTR(target.as_ptr()),
        0,
        0,
        &mut blob,
        Some(&mut errs),
    );
    if hr.is_err() {
        if let Some(e) = errs {
            let msg =
                core::slice::from_raw_parts(e.GetBufferPointer() as *const u8, e.GetBufferSize());
            log_line(&format!(
                "D3DCompile error: {}",
                String::from_utf8_lossy(msg)
            ));
        }
        return None;
    }
    blob
}

/// Synthesize + create a tex2d via the create_resource forwarder. Returns the
/// resource handle private storage (caller owns it).
unsafe fn make_tex2d(
    h: Hdevice,
    priv_: &mut u64,
    usage: u32,
    bind: u32,
) -> ddi::D3D10DDI_HRESOURCE {
    let mip = ddi::D3D10DDI_MIPINFO {
        TexelWidth: 64,
        TexelHeight: 64,
        TexelDepth: 1,
        PhysicalWidth: 64,
        PhysicalHeight: 64,
        PhysicalDepth: 1,
    };
    let mut desc = ddi::D3D11DDIARG_CREATERESOURCE::default();
    desc.pMipInfoList = &mip;
    desc.ResourceDimension = RES_TEX2D;
    desc.Format = 87; // BGRA
    desc.Usage = usage;
    desc.BindFlags = bind;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.SampleDesc.Count = 1;
    let hr = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: priv_ as *mut u64 as *mut c_void,
    };
    create_resource(h, &desc, hr, Default::default());
    hr
}

/// Draw a `SV_VertexID` triangle (no vertex buffer / input layout) into an
/// offscreen RT and read back the center pixel. Returns 0 on PASS.
pub unsafe fn selftest_triangle(h: Hdevice) -> i32 {
    let vs_src = b"float4 VS(uint id:SV_VertexID):SV_Position{float2 uv=float2((id<<1)&2,id&2);return float4(uv*2-1,0,1);}\0";
    let ps_src = b"float4 PS():SV_Target{return float4(1,0,0,1);}\0"; // red
    let Some(vsb) = compile_hlsl(vs_src, b"VS\0", b"vs_5_0\0") else {
        return 10;
    };
    let Some(psb) = compile_hlsl(ps_src, b"PS\0", b"ps_5_0\0") else {
        return 11;
    };

    let mut rt_priv = 0u64;
    let h_rt = make_tex2d(h, &mut rt_priv, 0, D3D11_BIND_RENDER_TARGET.0 as u32);
    if rt_priv == 0 {
        return 12;
    }
    let mut rtv_desc = ddi::D3D10DDIARG_CREATERENDERTARGETVIEW::default();
    rtv_desc.hDrvResource = h_rt;
    rtv_desc.Format = 87;
    rtv_desc.ResourceDimension = RES_TEX2D;
    let mut rtv_priv = 0u64;
    let h_rtv = ddi::D3D10DDI_HRENDERTARGETVIEW {
        pDrvPrivate: &mut rtv_priv as *mut u64 as *mut c_void,
    };
    create_rtv(h, &rtv_desc, h_rtv, Default::default());
    if rtv_priv == 0 {
        return 13;
    }

    let mut vs_priv = 0u64;
    let h_vs = ddi::D3D10DDI_HSHADER {
        pDrvPrivate: &mut vs_priv as *mut u64 as *mut c_void,
    };
    create_vertex_shader(
        h,
        vsb.GetBufferPointer() as *const u32,
        h_vs,
        Default::default(),
        core::ptr::null(),
    );
    let mut ps_priv = 0u64;
    let h_ps = ddi::D3D10DDI_HSHADER {
        pDrvPrivate: &mut ps_priv as *mut u64 as *mut c_void,
    };
    create_pixel_shader(
        h,
        psb.GetBufferPointer() as *const u32,
        h_ps,
        Default::default(),
        core::ptr::null(),
    );
    if vs_priv == 0 || ps_priv == 0 {
        log_line("selftest_triangle: shader create failed");
        return 14;
    }
    vs_set_shader(h, h_vs);
    ps_set_shader(h, h_ps);

    // CULL_NONE rasterizer so the (back-facing) triangle isn't culled — also
    // validates create_rasterizer_state + set_rasterizer_state.
    let mut rs_desc = ddi::D3D10_DDI_RASTERIZER_DESC::default();
    rs_desc.FillMode = 3; // SOLID
    rs_desc.CullMode = 1; // NONE
    rs_desc.DepthClipEnable = 1;
    let mut rs_priv = 0u64;
    let h_rs = ddi::D3D10DDI_HRASTERIZERSTATE {
        pDrvPrivate: &mut rs_priv as *mut u64 as *mut c_void,
    };
    create_rasterizer_state(h, &rs_desc, h_rs, Default::default());
    set_rasterizer_state(h, h_rs);

    let mut black = [0.0f32, 0.0, 0.0, 1.0];
    clear_rtv(h, h_rtv, black.as_mut_ptr());
    set_render_targets(
        h,
        &h_rtv,
        1,
        0,
        Default::default(),
        core::ptr::null(),
        core::ptr::null(),
        0,
        0,
        0,
        0,
    );
    let vp = ddi::D3D10_DDI_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: 64.0,
        Height: 64.0,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    };
    set_viewports(h, 1, 0, &vp);
    ia_set_topology(h, 4 /* TRIANGLELIST */);
    draw(h, 3, 0);
    flush(h);

    // Read back the center pixel.
    let mut st_priv = 0u64;
    let h_st = make_tex2d(h, &mut st_priv, 3 /* STAGING */, 0);
    if st_priv == 0 {
        return 15;
    }
    resource_copy(h, h_st, h_rt);
    flush(h);
    let mut mapped = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
    resource_map(h, h_st, 0, 1, 0, &mut mapped);
    let mut result = 16;
    if !mapped.pData.is_null() {
        let off = 32 * mapped.RowPitch as usize + 32 * 4;
        let px = (mapped.pData as *const u8).add(off);
        let (b, g, r, a) = (*px, *px.add(1), *px.add(2), *px.add(3));
        log_line(&format!(
            "selftest_triangle: center BGRA = {b} {g} {r} {a} (want red ~0 0 255 255)"
        ));
        result = if r > 250 && g < 5 && b < 5 && a == 255 {
            0
        } else {
            17
        };
        resource_unmap(h, h_st, 0);
    }
    destroy_resource(h, h_st);
    destroy_shader(h, h_ps);
    destroy_shader(h, h_vs);
    destroy_rtv(h, h_rtv);
    destroy_resource(h, h_rt);
    log_line(&format!("selftest_triangle: result={result} (0=PASS)"));
    result
}

/// Diagnostic: create an immutable constant buffer with known content, copy it
/// to a staging buffer, and read it back. Tells us whether buffer-content upload
/// works (red back) or not (zero back) — independent of shader binding.
pub unsafe fn selftest_cb_readback(h: Hdevice) -> i32 {
    let red: [f32; 4] = [1.0, 0.25, 0.5, 1.0];
    let mip = ddi::D3D10DDI_MIPINFO {
        TexelWidth: 16,
        ..Default::default()
    };
    let init = ddi::D3D10_DDIARG_SUBRESOURCE_UP {
        pSysMem: red.as_ptr() as *mut c_void,
        SysMemPitch: 16,
        SysMemSlicePitch: 16,
    };
    let mut cb = ddi::D3D11DDIARG_CREATERESOURCE::default();
    cb.pMipInfoList = &mip;
    cb.pInitialDataUP = &init;
    cb.ResourceDimension = RES_BUFFER;
    cb.Usage = 1; // IMMUTABLE
    cb.BindFlags = D3D11_BIND_CONSTANT_BUFFER.0 as u32;
    cb.MipLevels = 1;
    cb.ArraySize = 1;
    let mut cb_priv = 0u64;
    let h_cb = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut cb_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &cb, h_cb, Default::default());

    let mut st = ddi::D3D11DDIARG_CREATERESOURCE::default();
    st.pMipInfoList = &mip;
    st.ResourceDimension = RES_BUFFER;
    st.Usage = 3; // STAGING
    st.BindFlags = 0;
    st.MipLevels = 1;
    st.ArraySize = 1;
    let mut st_priv = 0u64;
    let h_st = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut st_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &st, h_st, Default::default());
    if cb_priv == 0 || st_priv == 0 {
        log_line(&format!(
            "cb_readback: create failed cb={cb_priv:#x} st={st_priv:#x}"
        ));
        return 1;
    }
    resource_copy(h, h_st, h_cb);
    flush(h);
    let mut mapped = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
    resource_map(h, h_st, 0, 1, 0, &mut mapped);
    if mapped.pData.is_null() {
        log_line("cb_readback: map failed");
        return 2;
    }
    let f = mapped.pData as *const f32;
    log_line(&format!(
        "cb_readback: floats = {} {} {} {} (want 1 0.25 0.5 1)",
        *f,
        *f.add(1),
        *f.add(2),
        *f.add(3)
    ));
    resource_unmap(h, h_st, 0);
    destroy_resource(h, h_st);
    destroy_resource(h, h_cb);
    0
}

/// Diagnostic: full draw of a triangle whose PS reads a constant buffer, with
/// probes to localize the "CB content never reaches the shader" bug. Returns 0
/// when the center pixel is the CB-supplied green.
///
/// Probes, in order:
///  1. CB created with immutable green initial-data (proven-good content path).
///  2. After `ps_set_constant_buffers`, call `PSGetConstantBuffers` and log
///     whether DXVK reports our buffer bound — splits "our bind didn't register"
///     from "bound but shader reads garbage".
///  3. After the draw, copy the CB to staging and read it back — confirms the
///     resource still holds green at draw time (rules out it being clobbered).
pub unsafe fn selftest_triangle_cb(h: Hdevice) -> i32 {
    let vs_src = b"float4 VS(uint id:SV_VertexID):SV_Position{float2 uv=float2((id<<1)&2,id&2);return float4(uv*2-1,0,1);}\0";
    // PS reads col from the constant buffer at b0.
    let ps_src = b"cbuffer C:register(b0){float4 col;} float4 PS():SV_Target{return col;}\0";
    let Some(vsb) = compile_hlsl(vs_src, b"VS\0", b"vs_5_0\0") else {
        return 30;
    };
    let Some(psb) = compile_hlsl(ps_src, b"PS\0", b"ps_5_0\0") else {
        return 31;
    };

    let mut rt_priv = 0u64;
    let h_rt = make_tex2d(h, &mut rt_priv, 0, D3D11_BIND_RENDER_TARGET.0 as u32);
    if rt_priv == 0 {
        return 32;
    }
    let mut rtv_desc = ddi::D3D10DDIARG_CREATERENDERTARGETVIEW::default();
    rtv_desc.hDrvResource = h_rt;
    rtv_desc.Format = 87;
    rtv_desc.ResourceDimension = RES_TEX2D;
    let mut rtv_priv = 0u64;
    let h_rtv = ddi::D3D10DDI_HRENDERTARGETVIEW {
        pDrvPrivate: &mut rtv_priv as *mut u64 as *mut c_void,
    };
    create_rtv(h, &rtv_desc, h_rtv, Default::default());
    if rtv_priv == 0 {
        return 33;
    }

    // Constant buffer: 16 bytes, immutable, content = green (0,1,0,1).
    let green: [f32; 4] = [0.0, 1.0, 0.0, 1.0];
    let mip = ddi::D3D10DDI_MIPINFO {
        TexelWidth: 16,
        ..Default::default()
    };
    let init = ddi::D3D10_DDIARG_SUBRESOURCE_UP {
        pSysMem: green.as_ptr() as *mut c_void,
        SysMemPitch: 16,
        SysMemSlicePitch: 16,
    };
    let mut cb = ddi::D3D11DDIARG_CREATERESOURCE::default();
    cb.pMipInfoList = &mip;
    cb.pInitialDataUP = &init;
    cb.ResourceDimension = RES_BUFFER;
    cb.Usage = 1; // IMMUTABLE
    cb.BindFlags = D3D11_BIND_CONSTANT_BUFFER.0 as u32;
    cb.MipLevels = 1;
    cb.ArraySize = 1;
    let mut cb_priv = 0u64;
    let h_cb = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut cb_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &cb, h_cb, Default::default());
    if cb_priv == 0 {
        log_line("selftest_triangle_cb: CB create failed");
        return 34;
    }
    log_line(&format!("selftest_triangle_cb: CB COM ptr = {cb_priv:#x}"));

    // Shaders.
    let mut vs_priv = 0u64;
    let h_vs = ddi::D3D10DDI_HSHADER {
        pDrvPrivate: &mut vs_priv as *mut u64 as *mut c_void,
    };
    create_vertex_shader(
        h,
        vsb.GetBufferPointer() as *const u32,
        h_vs,
        Default::default(),
        core::ptr::null(),
    );
    let mut ps_priv = 0u64;
    let h_ps = ddi::D3D10DDI_HSHADER {
        pDrvPrivate: &mut ps_priv as *mut u64 as *mut c_void,
    };
    create_pixel_shader(
        h,
        psb.GetBufferPointer() as *const u32,
        h_ps,
        Default::default(),
        core::ptr::null(),
    );
    if vs_priv == 0 || ps_priv == 0 {
        return 35;
    }
    vs_set_shader(h, h_vs);
    ps_set_shader(h, h_ps);

    // CULL_NONE so the triangle isn't culled.
    let mut rs_desc = ddi::D3D10_DDI_RASTERIZER_DESC::default();
    rs_desc.FillMode = 3;
    rs_desc.CullMode = 1;
    rs_desc.DepthClipEnable = 1;
    let mut rs_priv = 0u64;
    let h_rs = ddi::D3D10DDI_HRASTERIZERSTATE {
        pDrvPrivate: &mut rs_priv as *mut u64 as *mut c_void,
    };
    create_rasterizer_state(h, &rs_desc, h_rs, Default::default());
    set_rasterizer_state(h, h_rs);

    // Bind the constant buffer to PS slot b0.
    ps_set_constant_buffers(h, 0, 1, &h_cb);

    // PROBE 2: ask DXVK what it now has bound at PS b0.
    if let Some(c) = d3d11_context(h) {
        let mut bound: [Option<ID3D11Buffer>; 1] = [None];
        c.PSGetConstantBuffers(0, Some(&mut bound));
        match &bound[0] {
            Some(b) => {
                let raw = b.as_raw() as usize;
                log_line(&format!(
                    "selftest_triangle_cb: PSGetConstantBuffers[0] = {raw:#x} (CB was {cb_priv:#x}, match={})",
                    raw == cb_priv as usize
                ));
            }
            None => log_line(
                "selftest_triangle_cb: PSGetConstantBuffers[0] = NULL (bind did NOT register!)",
            ),
        }
    }

    let mut black = [0.0f32, 0.0, 0.0, 1.0];
    clear_rtv(h, h_rtv, black.as_mut_ptr());
    set_render_targets(
        h,
        &h_rtv,
        1,
        0,
        Default::default(),
        core::ptr::null(),
        core::ptr::null(),
        0,
        0,
        0,
        0,
    );
    let vp = ddi::D3D10_DDI_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: 64.0,
        Height: 64.0,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    };
    set_viewports(h, 1, 0, &vp);
    ia_set_topology(h, 4 /* TRIANGLELIST */);
    draw(h, 3, 0);
    flush(h);

    // PROBE 3: CB content still green at draw time?
    let mut cbst = ddi::D3D11DDIARG_CREATERESOURCE::default();
    cbst.pMipInfoList = &mip;
    cbst.ResourceDimension = RES_BUFFER;
    cbst.Usage = 3; // STAGING
    cbst.MipLevels = 1;
    cbst.ArraySize = 1;
    let mut cbst_priv = 0u64;
    let h_cbst = ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: &mut cbst_priv as *mut u64 as *mut c_void,
    };
    create_resource(h, &cbst, h_cbst, Default::default());
    if cbst_priv != 0 {
        resource_copy(h, h_cbst, h_cb);
        flush(h);
        let mut m = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
        resource_map(h, h_cbst, 0, 1, 0, &mut m);
        if !m.pData.is_null() {
            let f = m.pData as *const f32;
            log_line(&format!(
                "selftest_triangle_cb: CB content at draw time = {} {} {} {} (want 0 1 0 1)",
                *f,
                *f.add(1),
                *f.add(2),
                *f.add(3)
            ));
            resource_unmap(h, h_cbst, 0);
        }
        destroy_resource(h, h_cbst);
    }

    // Read back the center pixel.
    let mut st_priv = 0u64;
    let h_st = make_tex2d(h, &mut st_priv, 3, 0);
    if st_priv == 0 {
        return 36;
    }
    resource_copy(h, h_st, h_rt);
    flush(h);
    let mut mapped = ddi::D3D10DDI_MAPPED_SUBRESOURCE::default();
    resource_map(h, h_st, 0, 1, 0, &mut mapped);
    let mut result = 37;
    if !mapped.pData.is_null() {
        let off = 32 * mapped.RowPitch as usize + 32 * 4;
        let px = (mapped.pData as *const u8).add(off);
        let (b, g, r, a) = (*px, *px.add(1), *px.add(2), *px.add(3));
        log_line(&format!(
            "selftest_triangle_cb: center BGRA = {b} {g} {r} {a} (want green ~0 255 0 255)"
        ));
        result = if g > 250 && r < 5 && b < 5 && a == 255 {
            0
        } else {
            38
        };
        resource_unmap(h, h_st, 0);
    }
    destroy_resource(h, h_st);
    destroy_shader(h, h_ps);
    destroy_shader(h, h_vs);
    destroy_resource(h, h_cb);
    destroy_rtv(h, h_rtv);
    destroy_resource(h, h_rt);
    log_line(&format!("selftest_triangle_cb: result={result} (0=PASS)"));
    result
}

// --- Input layouts (lazy, via the VS input signature) -----------------------

/// One d3d10umddi input element, kept until the bound VS is known (the DDI gives
/// us the VS input *register*, not a semantic name, so we resolve names lazily).
struct DdiInputElement {
    input_slot: u32,
    aligned_byte_offset: u32,
    format: i32,
    input_slot_class: u32,
    instance_step_rate: u32,
    input_register: u32,
}

/// Element-layout data, Box'd and stashed in the CreateElementLayout handle.
struct LayoutData {
    elements: Vec<DdiInputElement>,
}

/// Parse a vertex shader's DXBC `ISGN` (input signature) chunk and return the
/// (semantic name, semantic index) of the element bound to input `register`.
/// `ID3D11Device::CreateInputLayout` needs semantic names, but the DDI only
/// passes the register index, so we recover the names from the shader bytecode.
unsafe fn isgn_lookup(dxbc: &[u8], register: u32) -> Option<(std::ffi::CString, u32)> {
    if dxbc.len() < 32 || &dxbc[0..4] != b"DXBC" {
        return None;
    }
    let chunk_count = u32::from_le_bytes(dxbc[28..32].try_into().ok()?) as usize;
    for i in 0..chunk_count {
        let off_pos = 32 + i * 4;
        if off_pos + 4 > dxbc.len() {
            return None;
        }
        let coff = u32::from_le_bytes(dxbc[off_pos..off_pos + 4].try_into().ok()?) as usize;
        if coff + 8 > dxbc.len() || &dxbc[coff..coff + 4] != b"ISGN" {
            continue;
        }
        let data = coff + 8; // skip FourCC + chunk size
        if data + 8 > dxbc.len() {
            return None;
        }
        let elem_count = u32::from_le_bytes(dxbc[data..data + 4].try_into().ok()?) as usize;
        for e in 0..elem_count {
            let ep = data + 8 + e * 24;
            if ep + 24 > dxbc.len() {
                return None;
            }
            let name_off = u32::from_le_bytes(dxbc[ep..ep + 4].try_into().ok()?) as usize;
            let sem_index = u32::from_le_bytes(dxbc[ep + 4..ep + 8].try_into().ok()?);
            let reg = u32::from_le_bytes(dxbc[ep + 16..ep + 20].try_into().ok()?);
            if reg == register {
                let nstart = data + name_off;
                let mut nend = nstart;
                while nend < dxbc.len() && dxbc[nend] != 0 {
                    nend += 1;
                }
                let name = std::ffi::CString::new(&dxbc[nstart..nend]).ok()?;
                return Some((name, sem_index));
            }
        }
        return None;
    }
    None
}

/// Build a minimal DXBC container with a synthetic `ISGN` chunk for the given
/// input registers, followed by the raw SM4/SM5 token stream (SHDR/SHEX).
///
/// The DDI hands shaders to the driver as raw token streams with no DXBC
/// container, so there are no semantic names to recover for
/// `ID3D11Device::CreateInputLayout`. Names are only a matching key between
/// the layout descs and the shader signature, and DXVK resolves an element's
/// vertex-input LOCATION from the matched signature entry's register — which
/// is also how dxbc-spv assigns locations (`dcl_input v[r]` → location r) when
/// it compiles the container-less shader. So a fabricated "TEXCOORD<r>" per
/// register keeps both sides consistent.
fn build_layout_signature_blob(registers: &[u32], tokens: &[u8]) -> Vec<u8> {
    const NAME: &[u8] = b"TEXCOORD\0";
    let entry_count = registers.len();
    let entries_size = entry_count * 24;
    let name_off = 8 + entries_size; // offsets are relative to chunk-data start
    let isgn_len_unpadded = name_off + NAME.len();
    let isgn_len = (isgn_len_unpadded + 3) & !3;

    // Code chunk tag from the version token (major >= 5 uses SHEX).
    let version_token = if tokens.len() >= 4 {
        u32::from_le_bytes(tokens[0..4].try_into().unwrap())
    } else {
        0
    };
    let code_tag: &[u8; 4] = if ((version_token >> 4) & 0xF) >= 5 {
        b"SHEX"
    } else {
        b"SHDR"
    };

    // DXBC header (32) + 2 chunk offsets (8).
    let isgn_chunk_off = 32 + 8;
    let code_chunk_off = isgn_chunk_off + 8 + isgn_len;
    let total = code_chunk_off + 8 + tokens.len();

    let mut blob = vec![0u8; total];
    blob[0..4].copy_from_slice(b"DXBC");
    // bytes 4..20: checksum left zero (DXVK does not verify it)
    blob[20..24].copy_from_slice(&1u32.to_le_bytes());
    blob[24..28].copy_from_slice(&(total as u32).to_le_bytes());
    blob[28..32].copy_from_slice(&2u32.to_le_bytes());
    blob[32..36].copy_from_slice(&(isgn_chunk_off as u32).to_le_bytes());
    blob[36..40].copy_from_slice(&(code_chunk_off as u32).to_le_bytes());

    blob[isgn_chunk_off..isgn_chunk_off + 4].copy_from_slice(b"ISGN");
    blob[isgn_chunk_off + 4..isgn_chunk_off + 8]
        .copy_from_slice(&(isgn_len as u32).to_le_bytes());
    let data = isgn_chunk_off + 8;
    blob[data..data + 4].copy_from_slice(&(entry_count as u32).to_le_bytes());
    blob[data + 4..data + 8].copy_from_slice(&8u32.to_le_bytes());
    for (i, reg) in registers.iter().enumerate() {
        let ep = data + 8 + i * 24;
        blob[ep..ep + 4].copy_from_slice(&(name_off as u32).to_le_bytes());
        blob[ep + 4..ep + 8].copy_from_slice(&reg.to_le_bytes()); // semantic index = register
        blob[ep + 8..ep + 12].copy_from_slice(&0u32.to_le_bytes()); // no system value
        blob[ep + 12..ep + 16].copy_from_slice(&3u32.to_le_bytes()); // float32
        blob[ep + 16..ep + 20].copy_from_slice(&reg.to_le_bytes());
        blob[ep + 20] = 0x0F; // mask
        blob[ep + 21] = 0x0F; // read/write mask
    }
    blob[data + name_off..data + name_off + NAME.len()].copy_from_slice(NAME);

    blob[code_chunk_off..code_chunk_off + 4].copy_from_slice(code_tag);
    blob[code_chunk_off + 4..code_chunk_off + 8]
        .copy_from_slice(&(tokens.len() as u32).to_le_bytes());
    blob[code_chunk_off + 8..code_chunk_off + 8 + tokens.len()].copy_from_slice(tokens);
    blob
}

unsafe extern "C" fn calc_size_element_layout(
    _h: Hdevice,
    _a: *const ddi::D3D10DDIARG_CREATEELEMENTLAYOUT,
) -> u64 {
    8
}

unsafe extern "C" fn create_element_layout(
    _h: Hdevice,
    arg: *const ddi::D3D10DDIARG_CREATEELEMENTLAYOUT,
    h_el: ddi::D3D10DDI_HELEMENTLAYOUT,
    _hrt: ddi::D3D10DDI_HRTELEMENTLAYOUT,
) {
    clear_handle(h_el.pDrvPrivate);
    let a = &*arg;
    let mut elems = Vec::with_capacity(a.NumElements as usize);
    for i in 0..a.NumElements as usize {
        let e = &*a.pVertexElements.add(i);
        elems.push(DdiInputElement {
            input_slot: e.InputSlot,
            aligned_byte_offset: e.AlignedByteOffset,
            format: e.Format as i32,
            input_slot_class: e.InputSlotClass as u32,
            instance_step_rate: e.InstanceDataStepRate,
            input_register: e.InputRegister,
        });
    }
    let boxed = Box::new(LayoutData { elements: elems });
    *(h_el.pDrvPrivate as *mut usize) = Box::into_raw(boxed) as usize;
}

unsafe extern "C" fn destroy_element_layout(_h: Hdevice, h_el: ddi::D3D10DDI_HELEMENTLAYOUT) {
    if h_el.pDrvPrivate.is_null() {
        return;
    }
    let p = *(h_el.pDrvPrivate as *const usize);
    if p != 0 {
        drop(Box::from_raw(p as *mut LayoutData));
        *(h_el.pDrvPrivate as *mut usize) = 0;
    }
}

unsafe extern "C" fn ia_set_input_layout(h: Hdevice, h_el: ddi::D3D10DDI_HELEMENTLAYOUT) {
    if let Some(dev) = helios_device(h) {
        let p = if h_el.pDrvPrivate.is_null() {
            0
        } else {
            *(h_el.pDrvPrivate as *const usize)
        };
        dev.ia.borrow_mut().current_layout = p;
    }
}

/// Lazily create + bind the `ID3D11InputLayout` for the current (element layout,
/// VS) pair, resolving element semantic names from the VS input signature.
unsafe fn bind_input_layout(h: Hdevice) {
    let Some(dev) = helios_device(h) else {
        return;
    };
    let (lp, vp) = {
        let ia = dev.ia.borrow();
        (ia.current_layout, ia.current_vs)
    };
    if lp == 0 || vp == 0 {
        let n = SHADER_BIND_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
        if n < 256 {
            log_line(&format!(
                "DDI bind_input_layout skipped: layout=0x{:x} vs=0x{:x}",
                lp, vp
            ));
        }
        return;
    }
    let cached = dev.ia.borrow().layout_cache.get(&(lp, vp)).copied();
    let il_raw = match cached {
        Some(p) => p,
        None => {
            let bytecode = match dev.ia.borrow().vs_bytecode.get(&vp) {
                Some(b) => b.clone(),
                None => {
                    log_line(&format!(
                        "DDI bind_input_layout skipped: missing VS bytecode layout=0x{:x} vs=0x{:x}",
                        lp, vp
                    ));
                    return;
                }
            };
            let layout = &*(lp as *const LayoutData);
            let is_dxbc = bytecode.len() >= 4 && &bytecode[0..4] == b"DXBC";
            // Reserve so the CString store never reallocates (the descs below
            // borrow raw pointers into it for the CreateInputLayout call).
            let mut names: Vec<std::ffi::CString> = Vec::with_capacity(layout.elements.len());
            let mut descs: Vec<D3D11_INPUT_ELEMENT_DESC> =
                Vec::with_capacity(layout.elements.len());
            let mut registers: Vec<u32> = Vec::with_capacity(layout.elements.len());
            for el in &layout.elements {
                let (name, sem_index) = if is_dxbc {
                    // Real container: recover the shader's own semantic names.
                    match isgn_lookup(&bytecode, el.input_register) {
                        Some(v) => v,
                        None => {
                            log_line(&format!(
                                "DDI bind_input_layout: no ISGN entry for input_register={} fmt={} slot={} offset={}",
                                el.input_register, el.format, el.input_slot, el.aligned_byte_offset
                            ));
                            continue;
                        }
                    }
                } else {
                    // Raw DDI token stream: no ISGN exists. Fabricate
                    // TEXCOORD<register> and pair it with a synthetic ISGN in
                    // the blob below so name-matching resolves to the register.
                    (
                        std::ffi::CString::new("TEXCOORD").unwrap(),
                        el.input_register,
                    )
                };
                names.push(name);
                let name_ptr = names.last().unwrap().as_ptr() as *const u8;
                if !registers.contains(&el.input_register) {
                    registers.push(el.input_register);
                }
                descs.push(D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: PCSTR(name_ptr),
                    SemanticIndex: sem_index,
                    Format: DXGI_FORMAT(el.format),
                    InputSlot: el.input_slot,
                    AlignedByteOffset: el.aligned_byte_offset,
                    InputSlotClass: D3D11_INPUT_CLASSIFICATION(el.input_slot_class as i32),
                    InstanceDataStepRate: el.instance_step_rate,
                });
            }
            if descs.is_empty() {
                log_line(&format!(
                    "DDI bind_input_layout skipped: empty descs elements={} vs_len={}",
                    layout.elements.len(),
                    bytecode.len()
                ));
                return;
            }
            let signature_blob;
            let blob_for_layout: &[u8] = if is_dxbc {
                &bytecode
            } else {
                signature_blob = build_layout_signature_blob(&registers, &bytecode);
                &signature_blob
            };
            let Some(device) = d3d11_device(h) else {
                return;
            };
            let mut il: Option<ID3D11InputLayout> = None;
            match device.CreateInputLayout(&descs, blob_for_layout, Some(&mut il)) {
                Ok(()) => match il {
                    Some(l) => {
                        let raw = l.into_raw() as usize;
                        log_line(&format!(
                            "DDI CreateInputLayout ok: layout=0x{:x} vs=0x{:x} elems={} raw=0x{:x}",
                            lp,
                            vp,
                            descs.len(),
                            raw
                        ));
                        dev.ia.borrow_mut().layout_cache.insert((lp, vp), raw);
                        raw
                    }
                    None => return,
                },
                Err(e) => {
                    log_line(&format!("CreateInputLayout failed: {e:?}"));
                    return;
                }
            }
        }
    };
    if let Some(context) = d3d11_context(h) {
        let il = ManuallyDrop::new(ID3D11InputLayout::from_raw(il_raw as *mut c_void));
        context.IASetInputLayout(&*il);
    }

    // VUID-Input-08733: the DDI never provides shader-input component types
    // (RegisterComponentType arrives 0/UNKNOWN — verified against both dwm's
    // SM4 shaders and the SM5 draw probe), so compiled VS inputs default to
    // float32 while layouts may bind SINT/UINT vertex formats: vertex-fetch
    // UB (dwm binds R16G16_SINT TEXCOORDs; the garbage is the prime Xid-109
    // suspect). The INPUT LAYOUT is the ground truth for the numeric class —
    // any (layout, VS) pair the runtime allows to bind matched the app's
    // original input signature — so bind a variant recompiled with the
    // layout's classes whenever any attribute is non-float.
    resolve_vs_input_variant(h, lp, vp);
}

/// Numeric class of a DXGI vertex format for Vulkan's vertex-input contract,
/// as a DXBC ISGN component type: 1 = UINT, 2 = SINT, 3 = FLOAT (covers
/// FLOAT/UNORM/SNORM — all float-class at the fetch).
fn dxgi_vertex_class(format: i32) -> u32 {
    match format {
        // *_UINT: R32G32B32A32, R32G32B32, R16G16B16A16, R32G32, R10G10B10A2,
        // R8G8B8A8, R16G16, R32, R8G8, R16, R8
        3 | 7 | 12 | 17 | 25 | 30 | 36 | 42 | 50 | 57 | 62 => 1,
        // *_SINT: same families
        4 | 8 | 14 | 18 | 32 | 38 | 43 | 52 | 59 | 64 => 2,
        _ => 3,
    }
}

/// Component mask of a DXGI vertex format (for synthesized ISGN entries).
fn dxgi_vertex_mask(format: i32) -> u32 {
    match format {
        1..=4 | 9..=14 | 19..=32 => 0xf,          // 4-component families
        5..=8 => 0x7,                             // R32G32B32
        15..=18 | 33..=38 | 48..=52 => 0x3,       // 2-component families
        _ => 0x1,                                 // scalars and the rest
    }
}

/// Pick (and lazily compile) the vertex-shader variant whose declared input
/// component types match the bound layout's format classes, then bind it.
/// All-float layouts (the overwhelmingly common case) bind the original.
unsafe fn resolve_vs_input_variant(h: Hdevice, lp: usize, vp: usize) {
    let Some(dev) = helios_device(h) else {
        return;
    };
    let layout = &*(lp as *const LayoutData);
    // (register, class, mask) per input register, merging multi-element
    // registers (matrix-style attributes span elements, same class).
    let mut classes: Vec<(u32, u32, u32)> = Vec::new();
    let mut any_nonfloat = false;
    for el in &layout.elements {
        let class = dxgi_vertex_class(el.format);
        let mask = dxgi_vertex_mask(el.format);
        if class != 3 {
            any_nonfloat = true;
        }
        if let Some(entry) = classes.iter_mut().find(|c| c.0 == el.input_register) {
            entry.1 = entry.1.max(class);
            entry.2 |= mask;
        } else {
            classes.push((el.input_register, class, mask));
        }
    }

    let desired = if !any_nonfloat {
        vp
    } else {
        // FNV-1a over (register, class) pairs = the variant cache key.
        let mut key: u64 = 0xcbf2_9ce4_8422_2325;
        for &(r, c, _) in &classes {
            key = (key ^ (((r as u64) << 8) | c as u64)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        let cached = dev.ia.borrow().vs_variants.get(&(vp, key)).copied();
        let variant = match cached {
            Some(v) => v,
            None => {
                let v = create_vs_input_variant(dev, vp, &classes);
                dev.ia.borrow_mut().vs_variants.insert((vp, key), v);
                v
            }
        };
        if variant != 0 { variant } else { vp }
    };

    if dev.ia.borrow().bound_vs_com == desired {
        return;
    }
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let s = ManuallyDrop::new(ID3D11VertexShader::from_raw(desired as *mut c_void));
    context.VSSetShader(&*s, None);
    dev.ia.borrow_mut().bound_vs_com = desired;
    let n = SHADER_BIND_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 256 {
        trace_line!(
            "DDI VS input-class variant bound: vs=0x{vp:x} -> 0x{desired:x}"
        );
    }
}

/// Recompile a vertex shader with its synthesized ISGN component types taken
/// from the bound input layout. Returns 0 on failure (caller falls back to
/// the original shader — no worse than the pre-variant behaviour).
unsafe fn create_vs_input_variant(
    dev: &HeliosDevice,
    vp: usize,
    classes: &[(u32, u32, u32)],
) -> usize {
    let (bytecode, mut words) = {
        let ia = dev.ia.borrow();
        let Some(b) = ia.vs_bytecode.get(&vp) else {
            log_line(&format!("VS variant: no bytecode for vs=0x{vp:x}"));
            return 0;
        };
        if b.len() >= 4 && &b[0..4] == b"DXBC" {
            // Real container: its own ISGN already carries real types.
            return 0;
        }
        let w = ia
            .vs_sig_words
            .get(&vp)
            .cloned()
            .unwrap_or_else(|| vec![0u32, 0u32]);
        (b.clone(), w)
    };

    let n_in = words[0] as usize;
    if n_in > 0 {
        // Patch the DDI-provided entries' component types from the layout.
        for i in 0..n_in {
            let base = 2 + i * 5;
            let reg = words[base + 1];
            if let Some(&(_, class, _)) = classes.iter().find(|c| c.0 == reg) {
                words[base + 3] = class;
            } else if words[base + 3] == 0 {
                words[base + 3] = 3;
            }
        }
    } else {
        // Shader arrived through the legacy untyped create: synthesize the
        // input entries wholesale from the layout (extra entries for unused
        // registers are declared-then-eliminated by the compiler).
        let n_out = words[1];
        let out_words = words.split_off(2);
        words = vec![classes.len() as u32, n_out];
        for &(reg, class, mask) in classes {
            words.extend_from_slice(&[0, reg, mask, class, 0]);
        }
        words.extend_from_slice(&out_words);
    }

    let Some(dxvk) = dev.dxvk.as_ref() else {
        return 0;
    };
    let raw = dxvk.create_shader_sig(
        0,
        bytecode.as_ptr(),
        bytecode.len(),
        words.as_ptr(),
        words.len(),
    );
    log_line(&format!(
        "VS input-class variant: vs=0x{vp:x} classes={:?} -> raw=0x{raw:x}",
        classes.iter().map(|c| (c.0, c.1)).collect::<Vec<_>>()
    ));
    raw
}

unsafe extern "C" fn ia_set_vertex_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    buffers: *const ddi::D3D10DDI_HRESOURCE,
    strides: *const u32,
    offsets: *const u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let mut bufs: Vec<Option<ID3D11Buffer>> = Vec::with_capacity(num as usize);
    for i in 0..num as usize {
        let p = (*buffers.add(i)).pDrvPrivate;
        bufs.push(load_resource(p).and_then(|r| (*r).cast::<ID3D11Buffer>().ok()));
    }
    context.IASetVertexBuffers(
        start,
        num,
        Some(bufs.as_ptr()),
        Some(strides),
        Some(offsets),
    );
}

unsafe extern "C" fn ia_set_index_buffer(
    h: Hdevice,
    h_buf: ddi::D3D10DDI_HRESOURCE,
    format: ddi::DXGI_FORMAT,
    offset: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let buf = load_resource(h_buf.pDrvPrivate).and_then(|r| (*r).cast::<ID3D11Buffer>().ok());
    context.IASetIndexBuffer(buf.as_ref(), DXGI_FORMAT(format as i32), offset);
}

// --- Blend state ------------------------------------------------------------

unsafe extern "C" fn calc_size_blend(_h: Hdevice, _d: *const ddi::D3D10_1_DDI_BLEND_DESC) -> u64 {
    8
}

unsafe extern "C" fn create_blend_state(
    h: Hdevice,
    desc: *const ddi::D3D10_1_DDI_BLEND_DESC,
    h_bs: ddi::D3D10DDI_HBLENDSTATE,
    _hrt: ddi::D3D10DDI_HRTBLENDSTATE,
) {
    clear_handle(h_bs.pDrvPrivate);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let d = &*desc;
    let mut rt: [D3D11_RENDER_TARGET_BLEND_DESC; 8] = Default::default();
    for i in 0..8 {
        let s = &d.RenderTarget[i];
        rt[i] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: windows::Win32::Foundation::BOOL(s.BlendEnable),
            SrcBlend: D3D11_BLEND(s.SrcBlend),
            DestBlend: D3D11_BLEND(s.DestBlend),
            BlendOp: D3D11_BLEND_OP(s.BlendOp),
            SrcBlendAlpha: D3D11_BLEND(s.SrcBlendAlpha),
            DestBlendAlpha: D3D11_BLEND(s.DestBlendAlpha),
            BlendOpAlpha: D3D11_BLEND_OP(s.BlendOpAlpha),
            RenderTargetWriteMask: s.RenderTargetWriteMask,
        };
    }
    let bd = D3D11_BLEND_DESC {
        AlphaToCoverageEnable: windows::Win32::Foundation::BOOL(d.AlphaToCoverageEnable),
        IndependentBlendEnable: windows::Win32::Foundation::BOOL(d.IndependentBlendEnable),
        RenderTarget: rt,
    };
    let mut bs: Option<ID3D11BlendState> = None;
    if device.CreateBlendState(&bd, Some(&mut bs)).is_ok() {
        if let Some(s) = bs {
            store_com(h_bs.pDrvPrivate, s);
        }
    }
}

/// D3D11.1-interface `pfnCalcPrivateBlendStateSize` (same 8-byte COM-pointer
/// slot as `calc_size_blend`; only the desc type differs).
unsafe extern "C" fn calc_size_blend_11_1(
    _h: Hdevice,
    _d: *const ddi::D3D11_1_DDI_BLEND_DESC,
) -> u64 {
    8
}

/// D3D11.1-interface `pfnCreateBlendState`. Every device-funcs table from
/// D3D11.1 up passes `D3D11_1_DDI_BLEND_DESC`, whose per-RT struct INSERTS
/// `LogicOpEnable` after `BlendEnable` and `LogicOp` before
/// `RenderTargetWriteMask` — it is NOT prefix-compatible with the 10.1 desc.
/// Installing the 10.1 reader here made the mask field land on the 11.1
/// desc's `BlendOpAlpha` (default D3D11_BLEND_OP_ADD = 1), so every blend
/// state — including the runtime's defaults — carried RenderTargetWriteMask =
/// RED-only and every draw on the stack wrote just the R channel (the
/// black/red-tinted composition class; minimal repro
/// tools/d3d11_shared_draw_probe.cpp, 2026-07-03).
unsafe extern "C" fn create_blend_state_11_1(
    h: Hdevice,
    desc: *const ddi::D3D11_1_DDI_BLEND_DESC,
    h_bs: ddi::D3D10DDI_HBLENDSTATE,
    _hrt: ddi::D3D10DDI_HRTBLENDSTATE,
) {
    clear_handle(h_bs.pDrvPrivate);
    let Some(device) = d3d11_device(h) else {
        return;
    };
    let Ok(device1) = device.cast::<ID3D11Device1>() else {
        log_line("DDI create_blend_state_11_1: ID3D11Device1 cast failed");
        return;
    };
    let d = &*desc;
    let mut rt: [D3D11_RENDER_TARGET_BLEND_DESC1; 8] = Default::default();
    for i in 0..8 {
        let s = &d.RenderTarget[i];
        rt[i] = D3D11_RENDER_TARGET_BLEND_DESC1 {
            BlendEnable: windows::Win32::Foundation::BOOL(s.BlendEnable),
            LogicOpEnable: windows::Win32::Foundation::BOOL(s.LogicOpEnable),
            SrcBlend: D3D11_BLEND(s.SrcBlend),
            DestBlend: D3D11_BLEND(s.DestBlend),
            BlendOp: D3D11_BLEND_OP(s.BlendOp),
            SrcBlendAlpha: D3D11_BLEND(s.SrcBlendAlpha),
            DestBlendAlpha: D3D11_BLEND(s.DestBlendAlpha),
            BlendOpAlpha: D3D11_BLEND_OP(s.BlendOpAlpha),
            // The API logic-op enum mirrors the DDI enum value-for-value.
            LogicOp: D3D11_LOGIC_OP(s.LogicOp),
            RenderTargetWriteMask: s.RenderTargetWriteMask,
        };
    }
    let bd = D3D11_BLEND_DESC1 {
        AlphaToCoverageEnable: windows::Win32::Foundation::BOOL(d.AlphaToCoverageEnable),
        IndependentBlendEnable: windows::Win32::Foundation::BOOL(d.IndependentBlendEnable),
        RenderTarget: rt,
    };
    let mut bs: Option<ID3D11BlendState1> = None;
    if device1.CreateBlendState1(&bd, Some(&mut bs)).is_ok() {
        if let Some(s) = bs {
            // `set_blend_state` loads an ID3D11BlendState from this slot;
            // store the base interface.
            if let Ok(base) = s.cast::<ID3D11BlendState>() {
                store_com(h_bs.pDrvPrivate, base);
            }
        }
    } else {
        log_line("DDI create_blend_state_11_1: CreateBlendState1 failed");
    }
}

unsafe extern "C" fn set_blend_state(
    h: Hdevice,
    h_bs: ddi::D3D10DDI_HBLENDSTATE,
    factor: *const f32,
    sample_mask: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let f = if factor.is_null() {
        [1.0f32; 4]
    } else {
        [*factor, *factor.add(1), *factor.add(2), *factor.add(3)]
    };
    match load_com::<ID3D11BlendState>(h_bs.pDrvPrivate) {
        Some(s) => context.OMSetBlendState(&*s, Some(&f), sample_mask),
        None => context.OMSetBlendState(None, Some(&f), sample_mask),
    }
}

unsafe extern "C" fn destroy_blend_state(_h: Hdevice, h_bs: ddi::D3D10DDI_HBLENDSTATE) {
    release_com(h_bs.pDrvPrivate);
}

// --- DXGI present -----------------------------------------------------------

unsafe fn dxgi_device_handle(h: ddi::DXGI_DDI_HDEVICE) -> Hdevice {
    Hdevice {
        pDrvPrivate: h as *mut c_void,
    }
}

unsafe fn dxgi_resource_handle(h: ddi::DXGI_DDI_HRESOURCE) -> ddi::D3D10DDI_HRESOURCE {
    ddi::D3D10DDI_HRESOURCE {
        pDrvPrivate: h as *mut c_void,
    }
}

/// DXGI `pfnPresent`: copy the source resource to the destination resource when
/// DXGI provides both handles, then flush submitted GPU work.
unsafe extern "C" fn dxgi_present(arg: *mut ddi::DXGI_DDI_ARG_PRESENT) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    // DXGI_DDI_HDEVICE is a UINT_PTR carrying the driver device handle, the same
    // private pointer stored in D3D10DDI_HDEVICE.pDrvPrivate.
    let h = dxgi_device_handle(a.hDevice);
    let context = d3d11_context(h);
    let src_h = dxgi_resource_handle(a.hSurfaceToPresent);
    let dst_h = dxgi_resource_handle(a.hDstResource);
    let src_alloc = resource_allocation(src_h.pDrvPrivate);
    let dst_alloc = resource_allocation(dst_h.pDrvPrivate);
    let mut copied = false;
    let mut present_hr = 0;
    let mut sync_value: u64 = 0;

    if let Some(context) = &context {
        if let (Some(dst), Some(src)) = (
            load_resource(dst_h.pDrvPrivate),
            load_resource(src_h.pDrvPrivate),
        ) {
            context.CopySubresourceRegion(&*dst, 0, 0, 0, 0, &*src, 0, None);
            copied = true;
        }
        // WS1 #4 producer: record the named-present-fence signal BEFORE the
        // flush so it submits WITH the frame's last work, and publish
        // (resid -> pid, value) for the IddCx consumer's bounded wait.
        // `HKLM\SOFTWARE\Helios!PresentSyncPublish = 0` kills the path.
        if present_sync_publish_enabled() {
            if let Some(dev) = helios_device(h) {
                sync_value = dev.dxvk.present_sync_publish(
                    resource_com_raw(src_h.pDrvPrivate),
                    resource_com_raw(dst_h.pDrvPrivate),
                );
            }
        }
        context.Flush();
    }

    // Frame-completion gate BEFORE the kernel flip becomes visible: dwm's
    // venus rendering produces no dxgkrnl-visible DMA fences, so nothing else
    // orders the IddCx consumer's per-acquire copy against in-flight GPU
    // writes of the presented buffer (the old whole-device rotate drain
    // masked this race; removing it surfaced occasional ghosting). Bounded:
    // on timeout the present proceeds — a rare one-frame ghost self-heals at
    // the next acquire refresh. `HKLM\SOFTWARE\Helios!PresentGateUs` (DWORD)
    // overrides the cap; 0 disables. Cost telemetry: `present-gate:` lines.
    let gate_us = present_gate_us();
    if gate_us != 0 {
        if let Some(dev) = helios_device(h) {
            dev.dxvk.present_frame_gate(gate_us);
        }
    }

    if let Some(dev) = helios_device(h) {
        if !dev.dxgi_callbacks.is_null() && src_alloc != 0 && !dev.h_context.is_null() {
            if let Some(present_cb) = (*dev.dxgi_callbacks).pfnPresentCb {
                let mut cb = ddi::DXGIDDICB_PRESENT::default();
                cb.hSrcAllocation = src_alloc;
                cb.hDstAllocation = dst_alloc;
                cb.pDXGIContext = a.pDXGIContext;
                cb.hContext = dev.h_context;
                cb.BroadcastContextCount = 0;
                cb.PrivateDriverDataSize = 0;
                cb.pPrivateDriverData = core::ptr::null_mut();
                cb.bOptimizeForComposition = 0;
                present_hr = present_cb(dev.h_rt_device, &mut cb);
            } else {
                log_line("DXGI Present: pfnPresentCb missing");
            }
        } else {
            log_line(&format!(
                "DXGI Present: skip PresentCb callbacks={} src=0x{:x} hContext={:p}",
                dev.dxgi_callbacks.is_null(),
                src_alloc,
                dev.h_context
            ));
        }
    }

    // Forensics for the DWM indirect-swapchain flip-present failure (3 OK then
    // 0x80070057): log the rotating runtime resource handle vs our collapsed
    // allocation handle, subresource indices, raw flags and flip interval, and
    // a per-process present ordinal so cycles can be told apart.
    static PRESENT_ORDINAL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let ordinal = PRESENT_ORDINAL.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    trace_line!(
        "DXGI Present: #{} src=0x{:x} dst=0x{:x} copied={} flags=0x{:x} presentCb=0x{:08x} \
         hSurf={:p} srcSub={} hDstRes={:p} dstSub={} flipInterval={} dxgiCtx={:p} hContext={:p} \
         syncVal={}",
        ordinal,
        src_alloc,
        dst_alloc,
        copied,
        *(&a.Flags as *const ddi::DXGI_DDI_PRESENT_FLAGS as *const u32),
        present_hr as u32,
        src_h.pDrvPrivate,
        a.SrcSubResourceIndex,
        dst_h.pDrvPrivate,
        a.DstSubResourceIndex,
        a.FlipInterval,
        a.pDXGIContext,
        dev_context_for_log(h),
        sync_value
    );
    present_hr
}

// Best-effort context handle for present logging (null when unavailable).
fn dev_context_for_log(h: ddi::D3D10DDI_HDEVICE) -> *mut core::ffi::c_void {
    unsafe { helios_device(h).map_or(core::ptr::null_mut(), |d| d.h_context) }
}

unsafe extern "C" fn dxgi_get_gamma_caps(
    arg: *mut ddi::DXGI_DDI_ARG_GET_GAMMA_CONTROL_CAPS,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let caps = (*arg).pGammaCapabilities;
    if !caps.is_null() {
        core::ptr::write_bytes(
            caps as *mut u8,
            0,
            core::mem::size_of::<ddi::DXGI_GAMMA_CONTROL_CAPABILITIES>(),
        );
        (*caps).MaxConvertedValue = 1.0;
        (*caps).MinConvertedValue = 0.0;
    }
    0
}

unsafe extern "C" fn dxgi_set_display_mode(arg: *mut ddi::DXGI_DDI_ARG_SETDISPLAYMODE) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    if let Some(context) = d3d11_context(dxgi_device_handle(a.hDevice)) {
        context.Flush();
    }
    0
}

unsafe extern "C" fn dxgi_set_resource_priority(
    _arg: *mut ddi::DXGI_DDI_ARG_SETRESOURCEPRIORITY,
) -> i32 {
    0
}

unsafe extern "C" fn dxgi_query_resource_residency(
    arg: *mut ddi::DXGI_DDI_ARG_QUERYRESOURCERESIDENCY,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    if !a.pStatus.is_null() {
        for i in 0..a.Resources as usize {
            *a.pStatus.add(i) = ddi::DXGI_DDI_RESIDENCY_DXGI_DDI_RESIDENCY_FULLY_RESIDENT;
        }
    }
    0
}

/// DXGI flip-model identity rotation. The runtime calls this after each flip
/// present so the app's fixed buffer objects walk the swapchain's allocation
/// ring: resource[i] takes resource[i+1]'s identity, the last takes the
/// first's. Two coordinated moves keep the world consistent:
///   1. the DXVK storages (venus memory + VkImage + KMT handles) rotate in
///      the bridge, so draws through existing views land in the allocation
///      the runtime now associates with the buffer;
///   2. our per-resource WDDM {allocation, km} records rotate here, so the
///      next present reports the rotated hSrcAllocation to dxgkrnl.
/// The old Flush-only stub pinned dwm's composition to ONE allocation while
/// dxgkrnl/IddCx walked all three swapchain buffers — two of every three
/// acquired frames were buffers dwm never rendered (black IDD output).
unsafe extern "C" fn dxgi_rotate_resource_identities(
    arg: *mut ddi::DXGI_DDI_ARG_ROTATE_RESOURCE_IDENTITIES,
) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    let h = dxgi_device_handle(a.hDevice);
    let n = a.Resources as usize;
    if n < 2 || a.pResources.is_null() {
        return 0;
    }

    // Collect the per-resource state pointers; all entries must be tracked
    // resources or the rotation is refused whole (a partial rotation would
    // permanently corrupt the swapchain mapping).
    let mut states: Vec<*mut ResourceState> = Vec::with_capacity(n);
    for i in 0..n {
        let hr = dxgi_resource_handle(*a.pResources.add(i));
        if hr.pDrvPrivate.is_null() {
            log_line("DXGI RotateResourceIdentities: null resource handle");
            return 0;
        }
        let state = *(hr.pDrvPrivate as *const *mut ResourceState);
        if state.is_null() {
            log_line("DXGI RotateResourceIdentities: untracked resource");
            return 0;
        }
        states.push(state);
    }

    let rotated = if let Some(dev) = helios_device(h) {
        let ptrs: Vec<usize> = states.iter().map(|s| (**s).com_raw).collect();
        dev.dxvk.rotate_resource_backings(ptrs.as_ptr(), ptrs.len())
    } else {
        false
    };
    if !rotated {
        log_line("DXGI RotateResourceIdentities: backing rotation FAILED");
        return 0;
    }

    // Rotate the WDDM identity records in lockstep with the storages.
    let first = (
        (*states[0]).allocation,
        (*states[0]).km_resource,
        (*states[0]).owns_allocation,
    );
    for i in 0..n - 1 {
        (*states[i]).allocation = (*states[i + 1]).allocation;
        (*states[i]).km_resource = (*states[i + 1]).km_resource;
        (*states[i]).owns_allocation = (*states[i + 1]).owns_allocation;
    }
    (*states[n - 1]).allocation = first.0;
    (*states[n - 1]).km_resource = first.1;
    (*states[n - 1]).owns_allocation = first.2;

    let c = ROTATE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if c < 64 {
        trace_line!(
            "DXGI RotateResourceIdentities: rotated {} resources, alloc[0]=0x{:x}",
            n,
            (*states[0]).allocation
        );
    }
    0
}

static ROTATE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn dxgi_blt(arg: *mut ddi::DXGI_DDI_ARG_BLT) -> i32 {
    if arg.is_null() {
        return 0;
    }
    let a = &*arg;
    let Some(context) = d3d11_context(dxgi_device_handle(a.hDevice)) else {
        return 0;
    };
    let dst_h = dxgi_resource_handle(a.hDstResource);
    let src_h = dxgi_resource_handle(a.hSrcResource);
    let (Some(dst), Some(src)) = (
        load_resource(dst_h.pDrvPrivate),
        load_resource(src_h.pDrvPrivate),
    ) else {
        log_line(&format!(
            "DXGI Blt: missing resource dst=0x{:x} src=0x{:x}",
            a.hDstResource, a.hSrcResource
        ));
        return 0;
    };

    // The DXGI 1.0 blit DDI has no source rectangle. For DWM/windowed present
    // setup the runtime uses it to move between compatible proxy/front-buffer
    // surfaces, so a full subresource copy is the safest baseline.
    context.CopySubresourceRegion(
        &*dst,
        a.DstSubresource,
        a.DstLeft,
        a.DstTop,
        0,
        &*src,
        a.SrcSubresource,
        None,
    );
    context.Flush();
    0
}

/// Install typed DXGI base-DDI handlers over the stub fill.
pub unsafe fn install_dxgi(funcs: *mut ddi::DXGI_DDI_BASE_FUNCTIONS) {
    let f = &mut *funcs;
    f.pfnPresent = Some(dxgi_present);
    f.pfnGetGammaCaps = Some(dxgi_get_gamma_caps);
    f.pfnSetDisplayMode = Some(dxgi_set_display_mode);
    f.pfnSetResourcePriority = Some(dxgi_set_resource_priority);
    f.pfnQueryResourceResidency = Some(dxgi_query_resource_residency);
    f.pfnRotateResourceIdentities = Some(dxgi_rotate_resource_identities);
    f.pfnBlt = Some(dxgi_blt);
}

pub unsafe fn install_dxgi_1_1(funcs: *mut ddi::DXGI1_1_DDI_BASE_FUNCTIONS) {
    let f = &mut *funcs;
    f.pfnResolveSharedResource = Some(dxgi_resolve_shared_resource);
}

/// Install the implemented forwarders into the device-funcs table (over the
/// stub fill). Uses the real bindgen PFN field types — no transmute.
pub unsafe fn install(funcs: *mut ddi::D3D11DDI_DEVICEFUNCS) {
    let f = &mut *funcs;
    f.pfnCalcPrivateResourceSize = Some(calc_size_resource);
    f.pfnCalcPrivateOpenedResourceSize = Some(calc_size_opened_resource);
    f.pfnCreateResource = Some(create_resource);
    f.pfnOpenResource = Some(open_resource);
    f.pfnDestroyResource = Some(destroy_resource);
    f.pfnCalcPrivateRenderTargetViewSize = Some(calc_size_rtv);
    f.pfnCreateRenderTargetView = Some(create_rtv);
    f.pfnDestroyRenderTargetView = Some(destroy_rtv);
    f.pfnClearRenderTargetView = Some(clear_rtv);
    f.pfnCalcPrivateDepthStencilViewSize = Some(calc_size_dsv);
    f.pfnCreateDepthStencilView = Some(create_dsv);
    f.pfnDestroyDepthStencilView = Some(destroy_dsv);
    f.pfnClearDepthStencilView = Some(clear_dsv);
    f.pfnResourceCopy = Some(resource_copy);
    f.pfnResourceCopyRegion = Some(resource_copy_region);
    f.pfnResourceConvert = Some(resource_copy);
    f.pfnResourceConvertRegion = Some(resource_copy_region);
    f.pfnResourceResolveSubresource = Some(resource_resolve_subresource);
    f.pfnResourceIsStagingBusy = Some(resource_is_staging_busy);
    f.pfnResourceMap = Some(resource_map);
    f.pfnResourceUnmap = Some(resource_unmap);
    f.pfnDynamicIABufferMapNoOverwrite = Some(resource_map);
    f.pfnDynamicIABufferUnmap = Some(resource_unmap);
    f.pfnDynamicConstantBufferMapDiscard = Some(resource_map);
    f.pfnDynamicIABufferMapDiscard = Some(resource_map);
    f.pfnDynamicConstantBufferUnmap = Some(resource_unmap);
    f.pfnDynamicResourceMapDiscard = Some(resource_map);
    f.pfnDynamicResourceUnmap = Some(resource_unmap);
    f.pfnStagingResourceMap = Some(resource_map);
    f.pfnStagingResourceUnmap = Some(resource_unmap);
    f.pfnShaderResourceViewReadAfterWriteHazard = Some(srv_read_after_write_hazard);
    f.pfnResourceReadAfterWriteHazard = Some(resource_read_after_write_hazard);
    f.pfnFlush = Some(flush);

    // Shaders + pipeline.
    f.pfnCalcPrivateShaderSize = Some(calc_size_shader);
    f.pfnCreateVertexShader = Some(create_vertex_shader);
    f.pfnCreateGeometryShader = Some(create_geometry_shader);
    f.pfnCreatePixelShader = Some(create_pixel_shader);
    f.pfnCalcPrivateGeometryShaderWithStreamOutput = Some(calc_size_geometry_shader_so);
    f.pfnCreateGeometryShaderWithStreamOutput = Some(create_geometry_shader_so);
    f.pfnCalcPrivateTessellationShaderSize = Some(calc_size_tess_shader);
    f.pfnCreateHullShader = Some(create_hull_shader);
    f.pfnCreateDomainShader = Some(create_domain_shader);
    f.pfnCreateComputeShader = Some(create_compute_shader);
    f.pfnDestroyShader = Some(destroy_shader);
    f.pfnVsSetShader = Some(vs_set_shader);
    f.pfnPsSetShader = Some(ps_set_shader);
    f.pfnGsSetShader = Some(gs_set_shader);
    f.pfnHsSetShader = Some(hs_set_shader);
    f.pfnDsSetShader = Some(ds_set_shader);
    f.pfnCsSetShader = Some(cs_set_shader);
    f.pfnPsSetShaderWithIfaces = Some(ps_set_shader_with_ifaces);
    f.pfnVsSetShaderWithIfaces = Some(vs_set_shader_with_ifaces);
    f.pfnGsSetShaderWithIfaces = Some(gs_set_shader_with_ifaces);
    f.pfnHsSetShaderWithIfaces = Some(hs_set_shader_with_ifaces);
    f.pfnDsSetShaderWithIfaces = Some(ds_set_shader_with_ifaces);
    f.pfnCsSetShaderWithIfaces = Some(cs_set_shader_with_ifaces);
    f.pfnSetRenderTargets = Some(set_render_targets);
    f.pfnSetViewports = Some(set_viewports);
    f.pfnSetScissorRects = Some(set_scissor_rects);
    f.pfnIaSetTopology = Some(ia_set_topology);
    f.pfnDraw = Some(draw);
    f.pfnDrawIndexed = Some(draw_indexed);
    f.pfnDrawInstanced = Some(draw_instanced);
    f.pfnDrawIndexedInstanced = Some(draw_indexed_instanced);
    f.pfnDrawAuto = Some(draw_auto);
    f.pfnDrawInstancedIndirect = Some(draw_instanced_indirect);
    f.pfnDrawIndexedInstancedIndirect = Some(draw_indexed_instanced_indirect);
    f.pfnSoSetTargets = Some(so_set_targets);
    f.pfnSetTextFilterSize = Some(set_text_filter_size);

    // Rasterizer + depth-stencil state.
    f.pfnCalcPrivateRasterizerStateSize = Some(calc_size_raster);
    f.pfnCreateRasterizerState = Some(create_rasterizer_state);
    f.pfnSetRasterizerState = Some(set_rasterizer_state);
    f.pfnDestroyRasterizerState = Some(destroy_raster_state);
    f.pfnCalcPrivateDepthStencilStateSize = Some(calc_size_depth);
    f.pfnCreateDepthStencilState = Some(create_depth_stencil_state);
    f.pfnSetDepthStencilState = Some(set_depth_stencil_state);
    f.pfnDestroyDepthStencilState = Some(destroy_depth_state);

    // SRVs, samplers, constant buffers, updates, format support.
    f.pfnCalcPrivateShaderResourceViewSize = Some(calc_size_srv);
    f.pfnCreateShaderResourceView = Some(create_srv);
    f.pfnDestroyShaderResourceView = Some(destroy_srv);
    f.pfnCalcPrivateSamplerSize = Some(calc_size_sampler);
    f.pfnCreateSampler = Some(create_sampler);
    f.pfnDestroySampler = Some(destroy_sampler);
    f.pfnPsSetConstantBuffers = Some(ps_set_constant_buffers);
    f.pfnVsSetConstantBuffers = Some(vs_set_constant_buffers);
    f.pfnGsSetConstantBuffers = Some(gs_set_constant_buffers);
    f.pfnHsSetConstantBuffers = Some(hs_set_constant_buffers);
    f.pfnDsSetConstantBuffers = Some(ds_set_constant_buffers);
    f.pfnCsSetConstantBuffers = Some(cs_set_constant_buffers);
    f.pfnPsSetShaderResources = Some(ps_set_shader_resources);
    f.pfnVsSetShaderResources = Some(vs_set_shader_resources);
    f.pfnGsSetShaderResources = Some(gs_set_shader_resources);
    f.pfnHsSetShaderResources = Some(hs_set_shader_resources);
    f.pfnDsSetShaderResources = Some(ds_set_shader_resources);
    f.pfnCsSetShaderResources = Some(cs_set_shader_resources);
    f.pfnPsSetSamplers = Some(ps_set_samplers);
    f.pfnVsSetSamplers = Some(vs_set_samplers);
    f.pfnGsSetSamplers = Some(gs_set_samplers);
    f.pfnHsSetSamplers = Some(hs_set_samplers);
    f.pfnDsSetSamplers = Some(ds_set_samplers);
    f.pfnCsSetSamplers = Some(cs_set_samplers);
    f.pfnResourceUpdateSubresourceUP = Some(resource_update_subresource);
    f.pfnDefaultConstantBufferUpdateSubresourceUP = Some(resource_update_subresource);
    f.pfnGenMips = Some(gen_mips);
    f.pfnCheckFormatSupport = Some(check_format_support);
    f.pfnCheckMultisampleQualityLevels = Some(check_multisample_quality_levels);
    f.pfnCheckCounterInfo = Some(check_counter_info);
    f.pfnCheckCounter = Some(check_counter);

    // Queries and predication.
    f.pfnCalcPrivateQuerySize = Some(calc_size_query);
    f.pfnCreateQuery = Some(create_query);
    f.pfnDestroyQuery = Some(destroy_query);
    f.pfnQueryBegin = Some(query_begin);
    f.pfnQueryEnd = Some(query_end);
    f.pfnQueryGetData = Some(query_get_data);
    f.pfnSetPredication = Some(set_predication);

    // D3D11 UAV/compute paths.
    f.pfnCalcPrivateUnorderedAccessViewSize = Some(calc_size_uav);
    f.pfnCreateUnorderedAccessView = Some(create_uav);
    f.pfnDestroyUnorderedAccessView = Some(destroy_uav);
    f.pfnClearUnorderedAccessViewUint = Some(clear_uav_uint);
    f.pfnClearUnorderedAccessViewFloat = Some(clear_uav_float);
    f.pfnCsSetUnorderedAccessViews = Some(cs_set_uavs);
    f.pfnCopyStructureCount = Some(copy_structure_count);
    f.pfnDispatch = Some(dispatch);
    f.pfnDispatchIndirect = Some(dispatch_indirect);
    f.pfnSetResourceMinLOD = Some(set_resource_min_lod);

    // Input layouts (lazy), vertex/index buffers, blend state.
    f.pfnCalcPrivateElementLayoutSize = Some(calc_size_element_layout);
    f.pfnCreateElementLayout = Some(create_element_layout);
    f.pfnDestroyElementLayout = Some(destroy_element_layout);
    f.pfnIaSetInputLayout = Some(ia_set_input_layout);
    f.pfnIaSetVertexBuffers = Some(ia_set_vertex_buffers);
    f.pfnIaSetIndexBuffer = Some(ia_set_index_buffer);
    f.pfnCalcPrivateBlendStateSize = Some(calc_size_blend);
    f.pfnCreateBlendState = Some(create_blend_state);
    f.pfnSetBlendState = Some(set_blend_state);
    f.pfnDestroyBlendState = Some(destroy_blend_state);
}

/// Install D3D11.1-specific handlers whose signatures differ from the D3D11.0
/// prefix or only exist in the D3D11.1 table.
pub unsafe fn install_11_1(funcs: *mut ddi::D3D11_1DDI_DEVICEFUNCS) {
    let f = &mut *funcs;
    f.pfnVsSetConstantBuffers = Some(vs_set_constant_buffers1);
    f.pfnPsSetConstantBuffers = Some(ps_set_constant_buffers1);
    f.pfnGsSetConstantBuffers = Some(gs_set_constant_buffers1);
    f.pfnHsSetConstantBuffers = Some(hs_set_constant_buffers1);
    f.pfnDsSetConstantBuffers = Some(ds_set_constant_buffers1);
    f.pfnCsSetConstantBuffers = Some(cs_set_constant_buffers1);
    f.pfnFlush = Some(flush_11_1);
    f.pfnResourceCopyRegion = Some(resource_copy_region_11_1);
    f.pfnResourceConvertRegion = Some(resource_copy_region_11_1);
    f.pfnResourceUpdateSubresourceUP = Some(resource_update_subresource_11_1);
    f.pfnDefaultConstantBufferUpdateSubresourceUP = Some(resource_update_subresource_11_1);
    f.pfnDiscard = Some(discard_11_1);
    f.pfnCheckDirectFlipSupport = Some(check_direct_flip_support_11_1);
    f.pfnClearView = Some(clear_view_11_1);
    // The >=11.1 tables pass D3D11_1_DDI_BLEND_DESC (LogicOpEnable/LogicOp
    // inserted mid-struct) — `install()`'s 10.1-desc handlers misread the
    // write mask (see create_blend_state_11_1). NOTE the 11.1 rasterizer desc
    // only APPENDS ForcedSampleCount, so the shared 10.x reader stays valid
    // for pfnCreateRasterizerState.
    f.pfnCalcPrivateBlendStateSize = Some(calc_size_blend_11_1);
    f.pfnCreateBlendState = Some(create_blend_state_11_1);
    // The >=11.1 shader creates carry TYPED signature entries
    // (D3D11_1DDIARG_SIGNATURE_ENTRY2.RegisterComponentType); forward them so
    // dxbc-spv declares correctly-typed shader I/O instead of assuming
    // float32 for everything (hull/domain use a different tessellation
    // signatures struct and compute has none — those keep the shared
    // handlers).
    f.pfnCreateVertexShader = Some(create_vertex_shader_11_1);
    f.pfnCreatePixelShader = Some(create_pixel_shader_11_1);
    f.pfnCreateGeometryShader = Some(create_geometry_shader_11_1);
}

pub unsafe fn install_wddm2_1(funcs: *mut ddi::D3DWDDM2_1DDI_DEVICEFUNCS) {
    let f = &mut *funcs;
    f.pfnCheckMultisampleQualityLevels = Some(check_multisample_quality_levels_wddm1_3);
    f.pfnAcquireResource = Some(acquire_resource_2_1);
    f.pfnReleaseResource = Some(release_resource_2_1);
}
