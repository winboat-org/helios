// Create a Helios D3D11 shared keyed-mutex texture, then inspect/open the
// resulting NT shared handle with raw D3DKMT.
//
// Build:
//   cl /EHsc /W4 d3d11_kmt_shared_probe.cpp /I"Z:\icd\win-build\wdk-include" /link dxgi.lib d3d11.lib gdi32.lib

#include <windows.h>
#include <d3d11_4.h>
#include <dxgi1_6.h>
#include <d3dkmthk.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>
#include <vector>

static D3DKMT_HANDLE g_adapter = 0;
static D3DKMT_HANDLE g_device = 0;

static void print_status(const char* label, NTSTATUS status) {
  printf("%s status=0x%08x\n", label, static_cast<unsigned>(status));
}

static IDXGIAdapter1* find_helios_dxgi(IDXGIFactory1* factory) {
  IDXGIAdapter1* adapter = nullptr;
  for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 desc{};
    adapter->GetDesc1(&desc);
    wprintf(L"DXGI[%u] \"%s\" Vendor=0x%04x Device=0x%04x Flags=0x%x\n",
            i, desc.Description, desc.VendorId, desc.DeviceId, desc.Flags);
    if (wcsstr(desc.Description, L"Helios"))
      return adapter;
    adapter->Release();
    adapter = nullptr;
  }
  return nullptr;
}

static HRESULT create_d3d11_device(IDXGIAdapter1* adapter, ID3D11Device** device) {
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
         static_cast<unsigned>(hr), static_cast<unsigned>(fl), *device);
  if (ctx)
    ctx->Release();
  return hr;
}

static int open_kmt_from_luid(LUID adapter_luid) {
  D3DKMT_OPENADAPTERFROMLUID open_adapter{};
  open_adapter.AdapterLuid = adapter_luid;
  NTSTATUS st = D3DKMTOpenAdapterFromLuid(&open_adapter);
  if (st != 0 || !open_adapter.hAdapter) {
    print_status("D3DKMTOpenAdapterFromLuid", st);
    return 1;
  }
  D3DKMT_HANDLE chosen = open_adapter.hAdapter;

  D3DKMT_CREATEDEVICE create_device{};
  create_device.hAdapter = chosen;
  st = D3DKMTCreateDevice(&create_device);
  if (st != 0) {
    print_status("D3DKMTCreateDevice", st);
    return 1;
  }

  g_adapter = chosen;
  g_device = create_device.hDevice;
  printf("KMT open ok adapter=0x%08x device=0x%08x luid=%08x:%08x\n",
         g_adapter, g_device,
         static_cast<unsigned>(adapter_luid.HighPart),
         static_cast<unsigned>(adapter_luid.LowPart));
  return 0;
}

static void try_kmt_keyed_mutex(D3DKMT_HANDLE h_keyed_mutex) {
  if (!h_keyed_mutex) {
    printf("KMT keyed mutex absent; skipping direct acquire/release\n");
    return;
  }

  D3DKMT_ACQUIREKEYEDMUTEX acquire{};
  LARGE_INTEGER timeout{};
  timeout.QuadPart = -10000000LL;
  acquire.hKeyedMutex = h_keyed_mutex;
  acquire.Key = 0;
  acquire.pTimeout = &timeout;
  NTSTATUS st = D3DKMTAcquireKeyedMutex(&acquire);
  print_status("D3DKMTAcquireKeyedMutex(key=0)", st);
  printf("  fence=%llu\n", static_cast<unsigned long long>(acquire.FenceValue));

  if (st == 0) {
    D3DKMT_RELEASEKEYEDMUTEX release{};
    release.hKeyedMutex = h_keyed_mutex;
    release.Key = 1;
    st = D3DKMTReleaseKeyedMutex(&release);
    print_status("D3DKMTReleaseKeyedMutex(key=1)", st);
    printf("  fence=%llu\n", static_cast<unsigned long long>(release.FenceValue));
  }
}

