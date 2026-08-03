// Helios UMD <-> DXVK engine bridge implementation.
//
// Wraps DXVK's DxvkInstance/DxvkAdapter/DxvkDevice behind the opaque
// HeliosDxvkDevice. The DXVK engine references a frontend-provided
// `Logger::s_instance` global (normally defined in src/d3d11/d3d11_main.cpp,
// which we do not build) — we provide it here.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <sddl.h>

#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <exception>
#include <share.h>
#include <tlhelp32.h>

#include "dxvk_bridge.h"

#include "bridge_common.h"
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
#include "dxvk_helios_present_sync.h"

// After the DXVK headers: see the include-order note in this header.
#include "bridge_icd_exports.h"

namespace dxbc_spv::dxbc {
  util::md5::Digest hashDxbcBinary(const void* data, size_t size);
}

namespace dxvk {
  // Frontend-provided global the DXVK engine links against. The string is the
  // log file name DXVK writes engine diagnostics to.
  Logger Logger::s_instance("helios_umd_dxvk.log");
}

namespace helios_bridge {
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

  // First `first` occurrences, then every `every`-th — the idiom the periodic
  // bridge telemetry already uses. The per-create lines below were
  // unconditional, and each one is an _fsopen + fprintf + fclose.
  bool bridge_log_budget(std::atomic<std::uint32_t>& counter,
                         std::uint32_t first,
                         std::uint32_t every) {
    const std::uint32_t n = counter.fetch_add(1, std::memory_order_relaxed) + 1;
    return n <= first || (every != 0 && (n % every) == 0);
  }


  // The "atomic total + CAS max + count + log every Nth + reset max" idiom,
  // open-coded at two telemetry sites with two different periods.
  //
  // `bridge_log_budget` above is deliberately a SEPARATE, smaller helper for the
  // simpler count-and-rate-limit counters -- forcing the two shapes together
  // would give both a worse API than either has now. (The review names three
  // accumulators; `copy-lat:` is gone, so there are two.)
  //
  // Per-site extras stay at the sites: `present-gate:` also accumulates
  // `s_gateTimeouts`, which it deliberately never resets, plus two failure
  // counters. Each site keeps its own log key and format string, so before/after
  // numbers stay directly comparable -- which is the whole reason those lines
  // exist.
  class PeriodicStat {
  public:
    struct Sample {
      std::uint32_t n;
      std::uint64_t avg_us;
      std::uint64_t max_us;
    };

    // `period` must be a power of two: the two sites tested `(n & 31u) == 0`
    // and `(n & 127u) == 0`, and keeping the mask form keeps that cadence exact.
    explicit constexpr PeriodicStat(std::uint32_t period) : mask_(period - 1u) {}

    // Record one measurement. Returns the sample to log when this call lands on
    // the period boundary, resetting the running max exactly as both sites did.
    std::optional<Sample> record(std::uint64_t us) {
      total_us_.fetch_add(us, std::memory_order_relaxed);
      std::uint64_t prev_max = max_us_.load(std::memory_order_relaxed);
      while (us > prev_max &&
             !max_us_.compare_exchange_weak(prev_max, us, std::memory_order_relaxed)) {}
      const std::uint32_t n = count_.fetch_add(1, std::memory_order_relaxed) + 1;
      if ((n & mask_) != 0)
        return std::nullopt;
      const Sample sample{
        n,
        total_us_.load(std::memory_order_relaxed) / n,
        max_us_.load(std::memory_order_relaxed),
      };
      max_us_.store(0, std::memory_order_relaxed);
      return sample;
    }

  private:
    const std::uint32_t mask_;
    std::atomic<std::uint64_t> total_us_{0};
    std::atomic<std::uint64_t> max_us_{0};
    std::atomic<std::uint32_t> count_{0};
  };

