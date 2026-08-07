//! D3D11 device object + device-funcs table fill (Gate 5b, Milestone 1).
//!
//! The OS D3D11 runtime drives `D3D11CreateDevice` into our adapter `CreateDevice`
//! DDI, which must hand back a fully-populated `D3D11DDI_DEVICEFUNCS` table (152
//! entries) and return S_OK. A null entry the runtime calls = crash, so we fill
//! **all** entries with a safe stub and specialise the few whose return value
//! matters. This is the minimal honest device that lets the runtime accept the
//! device (Milestone 1 = D3D11CreateDevice S_OK → DWM stops fail-fasting); real
//! rendering DDIs come later, backed by the DXVK device this object holds.
//!
//! ABI note: every device DDI takes `D3D10DDI_HDEVICE` (one pointer) as its first
//! arg and the x64 calling convention is caller-clean, so a uniform
//! `extern "C" fn(usize) -> usize` stub transmuted into each slot reads only the
//! first arg, ignores the rest, and returns in RAX — valid for the void / HRESULT
//! / SIZE_T return shapes alike.

use crate::bridge;
use crate::ddi;
use crate::log_error;
use core::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use windows::core::{IUnknown, Interface};

/// One cached dcomp-vehicle present source (road 4): an alias-imported D3D11
/// texture over the producing ICD's frame blob, keyed by venus resid. Owns
/// one COM ref on the imported resource, released on drop (eviction,
/// geometry change, or device teardown).
pub struct PresentSrcEntry {
    pub resid: u32,
    pub width: u32,
    pub height: u32,
    pub dxgi_format: u32,
    /// Owned `ID3D11Resource` COM pointer from `open_ddi_texture2d`.
    pub resource_raw: usize,
}

impl Drop for PresentSrcEntry {
    fn drop(&mut self) {
        if self.resource_raw != 0 {
            // SAFETY: `resource_raw` is the owned COM ref returned by
            // open_ddi_texture2d; from_raw adopts it so drop releases it.
            unsafe {
                drop(
                    windows::Win32::Graphics::Direct3D11::ID3D11Resource::from_raw(
                        self.resource_raw as *mut c_void,
                    ),
                );
            }
        }
    }
}

/// One slot of the D4b snapshot ring (FIX-DESIGN-d4b-snapshot.md §3): an
/// ICD-owned DirectOptimalScanout OPTIMAL image the present-time blit fills
/// from the presented primary, plus the identity the KMD binds it by. NO
/// runtime object and NO WDDM allocation exist for it — the KMD needs only
/// the resid + descriptor, and ownership is never transferred (the snapshot
/// stays an ICD-owned blob). Owns one COM ref on the created resource,
/// released on drop (ring recreate or `BridgeOwned::release`).
pub struct SnapshotSlot {
    /// Owned `ID3D11Resource` COM pointer from `create_ddi_scanout_texture2d`.
    pub resource_raw: usize,
    /// Venus resource id the KMD binds (`dxvk_resource_memory_info`).
    pub resid: u32,
    /// Logical scanout row pitch, from the same create outparam the primary
    /// path records into its `ScanoutGeometry`.
    pub pitch: u32,
    /// Memory-plane-0 offset, same source as `pitch`.
    pub plane_offset: u64,
    /// Venus blob size backing `resid`, for the KMD's bind-time undersize
    /// guard (`alloc_size >= plane_offset + pitch*height`).
    pub alloc_size: u64,
    /// Exact allocation memory type. Direct scanout never consumes this field,
    /// but a WindowedBlt snapshot imports the same image in the KMD Venus
    /// context and must not guess it from a resource id.
    pub memory_type_index: u32,
    /// `get_resource_alloc_identity` completed successfully. Zero is a valid
    /// memory-type index, so the value itself cannot prove this fact.
    pub alloc_identity_known: bool,
}

impl Drop for SnapshotSlot {
    fn drop(&mut self) {
        if self.resource_raw != 0 {
            // SAFETY: `resource_raw` is the owned COM ref adopted from
            // create_scanout_texture2d and handed over with into_raw;
            // from_raw re-adopts it so drop releases it.
            unsafe {
                drop(
                    windows::Win32::Graphics::Direct3D11::ID3D11Resource::from_raw(
                        self.resource_raw as *mut c_void,
                    ),
                );
            }
        }
    }
}

/// One D4b snapshot ring: 4 slots rotated per present for one exact geometry.
/// Never handed to `dxgi_rotate_resource_identities` — its list is
/// `a.pResources` only, so the ring is naturally excluded.
pub struct SnapshotRing {
    pub width: u32,
    pub height: u32,
    pub dxgi_format: u32,
    /// Exactly [`crate::forward::SNAPSHOT_RING_SLOTS`] entries once built.
    pub slots: Vec<SnapshotSlot>,
    /// Next slot index to rotate into.
    pub next: usize,
}

/// Device-local D4b rings, one per concurrently presented geometry.
///
/// Snapshot descriptors cross into the KMD as raw Venus resource IDs. There
/// is no WDDM allocation reference in that descriptor which could make a
/// geometry-change eviction scheduler-safe, so a ring that has ever been
/// published stays alive until device teardown. The forward path enforces a
/// hard count/byte budget and seals the cache when a new ring would exceed it;
/// later unknown geometries fail closed to the ordinary present path.
#[derive(Default)]
pub struct SnapshotRingCache {
    pub rings: Vec<SnapshotRing>,
    pub bytes: u64,
    pub sealed: bool,
}

/// WDDM 2.x paging queue used to order explicit residency operations.
///
/// The non-zero handles and non-null monitored-fence mapping are validated at
/// construction, so any allocation carrying a residency reference can rely on
/// this queue being usable without repeating raw-handle checks.
#[derive(Clone, Copy)]
pub struct RuntimePagingQueue {
    pub handle: core::num::NonZeroU32,
    pub sync_object: core::num::NonZeroU32,
    pub fence_value_cpu: core::ptr::NonNull<u64>,
}

/// One runtime-owned buffer window: a pointer and the capacity that describes
/// it, which are only ever meaningful together.
///
/// Pre-R808 these were six independent `Cell`s (`command_buffer` +
/// `command_buffer_size`, `allocation_list` + `allocation_list_size`,
/// `patch_list` + `patch_list_size`), so a pointer could be updated without its
/// size. Pairing them makes that unrepresentable.
pub struct Window<T> {
    pub ptr: core::ptr::NonNull<T>,
    pub capacity: u32,
}

// Hand-written rather than derived: `derive` would add a `T: Copy` bound, and
// the pointee types here (c_void, the DDI list structs) are not Copy. A window
// is a pointer and an integer; copying it never copies a `T`.
impl<T> Clone for Window<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Window<T> {}

impl<T> Window<T> {
    /// `None` for a null pointer, which is how the runtime spells "no window".
    /// A zero capacity is retained rather than rejected: `pfnRenderCb` returns
    /// pointer/size pairs and only the pointer decides presence, exactly as the
    /// pre-R808 `is_null()` checks did.
    pub fn new(ptr: *mut T, capacity: u32) -> Option<Self> {
        Some(Self {
            ptr: core::ptr::NonNull::new(ptr)?,
            capacity,
        })
    }
}

