// tools/d3d12_format_matrix_probe.cpp - the engine's per-format answer, as a CSV.
//
// `GATES.md` section 3.2 asks for this probe by name, mirroring `CONFORMANCE.md`
// C5: `CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT)` over the DXGI format
// range, for the `D12-G9` baseline. It gained a second, urgent job first.
//
// WHY IT EXISTS NOW. `D12-G7` failed with `DXGI_ERROR_DRIVER_INTERNAL_ERROR` and
// the ETW `Microsoft-Windows-Direct3D12` reason "MSAA quality reported to be 0"
// (index 62), after the runtime abandoned its 91-format sweep partway. The UMD's
// own log records what the DRIVER answered, which was the wrong half of the
// question: what was needed is what the ENGINE says, per format and per sample
// count, so the driver's translation can be checked against it instead of
// guessed at. Two runs were spent guessing. This is the instrument that stops
// that.
//
// ** It does NOT go through the DDI. ** It loads the deployed `helios_umd12.dll`
// and calls `helios_umd12_probe_create_device_v1`, the same test-only export
// `tools/d3d12_bridge_probe.cpp`'s third arm uses, to get a BORROWED
// `ID3D12Device*` straight from the vkd3d engine. So it needs no adapter
// restart, no `UmdD3D12` knob and no D3D12 runtime - and, because it reads the
// engine rather than the driver, its output is the input the driver's
// translation must be correct with respect to.
//
// ** Not an app-facing vkd3d arm. ** `DECISIONS.md` D2 forbids shipping or
// measuring vkd3d's `d3d12.dll` as an application's D3D12; this links no
// `d3d12.lib`, creates no D3D12 device through the runtime, and exercises the
// same export a gate probe already does. It is an instrument, not a path.
//
// BUILD (on the VM; `cl` needs vcvars64 - GATES.md section 3.2 rule 8):
//   $VC='C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
//   New-Item -ItemType Directory -Force -Path C:\Users\Rupansh\d12fmt | Out-Null
//   cmd /c "call `"$VC`" >nul && cl /nologo /EHsc /W4 Z:\tools\d3d12_format_matrix_probe.cpp /Fe:C:\Users\Rupansh\d12fmt\fmt.exe /link advapi32.lib"
// `advapi32` is for the three Reg* calls that locate the deployed DLL, and
// nothing else. It links no `d3d12.lib` and no `dxgi.lib`: the device comes from
// the UMD's own export, which is the whole point.
//
// RUN:
//   C:\Users\Rupansh\d12fmt\fmt.exe [path-to-helios_umd12.dll] > formats.csv
// With no argument it reads `UserModeDriverName[3]` out of the display class key
// and probes whatever is actually deployed, which is the copy that matters.

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>

#include <d3d12.h>

#include <cstdio>
#include <cstring>

// The two test-only exports this probe needs. Signatures from
// `umd12/src/probe12.rs`; both are `extern "C"`.
typedef HRESULT(__cdecl* PFN_PROBE_CREATE)(unsigned int luid_low, int luid_high,
                                           void** out_bridge, void** out_device);
typedef void(__cdecl* PFN_PROBE_DESTROY)(void* bridge);

// The highest DXGI_FORMAT worth walking. 115 is DXGI_FORMAT_B4G4R4A4_UNORM in
// the classic range; the runtime's own device-creation sweep touches 91 of them.
// Deliberately a walk of the whole range rather than a list: a list would be one
// more hand-maintained table to get wrong, and an unknown format is a legal
// answer here (the engine refuses it and the row says so).
static const int kMaxFormat = 115;

// Every sample count `vk_samples_from_sample_count` can map, plus 1. Anything
// else can only ever answer zero, so probing it would add rows and no facts.
static const unsigned int kSampleCounts[] = {1, 2, 4, 8, 16, 32};

