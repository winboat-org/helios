// d3d11_shared_content_probe.cpp — the discriminating experiment for the
// 2026-07-03 black-IDD-frame class: does CONTENT survive a cross-device
// shared-surface alias on Helios?
//
// Device 1 creates a shared (NTHANDLE) BGRA render target, clears it to a
// known color and flushes. Device 2 opens the shared handle, copies the
// texture to a staging resource and reads it back. Same-device
// clear+readback already passes (helios_clear_test), so a zero/garbage
// readback here isolates the venus two-images-one-memory aliasing path —
// exactly the dwm(render) → IDD(acquire+copy) shape that currently yields
// all-zero frames.
//
// Build (VM, vcvars64):
//   cl /EHsc /W4 Z:\tools\d3d11_shared_content_probe.cpp /link dxgi.lib d3d11.lib
#include <d3d11_4.h>
#include <dxgi1_6.h>
#include <cstdio>
#include <cwchar>

static IDXGIAdapter1* find_helios(IDXGIFactory1* factory) {
  IDXGIAdapter1* adapter = nullptr;
  for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 desc{};
    adapter->GetDesc1(&desc);
    wprintf(L"[%u] \"%s\" Vendor=0x%04x Device=0x%04x Flags=0x%x\n",
            i, desc.Description, desc.VendorId, desc.DeviceId, desc.Flags);
    if (wcsstr(desc.Description, L"Helios"))
      return adapter;
    adapter->Release();
    adapter = nullptr;
  }
  return nullptr;
}

static HRESULT create_device(IDXGIAdapter1* adapter, ID3D11Device** device,
                             ID3D11DeviceContext** ctx) {
  const D3D_FEATURE_LEVEL levels[] = {
      D3D_FEATURE_LEVEL_11_1,
      D3D_FEATURE_LEVEL_11_0,
      D3D_FEATURE_LEVEL_10_1,
      D3D_FEATURE_LEVEL_10_0,
  };
  D3D_FEATURE_LEVEL fl{};
  return D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                           D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                           levels, _countof(levels), D3D11_SDK_VERSION,
                           device, &fl, ctx);
}

