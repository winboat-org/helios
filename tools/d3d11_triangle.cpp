// d3d11_triangle.cpp — minimal instrumented windowed D3D11 app for debugging
// the "windowed D3D11 renders transparent" defect (ROADMAP priority #1).
//
// Strips away every DXUT/FaceWorks confound: a real on-screen HWND, an explicit
// adapter/output/swap-effect choice, a clear-to-BLUE + GREEN-triangle draw, and
// an OPTIONAL self-readback of the backbuffer BEFORE Present that separates
// "did the app render?" from "did DWM composite the window?".
//
//   working window  = solid blue field with a green triangle
//   the defect       = transparent / black client area (but readback non-black)
//
// Build (VM, WinLibs g++):
//   g++ -O2 -o d3d11_triangle.exe d3d11_triangle.cpp \
//       -ld3d11 -ldxgi -ld3dcompiler -ldxguid -lgdi32 -luser32
//
// Run (MUST be session 1 — schtasks /IT; session 0 has no desktop):
//   d3d11_triangle.exe [adapter] [swap] [seconds]
//     adapter : "default" (nullptr) | index N | "helios" (first name match) |
//               "heliosout" (the Helios adapter whose EnumOutputs succeeds)
//     swap    : "flip" (FLIP_DISCARD, default) | "blt" (legacy DISCARD)
//     seconds : run duration before auto-exit (default 20)
//   Logs to stdout AND C:\Users\Rupansh\d3d11_triangle.txt
//
// Capture the result with helios_capture_faceworks (CopyFromScreen ->
// Z:\tmp\screen_copy.png) or move/inspect the window titled "helios-tri".
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_6.h>
#include <d3dcompiler.h>
#include <cstdio>
#include <cstring>
#include <cstdint>
#include <cstdarg>
#include <cstdlib>
#include <cwchar>

static FILE* g_log = nullptr;
static void L(const char* fmt, ...) {
  va_list ap; va_start(ap, fmt);
  char buf[1024]; vsnprintf(buf, sizeof(buf), fmt, ap); va_end(ap);
  fputs(buf, stdout); fputc('\n', stdout); fflush(stdout);
  if (g_log) { fputs(buf, g_log); fputc('\n', g_log); fflush(g_log); }
}

static LRESULT CALLBACK WndProc(HWND h, UINT m, WPARAM w, LPARAM l) {
  if (m == WM_DESTROY) { PostQuitMessage(0); return 0; }
  return DefWindowProc(h, m, w, l);
}

// Pick the adapter per the "adapter" arg; also report each adapter's output count.
static IDXGIAdapter1* PickAdapter(IDXGIFactory1* f, const char* sel) {
  IDXGIAdapter1* chosen = nullptr;
  int wantIndex = (sel && sel[0] >= '0' && sel[0] <= '9') ? atoi(sel) : -1;
  bool wantHelios    = sel && _stricmp(sel, "helios") == 0;
  bool wantHeliosOut = sel && _stricmp(sel, "heliosout") == 0;
  IDXGIAdapter1* a = nullptr;
  for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 d{}; a->GetDesc1(&d);
    UINT outs = 0; IDXGIOutput* o = nullptr;
    for (UINT j = 0; a->EnumOutputs(j, &o) != DXGI_ERROR_NOT_FOUND; ++j) { if (o) { outs++; o->Release(); o = nullptr; } }
    L("adapter[%u] luid=%08lx:%08lx flags=0x%x outputs=%u name=%ls",
      i, (unsigned long)d.AdapterLuid.HighPart, (unsigned long)d.AdapterLuid.LowPart,
      (unsigned)d.Flags, outs, d.Description);
    bool isHelios = wcsstr(d.Description, L"Helios") != nullptr;
    bool take = ((int)i == wantIndex) || (wantHelios && isHelios && !chosen) ||
                (wantHeliosOut && isHelios && outs > 0 && !chosen);
    if (take && !chosen) { chosen = a; a->AddRef(); L("  -> SELECTED"); }
    a->Release(); a = nullptr;
  }
  return chosen; // nullptr => let D3D11 pick the default
}

