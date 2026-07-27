// Helios T5 gate — ownership soak.
//
// The T5 gate's headline is that the tranche's ownership encodings did not
// change lifetimes: "1000 CreateDevice/DestroyDevice cycles and 10000 resource
// create/destroy cycles with dwm and a test app, with handle count, module
// count and process working set FLAT at the end."
//
// T5 moved a lot of ownership around — Slot<P> take()/release (R803), the
// DeallocateForm discriminant (R804), BridgeOwned::release (R807), the
// ScanoutTarget variant that owns a COM ref (R809), and the whole owned-vs-
// borrowed bridge accessor split (R813/R815). Every one of those is a place a
// reference could now be released twice (a crash) or never (a leak). Nothing in
// the existing probe set exercises them at volume, so this exists.
//
// What it measures, in THIS process:
//   * GetProcessHandleCount   — kernel handles. Must be exactly flat: a leaked
//                               D3D device or allocation shows up here.
//   * EnumProcessModules      — loaded module count. Must be exactly flat: a
//                               device teardown that fails to unload the UMD,
//                               or one that reloads it per cycle, shows here.
//   * WorkingSetSize          — committed pages. Noisy by nature, so it is
//                               reported against a POST-WARMUP baseline with an
//                               explicit tolerance rather than demanded equal.
//
// It also samples dwm.exe's handle count and working set around the run, since
// the UMD is loaded there too and a leak in the compositor is the one that
// actually matters for a desktop that has to stay up.
//
// Build + run: tools\helios-ownership-soak.ps1
//
// Exit codes: 0 = flat, 2 = growth beyond tolerance, 1 = setup failure.

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <psapi.h>
#include <dxgi1_6.h>
#include <d3d11.h>

#include <cstdio>
#include <cstdlib>
#include <cwchar>
#include <vector>

namespace {

struct Sample {
    DWORD  handles = 0;
    DWORD  modules = 0;
    SIZE_T workingSet = 0;
};

bool sample_process(HANDLE p, Sample* out) {
    if (!GetProcessHandleCount(p, &out->handles))
        return false;
    PROCESS_MEMORY_COUNTERS pmc{};
    pmc.cb = sizeof(pmc);
    if (!GetProcessMemoryInfo(p, &pmc, sizeof(pmc)))
        return false;
    out->workingSet = pmc.WorkingSetSize;
    // Ask for the count only; the array is sized generously and the needed
    // byte count is what we actually report.
    HMODULE mods[1024];
    DWORD needed = 0;
    if (!EnumProcessModules(p, mods, sizeof(mods), &needed))
        return false;
    out->modules = needed / sizeof(HMODULE);
    return true;
}

void print_sample(const char* label, const Sample& s) {
    std::printf("%-24s handles=%-6lu modules=%-4lu workingSet=%llu KiB\n",
                label, s.handles, s.modules,
                static_cast<unsigned long long>(s.workingSet / 1024));
}

// dwm runs in session 1 as a different user; OpenProcess for query rights
// normally succeeds from an elevated context. A failure is reported, not fatal.
DWORD find_process_id(const wchar_t* exeName) {
    DWORD pids[4096];
    DWORD needed = 0;
    if (!EnumProcesses(pids, sizeof(pids), &needed))
        return 0;
    const DWORD count = needed / sizeof(DWORD);
    for (DWORD i = 0; i < count; ++i) {
        if (!pids[i])
            continue;
        HANDLE p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pids[i]);
        if (!p)
            continue;
        wchar_t path[MAX_PATH] = {};
        DWORD len = MAX_PATH;
        bool match = false;
        if (QueryFullProcessImageNameW(p, 0, path, &len)) {
            const wchar_t* base = wcsrchr(path, L'\\');
            base = base ? base + 1 : path;
            match = _wcsicmp(base, exeName) == 0;
        }
        CloseHandle(p);
        if (match)
            return pids[i];
    }
    return 0;
}

// `match` is a substring of DXGI_ADAPTER_DESC1::Description -- "Helios" for the
// driver under test, "Basic Render" or "Microsoft" for the WARP control.
//
// The control is not optional. A handle delta measured only against Helios
// cannot distinguish "our UMD leaks" from "D3D11CreateDevice leaks on this box
// regardless of adapter", and the project rule is not to blame a component
// without evidence that isolates it.
IDXGIAdapter1* find_adapter(IDXGIFactory1* factory, const wchar_t* match) {
    IDXGIAdapter1* adapter = nullptr;
    for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
        DXGI_ADAPTER_DESC1 desc{};
        adapter->GetDesc1(&desc);
        if (wcsstr(desc.Description, match) != nullptr) {
            std::wprintf(L"adapter: \"%s\"\n", desc.Description);
            return adapter;  // caller releases
        }
        adapter->Release();
    }
    return nullptr;
}

