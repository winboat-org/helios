//! The raw KMD Venus allocation contract. Unlike Mesa's ICD, this path has no
//! buffer handle-type normalization between the guest request and the renderer.

use crate::{ImageCreateSpec, ImagePNext, MemoryAllocateSpec, MemoryPNext, Writer};

/// The renderer's HOST_VISIBLE export and Mesa's matching buffer import use
/// DMA_BUF. Requesting OPAQUE_FD as well can exceed compatibleHandleTypes.
pub const DMA_BUF: u32 = 0x200;

pub fn linear_scanout_image(width: u32, height: u32) -> ImageCreateSpec {
    ImageCreateSpec {
        pnext: ImagePNext::ExternalMemory {
            handle_type: DMA_BUF,
        },
        flags: 0,
        format: 44, // B8G8R8A8_UNORM
        width,
        height,
        tiling: crate::IMAGE_TILING_LINEAR,
        usage: 3, // TRANSFER_SRC | TRANSFER_DST
        // VUID-VkImageCreateInfo-pNext-01443: external images start UNDEFINED.
        // The KMD must initialize/release it before exposing CPU-writable memory.
        initial_layout: 0,
    }
}

pub fn encode_present_buffer(device: u64, buffer: u64, size: u64) -> Writer {
    let mut w = Writer::new();
    w.header(50, crate::CMD_FLAG_GENERATE_REPLY); // vkCreateBuffer
    w.u64(device);
    w.count(true);
    w.i32(12); // VkBufferCreateInfo
    w.count(true);
    w.i32(1000072000); // VkExternalMemoryBufferCreateInfo
    w.count(false);
    w.u32(DMA_BUF);
    w.u32(0); // flags
    w.u64(size);
    w.u32(3); // TRANSFER_SRC | TRANSFER_DST, matching the Mesa/DXVK importer
    w.u32(crate::SHARING_MODE_EXCLUSIVE);
    w.u32(0); // queueFamilyIndexCount
    w.count(false);
    w.count(false); // allocator
    w.count(true);
    w.u64(buffer);
    w
}

pub fn present_buffer_memory(buffer: u64, size: u64, memory_type_index: u32) -> MemoryAllocateSpec {
    MemoryAllocateSpec {
        pnext: MemoryPNext::ExportDedicatedBuffer {
            handle_type: DMA_BUF,
            buffer,
        },
        size,
        memory_type_index,
    }
}

/// Only the initial GPU submission's lifetime; Ready does NOT retire later
/// copy/scanout consumers. Their existing completion guards still apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitialImageAccess {
    Pending,
    Ready,
    ImageDestroyed,
}

impl InitialImageAccess {
    /// Call only after the initialization submission's real Vulkan fence wait
    /// succeeds. A successful queue-submit reply is insufficient.
    pub fn fence_completed(&mut self) {
        if *self == Self::Pending {
            *self = Self::Ready;
        }
    }

    pub const fn may_publish(self) -> bool {
        matches!(self, Self::Ready)
    }

    pub const fn may_destroy_image(self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Call only after the image destruction command was accepted.
    pub fn image_destroyed(&mut self) {
        if self.may_destroy_image() {
            *self = Self::ImageDestroyed;
        }
    }

    pub const fn may_free_memory(self) -> bool {
        matches!(self, Self::ImageDestroyed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn buffer_create_and_dedicated_export_have_only_dma_buf() {
        let create = encode_present_buffer(0x1111, 0x2222, 0x3000);
        let create = create.finished().unwrap();
        // Offsets from vn_protocol_driver_buffer.h, including 64-bit pointer tags.
        assert_eq!(word(create, 0), 50);
        assert_eq!(word(create, 36), 1000072000);
        assert_eq!(word(create, 48), 0x200);
        assert_eq!(word(create, 56), 0x3000);
        assert_eq!(word(create, 64), 3);
        let alloc = crate::encode_memory_allocate(
            0x1111,
            0x4444,
            &present_buffer_memory(0x2222, 0x3000, 7),
        );
        let alloc = alloc.finished().unwrap();
        assert_eq!(word(alloc, 36), 1000072002); // export
        assert_eq!(word(alloc, 48), 1000127001); // dedicated
        assert_eq!(word(alloc, 68), 0x2222); // dedicated buffer
        assert_eq!(word(alloc, 76), 0x200); // no OPAQUE_FD | DMA_BUF union
        assert_eq!(word(alloc, 80), 0x3000);
    }

    #[test]
    fn ambiguous_submit_or_failed_wait_cannot_publish_or_reclaim() {
        let mut state = InitialImageAccess::Pending;
        // Neither failure supplies completion evidence. Even an attempted
        // teardown must not allow freeing a possibly GPU-owned allocation.
        state.image_destroyed();
        assert!(!state.may_publish());
        assert!(!state.may_destroy_image());
        assert!(!state.may_free_memory());
    }

    #[test]
    fn completed_setup_still_requires_image_destruction_before_memory_free() {
        let mut state = InitialImageAccess::Pending;
        state.fence_completed();
        assert!(state.may_publish());
        assert!(state.may_destroy_image());
        assert!(!state.may_free_memory());
        state.image_destroyed();
        assert!(state.may_free_memory());
        assert!(!state.may_publish());
        assert!(!state.may_destroy_image());
        state.fence_completed(); // stale completion cannot resurrect an image
        assert!(state.may_free_memory());
        assert!(!state.may_publish());
    }
}
