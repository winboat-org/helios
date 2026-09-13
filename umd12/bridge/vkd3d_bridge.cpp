#include <vector>
// Helios D3D12 UMD <-> vkd3d engine bridge implementation.
//
// Wraps the one `ID3D12Device*` the engine hands back behind the opaque
// `HeliosVkd3dDevice`. Structurally this is `umd/bridge/dxvk_bridge.cpp`'s
// create-device path (:1640-1704) with DXVK swapped for vkd3d, which is exactly
// what `DECISIONS.md` D4 says the D3D12 UMD is — but it shares no *code* with
// it: D3b forbids copying from `umd/`, so everything both bridges need already
// lives in `umd_common/bridge/` and is included, not duplicated.
//
// ⭐ The engine is STATICALLY LINKED (D4, flipped by the owner 2026-08-05).
// There is no `helios_vkd3d.dll`, no `LoadLibrary`, no `GetProcAddress` and no
// module pin anywhere in this file. `helios_vkd3d_create_device` and
// `helios_vkd3d_serialize_root_signature` are ARCHIVE symbols pulled out of
// `libhelios_d3d12_static.a` by the linker, declared below and called directly.
// The measured link set is that ONE archive plus `gdi32`
// (`tmp/dx12/gates/G1-static/RESULT.md`) — ⛔ never `dxgi`: a WDDM user-mode
// driver sits BELOW DXGI, and loading dxgi during device creation risks
// re-entering the adapter enumeration that loaded this very DLL
// (`vkd3d-proton-helios/libs/d3d12core/helios_entry.c:19-28`).
//
// ⛔ `HELIOS_BRIDGE_ENGINE_CATCH` is deliberately NOT defined here. It is
// `bridge_guard.h`'s one customization point and exists for `dxvk::DxvkError`,
// which is not a `std::exception`. vkd3d's C entry points can reach C++ shader
// compiler allocations; the shared generic arms contain escaping exceptions.

// ⚠ Guarded, not bare `#define`s as `dxvk_bridge.cpp:8-9` has them: `build.rs`
// passes both on the clang-cl command line for this crate, and a redefinition
// with a different replacement list (`-DNOMINMAX` is `1`, a bare `#define` is
// empty) is a diagnostic. Kept at all so the file still compiles standalone,
// which is how it was syntax-checked on the Linux host.
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>

#include <d3d12.h>

#include "vkd3d_bridge.h"

// ⚠ These two resolve to `umd_common/bridge/`, not to this directory —
// `build.rs` adds it to the include path after `bridge/`, so a same-named header
// in this crate would win (`DECISIONS.md` D3b: one copy of the source, shared
// with the D3D11 bridge).
#include "bridge_common.h"
#include "bridge_guard.h"
#include "bridge_icd_anchor.h"

#include <atomic>
#include <cstdio>
#include <cstdlib>
#include <mutex>
#include <new>
#include <share.h>

// ── the two engine entry points ─────────────────────────────────────────────
//
// Declared here rather than by including a vkd3d header: `vkd3d.h:43-49` pulls
// in `vulkan.h` and vkd3d's widl `D3D12_*` types, which collide with the SDK's.
// Definitions: `vkd3d-proton-helios/libs/d3d12core/helios_entry.c:112` and
// `:190`. Both are `extern "C"`; a C++-mangled declaration would link against
// nothing and the failure would be a link error, not a runtime one — but only
// if nothing else in the link happens to define the mangled name, so the
// `extern "C"` here is load-bearing, not decorative.
extern "C" HRESULT helios_vkd3d_create_device(LUID adapter_luid, REFIID iid,
                                              void** device);
extern "C" HRESULT helios_vkd3d_validate_native_feature_level(ID3D12Device* device,
    std::uint32_t minimum_feature_level, std::uint32_t* shader_model,
    std::uint32_t* raytracing_tier, std::uint8_t* device_uuid) noexcept(false);
extern "C" HRESULT helios_vkd3d_serialize_root_signature(
    const D3D12_ROOT_SIGNATURE_DESC* desc, D3D_ROOT_SIGNATURE_VERSION version,
    ID3DBlob** blob, ID3DBlob** error_blob);
extern "C" HRESULT helios_vkd3d_create_root_signature(ID3D12Device* device, UINT node_mask,
    const D3D12_VERSIONED_ROOT_SIGNATURE_DESC* desc, ID3D12RootSignature** root) noexcept(false);
extern "C" HRESULT helios_vkd3d_clear_root_arguments(ID3D12GraphicsCommandList* list) noexcept(false);
// vkd3d's public queue API (include/vkd3d.h): the *only* supported way to reach
// the VkQueue the ICD export needs, and the same lock the submission drain uses.
// Declared here rather than including vkd3d.h, whose widl-generated D3D12 types
// collide with the SDK's (see vkd3d_bridge.h's header rule).
extern "C" void* vkd3d_acquire_vk_queue(ID3D12CommandQueue* queue);
extern "C" void vkd3d_release_vk_queue(ID3D12CommandQueue* queue);
// Same SDK/widl COM ABI as the public pipeline factory; the separate symbol
// supplies SO origin without reserving a legal application semantic name.
extern "C" HRESULT helios_vkd3d_create_stream_output_pipeline(ID3D12Device* device,
    const D3D12_PIPELINE_STATE_STREAM_DESC* desc, ID3D12PipelineState** pipeline) noexcept(false);

std::int32_t helios_vkd3d_bridge_create_stream_output_pipeline(
    std::size_t device, std::size_t desc, std::size_t* pipeline_out) noexcept {
  if (!pipeline_out) return E_INVALIDARG;
  *pipeline_out = 0;
  if (!device || !desc) return E_INVALIDARG;
  // /EHsc assumes an extern C call cannot throw unless explicitly declared
  // otherwise above. The C engine reaches the C++ DXIL compiler. Contain an
  // escaping exception at this Rust boundary; this does not repair compiler
  // allocations owned by C frames skipped during unwinding.
  return helios_bridge::bridge_guard("create_stream_output_pipeline",
      std::int32_t(E_FAIL), [&]() -> std::int32_t {
        try {
          ID3D12PipelineState* pipeline = nullptr;
          const HRESULT hr = helios_vkd3d_create_stream_output_pipeline(
              reinterpret_cast<ID3D12Device*>(device),
              reinterpret_cast<const D3D12_PIPELINE_STATE_STREAM_DESC*>(desc), &pipeline);
          if (SUCCEEDED(hr)) *pipeline_out = reinterpret_cast<std::size_t>(pipeline);
          return hr;
        } catch (const std::bad_alloc&) {
          // Preserve OOM without diagnostic allocation. The native DDI counts
          // the failure before returning it; its output slot remains clear.
          return E_OUTOFMEMORY;
        }
      });
}

