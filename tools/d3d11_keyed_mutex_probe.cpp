// Create a keyed-mutex shared texture on one Helios D3D11 device, open it on a
// second Helios D3D11 device, and exercise IDXGIKeyedMutex acquire/release.
//
// Build:
//   cl /EHsc /W4 d3d11_keyed_mutex_probe.cpp /link dxgi.lib d3d11.lib

#include <d3d11_4.h>
#include <dxgi1_6.h>
#include <cstdio>
#include <cwchar>
#include <windows.h>

static void dump_vtable(const char* name, void* object) {
  if (!object) {
    printf("%s vtable: null object\n", name);
    return;
  }
  void** vtbl = *reinterpret_cast<void***>(object);
  printf("%s object=%p vtbl=%p\n", name, object, vtbl);
  for (int i = 0; i < 10; ++i) {
    HMODULE module = nullptr;
    char path[MAX_PATH] = {};
    if (GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                           GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                           reinterpret_cast<LPCSTR>(vtbl[i]), &module)) {
      GetModuleFileNameA(module, path, MAX_PATH);
    }
    printf("  [%d] %p %s\n", i, vtbl[i], path[0] ? path : "<unknown>");
  }
}

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

static HRESULT create_device(IDXGIAdapter1* adapter, ID3D11Device** device) {
  const D3D_FEATURE_LEVEL levels[] = {
      D3D_FEATURE_LEVEL_11_1,
      D3D_FEATURE_LEVEL_11_0,
      D3D_FEATURE_LEVEL_10_1,
      D3D_FEATURE_LEVEL_10_0,
  };
  ID3D11DeviceContext* ctx = nullptr;
  D3D_FEATURE_LEVEL fl{};
  HRESULT hr = D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                                 D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                                 levels, _countof(levels), D3D11_SDK_VERSION,
                                 device, &fl, &ctx);
  printf("D3D11CreateDevice hr=0x%08x fl=0x%04x device=%p\n",
         (unsigned)hr, (unsigned)fl, (void*)*device);
  if (ctx)
    ctx->Release();
  return hr;
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
    return 2;
  }

  ID3D11Device* dev1 = nullptr;
  ID3D11Device* dev2 = nullptr;
  hr = create_device(helios, &dev1);
  if (FAILED(hr) || !dev1)
    return 3;
  hr = create_device(helios, &dev2);
  if (FAILED(hr) || !dev2)
    return 4;

  D3D11_TEXTURE2D_DESC td{};
  td.Width = 256;
  td.Height = 256;
  td.MipLevels = 1;
  td.ArraySize = 1;
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  td.SampleDesc.Count = 1;
  td.Usage = D3D11_USAGE_DEFAULT;
  td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
  td.MiscFlags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE |
                 D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX;

  ID3D11Texture2D* tex = nullptr;
  hr = dev1->CreateTexture2D(&td, nullptr, &tex);
  printf("CreateTexture2D(KEYEDMUTEX|NTHANDLE) hr=0x%08x tex=%p\n",
         (unsigned)hr, (void*)tex);
  if (FAILED(hr) || !tex)
    return 5;

  IDXGIKeyedMutex* km1 = nullptr;
  hr = tex->QueryInterface(__uuidof(IDXGIKeyedMutex), reinterpret_cast<void**>(&km1));
  printf("QI source IDXGIKeyedMutex hr=0x%08x km1=%p\n", (unsigned)hr, (void*)km1);
  if (FAILED(hr) || !km1)
    return 6;
  dump_vtable("km1", km1);

  IDXGIResource1* res1 = nullptr;
  hr = tex->QueryInterface(__uuidof(IDXGIResource1), reinterpret_cast<void**>(&res1));
  printf("QI IDXGIResource1 hr=0x%08x res=%p\n", (unsigned)hr, (void*)res1);
  if (FAILED(hr) || !res1)
    return 7;

  HANDLE handle = nullptr;
  hr = res1->CreateSharedHandle(nullptr,
                                DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                nullptr, &handle);
  printf("CreateSharedHandle hr=0x%08x handle=%p\n", (unsigned)hr, handle);
  if (FAILED(hr) || !handle)
    return 8;

  ID3D11Device1* dev2_1 = nullptr;
  hr = dev2->QueryInterface(__uuidof(ID3D11Device1), reinterpret_cast<void**>(&dev2_1));
  printf("QI ID3D11Device1 hr=0x%08x dev2_1=%p\n", (unsigned)hr, (void*)dev2_1);
  if (FAILED(hr) || !dev2_1)
    return 9;

  ID3D11Texture2D* opened = nullptr;
  hr = dev2_1->OpenSharedResource1(handle, __uuidof(ID3D11Texture2D),
                                   reinterpret_cast<void**>(&opened));
  printf("OpenSharedResource1 hr=0x%08x opened=%p\n", (unsigned)hr, (void*)opened);
  if (FAILED(hr) || !opened)
    return 10;

  IDXGIKeyedMutex* km2 = nullptr;
  hr = opened->QueryInterface(__uuidof(IDXGIKeyedMutex), reinterpret_cast<void**>(&km2));
  printf("QI opened IDXGIKeyedMutex hr=0x%08x km2=%p\n", (unsigned)hr, (void*)km2);
  if (FAILED(hr) || !km2)
    return 11;
  dump_vtable("km2", km2);

  hr = km1->AcquireSync(0, 1000);
  printf("km1 AcquireSync(0) hr=0x%08x\n", (unsigned)hr);
  if (SUCCEEDED(hr)) {
    hr = km1->ReleaseSync(1);
    printf("km1 ReleaseSync(1) hr=0x%08x\n", (unsigned)hr);
  }

  hr = km2->AcquireSync(1, 1000);
  printf("km2 AcquireSync(1) hr=0x%08x\n", (unsigned)hr);
  if (SUCCEEDED(hr)) {
    hr = km2->ReleaseSync(2);
    printf("km2 ReleaseSync(2) hr=0x%08x\n", (unsigned)hr);
  }

  hr = km1->AcquireSync(2, 1000);
  printf("km1 AcquireSync(2) hr=0x%08x\n", (unsigned)hr);
  if (SUCCEEDED(hr)) {
    HRESULT hr2 = km1->ReleaseSync(3);
    printf("km1 ReleaseSync(3) hr=0x%08x\n", (unsigned)hr2);
  }

  if (km2) km2->Release();
  if (opened) opened->Release();
  if (dev2_1) dev2_1->Release();
  if (handle) CloseHandle(handle);
  if (res1) res1->Release();
  if (km1) km1->Release();
  if (tex) tex->Release();
  if (dev2) dev2->Release();
  if (dev1) dev1->Release();
  if (helios) helios->Release();
  if (factory) factory->Release();
  return FAILED(hr) ? 12 : 0;
}
