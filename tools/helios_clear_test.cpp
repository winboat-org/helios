// Offscreen clear+readback test for the Helios D3D11 UMD (Gate 5b Milestone 2).
// Selects the Helios adapter, creates a render-target texture, clears it to a
// known color, copies to a staging texture, maps and reads back pixel 0. No
// present — exercises the resource/view/clear/copy/map DDI forwarders end to end.
//
// Build: cl /EHsc /W3 helios_clear_test.cpp /link d3d11.lib dxgi.lib

#include <d3d11.h>
#include <dxgi.h>
#include <cstdio>
#include <cmath>

static const char* fl_name(D3D_FEATURE_LEVEL fl) {
  switch (fl) {
    case D3D_FEATURE_LEVEL_11_1: return "11_1";
    case D3D_FEATURE_LEVEL_11_0: return "11_0";
    case D3D_FEATURE_LEVEL_10_1: return "10_1";
    case D3D_FEATURE_LEVEL_10_0: return "10_0";
    default: return "other";
  }
}

int main() {
  IDXGIFactory* factory = nullptr;
  if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory), (void**)&factory))) {
    printf("CreateDXGIFactory1 failed\n"); return 1;
  }
  // Multiple Helios instances can be enumerated (the raw adapter plus one or
  // more indirect-display pairing instances, some stale) — try each until a
  // device creates.
  D3D_FEATURE_LEVEL got = {};
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  D3D_FEATURE_LEVEL want[] = { D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1,
                               D3D_FEATURE_LEVEL_10_0 };
  HRESULT hr = E_FAIL;
  IDXGIAdapter* adapter = nullptr;
  for (UINT i = 0; factory->EnumAdapters(i, &adapter) != DXGI_ERROR_NOT_FOUND; i++) {
    DXGI_ADAPTER_DESC d; adapter->GetDesc(&d);
    if (d.VendorId != 0x1af4) { adapter->Release(); continue; }
    hr = D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0,
        want, 3, D3D11_SDK_VERSION, &dev, &got, &ctx);
    printf("D3D11CreateDevice[adapter %u luid %08x:%08x] hr=0x%08x fl=%s dev=%p\n",
        i, (unsigned)d.AdapterLuid.HighPart, (unsigned)d.AdapterLuid.LowPart,
        hr, fl_name(got), (void*)dev);
    adapter->Release();
    if (SUCCEEDED(hr)) break;
  }
  if (FAILED(hr)) { printf("no Helios adapter creates a device\n"); return 2; }

  // Render-target texture.
  D3D11_TEXTURE2D_DESC rtd = {};
  rtd.Width = 64; rtd.Height = 64; rtd.MipLevels = 1; rtd.ArraySize = 1;
  rtd.Format = DXGI_FORMAT_B8G8R8A8_UNORM; rtd.SampleDesc.Count = 1;
  rtd.Usage = D3D11_USAGE_DEFAULT; rtd.BindFlags = D3D11_BIND_RENDER_TARGET;
  ID3D11Texture2D* rt = nullptr;
  hr = dev->CreateTexture2D(&rtd, nullptr, &rt);
  printf("CreateTexture2D(RT) hr=0x%08x tex=%p\n", hr, (void*)rt);
  if (FAILED(hr)) return 3;

  ID3D11RenderTargetView* rtv = nullptr;
  hr = dev->CreateRenderTargetView(rt, nullptr, &rtv);
  printf("CreateRenderTargetView hr=0x%08x rtv=%p\n", hr, (void*)rtv);
  if (FAILED(hr)) return 4;

  const float color[4] = { 0.25f, 0.5f, 0.75f, 1.0f }; // RGBA
  ctx->ClearRenderTargetView(rtv, color);

  // Staging texture for CPU readback.
  D3D11_TEXTURE2D_DESC sd = rtd;
  sd.Usage = D3D11_USAGE_STAGING; sd.BindFlags = 0;
  sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D* staging = nullptr;
  hr = dev->CreateTexture2D(&sd, nullptr, &staging);
  printf("CreateTexture2D(staging) hr=0x%08x tex=%p\n", hr, (void*)staging);
  if (FAILED(hr)) return 5;

  ctx->CopyResource(staging, rt);
  ctx->Flush();

  D3D11_MAPPED_SUBRESOURCE map = {};
  hr = ctx->Map(staging, 0, D3D11_MAP_READ, 0, &map);
  printf("Map(staging) hr=0x%08x pData=%p pitch=%u\n", hr, map.pData, map.RowPitch);
  if (FAILED(hr)) return 6;

  unsigned char* px = (unsigned char*)map.pData; // BGRA
  printf("readback BGRA = %u %u %u %u\n", px[0], px[1], px[2], px[3]);
  // Expected (BGRA from RGBA 0.25,0.5,0.75,1.0): B=191 G=128 R=64 A=255
  int ok = (abs(px[0]-191)<=2 && abs(px[1]-128)<=2 && abs(px[2]-64)<=2 && px[3]==255);
  printf("RESULT: %s\n", ok ? "PASS (cleared color read back)" : "FAIL");
  ctx->Unmap(staging, 0);
  return ok ? 0 : 7;
}