// ── ID3D12DXVKInteropDevice4, hand-declared ─────────────────────────────────
//
// ⛔ This file includes no vkd3d header (see the banner), so the interop interface
// vkd3d publishes in `include/vkd3d_device_vkd3d_ext.idl` is redeclared here rather
// than imported. Transcribed from that IDL, in vtable order, against the shipped
// `d3d12_dxvk_interop_device_vtbl` (`libs/vkd3d/device_vkd3d_ext.c`) — **22 slots**,
// three IUnknown plus nineteen, and the one this bridge calls is the LAST.
//
// ⚠ **An abstract class and not a hand-built vtable struct**, deliberately: a
// single-inheritance class with only pure virtuals has exactly the COM vtable
// layout under MSVC/clang-cl, and it makes a mis-ordered slot a compile-visible
// list rather than an index arithmetic error. ⛔ But the ORDER is still the whole
// correctness of it: a method inserted anywhere above the last one silently calls
// the wrong function. The IDL's interface chain is the authority
// (v -> v1 -> v2 -> v3 -> v4), and every method of every revision must appear
// here even though this bridge calls two.
//
// Vulkan dispatchable handles remain pointer-sized on both architectures;
// non-dispatchable resource/memory handles are uint64_t in this interop IDL.
// VkImageLayout and VkFormat are int-sized enums. Preserve these widths even
// when the UMD itself is x86.
struct ID3D12DXVKInteropDevice4 : public IUnknown {
  // ID3D12DXVKInteropDevice
  virtual HRESULT STDMETHODCALLTYPE GetDXGIAdapter(REFIID iid, void** object) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetInstanceExtensions(UINT* count, const char** ext) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetDeviceExtensions(UINT* count, const char** ext) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetDeviceFeatures(const void** features) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetVulkanHandles(void** vk_instance,
                                                    void** vk_physical_device,
                                                    void** vk_device) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetVulkanQueueInfo(ID3D12CommandQueue* queue,
                                                      void** vk_queue,
                                                      std::uint32_t* vk_queue_family) = 0;
  virtual void STDMETHODCALLTYPE GetVulkanImageLayout(ID3D12Resource* resource,
                                                      D3D12_RESOURCE_STATES state,
                                                      int* vk_layout) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetVulkanResourceInfo(ID3D12Resource* resource,
                                                          std::uint64_t* vk_handle,
                                                          std::uint64_t* buffer_offset) = 0;
  virtual HRESULT STDMETHODCALLTYPE LockCommandQueue(ID3D12CommandQueue* queue) = 0;
  virtual HRESULT STDMETHODCALLTYPE UnlockCommandQueue(ID3D12CommandQueue* queue) = 0;
  // ID3D12DXVKInteropDevice1
  virtual HRESULT STDMETHODCALLTYPE GetVulkanResourceInfo1(ID3D12Resource* resource,
                                                           std::uint64_t* vk_handle,
                                                           std::uint64_t* buffer_offset,
                                                           int* format) = 0;
  virtual HRESULT STDMETHODCALLTYPE CreateInteropCommandQueue(
      const D3D12_COMMAND_QUEUE_DESC* desc, std::uint32_t vk_queue_family_index,
      ID3D12CommandQueue** queue) = 0;
  virtual HRESULT STDMETHODCALLTYPE CreateInteropCommandAllocator(
      D3D12_COMMAND_LIST_TYPE type, std::uint32_t vk_queue_family_index,
      ID3D12CommandAllocator** allocator) = 0;
  virtual HRESULT STDMETHODCALLTYPE BeginVkCommandBufferInterop(ID3D12CommandList* list,
                                                                 void** command_buffer) = 0;
  virtual HRESULT STDMETHODCALLTYPE EndVkCommandBufferInterop(ID3D12CommandList* list) = 0;
  // ID3D12DXVKInteropDevice2
  virtual HRESULT STDMETHODCALLTYPE LockVulkanQueue(ID3D12CommandQueue* queue) = 0;
  virtual HRESULT STDMETHODCALLTYPE UnlockVulkanQueue(ID3D12CommandQueue* queue) = 0;
  // ID3D12DXVKInteropDevice3
  virtual HRESULT STDMETHODCALLTYPE GetVulkanHeapInfo(ID3D12Heap* heap,
                                                      std::uint64_t* vk_memory,
                                                      std::uint64_t* heap_offset,
                                                      std::uint32_t* vk_memory_type) = 0;
  // ID3D12DXVKInteropDevice4 — the one this bridge exists to call.
  virtual HRESULT STDMETHODCALLTYPE GetVulkanResourceMemoryInfo(
      ID3D12Resource* resource, std::uint64_t* vk_memory, std::uint64_t* memory_offset,
      std::uint64_t* memory_size, std::uint32_t* vk_memory_type) = 0;
};

// `uuid(9c0850e7-70f1-4229-ae05-440b387ec517)` from the IDL block above.
// ⛔ Spelled out rather than `__uuidof`: `__uuidof` needs a `DECLSPEC_UUID` on the
// declaration, and a wrong literal here would be a QueryInterface miss (loud,
// counted) rather than a wrong call — which is why the miss path is a named status
// and not an assumption.
static const GUID IID_HeliosID3D12DXVKInteropDevice4 = {
    0x9c0850e7, 0x70f1, 0x4229, {0xae, 0x05, 0x44, 0x0b, 0x38, 0x7e, 0xc5, 0x17}};

namespace helios_bridge {

// ── named counters ──────────────────────────────────────────────────────────
//
// AGENTS.md operating rule 2: every skipped or refused path gets a named
// counter, so "it silently did nothing" is never a possible reading of a log.
// These are process-local atomics rather than registry counters because this is
// user mode and the log is per-process and per-pid anyway; each is also logged
// at the moment it increments, with its running value.
std::atomic<std::uint32_t> g_vkd3dCreateDeviceFailed{0};   // engine returned a failure HRESULT
std::atomic<std::uint32_t> g_vkd3dCreateDeviceNullOut{0};  // engine returned S_OK with a null device
std::atomic<std::uint32_t> g_vkd3dSerializeBadArg{0};      // serialize refused: null desc/blob_out
std::atomic<std::uint32_t> g_vkd3dDrainBadArg{0}; // retired, diagnostic index preserved
std::atomic<std::uint32_t> g_vkd3dDrainNotAcquired{0}; // retired, diagnostic index preserved
std::atomic<std::uint32_t> g_vkd3dQueueFenceZero{0}; // retired, diagnostic index preserved
std::atomic<std::uint32_t> g_vkd3dNoInteropDevice{0};      // QI for ID3D12DXVKInteropDevice4 failed
std::atomic<std::uint32_t> g_vkd3dIdentityEngineRefused{0};// GetVulkanResourceMemoryInfo failed
std::atomic<std::uint32_t> g_vkd3dIdentityIcdRefused{0};   // the ICD memory exports answered 0
std::atomic<std::uint32_t> g_vkd3dOwnershipTransferFailed{0}; // transfer_resource_ownership gave 0
std::atomic<std::uint32_t> g_vkd3dSampleBadArg{0}; // retired, diagnostic index preserved
std::atomic<std::uint32_t> g_vkd3dSampleNotLocked{0}; // retired, diagnostic index preserved

// ── this DLL's `umd_log` ────────────────────────────────────────────────────
//
// ⛔ `bridge_common.h:35` declares `umd_log` and every bridge DEFINES its own.
// `helios_umd.dll` writes `umd-<pid>.log` with a `[dxvk-bridge] ` prefix; this
// one writes `umd12-<pid>.log` with `[vkd3d-bridge] `. Two drivers appending to
// one file would interleave unreadably and would break the per-module evidence
// discipline that reads them.
//
// Magic static, and a fixed `char[]` rather than the `std::string` the D3D11
// bridge's version holds (`dxvk_bridge.cpp:190-200`): this function is called
// from `bridge_guard`'s `catch (const std::exception&)` arm, which is reachable
// on `std::bad_alloc`, and an allocation inside a bad_alloc handler can throw
// again. Same rule as the guard's own arms.
static const char* umd_log_file() {
  struct LogPath { char buf[MAX_PATH]; };
  static const LogPath path = [] {
    LogPath p{};
    CreateDirectoryA("C:\\ProgramData\\Helios", nullptr);
    _snprintf_s(p.buf, sizeof(p.buf), _TRUNCATE,
                "C:\\ProgramData\\Helios\\umd12-%lu.log",
                (unsigned long)GetCurrentProcessId());
    return p;
  }();
  return path.buf;
}

void umd_log(const char* msg) {
  // ⛔ `_fsopen` with `_SH_DENYNO`, NEVER `fopen_s`. `fopen_s` opens
  // `_SH_SECURE` (deny-sharing); the Rust side of this same DLL holds a
  // persistent handle to this very file (`helios_umd_common::log`), so every
  // `fopen_s` here would fail and ALL bridge logging would silently vanish.
  // That already happened once on the D3D11 side — found in the 18th session,
  // where the DriverStore UMD contained the strings and the logs contained no
  // `[dxvk-bridge] ` lines at all (`dxvk_bridge.cpp:202-214`).
  FILE* f = _fsopen(umd_log_file(), "a", _SH_DENYNO);
  if (f) {
    fprintf(f, "[vkd3d-bridge] %s\n", msg);
    fclose(f);
  }
}

}  // namespace helios_bridge

