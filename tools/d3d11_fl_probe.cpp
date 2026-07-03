// Helios path-A — D3D11 feature-level localization probe.
// app-level D3D11CreateDevice on Helios fails DXGI_ERROR_UNSUPPORTED (0x887a0020)
// even though the UMD DDI CreateDevice returns S_OK. Try each feature level
// individually (single-element arrays) + the NULL/default list to localize
// which level the runtime can't satisfy.
//   Build: cl /EHsc /W4 d3d11_fl_probe.cpp /link dxgi.lib d3d11.lib
#include <dxgi1_6.h>
#include <d3d11.h>
#include <cstdio>
#include <cwchar>

static IDXGIAdapter1* find_helios(IDXGIFactory1* f) {
    IDXGIAdapter1* a = nullptr;
    for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
        DXGI_ADAPTER_DESC1 d{}; a->GetDesc1(&d);
        if (wcsstr(d.Description, L"Helios")) return a;
        a->Release();
    }
    return nullptr;
}

static void try_create(IDXGIAdapter1* hel, const char* tag,
                       const D3D_FEATURE_LEVEL* levels, UINT n, UINT flags) {
    ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
    D3D_FEATURE_LEVEL got = (D3D_FEATURE_LEVEL)0;
    HRESULT hr = D3D11CreateDevice(hel, D3D_DRIVER_TYPE_UNKNOWN, nullptr, flags,
                                   levels, n, D3D11_SDK_VERSION, &dev, &got, &ctx);
    printf("%-28s hr=0x%08x got=0x%04x dev=%p\n", tag, (unsigned)hr, (unsigned)got, (void*)dev);
    fflush(stdout);
    if (ctx) ctx->Release();
    if (dev) dev->Release();
}

int main() {
    IDXGIFactory1* f = nullptr;
    if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&f))) return 1;
    IDXGIAdapter1* hel = find_helios(f);
    if (!hel) { printf("no Helios\n"); return 2; }

    const D3D_FEATURE_LEVEL l111[] = { D3D_FEATURE_LEVEL_11_1 };
    const D3D_FEATURE_LEVEL l110[] = { D3D_FEATURE_LEVEL_11_0 };
    const D3D_FEATURE_LEVEL l101[] = { D3D_FEATURE_LEVEL_10_1 };
    const D3D_FEATURE_LEVEL l100[] = { D3D_FEATURE_LEVEL_10_0 };
    const D3D_FEATURE_LEVEL l91[]  = { D3D_FEATURE_LEVEL_9_1 };

    try_create(hel, "FL_11_1 only",        l111, 1, 0);
    try_create(hel, "FL_11_0 only",        l110, 1, 0);
    try_create(hel, "FL_10_1 only",        l101, 1, 0);
    try_create(hel, "FL_10_0 only",        l100, 1, 0);
    try_create(hel, "FL_9_1 only",         l91,  1, 0);
    try_create(hel, "default (NULL levels)", nullptr, 0, 0);

    hel->Release(); f->Release();
    return 0;
}
