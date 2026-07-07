// Helios D3D11 MSAA view translation probe.
//
// Exercises Texture2D MSAA RTV/DSV/SRV creation through the DDI path, then
// clears + resolves a 4x color RT and reads one pixel back.
//
// Build:
//   cl /EHsc /W4 d3d11_msaa_view_probe.cpp /link dxgi.lib d3d11.lib

#include <d3d11.h>
#include <dxgi1_6.h>
#include <cstdint>
#include <cstdio>
#include <cwchar>

static IDXGIAdapter1* find_helios(IDXGIFactory1* factory) {
  IDXGIAdapter1* adapter = nullptr;
  for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 desc{};
    adapter->GetDesc1(&desc);
    wprintf(L"adapter[%u] \"%ls\" vendor=0x%04x device=0x%04x luid=%08x:%08x flags=0x%x\n",
            i, desc.Description, desc.VendorId, desc.DeviceId,
            desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart, desc.Flags);
    if (desc.VendorId == 0x1af4 && desc.DeviceId == 0x1050)
      return adapter;
    adapter->Release();
    adapter = nullptr;
  }
  return nullptr;
}

int main() {
  IDXGIFactory1* factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1),
                                  reinterpret_cast<void**>(&factory));
  printf("CreateDXGIFactory1 hr=0x%08x factory=%p\n", (unsigned)hr, (void*)factory);
  if (FAILED(hr))
    return 1;

  IDXGIAdapter1* adapter = find_helios(factory);
  if (!adapter) {
    printf("no Helios adapter found\n");
    factory->Release();
    return 2;
  }

  ID3D11Device* device = nullptr;
  ID3D11DeviceContext* context = nullptr;
  D3D_FEATURE_LEVEL got{};
  const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_0};
  hr = D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0,
                         levels, _countof(levels), D3D11_SDK_VERSION,
                         &device, &got, &context);
  printf("D3D11CreateDevice hr=0x%08x fl=0x%04x device=%p context=%p\n",
         (unsigned)hr, (unsigned)got, (void*)device, (void*)context);
  if (FAILED(hr) || !device || !context)
    return 3;

  UINT quality = 0;
  hr = device->CheckMultisampleQualityLevels(DXGI_FORMAT_R8G8B8A8_UNORM, 4, &quality);
  printf("CheckMSAA RGBA8 4x hr=0x%08x quality=%u\n", (unsigned)hr, quality);
  if (FAILED(hr) || quality == 0)
    return 4;

  D3D11_TEXTURE2D_DESC color_desc{};
  color_desc.Width = 256;
  color_desc.Height = 256;
  color_desc.MipLevels = 1;
  color_desc.ArraySize = 1;
  color_desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
  color_desc.SampleDesc.Count = 4;
  color_desc.SampleDesc.Quality = 0;
  color_desc.Usage = D3D11_USAGE_DEFAULT;
  color_desc.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;

  ID3D11Texture2D* color = nullptr;
  hr = device->CreateTexture2D(&color_desc, nullptr, &color);
  printf("CreateTexture2D color MSAA hr=0x%08x tex=%p\n", (unsigned)hr, (void*)color);
  if (FAILED(hr) || !color)
    return 5;

  D3D11_RENDER_TARGET_VIEW_DESC rtv_desc{};
  rtv_desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
  rtv_desc.ViewDimension = D3D11_RTV_DIMENSION_TEXTURE2DMS;

  ID3D11RenderTargetView* rtv = nullptr;
  hr = device->CreateRenderTargetView(color, &rtv_desc, &rtv);
  printf("CreateRenderTargetView 2DMS hr=0x%08x rtv=%p\n", (unsigned)hr, (void*)rtv);
  if (FAILED(hr) || !rtv)
    return 6;

  D3D11_SHADER_RESOURCE_VIEW_DESC srv_desc{};
  srv_desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
  srv_desc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2DMS;

  ID3D11ShaderResourceView* srv = nullptr;
  hr = device->CreateShaderResourceView(color, &srv_desc, &srv);
  printf("CreateShaderResourceView 2DMS hr=0x%08x srv=%p\n", (unsigned)hr, (void*)srv);
  if (FAILED(hr) || !srv)
    return 7;

  D3D11_TEXTURE2D_DESC depth_desc{};
  depth_desc.Width = 256;
  depth_desc.Height = 256;
  depth_desc.MipLevels = 1;
  depth_desc.ArraySize = 1;
  depth_desc.Format = DXGI_FORMAT_D24_UNORM_S8_UINT;
  depth_desc.SampleDesc.Count = 4;
  depth_desc.SampleDesc.Quality = 0;
  depth_desc.Usage = D3D11_USAGE_DEFAULT;
  depth_desc.BindFlags = D3D11_BIND_DEPTH_STENCIL;

  ID3D11Texture2D* depth = nullptr;
  hr = device->CreateTexture2D(&depth_desc, nullptr, &depth);
  printf("CreateTexture2D depth MSAA hr=0x%08x tex=%p\n", (unsigned)hr, (void*)depth);
  if (FAILED(hr) || !depth)
    return 8;

  D3D11_DEPTH_STENCIL_VIEW_DESC dsv_desc{};
  dsv_desc.Format = DXGI_FORMAT_D24_UNORM_S8_UINT;
  dsv_desc.ViewDimension = D3D11_DSV_DIMENSION_TEXTURE2DMS;

  ID3D11DepthStencilView* dsv = nullptr;
  hr = device->CreateDepthStencilView(depth, &dsv_desc, &dsv);
  printf("CreateDepthStencilView 2DMS hr=0x%08x dsv=%p\n", (unsigned)hr, (void*)dsv);
  if (FAILED(hr) || !dsv)
    return 9;

  D3D11_TEXTURE2D_DESC resolved_desc = color_desc;
  resolved_desc.SampleDesc.Count = 1;
  resolved_desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;

  ID3D11Texture2D* resolved = nullptr;
  hr = device->CreateTexture2D(&resolved_desc, nullptr, &resolved);
  printf("CreateTexture2D resolved hr=0x%08x tex=%p\n", (unsigned)hr, (void*)resolved);
  if (FAILED(hr) || !resolved)
    return 10;

  const FLOAT clear[4] = {0.25f, 0.50f, 0.75f, 1.0f};
  context->OMSetRenderTargets(1, &rtv, dsv);
  context->ClearRenderTargetView(rtv, clear);
  context->ClearDepthStencilView(dsv, D3D11_CLEAR_DEPTH | D3D11_CLEAR_STENCIL, 1.0f, 0);
  context->ResolveSubresource(resolved, 0, color, 0, DXGI_FORMAT_R8G8B8A8_UNORM);

  D3D11_TEXTURE2D_DESC staging_desc = resolved_desc;
  staging_desc.BindFlags = 0;
  staging_desc.Usage = D3D11_USAGE_STAGING;
  staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;

  ID3D11Texture2D* staging = nullptr;
  hr = device->CreateTexture2D(&staging_desc, nullptr, &staging);
  printf("CreateTexture2D staging hr=0x%08x tex=%p\n", (unsigned)hr, (void*)staging);
  if (FAILED(hr) || !staging)
    return 11;

  context->CopyResource(staging, resolved);
  context->Flush();

  D3D11_MAPPED_SUBRESOURCE mapped{};
  hr = context->Map(staging, 0, D3D11_MAP_READ, 0, &mapped);
  printf("Map staging hr=0x%08x data=%p rowPitch=%u\n",
         (unsigned)hr, mapped.pData, mapped.RowPitch);
  if (SUCCEEDED(hr) && mapped.pData) {
    const auto* p = static_cast<const std::uint8_t*>(mapped.pData);
    printf("pixel[0,0] rgba=%u,%u,%u,%u\n", p[0], p[1], p[2], p[3]);
    context->Unmap(staging, 0);
  }

  if (staging) staging->Release();
  if (resolved) resolved->Release();
  if (dsv) dsv->Release();
  if (depth) depth->Release();
  if (srv) srv->Release();
  if (rtv) rtv->Release();
  if (color) color->Release();
  context->Release();
  device->Release();
  adapter->Release();
  factory->Release();
  return FAILED(hr) ? 12 : 0;
}