using helios_bridge::umd_log;

// ── the pimpl ───────────────────────────────────────────────────────────────
//
// ⛔ No `FreeLibrary` and no module pin, unlike the shape a DLL-hosted engine
// would need: under D4-static the engine's code is in `helios_umd12.dll`'s own
// image, so there is no module whose lifetime could end under a live device.
struct HeliosVkd3dDeviceImpl {
  ID3D12Device* d3d12 = nullptr;
  std::uint32_t minimum_feature_level = 0;

  // UP-2c. The engine's interop interface, queried ONCE at device create.
  //
  // ⛔ Once, and for the same class of reason the ICD export is resolved once: a
  // per-resource `QueryInterface` is an `AddRef`/`Release` pair per create on the
  // device that owns the whole Vulkan device, and a missed `Release` on that path
  // is the 54th session's per-object leak with the largest possible object. Held
  // as an OWNING reference, released below.
  //
  // ⚠ `nullptr` is a legal state, not a failure of device creation: an engine
  // build without `ID3D12DXVKInteropDevice4` still renders, it just cannot hand
  // out a present identity. Counted (`Vkd3dNoInteropDevice`) and reported as
  // `HELIOS_VKD3D_IDENTITY_NO_INTEROP`.
  ID3D12DXVKInteropDevice4* interop = nullptr;
  // S4b. Read ON THE CREATING THREAD — see `read_venus_ctx_id_now` below.
  std::uint32_t venus_ctx_id = 0;
  // UP-5. The instance-scoped ctx id, also read on the creating thread. This is
  // the one that may be stamped into an identity; see the header.
  std::uint32_t venus_instance_ctx_id = 0;

  ~HeliosVkd3dDeviceImpl() {
    // ⛔ The interop interface FIRST: it is a reference on the same object as
    // `d3d12` (vkd3d's `QueryInterface` hands back the containing
    // `d3d12_device`), so releasing `d3d12` first would leave this one pointing
    // at an object whose last reference we are about to drop through a different
    // pointer. Order is free insurance here and would be a use-after-free if the
    // two were ever separate objects.
    if (interop) {
      interop->Release();
      interop = nullptr;
    }
    if (d3d12) {
      d3d12->Release();
      d3d12 = nullptr;
    }
  }
};

