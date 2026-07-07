// Capture the D3D11 debug layer's OutputDebugString while attempting an FL11_0
// create on the Helios adapter, to get d3d11.dll's exact rejection reason.
// Sets up the classic DBWIN_BUFFER shared-memory listener in a background
// thread, then calls D3D11CreateDevice(FL11_0, DEBUG). Run with
// HKLM\SOFTWARE\Helios!FeatureLevel11=1 (UMD claims 11_0).
//
// Build (WinLibs g++):
//   g++ -O2 -o C:\Users\Rupansh\dbwin_flprobe.exe Z:\tools\dbwin_flprobe.cpp \
//       -ld3d11 -ldxgi -ldxguid
#define INITGUID
#include <windows.h>
#include <d3d11.h>
#include <dxgi.h>
#include <cstdio>
#include <cwchar>

struct DbwinBuffer { DWORD pid; char data[4096 - sizeof(DWORD)]; };

static volatile bool g_stop = false;
static HANDLE g_dataReady = nullptr;
static HANDLE g_bufferReady = nullptr;
static DbwinBuffer* g_buf = nullptr;

static DWORD WINAPI listener(LPVOID) {
  while (!g_stop) {
    DWORD w = WaitForSingleObject(g_dataReady, 500);
    if (w == WAIT_TIMEOUT) continue;
    if (w != WAIT_OBJECT_0) break;
    // Copy out, NUL-terminate defensively.
    char line[4096];
    size_t n = 0;
    for (; n < sizeof(g_buf->data) - 1 && g_buf->data[n]; ++n) line[n] = g_buf->data[n];
    line[n] = 0;
    printf("[ODS pid=%lu] %s", (unsigned long)g_buf->pid, line);
    if (n == 0 || line[n ? n - 1 : 0] != '\n') printf("\n");
    fflush(stdout);
    SetEvent(g_bufferReady);
  }
  return 0;
}

int main() {
  // DBWIN setup — become the OutputDebugString listener.
  g_bufferReady = CreateEventA(nullptr, FALSE, TRUE, "DBWIN_BUFFER_READY");
  g_dataReady = CreateEventA(nullptr, FALSE, FALSE, "DBWIN_DATA_READY");
  HANDLE map = CreateFileMappingA(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE,
                                  0, sizeof(DbwinBuffer), "DBWIN_BUFFER");
  if (!g_bufferReady || !g_dataReady || !map) { printf("DBWIN setup failed (already listening?)\n"); }
  else {
    g_buf = (DbwinBuffer*)MapViewOfFile(map, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, sizeof(DbwinBuffer));
    SetEvent(g_bufferReady);
    CreateThread(nullptr, 0, listener, nullptr, 0, nullptr);
  }

  IDXGIFactory1* f = nullptr;
  if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&f))) { printf("factory fail\n"); return 1; }
  IDXGIAdapter1* hel = nullptr; IDXGIAdapter1* a = nullptr;
  for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
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
  Sleep(600); // let the listener flush any queued ODS lines
  printf("--- result: hr=0x%08x got=0x%04x ---\n", (unsigned)hr, (unsigned)got);
  fflush(stdout);

  g_stop = true;
  Sleep(200);
  if (ctx) ctx->Release();
  if (dev) dev->Release();
  hel->Release(); f->Release();
  return 0;
}
