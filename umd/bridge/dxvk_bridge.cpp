// Helios UMD <-> DXVK engine bridge implementation.
//
// Wraps DXVK's DxvkInstance/DxvkAdapter/DxvkDevice behind the opaque
// HeliosDxvkDevice. The DXVK engine references a frontend-provided
// `Logger::s_instance` global (normally defined in src/d3d11/d3d11_main.cpp,
// which we do not build) — we provide it here.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <mmsystem.h>
#include <sddl.h>

#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <exception>
#include <share.h>
#include <tlhelp32.h>

#include "dxvk_bridge.h"

// ⚠ These two resolve to `umd_common/bridge/`, not to this directory —
// `build.rs` adds it to the include path (`DECISIONS.md` D3b: one copy of the
// source, shared with the D3D12 bridge). `bridge_guard.h` is NOT included here:
// it needs `dxvk::DxvkError` for its engine arm, so it comes after the DXVK
// headers below.
#include "bridge_common.h"
#include "bridge_util.h"

#include "bridge_dxbc.h"

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstring>
#include <memory>
#include <mutex>
#include <optional>
#include <thread>
#include <type_traits>
#include <d3d11.h>
#include <dxgi.h>
#include <fstream>
#include <sstream>
#include <string>
#include <vector>

#include "dxvk_instance.h"
#include "dxvk_adapter.h"
#include "dxvk_device.h"
#include "dxvk_fence.h"
#include "../src/util/util_error.h"
#include "dxbc/dxbc_container.h"

// DXVK's full D3D11 COM implementation (built as libhelios_d3d11_static.a). We
// instantiate D3D11DXGIDevice from our DxvkDevice and forward the d3d10umddi DDI
// to ID3D11Device / ID3D11DeviceContext.
#include "d3d11_device.h"
#include "d3d11_context_def.h"
#include "d3d11_texture.h"
#include "d3d11_context_imm.h"
#include "dxvk_helios_feed_trace.h"
#include "dxvk_helios_producer.h"

// After the DXVK headers: see the include-order note in this header.
#include "bridge_icd_anchor.h"
#include "bridge_icd_exports.h"

// ── the shared bridge_guard, with this bridge's one engine-specific arm ──────
//
// `dxvk::DxvkError` is not a `std::exception`, so without this arm every DXVK
// failure would log "unknown exception". The macro is the header's documented
// customization point. Preserve the engine's refusal reason so an invalid
// external-image template can be distinguished from an allocation failure.
// It must be defined AFTER `util_error.h` above, because it is expanded when
// `bridge_guard.h` is preprocessed.
//
// ⛔ Must not allocate — a `std::string` built inside a `std::bad_alloc`
// handler can throw again. DxvkError::message() returns a borrowed const
// reference; assert that contract before formatting into fixed stack storage.
#define HELIOS_BRIDGE_ENGINE_CATCH(what)                          \
  catch (const dxvk::DxvkError& error) {                          \
    static_assert(std::is_same_v<decltype(error.message()), const std::string&>); \
    char msg[512];                                                \
    std::snprintf(msg, sizeof(msg), "%s: DxvkError: %.320s",       \
      (what), error.message().c_str());                           \
    ::helios_bridge::umd_log(msg);                                \
  }
#include "bridge_guard.h"

namespace dxbc_spv::dxbc {
  util::md5::Digest hashDxbcBinary(const void* data, size_t size);
}

namespace dxvk {
  // Frontend-provided global the DXVK engine links against. The string is the
  // log file name DXVK writes engine diagnostics to.
  Logger Logger::s_instance("helios_umd_dxvk.log");
}

namespace helios_bridge {

// Non-dispatchable Vulkan handles are pointer-shaped only in 64-bit builds;
// in x86 they remain uint64_t and must never truncate through uintptr_t.
static std::uint64_t memory_handle_bits(VkDeviceMemory memory) {
#if VK_USE_64_BIT_PTR_DEFINES
  return reinterpret_cast<std::uint64_t>(memory);
#else
  return static_cast<std::uint64_t>(memory);
#endif
}

  // The rotate-sample instrument reads rows as std::uint32_t, so it is only
  // valid against a 32-bit-per-pixel format.
  bool is_32bpp_dxgi_format(DXGI_FORMAT format) {
    switch (format) {
    case DXGI_FORMAT_R8G8B8A8_TYPELESS:
    case DXGI_FORMAT_R8G8B8A8_UNORM:
    case DXGI_FORMAT_R8G8B8A8_UNORM_SRGB:
    case DXGI_FORMAT_R8G8B8A8_UINT:
    case DXGI_FORMAT_R8G8B8A8_SNORM:
    case DXGI_FORMAT_R8G8B8A8_SINT:
    case DXGI_FORMAT_B8G8R8A8_TYPELESS:
    case DXGI_FORMAT_B8G8R8A8_UNORM:
    case DXGI_FORMAT_B8G8R8A8_UNORM_SRGB:
    case DXGI_FORMAT_B8G8R8X8_TYPELESS:
    case DXGI_FORMAT_B8G8R8X8_UNORM:
    case DXGI_FORMAT_B8G8R8X8_UNORM_SRGB:
    case DXGI_FORMAT_R10G10B10A2_TYPELESS:
    case DXGI_FORMAT_R10G10B10A2_UNORM:
    case DXGI_FORMAT_R10G10B10A2_UINT:
    case DXGI_FORMAT_R11G11B10_FLOAT:
    case DXGI_FORMAT_R16G16_TYPELESS:
    case DXGI_FORMAT_R16G16_FLOAT:
    case DXGI_FORMAT_R16G16_UNORM:
    case DXGI_FORMAT_R16G16_UINT:
    case DXGI_FORMAT_R16G16_SNORM:
    case DXGI_FORMAT_R16G16_SINT:
    case DXGI_FORMAT_R32_TYPELESS:
    case DXGI_FORMAT_R32_FLOAT:
    case DXGI_FORMAT_R32_UINT:
    case DXGI_FORMAT_R32_SINT:
      return true;
    default:
      return false;
    }
  }

  // ⚠ `bridge_log_budget`, `PeriodicStat`, `qpc_elapsed_us` and `ComRelease<T>`
  // moved to `umd_common/bridge/{bridge_common,bridge_util}.h` at stage S1
  // (`DECISIONS.md` D3b) — same namespace, same signatures, so every use site
  // below is unchanged. They are engine-agnostic and the D3D12 bridge gets them
  // from the same file rather than a copy.

  /// The direct-scanout PRIMARY create's QI failure — the one whose silent zero
  /// R401 now reports to the runtime.
  ///
  /// Stays here: it is a DXVK-path counter, not shared machinery.
  std::atomic<std::uint32_t> g_scanoutPrimaryQiFailed{0};

  // Minimal IDXGIAdapter the D3D11DXGIDevice constructor stores (it is not
  // queried during construction — the Dxvk objects are passed directly).
  class HeliosStubAdapter : public IDXGIAdapter {
    std::atomic<ULONG> m_ref{1};
  public:
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void** ppv) override {
      if (!ppv) return E_POINTER;
      if (riid == __uuidof(IUnknown) || riid == __uuidof(IDXGIObject) ||
          riid == __uuidof(IDXGIAdapter)) {
        *ppv = static_cast<IDXGIAdapter*>(this);
        AddRef();
        return S_OK;
      }
      *ppv = nullptr;
      return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef() override { return ++m_ref; }
    ULONG STDMETHODCALLTYPE Release() override {
      ULONG r = --m_ref;
      if (!r) delete this;
      return r;
    }
    HRESULT STDMETHODCALLTYPE SetPrivateData(REFGUID, UINT, const void*) override { return E_NOTIMPL; }
    HRESULT STDMETHODCALLTYPE SetPrivateDataInterface(REFGUID, const IUnknown*) override { return E_NOTIMPL; }
    HRESULT STDMETHODCALLTYPE GetPrivateData(REFGUID, UINT*, void*) override { return E_NOTIMPL; }
    HRESULT STDMETHODCALLTYPE GetParent(REFIID, void** pp) override { if (pp) *pp = nullptr; return E_NOINTERFACE; }
    HRESULT STDMETHODCALLTYPE EnumOutputs(UINT, IDXGIOutput**) override { return DXGI_ERROR_NOT_FOUND; }
    HRESULT STDMETHODCALLTYPE GetDesc(DXGI_ADAPTER_DESC* d) override { if (d) std::memset(d, 0, sizeof(*d)); return S_OK; }
    HRESULT STDMETHODCALLTYPE CheckInterfaceSupport(REFGUID, LARGE_INTEGER*) override { return DXGI_ERROR_UNSUPPORTED; }
  };
}

namespace helios_bridge {
  // Per-process log path under C:\ProgramData\Helios (see umd_log_path() in
  // lib.rs). The restricted IddCx host process cannot write C:\Windows\Temp, so
  // its DXVK-bridge log lines vanished; ProgramData is standard-user writable and
  // the per-pid name keeps each process's file owned by that process.
  // Magic static, matching `shader_bytecode_dump_path()` in this same anonymous
  // namespace — the lazy `if (path[0] == 0)` form it used was an
  // unsynchronised write to shared storage, and the file already contradicted
  // itself on this point.
  const char* umd_log_file() {
    static const std::string path = [] {
      CreateDirectoryA("C:\\ProgramData\\Helios", nullptr);
      char buf[MAX_PATH] = {};
      _snprintf_s(buf, sizeof(buf), _TRUNCATE,
                  "C:\\ProgramData\\Helios\\umd-%lu.log",
                  (unsigned long)GetCurrentProcessId());
      return std::string(buf);
    }();
    return path.c_str();
  }

  void umd_log(const char* msg) {
    // _fsopen with _SH_DENYNO, NOT fopen_s: fopen_s opens in _SH_SECURE
    // (deny-sharing) mode, and the Rust side holds a persistent handle to the
    // same umd-<pid>.log since e88f2c6 — fopen_s then fails on EVERY call and
    // all bridge logging (incl. rotate-perf telemetry) silently vanishes
    // (found 18th session: DriverStore UMD had the strings, logs had no
    // [dxvk-bridge] lines).
    FILE* f = _fsopen(umd_log_file(), "a", _SH_DENYNO);
    if (f) {
      fprintf(f, "[dxvk-bridge] %s\n", msg);
      fclose(f);
    }
  }

