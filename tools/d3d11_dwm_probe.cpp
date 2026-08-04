// Helios path-A — DWM-faithful D3D11 device/capability probe.
//
// dwm.exe faults in dwmcore!CD3DDevice::CreateD3D11Device raising DWM error
// 0x889800b0 when DWM is pointed at the Helios render adapter (cross-adapter
// composition for the Looking Glass IndirectKMD). A plain D3D11CreateDevice on
// Helios already returns S_OK (tools/d3d11_devicecreate_probe.cpp). So DWM needs
// a capability the basic device lacks. This probe reproduces what dwmcore does
// for a composition device on a cross-adapter render target and reports exactly
// which step fails:
//   1. D3D11CreateDevice with BGRA_SUPPORT (DWM mandates BGRA), FL 11_1..10_0.
//   2. QI ID3D11Device5 (needed for monitored/shared fences).
//   3. CreateFence(SHARED | SHARED_CROSS_ADAPTER) + CreateSharedHandle.
//   4. Cross-adapter shared Texture2D (SHARED | SHARED_NTHANDLE |
//      SHARED_CROSS_ADAPTER, row-major) + CreateSharedHandle.
//   5. CheckFeatureSupport D3D11_OPTIONS / OPTIONS2 (cross-adapter row-major).
//   6. BGRA (B8G8R8A8) render-target format support.
//
// Build (on win11): cl /EHsc /W4 d3d11_dwm_probe.cpp /link dxgi.lib d3d11.lib

#include <dxgi1_6.h>
#include <d3d11_4.h>
#include <tlhelp32.h>
#include <cstdio>
#include <cwchar>

static const char* boolstr(bool b) { return b ? "YES" : "no"; }

// Both Helios modules are deployed under CONTENT-HASHED file names — the Mesa
// Venus ICD installs as `vulkan_virtio-<sha12>.dll` (tools/install-helios-icd.ps1)
// and the UMD as `helios_umd_<sha16>.dll` (tools/hotplug-helios-umd.ps1) — so a
// hardcoded GetModuleHandleA name can never resolve one. Match the STABLE PREFIX
// against the process's real module list instead.
static HMODULE find_module_by_prefix(const wchar_t* prefix, wchar_t* found, size_t found_cap) {
    if (found && found_cap) found[0] = L'\0';
    HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, GetCurrentProcessId());
    if (snap == INVALID_HANDLE_VALUE) {
        printf("   [module-scan] CreateToolhelp32Snapshot FAILED gle=%lu -- cannot look up \"%ls*\"\n",
               GetLastError(), prefix);
        return nullptr;
    }
    MODULEENTRY32W me{};
    me.dwSize = sizeof(me);
    HMODULE hit = nullptr;
    const size_t n = wcslen(prefix);
    if (Module32FirstW(snap, &me)) {
        do {
            if (_wcsnicmp(me.szModule, prefix, n) == 0) {
                hit = me.hModule;
                // Manual copy: wcsncpy raises MSVC C4996 under /W4, and the
                // secure variants are not portable to the WinLibs g++ used for
                // some of the other probes in this directory.
                if (found && found_cap) {
                    size_t i = 0;
                    for (; i + 1 < found_cap && me.szModule[i] != L'\0'; ++i)
                        found[i] = me.szModule[i];
                    found[i] = L'\0';
                }
                break;
            }
        } while (Module32NextW(snap, &me));
    }
    CloseHandle(snap);
    return hit;
}

