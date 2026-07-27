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

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <cstring>
#include <memory>
#include <mutex>
#include <optional>
#include <thread>
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
#include "d3d11_texture.h"
#include "d3d11_context_imm.h"

namespace dxbc_spv::dxbc {
  util::md5::Digest hashDxbcBinary(const void* data, size_t size);
}

namespace dxvk {
  // Frontend-provided global the DXVK engine links against. The string is the
  // log file name DXVK writes engine diagnostics to.
  Logger Logger::s_instance("helios_umd_dxvk.log");
}

namespace {
  void umd_log(const char* msg);

  template<typename Fn>
  Fn find_export_in_loaded_modules(const char* export_name) {
    HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, GetCurrentProcessId());
    if (snapshot != INVALID_HANDLE_VALUE) {
      MODULEENTRY32 module = {};
      module.dwSize = sizeof(module);
      if (Module32First(snapshot, &module)) {
        do {
          auto fn = reinterpret_cast<Fn>(GetProcAddress(module.hModule, export_name));
          if (fn) {
            CloseHandle(snapshot);
            return fn;
          }
        } while (Module32Next(snapshot, &module));
      }
      CloseHandle(snapshot);
    }
    return nullptr;
  }

  std::string trim_ascii(std::string s) {
    while (!s.empty() && (s.front() == ' ' || s.front() == '\t' || s.front() == '\r' || s.front() == '\n'))
      s.erase(s.begin());
    while (!s.empty() && (s.back() == ' ' || s.back() == '\t' || s.back() == '\r' || s.back() == '\n'))
      s.pop_back();
    return s;
  }

  std::string dirname_of(const std::string& path) {
    const auto pos = path.find_last_of("\\/");
    if (pos == std::string::npos)
      return {};
    return path.substr(0, pos);
  }

  bool is_absolute_windows_path(const std::string& path) {
    return path.size() >= 3 &&
      ((path[0] >= 'A' && path[0] <= 'Z') || (path[0] >= 'a' && path[0] <= 'z')) &&
      path[1] == ':' &&
      (path[2] == '\\' || path[2] == '/');
  }

  std::string unescape_json_string(const std::string& in) {
    std::string out;
    out.reserve(in.size());
    for (std::size_t i = 0; i < in.size(); i++) {
      if (in[i] != '\\' || i + 1 >= in.size()) {
        out.push_back(in[i]);
        continue;
      }
      const char c = in[++i];
      switch (c) {
      case '\\': out.push_back('\\'); break;
      case '/': out.push_back('/'); break;
      case '"': out.push_back('"'); break;
      case 'n': out.push_back('\n'); break;
      case 'r': out.push_back('\r'); break;
      case 't': out.push_back('\t'); break;
      default:
        out.push_back(c);
        break;
      }
    }
    return out;
  }

  std::string read_text_file(const std::string& path) {
    std::ifstream file(path, std::ios::binary);
    if (!file)
      return {};
    std::ostringstream ss;
    ss << file.rdbuf();
    return ss.str();
  }

  std::string parse_icd_library_path(const std::string& json) {
    const std::string key = "\"library_path\"";
    const auto key_pos = json.find(key);
    if (key_pos == std::string::npos)
      return {};
    const auto colon = json.find(':', key_pos + key.size());
    if (colon == std::string::npos)
      return {};
    auto quote = json.find('"', colon + 1);
    if (quote == std::string::npos)
      return {};

    std::string raw;
    bool escaped = false;
    for (std::size_t i = quote + 1; i < json.size(); i++) {
      const char c = json[i];
      if (escaped) {
        raw.push_back('\\');
        raw.push_back(c);
        escaped = false;
        continue;
      }
      if (c == '\\') {
        escaped = true;
        continue;
      }
      if (c == '"')
        return unescape_json_string(raw);
      raw.push_back(c);
    }
    return {};
  }

  std::string resolve_icd_library_path(const std::string& manifest_path) {
    const auto json = read_text_file(manifest_path);
    if (json.empty())
      return {};

    auto library_path = parse_icd_library_path(json);
    if (library_path.empty())
      return {};

    for (auto& c : library_path) {
      if (c == '/')
        c = '\\';
    }

    if (is_absolute_windows_path(library_path) || (library_path.size() >= 2 && library_path[0] == '\\' && library_path[1] == '\\'))
      return library_path;

    const auto dir = dirname_of(manifest_path);
    if (dir.empty())
      return library_path;
    return dir + "\\" + library_path;
  }

  void add_manifest_list(std::vector<std::string>& out, const char* list) {
    if (!list || !*list)
      return;

    const char* p = list;
    while (*p) {
      const char* start = p;
      while (*p && *p != ';')
        p++;
      auto item = trim_ascii(std::string(start, p - start));
      if (!item.empty())
        out.push_back(item);
      if (*p == ';')
        p++;
    }
  }

  void add_env_manifests(std::vector<std::string>& manifests) {
    char* value = nullptr;
    std::size_t len = 0;
    if (_dupenv_s(&value, &len, "VK_DRIVER_FILES") == 0 && value) {
      add_manifest_list(manifests, value);
      std::free(value);
    }
    value = nullptr;
    len = 0;
    if (_dupenv_s(&value, &len, "VK_ICD_FILENAMES") == 0 && value) {
      add_manifest_list(manifests, value);
      std::free(value);
    }
  }

  void add_registry_manifests_from(HKEY root, const char* subkey, std::vector<std::string>& manifests) {
    HKEY key = nullptr;
    if (RegOpenKeyExA(root, subkey, 0, KEY_READ | KEY_WOW64_64KEY, &key) != ERROR_SUCCESS)
      return;

    for (DWORD i = 0;; i++) {
      char name[1024] = {};
      DWORD name_len = sizeof(name);
      DWORD type = 0;
      DWORD value = 1;
      DWORD value_len = sizeof(value);
      const auto rc = RegEnumValueA(key, i, name, &name_len, nullptr, &type,
        reinterpret_cast<LPBYTE>(&value), &value_len);
      if (rc == ERROR_NO_MORE_ITEMS)
        break;
      if (rc != ERROR_SUCCESS)
        continue;
      if (type == REG_DWORD && value == 0 && name[0])
        manifests.push_back(name);
    }

    RegCloseKey(key);
  }

  std::vector<std::string> discover_vulkan_icd_manifests() {
    std::vector<std::string> manifests;
    add_env_manifests(manifests);
    add_registry_manifests_from(HKEY_LOCAL_MACHINE, "SOFTWARE\\Khronos\\Vulkan\\Drivers", manifests);
    add_registry_manifests_from(HKEY_CURRENT_USER, "SOFTWARE\\Khronos\\Vulkan\\Drivers", manifests);
    manifests.push_back("C:\\ProgramData\\HeliosVulkan\\virtio_devenv_icd.x86_64.json");
    return manifests;
  }

  template<typename Fn>
  Fn find_export_via_vulkan_icd_manifests(const char* export_name) {
    auto manifests = discover_vulkan_icd_manifests();
    for (const auto& manifest : manifests) {
      const auto dll = resolve_icd_library_path(manifest);
      if (dll.empty())
        continue;
      HMODULE mod = LoadLibraryA(dll.c_str());
      if (!mod)
        continue;
      auto fn = reinterpret_cast<Fn>(GetProcAddress(mod, export_name));
      if (!fn) {
        // Release the reference this call took. Without it the refcount grows
        // on EVERY probe of an export this module does not have — and the
        // success cache does not bound that, because a miss is retried on
        // every call by design (an export can legitimately appear later). The
        // miss path is the only one where the growth was ever unbounded.
        // LoadLibraryA on an already-loaded module only increments, so this
        // returns the module to exactly its previous state and can never
        // unload one another component is using.
        FreeLibrary(mod);
        continue;
      }
      // Deliberately NOT freed on the hit path: `fn` points into this module,
      // so the reference is what keeps it valid. The success cache bounds that
      // to one extra reference per export (seven total).

      char msg[512];
      std::snprintf(msg, sizeof(msg), "resolved %s via ICD manifest %s -> %s",
        export_name, manifest.c_str(), dll.c_str());
      umd_log(msg);
      return fn;
    }
    return nullptr;
  }

  // The seven Mesa-ICD exports this bridge resolves, as a process-wide table.
  //
  // `find_helios_icd_export` used to cache NOTHING. Every call took a full
  // TH32CS_SNAPMODULE snapshot and called GetProcAddress on every loaded module
  // until it reached the Mesa ICD — which is late in the load order, so most of
  // the list gets walked — and on a miss it then walked the Vulkan ICD manifest
  // list, read and parsed JSON off disk, and LoadLibraryA'd each candidate with
  // NO matching FreeLibrary, so a persistent miss also grew module refcounts
  // without bound. `get_resource_memory_info` alone costs two lookups per call.
  //
  // CRITICAL: cache SUCCESSES ONLY, per export. A std::call_once or a magic
  // static over the resolution would latch an early nullptr — the Mesa ICD is
  // not loaded until `new dxvk::DxvkInstance`, and helios_venus_query_scanout in
  // particular can legitimately miss before the KMD has a bound primary — and
  // that would permanently disable the venus identity plumbing for the process.
  // Failure retries exactly as it did before; the per-slot std::atomic is what
  // makes retry-on-failure race-free without a mutex. No code path stores
  // nullptr into a slot.
  //
  // FreeLibrary: released on the MISS path (see
  // find_export_via_vulkan_icd_manifests), deliberately held on the HIT path
  // because the cached pointer is INTO that module. Caching bounds the hit-path
  // references to one per export; it does NOT bound the miss path, which is
  // retried on every call by design — that is why the miss needs the free.
  enum class HeliosIcdExport : std::size_t {
    CurrentCtxId = 0,
    InstanceCtxId,
    MemoryId,
    MemoryResId,
    MemoryTransferOwnership,
    MemoryAllocInfo,
    Count,
  };

  constexpr std::size_t kHeliosIcdExportCount =
    static_cast<std::size_t>(HeliosIcdExport::Count);

  const char* helios_icd_export_name(HeliosIcdExport slot) {
    switch (slot) {
    case HeliosIcdExport::CurrentCtxId:  return "helios_venus_current_ctx_id";
    case HeliosIcdExport::InstanceCtxId: return "helios_venus_instance_ctx_id";
    case HeliosIcdExport::MemoryId:      return "helios_venus_memory_id";
    case HeliosIcdExport::MemoryResId:   return "helios_venus_memory_res_id";
    case HeliosIcdExport::MemoryTransferOwnership:
      return "helios_venus_memory_transfer_resource_ownership";
    case HeliosIcdExport::MemoryAllocInfo: return "helios_venus_memory_alloc_info";
    default: return "";
    }
  }

  // Discovery order is unchanged, so a resolution that works today still works.
  void* resolve_helios_icd_export(HeliosIcdExport slot) {
    static std::atomic<void*> s_cache[kHeliosIcdExportCount];
    const auto index = static_cast<std::size_t>(slot);
    if (index >= kHeliosIcdExportCount)
      return nullptr;
    if (void* cached = s_cache[index].load(std::memory_order_acquire))
      return cached;

    const char* export_name = helios_icd_export_name(slot);
    void* fn = find_export_in_loaded_modules<void*>(export_name);
    if (!fn)
      fn = find_export_via_vulkan_icd_manifests<void*>(export_name);
    if (fn)
      s_cache[index].store(fn, std::memory_order_release);
    return fn;
  }

  template<typename Fn>
  Fn helios_icd_export(HeliosIcdExport slot) {
    return reinterpret_cast<Fn>(resolve_helios_icd_export(slot));
  }

  // The "export unavailable" lines were per-call file I/O on a path that runs
  // once or twice per resource create; rate-limit them the way the rest of the
  // bridge telemetry is limited.
  void log_export_unavailable(HeliosIcdExport slot) {
    static std::atomic<std::uint32_t> s_counts[kHeliosIcdExportCount];
    const auto index = static_cast<std::size_t>(slot);
    if (index >= kHeliosIcdExportCount)
      return;
    const std::uint32_t n =
      s_counts[index].fetch_add(1, std::memory_order_relaxed) + 1;
    if (n == 1 || (n % 512u) == 0) {
      char msg[192];
      std::snprintf(msg, sizeof(msg), "%s export unavailable (x%u)",
        helios_icd_export_name(slot), n);
      umd_log(msg);
    }
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

  // First `first` occurrences, then every `every`-th — the idiom the periodic
  // bridge telemetry already uses. The per-create lines below were
  // unconditional, and each one is an _fsopen + fprintf + fclose.
  bool bridge_log_budget(std::atomic<std::uint32_t>& counter,
                         std::uint32_t first,
                         std::uint32_t every) {
    const std::uint32_t n = counter.fetch_add(1, std::memory_order_relaxed) + 1;
    return n <= first || (every != 0 && (n % every) == 0);
  }


  bool plausible_venus_context_id(std::uint32_t ctx) {
    // KMD-assigned context ids are small monotonically allocated integers. A
    // value such as 0xcccccc00 means the instance-scoped export decoded the
    // wrong handle/object, not a real Venus context.
    //
    // The `(ctx & 0xff000000u) != 0xcc000000u` conjunct that used to be here was
    // provably inert: `ctx < 0x01000000u` already forces the top byte to zero,
    // so it excluded nothing the range check had not already excluded.
    return ctx != 0 && ctx < 0x01000000u;
  }

  std::atomic<std::uint32_t> g_venusCtxFallbackImplausible{0};

  std::uint32_t read_current_venus_context_id() {
    using Fn = std::uint32_t (__cdecl*)();
    auto fn = helios_icd_export<Fn>(HeliosIcdExport::CurrentCtxId);
    if (!fn)
      return 0;

    const auto ctx = fn();
    // The process-global export is last-writer-wins across two live venus
    // instances, so its result was the LEAST trustworthy of the two and was the
    // only one no plausibility test was applied to. Return 0 on an implausible
    // value so the caller's loud "Venus context export returned 0" path fires
    // instead of a silent wrong stamp on every WDDM allocation identity this
    // device creates.
    if (ctx && !plausible_venus_context_id(ctx)) {
      const std::uint32_t n =
        g_venusCtxFallbackImplausible.fetch_add(1, std::memory_order_relaxed) + 1;
      char msg[160];
      std::snprintf(msg, sizeof(msg),
        "process-global venus ctx export returned implausible ctx_id=%u -> 0 (x%u)",
        ctx, n);
      umd_log(msg);
      return 0;
    }
    if (ctx) {
      char msg[128];
      std::snprintf(msg, sizeof(msg), "Venus context export returned ctx_id=%u", ctx);
      umd_log(msg);
    }

    return ctx;
  }

  // Instance-scoped venus ctx id (23rd-session audit): the process-global
  // "current" export is last-writer-wins, and with the dcomp present vehicle
  // a game process holds TWO live venus instances — a concurrent instance
  // create (overlays) between our device init and this read would mis-stamp
  // every WDDM allocation identity this device creates. Resolve through OUR
  // VkInstance; fall back to the process-global export against an older ICD.
  std::uint32_t read_instance_venus_context_id(VkInstance instance) {
    using Fn = std::uint32_t (__cdecl*)(VkInstance);
    if (instance) {
      if (auto fn = helios_icd_export<Fn>(HeliosIcdExport::InstanceCtxId)) {
        const auto ctx = fn(instance);
        if (plausible_venus_context_id(ctx)) {
          char msg[128];
          std::snprintf(msg, sizeof(msg),
            "Venus instance-scoped ctx export returned ctx_id=%u", ctx);
          umd_log(msg);
          return ctx;
        }
        // Mutually exclusive with the "unavailable" line below: printing both
        // gave triage two contradicting causes for the same fallback.
        if (ctx) {
          char msg[160];
          std::snprintf(msg, sizeof(msg),
            "Venus instance-scoped ctx export returned implausible ctx_id=%u; falling back",
            ctx);
          umd_log(msg);
        }
        return read_current_venus_context_id();
      }
    }
    umd_log("instance-scoped venus ctx export unavailable; "
            "falling back to process-global current_ctx_id");
    return read_current_venus_context_id();
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

namespace {
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
  // a failure. Most methods in this file are already wrapped in a three-arm
  // catch triple; seven were not, and every one of them reaches code that
  // allocates (find_helios_icd_export -> discover_vulkan_icd_manifests builds a
  // std::vector<std::string>, runs ifstream/ostringstream over the manifest and
  // concatenates strings; the now-retired present_flip_wait_setup additionally
  // took a lock_guard, make_shared and constructed a std::thread). Defect class: a
  // recoverable resource failure escalated to unconditional death of the
  // compositor.
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
  template <typename R, typename Fn>
  R bridge_guard(const char* what, R on_error, Fn&& fn) noexcept {
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


  const char* shader_bytecode_dump_path() {
    static std::string path = [] {
      char value[MAX_PATH] = {};
      DWORD size = sizeof(value);
      if (RegGetValueA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", "ShaderBytecodeDumpPath",
                       RRF_RT_REG_SZ, nullptr, value, &size) != ERROR_SUCCESS ||
          !value[0])
        return std::string();

      CreateDirectoryA(value, nullptr);
      return std::string(value);
    }();

    return path.empty() ? nullptr : path.c_str();
  }

  void dump_shader_bytecode(
      const char* stage,
      const char* form,
      const void* data,
      std::size_t len) {
    const char* dir = shader_bytecode_dump_path();
    if (!dir || !data || !len)
      return;

    static std::atomic<std::uint32_t> s_seq { 0u };
    const auto seq = s_seq.fetch_add(1u, std::memory_order_relaxed);

    char path[MAX_PATH] = {};
    _snprintf_s(path, sizeof(path), _TRUNCATE,
      "%s\\shader-%lu-%05u-%s-%s-%zu.dxbc",
      dir,
      static_cast<unsigned long>(GetCurrentProcessId()),
      seq,
      stage,
      form,
      len);

    std::ofstream file(path, std::ios_base::out | std::ios_base::binary | std::ios_base::trunc);
    if (!file) {
      umd_log("shader bytecode dump open failed");
      return;
    }

    file.write(reinterpret_cast<const char*>(data), len);
  }

  // A shader blob that is either BORROWED from the caller's buffer or OWNED in
  // a vector this object built.
  //
  // Pre-R819 `data` meant two different things depending on which factory
  // produced the value -- a pointer into the caller's buffer with `owned`
  // empty, or a pointer aliasing `owned.data()`. The struct was COPYABLE, and
  // copying it in the second case left the copy's `data` pointing at the
  // ORIGINAL vector's heap buffer: a dangling pointer as soon as the original
  // died, handed to CreateVertexShader as the shader blob. It survived only
  // because every use is a single local initialised by `auto x = factory(...)`
  // and never copied or reassigned.
  //
  // Two changes make the invalid state unrepresentable rather than merely
  // unreached: the type is move-only, and the range is DERIVED from the owner
  // instead of stored beside it, so there is no `data` member that can alias a
  // vector which is not this object's. Moving is safe and stays allowed --
  // moving a std::vector transfers the heap buffer, so a derived pointer stays
  // valid; it is copying that reallocates.
  //
  // No std::span: this TU is C++17 (see umd/build.rs).
  class ShaderBytecode {
   public:
    ShaderBytecode() = default;
    ShaderBytecode(const ShaderBytecode&) = delete;
    ShaderBytecode& operator=(const ShaderBytecode&) = delete;
    // A user-declared copy constructor -- even a deleted one -- suppresses the
    // implicit move operations, so they are defaulted explicitly.
    ShaderBytecode(ShaderBytecode&&) = default;
    ShaderBytecode& operator=(ShaderBytecode&&) = default;

    /// Borrow the caller's buffer. Its lifetime must exceed this object's --
    /// the one precondition that stays a documented rule.
    void borrow(const std::uint8_t* p, std::size_t n) {
      borrowed_ = p;
      borrowed_len_ = n;
    }

    /// The owned buffer, built in place by the wrapping factories.
    std::vector<std::uint8_t>& owned() { return owned_; }

    const std::uint8_t* data() const {
      return owned_.empty() ? borrowed_ : owned_.data();
    }
    std::size_t len() const {
      return owned_.empty() ? borrowed_len_ : owned_.size();
    }

    explicit operator bool() const {
      return data() && len();
    }

   private:
    std::vector<std::uint8_t> owned_;
    const std::uint8_t* borrowed_ = nullptr;
    std::size_t borrowed_len_ = 0;
  };

  template<typename T>
  void write_le(std::vector<std::uint8_t>& dst, std::size_t offset, T value) {
    for (std::size_t i = 0; i < sizeof(T); i++)
      dst[offset + i] = std::uint8_t((value >> (8u * i)) & 0xffu);
  }

  std::uint64_t venus_memory_id_from_handle(VkDeviceMemory memory) {
   if (memory == VK_NULL_HANDLE)
      return 0;

    using Fn = std::uint64_t (__cdecl*)(VkDeviceMemory);
    if (auto fn = helios_icd_export<Fn>(HeliosIcdExport::MemoryId))
      return fn(memory);

    log_export_unavailable(HeliosIcdExport::MemoryId);
    return 0;
  }

  std::uint32_t venus_memory_resource_id_from_handle(VkDeviceMemory memory) {
    if (memory == VK_NULL_HANDLE)
      return 0;

    using Fn = std::uint32_t (__cdecl*)(VkDeviceMemory);
    if (auto fn = helios_icd_export<Fn>(HeliosIcdExport::MemoryResId))
      return fn(memory);

    return 0;
  }

  std::uint32_t venus_memory_transfer_resource_ownership(VkDeviceMemory memory) {
    if (memory == VK_NULL_HANDLE)
      return 0;

    using Fn = std::uint32_t (__cdecl*)(VkDeviceMemory);
    if (auto fn = helios_icd_export<Fn>(HeliosIcdExport::MemoryTransferOwnership))
      return fn(memory);

    log_export_unavailable(HeliosIcdExport::MemoryTransferOwnership);
    return 0;
  }

  bool venus_memory_alloc_info_from_handle(VkDeviceMemory memory,
                                           std::uint64_t* alloc_size,
                                           std::uint32_t* memory_type_index) {
    if (alloc_size)
      *alloc_size = 0;
    if (memory_type_index)
      *memory_type_index = 0;
    if (memory == VK_NULL_HANDLE)
      return false;

    using Fn = bool (__cdecl*)(VkDeviceMemory, std::uint64_t*, std::uint32_t*);
    if (auto fn = helios_icd_export<Fn>(HeliosIcdExport::MemoryAllocInfo))
      return fn(memory, alloc_size, memory_type_index);

    log_export_unavailable(HeliosIcdExport::MemoryAllocInfo);
    return false;
  }

  // One DDI signature entry flattened by the Rust side:
  // (SystemValue, Register, Mask, RegisterComponentType, Stream).
  constexpr std::size_t kSigEntryWords = 5;

  // D3D11 has at most 32 input/output registers per stage; a few hundred is
  // generous. The bound exists so the length check below cannot be satisfied by
  // a wrapped sum, since it is the only thing standing between caller-supplied
  // counts and the indexing that follows.
  constexpr std::uint32_t kMaxSignatureEntries = 512u;

  std::atomic<std::uint32_t> g_signatureCountRefused{0};

  bool signature_count_ok(const char* what, std::uint32_t count) {
    if (count <= kMaxSignatureEntries)
      return true;
    const std::uint32_t n =
      g_signatureCountRefused.fetch_add(1, std::memory_order_relaxed) + 1;
    char msg[160];
    std::snprintf(msg, sizeof(msg),
      "%s REFUSED: signature entry count %u exceeds %u (x%u)",
      what, count, kMaxSignatureEntries, n);
    umd_log(msg);
    return false;
  }

  struct EncodedSignatureEntry {
    const char* semantic_name;
    std::uint32_t semantic_index;
    std::uint32_t system_value;
  };

  bool is_patch_signature_chunk(const char tag[4]) {
    return std::memcmp(tag, "PCSG", 4) == 0 || std::memcmp(tag, "PSG1", 4) == 0;
  }

  EncodedSignatureEntry encode_signature_entry(const char tag[4],
                                               std::uint32_t sysval,
                                               std::uint32_t reg) {
    EncodedSignatureEntry encoded = { "TEXCOORD", reg, sysval };

    if (!is_patch_signature_chunk(tag))
      return encoded;

    // D3D11 DDI tessellation signatures carry D3D10_SB_NAME token values
    // (individual final edge/inside factors: 11..22). DXBC container
    // signatures carry the collapsed D3D_NAME reflection values used by
    // dxbc-spv (edge/inside semantic plus semantic index). Without this
    // translation, hull shader tess factors are not declared as SPIR-V
    // TessLevelOuter/Inner built-ins and tessellated draws can disappear.
    switch (sysval) {
      case 11: encoded = { "SV_TessFactor",       0u, 11u }; break; // quad U0 edge
      case 12: encoded = { "SV_TessFactor",       1u, 11u }; break; // quad V0 edge
      case 13: encoded = { "SV_TessFactor",       2u, 11u }; break; // quad U1 edge
      case 14: encoded = { "SV_TessFactor",       3u, 11u }; break; // quad V1 edge
      case 15: encoded = { "SV_InsideTessFactor", 0u, 12u }; break; // quad U inside
      case 16: encoded = { "SV_InsideTessFactor", 1u, 12u }; break; // quad V inside
      case 17: encoded = { "SV_TessFactor",       0u, 13u }; break; // tri U edge
      case 18: encoded = { "SV_TessFactor",       1u, 13u }; break; // tri V edge
      case 19: encoded = { "SV_TessFactor",       2u, 13u }; break; // tri W edge
      case 20: encoded = { "SV_InsideTessFactor", 0u, 14u }; break; // tri inside
      case 21: encoded = { "SV_TessFactor",       0u, 15u }; break; // line detail
      case 22: encoded = { "SV_InsideTessFactor", 0u, 16u }; break; // line density
      default: break;
    }

    return encoded;
  }

  // Append one 24-byte DXBC signature chunk (ISGN/OSGN) built from flattened
  // D3D11_1DDIARG_SIGNATURE_ENTRY2 values. Semantic names are synthesized as
  // "TEXCOORD<register>" — names are only a matching key (the input-layout
  // path fabricates the same convention); the load-bearing fields are the
  // register, mask, system value and, critically, the COMPONENT TYPE the raw
  // token stream cannot express (dwm binds R16G16_SINT vertex data against
  // shaders whose ISGN declares SINT inputs — typing them float32 was
  // VUID-Input-08733 UB that rasterized nothing).
  void append_signature_chunk(std::vector<std::uint8_t>& blob,
                              const char tag[4],
                              const std::uint32_t* entries,
                              std::uint32_t count) {
    // 24 bytes per entry, widened: `count` is caller-supplied and the result
    // feeds `name_base` and every offset written into the chunk.
    if (!signature_count_ok("append_signature_chunk", count))
      return;
    const std::uint32_t entries_size = count * 24u;
    const std::uint32_t name_base = 8u + entries_size;  // relative to chunk data
    std::vector<EncodedSignatureEntry> encoded_entries;
    std::vector<std::uint32_t> name_offsets;
    encoded_entries.reserve(count);
    name_offsets.reserve(count);

    std::uint32_t names_size = 0u;
    for (std::uint32_t i = 0; i < count; ++i) {
      const std::uint32_t* e = entries + std::size_t(i) * kSigEntryWords;
      auto encoded = encode_signature_entry(tag, e[0], e[1]);
      encoded_entries.push_back(encoded);
      name_offsets.push_back(name_base + names_size);
      names_size += std::uint32_t(std::strlen(encoded.semantic_name) + 1u);
    }

    std::uint32_t data_len = name_base + names_size;
    data_len = (data_len + 3u) & ~3u;

    auto put32 = [&blob](std::uint32_t v) {
      blob.push_back(std::uint8_t(v));
      blob.push_back(std::uint8_t(v >> 8));
      blob.push_back(std::uint8_t(v >> 16));
      blob.push_back(std::uint8_t(v >> 24));
    };

    blob.insert(blob.end(), tag, tag + 4);
    put32(data_len);
    const std::size_t data_start = blob.size();
    put32(count);
    put32(8u);
    for (std::uint32_t i = 0; i < count; ++i) {
      const std::uint32_t* e = entries + std::size_t(i) * kSigEntryWords;
      const std::uint32_t sysval = e[0];
      const std::uint32_t reg = e[1];
      const std::uint32_t mask = e[2] & 0xFu;
      const auto encoded = encoded_entries.at(i);
      // UNKNOWN(0) component type: default to float32 (matches the previous
      // behaviour and the D3D convention for untyped registers).
      const std::uint32_t comptype = e[3] ? e[3] : 3u;
      if (e[4]) {
        char msg[96];
        std::snprintf(msg, sizeof(msg),
                      "shader signature entry reg=%u has stream=%u (unencoded)", reg, e[4]);
        umd_log(msg);
      }
      put32(name_offsets.at(i));
      put32(encoded.semantic_index);
      put32(encoded.system_value);
      put32(comptype);
      put32(reg);
      put32(mask | (mask << 8));  // mask | read/write mask, 2 pad bytes
      if (encoded.system_value != sysval || encoded.semantic_index != reg) {
        static std::atomic<std::uint32_t> s_tess_sig_remap_logs { 0u };
        const auto n = s_tess_sig_remap_logs.fetch_add(1u, std::memory_order_relaxed);
        if (n < 64u) {
          char msg[160];
          std::snprintf(msg, sizeof(msg),
            "shader patch signature remap: raw_sv=%u reg=%u -> sig_sv=%u sem=%s%u",
            sysval, reg, encoded.system_value, encoded.semantic_name, encoded.semantic_index);
          umd_log(msg);
        }
      }
    }
    for (const auto& e : encoded_entries) {
      const auto len = std::strlen(e.semantic_name) + 1u;
      blob.insert(blob.end(), e.semantic_name, e.semantic_name + len);
    }
    while ((blob.size() - data_start) < data_len)
      blob.push_back(0);
  }

  // Wrap a raw SM4/SM5 token stream in a DXBC container carrying REAL input/
  // output signature chunks from the >=11.1 DDI. Layout mirrors
  // prepare_shader_bytecode's code-only wrap, with two extra chunks.
  ShaderBytecode prepare_shader_bytecode_with_sigs(
      const std::uint8_t* code, std::size_t len,
      const std::uint32_t* in_entries, std::uint32_t n_in,
      const std::uint32_t* out_entries, std::uint32_t n_out) {
    ShaderBytecode result = { };
    if (!code || !len || len < 8 || (len & 3u))
      return result;

    const auto* dwords = reinterpret_cast<const std::uint32_t*>(code);
    const std::uint32_t major = (dwords[0] >> 4u) & 0xfu;
    if (std::size_t(dwords[1]) * sizeof(std::uint32_t) != len) {
      umd_log("raw shader bytecode dword count mismatch (sig wrap)");
      return result;
    }
    const char* code_tag = major >= 5u ? "SHEX" : "SHDR";

    // Build the three chunks into a scratch buffer first.
    std::vector<std::uint8_t> chunks;
    std::array<std::uint32_t, 3> chunk_offsets = { };
    std::uint32_t chunk_count = 0;

    chunk_offsets.at(chunk_count++) = std::uint32_t(chunks.size());
    append_signature_chunk(chunks, "ISGN", in_entries, n_in);
    chunk_offsets.at(chunk_count++) = std::uint32_t(chunks.size());
    append_signature_chunk(chunks, "OSGN", out_entries, n_out);
    chunk_offsets.at(chunk_count++) = std::uint32_t(chunks.size());
    chunks.insert(chunks.end(), code_tag, code_tag + 4);
    {
      std::uint32_t v = std::uint32_t(len);
      chunks.push_back(std::uint8_t(v));
      chunks.push_back(std::uint8_t(v >> 8));
      chunks.push_back(std::uint8_t(v >> 16));
      chunks.push_back(std::uint8_t(v >> 24));
    }
    chunks.insert(chunks.end(), code, code + len);

    const std::uint32_t file_header_size = 32u;
    const std::uint32_t offset_table_size = chunk_count * sizeof(std::uint32_t);
    const std::uint32_t chunk_base = file_header_size + offset_table_size;
    const std::uint32_t file_size = chunk_base + std::uint32_t(chunks.size());

    result.owned().resize(file_size);
    std::memcpy(&result.owned()[0], "DXBC", 4);
    write_le(result.owned(), 20u, std::uint32_t(1u));
    write_le(result.owned(), 24u, file_size);
    write_le(result.owned(), 28u, chunk_count);
    for (std::uint32_t i = 0; i < chunk_count; ++i)
      write_le(result.owned(), 32u + 4u * i, chunk_base + chunk_offsets.at(i));
    std::memcpy(&result.owned()[chunk_base], chunks.data(), chunks.size());

    auto digest = dxbc_spv::dxbc::hashDxbcBinary(result.owned().data(), result.owned().size());
    std::memcpy(&result.owned()[4], digest.data.data(), digest.data.size());

    // No `result.data = result.owned().data()` any more: the range is derived.
    return result;
  }

  ShaderBytecode prepare_shader_bytecode_with_tess_sigs(
      const std::uint8_t* code, std::size_t len,
      const std::uint32_t* in_entries, std::uint32_t n_in,
      const std::uint32_t* out_entries, std::uint32_t n_out,
      const std::uint32_t* patch_entries, std::uint32_t n_patch) {
    ShaderBytecode result = { };
    if (!code || !len || len < 8 || (len & 3u))
      return result;

    const auto* dwords = reinterpret_cast<const std::uint32_t*>(code);
    const std::uint32_t major = (dwords[0] >> 4u) & 0xfu;
    if (std::size_t(dwords[1]) * sizeof(std::uint32_t) != len) {
      umd_log("raw shader bytecode dword count mismatch (tess sig wrap)");
      return result;
    }
    const char* code_tag = major >= 5u ? "SHEX" : "SHDR";

    std::vector<std::uint8_t> chunks;
    std::array<std::uint32_t, 4> chunk_offsets = { };
    std::uint32_t chunk_count = 0;

    chunk_offsets.at(chunk_count++) = std::uint32_t(chunks.size());
    append_signature_chunk(chunks, "ISGN", in_entries, n_in);
    chunk_offsets.at(chunk_count++) = std::uint32_t(chunks.size());
    append_signature_chunk(chunks, "OSGN", out_entries, n_out);
    chunk_offsets.at(chunk_count++) = std::uint32_t(chunks.size());
    append_signature_chunk(chunks, "PCSG", patch_entries, n_patch);
    chunk_offsets.at(chunk_count++) = std::uint32_t(chunks.size());
    chunks.insert(chunks.end(), code_tag, code_tag + 4);
    {
      std::uint32_t v = std::uint32_t(len);
      chunks.push_back(std::uint8_t(v));
      chunks.push_back(std::uint8_t(v >> 8));
      chunks.push_back(std::uint8_t(v >> 16));
      chunks.push_back(std::uint8_t(v >> 24));
    }
    chunks.insert(chunks.end(), code, code + len);

    const std::uint32_t file_header_size = 32u;
    const std::uint32_t offset_table_size = chunk_count * sizeof(std::uint32_t);
    const std::uint32_t chunk_base = file_header_size + offset_table_size;
    const std::uint32_t file_size = chunk_base + std::uint32_t(chunks.size());

    result.owned().resize(file_size);
    std::memcpy(&result.owned()[0], "DXBC", 4);
    write_le(result.owned(), 20u, std::uint32_t(1u));
    write_le(result.owned(), 24u, file_size);
    write_le(result.owned(), 28u, chunk_count);
    for (std::uint32_t i = 0; i < chunk_count; ++i)
      write_le(result.owned(), 32u + 4u * i, chunk_base + chunk_offsets.at(i));
    std::memcpy(&result.owned()[chunk_base], chunks.data(), chunks.size());

    auto digest = dxbc_spv::dxbc::hashDxbcBinary(result.owned().data(), result.owned().size());
    std::memcpy(&result.owned()[4], digest.data.data(), digest.data.size());

    // No `result.data = result.owned().data()` any more: the range is derived.
    return result;
  }

  ShaderBytecode prepare_shader_bytecode(const std::uint8_t* code, std::size_t len) {
    ShaderBytecode result = { };

    if (!code || !len)
      return result;

    if (len >= 4 && std::memcmp(code, "DXBC", 4) == 0) {
      result.borrow(code, len);
      return result;
    }

    if (len < 8 || (len & 3u)) {
      umd_log("raw shader bytecode has invalid size");
      return result;
    }

    const auto* dwords = reinterpret_cast<const std::uint32_t*>(code);
    const std::uint32_t version_token = dwords[0];
    const std::uint32_t dword_count = dwords[1];
    const std::uint32_t major = (version_token >> 4u) & 0xfu;

    if (dword_count < 2u || std::size_t(dword_count) * sizeof(std::uint32_t) != len) {
      umd_log("raw shader bytecode dword count mismatch");
      return result;
    }

    const char* chunk_tag = major >= 5u ? "SHEX" : "SHDR";
    constexpr std::uint32_t file_header_size = 32u;
    constexpr std::uint32_t chunk_count = 1u;
    constexpr std::uint32_t chunk_offset_table_size = chunk_count * sizeof(std::uint32_t);
    constexpr std::uint32_t chunk_offset = file_header_size + chunk_offset_table_size;
    constexpr std::uint32_t chunk_header_size = 8u;
    const std::uint32_t file_size = chunk_offset + chunk_header_size + std::uint32_t(len);

    result.owned().resize(file_size);
    std::memcpy(&result.owned()[0], "DXBC", 4);
    write_le(result.owned(), 20u, std::uint32_t(1u));
    write_le(result.owned(), 24u, file_size);
    write_le(result.owned(), 28u, chunk_count);
    write_le(result.owned(), 32u, chunk_offset);
    std::memcpy(&result.owned()[chunk_offset], chunk_tag, 4);
    write_le(result.owned(), chunk_offset + 4u, std::uint32_t(len));
    std::memcpy(&result.owned()[chunk_offset + chunk_header_size], code, len);

    auto digest = dxbc_spv::dxbc::hashDxbcBinary(result.owned().data(), result.owned().size());
    std::memcpy(&result.owned()[4], digest.data.data(), digest.data.size());

    // No `result.data = result.owned().data()` any more: the range is derived.
    return result;
  }

}

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

  ~HeliosDxvkDeviceImpl() {
    if (context) context->Release();
    if (d3d11) d3d11->Release();
  }
};

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

  try {
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
  } catch (const dxvk::DxvkError& e) {
    umd_log(("OpenDdiTexture2D DxvkError: " + e.message()).c_str());
  } catch (const std::exception& e) {
    umd_log(e.what());
  } catch (...) {
    umd_log("unknown exception in OpenDdiTexture2D");
  }

  return 0;
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

  try {
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
  } catch (const dxvk::DxvkError& e) {
    umd_log(("CreateDdiScanoutTexture2D DxvkError: " + e.message()).c_str());
  } catch (const std::exception& e) {
    umd_log(e.what());
  } catch (...) {
    umd_log("unknown exception in CreateDdiScanoutTexture2D");
  }

  return 0;
}

