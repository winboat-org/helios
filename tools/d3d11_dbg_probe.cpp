// Helios path-A — D3D11 debug-layer device-create probe.
// Run under cdb so the D3D11 debug layer's OutputDebugString rejection reason is
// captured: cdb -c "g;q" d3d11_dbg_probe.exe
//   Build: cl /EHsc /W4 d3d11_dbg_probe.cpp /link dxgi.lib d3d11.lib
#include <dxgi1_6.h>
#include <d3d11.h>
#include <cstdio>
#include <cwchar>

int main() {
    IDXGIFactory1* f = nullptr;
    if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&f))) return 1;
    IDXGIAdapter1* hel = nullptr; IDXGIAdapter1* a = nullptr;
    for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
        DXGI_ADAPTER_DESC1 d{}; a->GetDesc1(&d);
        if (wcsstr(d.Description, L"Helios")) { hel = a; break; }
        a->Release();
    }
    if (!hel) { printf("no Helios\n"); return 2; }

    ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
    D3D_FEATURE_LEVEL got = (D3D_FEATURE_LEVEL)0;

    // Try WITH the debug layer first (surfaces the reject reason via OutputDebugString).
    HRESULT hr = D3D11CreateDevice(hel, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                                   D3D11_CREATE_DEVICE_DEBUG, nullptr, 0,
                                   D3D11_SDK_VERSION, &dev, &got, &ctx);
    printf("DEBUG-layer create hr=0x%08x got=0x%04x dev=%p\n", (unsigned)hr, (unsigned)got, (void*)dev);
    fflush(stdout);
    if (hr == DXGI_ERROR_SDK_COMPONENT_MISSING) {
        printf("(debug layer not installed)\n");
    }
    if (ctx) { ctx->Release(); ctx = nullptr; }
    if (dev) { dev->Release(); dev = nullptr; }

    hel->Release(); f->Release();
    return 0;
}