  // ⚠ `bridge_guard` moved to `umd_common/bridge/bridge_guard.h` at stage S1
  // (`DECISIONS.md` D3b), together with the ~30 lines of comment recording why
  // it exists and why its compile-time assert is not optional (commit
  // `ead692e`, the truncation that crash-looped dwm and LogonUI at cold boot).
  // Read it there; the check that it is still the only one is
  // `grep -rnE '^[[:space:]]*static_assert\(' umd/bridge umd12/bridge
  // umd_common/bridge` -> exactly one hit, in that file. ⚠ The anchor is
  // load-bearing: without it this comment counts itself.
  //
  // The DXVK-specific arm stays here, as the header's one customization point.
  // `dxvk::DxvkError` is not a `std::exception`, so without this the generic
  // arms would not name it and every DXVK failure would log "unknown
  // exception". It expands to the identical catch clause the guard had before
  // the move -- S1 is a move, and a behaviour change here would be a defect.
  //
  // ⛔ Must not allocate: a `std::string` built inside a `std::bad_alloc`
  // handler can throw again. That is also why `DxvkError::message()` (returns
  // `std::string`) is not called.

}

using namespace helios_bridge;

// Scan-out row-pitch alignment. The host reconstructs the DWM primary from a
// linear stride, and 256 is the cross-adapter alignment the QEMU fork's
// reconstruction and the KMD's SET_SCANOUT_BLOB both assume. It is NOT a
// hardware requirement of this device and must not be "optimised" to the
// natural row length. R822.
static constexpr std::uint32_t kScanoutPitchAlign = 256u;

// The 32bpp scan-out formats, and the bytes-per-pixel the pitch arithmetic
// needs. Mirrors forward.rs's `matches!(a.Format as u32, 28 | 87 | 88)`:
// R8G8B8A8_UNORM (28), B8G8R8A8_UNORM (87), B8G8R8X8_UNORM (88).
struct ScanoutFormat {
  std::uint32_t dxgiValue;
  std::uint32_t bytesPerPixel;

  static std::optional<ScanoutFormat> from_dxgi(std::uint32_t format) {
    switch (format) {
      case 28u:
      case 87u:
      case 88u:
        return ScanoutFormat{ format, 4u };
      default:
        return std::nullopt;
    }
  }
};

class TimerResolution {
public:
  explicit TimerResolution(bool enabled) noexcept {
    if (enabled) {
      const auto result = timeBeginPeriod(1);
      active = result == TIMERR_NOERROR;
      if (!active)
        record_failure("timeBeginPeriod", result);
    }
  }

  ~TimerResolution() {
    if (active) {
      const auto result = timeEndPeriod(1);
      if (result != TIMERR_NOERROR)
        record_failure("timeEndPeriod", result);
    }
  }

  TimerResolution(const TimerResolution&) = delete;
  TimerResolution& operator=(const TimerResolution&) = delete;

private:
  bool active = false;

  static void record_failure(const char* operation, MMRESULT result) noexcept {
    static std::atomic<std::uint32_t> failures { 0 };
    char message[128];
    std::snprintf(message, sizeof(message),
      "timer-resolution: %s failed code=%u failures=%u", operation,
      unsigned(result), unsigned(failures.fetch_add(1, std::memory_order_relaxed) + 1));
    umd_log(message);
  }
};

struct HeliosDxvkDeviceImpl {
  explicit HeliosDxvkDeviceImpl(bool timer_enabled) : timer_resolution(timer_enabled) { }

  // Declared first so the request outlives all DXVK workers, including failed initialization.
  TimerResolution timer_resolution;
  dxvk::Rc<dxvk::DxvkInstance> instance;
  dxvk::Rc<dxvk::DxvkAdapter>  adapter;
  dxvk::Rc<dxvk::DxvkDevice>   device;
  ID3D11Device*        d3d11   = nullptr; // QI'd from D3D11DXGIDevice; holds it alive
  ID3D11DeviceContext* context = nullptr; // immediate context
  std::uint32_t venus_ctx_id = 0;
  // WSI borrows its handle only until Present returns. Keep a duplicate for
  // exact object comparison and an imported semaphore for the helper device.
  std::mutex vehicle_semaphore_mutex;
  HANDLE vehicle_semaphore_handle = nullptr;
  dxvk::Rc<dxvk::DxvkFence> vehicle_semaphore;


  // One unnamed, registered timeline per device. Resource publications use
  // exact allocation bindings; the fence signal stays folded into frame work.
  // Initialization enters Vulkan with this lock dropped.
  std::mutex present_order_mutex;
  std::condition_variable present_order_ready;
  // Remaining producer timeline state is guarded by present_order_mutex.
  bool present_fence_initializing = false;
  dxvk::Rc<dxvk::DxvkFence> present_fence;
  std::uint64_t present_value    = 0;
  bool          present_fence_failed = false;

  // Registration failure is terminal; it never permits an unordered read.
  std::uint64_t present_stream_cookie = 0;

  ~HeliosDxvkDeviceImpl() {
    if (vehicle_semaphore_handle) CloseHandle(vehicle_semaphore_handle);
    if (context) context->Release();
    if (d3d11) d3d11->Release();
  }
};

namespace {

  // The body every plain `create_*_shader` forwarder shares.
  //
  // Six bodies were identical apart from one COM interface type, one
  // `ID3D11Device` method and a two-letter dump tag. That is the shape where a
  // fix (a new dump, a changed refusal) lands in five of six and the sixth
  // behaves differently only under the workload that binds that stage.
  //
  // `Create` is a lambda rather than a pointer-to-member-function because the
  // six `ID3D11Device::Create*Shader` overloads have six different out-param
  // types; the lambda pins the pairing at each call site, where it is visible.
  template <typename Iface, typename Create>
  std::size_t create_shader_impl(const HeliosDxvkDeviceImpl* impl,
                                 const char* dump_tag,
                                 const char* name,
                                 const std::uint8_t* code,
                                 std::size_t len,
                                 Create create) {
    if (!impl || !impl->d3d11 || !code || !len)
      return 0;
    Iface* shader = nullptr;
    return bridge_guard(name, std::size_t(0), [&]() -> std::size_t {
      auto bytecode = prepare_shader_bytecode(code, len);
      if (!bytecode)
        return 0;
      dump_shader_bytecode(dump_tag, "raw", code, len);
      dump_shader_bytecode(dump_tag, "wrapped", bytecode.data(), bytecode.len());
      HRESULT hr = create(impl->d3d11, bytecode.data(), bytecode.len(), &shader);
      if (FAILED(hr)) {
        char msg[96];
        std::snprintf(msg, sizeof(msg), "%s returned failure", name);
        umd_log(msg);
        return 0;
      }
      return reinterpret_cast<std::size_t>(shader);
    });
  }

}

// Out-of-line ctor/dtor, defined where HeliosDxvkDeviceImpl is complete so the
// header (and the cxx glue) need no DXVK headers.
HeliosDxvkDevice::HeliosDxvkDevice() noexcept = default;
HeliosDxvkDevice::~HeliosDxvkDevice() = default;

std::size_t HeliosDxvkDevice::d3d11_device_ptr() const {
  return impl ? reinterpret_cast<std::size_t>(impl->d3d11) : 0;
}
std::size_t HeliosDxvkDevice::d3d11_context_ptr() const {
  return impl ? reinterpret_cast<std::size_t>(impl->context) : 0;
}

std::uint32_t HeliosDxvkDevice::venus_context_id() const {
  return impl ? impl->venus_ctx_id : 0;
}

std::uint64_t HeliosDxvkDevice::feed_trace_timestamp_ns() const noexcept {
  return dxvk::helios_feed::timestampNs();
}

void HeliosDxvkDevice::feed_trace_render_callback(
    std::uint64_t duration_ns) const noexcept {
  dxvk::helios_feed::umdRenderCallback(duration_ns);
}

void HeliosDxvkDevice::feed_trace_present_callback(
    std::uint64_t duration_ns) const noexcept {
  dxvk::helios_feed::umdPresentCallback(duration_ns);
}

bool HeliosDxvkDevice::recycle_deferred_command_list(
    std::size_t deferred_context_ptr,
    std::size_t command_list_ptr) const noexcept {
  return bridge_guard("recycle_deferred_command_list", false, [&]() -> bool {
    if (!impl || !impl->d3d11 || !deferred_context_ptr || !command_list_ptr)
      return false;

    // The UMD passes only its owned deferred COM context (created through this
    // bridge) and the owned command list its FinishCommandList just returned.
    // Do not probe them with GetType/GetDevice here: those methods AddRef and
    // Release the shared device on every handoff, defeating this hot-path
    // optimization. D3D11CommandList::IsReusableBy is the narrow contract
    // guard for a same-device but wrong-DC handoff.
    auto* context = reinterpret_cast<ID3D11DeviceContext*>(deferred_context_ptr);
    auto* commandList = reinterpret_cast<ID3D11CommandList*>(command_list_ptr);
    return static_cast<dxvk::D3D11DeferredContext*>(context)
      ->RecycleCommandList(static_cast<dxvk::D3D11CommandList*>(commandList));
  });
}

bool HeliosDxvkDevice::enable_deferred_context_ddi_logical_reset(
    std::size_t deferred_context_ptr) const noexcept {
  return bridge_guard("enable_deferred_context_ddi_logical_reset", false, [&]() -> bool {
    if (!impl || !impl->d3d11 || !deferred_context_ptr)
      return false;

    // This is called exactly once, immediately after this bridge created the
    // private deferred context. Do not QI or GetDevice on the hot DDI route.
    auto* context = reinterpret_cast<ID3D11DeviceContext*>(deferred_context_ptr);
    static_cast<dxvk::D3D11DeferredContext*>(context)
      ->EnableHeliosDdiLogicalReset();
    return true;
  });
}

bool HeliosDxvkDevice::set_resource_kmt_handles(
    std::size_t d3d11_resource_ptr,
    std::uint32_t local,
    std::uint32_t global) const noexcept {
  return bridge_guard("set_resource_kmt_handles", false, [&]() -> bool {
    if (!d3d11_resource_ptr || !local)
      return false;

    auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr);
    auto* texture = dxvk::GetCommonTexture(resource);
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return false;

    auto image = texture->GetImage();
    dxvk::Rc<dxvk::HeliosProducerBinding> producer = new dxvk::HeliosProducerBinding(
      impl->device->vkd()->device(), impl->device->vkd()->vkGetSemaphoreCounterValue, local);
    if (!image->storage()->setHeliosProducer(producer))
      return false;
    if (image->heliosStagingImage() != nullptr
     && !image->heliosStagingImage()->storage()->setHeliosProducer(producer))
      return false;
    image->storage()->setKmtHandles(local, global);

    static std::atomic<std::uint32_t> s_setKmtLogs{0};
    if (bridge_log_budget(s_setKmtLogs, 64, 512)) {
      char msg[160];
      std::snprintf(msg, sizeof(msg),
        "set_resource_kmt_handles resource=%p local=0x%08x global=0x%08x",
        resource, local, global);
      umd_log(msg);
    }
    return true;
  });
}