/// The kernel context every present path submits through, plus the three
/// runtime-owned buffer windows that arrive with it.
///
/// Seven fields used to become meaningful together or not at all, depending on
/// one `hr` the caller never saw — `create_runtime_context` returned unit — and
/// every consumer re-derived the invariant by hand with its own
/// `h_context.is_null()` test. `RuntimePagingQueue`, twenty lines above, already
/// demonstrated the fix and even documented it; the context group was never
/// converted. R808.
///
/// Stored as `Option<RuntimeContext>` exactly like `paging_queue`, so "context
/// exists" is one check that yields a value in which a pointer and its capacity
/// can never disagree.
pub struct RuntimeContext {
    pub handle: core::ptr::NonNull<c_void>,
    /// Legacy command buffer recycled by `pfnRenderCb`.
    pub command: core::cell::Cell<Option<Window<c_void>>>,
    pub allocations: core::cell::Cell<Option<Window<ddi::D3DDDI_ALLOCATIONLIST>>>,
    pub patches: core::cell::Cell<Option<Window<ddi::D3DDDI_PATCHLOCATIONLIST>>>,
}

/// Every COM object this device holds that came OUT of the bridge.
///
/// `HeliosDevice` used to state an ownership rule — "declared before `dxvk` so
/// entries release their D3D11 textures before the bridge device drops" — and
/// enforce it with field declaration order for exactly ONE of its four
/// COM-owning fields. `scanout_import`, `composition_source` and `ia` all sat
/// *after* `dxvk`, so all three dropped after the bridge `UniquePtr` had run
/// `~HeliosDxvkDeviceImpl`. `ia` was patched around explicitly in
/// `ddi_destroy_device`; the other two were not.
///
/// Nothing has crashed because DXVK's `D3D11DeviceChild` holds a strong parent
/// reference, so a surviving child keeps the device alive. The concrete defect
/// is that the stated invariant was false, and the next COM-owning field added
/// below `dxvk` would inherit an unreviewed assumption — a bridge-derived
/// object that does NOT take a parent reference (a raw Vulkan-side handle, or a
/// DXGI object owned by the impl) would touch freed state in its `Drop`.
///
/// Grouping them into one field placed before `dxvk` encodes the rule for all
/// present and future members at once. [`BridgeOwned::release`] then makes
/// correctness not depend on drop order at all; `Drop` remains as the
/// rollback/panic path. R807.
pub struct BridgeOwned {
    /// Dcomp present-vehicle source cache. Immediate-path-only RefCell: only
    /// present-path DDIs touch it, and those stay runtime-serialized with the
    /// rest of the immediate context even under FREETHREADED caps.
    pub present_src_cache: core::cell::RefCell<Vec<PresentSrcEntry>>,
    /// D4b rings, keyed by geometry and retained until device teardown. Same
    /// immediate-path-only RefCell contract as `present_src_cache`.
    pub snapshot_rings: core::cell::RefCell<SnapshotRingCache>,
    /// Device-global shader/layout caches for lazy `ID3D11InputLayout`
    /// creation. The d3d10umddi `CreateElementLayout` DDI does NOT pass the
    /// vertex-shader input-signature bytecode that
    /// `ID3D11Device::CreateInputLayout` requires, so we stash the element
    /// descs + the bound VS bytecode and create the layout lazily at draw.
    ///
    /// Mutex, not RefCell: create/destroy DDIs mutate these from any thread
    /// once FREETHREADED caps are reported, concurrently with draw-path
    /// lookups. Lock via [`BridgeOwned::caches_lock`]; never hold the guard
    /// across a bridge/COM call (COM releases and creates run arbitrary DXVK
    /// code).
    pub caches: std::sync::Mutex<ShaderCaches>,
    /// Immediate-context pipeline binding shadow. Per-context state — each
    /// deferred context gets its own copy when command lists land.
    pub bindings: CtxBindings,
}

impl BridgeOwned {
    pub fn new() -> Self {
        Self {
            present_src_cache: core::cell::RefCell::new(Vec::new()),
            snapshot_rings: core::cell::RefCell::new(SnapshotRingCache::default()),
            caches: std::sync::Mutex::new(ShaderCaches::default()),
            bindings: CtxBindings::default(),
        }
    }

    /// Lock the shader/layout caches, ignoring poison: with `panic = "abort"`
    /// in both profiles no unwind can ever poison the mutex, and a DDI must
    /// never panic on lock regardless.
    pub fn caches_lock(&self) -> std::sync::MutexGuard<'_, ShaderCaches> {
        self.caches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Release every bridge-derived COM object, explicitly and in order, while
    /// the bridge device is still alive.
    ///
    /// Returns the shader caches' `(variants, layouts)` so the existing
    /// `DDI DestroyDevice: released IA cache variants=N layouts=M` line keeps
    /// its counts verbatim.
    ///
    /// Must be called on EVERY teardown path — `ddi_destroy_device` and the
    /// `CreateDevice` rollback. If it is missed, the refs do not leak forever,
    /// but they move from "released here" to "released whenever the field
    /// drops", which is the ordering this type exists to stop depending on.
    pub fn release(&mut self) -> (usize, usize) {
        // Order: present caches first, shader caches last, matching the
        // pre-R807 sequence where `ia` was the field released explicitly. The
        // Snapshot rings are a cache too: dropping the slots releases their
        // COM refs, and that is ALL their teardown — no WDDM handles or KMD
        // registrations to unwind. This happens only at device teardown;
        // mid-device eviction is forbidden because the KMD carries snapshot
        // identities by value and may consume them after Present returns.
        self.present_src_cache.get_mut().clear();
        *self.snapshot_rings.get_mut() = SnapshotRingCache::default();
        self.bindings.bound_vs_com.store(0, Ordering::Relaxed);
        match self.caches.get_mut() {
            Ok(caches) => caches.release_owned_com(),
            Err(poisoned) => poisoned.into_inner().release_owned_com(),
        }
    }
}

/// `D3D11DDICAPS_FREETHREADED` (d3d10umddi.h; NOT in the bindgen output —
/// the cap *bit* macros are preprocessor defines bindgen's allowlist misses).
pub const D3D11DDICAPS_FREETHREADED: u32 = 0x1;
/// `D3D11DDICAPS_COMMANDLISTS` (deprecated at BUILD_VERSION >= 2) and
/// `D3D11DDICAPS_COMMANDLISTS_BUILD_2`. Declared ONLY for the compile-time
/// guarantee below; nothing may report them until the R812 slots are real.
pub const D3D11DDICAPS_COMMANDLISTS: u32 = 0x2;
pub const D3D11DDICAPS_COMMANDLISTS_BUILD_2: u32 = 0x4;

/// Every THREADING cap this driver can EVER report with the current slot
/// implementations. Phase C widened this together with the real deferred-
/// context/command-list slots — the pairing the R811/R812 assert existed to
/// force. The deprecated COMMANDLISTS (0x2) bit stays impossible; BUILD_2
/// devices report only 0x1|0x4.
pub const THREADING_CAPS_POSSIBLE: u32 =
    D3D11DDICAPS_FREETHREADED | D3D11DDICAPS_COMMANDLISTS_BUILD_2;

/// `D3D11DDI_THREADING_CAPS::Caps`, the value `get_caps` reports.
///
/// Phase B (`tmp/handoff-perf-structural/PLAN-commandlists.md`): FREETHREADED
/// when the `UmdFreeThreaded` knob is on (absent = ON; explicit 0 is the kill
/// switch), else 0. The runtime then calls create/destroy/calc DDIs from any
/// thread, concurrent with the immediate context — the state that exposes
/// went thread-safe in Phase A ([`ShaderCaches`] mutex, [`CtxBindings`]
/// atomics, `direct_scanout_allocations` mutex). The present-path `RefCell`s
/// ([`BridgeOwned::present_src_cache`], [`BridgeOwned::snapshot_rings`]) and
/// the [`RuntimeContext`] window `Cell`s remain sound: present and the other
/// immediate-context DDIs stay runtime-serialized under FREETHREADED.
///
/// Phase C: |= COMMANDLISTS_BUILD_2 when `UmdCommandLists` is also on (the
/// knob accessor itself forces it off without FREETHREADED — COMMANDLISTS
/// requires FREETHREADED per the WDK). The deferred-context/command-list
/// slots this invites the runtime to call are REAL and installed
/// unconditionally by `install_calc_and_lifecycle`/`forward::install`, so the
/// R812 hazard (a 256-byte calc stub paired with a live Create) is gone
/// structurally: caps only decide whether the runtime USES the slots, never
/// whether they are safe to call.
pub fn threading_caps() -> u32 {
    if !crate::umd_free_threaded() {
        return 0;
    }
    let mut caps = D3D11DDICAPS_FREETHREADED;
    if crate::umd_command_lists() {
        caps |= D3D11DDICAPS_COMMANDLISTS_BUILD_2;
    }
    caps
}