static void print_helios_ctx_export() {
    using Fn = unsigned (__cdecl*)();

    // LOUD on every miss: a NULL handle here silently disables the ctx-id
    // readout, which is exactly how this probe spent its life printing
    // module=0000000000000000 and nothing else.
    wchar_t icd[MAX_MODULE_NAME32 + 1];
    HMODULE mod = find_module_by_prefix(L"vulkan_virtio", icd, MAX_MODULE_NAME32 + 1);
    if (!mod) {
        printf("   helios ctx export: UNAVAILABLE -- no loaded module matches \"vulkan_virtio*\";"
               " the Venus ICD is not loaded in this process, so the ctx id cannot be read\n");
    } else {
        printf("   helios ctx export: module=%ls (%p)", icd, (void*)mod);
        Fn fn = reinterpret_cast<Fn>(GetProcAddress(mod, "helios_venus_current_ctx_id"));
        if (!fn)
            printf(" -- UNAVAILABLE: export \"helios_venus_current_ctx_id\" not found (gle=%lu)\n",
                   GetLastError());
        else
            printf(" ctx=%u\n", fn());
    }

    wchar_t umd[MAX_MODULE_NAME32 + 1];
    HMODULE umd_mod = find_module_by_prefix(L"helios_umd", umd, MAX_MODULE_NAME32 + 1);
    if (umd_mod)
        printf("   UMD module: %ls (%p)\n", umd, (void*)umd_mod);
    else
        printf("   UMD module: NONE LOADED -- no module matches \"helios_umd*\" in this process\n");
}