bool HeliosDxvkDevice::get_resource_memory_info(
    std::size_t d3d11_resource_ptr,
    std::uint64_t* memory,
    std::uint64_t* size,
    std::uint64_t* offset,
    std::uint32_t* resource_id) const noexcept {
  return bridge_guard("get_resource_memory_info", false, [&]() -> bool {
    if (memory)
      *memory = 0;
    if (size)
      *size = 0;
    if (offset)
      *offset = 0;
    if (resource_id)
      *resource_id = 0;

    if (!d3d11_resource_ptr)
      return false;

    auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr);
    auto* texture = dxvk::GetCommonTexture(resource);
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return false;

    auto info = texture->GetImage()->storage()->getMemoryInfo();
    const auto rawMemory = memory_handle_bits(info.memory);
    const auto venusId = venus_memory_id_from_handle(info.memory);
    const auto resourceId = venus_memory_resource_id_from_handle(info.memory);
    if (memory)
      *memory = venusId;
    if (size)
      *size = info.size;
    if (offset)
      *offset = info.offset;
    if (resource_id)
      *resource_id = resourceId;

    static std::atomic<std::uint32_t> s_memInfoLogs{0};
    if (bridge_log_budget(s_memInfoLogs, 64, 512)) {
      char msg[256];
      std::snprintf(msg, sizeof(msg),
        "get_resource_memory_info resource=%p memory_raw=0x%llx venus_id=0x%llx res_id=%u size=%llu offset=%llu",
        resource,
        static_cast<unsigned long long>(rawMemory),
        static_cast<unsigned long long>(venusId),
        resourceId,
        static_cast<unsigned long long>(info.size),
        static_cast<unsigned long long>(info.offset));
      umd_log(msg);
    }
    return venusId != 0 && info.size != 0;
  });
}

bool HeliosDxvkDevice::get_resource_alloc_identity(
    std::size_t d3d11_resource_ptr,
    std::uint64_t* venus_alloc_size,
    std::uint32_t* memory_type_index,
    std::uint64_t* global_vidmm_tracker) const noexcept {
  return bridge_guard("get_resource_alloc_identity", false, [&]() -> bool {
    if (venus_alloc_size)
      *venus_alloc_size = 0;
    if (memory_type_index)
      *memory_type_index = 0;
    if (global_vidmm_tracker)
      *global_vidmm_tracker = 0;

    if (!d3d11_resource_ptr)
      return false;

    auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr);
    auto* texture = dxvk::GetCommonTexture(resource);
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return false;

    auto info = texture->GetImage()->storage()->getMemoryInfo();
    const bool valid = venus_memory_alloc_info_from_handle(
      info.memory, venus_alloc_size, memory_type_index);
    if (valid && global_vidmm_tracker) {
      *global_vidmm_tracker =
        venus_memory_vidmm_global_identity_from_handle(info.memory);
    }
    return valid;
  });
}

bool HeliosDxvkDevice::transfer_resource_ownership(
    std::size_t d3d11_resource_ptr) const noexcept {
  return bridge_guard("transfer_resource_ownership", false, [&]() -> bool {
    if (!d3d11_resource_ptr)
      return false;

    auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr);
    auto* texture = dxvk::GetCommonTexture(resource);
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return false;

    auto info = texture->GetImage()->storage()->getMemoryInfo();
    const auto resourceId = venus_memory_transfer_resource_ownership(info.memory);

    static std::atomic<std::uint32_t> s_xferOwnLogs{0};
    if (bridge_log_budget(s_xferOwnLogs, 64, 512)) {
      char msg[192];
      std::snprintf(msg, sizeof(msg),
        "transfer_resource_ownership resource=%p memory=0x%llx res_id=%u",
        resource,
        static_cast<unsigned long long>(memory_handle_bits(info.memory)),
        resourceId);
      umd_log(msg);
    }
    return resourceId != 0;
  });
}

std::size_t HeliosDxvkDevice::open_ddi_texture2d(
    std::uint32_t width,
    std::uint32_t height,
    std::uint32_t format,
    std::uint32_t bind_flags,
    std::uint32_t misc_flags,
    std::uint32_t global,
    std::uint32_t renderer_resource_id,
    std::uint64_t venus_alloc_size,
    std::uint32_t memory_type_index,
    std::uint64_t global_vidmm_tracker,
    bool scanout_linear,
    bool linear_scanout_target,
    bool cross_context_optimal,
    bool dedicated_present_buffer,
    std::size_t source_image_create_info,
    bool source_external_ownership) const {
  if (!impl || !impl->d3d11 || !global || !renderer_resource_id || !width || !height)
    return 0;

  return bridge_guard("open_ddi_texture2d", std::size_t(0), [&]() -> std::size_t {
      {
        static std::atomic<std::uint32_t> s_openBeginLogs{0};
        if (bridge_log_budget(s_openBeginLogs, 64, 512)) {
          char msg[256];
          std::snprintf(msg, sizeof(msg),
            "OpenDdiTexture2D begin %ux%u fmt=%u bind=0x%08x misc=0x%08x global=0x%08x renderer_res=%u alloc_size=%llu mem_type=%u vidmm_identity=0x%016llx",
            width, height, format, bind_flags, misc_flags, global, renderer_resource_id,
            static_cast<unsigned long long>(venus_alloc_size), memory_type_index,
            static_cast<unsigned long long>(global_vidmm_tracker));
          umd_log(msg);
        }
      }

      dxvk::D3D11_COMMON_TEXTURE_DESC desc = { };
      desc.Width = width;
      desc.Height = height;
      desc.Depth = 1;
      desc.MipLevels = 1;
      desc.ArraySize = 1;
      desc.Format = static_cast<DXGI_FORMAT>(format);
      desc.SampleDesc.Count = 1;
      desc.SampleDesc.Quality = 0;
      desc.Usage = D3D11_USAGE_DEFAULT;
      desc.BindFlags = bind_flags;
      desc.CPUAccessFlags = 0;
      desc.MiscFlags = misc_flags | D3D11_RESOURCE_MISC_SHARED;
      desc.TextureLayout = D3D11_TEXTURE_LAYOUT_UNDEFINED;

      // Typed venus import identity (C1): the resid plus the creator's exact
      // allocation size/memory type from the KMD's open-identity record. The
      // HANDLE parameter still carries the resid value only to select Import
      // mode in the shared-texture path; the import itself reads the typed info.
      dxvk::D3D11_HELIOS_IMPORT_INFO importInfo = { };
      importInfo.ResourceId      = renderer_resource_id;
      importInfo.AllocSize       = venus_alloc_size;
      // memory_type_index 0 is a real venus type; the identity is only recorded
      // as a (size, type) pair, so size == 0 means "no recorded identity" and
      // the type must not be applied as an override either.
      importInfo.MemoryTypeIndex = venus_alloc_size ? memory_type_index : ~0u;
      // Rebuild a flagged primary as the creator's plain LINEAR+DMA_BUF image.
      importInfo.ScanoutLinear   = scanout_linear;
      importInfo.LinearScanoutTarget = linear_scanout_target;
      importInfo.CrossContextOptimal = cross_context_optimal;
      importInfo.DedicatedPresentBuffer = dedicated_present_buffer;
      // Same-thread vehicle v3 borrow. DXVK deep-copies this template and its
      // supported pNext/array data during the synchronous texture constructor.
      importInfo.SourceCreateInfo =
        reinterpret_cast<const VkImageCreateInfo*>(source_image_create_info);
      importInfo.SourceExternalOwnership = source_external_ownership;

      // static_cast, matching the sibling context downcast in this file. Zero
      // runtime change today (the base sits at offset 0), but if an upstream DXVK
      // rebase inserts a base class into D3D11Device this becomes a compile error
      // instead of a silently mis-offset `this`. R823.
      auto* device = static_cast<dxvk::D3D11Device*>(impl->d3d11);
      auto* texture = new dxvk::D3D11Texture2D(
          device, &desc, nullptr,
          reinterpret_cast<HANDLE>(static_cast<std::uintptr_t>(renderer_resource_id)),
          &importInfo);

      ID3D11Resource* resource = nullptr;
      HRESULT hr = texture->QueryInterface(
          __uuidof(ID3D11Resource),
          reinterpret_cast<void**>(&resource));

      static std::atomic<std::uint32_t> s_openDoneLogs{0};
      if (bridge_log_budget(s_openDoneLogs, 64, 512)) {
        char msg[224];
        std::snprintf(msg, sizeof(msg),
          "OpenDdiTexture2D %ux%u fmt=%u bind=0x%08x misc=0x%08x global=0x%08x renderer_res=%u hr=0x%08lx resource=%p",
          width, height, format, bind_flags, misc_flags, global, renderer_resource_id,
          static_cast<unsigned long>(hr), resource);
        umd_log(msg);
      }

      if (FAILED(hr) || !resource)
        return 0;

      // The payload WDDM allocation and this imported Venus memory now share
      // one system-wide VidMm tracker. Opening it here gives this process a
      // reference before the creator can close its VkDeviceMemory. A shrunk
      // payload is not safe to expose without that lifetime reference (or the
      // ICD's full-size fallback), so retention failure rejects the open.
      if (global_vidmm_tracker) {
        auto* common = dxvk::GetCommonTexture(resource);
        const bool retained = common && common->GetImage() &&
          common->GetImage()->storage() && venus_memory_open_vidmm_tracker(
            common->GetImage()->storage()->getMemoryInfo().memory,
            global_vidmm_tracker);
        if (!retained) {
          char msg[192];
          std::snprintf(msg, sizeof(msg),
            "OpenDdiTexture2D VidMm tracker open failed renderer_res=%u identity=0x%016llx",
            renderer_resource_id,
            static_cast<unsigned long long>(global_vidmm_tracker));
          umd_log(msg);
          resource->Release();
          return 0;
        }
      }

      return reinterpret_cast<std::size_t>(resource);
  });
}

