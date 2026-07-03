// Helios path-A — D3D11CreateDevice trace probe (loop + trigger-file gated).
// Waits for C:\Windows\Temp\helios_go.txt, then calls D3D11CreateDevice in a loop
// (30 iters, 3s apart). The first call pages in d3d11!D3D11CreateDevice; the
// debugger can then set a software breakpoint and catch a later iteration. Default
// (NULL) feature-level list. Build: cl /EHsc /W4 d3d11_trace_probe.cpp /link dxgi.lib d3d11.lib
#include <dxgi1_6.h>
#include <d3d11.h>
#include <cstdio>
#include <cwchar>
#include <windows.h>

int main() {
    printf("[trace] pid=%lu waiting for C:\\Windows\\Temp\\helios_go.txt\n", GetCurrentProcessId());
    fflush(stdout);

    IDXGIFactory1* f = nullptr;
    if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&f))) return 1;

    while (GetFileAttributesA("C:\\Windows\\Temp\\helios_go.txt") == INVALID_FILE_ATTRIBUTES) {
        Sleep(200);
    }

    for (int iter = 0; iter < 30; ++iter) {
        IDXGIAdapter1* hel = nullptr; IDXGIAdapter1* a = nullptr;
        for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
            DXGI_ADAPTER_DESC1 d{}; a->GetDesc1(&d);
            if (!hel && wcsstr(d.Description, L"Helios")) { hel = a; hel->AddRef(); }
            a->Release();
        }
        if (!hel) { printf("[trace] no Helios\n"); Sleep(3000); continue; }

        ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
        D3D_FEATURE_LEVEL got = (D3D_FEATURE_LEVEL)0;
        HRESULT hr = D3D11CreateDevice(hel, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0,
                                       nullptr, 0, D3D11_SDK_VERSION, &dev, &got, &ctx);
        printf("[trace] iter=%d hr=0x%08x got=0x%04x dev=%p\n", iter, (unsigned)hr, (unsigned)got, (void*)dev);
        fflush(stdout);
        if (ctx) ctx->Release();
        if (dev) dev->Release();
        hel->Release();
        Sleep(3000);
    }
    f->Release();
    return 0;
}