/// First word of every object this driver constructs behind a
/// `D3D10DDI_HDEVICE`'s `pDrvPrivate`. A deferred context IS an HDEVICE at
/// the DDI level (there is no pfnDestroyContext — DCs are destroyed through
/// pfnDestroyDevice), so once command lists exist two different types share
/// one handle namespace and every resolver must discriminate BEFORE casting.
/// Full-word magic values, not small enums: a stray private block cannot
/// alias a valid tag by accident.
pub const HELIOS_TAG_DEVICE: usize = 0x4845_4C49_4F44_4556; // "HELIODEV"
pub const HELIOS_TAG_DEFERRED: usize = 0x4845_4C49_4F44_4643; // "HELIODFC"

/// `D3D10DDI_HDEVICE` handles whose private block carried neither tag.
/// Count + refuse, never cast: a wild cast here is the worst-risk failure of
/// the whole deferred-context feature (`ddi_destroy_device` tearing a device
/// down through a DC pointer, or vice versa).
pub static DEVICE_TAG_MISMATCH: AtomicUsize = AtomicUsize::new(0);

/// Bump the mismatch counter and log its first hits.
pub(crate) fn note_device_tag_mismatch(site: &str, tag: usize) {
    let n = DEVICE_TAG_MISMATCH.fetch_add(1, Ordering::Relaxed);
    if n < 16 {
        log_error!(
            "{site}: HDEVICE tag mismatch tag=0x{tag:016x} (x{}) — refused",
            n + 1
        );
    }
}

/// Per-device UMD state, constructed in-place in the runtime-allocated private
/// device memory (size = [`device_private_size`]). Owns the DXVK device the cxx
/// bridge created on the Helios venus adapter.
///
/// `#[repr(C)]` so `tag` is guaranteed to sit at offset 0 — the resolvers and
/// `ddi_destroy_device` read that word through the raw handle before deciding
/// which type the block is. Field DROP order is still declaration order
/// (`owned` before `dxvk`, see [`BridgeOwned`]); repr(C) fixes offsets, not
/// drop semantics.
#[repr(C)]
pub struct HeliosDevice {
    /// Always [`HELIOS_TAG_DEVICE`]; must stay the first field.
    pub tag: usize,
    /// Everything this device owns that came OUT of the bridge, in one field
    /// declared before `dxvk`. See [`BridgeOwned`] for why the position and
    /// the explicit `release()` both exist.
    pub owned: BridgeOwned,
    pub dxvk: bridge::BridgeDevice,
    pub h_rt_device: ddi::HANDLE,
    /// The kernel context and its buffer windows, validated at construction.
    /// `None` until `create_runtime_context` succeeds -- and CreateDevice now
    /// refuses a device that never gets one, so a live device always has it.
    pub context: Option<RuntimeContext>,
    pub kt_callbacks: *const ddi::D3DDDI_DEVICECALLBACKS,
    /// Created once with pfnCreatePagingQueueCb. WDDM 2.x residency is an
    /// explicit per-device list; allocation/patch lists do not make resources
    /// resident.
    pub paging_queue: Option<RuntimePagingQueue>,
    pub dxgi_callbacks: *mut ddi::DXGI_DDI_BASE_CALLBACKS,
    /// Exact pPrimaryDesc allocation identity -> Venus scanout metadata.
    /// DXGI can present a stable resource object while rotating its allocation
    /// handle, so the allocation is the authoritative lookup key.
    ///
    /// Mutex, not RefCell: written by CreateResource/DestroyResource (any
    /// thread under FREETHREADED caps), read by the present path. Lock with
    /// `crate::forward::lock_ignore_poison`.
    pub direct_scanout_allocations:
        std::sync::Mutex<Vec<(u32, helios_protocol::HeliosPresentPrivateData)>>,
    /// Runtime corelayer handle + callbacks (pfnSetErrorCb) so VOID-returning
    /// DDIs can report failures to the runtime instead of leaving null handles.
    pub h_rt_core_layer: *mut core::ffi::c_void,
    pub um_callbacks: *const core::ffi::c_void,
    /// The interface level this device negotiated at CreateDevice. A deferred
    /// context created on this device fills its context-funcs table in the
    /// SAME shape (`D3D11DDIARG_CREATEDEFERREDCONTEXT`'s funcs union member is
    /// selected by the device's negotiated level).
    pub negotiated: crate::adapter::NegotiatedInterface,
}

/// Per-deferred-context UMD state, constructed in-place in the runtime-
/// allocated `hDrvContext` private memory (size =
/// [`deferred_context_private_size`]). At the DDI level a deferred context IS
/// an HDEVICE — same handle type, destroyed through `pfnDestroyDevice` — so
/// this starts with the same tag header as [`HeliosDevice`] and every
/// resolver discriminates before casting.
#[repr(C)]
pub struct HeliosDeferredContext {
    /// Always [`HELIOS_TAG_DEFERRED`]; must stay the first field.
    pub tag: usize,
    /// The device this DC records against. Valid for the DC's whole life:
    /// the runtime guarantees the device (an IC handle is first-created/
    /// last-destroyed) outlives every DC created on it.
    pub parent: *const HeliosDevice,
    /// Owned DXVK deferred COM context from `ID3D11Device::CreateDeferredContext`.
    /// Never crosses the cxx bridge — the five immediate-context static_casts
    /// in dxvk_bridge.cpp must never see a deferred pointer.
    pub dc: Option<windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext>,
    /// This DC's own pipeline binding shadow (the immediate context's copy is
    /// [`BridgeOwned::bindings`]).
    pub bindings: CtxBindings,
    /// The DC's OWN corelayer handle + callbacks: per the WDK, a DC create/
    /// record error is reported through the DC's pfnSetErrorCb, not the
    /// device's.
    pub dc_core_layer: *mut core::ffi::c_void,
    pub dc_um_callbacks: *const core::ffi::c_void,
}

pub fn deferred_context_private_size() -> usize {
    core::mem::size_of::<HeliosDeferredContext>()
}

/// Device-global shader/layout caches (see [`BridgeOwned::caches`]).
///
/// Keys are context-invariant identities — VS COM pointers and `LayoutData`
/// box pointers — never DDI handle-region addresses, so the same entries will
/// serve deferred contexts once context-local handles exist (a DC-local handle
/// region carries a copy of the same identity word).
#[derive(Default)]
pub struct ShaderCaches {
    /// VS COM pointer (as `usize`) → its DXBC input-signature bytecode.
    pub vs_bytecode: std::collections::HashMap<usize, Vec<u8>>,
    /// VS COM pointer → the flattened DDI signature words it was created with
    /// ([n_in, n_out, (sysval, reg, mask, comptype, stream) × …]); used to
    /// recompile input-class variants (see `resolve_vs_input_variant`).
    pub vs_sig_words: std::collections::HashMap<usize, Vec<u32>>,
    /// (VS COM pointer, layout input-class key) → variant VS COM pointer,
    /// recompiled with the layout's per-register numeric classes. Variants
    /// live until device teardown (bounded: shaders × distinct class sets).
    pub vs_variants: std::collections::HashMap<(usize, u64), usize>,
    /// Cache of created input layouts keyed by (LayoutData box ptr, VS COM
    /// ptr) → owned `ID3D11InputLayout` raw pointer.
    pub layout_cache: std::collections::HashMap<(usize, usize), usize>,
}

