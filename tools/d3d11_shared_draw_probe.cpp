// d3d11_shared_draw_probe.cpp — do real pipeline DRAWS (not just clears/
// UpdateSubresource) land in a shared surface and propagate cross-device?
//
// 2026-07-03 (NVIDIA boot): dwm renders composition draws into its swapchain
// backbuffer (= the IddCx buffer, venus resid confirmed) yet the IDD samples
// all-zero. The shared-content probe passes with clears+UpdateSubresource, so
// the remaining write-side suspects are draw-specific: either draws don't
// rasterize at all on this stack, or dwm's specific pipelines rasterize
// nothing (the UMD's synthetic float input signatures vs dwm's R16G16_SINT
// vertex formats — VUID-Input-08733). This probe uses a CLEAN float pipeline:
//   dev1: shared BGRA RT 256x256, clear RED, draw a half-covering GREEN
//         triangle (float2 positions, cull off), Flush.
//   dev1 self-readback: inside-triangle texel green? outside red?
//   dev2: OpenSharedResource1 + staging readback of the same two texels.
// PASS = draws rasterize (dev1 green/red) and propagate (dev2 green/red);
// then dwm's SINT-input pipelines are the only standing suspect for the
// black composition.
//
// Build (VM, vcvars64):
//   cl /EHsc /W4 Z:\tools\d3d11_shared_draw_probe.cpp /link dxgi.lib d3d11.lib d3dcompiler.lib
#include <d3d11_4.h>
#include <dxgi1_6.h>
#include <d3dcompiler.h>
#include <cstdio>
#include <cwchar>

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

static HRESULT create_device(IDXGIAdapter1* adapter, ID3D11Device** device,
                             ID3D11DeviceContext** ctx) {
  const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0,
                                      D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0};
  D3D_FEATURE_LEVEL fl{};
  return D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                           D3D11_CREATE_DEVICE_BGRA_SUPPORT, levels,
                           _countof(levels), D3D11_SDK_VERSION, device, &fl, ctx);
}

static const char* kVs =
    "float4 main(float2 pos : POSITION) : SV_Position {"
    "  return float4(pos, 0.0, 1.0);"
    "}";
// Distinct value per component so the readback shows exactly WHICH components
// the translated store writes: full write → BGRA bytes (153,102,51,204) =
// dword 0xCC336699; an x-only write over the red clear → 0xFF330000.
static const char* kPs =
    "float4 main() : SV_Target {"
    "  return float4(0.2, 0.4, 0.6, 0.8);"
    "}";

// Readback two texels: (64,64) expected INSIDE the triangle after y-flip,
// (192,192) expected OUTSIDE (still the clear color).
static int readback(ID3D11Device* dev, ID3D11DeviceContext* ctx,
                    ID3D11Texture2D* src, const D3D11_TEXTURE2D_DESC& td,
                    const char* label, unsigned* inside, unsigned* outside) {
  D3D11_TEXTURE2D_DESC sd = td;
  sd.BindFlags = 0;
  sd.MiscFlags = 0;
  sd.Usage = D3D11_USAGE_STAGING;
  sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D* staging = nullptr;
  HRESULT hr = dev->CreateTexture2D(&sd, nullptr, &staging);
  if (FAILED(hr)) { printf("%s staging hr=0x%08x\n", label, (unsigned)hr); return 1; }
  ctx->CopyResource(staging, src);
  ctx->Flush();
  D3D11_MAPPED_SUBRESOURCE map{};
  hr = ctx->Map(staging, 0, D3D11_MAP_READ, 0, &map);
  if (FAILED(hr)) { printf("%s map hr=0x%08x\n", label, (unsigned)hr); staging->Release(); return 1; }
  const unsigned char* base = (const unsigned char*)map.pData;
  *inside = ((const unsigned*)(base + (size_t)64 * map.RowPitch))[64];
  *outside = ((const unsigned*)(base + (size_t)192 * map.RowPitch))[192];
  printf("%s: inside(64,64)=%08x outside(192,192)=%08x\n", label, *inside, *outside);
  ctx->Unmap(staging, 0);
  staging->Release();
  return 0;
}