  // Microseconds between two QueryPerformanceCounter reads. Both telemetry
  // sites spelled this division out.
  std::uint64_t qpc_elapsed_us(const LARGE_INTEGER& freq,
                               const LARGE_INTEGER& t0,
                               const LARGE_INTEGER& t1) {
    return std::uint64_t(t1.QuadPart - t0.QuadPart) * 1000000ull
         / std::uint64_t(freq.QuadPart);
  }
  /// The direct-scanout PRIMARY create's QI failure — the one whose silent zero
  /// R401 now reports to the runtime.
  std::atomic<std::uint32_t> g_scanoutPrimaryQiFailed{0};

  // Owns one COM reference until it is deliberately released. Non-copyable, so
  // the reference cannot be duplicated into a second owner by accident.
  template <typename T>
  class ComRelease {
  public:
    explicit ComRelease(T* ptr) : m_ptr(ptr) {}
    ComRelease(const ComRelease&) = delete;
    ComRelease& operator=(const ComRelease&) = delete;
    ~ComRelease() { reset(); }
    T* get() const { return m_ptr; }
    void reset() {
      if (m_ptr) {
        m_ptr->Release();
        m_ptr = nullptr;
      }
    }
  private:
    T* m_ptr = nullptr;
  };

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

  // cxx emits EVERY generated C++ shim `noexcept` (verified verbatim in the
  // checked-in generated artifact, bridge.rs.cc), so an exception escaping a
  // bridge method is std::terminate — dwm.exe dies instead of the DDI returning
  // a failure. Seven methods had no handler at all, and every one of them
  // reaches code that allocates (find_helios_icd_export ->
  // discover_vulkan_icd_manifests builds a std::vector<std::string>, runs
  // ifstream/ostringstream over the manifest and concatenates strings; the
  // now-retired present_flip_wait_setup additionally took a lock_guard,
  // make_shared and constructed a std::thread). Defect class: a recoverable
  // resource failure escalated to unconditional death of the compositor.
  //
  // R1014(4): this is now the ONLY catch triple in the file. The other nine
  // were hand-written copies whose DxvkError arm built a std::string, which is
  // the bug the paragraph below describes -- so folding them in is a
  // robustness fix, not only a dedupe.
  //
  // Making this the only path that can return the sentinel collapses "error
  // sentinel" and "escaped exception" into one code path. The compiler cannot
  // prove a body is exception-free, so that is the honest limit of the
  // guarantee.
  //
  // The catch arms must not allocate: a std::string built inside a
  // std::bad_alloc handler can throw again. Fixed char[] + snprintf only —
  // which is also why DxvkError::message() (returns std::string) is not called
  // here.
  //
  // `R` is deduced from `on_error` ALONE — the body's return type is not a
  // deduction context — so the sentinel silently decides the type every
  // SUCCESS value is converted to on the way out. A bare `0` against a
  // `std::size_t` body deduced `R = int` and truncated every returned pointer
  // to 32 bits: that shipped in T7 and access-violated dwm and LogonUI at the
  // first `VSSetShader`. The static_assert makes the mismatch a compile error
  // instead of a pointer that looks plausible in a log line.
  template <typename R, typename Fn>
  R bridge_guard(const char* what, R on_error, Fn&& fn) noexcept {
    static_assert(std::is_same_v<R, decltype(fn())>,
                  "bridge_guard's error value must have the guarded body's exact "
                  "return type; otherwise the success path is converted too");
    try {
      return fn();
    } catch (const dxvk::DxvkError&) {
      char msg[160];
      std::snprintf(msg, sizeof(msg), "%s: DxvkError", what);
      umd_log(msg);
    } catch (const std::exception& e) {
      char msg[256];
      std::snprintf(msg, sizeof(msg), "%s: exception: %s", what, e.what());
      umd_log(msg);
    } catch (...) {
      char msg[160];
      std::snprintf(msg, sizeof(msg), "%s: unknown exception", what);
      umd_log(msg);
    }
    return on_error;
  }

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

struct HeliosDxvkDeviceImpl {
  dxvk::Rc<dxvk::DxvkInstance> instance;
  dxvk::Rc<dxvk::DxvkAdapter>  adapter;
  dxvk::Rc<dxvk::DxvkDevice>   device;
  ID3D11Device*        d3d11   = nullptr; // QI'd from D3D11DXGIDevice; holds it alive
  ID3D11DeviceContext* context = nullptr; // immediate context
  std::uint32_t venus_ctx_id = 0;

