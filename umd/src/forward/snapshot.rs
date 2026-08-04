//! D4b ordered-snapshot substitution — the UMD half
//! (FIX-DESIGN-d4b-snapshot.md §3).
//!
//! On the direct-flip present path the app's own venus submission stream is
//! the only insertion point that orders capture between frame N and the app's
//! clear of frame N+2. So, at present time, DXVK records a GPU image copy of
//! the presented primary X into S_i — one slot of a 4-deep ring of ICD-owned
//! DirectOptimalScanout OPTIMAL images — and the present's private data then
//! describes S_i (`HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT`) so the KMD binds
//! and flushes the snapshot instead of X. Nothing ever clears S; the black
//! margin race dies structurally. **No CPU waits anywhere**: the blit rides
//! frame N's own command list (recorded BEFORE the present-time
//! `context.Flush()`, so `HeliosWaitFrameSubmitted` — the existing SUBMITTED
//! gate — covers it), and ordering against frame N+1 is queue order.
//!
//! The control flow is deliberately value-threaded, never device-latched:
//! `snapshot_for_present` returns a [`SnapshotPlan`] ONLY when the blit was
//! recorded this present, `dxgi_present` moves that plan into
//! `finish_present`, and [`apply_snapshot_override`] mutates the OWNED LOCAL
//! `present_private` before both of its consumers. A present whose blit arm
//! was skipped can therefore never carry a snapshot descriptor — a stale
//! descriptor would be a stale-frame display, the exact defect class this
//! mechanism exists to close.
//!
//! What the ring deliberately is NOT: it has no runtime objects, no WDDM
//! allocations, no KMT stamps, no `transfer_resource_ownership` (that is the
//! WDDM-adopt path; snapshots stay ICD-owned blobs — the KMD needs only the
//! resid + descriptor and its executor's `resource_is_live` arm refuses a
//! dead resid loudly), and no entry in `direct_scanout_allocations` or the
//! `dxgi_rotate_resource_identities` list (that list is `a.pResources` only).
//! Teardown is exactly the COM releases in `BridgeOwned::release`.

use super::*;

use crate::device_funcs::{SnapshotRing, SnapshotSlot};

/// Ring depth. 4 matches the deepest DXGI flip ring the direct primary
/// rotates through, giving a reuse distance of ~4 present periods before a
/// slot is re-written — and the D4a acquire additionally gates the blit on
/// any still-in-flight host read of the slot (the designed backstop).
pub(crate) const SNAPSHOT_RING_SLOTS: usize = 4;

/// Everything `finish_present` needs to substitute the descriptor: S_i's
/// identity and layout, captured from the ring slot the blit just targeted.
/// Constructed in exactly one place, the successful-blit arm of
/// [`snapshot_for_present`].
pub(crate) struct SnapshotPlan {
    pub(crate) resid: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pitch: u32,
    pub(crate) plane_offset: u64,
    pub(crate) dxgi_format: u32,
    pub(crate) venus_alloc_size: u64,
    pub(crate) memory_type_index: u32,
    pub(crate) purpose: SnapshotPurpose,
}

/// The typed consumer of a snapshot.  The direct arm binds it; the windowed
/// BLT arm imports it as an ordinary Vulkan source.  Keeping the distinction in
/// the plan prevents a WindowedBlt descriptor from ever reaching a scanout
/// refresh arm.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotPurpose {
    DirectFlip,
    WindowedBlt,
}

