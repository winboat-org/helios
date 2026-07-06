// dcomp_present_probe — road-4 feasibility probe (2026-07-06, WS2 present research).
//
// Question: does DXGI + dwm accept a CreateSwapChainForComposition flip-model
// swapchain whose presenting device is a D3D11 device on the HELIOS adapter,
// bound to an HWND via DirectComposition (DCompositionCreateDevice(NULL) +
// CreateTargetForHwnd + SetContent + Commit)?
//
// This is the exact recipe mesa's dzn WSI uses (wsi_common_win32.cpp dxgi
// branch); if it works here, the Vulkan ICD can ride a D3D11 vehicle device
// for token minting while doing the pixel copy venus-side (zero CPU bytes).
//
// Pure D3D11/DXGI/dcomp — no Vulkan, no ICD changes. Renders an animated
// color gradient via ClearRenderTargetView + Present(0). Success criteria:
// swapchain + dcomp calls all S_OK AND the window shows the animation in a
// paintcap (dwm composes it).
//
// Build (VM, WinLibs g++):
//   g++ -O2 -municode -o dcomp_present_probe.exe Z:\tools\dcomp_present_probe.cpp \
//       -ld3d11 -ldxgi -ldcomp -lgdi32 -luser32
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_4.h>
#include <dcomp.h>
#include <stdio.h>
#include <math.h>

#define CHECK(expr)                                                          \
   do {                                                                      \
      HRESULT _hr = (expr);                                                  \
      printf("%-55s hr=0x%08lx\n", #expr, (unsigned long)_hr);               \
      fflush(stdout);                                                        \
      if (FAILED(_hr))                                                       \
         return 10;                                                          \
   } while (0)

static LRESULT CALLBACK wnd_proc(HWND h, UINT m, WPARAM w, LPARAM l)
{
   if (m == WM_DESTROY) {
      PostQuitMessage(0);
      return 0;
   }
   return DefWindowProcW(h, m, w, l);
}

int wmain(int argc, wchar_t **argv)
{
   const UINT width = 480, height = 360;
   int seconds = argc > 1 ? _wtoi(argv[1]) : 20;

   WNDCLASSW wc = {};
   wc.lpfnWndProc = wnd_proc;
   wc.hInstance = GetModuleHandleW(NULL);
   wc.lpszClassName = L"HeliosDcompProbe";
   RegisterClassW(&wc);
   HWND hwnd = CreateWindowExW(0, wc.lpszClassName, L"Helios dcomp probe",
                               WS_OVERLAPPEDWINDOW | WS_VISIBLE, 1390, 560,
                               width, height, NULL, NULL, wc.hInstance, NULL);
   if (!hwnd) {
      printf("CreateWindowExW failed %lu\n", GetLastError());
      return 2;
   }

   // Device on the DEFAULT adapter = Helios (the only render adapter).
   ID3D11Device *dev = NULL;
   ID3D11DeviceContext *ctx = NULL;
   D3D_FEATURE_LEVEL fl;
   CHECK(D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL,
                           D3D11_CREATE_DEVICE_BGRA_SUPPORT, NULL, 0,
                           D3D11_SDK_VERSION, &dev, &fl, &ctx));
   printf("feature level 0x%x\n", fl);

   IDXGIDevice *dxgi_dev = NULL;
   CHECK(dev->QueryInterface(__uuidof(IDXGIDevice), (void **)&dxgi_dev));
   IDXGIAdapter *adapter = NULL;
   CHECK(dxgi_dev->GetAdapter(&adapter));
   DXGI_ADAPTER_DESC ad = {};
   adapter->GetDesc(&ad);
   printf("adapter: %ls\n", ad.Description);

   IDXGIFactory4 *factory = NULL;
   CHECK(CreateDXGIFactory2(0, __uuidof(IDXGIFactory4), (void **)&factory));

   DXGI_SWAP_CHAIN_DESC1 desc = {};
   desc.Width = width;
   desc.Height = height;
   desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
   desc.SampleDesc.Count = 1;
   desc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
   desc.BufferCount = 3;
   desc.Scaling = DXGI_SCALING_STRETCH;
   desc.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
   desc.AlphaMode = DXGI_ALPHA_MODE_IGNORE;

   IDXGISwapChain1 *sc1 = NULL;
   CHECK(factory->CreateSwapChainForComposition(dev, &desc, NULL, &sc1));
   IDXGISwapChain3 *sc = NULL;
   CHECK(sc1->QueryInterface(__uuidof(IDXGISwapChain3), (void **)&sc));

   IDCompositionDevice *dcomp = NULL;
   CHECK(DCompositionCreateDevice(NULL, __uuidof(IDCompositionDevice),
                                  (void **)&dcomp));
   IDCompositionTarget *target = NULL;
   CHECK(dcomp->CreateTargetForHwnd(hwnd, FALSE, &target));
   IDCompositionVisual *visual = NULL;
   CHECK(dcomp->CreateVisual(&visual));
   CHECK(visual->SetContent(sc));
   CHECK(target->SetRoot(visual));
   CHECK(dcomp->Commit());

   printf("setup OK; presenting for %d s...\n", seconds);
   fflush(stdout);

   ULONGLONG t0 = GetTickCount64();
   unsigned frame = 0;
   HRESULT last_present = S_OK;
   while (GetTickCount64() - t0 < (ULONGLONG)seconds * 1000) {
      MSG msg;
      while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) {
         TranslateMessage(&msg);
         DispatchMessageW(&msg);
      }

      ID3D11Texture2D *buf = NULL;
      UINT idx = sc->GetCurrentBackBufferIndex();
      if (FAILED(sc->GetBuffer(idx, __uuidof(ID3D11Texture2D), (void **)&buf)))
         break;
      ID3D11RenderTargetView *rtv = NULL;
      if (FAILED(dev->CreateRenderTargetView(buf, NULL, &rtv))) {
         buf->Release();
         break;
      }
      float t = frame * 0.02f;
      float color[4] = { 0.5f + 0.5f * sinf(t), 0.5f + 0.5f * sinf(t + 2.1f),
                         0.5f + 0.5f * sinf(t + 4.2f), 1.0f };
      ctx->ClearRenderTargetView(rtv, color);
      rtv->Release();
      buf->Release();

      last_present = sc->Present(0, 0);
      if (FAILED(last_present)) {
         printf("Present FAILED at frame %u hr=0x%08lx\n", frame,
                (unsigned long)last_present);
         break;
      }
      frame++;
      Sleep(15);
   }

   printf("frames presented: %u, last Present hr=0x%08lx\n", frame,
          (unsigned long)last_present);
   printf(SUCCEEDED(last_present) && frame > 60 ? "PROBE PASS\n"
                                                : "PROBE FAIL\n");
   return SUCCEEDED(last_present) && frame > 60 ? 0 : 1;
}
