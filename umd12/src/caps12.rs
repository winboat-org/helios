//! L1 — the caps gauntlet: 43 `D3D12DDICAPS_TYPE` answers plus 3 device-core
//! format/MSAA query slots.
//!
//! ⚠ **S6-0: this lane has not landed.** `adapter12::get_caps` refuses every
//! type with a named counter, and the three device-core slots carry
//! `forward12::noop12`'s per-slot counting noops. `install` below is a live link
//! in the sequencer's chain; what is empty is its body.
//!
//! # ⛔ Why this lane is one agent's, whole, and why it comes first
//!
//! `PARALLEL.md` §8: `D3D12Core.dll` enforces **~60 cross-tier consistency
//! rules** and states them in English on ETW — *"Driver did not respond to
//! `D3D12DDICAPS_TYPE_D3D12_OPTIONS` caps query."*, *"Drivers that support
//! raytracing must expose shader model 6.3."* Splitting caps across agents is
//! how two individually-plausible tiers become a rejected device. And
//! `DECISIONS.md` §7.8: **advertising a capability that is not backed is a lie
//! the OS acts on.**
//!
//! It comes first because **caps decide whether a device is created at all** —
//! and on Helios that is measured, not inferred. S5's knob-ON run
//! (`tmp/dx12/gates/G6/RESULT.md`) shows the whole negotiation:
//!
//! ```text
//! OpenAdapter12 -> GetCaps(1074) -> GetCaps(1007)
//!               -> GetSupportedVersions(0, NULL) -> GetSupportedVersions(1, buf)
//!               -> CloseAdapter
//! ```
//!
//! Refusing 1074 aborts device creation two calls in — before
//! `pfnGetOptionalDDITables`, before `pfnFillDDITable`, before
//! `pfnCalcPrivateDeviceSize`. ⇒ **`D12-G7` is not reachable until this lane
//! lands**, whatever the rest of the table does.
//!
//! ⭐ **Two things that run measured against the doc set, and the second is a
//! correction:**
//!
//! 1. **1074 first, 1007 as the fallback.** `D3D12DDICAPS_TYPE_0081_3DPIPELINESUPPORT1`
//!    is asked before `D3D12DDICAPS_TYPE_3DPIPELINESUPPORT`, both with
//!    `pInfo = NULL` and `DataSize` 8 and 4. `DX12.md` §4.3 row 2: an
//!    unimplemented 1074 **silently caps Helios at FL 12_1 with no error
//!    anywhere**, because 1007 may never answer above 12_1.
//! 2. ⛔ **`pfnGetCaps` runs BEFORE `pfnGetSupportedVersions`** — the opposite of
//!    `ARCHITECTURE.md` §1.2's original step order, corrected there from this
//!    measurement. ⇒ **the caps answer cannot depend on a negotiated interface
//!    version, because at `pfnGetCaps` time there is not one.** A `caps12` that
//!    branches on the revision is reading a value the runtime has not supplied.
//!
//! # The three that must be pinned conservatively from commit 1
//!
//! `DDI_REFERENCE.md` §11.6, and one of them is a `GATES.md` D12-G7 counter
//! criterion rather than a preference:
//!
//! * **`HARDWARE_SCHEDULING_CAPS_0050.ComputeQueuesPer3DQueue = 0`.** A D3D12
//!   device must never reach `DxgkDdiCreateHwQueue`, which the KMD refuses at
//!   `kmd_render/src/ddi/scheduler.rs:180-187`; `HwQRef` not moving is G7's
//!   evidence that it did not.
//! * **`TiledResourcesTier` clamped, explicitly.** vkd3d reports 4, the DDI enum
//!   stops at 3, and an out-of-range tier is **clamped silently** — so without
//!   an explicit clamp Helios ships a number nobody chose, which is CLAUDE.md
//!   rule 8 in its purest form (`DX12.md` §4.3 row 3).
//! * **`EnhancedBarriersSupported = 0`** until L3c's `pfnBarrier` is real. At 1
//!   the runtime stops invoking the legacy barrier DDIs entirely
//!   (`DX12.md` §4.3 row 1) — there is no fallback left.
//!
//! ⚠ And a substrate ceiling this lane must not walk into:
//! `MaxSamplerDescriptorHeapSize` must be **≥ 4000** at 0102+, while the host
//! GPU's `maxSamplerAllocationCount` is **exactly 4000** — zero headroom if
//! vkd3d allocates one `VkSampler` per descriptor. `GATES.md` §7.24 owns it and
//! wants an answer **before** this lane commits to a number.

use crate::forward12::tables12::{stage, DeviceCoreTable, Filling};

/// Install L1's 3 device-core slots: `pfnCheckFormatSupport`,
/// `pfnCheckMultisampleQualityLevels`, `pfnGetMipPacking`.
///
/// Chain position: `Stubbed` -> `CapsSlots` on the device-core table — first,
/// because caps decide everything downstream.
pub(crate) fn install(
    mut filling: Filling<'_, DeviceCoreTable, stage::Stubbed>,
) -> Filling<'_, DeviceCoreTable, stage::CapsSlots> {
    // Touching the table here is what makes the borrow real rather than a
    // formality, and it is what a landing lane replaces with typed field
    // assignments: `f.pfnCheckFormatSupport = Some(check_format_support);`,
    // each checked by the compiler against the bindgen signature.
    let _table = filling.table();
    filling.advance()
}