/// Presents whose descriptor was substituted with a snapshot (the UMD-side
/// twin of the KMD's `SnSub`).
pub(crate) static SNAP_SUBSTITUTED: AtomicUsize = AtomicUsize::new(0);
/// Ring builds that failed (create/identity/pitch). Present ran as today.
pub(crate) static SNAP_RING_CREATE_FAILS: AtomicUsize = AtomicUsize::new(0);
/// Blit bridge calls that failed or reported an extent mismatch. Present ran
/// as today.
pub(crate) static SNAP_COPY_FAILS: AtomicUsize = AtomicUsize::new(0);
/// Presents skipped because the presented geometry moved off the ring's
/// (resize / fullscreen transition); the ring was torn down for a lazy
/// rebuild on the next present.
pub(crate) static SNAP_GEOM_SKIPS: AtomicUsize = AtomicUsize::new(0);
/// `apply_snapshot_override` refusals: the private data re-read in
/// `finish_present` was absent or no longer matched the geometry the blit
/// was recorded against. The stale-descriptor guard's loud half.
pub(crate) static SNAP_PRIVATE_SKIPS: AtomicUsize = AtomicUsize::new(0);

fn counter_summary() -> String {
    format!(
        "sub={} ring_fails={} copy_fails={} geom_skips={} private_skips={}",
        SNAP_SUBSTITUTED.load(Ordering::Relaxed),
        SNAP_RING_CREATE_FAILS.load(Ordering::Relaxed),
        SNAP_COPY_FAILS.load(Ordering::Relaxed),
        SNAP_GEOM_SKIPS.load(Ordering::Relaxed),
        SNAP_PRIVATE_SKIPS.load(Ordering::Relaxed),
    )
}

/// Build the 4-slot ring against the presented primary's geometry.
///
/// Any per-slot failure abandons the whole build: the partially-filled `Vec`
/// drops, releasing the earlier slots' COM refs, and the caller presents as
/// today (§6 "ring create fails" row). Loud on every early failure.
///
/// # Safety
/// `h` must be the live device the caller resolved `dev` from.
unsafe fn build_ring(
    dev: &HeliosDevice,
    h: Hdevice,
    width: u32,
    height: u32,
    dxgi_format: u32,
) -> Option<SnapshotRing> {
    // The composition-surface baseline. Sharing/export is driven by the
    // DirectOptimalScanout marker inside create_ddi_scanout_texture2d, not by
    // these flags, and DXVK images are transfer-dst capable regardless.
    let bind = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
    let mut slots: Vec<SnapshotSlot> = Vec::with_capacity(SNAPSHOT_RING_SLOTS);
    for i in 0..SNAPSHOT_RING_SLOTS {
        let Some((res, row_pitch, plane_offset)) =
            dev.dxvk
                .create_scanout_texture2d(width, height, dxgi_format, bind, 0)
        else {
            let n = SNAP_RING_CREATE_FAILS.fetch_add(1, Ordering::Relaxed);
            if n < 16 || n % 512 == 0 {
                log_error!(
                    "scanout-snapshot: ring slot {i} create FAILED {}x{} fmt={} ({})",
                    width,
                    height,
                    dxgi_format,
                    counter_summary()
                );
            }
            return None;
        };
        // The KMD binds by venus resource id, so the slot must have the same
        // importable dedicated backing a direct primary has: a whole venus
        // memory (offset 0) with a live resid. Same gates as
        // finish_wddm_tex2d's backing classification.
        let (memory, memory_size, memory_offset, resid) = dxvk_resource_memory_info(h, &res);
        // Exact vkAllocateMemory identity where recorded (the C1 source);
        // the memory-info blob size is the fallback, mirroring the
        // open-path's preference order in resource.rs.
        let (mut alloc_size, mut memory_type_index) = (0u64, 0u32);
        // SAFETY: `res` is the live resource created above.
        let alloc_identity_known = unsafe {
            dev.dxvk.get_resource_alloc_identity(
                res.as_raw() as usize,
                &mut alloc_size,
                &mut memory_type_index,
            )
        };
        if !alloc_identity_known || alloc_size == 0 {
            alloc_size = memory_size;
        }
        let pitch = row_pitch as u32;
        if memory == 0 || memory_offset != 0 || resid == 0 || pitch == 0 || alloc_size == 0 {
            let n = SNAP_RING_CREATE_FAILS.fetch_add(1, Ordering::Relaxed);
            if n < 16 || n % 512 == 0 {
                log_error!(
                    "scanout-snapshot: ring slot {i} has no bindable identity \
                     memory=0x{:x} offset={} resid={} pitch={} alloc={} ({})",
                    memory,
                    memory_offset,
                    resid,
                    pitch,
                    alloc_size,
                    counter_summary()
                );
            }
            // `res` drops here, releasing the slot; `slots` drops the rest.
            return None;
        }
        // The ring owns the reference from here; into_raw hands it over so
        // the adopted wrapper does not release it (the PresentSrcEntry
        // precedent — SnapshotSlot::drop is the single release site).
        slots.push(SnapshotSlot {
            resource_raw: res.into_raw() as usize,
            resid,
            pitch,
            plane_offset,
            alloc_size,
            memory_type_index,
            alloc_identity_known,
        });
    }
    log_error!(
        "scanout-snapshot: ring built {}x{} fmt={} resids=[{}, {}, {}, {}] pitch={}",
        width,
        height,
        dxgi_format,
        slots[0].resid,
        slots[1].resid,
        slots[2].resid,
        slots[3].resid,
        slots[0].pitch,
    );
    Some(SnapshotRing {
        width,
        height,
        dxgi_format,
        slots,
        next: 0,
    })
}

