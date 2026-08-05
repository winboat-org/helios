// d3d12_devicecreate_probe.cpp -- does D3D12CreateDevice succeed on the Helios
// adapter, and what does the driver say when it does not?
//
// This is `GATES.md` D12-G6's and D12-G7's first command. G6 requires that
// D3D12CreateDevice on Helios still FAILS with the `UmdD3D12` kill switch
// absent; G7 requires that it SUCCEEDS with the switch on. One probe, one
// `--expect` argument, so the same binary is the pass criterion for both and
// the two gates cannot drift apart.
//
// ASCII only, on purpose: this file lives on the Z:\ 9p share, which the VM's
// tooling reads as ANSI.
//
// Build (on the VM, from an x64 developer prompt):
//   cl /nologo /EHsc /W4 Z:\tools\d3d12_devicecreate_probe.cpp
//      /Fe:C:\Users\Rupansh\d12g6\devcreate.exe
//      /link d3d12.lib dxgi.lib dxguid.lib
//
// Usage:
//   devcreate.exe                 report only, exit 0
//   devcreate.exe --expect fail   exit 0 iff creation FAILED   (D12-G6)
//   devcreate.exe --expect ok     exit 0 iff creation SUCCEEDED (D12-G7)
//
// WHY IT USES DXGI AND THE DRIVER MUST NOT. A WDDM UMD sits *below* DXGI and
// must never link it (`umd/build.rs:247-250`, ARCHITECTURE.md §12 rule 15).
// That rule is about the driver. This is an app: DXGI adapter enumeration is
// the only way an app names a specific adapter for D3D12CreateDevice, and the
// gate's whole point is to exercise the real client path.
//
// ADAPTER SELECTION. Helios is matched on its DXGI description, which is the
// `DeviceDesc` string the INF installs ("Helios vGPU Render Adapter"). The
// D3DKMT route the D3D11 probes use (KMTQAITYPE_UMDRIVERNAME, because
// KMTQAITYPE_ADAPTERREGISTRYINFO fails on every adapter on this box) is not
// available here: D3D12CreateDevice wants an IDXGIAdapter, so the enumeration
// has to be DXGI's either way.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

#include <d3d12.h>
#include <dxgi1_6.h>

#include <cstdio>
#include <cstring>
#include <cwchar>

namespace {

// The substring that identifies Helios in a DXGI adapter description. Matches
// "Helios vGPU Render Adapter (WDDM bring-up)" without pinning the parenthetical,
// which is a bring-up label and is expected to change.
const wchar_t* kHeliosDescription = L"Helios";

enum class Expect { Report, Fail, Ok };

void print_hr(const char* what, HRESULT hr) {
    printf("%-34s hr=0x%08lX\n", what, static_cast<unsigned long>(hr));
}

const char* feature_level_name(D3D_FEATURE_LEVEL fl) {
    switch (fl) {
        case D3D_FEATURE_LEVEL_11_0: return "11_0";
        case D3D_FEATURE_LEVEL_11_1: return "11_1";
        case D3D_FEATURE_LEVEL_12_0: return "12_0";
        case D3D_FEATURE_LEVEL_12_1: return "12_1";
        case D3D_FEATURE_LEVEL_12_2: return "12_2";
        default: return "?";
    }
}

}  // namespace

