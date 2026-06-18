//! helios_protocol — shared wire-format definitions for the Helios vGPU stack.
//!
//! This crate is the single source of truth for every byte that crosses a
//! trust/ABI boundary in Helios:
//!
//!   ICD (user-mode)  --DeviceIoControl(IOCTL)-->  KMD (kernel-mode, no_std)
//!   KMD              --virtqueue-->               virtio-gpu device / virglrenderer
//!
//! The `helios_kmd` crate depends on this crate so the IOCTL payload structs,
//! the IOCTL codes/GUID, and the virtio-gpu command structs can never drift
//! apart; the (C, Mesa-venus) ICD mirrors the same constants. It is
//! `#![no_std]` so the kernel-mode KMD can use it.
//!
//! References:
//!   - TRANSPORT.md (this repo) — escape protocol + virtio-gpu layouts
//!   - KMD.md Phase 2 — virtio-gpu command structs
//!   - VirtIO 1.2 spec §5.7 (GPU Device):
//!     https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html#sec-gpu

#![no_std]
#![allow(non_camel_case_types, non_upper_case_globals)]

pub mod virtio_gpu;
pub mod escape;
pub mod features;
pub mod ioctl;
pub mod wddm;

pub use escape::*;
pub use features::*;
pub use ioctl::*;
pub use virtio_gpu::*;
pub use wddm::*;
