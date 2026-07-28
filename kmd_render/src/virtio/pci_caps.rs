//! PCI capability walking: the host-visible blob window and the ISR-status
//! register, discovered from config space before any `VirtioGpu` state exists.
//!
//! Moved verbatim out of `virtio/gpu.rs` by T8/R1103. Every function here is
//! pure over `DxgkConfigAccess` and touches no transport state, which is what
//! the split makes a compile-time fact.

use helios_protocol::{
    VIRTIO_GPU_SHM_ID_HOST_VISIBLE, VIRTIO_PCI_CAP_ISR_CFG, VIRTIO_PCI_CAP_SHARED_MEMORY_CFG,
};
use virtio_drivers::transport::pci::bus::{ConfigurationAccess, DeviceFunction};

use super::config::DxgkConfigAccess;

// ── Host-visible window discovery (Gate 5a Stage 2) ─────────────────────────
// Ported from the proven System-class `kmd/src/virtio/gpu.rs`. The host-visible
// window is a prefetchable 64-bit PCI BAR (QEMU `hostmem=`) that
// `RESOURCE_MAP_BLOB` injects HOST3D blob mappings into. The WDDM Lock2 path
// reports this window as a CPU-visible memory segment so dxgkrnl/VidMm can map
// blobs to user space (there is no DxgkDdiLock; see GATE5_STAGE2_ALLOC_DESIGN.md).

const PCI_CFG_STATUS: u8 = 0x04; // command (low 16) | status (high 16)
const PCI_STATUS_CAP_LIST: u32 = 1 << 4; // status bit 4: capability list present
const PCI_CFG_CAP_PTR: u8 = 0x34; // first capability offset (low byte)
const PCI_CFG_BAR0: u8 = 0x10; // BAR0; BARn at 0x10 + n*4
const PCI_CAP_ID_VNDR: u32 = 0x09; // generic PCI vendor-specific capability id

/// The host-visible memory window discovered from the SHARED_MEMORY_CFG /
/// HOST_VISIBLE virtio capability.
#[derive(Clone, Copy)]
pub struct HostVisibleWindow {
    /// Guest-physical base of the window (BAR base + the cap's offset).
    pub base: u64,
    /// Window length in bytes (== QEMU `hostmem=`).
    pub len: u64,
}

/// Read 4 bytes of our device's PCI config space at `off` via the Dxgkrnl
/// config-space callback. `off` is held in a `u16` (like the System-class scan)
/// so the `cap + 20` cap-structure reads never overflow the `u8` arithmetic;
/// PCI config space is 256 bytes, so the `as u8` truncation is lossless.
/// One dword of PCI config space at `off`.
///
/// The parameter is a `u8` on purpose. It used to be a `u16` truncated with
/// `as u8` and a comment claiming the truncation was lossless — it is not, and
/// the capability walks below could produce an out-of-range offset: `cap` is
/// masked to `& 0xFC` and bounded only by `cap != 0` and a 48-iteration count,
/// never by an upper bound, so `cap + 20` with `cap >= 0xEC` WRAPPED to
/// `PCI_CFG_BAR0` (0x10) and `cap + 8` to `PCI_CFG_STATUS` (0x04). The device's
/// length would then be read out of a BAR register. Making the parameter `u8`
/// moves that arithmetic to the walks, where it can be checked.
fn cfg_read32(access: &DxgkConfigAccess, off: u8) -> u32 {
    access.read_word(
        DeviceFunction {
            bus: 0,
            device: 0,
            function: 0,
        },
        off,
    )
}

/// Bytes of a `virtio_pci_cap64` — the largest structure either capability walk
/// reads, at `cap + 20`.
const VIRTIO_PCI_CAP64_BYTES: u8 = 24;

/// Whether every field of a `virtio_pci_cap64` at `cap` fits in config space.
///
/// A capability whose header sits inside 256 bytes can still have its tail
/// outside it. Refusing such a capability (and counting it) is the whole point:
/// the alternative is a silent wrap onto an unrelated register.
fn cap_fits(cap: u8) -> bool {
    if cap > u8::MAX - VIRTIO_PCI_CAP64_BYTES {
        crate::diag::record_named_bytes(b"PciCapOob", u32::from(cap));
        return false;
    }
    true
}

/// Read the guest-physical base a memory BAR was assigned, handling the 64-bit
/// (type 0b10) layout the prefetchable host-visible window uses.
fn bar_base(access: &DxgkConfigAccess, bar: u16) -> Option<u64> {
    if bar > 5 {
        return None;
    }
    // bar <= 5, so reg <= 0x24 and reg + 4 <= 0x28 — both inside config space.
    let reg = PCI_CFG_BAR0 + (bar as u8) * 4;
    let lo = cfg_read32(access, reg);
    if lo & 0x1 != 0 {
        return None; // I/O-space BAR — not the memory window
    }
    let base = (lo & 0xFFFF_FFF0) as u64;
    // Memory BAR type in bits [2:1]: 0b10 == 64-bit (high half in BARn+1).
    if (lo >> 1) & 0x3 == 0x2 {
        Some(base | ((cfg_read32(access, reg + 4) as u64) << 32))
    } else {
        Some(base)
    }
}

