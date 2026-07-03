// Helios UMD <-> DXVK engine bridge implementation.
//
// Wraps DXVK's DxvkInstance/DxvkAdapter/DxvkDevice behind the opaque
// HeliosDxvkDevice. The DXVK engine references a frontend-provided
// `Logger::s_instance` global (normally defined in src/d3d11/d3d11_main.cpp,
// which we do not build) — we provide it here.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

#include <cstdio>
#include <cstdlib>
#include <exception>
#include <tlhelp32.h>

#include "dxvk_bridge.h"

#include <atomic>
#include <cstring>
#include <d3d11.h>
#include <dxgi.h>
#include <fstream>
#include <sstream>
#include <string>
#include <vector>

#include "dxvk_instance.h"
#include "dxvk_adapter.h"
#include "dxvk_device.h"
#include "../src/util/util_error.h"
#include "dxbc/dxbc_container.h"

// DXVK's full D3D11 COM implementation (built as libhelios_d3d11_static.a). We
// instantiate D3D11DXGIDevice from our DxvkDevice and forward the d3d10umddi DDI
// to ID3D11Device / ID3D11DeviceContext.
#include "d3d11_device.h"
#include "d3d11_texture.h"

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
      if (!fn)
        continue;

      char msg[512];
      std::snprintf(msg, sizeof(msg), "resolved %s via ICD manifest %s -> %s",
        export_name, manifest.c_str(), dll.c_str());
      umd_log(msg);
      return fn;
    }
    return nullptr;
  }

  template<typename Fn>
  Fn find_helios_icd_export(const char* export_name) {
    if (auto fn = find_export_in_loaded_modules<Fn>(export_name))
      return fn;
    return find_export_via_vulkan_icd_manifests<Fn>(export_name);
  }

  std::uint32_t read_current_venus_context_id() {
    using Fn = std::uint32_t (__cdecl*)();
    constexpr const char* export_name = "helios_venus_current_ctx_id";

    auto fn = find_helios_icd_export<Fn>(export_name);
    if (!fn)
      return 0;

    const auto ctx = fn();
    if (ctx) {
      char msg[128];
      std::snprintf(msg, sizeof(msg), "Venus context export returned ctx_id=%u", ctx);
      umd_log(msg);
    }

    return ctx;
  }

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
  const char* umd_log_file() {
    static char path[MAX_PATH] = {0};
    if (path[0] == 0) {
      CreateDirectoryA("C:\\ProgramData\\Helios", nullptr);
      _snprintf_s(path, sizeof(path), _TRUNCATE,
                  "C:\\ProgramData\\Helios\\umd-%lu.log",
                  (unsigned long)GetCurrentProcessId());
    }
    return path;
  }

  void umd_log(const char* msg) {
    FILE* f = nullptr;
    if (fopen_s(&f, umd_log_file(), "a") == 0 && f) {
      fprintf(f, "[dxvk-bridge] %s\n", msg);
      fclose(f);
    }
  }

  struct ShaderBytecode {
    std::vector<std::uint8_t> owned;
    const std::uint8_t* data = nullptr;
    std::size_t len = 0;

    explicit operator bool() const {
      return data && len;
    }
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
    constexpr const char* export_name = "helios_venus_memory_id";

    if (auto fn = find_helios_icd_export<Fn>(export_name))
      return fn(memory);

    umd_log("helios_venus_memory_id export unavailable");
    return 0;
  }

  std::uint32_t venus_memory_resource_id_from_handle(VkDeviceMemory memory) {
    if (memory == VK_NULL_HANDLE)
      return 0;

    using Fn = std::uint32_t (__cdecl*)(VkDeviceMemory);
    constexpr const char* export_name = "helios_venus_memory_res_id";

    if (auto fn = find_helios_icd_export<Fn>(export_name))
      return fn(memory);

    return 0;
  }

  std::uint32_t venus_memory_transfer_resource_ownership(VkDeviceMemory memory) {
    if (memory == VK_NULL_HANDLE)
      return 0;

    using Fn = std::uint32_t (__cdecl*)(VkDeviceMemory);
    constexpr const char* export_name = "helios_venus_memory_transfer_resource_ownership";

    if (auto fn = find_helios_icd_export<Fn>(export_name))
      return fn(memory);

    umd_log("helios_venus_memory_transfer_resource_ownership export unavailable");
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
    constexpr const char* export_name = "helios_venus_memory_alloc_info";

    if (auto fn = find_helios_icd_export<Fn>(export_name))
      return fn(memory, alloc_size, memory_type_index);

    umd_log("helios_venus_memory_alloc_info export unavailable");
    return false;
  }

  ShaderBytecode prepare_shader_bytecode(const std::uint8_t* code, std::size_t len) {
    ShaderBytecode result = { };

    if (!code || !len)
      return result;

    if (len >= 4 && std::memcmp(code, "DXBC", 4) == 0) {
      result.data = code;
      result.len = len;
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

    result.owned.resize(file_size);
    std::memcpy(&result.owned[0], "DXBC", 4);
    write_le(result.owned, 20u, std::uint32_t(1u));
    write_le(result.owned, 24u, file_size);
    write_le(result.owned, 28u, chunk_count);
    write_le(result.owned, 32u, chunk_offset);
    std::memcpy(&result.owned[chunk_offset], chunk_tag, 4);
    write_le(result.owned, chunk_offset + 4u, std::uint32_t(len));
    std::memcpy(&result.owned[chunk_offset + chunk_header_size], code, len);

    auto digest = dxbc_spv::dxbc::hashDxbcBinary(result.owned.data(), result.owned.size());
    std::memcpy(&result.owned[4], digest.data.data(), digest.data.size());

    result.data = result.owned.data();
    result.len = result.owned.size();
    return result;
  }

}