namespace {

/// Resolve the process's canonical venus ICD and read its context id, **on the
/// calling thread**.
///
/// ⚠ The thread matters and is not a detail. `helios_venus_instance_ctx_id`
/// ignores its `VkInstance` argument and returns a `_Thread_local`
/// (`icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:639-644`), so an engine
/// that creates its instance on one thread and asks on another gets 0 or
/// another thread's answer. vkd3d creates its `VkInstance` on the calling
/// thread of `vkd3d_create_instance`, which is the thread inside
/// `helios_vkd3d_create_device` — so this is called from there, synchronously,
/// immediately after the device comes back. `ARCHITECTURE.md` §6.4; the D3D11
/// bridge draws the same line at `dxvk_bridge.cpp`'s
/// `read_instance_venus_context_id` right after `new DxvkInstance`.
///
/// Returns false only when the process ANCHOR refused — two venus ICD modules
/// in one process. A ctx id of 0 is not a refusal (an old ICD without the
/// export is a degraded read, not a coherence failure).
bool read_venus_ctx_id_now(std::uint32_t* out) {
  *out = 0;
  void* icd = helios_bridge::find_venus_icd_module();
  if (!icd) {
    umd_log("venus ctx id: no loaded module exports helios_venus_memory_alloc_info");
    return true;
  }
  // ⛔ THE S4b STEP. Both UMDs run this; whichever loaded first published, and
  // a disagreement means this process has two ICD builds live.
  void* canonical = helios_bridge::reconcile_icd_anchor(icd);
  if (!canonical) {
    return false;   // IcdAnchorMismatch already counted and logged
  }
  *out = helios_bridge::read_venus_ctx_id(canonical);
  return true;
}

// ── the venus MEMORY identity exports, resolved ONCE ────────────────────────
//
// UP-2c. Four exports out of the same anchored module, all from
// `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c`:
//
//   `helios_venus_memory_res_id` (:698)                       -> the virtio resource id
//   `helios_venus_memory_alloc_info` (:714)                   -> {allocationSize, memoryTypeIndex}
//   `helios_venus_memory_transfer_resource_ownership` (:843)  -> hand the resource over
//   `helios_venus_instance_ctx_id` (:681)                     -> the thread-local ctx id
//
// ⛔ **One table, one resolution, and the module reference argument from
// applies here**: `find_venus_icd_module()` takes a
// module reference on every successful call and nothing releases it, so resolving
// per resource would leak one ICD module reference per `pfnCreateHeapAndResource`.
// ⛔ And it goes through `reconcile_icd_anchor`, never a bare
// `find_venus_icd_module`, because a mismatch must REFUSE: these functions decode a
// `VkDeviceMemory` that came from whichever image the Vulkan loader bound, and
// handing it to a different ICD build is precisely the foreign-handle bug S4b
// exists to prevent.
//
// Match Vulkan's VK_DEFINE_NON_DISPATCHABLE_HANDLE on each architecture.
// VkDeviceMemory stays 64 bits on x86; reducing it to a pointer both truncates
// its value and places subsequent cdecl arguments at the wrong stack offsets.
#if INTPTR_MAX == INT64_MAX
using VenusDeviceMemory = void*;
static VenusDeviceMemory venus_device_memory(std::uint64_t value) noexcept {
  return reinterpret_cast<VenusDeviceMemory>(value);
}
#else
using VenusDeviceMemory = std::uint64_t;
static VenusDeviceMemory venus_device_memory(std::uint64_t value) noexcept {
  return value;
}
#endif
static_assert(sizeof(VenusDeviceMemory) == sizeof(std::uint64_t));

// ⚠ **All four are resolved or none is.** The ICD exports them from the same
// image, so a module with `helios_venus_memory_res_id` but not
// `helios_venus_memory_alloc_info` is not an old ICD, it is a broken one — and
// filling half an identity is worse than filling none, because the halves are what
// a later reader compares to detect drift. `NO_EXPORT` therefore covers the whole
// group.
using MemoryResIdFn = std::uint32_t(__cdecl*)(VenusDeviceMemory);
using MemoryAllocInfoFn = bool(__cdecl*)(VenusDeviceMemory, std::uint64_t*,
                                         std::uint32_t*);
using MemoryTransferOwnershipFn = std::uint32_t(__cdecl*)(VenusDeviceMemory);
using InstanceCtxIdFn = std::uint32_t(__cdecl*)(void* /*VkInstance*/);

struct MemoryIdentityExports {
  MemoryResIdFn res_id = nullptr;
  MemoryAllocInfoFn alloc_info = nullptr;
  MemoryTransferOwnershipFn transfer_ownership = nullptr;
  InstanceCtxIdFn instance_ctx_id = nullptr;
  // `HELIOS_VKD3D_IDENTITY_NO_ICD` / `_NO_EXPORT` / `_RESOLVED`, decided once.
  std::uint32_t status = HELIOS_VKD3D_IDENTITY_NO_ICD;
};

// `bool helios_venus_queue_gpu_fence(VkQueue, uint64_t *)` — the ICD's own
// `__cdecl` export. The VkQueue crosses as `void*`: VkQueue is a dispatchable
// pointer handle on Windows x64, and naming it would drag vulkan.h across this
// seam for no gain.
using QueueGpuFenceFn = bool(__cdecl*)(void*, std::uint64_t*);

struct QueueGpuFenceExport {
  QueueGpuFenceFn fn = nullptr;
};

const QueueGpuFenceExport& queue_gpu_fence_export() {
  static const QueueGpuFenceExport resolved = [] {
    QueueGpuFenceExport out;
    void* candidate = helios_bridge::find_venus_icd_module();
    if (!candidate) return out;
    void* canonical = helios_bridge::reconcile_icd_anchor(candidate);
    if (!canonical) return out;
    out.fn = reinterpret_cast<QueueGpuFenceFn>(reinterpret_cast<void*>(
        GetProcAddress(static_cast<HMODULE>(canonical), "helios_venus_queue_gpu_fence")));
    if (!out.fn) {
      // Not an error: an ICD predating the export means D3D12 packets retire at
      // worker completion exactly as they did before, and the KMD counts that
      // separately (`D12Fn0`) instead of trusting a silent zero.
      umd_log("queue_gpu_fence: venus ICD does not export helios_venus_queue_gpu_fence "
              "-- D3D12 packets keep retiring at worker completion");
    }
    return out;
  }();
  return resolved;
}

std::uint64_t helios_umd12_queue_gpu_fence(std::size_t queue) noexcept {
  if (!queue) return 0;
  const QueueGpuFenceExport& resolved = queue_gpu_fence_export();
  if (!resolved.fn) return 0;

  auto* command_queue = reinterpret_cast<ID3D12CommandQueue*>(queue);
  void* vk_queue = vkd3d_acquire_vk_queue(command_queue);
  if (!vk_queue) return 0;

  std::uint64_t fence = 0;
  // A refusal leaves `fence` at 0 and is not an error: the KMD treats 0 as "no
  // boundary", which is the pre-existing behaviour rather than a wrong fence.
  resolved.fn(vk_queue, &fence);
  vkd3d_release_vk_queue(command_queue);
  return fence;
}

const MemoryIdentityExports& memory_identity_exports() {
  static const MemoryIdentityExports resolved = [] {
    MemoryIdentityExports out;
    void* candidate = helios_bridge::find_venus_icd_module();
    if (!candidate) {
      umd_log("memory_identity: no loaded module exports the venus ICD probe symbol "
              "-- no D3D12 resource can carry a present identity");
      return out;
    }
    void* canonical = helios_bridge::reconcile_icd_anchor(candidate);
    if (!canonical) {
      // IcdAnchorMismatch is already counted and logged by the anchor.
      umd_log("memory_identity: venus ICD anchor mismatch -- refusing to resolve the "
              "memory identity exports");
      return out;
    }
    HMODULE m = static_cast<HMODULE>(canonical);
    // Convert through `void*`: a function
    // pointer is not `reinterpret_cast`-able from `FARPROC` without a
    // -Wcast-function-type diagnostic on clang-cl.
    out.res_id = reinterpret_cast<MemoryResIdFn>(
        reinterpret_cast<void*>(GetProcAddress(m, "helios_venus_memory_res_id")));
    out.alloc_info = reinterpret_cast<MemoryAllocInfoFn>(
        reinterpret_cast<void*>(GetProcAddress(m, "helios_venus_memory_alloc_info")));
    out.transfer_ownership = reinterpret_cast<MemoryTransferOwnershipFn>(
        reinterpret_cast<void*>(
            GetProcAddress(m, "helios_venus_memory_transfer_resource_ownership")));
    out.instance_ctx_id = reinterpret_cast<InstanceCtxIdFn>(
        reinterpret_cast<void*>(GetProcAddress(m, "helios_venus_instance_ctx_id")));
    if (!out.res_id || !out.alloc_info || !out.transfer_ownership ||
        !out.instance_ctx_id) {
      char msg[224];
      std::snprintf(msg, sizeof(msg),
                    "memory_identity: the anchored venus ICD is missing an export "
                    "(res_id=%d alloc_info=%d transfer=%d instance_ctx=%d) -- no D3D12 "
                    "resource can carry a present identity",
                    out.res_id != nullptr, out.alloc_info != nullptr,
                    out.transfer_ownership != nullptr, out.instance_ctx_id != nullptr);
      umd_log(msg);
      out.res_id = nullptr;
      out.alloc_info = nullptr;
      out.transfer_ownership = nullptr;
      out.instance_ctx_id = nullptr;
      out.status = HELIOS_VKD3D_IDENTITY_NO_EXPORT;
      return out;
    }
    umd_log("memory_identity: resolved the four venus memory identity exports from "
            "the anchored venus ICD");
    out.status = HELIOS_VKD3D_IDENTITY_RESOLVED;
    return out;
  }();
  return resolved;
}

}  // namespace

HeliosVkd3dDevice::HeliosVkd3dDevice() noexcept = default;

// Defined out-of-line here, where `HeliosVkd3dDeviceImpl` is complete — that is
// the whole point of the declaration in the header.
HeliosVkd3dDevice::~HeliosVkd3dDevice() = default;

std::size_t HeliosVkd3dDevice::d3d12_device_ptr() const noexcept {
  // BORROWED. No `AddRef`: the caller is looking at the reference this object
  // owns and must not release it. ⚠ Adopting this on the Rust side into an
  // owning `ID3D12Device` is a double release at drop, and the crash lands
  // nowhere near here.
  return impl ? reinterpret_cast<std::size_t>(impl->d3d12) : 0;
}

bool HeliosVkd3dDevice::native_optional_caps(std::uint32_t& maximum_feature_level,
    std::uint32_t& shader_model,
    std::uint32_t& raytracing_tier, rust::Slice<std::uint8_t> device_uuid) const noexcept {
  if (!impl || !impl->d3d12 || device_uuid.size() != 16)
    return false;
  // Revalidate even when adopting the discovery engine: an environment override
  // installed after adapter discovery must not bypass native admission.
  if (FAILED(helios_vkd3d_validate_native_feature_level(impl->d3d12,
      impl->minimum_feature_level, &shader_model, &raytracing_tier, device_uuid.data())))
    return false;
  // Higher native levels have additional backing requirements. Their absence
  // must not discard a device that satisfies the baseline contract.
  const HRESULT extended = helios_vkd3d_validate_native_feature_level(impl->d3d12,
      D3D_FEATURE_LEVEL_12_1, &shader_model, &raytracing_tier, device_uuid.data());
  if (FAILED(extended) && extended != DXGI_ERROR_UNSUPPORTED)
    return false;
  maximum_feature_level = SUCCEEDED(extended) ? D3D_FEATURE_LEVEL_12_1 : impl->minimum_feature_level;
  return true;
}

std::uint32_t HeliosVkd3dDevice::venus_context_id() const noexcept {
  // Already read, on the creating thread, at device-create time. ⛔ Do NOT
  // re-read it here: this accessor can be called from any thread and the ICD's
  // export is thread-local, so a lazy read would answer 0 or another thread's
  // context on every caller but the first.
  return impl ? impl->venus_ctx_id : 0;
}

std::uint32_t HeliosVkd3dDevice::venus_instance_context_id() const noexcept {
  // Same rule, and it binds harder here: `helios_venus_instance_ctx_id` returns a
  // `_Thread_local`, so on any thread but the creating one it answers 0.
  return impl ? impl->venus_instance_ctx_id : 0;
}

