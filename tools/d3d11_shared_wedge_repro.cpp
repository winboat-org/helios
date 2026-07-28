// d3d11_shared_wedge_repro.cpp — on-demand repro for ROADMAP WS1 defect 0w:
// creating a SHARED texture wedges the calling thread forever inside
// D3D11Initializer::SyncSharedTexture -> DxvkDevice::waitForResource.
//
// WHY THIS SHAPE. The wedged StartMenuExperienceHost thread died on exactly one
// call, recovered from its minidump and its UMD log:
//
//   create_resource(tex2d): 704x576 fmt=65 bind 0xa8->0x28 misc 0x2->0x802
//
// i.e. a 704x576 DXGI_FORMAT_A8_UNORM (65) texture, BIND_SHADER_RESOURCE |
// BIND_RENDER_TARGET, MISC_SHARED | MISC_SHARED_NTHANDLE — a XAML glyph atlas.
//
// THE MECHANISM AND THE RACE. Our fork (not upstream) re-tracks every shared
// image with DxvkAccess::Write into each NEW command list, via
// DxvkContext::acquireSharedImagesFromExternal (the QFOT re-acquire). So:
//
//   list N   : initImage writes the new texture -> tracked Write in N
//   flush    : endCurrentCommands releases it to EXTERNAL, submits N,
//              beginCurrentCommands re-acquires it -> tracked Write in N+1
//   N retires: N's refs released; N+1 still holds one Write ref and is OPEN
//   waiter   : waitForResource(image, Write) can never be satisfied
//
// The race is whether the CS thread has executed initImage BEFORE the caller
// reaches waitForResource. ExecuteFlush only *injects* the chunk; it does not
// wait. If the caller wins, the image is not tracked yet, isInUse(Write) is
// false and the wait is trivially satisfied (no wedge, and no real
// synchronisation either). If the CS thread wins, the caller blocks forever.
// That is why this is rare on an idle box and why it bit a XAML startup during
// a shell-restart storm — so this probe deliberately manufactures preemption
// pressure with --contend/--threads to lose that race on purpose.
//
// Build (VM, WinLibs g++):
//   g++ -O2 -o d3d11_shared_wedge_repro.exe d3d11_shared_wedge_repro.cpp \
//       -ld3d11 -ldxgi -ldxguid
//
// Run (session 0 is fine — no window, no desktop needed):
//   d3d11_shared_wedge_repro.exe [--iters N] [--threads N] [--contend N]
//                                [--watchdog-ms N] [--hold] [--clear] [--keep]
//                                [--width W] [--height H] [--fmt N] [--misc 0xN]
//     --hold : on wedge, STAY blocked so the hung process can be minidumped
//              (tools\take-minidump.ps1) and its stack compared against the
//              StartMenuExperienceHost one.
//
// Exit codes: 0 = all creates returned (no wedge), 2 = wedged, 1 = setup error.

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi.h>

#include <atomic>
#include <cstdarg>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <thread>
#include <vector>

namespace {

constexpr int kMaxThreads = 64;

std::atomic<long long> g_createStartMs[kMaxThreads];  // 0 = not in a create
std::atomic<int>       g_createIter[kMaxThreads];
std::atomic<bool>      g_done{ false };
std::atomic<bool>      g_wedged{ false };
std::atomic<bool>      g_stopSpin{ false };
std::atomic<int>       g_completed{ 0 };

long long nowMs() {
  static LARGE_INTEGER freq = [] { LARGE_INTEGER f; QueryPerformanceFrequency(&f); return f; }();
  LARGE_INTEGER c;
  QueryPerformanceCounter(&c);
  return (c.QuadPart * 1000LL) / freq.QuadPart;
}

CRITICAL_SECTION g_logLock;

// Unbuffered, timestamped, flushed on every line: a wedge means the process
// stops mid-line, so a buffered log would lose the very evidence we want.
void logf(const char* fmt, ...) {
  char buf[1024];
  va_list ap;
  va_start(ap, fmt);
  vsnprintf(buf, sizeof(buf), fmt, ap);
  va_end(ap);
  EnterCriticalSection(&g_logLock);
  printf("[%8lld ms] %s\n", nowMs(), buf);
  fflush(stdout);
  LeaveCriticalSection(&g_logLock);
}

} // namespace