/// The per-present snapshot arm, called from `dxgi_present`'s direct-flip
/// path ONLY (`published_to_scanout`), BEFORE the present-time
/// `context.Flush()` — that position is what keeps the blit inside frame N's
/// command list and under the frame watermark. Vehicle, windowed-BLT and
/// Present1-multi paths never call this.
///
/// `private` is the presented primary's private data, read from THIS present
/// (it rotates with the allocation — never cache it per resource); its
/// geometry keys the ring. Returns the substitution plan iff the blit was
/// recorded; every refusal presents exactly as today, counted and logged.
///
/// # Safety
/// `h`/`src_h` are the live device/source handles of the present in progress.
pub(crate) unsafe fn snapshot_for_present(
    h: Hdevice,
    src_h: ddi::D3D10DDI_HRESOURCE,
    width: u32,
    height: u32,
    dxgi_format: u32,
    purpose: SnapshotPurpose,
) -> Option<SnapshotPlan> {
    // The single cheap gate (§6 row 1): knob off or a KMD without
    // CAP_SNAPSHOT_BIND leaves the present path bit-identical to a build
    // without the mechanism — no ring, no blit, no logs.
    if !crate::scanout_snapshot_knob()
        || !(match purpose {
            SnapshotPurpose::DirectFlip => crate::scanout_acquire::scanout_snapshot_capable(),
            SnapshotPurpose::WindowedBlt => crate::scanout_acquire::windowed_blt_snapshot_capable(),
        })
    {
        return None;
    }
    let dev = helios_device(h)?;
    if width == 0 || height == 0 {
        // A valid private with zero extent cannot exist (finish_wddm_tex2d
        // mints from a real mip0); refuse rather than build a 0x0 ring.
        let n = SNAP_COPY_FAILS.fetch_add(1, Ordering::Relaxed);
        if n < 16 || n % 512 == 0 {
            log_error!(
                "scanout-snapshot: refused zero-extent private {}x{} ({})",
                width,
                height,
                counter_summary()
            );
        }
        return None;
    }

    let mut ring_cell = dev.owned.snapshot_ring.borrow_mut();
    // Geometry key check as a value, so no reference into the cell is live
    // when the cell is reassigned below.
    let stale_geometry: Option<(u32, u32, u32)> = ring_cell.as_ref().and_then(|ring| {
        (ring.width != width || ring.height != height || ring.dxgi_format != dxgi_format)
            .then_some((ring.width, ring.height, ring.dxgi_format))
    });
    if let Some((old_w, old_h, old_fmt)) = stale_geometry {
        // Geometry moved (resize / fullscreen transition): skip THIS present
        // — a min-extent blit into a wrong-shaped slot must never be bound —
        // tear the ring down and lazily rebuild next present.
        let n = SNAP_GEOM_SKIPS.fetch_add(1, Ordering::Relaxed);
        if n < 16 || n % 512 == 0 {
            log_error!(
                "scanout-snapshot: geometry change {}x{} fmt={} -> {}x{} fmt={}, \
                 ring torn down for lazy rebuild ({})",
                old_w,
                old_h,
                old_fmt,
                width,
                height,
                dxgi_format,
                counter_summary()
            );
        }
        *ring_cell = None;
        return None;
    }
    if ring_cell.is_none() {
        match build_ring(dev, h, width, height, dxgi_format) {
            Some(ring) => *ring_cell = Some(ring),
            None => return None, // counted + logged in build_ring
        }
    }
    // Rotate to the next slot and copy its (all-Copy) identity out, so the
    // borrow arithmetic below stays trivial.
    let (slot_index, dst_raw, plan, alloc_identity_known) = {
        let ring = ring_cell.as_mut()?; // Some by construction above
        let index = ring.next % SNAPSHOT_RING_SLOTS;
        ring.next = (index + 1) % SNAPSHOT_RING_SLOTS;
        let slot = &ring.slots[index];
        (
            index,
            slot.resource_raw,
            SnapshotPlan {
                resid: slot.resid,
                width: ring.width,
                height: ring.height,
                pitch: slot.pitch,
                plane_offset: slot.plane_offset,
                dxgi_format: ring.dxgi_format,
                venus_alloc_size: slot.alloc_size,
                memory_type_index: slot.memory_type_index,
                purpose,
            },
            slot.alloc_identity_known,
        )
    };

    if purpose == SnapshotPurpose::WindowedBlt && !alloc_identity_known {
        let n = SNAP_COPY_FAILS.fetch_add(1, Ordering::Relaxed);
        if n < 16 || n % 512 == 0 {
            log_error!(
                "scanout-snapshot: WindowedBlt refused snapshot slot {} without exact allocation identity ({})",
                slot_index,
                counter_summary()
            );
        }
        return None;
    }

    // Copy direction: S_i <- the PRESENTED source resource — the same COM
    // identity publish_present_order names on the flip path.
    let src_raw = resource_com_raw(src_h);
    if src_raw == 0 {
        let n = SNAP_COPY_FAILS.fetch_add(1, Ordering::Relaxed);
        if n < 16 || n % 512 == 0 {
            log_error!(
                "scanout-snapshot: presented source has no COM resource ({})",
                counter_summary()
            );
        }
        return None;
    }
    // SAFETY: `dst_raw` is the ring's owned live ID3D11Resource (the ring
    // outlives this call — it is only torn down above, under the same
    // RefCell, or at device teardown); `src_raw` is the presented resource's
    // live COM pointer for the duration of this DDI call.
    match unsafe {
        dev.dxvk.present_snapshot_copy(
            DstRes(dst_raw),
            SrcRes(src_raw),
            purpose == SnapshotPurpose::WindowedBlt,
        )
    } {
        0 => {
            let n = SNAP_SUBSTITUTED.fetch_add(1, Ordering::Relaxed);
            // Steady-state cadence: first presents prove the arm is live, then
            // ~one line per 16k presents keeps the census visible without the
            // per-512 chatter of the bring-up build (T2's log-growth budget).
            if n < 2 || (n + 1) % 16384 == 0 {
                log_error!(
                    "scanout-snapshot present #{}: slot={} resid={} {}x{} pitch={} alloc={} ({})",
                    n + 1,
                    slot_index,
                    plan.resid,
                    plan.width,
                    plan.height,
                    plan.pitch,
                    plan.venus_alloc_size,
                    counter_summary()
                );
            }
            Some(plan)
        }
        1 => {
            // The bridge copied a min region: the ring matches the PRIVATE
            // geometry but the resource's own extent disagreed. The slot is
            // partially stale, so it must not be bound. The ring is KEPT —
            // its key still matches the private, so tearing it down would
            // rebuild an identical ring every present (a create storm); if
            // this state persists the counter and line below stay loud.
            let n = SNAP_COPY_FAILS.fetch_add(1, Ordering::Relaxed);
            if n < 16 || n % 512 == 0 {
                log_error!(
                    "scanout-snapshot: blit extent mismatch vs private {}x{} — \
                     substitution skipped ({})",
                    width,
                    height,
                    counter_summary()
                );
            }
            None
        }
        rc => {
            let n = SNAP_COPY_FAILS.fetch_add(1, Ordering::Relaxed);
            if n < 16 || n % 512 == 0 {
                log_error!(
                    "scanout-snapshot: blit FAILED rc={} slot={} resid={} ({})",
                    rc,
                    slot_index,
                    plan.resid,
                    counter_summary()
                );
            }
            None
        }
    }
}