std::size_t HeliosDxvkDevice::create_vertex_shader(const std::uint8_t* code, std::size_t len) const {
  if (!impl || !impl->d3d11 || !code || !len)
    return 0;
  ID3D11VertexShader* shader = nullptr;
  try {
    auto bytecode = prepare_shader_bytecode(code, len);
    if (!bytecode)
      return 0;
    dump_shader_bytecode("vs", "raw", code, len);
    dump_shader_bytecode("vs", "wrapped", bytecode.data(), bytecode.len());
    HRESULT hr = impl->d3d11->CreateVertexShader(bytecode.data(), bytecode.len(), nullptr, &shader);
    if (FAILED(hr)) {
      umd_log("CreateVertexShader returned failure");
      return 0;
    }
    return reinterpret_cast<std::size_t>(shader);
  } catch (const dxvk::DxvkError& e) {
    umd_log(("CreateVertexShader DxvkError: " + e.message()).c_str());
  } catch (const std::exception& e) {
    umd_log(e.what());
  } catch (...) {
    umd_log("unknown exception in CreateVertexShader");
  }
  return 0;
}

std::size_t HeliosDxvkDevice::create_pixel_shader(const std::uint8_t* code, std::size_t len) const {
  if (!impl || !impl->d3d11 || !code || !len)
    return 0;
  ID3D11PixelShader* shader = nullptr;
  try {
    auto bytecode = prepare_shader_bytecode(code, len);
    if (!bytecode)
      return 0;
    dump_shader_bytecode("ps", "raw", code, len);
    dump_shader_bytecode("ps", "wrapped", bytecode.data(), bytecode.len());
    HRESULT hr = impl->d3d11->CreatePixelShader(bytecode.data(), bytecode.len(), nullptr, &shader);
    if (FAILED(hr)) {
      umd_log("CreatePixelShader returned failure");
      return 0;
    }
    return reinterpret_cast<std::size_t>(shader);
  } catch (const dxvk::DxvkError& e) {
    umd_log(("CreatePixelShader DxvkError: " + e.message()).c_str());
  } catch (const std::exception& e) {
    umd_log(e.what());
  } catch (...) {
    umd_log("unknown exception in CreatePixelShader");
  }
  return 0;
}