int main(int argc, char** argv) {
  InitializeCriticalSection(&g_logLock);

  int  iters      = 64;
  int  threads    = 4;
  int  contend    = 32;
  int  watchdogMs = 15000;
  bool hold       = false;
  bool keep       = false;
  bool clearRtv   = false;
  UINT width      = 704;
  UINT height     = 576;
  UINT fmt        = 65;     // DXGI_FORMAT_A8_UNORM, as the wedged call used
  UINT misc       = 0x802;  // SHARED | SHARED_NTHANDLE

  for (int i = 1; i < argc; i++) {
    if      (!strcmp(argv[i], "--iters")       && i + 1 < argc) iters      = atoi(argv[++i]);
    else if (!strcmp(argv[i], "--threads")     && i + 1 < argc) threads    = atoi(argv[++i]);
    else if (!strcmp(argv[i], "--contend")     && i + 1 < argc) contend    = atoi(argv[++i]);
    else if (!strcmp(argv[i], "--watchdog-ms") && i + 1 < argc) watchdogMs = atoi(argv[++i]);
    else if (!strcmp(argv[i], "--width")       && i + 1 < argc) width      = (UINT)atoi(argv[++i]);
    else if (!strcmp(argv[i], "--height")      && i + 1 < argc) height     = (UINT)atoi(argv[++i]);
    else if (!strcmp(argv[i], "--fmt")         && i + 1 < argc) fmt        = (UINT)atoi(argv[++i]);
    else if (!strcmp(argv[i], "--misc")        && i + 1 < argc) misc       = (UINT)strtoul(argv[++i], nullptr, 0);
    else if (!strcmp(argv[i], "--hold"))                       hold       = true;
    else if (!strcmp(argv[i], "--keep"))                       keep       = true;
    else if (!strcmp(argv[i], "--clear"))                      clearRtv   = true;
    else { printf("unknown arg: %s\n", argv[i]); return 1; }
  }
  if (threads < 1) threads = 1;
  if (threads > kMaxThreads) threads = kMaxThreads;

  for (int i = 0; i < kMaxThreads; i++) { g_createStartMs[i].store(0); g_createIter[i].store(-1); }

  logf("repro start: iters=%d threads=%d contend=%d watchdog=%dms %ux%u fmt=%u misc=0x%x clear=%d keep=%d",
       iters, threads, contend, watchdogMs, width, height, fmt, misc, (int)clearRtv, (int)keep);

  D3D_FEATURE_LEVEL got = {};
  ID3D11Device*        dev = nullptr;
  ID3D11DeviceContext* ctx = nullptr;
  D3D_FEATURE_LEVEL want[] = { D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0 };

  HRESULT hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, 0,
                                 want, 2, D3D11_SDK_VERSION, &dev, &got, &ctx);
  if (FAILED(hr)) {
    logf("D3D11CreateDevice FAILED hr=0x%08lx", (unsigned long)hr);
    return 1;
  }
  logf("device created, feature level 0x%x", (unsigned)got);

  // Name the adapter so the transcript records which driver was exercised —
  // a repro that silently ran on WARP would prove nothing.
  IDXGIDevice*  dxgiDev = nullptr;
  IDXGIAdapter* adapter = nullptr;
  if (SUCCEEDED(dev->QueryInterface(__uuidof(IDXGIDevice), (void**)&dxgiDev)) &&
      SUCCEEDED(dxgiDev->GetAdapter(&adapter))) {
    DXGI_ADAPTER_DESC ad = {};
    adapter->GetDesc(&ad);
    char name[128] = {};
    WideCharToMultiByte(CP_UTF8, 0, ad.Description, -1, name, sizeof(name) - 1, nullptr, nullptr);
    logf("adapter: %s (vendor 0x%04x device 0x%04x)", name, ad.VendorId, ad.DeviceId);
  }
  if (adapter) adapter->Release();
  if (dxgiDev) dxgiDev->Release();

  // Preemption pressure. The wedge needs the CS thread to execute initImage
  // before the caller reaches waitForResource; oversubscribing the CPU makes
  // the caller lose that race often instead of almost never.
  std::vector<std::thread> spinners;
  for (int i = 0; i < contend; i++) {
    spinners.emplace_back([] {
      volatile unsigned long long x = 0;
      while (!g_stopSpin.load(std::memory_order_relaxed))
        for (int k = 0; k < 100000; k++) x += k;
    });
  }

  // Watchdog: the wedge is an unbounded condition-variable wait, so the only
  // way this process reports anything is another thread noticing that a create
  // has been in flight far longer than any healthy one (~15 ms).
  std::thread watchdog([&] {
    while (!g_done.load()) {
      Sleep(250);
      for (int t = 0; t < kMaxThreads; t++) {
        long long start = g_createStartMs[t].load();
        if (!start) continue;
        long long elapsed = nowMs() - start;
        if (elapsed > watchdogMs && !g_wedged.exchange(true)) {
          logf("*** WEDGED *** thread %d: CreateTexture2D iteration %d blocked for %lld ms",
               t, g_createIter[t].load(), elapsed);
          logf("*** pid=%lu — minidump it now:", (unsigned long)GetCurrentProcessId());
          logf("***   powershell -File Z:\\tools\\take-minidump.ps1 -ProcessId %lu -Path C:\\ProgramData\\HeliosDumps\\wedge.dmp",
               (unsigned long)GetCurrentProcessId());
          if (!hold) {
            logf("exiting with code 2 (pass --hold to stay blocked for a dump)");
            fflush(stdout);
            TerminateProcess(GetCurrentProcess(), 2);
          }
        }
      }
    }
  });

  std::vector<std::thread> workers;
  for (int t = 0; t < threads; t++) {
    workers.emplace_back([&, t] {
      std::vector<ID3D11Texture2D*> kept;
      for (int i = 0; i < iters && !g_wedged.load(); i++) {
        D3D11_TEXTURE2D_DESC td = {};
        td.Width          = width;
        td.Height         = height;
        td.MipLevels      = 1;
        td.ArraySize      = 1;
        td.Format         = (DXGI_FORMAT)fmt;
        td.SampleDesc     = { 1, 0 };
        td.Usage          = D3D11_USAGE_DEFAULT;
        td.BindFlags      = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET;
        td.CPUAccessFlags = 0;
        td.MiscFlags      = misc;

        ID3D11Texture2D* tex = nullptr;
        g_createIter[t].store(i);
        g_createStartMs[t].store(nowMs());

        long long t0 = nowMs();
        HRESULT chr = dev->CreateTexture2D(&td, nullptr, &tex);
        long long dt = nowMs() - t0;

        g_createStartMs[t].store(0);

        if (FAILED(chr)) {
          logf("thread %d iter %d: CreateTexture2D FAILED hr=0x%08lx after %lld ms",
               t, i, (unsigned long)chr, dt);
          continue;  // a refusal is a legitimate answer, not a wedge
        }

        g_completed.fetch_add(1);
        if (dt > 100)
          logf("thread %d iter %d: SLOW create %lld ms", t, i, dt);

        if (clearRtv && tex) {
          ID3D11RenderTargetView* rtv = nullptr;
          if (SUCCEEDED(dev->CreateRenderTargetView(tex, nullptr, &rtv)) && rtv) {
            const FLOAT black[4] = { 0.0f, 0.0f, 0.0f, 0.0f };
            ctx->ClearRenderTargetView(rtv, black);
            rtv->Release();
          }
        }

        // Deliberately no flush and no present: the open command list must
        // stay open, exactly as it does on an idle XAML UI thread.
        if (keep) kept.push_back(tex);
        else if (tex) tex->Release();
      }
      for (auto* p : kept) if (p) p->Release();
    });
  }

  for (auto& w : workers) w.join();

  g_done.store(true);
  g_stopSpin.store(true);
  watchdog.join();
  for (auto& s : spinners) s.join();

  logf("done: %d creates returned (target %d), wedged=%d",
       g_completed.load(), iters * threads, (int)g_wedged.load());

  if (ctx) ctx->Release();
  if (dev) dev->Release();
  return g_wedged.load() ? 2 : 0;
}
