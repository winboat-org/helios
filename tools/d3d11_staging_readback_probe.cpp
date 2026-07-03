// Exercise the Helios D3D11 staging readback path used by the Looking Glass IDD
// D3D11 fallback:
//   DEFAULT BGRA render-target texture -> STAGING texture -> Map(READ).
//
// Build:
//   cl /EHsc /W4 d3d11_staging_readback_probe.cpp /link dxgi.lib d3d11.lib

#include <d3d11_4.h>
#include <dxgi1_6.h>
#include <cstdio>
#include <cwchar>
#include <cstdint>

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

int main() {
  IDXGIFactory1* factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1), reinterpret_cast<void**>(&factory));
  if (FAILED(hr)) {
    printf("CreateDXGIFactory1 hr=0x%08x\n", (unsigned)hr);
    return 1;
  }

  IDXGIAdapter1* helios = find_helios(factory);
  if (!helios) {
    printf("Helios adapter not found\n");
    factory->Release();
    return 2;
  }

  const D3D_FEATURE_LEVEL levels[] = {
      D3D_FEATURE_LEVEL_11_1,
      D3D_FEATURE_LEVEL_11_0,
      D3D_FEATURE_LEVEL_10_1,
      D3D_FEATURE_LEVEL_10_0,
  };
  ID3D11Device* device = nullptr;
  ID3D11DeviceContext* context = nullptr;
  D3D_FEATURE_LEVEL fl{};
  hr = D3D11CreateDevice(helios, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                         D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                         levels, _countof(levels), D3D11_SDK_VERSION,
                         &device, &fl, &context);
  printf("D3D11CreateDevice hr=0x%08x fl=0x%04x device=%p context=%p\n",
         (unsigned)hr, (unsigned)fl, (void*)device, (void*)context);
  if (FAILED(hr) || !device || !context)
    return 3;

  D3D11_TEXTURE2D_DESC desc{};
  desc.Width = 256;
  desc.Height = 256;
  desc.MipLevels = 1;
  desc.ArraySize = 1;
  desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  desc.SampleDesc.Count = 1;
  desc.Usage = D3D11_USAGE_DEFAULT;
  desc.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;

  ID3D11Texture2D* src = nullptr;
  hr = device->CreateTexture2D(&desc, nullptr, &src);
  printf("CreateTexture2D default hr=0x%08x tex=%p\n", (unsigned)hr, (void*)src);
  if (FAILED(hr) || !src)
    return 4;

  FLOAT color[4] = {0.25f, 0.5f, 0.75f, 1.0f};
  ID3D11RenderTargetView* rtv = nullptr;
  hr = device->CreateRenderTargetView(src, nullptr, &rtv);
  printf("CreateRenderTargetView hr=0x%08x rtv=%p\n", (unsigned)hr, (void*)rtv);
  if (SUCCEEDED(hr) && rtv) {
    context->ClearRenderTargetView(rtv, color);
    rtv->Release();
  }

  D3D11_TEXTURE2D_DESC staging = desc;
  staging.BindFlags = 0;
  staging.MiscFlags = 0;
  staging.Usage = D3D11_USAGE_STAGING;
  staging.CPUAccessFlags = D3D11_CPU_ACCESS_READ;

  ID3D11Texture2D* dst = nullptr;
  hr = device->CreateTexture2D(&staging, nullptr, &dst);
  printf("CreateTexture2D staging hr=0x%08x tex=%p\n", (unsigned)hr, (void*)dst);
  if (FAILED(hr) || !dst)
    return 5;

  printf("CopyResource staging<-default\n");
  context->CopyResource(dst, src);
  context->Flush();

  D3D11_MAPPED_SUBRESOURCE mapped{};
  hr = context->Map(dst, 0, D3D11_MAP_READ, 0, &mapped);
  printf("Map staging hr=0x%08x data=%p rowPitch=%u depthPitch=%u\n",
         (unsigned)hr, mapped.pData, mapped.RowPitch, mapped.DepthPitch);
  if (SUCCEEDED(hr) && mapped.pData) {
    const auto* p = static_cast<const std::uint8_t*>(mapped.pData);
    printf("first bytes: %02x %02x %02x %02x\n", p[0], p[1], p[2], p[3]);
    context->Unmap(dst, 0);
  }

  dst->Release();
  src->Release();
  context->Release();
  device->Release();
  helios->Release();
  factory->Release();
  return FAILED(hr) ? 6 : 0;
}