/// The descriptor override, in `finish_present`, before BOTH consumers of the
/// owned local (`pPrivateDriverData` and the `HeliosPresentRenderCmd`).
///
/// The stale-descriptor guard, second leg: the plan only exists if the blit
/// was recorded this present (first leg, by construction), and here the
/// private data — re-read inside `finish_present` — must still describe the
/// geometry the blit was recorded against, or the override is refused. It
/// mutates the LOCAL only; `ResourceState::present_private` and
/// `direct_scanout_allocations` are rotation-coupled state and are never
/// touched.
pub(crate) fn apply_snapshot_override(
    present_private: &mut Option<HeliosPresentPrivateData>,
    plan: &SnapshotPlan,
) {
    if plan.purpose == SnapshotPurpose::WindowedBlt {
        // Windowed DXGI Present has no direct primary private record. Build a
        // fresh typed *source* descriptor, which is deliberately not marked
        // DIRECT_SCANOUT and can therefore never select scanout in the KMD.
        *present_private = Some(HeliosPresentPrivateData {
            plane_offset: plan.plane_offset,
            magic: HELIOS_PRESENT_PRIVATE_MAGIC,
            version: HELIOS_PRESENT_PRIVATE_VERSION,
            resource_id: plan.resid,
            width: plan.width,
            height: plan.height,
            pitch: plan.pitch,
            dxgi_format: plan.dxgi_format,
            reserved: HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT
                | HELIOS_PRESENT_PRIVATE_FLAG_WINDOWED_BLT_SNAPSHOT,
            venus_alloc_size: plan.venus_alloc_size,
            present_ctx_id: 0,
            present_value: 0,
            present_cookie: 0,
            snapshot_memory_type_index: plan.memory_type_index,
            snapshot_purpose: HELIOS_PRESENT_SNAPSHOT_PURPOSE_WINDOWED_BLT,
        });
        return;
    }
    let Some(private) = present_private.as_mut() else {
        let n = SNAP_PRIVATE_SKIPS.fetch_add(1, Ordering::Relaxed);
        if n < 16 || n % 512 == 0 {
            log_error!(
                "scanout-snapshot: blit recorded but the present carries no private data — \
                 override refused ({})",
                counter_summary()
            );
        }
        return;
    };
    if private.width != plan.width
        || private.height != plan.height
        || private.dxgi_format != plan.dxgi_format
    {
        let n = SNAP_PRIVATE_SKIPS.fetch_add(1, Ordering::Relaxed);
        if n < 16 || n % 512 == 0 {
            log_error!(
                "scanout-snapshot: private geometry moved under the present \
                 ({}x{} fmt={} vs plan {}x{} fmt={}) — override refused ({})",
                private.width,
                private.height,
                private.dxgi_format,
                plan.width,
                plan.height,
                plan.dxgi_format,
                counter_summary()
            );
        }
        return;
    }
    private.resource_id = plan.resid;
    private.width = plan.width;
    private.height = plan.height;
    private.pitch = plan.pitch;
    private.plane_offset = plan.plane_offset;
    private.dxgi_format = plan.dxgi_format;
    private.venus_alloc_size = plan.venus_alloc_size;
    private.reserved |= HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT;
    private.snapshot_memory_type_index = plan.memory_type_index;
    private.snapshot_purpose = HELIOS_PRESENT_SNAPSHOT_PURPOSE_NONE;
}
