// End-to-end cross-process VidMm lifetime probe for Helios D3D11.
//
// A child creates and clears a 64-MiB legacy-shared render target.  The parent
// opens it, lets the creator process exit, then verifies both its pixels and
// its dedicated-memory charge before releasing the last reference.
//
// Build from an MSVC developer command prompt:
//   cl /nologo /EHsc /W4 /O2 tools\d3d11_xproc_lifetime_probe.cpp \
//      /Iicd\win-build\wdk-include /Fe:d3d11_xproc_lifetime_probe.exe \
//      /link dxgi.lib d3d11.lib gdi32.lib

#define WIN32_LEAN_AND_MEAN
#include <windows.h>

#include <d3d11.h>
#ifndef _NTDEF_
typedef LONG NTSTATUS, *PNTSTATUS;
#endif
#include <d3dkmthk.h>
#include <dxgi1_2.h>

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>

namespace {

constexpr UINT kWidth = 4096;
constexpr UINT kHeight = 4096;
constexpr uint64_t kExpectedBytes = static_cast<uint64_t>(kWidth) * kHeight * 4;
constexpr uint64_t kProcessTolerance = 2ull * 1024 * 1024;
// Adapter totals include DWM and every other process, so allow bounded desktop
// churn while the importer-local counter remains the exact assertion.
constexpr uint64_t kGlobalTolerance = 16ull * 1024 * 1024;
constexpr UINT kResultMagic = 0x44335850; // 'D3XP'

struct ChildResult {
  UINT magic;
  UINT status;
  UINT64 shared_handle;
};

ID3D11Device *g_device;
ID3D11DeviceContext *g_context;
LUID g_adapter_luid;

void release_device() {
  if (g_context)
    g_context->Release();
  if (g_device)
    g_device->Release();
  g_context = nullptr;
  g_device = nullptr;
}

bool make_device() {
  IDXGIFactory1 *factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(IID_PPV_ARGS(&factory));
  if (FAILED(hr)) {
    std::printf("CreateDXGIFactory1 failed: 0x%08x\n", static_cast<UINT>(hr));
    return false;
  }

  IDXGIAdapter1 *helios = nullptr;
  for (UINT i = 0; factory->EnumAdapters1(i, &helios) != DXGI_ERROR_NOT_FOUND;
       ++i) {
    DXGI_ADAPTER_DESC1 desc{};
    if (SUCCEEDED(helios->GetDesc1(&desc)) &&
        std::wcsstr(desc.Description, L"Helios")) {
      g_adapter_luid = desc.AdapterLuid;
      break;
    }
    helios->Release();
    helios = nullptr;
  }
  if (!helios) {
    std::puts("Helios adapter not found");
    factory->Release();
    return false;
  }

  static const D3D_FEATURE_LEVEL levels[] = {
      D3D_FEATURE_LEVEL_11_1,
      D3D_FEATURE_LEVEL_11_0,
      D3D_FEATURE_LEVEL_10_1,
      D3D_FEATURE_LEVEL_10_0,
  };
  D3D_FEATURE_LEVEL level{};
  hr = D3D11CreateDevice(helios, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                         D3D11_CREATE_DEVICE_BGRA_SUPPORT, levels,
                         _countof(levels), D3D11_SDK_VERSION, &g_device, &level,
                         &g_context);
  std::printf("D3D11CreateDevice hr=0x%08x fl=0x%04x\n", static_cast<UINT>(hr),
              static_cast<UINT>(level));
  helios->Release();
  factory->Release();
  return SUCCEEDED(hr) && g_device && g_context;
}

bool query_committed(bool global, const char *label, uint64_t *value) {
  uint64_t local = 0;
  bool found_local = false;
  bool valid = true;
  for (ULONG segment_id = 1; segment_id <= 2; ++segment_id) {
    D3DKMT_QUERYSTATISTICS segment{};
    segment.Type = D3DKMT_QUERYSTATISTICS_SEGMENT;
    segment.AdapterLuid = g_adapter_luid;
    segment.QuerySegment.SegmentId = segment_id;
    const NTSTATUS segment_status = D3DKMTQueryStatistics(&segment);
    if (segment_status != 0) {
      std::printf("%s adapter segment %lu query failed: 0x%08x\n", label,
                  segment_id, static_cast<UINT>(segment_status));
      valid = false;
      continue;
    }
    if (segment.QueryResult.SegmentInformation.Aperture)
      continue;
    found_local = true;

    if (global) {
      local += segment.QueryResult.SegmentInformation.BytesCommitted;
    } else {
      D3DKMT_QUERYSTATISTICS process{};
      process.Type = D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT;
      process.AdapterLuid = g_adapter_luid;
      process.hProcess = GetCurrentProcess();
      process.QueryProcessSegment.SegmentId = segment_id;
      const NTSTATUS process_status = D3DKMTQueryStatistics(&process);
      if (process_status == 0) {
        local += process.QueryResult.ProcessSegmentInformation.BytesCommitted;
      } else {
        std::printf("%s process segment %lu query failed: 0x%08x\n", label,
                    segment_id, static_cast<UINT>(process_status));
        valid = false;
      }
    }
  }
  std::printf("%-16s %s local committed = %.2f MiB\n", label,
              global ? "adapter" : "process",
              static_cast<double>(local) / (1024.0 * 1024.0));
  if (value)
    *value = local;
  return valid && found_local;
}

bool wait_for_usage(const char *label, uint64_t global_before,
                    uint64_t process_before, bool retained,
                    uint64_t *global_value, uint64_t *process_value) {
  const ULONGLONG deadline = GetTickCount64() + 2000;
  for (;;) {
    if (!query_committed(true, label, global_value) ||
        !query_committed(false, label, process_value))
      return false;
    const uint64_t global_delta =
        *global_value >= global_before ? *global_value - global_before : 0;
    const uint64_t process_delta =
        *process_value >= process_before ? *process_value - process_before : 0;
    const bool ready =
        retained ? global_delta + kGlobalTolerance >= kExpectedBytes &&
                       global_delta <= kExpectedBytes + kGlobalTolerance &&
                       process_delta >= kExpectedBytes &&
                       process_delta <= kExpectedBytes + kProcessTolerance
                 : *global_value <= global_before + kGlobalTolerance &&
                       *process_value <= process_before + kProcessTolerance;
    if (ready)
      return true;
    if (GetTickCount64() >= deadline)
      return false;
    Sleep(25);
  }
}

bool write_result(HANDLE pipe, UINT status, HANDLE shared_handle) {
  ChildResult result{
      kResultMagic, status,
      static_cast<UINT64>(reinterpret_cast<UINT_PTR>(shared_handle))};
  DWORD written = 0;
  return WriteFile(pipe, &result, sizeof(result), &written, nullptr) &&
         written == sizeof(result);
}

bool read_result(HANDLE pipe, HANDLE child, ChildResult *result) {
  const ULONGLONG deadline = GetTickCount64() + 30000;
  for (;;) {
    DWORD available = 0;
    DWORD read = 0;
    if (!PeekNamedPipe(pipe, nullptr, 0, nullptr, &available, nullptr)) {
      std::printf("PeekNamedPipe failed: %lu\n", GetLastError());
      return false;
    }
    if (available >= sizeof(*result)) {
      return ReadFile(pipe, result, sizeof(*result), &read, nullptr) &&
             read == sizeof(*result);
    }
    if (WaitForSingleObject(child, 10) == WAIT_OBJECT_0) {
      if (!PeekNamedPipe(pipe, nullptr, 0, nullptr, &available, nullptr) ||
          available < sizeof(*result)) {
        std::puts("creator exited before publishing its result");
        return false;
      }
    }
    if (GetTickCount64() >= deadline) {
      std::puts("timed out waiting for creator result");
      return false;
    }
  }
}

int creator_child(uint64_t result_pipe_value, uint64_t release_event_value) {
  HANDLE result_pipe =
      reinterpret_cast<HANDLE>(static_cast<UINT_PTR>(result_pipe_value));
  HANDLE release_event =
      reinterpret_cast<HANDLE>(static_cast<UINT_PTR>(release_event_value));
  ID3D11Texture2D *texture = nullptr;
  ID3D11RenderTargetView *view = nullptr;
  IDXGIResource *resource = nullptr;
  HANDLE shared_handle = nullptr;
  UINT status = 1;

  if (make_device()) {
    D3D11_TEXTURE2D_DESC desc{};
    desc.Width = kWidth;
    desc.Height = kHeight;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED;
    HRESULT hr = g_device->CreateTexture2D(&desc, nullptr, &texture);
    if (SUCCEEDED(hr))
      hr = g_device->CreateRenderTargetView(texture, nullptr, &view);
    if (SUCCEEDED(hr)) {
      const FLOAT magenta[] = {1.0f, 0.0f, 1.0f, 1.0f};
      g_context->ClearRenderTargetView(view, magenta);
      g_context->Flush();
      hr = texture->QueryInterface(IID_PPV_ARGS(&resource));
    }
    if (SUCCEEDED(hr))
      hr = resource->GetSharedHandle(&shared_handle);
    std::printf("creator resource hr=0x%08x handle=%p\n", static_cast<UINT>(hr),
                shared_handle);
    if (SUCCEEDED(hr) && shared_handle)
      status = 0;
  }

  const bool published = write_result(result_pipe, status, shared_handle);
  CloseHandle(result_pipe);
  if (status == 0 && published &&
      WaitForSingleObject(release_event, 30000) != WAIT_OBJECT_0) {
    std::puts("creator timed out waiting for importer");
    status = 1;
  }
  CloseHandle(release_event);
  if (resource)
    resource->Release();
  if (view)
    view->Release();
  if (texture)
    texture->Release();
  if (g_context)
    g_context->Flush();
  release_device();
  return status == 0 && published ? 0 : 1;
}

bool verify_pixel(ID3D11Texture2D *source) {
  D3D11_TEXTURE2D_DESC desc{};
  source->GetDesc(&desc);
  desc.BindFlags = 0;
  desc.MiscFlags = 0;
  desc.Usage = D3D11_USAGE_STAGING;
  desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D *staging = nullptr;
  HRESULT hr = g_device->CreateTexture2D(&desc, nullptr, &staging);
  if (FAILED(hr))
    return false;
  g_context->CopyResource(staging, source);
  g_context->Flush();
  D3D11_MAPPED_SUBRESOURCE mapped{};
  hr = g_context->Map(staging, 0, D3D11_MAP_READ, 0, &mapped);
  UINT pixel = 0;
  if (SUCCEEDED(hr)) {
    pixel = *static_cast<const UINT *>(mapped.pData);
    g_context->Unmap(staging, 0);
  }
  staging->Release();
  std::printf("pixel after creator exit = %08x (expected ffff00ff)\n", pixel);
  return SUCCEEDED(hr) && pixel == 0xffff00ffu;
}

} // namespace