std::size_t HeliosDxvkDevice::create_geometry_shader(const std::uint8_t* code, std::size_t len) const {
  if (!impl || !impl->d3d11 || !code || !len)
    return 0;
  ID3D11GeometryShader* shader = nullptr;
  try {
    auto bytecode = prepare_shader_bytecode(code, len);
    if (!bytecode)
      return 0;
    dump_shader_bytecode("gs", "raw", code, len);
    dump_shader_bytecode("gs", "wrapped", bytecode.data(), bytecode.len());
    HRESULT hr = impl->d3d11->CreateGeometryShader(bytecode.data(), bytecode.len(), nullptr, &shader);
    if (FAILED(hr)) {
      umd_log("CreateGeometryShader returned failure");
      return 0;
    }
    return reinterpret_cast<std::size_t>(shader);
  } catch (const dxvk::DxvkError& e) {
    umd_log(("CreateGeometryShader DxvkError: " + e.message()).c_str());
  } catch (const std::exception& e) {
    umd_log(e.what());
  } catch (...) {
    umd_log("unknown exception in CreateGeometryShader");
  }
  return 0;
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
  try {
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
  } catch (const dxvk::DxvkError& e) {
    umd_log(("create_shader_sig DxvkError: " + e.message()).c_str());
  } catch (const std::exception& e) {
    umd_log(e.what());
  } catch (...) {
    umd_log("unknown exception in create_shader_sig");
  }
  return 0;
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
  try {
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
  } catch (const dxvk::DxvkError& e) {
    umd_log(("create_tess_shader_sig DxvkError: " + e.message()).c_str());
  } catch (const std::exception& e) {
    umd_log(e.what());
  } catch (...) {
    umd_log("unknown exception in create_tess_shader_sig");
  }
  return 0;
}

