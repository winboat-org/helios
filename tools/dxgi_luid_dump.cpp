// Minimal DXGI adapter -> {name, vendor, device, LUID} dumper. Authoritative
// adapter identity for matching IddCx render-adapter LUIDs to Helios/WARP/etc.
#include <dxgi1_6.h>
#include <stdio.h>
#pragma comment(lib, "dxgi.lib")
int main() {
    IDXGIFactory1* f = nullptr;
    if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&f))) { printf("CreateDXGIFactory1 failed\n"); return 1; }
    IDXGIAdapter1* a = nullptr;
    for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
        DXGI_ADAPTER_DESC1 d; a->GetDesc1(&d);
        printf("adapter[%u] luid=%08lx:%08lx vendor=0x%04x device=0x%04x flags=0x%x name=%ls\n",
               i, (unsigned long)d.AdapterLuid.HighPart, (unsigned long)d.AdapterLuid.LowPart,
               d.VendorId, d.DeviceId, d.Flags, d.Description);
        a->Release(); a = nullptr;
    }
    f->Release();
    return 0;
}