int main() {
  IDXGIFactory1* factory = nullptr;
  if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) return 1;
  IDXGIAdapter1* helios = find_helios(factory);
  if (!helios) { printf("no Helios adapter\n"); return 2; }

  ID3D11Device *dev1 = nullptr, *dev2 = nullptr;
  ID3D11DeviceContext *ctx1 = nullptr, *ctx2 = nullptr;
  HRESULT hr = create_device(helios, &dev1, &ctx1);
  printf("dev1 hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr)) return 3;
  hr = create_device(helios, &dev2, &ctx2);
  printf("dev2 hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr)) return 4;

  D3D11_TEXTURE2D_DESC td{};
  td.Width = 256; td.Height = 256; td.MipLevels = 1; td.ArraySize = 1;
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  td.SampleDesc.Count = 1;
  td.Usage = D3D11_USAGE_DEFAULT;
  td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
  td.MiscFlags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED;
  ID3D11Texture2D* tex = nullptr;
  hr = dev1->CreateTexture2D(&td, nullptr, &tex);
  printf("CreateTexture2D hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr)) return 5;
  ID3D11RenderTargetView* rtv = nullptr;
  if (FAILED(dev1->CreateRenderTargetView(tex, nullptr, &rtv))) return 6;

  // Shaders + input layout (float2 POSITION — the clean, spec-legal shape).
  ID3DBlob *vsb = nullptr, *psb = nullptr, *err = nullptr;
  hr = D3DCompile(kVs, strlen(kVs), nullptr, nullptr, nullptr, "main", "vs_4_0", 0, 0, &vsb, &err);
  if (FAILED(hr)) { printf("vs compile hr=0x%08x %s\n", (unsigned)hr, err ? (char*)err->GetBufferPointer() : ""); return 7; }
  hr = D3DCompile(kPs, strlen(kPs), nullptr, nullptr, nullptr, "main", "ps_4_0", 0, 0, &psb, &err);
  if (FAILED(hr)) { printf("ps compile hr=0x%08x %s\n", (unsigned)hr, err ? (char*)err->GetBufferPointer() : ""); return 8; }
  ID3D11VertexShader* vs = nullptr;
  ID3D11PixelShader* ps = nullptr;
  if (FAILED(dev1->CreateVertexShader(vsb->GetBufferPointer(), vsb->GetBufferSize(), nullptr, &vs))) return 9;
  if (FAILED(dev1->CreatePixelShader(psb->GetBufferPointer(), psb->GetBufferSize(), nullptr, &ps))) return 10;
  D3D11_INPUT_ELEMENT_DESC ied{"POSITION", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0,
                               D3D11_INPUT_PER_VERTEX_DATA, 0};
  ID3D11InputLayout* layout = nullptr;
  hr = dev1->CreateInputLayout(&ied, 1, vsb->GetBufferPointer(), vsb->GetBufferSize(), &layout);
  printf("CreateInputLayout hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr)) return 11;

  // Half-covering triangle in clip space: (-1,-1) (-1,1) (1,1) → after the
  // y-flip this covers the TOP-LEFT region of the texture; texel (64,64) is
  // inside, (192,192) outside.
  const float verts[6] = {-1.f, -1.f, -1.f, 1.f, 1.f, 1.f};
  D3D11_BUFFER_DESC bd{};
  bd.ByteWidth = sizeof(verts);
  bd.Usage = D3D11_USAGE_IMMUTABLE;
  bd.BindFlags = D3D11_BIND_VERTEX_BUFFER;
  D3D11_SUBRESOURCE_DATA init{verts, 0, 0};
  ID3D11Buffer* vb = nullptr;
  if (FAILED(dev1->CreateBuffer(&bd, &init, &vb))) return 12;

  D3D11_RASTERIZER_DESC rd{};
  rd.FillMode = D3D11_FILL_SOLID;
  rd.CullMode = D3D11_CULL_NONE;
  rd.DepthClipEnable = TRUE;
  ID3D11RasterizerState* rs = nullptr;
  if (FAILED(dev1->CreateRasterizerState(&rd, &rs))) return 13;

  const float clearcol[4] = {1.f, 1.f, 1.f, 1.f};  // white: distinguishes partial-component writes
  ctx1->ClearRenderTargetView(rtv, clearcol);

  ctx1->OMSetRenderTargets(1, &rtv, nullptr);
  D3D11_VIEWPORT vp{0.f, 0.f, 256.f, 256.f, 0.f, 1.f};
  ctx1->RSSetViewports(1, &vp);
  ctx1->RSSetState(rs);
  UINT stride = 8, offset = 0;
  ctx1->IASetVertexBuffers(0, 1, &vb, &stride, &offset);
  ctx1->IASetInputLayout(layout);
  ctx1->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
  ctx1->VSSetShader(vs, nullptr, 0);
  ctx1->PSSetShader(ps, nullptr, 0);
  ctx1->Draw(3, 0);
  ID3D11RenderTargetView* nullrtv = nullptr;
  ctx1->OMSetRenderTargets(1, &nullrtv, nullptr);
  ctx1->Flush();

  unsigned in1 = 0, out1 = 0, in2 = 0, out2 = 0;
  if (readback(dev1, ctx1, tex, td, "[dev1 self]", &in1, &out1)) return 14;

  // ── dwm-shaped pass: DrawIndexed + R16G16_SINT positions + CB transform ──
  // dwm's composition quads bind SINT vertex formats against int shader
  // inputs and read transforms from constant buffers; reproduce that exact
  // shape over the bottom-right region (outside the float triangle).
  {
    static const char* kVsSint =
        "cbuffer cb0 : register(b0) { float4 scale_bias; };"
        "float4 main(int2 pos : POSITION) : SV_Position {"
        "  return float4(float2(pos) * scale_bias.xy + scale_bias.zw, 0.0, 1.0);"
        "}";
    static const char* kPsMagenta =
        "float4 main() : SV_Target {"
        "  return float4(1.0, 0.0, 1.0, 1.0);"  // magenta
        "}";
    ID3DBlob *vsb2 = nullptr, *psb2 = nullptr, *err2 = nullptr;
    HRESULT hr2 = D3DCompile(kVsSint, strlen(kVsSint), nullptr, nullptr, nullptr,
                             "main", "vs_4_0", 0, 0, &vsb2, &err2);
    if (FAILED(hr2)) { printf("sint vs compile hr=0x%08x %s\n", (unsigned)hr2, err2 ? (char*)err2->GetBufferPointer() : ""); return 21; }
    if (FAILED(D3DCompile(kPsMagenta, strlen(kPsMagenta), nullptr, nullptr, nullptr, "main", "ps_4_0", 0, 0, &psb2, &err2))) return 22;
    ID3D11VertexShader* vs2 = nullptr;
    ID3D11PixelShader* ps2 = nullptr;
    if (FAILED(dev1->CreateVertexShader(vsb2->GetBufferPointer(), vsb2->GetBufferSize(), nullptr, &vs2))) return 23;
    if (FAILED(dev1->CreatePixelShader(psb2->GetBufferPointer(), psb2->GetBufferSize(), nullptr, &ps2))) return 24;
    D3D11_INPUT_ELEMENT_DESC ied2{"POSITION", 0, DXGI_FORMAT_R16G16_SINT, 0, 0,
                                  D3D11_INPUT_PER_VERTEX_DATA, 0};
    ID3D11InputLayout* layout2 = nullptr;
    hr2 = dev1->CreateInputLayout(&ied2, 1, vsb2->GetBufferPointer(), vsb2->GetBufferSize(), &layout2);
    printf("sint CreateInputLayout hr=0x%08x\n", (unsigned)hr2);
    if (FAILED(hr2)) return 25;

    // Quad over pixel-space [128,128)..(256,256); the CB maps pixel coords to
    // clip space: clip = pixel * (2/256, -2/256) + (-1, 1).
    const short verts2[8] = {128, 128, 256, 128, 128, 256, 256, 256};
    D3D11_BUFFER_DESC vbd{};
    vbd.ByteWidth = sizeof(verts2);
    vbd.Usage = D3D11_USAGE_IMMUTABLE;
    vbd.BindFlags = D3D11_BIND_VERTEX_BUFFER;
    D3D11_SUBRESOURCE_DATA vinit{verts2, 0, 0};
    ID3D11Buffer* vb2 = nullptr;
    if (FAILED(dev1->CreateBuffer(&vbd, &vinit, &vb2))) return 26;

    const unsigned short indices[6] = {0, 1, 2, 2, 1, 3};
    D3D11_BUFFER_DESC ibd{};
    ibd.ByteWidth = sizeof(indices);
    ibd.Usage = D3D11_USAGE_IMMUTABLE;
    ibd.BindFlags = D3D11_BIND_INDEX_BUFFER;
    D3D11_SUBRESOURCE_DATA iinit{indices, 0, 0};
    ID3D11Buffer* ib = nullptr;
    if (FAILED(dev1->CreateBuffer(&ibd, &iinit, &ib))) return 27;

    const float cbdata[4] = {2.0f / 256.0f, -2.0f / 256.0f, -1.0f, 1.0f};
    D3D11_BUFFER_DESC cbd{};
    cbd.ByteWidth = sizeof(cbdata) * 4;  // 64-byte min alignment comfort
    cbd.Usage = D3D11_USAGE_IMMUTABLE;
    cbd.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
    float cbfull[16] = {};
    memcpy(cbfull, cbdata, sizeof(cbdata));
    D3D11_SUBRESOURCE_DATA cinit{cbfull, 0, 0};
    ID3D11Buffer* cb = nullptr;
    if (FAILED(dev1->CreateBuffer(&cbd, &cinit, &cb))) return 28;

    ctx1->OMSetRenderTargets(1, &rtv, nullptr);
    ctx1->RSSetViewports(1, &vp);
    ctx1->RSSetState(rs);
    UINT stride2 = 4, offset2 = 0;
    ctx1->IASetVertexBuffers(0, 1, &vb2, &stride2, &offset2);
    ctx1->IASetIndexBuffer(ib, DXGI_FORMAT_R16_UINT, 0);
    ctx1->IASetInputLayout(layout2);
    ctx1->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    ctx1->VSSetShader(vs2, nullptr, 0);
    ctx1->VSSetConstantBuffers(0, 1, &cb);
    ctx1->PSSetShader(ps2, nullptr, 0);
    ctx1->DrawIndexed(6, 0, 0);
    ctx1->OMSetRenderTargets(1, &nullrtv, nullptr);
    ctx1->Flush();

    unsigned in3 = 0, out3 = 0;
    // (192,192) is inside the SINT quad now; (64,64) still the float triangle.
    if (readback(dev1, ctx1, tex, td, "[dev1 after SINT-indexed-CB quad]", &in3, &out3)) return 29;
    // Expect magenta 0xFFFF00FF at (192,192).
    printf("SINT quad result: quad(192,192)=%08x %s\n", out3,
           out3 == 0xFFFF00FFu ? "PASS" : "FAIL");

    // ── textured pass (dwm's remaining shape): sample an SRV with src-over
    // blending, quad over pixels [0,128)..(128,256). Source texel = orange.
    static const char* kPsTex =
        "Texture2D t0 : register(t0);"
        "SamplerState s0 : register(s0);"
        "float4 main(float4 pos : SV_Position) : SV_Target {"
        "  return t0.Sample(s0, pos.xy / 256.0);"
        "}";
    ID3DBlob* psb3 = nullptr;
    if (FAILED(D3DCompile(kPsTex, strlen(kPsTex), nullptr, nullptr, nullptr, "main", "ps_4_0", 0, 0, &psb3, &err2))) return 30;
    ID3D11PixelShader* ps3 = nullptr;
    if (FAILED(dev1->CreatePixelShader(psb3->GetBufferPointer(), psb3->GetBufferSize(), nullptr, &ps3))) return 31;

    D3D11_TEXTURE2D_DESC srctd{};
    srctd.Width = 2; srctd.Height = 2; srctd.MipLevels = 1; srctd.ArraySize = 1;
    srctd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    srctd.SampleDesc.Count = 1;
    srctd.Usage = D3D11_USAGE_IMMUTABLE;
    srctd.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    const unsigned orange[4] = {0xFF0080FF, 0xFF0080FF, 0xFF0080FF, 0xFF0080FF};
    D3D11_SUBRESOURCE_DATA sinit{orange, 8, 0};
    ID3D11Texture2D* srctex = nullptr;
    if (FAILED(dev1->CreateTexture2D(&srctd, &sinit, &srctex))) return 32;
    ID3D11ShaderResourceView* srv = nullptr;
    if (FAILED(dev1->CreateShaderResourceView(srctex, nullptr, &srv))) return 33;
    D3D11_SAMPLER_DESC samd{};
    samd.Filter = D3D11_FILTER_MIN_MAG_MIP_POINT;
    samd.AddressU = samd.AddressV = samd.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
    samd.MaxLOD = D3D11_FLOAT32_MAX;
    ID3D11SamplerState* sam = nullptr;
    if (FAILED(dev1->CreateSamplerState(&samd, &sam))) return 34;

    // dwm-style premultiplied src-over blend.
    D3D11_BLEND_DESC bdsc{};
    bdsc.RenderTarget[0].BlendEnable = TRUE;
    bdsc.RenderTarget[0].SrcBlend = D3D11_BLEND_ONE;
    bdsc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
    bdsc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    bdsc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    bdsc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_INV_SRC_ALPHA;
    bdsc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
    bdsc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL;
    ID3D11BlendState* blend = nullptr;
    if (FAILED(dev1->CreateBlendState(&bdsc, &blend))) return 35;

    // Quad over pixel-space [0,128)..(128,256) via the SINT VS + CB.
    const short verts3[8] = {0, 128, 128, 128, 0, 256, 128, 256};
    D3D11_SUBRESOURCE_DATA vinit3{verts3, 0, 0};
    ID3D11Buffer* vb3 = nullptr;
    if (FAILED(dev1->CreateBuffer(&vbd, &vinit3, &vb3))) return 36;

    ctx1->OMSetRenderTargets(1, &rtv, nullptr);
    const float bf[4] = {1, 1, 1, 1};
    ctx1->OMSetBlendState(blend, bf, 0xFFFFFFFF);
    ctx1->RSSetViewports(1, &vp);
    ctx1->RSSetState(rs);
    ctx1->IASetVertexBuffers(0, 1, &vb3, &stride2, &offset2);
    ctx1->IASetIndexBuffer(ib, DXGI_FORMAT_R16_UINT, 0);
    ctx1->IASetInputLayout(layout2);
    ctx1->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    ctx1->VSSetShader(vs2, nullptr, 0);
    ctx1->VSSetConstantBuffers(0, 1, &cb);
    ctx1->PSSetShader(ps3, nullptr, 0);
    ctx1->PSSetShaderResources(0, 1, &srv);
    ctx1->PSSetSamplers(0, 1, &sam);
    ctx1->DrawIndexed(6, 0, 0);
    ctx1->OMSetRenderTargets(1, &nullrtv, nullptr);
    ctx1->OMSetBlendState(nullptr, bf, 0xFFFFFFFF);
    ctx1->Flush();

    // Textured quad readback: sample point (64, 192) inside it.
    {
      D3D11_TEXTURE2D_DESC sd2 = td;
      sd2.BindFlags = 0;
      sd2.MiscFlags = 0;
      sd2.Usage = D3D11_USAGE_STAGING;
      sd2.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
      ID3D11Texture2D* st2 = nullptr;
      if (SUCCEEDED(dev1->CreateTexture2D(&sd2, nullptr, &st2))) {
        ctx1->CopyResource(st2, tex);
        ctx1->Flush();
        D3D11_MAPPED_SUBRESOURCE m2{};
        if (SUCCEEDED(ctx1->Map(st2, 0, D3D11_MAP_READ, 0, &m2))) {
          unsigned v = ((const unsigned*)((const unsigned char*)m2.pData + (size_t)192 * m2.RowPitch))[64];
          printf("textured+blend quad(64,192)=%08x %s\n", v,
                 v == 0xFF0080FFu ? "PASS" : "FAIL");
          ctx1->Unmap(st2, 0);
        }
        st2->Release();
      }
    }
  }

  IDXGIResource1* res1 = nullptr;
  if (FAILED(tex->QueryInterface(IID_PPV_ARGS(&res1)))) return 15;
  HANDLE handle = nullptr;
  hr = res1->CreateSharedHandle(nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                nullptr, &handle);
  printf("CreateSharedHandle hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr) || !handle) return 16;
  ID3D11Device1* dev2_1 = nullptr;
  if (FAILED(dev2->QueryInterface(IID_PPV_ARGS(&dev2_1)))) return 17;
  ID3D11Texture2D* opened = nullptr;
  hr = dev2_1->OpenSharedResource1(handle, IID_PPV_ARGS(&opened));
  printf("OpenSharedResource1 hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr) || !opened) return 18;
  if (readback(dev2, ctx2, opened, td, "[dev2 opened]", &in2, &out2)) return 19;

  bool draw_ok = (in1 == 0xCC336699u) && (out1 == 0xFFFFFFFFu);
  bool prop_ok = (in2 == in1) && (out2 == out1);
  printf("RESULT: draw(dev1)=%s propagate(dev2)=%s\n",
         draw_ok ? "PASS" : "FAIL", prop_ok ? "PASS" : "FAIL");
  return (draw_ok && prop_ok) ? 0 : 20;
}