ID3D11Device* create_device(IDXGIAdapter1* adapter) {
    ID3D11Device* device = nullptr;
    D3D_FEATURE_LEVEL got{};
    const D3D_FEATURE_LEVEL levels[] = { D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1,
                                         D3D_FEATURE_LEVEL_10_0 };
    HRESULT hr = D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0, levels,
                                   ARRAYSIZE(levels), D3D11_SDK_VERSION, &device, &got, nullptr);
    if (FAILED(hr))
        return nullptr;
    return device;
}

// One resource cycle exercises the paths T5 actually touched: a WDDM-allocated
// texture (R803's Slot<Boxed<ResourceState>> + R804's DeallocateForm + R806's
// descriptors), a render-target view over it (R803's Slot<Boxed<RtvState>>,
// whose release path R803 rewrote to take()), and a buffer (the plain COM slot).
bool resource_cycle(ID3D11Device* device) {
    D3D11_TEXTURE2D_DESC td{};
    td.Width = 64;
    td.Height = 64;
    td.MipLevels = 1;
    td.ArraySize = 1;
    td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;

    ID3D11Texture2D* tex = nullptr;
    if (FAILED(device->CreateTexture2D(&td, nullptr, &tex)))
        return false;

    ID3D11RenderTargetView* rtv = nullptr;
    HRESULT hr = device->CreateRenderTargetView(tex, nullptr, &rtv);
    if (SUCCEEDED(hr))
        rtv->Release();

    D3D11_BUFFER_DESC bd{};
    bd.ByteWidth = 4096;
    bd.Usage = D3D11_USAGE_DEFAULT;
    bd.BindFlags = D3D11_BIND_VERTEX_BUFFER;
    ID3D11Buffer* buf = nullptr;
    if (SUCCEEDED(device->CreateBuffer(&bd, nullptr, &buf)))
        buf->Release();

    tex->Release();
    return SUCCEEDED(hr);
}

}  // namespace

