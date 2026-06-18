// Helios Gate 4 — D3D11 device-create probe.
//
// Normal DXGI/dxdiag enumeration only drives the Helios UMD through
// OpenAdapter -> GetCaps -> GetSupportedVersions -> CloseAdapter; it never
// reaches the CreateDevice DDI. This probe deliberately enumerates DXGI
// adapters, selects the Helios render-only adapter by description, and calls
// D3D11CreateDevice against it so the runtime loads helios_umd.dll and invokes
// its CreateDevice DDI. It prints the negotiated feature level / HRESULT so the
// honest failure (E_NOTIMPL, surfaced as a clean D3D error, NOT a crash) is
// observable, and pairs with C:\Windows\Temp\helios_umd.log which now records
// the exact Interface/Version/Flags the runtime requested.
//
// Build (on win11): see tools/d3d11_devicecreate_probe.build.ps1
//   cl /EHsc /W4 d3d11_devicecreate_probe.cpp /link dxgi.lib d3d11.lib

#include <dxgi1_6.h>
#include <d3d11.h>
#include <cstdio>
#include <cwchar>

int main() {
    IDXGIFactory1* factory = nullptr;
    HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1), reinterpret_cast<void**>(&factory));
    if (FAILED(hr)) {
        printf("CreateDXGIFactory1 failed hr=0x%08x\n", static_cast<unsigned>(hr));
        return 1;
    }

    IDXGIAdapter1* adapter = nullptr;
    IDXGIAdapter1* helios = nullptr;
    for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
        DXGI_ADAPTER_DESC1 desc{};
        adapter->GetDesc1(&desc);
        wprintf(L"[%u] \"%s\" Vendor=0x%04x Device=0x%04x SubSys=0x%08x Rev=0x%02x Flags=0x%x\n",
                i, desc.Description, desc.VendorId, desc.DeviceId,
                desc.SubSysId, desc.Revision, desc.Flags);
        if (wcsstr(desc.Description, L"Helios") != nullptr) {
            helios = adapter;
            helios->AddRef();
        }
        adapter->Release();
        adapter = nullptr;
    }

    if (helios == nullptr) {
        printf("Helios adapter not found in DXGI enumeration\n");
        factory->Release();
        return 2;
    }

    printf("Selecting Helios adapter; calling D3D11CreateDevice (DRIVER_TYPE_UNKNOWN)...\n");
    fflush(stdout);

    const D3D_FEATURE_LEVEL levels[] = {
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    };
    ID3D11Device* device = nullptr;
    ID3D11DeviceContext* context = nullptr;
    D3D_FEATURE_LEVEL achieved = static_cast<D3D_FEATURE_LEVEL>(0);

    hr = D3D11CreateDevice(
        helios,
        D3D_DRIVER_TYPE_UNKNOWN,  // required when an explicit adapter is supplied
        nullptr,
        0,
        levels,
        _countof(levels),
        D3D11_SDK_VERSION,
        &device,
        &achieved,
        &context);

    printf("D3D11CreateDevice hr=0x%08x featureLevel=0x%04x device=%p context=%p\n",
           static_cast<unsigned>(hr), static_cast<unsigned>(achieved),
           static_cast<void*>(device), static_cast<void*>(context));

    if (context != nullptr) context->Release();
    if (device != nullptr) device->Release();
    helios->Release();
    factory->Release();
    return 0;
}