impl ShaderCaches {
    /// Release the owned COM references held by the lazy IA caches.
    ///
    /// Cache keys are non-owning runtime/DXVK identities. Only the cache
    /// values own references transferred by `CreateInputLayout` and
    /// `create_shader_sig`.
    pub fn release_owned_com(&mut self) -> (usize, usize) {
        let variant_count = self.vs_variants.values().filter(|&&raw| raw != 0).count();
        let layout_count = self.layout_cache.values().filter(|&&raw| raw != 0).count();
        let mut owned = std::collections::HashSet::new();
        owned.extend(self.vs_variants.drain().filter_map(
            |(_, raw)| {
                if raw == 0 {
                    None
                } else {
                    Some(raw)
                }
            },
        ));
        owned.extend(self.layout_cache.drain().filter_map(
            |(_, raw)| {
                if raw == 0 {
                    None
                } else {
                    Some(raw)
                }
            },
        ));
        for raw in owned {
            // SAFETY: cache values are owned COM references whose ownership
            // was transferred into the cache with `into_raw` or returned by
            // the bridge's Create* call. `from_raw` adopts exactly that ref.
            unsafe {
                drop(IUnknown::from_raw(raw as *mut c_void));
            }
        }
        (variant_count, layout_count)
    }
}

impl Drop for ShaderCaches {
    fn drop(&mut self) {
        // Normal DestroyDevice calls this explicitly before the DXVK bridge
        // drops. Keep Drop as rollback/panic-path ownership protection.
        self.release_owned_com();
    }
}

/// Per-context pipeline binding shadow (draw diagnostics + the lazy
/// input-layout keys). The immediate context's copy is
/// [`BridgeOwned::bindings`]; each deferred context will own one.
///
/// Every field is an independent relaxed atomic scalar rather than a
/// `RefCell`: a free-threaded `DestroyShader`/`DestroyElementLayout` clears a
/// matching binding concurrently with draw-path reads, and a `RefCell`
/// double-borrow there is a panic — with `panic = "abort"` an immediate dwm
/// kill. No compound invariant spans two fields: the (layout, VS) pair read in
/// `bind_input_layout` tolerates tearing because the runtime's use-counting
/// never destroys an object that is still bound, so a torn read only ever
/// sees values that were both current a moment ago.
#[derive(Default)]
pub struct CtxBindings {
    /// The VS COM pointer most recently handed to DXVK's VSSetShader (may be
    /// a variant; `current_vs` stays the runtime's own binding).
    pub bound_vs_com: AtomicUsize,
    /// Currently-bound per-stage shader COM pointers.
    pub current_vs: AtomicUsize,
    pub current_ps: AtomicUsize,
    pub current_gs: AtomicUsize,
    pub current_hs: AtomicUsize,
    pub current_ds: AtomicUsize,
    pub current_cs: AtomicUsize,
    /// Currently-bound primitive topology and first IA buffer state, for draw
    /// diagnostics on complex D3D11 content.
    pub current_topology: AtomicU32,
    pub current_vb0: AtomicUsize,
    pub current_vb0_stride: AtomicU32,
    pub current_vb0_offset: AtomicU32,
    pub current_ib: AtomicUsize,
    pub current_ib_format: AtomicU32,
    pub current_ib_offset: AtomicU32,
    /// Allocation behind RTV slot 0, for live composition diagnostics.
    pub current_rt0_alloc: AtomicU32,
    /// Dimensions/format behind RTV slot 0, for live composition diagnostics.
    pub current_rt0_width: AtomicU32,
    pub current_rt0_height: AtomicU32,
    pub current_rt0_format: AtomicU32,
    /// Currently-bound element layout's `LayoutData` raw pointer (0 = none).
    pub current_layout: AtomicUsize,
}

impl CtxBindings {
    /// Zero every shadow slot for clear-state semantics: post-
    /// `pfnCommandListExecute` the runtime treats everything as unbound and
    /// rebinds lazily.
    pub fn reset(&self) {
        self.bound_vs_com.store(0, Ordering::Relaxed);
        self.current_vs.store(0, Ordering::Relaxed);
        self.current_ps.store(0, Ordering::Relaxed);
        self.current_gs.store(0, Ordering::Relaxed);
        self.current_hs.store(0, Ordering::Relaxed);
        self.current_ds.store(0, Ordering::Relaxed);
        self.current_cs.store(0, Ordering::Relaxed);
        self.current_topology.store(0, Ordering::Relaxed);
        self.current_vb0.store(0, Ordering::Relaxed);
        self.current_vb0_stride.store(0, Ordering::Relaxed);
        self.current_vb0_offset.store(0, Ordering::Relaxed);
        self.current_ib.store(0, Ordering::Relaxed);
        self.current_ib_format.store(0, Ordering::Relaxed);
        self.current_ib_offset.store(0, Ordering::Relaxed);
        self.current_rt0_alloc.store(0, Ordering::Relaxed);
        self.current_rt0_width.store(0, Ordering::Relaxed);
        self.current_rt0_height.store(0, Ordering::Relaxed);
        self.current_rt0_format.store(0, Ordering::Relaxed);
        self.current_layout.store(0, Ordering::Relaxed);
    }

    /// Reset the pipeline-semantic shadows required when a deferred context
    /// is reborn. With tracing enabled, retain the full diagnostic reset.
    pub fn reset_for_deferred_context_rebirth(&self) {
        self.reset_for_deferred_clear_state(crate::trace_enabled());
    }

    /// Reset after ExecuteCommandList(..., FALSE).  The three semantic shadows
    /// drive the input-layout/vertex-shader variant path after the runtime
    /// clears state; the remaining slots exist only for trace diagnostics.
    /// Keep their full reset whenever either diagnostic surface is enabled.
    pub fn reset_after_command_list_execute(&self) {
        self.reset_for_deferred_clear_state(
            crate::trace_enabled() || crate::umd_deferred_diagnostics(),
        );
    }