std::size_t HeliosDxvkDevice::create_ddi_scanout_texture2d(
    std::uint32_t width,
    std::uint32_t height,
    std::uint32_t format,
    std::uint32_t bind_flags,
    std::uint32_t misc_flags,
    bool kmd_transfer_source,
    std::uint64_t* out_row_pitch,
    std::uint64_t* out_offset) const {
  if (out_row_pitch) *out_row_pitch = 0;
  if (out_offset)    *out_offset = 0;
  if (!impl || !impl->d3d11 || !width || !height)
    return 0;

  // R822: the pitch below assumes 4 bytes per pixel, and that was true only
  // because of a check in ANOTHER LANGUAGE -- forward.rs gates the caller on
  // `matches!(a.Format as u32, 28 | 87 | 88)`, while this function accepted any
  // DXGI_FORMAT and static_cast it straight into the texture desc. Validated
  // here as well, so the arithmetic and its precondition live together. The
  // Rust-side check stays as defence in depth.
  const auto scanoutFormat = ScanoutFormat::from_dxgi(format);
  if (!scanoutFormat) {
    static std::atomic<std::uint32_t> s_badFormat{0};
    const std::uint32_t n = s_badFormat.fetch_add(1, std::memory_order_relaxed) + 1;
    if (n <= 8 || (n % 512) == 0) {
      char msg[160];
      std::snprintf(msg, sizeof(msg),
        "CreateDdiScanoutTexture2D REFUSED: fmt=%u is not a 32bpp scan-out "
        "format (x%u)", format, n);
      umd_log(msg);
    }
    return 0;
  }

  return bridge_guard("create_ddi_scanout_texture2d", std::size_t(0), [&]() -> std::size_t {
      {
        static std::atomic<std::uint32_t> s_scanBeginLogs{0};
        if (bridge_log_budget(s_scanBeginLogs, 64, 512)) {
          char msg[224];
          std::snprintf(msg, sizeof(msg),
            "CreateDdiScanoutTexture2D begin %ux%u fmt=%u bind=0x%08x misc=0x%08x kmdTransfer=%u",
            width, height, format, bind_flags, misc_flags,
            kmd_transfer_source ? 1u : 0u);
          umd_log(msg);
        }
      }

      // Build a plain 2D DEFAULT-usage description. The scan-out primary is a
      // device-local render target the host scans out of; sharing is driven by
      // the D3D11_HELIOS_CREATE_INFO marker (Export + DMA_BUF), not by the desc's
      // D3D11 MiscFlags, so we do not force MISC_SHARED here.
      dxvk::D3D11_COMMON_TEXTURE_DESC desc = { };
      desc.Width          = width;
      desc.Height         = height;
      desc.Depth          = 1;
      desc.MipLevels      = 1;
      desc.ArraySize      = 1;
      desc.Format         = static_cast<DXGI_FORMAT>(format);
      desc.SampleDesc.Count   = 1;
      desc.SampleDesc.Quality = 0;
      desc.Usage          = D3D11_USAGE_DEFAULT;
      desc.BindFlags      = bind_flags;
      desc.CPUAccessFlags = 0;
      desc.MiscFlags      = misc_flags;
      desc.TextureLayout  = D3D11_TEXTURE_LAYOUT_UNDEFINED;

      dxvk::D3D11_HELIOS_CREATE_INFO createInfo = { };
      createInfo.DirectOptimalScanout = true;
      createInfo.KmdTransferSource = kmd_transfer_source;

      // static_cast, matching the sibling context downcast in this file. Zero
      // runtime change today (the base sits at offset 0), but if an upstream DXVK
      // rebase inserts a base class into D3D11Device this becomes a compile error
      // instead of a silently mis-offset `this`. R823.
      auto* device = static_cast<dxvk::D3D11Device*>(impl->d3d11);
      // Fresh Export create: no imported vkImage, no shared handle, no import
      // identity. The last argument marks it as the DWM scan-out primary.
      auto* texture = new dxvk::D3D11Texture2D(
          device, &desc, nullptr,
          INVALID_HANDLE_VALUE,
          nullptr,       // pHeliosImport
          &createInfo);  // pHeliosCreate → DirectOptimalScanout

      ID3D11Resource* resource = nullptr;
      HRESULT hr = texture->QueryInterface(
          __uuidof(ID3D11Resource),
          reinterpret_cast<void**>(&resource));
      if (FAILED(hr) || !resource) {
        // Counted, not just logged: `open_ddi_texture2d` has no counter on its
        // equivalent branch either, and a failing QI here returns a silent zero
        // that reads exactly like "no primary was asked for".
        const std::uint32_t n =
          g_scanoutPrimaryQiFailed.fetch_add(1, std::memory_order_relaxed) + 1;
        char qimsg[128];
        std::snprintf(qimsg, sizeof(qimsg),
          "CreateDdiScanoutTexture2D: QI(ID3D11Resource) failed (x%u)", n);
        umd_log(qimsg);
        return 0;
      }

      // Arithmetic DELIBERATELY unchanged: (width * bpp + 255) & ~255 gives 7680
      // for a 1896-wide primary, which is what the frozen host reconstruction
      // expects. What changes is that `bpp` now comes from the validated
      // descriptor instead of a bare 4, and the 256 has a name and a reason.
      const std::uint64_t pitch =
          (std::uint64_t(width) * scanoutFormat->bytesPerPixel + (kScanoutPitchAlign - 1))
          & ~(std::uint64_t(kScanoutPitchAlign) - 1);
      if (out_row_pitch) *out_row_pitch = pitch;
      if (out_offset)    *out_offset = 0;
      static std::atomic<std::uint32_t> s_scanDoneLogs{0};
      if (bridge_log_budget(s_scanDoneLogs, 64, 512)) {
        char msg[192];
        std::snprintf(msg, sizeof(msg),
          "CreateDdiScanoutTexture2D OPTIMAL %ux%u fmt=%u logicalPitch=%llu resource=%p",
          width, height, format, static_cast<unsigned long long>(pitch), resource);
        umd_log(msg);
      }
      return reinterpret_cast<std::size_t>(resource);
  });
}

std::size_t HeliosDxvkDevice::create_vertex_shader(const std::uint8_t* code, std::size_t len) const {
  return create_shader_impl<ID3D11VertexShader>(
      impl.get(), "vs", "CreateVertexShader", code, len,
      [](ID3D11Device* d, const void* bc, std::size_t n, ID3D11VertexShader** out) {
        return d->CreateVertexShader(bc, n, nullptr, out);
      });
}

std::size_t HeliosDxvkDevice::create_pixel_shader(const std::uint8_t* code, std::size_t len) const {
  return create_shader_impl<ID3D11PixelShader>(
      impl.get(), "ps", "CreatePixelShader", code, len,
      [](ID3D11Device* d, const void* bc, std::size_t n, ID3D11PixelShader** out) {
        return d->CreatePixelShader(bc, n, nullptr, out);
      });
}

std::size_t HeliosDxvkDevice::create_geometry_shader(const std::uint8_t* code, std::size_t len) const {
  return create_shader_impl<ID3D11GeometryShader>(
      impl.get(), "gs", "CreateGeometryShader", code, len,
      [](ID3D11Device* d, const void* bc, std::size_t n, ID3D11GeometryShader** out) {
        return d->CreateGeometryShader(bc, n, nullptr, out);
      });
}

std::size_t HeliosDxvkDevice::create_shader_sig(
    std::uint32_t kind,
    const std::uint8_t* code,
    std::size_t len,
    const std::uint32_t* sig_words,
    std::size_t sig_words_len) const {
  if (!impl || !impl->d3d11 || !code || !len || !sig_words || sig_words_len < 2)
    return 0;
  const std::uint32_t n_in = sig_words[0];
  const std::uint32_t n_out = sig_words[1];
  if (!signature_count_ok("create_shader_sig", n_in) ||
      !signature_count_ok("create_shader_sig", n_out))
    return 0;
  // Widen BEFORE adding: `n_in + n_out` was evaluated in std::uint32_t and only
  // then promoted, so a wrapped sum could satisfy the check the indexing below
  // depends on.
  if (sig_words_len != 2 + (std::size_t(n_in) + std::size_t(n_out)) * kSigEntryWords) {
    umd_log("create_shader_sig: signature word count mismatch");
    return 0;
  }
  const std::uint32_t* in_entries = sig_words + 2;
  const std::uint32_t* out_entries = in_entries + std::size_t(n_in) * kSigEntryWords;
  return bridge_guard("create_shader_sig", std::size_t(0), [&]() -> std::size_t {
      auto bytecode = prepare_shader_bytecode_with_sigs(
          code, len, in_entries, n_in, out_entries, n_out);
      if (!bytecode)
        return 0;
      const char* stage = kind == 0 ? "vs-sig" : kind == 1 ? "ps-sig" : "gs-sig";
      dump_shader_bytecode(stage, "raw", code, len);
      dump_shader_bytecode(stage, "wrapped", bytecode.data(), bytecode.len());
      HRESULT hr = E_FAIL;
      void* shader = nullptr;
      switch (kind) {
        case 0:
          hr = impl->d3d11->CreateVertexShader(bytecode.data(), bytecode.len(), nullptr,
                                               reinterpret_cast<ID3D11VertexShader**>(&shader));
          break;
        case 1:
          hr = impl->d3d11->CreatePixelShader(bytecode.data(), bytecode.len(), nullptr,
                                              reinterpret_cast<ID3D11PixelShader**>(&shader));
          break;
        case 2:
          hr = impl->d3d11->CreateGeometryShader(bytecode.data(), bytecode.len(), nullptr,
                                                 reinterpret_cast<ID3D11GeometryShader**>(&shader));
          break;
        default:
          umd_log("create_shader_sig: unknown shader kind");
          return 0;
      }
      if (FAILED(hr)) {
        umd_log("create_shader_sig: shader creation returned failure");
        return 0;
      }
      return reinterpret_cast<std::size_t>(shader);
  });
}

