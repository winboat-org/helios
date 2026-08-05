// d3d11_rdp_capture_probe.cpp — measure the per-frame cost of the RDP /
// IddCx desktop-capture loop on the Helios adapter.
//
// WHY THIS EXISTS
// ---------------
// In an RDP session the whole desktop renders on Helios and is consumed by the
// RDP indirect display driver (RDPIDD) hosted in WUDFHost. Its Helios UMD log
// shows it creates exactly one resource of its own:
//
//   create_resource(tex2d): 1920x1080 fmt=87 usage=3 cpu=0x30000
//     fmt 87  = DXGI_FORMAT_B8G8R8A8_UNORM
//     usage 3 = D3D11_USAGE_STAGING
//     cpu     = D3D11_CPU_ACCESS_WRITE | D3D11_CPU_ACCESS_READ
//
// ...and OpenResource()s DWM's shared swapchain buffers. So every captured
// frame is: CopyResource(shared -> staging) ; Map(READ) ; read 8.3 MB ; Unmap.
// This probe replicates that loop exactly and splits the cost per phase, so
// "the RDP consumer burns a core during a window drag" becomes an attributed
// number instead of a description.
//
// The control matters as much as the measurement: the same byte count is
// memcpy'd heap->heap. If the mapped read is dramatically slower than the heap
// control, the staging allocation is not cached (WB) memory — and since RDPIDD
// is Microsoft's code doing an ordinary memcpy, the only fixable end is which
// memory type we back the staging resource with.
//
// The streaming-load pass is a discriminator, not a proposed fix: MOVNTDQA is
// dramatically faster than memcpy *only* on write-combined memory. If the
// stream pass beats the memcpy pass by a wide margin, the memory is WC.
//
// BUILD (cl under vcvars64; see tools/d3d11-rdp-capture-probe.ps1):
//   cl /nologo /EHsc /W4 /O2 d3d11_rdp_capture_probe.cpp \
//      /link d3d11.lib dxgi.lib /OUT:d3d11_rdp_capture_probe.exe
//
// Exit code: 0 on success, non-zero on setup failure.

#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <immintrin.h>

#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <vector>

#pragma comment(lib, "d3d11.lib")
#pragma comment(lib, "dxgi.lib")

namespace {

const UINT kWidth = 1920;
const UINT kHeight = 1080;
const UINT kBytesPerPixel = 4;

double g_qpcFreq = 0.0;

double NowMs() {
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    return (double)t.QuadPart * 1000.0 / g_qpcFreq;
}

double Pct(std::vector<double> v, double p) {
    if (v.empty()) return 0.0;
    std::sort(v.begin(), v.end());
    size_t i = (size_t)(p * (double)(v.size() - 1) + 0.5);
    return v[i];
}

void Report(const char* name, std::vector<double>& v, double bytesPerIter) {
    if (v.empty()) {
        printf("  %-24s (no samples)\n", name);
        return;
    }
    double p50 = Pct(v, 0.50);
    double p90 = Pct(v, 0.90);
    double mx = *std::max_element(v.begin(), v.end());
    double sum = 0.0;
    for (size_t i = 0; i < v.size(); ++i) sum += v[i];
    double mean = sum / (double)v.size();

    if (bytesPerIter > 0.0 && p50 > 0.0) {
        double mbps = (bytesPerIter / (1024.0 * 1024.0)) / (p50 / 1000.0);
        printf("  %-24s mean=%8.3f  p50=%8.3f  p90=%8.3f  max=%8.3f ms   [%8.1f MB/s @p50]\n",
               name, mean, p50, p90, mx, mbps);
    } else {
        printf("  %-24s mean=%8.3f  p50=%8.3f  p90=%8.3f  max=%8.3f ms\n",
               name, mean, p50, p90, mx);
    }
}

// Plain memcpy per row — exactly what an ordinary capture consumer does.
unsigned __int64 ReadRowsMemcpy(void* dst, const void* src, UINT rowBytes,
                                UINT rows, UINT srcPitch) {
    char* d = (char*)dst;
    const char* s = (const char*)src;
    for (UINT y = 0; y < rows; ++y)
        memcpy(d + (size_t)y * rowBytes, s + (size_t)y * srcPitch, rowBytes);
    // Touch the destination so the copy cannot be elided.
    return *(const unsigned __int64*)d;
}

// Non-temporal (streaming) loads. Only meaningfully faster than memcpy when
// the source is write-combined; used here purely to classify the memory.
unsigned __int64 ReadRowsStream(void* dst, const void* src, UINT rowBytes,
                                UINT rows, UINT srcPitch) {
    char* d = (char*)dst;
    const char* s = (const char*)src;
    __m128i acc = _mm_setzero_si128();
    for (UINT y = 0; y < rows; ++y) {
        const __m128i* sp = (const __m128i*)(s + (size_t)y * srcPitch);
        __m128i* dp = (__m128i*)(d + (size_t)y * rowBytes);
        UINT n = rowBytes / 16;
        for (UINT x = 0; x < n; ++x) {
            __m128i v = _mm_stream_load_si128((__m128i*)(sp + x));
            _mm_storeu_si128(dp + x, v);
            acc = _mm_xor_si128(acc, v);
        }
    }
    _mm_mfence();
    return (unsigned __int64)_mm_cvtsi128_si64(acc);
}

}  // namespace