namespace {

/// The `VkDeviceMemory` an `ID3D12Resource` is bound to, plus its geometry, out of
/// the engine's interop interface.
///
/// Shared by `resource_venus_identity` and `transfer_resource_ownership` because
/// both need exactly this and getting it twice by two spellings is how they come to
/// disagree about which memory a resource is bound to.
///
/// Returns one of the `HELIOS_VKD3D_IDENTITY_*` statuses; `*out_vk_memory` is
/// non-zero only on `_RESOLVED`.
std::uint32_t engine_resource_memory(const HeliosVkd3dDeviceImpl* impl,
                                    std::size_t resource,
                                    std::uint64_t* out_vk_memory,
                                    std::uint64_t* out_memory_offset,
                                    std::uint64_t* out_memory_size,
                                    std::uint32_t* out_memory_type_index) {
  *out_vk_memory = 0;
  *out_memory_offset = 0;
  *out_memory_size = 0;
  *out_memory_type_index = 0;

  if (!impl || !resource) {
    return HELIOS_VKD3D_IDENTITY_BAD_ARG;
  }
  if (!impl->interop) {
    // Counted at device create, where the QI actually failed; counted again here
    // because "the engine has no interop interface" and "N resources could not get
    // an identity because of it" are different numbers.
    helios_bridge::g_vkd3dNoInteropDevice.fetch_add(1, std::memory_order_relaxed);
    return HELIOS_VKD3D_IDENTITY_NO_INTEROP;
  }

  auto* res = reinterpret_cast<ID3D12Resource*>(resource);
  const HRESULT hr = impl->interop->GetVulkanResourceMemoryInfo(
      res, out_vk_memory, out_memory_offset, out_memory_size, out_memory_type_index);
  if (FAILED(hr) || *out_vk_memory == 0) {
    // ⛔ `|| == 0` as well as the HRESULT: the method returns E_FAIL for a resource
    // with no bound device memory, and a hypothetical S_OK with a null handle would
    // otherwise become a zero identity labelled as a real one.
    *out_vk_memory = 0;
    const std::uint32_t n =
        helios_bridge::g_vkd3dIdentityEngineRefused.fetch_add(1, std::memory_order_relaxed) + 1;
    // ⛔ The budget is computed from `n`, NOT from a second
    // `bridge_log_budget(g_vkd3dIdentityEngineRefused, ...)` call: that helper does
    // its own `fetch_add`, so it would count every refusal twice and the counter
    // this file publishes would be exactly double the truth.
    if (n <= 8 || (n % 4096) == 0) {
      char msg[192];
      std::snprintf(msg, sizeof(msg),
                    "resource_venus_identity: GetVulkanResourceMemoryInfo(%p) hr=0x%08lx "
                    "(Vkd3dIdentityEngineRefused=%u)",
                    (void*)res, (unsigned long)hr, n);
      umd_log(msg);
    }
    return HELIOS_VKD3D_IDENTITY_ENGINE_REFUSED;
  }
  return HELIOS_VKD3D_IDENTITY_RESOLVED;
}

}  // namespace

bool HeliosVkd3dDevice::resource_venus_identity(
    std::size_t resource, std::uint64_t* out_vk_memory, std::uint64_t* out_memory_offset,
    std::uint64_t* out_memory_size, std::uint32_t* out_memory_type_index,
    std::uint32_t* out_venus_res_id, std::uint64_t* out_venus_alloc_size,
    std::uint32_t* out_status) const noexcept {
  // ⛔ Every out-param cleared FIRST, before anything that can fail or throw. A
  // caller that read an untouched pair after a `false` return would take stack
  // garbage for a venus resource id — and a wrong resource id is an allocation
  // adopting somebody else's memory, which the KMD accepts as readily as the right
  // one (it validates liveness, not provenance).
  if (out_vk_memory) *out_vk_memory = 0;
  if (out_memory_offset) *out_memory_offset = 0;
  if (out_memory_size) *out_memory_size = 0;
  if (out_memory_type_index) *out_memory_type_index = 0;
  if (out_venus_res_id) *out_venus_res_id = 0;
  if (out_venus_alloc_size) *out_venus_alloc_size = 0;
  if (out_status) *out_status = HELIOS_VKD3D_IDENTITY_BAD_ARG;

  return helios_bridge::bridge_guard(
      "helios_vkd3d_bridge_resource_venus_identity", false, [&]() -> bool {
        if (!out_vk_memory || !out_memory_offset || !out_memory_size ||
            !out_memory_type_index || !out_venus_res_id || !out_venus_alloc_size ||
            !out_status) {
          // ⛔ All-or-nothing, unlike the fence pair's both-or-neither: a partial
          // identity has no legal meaning. The caller is `bridge12.rs`, which passes
          // seven live locals, so this is a contract check and not a supported mode.
          return false;
        }

        std::uint32_t status = engine_resource_memory(
            impl.get(), resource, out_vk_memory, out_memory_offset, out_memory_size,
            out_memory_type_index);
        *out_status = status;
        if (status != HELIOS_VKD3D_IDENTITY_RESOLVED) {
          return false;
        }

        const MemoryIdentityExports& e = memory_identity_exports();
        if (!e.res_id) {
          // NO_ICD or NO_EXPORT, decided and logged once at resolution time. The
          // engine half of the answer is left in place on purpose: it is true, and a
          // caller logging it can tell "vkd3d answered, the ICD did not" from "the
          // resource has no memory at all".
          *out_status = e.status;
          return false;
        }

        const VenusDeviceMemory memory = venus_device_memory(*out_vk_memory);
        const std::uint32_t res_id = e.res_id(memory);
        std::uint64_t alloc_size = 0;
        std::uint32_t alloc_mti = 0;
        const bool alloc_ok = e.alloc_info(memory, &alloc_size, &alloc_mti);
        if (res_id == 0 || !alloc_ok || alloc_size == 0) {
          // ⛔ **This is the expected failure while the export chain is not in
          // place, and it must stay loud.** A venus memory object only has a
          // resource id if it was allocated on the ICD's export arm, which is what
          // `VKD3D_HEAP_FLAG_HELIOS_VENUS_EXPORT` exists to force. A 0 here means
          // either that flag did not reach vkd3d or vkd3d suballocated anyway — both
          // of which make the resource unpresentable, and neither of which may be
          // rounded to "no identity today".
          *out_status = HELIOS_VKD3D_IDENTITY_ICD_REFUSED;
          const std::uint32_t n =
              helios_bridge::g_vkd3dIdentityIcdRefused.fetch_add(1, std::memory_order_relaxed) + 1;
          if (n <= 8 || (n % 4096) == 0) {
            char msg[256];
            std::snprintf(msg, sizeof(msg),
                          "resource_venus_identity: vk_memory=0x%llx off=%llu size=%llu "
                          "mti=%u has NO venus resource (res_id=%u alloc_ok=%d "
                          "alloc_size=%llu) -- not exportable "
                          "(Vkd3dIdentityIcdRefused=%u)",
                          (unsigned long long)*out_vk_memory,
                          (unsigned long long)*out_memory_offset,
                          (unsigned long long)*out_memory_size, *out_memory_type_index,
                          res_id, (int)alloc_ok, (unsigned long long)alloc_size, n);
            umd_log(msg);
          }
          return false;
        }

        *out_venus_res_id = res_id;
        *out_venus_alloc_size = alloc_size;
        // ⚠ The ICD's `memoryTypeIndex` is deliberately NOT written over the
        // engine's. They are two reads of one number and the caller keeps both so a
        // disagreement is visible; overwriting here would make the check
        // unwritable. `alloc_mti` is compared, not stored.
        if (alloc_mti != *out_memory_type_index) {
          char msg[192];
          std::snprintf(msg, sizeof(msg),
                        "resource_venus_identity: MEMORY TYPE DISAGREEMENT vk_memory=0x%llx "
                        "engine=%u icd=%u -- an importer will use the engine's",
                        (unsigned long long)*out_vk_memory, *out_memory_type_index,
                        alloc_mti);
          umd_log(msg);
        }
        *out_status = HELIOS_VKD3D_IDENTITY_RESOLVED;
        return true;
      });
}