std::size_t HeliosDxvkDevice::create_tess_shader_sig(
    std::uint32_t kind,
    const std::uint8_t* code,
    std::size_t len,
    const std::uint32_t* sig_words,
    std::size_t sig_words_len) const {
  if (!impl || !impl->d3d11 || !code || !len || !sig_words || sig_words_len < 3)
    return 0;
  const std::uint32_t n_in = sig_words[0];
  const std::uint32_t n_out = sig_words[1];
  const std::uint32_t n_patch = sig_words[2];
  if (!signature_count_ok("create_tess_shader_sig", n_in) ||
      !signature_count_ok("create_tess_shader_sig", n_out) ||
      !signature_count_ok("create_tess_shader_sig", n_patch))
    return 0;
  if (sig_words_len !=
      3 + (std::size_t(n_in) + std::size_t(n_out) + std::size_t(n_patch)) * kSigEntryWords) {
    umd_log("create_tess_shader_sig: signature word count mismatch");
    return 0;
  }
  const std::uint32_t* in_entries = sig_words + 3;
  const std::uint32_t* out_entries = in_entries + std::size_t(n_in) * kSigEntryWords;
  const std::uint32_t* patch_entries = out_entries + std::size_t(n_out) * kSigEntryWords;
  return bridge_guard("create_tess_shader_sig", std::size_t(0), [&]() -> std::size_t {
      auto bytecode = prepare_shader_bytecode_with_tess_sigs(
          code, len, in_entries, n_in, out_entries, n_out, patch_entries, n_patch);
      if (!bytecode)
        return 0;
      const char* stage = kind == 0 ? "hs-sig" : "ds-sig";
      dump_shader_bytecode(stage, "raw", code, len);
      dump_shader_bytecode(stage, "wrapped", bytecode.data(), bytecode.len());
      HRESULT hr = E_FAIL;
      void* shader = nullptr;
      switch (kind) {
        case 0:
          hr = impl->d3d11->CreateHullShader(bytecode.data(), bytecode.len(), nullptr,
                                             reinterpret_cast<ID3D11HullShader**>(&shader));
          break;
        case 1:
          hr = impl->d3d11->CreateDomainShader(bytecode.data(), bytecode.len(), nullptr,
                                               reinterpret_cast<ID3D11DomainShader**>(&shader));
          break;
        default:
          umd_log("create_tess_shader_sig: unknown shader kind");
          return 0;
      }
      if (FAILED(hr)) {
        umd_log("create_tess_shader_sig: shader creation returned failure");
        return 0;
      }
      return reinterpret_cast<std::size_t>(shader);
  });
}

bool HeliosDxvkDevice::rotate_resource_backings(
    const std::size_t* d3d11_resource_ptrs,
    std::size_t count) const {
  if (!impl || !impl->d3d11 || !impl->context || !d3d11_resource_ptrs || count < 2)
    return false;
  return bridge_guard("rotate_resource_backings", false, [&]() -> bool {
      // Collect the DXVK images first; refuse the whole rotation if any entry
      // is not a storage-backed texture (a partial rotation would corrupt the
      // swapchain identity mapping). Rc refs: the swap executes later on the
      // CS thread and must not race resource destruction.
      std::vector<dxvk::Rc<dxvk::DxvkImage>> images;
      images.reserve(count);
      for (std::size_t i = 0; i < count; ++i) {
        auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptrs[i]);
        auto* texture = resource ? dxvk::GetCommonTexture(resource) : nullptr;
        if (!texture || texture->GetImage() == nullptr || texture->GetImage()->storage() == nullptr) {
          umd_log("rotate_resource_backings: entry without image storage");
          return false;
        }
        images.push_back(texture->GetImage());
      }

      // DXGI may only rotate identities within one compatible swapchain ring.
      // invalidateImage replaces the storage but intentionally leaves the
      // stable DxvkImage's format/shape metadata in place, so accepting a
      // mixed ring would attach (for example) packed RGB10A2 storage to a
      // BGRA8 image and make every downstream user reinterpret its bytes.
      const auto& expected = images[0]->info();
      for (std::size_t i = 1; i < images.size(); ++i) {
        const auto& actual = images[i]->info();
        if (actual.type        != expected.type
         || actual.format      != expected.format
         || actual.sampleCount != expected.sampleCount
         || actual.extent.width  != expected.extent.width
         || actual.extent.height != expected.extent.height
         || actual.extent.depth  != expected.extent.depth
         || actual.numLayers   != expected.numLayers
         || actual.mipLevels   != expected.mipLevels) {
          char msg[320];
          std::snprintf(msg, sizeof(msg),
            "rotate_resource_backings: incompatible slot %zu/%zu "
            "expected fmt=%u extent=%ux%ux%u samples=%u layers=%u mips=%u; "
            "got fmt=%u extent=%ux%ux%u samples=%u layers=%u mips=%u",
            i, images.size(),
            static_cast<unsigned>(expected.format),
            expected.extent.width, expected.extent.height, expected.extent.depth,
            static_cast<unsigned>(expected.sampleCount),
            expected.numLayers, expected.mipLevels,
            static_cast<unsigned>(actual.format),
            actual.extent.width, actual.extent.height, actual.extent.depth,
            static_cast<unsigned>(actual.sampleCount),
            actual.numLayers, actual.mipLevels);
          umd_log(msg);
          return false;
        }
      }

      // CS-side identity rotation (18th session), mirroring upstream
      // D3D11SwapChain::RotateBackBuffers: swap the storages ON the CS thread
      // via DxvkContext::invalidateImage. No GPU drain is needed — every
      // already-recorded command holds its own storage ref and keeps targeting
      // the pre-rotation memory; the swap applies in CS order for everything
      // recorded after this DDI. The two rejected designs, for the record:
      //  - whole-device event-query drain (bring-up shim): 15-25 ms per
      //    present, dominated by Sleep(1) timer quantization;
      //  - per-image waitForResource on the present thread: WEDGES dwm — the
      //    bound backbuffer RTV is re-recorded into every new open cmdlist, so
      //    isInUse(Read) never clears for a bound render target (proven live
      //    with a dwm minidump, thread 1 parked in synchronizeUntil).
      LARGE_INTEGER qpcFreq, qpcT0;
      QueryPerformanceFrequency(&qpcFreq);
      QueryPerformanceCounter(&qpcT0);

      // InjectCsOrderedAfterPending dispatches the open recording chunk and
      // appends the swap on the ordered CS queue WITHOUT waiting for the CS
      // thread: the earlier SynchronizeCsThread variant blocked the present
      // thread behind the whole CS queue — up to 1.9 s per present during
      // login churn (rotate-perf) — the owner-visible "occasional dips".
      auto* immediateContext = static_cast<dxvk::D3D11ImmediateContext*>(impl->context);

      immediateContext->InjectCsOrderedAfterPending([
        cImages = std::move(images)
      ] (dxvk::DxvkContext* ctx) {
        auto first = cImages[0]->storage();

        for (std::size_t i = 0; i + 1 < cImages.size(); ++i) {
          ctx->invalidateImage(cImages[i], cImages[i + 1]->storage(),
            cImages[i + 1]->info().layout);
        }

        ctx->invalidateImage(cImages[cImages.size() - 1u],
          std::move(first), cImages[0]->info().layout);
      });

      // Drain-cost telemetry (measure-first, PSC WS2): same key and format as
      // the old whole-device drain so before/after numbers compare directly.
      // One log line per 32 rotations.
      {
        LARGE_INTEGER qpcT1;
        QueryPerformanceCounter(&qpcT1);
        static PeriodicStat s_drainStat(32u);
        if (const auto sample = s_drainStat.record(qpc_elapsed_us(qpcFreq, qpcT0, qpcT1))) {
          char msg[128];
          std::snprintf(msg, sizeof(msg),
                        "rotate-perf: n=%u drain_avg_us=%llu drain_max_us=%llu",
                        sample->n,
                        static_cast<unsigned long long>(sample->avg_us),
                        static_cast<unsigned long long>(sample->max_us));
          umd_log(msg);
        }
      }

      // Debug instrument (registry-gated, off by default): sample the ring
      // buffers — write-side ground truth for "does the composed frame carry
      // pixels". Records after the injected swap, so slots are POST-rotation
      // identities. HKLM\SOFTWARE\Helios!RotateSample (DWORD) = sample every
      // Nth rotation.
      static std::atomic<std::uint32_t> s_sampleEvery{~0u};
      static std::atomic<std::uint32_t> s_rotateCount{0};
      std::uint32_t sampleEvery = s_sampleEvery.load(std::memory_order_relaxed);
      if (sampleEvery == ~0u) {
        DWORD value = 0, size = sizeof(value);
        if (RegGetValueA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", "RotateSample",
                         RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY, nullptr, &value, &size) != ERROR_SUCCESS)
          value = 0;
        sampleEvery = value;
        s_sampleEvery.store(sampleEvery, std::memory_order_relaxed);
      }
      if (sampleEvery && (s_rotateCount.fetch_add(1) % sampleEvery) == 0) {
        // Sample EVERY buffer in the ring, not just the presented one: a
        // nonzero count appearing in a slot other than [0] means content lands
        // in a buffer the present/rotation bookkeeping does not associate with
        // the presented allocation (ring misalignment), while all-zero across
        // the whole ring means the composition draws genuinely write nothing.
        for (std::size_t s = 0; s < count; ++s) {
          auto* res = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptrs[s]);
          ID3D11Texture2D* tex = nullptr;
          if (FAILED(res->QueryInterface(__uuidof(ID3D11Texture2D),
                                         reinterpret_cast<void**>(&tex)))) {
            char skipmsg[128];
            std::snprintf(skipmsg, sizeof(skipmsg),
                          "rotate-sample: slot=%zu/%zu SKIPPED (not a Texture2D)", s, count);
            umd_log(skipmsg);
            continue;
          }
          D3D11_TEXTURE2D_DESC td = {};
          tex->GetDesc(&td);
          // The old code forced MipLevels/ArraySize to 1 on the staging desc while
          // copying from the real one. With ArraySize > 1 the descriptions
          // mismatch, CopyResource is a silent no-op, and the tool reports
          // nonzero=0/N — the exact false conclusion ("the composition draws write
          // nothing") it exists to test for. And the rows below are read as
          // std::uint32_t, i.e. 32bpp is assumed: against a 16bpp ring the 4-byte
          // column stride reads past the last row, which is an OOB read, not
          // merely a wrong number. Skip and SAY SO instead.
          if (td.MipLevels != 1 || td.ArraySize != 1 || !is_32bpp_dxgi_format(td.Format)) {
            char skipmsg[192];
            std::snprintf(skipmsg, sizeof(skipmsg),
                          "rotate-sample: slot=%zu/%zu SKIPPED (mips=%u array=%u fmt=%u — "
                          "needs a single-subresource 32bpp texture)",
                          s, count, td.MipLevels, td.ArraySize,
                          static_cast<unsigned>(td.Format));
            umd_log(skipmsg);
            tex->Release();
            continue;
          }
          D3D11_TEXTURE2D_DESC sd = td;
          sd.BindFlags = 0;
          sd.MiscFlags = 0;
          sd.Usage = D3D11_USAGE_STAGING;
          sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
          ID3D11Texture2D* staging = nullptr;
          if (SUCCEEDED(impl->d3d11->CreateTexture2D(&sd, nullptr, &staging)) && staging) {
            impl->context->CopyResource(staging, tex);
            D3D11_MAPPED_SUBRESOURCE map = {};
            if (SUCCEEDED(impl->context->Map(staging, 0, D3D11_MAP_READ, 0, &map)) &&
                map.pData != nullptr) {
              const auto* base = static_cast<const std::uint8_t*>(map.pData);
              std::uint32_t nonzero = 0, samples = 0;
              for (UINT y = 0; y < td.Height; y += 64) {
                const auto* row = reinterpret_cast<const std::uint32_t*>(base + std::size_t(y) * map.RowPitch);
                for (UINT x = 0; x < td.Width; x += 64) {
                  ++samples;
                  nonzero += row[x] != 0;
                }
              }
              char msg[160];
              std::snprintf(msg, sizeof(msg),
                            "rotate-sample: slot=%zu/%zu %ux%u nonzero=%u/%u center=0x%08x",
                            s, count, td.Width, td.Height, nonzero, samples,
                            reinterpret_cast<const std::uint32_t*>(
                                base + std::size_t(td.Height / 2) * map.RowPitch)[td.Width / 2]);
              umd_log(msg);
              impl->context->Unmap(staging, 0);
            } else {
              char skipmsg[128];
              std::snprintf(skipmsg, sizeof(skipmsg),
                            "rotate-sample: slot=%zu/%zu SKIPPED (staging Map returned no data)",
                            s, count);
              umd_log(skipmsg);
            }
            staging->Release();
          } else {
            char skipmsg[128];
            std::snprintf(skipmsg, sizeof(skipmsg),
                          "rotate-sample: slot=%zu/%zu SKIPPED (staging CreateTexture2D failed)",
                          s, count);
            umd_log(skipmsg);
          }
          tex->Release();
        }
      }

      // The storage swap itself (resource[i] takes resource[i+1]'s, the last
      // takes the first's) executes in the injected CS command above.
      return true;
  });
}