static const char* find_deployed_umd12(char* buf, DWORD cb) {
  // The display class key, enumerated the way `hotplug-helios-umd.ps1` does:
  // subkey 0000.. under the class GUID, whichever has our DriverDesc.
  static const char* kClass =
      "SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}";
  for (DWORD i = 0; i < 32; ++i) {
    char sub[512];
    _snprintf_s(sub, sizeof(sub), _TRUNCATE, "%s\\%04u", kClass, i);
    HKEY k;
    if (RegOpenKeyExA(HKEY_LOCAL_MACHINE, sub, 0, KEY_READ, &k) != ERROR_SUCCESS) continue;
    char desc[256] = {};
    DWORD n = sizeof(desc), type = 0;
    bool ours = RegQueryValueExA(k, "DriverDesc", nullptr, &type, (LPBYTE)desc, &n) == ERROR_SUCCESS &&
                strstr(desc, "Helios") != nullptr;
    if (ours) {
      // UserModeDriverName is REG_MULTI_SZ; slot 3 is the D3D12 UMD
      // (`DECISIONS.md` D3). Walk to the fourth string.
      char multi[4096] = {};
      n = sizeof(multi);
      if (RegQueryValueExA(k, "UserModeDriverName", nullptr, &type, (LPBYTE)multi, &n) == ERROR_SUCCESS) {
        const char* p = multi;
        for (int slot = 0; slot < 3 && *p; ++slot) p += strlen(p) + 1;
        if (*p) {
          strncpy_s(buf, cb, p, _TRUNCATE);
          RegCloseKey(k);
          return buf;
        }
      }
    }
    RegCloseKey(k);
  }
  return nullptr;
}

int main(int argc, char** argv) {
  char path[MAX_PATH] = {};
  const char* dll = (argc > 1) ? argv[1] : find_deployed_umd12(path, sizeof(path));
  if (!dll) {
    std::fprintf(stderr, "FAIL: no helios_umd12.dll given and UserModeDriverName[3] not found\n");
    return 2;
  }
  std::fprintf(stderr, "umd12: %s\n", dll);

  HMODULE m = LoadLibraryA(dll);
  if (!m) {
    std::fprintf(stderr, "FAIL: LoadLibrary(%s) -> %lu\n", dll, GetLastError());
    return 2;
  }
  auto create = (PFN_PROBE_CREATE)GetProcAddress(m, "helios_umd12_probe_create_device_v1");
  auto destroy = (PFN_PROBE_DESTROY)GetProcAddress(m, "helios_umd12_probe_destroy_device_v1");
  if (!create || !destroy) {
    std::fprintf(stderr, "FAIL: probe exports missing (create=%p destroy=%p)\n",
                 (void*)create, (void*)destroy);
    return 2;
  }

  void* bridge = nullptr;
  void* raw = nullptr;
  // (0, 0) is "do not match on LUID" - the same argument `device12::create_device`
  // passes, and for the same reason (a D3D12 UMD has no supported way to obtain
  // its adapter's LUID, and vkd3d does not LUID-match).
  HRESULT hr = create(0, 0, &bridge, &raw);
  if (FAILED(hr) || !raw) {
    std::fprintf(stderr, "FAIL: probe_create_device -> 0x%08lx dev=%p\n", (unsigned long)hr, raw);
    return 2;
  }
  ID3D12Device* dev = (ID3D12Device*)raw;
  // BORROWED: the bridge owns the reference, so this probe must not Release it.
  // AddRef+Release around use would be equally correct; not touching the count
  // at all is simpler and cannot get out of balance.

  std::printf("format,support1,support2,quality_levels_by_sample_count\n");
  for (int f = 0; f <= kMaxFormat; ++f) {
    D3D12_FEATURE_DATA_FORMAT_SUPPORT fs = {};
    fs.Format = (DXGI_FORMAT)f;
    HRESULT fhr = dev->CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT, &fs, sizeof(fs));

    char levels[128] = {};
    size_t off = 0;
    for (size_t i = 0; i < sizeof(kSampleCounts) / sizeof(kSampleCounts[0]); ++i) {
      D3D12_FEATURE_DATA_MULTISAMPLE_QUALITY_LEVELS ms = {};
      ms.Format = (DXGI_FORMAT)f;
      ms.SampleCount = kSampleCounts[i];
      ms.Flags = D3D12_MULTISAMPLE_QUALITY_LEVELS_FLAG_NONE;
      HRESULT mhr = dev->CheckFeatureSupport(D3D12_FEATURE_MULTISAMPLE_QUALITY_LEVELS, &ms, sizeof(ms));
      int wrote = _snprintf_s(levels + off, sizeof(levels) - off, _TRUNCATE, "%s%u:%s",
                              i ? " " : "", kSampleCounts[i],
                              FAILED(mhr) ? "ERR" : (ms.NumQualityLevels ? "1" : "0"));
      if (wrote > 0) off += (size_t)wrote;
    }

    if (FAILED(fhr)) {
      std::printf("%d,ERR_0x%08lx,,%s\n", f, (unsigned long)fhr, levels);
    } else {
      std::printf("%d,0x%08x,0x%08x,%s\n", f, (unsigned)fs.Support1, (unsigned)fs.Support2, levels);
    }
  }

  destroy(bridge);
  FreeLibrary(m);
  return 0;
}
