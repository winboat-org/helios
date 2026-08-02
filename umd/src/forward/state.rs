//! Per-object state behind `pDrvPrivate`, and the accessors that reach it.
//!
//! `ResourceState`, `RtvState`, `ResidentAllocation` with its eviction Drop,
//! the allocation-ownership and deallocate-form enums, the standard-allocation
//! metadata readers, the device/context getters, and the store/load/release
//! helpers for COM handles, resources and RTVs.
//!
//! The typed slot decoding itself lives in [`super::handles`] (T5/R803); this
//! module is the state those slots point AT.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

pub(crate) struct ResourceState {
    pub(crate) com_raw: usize,
    /// The same object's `ID3D11Buffer` interface pointer, pre-cast ONCE at
    /// store time; 0 for non-buffer resources. Constant-buffer bind DDIs read
    /// this word instead of a per-call `QueryInterface` + transient
    /// AddRef/Release pair, which profiled as part of an ~8 % render-thread
    /// cost (tmp/handoff-perf-saturation/reports/p1-attribution.md §2).
    /// No reference is held through this field: `com_raw` owns the object,
    /// and a COM interface pointer stays valid while its object lives.
    pub(crate) buffer_raw: usize,
    /// A WDDM allocation is stored only after pfnMakeResidentCb has added one
    /// device-residency reference. Dropping the guard removes exactly that
    /// reference before the allocation is deallocated.
    pub(crate) allocation: Option<ResidentAllocation>,
    pub(crate) km_resource: ddi::D3DKMT_HANDLE,
    pub(crate) rt_resource: ddi::HANDLE,
    /// True when this UMD allocated `allocation` itself (pfnAllocateCb in
    /// `create_resource`); false for handles the runtime handed us at
    /// `open_resource`. `release_resource` may only pass allocation handles it
    /// created to pfnDeallocateCb's HandleList form — deallocating opened
    /// handles that way is what returned 0x80070057 and leaked the runtime's
    /// side of the open.
    pub(crate) ownership: AllocationOwnership,
    pub(crate) present_private: HeliosPresentPrivateData,
}

/// Who owns the WDDM allocation behind a resource.
///
/// This used to be an unnamed positional `bool` sitting between `km_resource`
/// and `rt_resource` in a seven-argument call, and the deallocate form was
/// reconstructed from `(rt_resource.is_null(), owns_allocation)` at destroy
/// time. R804.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AllocationOwnership {
    /// This UMD called `pfnAllocateCb` for it, in `create_resource`.
    CreatedByUmd,
    /// The runtime handed us the handle at `open_resource`. Passing these to
    /// `pfnDeallocateCb`'s HandleList form is what returned 0x80070057 and
    /// leaked the runtime's side of the open.
    OpenedByRuntime,
}

impl AllocationOwnership {
    /// The value the `owned=` log key has always carried. Kept so the
    /// `DDI deallocate_resource:` line stays byte-identical across R804.
    pub(crate) fn owns(self) -> bool {
        matches!(self, Self::CreatedByUmd)
    }
}

/// The one legal shape of a `D3DDDICB_DEALLOCATE` call.
///
/// The wire contract is three-way: EITHER `hResource` alone, OR
/// `NumAllocations`+`HandleList` with `hResource` NULL. Both together is
/// E_INVALIDARG -- the old 0x80070057, which also leaked opened resources.
/// Constructing this enum is the only way a `D3DDDICB_DEALLOCATE` is built, so
/// the both-set combination is unrepresentable rather than merely avoided.
///
/// `Nothing` must still exist: it is the (currently unreachable) case of no
/// runtime resource and an allocation we did not create. The guarantee is that
/// it is named and logged, not that it is gone.
pub(crate) enum DeallocateForm {
    ByResource(ddi::HANDLE),
    ByHandleList(core::num::NonZeroU32),
    Nothing { reason: &'static str },
}

impl DeallocateForm {
    /// The single exhaustive decision. Order matters and is the pre-R804
    /// behaviour exactly: a runtime resource handle wins over ownership,
    /// because the runtime then releases every allocation it tracks for the
    /// resource -- created AND opened instances.
    pub(crate) fn select(
        rt_resource: ddi::HANDLE,
        ownership: AllocationOwnership,
        allocation: ddi::D3DKMT_HANDLE,
    ) -> Self {
        if !rt_resource.is_null() {
            return Self::ByResource(rt_resource);
        }
        match (ownership, core::num::NonZeroU32::new(allocation)) {
            (AllocationOwnership::CreatedByUmd, Some(a)) => Self::ByHandleList(a),
            (AllocationOwnership::CreatedByUmd, None) => Self::Nothing {
                reason: "owner but no allocation handle",
            },
            (AllocationOwnership::OpenedByRuntime, _) => Self::Nothing {
                reason: "not owner",
            },
        }
    }
}

pub(crate) type EvictCallback =
    unsafe extern "C" fn(ddi::HANDLE, *mut ddi::D3DDDICB_EVICT) -> ddi::HRESULT;

/// One persistent WDDM 2.x device-residency reference.
///
/// This type is deliberately non-Clone and non-Copy: every successful
/// pfnMakeResidentCb call creates exactly one guard, and moving/dropping that
/// guard is the only way the reference can change ownership or be evicted.
pub(crate) struct ResidentAllocation {
    pub(crate) handle: core::num::NonZeroU32,
    pub(crate) h_rt_device: ddi::HANDLE,
    pub(crate) evict_cb: EvictCallback,
}

impl ResidentAllocation {
    pub(crate) fn handle(&self) -> ddi::D3DKMT_HANDLE {
        self.handle.get()
    }
}

impl Drop for ResidentAllocation {
    fn drop(&mut self) {
        let handle = self.handle.get();
        let mut evict = ddi::D3DDDICB_EVICT::default();
        evict.NumAllocations = 1;
        evict.AllocationList = &handle;
        let hr = unsafe { (self.evict_cb)(self.h_rt_device, &mut evict) };
        trace_line!(
            "WDDM residency: Evict alloc=0x{:x} hr=0x{:08x}",
            handle,
            hr as u32
        );
        if hr != 0 {
            log_error!(
                "WDDM residency: Evict FAILED alloc=0x{:x} hr=0x{:08x}",
                handle,
                hr as u32
            );
        }
    }
}

pub(crate) struct RtvState {
    pub(crate) com_raw: usize,
    /// Non-owning resource pointer; the RTV itself keeps the resource alive.
    pub(crate) resource_raw: usize,
    pub(crate) allocation: ddi::D3DKMT_HANDLE,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) format: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct RuntimeAllocPrivate {
    pub(crate) alloc: HeliosWddmAllocPrivate,
    pub(crate) meta: HeliosWddmAllocMeta,
}

#[inline]
pub(crate) fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

/// The three present-path debug knobs, read ONCE per process.
///
/// Each was an uncached `GetEnvironmentVariableW` plus an `OsString`
/// allocation on every present: two from the debug hooks and one for
/// `bOptimizeForComposition`. OBSERVABLE CHANGE, stated deliberately: these
/// become read-once-per-process, so setting them on a live process no longer
/// takes effect — they must be set before the process starts, like every
/// registry knob in `lib.rs`, all of which already use `OnceLock`.
///
/// Cost honesty (the review asks for it): three env reads per present against
/// a 10 ms frame gate is not where the present path spends its time. This is
/// removing avoidable per-frame work, not a measured win.
pub(crate) fn present_readback_enabled() -> bool {
    static VALUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| env_flag("HELIOS_PRESENT_READBACK"))
}

