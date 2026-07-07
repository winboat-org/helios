#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <stdio.h>

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
  if (FAILED(hr)) {
    printf("CreateDXGIFactory1 hr=0x%08lx %s\n", hr, hr_name(hr));
    return hr;
  }

  for (UINT i = 0;; ++i) {
    IDXGIAdapter1* adapter = nullptr;
    hr = factory->EnumAdapters1(i, &adapter);
    if (hr == DXGI_ERROR_NOT_FOUND)
      break;
    if (FAILED(hr)) {
      printf("EnumAdapters1[%u] hr=0x%08lx %s\n", i, hr, hr_name(hr));
      break;
    }

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

static HRESULT create_buffer_uav(
    ID3D11Device* device,
    const char* name,
    UINT bindFlags,
    UINT miscFlags,
    UINT stride,
    DXGI_FORMAT uavFormat,
    UINT uavFlags,
    UINT byteWidth) {
  D3D11_BUFFER_DESC bd = {};
  bd.ByteWidth = byteWidth;
  bd.Usage = D3D11_USAGE_DEFAULT;
  bd.BindFlags = bindFlags;
  bd.CPUAccessFlags = 0;
  bd.MiscFlags = miscFlags;
  bd.StructureByteStride = stride;

  ID3D11Buffer* buffer = nullptr;
  HRESULT hr = device->CreateBuffer(&bd, nullptr, &buffer);
  printf("%s CreateBuffer bind=0x%x misc=0x%x stride=%u hr=0x%08lx %s\n",
         name, bindFlags, miscFlags, stride, hr, hr_name(hr));
  if (FAILED(hr))
    return hr;

  D3D11_UNORDERED_ACCESS_VIEW_DESC ud = {};
  ud.Format = uavFormat;
  ud.ViewDimension = D3D11_UAV_DIMENSION_BUFFER;
  ud.Buffer.FirstElement = 0;
  ud.Buffer.NumElements = stride ? byteWidth / stride : byteWidth / 4;
  ud.Buffer.Flags = uavFlags;

  ID3D11UnorderedAccessView* uav = nullptr;
  hr = device->CreateUnorderedAccessView(buffer, &ud, &uav);
  printf("%s CreateUAV fmt=%u elems=%u flags=0x%x hr=0x%08lx %s\n",
         name, ud.Format, ud.Buffer.NumElements, ud.Buffer.Flags, hr, hr_name(hr));

  if (SUCCEEDED(hr)) {
    ID3D11DeviceContext* ctx = nullptr;
    device->GetImmediateContext(&ctx);
    UINT clear[4] = { 1, 2, 3, 4 };
    ctx->ClearUnorderedAccessViewUint(uav, clear);
    UINT initial = 0;
    ctx->CSSetUnorderedAccessViews(0, 1, &uav, &initial);
    ID3D11UnorderedAccessView* nullUav = nullptr;
    ctx->CSSetUnorderedAccessViews(0, 1, &nullUav, &initial);
    ctx->OMSetRenderTargetsAndUnorderedAccessViews(0, nullptr, nullptr, 0, 1, &uav, &initial);
    ctx->OMSetRenderTargetsAndUnorderedAccessViews(0, nullptr, nullptr, 0, 1, &nullUav, &initial);
    ctx->Flush();
    ctx->Release();
  }

  releasep(&uav);
  releasep(&buffer);
  return hr;
}

int main() {
  printf("d3d11_uav_probe pid=%lu\n", GetCurrentProcessId());

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
  releasep(&context);
  releasep(&adapter);
  if (FAILED(hr))
    return 3;

  int failures = 0;
  failures += FAILED(create_buffer_uav(device, "typed",
      D3D11_BIND_UNORDERED_ACCESS, 0, 0, DXGI_FORMAT_R32_UINT, 0, 1024));
  failures += FAILED(create_buffer_uav(device, "raw",
      D3D11_BIND_UNORDERED_ACCESS, D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS,
      0, DXGI_FORMAT_R32_TYPELESS, D3D11_BUFFER_UAV_FLAG_RAW, 1024));
  failures += FAILED(create_buffer_uav(device, "structured",
      D3D11_BIND_UNORDERED_ACCESS, D3D11_RESOURCE_MISC_BUFFER_STRUCTURED,
      16, DXGI_FORMAT_UNKNOWN, 0, 1024));
  failures += FAILED(create_buffer_uav(device, "indirect",
      D3D11_BIND_UNORDERED_ACCESS, D3D11_RESOURCE_MISC_DRAWINDIRECT_ARGS,
      0, DXGI_FORMAT_R32_UINT, 0, 1024));

  releasep(&device);
  printf("failures=%d\n", failures);
  return failures ? 1 : 0;
}
