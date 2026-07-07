#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <stdint.h>
#include <stdio.h>
#include <vector>

template <typename T>
static void releasep(T** p) {
  if (*p) {
    (*p)->Release();
    *p = nullptr;
  }
}

static const char* hr_name(HRESULT hr) {
  switch (hr) {
    case S_OK: return "S_OK";
    case E_INVALIDARG: return "E_INVALIDARG";
    case DXGI_ERROR_UNSUPPORTED: return "DXGI_ERROR_UNSUPPORTED";
    case DXGI_ERROR_DEVICE_REMOVED: return "DXGI_ERROR_DEVICE_REMOVED";
    case DXGI_ERROR_DEVICE_RESET: return "DXGI_ERROR_DEVICE_RESET";
    default: return "other";
  }
}

static HRESULT find_helios_adapter(IDXGIAdapter1** out) {
  *out = nullptr;
  IDXGIFactory1* factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&factory);
  if (FAILED(hr))
    return hr;

  for (UINT i = 0;; ++i) {
    IDXGIAdapter1* adapter = nullptr;
    hr = factory->EnumAdapters1(i, &adapter);
    if (hr == DXGI_ERROR_NOT_FOUND)
      break;
    if (FAILED(hr))
      break;

    DXGI_ADAPTER_DESC1 desc = {};
    adapter->GetDesc1(&desc);
    printf("adapter[%u] vendor=0x%04x device=0x%04x luid=%08lx:%08lx desc=%ls\n",
           i, desc.VendorId, desc.DeviceId, desc.AdapterLuid.HighPart,
           desc.AdapterLuid.LowPart, desc.Description);
    if (desc.VendorId == 0x1af4 && desc.DeviceId == 0x1050 && !*out) {
      *out = adapter;
      (*out)->AddRef();
    }
    adapter->Release();
  }

  factory->Release();
  return *out ? S_OK : DXGI_ERROR_NOT_FOUND;
}

int main() {
  printf("d3d11_tex3d_probe pid=%lu\n", GetCurrentProcessId());

  IDXGIAdapter1* adapter = nullptr;
  HRESULT hr = find_helios_adapter(&adapter);
  if (FAILED(hr)) {
    printf("no Helios adapter hr=0x%08lx %s\n", hr, hr_name(hr));
    return 2;
  }

  D3D_FEATURE_LEVEL requested[] = {
    D3D_FEATURE_LEVEL_11_1,
    D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_10_0,
  };
  D3D_FEATURE_LEVEL got = {};
  ID3D11Device* device = nullptr;
  ID3D11DeviceContext* context = nullptr;
  hr = D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0,
                         requested, ARRAYSIZE(requested), D3D11_SDK_VERSION,
                         &device, &got, &context);
  printf("D3D11CreateDevice hr=0x%08lx %s fl=0x%04x device=%p context=%p\n",
         hr, hr_name(hr), got, device, context);
  releasep(&adapter);
  if (FAILED(hr))
    return 3;

  const UINT width = 32;
  const UINT height = 16;
  const UINT depth = 8;
  std::vector<uint32_t> voxels(width * height * depth);
  for (UINT z = 0; z < depth; ++z)
    for (UINT y = 0; y < height; ++y)
      for (UINT x = 0; x < width; ++x)
        voxels[(z * height + y) * width + x] = 0xff000000u | (z << 16) | (y << 8) | x;

  D3D11_TEXTURE3D_DESC td = {};
  td.Width = width;
  td.Height = height;
  td.Depth = depth;
  td.MipLevels = 1;
  td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
  td.Usage = D3D11_USAGE_DEFAULT;
  td.BindFlags = D3D11_BIND_SHADER_RESOURCE;

  D3D11_SUBRESOURCE_DATA init = {};
  init.pSysMem = voxels.data();
  init.SysMemPitch = width * sizeof(uint32_t);
  init.SysMemSlicePitch = width * height * sizeof(uint32_t);

  ID3D11Texture3D* tex = nullptr;
  hr = device->CreateTexture3D(&td, &init, &tex);
  printf("CreateTexture3D hr=0x%08lx %s tex=%p\n", hr, hr_name(hr), tex);
  if (FAILED(hr)) {
    releasep(&context);
    releasep(&device);
    return 4;
  }

  D3D11_SHADER_RESOURCE_VIEW_DESC sd = {};
  sd.Format = td.Format;
  sd.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE3D;
  sd.Texture3D.MostDetailedMip = 0;
  sd.Texture3D.MipLevels = 1;

  ID3D11ShaderResourceView* srv = nullptr;
  hr = device->CreateShaderResourceView(tex, &sd, &srv);
  printf("CreateSRV3D hr=0x%08lx %s srv=%p\n", hr, hr_name(hr), srv);
  if (SUCCEEDED(hr)) {
    context->PSSetShaderResources(0, 1, &srv);
    ID3D11ShaderResourceView* nullSrv = nullptr;
    context->PSSetShaderResources(0, 1, &nullSrv);
    context->Flush();
  }

  releasep(&srv);
  releasep(&tex);
  releasep(&context);
  releasep(&device);
  return FAILED(hr) ? 5 : 0;
}