    fn reset_for_deferred_clear_state(&self, full_diagnostic_reset: bool) {
        if full_diagnostic_reset {
            self.reset();
            return;
        }

        self.bound_vs_com.store(0, Ordering::Relaxed);
        self.current_vs.store(0, Ordering::Relaxed);
        self.current_layout.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_clear_fast_path_preserves_diagnostic_shadows() {
        let bindings = CtxBindings::default();
        bindings.bound_vs_com.store(1, Ordering::Relaxed);
        bindings.current_vs.store(2, Ordering::Relaxed);
        bindings.current_layout.store(3, Ordering::Relaxed);
        bindings.current_ps.store(4, Ordering::Relaxed);
        bindings.current_topology.store(5, Ordering::Relaxed);
        bindings.current_rt0_alloc.store(6, Ordering::Relaxed);

        bindings.reset_for_deferred_clear_state(false);

        assert_eq!(bindings.bound_vs_com.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_vs.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_layout.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_ps.load(Ordering::Relaxed), 4);
        assert_eq!(bindings.current_topology.load(Ordering::Relaxed), 5);
        assert_eq!(bindings.current_rt0_alloc.load(Ordering::Relaxed), 6);
    }

    #[test]
    fn deferred_clear_diagnostics_reset_every_shadow() {
        let bindings = CtxBindings::default();
        bindings.bound_vs_com.store(1, Ordering::Relaxed);
        bindings.current_vs.store(2, Ordering::Relaxed);
        bindings.current_ps.store(3, Ordering::Relaxed);
        bindings.current_gs.store(4, Ordering::Relaxed);
        bindings.current_hs.store(5, Ordering::Relaxed);
        bindings.current_ds.store(6, Ordering::Relaxed);
        bindings.current_cs.store(7, Ordering::Relaxed);
        bindings.current_topology.store(8, Ordering::Relaxed);
        bindings.current_vb0.store(9, Ordering::Relaxed);
        bindings.current_vb0_stride.store(10, Ordering::Relaxed);
        bindings.current_vb0_offset.store(11, Ordering::Relaxed);
        bindings.current_ib.store(12, Ordering::Relaxed);
        bindings.current_ib_format.store(13, Ordering::Relaxed);
        bindings.current_ib_offset.store(14, Ordering::Relaxed);
        bindings.current_rt0_alloc.store(15, Ordering::Relaxed);
        bindings.current_rt0_width.store(16, Ordering::Relaxed);
        bindings.current_rt0_height.store(17, Ordering::Relaxed);
        bindings.current_rt0_format.store(18, Ordering::Relaxed);
        bindings.current_layout.store(19, Ordering::Relaxed);

        bindings.reset_for_deferred_clear_state(true);

        assert_eq!(bindings.bound_vs_com.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_vs.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_ps.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_gs.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_hs.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_ds.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_cs.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_topology.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_vb0.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_vb0_stride.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_vb0_offset.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_ib.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_ib_format.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_ib_offset.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_rt0_alloc.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_rt0_width.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_rt0_height.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_rt0_format.load(Ordering::Relaxed), 0);
        assert_eq!(bindings.current_layout.load(Ordering::Relaxed), 0);
    }
}

pub fn device_private_size() -> usize {
    core::mem::size_of::<HeliosDevice>()
}

/// Uniform stub signature (one machine word in, one out).
type UniformFn = unsafe extern "C" fn(usize) -> usize;

static DEVICE_NOOP_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static DXGI_NOOP_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static WDDM13_TABLE_AUDIT_COUNT: AtomicUsize = AtomicUsize::new(0);
static DXGI13_TABLE_AUDIT_COUNT: AtomicUsize = AtomicUsize::new(0);

#[link(name = "kernel32")]
extern "system" {
    fn RtlCaptureStackBackTrace(
        frames_to_skip: u32,
        frames_to_capture: u32,
        back_trace: *mut *mut c_void,
        back_trace_hash: *mut u32,
    ) -> u16;
}

pub(crate) unsafe fn log_backtrace(tag: &str) {
    let mut frames = [core::ptr::null_mut::<c_void>(); 32];
    let captured = RtlCaptureStackBackTrace(
        0,
        frames.len() as u32,
        frames.as_mut_ptr(),
        core::ptr::null_mut(),
    );
    let mut out = String::new();
    for i in 0..captured as usize {
        out.push_str(&format!(" #{i}=0x{:x}", frames[i] as usize));
    }
    log_error!("{tag} stack{out}");
}

/// No-op DDI stub: returns 0 (S_OK for HRESULT funcs; ignored for void funcs).
///
/// The counter is the WS3 "drive noop-DDI hit counts to zero" metric and stays
/// unconditional. Only the I/O is gated: this used to do a heap-allocating
/// `format!` plus an unbuffered write under the process-global log mutex from
/// inside the runtime's call — and the very first hit additionally captured and
/// formatted 32 stack frames through `RtlCaptureStackBackTrace` — none of it
/// behind `trace_enabled()`, unlike the rest of the repeat traffic.
unsafe extern "C" fn ddi_noop_device(_a: usize) -> usize {
    let n = DEVICE_NOOP_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 512 && crate::trace_enabled() {
        if n == 0 {
            log_backtrace("DDI noop(device)");
        } else {
            log_error!("DDI noop(device) hit={n}");
        }
    }
    0
}

/// DXGI base no-op DDI stub. Kept separate so Present-adjacent missing funcs are
/// distinguishable from D3D11 device-func misses.
///
/// PROVEN DEAD (T2 R419, name-diffed against the generated structs):
/// `install_dxgi`, `install_dxgi_1_1` and `install_dxgi_1_3` overwrite all
/// 7 / 8 / 18 slots of every DXGI table, so no slot is left pointing here. The
/// deletion belongs to T6, which owns deletions; this comment records the proof
/// so the next reader does not re-derive it.
unsafe extern "C" fn ddi_noop_dxgi(_a: usize) -> usize {
    let n = DXGI_NOOP_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 256 && crate::trace_enabled() {
        if n == 0 {
            log_backtrace("DDI noop(dxgi)");
        } else {
            log_error!("DDI noop(dxgi) hit={n}");
        }
    }
    0
}

/// CalcPrivate*Size stub: return a small nonzero, pointer-aligned size so the
/// runtime's driver-private object allocation is valid. Our Create* stubs never
/// write into it and no other stub reads it, so the exact size is immaterial.
unsafe extern "C" fn ddi_calc_size(_a: usize) -> usize {
    256
}

/// `pfnRelocateDeviceFuncs` is a NOTIFICATION: the runtime has already
/// copied the (driver-filled) table to the new location and tells the driver
/// so it can update any cached table pointer. This driver caches none, so
/// the correct implementation is a counted no-op.
///
/// ⚠ It must NOT refill the table. Under command lists the runtime relocates
/// TWICE PER pfnCommandListExecute (measured 1,585,160 calls = 2 × 792k
/// executes in one Fire Strike run) on the render thread, while FREETHREADED
/// create/calc DDIs read the same table from worker threads. The old
/// refill-on-relocate (stub-sweep every slot to `ddi_noop_device`, then
/// reinstall) made a concurrent `CalcPrivate*Size` transiently return 0 —
/// the runtime then allocates a zero-byte private region, the paired Create
/// writes through it, and the heap corruption surfaces as a wild
/// call (3DMarkICFWorkload c0000005 at a data address, faulting module
/// "unknown", 2026-08-03). The per-call log line was its own T2-class cost
/// (~9k mutex-serialized writes/s); first 8 + every 65536th now.
static RELOCATE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

fn relocate_log(tag: &str) {
    let n = RELOCATE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 8 || n % 65536 == 0 {
        log_error!(
            "DDI RelocateDeviceFuncs({tag}) (x{}) — noted, table untouched",
            n + 1
        );
    }
}

unsafe extern "C" fn ddi_relocate_device_funcs(
    _h_device: ddi::D3D10DDI_HDEVICE,
    _funcs: *mut ddi::D3D11DDI_DEVICEFUNCS,
) {
    relocate_log("D3D11");
}

unsafe extern "C" fn ddi_relocate_device_funcs_11_1(
    _h_device: ddi::D3D10DDI_HDEVICE,
    _funcs: *mut ddi::D3D11_1DDI_DEVICEFUNCS,
) {
    relocate_log("D3D11.1");
}

unsafe extern "C" fn ddi_relocate_device_funcs_wddm1_3(
    _h_device: ddi::D3D10DDI_HDEVICE,
    _funcs: *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS,
) {
    relocate_log("WDDM1.3");
}

unsafe fn audit_wddm1_3_device_funcs(tag: &str, funcs: *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS) {
    let hit = WDDM13_TABLE_AUDIT_COUNT.fetch_add(1, Ordering::Relaxed);
    if hit >= 32 || !crate::trace_enabled() {
        return;
    }

    let n = core::mem::size_of::<ddi::D3DWDDM1_3DDI_DEVICEFUNCS>() / core::mem::size_of::<usize>();
    let slots = funcs as *const usize;
    log_error!(
        "{tag}: WDDM1.3 funcs table={funcs:p} slots={n} audit={}",
        hit + 1
    );

    const EXT_NAMES: [&str; 9] = [
        "UpdateTileMappings",
        "CopyTileMappings",
        "CopyTiles",
        "UpdateTiles",
        "TiledResourceBarrier",
        "GetMipPacking",
        "ResizeTilePool",
        "SetMarker",
        "SetMarkerMode",
    ];
    for (offset, name) in EXT_NAMES.iter().enumerate() {
        let index = 155 + offset;
        if index < n {
            log_error!(
                "{tag}: WDDM1.3 slot[{index:03}] {name}=0x{:016x}",
                *slots.add(index)
            );
        }
    }

    // Exact stub identities, not an ASLR assumption. `value < 0x1_0000_0000` was
    // a guess about where modules load: it flagged every legitimately
    // low-loaded pointer and said nothing about WHICH stub a slot held. Both
    // addresses are already in scope here, so the classification is exact and
    // per-slot — that is what answers "which DDI is still a stub", at fill
    // time and by index, instead of a 32-frame backtrace at hit time.
    let noop = ddi_noop_device as UniformFn as usize;
    let calc = ddi_calc_size as UniformFn as usize;
    let mut null_slots = 0usize;
    let mut noop_slots = 0usize;
    let mut calc_slots = 0usize;
    for i in 0..n {
        let value = *slots.add(i);
        if value == 0 {
            null_slots += 1;
            log_error!("{tag}: WDDM1.3 NULL slot[{i:03}]");
        } else if value == noop {
            noop_slots += 1;
            if noop_slots <= 32 {
                log_error!("{tag}: WDDM1.3 noop slot[{i:03}]");
            }
        } else if value == calc {
            calc_slots += 1;
        }
    }
    log_error!(
        "{tag}: WDDM1.3 slots real={} noop={} calc={} null={}",
        n.saturating_sub(noop_slots + calc_slots + null_slots),
        noop_slots,
        calc_slots,
        null_slots
    );
}

unsafe fn audit_dxgi_1_3_base_funcs(tag: &str, funcs: *mut ddi::DXGI1_3_DDI_BASE_FUNCTIONS) {
    let hit = DXGI13_TABLE_AUDIT_COUNT.fetch_add(1, Ordering::Relaxed);
    if hit >= 32 || !crate::trace_enabled() {
        return;
    }

    let n = core::mem::size_of::<ddi::DXGI1_3_DDI_BASE_FUNCTIONS>() / core::mem::size_of::<usize>();
    let slots = funcs as *const usize;
    log_error!(
        "{tag}: DXGI1.3 funcs table={funcs:p} slots={n} audit={}",
        hit + 1
    );

    const NAMES: [&str; 18] = [
        "Present",
        "GetGammaCaps",
        "SetDisplayMode",
        "SetResourcePriority",
        "QueryResourceResidency",
        "RotateResourceIdentities",
        "Blt",
        "ResolveSharedResource",
        "Blt1",
        "OfferResources",
        "ReclaimResources",
        "GetMultiplaneOverlayCaps",
        "GetMultiplaneOverlayGroupCaps",
        "Reserved1",
        "PresentMultiplaneOverlay",
        "Reserved2",
        "Present1",
        "CheckPresentDurationSupport",
    ];
    for (i, name) in NAMES.iter().enumerate() {
        if i < n {
            log_error!(
                "{tag}: DXGI1.3 slot[{i:02}] {name}=0x{:016x}",
                *slots.add(i)
            );
        }
    }

    // Same exact classification as the device-funcs auditor. `noop` here should
    // never be found: install_dxgi/_1_1/_1_3 overwrite every slot of every DXGI
    // table (the proof recorded on ddi_noop_dxgi), and this line is what would
    // contradict that if it ever stopped holding.
    let noop = ddi_noop_dxgi as UniformFn as usize;
    let mut null_slots = 0usize;
    let mut noop_slots = 0usize;
    for i in 0..n {
        let value = *slots.add(i);
        if value == 0 {
            null_slots += 1;
            log_error!("{tag}: DXGI1.3 NULL slot[{i:02}]");
        } else if value == noop {
            noop_slots += 1;
            log_error!("{tag}: DXGI1.3 noop slot[{i:02}]");
        }
    }
    log_error!(
        "{tag}: DXGI1.3 slots real={} noop={} null={}",
        n.saturating_sub(noop_slots + null_slots),
        noop_slots,
        null_slots
    );
}

/// Real DestroyDevice: drop the in-place object behind the handle.
/// The runtime owns the backing memory, so we only run the destructor.
///
/// ⚠ TAG DISCRIMINATION IS LOAD-BEARING: a deferred context is destroyed
/// through THIS entry point too (no pfnDestroyContext exists). Running the
/// device teardown below on a DC handle would unregister/release/drop state
/// the parent device still owns — the single highest-risk confusion of the
/// command-list feature, which is why the tag is checked before any cast.
pub(crate) unsafe extern "C" fn ddi_destroy_device(h_device: ddi::D3D10DDI_HDEVICE) {
    if h_device.pDrvPrivate.is_null() {
        log_error!("DDI: DestroyDevice on null handle — refused");
        return;
    }
    match *(h_device.pDrvPrivate as *const usize) {
        HELIOS_TAG_DEVICE => {}
        HELIOS_TAG_DEFERRED => {
            // A deferred context dying through the shared DestroyDevice entry
            // point. Its teardown is ONLY its own state: drop the owned DXVK
            // deferred COM context and run the in-place destructor. None of
            // the device teardown below may run — the parent device still
            // owns all of it.
            crate::forward::note_deferred_context_destroyed();
            core::ptr::drop_in_place(h_device.pDrvPrivate as *mut HeliosDeferredContext);
            return;
        }
        tag => {
            note_device_tag_mismatch("DDI DestroyDevice", tag);
            return;
        }
    }
    log_error!("DDI: DestroyDevice");
    // R911: the nine refusal counters, once per device teardown. Process-global
    // rather than per-device, so this is a running total; the point is that
    // they are READ at all, which the T5 scan-out counters were not.
    log_error!("{}", crate::forward::ddi_refusal_summary());
    // The deferred-context surface, same readout discipline (Phase C).
    log_error!("{}", crate::forward::deferred_summary());
    // Bounded, process-global Present/Present1/MPO entry and callback evidence.
    // This is emitted before teardown while the UMD log remains available.
    log_error!("{}", crate::forward::present_boundary_summary());
    // Drop it from the liveness registry BEFORE anything is torn down, so a
    // concurrent `wait_last_present` on an ICD worker refuses rather than
    // dereferencing a block dxgkrnl is about to free and reuse (R415).
    crate::forward::unregister_live_device(h_device.pDrvPrivate as usize);
    {
        let dev = &mut *(h_device.pDrvPrivate as *mut HeliosDevice);
        // One explicit release of everything bridge-derived, while the bridge
        // device is still alive. Pre-R807 this released `ia` only; the other
        // three COM owners relied on a drop order that did not hold.
        let (variants, layouts) = dev.owned.release();
        log_error!(
            "DDI DestroyDevice: released IA cache variants={} layouts={}",
            variants,
            layouts
        );
        destroy_runtime_objects(dev);
        core::ptr::drop_in_place(h_device.pDrvPrivate as *mut HeliosDevice);
        // D4a scanout acquire, the tail of the §5.3 teardown order. The
        // drop_in_place above released the bridge device, and ~DxvkDevice ran
        // the DXVK half (stop arming → signal every gate to max → join the
        // signaler) — so by here no thread waits the event and no reader can
        // pick this device's ledger VA (the registry entry is removed under
        // the same mutex every reader holds). Unregister + close + unmap.
        crate::scanout_acquire::teardown_for_device(h_device.pDrvPrivate as usize);
    }
}

/// Destroy runtime-owned kernel objects while the runtime device handle and
/// callback table are still valid. This is shared by normal DestroyDevice and
/// the CreateDevice rollback path.
pub unsafe fn destroy_runtime_objects(dev: &mut HeliosDevice) {
    if !dev.kt_callbacks.is_null() {
        if let Some(queue) = dev.paging_queue.take() {
            if let Some(destroy_queue_cb) = (*dev.kt_callbacks).pfnDestroyPagingQueueCb {
                let arg = ddi::D3DDDI_DESTROYPAGINGQUEUE {
                    hPagingQueue: queue.handle.get(),
                };
                let hr = destroy_queue_cb(dev.h_rt_device, &arg);
                log_error!(
                    "DDI DestroyDevice: DestroyPagingQueue hQueue=0x{:x} hr=0x{:08x}",
                    queue.handle.get(),
                    hr as u32
                );
            }
        }

        if let Some(ctx) = dev.context.take() {
            if let Some(destroy_context_cb) = (*dev.kt_callbacks).pfnDestroyContextCb {
                let arg = ddi::D3DDDICB_DESTROYCONTEXT {
                    hContext: ctx.handle.as_ptr(),
                };
                let hr = destroy_context_cb(dev.h_rt_device, &arg);
                log_error!(
                    "DDI DestroyDevice: DestroyContext hContext={:p} hr=0x{:08x}",
                    ctx.handle.as_ptr(),
                    hr as u32
                );
            }
        }
    }
}

/// Create the kernel context every present path submits through. Returns an
/// HRESULT: a context-less device is not a usable device — its presents run the
/// GPU copy and the flush, report S_OK to DXGI and never call `pfnPresentCb`,
/// so the swapchain token is never minted and DXGI never falls back.
pub unsafe fn create_runtime_context(dev: &mut HeliosDevice) -> i32 {
    use crate::hr::E_FAIL;

    if dev.kt_callbacks.is_null() {
        log_error!("CreateDevice: no KT callbacks for CreateContext");
        return E_FAIL;
    }
    let Some(create_context_cb) = (*dev.kt_callbacks).pfnCreateContextCb else {
        log_error!("CreateDevice: pfnCreateContextCb missing");
        return E_FAIL;
    };

    let mut arg = ddi::D3DDDICB_CREATECONTEXT::default();
    arg.NodeOrdinal = 0;
    arg.EngineAffinity = 0;
    let hr = create_context_cb(dev.h_rt_device, &mut arg);
    log_error!(
        "CreateDevice: CreateContext hr=0x{:08x} hContext={:p} cmd={:p}/{} allocList={:p}/{} patchList={:p}/{}",
        hr as u32,
        arg.hContext,
        arg.pCommandBuffer,
        arg.CommandBufferSize,
        arg.pAllocationList,
        arg.AllocationListSize,
        arg.pPatchLocationList,
        arg.PatchLocationListSize
    );
    if hr != 0 {
        return hr;
    }
    // The whole group becomes meaningful at once, or the call fails. A null
    // hContext with hr == 0 would previously have left six companion fields set
    // and every consumer to discover it five checks deep.
    let Some(handle) = core::ptr::NonNull::new(arg.hContext) else {
        log_error!("CreateDevice: CreateContext returned S_OK with a null hContext");
        return E_FAIL;
    };
    dev.context = Some(RuntimeContext {
        handle,
        command: core::cell::Cell::new(Window::new(arg.pCommandBuffer, arg.CommandBufferSize)),
        allocations: core::cell::Cell::new(Window::new(
            arg.pAllocationList,
            arg.AllocationListSize,
        )),
        patches: core::cell::Cell::new(Window::new(
            arg.pPatchLocationList,
            arg.PatchLocationListSize,
        )),
    });
    0
}

/// Create the monitored-fence paging queue required by WDDM 2.x
/// pfnMakeResidentCb. Returns an HRESULT and leaves `paging_queue` empty on
/// failure.
pub unsafe fn create_runtime_paging_queue(dev: &mut HeliosDevice) -> i32 {
    use crate::hr::E_FAIL;

    if dev.kt_callbacks.is_null() {
        log_error!("CreateDevice: no KT callbacks for CreatePagingQueue");
        return E_FAIL;
    }
    let Some(create_queue_cb) = (*dev.kt_callbacks).pfnCreatePagingQueueCb else {
        log_error!("CreateDevice: pfnCreatePagingQueueCb missing");
        return E_FAIL;
    };

    let mut arg = ddi::D3DDDICB_CREATEPAGINGQUEUE::default();
    // D3DDDI_PAGINGQUEUE_PRIORITY_NORMAL == 0.
    arg.Priority = 0;
    arg.PhysicalAdapterIndex = 0;
    let hr = create_queue_cb(dev.h_rt_device, &mut arg);
    log_error!(
        "CreateDevice: CreatePagingQueue hr=0x{:08x} hQueue=0x{:x} hSync=0x{:x} fence={:p}",
        hr as u32,
        arg.hPagingQueue,
        arg.hSyncObject,
        arg.FenceValueCPUVirtualAddress
    );
    if hr != 0 {
        return hr;
    }

    let queue = core::num::NonZeroU32::new(arg.hPagingQueue);
    let sync_object = core::num::NonZeroU32::new(arg.hSyncObject);
    let fence_value_cpu = core::ptr::NonNull::new(arg.FenceValueCPUVirtualAddress.cast::<u64>());
    let (Some(handle), Some(sync_object), Some(fence_value_cpu)) =
        (queue, sync_object, fence_value_cpu)
    else {
        log_error!("CreateDevice: CreatePagingQueue returned invalid outputs");
        if let Some(destroy_queue_cb) = (*dev.kt_callbacks).pfnDestroyPagingQueueCb {
            if arg.hPagingQueue != 0 {
                let destroy = ddi::D3DDDI_DESTROYPAGINGQUEUE {
                    hPagingQueue: arg.hPagingQueue,
                };
                let _ = destroy_queue_cb(dev.h_rt_device, &destroy);
            }
        }
        return E_FAIL;
    };

    dev.paging_queue = Some(RuntimePagingQueue {
        handle,
        sync_object,
        fence_value_cpu,
    });
    0
}

/// Bulk-fill every pointer slot of a device-funcs table with `ddi_noop_device`
/// and return the D3D11.0-typed view of it.
///
/// The slot count comes from `size_of::<T>()`, so it CANNOT disagree with the
/// table actually being filled. That matters: the failure mode this replaces is
/// a wrong length under-stubbing a table and leaving uninitialised slots past
/// the prefix, which is precisely what `fill_dxgi_1_3_base_funcs`'s comment
/// exists to warn about. The three fills each spelled the length out by hand.
///
/// # Safety
/// `funcs` must point to a writable `T` whose every field is a pointer-sized
/// `Option<fn>`, and `T` must be a layout-compatible extension of
/// `D3D11DDI_DEVICEFUNCS` (a WDK header property no Rust type can assert).
unsafe fn stub_fill_device_table<T>(funcs: *mut T) -> *mut ddi::D3D11DDI_DEVICEFUNCS {
    let n = core::mem::size_of::<T>() / core::mem::size_of::<usize>();
    let slots = funcs as *mut Option<UniformFn>;
    for i in 0..n {
        *slots.add(i) = Some(ddi_noop_device);
    }
    funcs as *mut ddi::D3D11DDI_DEVICEFUNCS
}

/// The `CalcPrivate*Size` entries and the two real lifecycle entries every
/// device-funcs table gets, applied identically at every interface level.
///
/// Was three verbatim copies of one 18-name `calc!` list plus the same two
/// assignments and the same 10-line rationale comment.
///
/// # Safety
/// `f` must be the D3D11.0-typed view of a stub-filled table.
unsafe fn install_calc_and_lifecycle(f: &mut ddi::D3D11DDI_DEVICEFUNCS) {
    // CalcPrivate*Size funcs must return a valid nonzero size.
    macro_rules! calc {
        ($($field:ident),* $(,)?) => {$(
            f.$field = core::mem::transmute::<UniformFn, _>(ddi_calc_size as UniformFn);
        )*};
    }
    calc!(
        pfnCalcPrivateResourceSize,
        pfnCalcPrivateOpenedResourceSize,
        pfnCalcPrivateShaderResourceViewSize,
        pfnCalcPrivateRenderTargetViewSize,
        pfnCalcPrivateDepthStencilViewSize,
        pfnCalcPrivateElementLayoutSize,
        pfnCalcPrivateBlendStateSize,
        pfnCalcPrivateDepthStencilStateSize,
        pfnCalcPrivateRasterizerStateSize,
        pfnCalcPrivateShaderSize,
        pfnCalcPrivateGeometryShaderWithStreamOutput,
        pfnCalcPrivateSamplerSize,
        pfnCalcPrivateQuerySize,
        pfnCalcPrivateTessellationShaderSize,
        pfnCalcPrivateUnorderedAccessViewSize,
    );
    // The deferred-context/command-list size family is REAL (Phase C), no
    // longer the 256-byte stub: the paired Create slots are live in
    // `forward::install`, so a stub size here would be exactly the R812 heap
    // corruption the old compile-time assert guarded against. The deprecated
    // COMMANDLISTS (0x2) bit remains impossible — BUILD_2 devices report
    // 0x1|0x4 only.
    const _: () = assert!(THREADING_CAPS_POSSIBLE & D3D11DDICAPS_COMMANDLISTS == 0);
    f.pfnCalcDeferredContextHandleSize = Some(crate::forward::calc_deferred_context_handle_size);
    f.pfnCalcPrivateDeferredContextSize = Some(crate::forward::calc_private_deferred_context_size);
    f.pfnCalcPrivateCommandListSize = Some(crate::forward::calc_private_command_list_size);
    // Not a size getter -- a void writer. Installed with its real signature, no
    // transmute, identically in every table. R812; real array since Phase C.
    f.pfnCheckDeferredContextHandleSizes =
        Some(crate::forward::check_deferred_context_handle_sizes);

    // Real cleanup on device teardown (matching signature, no transmute).
    f.pfnDestroyDevice = Some(ddi_destroy_device);
}

/// Fill every entry of a `D3D11DDI_DEVICEFUNCS` table with safe stubs, then
/// specialise the entries whose behaviour matters for device creation.
///
/// # Safety
/// `funcs` must point to a writable `D3D11DDI_DEVICEFUNCS` (the runtime's table,
/// selected when Interface == D3D11_0_DDI_INTERFACE_VERSION).
pub unsafe fn fill_d3d11_device_funcs(funcs: *mut ddi::D3D11DDI_DEVICEFUNCS) {
    let f = &mut *stub_fill_device_table(funcs);
    install_calc_and_lifecycle(f);
    f.pfnRelocateDeviceFuncs = Some(ddi_relocate_device_funcs);

    // Override stubs with the real D3D11 COM forwarders. The 11.0 table stops
    // here: the returned proof is what the higher levels below consume.
    let _base = crate::forward::install(funcs);
}

/// Fill a D3D11.1 device-funcs table. The D3D11.1 layout is an extension of the
/// D3D11.0 prefix, so the implemented forwarders can be installed through the
/// D3D11.0 view after the whole larger table has been stub-filled.
pub unsafe fn fill_d3d11_1_device_funcs(funcs: *mut ddi::D3D11_1DDI_DEVICEFUNCS) {
    let f = &mut *stub_fill_device_table(funcs);
    install_calc_and_lifecycle(f);
    (*funcs).pfnRelocateDeviceFuncs = Some(ddi_relocate_device_funcs_11_1);

    // The ordering is now structural, not textual: `install_11_1` cannot be
    // called without the `Filled11_0` token `install` returns.
    let base = crate::forward::install(f);
    let _l1 = crate::forward::install_11_1(base, funcs);
}

pub unsafe fn fill_wddm1_3_device_funcs(funcs: *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS) {
    let f = &mut *stub_fill_device_table(funcs);
    install_calc_and_lifecycle(f);
    (*funcs).pfnRelocateDeviceFuncs = Some(ddi_relocate_device_funcs_wddm1_3);

    let base = crate::forward::install(f);
    let l1 = crate::forward::install_11_1(base, funcs as *mut ddi::D3D11_1DDI_DEVICEFUNCS);
    let _l13 = crate::forward::install_wddm1_3(l1, funcs);
    audit_wddm1_3_device_funcs("FillDeviceFuncs", funcs);
}

/// Fill the DXGI base DDI table (presentation/resource base funcs) the runtime
/// hands us in the CREATEDEVICE args. All stubbed for Milestone 1 (no present).
///
/// # Safety
/// `funcs` must point to a writable `DXGI_DDI_BASE_FUNCTIONS`, or be null.
pub unsafe fn fill_dxgi_base_funcs(funcs: *mut ddi::DXGI_DDI_BASE_FUNCTIONS) {
    if funcs.is_null() {
        return;
    }
    let n = core::mem::size_of::<ddi::DXGI_DDI_BASE_FUNCTIONS>() / core::mem::size_of::<usize>();
    let slots = funcs as *mut Option<UniformFn>;
    for i in 0..n {
        *slots.add(i) = Some(ddi_noop_dxgi);
    }
    // Real (benign) present so LogonUI/DWM don't fail-fast on present.
    crate::forward::install_dxgi(funcs);
}

/// Fill the DXGI 1.1 base table. This is required for D3D11.1 device creation
/// because the table adds pfnResolveSharedResource after the D3D10-era prefix.
pub unsafe fn fill_dxgi_1_1_base_funcs(funcs: *mut ddi::DXGI1_1_DDI_BASE_FUNCTIONS) {
    if funcs.is_null() {
        return;
    }
    let n = core::mem::size_of::<ddi::DXGI1_1_DDI_BASE_FUNCTIONS>() / core::mem::size_of::<usize>();
    let slots = funcs as *mut Option<UniformFn>;
    for i in 0..n {
        *slots.add(i) = Some(ddi_noop_dxgi);
    }
    crate::forward::install_dxgi(funcs as *mut ddi::DXGI_DDI_BASE_FUNCTIONS);
    crate::forward::install_dxgi_1_1(funcs);
}

/// Fill the DXGI 1.3 base table required by WDDM1.3 devices. DWM can call the
/// later Present1/MPO/residency slots immediately after CreateDevice; handing it
/// only the DXGI 1.1 prefix leaves uninitialized callback pointers past slot 7.
pub unsafe fn fill_dxgi_1_3_base_funcs(funcs: *mut ddi::DXGI1_3_DDI_BASE_FUNCTIONS) {
    if funcs.is_null() {
        return;
    }
    let n = core::mem::size_of::<ddi::DXGI1_3_DDI_BASE_FUNCTIONS>() / core::mem::size_of::<usize>();
    let slots = funcs as *mut Option<UniformFn>;
    for i in 0..n {
        *slots.add(i) = Some(ddi_noop_dxgi);
    }
    crate::forward::install_dxgi(funcs as *mut ddi::DXGI_DDI_BASE_FUNCTIONS);
    crate::forward::install_dxgi_1_1(funcs as *mut ddi::DXGI1_1_DDI_BASE_FUNCTIONS);
    crate::forward::install_dxgi_1_3(funcs);
    audit_dxgi_1_3_base_funcs("FillDXGIBaseFuncs", funcs);
}
