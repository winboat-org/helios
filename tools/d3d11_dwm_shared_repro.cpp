// Reproduce DWM's failing shared-texture creates on the Helios adapter:
//   1896x1030 B8G8R8A8_UNORM, bind RT|SR, misc 0x2 (SHARED) and 0x802
//   (SHARED|SHARED_NTHANDLE) — both fail 0x80070057 in dwm with no DXVK log.
// Run from a console with DXVK_LOG_PATH set (e.g. C:\Users\Rupansh) so the
// DXVK Logger error (the DxvkError message CreateTexture2D swallows into
// E_INVALIDARG) lands in <exe>_helios_umd_dxvk.log.
//
// Build:
//   clang-cl /nologo /MD /O2 Z:\tools\d3d11_dwm_shared_repro.cpp \
//     /Fed3d11_dwm_shared_repro.exe /link d3d11.lib dxgi.lib
//
// Run:
//   $env:DXVK_LOG_PATH='C:\Users\Rupansh'; .\d3d11_dwm_shared_repro.exe

#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <stdio.h>
#include <wchar.h>

static IDXGIAdapter1* find_helios(IDXGIFactory1* factory) {
  IDXGIAdapter1* adapter = nullptr;
  for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 desc{};
    adapter->GetDesc1(&desc);
    wprintf(L"DXGI[%u] \"%s\" Vendor=0x%04x\n", i, desc.Description, desc.VendorId);
    if (wcsstr(desc.Description, L"Helios"))
      return adapter;
    adapter->Release();
    adapter = nullptr;
  }
  return nullptr;
}

static void try_create(ID3D11Device* dev, UINT w, UINT h, UINT bind, UINT misc,
                       const char* label) {
  D3D11_TEXTURE2D_DESC desc{};
  desc.Width = w;
  desc.Height = h;
  desc.MipLevels = 1;
  desc.ArraySize = 1;
  desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  desc.SampleDesc.Count = 1;
  desc.Usage = D3D11_USAGE_DEFAULT;
  desc.BindFlags = bind;
  desc.MiscFlags = misc;

  ID3D11Texture2D* tex = nullptr;
  HRESULT hr = dev->CreateTexture2D(&desc, nullptr, &tex);
  printf("%s: %ux%u bind=0x%x misc=0x%x -> hr=0x%08x tex=%p\n", label, w, h,
         bind, misc, (unsigned)hr, tex);
  fflush(stdout);
  if (tex)
    tex->Release();
}

int main() {
  IDXGIFactory1* factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&factory);
  if (FAILED(hr)) { printf("CreateDXGIFactory1 hr=0x%08x\n", (unsigned)hr); return 1; }

  IDXGIAdapter1* helios = find_helios(factory);
  if (!helios) { printf("no Helios adapter\n"); return 2; }

  const D3D_FEATURE_LEVEL levels[] = { D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0 };
  ID3D11Device* dev = nullptr;
  D3D_FEATURE_LEVEL fl{};
  hr = D3D11CreateDevice(helios, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                         D3D11_CREATE_DEVICE_BGRA_SUPPORT, levels, 2,
                         D3D11_SDK_VERSION, &dev, &fl, nullptr);
  printf("D3D11CreateDevice hr=0x%08x fl=0x%x\n", (unsigned)hr, (unsigned)fl);
  if (FAILED(hr) || !dev) return 3;

  const UINT RT_SR = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;

  try_create(dev, 1896, 1030, RT_SR, 0x0,   "plain          ");
  try_create(dev, 1896, 1030, RT_SR, 0x2,   "SHARED         ");
  try_create(dev, 1896, 1030, RT_SR, 0x802, "SHARED|NT      ");
  try_create(dev, 1896, 1030, RT_SR, 0x810, "KEYEDMUTEX|NT  ");
  try_create(dev, 1896, 1030, RT_SR, 0x12,  "SHARED|KM(bad) ");
  try_create(dev,  256,  256, RT_SR, 0x2,   "SHARED small   ");
  try_create(dev,  256,  256, RT_SR, 0x802, "SHARED|NT small");

  printf("done\n");
  dev->Release();
  helios->Release();
  factory->Release();
  return 0;
}
