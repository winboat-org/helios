//! CPU executor for `DxgkDdiRenderGdi` GDI command streams.
//!
//! `SupportKernelModeCommandBuffer` (the GDI hardware-acceleration opt-in) is
//! LOAD-MANDATORY for this adapter shape (bisected 2026-07-02, v22.22.34.0:
//! dropping it => CM_PROB_FAILED_ADD), so ALL window content — explorer, shell,
//! LogonUI, everything GDI-drawn — arrives here as `DXGK_RENDERKM_COMMAND` ops.
//! The null engine executes nothing, so before this module existed every GDI
//! surface stayed zero and DWM (whose own D3D composition pipeline works —
//! `helios_clear_test` readback passes) correctly composed a black desktop.
//!
//! This module performs the raster ops on the CPU, directly against the
//! allocations' host-visible venus blob memory — the same bytes the host GPU
//! samples when DWM composes — at `DxgkDdiRenderGdi` time (PASSIVE_LEVEL, per
//! the DDI's IRQL annotation).
//!
//! Scope/limits (counted in the diag ring, never fatal):
//! - 32-bpp surfaces only (GDI HW acceleration is 32-bpp by contract).
//! - BITBLT rops SRCCOPY/SRCINVERT/SRCAND/SRCOR are exact; ROP3 falls back to
//!   SRCCOPY. COLORFILL is executed as PATCOPY regardless of fill rop.
//! - CLEARTYPEBLEND approximates gamma-corrected text blending with a straight
//!   per-channel alpha blend of the foreground color (gamma tables ignored).
//! - Every rect is clipped to the surface extent AND the mapped blob length; an
//!   op whose surfaces cannot be resolved/mapped is skipped and counted.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use wdk_sys::ntddk::{MmMapIoSpace, MmUnmapIoSpace};
use wdk_sys::_MEMORY_CACHING_TYPE;
use wdk_sys::PHYSICAL_ADDRESS;

use crate::adapter::AdapterContext;
use crate::dxgk::*;

use super::create_allocation::{cross_adapter_pitch, present_alloc_info};

/// Total ops executed / skipped since boot (diag-ring visible as GdiE/GdiS).
static OPS_EXECUTED: AtomicU32 = AtomicU32::new(0);
static OPS_SKIPPED: AtomicU32 = AtomicU32::new(0);

/// Why `surface()` failed, tallied per reason (registry-visible as GdFa/GdFg/
/// GdFs/GdFb/GdFm) + the last resource id that failed blob lookup (GdFr).
static SKIP_NO_ALLOC: AtomicU32 = AtomicU32::new(0); // alloc-list/present_alloc_info
static SKIP_NO_GEOM: AtomicU32 = AtomicU32::new(0); // zero width/height
static SKIP_NO_SLOT: AtomicU32 = AtomicU32::new(0); // MAX_MAPPINGS exhausted
static SKIP_NO_BLOB: AtomicU32 = AtomicU32::new(0); // blob_kernel_range failed
static SKIP_NO_MAP: AtomicU32 = AtomicU32::new(0); // MmMapIoSpace returned null
static LAST_SKIP_RESID: AtomicU32 = AtomicU32::new(0);
/// Commands whose declared payload does not fit the remaining batch bytes
/// (registry-visible as GdTc) and StretchBlts with an empty source rect,
/// executed as no-ops (GdDs). Both were previously SILENT skip paths; GdTc in
/// particular hid a systematic drop: the old guard required the FULL
/// `DXGK_RENDERKM_COMMAND` struct (whose union is sized by the largest arm) to
/// fit the buffer, so the final command of a tightly-sized batch was dropped
/// whenever its arm was smaller — e.g. the lone COLORFILL that paints the
/// desktop background (1 skip per desktop repaint, ~48% of all ops by
/// 2026-07-05). Validation is now per-arm.
static SKIP_TRUNC_CMD: AtomicU32 = AtomicU32::new(0);
static DEGENERATE_STRETCH: AtomicU32 = AtomicU32::new(0);