int main() {
    IDXGIFactory1* factory = nullptr;
    HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1), reinterpret_cast<void**>(&factory));
    if (FAILED(hr)) { printf("CreateDXGIFactory1 hr=0x%08x\n", (unsigned)hr); return 1; }

    IDXGIAdapter1* adapter = nullptr;
    IDXGIAdapter1* helios = nullptr;
    for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
        DXGI_ADAPTER_DESC1 desc{};
        adapter->GetDesc1(&desc);
        wprintf(L"[%u] \"%s\" Vendor=0x%04x Device=0x%04x Flags=0x%x\n",
                i, desc.Description, desc.VendorId, desc.DeviceId, desc.Flags);
        if (!helios && wcsstr(desc.Description, L"Helios")) { helios = adapter; helios->AddRef(); }
        adapter->Release(); adapter = nullptr;
    }
    if (!helios) { printf("Helios adapter not found\n"); factory->Release(); return 2; }

    const D3D_FEATURE_LEVEL levels[] = {
        D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0,
    };
    ID3D11Device* device = nullptr;
    ID3D11DeviceContext* context = nullptr;
    D3D_FEATURE_LEVEL fl = (D3D_FEATURE_LEVEL)0;

    printf("\n== 1. D3D11CreateDevice(BGRA_SUPPORT) ==\n"); fflush(stdout);
    hr = D3D11CreateDevice(helios, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                           D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                           levels, _countof(levels), D3D11_SDK_VERSION,
                           &device, &fl, &context);
    printf("   hr=0x%08x featureLevel=0x%04x device=%p\n", (unsigned)hr, (unsigned)fl, (void*)device);
    if (FAILED(hr) || !device) { printf("   -> BGRA device-create FAILED (candidate root cause)\n");
        if (helios) helios->Release(); if (factory) factory->Release(); return 3; }
    print_helios_ctx_export();

    printf("\n== 2. QI ID3D11Device5 ==\n");
    ID3D11Device5* dev5 = nullptr;
    hr = device->QueryInterface(__uuidof(ID3D11Device5), (void**)&dev5);
    printf("   QI ID3D11Device5 hr=0x%08x dev5=%p\n", (unsigned)hr, (void*)dev5);

    if (dev5) {
        printf("\n== 3. CreateFence(SHARED | SHARED_CROSS_ADAPTER) ==\n");
        ID3D11Fence* fence = nullptr;
        hr = dev5->CreateFence(0, (D3D11_FENCE_FLAG)(D3D11_FENCE_FLAG_SHARED | D3D11_FENCE_FLAG_SHARED_CROSS_ADAPTER),
                               __uuidof(ID3D11Fence), (void**)&fence);
        printf("   CreateFence(cross-adapter) hr=0x%08x fence=%p\n", (unsigned)hr, (void*)fence);
        if (fence) {
            HANDLE h = nullptr;
            HRESULT hr2 = fence->CreateSharedHandle(nullptr, GENERIC_ALL, nullptr, &h);
            printf("   Fence CreateSharedHandle hr=0x%08x handle=%p\n", (unsigned)hr2, (void*)h);
            if (h) CloseHandle(h);
            fence->Release();
        } else {
            printf("   -> cross-adapter fence FAILED (candidate root cause)\n");
        }
        // Plain monitored fence (non-cross-adapter) for comparison.
        ID3D11Fence* f2 = nullptr;
        HRESULT hr3 = dev5->CreateFence(0, D3D11_FENCE_FLAG_NONE, __uuidof(ID3D11Fence), (void**)&f2);
        printf("   CreateFence(NONE) hr=0x%08x fence=%p\n", (unsigned)hr3, (void*)f2);
        if (f2) f2->Release();
        dev5->Release();
    }

    printf("\n== 4. Cross-adapter shared Texture2D ==\n");
    {
        D3D11_TEXTURE2D_DESC td{};
        td.Width = 256; td.Height = 256; td.MipLevels = 1; td.ArraySize = 1;
        td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        td.SampleDesc.Count = 1;
        td.Usage = D3D11_USAGE_DEFAULT;
        td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
        td.MiscFlags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX;
        ID3D11Texture2D* tex = nullptr;
        hr = device->CreateTexture2D(&td, nullptr, &tex);
        printf("   shared(NTHANDLE|KEYEDMUTEX) tex hr=0x%08x tex=%p\n", (unsigned)hr, (void*)tex);
        if (tex) { tex->Release(); }

        // Now the true cross-adapter variant (what DWM uses for IDD composition).
        td.MiscFlags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED;
        // Cross-adapter resources must be row-major; some runtimes require the
        // cross-adapter misc bit which D3D11 expresses via the same NTHANDLE path.
        ID3D11Texture2D* tex2 = nullptr;
        HRESULT hr4 = device->CreateTexture2D(&td, nullptr, &tex2);
        printf("   shared(NTHANDLE|SHARED) tex hr=0x%08x tex=%p\n", (unsigned)hr4, (void*)tex2);
        if (tex2) {
            IDXGIResource1* res1 = nullptr;
            if (SUCCEEDED(tex2->QueryInterface(__uuidof(IDXGIResource1), (void**)&res1)) && res1) {
                HANDLE sh = nullptr;
                HRESULT hr5 = res1->CreateSharedHandle(nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE, nullptr, &sh);
                printf("   tex CreateSharedHandle hr=0x%08x handle=%p\n", (unsigned)hr5, (void*)sh);
                if (sh) CloseHandle(sh);
                res1->Release();
            }
            tex2->Release();
        }
    }

    printf("\n== 5. CheckFeatureSupport ==\n");
    {
        D3D11_FEATURE_DATA_D3D11_OPTIONS o{};
        hr = device->CheckFeatureSupport(D3D11_FEATURE_D3D11_OPTIONS, &o, sizeof(o));
        printf("   OPTIONS hr=0x%08x: extResourceSharing=%s clearView=%s\n", (unsigned)hr,
               boolstr(o.ExtendedResourceSharing != 0), boolstr(o.ClearView != 0));
        D3D11_FEATURE_DATA_D3D11_OPTIONS2 o2{};
        hr = device->CheckFeatureSupport(D3D11_FEATURE_D3D11_OPTIONS2, &o2, sizeof(o2));
        printf("   OPTIONS2 hr=0x%08x: UnifiedMemoryArchitecture=%s MapOnDefaultTextures=%s\n",
               (unsigned)hr, boolstr(o2.UnifiedMemoryArchitecture != 0), boolstr(o2.MapOnDefaultTextures != 0));
    }

    printf("\n== 6. BGRA format support ==\n");
    {
        UINT sup = 0;
        hr = device->CheckFormatSupport(DXGI_FORMAT_B8G8R8A8_UNORM, &sup);
        printf("   B8G8R8A8 CheckFormatSupport hr=0x%08x support=0x%08x (RT=%s DISPLAY=%s)\n",
               (unsigned)hr, sup,
               boolstr((sup & D3D11_FORMAT_SUPPORT_RENDER_TARGET) != 0),
               boolstr((sup & D3D11_FORMAT_SUPPORT_DISPLAY) != 0));
    }

    printf("\nprobe done.\n");
    if (context) context->Release();
    if (device) device->Release();
    helios->Release();
    factory->Release();
    return 0;
}