int main() {
  IDXGIFactory1* factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1),
                                  reinterpret_cast<void**>(&factory));
  if (FAILED(hr)) {
    printf("CreateDXGIFactory1 hr=0x%08x\n", static_cast<unsigned>(hr));
    return 1;
  }

  IDXGIAdapter1* helios = find_helios_dxgi(factory);
  if (!helios) {
    printf("Helios DXGI adapter not found\n");
    return 2;
  }
  DXGI_ADAPTER_DESC1 adapter_desc{};
  helios->GetDesc1(&adapter_desc);
  printf("Helios DXGI LUID=%08x:%08x\n",
         static_cast<unsigned>(adapter_desc.AdapterLuid.HighPart),
         static_cast<unsigned>(adapter_desc.AdapterLuid.LowPart));

  ID3D11Device* dev = nullptr;
  hr = create_d3d11_device(helios, &dev);
  if (FAILED(hr) || !dev)
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
  desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE |
                   D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX;

  ID3D11Texture2D* tex = nullptr;
  hr = dev->CreateTexture2D(&desc, nullptr, &tex);
  printf("CreateTexture2D(KEYEDMUTEX|NTHANDLE) hr=0x%08x tex=%p\n",
         static_cast<unsigned>(hr), tex);
  if (FAILED(hr) || !tex)
    return 4;

  IDXGIKeyedMutex* km = nullptr;
  hr = tex->QueryInterface(__uuidof(IDXGIKeyedMutex), reinterpret_cast<void**>(&km));
  printf("QI IDXGIKeyedMutex hr=0x%08x km=%p\n", static_cast<unsigned>(hr), km);

  IDXGIResource1* res = nullptr;
  hr = tex->QueryInterface(__uuidof(IDXGIResource1), reinterpret_cast<void**>(&res));
  printf("QI IDXGIResource1 hr=0x%08x res=%p\n", static_cast<unsigned>(hr), res);
  if (FAILED(hr) || !res)
    return 5;

  HANDLE shared_handle = nullptr;
  hr = res->CreateSharedHandle(nullptr,
                               DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                               nullptr, &shared_handle);
  printf("CreateSharedHandle hr=0x%08x handle=%p\n",
         static_cast<unsigned>(hr), shared_handle);
  if (FAILED(hr) || !shared_handle)
    return 6;

  if (open_kmt_from_luid(adapter_desc.AdapterLuid))
    return 7;

  std::vector<unsigned char> runtime(1024);
  D3DKMT_QUERYRESOURCEINFOFROMNTHANDLE query{};
  query.hDevice = g_device;
  query.hNtHandle = shared_handle;
  query.pPrivateRuntimeData = runtime.data();
  query.PrivateRuntimeDataSize = static_cast<UINT>(runtime.size());
  NTSTATUS st = D3DKMTQueryResourceInfoFromNtHandle(&query);
  print_status("D3DKMTQueryResourceInfoFromNtHandle", st);
  printf("  PrivateRuntimeDataSize=%u TotalPrivateDriverDataSize=%u ResourcePrivateDriverDataSize=%u NumAllocations=%u\n",
         query.PrivateRuntimeDataSize,
         query.TotalPrivateDriverDataSize,
         query.ResourcePrivateDriverDataSize,
         query.NumAllocations);

  if (st != 0)
    return 8;

  runtime.resize(query.PrivateRuntimeDataSize);
  std::vector<unsigned char> resource_private(query.ResourcePrivateDriverDataSize);
  std::vector<unsigned char> total_private(query.TotalPrivateDriverDataSize);
  std::vector<D3DDDI_OPENALLOCATIONINFO2> open_allocs(query.NumAllocations);

  D3DKMT_OPENRESOURCEFROMNTHANDLE open{};
  open.hDevice = g_device;
  open.hNtHandle = shared_handle;
  open.NumAllocations = query.NumAllocations;
  open.pOpenAllocationInfo2 = open_allocs.data();
  open.PrivateRuntimeDataSize = query.PrivateRuntimeDataSize;
  open.pPrivateRuntimeData = runtime.data();
  open.ResourcePrivateDriverDataSize = query.ResourcePrivateDriverDataSize;
  open.pResourcePrivateDriverData = resource_private.empty() ? nullptr : resource_private.data();
  open.TotalPrivateDriverDataBufferSize = query.TotalPrivateDriverDataSize;
  open.pTotalPrivateDriverDataBuffer = total_private.empty() ? nullptr : total_private.data();

  st = D3DKMTOpenResourceFromNtHandle(&open);
  print_status("D3DKMTOpenResourceFromNtHandle", st);
  printf("  hResource=0x%08x hKeyedMutex=0x%08x hSyncObject=0x%08x totalWritten=%u\n",
         open.hResource,
         open.hKeyedMutex,
         open.hSyncObject,
         open.TotalPrivateDriverDataBufferSize);

  for (UINT i = 0; i < query.NumAllocations; ++i) {
    printf("  alloc[%u] hAllocation=0x%08x private=%p/%u gpuva=0x%llx\n",
           i,
           open_allocs[i].hAllocation,
           open_allocs[i].pPrivateDriverData,
           open_allocs[i].PrivateDriverDataSize,
           static_cast<unsigned long long>(open_allocs[i].GpuVirtualAddress));
  }

  if (st == 0)
    try_kmt_keyed_mutex(open.hKeyedMutex);

  if (open.hKeyedMutex) {
    D3DKMT_DESTROYKEYEDMUTEX destroy_mutex{};
    destroy_mutex.hKeyedMutex = open.hKeyedMutex;
    print_status("D3DKMTDestroyKeyedMutex", D3DKMTDestroyKeyedMutex(&destroy_mutex));
  }
  if (open.hSyncObject) {
    D3DKMT_DESTROYSYNCHRONIZATIONOBJECT destroy_sync{};
    destroy_sync.hSyncObject = open.hSyncObject;
    print_status("D3DKMTDestroySynchronizationObject",
                 D3DKMTDestroySynchronizationObject(&destroy_sync));
  }
  if (open.hResource) {
    D3DKMT_DESTROYALLOCATION destroy_alloc{};
    destroy_alloc.hDevice = g_device;
    destroy_alloc.hResource = open.hResource;
    print_status("D3DKMTDestroyAllocation(opened resource)",
                 D3DKMTDestroyAllocation(&destroy_alloc));
  }
  if (g_device) {
    D3DKMT_DESTROYDEVICE destroy_device{};
    destroy_device.hDevice = g_device;
    print_status("D3DKMTDestroyDevice", D3DKMTDestroyDevice(&destroy_device));
  }
  if (g_adapter) {
    D3DKMT_CLOSEADAPTER close{};
    close.hAdapter = g_adapter;
    print_status("D3DKMTCloseAdapter", D3DKMTCloseAdapter(&close));
  }

  if (shared_handle) CloseHandle(shared_handle);
  if (res) res->Release();
  if (km) km->Release();
  if (tex) tex->Release();
  if (dev) dev->Release();
  if (helios) helios->Release();
  if (factory) factory->Release();
  return st == 0 ? 0 : 9;
}