/// Walk the PCI capability list for the virtio `SHARED_MEMORY_CFG` capability
/// whose shmid is `HOST_VISIBLE`, returning its guest-physical (base, length).
/// virtio-drivers' `PciTransport` ignores cap type 8, so we scan it ourselves.
/// Returns `None` if absent (a device built without blob/hostmem), which makes
/// the blob map path unavailable rather than crashing.
pub(super) fn scan_host_visible_window(access: &DxgkConfigAccess) -> Option<HostVisibleWindow> {
    if (cfg_read32(access, PCI_CFG_STATUS) >> 16) & PCI_STATUS_CAP_LIST == 0 {
        return None;
    }
    // Capability pointers are dword-aligned; mask the reserved low 2 bits.
    let mut cap = (cfg_read32(access, PCI_CFG_CAP_PTR) & 0xFF) as u8 & 0xFC;
    // Bounded walk — a corrupt cap_next cannot escape the 256-byte config space.
    for _ in 0..48 {
        if cap == 0 {
            break;
        }
        let d0 = cfg_read32(access, cap);
        let cap_id = d0 & 0xFF;
        let cap_next = ((d0 >> 8) & 0xFF) as u8 & 0xFC;
        let cfg_type = (d0 >> 24) & 0xFF;
        if cap_id == PCI_CAP_ID_VNDR && cfg_type == VIRTIO_PCI_CAP_SHARED_MEMORY_CFG as u32 {
            if !cap_fits(cap) {
                break;
            }
            // `virtio_pci_cap`: bar at +4 byte0, id (shmid) at +4 byte1.
            let d1 = cfg_read32(access, cap + 4);
            let bar = (d1 & 0xFF) as u16;
            let shmid = (d1 >> 8) & 0xFF;
            if shmid == VIRTIO_GPU_SHM_ID_HOST_VISIBLE as u32 {
                // `virtio_pci_cap64`: offset lo/hi at +8/+16, length lo/hi at +12/+20.
                let off = cfg_read32(access, cap + 8) as u64
                    | ((cfg_read32(access, cap + 16) as u64) << 32);
                let len = cfg_read32(access, cap + 12) as u64
                    | ((cfg_read32(access, cap + 20) as u64) << 32);
                let base = bar_base(access, bar)?;
                return Some(HostVisibleWindow {
                    base: base + off,
                    len,
                });
            }
        }
        cap = cap_next;
    }
    None
}

/// Walk the PCI capability list for the virtio `ISR_CFG` capability and map its
/// 1-byte ISR-status register, returning the mapped kernel VA (0 if absent).
///
/// This register is **read-to-clear**: reading it returns the pending-interrupt
/// bits (bit0 = used-ring/queue interrupt, bit1 = config change) and DEASSERTS
/// the device's level-triggered INTx line. `DxgkDdiInterruptRoutine` reads it at
/// DIRQL to acknowledge the line (the device is line-based INTx — `MSISupported=0`
/// — so without this read the line stays high and Windows' interrupt-storm
/// detector disables the adapter → Code 43). virtio-drivers' `PciTransport`
/// owns this register internally and never exposes its VA, and its `ack_interrupt`
/// needs `&mut self` (the queue lock) which the ISR cannot take at DIRQL — so we
/// locate and map the register ourselves and read it lock-free.
pub(super) fn map_isr_status_register(access: &DxgkConfigAccess) -> usize {
    if (cfg_read32(access, PCI_CFG_STATUS) >> 16) & PCI_STATUS_CAP_LIST == 0 {
        return 0;
    }
    let mut cap = (cfg_read32(access, PCI_CFG_CAP_PTR) & 0xFF) as u8 & 0xFC;
    for _ in 0..48 {
        if cap == 0 {
            break;
        }
        let d0 = cfg_read32(access, cap);
        let cap_id = d0 & 0xFF;
        let cap_next = ((d0 >> 8) & 0xFF) as u8 & 0xFC;
        let cfg_type = (d0 >> 24) & 0xFF;
        if cap_id == PCI_CAP_ID_VNDR && cfg_type == VIRTIO_PCI_CAP_ISR_CFG as u32 {
            if !cap_fits(cap) {
                break;
            }
            // `virtio_pci_cap`: bar at +4 byte0; offset (u32) at +8.
            let bar = (cfg_read32(access, cap + 4) & 0xFF) as u16;
            let offset = cfg_read32(access, cap + 8) as u64;
            let Some(base) = bar_base(access, bar) else {
                return 0;
            };
            let phys = base + offset;
            // SAFETY: maps a real device BAR sub-region (the ISR-status register)
            // at PASSIVE_LEVEL via the shared MMIO cache; non-cached MMIO.
            //
            // try_mmio_map, NOT the infallible Hal method: that one returns
            // NonNull::dangling() (address 0x1) on failure, which this function
            // would convert to a nonzero usize and return as a valid VA. `init`
            // would then record the SUCCESS breadcrumb and read_volatile(1) to
            // clear the register - a fault at PASSIVE inside StartDevice, where
            // the documented degrade path 0x0B00_00E6 already existed.
            let va =
                unsafe { crate::virtio::hal::try_mmio_map(phys as virtio_drivers::PhysAddr, 16) };
            return match va {
                Some(p) => p.as_ptr() as usize,
                None => {
                    // Distinct from "no ISR cap present" (0x0B00_00E6): the cap
                    // is there and we failed to map it. 0x0B00_00E0/E5/E6/E7/E8
                    // are all taken.
                    crate::diag::record(0x0B00_00E9);
                    0
                }
            };
        }
        cap = cap_next;
    }
    0
}
