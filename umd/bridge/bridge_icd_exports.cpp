// Finding the Helios venus ICD, and reading through its exports.
//
// Moved verbatim out of `dxvk_bridge.cpp` by T8/R1105: the Win32 module walk,
// the Vulkan ICD manifest discovery (registry + environment), the hand-rolled
// JSON `library_path` parser, the resolve-once export table, and the six
// readers `bridge_icd_exports.h` publishes.
//
// Everything else here is PRIVATE to this TU -- `find_export_in_loaded_modules`,
// `discover_vulkan_icd_manifests`, `parse_icd_library_path`,
// `helios_icd_export<Fn>`, `log_export_unavailable` -- which is the
// reachability reduction the split buys: nothing outside this file can reach
// the loader.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <tlhelp32.h>

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <sstream>
#include <string>
#include <vector>

// The DXVK Vulkan loader shim must be the first Vulkan header this TU sees --
// see the include-order note in `bridge_icd_exports.h`.
#include "dxvk_instance.h"

#include "bridge_common.h"
#include "bridge_icd_exports.h"

namespace helios_bridge {

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
    RegisterPresentStream,
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
    case HeliosIcdExport::RegisterPresentStream:
      return "helios_venus_register_present_stream";
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

  bool venus_register_present_stream(VkDevice device,
                                     VkSemaphore semaphore,
                                     std::uint64_t* out_cookie) {
    if (out_cookie)
      *out_cookie = 0;
    if (device == VK_NULL_HANDLE || semaphore == VK_NULL_HANDLE || !out_cookie)
      return false;

    using Fn = bool (__cdecl*)(VkDevice, VkSemaphore, std::uint64_t*);
    if (auto fn = helios_icd_export<Fn>(HeliosIcdExport::RegisterPresentStream))
      return fn(device, semaphore, out_cookie) && *out_cookie != 0;

    log_export_unavailable(HeliosIcdExport::RegisterPresentStream);
    return false;
  }

}  // namespace helios_bridge