std::int32_t HeliosDxvkDevice::dxgi_blt_convert(
    std::size_t dst_resource_ptr,
    std::uint32_t dst_subresource,
    std::uint32_t dst_x,
    std::uint32_t dst_y,
    std::size_t src_resource_ptr,
    std::uint32_t src_subresource,
    bool use_src_box,
    std::uint32_t src_left,
    std::uint32_t src_top,
    std::uint32_t src_right,
    std::uint32_t src_bottom) const {
  if (!impl || !impl->context || !dst_resource_ptr || !src_resource_ptr)
    return -1;

  return bridge_guard("dxgi_blt_convert", -1, [&]() -> std::int32_t {
      auto* dstTex = dxvk::GetCommonTexture(
        reinterpret_cast<ID3D11Resource*>(dst_resource_ptr));
      auto* srcTex = dxvk::GetCommonTexture(
        reinterpret_cast<ID3D11Resource*>(src_resource_ptr));
      if (!dstTex || !dstTex->GetImage() || !srcTex || !srcTex->GetImage()) {
        umd_log("dxgi_blt_convert: non-texture resource");
        return -1;
      }
      if (dst_subresource >= dstTex->CountSubresources()
       || src_subresource >= srcTex->CountSubresources()) {
        umd_log("dxgi_blt_convert: subresource out of range");
        return -1;
      }

      dxvk::Rc<dxvk::DxvkImage> dstImage = dstTex->GetImage();
      dxvk::Rc<dxvk::DxvkImage> srcImage = srcTex->GetImage();
      const auto* dstFormat = dstImage->formatInfo();
      const auto* srcFormat = srcImage->formatInfo();
      if (!dstFormat || !srcFormat
       || dstFormat->aspectMask != VK_IMAGE_ASPECT_COLOR_BIT
       || srcFormat->aspectMask != VK_IMAGE_ASPECT_COLOR_BIT
       || dstImage->info().sampleCount != VK_SAMPLE_COUNT_1_BIT
       || srcImage->info().sampleCount != VK_SAMPLE_COUNT_1_BIT) {
        umd_log("dxgi_blt_convert: needs single-sampled color images");
        return -1;
      }

      const VkImageSubresourceLayers dstLayers = dxvk::vk::makeSubresourceLayers(
        dstTex->GetSubresourceFromIndex(dstFormat->aspectMask, dst_subresource));
      const VkImageSubresourceLayers srcLayers = dxvk::vk::makeSubresourceLayers(
        srcTex->GetSubresourceFromIndex(srcFormat->aspectMask, src_subresource));
      const VkExtent3D srcMip = srcTex->MipLevelExtent(srcLayers.mipLevel);
      const VkExtent3D dstMip = dstTex->MipLevelExtent(dstLayers.mipLevel);

      VkOffset3D srcOffset = { 0, 0, 0 };
      VkExtent3D extent = srcMip;
      if (use_src_box) {
        if (src_right <= src_left || src_bottom <= src_top
         || src_right > srcMip.width || src_bottom > srcMip.height) {
          umd_log("dxgi_blt_convert: invalid source rectangle");
          return -1;
        }
        srcOffset = {
          static_cast<std::int32_t>(src_left),
          static_cast<std::int32_t>(src_top),
          0,
        };
        extent = { src_right - src_left, src_bottom - src_top, 1u };
      }
      if (dst_x > dstMip.width || dst_y > dstMip.height
       || extent.width > dstMip.width - dst_x
       || extent.height > dstMip.height - dst_y
       || extent.depth != 1u) {
        umd_log("dxgi_blt_convert: destination region out of bounds");
        return -1;
      }

      static_cast<dxvk::D3D11ImmediateContext*>(impl->context)->HeliosConvertImage(
        dstImage, dstLayers,
        VkOffset3D { static_cast<std::int32_t>(dst_x), static_cast<std::int32_t>(dst_y), 0 },
        srcImage, srcLayers, srcOffset, extent);
      static std::atomic<std::uint32_t> s_converted{0};
      const std::uint32_t n = s_converted.fetch_add(1, std::memory_order_relaxed) + 1;
      if (n <= 4 || (n % 16384u) == 0) {
        char msg[192];
        std::snprintf(msg, sizeof(msg),
          "dxgi_blt_convert: queued #%u src_vk=%u dst_vk=%u extent=%ux%u",
          n,
          static_cast<unsigned>(srcImage->info().format),
          static_cast<unsigned>(dstImage->info().format),
          extent.width, extent.height);
        umd_log(msg);
      }
      return 0;
  });
}

// Keep backend failures separate from bounded completion timeouts.
static std::atomic<std::uint32_t> s_gateFailures{0};
static std::atomic<std::uint32_t> s_gateNoContext{0};

// Producers whose present could not be published, by stage. Every one of these
// means a consumer will read that surface UNORDERED -- the black-frame defect --
// so none of them may be silent.
static std::atomic<std::uint32_t> s_publishNoResource{0};
static std::atomic<std::uint32_t> s_publishFenceFailed{0};
static std::atomic<std::uint32_t> s_publishSlotFailed{0};
static std::atomic<std::uint64_t> s_publishOk{0};
static std::atomic<std::uint32_t> s_presentStreamRegistered{0};
static std::atomic<std::uint32_t> s_presentStreamUnavailable{0};

bool HeliosDxvkDevice::publish_present_order(std::size_t d3d11_resource_ptr,
                                             std::uint32_t* out_ctx_id,
                                             std::uint32_t* out_value32,
                                             std::uint64_t* out_cookie) const {
  if (out_ctx_id) *out_ctx_id = 0;
  if (out_value32) *out_value32 = 0;
  if (out_cookie) *out_cookie = 0;
  if (!impl || !impl->context)
    return false;

  return bridge_guard("publish_present_order", false, [&]() -> bool {
    if (!d3d11_resource_ptr)
      return false;

    auto* texture = dxvk::GetCommonTexture(
      reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr));
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return false;
    auto storage = texture->GetImage()->storage();
    auto producer = storage->heliosProducer();
    if (producer == nullptr) {
      s_publishNoResource.fetch_add(1, std::memory_order_relaxed);
      umd_log("producer: missing exact allocation binding");
      return false;
    }

    // All potentially re-entrant work stays outside present_order_mutex.  At
    // most one caller initializes the timeline; concurrent callers wait for
    // that one attempt and observe either its ready state or its permanent
    // failure latch.
    bool initialize_present_fence = false;
    {
      std::unique_lock lock(impl->present_order_mutex);
      while (impl->present_fence_initializing)
        impl->present_order_ready.wait(lock);
      if (impl->present_fence_failed)
        return false;
      if (impl->present_fence == nullptr) {
        impl->present_fence_initializing = true;
        initialize_present_fence = true;
      }
    }

    if (initialize_present_fence) {
      dxvk::Rc<dxvk::DxvkFence> fence;
      std::uint64_t cookie = 0;

      // `createFence` enters Vulkan and the private registration enters the
      // ICD/KMD, so neither runs under present_order_mutex.
      try {
        dxvk::DxvkFenceCreateInfo fenceInfo = { };
        fenceInfo.sharedType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT;
        fence = impl->device->createFence(fenceInfo);

        uint32_t ctx = 0;
        if (producer->registerStream(impl->device->vkd()->device(), fence->handle(), &ctx, &cookie)
         && ctx == impl->venus_ctx_id) {
          const auto n = s_presentStreamRegistered.fetch_add(
              1, std::memory_order_relaxed) + 1;
          char stream_msg[192];
          std::snprintf(stream_msg, sizeof(stream_msg),
            "present-stream: registered ctx=%u cookie=%llu (x%u)",
            impl->venus_ctx_id, static_cast<unsigned long long>(cookie), n);
          umd_log(stream_msg);
        } else {
          const auto n = s_presentStreamUnavailable.fetch_add(
              1, std::memory_order_relaxed) + 1;
          char stream_msg[192];
          std::snprintf(stream_msg, sizeof(stream_msg),
            "present-stream: unavailable (old ICD/KMD or refused registration, x%u)", n);
          umd_log(stream_msg);
          throw dxvk::DxvkError("Helios: exact producer stream registration failed");
        }
      } catch (const dxvk::DxvkError& e) {
        // Latch: retrying per present would spam and never succeed.  Release
        // every concurrent waiter before returning through the old fallback.
        {
          std::lock_guard lock(impl->present_order_mutex);
          impl->present_fence_failed = true;
          impl->present_fence_initializing = false;
        }
        impl->present_order_ready.notify_all();
        s_publishFenceFailed.fetch_add(1, std::memory_order_relaxed);
        char msg[256];
        std::snprintf(msg, sizeof(msg),
          "producer: stream initialization FAILED (%s)",
          e.message().c_str());
        umd_log(msg);
        producer->abort();
        return false;
      } catch (...) {
        // bridge_guard owns the diagnostic, but must not leave concurrent
        // publishers waiting forever after an unexpected initialization error.
        {
          std::lock_guard lock(impl->present_order_mutex);
          impl->present_fence_failed = true;
          impl->present_fence_initializing = false;
        }
        impl->present_order_ready.notify_all();
        throw;
      }

      {
        std::lock_guard lock(impl->present_order_mutex);
        impl->present_fence = std::move(fence);
        impl->present_stream_cookie = cookie;
        impl->present_fence_initializing = false;
      }
      impl->present_order_ready.notify_all();

    }

    // Recording and publication share one order. The closure and its command
    // list retain an operation that fails the allocation if never submitted.
    // This preserves the folded signal and the existing present flush/batching.
    std::lock_guard lock(impl->present_order_mutex);
    if (impl->present_fence_failed || impl->present_fence == nullptr
     || impl->present_value == UINT32_MAX || !impl->venus_ctx_id) {
      producer->abort();
      return false;
    }
    const auto value = ++impl->present_value;
    dxvk::Rc<dxvk::HeliosProducerOperation> operation =
      new dxvk::HeliosProducerOperation(producer);
    static_cast<dxvk::D3D11ImmediateContext*>(impl->context)
      ->HeliosSignalPresentFence(impl->present_fence, value, operation);
    uint64_t epoch = 0;
    if (!producer->publish(impl->present_fence->handle(), value, &epoch)) {
      producer->abort();
      s_publishSlotFailed.fetch_add(1, std::memory_order_relaxed);
      umd_log("producer: allocation epoch publication failed");
      return false;
    }
    if (out_ctx_id) *out_ctx_id = impl->venus_ctx_id;
    if (out_value32) *out_value32 = uint32_t(value);
    if (out_cookie) *out_cookie = impl->present_stream_cookie;
    s_publishOk.fetch_add(1, std::memory_order_relaxed);
    return true;
  });
}

