// d3d11_xproc_draw_probe.cpp — CROSS-PROCESS replica of the dwm→IDD shape:
// process A creates a legacy-shared (KMT, misc=SHARED) BGRA render target,
// clears white and draws a half-covering triangle with distinct per-component
// colors; process B opens the surface via the global KMT handle
// (OpenSharedResource — exactly WUDFHost's path) and staging-reads it back.
//
// Modes:
//   write  — create + draw + publish handle to xproc_handle.txt, self-read,
//            then keep the texture alive for 60 s.
//   read   — poll xproc_handle.txt, OpenSharedResource, read back both texels.
//
// Expected (write side fixed 2026-07-03): inside=CC336699 outside=FFFFFFFF on
// BOTH sides. A zero/blank read on B with a correct self-read on A isolates
// the cross-process aliasing path.
//
// Build (VM, vcvars64):
//   cl /EHsc /W4 Z:\tools\d3d11_xproc_draw_probe.cpp /Fe:xproc_draw_probe.exe /link dxgi.lib d3d11.lib d3dcompiler.lib
#include <d3d11_4.h>
#include <dxgi1_6.h>
#include <d3dcompiler.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>

static const char* kHandleFile = "C:\\Users\\Rupansh\\helios-probe\\xproc_handle.txt";

static IDXGIAdapter1* find_helios(IDXGIFactory1* factory) {
  IDXGIAdapter1* adapter = nullptr;
  for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 desc{};
    adapter->GetDesc1(&desc);
    if (wcsstr(desc.Description, L"Helios")) return adapter;
    adapter->Release();
    adapter = nullptr;
  }
  return nullptr;
}

static ID3D11Device* g_dev;
static ID3D11DeviceContext* g_ctx;

static int make_device() {
  IDXGIFactory1* factory = nullptr;
  if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) return 1;
  IDXGIAdapter1* helios = find_helios(factory);
  if (!helios) { printf("no Helios adapter\n"); return 1; }
  const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0,
                                      D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0};
  D3D_FEATURE_LEVEL fl{};
  HRESULT hr = D3D11CreateDevice(helios, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                                 D3D11_CREATE_DEVICE_BGRA_SUPPORT, levels,
                                 _countof(levels), D3D11_SDK_VERSION, &g_dev, &fl, &g_ctx);
  printf("device hr=0x%08x fl=0x%x\n", (unsigned)hr, (unsigned)fl);
  return FAILED(hr) ? 1 : 0;
}

static void readback(ID3D11Texture2D* src, const char* label) {
  D3D11_TEXTURE2D_DESC td{};
  src->GetDesc(&td);
  D3D11_TEXTURE2D_DESC sd = td;
  sd.BindFlags = 0;
  sd.MiscFlags = 0;
  sd.Usage = D3D11_USAGE_STAGING;
  sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D* staging = nullptr;
  HRESULT hr = g_dev->CreateTexture2D(&sd, nullptr, &staging);
  if (FAILED(hr)) { printf("%s staging hr=0x%08x\n", label, (unsigned)hr); return; }
  g_ctx->CopyResource(staging, src);
  g_ctx->Flush();
  D3D11_MAPPED_SUBRESOURCE map{};
  hr = g_ctx->Map(staging, 0, D3D11_MAP_READ, 0, &map);
  if (FAILED(hr)) { printf("%s map hr=0x%08x\n", label, (unsigned)hr); staging->Release(); return; }
  const unsigned char* base = (const unsigned char*)map.pData;
  unsigned inside = ((const unsigned*)(base + (size_t)64 * map.RowPitch))[64];
  unsigned outside = ((const unsigned*)(base + (size_t)192 * map.RowPitch))[192];
  printf("%s: inside(64,64)=%08x outside(192,192)=%08x\n", label, inside, outside);
  g_ctx->Unmap(staging, 0);
  staging->Release();
}

static const char* kVs =
    "float4 main(float2 pos : POSITION) : SV_Position {"
    "  return float4(pos, 0.0, 1.0);"
    "}";
static const char* kPs =
    "float4 main() : SV_Target {"
    "  return float4(0.2, 0.4, 0.6, 0.8);"
    "}";

