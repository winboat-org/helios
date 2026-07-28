//! The one-shot Present destination probe, behind the `PresentProbe` knob.
//!
//! Moved verbatim out of `virtio/venus.rs` by T8/R1104. The review expected
//! this module to be ~75 lines and it is ~105: `gpu_clear_scanout_image`,
//! `allocate_scanout_image_blob` and the `diag_scanout_*` atomics were already
//! deleted by T6, exactly as predicted, and what is left is the probe plus its
//! `take_pending_probe` handoff.

use super::ring::*;
use super::*;

impl VenusClient {
    /// Take the armed one-shot probe, if any. Called under the venus mutex by
    /// the PASSIVE worker, which then runs the probe outside it.
    pub fn take_pending_probe(&mut self) -> Option<(PresentBufferDesc, u64)> {
        self.probe_pending.take()
    }

    /// One-shot diagnostic proving whether the completed Vulkan copy populated
    /// dxgkrnl's host-visible Present destination. This is deliberately not a
    /// correctness mechanism: all failures are loud breadcrumbs, and the
    /// caller's per-pair `probe_done` bit prevents retry loops.
    ///
    /// PASSIVE_LEVEL, and MUST NOT be called with the venus mutex held.
    pub(crate) fn probe_present_destination(
        passive: PassiveLevel,
        adapter: &AdapterContext,
        destination: PresentBufferDesc,
        fence_id: u64,
    ) {
        crate::diag::record_named_bytes(b"PBPrF", 1);
        match ctrl::wait_fence(passive, adapter, fence_id, 5_000_000_000) {
            ctrl::WaitFenceOutcome::Complete => {
                crate::diag::record_named_bytes(b"PBPrF", 2);
            }
            ctrl::WaitFenceOutcome::TimedOut => {
                crate::diag::record_named_bytes(b"PBPrF", 0xE1);
                return;
            }
            ctrl::WaitFenceOutcome::Invalid => {
                crate::diag::record_named_bytes(b"PBPrF", 0xE2);
                return;
            }
        }

        let prep = match ctrl::map_blob_prepare(
            passive,
            adapter,
            crate::virtio::gpu::OwnerFilter::Any,
            destination.resource_id,
        ) {
            Ok(prep) => prep,
            Err(_) => {
                crate::diag::record_named_bytes(b"PBPrF", 0xE3);
                return;
            }
        };
        if prep.size < destination.allocation_size {
            crate::diag::record_named_bytes(b"PBPrF", 0xE4);
            return;
        }
        let map = match KernelMap::new(prep.gpa, prep.size, prep.map_cache) {
            Some(map) => map,
            None => {
                crate::diag::record_named_bytes(b"PBPrF", 0xE5);
                return;
            }
        };
        crate::diag::record_named_bytes(b"PBPrF", 3);

        const GRID: u64 = 8;
        let bytes_per_pixel = u64::from(destination.pixel_format().bytes_per_pixel());
        let mut nonblack = 0u32;
        let mut rgb_sum = 0u32;
        for gy in 0..GRID {
            let y = u64::from(destination.height - 1) * gy / (GRID - 1);
            for gx in 0..GRID {
                let x = u64::from(destination.width - 1) * gx / (GRID - 1);
                let offset = y * u64::from(destination.pitch) + x * bytes_per_pixel;
                // PresentBufferDesc::new proves the complete pitched image is
                // within allocation_size, and prep.size covers that allocation —
                // but that is an argument spread over two files, so take the
                // checked read and breadcrumb the refusal instead.
                let (Some(b), Some(g), Some(r)) = (
                    map.read_byte(offset),
                    map.read_byte(offset + 1),
                    map.read_byte(offset + 2),
                ) else {
                    crate::diag::record_named_bytes(b"PBPrF", 0xE6);
                    return;
                };
                let pixel_sum = u32::from(r) + u32::from(g) + u32::from(b);
                rgb_sum = rgb_sum.saturating_add(pixel_sum);
                nonblack += u32::from(pixel_sum != 0);
            }
        }
        let center_offset = u64::from(destination.height / 2) * u64::from(destination.pitch)
            + u64::from(destination.width / 2) * bytes_per_pixel;
        let (Some(c0), Some(c1), Some(c2), Some(c3)) = (
            map.read_byte(center_offset),
            map.read_byte(center_offset + 1),
            map.read_byte(center_offset + 2),
            map.read_byte(center_offset + 3),
        ) else {
            crate::diag::record_named_bytes(b"PBPrF", 0xE6);
            return;
        };
        let center = u32::from(c0)
            | (u32::from(c1) << 8)
            | (u32::from(c2) << 16)
            | (u32::from(c3) << 24);
        crate::diag::record_named_bytes(b"PBPrNz", nonblack);
        crate::diag::record_named_bytes(b"PBPrSum", rgb_sum);
        crate::diag::record_named_bytes(b"PBPrCtr", center);
        crate::diag::record_named_bytes(b"PBPrF", 0x10);
    }
}