bool HeliosDxvkDevice::set_scanout_acquire_event(std::size_t event_handle) const noexcept {
  return bridge_guard("set_scanout_acquire_event", false, [&]() -> bool {
    if (!impl || impl->device == nullptr || !event_handle)
      return false;

    impl->device->heliosScanoutAcquire().setEventHandle(
      reinterpret_cast<HANDLE>(event_handle));

    char msg[96];
    std::snprintf(msg, sizeof(msg),
      "scanout-acquire: retirement event 0x%zx delivered to DXVK signaler",
      event_handle);
    umd_log(msg);
    return true;
  });
}

std::int32_t HeliosDxvkDevice::present_frame_gate(std::uint32_t timeout_us,
                                               std::uint32_t order_mode) const {
  if (!impl || !impl->context) {
    s_gateNoContext.fetch_add(1, std::memory_order_relaxed);
    return E_FAIL;
  }
  const auto outcome = bridge_guard<std::int32_t>(
      "present_frame_gate", E_FAIL, [&]() -> std::int32_t {
    LARGE_INTEGER qpcFreq, qpcT0, qpcT1;
    QueryPerformanceFrequency(&qpcFreq);
    QueryPerformanceCounter(&qpcT0);

    auto* immediateContext = static_cast<dxvk::D3D11ImmediateContext*>(impl->context);
    bool completed;
    if (order_mode == kPresentOrderSubmitted) {
      if (!immediateContext->HeliosWaitFrameSubmitted()) {
        umd_log("present_frame_gate: command stream/submission failed");
        return E_FAIL;
      }
      completed = true;
    } else if (order_mode == kPresentOrderComplete) {
      const auto result = immediateContext->HeliosWaitFrameComplete(timeout_us);
      if (result != VK_SUCCESS && result != VK_TIMEOUT) {
        umd_log("present_frame_gate: command stream/submission failed");
        return E_FAIL;
      }
      completed = result == VK_SUCCESS;
    } else {
      return E_FAIL;
    }

    // Gate-cost telemetry (PSC WS2 discipline): one line per 128 presents.
    QueryPerformanceCounter(&qpcT1);
    // Site-local extra, deliberately NEVER reset (unlike the running max), so
    // the printed count is cumulative for the process.
    static std::atomic<std::uint32_t> s_gateTimeouts{0};
    if (!completed)
      s_gateTimeouts.fetch_add(1, std::memory_order_relaxed);
    static PeriodicStat s_gateStat(128u);
    if (const auto sample = s_gateStat.record(qpc_elapsed_us(qpcFreq, qpcT0, qpcT1))) {
      // The existing keys keep their names, order and format so before/after
      // runs compare directly; `failed` and `noctx` are APPENDED. R826.
      char msg[220];
      std::snprintf(msg, sizeof(msg),
                    "present-gate: n=%u avg_us=%llu max_us=%llu timeouts=%u "
                    "failed=%u noctx=%u mode=%u",
                    sample->n,
                    static_cast<unsigned long long>(sample->avg_us),
                    static_cast<unsigned long long>(sample->max_us),
                    s_gateTimeouts.load(std::memory_order_relaxed),
                    s_gateFailures.load(std::memory_order_relaxed),
                    s_gateNoContext.load(std::memory_order_relaxed),
                    order_mode);
      umd_log(msg);
    }
    return completed ? S_OK : S_FALSE;
  });
  if (outcome < 0)
    s_gateFailures.fetch_add(1, std::memory_order_relaxed);
  return outcome;
}

std::uint64_t HeliosDxvkDevice::flush_present_copy() const {
  return bridge_guard("flush_present_copy", std::uint64_t(0), [&]() -> std::uint64_t {
    if (!impl || !impl->context || impl->device->getDeviceStatus() != VK_SUCCESS)
      return 0;
    return static_cast<dxvk::D3D11ImmediateContext*>(impl->context)->HeliosFlushFrame();
  });
}

std::int32_t HeliosDxvkDevice::wait_present_copy(
    std::uint64_t submission_id, std::uint32_t timeout_us) const {
  return bridge_guard("wait_present_copy", -1, [&]() -> std::int32_t {
    if (!impl || !impl->context || !submission_id)
      return -1;
    const auto status = static_cast<dxvk::D3D11ImmediateContext*>(impl->context)
      ->HeliosWaitSubmissionComplete(submission_id, timeout_us);
    return status == VK_SUCCESS ? 0 : status == VK_TIMEOUT ? 1 : -1;
  });
}

std::int32_t HeliosDxvkDevice::present_vehicle_copy(
    std::size_t dst_resource_ptr,
    std::size_t src_resource_ptr,
    std::size_t semaphore_handle,
    std::uint64_t semaphore_value) const {
  if (!impl || !impl->context || !dst_resource_ptr || !src_resource_ptr)
    return -1;

  return bridge_guard("present_vehicle_copy", -1, [&]() -> std::int32_t {
      if (!semaphore_handle || !semaphore_value)
        return -1;
      dxvk::Rc<dxvk::DxvkFence> semaphore;
      {
        std::lock_guard lock(impl->vehicle_semaphore_mutex);
        using CompareFn = BOOL (WINAPI*)(HANDLE, HANDLE);
        static const auto compare = reinterpret_cast<CompareFn>(GetProcAddress(
          GetModuleHandleW(L"KernelBase.dll"), "CompareObjectHandles"));
        if (!compare) return -1;
        const HANDLE source = reinterpret_cast<HANDLE>(semaphore_handle);
        if (!impl->vehicle_semaphore_handle || !compare(source, impl->vehicle_semaphore_handle)) {
          HANDLE retained = nullptr;
          if (!DuplicateHandle(GetCurrentProcess(), source, GetCurrentProcess(),
                &retained, 0, FALSE, DUPLICATE_SAME_ACCESS)) return -1;
          dxvk::DxvkFenceCreateInfo info = { };
          info.sharedType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT;
          info.sharedHandle = retained;
          try {
            semaphore = impl->device->createFence(info);
          } catch (...) {
            CloseHandle(retained);
            throw;
          }
          if (impl->vehicle_semaphore_handle) CloseHandle(impl->vehicle_semaphore_handle);
          impl->vehicle_semaphore_handle = retained;
          impl->vehicle_semaphore = semaphore;
        } else {
          semaphore = impl->vehicle_semaphore;
        }
      }
      auto* dstTex = dxvk::GetCommonTexture(
        reinterpret_cast<ID3D11Resource*>(dst_resource_ptr));
      auto* srcTex = dxvk::GetCommonTexture(
        reinterpret_cast<ID3D11Resource*>(src_resource_ptr));
      if (!dstTex || !dstTex->GetImage() || !srcTex || !srcTex->GetImage()) {
        umd_log("present_vehicle_copy: non-texture resource");
        return -1;
      }

      dxvk::Rc<dxvk::DxvkImage> dstImage = dstTex->GetImage();
      dxvk::Rc<dxvk::DxvkImage> srcImage = srcTex->GetImage();

      // Source the LIVE storage: device-local imports carry the creator's
      // pixels in the direct-bind staging ALIAS image; the texture's own image
      // is a private surface refreshed only when a prior read armed it (frame 1
      // would be undefined). Direct (non-staged) imports read their own image.
      if (srcImage->heliosStagingImage() != nullptr)
        srcImage = srcImage->heliosStagingImage();

      const VkExtent3D dstExtent = dstImage->info().extent;
      const VkExtent3D srcExtent = srcImage->info().extent;
      const VkExtent3D extent = {
        std::min(dstExtent.width,  srcExtent.width),
        std::min(dstExtent.height, srcExtent.height),
        1u,
      };

      static_cast<dxvk::D3D11ImmediateContext*>(impl->context)
        ->HeliosCopyExternalFrame(dstImage, srcImage, extent, semaphore, semaphore_value);

      // Geometry mismatch is copyable (min region) but must be loud — during
      // resize churn one letterboxed frame is fine, a silent steady state of
      // them is a caller bug.
      const bool mismatch = dstExtent.width != srcExtent.width
                         || dstExtent.height != srcExtent.height;
      if (mismatch) {
        static std::atomic<std::uint32_t> s_mismatch{0};
        const std::uint32_t n = s_mismatch.fetch_add(1, std::memory_order_relaxed) + 1;
        if (n == 1 || (n % 128u) == 0) {
          char msg[160];
          std::snprintf(msg, sizeof(msg),
            "present_vehicle_copy: geometry mismatch dst=%ux%u src=%ux%u (x%u)",
            dstExtent.width, dstExtent.height, srcExtent.width, srcExtent.height, n);
          umd_log(msg);
        }
        return 1;
      }
      return 0;
  });
}