std::uint32_t HeliosVkd3dDevice::transfer_resource_ownership(
    std::size_t resource) const noexcept {
  return helios_bridge::bridge_guard(
      "helios_vkd3d_bridge_transfer_resource_ownership", std::uint32_t(0),
      [&]() -> std::uint32_t {
        std::uint64_t vk_memory = 0;
        std::uint64_t offset = 0;
        std::uint64_t size = 0;
        std::uint32_t mti = 0;
        const std::uint32_t status = engine_resource_memory(
            impl.get(), resource, &vk_memory, &offset, &size, &mti);
        const MemoryIdentityExports& e = memory_identity_exports();
        std::uint32_t handed = 0;
        if (status == HELIOS_VKD3D_IDENTITY_RESOLVED && e.transfer_ownership) {
          handed = e.transfer_ownership(venus_device_memory(vk_memory));
        }
        if (handed == 0) {
          // ⛔ Loud, and the caller treats it as a defect: `pfnAllocateCb` has
          // already succeeded by the time this runs, so the KMD's allocation now
          // owns a resource the ICD still believes it owns, and both will unref it.
          const std::uint32_t n =
              helios_bridge::g_vkd3dOwnershipTransferFailed.fetch_add(
                  1, std::memory_order_relaxed) + 1;
          char msg[224];
          std::snprintf(msg, sizeof(msg),
                        "transfer_resource_ownership(%p) FAILED: status=%u vk_memory=0x%llx "
                        "export=%d (Vkd3dOwnershipTransferFailed=%u)",
                        (void*)resource, status, (unsigned long long)vk_memory,
                        e.transfer_ownership != nullptr, n);
          umd_log(msg);
        }
        return handed;
      });
}

std::unique_ptr<HeliosVkd3dDevice> helios_vkd3d_bridge_create_device(
    std::uint32_t luid_low, std::int32_t luid_high, std::uint32_t minimum_feature_level) {
  // ── process-global env configuration, exactly once ────────────────────────
  //
  // `std::call_once` and not "do it on every create": `_putenv_s` is not safe
  // against a concurrent `getenv`, and one process makes several devices. That
  // is R824 on the D3D11 side (`dxvk_bridge.cpp:1600-1612`) — the same hazard
  // reached the same way, not shared code.
  static std::once_flag s_envOnce;
  std::call_once(s_envOnce, [] {
    // ⭐ Set `VKD3D_LOG_FILE` only if it is ABSENT. vkd3d defaults its log to
    // stderr, which is a black hole in `dwm.exe` — so an unset variable must be
    // given a real file. But a gate script that already set one must WIN: if
    // this overwrote it, the gate's log would move out from under the script
    // that is reading it, and the run would look silent.
    //
    // ⛔ `VKD3D_DEBUG` and `VKD3D_CONFIG` are deliberately NOT set here. They
    // change engine behaviour and verbosity; a driver that pins them removes
    // the operator's only lever and makes every measurement configuration-blind.
    const char* existing = std::getenv("VKD3D_LOG_FILE");
    char chosen[MAX_PATH] = {};
    if (existing && existing[0]) {
      _snprintf_s(chosen, sizeof(chosen), _TRUNCATE, "%s", existing);
    } else {
      CreateDirectoryA("C:\\ProgramData\\Helios", nullptr);
      _snprintf_s(chosen, sizeof(chosen), _TRUNCATE,
                  "C:\\ProgramData\\Helios\\umd12-%lu-vkd3d.log",
                  (unsigned long)GetCurrentProcessId());
      _putenv_s("VKD3D_LOG_FILE", chosen);
    }

    char msg[MAX_PATH + 96];
    std::snprintf(msg, sizeof(msg),
                  "vkd3d env configured once: VKD3D_LOG_FILE=%s (%s)", chosen,
                  (existing && existing[0]) ? "pre-set, kept" : "set by bridge");
    umd_log(msg);
  });

  // ⚠ The sentinel is written with its explicit type. `bridge_guard` deduces
  // `R` from the ERROR VALUE ALONE — the body's return type is not a deduction
  // context — so a sentinel of the wrong type silently decides what every
  // SUCCESS value is converted to on the way out. Commit `ead692e`: a bare `0`
  // against a `std::size_t` body deduced `R = int` and truncated every returned
  // pointer to 32 bits; dwm and LogonUI crash-looped at cold boot and nothing
  // warned. The `static_assert` in `umd_common/bridge/bridge_guard.h` is the
  // fix — ⛔ never defeat it with a cast; if it fires, the sentinel is wrong.
  return helios_bridge::bridge_guard(
      "helios_vkd3d_bridge_create_device", std::unique_ptr<HeliosVkd3dDevice>{},
      [&]() -> std::unique_ptr<HeliosVkd3dDevice> {
        auto out = std::make_unique<HeliosVkd3dDevice>();
        out->impl = std::make_unique<HeliosVkd3dDeviceImpl>();

        LUID luid;
        // `LUID` is `{ DWORD LowPart; LONG HighPart; }` — unsigned low, signed
        // high. The two halves are carried separately across the cxx seam
        // because cxx has no `LUID`, and this is the one place their order is
        // reassembled.
        luid.LowPart = luid_low;
        luid.HighPart = luid_high;

        // ⛔ `__uuidof(ID3D12Device)`, not `IID_ID3D12Device`: the latter is a
        // `dxguid.lib` symbol, and the measured link set is one archive plus
        // `gdi32` and nothing else (`G1-static/RESULT.md`).
        ID3D12Device* dev = nullptr;
        const HRESULT hr = helios_vkd3d_create_device(
            luid, __uuidof(ID3D12Device), reinterpret_cast<void**>(&dev));
        if (FAILED(hr)) {
          const std::uint32_t n =
              helios_bridge::g_vkd3dCreateDeviceFailed.fetch_add(
                  1, std::memory_order_relaxed) + 1;
          char msg[192];
          std::snprintf(msg, sizeof(msg),
                        "helios_vkd3d_create_device(luid %08x:%08x) failed hr=0x%08lx "
                        "(Vkd3dCreateDeviceFailed=%u)",
                        (unsigned)luid_high, (unsigned)luid_low,
                        (unsigned long)hr, n);
          umd_log(msg);
          return std::unique_ptr<HeliosVkd3dDevice>{};
        }
        if (!dev) {
          // S_OK with a null out-pointer is an engine contract violation, not a
          // failure we can act on — but it is exactly the shape that would
          // otherwise become a null deref two layers up, so it is counted and
          // refused here.
          const std::uint32_t n =
              helios_bridge::g_vkd3dCreateDeviceNullOut.fetch_add(
                  1, std::memory_order_relaxed) + 1;
          char msg[160];
          std::snprintf(msg, sizeof(msg),
                        "helios_vkd3d_create_device returned S_OK with a null device "
                        "(Vkd3dCreateDeviceNullOut=%u)", n);
          umd_log(msg);
          return std::unique_ptr<HeliosVkd3dDevice>{};
        }

        // `Impl` now owns the one reference; its destructor releases it, on
        // every exit path including an exception unwinding out of the guard.
        out->impl->d3d12 = dev;

        // The native baseline is supplied by caps12, not an environment override.
        // Keep ownership in Impl so refusal or an exception releases the engine.
        out->impl->minimum_feature_level = minimum_feature_level;
        std::uint32_t shader_model = 0, raytracing_tier = 0;
        std::uint8_t device_uuid[16] = {};
        const HRESULT admission = helios_vkd3d_validate_native_feature_level(dev, minimum_feature_level,
            &shader_model, &raytracing_tier, device_uuid);
        if (FAILED(admission)) {
          const std::uint32_t n = helios_bridge::g_vkd3dCreateDeviceFailed.fetch_add(1, std::memory_order_relaxed) + 1;
          char msg[192];
          std::snprintf(msg, sizeof(msg), "Native feature contract 0x%x unavailable hr=0x%08lx (Vkd3dCreateDeviceFailed=%u)",
                        minimum_feature_level, (unsigned long)admission, n);
          umd_log(msg);
          return std::unique_ptr<HeliosVkd3dDevice>{};
        }

        // ⛔ S4b: on THIS thread, before returning. The ICD's ctx-id export is
        // thread-local, and vkd3d created its `VkInstance` on this thread
        // inside the call above.
        std::uint32_t ctx = 0;
        if (!read_venus_ctx_id_now(&ctx)) {
          // The anchor refused: two venus ICD modules in one process. Refusing
          // device creation is `ARCHITECTURE.md` §6.4's stated behaviour, and
          // it is the right one — a device built here would hand foreign
          // handles across the two ICDs and fail much later, somewhere else.
          // `IcdAnchorMismatch` is already counted and logged.
          umd_log("REFUSING ID3D12Device: venus ICD anchor mismatch");
          return std::unique_ptr<HeliosVkd3dDevice>{};
        }
        out->impl->venus_ctx_id = ctx;

        // ── UP-2c: the engine's interop interface, ONCE ────────────────────
        //
        // ⚠ A failure here does NOT fail device creation, and that asymmetry with
        // the anchor mismatch above is deliberate: a mismatch means this process
        // holds Vulkan objects from two ICDs and nothing it does is trustworthy,
        // while a missing interop interface means this engine build cannot hand out
        // a present identity — every other D3D12 obligation still works. Counted,
        // logged, and reported per-resource as `NO_INTEROP`.
        {
          ID3D12DXVKInteropDevice4* interop = nullptr;
          const HRESULT qi = dev->QueryInterface(
              IID_HeliosID3D12DXVKInteropDevice4, reinterpret_cast<void**>(&interop));
          if (SUCCEEDED(qi) && interop) {
            out->impl->interop = interop;
          } else {
            const std::uint32_t n =
                helios_bridge::g_vkd3dNoInteropDevice.fetch_add(
                    1, std::memory_order_relaxed) + 1;
            char qmsg[224];
            std::snprintf(qmsg, sizeof(qmsg),
                          "ID3D12DXVKInteropDevice4 unavailable hr=0x%08lx -- no D3D12 "
                          "resource can carry a present identity "
                          "(Vkd3dNoInteropDevice=%u)",
                          (unsigned long)qi, n);
            umd_log(qmsg);
          }
        }

        // ── UP-5: the INSTANCE-SCOPED ctx id, on this thread ──────────────
        //
        // ⛔ Here and nowhere else. `helios_venus_instance_ctx_id` returns a
        // `_Thread_local` written by this thread's own CTX_CREATE inside
        // `helios_vkd3d_create_device` above, so any later thread reads 0. This is
        // the value UP-5 stamps into an allocation identity; `venus_ctx_id` above is
        // the process-global one the anchor header forbids stamping.
        //
        // ⚠ The `VkInstance` is obtained honestly from the engine rather than passed
        // as null, even though the ICD's export ignores its argument
        // (`vn_renderer_helios.c:683`, `(void)instance;`) — a driver that relies on
        // an implementation detail of a function it does not own is one ICD change
        // away from a wrong ctx id, and a wrong ctx id is a diagnostic that lies.
        {
          const MemoryIdentityExports& e = memory_identity_exports();
          if (e.instance_ctx_id) {
            void* vk_instance = nullptr;
            void* vk_physical_device = nullptr;
            void* vk_device = nullptr;
            if (out->impl->interop &&
                SUCCEEDED(out->impl->interop->GetVulkanHandles(
                    &vk_instance, &vk_physical_device, &vk_device))) {
              out->impl->venus_instance_ctx_id = e.instance_ctx_id(vk_instance);
            } else {
              out->impl->venus_instance_ctx_id = e.instance_ctx_id(nullptr);
            }
          }
        }

        char msg[256];
        std::snprintf(msg, sizeof(msg),
                      "ID3D12Device created OK on luid %08x:%08x venus ctx_id=%u "
                      "instance_ctx_id=%u interop=%d (static vkd3d engine)",
                      (unsigned)luid_high, (unsigned)luid_low, ctx,
                      out->impl->venus_instance_ctx_id,
                      out->impl->interop != nullptr);
        umd_log(msg);
        // ⚠ A DISAGREEMENT between the two ctx ids is a finding, not noise: it means
        // another `VkInstance` was created between vkd3d's CTX_CREATE and the
        // process-global read, i.e. the value the anchor header forbids stamping was
        // about to be stamped. Logged rather than refused because the
        // instance-scoped read is the one that is used and it is unaffected.
        if (out->impl->venus_instance_ctx_id != 0 && ctx != 0 &&
            out->impl->venus_instance_ctx_id != ctx) {
          char wmsg[192];
          std::snprintf(wmsg, sizeof(wmsg),
                        "venus ctx id DISAGREEMENT: process-global=%u instance-scoped=%u "
                        "-- a concurrent instance create replaced the global one; "
                        "stamping the instance-scoped value",
                        ctx, out->impl->venus_instance_ctx_id);
          umd_log(wmsg);
        }
        return out;
      });
}