/// Where does the desktop plate fill go? (black-desktop hunt, 15th session)
/// Large ops only (dst area >= 64k px) to keep registry churn low:
/// GdCn/GdCr/GdCc/GdCg = large-COLORFILL count / last dst resid / color / geom
/// (w<<16|h); GdBn/GdBr/GdBg = same for large BITBLTs. GdXn/GdXz/GdXr = total
/// CLEARTYPEBLEND ops / ops whose sampled alpha row was ALL-ZERO / last alpha
/// resid — tests the "glyph atlas is an empty blob → text invisible" theory.
static BIG_FILL_COUNT: AtomicU32 = AtomicU32::new(0);
static BIG_FILL_RESID: AtomicU32 = AtomicU32::new(0);
static BIG_FILL_COLOR: AtomicU32 = AtomicU32::new(0);
static BIG_FILL_GEOM: AtomicU32 = AtomicU32::new(0);
static BIG_BLT_COUNT: AtomicU32 = AtomicU32::new(0);
static BIG_BLT_RESID: AtomicU32 = AtomicU32::new(0);
static BIG_BLT_GEOM: AtomicU32 = AtomicU32::new(0);
static CT_COUNT: AtomicU32 = AtomicU32::new(0);
static CT_ALPHA_ZERO: AtomicU32 = AtomicU32::new(0);
static CT_ALPHA_RESID: AtomicU32 = AtomicU32::new(0);

/// Threshold above which a fill/blt is "large" (the desktop plate is ~1.9M px).
const BIG_OP_AREA: i64 = 65536;

/// Cap on sub-rect list length; a longer list is truncated.
const MAX_SUBRECTS: usize = 1024;
/// Distinct surfaces mappable per command batch.
const MAX_MAPPINGS: usize = 8;

/// A kernel mapping of one surface's backing blob for the current batch.
struct SurfMap {
    resource_id: u32,
    view: SurfView,
}

/// Copyable geometry + mapping view of a surface (no borrow of the map table).
#[derive(Clone, Copy)]
struct SurfView {
    va: *mut u8,
    len: usize,
    width: i32,
    height: i32,
    pitch: usize,
    resource_id: u32,
}

/// Execute every op in the batch. Never fails: unexecutable ops are skipped
/// (the caller still records the command bytes into the DMA buffer as before,
/// so submission bookkeeping is unchanged).
pub unsafe fn execute(adapter: &AdapterContext, args: &DXGKARG_RENDERGDI) {
    if args.pCommand.is_null() || args.CommandLength == 0 {
        return;
    }
    let mut maps: [Option<SurfMap>; MAX_MAPPINGS] = Default::default();

    let base = args.pCommand as *const u8;
    let total = args.CommandLength as usize;
    let mut off = 0usize;
    while off + 8 <= total {
        // SAFETY: off+8 <= total; OpCode/CommandSize are the first two u32s.
        let cmd = unsafe { base.add(off) as *const DXGK_RENDERKM_COMMAND };
        let opcode = unsafe { core::ptr::read_unaligned(&(*cmd).OpCode) };
        let csize = unsafe { core::ptr::read_unaligned(&(*cmd).CommandSize) } as usize;
        if csize < 8 || off + csize > total {
            break;
        }
        // Payload bounds are validated PER UNION ARM inside dispatch (read_arm):
        // requiring the whole DXGK_RENDERKM_COMMAND here dropped the final
        // command of every tightly-sized batch whose arm was smaller than the
        // largest union member (the desktop-background COLORFILL among them).
        unsafe { dispatch(adapter, args, &mut maps, opcode, cmd, total - off) };
        off += csize;
    }

    for m in maps.iter().flatten() {
        // SAFETY: `va` came from MmMapIoSpace in `surface`; unmapped once here.
        unsafe { MmUnmapIoSpace(m.view.va as *mut c_void, m.view.len as u64) };
    }
    crate::diag::record_named_bytes(b"GdiE", OPS_EXECUTED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdiS", OPS_SKIPPED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdFa", SKIP_NO_ALLOC.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdFg", SKIP_NO_GEOM.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdFs", SKIP_NO_SLOT.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdFb", SKIP_NO_BLOB.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdFm", SKIP_NO_MAP.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdFr", LAST_SKIP_RESID.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdTc", SKIP_TRUNC_CMD.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdDs", DEGENERATE_STRETCH.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdCn", BIG_FILL_COUNT.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdCr", BIG_FILL_RESID.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdCc", BIG_FILL_COLOR.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdCg", BIG_FILL_GEOM.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdBn", BIG_BLT_COUNT.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdBr", BIG_BLT_RESID.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdBg", BIG_BLT_GEOM.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdXn", CT_COUNT.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdXz", CT_ALPHA_ZERO.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"GdXr", CT_ALPHA_RESID.load(Ordering::Relaxed));
}