int main() {
  IDXGIFactory1* factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1),
                                  reinterpret_cast<void**>(&factory));
  if (FAILED(hr)) { printf("CreateDXGIFactory1 hr=0x%08x\n", (unsigned)hr); return 1; }

  IDXGIAdapter1* helios = find_helios(factory);
  if (!helios) { printf("Helios adapter not found\n"); return 2; }

  ID3D11Device* dev1 = nullptr;  ID3D11DeviceContext* ctx1 = nullptr;
  ID3D11Device* dev2 = nullptr;  ID3D11DeviceContext* ctx2 = nullptr;
  hr = create_device(helios, &dev1, &ctx1);
  printf("dev1 create hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr)) return 3;
  hr = create_device(helios, &dev2, &ctx2);
  printf("dev2 create hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr)) return 4;

  D3D11_TEXTURE2D_DESC td{};
  td.Width = 256;
  td.Height = 256;
  td.MipLevels = 1;
  td.ArraySize = 1;
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  td.SampleDesc.Count = 1;
  td.Usage = D3D11_USAGE_DEFAULT;
  td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
  td.MiscFlags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED;

  ID3D11Texture2D* tex = nullptr;
  hr = dev1->CreateTexture2D(&td, nullptr, &tex);
  printf("CreateTexture2D(shared RT) hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr)) return 5;

  // Clear to a distinctive color on device 1 and flush so the venus
  // submission retires before device 2 reads.
  ID3D11RenderTargetView* rtv = nullptr;
  hr = dev1->CreateRenderTargetView(tex, nullptr, &rtv);
  printf("CreateRenderTargetView hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr)) return 6;
  const float color[4] = { 0.25f, 0.50f, 0.75f, 1.00f }; // B=191 G=127 R=63 (BGRA bytes: 191,127,63,255? note float->byte)
  ctx1->ClearRenderTargetView(rtv, color);
  ctx1->Flush();

  IDXGIResource1* res1 = nullptr;
  hr = tex->QueryInterface(__uuidof(IDXGIResource1), reinterpret_cast<void**>(&res1));
  if (FAILED(hr)) return 7;
  HANDLE handle = nullptr;
  hr = res1->CreateSharedHandle(nullptr,
                                DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                nullptr, &handle);
  printf("CreateSharedHandle hr=0x%08x handle=%p\n", (unsigned)hr, handle);
  if (FAILED(hr) || !handle) return 8;

  ID3D11Device1* dev2_1 = nullptr;
  hr = dev2->QueryInterface(__uuidof(ID3D11Device1), reinterpret_cast<void**>(&dev2_1));
  if (FAILED(hr)) return 9;
  ID3D11Texture2D* opened = nullptr;
  hr = dev2_1->OpenSharedResource1(handle, __uuidof(ID3D11Texture2D),
                                   reinterpret_cast<void**>(&opened));
  printf("OpenSharedResource1 hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr) || !opened) return 10;

  // Readback helper: copy `src` to a fresh staging texture on (dev,ctx),
  // map and report the center pixel.
  auto readback = [&](ID3D11Device* dev, ID3D11DeviceContext* ctx,
                      ID3D11Texture2D* src, const char* label) -> unsigned {
    D3D11_TEXTURE2D_DESC sd = td;
    sd.BindFlags = 0;
    sd.MiscFlags = 0;
    sd.Usage = D3D11_USAGE_STAGING;
    sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    ID3D11Texture2D* staging = nullptr;
    HRESULT h = dev->CreateTexture2D(&sd, nullptr, &staging);
    if (FAILED(h)) { printf("[%s] staging create hr=0x%08x\n", label, (unsigned)h); return 0xEEEEEEEE; }
    ctx->CopyResource(staging, src);
    D3D11_MAPPED_SUBRESOURCE map{};
    h = ctx->Map(staging, 0, D3D11_MAP_READ, 0, &map);
    if (FAILED(h)) { printf("[%s] map hr=0x%08x\n", label, (unsigned)h); staging->Release(); return 0xEEEEEEEE; }
    const unsigned char* p = reinterpret_cast<const unsigned char*>(map.pData);
    unsigned nonzero = 0;
    for (int i = 0; i < 64; i++) {
      int x = (i % 8) * 32 + 16, y = (i / 8) * 32 + 16;
      const unsigned char* px = p + y * map.RowPitch + x * 4;
      if (px[0] | px[1] | px[2] | px[3]) nonzero++;
    }
    const unsigned char* c = p + 128 * map.RowPitch + 128 * 4;
    unsigned val = (unsigned)c[0] | ((unsigned)c[1] << 8) | ((unsigned)c[2] << 16) | ((unsigned)c[3] << 24);
    printf("[%s] center BGRA = %u %u %u %u  nonzero=%u/64\n", label, c[0], c[1], c[2], c[3], nonzero);
    ctx->Unmap(staging, 0);
    staging->Release();
    return val;
  };

  // (A) creator-side readback of the shared texture: did the clear land?
  unsigned a = readback(dev1, ctx1, tex, "A dev1 self");

  // (B) opener-side readback: does content survive the open/alias?
  unsigned b = readback(dev2, ctx2, opened, "B dev2 opened");

  // (C) re-clear on dev1 AFTER the open (new color), flush, read on dev2:
  // discriminates a one-time open-transition discard from a persistent
  // aliasing break.
  const float color2[4] = { 1.00f, 0.25f, 0.50f, 1.00f }; // R=255 G=64 B=127
  ctx1->ClearRenderTargetView(rtv, color2);
  ctx1->Flush();
  unsigned cval0 = readback(dev2, ctx2, opened, "C0 dev2 immediately after re-clear");
  Sleep(3000); // discriminate ordering latency from broken propagation
  unsigned cval = readback(dev2, ctx2, opened, "C1 dev2 3s after re-clear");
  (void)cval0;

  // (D) write on dev2 through the alias, read on dev1 (reverse direction).
  ID3D11RenderTargetView* rtv2 = nullptr;
  hr = dev2->CreateRenderTargetView(opened, nullptr, &rtv2);
  printf("dev2 CreateRenderTargetView(opened) hr=0x%08x\n", (unsigned)hr);
  unsigned dval = 0xEEEEEEEE;
  if (SUCCEEDED(hr)) {
    const float color3[4] = { 0.50f, 1.00f, 0.25f, 1.00f }; // R=127 G=255 B=64
    ctx2->ClearRenderTargetView(rtv2, color3);
    ctx2->Flush();
    Sleep(3000);
    readback(dev2, ctx2, opened, "D0 dev2 self after own clear");
    dval = readback(dev1, ctx1, tex, "D dev1 3s after dev2 clear");
  }

  // (E) copy-engine write instead of a clear: UpdateSubresource writes raw
  // bytes via staging+copy — discriminates fast-clear/compression metadata
  // divergence (clears diverge, copies propagate) from fully separate
  // storage (nothing propagates).
  {
    static unsigned char pattern[256 * 256 * 4];
    for (size_t i = 0; i < sizeof(pattern); i += 4) {
      pattern[i + 0] = 0x11; pattern[i + 1] = 0x22;
      pattern[i + 2] = 0x33; pattern[i + 3] = 0xFF;
    }
    ctx1->UpdateSubresource(tex, 0, nullptr, pattern, 256 * 4, 0);
    ctx1->Flush();
    readback(dev1, ctx1, tex, "E0 dev1 self after UpdateSubresource");
    Sleep(2000);
    readback(dev2, ctx2, opened, "E1 dev2 2s after dev1 UpdateSubresource");
  }

  bool passA = (a & 0xFFFFFF) == 0x3F7FBF || (a & 0xFFFFFF) == 0x407FBF;
  bool passB = b == a;
  printf("RESULT: A(dev1 self)=%s B(dev2 opened)=%s C=0x%08x D=0x%08x\n",
         passA ? "PASS" : "FAIL", passB ? "PASS" : "FAIL", cval, dval);
  return (passA && passB) ? 0 : 13;
}