std::int32_t helios_vkd3d_bridge_serialize_root_signature(
    std::size_t desc, std::uint32_t version,
    std::size_t* blob_out, std::size_t* err_out) noexcept {
  // ⚠ `std::int32_t(0x80004005)` — `E_FAIL` spelled with its explicit type, not
  // a bare literal. Same `ead692e` class as the create path above: the sentinel
  // alone deduces `R`. Written as the constant rather than `E_FAIL` so the
  // type is visible at a glance; `E_FAIL` is `_HRESULT_TYPEDEF_(0x80004005L)`.
  return helios_bridge::bridge_guard(
      "helios_vkd3d_bridge_serialize_root_signature", std::int32_t(0x80004005),
      [&]() -> std::int32_t {
        if (!desc || !blob_out) {
          const std::uint32_t n =
              helios_bridge::g_vkd3dSerializeBadArg.fetch_add(
                  1, std::memory_order_relaxed) + 1;
          char msg[160];
          std::snprintf(msg, sizeof(msg),
                        "serialize_root_signature refused: desc=%p blob_out=%p "
                        "(Vkd3dSerializeBadArg=%u)",
                        (void*)desc, (void*)blob_out, n);
          umd_log(msg);
          // E_INVALIDARG — the caller's argument is wrong, which is a distinct
          // report from the guard's E_FAIL sentinel.
          return std::int32_t(0x80070057);
        }

        // Zero both outs before the call: the engine writes them on success,
        // and a caller that reads an untouched `err_out` after a failure would
        // otherwise release stack garbage.
        *blob_out = 0;
        if (err_out) *err_out = 0;

        ID3DBlob* blob = nullptr;
        ID3DBlob* err = nullptr;
        const HRESULT hr = helios_vkd3d_serialize_root_signature(
            reinterpret_cast<const D3D12_ROOT_SIGNATURE_DESC*>(desc),
            static_cast<D3D_ROOT_SIGNATURE_VERSION>(version), &blob, &err);

        // Both blobs are OWNED by the caller from here on. `err` is handed back
        // even on success (the engine may emit warnings) and is dropped if the
        // caller did not ask for it — releasing it rather than leaking, because
        // this path can run per-PSO.
        *blob_out = reinterpret_cast<std::size_t>(blob);
        if (err_out) {
          *err_out = reinterpret_cast<std::size_t>(err);
        } else if (err) {
          err->Release();
        }
        return static_cast<std::int32_t>(hr);
      });
}