static int do_write() {
  D3D11_TEXTURE2D_DESC td{};
  td.Width = 256; td.Height = 256; td.MipLevels = 1; td.ArraySize = 1;
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  td.SampleDesc.Count = 1;
  td.Usage = D3D11_USAGE_DEFAULT;
  td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
  td.MiscFlags = D3D11_RESOURCE_MISC_SHARED;  // legacy KMT sharing, like dwm's buffers
  ID3D11Texture2D* tex = nullptr;
  HRESULT hr = g_dev->CreateTexture2D(&td, nullptr, &tex);
  printf("CreateTexture2D hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr)) return 2;
  ID3D11RenderTargetView* rtv = nullptr;
  if (FAILED(g_dev->CreateRenderTargetView(tex, nullptr, &rtv))) return 3;

  ID3DBlob *vsb = nullptr, *psb = nullptr, *err = nullptr;
  if (FAILED(D3DCompile(kVs, strlen(kVs), nullptr, nullptr, nullptr, "main", "vs_4_0", 0, 0, &vsb, &err))) return 4;
  if (FAILED(D3DCompile(kPs, strlen(kPs), nullptr, nullptr, nullptr, "main", "ps_4_0", 0, 0, &psb, &err))) return 5;
  ID3D11VertexShader* vs = nullptr;
  ID3D11PixelShader* ps = nullptr;
  if (FAILED(g_dev->CreateVertexShader(vsb->GetBufferPointer(), vsb->GetBufferSize(), nullptr, &vs))) return 6;
  if (FAILED(g_dev->CreatePixelShader(psb->GetBufferPointer(), psb->GetBufferSize(), nullptr, &ps))) return 7;
  D3D11_INPUT_ELEMENT_DESC ied{"POSITION", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0,
                               D3D11_INPUT_PER_VERTEX_DATA, 0};
  ID3D11InputLayout* layout = nullptr;
  if (FAILED(g_dev->CreateInputLayout(&ied, 1, vsb->GetBufferPointer(), vsb->GetBufferSize(), &layout))) return 8;
  const float verts[6] = {-1.f, -1.f, -1.f, 1.f, 1.f, 1.f};
  D3D11_BUFFER_DESC bd{};
  bd.ByteWidth = sizeof(verts);
  bd.Usage = D3D11_USAGE_IMMUTABLE;
  bd.BindFlags = D3D11_BIND_VERTEX_BUFFER;
  D3D11_SUBRESOURCE_DATA init{verts, 0, 0};
  ID3D11Buffer* vb = nullptr;
  if (FAILED(g_dev->CreateBuffer(&bd, &init, &vb))) return 9;
  D3D11_RASTERIZER_DESC rd{};
  rd.FillMode = D3D11_FILL_SOLID;
  rd.CullMode = D3D11_CULL_NONE;
  rd.DepthClipEnable = TRUE;
  ID3D11RasterizerState* rs = nullptr;
  if (FAILED(g_dev->CreateRasterizerState(&rd, &rs))) return 10;

  const float white[4] = {1.f, 1.f, 1.f, 1.f};
  g_ctx->ClearRenderTargetView(rtv, white);
  g_ctx->OMSetRenderTargets(1, &rtv, nullptr);
  D3D11_VIEWPORT vp{0.f, 0.f, 256.f, 256.f, 0.f, 1.f};
  g_ctx->RSSetViewports(1, &vp);
  g_ctx->RSSetState(rs);
  UINT stride = 8, offset = 0;
  g_ctx->IASetVertexBuffers(0, 1, &vb, &stride, &offset);
  g_ctx->IASetInputLayout(layout);
  g_ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
  g_ctx->VSSetShader(vs, nullptr, 0);
  g_ctx->PSSetShader(ps, nullptr, 0);
  g_ctx->Draw(3, 0);
  ID3D11RenderTargetView* nullrtv = nullptr;
  g_ctx->OMSetRenderTargets(1, &nullrtv, nullptr);
  g_ctx->Flush();

  readback(tex, "[writer self]");

  IDXGIResource* res = nullptr;
  if (FAILED(tex->QueryInterface(IID_PPV_ARGS(&res)))) return 11;
  HANDLE handle = nullptr;
  hr = res->GetSharedHandle(&handle);
  printf("GetSharedHandle hr=0x%08x handle=%p\n", (unsigned)hr, handle);
  if (FAILED(hr) || !handle) return 12;
  FILE* f = fopen(kHandleFile, "w");
  if (!f) { printf("handle file open failed\n"); return 13; }
  fprintf(f, "%llx\n", (unsigned long long)(UINT_PTR)handle);
  fclose(f);

  // Keep the texture (and the underlying allocation) alive for the reader.
  for (int i = 0; i < 60; ++i) Sleep(1000);
  return 0;
}

static int do_read() {
  // Poll for the writer's handle.
  HANDLE handle = nullptr;
  for (int i = 0; i < 120; ++i) {
    FILE* f = fopen(kHandleFile, "r");
    if (f) {
      unsigned long long v = 0;
      if (fscanf(f, "%llx", &v) == 1 && v)
        handle = (HANDLE)(UINT_PTR)v;
      fclose(f);
      if (handle) break;
    }
    Sleep(1000);
  }
  if (!handle) { printf("no handle published\n"); return 2; }
  printf("opening handle=%p\n", handle);
  ID3D11Texture2D* tex = nullptr;
  HRESULT hr = g_dev->OpenSharedResource(handle, IID_PPV_ARGS(&tex));
  printf("OpenSharedResource hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr) || !tex) return 3;
  readback(tex, "[reader xproc]");
  return 0;
}

int main(int argc, char** argv) {
  if (argc < 2) { printf("usage: %s write|read\n", argv[0]); return 1; }
  if (make_device()) return 1;
  if (!strcmp(argv[1], "write")) return do_write();
  if (!strcmp(argv[1], "read")) return do_read();
  printf("unknown mode %s\n", argv[1]);
  return 1;
}