  // Cross-process present ordering, PRODUCER side. The named timeline this
  // device signals at each frame's GPU completion, published per presented
  // resource id so a consumer (dwm, compositing this app's window) can turn it
  // into a GPU-side wait instead of us CPU-blocking on our own GPU work.
  // Created lazily on the first present: an app that never presents never mints
  // a kernel object.
  //
  // The lock serializes lazy creation and signal recording across free-threaded
  // DDI callers.  It is never held while creating the Vulkan fence or sending
  // the private ICD registration escape, both of which can leave this bridge.
  std::mutex present_order_mutex;
  std::condition_variable present_order_ready;
  // Remaining producer timeline state is guarded by present_order_mutex.
  bool present_fence_initializing = false;
  dxvk::Rc<dxvk::DxvkFence> present_fence;
  std::uint32_t present_fence_id = 0;
  std::uint64_t present_value    = 0;
  bool          present_fence_failed = false;

  // A missing new ICD export or old KMD is a permanent per-device fallback,
  // never a per-present retry/escape.  The initialized fence state makes the
  // one registration attempt explicit without a separate mutable flag.
  std::uint64_t present_stream_cookie = 0;

  ~HeliosDxvkDeviceImpl() {
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

    texture->GetImage()->storage()->setKmtHandles(local, global);

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
    const auto rawMemory = reinterpret_cast<std::uintptr_t>(info.memory);
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
    std::uint32_t* memory_type_index) const noexcept {
  return bridge_guard("get_resource_alloc_identity", false, [&]() -> bool {
    if (venus_alloc_size)
      *venus_alloc_size = 0;
    if (memory_type_index)
      *memory_type_index = 0;

    if (!d3d11_resource_ptr)
      return false;

    auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr);
    auto* texture = dxvk::GetCommonTexture(resource);
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return false;

    auto info = texture->GetImage()->storage()->getMemoryInfo();
    return venus_memory_alloc_info_from_handle(info.memory, venus_alloc_size, memory_type_index);
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
        static_cast<unsigned long long>(reinterpret_cast<std::uintptr_t>(info.memory)),
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
    bool scanout_linear,
    bool linear_scanout_target,
    bool cross_context_optimal) const {
  if (!impl || !impl->d3d11 || !global || !renderer_resource_id || !width || !height)
    return 0;

  return bridge_guard("open_ddi_texture2d", std::size_t(0), [&]() -> std::size_t {
      {
        static std::atomic<std::uint32_t> s_openBeginLogs{0};
        if (bridge_log_budget(s_openBeginLogs, 64, 512)) {
          char msg[256];
          std::snprintf(msg, sizeof(msg),
            "OpenDdiTexture2D begin %ux%u fmt=%u bind=0x%08x misc=0x%08x global=0x%08x renderer_res=%u alloc_size=%llu mem_type=%u",
            width, height, format, bind_flags, misc_flags, global, renderer_resource_id,
            static_cast<unsigned long long>(venus_alloc_size), memory_type_index);
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

      return reinterpret_cast<std::size_t>(resource);
  });
}

std::size_t HeliosDxvkDevice::create_ddi_scanout_texture2d(
    std::uint32_t width,
    std::uint32_t height,
    std::uint32_t format,
    std::uint32_t bind_flags,
    std::uint32_t misc_flags,
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
          char msg[192];
          std::snprintf(msg, sizeof(msg),
            "CreateDdiScanoutTexture2D begin %ux%u fmt=%u bind=0x%08x misc=0x%08x",
            width, height, format, bind_flags, misc_flags);
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
                         RRF_RT_REG_DWORD, nullptr, &value, &size) != ERROR_SUCCESS)
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

// R826 counters. The gate is the stage's own measurement instrument and it
// could not see one of its own failure modes: the catch arms returned false
// without touching s_gateTimeouts, so a thrown exception was indistinguishable
// from a timeout to every consumer -- forward.rs maps false to return code
// 1 = timeout and increments EXT_FLIP_GATE_TIMEOUTS. Since R1014(4) the arms
// live in bridge_guard and the distinction is the std::nullopt outcome.
static std::atomic<std::uint32_t> s_gateExceptions{0};
// The no-context arm. Counted at zero cost, but NOT a live failure mode:
// GetImmediateContext runs before the device is handed to Rust, so this is
// unreachable rather than rare. Recorded so the distinction is in the code.
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

    // The id the CONSUMER will look up. dwm imports our presented surface by
    // venus resource id (DxvkSharedHandleInfo::heliosResourceId), so the
    // publish key has to be that same id, taken from the backing memory.
    auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr);
    auto* texture = dxvk::GetCommonTexture(resource);
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return false;

    // Keep the exact backing allocation alive through the slot publication and
    // mark it on success. Its destructor is the only valid release boundary:
    // D3D wrappers can rotate backing storages while this Venus resource lives.
    auto storage = texture->GetImage()->storage();
    const auto info = storage->getMemoryInfo();
    const std::uint32_t resid = venus_memory_resource_id_from_handle(info.memory);

    if (!resid) {
      const auto n = s_publishNoResource.fetch_add(1, std::memory_order_relaxed) + 1;
      if (n == 1 || (n % 512) == 0) {
        char msg[160];
        std::snprintf(msg, sizeof(msg),
          "present-order: presented surface has no venus resource id (x%u) "
          "- consumers will read it unordered", n);
        umd_log(msg);
      }
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
      // One named timeline per D3D11 device. A NULL DACL is deliberate and is
      // the reason this needs a security descriptor at all: the consumer is
      // dwm, which runs as its own principal (Window Manager\DWM-N), so the
      // default descriptor -- owner-only -- would make the name unopenable and
      // every consumer wait silently degrade to no wait. The object is a
      // timeline semaphore carrying a frame counter; it grants no access to
      // memory or content.
      SECURITY_DESCRIPTOR sd = { };
      SECURITY_ATTRIBUTES sa = { };
      bool haveSa = false;

      if (InitializeSecurityDescriptor(&sd, SECURITY_DESCRIPTOR_REVISION)
       && SetSecurityDescriptorDacl(&sd, TRUE, nullptr, FALSE)) {
        sa.nLength = sizeof(sa);
        sa.lpSecurityDescriptor = &sd;
        sa.bInheritHandle = FALSE;
        haveSa = true;
      }

      static std::atomic<std::uint32_t> s_nextFenceId{1};
      const std::uint32_t fenceId = s_nextFenceId.fetch_add(1, std::memory_order_relaxed);
      dxvk::Rc<dxvk::DxvkFence> fence;
      std::uint64_t cookie = 0;

      // `createFence` enters Vulkan and the private registration enters the
      // ICD/KMD, so neither runs under present_order_mutex.
      try {
        const std::wstring name = L"Global\\HeliosPresentFence_"
          + std::to_wstring(static_cast<unsigned long>(GetCurrentProcessId()))
          + L"_" + std::to_wstring(fenceId);
        dxvk::DxvkFenceCreateInfo fenceInfo = { };
        fenceInfo.initialValue = 0u;
        fenceInfo.sharedType   = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT;
        fenceInfo.ntExportName = name.c_str();
        fenceInfo.ntSecurityAttributes = haveSa ? &sa : nullptr;
        fence = impl->device->createFence(fenceInfo);

        // The exact VkDevice + VkSemaphore pair DXVK just created is the only
        // admissible registration identity.  An old ICD missing this private
        // DLL export stays a zero-correlation fallback without changing
        // ordinary present publication.
        if (venus_register_present_stream(
              impl->device->vkd()->device(), fence->handle(), &cookie)) {
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
          "present-order: named present fence creation FAILED (%s) "
          "- consumers will read this process's surfaces unordered",
          e.message().c_str());
        umd_log(msg);
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
        impl->present_fence_id = fenceId;
        impl->present_stream_cookie = cookie;
        impl->present_fence_initializing = false;
      }
      impl->present_order_ready.notify_all();

      char msg[200];
      std::snprintf(msg, sizeof(msg),
        "present-order: publishing as pid=%lu fence=%u",
        static_cast<unsigned long>(GetCurrentProcessId()), fenceId);
      umd_log(msg);
    }

    // Preserve the signal-then-slot-publication order across free-threaded
    // callers.  HeliosSignalPresentFence only takes DXVK's immediate-context
    // lock and emits a CS closure; HeliosPresentSync::publish only performs
    // shared-map/seqlock work.  Neither calls back into this UMD, so this
    // narrow lock cannot be recursively re-entered by either operation.
    std::uint64_t value = 0;
    std::uint32_t fenceId = 0;
    std::uint64_t streamCookie = 0;
    bool slot_published = false;
    {
      std::lock_guard lock(impl->present_order_mutex);
      if (impl->present_fence_failed || impl->present_fence == nullptr)
        return false;

      // Record the signal on the CS stream BEFORE publishing, so the value a
      // consumer reads is one this device has already committed to reaching.
      // The signal executes at GPU completion of everything recorded so far,
      // i.e. the frame being presented.
      value = ++impl->present_value;
      auto* immediateContext =
        static_cast<dxvk::D3D11ImmediateContext*>(impl->context);
      immediateContext->HeliosSignalPresentFence(impl->present_fence, value);

      fenceId = impl->present_fence_id;
      streamCookie = impl->present_stream_cookie;
      slot_published = dxvk::HeliosPresentSync::publish(resid,
        static_cast<std::uint32_t>(GetCurrentProcessId()), fenceId, value);
      if (slot_published && !storage->setHeliosPresentSlot(resid, fenceId)) {
        // A VkDeviceMemory has one immutable (fence generation, Venus resid)
        // publication identity. Do not leave an untracked slot behind if that
        // invariant is violated.
        dxvk::HeliosPresentSync::release(resid, fenceId);
        slot_published = false;
      }
      if (slot_published && streamCookie && value > 0 &&
          value <= UINT32_MAX && impl->venus_ctx_id) {
        if (out_ctx_id) *out_ctx_id = impl->venus_ctx_id;
        if (out_value32) *out_value32 = static_cast<std::uint32_t>(value);
        if (out_cookie) *out_cookie = streamCookie;
      }
    }

    if (!slot_published) {
      const auto n = s_publishSlotFailed.fetch_add(1, std::memory_order_relaxed) + 1;
      if (n == 1 || (n % 512) == 0) {
        char msg[160];
        std::snprintf(msg, sizeof(msg),
          "present-order: slot publish FAILED for resid %u (x%u)", resid, n);
        umd_log(msg);
      }
      return false;
    }

    const auto ok = s_publishOk.fetch_add(1, std::memory_order_relaxed) + 1;
    if (ok == 1 || (ok % 2048) == 0) {
      char msg[200];
      std::snprintf(msg, sizeof(msg),
        "present-order: published n=%llu resid=%u value=%llu "
        "(no_resid=%u fence_fail=%u slot_fail=%u)",
        static_cast<unsigned long long>(ok), resid,
        static_cast<unsigned long long>(value),
        s_publishNoResource.load(std::memory_order_relaxed),
        s_publishFenceFailed.load(std::memory_order_relaxed),
        s_publishSlotFailed.load(std::memory_order_relaxed));
      umd_log(msg);
    }
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

bool HeliosDxvkDevice::present_frame_gate(std::uint32_t timeout_us,
                                          std::uint32_t order_mode) const {
  if (!impl || !impl->context) {
    s_gateNoContext.fetch_add(1, std::memory_order_relaxed);
    return false;
  }
  // A tri-state rather than a plain `bool` sentinel: an exception is a
  // DIFFERENT outcome from a timeout (R826) and bumps its own counter, so
  // `bridge_guard`'s error value has to be distinguishable from `false`.
  const auto outcome = bridge_guard<std::optional<bool>>(
      "present_frame_gate", std::nullopt, [&]() -> std::optional<bool> {
    LARGE_INTEGER qpcFreq, qpcT0, qpcT1;
    QueryPerformanceFrequency(&qpcFreq);
    QueryPerformanceCounter(&qpcT0);

    auto* immediateContext = static_cast<dxvk::D3D11ImmediateContext*>(impl->context);
    // Both arms satisfy the same ordering contract -- the frame's Venus work is
    // on the wire before pfnRenderCb samples the KMD watermark. The SUBMITTED
    // arm stops there; the COMPLETE arm additionally waits out the GPU, which
    // is a whole frame of CPU/GPU overlap the contract never asked for. Kept as
    // one entry point with one telemetry line so the two compare directly.
    bool completed;
    if (order_mode == kPresentOrderSubmitted) {
      immediateContext->HeliosWaitFrameSubmitted();
      completed = true;
    } else {
      completed = immediateContext->HeliosWaitFrameComplete(timeout_us);
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
                    s_gateExceptions.load(std::memory_order_relaxed),
                    s_gateNoContext.load(std::memory_order_relaxed),
                    order_mode);
      umd_log(msg);
    }
    return completed;
  });
  // R826: the exception arms are a DIFFERENT outcome from a timeout and are
  // counted as one. The `bool` return and the bounded timeout are KEPT -- this
  // is a real event wait with a safety bound, which the frozen baseline keeps.
  // A later, separate commit may return an
  // `enum class GateOutcome { Completed, TimedOut, Failed }` so the Rust caller
  // must handle Failed explicitly instead of reporting it as a timeout.
  if (!outcome) {
    s_gateExceptions.fetch_add(1, std::memory_order_relaxed);
    return false;
  }
  return *outcome;
}

std::int32_t HeliosDxvkDevice::present_vehicle_copy(
    std::size_t dst_resource_ptr,
    std::size_t src_resource_ptr) const {
  if (!impl || !impl->context || !dst_resource_ptr || !src_resource_ptr)
    return -1;

  return bridge_guard("present_vehicle_copy", -1, [&]() -> std::int32_t {
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
        ->HeliosCopyExternalFrame(dstImage, srcImage, extent);

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
    std::size_t src_resource_ptr) const {
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
      const VkExtent3D extent = {
        std::min(dstExtent.width,  srcExtent.width),
        std::min(dstExtent.height, srcExtent.height),
        1u,
      };

      static_cast<dxvk::D3D11ImmediateContext*>(impl->context)
        ->HeliosCopyPresentSnapshot(dstImage, srcImage, extent);

      // A mismatch means the ring was built against stale geometry. The min
      // region has been copied, but the caller must NOT substitute this
      // present — the slot's remaining pixels are whatever a previous frame
      // left there — so this is loud on every early occurrence.
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
    std::int32_t  luid_high) {
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
    _putenv_s("HELIOS_DXVK_KMT_SHARED", "1");

    // Debug instrument (registry-gated, off by default): route DXVK's shader
    // dumping into every UMD-hosting process — session-0 services (dwm) cannot
    // be given process env vars any other way. HKLM\SOFTWARE\Helios!
    // ShaderDumpPath (REG_SZ) = target directory.
    char dumpPath[MAX_PATH] = {};
    DWORD size = sizeof(dumpPath);
    const bool haveDump =
        RegGetValueA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", "ShaderDumpPath",
                     RRF_RT_REG_SZ, nullptr, dumpPath, &size) == ERROR_SUCCESS &&
        dumpPath[0];
    if (haveDump)
      _putenv_s("DXVK_SHADER_DUMP_PATH", dumpPath);

    char msg[MAX_PATH + 128];
    std::snprintf(msg, sizeof(msg),
      "dxvk env configured once: DXVK_FILTER_DEVICE_NAME=Virtio-GPU Venus "
      "HELIOS_DXVK_KMT_SHARED=1 DXVK_SHADER_DUMP_PATH=%s",
      haveDump ? dumpPath : "(unset)");
    umd_log(msg);
  });

  return bridge_guard<std::unique_ptr<HeliosDxvkDevice>>(
      "helios_dxvk_create_device", nullptr,
      [&]() -> std::unique_ptr<HeliosDxvkDevice> {
      auto out = std::make_unique<HeliosDxvkDevice>();
      out->impl = std::make_unique<HeliosDxvkDeviceImpl>();
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
