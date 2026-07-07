#include <d3d11.h>
#include <d3dcompiler.h>
#include <dxgi1_2.h>
#include <windows.h>

#include <cstring>
#include <cstdio>

static ID3DBlob* compile_shader(const char* source, const char* entry, const char* target) {
  ID3DBlob* code = nullptr;
  ID3DBlob* err = nullptr;
  HRESULT hr = D3DCompile(source, strlen(source), nullptr, nullptr, nullptr,
                          entry, target, 0, 0, &code, &err);
  std::printf("D3DCompile %s hr=0x%08lx code=%p\n", target, (unsigned long)hr, code);
  if (err) {
    std::printf("compile log: %.*s\n", (int)err->GetBufferSize(),
                (const char*)err->GetBufferPointer());
    err->Release();
  }
  return SUCCEEDED(hr) ? code : nullptr;
}

int main() {
  std::printf("d3d11_tess_probe pid=%lu session=%lu\n",
              GetCurrentProcessId(), WTSGetActiveConsoleSessionId());

  IDXGIFactory1* factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&factory);
  std::printf("CreateDXGIFactory1 hr=0x%08lx factory=%p\n", (unsigned long)hr, factory);
  if (FAILED(hr))
    return 1;

  IDXGIAdapter1* adapter = nullptr;
  for (UINT i = 0; ; ++i) {
    IDXGIAdapter1* cur = nullptr;
    hr = factory->EnumAdapters1(i, &cur);
    if (hr == DXGI_ERROR_NOT_FOUND)
      break;
    DXGI_ADAPTER_DESC1 desc = {};
    cur->GetDesc1(&desc);
    wprintf(L"adapter[%u] %ls vendor=0x%04x device=0x%04x luid=%08x:%08x flags=0x%x\n",
            i, desc.Description, desc.VendorId, desc.DeviceId,
            desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart, desc.Flags);
    if (!adapter && desc.VendorId == 0x1af4 && desc.DeviceId == 0x1050)
      adapter = cur;
    else
      cur->Release();
  }
  if (!adapter) {
    std::printf("no Helios adapter found\n");
    factory->Release();
    return 2;
  }

  ID3D11Device* dev = nullptr;
  ID3D11DeviceContext* ctx = nullptr;
  D3D_FEATURE_LEVEL levels[] = { D3D_FEATURE_LEVEL_11_0 };
  D3D_FEATURE_LEVEL got = {};
  hr = D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0,
                         levels, 1, D3D11_SDK_VERSION, &dev, &got, &ctx);
  std::printf("D3D11CreateDevice hr=0x%08lx dev=%p level=0x%x\n",
              (unsigned long)hr, dev, (unsigned)got);
  if (FAILED(hr)) {
    adapter->Release();
    factory->Release();
    return 3;
  }

  const char* hs_source =
    "struct VSOut { float4 pos : SV_POSITION; };\n"
    "struct HSConst { float edges[3] : SV_TessFactor; float inside : SV_InsideTessFactor; };\n"
    "HSConst PatchConst(InputPatch<VSOut,3> patch, uint pid : SV_PrimitiveID) {\n"
    "  HSConst o; o.edges[0] = 2.0; o.edges[1] = 2.0; o.edges[2] = 2.0; o.inside = 2.0; return o;\n"
    "}\n"
    "[domain(\"tri\")][partitioning(\"integer\")][outputtopology(\"triangle_cw\")]\n"
    "[outputcontrolpoints(3)][patchconstantfunc(\"PatchConst\")]\n"
    "VSOut main(InputPatch<VSOut,3> patch, uint i : SV_OutputControlPointID, uint pid : SV_PrimitiveID) {\n"
    "  return patch[i];\n"
    "}\n";
  const char* ds_source =
    "struct VSOut { float4 pos : SV_POSITION; };\n"
    "struct HSConst { float edges[3] : SV_TessFactor; float inside : SV_InsideTessFactor; };\n"
    "[domain(\"tri\")]\n"
    "VSOut main(HSConst hs, const OutputPatch<VSOut,3> patch, float3 uvw : SV_DomainLocation) {\n"
    "  VSOut o; o.pos = patch[0].pos * uvw.x + patch[1].pos * uvw.y + patch[2].pos * uvw.z; return o;\n"
    "}\n";

  ID3DBlob* hs_blob = compile_shader(hs_source, "main", "hs_5_0");
  ID3DBlob* ds_blob = compile_shader(ds_source, "main", "ds_5_0");
  if (!hs_blob || !ds_blob)
    return 4;

  ID3D11HullShader* hs = nullptr;
  ID3D11DomainShader* ds = nullptr;
  hr = dev->CreateHullShader(hs_blob->GetBufferPointer(), hs_blob->GetBufferSize(), nullptr, &hs);
  std::printf("CreateHullShader hr=0x%08lx hs=%p\n", (unsigned long)hr, hs);
  HRESULT hr_ds = dev->CreateDomainShader(ds_blob->GetBufferPointer(), ds_blob->GetBufferSize(), nullptr, &ds);
  std::printf("CreateDomainShader hr=0x%08lx ds=%p\n", (unsigned long)hr_ds, ds);

  if (hs)
    hs->Release();
  if (ds)
    ds->Release();
  hs_blob->Release();
  ds_blob->Release();
  ctx->Release();
  dev->Release();
  adapter->Release();
  factory->Release();
  return SUCCEEDED(hr) && SUCCEEDED(hr_ds) ? 0 : 5;
}