int main(int argc, char** argv) {
    int iterations = 60;
    if (argc > 1) iterations = atoi(argv[1]);
    if (iterations < 4) iterations = 4;

    LARGE_INTEGER f;
    QueryPerformanceFrequency(&f);
    g_qpcFreq = (double)f.QuadPart;

    IDXGIFactory1* factory = nullptr;
    if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&factory))) {
        printf("FAIL: CreateDXGIFactory1\n");
        return 1;
    }

    IDXGIAdapter1* adapter = nullptr;
    IDXGIAdapter1* cand = nullptr;
    for (UINT i = 0; factory->EnumAdapters1(i, &cand) != DXGI_ERROR_NOT_FOUND; ++i) {
        DXGI_ADAPTER_DESC1 d;
        cand->GetDesc1(&d);
        if (d.VendorId == 0x1af4) {
            adapter = cand;
            wprintf(L"adapter: %s (vendor 0x%04x device 0x%04x)\n",
                    d.Description, d.VendorId, d.DeviceId);
            break;
        }
        cand->Release();
    }
    if (!adapter) {
        printf("FAIL: no virtio (0x1af4) adapter found\n");
        return 2;
    }

    ID3D11Device* dev = nullptr;
    ID3D11DeviceContext* ctx = nullptr;
    D3D_FEATURE_LEVEL want[] = { D3D_FEATURE_LEVEL_11_0 };
    D3D_FEATURE_LEVEL got;
    HRESULT hr = D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0,
                                   want, 1, D3D11_SDK_VERSION, &dev, &got, &ctx);
    if (FAILED(hr)) {
        printf("FAIL: D3D11CreateDevice hr=0x%08lx\n", (unsigned long)hr);
        return 3;
    }

    // The "composed desktop frame" DWM hands the IDD.
    D3D11_TEXTURE2D_DESC src = {};
    src.Width = kWidth;
    src.Height = kHeight;
    src.MipLevels = 1;
    src.ArraySize = 1;
    src.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    src.SampleDesc.Count = 1;
    src.Usage = D3D11_USAGE_DEFAULT;
    src.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;

    // RDPIDD's own staging texture, byte-for-byte the desc from its UMD log.
    D3D11_TEXTURE2D_DESC dst = {};
    dst.Width = kWidth;
    dst.Height = kHeight;
    dst.MipLevels = 1;
    dst.ArraySize = 1;
    dst.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    dst.SampleDesc.Count = 1;
    dst.Usage = D3D11_USAGE_STAGING;
    dst.BindFlags = 0;
    dst.CPUAccessFlags = D3D11_CPU_ACCESS_READ | D3D11_CPU_ACCESS_WRITE;

    ID3D11Texture2D* texSrc = nullptr;
    ID3D11Texture2D* texDst = nullptr;
    if (FAILED(dev->CreateTexture2D(&src, nullptr, &texSrc))) {
        printf("FAIL: CreateTexture2D(DEFAULT)\n");
        return 4;
    }
    if (FAILED(dev->CreateTexture2D(&dst, nullptr, &texDst))) {
        printf("FAIL: CreateTexture2D(STAGING cpu=READ|WRITE)\n");
        return 5;
    }

    ID3D11RenderTargetView* rtv = nullptr;
    if (FAILED(dev->CreateRenderTargetView(texSrc, nullptr, &rtv))) {
        printf("FAIL: CreateRenderTargetView\n");
        return 6;
    }

    const UINT rowBytes = kWidth * kBytesPerPixel;
    const size_t frameBytes = (size_t)rowBytes * kHeight;
    std::vector<char> out(frameBytes);
    std::vector<char> ctrlA(frameBytes), ctrlB(frameBytes);
    for (size_t i = 0; i < frameBytes; ++i) ctrlA[i] = (char)(i & 0x7f);

    printf("frame: %ux%u BGRA = %zu bytes, iterations=%d\n\n",
           kWidth, kHeight, frameBytes, iterations);

    std::vector<double> tCopy, tMap, tReadCpy, tReadStrm, tUnmap, tTotal;
    unsigned __int64 sink = 0;
    UINT reportedPitch = 0;
    void* firstPtr = nullptr;

    for (int i = 0; i < iterations; ++i) {
        FLOAT color[4] = { (float)(i % 16) / 16.0f, 0.25f, 0.75f, 1.0f };
        ctx->ClearRenderTargetView(rtv, color);

        double t0 = NowMs();
        ctx->CopyResource(texDst, texSrc);
        double t1 = NowMs();

        D3D11_MAPPED_SUBRESOURCE m = {};
        HRESULT mhr = ctx->Map(texDst, 0, D3D11_MAP_READ, 0, &m);
        double t2 = NowMs();
        if (FAILED(mhr)) {
            printf("FAIL: Map hr=0x%08lx at iter %d\n", (unsigned long)mhr, i);
            return 7;
        }
        if (i == 0) {
            reportedPitch = m.RowPitch;
            firstPtr = m.pData;
        }

        // Alternate the read strategy so both are measured against the same
        // steady state rather than in two separately-warmed phases.
        double t3;
        if (i & 1) {
            sink += ReadRowsMemcpy(out.data(), m.pData, rowBytes, kHeight, m.RowPitch);
            t3 = NowMs();
            if (i > 2) tReadCpy.push_back(t3 - t2);
        } else {
            sink += ReadRowsStream(out.data(), m.pData, rowBytes, kHeight, m.RowPitch);
            t3 = NowMs();
            if (i > 2) tReadStrm.push_back(t3 - t2);
        }

        ctx->Unmap(texDst, 0);
        double t4 = NowMs();

        if (i > 2) {  // drop warm-up
            tCopy.push_back(t1 - t0);
            tMap.push_back(t2 - t1);
            tUnmap.push_back(t4 - t3);
            tTotal.push_back(t4 - t0);
        }
    }

    // Control: same byte count, ordinary cached heap memory.
    std::vector<double> tCtrl;
    for (int i = 0; i < 16; ++i) {
        double c0 = NowMs();
        memcpy(ctrlB.data(), ctrlA.data(), frameBytes);
        double c1 = NowMs();
        sink += (unsigned __int64)ctrlB[i];
        tCtrl.push_back(c1 - c0);
    }

    printf("staging Map returned pitch=%u (tight=%u) ptr=%p\n\n",
           reportedPitch, rowBytes, firstPtr);

    printf("per-frame capture loop (CopyResource -> Map(READ) -> read -> Unmap):\n");
    Report("CopyResource", tCopy, 0.0);
    Report("Map(READ)", tMap, 0.0);
    Report("read: memcpy", tReadCpy, (double)frameBytes);
    Report("read: MOVNTDQA stream", tReadStrm, (double)frameBytes);
    Report("Unmap", tUnmap, 0.0);
    Report("TOTAL per frame", tTotal, 0.0);
    printf("\ncontrol (cached heap->heap, same byte count):\n");
    Report("memcpy heap->heap", tCtrl, (double)frameBytes);

    double capP50 = Pct(tTotal, 0.50);
    if (capP50 > 0.0)
        printf("\nimplied capture ceiling: %.1f fps (p50 total %.3f ms)\n",
               1000.0 / capP50, capP50);
    printf("checksum: %llu\n", (unsigned long long)sink);

    rtv->Release();
    texDst->Release();
    texSrc->Release();
    ctx->Release();
    dev->Release();
    adapter->Release();
    factory->Release();
    return 0;
}