int main(int argc, char** argv) {
    Expect expect = Expect::Report;
    for (int i = 1; i < argc; ++i) {
        if (std::strcmp(argv[i], "--expect") == 0 && i + 1 < argc) {
            ++i;
            if (std::strcmp(argv[i], "fail") == 0) {
                expect = Expect::Fail;
            } else if (std::strcmp(argv[i], "ok") == 0) {
                expect = Expect::Ok;
            } else {
                printf("FAIL: --expect takes 'fail' or 'ok', got '%s'\n", argv[i]);
                return 2;
            }
        } else {
            printf("FAIL: unrecognised argument '%s'\n", argv[i]);
            return 2;
        }
    }

    IDXGIFactory4* factory = nullptr;
    HRESULT hr = CreateDXGIFactory2(0, IID_PPV_ARGS(&factory));
    print_hr("CreateDXGIFactory2", hr);
    if (FAILED(hr)) {
        return 2;
    }

    IDXGIAdapter1* helios = nullptr;
    DXGI_ADAPTER_DESC1 helios_desc = {};
    printf("\n-- adapters --\n");
    for (UINT i = 0;; ++i) {
        IDXGIAdapter1* adapter = nullptr;
        if (factory->EnumAdapters1(i, &adapter) == DXGI_ERROR_NOT_FOUND) {
            break;
        }
        DXGI_ADAPTER_DESC1 desc = {};
        adapter->GetDesc1(&desc);
        printf("[%u] %-46ws vendor=%04X device=%04X luid=%08lX:%08lX flags=%u\n",
               i,
               desc.Description,
               desc.VendorId,
               desc.DeviceId,
               static_cast<unsigned long>(desc.AdapterLuid.HighPart),
               static_cast<unsigned long>(desc.AdapterLuid.LowPart),
               desc.Flags);
        if (!helios && std::wcsstr(desc.Description, kHeliosDescription) != nullptr) {
            helios = adapter;  // keep this reference
            helios_desc = desc;
            continue;
        }
        adapter->Release();
    }

    if (!helios) {
        printf("\nFAIL: no adapter whose description contains '%ws'\n", kHeliosDescription);
        factory->Release();
        return 2;
    }
    printf("\nHelios adapter: %ws (luid=%08lX:%08lX)\n",
           helios_desc.Description,
           static_cast<unsigned long>(helios_desc.AdapterLuid.HighPart),
           static_cast<unsigned long>(helios_desc.AdapterLuid.LowPart));

    // FL 11_0 deliberately: it is the lowest level D3D12 defines, so a failure
    // here cannot be "the driver does not do 12_x" -- it is the driver refusing
    // the DDI, which is what both gates are about.
    ID3D12Device* device = nullptr;
    hr = D3D12CreateDevice(helios, D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&device));
    printf("\n");
    print_hr("D3D12CreateDevice(FL 11_0)", hr);

    const bool created = SUCCEEDED(hr) && device != nullptr;
    if (created) {
        LUID luid = device->GetAdapterLuid();
        printf("%-34s luid=%08lX:%08lX nodes=%u\n",
               "device reports",
               static_cast<unsigned long>(luid.HighPart),
               static_cast<unsigned long>(luid.LowPart),
               device->GetNodeCount());

        // Which feature level did the driver's caps actually back? The create
        // above asked for the floor; this asks what it can really do.
        D3D_FEATURE_LEVEL levels[] = {
            D3D_FEATURE_LEVEL_12_2, D3D_FEATURE_LEVEL_12_1, D3D_FEATURE_LEVEL_12_0,
            D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0,
        };
        D3D12_FEATURE_DATA_FEATURE_LEVELS fl = {};
        fl.NumFeatureLevels = static_cast<UINT>(_countof(levels));
        fl.pFeatureLevelsRequested = levels;
        HRESULT flhr = device->CheckFeatureSupport(D3D12_FEATURE_FEATURE_LEVELS, &fl, sizeof(fl));
        if (SUCCEEDED(flhr)) {
            printf("%-34s %s\n", "max feature level", feature_level_name(fl.MaxSupportedFeatureLevel));
        } else {
            print_hr("CheckFeatureSupport(LEVELS)", flhr);
        }

        ULONG rc = device->Release();
        printf("%-34s refcount=%lu\n", "final Release()", static_cast<unsigned long>(rc));
    }

    helios->Release();
    factory->Release();

    switch (expect) {
        case Expect::Report:
            printf("\nRESULT: %s (no --expect given, reporting only)\n",
                   created ? "device CREATED" : "device NOT created");
            return 0;
        case Expect::Fail:
            if (created) {
                printf("\nFAIL: expected D3D12CreateDevice to FAIL and it SUCCEEDED.\n");
                printf("      With UmdD3D12 absent the D3D12 path must be bit-identical to a\n");
                printf("      build without it (DECISIONS.md D11). Check the knob really is absent.\n");
                return 1;
            }
            printf("\nPASS: D3D12CreateDevice failed, as required with the kill switch absent.\n");
            return 0;
        case Expect::Ok:
            if (!created) {
                printf("\nFAIL: expected D3D12CreateDevice to SUCCEED and it FAILED.\n");
                printf("      Read C:\\ProgramData\\Helios\\umd12-<pid>.log AFTER the last\n");
                printf("      'UMD module:' line -- the refusal counters name the step.\n");
                return 1;
            }
            printf("\nPASS: D3D12CreateDevice succeeded through helios_umd12.dll.\n");
            return 0;
    }
    return 2;
}