/// Read the command's union payload as a `T`, validating that `T` fits the
/// bytes remaining in the batch (`avail` = buffer bytes from the command's
/// start). `None` (counted in GdTc) means the runtime supplied fewer bytes
/// than the arm needs — never read past the buffer on a tight final command.
unsafe fn read_arm<T>(cmd: *const DXGK_RENDERKM_COMMAND, avail: usize) -> Option<T> {
    // The union payload starts right after the two u32 header fields.
    let payload_off = core::mem::offset_of!(DXGK_RENDERKM_COMMAND, Command);
    if avail < payload_off + core::mem::size_of::<T>() {
        SKIP_TRUNC_CMD.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // SAFETY: avail bytes at `cmd` are within the command buffer (caller), and
    // payload_off + size_of::<T>() <= avail was checked above. addr_of! avoids
    // materializing a reference to the full (possibly short) struct.
    Some(unsafe {
        core::ptr::read_unaligned(core::ptr::addr_of!((*cmd).Command) as *const T)
    })
}

unsafe fn dispatch(
    adapter: &AdapterContext,
    args: &DXGKARG_RENDERGDI,
    maps: &mut [Option<SurfMap>; MAX_MAPPINGS],
    opcode: DXGK_RENDERKM_OPERATION,
    cmd: *const DXGK_RENDERKM_COMMAND,
    avail: usize,
) {
    let ok = match opcode {
        _DXGK_RENDERKM_OPERATION::DXGK_GDIOP_BITBLT => {
            match unsafe { read_arm::<DXGK_GDIARG_BITBLT>(cmd, avail) } {
                Some(a) => unsafe { op_bitblt(adapter, args, maps, &a) },
                None => false,
            }
        }
        _DXGK_RENDERKM_OPERATION::DXGK_GDIOP_COLORFILL => {
            match unsafe { read_arm::<DXGK_GDIARG_COLORFILL>(cmd, avail) } {
                Some(a) => unsafe { op_colorfill(adapter, args, maps, &a) },
                None => false,
            }
        }
        _DXGK_RENDERKM_OPERATION::DXGK_GDIOP_ALPHABLEND => {
            match unsafe { read_arm::<DXGK_GDIARG_ALPHABLEND>(cmd, avail) } {
                Some(a) => unsafe { op_alphablend(adapter, args, maps, &a) },
                None => false,
            }
        }
        _DXGK_RENDERKM_OPERATION::DXGK_GDIOP_STRETCHBLT => {
            match unsafe { read_arm::<DXGK_GDIARG_STRETCHBLT>(cmd, avail) } {
                Some(a) => unsafe { op_stretchblt(adapter, args, maps, &a) },
                None => false,
            }
        }
        _DXGK_RENDERKM_OPERATION::DXGK_GDIOP_TRANSPARENTBLT => {
            match unsafe { read_arm::<DXGK_GDIARG_TRANSPARENTBLT>(cmd, avail) } {
                Some(a) => unsafe { op_transparentblt(adapter, args, maps, &a) },
                None => false,
            }
        }
        _DXGK_RENDERKM_OPERATION::DXGK_GDIOP_CLEARTYPEBLEND => {
            match unsafe { read_arm::<DXGK_GDIARG_CLEARTYPEBLEND>(cmd, avail) } {
                Some(a) => unsafe { op_cleartypeblend(adapter, args, maps, &a) },
                None => false,
            }
        }
        // DXGK_GDIOP_ESCAPE and anything unknown: driver ignores by contract.
        _ => true,
    };
    if ok {
        OPS_EXECUTED.fetch_add(1, Ordering::Relaxed);
    } else {
        OPS_SKIPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Resolve allocation-list index `idx` to a mapped surface view, mapping its
/// backing blob on first use in this batch.
unsafe fn surface(
    adapter: &AdapterContext,
    args: &DXGKARG_RENDERGDI,
    maps: &mut [Option<SurfMap>; MAX_MAPPINGS],
    idx: u32,
) -> Option<SurfView> {
    if idx >= args.AllocationListSize || args.pAllocationList.is_null() {
        SKIP_NO_ALLOC.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // SAFETY: idx < AllocationListSize entries provided by dxgkrnl.
    let entry = unsafe { core::ptr::read_unaligned(args.pAllocationList.add(idx as usize)) };
    // SAFETY: hDeviceSpecificAllocation round-trips our OpenAllocation context.
    let Some(info) = (unsafe { present_alloc_info(entry.hDeviceSpecificAllocation) }) else {
        SKIP_NO_ALLOC.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if info.width == 0 || info.height == 0 {
        SKIP_NO_GEOM.fetch_add(1, Ordering::Relaxed);
        LAST_SKIP_RESID.store(info.resource_id, Ordering::Relaxed);
        return None;
    }

    for m in maps.iter().flatten() {
        if m.resource_id == info.resource_id {
            return Some(m.view);
        }
    }
    let Some(slot) = maps.iter().position(|m| m.is_none()) else {
        SKIP_NO_SLOT.fetch_add(1, Ordering::Relaxed);
        return None;
    };

    // Any-owner resolve + host-map of the allocation's blob (PASSIVE flow —
    // DxgkDdiRenderGdi runs at PASSIVE_LEVEL).
    let prep = match crate::virtio::ctrl::map_blob_prepare(adapter, None, info.resource_id) {
        Ok(p) => p,
        Err(_) => {
            SKIP_NO_BLOB.fetch_add(1, Ordering::Relaxed);
            LAST_SKIP_RESID.store(info.resource_id, Ordering::Relaxed);
            return None;
        }
    };
    let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
    pa.QuadPart = prep.gpa as i64;
    // SAFETY: PASSIVE_LEVEL (DxgkDdiRenderGdi's IRQL); the range was
    // RESOURCE_MAP_BLOB'd into the host-visible window, so the pages are backed.
    // Unmapped in `execute` once the batch completes.
    let va = unsafe { MmMapIoSpace(pa, prep.size, _MEMORY_CACHING_TYPE::MmCached) } as *mut u8;
    if va.is_null() {
        SKIP_NO_MAP.fetch_add(1, Ordering::Relaxed);
        LAST_SKIP_RESID.store(info.resource_id, Ordering::Relaxed);
        return None;
    }
    let view = SurfView {
        va,
        len: prep.size as usize,
        width: info.width as i32,
        height: info.height as i32,
        pitch: cross_adapter_pitch(info.width) as usize,
        resource_id: info.resource_id,
    };
    maps[slot] = Some(SurfMap {
        resource_id: info.resource_id,
        view,
    });
    Some(view)
}

#[derive(Clone, Copy)]
struct Rect {
    l: i32,
    t: i32,
    r: i32,
    b: i32,
}

impl Rect {
    fn from(r: &RECT) -> Self {
        Self {
            l: r.left,
            t: r.top,
            r: r.right,
            b: r.bottom,
        }
    }
    fn clip(&self, o: &Rect) -> Rect {
        Rect {
            l: self.l.max(o.l),
            t: self.t.max(o.t),
            r: self.r.min(o.r),
            b: self.b.min(o.b),
        }
    }
    fn empty(&self) -> bool {
        self.r <= self.l || self.b <= self.t
    }
}

fn surf_rect(v: &SurfView) -> Rect {
    Rect {
        l: 0,
        t: 0,
        r: v.width,
        b: v.height,
    }
}

fn pitch_or(v: &SurfView, given: u32) -> usize {
    if given != 0 {
        given as usize
    } else {
        v.pitch
    }
}

/// Iterate the effective destination rects of an op: each sub-rect (clipped to
/// DstRect) when a sub-rect list is present, else DstRect itself.
fn for_each_dst_rect(
    dst_rect: &RECT,
    num_subrects: u32,
    p_subrects: *mut RECT,
    mut f: impl FnMut(Rect),
) {
    let d = Rect::from(dst_rect);
    if num_subrects == 0 || p_subrects.is_null() {
        if !d.empty() {
            f(d);
        }
        return;
    }
    let n = (num_subrects as usize).min(MAX_SUBRECTS);
    for i in 0..n {
        // SAFETY: dxgkrnl supplies NumSubRects rects at pSubRects (kernel memory
        // owned by the command stream for the duration of the DDI call).
        let sr = unsafe { core::ptr::read_unaligned(p_subrects.add(i)) };
        let c = Rect::from(&sr).clip(&d);
        if !c.empty() {
            f(c);
        }
    }
}

/// Bounds-checked pointer to `count` pixels at (x, y) of a surface view.
/// Returns null when any part is outside the mapping.
fn row_ptr(v: &SurfView, pitch: usize, x: i32, y: i32, count: i32) -> *mut u8 {
    if x < 0 || y < 0 || count <= 0 {
        return core::ptr::null_mut();
    }
    let start = (y as usize).saturating_mul(pitch) + (x as usize) * 4;
    let end = start + (count as usize) * 4;
    if end > v.len {
        return core::ptr::null_mut();
    }
    // SAFETY: start < end <= len checked above.
    unsafe { v.va.add(start) }
}

/// Scale one 8-bit channel by `f`/255.
#[inline(always)]
fn scale8(v: u32, f: u32) -> u32 {
    (v * f + 127) / 255
}

unsafe fn op_colorfill(
    adapter: &AdapterContext,
    args: &DXGKARG_RENDERGDI,
    maps: &mut [Option<SurfMap>; MAX_MAPPINGS],
    a: &DXGK_GDIARG_COLORFILL,
) -> bool {
    let Some(d) = (unsafe { surface(adapter, args, maps, a.DstAllocationIndex) }) else {
        return false;
    };
    let bounds = surf_rect(&d);
    let color = a.Color;
    let area = (a.DstRect.right - a.DstRect.left) as i64
        * (a.DstRect.bottom - a.DstRect.top) as i64;
    if area >= BIG_OP_AREA {
        BIG_FILL_COUNT.fetch_add(1, Ordering::Relaxed);
        BIG_FILL_RESID.store(d.resource_id, Ordering::Relaxed);
        BIG_FILL_COLOR.store(color, Ordering::Relaxed);
        BIG_FILL_GEOM.store(
            (((a.DstRect.right - a.DstRect.left) as u32) << 16)
                | ((a.DstRect.bottom - a.DstRect.top) as u32 & 0xFFFF),
            Ordering::Relaxed,
        );
    }
    for_each_dst_rect(&a.DstRect, a.NumSubRects, a.pSubRects, |r| {
        let c = r.clip(&bounds);
        if c.empty() {
            return;
        }
        for y in c.t..c.b {
            let p = row_ptr(&d, d.pitch, c.l, y, c.r - c.l);
            if p.is_null() {
                continue;
            }
            for x in 0..(c.r - c.l) as usize {
                // SAFETY: row_ptr bounds-checked (c.r-c.l) pixels at p.
                unsafe { core::ptr::write_unaligned((p as *mut u32).add(x), color) };
            }
        }
    });
    true
}

unsafe fn op_bitblt(
    adapter: &AdapterContext,
    args: &DXGKARG_RENDERGDI,
    maps: &mut [Option<SurfMap>; MAX_MAPPINGS],
    a: &DXGK_GDIARG_BITBLT,
) -> bool {
    let Some(s) = (unsafe { surface(adapter, args, maps, a.SrcAllocationIndex) }) else {
        return false;
    };
    let Some(d) = (unsafe { surface(adapter, args, maps, a.DstAllocationIndex) }) else {
        return false;
    };
    let sp = pitch_or(&s, a.SrcPitch);
    let dp = pitch_or(&d, a.DstPitch);
    let s_bounds = surf_rect(&s);
    let d_bounds = surf_rect(&d);
    let blt_area = (a.DstRect.right - a.DstRect.left) as i64
        * (a.DstRect.bottom - a.DstRect.top) as i64;
    if blt_area >= BIG_OP_AREA {
        BIG_BLT_COUNT.fetch_add(1, Ordering::Relaxed);
        BIG_BLT_RESID.store(d.resource_id, Ordering::Relaxed);
        BIG_BLT_GEOM.store(
            (((a.DstRect.right - a.DstRect.left) as u32) << 16)
                | ((a.DstRect.bottom - a.DstRect.top) as u32 & 0xFFFF),
            Ordering::Relaxed,
        );
    }
    let rop = a.Rop as i32;
    let dx0 = a.DstRect.left;
    let dy0 = a.DstRect.top;
    let sx0 = a.SrcRect.left;
    let sy0 = a.SrcRect.top;
    let same = s.va == d.va;

    for_each_dst_rect(&a.DstRect, a.NumSubRects, a.pSubRects, |r| {
        let c = r.clip(&d_bounds);
        if c.empty() {
            return;
        }
        let w = c.r - c.l;
        // Rows bottom-up when src/dst overlap downward in the same surface (GDI
        // scroll blits); `core::ptr::copy` (memmove) handles the x overlap.
        let down = same && (sy0 + (c.t - dy0)) < c.t;
        let step: i32 = if down { -1 } else { 1 };
        let mut y = if down { c.b - 1 } else { c.t };
        while (down && y >= c.t) || (!down && y < c.b) {
            let sx = sx0 + (c.l - dx0);
            let sy = sy0 + (y - dy0);
            if sy >= s_bounds.t && sy < s_bounds.b && sx >= s_bounds.l && sx + w <= s_bounds.r
            {
                let sptr = row_ptr(&s, sp, sx, sy, w);
                let dptr = row_ptr(&d, dp, c.l, y, w);
                if !sptr.is_null() && !dptr.is_null() {
                    // SAFETY: both rows bounds-checked against their mappings.
                    unsafe {
                        match rop {
                            _DXGK_GDIROP_BITBLT::DXGK_GDIROP_SRCINVERT
                            | _DXGK_GDIROP_BITBLT::DXGK_GDIROP_SRCAND
                            | _DXGK_GDIROP_BITBLT::DXGK_GDIROP_SRCOR => {
                                for x in 0..w as usize {
                                    let sv = core::ptr::read_unaligned(
                                        (sptr as *const u32).add(x),
                                    );
                                    let dpx = (dptr as *mut u32).add(x);
                                    let dv = core::ptr::read_unaligned(dpx);
                                    let nv = match rop {
                                        _DXGK_GDIROP_BITBLT::DXGK_GDIROP_SRCINVERT => dv ^ sv,
                                        _DXGK_GDIROP_BITBLT::DXGK_GDIROP_SRCAND => dv & sv,
                                        _ => dv | sv,
                                    };
                                    core::ptr::write_unaligned(dpx, nv);
                                }
                            }
                            // SRCCOPY, ROP3 (approximated), anything else: copy.
                            _ => core::ptr::copy(sptr, dptr, (w as usize) * 4),
                        }
                    }
                }
            }
            y += step;
        }
    });
    true
}

unsafe fn op_alphablend(
    adapter: &AdapterContext,
    args: &DXGKARG_RENDERGDI,
    maps: &mut [Option<SurfMap>; MAX_MAPPINGS],
    a: &DXGK_GDIARG_ALPHABLEND,
) -> bool {
    let Some(s) = (unsafe { surface(adapter, args, maps, a.SrcAllocationIndex) }) else {
        return false;
    };
    let Some(d) = (unsafe { surface(adapter, args, maps, a.DstAllocationIndex) }) else {
        return false;
    };
    let sp = pitch_or(&s, a.SrcPitch);
    let s_bounds = surf_rect(&s);
    let d_bounds = surf_rect(&d);
    let const_a = a.SourceConstantAlpha as u32;
    let src_has_alpha = a.SourceHasAlpha != 0;
    let dx0 = a.DstRect.left;
    let dy0 = a.DstRect.top;
    let sx0 = a.SrcRect.left;
    let sy0 = a.SrcRect.top;

    for_each_dst_rect(&a.DstRect, a.NumSubRects, a.pSubRects, |r| {
        let c = r.clip(&d_bounds);
        if c.empty() {
            return;
        }
        let w = c.r - c.l;
        for y in c.t..c.b {
            let sx = sx0 + (c.l - dx0);
            let sy = sy0 + (y - dy0);
            if sy < s_bounds.t || sy >= s_bounds.b || sx < s_bounds.l || sx + w > s_bounds.r {
                continue;
            }
            let sptr = row_ptr(&s, sp, sx, sy, w);
            let dptr = row_ptr(&d, d.pitch, c.l, y, w);
            if sptr.is_null() || dptr.is_null() {
                continue;
            }
            for x in 0..w as usize {
                // SAFETY: rows bounds-checked above.
                unsafe {
                    let sv = core::ptr::read_unaligned((sptr as *const u32).add(x));
                    let dpx = (dptr as *mut u32).add(x);
                    let dv = core::ptr::read_unaligned(dpx);
                    let nv = if src_has_alpha {
                        // Premultiplied source (AC_SRC_ALPHA) scaled by the
                        // constant alpha: out = src*ca + dst*(1 - srcA*ca).
                        let sa = scale8(sv >> 24, const_a);
                        let inv = 255 - sa;
                        let b = scale8(sv & 0xFF, const_a) + scale8(dv & 0xFF, inv);
                        let g =
                            scale8((sv >> 8) & 0xFF, const_a) + scale8((dv >> 8) & 0xFF, inv);
                        let r8 = scale8((sv >> 16) & 0xFF, const_a)
                            + scale8((dv >> 16) & 0xFF, inv);
                        let a8 = sa + scale8(dv >> 24, inv);
                        (a8.min(255) << 24)
                            | (r8.min(255) << 16)
                            | (g.min(255) << 8)
                            | b.min(255)
                    } else {
                        let inv = 255 - const_a;
                        let b = scale8(sv & 0xFF, const_a) + scale8(dv & 0xFF, inv);
                        let g =
                            scale8((sv >> 8) & 0xFF, const_a) + scale8((dv >> 8) & 0xFF, inv);
                        let r8 = scale8((sv >> 16) & 0xFF, const_a)
                            + scale8((dv >> 16) & 0xFF, inv);
                        let a8 = scale8(sv >> 24, const_a) + scale8(dv >> 24, inv);
                        (a8.min(255) << 24)
                            | (r8.min(255) << 16)
                            | (g.min(255) << 8)
                            | b.min(255)
                    };
                    core::ptr::write_unaligned(dpx, nv);
                }
            }
        }
    });
    true
}

unsafe fn op_transparentblt(
    adapter: &AdapterContext,
    args: &DXGKARG_RENDERGDI,
    maps: &mut [Option<SurfMap>; MAX_MAPPINGS],
    a: &DXGK_GDIARG_TRANSPARENTBLT,
) -> bool {
    let Some(s) = (unsafe { surface(adapter, args, maps, a.SrcAllocationIndex) }) else {
        return false;
    };
    let Some(d) = (unsafe { surface(adapter, args, maps, a.DstAllocationIndex) }) else {
        return false;
    };
    let sp = pitch_or(&s, a.SrcPitch);
    let s_bounds = surf_rect(&s);
    let d_bounds = surf_rect(&d);
    let key = a.Color & 0x00FF_FFFF;
    let dx0 = a.DstRect.left;
    let dy0 = a.DstRect.top;
    let sx0 = a.SrcRect.left;
    let sy0 = a.SrcRect.top;

    for_each_dst_rect(&a.DstRect, a.NumSubRects, a.pSubRects, |r| {
        let c = r.clip(&d_bounds);
        if c.empty() {
            return;
        }
        let w = c.r - c.l;
        for y in c.t..c.b {
            let sx = sx0 + (c.l - dx0);
            let sy = sy0 + (y - dy0);
            if sy < s_bounds.t || sy >= s_bounds.b || sx < s_bounds.l || sx + w > s_bounds.r {
                continue;
            }
            let sptr = row_ptr(&s, sp, sx, sy, w);
            let dptr = row_ptr(&d, d.pitch, c.l, y, w);
            if sptr.is_null() || dptr.is_null() {
                continue;
            }
            for x in 0..w as usize {
                // SAFETY: rows bounds-checked above.
                unsafe {
                    let sv = core::ptr::read_unaligned((sptr as *const u32).add(x));
                    if (sv & 0x00FF_FFFF) != key {
                        core::ptr::write_unaligned((dptr as *mut u32).add(x), sv);
                    }
                }
            }
        }
    });
    true
}

unsafe fn op_stretchblt(
    adapter: &AdapterContext,
    args: &DXGKARG_RENDERGDI,
    maps: &mut [Option<SurfMap>; MAX_MAPPINGS],
    a: &DXGK_GDIARG_STRETCHBLT,
) -> bool {
    let Some(s) = (unsafe { surface(adapter, args, maps, a.SrcAllocationIndex) }) else {
        return false;
    };
    let Some(d) = (unsafe { surface(adapter, args, maps, a.DstAllocationIndex) }) else {
        return false;
    };
    let sp = pitch_or(&s, a.SrcPitch);
    let s_bounds = surf_rect(&s);
    let d_bounds = surf_rect(&d);
    let dw = (a.DstRect.right - a.DstRect.left).max(1) as i64;
    let dh = (a.DstRect.bottom - a.DstRect.top).max(1) as i64;
    let sw = (a.SrcRect.right - a.SrcRect.left) as i64;
    let sh = (a.SrcRect.bottom - a.SrcRect.top) as i64;
    if sw <= 0 || sh <= 0 {
        // An empty source rect stretches nothing — a valid no-op, not a skip
        // (counted separately so this path can never hide work again).
        DEGENERATE_STRETCH.fetch_add(1, Ordering::Relaxed);
        return true;
    }
    // Bitfield union: bit16 = MirrorX, bit17 = MirrorY (Mode occupies bits 0-15).
    let flags = unsafe { a.__bindgen_anon_1.Flags };
    let mirror_x = (flags >> 16) & 1 != 0;
    let mirror_y = (flags >> 17) & 1 != 0;

    for_each_dst_rect(&a.DstRect, a.NumSubRects, a.pSubRects, |r| {
        let c = r.clip(&d_bounds);
        if c.empty() {
            return;
        }
        for y in c.t..c.b {
            let mut ty = ((y - a.DstRect.top) as i64 * sh) / dh;
            if mirror_y {
                ty = sh - 1 - ty;
            }
            let sy = a.SrcRect.top + ty as i32;
            if sy < s_bounds.t || sy >= s_bounds.b {
                continue;
            }
            let dptr = row_ptr(&d, d.pitch, c.l, y, c.r - c.l);
            if dptr.is_null() {
                continue;
            }
            for (xi, x) in (c.l..c.r).enumerate() {
                let mut tx = ((x - a.DstRect.left) as i64 * sw) / dw;
                if mirror_x {
                    tx = sw - 1 - tx;
                }
                let sx = a.SrcRect.left + tx as i32;
                if sx < s_bounds.l || sx >= s_bounds.r {
                    continue;
                }
                let sptr = row_ptr(&s, sp, sx, sy, 1);
                if sptr.is_null() {
                    continue;
                }
                // SAFETY: single pixel bounds-checked via row_ptr on both sides.
                unsafe {
                    let sv = core::ptr::read_unaligned(sptr as *const u32);
                    core::ptr::write_unaligned((dptr as *mut u32).add(xi), sv);
                }
            }
        }
    });
    true
}

unsafe fn op_cleartypeblend(
    adapter: &AdapterContext,
    args: &DXGKARG_RENDERGDI,
    maps: &mut [Option<SurfMap>; MAX_MAPPINGS],
    a: &DXGK_GDIARG_CLEARTYPEBLEND,
) -> bool {
    let Some(al) = (unsafe { surface(adapter, args, maps, a.AlphaSurfAllocationIndex) }) else {
        return false;
    };
    let Some(d) = (unsafe { surface(adapter, args, maps, a.DstAllocationIndex) }) else {
        return false;
    };
    let ap = pitch_or(&al, a.AlphaSurfPitch);
    let a_bounds = surf_rect(&al);
    let d_bounds = surf_rect(&d);
    // Approximation: per-channel blend of the gamma-corrected foreground color
    // with the 32-bpp per-channel ClearType alpha surface; gamma tables ignored.
    let fg = a.Color;
    // Diagnostic: is the glyph alpha source EMPTY? Sample the first alpha row
    // this op reads; an all-zero row on every text op = the "glyph atlas never
    // written" theory (text invisible everywhere while fills render).
    CT_COUNT.fetch_add(1, Ordering::Relaxed);
    CT_ALPHA_RESID.store(al.resource_id, Ordering::Relaxed);
    {
        let w = (a.DstRect.right - a.DstRect.left).max(1);
        let ax = a.DstRect.left + a.DstToAlphaOffsetX;
        let ay = a.DstRect.top + a.DstToAlphaOffsetY;
        let p = row_ptr(&al, ap, ax.max(0), ay.max(0), w.min(al.width));
        if !p.is_null() {
            let mut acc = 0u8;
            for i in 0..((w.min(al.width) as usize) * 4) {
                // SAFETY: row_ptr bounds-checked w.min(al.width) pixels at p.
                acc |= unsafe { core::ptr::read_unaligned(p.add(i)) };
            }
            if acc == 0 {
                CT_ALPHA_ZERO.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    for_each_dst_rect(&a.DstRect, a.NumSubRects, a.pSubRects, |r| {
        let c = r.clip(&d_bounds);
        if c.empty() {
            return;
        }
        let w = c.r - c.l;
        for y in c.t..c.b {
            let ax0 = c.l + a.DstToAlphaOffsetX;
            let ay = y + a.DstToAlphaOffsetY;
            if ay < a_bounds.t || ay >= a_bounds.b || ax0 < a_bounds.l || ax0 + w > a_bounds.r
            {
                continue;
            }
            let aptr = row_ptr(&al, ap, ax0, ay, w);
            let dptr = row_ptr(&d, d.pitch, c.l, y, w);
            if aptr.is_null() || dptr.is_null() {
                continue;
            }
            for x in 0..w as usize {
                // SAFETY: rows bounds-checked above.
                unsafe {
                    let av = core::ptr::read_unaligned((aptr as *const u32).add(x));
                    let dpx = (dptr as *mut u32).add(x);
                    let dv = core::ptr::read_unaligned(dpx);
                    let ab = av & 0xFF;
                    let ag = (av >> 8) & 0xFF;
                    let ar = (av >> 16) & 0xFF;
                    let b = scale8(fg & 0xFF, ab) + scale8(dv & 0xFF, 255 - ab);
                    let g = scale8((fg >> 8) & 0xFF, ag) + scale8((dv >> 8) & 0xFF, 255 - ag);
                    let r8 =
                        scale8((fg >> 16) & 0xFF, ar) + scale8((dv >> 16) & 0xFF, 255 - ar);
                    let nv = (dv & 0xFF00_0000)
                        | (r8.min(255) << 16)
                        | (g.min(255) << 8)
                        | b.min(255);
                    core::ptr::write_unaligned(dpx, nv);
                }
            }
        }
    });
    true
}
