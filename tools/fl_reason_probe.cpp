// Retrieve d3d11.dll's exact reason for refusing an FL11_0 device on Helios,
// via the global DXGI InfoQueue (works even though CreateDevice fails and no
// ID3D11InfoQueue exists). DXGIGetDebugInterface1 is exported from dxgi.dll
// (NOT dxgidebug.dll). Run with HKLM\SOFTWARE\Helios!FeatureLevel11=1.
//
// Build (WinLibs g++):
//   g++ -O2 -o C:\Users\Rupansh\fl_reason_probe.exe Z:\tools\fl_reason_probe.cpp \
//       -ld3d11 -ldxgi -ldxguid
#define INITGUID
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_3.h>
#include <dxgidebug.h>
#include <cstdio>
#include <cwchar>

typedef HRESULT (WINAPI *PFN_DXGIGetDebugInterface1)(UINT, REFIID, void**);

static void dump(IDXGIInfoQueue* iq, const char* tag) {
  if (!iq) { printf("  [%s: no infoqueue]\n", tag); return; }
  UINT64 n = iq->GetNumStoredMessages(DXGI_DEBUG_ALL);
  printf("  [%s] %llu messages\n", tag, (unsigned long long)n);
  for (UINT64 i = 0; i < n; ++i) {
    SIZE_T len = 0;
    iq->GetMessageA(DXGI_DEBUG_ALL, i, nullptr, &len);
    if (!len) continue;
    DXGI_INFO_QUEUE_MESSAGE* m = (DXGI_INFO_QUEUE_MESSAGE*)malloc(len);
    if (iq->GetMessageA(DXGI_DEBUG_ALL, i, m, &len) == S_OK)
      printf("    sev=%d cat=%d id=%d: %.*s\n", (int)m->Severity, (int)m->Category,
             (int)m->ID, (int)m->DescriptionByteLength, m->pDescription);
    free(m);
  }
}

int main() {
  HMODULE dxgi = LoadLibraryA("dxgi.dll");
  PFN_DXGIGetDebugInterface1 pGet =
      dxgi ? (PFN_DXGIGetDebugInterface1)GetProcAddress(dxgi, "DXGIGetDebugInterface1") : nullptr;
  IDXGIInfoQueue* iq = nullptr;
  if (pGet) {
    if (SUCCEEDED(pGet(0, IID_PPV_ARGS(&iq)))) {
      iq->SetMuteDebugOutput(DXGI_DEBUG_ALL, FALSE);
      iq->ClearStoredMessages(DXGI_DEBUG_ALL);
    }
  } else {
    printf("DXGIGetDebugInterface1 unavailable\n");
  }

  IDXGIFactory1* f = nullptr;
  if (FAILED(CreateDXGIFactory2(DXGI_CREATE_FACTORY_DEBUG, IID_PPV_ARGS(&f))))
    CreateDXGIFactory1(IID_PPV_ARGS(&f));
  IDXGIAdapter1* hel = nullptr; IDXGIAdapter1* a = nullptr;
  for (UINT i = 0; f && f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 d{}; a->GetDesc1(&d);
    if (wcsstr(d.Description, L"Helios")) { hel = a; break; }
    a->Release();
  }
  if (!hel) { printf("no Helios\n"); return 2; }

  printf("--- D3D11CreateDevice(FL11_0, DEBUG) ---\n"); fflush(stdout);
  const D3D_FEATURE_LEVEL l110[] = { D3D_FEATURE_LEVEL_11_0 };
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  D3D_FEATURE_LEVEL got = (D3D_FEATURE_LEVEL)0;
  HRESULT hr = D3D11CreateDevice(hel, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                                 D3D11_CREATE_DEVICE_DEBUG, l110, 1,
                                 D3D11_SDK_VERSION, &dev, &got, &ctx);
  printf("hr=0x%08x got=0x%04x\n", (unsigned)hr, (unsigned)got);
  dump(iq, "after FL11_0 DEBUG");

  // Also try the whole descending list, to catch a message about the max level.
  if (iq) iq->ClearStoredMessages(DXGI_DEBUG_ALL);
  const D3D_FEATURE_LEVEL arr[] = {
    D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_9_1,
  };
  if (dev) { dev->Release(); dev = nullptr; }
  if (ctx) { ctx->Release(); ctx = nullptr; }
  hr = D3D11CreateDevice(hel, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                         D3D11_CREATE_DEVICE_DEBUG, arr, 5, D3D11_SDK_VERSION,
                         &dev, &got, &ctx);
  printf("--- descending list: hr=0x%08x got=0x%04x ---\n", (unsigned)hr, (unsigned)got);
  dump(iq, "after descending DEBUG");

  if (ctx) ctx->Release();
  if (dev) dev->Release();
  if (hel) hel->Release();
  if (f) f->Release();
  if (iq) iq->Release();
  return 0;
}
