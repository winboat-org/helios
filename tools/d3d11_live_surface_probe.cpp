// d3d11_live_surface_probe.cpp — read the CONTENT of live shared surfaces by
// their global KMT handles, from a fresh process/venus context.
//
// 2026-07-03 (NVIDIA boot, black steady state): dwm renders composition into
// resid 175 (its constant src_alloc 0x400055c0 — RotateResourceIdentities is a
// no-op stub) while the IDD resolve-reads resid 174 and samples all-zero. This
// probe opens each IddCx swapchain buffer via OpenSharedResource(global KMT
// handle), copies to a CPU staging texture, and histograms the bytes — the
// ground truth for WHERE dwm's composed pixels actually land (if anywhere).
//
// Usage: d3d11_live_surface_probe.exe <hexhandle> [<hexhandle> ...]
//   e.g. d3d11_live_surface_probe.exe 0x40003480 0x80001b40 0x80001b00
// Repeats the read 3 times, 2 s apart, per handle (content changes if dwm is
// actively presenting into the surface).
//
// Build (VM, vcvars64):
//   cl /EHsc /W4 Z:\tools\d3d11_live_surface_probe.cpp /link dxgi.lib d3d11.lib

#include <d3d11_4.h>
#include <dxgi1_6.h>
#include <cstdio>
#include <cstdlib>
#include <cwchar>

static IDXGIAdapter1* find_helios(IDXGIFactory1* factory) {
  IDXGIAdapter1* adapter = nullptr;
  for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 desc{};
    adapter->GetDesc1(&desc);
    if (wcsstr(desc.Description, L"Helios"))
      return adapter;
    adapter->Release();
    adapter = nullptr;
  }
  return nullptr;
}

static void dump_surface(ID3D11Device* dev, ID3D11DeviceContext* ctx,
                         ID3D11Texture2D* tex, const char* tag) {
  D3D11_TEXTURE2D_DESC d{};
  tex->GetDesc(&d);
  printf("%s: %ux%u fmt=%u usage=%u bind=0x%x misc=0x%x\n", tag, d.Width,
         d.Height, (unsigned)d.Format, (unsigned)d.Usage, d.BindFlags,
         d.MiscFlags);

  D3D11_TEXTURE2D_DESC sd = d;
  sd.Usage = D3D11_USAGE_STAGING;
  sd.BindFlags = 0;
  sd.MiscFlags = 0;
  sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  sd.MipLevels = 1;
  sd.ArraySize = 1;
  ID3D11Texture2D* staging = nullptr;
  HRESULT hr = dev->CreateTexture2D(&sd, nullptr, &staging);
  if (FAILED(hr)) {
    printf("%s: staging create hr=0x%08x\n", tag, (unsigned)hr);
    return;
  }
  ctx->CopyResource(staging, tex);
  ctx->Flush();
  D3D11_MAPPED_SUBRESOURCE map{};
  hr = ctx->Map(staging, 0, D3D11_MAP_READ, 0, &map);
  if (FAILED(hr)) {
    printf("%s: map hr=0x%08x\n", tag, (unsigned)hr);
    staging->Release();
    return;
  }
  // Histogram: nonzero dwords, distinct-ish hash, and a few sample pixels.
  const unsigned char* base = (const unsigned char*)map.pData;
  unsigned long long nonzero = 0, total = 0, hash = 1469598103934665603ull;
  for (UINT y = 0; y < d.Height; ++y) {
    const unsigned* row = (const unsigned*)(base + (size_t)y * map.RowPitch);
    for (UINT x = 0; x < d.Width; ++x) {
      unsigned v = row[x];
      total++;
      if (v != 0) nonzero++;
      hash = (hash ^ v) * 1099511628211ull;
    }
  }
  const unsigned* c =
      (const unsigned*)(base + (size_t)(d.Height / 2) * map.RowPitch);
  printf("%s: nonzero=%llu/%llu hash=%016llx center=%08x %08x %08x corner=%08x\n",
         tag, nonzero, total, hash, c[d.Width / 2], c[d.Width / 2 + 1],
         c[d.Width / 2 + 2], ((const unsigned*)base)[0]);
  ctx->Unmap(staging, 0);
  staging->Release();
}

int main(int argc, char** argv) {
  if (argc < 2) {
    printf("usage: %s <hexhandle> [...]\n", argv[0]);
    return 2;
  }
  IDXGIFactory1* factory = nullptr;
  if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) {
    printf("CreateDXGIFactory1 failed\n");
    return 1;
  }
  IDXGIAdapter1* adapter = find_helios(factory);
  if (!adapter) {
    printf("no Helios adapter\n");
    return 1;
  }
  ID3D11Device* dev = nullptr;
  ID3D11DeviceContext* ctx = nullptr;
  const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1,
                                      D3D_FEATURE_LEVEL_11_0,
                                      D3D_FEATURE_LEVEL_10_1,
                                      D3D_FEATURE_LEVEL_10_0};
  D3D_FEATURE_LEVEL fl{};
  HRESULT hr = D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                                 D3D11_CREATE_DEVICE_BGRA_SUPPORT, levels,
                                 _countof(levels), D3D11_SDK_VERSION, &dev, &fl,
                                 &ctx);
  printf("device hr=0x%08x fl=0x%x\n", (unsigned)hr, (unsigned)fl);
  if (FAILED(hr))
    return 1;

  for (int round = 0; round < 3; ++round) {
    printf("--- round %d ---\n", round);
    for (int i = 1; i < argc; ++i) {
      HANDLE h = (HANDLE)(UINT_PTR)strtoull(argv[i], nullptr, 16);
      ID3D11Texture2D* tex = nullptr;
      hr = dev->OpenSharedResource(h, IID_PPV_ARGS(&tex));
      if (FAILED(hr)) {
        printf("open %s hr=0x%08x\n", argv[i], (unsigned)hr);
        continue;
      }
      char tag[64];
      snprintf(tag, sizeof(tag), "surface %s r%d", argv[i], round);
      dump_surface(dev, ctx, tex, tag);
      tex->Release();
    }
    if (round != 2) Sleep(2000);
  }
  dev->Release();
  ctx->Release();
  factory->Release();
  adapter->Release();
  return 0;
}