pub(crate) fn present_force_opaque_enabled() -> bool {
    static VALUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| env_flag("HELIOS_PRESENT_FORCE_OPAQUE"))
}

pub(crate) fn present_optimize_composition_enabled() -> bool {
    static VALUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| env_flag("HELIOS_PRESENT_OPTIMIZE_COMPOSITION"))
}

pub(crate) fn empty_present_private() -> HeliosPresentPrivateData {
    HeliosPresentPrivateData {
        plane_offset: 0,
        magic: 0,
        version: 0,
        resource_id: 0,
        width: 0,
        height: 0,
        pitch: 0,
        dxgi_format: 0,
        reserved: 0,
        venus_alloc_size: 0,
    }
}

/// Legacy 24-byte trailer (geometry + bind/misc, no venus identity) written by
/// pre-identity driver builds. Parse-only.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub(crate) struct StandardAllocMetaV2 {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) format: u32,
    pub(crate) pitch: u32,
    pub(crate) bind_flags: u32,
    pub(crate) misc_flags: u32,
}

/// Oldest legacy 16-byte trailer. Parse-only.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub(crate) struct StandardAllocMetaV1 {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) format: u32,
    pub(crate) pitch: u32,
}

// The eight range tables that used to sit here are one row per format in
// `crate::format`, which also carries the `0..=200` equivalence test that pins
// every answer to the pre-change predicate. These are the thin adapters that
// keep the call sites reading as they did.

/// Parse the meta trailer at `base_off` bytes into the buffer, tolerating the
/// two legacy (shorter) layouts. Returns a zero-extended [`HeliosWddmAllocMeta`].
pub(crate) unsafe fn read_alloc_meta(
    ptr: *const c_void,
    size: u32,
    base_off: usize,
) -> Option<HeliosWddmAllocMeta> {
    let avail = (size as usize).checked_sub(base_off)?;
    let meta_ptr = (ptr as *const u8).add(base_off);
    if avail >= core::mem::size_of::<HeliosWddmAllocMeta>() {
        return Some(core::ptr::read_unaligned(
            meta_ptr as *const HeliosWddmAllocMeta,
        ));
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
            plane_offset: 0,
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
            plane_offset: 0,
        });
    }
    None
}

/// A fully identified open: the KMD's identity record AND the creator's meta
/// trailer. The trailer is non-optional by construction — the import geometry
/// is read out of it, so there is no `Option` left to default at the
/// `open_ddi_texture2d` call and the 1x1 alias cannot be reconstructed.
#[derive(Clone, Copy)]
pub(crate) struct OpenedAllocation {
    pub(crate) ident: HeliosWddmOpenIdentity,
    pub(crate) meta: HeliosWddmAllocMeta,
}