std::int32_t HeliosDxvkDevice::present_snapshot_copy(
    std::size_t dst_resource_ptr,
    std::size_t src_resource_ptr,
    bool windowed_blt_reservation) const {
  if (!impl || !impl->context || !dst_resource_ptr || !src_resource_ptr)
    return -1;

  return bridge_guard("present_snapshot_copy", -1, [&]() -> std::int32_t {
      auto* dstTex = dxvk::GetCommonTexture(
        reinterpret_cast<ID3D11Resource*>(dst_resource_ptr));
      auto* srcTex = dxvk::GetCommonTexture(
        reinterpret_cast<ID3D11Resource*>(src_resource_ptr));
      if (!dstTex || !dstTex->GetImage() || !srcTex || !srcTex->GetImage()) {
        umd_log("present_snapshot_copy: non-texture resource");
        return -1;
      }

      dxvk::Rc<dxvk::DxvkImage> dstImage = dstTex->GetImage();
      dxvk::Rc<dxvk::DxvkImage> srcImage = srcTex->GetImage();

      // No staging-alias substitution here, deliberately: the presented
      // primary is this device's own DXVK image, never an import, so its own
      // image IS the live storage. present_vehicle_copy's heliosStagingImage
      // arm exists only for cross-context imports.

      const VkExtent3D dstExtent = dstImage->info().extent;
      const VkExtent3D srcExtent = srcImage->info().extent;
      // Reject stale geometry before recording work: a partial snapshot
      // would retain pixels from a previous frame outside the copied region.
      const bool mismatch = dstExtent.width != srcExtent.width
                         || dstExtent.height != srcExtent.height;
      if (mismatch) {
        static std::atomic<std::uint32_t> s_mismatch{0};
        const std::uint32_t n = s_mismatch.fetch_add(1, std::memory_order_relaxed) + 1;
        if (n <= 8 || (n % 128u) == 0) {
          char msg[160];
          std::snprintf(msg, sizeof(msg),
            "present_snapshot_copy: geometry mismatch dst=%ux%u src=%ux%u (x%u)",
            dstExtent.width, dstExtent.height, srcExtent.width, srcExtent.height, n);
          umd_log(msg);
        }
        return 1;
      }
      const VkExtent3D extent = srcExtent;

      if (!static_cast<dxvk::D3D11ImmediateContext*>(impl->context)
            ->HeliosCopyPresentSnapshot(
              dstImage, srcImage, extent, windowed_blt_reservation)) {
        // A WindowedBlt reader lease is reserved by KMD Present. Without the
        // pre-arm reservation the producer list could wait on that same lease,
        // so this is a hard snapshot refusal, never a best-effort copy.
        umd_log("present_snapshot_copy: incompatible image geometry or WindowedBlt reuse reservation unavailable");
        return -1;
      }

      return 0;
  });
}

std::size_t HeliosDxvkDevice::create_hull_shader(const std::uint8_t* code, std::size_t len) const {
  return create_shader_impl<ID3D11HullShader>(
      impl.get(), "hs", "CreateHullShader", code, len,
      [](ID3D11Device* d, const void* bc, std::size_t n, ID3D11HullShader** out) {
        return d->CreateHullShader(bc, n, nullptr, out);
      });
}

std::size_t HeliosDxvkDevice::create_domain_shader(const std::uint8_t* code, std::size_t len) const {
  return create_shader_impl<ID3D11DomainShader>(
      impl.get(), "ds", "CreateDomainShader", code, len,
      [](ID3D11Device* d, const void* bc, std::size_t n, ID3D11DomainShader** out) {
        return d->CreateDomainShader(bc, n, nullptr, out);
      });
}

std::size_t HeliosDxvkDevice::create_compute_shader(const std::uint8_t* code, std::size_t len) const {
  return create_shader_impl<ID3D11ComputeShader>(
      impl.get(), "cs", "CreateComputeShader", code, len,
      [](ID3D11Device* d, const void* bc, std::size_t n, ID3D11ComputeShader** out) {
        return d->CreateComputeShader(bc, n, nullptr, out);
      });
}

std::unique_ptr<HeliosDxvkDevice> helios_dxvk_create_device(
    std::uint32_t luid_low,
    std::int32_t  luid_high,
    bool timer_resolution) {
  // R824: configuration delivered as a process-global side effect, whose
  // correctness used to be statement position -- these writes happened on EVERY
  // CreateDevice DDI, and one process (dwm) creates several D3D11 devices, so
  // the block was rewritten while earlier DxvkInstances were live and
  // _putenv_s is not safe against a concurrent getenv. The values are identical
  // on every call, so doing it once is behaviour-preserving; what goes away is
  // the repeat writes and the concurrent-write window.
  //
  // Static guarantee: none. std::call_once is a runtime construct and an
  // `EnvConfigured` token would be ceremony around one call site. _putenv_s
  // stays the mechanism because DXVK reads env; changing that is out of scope.
  static std::once_flag s_envOnce;
  std::call_once(s_envOnce, [] {
    // Force selection of the Helios venus device if other ICDs are present.
    _putenv_s("DXVK_FILTER_DEVICE_NAME", "Virtio-GPU Venus");
    // HELIOS_DXVK_KMT_SHARED is no longer forced here: the engine defaults it
    // ON (2026-08-05). Forcing it made a knob that could not be off in any
    // configuration this process ever produced, which hid the fact that the
    // engine's own default disagreed with every measurement taken.

    // Debug instrument (registry-gated, off by default): route DXVK's shader
    // dumping into every UMD-hosting process — session-0 services (dwm) cannot
    // be given process env vars any other way. HKLM\SOFTWARE\Helios!
    // ShaderDumpPath (REG_SZ) = target directory.
    char dumpPath[MAX_PATH] = {};
    DWORD size = sizeof(dumpPath);
    const bool haveDump =
        RegGetValueA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", "ShaderDumpPath",
                     RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY, nullptr, dumpPath, &size) == ERROR_SUCCESS &&
        dumpPath[0];
    if (haveDump)
      _putenv_s("DXVK_SHADER_DUMP_PATH", dumpPath);

    char msg[MAX_PATH + 128];
    std::snprintf(msg, sizeof(msg),
      "dxvk env configured once: DXVK_FILTER_DEVICE_NAME=Virtio-GPU Venus "
      "kmt-shared=default-on DXVK_SHADER_DUMP_PATH=%s",
      haveDump ? dumpPath : "(unset)");
    umd_log(msg);
  });

  return bridge_guard<std::unique_ptr<HeliosDxvkDevice>>(
      "helios_dxvk_create_device", nullptr,
      [&]() -> std::unique_ptr<HeliosDxvkDevice> {
      auto out = std::make_unique<HeliosDxvkDevice>();
      out->impl = std::make_unique<HeliosDxvkDeviceImpl>(timer_resolution);
      auto& d = *out->impl;

      d.instance = new dxvk::DxvkInstance(dxvk::DxvkInstanceFlags());

      if (luid_low != 0 || luid_high != 0) {
        LUID luid;
        luid.LowPart  = luid_low;
        luid.HighPart = luid_high;
        d.adapter = d.instance->findAdapterByLuid(&luid);
        if (d.adapter == nullptr)
          umd_log("findAdapterByLuid found nothing; falling back to adapter 0");
      }

      if (d.adapter == nullptr)
        d.adapter = d.instance->enumAdapters(0);

      if (d.adapter == nullptr) {
        umd_log("no Vulkan adapter enumerated (venus ICD not present?)");
        return nullptr;
      }

      d.device = d.adapter->createDevice();
      if (d.device == nullptr) {
        umd_log("DxvkAdapter::createDevice returned null");
        return nullptr;
      }
      d.venus_ctx_id = read_instance_venus_context_id(d.instance->handle());
      // ⛔ S4b (`ARCHITECTURE.md` §6.4). The read above is the first thing that
      // forces `resolve_helios_icd_module`, which now reconciles against the
      // process-global anchor. If it refused, a SECOND venus ICD module is live
      // in this process — `helios_umd12.dll` selected a different one — and
      // every `VkDeviceMemory`/`VkInstance` identity this device would go on to
      // stamp is derived from the wrong one.
      //
      // ⚠ This is the one place S4b can change SHIPPING D3D11 behaviour, so be
      // exact about when: `icd_anchor_poisoned()` is false unless two distinct
      // modules both exported `helios_venus_memory_alloc_info` and the two UMDs
      // disagreed. In every process on this box today — one UMD, one ICD — it
      // cannot fire. Degrading instead would be fake success: the device comes
      // up, renders, and writes cross-process allocation identities nobody can
      // resolve.
      if (helios_bridge::icd_anchor_poisoned()) {
        umd_log("REFUSING DXVK device: venus ICD anchor mismatch "
                "(two ICD modules live in this process)");
        return nullptr;
      }
      if (!d.venus_ctx_id)
        umd_log("DXVK device created but Venus context export returned 0");
      umd_log("DxvkDevice created on venus adapter OK");

      // Instantiate DXVK's full D3D11 COM device from the DxvkDevice. The DDI
      // device-funcs forward to this ID3D11Device / its immediate context.
      // `new HeliosStubAdapter()` starts at refcount 1 and is only released AFTER
      // the D3D11DXGIDevice constructor returns — but that constructor builds the
      // D3D11 device and its immediate context and can throw dxvk::DxvkError, in
      // which case the catch below returns nullptr and the Release() never runs.
      // The guard makes the zero-refcount window exit through exactly one path.
      ComRelease<HeliosStubAdapter> stubAdapter(new HeliosStubAdapter());
      auto* dxgiDevice = new dxvk::D3D11DXGIDevice(
          stubAdapter.get(), nullptr, nullptr,
          d.instance, d.adapter, d.device,
          D3D_FEATURE_LEVEL_11_0, 0);
      stubAdapter.reset(); // dxgiDevice holds its own ref now

      HRESULT hr = dxgiDevice->QueryInterface(__uuidof(ID3D11Device),
                                              reinterpret_cast<void**>(&d.d3d11));
      if (FAILED(hr) || d.d3d11 == nullptr) {
        umd_log("QueryInterface(ID3D11Device) on D3D11DXGIDevice failed");
        // dxgiDevice has refcount 0 here (QI failed) — drop it.
        delete dxgiDevice;
        return nullptr;
      }
      // d.d3d11 now holds the one ref that keeps dxgiDevice alive.
      d.d3d11->GetImmediateContext(&d.context);
      umd_log("D3D11 COM device + immediate context created OK");
      return out;
  });
}