int main(int argc, char** argv) {
    unsigned deviceCycles = 1000;
    unsigned resourceCycles = 10000;
    // Working-set tolerance. Allocator behaviour and DXVK's own caches make an
    // exactly-equal demand meaningless; anything genuinely leaking per cycle
    // blows well past this over 10k iterations.
    // 16 MiB. The WARP control grows ~8 MiB of working set over 1000 resource
    // cycles with a ZERO handle delta, so committed pages are partly a D3D11
    // runtime/allocator property rather than a driver signal. A tolerance
    // tighter than the control's own noise reports the runtime as a leak.
    unsigned wsToleranceKiB = 16384;
    const wchar_t* adapterMatch = L"Helios";
    if (argc > 1) deviceCycles = strtoul(argv[1], nullptr, 10);
    if (argc > 2) resourceCycles = strtoul(argv[2], nullptr, 10);
    if (argc > 3) wsToleranceKiB = strtoul(argv[3], nullptr, 10);
    if (argc > 4 && argv[4][0] == 'w') adapterMatch = L"Basic Render";  // WARP control

    std::printf("helios ownership soak: deviceCycles=%u resourceCycles=%u wsTolKiB=%u\n",
                deviceCycles, resourceCycles, wsToleranceKiB);

    IDXGIFactory1* factory = nullptr;
    if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), reinterpret_cast<void**>(&factory)))) {
        std::printf("FAIL: CreateDXGIFactory1\n");
        return 1;
    }
    IDXGIAdapter1* adapter = find_adapter(factory, adapterMatch);
    if (!adapter) {
        std::wprintf(L"FAIL: no adapter matching \"%s\" enumerated\n", adapterMatch);
        factory->Release();
        return 1;
    }

    HANDLE self = GetCurrentProcess();
    const DWORD dwmPid = find_process_id(L"dwm.exe");
    HANDLE dwm = dwmPid ? OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, dwmPid) : nullptr;
    Sample dwmBefore{};
    if (dwm && sample_process(dwm, &dwmBefore))
        print_sample("dwm before:", dwmBefore);
    else
        std::printf("dwm before:              (unavailable, pid=%lu)\n", dwmPid);

    // ---- warm up ---------------------------------------------------------
    // The FIRST device load pulls in helios_umd.dll, DXVK and the venus ICD,
    // and the first resources populate DXVK's caches. Measuring from before
    // that would report one-time initialisation as a leak.
    {
        ID3D11Device* warm = create_device(adapter);
        if (!warm) {
            std::printf("FAIL: D3D11CreateDevice on the selected adapter\n");
            adapter->Release();
            factory->Release();
            return 1;
        }
        for (unsigned i = 0; i < 64; ++i)
            resource_cycle(warm);
        warm->Release();
    }

    Sample base{};
    if (!sample_process(self, &base)) {
        std::printf("FAIL: sample_process(self)\n");
        return 1;
    }
    print_sample("baseline (post-warmup):", base);

    // ---- phase 1: device create/destroy ----------------------------------
    unsigned deviceFailures = 0;
    for (unsigned i = 0; i < deviceCycles; ++i) {
        ID3D11Device* d = create_device(adapter);
        if (!d) {
            ++deviceFailures;
            continue;
        }
        d->Release();
        if (((i + 1) % 100) == 0) {
            Sample s{};
            if (sample_process(self, &s))
                std::printf("  device cycle %-5u handles=%-6lu modules=%-4lu ws=%llu KiB\n",
                            i + 1, s.handles, s.modules,
                            static_cast<unsigned long long>(s.workingSet / 1024));
        }
    }
    Sample afterDevices{};
    sample_process(self, &afterDevices);
    print_sample("after device cycles:", afterDevices);

    // ---- phase 2: resource create/destroy --------------------------------
    unsigned resourceFailures = 0;
    {
        ID3D11Device* d = create_device(adapter);
        if (!d) {
            std::printf("FAIL: device for the resource phase\n");
            adapter->Release();
            factory->Release();
            return 1;
        }
        for (unsigned i = 0; i < resourceCycles; ++i) {
            if (!resource_cycle(d))
                ++resourceFailures;
            if (((i + 1) % 1000) == 0) {
                Sample s{};
                if (sample_process(self, &s))
                    std::printf("  resource cycle %-6u handles=%-6lu modules=%-4lu ws=%llu KiB\n",
                                i + 1, s.handles, s.modules,
                                static_cast<unsigned long long>(s.workingSet / 1024));
            }
        }
        d->Release();
    }

    Sample final{};
    sample_process(self, &final);
    print_sample("final:", final);

    Sample dwmAfter{};
    if (dwm && sample_process(dwm, &dwmAfter))
        print_sample("dwm after:", dwmAfter);

    adapter->Release();
    factory->Release();
    if (dwm)
        CloseHandle(dwm);

    // ---- verdict ---------------------------------------------------------
    const long handleDelta = static_cast<long>(final.handles) - static_cast<long>(base.handles);
    const long moduleDelta = static_cast<long>(final.modules) - static_cast<long>(base.modules);
    const long long wsDeltaKiB =
        (static_cast<long long>(final.workingSet) - static_cast<long long>(base.workingSet)) / 1024;

    // The per-device-cycle figure is the one that compares across runs and
    // across adapters. Measured 2026-07-27 on the T4b release UMD
    // (DriverStore, 5,917,696 B): Helios 6.00, WARP 0.00 over 300 cycles.
    const long deviceHandleDelta =
        static_cast<long>(afterDevices.handles) - static_cast<long>(base.handles);
    const double perDeviceCycle =
        deviceCycles ? static_cast<double>(deviceHandleDelta) / deviceCycles : 0.0;

    std::printf("\ndeltas vs post-warmup baseline: handles=%+ld modules=%+ld ws=%+lld KiB\n",
                handleDelta, moduleDelta, wsDeltaKiB);
    std::printf("device phase: handles=%+ld over %u cycles = %.2f handles/device\n",
                deviceHandleDelta, deviceCycles, perDeviceCycle);
    std::printf("failures: device=%u resource=%u\n", deviceFailures, resourceFailures);
    if (dwmPid && dwmAfter.handles) {
        std::printf("dwm deltas: handles=%+ld ws=%+lld KiB\n",
                    static_cast<long>(dwmAfter.handles) - static_cast<long>(dwmBefore.handles),
                    (static_cast<long long>(dwmAfter.workingSet) -
                     static_cast<long long>(dwmBefore.workingSet)) / 1024);
    }

    bool flat = true;
    // Handles and modules are demanded EXACTLY flat: both are the signals a
    // leaked device or a per-cycle module load would move, and neither has a
    // legitimate reason to drift across a create/destroy pair.
    if (handleDelta != 0) {
        std::printf("NOT FLAT: handle count moved by %+ld\n", handleDelta);
        flat = false;
    }
    if (moduleDelta != 0) {
        std::printf("NOT FLAT: module count moved by %+ld\n", moduleDelta);
        flat = false;
    }
    if (wsDeltaKiB > static_cast<long long>(wsToleranceKiB)) {
        std::printf("NOT FLAT: working set grew %+lld KiB (tolerance %u KiB)\n",
                    wsDeltaKiB, wsToleranceKiB);
        flat = false;
    }
    if (deviceFailures || resourceFailures) {
        std::printf("NOT CLEAN: %u device and %u resource cycles failed\n",
                    deviceFailures, resourceFailures);
        flat = false;
    }

    std::printf("%s\n", flat ? "OWNERSHIP SOAK PASS" : "OWNERSHIP SOAK FAIL");
    return flat ? 0 : 2;
}