/// Parse the OPEN-time private data: the KMD's versioned [`HeliosWddmOpenIdentity`]
/// record (written in `DxgkDdiOpenAllocation` after validating the venus resource
/// is LIVE) plus the creator's meta trailer. This replaces the adopt-id
/// heuristics: an open-time buffer either carries a valid identity or the open is
/// not backed by an identified venus resource.
///
/// This is the DIAGNOSTIC parse: it tolerates a missing trailer so the C1
/// identity evidence lines can still print what the buffer actually held. An
/// open must go through [`read_opened_allocation`] instead — an identity with
/// no trailer carries no geometry and is not openable.
pub(crate) unsafe fn read_open_identity(
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

/// The parse an OPEN may use: identity plus a present meta trailer, or nothing.
pub(crate) unsafe fn read_opened_allocation(
    ptr: *const c_void,
    size: u32,
) -> Option<OpenedAllocation> {
    let (ident, meta) = read_open_identity(ptr, size)?;
    Some(OpenedAllocation { ident, meta: meta? })
}

// --- handle <-> COM helpers -------------------------------------------------

pub(crate) unsafe fn d3d11_device(h: Hdevice) -> Option<ManuallyDrop<ID3D11Device>> {
    let hd = h.pDrvPrivate as *const HeliosDevice;
    if hd.is_null() {
        return None;
    }
    // Borrowed, not adopted: the bridge keeps the owning reference. The
    // ManuallyDrop lives inside the wrapper now, so this cannot be written as
    // an adopting `from_raw` by a future edit. R813.
    (*hd).dxvk.d3d11_device()
}

/// Borrow the `HeliosDevice` behind a device handle (for the deferred
/// input-assembler state). Does not take ownership.
pub(crate) unsafe fn helios_device<'a>(h: Hdevice) -> Option<&'a HeliosDevice> {
    let hd = h.pDrvPrivate as *const HeliosDevice;
    if hd.is_null() {
        None
    } else {
        Some(&*hd)
    }
}

/// Report an error to the D3D11 runtime for a VOID-returning DDI via the
/// corelayer `pfnSetErrorCb`. The runtime fails the API call that invoked the
/// DDI (e.g. `OpenSharedResource`), which is the contractual way for
/// `open_resource`/`create_*` to fail loudly instead of leaving a null handle
/// the runtime will dereference.
pub(crate) unsafe fn set_runtime_error(h: Hdevice, hr: i32) {
    let Some(dev) = helios_device(h) else {
        return;
    };
    if dev.um_callbacks.is_null() {
        log_error!("set_runtime_error: no corelayer callbacks");
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

pub(crate) unsafe fn d3d11_context(h: Hdevice) -> Option<ManuallyDrop<ID3D11DeviceContext>> {
    let hd = h.pDrvPrivate as *const HeliosDevice;
    if hd.is_null() {
        return None;
    }
    (*hd).dxvk.d3d11_context()
}

/// The immediate context as `ID3D11DeviceContext1`, borrowed.
///
/// The bridge's immediate context is DXVK's `D3D11ImmediateContext`, whose
/// `QueryInterface` returns `this` for every `ID3D11DeviceContext{,1..4}`
/// IID (dxvk-helios `d3d11_context.cpp:87-96`) — one vtable serves the whole
/// chain, so the same pointer IS the derived interface. The previous per-call
/// `.cast()` was a QI + transient AddRef/Release on the hot
/// `SetConstantBuffers1` family (p1-attribution §2).
pub(crate) unsafe fn d3d11_context1(h: Hdevice) -> Option<ManuallyDrop<ID3D11DeviceContext1>> {
    let context = d3d11_context(h)?;
    // SAFETY: same object, same vtable (see above); ManuallyDrop keeps this a
    // borrow — the bridge holds the owning reference.
    Some(ManuallyDrop::new(ID3D11DeviceContext1::from_raw(
        context.as_raw(),
    )))
}

/// As [`d3d11_context1`], for the tiled-resource DDIs.
pub(crate) unsafe fn d3d11_context2(h: Hdevice) -> Option<ManuallyDrop<ID3D11DeviceContext2>> {
    let context = d3d11_context(h)?;
    // SAFETY: as d3d11_context1 — the DXVK QI covers ID3D11DeviceContext2.
    Some(ManuallyDrop::new(ID3D11DeviceContext2::from_raw(
        context.as_raw(),
    )))
}

pub(crate) unsafe fn d3d11_device2(h: Hdevice) -> Option<ID3D11Device2> {
    let device = d3d11_device(h)?;
    (*device).cast::<ID3D11Device2>().ok()
}

/// Store a COM interface's raw pointer (ownership transferred) in a DDI handle.
///
/// The null check is new: this was the one writer of the three that lacked it,
/// so a null slot wrote through a null pointer where `store_raw_com` and
/// `clear_handle` returned quietly. R803.
pub(crate) unsafe fn store_com<T: Interface>(h: impl ComHandle, obj: T) {
    match Slot::<Com<T>>::from_priv(h.drv_private()) {
        Some(slot) => slot.store(obj),
        // Dropping `obj` here releases the reference we were asked to hand to
        // the runtime. That is correct: with no slot to put it in, the
        // alternative is leaking it.
        None => drop(obj),
    }
}

/// Map a DXVK create failure onto an HRESULT the invoking API is documented to
/// return. Every `Create*` DDI this is used from documents exactly
/// `E_OUTOFMEMORY` and `E_INVALIDARG`; an HRESULT outside that set is itself
/// logged by the runtime as a driver bug, so a DXVK code outside it is
/// substituted rather than passed through.
pub(crate) fn create_error_hr(e: &windows::core::Error) -> i32 {
    match e.code().0 {
        hr @ (E_OUTOFMEMORY | E_INVALIDARG) => hr,
        _ => E_OUTOFMEMORY,
    }
}

/// The only way a VOID-returning `Create*` DDI may leave its handle slot:
/// either `store` runs, or the runtime is told the invoking API call failed.
///
/// The DDI has no return value, so `pfnSetErrorCb` is the sole channel through
/// which a failed create becomes a failed `CreateRasterizerState` /
/// `CreateRenderTargetView` / `CreateTexture2D` instead of an S_OK with a null
/// driver handle that the app then binds to nothing.
///
/// `result` is the DXVK call's `Result<()>` and `obj` its out-param: S_OK with
/// no object is as much a fake success as an error HRESULT, so both report.
/// Panic-free by construction (no `unwrap`, no indexing) — these are
/// `extern "C"` entry points under `panic = "abort"`.
pub(crate) unsafe fn finish_create<T: Interface>(
    h: Hdevice,
    result: windows::core::Result<()>,
    obj: Option<T>,
    store: impl FnOnce(T),
) {
    match result {
        Ok(()) => match obj {
            Some(o) => store(o),
            None => set_runtime_error(h, E_OUTOFMEMORY),
        },
        Err(e) => set_runtime_error(h, create_error_hr(&e)),
    }
}

pub(crate) unsafe fn store_raw_com(h: impl ComHandle, raw: usize) {
    if let Some(slot) = Slot::<Com<IUnknown>>::from_priv(h.drv_private()) {
        slot.store_raw(raw);
    }
}

/// Null a handle's slot. Payload-agnostic on purpose -- this writes the word
/// and never touches what it pointed at -- so it takes any `DdiHandle`,
/// including the boxed-payload ones. Every `Create*`/`Open*` DDI calls it on
/// entry so a failure leaves a null handle rather than stale garbage.
pub(crate) unsafe fn clear_handle(h: impl DdiHandle) {
    if let Some(slot) = Slot::<Com<IUnknown>>::from_priv(h.drv_private()) {
        slot.clear();
    }
}

/// The raw word behind a bare-COM DDI handle (0 when absent).
///
/// Correct for the shader/DSV/state slots it is used on, and silently the
/// `Box` pointer if ever applied to a resource or RTV slot -- which is exactly
/// the confusion `Slot<P>` exists to remove. Retained with its old signature so
/// this commit churns no call sites; the per-family conversions replace each
/// use with `Slot::<Com<T>>::word()` on a typed handle.
pub(crate) unsafe fn handle_com_raw(h: impl ComHandle) -> usize {
    handle_com_raw_at(h.drv_private())
}

/// `handle_com_raw` for the runtime-tag dispatches, which receive a bare
/// `pDrvPrivate` whose payload is selected by a `D3D11DDI_HANDLETYPE` value at
/// run time and so cannot be keyed on a static handle type. Callers must be an
/// arm of such a dispatch that has already matched a bare-COM tag.
pub(crate) unsafe fn handle_com_raw_at(handle_priv: *mut c_void) -> usize {
    match Slot::<Com<IUnknown>>::from_priv(handle_priv) {
        Some(slot) => slot.word(),
        None => 0,
    }
}

pub(crate) unsafe fn store_resource(
    h_res: ddi::D3D10DDI_HRESOURCE,
    obj: ID3D11Resource,
    allocation: Option<ResidentAllocation>,
    km_resource: ddi::D3DKMT_HANDLE,
    rt_resource: ddi::HANDLE,
    ownership: AllocationOwnership,
    present_private: HeliosPresentPrivateData,
) {
    let Some(slot) = boxed_slot(h_res) else {
        drop(obj);
        return;
    };
    // One QI here instead of one per bind call. The temporary's release is
    // fine: `com_raw` keeps the object alive, which keeps the pointer valid.
    let buffer_raw = obj
        .cast::<ID3D11Buffer>()
        .map(|b| b.as_raw() as usize)
        .unwrap_or(0);
    slot.store(ResourceState {
        com_raw: obj.into_raw() as usize,
        buffer_raw,
        allocation,
        km_resource,
        rt_resource,
        ownership,
        present_private,
    });
}

pub(crate) unsafe fn stamp_dxvk_resource_kmt_handles(
    h: Hdevice,
    obj: &ID3D11Resource,
    local: ddi::D3DKMT_HANDLE,
    global: ddi::D3DKMT_HANDLE,
) {
    trace_line!(
        "DDI resource KMT stamp enter: raw_local=0x{:x} raw_global=0x{:x}",
        local,
        global
    );
    let local = if local != 0 { local } else { global };
    if local == 0 {
        trace_line!("DDI resource KMT stamp skipped: no usable handle");
        return;
    }
    let Some(dev) = helios_device(h) else {
        log_error!("DDI resource KMT stamp skipped: missing device");
        return;
    };
    if dev
        .dxvk
        // SAFETY: `obj` is a live ID3D11Resource borrowed for this call, so the
        // pointer the bridge reinterpret_casts is valid for its duration.
        // R814 moved `unsafe` onto this declaration because it launders a
        // pointer; the marking now matches where the precondition is.
        .set_resource_kmt_handles(obj.as_raw() as usize, local, global)
    {
        log_error!(
            "DDI resource KMT handles stamped: local=0x{:x} global=0x{:x}",
            local,
            global
        );
    } else {
        log_error!(
            "DDI resource KMT handle stamp failed: local=0x{:x} global=0x{:x}",
            local,
            global
        );
    }
}

pub(crate) unsafe fn load_resource(
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> Option<ManuallyDrop<ID3D11Resource>> {
    load_resource_at(h_res.pDrvPrivate)
}

/// `load_resource` for the runtime-tag dispatches (`Discard`, the tiled-resource
/// barrier), which receive a bare `pDrvPrivate` whose payload is selected by a
/// `D3D11DDI_HANDLETYPE` value at run time and so cannot be keyed on a static
/// handle type. Callers must be an arm of such a dispatch that has already
/// matched `HT_RESOURCE`. Same contract as `handle_com_raw_at`/`load_com_at`.
pub(crate) unsafe fn load_resource_at(
    handle_priv: *mut c_void,
) -> Option<ManuallyDrop<ID3D11Resource>> {
    let state = resource_state_at(handle_priv)?;
    if state.com_raw == 0 {
        return None;
    }
    Some(ManuallyDrop::new(ID3D11Resource::from_raw(
        state.com_raw as *mut c_void,
    )))
}

/// The `ResourceState` behind a DDI resource handle, or `None` for an empty
/// slot. The single place the resource slot is decoded; every reader below
/// goes through it instead of repeating the two-step null dance.
///
/// Taking the handle rather than its `pDrvPrivate` is what makes the payload
/// follow from the handle's type: `resource_state(h_rtv)` does not resolve.
pub(crate) unsafe fn resource_state(
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> Option<&'static ResourceState> {
    boxed_slot(h_res)?.get()
}

/// `resource_state` for the runtime-tag dispatches. See `load_resource_at`.
pub(crate) unsafe fn resource_state_at(handle_priv: *mut c_void) -> Option<&'static ResourceState> {
    Slot::<Boxed<ResourceState>>::from_priv(handle_priv)?.get()
}

/// Raw ID3D11Resource COM pointer behind a DDI resource handle (0 when
/// absent) — for bridge calls that inspect the DXVK image without taking a COM
/// reference.
pub(crate) unsafe fn resource_com_raw(h_res: ddi::D3D10DDI_HRESOURCE) -> usize {
    resource_state(h_res).map_or(0, |s| s.com_raw)
}

pub(crate) unsafe fn resource_allocation(h_res: ddi::D3D10DDI_HRESOURCE) -> ddi::D3DKMT_HANDLE {
    resource_state(h_res).map_or(0, |s| {
        s.allocation
            .as_ref()
            .map(ResidentAllocation::handle)
            .unwrap_or(0)
    })
}

pub(crate) unsafe fn resource_parent_handles(
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> (ddi::HANDLE, ddi::D3DKMT_HANDLE) {
    resource_state(h_res).map_or((core::ptr::null_mut(), 0), |s| {
        (s.rt_resource, s.km_resource)
    })
}

pub(crate) unsafe fn resource_present_private(
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> Option<HeliosPresentPrivateData> {
    let p = resource_state(h_res)?.present_private;
    p.is_valid().then_some(p)
}

/// Resolve scanout metadata by the allocation Windows is actually presenting.
/// DXGI may keep one stable resource object while rotating its `hAllocation`
/// among the pPrimaryDesc ring, so resource-local metadata alone is not enough.
pub(crate) unsafe fn presented_primary_private(
    h: Hdevice,
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> Option<HeliosPresentPrivateData> {
    if let Some(private) = unsafe { resource_present_private(h_res) } {
        return Some(private);
    }
    let allocation = unsafe { resource_allocation(h_res) };
    if allocation == 0 {
        return None;
    }
    let dev = unsafe { helios_device(h) }?;
    dev.direct_scanout_allocations
        .borrow()
        .iter()
        .find_map(|(candidate, private)| (*candidate == allocation).then_some(*private))
}

pub(crate) unsafe fn remember_direct_scanout_allocation(
    h: Hdevice,
    allocation: ddi::D3DKMT_HANDLE,
    private: HeliosPresentPrivateData,
) {
    if allocation == 0 || !private.is_valid() {
        return;
    }
    let Some(dev) = (unsafe { helios_device(h) }) else {
        return;
    };
    {
        let mut entries = dev.direct_scanout_allocations.borrow_mut();
        entries.retain(|(candidate, _)| *candidate != allocation);
        entries.push((allocation, private));
    }
}

/// Drop a destroyed allocation's direct-scanout entry. Returns
/// `(removed, remaining)` when something was removed, so the caller can log the
/// list length — the property that must stay bounded.
pub(crate) unsafe fn forget_direct_scanout_allocation(
    h: Hdevice,
    allocation: ddi::D3DKMT_HANDLE,
) -> Option<(usize, usize)> {
    if allocation == 0 {
        return None;
    }
    let dev = unsafe { helios_device(h) }?;
    let mut entries = dev.direct_scanout_allocations.borrow_mut();
    let before = entries.len();
    entries.retain(|(candidate, _)| *candidate != allocation);
    let removed = before - entries.len();
    let remaining = entries.len();
    drop(entries);
    (removed != 0).then_some((removed, remaining))
}

pub(crate) fn needs_wddm_texture_allocation(a: &ddi::D3D11DDIARG_CREATERESOURCE) -> bool {
    const DDI_BIND_PRESENT: u32 = 0x0000_0080;
    const DDI_MISC_SHARED: u32 = 0x0000_0002;
    const DDI_MISC_SHARED_KEYEDMUTEX: u32 = 0x0000_0100;

    !a.pPrimaryDesc.is_null()
        || (a.BindFlags & DDI_BIND_PRESENT) != 0
        || (a.MiscFlags & (DDI_MISC_SHARED | DDI_MISC_SHARED_KEYEDMUTEX)) != 0
}

pub(crate) unsafe fn dxvk_resource_memory_info(
    h: Hdevice,
    obj: &ID3D11Resource,
) -> (u64, u64, u64, u32) {
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

pub(crate) unsafe fn resource_dimensions(h_res: ddi::D3D10DDI_HRESOURCE) -> (u32, u32) {
    let Some(res) = load_resource(h_res) else {
        return (0, 0);
    };
    let Ok(tex) = (*res).cast::<ID3D11Texture2D>() else {
        return (0, 0);
    };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    (desc.Width, desc.Height)
}

pub(crate) unsafe fn resource_sample_count(h_res: ddi::D3D10DDI_HRESOURCE) -> u32 {
    let Some(res) = load_resource(h_res) else {
        return 1;
    };
    let Ok(tex) = (*res).cast::<ID3D11Texture2D>() else {
        return 1;
    };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    desc.SampleDesc.Count.max(1)
}

/// Which 1D view shape an array size selects.
///
/// One of the two decisions that the four view-descriptor translators each
/// wrote out by hand. Purpose-scoped rather than one big `ViewShape` enum so
/// each translator's `match` is exhaustive with no catch-all: adding a shape
/// here is a compile error in every translator, which is the whole point.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tex1DShape {
    Plain,
    Array,
}

/// Which 2D view shape an (array size, sample count) pair selects.
///
/// This is the fork that was written out THREE times — RTV, DSV and SRV each
/// had `if is_msaa && ArraySize > 1 / else if is_msaa / else if ArraySize > 1
/// / else` — and the one where the wrong answer silently mis-binds a Fire
/// Strike MSAA render target rather than failing. RTV, DSV and SRV can no
/// longer disagree about which shape a resource is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tex2DShape {
    Plain,
    Array,
    Ms,
    MsArray,
}

pub(crate) const fn tex1d_shape(array_size: u32) -> Tex1DShape {
    if array_size > 1 {
        Tex1DShape::Array
    } else {
        Tex1DShape::Plain
    }
}

/// `sample_count` is the resource's, from [`resource_sample_count`], which
/// clamps to 1 for a non-texture or unloadable handle.
///
/// The evaluation order is load-bearing and matches all three originals: MSAA
/// wins over array-ness, so a multisampled array is `MsArray` and NOT
/// `Array`.
pub(crate) const fn tex2d_shape(array_size: u32, sample_count: u32) -> Tex2DShape {
    match (sample_count > 1, array_size > 1) {
        (true, true) => Tex2DShape::MsArray,
        (true, false) => Tex2DShape::Ms,
        (false, true) => Tex2DShape::Array,
        (false, false) => Tex2DShape::Plain,
    }
}

/// The sample count a translator passes to [`tex2d_shape`] when the view kind
/// has no multisampled form at all.
///
/// D3D11 has no multisampled UAV — there is no
/// `D3D11_UAV_DIMENSION_TEXTURE2DMS` — so `uav_desc` never consulted the
/// resource's sample count and must keep not consulting it. Naming the 1 says
/// that is deliberate rather than a missing `resource_sample_count` call.
pub(crate) const NO_MULTISAMPLED_FORM: u32 = 1;

pub(crate) unsafe fn resource_dxgi_format(h_res: ddi::D3D10DDI_HRESOURCE) -> DXGI_FORMAT {
    let Some(res) = load_resource(h_res) else {
        return DXGI_FORMAT(0);
    };
    let Ok(tex) = (*res).cast::<ID3D11Texture2D>() else {
        return DXGI_FORMAT(0);
    };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    desc.Format
}

pub(crate) unsafe fn resource_summary(
    h_res: ddi::D3D10DDI_HRESOURCE,
) -> (ddi::D3DKMT_HANDLE, &'static str, u32, u32, u32, u32) {
    let allocation = resource_allocation(h_res);
    let Some(res) = load_resource(h_res) else {
        return (allocation, "missing", 0, 0, 0, 0);
    };
    if let Ok(buf) = (*res).cast::<ID3D11Buffer>() {
        let mut desc = D3D11_BUFFER_DESC::default();
        buf.GetDesc(&mut desc);
        return (allocation, "buffer", desc.ByteWidth, 1, 1, 0);
    }
    if let Ok(tex) = (*res).cast::<ID3D11Texture2D>() {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut desc);
        return (
            allocation,
            "tex2d",
            desc.Width,
            desc.Height,
            desc.ArraySize,
            desc.Format.0 as u32,
        );
    }
    if let Ok(tex) = (*res).cast::<ID3D11Texture3D>() {
        let mut desc = D3D11_TEXTURE3D_DESC::default();
        tex.GetDesc(&mut desc);
        return (
            allocation,
            "tex3d",
            desc.Width,
            desc.Height,
            desc.Depth,
            desc.Format.0 as u32,
        );
    }
    (allocation, "resource", 0, 0, 0, 0)
}

pub(crate) unsafe fn store_rtv(
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
    obj: ID3D11RenderTargetView,
    resource_raw: usize,
    allocation: ddi::D3DKMT_HANDLE,
    width: u32,
    height: u32,
    format: u32,
) {
    let Some(slot) = boxed_slot(h_rtv) else {
        drop(obj);
        return;
    };
    slot.store(RtvState {
        com_raw: obj.into_raw() as usize,
        resource_raw,
        allocation,
        width,
        height,
        format,
    });
}

/// The `RtvState` behind a DDI render-target-view handle, or `None` for an
/// empty slot. The single place the RTV slot is decoded.
pub(crate) unsafe fn rtv_state(
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
) -> Option<&'static RtvState> {
    boxed_slot(h_rtv)?.get()
}

/// `rtv_state` for the runtime-tag dispatches. See `load_resource_at`.
pub(crate) unsafe fn rtv_state_at(handle_priv: *mut c_void) -> Option<&'static RtvState> {
    Slot::<Boxed<RtvState>>::from_priv(handle_priv)?.get()
}

pub(crate) unsafe fn load_rtv(
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
) -> Option<ManuallyDrop<ID3D11RenderTargetView>> {
    load_rtv_at(h_rtv.pDrvPrivate)
}

/// `load_rtv` for the runtime-tag dispatches (`Discard`, `ClearView`, the
/// tiled-resource barrier). See `load_resource_at`.
pub(crate) unsafe fn load_rtv_at(
    handle_priv: *mut c_void,
) -> Option<ManuallyDrop<ID3D11RenderTargetView>> {
    let state = rtv_state_at(handle_priv)?;
    if state.com_raw == 0 {
        return None;
    }
    Some(ManuallyDrop::new(ID3D11RenderTargetView::from_raw(
        state.com_raw as *mut c_void,
    )))
}

/// Geometry + identity of the RTV behind a handle, all zeros when absent.
///
/// `clear_rtv` used to re-derive these by casting the slot inline, duplicating
/// this function's body four lines from a `load_rtv` call on the same handle.
/// That copy is gone; the log line it feeds is unchanged. R803.
pub(crate) unsafe fn rtv_info(
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
) -> (ddi::D3DKMT_HANDLE, u32, u32, u32, usize) {
    rtv_state(h_rtv).map_or((0, 0, 0, 0, 0), |s| {
        (s.allocation, s.width, s.height, s.format, s.resource_raw)
    })
}

pub(crate) unsafe fn release_rtv(h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW) {
    let Some(slot) = boxed_slot(h_rtv) else {
        return;
    };
    // `take` empties the slot as it hands the box over, so a second release on
    // the same handle finds `None` instead of freeing twice.
    if let Some(state) = slot.take() {
        if state.com_raw != 0 {
            drop(IUnknown::from_raw(state.com_raw as *mut c_void));
        }
    }
}

pub(crate) unsafe fn release_resource(h: Hdevice, h_res: ddi::D3D10DDI_HRESOURCE) {
    let Some(slot) = boxed_slot(h_res) else {
        return;
    };
    // Take the box (and empty the slot) BEFORE the teardown below, which
    // re-enters this driver through helios_device/pfnDeallocateCb. A second
    // release of the same handle now finds an empty slot instead of freeing
    // twice. `state` is an owned Box, so the drop at the end of this scope is
    // what frees it -- the explicit `Box::from_raw` is gone.
    if let Some(mut state) = slot.take() {
        let state = &mut *state;
        let allocation = (*state)
            .allocation
            .as_ref()
            .map(ResidentAllocation::handle)
            .unwrap_or(0);
        // The direct-scanout registry was add-only: it grew without bound across
        // mode changes and DWM primary generations inside one process, the
        // per-present `presented_primary_private` lookup became a linear scan
        // over dead entries, and if dxgkrnl reissues a freed D3DKMT_HANDLE the
        // lookup returns the previous primary's resource_id/width/height/pitch
        // for the new allocation — a stale scanout identity handed to the KMD.
        if let Some((removed, remaining)) = forget_direct_scanout_allocation(h, allocation) {
            log_error!(
                "DDI scanout registry: dropped {removed} entry alloc=0x{allocation:x} remaining={remaining}"
            );
        }
        // Evict while the allocation and runtime device are both still valid.
        // Option::take makes it impossible for ResourceState::drop to evict a
        // second time after pfnDeallocateCb.
        drop((*state).allocation.take());
        if allocation != 0 || !(*state).rt_resource.is_null() {
            if let Some(dev) = helios_device(h) {
                if !dev.kt_callbacks.is_null() {
                    if let Some(deallocate_cb) = (*dev.kt_callbacks).pfnDeallocateCb {
                        // D3DDDICB_DEALLOCATE contract: EITHER hResource (the
                        // runtime releases/closes every allocation it tracks
                        // for the resource — created AND opened instances) OR
                        // NumAllocations+HandleList with hResource NULL. Both
                        // together is E_INVALIDARG (the old 0x80070057, which
                        // also leaked opened resources).
                        let mut allocation = allocation;
                        // One exhaustive decision, then one construction. The
                        // hResource-and-HandleList-together shape the runtime
                        // rejects cannot be built from any DeallocateForm.
                        let form = DeallocateForm::select(
                            (*state).rt_resource,
                            (*state).ownership,
                            allocation,
                        );
                        let mut dealloc = match form {
                            DeallocateForm::ByResource(h_resource) => ddi::D3DDDICB_DEALLOCATE {
                                hResource: h_resource,
                                NumAllocations: 0,
                                HandleList: core::ptr::null_mut(),
                            },
                            DeallocateForm::ByHandleList(handle) => {
                                // Take the handle from the form, not from the
                                // surrounding local: the form is what validated
                                // it as non-zero, and `HandleList` must point at
                                // exactly that value.
                                allocation = handle.get();
                                ddi::D3DDDICB_DEALLOCATE {
                                    hResource: core::ptr::null_mut(),
                                    NumAllocations: 1,
                                    HandleList: &mut allocation,
                                }
                            }
                            DeallocateForm::Nothing { reason } => {
                                log_error!(
                                    "DDI deallocate_resource skip: {} alloc=0x{:x} km=0x{:x}",
                                    reason,
                                    allocation,
                                    (*state).km_resource
                                );
                                ddi::D3DDDICB_DEALLOCATE {
                                    hResource: core::ptr::null_mut(),
                                    NumAllocations: 0,
                                    HandleList: core::ptr::null_mut(),
                                }
                            }
                        };
                        if !matches!(form, DeallocateForm::Nothing { .. }) {
                            let hr = deallocate_cb(dev.h_rt_device, &mut dealloc);
                            trace_line!(
                                "DDI deallocate_resource: hr=0x{:08x} alloc=0x{:x} km=0x{:x} rt={:p} owned={}",
                                hr as u32,
                                allocation,
                                (*state).km_resource,
                                (*state).rt_resource,
                                (*state).ownership.owns()
                            );
                        }
                    }
                }
            }
        }
        if (*state).com_raw != 0 {
            drop(IUnknown::from_raw((*state).com_raw as *mut c_void));
        }
    }
}

/// Borrow the COM interface stored in a DDI handle (does not take ownership).
pub(crate) unsafe fn load_com<T: Interface>(h: impl ComHandle) -> Option<ManuallyDrop<T>> {
    load_com_at::<T>(h.drv_private())
}

/// `load_com` for the three runtime-tag dispatches (`discard_11_1`,
/// `clear_view_11_1`, `tiled_barrier_child`).
///
/// Those receive one `pDrvPrivate` plus a `D3D11DDI_HANDLETYPE` that selects
/// the payload at run time, so the static `ComHandle` bound cannot apply. Each
/// already matches the tag and calls the decoder its arm proved correct --
/// `load_resource` for HT_RESOURCE, `load_rtv` for HT_RENDERTARGETVIEW, this
/// for the bare-COM view tags. Do not call it from anywhere else: it is the
/// unchecked form `load_com` exists to replace.
pub(crate) unsafe fn load_com_at<T: Interface>(
    handle_priv: *mut c_void,
) -> Option<ManuallyDrop<T>> {
    Slot::<Com<T>>::from_priv(handle_priv)?.load()
}

/// Release the COM interface stored in a DDI handle.
pub(crate) unsafe fn release_com(h: impl ComHandle) {
    if let Some(slot) = Slot::<Com<IUnknown>>::from_priv(h.drv_private()) {
        slot.release();
    }
}

pub(crate) fn cpu_access(usage: u32) -> u32 {
    // D3D11_USAGE: DEFAULT=0, IMMUTABLE=1, DYNAMIC=2, STAGING=3.
    match usage {
        2 => D3D11_CPU_ACCESS_WRITE.0 as u32,
        3 => (D3D11_CPU_ACCESS_WRITE.0 | D3D11_CPU_ACCESS_READ.0) as u32,
        _ => 0,
    }
}

pub(crate) fn api_bind_flags(ddi_bind: u32) -> u32 {
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

pub(crate) fn api_misc_flags(ddi_misc: u32, ddi_bind: u32, is_buffer: bool) -> u32 {
    const DDI_MISC_AUTO_GEN_MIP_MAP: u32 = 0x0000_0001;
    const DDI_MISC_SHARED: u32 = 0x0000_0002;
    const DDI_MISC_DRAWINDIRECT_ARGS: u32 = 0x0000_0010;
    const DDI_MISC_BUFFER_ALLOW_RAW_VIEWS: u32 = 0x0000_0020;
    const DDI_MISC_BUFFER_STRUCTURED: u32 = 0x0000_0040;
    const DDI_MISC_RESOURCE_CLAMP: u32 = 0x0000_0080;
    const DDI_MISC_SHARED_KEYEDMUTEX: u32 = 0x0000_0100;
    const DDI_MISC_GDI_COMPATIBLE: u32 = 0x0000_0200;
    const DDI_MISC_TILED: u32 = 0x0000_4000;
    const DDI_MISC_TILE_POOL: u32 = 0x0000_8000;
    const DDI_BIND_PRESENT: u32 = 0x0000_0080;
    const API_MISC_SHARED_NTHANDLE: u32 = 0x0000_0800;
    const API_MISC_TILE_POOL: u32 = 0x0002_0000;
    const API_MISC_TILED: u32 = 0x0004_0000;

    if is_buffer {
        let mut out = ddi_misc
            & (DDI_MISC_DRAWINDIRECT_ARGS
                | DDI_MISC_BUFFER_ALLOW_RAW_VIEWS
                | DDI_MISC_BUFFER_STRUCTURED
                | DDI_MISC_RESOURCE_CLAMP);
        if ddi_misc & DDI_MISC_TILE_POOL != 0 {
            out |= API_MISC_TILE_POOL;
        }
        if ddi_misc & DDI_MISC_TILED != 0 {
            out |= API_MISC_TILED;
        }
        out
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
        if ddi_misc & DDI_MISC_TILED != 0 {
            out |= API_MISC_TILED;
        }
        out
    }
}