int main(int argc, char **argv) {
  if (argc == 4 && std::strcmp(argv[1], "--creator-child") == 0) {
    return creator_child(_strtoui64(argv[2], nullptr, 0),
                         _strtoui64(argv[3], nullptr, 0));
  }
  if (argc != 1) {
    std::printf("usage: %s\n", argv[0]);
    return 2;
  }
  if (!make_device())
    return 1;

  uint64_t global_before = 0;
  uint64_t process_before = 0;
  if (!query_committed(true, "baseline", &global_before) ||
      !query_committed(false, "baseline", &process_before)) {
    release_device();
    return 1;
  }
  SECURITY_ATTRIBUTES security{sizeof(security), nullptr, TRUE};
  HANDLE read_pipe = nullptr;
  HANDLE write_pipe = nullptr;
  HANDLE release_event = nullptr;
  PROCESS_INFORMATION process{};
  bool failed = false;

  if (!CreatePipe(&read_pipe, &write_pipe, &security, 0) ||
      !SetHandleInformation(read_pipe, HANDLE_FLAG_INHERIT, 0)) {
    std::puts("failed to create result pipe");
    failed = true;
  }
  if (!failed) {
    release_event = CreateEventA(&security, TRUE, FALSE, nullptr);
    failed = release_event == nullptr;
  }
  if (!failed) {
    char executable[MAX_PATH]{};
    char command[MAX_PATH + 128]{};
    const DWORD executable_length =
        GetModuleFileNameA(nullptr, executable, _countof(executable));
    if (executable_length == 0 || executable_length >= _countof(executable)) {
      std::printf("GetModuleFileName failed or truncated: %lu\n",
                  GetLastError());
      failed = true;
    }
    const int command_length = std::snprintf(
        command, sizeof(command), "\"%s\" --creator-child %llu %llu",
        executable,
        static_cast<unsigned long long>(reinterpret_cast<UINT_PTR>(write_pipe)),
        static_cast<unsigned long long>(
            reinterpret_cast<UINT_PTR>(release_event)));
    if (command_length < 0 ||
        static_cast<size_t>(command_length) >= sizeof(command)) {
      std::puts("creator command line is too long");
      failed = true;
    }
    STARTUPINFOA startup{};
    startup.cb = sizeof(startup);
    if (!failed && !CreateProcessA(nullptr, command, nullptr, nullptr, TRUE, 0,
                                   nullptr, nullptr, &startup, &process)) {
      std::printf("CreateProcess failed: %lu\n", GetLastError());
      failed = true;
    }
  }
  if (write_pipe) {
    CloseHandle(write_pipe);
    write_pipe = nullptr;
  }

  ChildResult result{};
  if (!failed && (!read_result(read_pipe, process.hProcess, &result) ||
                  result.magic != kResultMagic || result.status != 0 ||
                  !result.shared_handle)) {
    std::printf("creator result invalid: magic=%08x status=%u handle=%llx\n",
                result.magic, result.status,
                static_cast<unsigned long long>(result.shared_handle));
    failed = true;
  }

  ID3D11Texture2D *opened = nullptr;
  if (!failed) {
    const HANDLE shared_handle =
        reinterpret_cast<HANDLE>(static_cast<UINT_PTR>(result.shared_handle));
    const HRESULT hr =
        g_device->OpenSharedResource(shared_handle, IID_PPV_ARGS(&opened));
    std::printf("OpenSharedResource hr=0x%08x texture=%p\n",
                static_cast<UINT>(hr), opened);
    failed = FAILED(hr) || !opened;
  }

  if (!failed && opened) {
    uint64_t global_open = 0;
    uint64_t process_open = 0;
    if (!wait_for_usage("both-open", global_before, process_before, true,
                        &global_open, &process_open)) {
      std::puts("shared open did not preserve exactly one global charge");
      failed = true;
    }
  }

  if (release_event)
    SetEvent(release_event);
  DWORD child_exit = 1;
  if (process.hProcess &&
      WaitForSingleObject(process.hProcess, 30000) != WAIT_OBJECT_0) {
    std::puts("creator timed out during teardown; terminating it");
    TerminateProcess(process.hProcess, 1);
    WaitForSingleObject(process.hProcess, 5000);
    failed = true;
  }
  if (process.hProcess &&
      (!GetExitCodeProcess(process.hProcess, &child_exit) || child_exit != 0)) {
    std::printf("creator did not exit cleanly: %lu\n", child_exit);
    failed = true;
  }
  if (opened) {
    uint64_t global_alive = 0;
    uint64_t process_alive = 0;
    if (!wait_for_usage("creator-exited", global_before, process_before, true,
                        &global_alive, &process_alive))
      failed = true;
    const uint64_t global_delta =
        global_alive >= global_before ? global_alive - global_before : 0;
    const uint64_t process_delta =
        process_alive >= process_before ? process_alive - process_before : 0;
    std::printf("retained adapter delta = %.2f MiB; process delta = %.2f MiB\n",
                static_cast<double>(global_delta) / (1024.0 * 1024.0),
                static_cast<double>(process_delta) / (1024.0 * 1024.0));
    if (!verify_pixel(opened)) {
      failed = true;
    }
    opened->Release();
    g_context->ClearState();
    g_context->Flush();
    // DXVK may retain submitted resources until device teardown reaps the
    // command stream.  Destroy the last device before checking final VidMm
    // cleanup; a COM Release followed only by Flush is not a lifetime fence.
    release_device();
  }

  uint64_t global_after = 0;
  uint64_t process_after = 0;
  if (!wait_for_usage("released", global_before, process_before, false,
                      &global_after, &process_after)) {
    std::puts("memory charge did not return to baseline");
    failed = true;
  }

  if (process.hProcess &&
      WaitForSingleObject(process.hProcess, 0) == WAIT_TIMEOUT) {
    TerminateProcess(process.hProcess, 1);
    WaitForSingleObject(process.hProcess, 5000);
    failed = true;
  }

  if (process.hThread)
    CloseHandle(process.hThread);
  if (process.hProcess)
    CloseHandle(process.hProcess);
  if (release_event)
    CloseHandle(release_event);
  if (read_pipe)
    CloseHandle(read_pipe);
  release_device();
  std::puts(failed ? "D3D11 XPROC LIFETIME: FAIL"
                   : "D3D11 XPROC LIFETIME: PASS");
  return failed ? 1 : 0;
}
