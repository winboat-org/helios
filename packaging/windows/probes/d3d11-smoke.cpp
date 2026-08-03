#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <cstdio>
#include <cwchar>

int wmain() {
    IDXGIFactory1 *factory = nullptr;
    HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1), reinterpret_cast<void **>(&factory));
    if (FAILED(hr)) {
        std::fprintf(stderr, "CreateDXGIFactory1 failed: 0x%08lx\n", static_cast<unsigned long>(hr));
        return 1;
    }
    IDXGIAdapter1 *helios = nullptr;
    for (UINT index = 0; ; ++index) {
        IDXGIAdapter1 *adapter = nullptr;
        if (factory->EnumAdapters1(index, &adapter) == DXGI_ERROR_NOT_FOUND) break;
        DXGI_ADAPTER_DESC1 description = {};
        adapter->GetDesc1(&description);
        std::wprintf(L"DXGI adapter %u: %ls\n", index, description.Description);
        if (std::wcsstr(description.Description, L"Helios")) {
            helios = adapter;
            break;
        }
        adapter->Release();
    }
    if (!helios) {
        factory->Release();
        std::fprintf(stderr, "Helios DXGI adapter was not found.\n");
        return 2;
    }
    ID3D11Device *device = nullptr;
    ID3D11DeviceContext *context = nullptr;
    D3D_FEATURE_LEVEL level = D3D_FEATURE_LEVEL_9_1;
    hr = D3D11CreateDevice(helios, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0, nullptr, 0,
                           D3D11_SDK_VERSION, &device, &level, &context);
    if (FAILED(hr)) {
        std::fprintf(stderr, "D3D11CreateDevice on Helios failed: 0x%08lx\n", static_cast<unsigned long>(hr));
        helios->Release();
        factory->Release();
        return 3;
    }
    std::printf("Direct3D 11 device created on Helios; feature level 0x%x.\n", level);
    context->Release();
    device->Release();
    helios->Release();
    factory->Release();
    return 0;
}
