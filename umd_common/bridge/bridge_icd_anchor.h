// The process-global venus-ICD anchor, shared by both Helios UMD bridges.
//
// `ARCHITECTURE.md` §6.4, stage S4b. ⛔ **This file exists so that the two
// bridges cannot select DIFFERENT venus ICD modules in one process.**
//
// # The hazard, stated precisely
//
// `umd/bridge/bridge_icd_exports.cpp`'s `find_helios_icd_in_loaded_modules`
// walks `TH32CS_SNAPMODULE` and takes the first module exporting
// `helios_venus_memory_alloc_info`. Its own comment says why it selects a whole
// module rather than resolving each export independently: *"Looking up each
// export independently can mix two ICD versions and call a function with a
// foreign `VkDeviceMemory`/`VkInstance` handle."*
//
// Two UMDs, each with its own copy of that resolver, reintroduce exactly that
// hazard one level up — two independent selections, no coordination.
//
// # The mechanism, and why it is this one
//
// `ARCHITECTURE.md` §6.4 considered and REJECTED "`umd12` asks `helios_umd.dll`
// when that module is present": the *when present* clause needs a fallback for
// the pure-D3D12 process, and that fallback is the divergent per-module
// resolver it was meant to remove — so the divergence would appear only in the
// mixed process, i.e. the case nobody tests.
//
// Instead: **both cdylibs export one name**, and every resolver finds it with
// the same first-hit-wins module walk the Mesa ICD itself uses
// (`icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:711-732`). Whichever module
// the loader enumerated first becomes the single publisher for the whole
// process, whoever that is — and a lone `umd12` finds its own export and is
// trivially self-consistent. No new OS primitive, no load-order dependency
// between the two UMDs.
//
// ⛔ The name is deliberately NOT in the `helios_umd_*` family. §6.1's rule is
// that exactly ONE DLL exports those, because the Mesa ICD looks *them* up
// first-hit-wins and a duplicate would silently steal the D3D11 present
// vehicle. The ICD never looks up `helios_icd_anchor_v1`, so both DLLs
// exporting it is the mechanism rather than a collision.
//
// ⚠ Deliberately `void*` and not `HMODULE` in the public signature, so this
// header pulls in no Windows header and can sit beside `bridge_common.h`.
#pragma once

extern "C" {

/// Publish or query the process's single canonical venus-ICD module.
///
/// Called with this module's own resolution as `candidate`:
///  * the first caller in the process publishes it and gets it straight back;
///  * every later caller gets the **published** module, whatever it passed.
///
/// A null `candidate` is a pure query and publishes nothing (it answers null
/// when nothing has published yet).
///
/// ⛔ Exported from **both** cdylibs. The copy that wins is the one in the
/// module the loader enumerated first; that copy's static is the single source
/// of truth, and the other copy is never called.
///
/// Idempotent, thread-safe, and never allocates.
__declspec(dllexport) void* helios_icd_anchor_v1(void* candidate);

}  // extern "C"

namespace helios_bridge {

/// The export whose presence identifies a module as the Helios venus ICD.
///
/// ⛔ ONE spelling, shared. Both the loaded-module walk below and
/// `helios_umd.dll`'s manifest fallback (`bridge_icd_exports.cpp`, which
/// `LoadLibrary`s a candidate and then tests it) must probe for the SAME symbol:
/// two spellings would let the two paths disagree about what counts as the ICD,
/// which is a subtler version of the very divergence the anchor exists to stop.
constexpr const char* kVenusIcdProbeExport = "helios_venus_memory_alloc_info";

/// The venus ICD module already loaded in this process, or null.
///
/// Walks `TH32CS_SNAPMODULE` and takes the first module exporting
/// `helios_venus_memory_alloc_info`. ⛔ It selects a whole MODULE rather than
/// resolving each export independently, and that is load-bearing: *"Looking up
/// each export independently can mix two ICD versions and call a function with
/// a foreign `VkDeviceMemory`/`VkInstance` handle"*
/// (`umd/bridge/bridge_icd_exports.cpp`).
///
/// Holds one reference on the returned module for the process's lifetime.
///
/// ⛔ **Shared because both bridges need exactly this and nothing more.**
/// `helios_umd.dll` follows a miss with its Vulkan-ICD-manifest fallback
/// (registry + environment + a JSON `library_path` parse) and then drives a
/// nine-entry typed export table; `helios_umd12.dll` needs neither — by the
/// time it asks, vkd3d has already loaded `vulkan-1.dll` and the ICD with it,
/// and it reads exactly one export. So the WALK is shared and the two things
/// built on top of it stay where they are used. `DECISIONS.md` D3b: what would
/// be a second copy moves here; what is genuinely per-driver does not.
void* find_venus_icd_module();

/// `helios_venus_current_ctx_id()` read out of `module`, or 0.
///
/// ⚠ This is the argument-free, **thread-local** export
/// (`icd/mesa/.../vn_renderer_helios.c:639-644`) — `helios_venus_instance_ctx_id`
/// ignores its `VkInstance` argument and returns the same thread-local, which is
/// why `ARCHITECTURE.md` §6.4 requires the ctx id to be read **on the thread
/// that created the instance, synchronously**. Both bridges therefore call this
/// immediately after their engine's device create, on that same thread.
///
/// Returns 0 for an implausible value rather than a wrong one: KMD-assigned
/// context ids are small monotonically allocated integers, so anything
/// >= 0x0100_0000 means the export decoded the wrong object.
unsigned int read_venus_ctx_id(void* module);

/// Reconcile this module's own ICD resolution against the process anchor.
///
/// Returns the module the caller should use, or **null to REFUSE**.
///
/// Null is returned only when the published anchor is a *different* module from
/// `candidate`. ⛔ That case must not silently adopt the published one: the
/// Vulkan objects this module already holds came from whichever ICD the *Vulkan
/// loader* bound — `resolve_helios_icd_module` runs lazily, after the instance
/// exists — so calling a different module's `helios_venus_instance_ctx_id` with
/// them is precisely the foreign-handle bug the anchor exists to prevent.
/// Adopting would *cause* it; refusing makes it loud.
///
/// Increments `IcdAnchorMismatch` and logs on the mismatch path.
void* reconcile_icd_anchor(void* candidate);

/// Whether a mismatch has ever been observed in this process.
///
/// Sticky, and it is what the device-create paths consult: `ARCHITECTURE.md`
/// §6.4 requires the resolver to **refuse device creation** on a mismatch
/// (`E_FAIL` out of the bridge, `DXGI_ERROR_UNSUPPORTED` out of
/// `OpenAdapter12`) rather than degrade to an "export unavailable" miss, which
/// is what a bare null return would become.
bool icd_anchor_poisoned();

/// `IcdAnchorMismatch` — expected **0**. Non-zero means two venus ICD modules
/// are live in one process and this driver refused rather than mix them.
unsigned int icd_anchor_mismatch_count();

}  // namespace helios_bridge