bool HeliosDxvkDevice::rotate_resource_backings(
    const std::size_t* d3d11_resource_ptrs,
    std::size_t count) const {
  if (!impl || !impl->d3d11 || !impl->context || !d3d11_resource_ptrs || count < 2)
    return false;
  try {
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
      const std::uint64_t us = std::uint64_t(qpcT1.QuadPart - qpcT0.QuadPart)
        * 1000000ull / std::uint64_t(qpcFreq.QuadPart);
      static std::atomic<std::uint64_t> s_drainTotalUs{0};
      static std::atomic<std::uint64_t> s_drainMaxUs{0};
      static std::atomic<std::uint32_t> s_drainCount{0};
      s_drainTotalUs.fetch_add(us, std::memory_order_relaxed);
      std::uint64_t prevMax = s_drainMaxUs.load(std::memory_order_relaxed);
      while (us > prevMax &&
             !s_drainMaxUs.compare_exchange_weak(prevMax, us, std::memory_order_relaxed)) {}
      const std::uint32_t n = s_drainCount.fetch_add(1, std::memory_order_relaxed) + 1;
      if ((n & 31u) == 0) {
        char msg[128];
        std::snprintf(msg, sizeof(msg),
                      "rotate-perf: n=%u drain_avg_us=%llu drain_max_us=%llu",
                      n,
                      static_cast<unsigned long long>(
                          s_drainTotalUs.load(std::memory_order_relaxed) / n),
                      static_cast<unsigned long long>(
                          s_drainMaxUs.load(std::memory_order_relaxed)));
        umd_log(msg);
        s_drainMaxUs.store(0, std::memory_order_relaxed);
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
  } catch (const dxvk::DxvkError& e) {
    umd_log(("rotate_resource_backings DxvkError: " + e.message()).c_str());
  } catch (const std::exception& e) {
    umd_log(e.what());
  } catch (...) {
    umd_log("unknown exception in rotate_resource_backings");
  }
  return false;
}

// R826 counters. The gate is the stage's own measurement instrument and it
// could not see one of its own failure modes: the three catch arms below
// returned false without touching s_gateTimeouts, so a thrown exception was
// indistinguishable from a timeout to every consumer -- forward.rs maps false
// to return code 1 = timeout and increments EXT_FLIP_GATE_TIMEOUTS.
static std::atomic<std::uint32_t> s_gateExceptions{0};
// The no-context arm. Counted at zero cost, but NOT a live failure mode:
// GetImmediateContext runs before the device is handed to Rust, so this is
// unreachable rather than rare. Recorded so the distinction is in the code.
static std::atomic<std::uint32_t> s_gateNoContext{0};

bool HeliosDxvkDevice::present_frame_gate(std::uint32_t timeout_us) const {
  if (!impl || !impl->context) {
    s_gateNoContext.fetch_add(1, std::memory_order_relaxed);
    return false;
  }
  try {
    LARGE_INTEGER qpcFreq, qpcT0, qpcT1;
    QueryPerformanceFrequency(&qpcFreq);
    QueryPerformanceCounter(&qpcT0);

    auto* immediateContext = static_cast<dxvk::D3D11ImmediateContext*>(impl->context);
    const bool completed = immediateContext->HeliosWaitFrameComplete(timeout_us);

    // Gate-cost telemetry (PSC WS2 discipline): one line per 128 presents.
    QueryPerformanceCounter(&qpcT1);
    const std::uint64_t us = std::uint64_t(qpcT1.QuadPart - qpcT0.QuadPart)
      * 1000000ull / std::uint64_t(qpcFreq.QuadPart);
    static std::atomic<std::uint64_t> s_gateTotalUs{0};
    static std::atomic<std::uint64_t> s_gateMaxUs{0};
    static std::atomic<std::uint32_t> s_gateCount{0};
    static std::atomic<std::uint32_t> s_gateTimeouts{0};
    s_gateTotalUs.fetch_add(us, std::memory_order_relaxed);
    if (!completed)
      s_gateTimeouts.fetch_add(1, std::memory_order_relaxed);
    std::uint64_t prevMax = s_gateMaxUs.load(std::memory_order_relaxed);
    while (us > prevMax &&
           !s_gateMaxUs.compare_exchange_weak(prevMax, us, std::memory_order_relaxed)) {}
    const std::uint32_t n = s_gateCount.fetch_add(1, std::memory_order_relaxed) + 1;
    if ((n & 127u) == 0) {
      // The existing keys keep their names, order and format so before/after
      // runs compare directly; `failed` and `noctx` are APPENDED. R826.
      char msg[200];
      std::snprintf(msg, sizeof(msg),
                    "present-gate: n=%u avg_us=%llu max_us=%llu timeouts=%u "
                    "failed=%u noctx=%u",
                    n,
                    static_cast<unsigned long long>(
                        s_gateTotalUs.load(std::memory_order_relaxed) / n),
                    static_cast<unsigned long long>(
                        s_gateMaxUs.load(std::memory_order_relaxed)),
                    s_gateTimeouts.load(std::memory_order_relaxed),
                    s_gateExceptions.load(std::memory_order_relaxed),
                    s_gateNoContext.load(std::memory_order_relaxed));
      umd_log(msg);
      s_gateMaxUs.store(0, std::memory_order_relaxed);
    }
    return completed;
  } catch (const dxvk::DxvkError& e) {
    umd_log(("present_frame_gate DxvkError: " + e.message()).c_str());
  } catch (const std::exception& e) {
    umd_log(e.what());
  } catch (...) {
    umd_log("unknown exception in present_frame_gate");
  }
  // R826: the exception arms are a DIFFERENT outcome from a timeout and are now
  // counted as one. The `bool` return and the bounded timeout are KEPT -- this
  // is a real event wait with a safety bound, which the frozen baseline keeps.
  // A later, separate commit may return an
  // `enum class GateOutcome { Completed, TimedOut, Failed }` so the Rust caller
  // must handle Failed explicitly instead of reporting it as a timeout.
  s_gateExceptions.fetch_add(1, std::memory_order_relaxed);
  return false;
}

std::int32_t HeliosDxvkDevice::present_vehicle_copy(
    std::size_t dst_resource_ptr,
    std::size_t src_resource_ptr) const {
  if (!impl || !impl->context || !dst_resource_ptr || !src_resource_ptr)
    return -1;

  try {
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
  } catch (const dxvk::DxvkError& e) {
    umd_log(("present_vehicle_copy DxvkError: " + e.message()).c_str());
  } catch (const std::exception& e) {
    umd_log(e.what());
  } catch (...) {
    umd_log("unknown exception in present_vehicle_copy");
  }
  return -1;
}

std::size_t HeliosDxvkDevice::create_hull_shader(const std::uint8_t* code, std::size_t len) const {
  if (!impl || !impl->d3d11 || !code || !len)
    return 0;
  ID3D11HullShader* shader = nullptr;
  try {
    auto bytecode = prepare_shader_bytecode(code, len);
    if (!bytecode)
      return 0;
    dump_shader_bytecode("hs", "raw", code, len);
    dump_shader_bytecode("hs", "wrapped", bytecode.data(), bytecode.len());
    HRESULT hr = impl->d3d11->CreateHullShader(bytecode.data(), bytecode.len(), nullptr, &shader);
    if (FAILED(hr)) {
      umd_log("CreateHullShader returned failure");
      return 0;
    }
    return reinterpret_cast<std::size_t>(shader);
  } catch (const dxvk::DxvkError& e) {
    umd_log(("CreateHullShader DxvkError: " + e.message()).c_str());
  } catch (const std::exception& e) {
    umd_log(e.what());
  } catch (...) {
    umd_log("unknown exception in CreateHullShader");
  }
  return 0;
}

std::size_t HeliosDxvkDevice::create_domain_shader(const std::uint8_t* code, std::size_t len) const {
  if (!impl || !impl->d3d11 || !code || !len)
    return 0;
  ID3D11DomainShader* shader = nullptr;
  try {
    auto bytecode = prepare_shader_bytecode(code, len);
    if (!bytecode)
      return 0;
    dump_shader_bytecode("ds", "raw", code, len);
    dump_shader_bytecode("ds", "wrapped", bytecode.data(), bytecode.len());
    HRESULT hr = impl->d3d11->CreateDomainShader(bytecode.data(), bytecode.len(), nullptr, &shader);
    if (FAILED(hr)) {
      umd_log("CreateDomainShader returned failure");
      return 0;
    }
    return reinterpret_cast<std::size_t>(shader);
  } catch (const dxvk::DxvkError& e) {
    umd_log(("CreateDomainShader DxvkError: " + e.message()).c_str());
  } catch (const std::exception& e) {
    umd_log(e.what());
  } catch (...) {
    umd_log("unknown exception in CreateDomainShader");
  }
  return 0;
}

std::size_t HeliosDxvkDevice::create_compute_shader(const std::uint8_t* code, std::size_t len) const {
  if (!impl || !impl->d3d11 || !code || !len)
    return 0;
  ID3D11ComputeShader* shader = nullptr;
  try {
    auto bytecode = prepare_shader_bytecode(code, len);
    if (!bytecode)
      return 0;
    dump_shader_bytecode("cs", "raw", code, len);
    dump_shader_bytecode("cs", "wrapped", bytecode.data(), bytecode.len());
    HRESULT hr = impl->d3d11->CreateComputeShader(bytecode.data(), bytecode.len(), nullptr, &shader);
    if (FAILED(hr)) {
      umd_log("CreateComputeShader returned failure");
      return 0;
    }
    return reinterpret_cast<std::size_t>(shader);
  } catch (const dxvk::DxvkError& e) {
    umd_log(("CreateComputeShader DxvkError: " + e.message()).c_str());
  } catch (const std::exception& e) {
    umd_log(e.what());
  } catch (...) {
    umd_log("unknown exception in CreateComputeShader");
  }
  return 0;
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

  try {
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
  } catch (const dxvk::DxvkError& e) {
    umd_log(("DxvkError: " + e.message()).c_str());
    return nullptr;
  } catch (const std::exception& e) {
    umd_log(e.what());
    return nullptr;
  } catch (...) {
    umd_log("unknown C++ exception in helios_dxvk_create_device");
    return nullptr;
  }
}