int main(int argc, char** argv) {
  g_log = fopen("C:\\Users\\Rupansh\\d3d11_triangle.txt", "w");
  const char* selAdapter = argc > 1 ? argv[1] : "default";
  const char* selSwap    = argc > 2 ? argv[2] : "flip";
  int seconds            = argc > 3 ? atoi(argv[3]) : 20;
  bool useFlip = _stricmp(selSwap, "blt") != 0;
  L("d3d11_triangle pid=%lu adapter=%s swap=%s seconds=%d",
    GetCurrentProcessId(), selAdapter, selSwap, seconds);

  // --- window ---
  WNDCLASSA wc = {}; wc.lpfnWndProc = WndProc; wc.hInstance = GetModuleHandle(nullptr);
  wc.lpszClassName = "heliostri"; wc.hCursor = LoadCursor(nullptr, IDC_ARROW);
  RegisterClassA(&wc);
  RECT rc = {100, 100, 100 + 1280, 100 + 720};
  AdjustWindowRect(&rc, WS_OVERLAPPEDWINDOW, FALSE);
  HWND hwnd = CreateWindowA("heliostri", "helios-tri", WS_OVERLAPPEDWINDOW,
    rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top,
    nullptr, nullptr, wc.hInstance, nullptr);
  ShowWindow(hwnd, SW_SHOW); UpdateWindow(hwnd);
  L("hwnd=%p rect=%ld,%ld", hwnd, rc.left, rc.top);

  // --- factory + adapter ---
  IDXGIFactory2* factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory2), (void**)&factory);
  L("CreateDXGIFactory1 hr=0x%08x", (unsigned)hr);
  IDXGIAdapter1* adapter = PickAdapter((IDXGIFactory1*)factory, selAdapter);

  // --- device ---
  D3D_FEATURE_LEVEL lv[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1};
  D3D_FEATURE_LEVEL got{};
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  hr = D3D11CreateDevice(adapter, adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE,
    nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, lv, _countof(lv), D3D11_SDK_VERSION, &dev, &got, &ctx);
  L("D3D11CreateDevice hr=0x%08x level=0x%x", (unsigned)hr, (unsigned)got);
  if (FAILED(hr)) return 2;

  // --- swapchain (flip-model or legacy blt) ---
  DXGI_SWAP_CHAIN_DESC1 sd = {};
  sd.Width = 1280; sd.Height = 720; sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  sd.SampleDesc.Count = 1; sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
  sd.BufferCount = useFlip ? 2 : 1;
  sd.SwapEffect = useFlip ? DXGI_SWAP_EFFECT_FLIP_DISCARD : DXGI_SWAP_EFFECT_DISCARD;
  sd.AlphaMode = DXGI_ALPHA_MODE_IGNORE;
  IDXGISwapChain1* sc = nullptr;
  hr = factory->CreateSwapChainForHwnd(dev, hwnd, &sd, nullptr, nullptr, &sc);
  L("CreateSwapChainForHwnd(%s) hr=0x%08x", useFlip ? "FLIP_DISCARD" : "BLT_DISCARD", (unsigned)hr);
  if (FAILED(hr)) return 3;
  factory->MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);

  // --- shaders (float2 pos triangle, solid green PS) ---
  const char* vs = "float4 main(float2 p:POS):SV_Position{return float4(p,0,1);}";
  const char* ps = "float4 main():SV_Target{return float4(0.15,0.9,0.25,1);}";
  ID3DBlob *vsb=nullptr,*psb=nullptr,*err=nullptr;
  if (FAILED(D3DCompile(vs,strlen(vs),0,0,0,"main","vs_4_0",0,0,&vsb,&err))) { L("vs compile fail %s", err?(char*)err->GetBufferPointer():""); return 4; }
  if (FAILED(D3DCompile(ps,strlen(ps),0,0,0,"main","ps_4_0",0,0,&psb,&err))) { L("ps compile fail"); return 5; }
  ID3D11VertexShader* vsh=nullptr; ID3D11PixelShader* psh=nullptr;
  dev->CreateVertexShader(vsb->GetBufferPointer(),vsb->GetBufferSize(),0,&vsh);
  dev->CreatePixelShader(psb->GetBufferPointer(),psb->GetBufferSize(),0,&psh);
  D3D11_INPUT_ELEMENT_DESC ied={"POS",0,DXGI_FORMAT_R32G32_FLOAT,0,0,D3D11_INPUT_PER_VERTEX_DATA,0};
  ID3D11InputLayout* il=nullptr;
  dev->CreateInputLayout(&ied,1,vsb->GetBufferPointer(),vsb->GetBufferSize(),&il);
  float tri[6] = {0.0f,0.7f, 0.7f,-0.7f, -0.7f,-0.7f};
  D3D11_BUFFER_DESC bd={}; bd.ByteWidth=sizeof(tri); bd.Usage=D3D11_USAGE_IMMUTABLE; bd.BindFlags=D3D11_BIND_VERTEX_BUFFER;
  D3D11_SUBRESOURCE_DATA sr={tri,0,0}; ID3D11Buffer* vb=nullptr; dev->CreateBuffer(&bd,&sr,&vb);

  // --- render loop ---
  ID3D11RenderTargetView* rtv=nullptr;
  auto makeRTV=[&](){ if(rtv){rtv->Release();rtv=nullptr;} ID3D11Texture2D* bb=nullptr;
    if(SUCCEEDED(sc->GetBuffer(0,__uuidof(ID3D11Texture2D),(void**)&bb))){ dev->CreateRenderTargetView(bb,nullptr,&rtv); bb->Release(); } };
  makeRTV();
  const float blue[4] = {0.05f, 0.10f, 0.55f, 1.0f};
  ULONGLONG t0 = GetTickCount64(); UINT frame = 0; bool readbackDone = false;
  MSG msg = {};
  while (true) {
    while (PeekMessage(&msg,nullptr,0,0,PM_REMOVE)) { if(msg.message==WM_QUIT) goto done; TranslateMessage(&msg); DispatchMessage(&msg); }
    if (GetTickCount64() - t0 > (ULONGLONG)seconds*1000) break;

    ctx->OMSetRenderTargets(1,&rtv,nullptr);
    ctx->ClearRenderTargetView(rtv, blue);
    D3D11_VIEWPORT vp={0,0,1280,720,0,1}; ctx->RSSetViewports(1,&vp);
    UINT stride=8,off=0; ctx->IASetVertexBuffers(0,1,&vb,&stride,&off);
    ctx->IASetInputLayout(il); ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    ctx->VSSetShader(vsh,0,0); ctx->PSSetShader(psh,0,0);
    ctx->Draw(3,0);

    // One-shot self-readback AFTER the draw, BEFORE Present: proves the app
    // rendered content independent of whether the window composites.
    if (!readbackDone && frame == 30) {
      readbackDone = true;
      ID3D11Texture2D* bb=nullptr;
      if (SUCCEEDED(sc->GetBuffer(0,__uuidof(ID3D11Texture2D),(void**)&bb))) {
        D3D11_TEXTURE2D_DESC td={}; bb->GetDesc(&td);
        td.Usage=D3D11_USAGE_STAGING; td.BindFlags=0; td.CPUAccessFlags=D3D11_CPU_ACCESS_READ; td.MiscFlags=0;
        ID3D11Texture2D* stg=nullptr;
        if (SUCCEEDED(dev->CreateTexture2D(&td,nullptr,&stg))) {
          ctx->CopyResource(stg,bb);
          D3D11_MAPPED_SUBRESOURCE mp={};
          if (SUCCEEDED(ctx->Map(stg,0,D3D11_MAP_READ,0,&mp))) {
            const uint8_t* p=(const uint8_t*)mp.pData;
            auto px=[&](int x,int y){ const uint8_t* q=p+(size_t)y*mp.RowPitch+(size_t)x*4; return (unsigned)(q[0]|q[1]<<8|q[2]<<16|q[3]<<24); };
            L("SELF-READBACK frame=%u center(640,360)=0x%08x corner(20,20)=0x%08x (blue=~0xff8c1a0d, green tri=~0xff40e526)",
              frame, px(640,360), px(20,20));
            ctx->Unmap(stg,0);
          } else L("SELF-READBACK map failed");
          stg->Release();
        }
        bb->Release();
      }
    }

    hr = sc->Present(1,0);
    if (frame < 4 || (frame % 240)==0) L("Present #%u hr=0x%08x", frame, (unsigned)hr);
    frame++;
  }
done:
  L("exit after %u frames", frame);
  return 0;
}