// Opaque to the public header / cxx glue; owns the DXVK Rc<> objects + the DXVK
// D3D11 COM device the DDI forwards to.
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
    std::uint32_t global) const {
  if (!d3d11_resource_ptr || !local)
    return false;

  auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr);
  auto* texture = dxvk::GetCommonTexture(resource);
  if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
    return false;

  texture->GetImage()->storage()->setKmtHandles(local, global);

  char msg[160];
  std::snprintf(msg, sizeof(msg),
    "set_resource_kmt_handles resource=%p local=0x%08x global=0x%08x",
    resource, local, global);
  umd_log(msg);
  return true;
}

bool HeliosDxvkDevice::get_resource_memory_info(
    std::size_t d3d11_resource_ptr,
    std::uint64_t* memory,
    std::uint64_t* size,
    std::uint64_t* offset,
    std::uint32_t* resource_id) const {
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
  return venusId != 0 && info.size != 0;
}

bool HeliosDxvkDevice::get_resource_alloc_identity(
    std::size_t d3d11_resource_ptr,
    std::uint64_t* venus_alloc_size,
    std::uint32_t* memory_type_index) const {
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
}

bool HeliosDxvkDevice::transfer_resource_ownership(
    std::size_t d3d11_resource_ptr) const {
  if (!d3d11_resource_ptr)
    return false;

  auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr);
  auto* texture = dxvk::GetCommonTexture(resource);
  if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
    return false;

  auto info = texture->GetImage()->storage()->getMemoryInfo();
  const auto resourceId = venus_memory_transfer_resource_ownership(info.memory);

  char msg[192];
  std::snprintf(msg, sizeof(msg),
    "transfer_resource_ownership resource=%p memory=0x%llx res_id=%u",
    resource,
    static_cast<unsigned long long>(reinterpret_cast<std::uintptr_t>(info.memory)),
    resourceId);
  umd_log(msg);
  return resourceId != 0;
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
    std::uint32_t memory_type_index) const {
  if (!impl || !impl->d3d11 || !global || !renderer_resource_id || !width || !height)
    return 0;

  try {
    {
      char msg[256];
      std::snprintf(msg, sizeof(msg),
        "OpenDdiTexture2D begin %ux%u fmt=%u bind=0x%08x misc=0x%08x global=0x%08x renderer_res=%u alloc_size=%llu mem_type=%u",
        width, height, format, bind_flags, misc_flags, global, renderer_resource_id,
        static_cast<unsigned long long>(venus_alloc_size), memory_type_index);
      umd_log(msg);
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

    auto* device = reinterpret_cast<dxvk::D3D11Device*>(impl->d3d11);
    auto* texture = new dxvk::D3D11Texture2D(
        device, &desc, nullptr,
        reinterpret_cast<HANDLE>(static_cast<std::uintptr_t>(renderer_resource_id)),
        &importInfo);

    ID3D11Resource* resource = nullptr;
    HRESULT hr = texture->QueryInterface(
        __uuidof(ID3D11Resource),
        reinterpret_cast<void**>(&resource));

    char msg[224];
    std::snprintf(msg, sizeof(msg),
      "OpenDdiTexture2D %ux%u fmt=%u bind=0x%08x misc=0x%08x global=0x%08x renderer_res=%u hr=0x%08lx resource=%p",
      width, height, format, bind_flags, misc_flags, global, renderer_resource_id,
      static_cast<unsigned long>(hr), resource);
    umd_log(msg);

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

std::size_t HeliosDxvkDevice::create_vertex_shader(const std::uint8_t* code, std::size_t len) const {
  if (!impl || !impl->d3d11 || !code || !len)
    return 0;
  ID3D11VertexShader* shader = nullptr;
  try {
    auto bytecode = prepare_shader_bytecode(code, len);
    if (!bytecode)
      return 0;
    HRESULT hr = impl->d3d11->CreateVertexShader(bytecode.data, bytecode.len, nullptr, &shader);
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
    HRESULT hr = impl->d3d11->CreatePixelShader(bytecode.data, bytecode.len, nullptr, &shader);
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
    HRESULT hr = impl->d3d11->CreateGeometryShader(bytecode.data, bytecode.len, nullptr, &shader);
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

std::size_t HeliosDxvkDevice::create_hull_shader(const std::uint8_t* code, std::size_t len) const {
  if (!impl || !impl->d3d11 || !code || !len)
    return 0;
  ID3D11HullShader* shader = nullptr;
  try {
    auto bytecode = prepare_shader_bytecode(code, len);
    if (!bytecode)
      return 0;
    HRESULT hr = impl->d3d11->CreateHullShader(bytecode.data, bytecode.len, nullptr, &shader);
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
    HRESULT hr = impl->d3d11->CreateDomainShader(bytecode.data, bytecode.len, nullptr, &shader);
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
    HRESULT hr = impl->d3d11->CreateComputeShader(bytecode.data, bytecode.len, nullptr, &shader);
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
  // Force selection of the Helios venus device if other ICDs are present.
  _putenv_s("DXVK_FILTER_DEVICE_NAME", "Virtio-GPU Venus");
  _putenv_s("HELIOS_DXVK_KMT_SHARED", "1");

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
    d.venus_ctx_id = read_current_venus_context_id();
    if (!d.venus_ctx_id)
      umd_log("DXVK device created but Venus context export returned 0");
    umd_log("DxvkDevice created on venus adapter OK");

    // Instantiate DXVK's full D3D11 COM device from the DxvkDevice. The DDI
    // device-funcs forward to this ID3D11Device / its immediate context.
    HeliosStubAdapter* stubAdapter = new HeliosStubAdapter();
    auto* dxgiDevice = new dxvk::D3D11DXGIDevice(
        stubAdapter, nullptr, nullptr,
        d.instance, d.adapter, d.device,
        D3D_FEATURE_LEVEL_11_0, 0);
    stubAdapter->Release(); // dxgiDevice holds its own ref now

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
