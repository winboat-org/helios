// Helios DXGI enumeration + per-adapter D3D11CreateDevice probe.
// Logs every step to a file (flushed per line) and wraps each device-create in
// SEH so we can see exactly which adapter faults vs. returns a clean HR.
//   cl /EHsc dxgi_enum.cpp /link dxgi.lib d3d11.lib
#include <dxgi1_6.h>
#include <d3d11.h>
#include <windows.h>
#include <cstdio>

static FILE* g = nullptr;
static void L(const char* fmt, ...) {
  va_list ap; va_start(ap, fmt);
  vfprintf(g, fmt, ap); va_end(ap);
  fflush(g);
}

static HRESULT try_create(IDXGIAdapter1* a, DWORD* exc) {
  HRESULT hr = E_FAIL; *exc = 0;
  D3D_FEATURE_LEVEL lv[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0,
                            D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0};
  __try {
    ID3D11Device* dev = nullptr; D3D_FEATURE_LEVEL got{};
    hr = D3D11CreateDevice(a, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0, lv, 4,
                           D3D11_SDK_VERSION, &dev, &got, nullptr);
    if (dev) dev->Release();
  } __except (EXCEPTION_EXECUTE_HANDLER) {
    *exc = GetExceptionCode();
  }
  return hr;
}

int main() {
  g = fopen("C:\\Users\\Rupansh\\helios-probe\\enum.log", "w");
  L("start\n");
  IDXGIFactory1* f = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&f);
  L("CreateDXGIFactory1 hr=0x%08x\n", (unsigned)hr);
  IDXGIAdapter1* a = nullptr;
  for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
    L("EnumAdapters1[%u] ok\n", i);
    DXGI_ADAPTER_DESC1 d{}; HRESULT dh = a->GetDesc1(&d);
    char name[128]{}; wcstombs(name, d.Description, sizeof(name) - 1);
    L("[%u] GetDesc1 hr=0x%08x \"%s\" V=%04x D=%04x Flags=0x%x LUID=%08x:%08x\n",
      i, (unsigned)dh, name, d.VendorId, d.DeviceId, d.Flags,
      (unsigned)d.AdapterLuid.HighPart, (unsigned)d.AdapterLuid.LowPart);
    DWORD oexc = 0; UINT outs = 0;
    __try {
      IDXGIOutput* o = nullptr;
      for (UINT j = 0; a->EnumOutputs(j, &o) != DXGI_ERROR_NOT_FOUND; ++j) { outs++; o->Release(); }
    } __except (EXCEPTION_EXECUTE_HANDLER) { oexc = GetExceptionCode(); }
    if (oexc) L("[%u] EnumOutputs CRASHED exc=0x%08x\n", i, (unsigned)oexc);
    else      L("[%u] outputs=%u\n", i, outs);
    DWORD exc = 0;
    HRESULT chr = try_create(a, &exc);
    if (exc) L("    -> D3D11CreateDevice CRASHED exc=0x%08x\n", (unsigned)exc);
    else     L("    -> D3D11CreateDevice hr=0x%08x\n", (unsigned)chr);
    a->Release(); a = nullptr;
  }
  f->Release();
  L("done\n");
  fclose(g);
  return 0;
}
