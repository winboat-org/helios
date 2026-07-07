// D3D11 feature-level probe WITH the debug layer + DXGI InfoQueue dump, to
// capture WHY the runtime refuses an FL11_0 device on the Helios adapter (it
// CloseAdapters right after GetCaps(3DPIPELINESUPPORT=11_0), before CreateDevice).
// Run with HKLM\SOFTWARE\Helios!FeatureLevel11=1 so the UMD advertises 11_0.
//
// Build (WinLibs g++):
//   g++ -O2 -o C:\Users\Rupansh\d3d11_fl_debug_probe.exe Z:\tools\d3d11_fl_debug_probe.cpp \
//       -ld3d11 -ldxgi -ldxguid
#define INITGUID
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_3.h>
#include <dxgidebug.h>
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

static void dump_dxgi_infoqueue() {
  typedef HRESULT (WINAPI *PFN)(REFIID, void**);
  HMODULE dxgidebug = LoadLibraryA("dxgidebug.dll");
  if (!dxgidebug) { printf("  [no dxgidebug.dll]\n"); return; }
  PFN getdbg = (PFN)GetProcAddress(dxgidebug, "DXGIGetDebugInterface1");
  if (!getdbg) { printf("  [no DXGIGetDebugInterface1]\n"); return; }
  IDXGIInfoQueue* iq = nullptr;
  if (FAILED(getdbg(IID_PPV_ARGS(&iq))) || !iq) { printf("  [no IDXGIInfoQueue]\n"); return; }
  UINT64 n = iq->GetNumStoredMessages(DXGI_DEBUG_ALL);
  printf("  DXGI InfoQueue: %llu stored messages\n", (unsigned long long)n);
  for (UINT64 i = 0; i < n; ++i) {
    SIZE_T len = 0;
    iq->GetMessage(DXGI_DEBUG_ALL, i, nullptr, &len);
    if (!len) continue;
    DXGI_INFO_QUEUE_MESSAGE* m = (DXGI_INFO_QUEUE_MESSAGE*)malloc(len);
    if (iq->GetMessage(DXGI_DEBUG_ALL, i, m, &len) == S_OK) {
      printf("    [sev=%d id=%d] %.*s\n", (int)m->Severity, (int)m->ID,
             (int)m->DescriptionByteLength, m->pDescription);
    }
    free(m);
  }
  iq->Release();
}

int main() {
  // Turn on DXGI debug-message storage before creating anything.
  IDXGIFactory1* f = nullptr;
  if (FAILED(CreateDXGIFactory2(DXGI_CREATE_FACTORY_DEBUG, IID_PPV_ARGS(&f)))) {
    if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&f)))) { printf("factory fail\n"); return 1; }
  }
  IDXGIAdapter1* hel = find_helios(f);
  if (!hel) { printf("no Helios\n"); return 2; }

  const D3D_FEATURE_LEVEL l110[] = { D3D_FEATURE_LEVEL_11_0 };
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  D3D_FEATURE_LEVEL got = (D3D_FEATURE_LEVEL)0;

  HRESULT hr = D3D11CreateDevice(hel, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                                 D3D11_CREATE_DEVICE_DEBUG, l110, 1,
                                 D3D11_SDK_VERSION, &dev, &got, &ctx);
  printf("D3D11CreateDevice(FL11_0, DEBUG): hr=0x%08x got=0x%04x\n", (unsigned)hr, (unsigned)got);
  if (hr == DXGI_ERROR_SDK_COMPONENT_MISSING)
    printf("  (debug layer not installed; retrying without DEBUG for infoqueue)\n");
  dump_dxgi_infoqueue();

  // Retry without the DEBUG flag so we still see the plain failure code, and to
  // let the DXGI infoqueue accumulate whatever the runtime records for it.
  if (dev) { dev->Release(); dev = nullptr; }
  if (ctx) { ctx->Release(); ctx = nullptr; }
  hr = D3D11CreateDevice(hel, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0, l110, 1,
                         D3D11_SDK_VERSION, &dev, &got, &ctx);
  printf("D3D11CreateDevice(FL11_0, no-debug): hr=0x%08x got=0x%04x\n", (unsigned)hr, (unsigned)got);
  dump_dxgi_infoqueue();

  if (ctx) ctx->Release();
  if (dev) dev->Release();
  hel->Release(); f->Release();
  return 0;
}