std::int32_t helios_vkd3d_bridge_create_root_signature(std::size_t device,
    std::uint32_t node_mask, std::size_t desc, std::size_t* root_out) noexcept {
  if (!root_out) return E_INVALIDARG;
  *root_out = 0;
  if (!device || !desc) return E_INVALIDARG;
  try {
    ID3D12RootSignature* root = nullptr;
    const HRESULT hr = helios_vkd3d_create_root_signature(
        reinterpret_cast<ID3D12Device*>(device), node_mask,
        reinterpret_cast<const D3D12_VERSIONED_ROOT_SIGNATURE_DESC*>(desc), &root);
    if (SUCCEEDED(hr)) *root_out = reinterpret_cast<std::size_t>(root);
    return hr;
  } catch (const std::bad_alloc&) {
    return E_OUTOFMEMORY;
  } catch (...) {
    return E_FAIL;
  }
}

std::int32_t helios_vkd3d_bridge_clear_root_arguments(std::size_t list) noexcept {
  if (!list) return E_INVALIDARG;
  try {
    return helios_vkd3d_clear_root_arguments(reinterpret_cast<ID3D12GraphicsCommandList*>(list));
  } catch (const std::bad_alloc&) {
    return E_OUTOFMEMORY;
  } catch (...) {
    return E_FAIL;
  }
}

extern "C" HRESULT helios_vkd3d_enqueue_producer(ID3D12CommandQueue*, ID3D12Resource*,
    std::uint32_t, HANDLE, std::uint32_t*, std::uint32_t*, std::uint64_t*);

bool helios_vkd3d_bridge_publish_producer(std::size_t queue, std::size_t resource,
    std::uint32_t allocation, std::size_t admission_event, std::uint32_t* ctx, std::uint32_t* value, std::uint64_t* cookie) {
  return helios_bridge::bridge_guard("publish_producer12", false, [&]() {
    return SUCCEEDED(helios_vkd3d_enqueue_producer(
      reinterpret_cast<ID3D12CommandQueue*>(queue), reinterpret_cast<ID3D12Resource*>(resource),
      allocation, reinterpret_cast<HANDLE>(admission_event), ctx, value, cookie));
  });
}

extern "C" HRESULT helios_vkd3d_execute_command_lists(ID3D12CommandQueue*, UINT,
    ID3D12CommandList* const*, HANDLE, std::uint32_t*, std::uint32_t*, std::uint64_t*);
extern "C" void helios_vkd3d_cancel_execution(ID3D12CommandQueue*, HRESULT);

extern "C" HRESULT helios_vkd3d_try_reset_command_allocator(ID3D12CommandAllocator*);
std::int32_t helios_vkd3d_bridge_try_reset_allocator(std::size_t allocator) {
  return helios_bridge::bridge_guard("try_reset_allocator12", std::int32_t(E_FAIL), [&]() -> std::int32_t {
    return helios_vkd3d_try_reset_command_allocator(reinterpret_cast<ID3D12CommandAllocator*>(allocator));
  });
}

std::int32_t helios_vkd3d_bridge_execute(std::size_t queue, rust::Slice<const std::size_t> lists,
    std::size_t admission_event, std::uint32_t* ctx, std::uint32_t* value, std::uint64_t* cookie) {
  return helios_bridge::bridge_guard("execute12", std::int32_t(E_FAIL), [&]() -> std::int32_t {
    if (lists.empty() || lists.size() > UINT_MAX) return E_INVALIDARG;
    std::vector<ID3D12CommandList*> commands;
    commands.reserve(lists.size());
    for (auto list : lists) commands.push_back(reinterpret_cast<ID3D12CommandList*>(list));
    return helios_vkd3d_execute_command_lists(reinterpret_cast<ID3D12CommandQueue*>(queue),
      static_cast<UINT>(commands.size()), commands.data(), reinterpret_cast<HANDLE>(admission_event),
      ctx, value, cookie);
  });
}

void helios_vkd3d_bridge_cancel_execution(std::size_t queue, std::int32_t reason) {
  helios_vkd3d_cancel_execution(reinterpret_cast<ID3D12CommandQueue*>(queue), reason);
}

extern "C" HRESULT helios_vkd3d_update_tile_mappings(ID3D12CommandQueue*, ID3D12Resource*, UINT,
    const D3D12_TILED_RESOURCE_COORDINATE*, const D3D12_TILE_REGION_SIZE*, ID3D12Heap*, UINT,
    const D3D12_TILE_RANGE_FLAGS*, const UINT*, const UINT*, D3D12_TILE_MAPPING_FLAGS,
    HANDLE, std::uint32_t*, std::uint32_t*, std::uint64_t*);
extern "C" HRESULT helios_vkd3d_copy_tile_mappings(ID3D12CommandQueue*, ID3D12Resource*,
    const D3D12_TILED_RESOURCE_COORDINATE*, ID3D12Resource*, const D3D12_TILED_RESOURCE_COORDINATE*,
    const D3D12_TILE_REGION_SIZE*, D3D12_TILE_MAPPING_FLAGS, HANDLE,
    std::uint32_t*, std::uint32_t*, std::uint64_t*);

std::int32_t helios_vkd3d_bridge_update_tiles(std::size_t queue, std::size_t resource,
    std::uint32_t region_count, std::size_t coords, std::size_t sizes, std::size_t heap,
    std::uint32_t range_count, std::size_t flags, std::size_t offsets, std::size_t counts,
    std::int32_t mapping_flags, std::size_t admission, std::uint32_t* ctx,
    std::uint32_t* value, std::uint64_t* cookie) {
  return helios_bridge::bridge_guard("update_tiles12", std::int32_t(E_FAIL), [&]() -> std::int32_t {
    return helios_vkd3d_update_tile_mappings(reinterpret_cast<ID3D12CommandQueue*>(queue),
      reinterpret_cast<ID3D12Resource*>(resource), region_count,
      reinterpret_cast<const D3D12_TILED_RESOURCE_COORDINATE*>(coords),
      reinterpret_cast<const D3D12_TILE_REGION_SIZE*>(sizes), reinterpret_cast<ID3D12Heap*>(heap),
      range_count, reinterpret_cast<const D3D12_TILE_RANGE_FLAGS*>(flags),
      reinterpret_cast<const UINT*>(offsets), reinterpret_cast<const UINT*>(counts),
      static_cast<D3D12_TILE_MAPPING_FLAGS>(mapping_flags), reinterpret_cast<HANDLE>(admission),
      ctx, value, cookie);
  });
}

std::int32_t helios_vkd3d_bridge_copy_tiles(std::size_t queue, std::size_t dst,
    std::size_t dst_coord, std::size_t src, std::size_t src_coord, std::size_t size,
    std::int32_t flags, std::size_t admission, std::uint32_t* ctx, std::uint32_t* value,
    std::uint64_t* cookie) {
  return helios_bridge::bridge_guard("copy_tile_mappings12", std::int32_t(E_FAIL), [&]() -> std::int32_t {
    return helios_vkd3d_copy_tile_mappings(reinterpret_cast<ID3D12CommandQueue*>(queue),
      reinterpret_cast<ID3D12Resource*>(dst), reinterpret_cast<const D3D12_TILED_RESOURCE_COORDINATE*>(dst_coord),
      reinterpret_cast<ID3D12Resource*>(src), reinterpret_cast<const D3D12_TILED_RESOURCE_COORDINATE*>(src_coord),
      reinterpret_cast<const D3D12_TILE_REGION_SIZE*>(size), static_cast<D3D12_TILE_MAPPING_FLAGS>(flags),
      reinterpret_cast<HANDLE>(admission), ctx, value, cookie);
  });
}
