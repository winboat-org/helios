// Native Windows D3D12 flip-discard presentation and full RGBA readback probe.
// Build: tools/build-d3d12-present-probe.ps1 (x86, x64, or both).
// Usage: d3d12-present.exe [--queued] [seconds [width height [flags]]]
// Defaults: 15 seconds, primary-screen dimensions, flags=1.
// flags: bit 0 = borderless/topmost; bit 1 = swapchain UAV usage.
// Run in the interactive desktop session, capturing stdout and the exit code.
// Exit 0 requires every frame's readback and orderly teardown to succeed.
// A desktop screenshot is STILL required: readback does not prove scanout.
// The bottom 8 pixels encode the frame's low 16 bits in black/white cells
// (least significant bit at the left), detecting stale readbacks and frames.
// --self-test checks the pattern geometry and pixel grader without using a GPU.
// --queued keeps a separate allocator/list/readback per backbuffer and only
// waits when that slot is reused or during the final drain. Its per-frame QPC
// records correlate the visible serial with Present and the exact app fence;
// those fences do not prove DWM/scanout completion or visible frame ordering.
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <d3d12.h>
#include <dxgi1_4.h>
#include <algorithm>
#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cwchar>
#include <exception>
#include <limits>
#include <vector>

struct Failure {};
static void require(bool condition, const char* operation) {
    if (!condition) {
        std::printf("FAIL %s (Win32=%lu)\n", operation, GetLastError());
        throw Failure{};
    }
}
static void check(HRESULT hr, const char* operation) {
    if (hr != S_OK) {
        // Success statuses such as DXGI_STATUS_OCCLUDED are not visible acceptance.
        std::printf("FAIL %s HRESULT=0x%08lx\n", operation, static_cast<unsigned long>(hr));
        throw Failure{};
    }
}
#define CHECK(expression) check((expression), #expression)

struct Rgba { uint8_t r, g, b, a; };
static_assert(sizeof(Rgba) == 4, "RGBA8 pixel size");
static constexpr Rgba bars[] = {
    {255,0,0,255}, {0,255,0,255}, {0,0,255,255}, {255,255,0,255},
    {255,0,255,255}, {0,255,255,255}, {255,255,255,255}, {128,128,128,255}
};
static Rgba expected(int x, int y, int width, int height, uint64_t serial) {
    if (y >= std::max(0,height-8)) {
        const uint8_t level = serial & (uint64_t{1} << (x*16/width)) ? 255 : 0;
        return {level,level,level,255};
    }
    if (x / 64 == y / 64) return {255,255,255,255};
    if (y < height / 2) return bars[(x / 128) % 8];
    if ((y / 32) & 1) return {255,128,0,255};
    return {40,40,40,255};
}

// Geometry and the analytic pixel oracle are deliberately separate. Rectangles
// are clipped, including an orange band crossing an odd/non-aligned half-height.
template<class Clear> static void pattern(int width, int height, uint64_t serial, Clear clear) {
    clear(Rgba{40,40,40,255}, D3D12_RECT{0,0,width,height});
    for (int x = 0; x < width; x += 128)
        if (height / 2)
            clear(bars[(x / 128) % 8], D3D12_RECT{x,0,std::min(x+128,width),height/2});
    for (int y = 32; y < height; y += 64) {
        const int top = std::max(y, height / 2);
        const int bottom = std::min(y + 32, height);
        if (top < bottom) clear(Rgba{255,128,0,255}, D3D12_RECT{0,top,width,bottom});
    }
    for (int p = 0; p < std::min(width, height); p += 64)
        clear(Rgba{255,255,255,255}, D3D12_RECT{p,p,std::min(p+64,width),std::min(p+64,height)});
    for (int bit=0; bit<16; ++bit) {
        const int left=(bit*width+15)/16, right=((bit+1)*width+15)/16;
        const uint8_t level=serial & (uint64_t{1} << bit) ? 255 : 0;
        if (left < right)
            clear(Rgba{level,level,level,255},D3D12_RECT{left,std::max(0,height-8),right,height});
    }
}

static uint64_t compare_pixels(const uint8_t* data, size_t pitch, int width, int height,
                               uint64_t serial, bool report_first) {
    uint64_t bad = 0;
    for (int y = 0; y < height; ++y) {
        for (int x = 0; x < width; ++x) {
            const auto* actual = data + static_cast<size_t>(y) * pitch + static_cast<size_t>(x) * 4;
            const Rgba want = expected(x, y, width, height, serial);
            if (actual[0] != want.r || actual[1] != want.g || actual[2] != want.b || actual[3] != want.a) {
                if (!bad && report_first)
                    std::printf("FAIL pixel (%d,%d): actual=%u,%u,%u,%u expected=%u,%u,%u,%u\n",
                        x,y,actual[0],actual[1],actual[2],actual[3],want.r,want.g,want.b,want.a);
                ++bad;
            }
        }
    }
    return bad;
}

static int self_test() {
    // Includes partial squares, bars, and half-height inside an orange band;
    // padding also verifies that the grader uses the supplied row pitch.
    const int dimensions[][2] = {{1,1},{63,65},{65,95},{129,97},{191,193},{1280,800},{1025,769}};
    for (const auto& size : dimensions) {
        const int w = size[0], h = size[1];
        const size_t pitch = static_cast<size_t>(w) * 4 + 28;
        std::vector<uint8_t> pixels(pitch * h, 0xcc);
        constexpr uint64_t serial=0xa55a;
        pattern(w, h, serial, [&](Rgba color, D3D12_RECT r) {
            for (LONG y = r.top; y < r.bottom; ++y)
                for (LONG x = r.left; x < r.right; ++x) {
                    auto* p = pixels.data() + static_cast<size_t>(y) * pitch + static_cast<size_t>(x) * 4;
                    p[0]=color.r; p[1]=color.g; p[2]=color.b; p[3]=color.a;
                }
        });
        require(compare_pixels(pixels.data(),pitch,w,h,serial,true) == 0, "pattern geometry oracle");
        require(compare_pixels(pixels.data(),pitch,w,h,serial ^ 1,false) != 0, "stale frame detected");
        for (unsigned channel = 0; channel < 4; ++channel) {
            pixels[channel] ^= 1;
            require(compare_pixels(pixels.data(),pitch,w,h,serial,false) == 1, "single-channel corruption detected");
            pixels[channel] ^= 1;
        }
    }
    std::printf("PASS CPU self-test: 7 dimensions; exact RGBA, padded rows, corruption/stale-frame detection\n");
    return 0;
}

static bool close_requested;
static LRESULT CALLBACK window_proc(HWND window, UINT message, WPARAM wparam, LPARAM lparam) {
    if (message == WM_CLOSE || (message == WM_KEYDOWN && wparam == VK_ESCAPE)) {
        close_requested = true; // Destroy only after the queue and swapchain drain.
        return 0;
    }
    if (message == WM_DESTROY) { PostQuitMessage(0); return 0; }
    return DefWindowProcW(window, message, wparam, lparam);
}
static void pump_messages() {
    MSG message{};
    while (PeekMessageW(&message,nullptr,0,0,PM_REMOVE)) {
        if (message.message == WM_QUIT) close_requested = true;
        else { TranslateMessage(&message); DispatchMessageW(&message); }
    }
}
template<class T> static ULONG release(T*& object, const char* name) {
    if (object) {
        std::printf("cleanup begin %s\n", name);
        const ULONG refs = object->Release();
        object = nullptr;
        std::printf("cleanup end %s Release=%lu\n", name, refs);
        return refs;
    }
    return 0;
}

struct Probe {
    static constexpr UINT buffer_count = 3;
    static constexpr DWORD wait_ms = 10000;
    int width, height, flags;
    bool queued;
    HMODULE d3d12{}, dxgi{};
    HINSTANCE instance{};
    ATOM window_class{};
    HWND window{};
    HANDLE event{};
    IDXGIFactory4* factory{};
    IDXGIAdapter1* adapter{};
    ID3D12Device* device{};
    ID3D12CommandQueue* queue{};
    IDXGISwapChain1* swapchain1{};
    IDXGISwapChain3* swapchain{};
    ID3D12DescriptorHeap* heap{};
    ID3D12Resource* buffers[buffer_count]{};
    ID3D12Resource* readback{};
    ID3D12CommandAllocator* allocator{};
    ID3D12GraphicsCommandList* list{};
    ID3D12Fence* fence{};
    D3D12_CPU_DESCRIPTOR_HANDLE rtvs[buffer_count]{};
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT footprint{};
    SIZE_T readback_size{};
    UINT64 fence_value{};
    uint64_t frames{}, checked_pixels{};
    UINT buffers_seen{};
    bool in_flight{};
    struct FrameSlot {
        ID3D12Resource* readback{};
        ID3D12CommandAllocator* allocator{};
        ID3D12GraphicsCommandList* list{};
        UINT64 completion{};
        uint64_t serial{};
    };
    FrameSlot slots[buffer_count]{};
    uint64_t checked_frames{};
    UINT outstanding{}, max_outstanding{};

    explicit Probe(int w, int h, int f, bool q) : width(w), height(h), flags(f), queued(q) {}

    static LONGLONG qpc() {
        LARGE_INTEGER now{};
        require(QueryPerformanceCounter(&now) != FALSE,"QueryPerformanceCounter");
        return now.QuadPart;
    }

    static HMODULE load_runtime(const wchar_t* name) {
        HMODULE module = LoadLibraryExW(name,nullptr,LOAD_LIBRARY_SEARCH_SYSTEM32);
        require(module != nullptr, "LoadLibraryExW system runtime");
        wchar_t path[MAX_PATH]{};
        const DWORD length = GetModuleFileNameW(module,path,MAX_PATH);
        if (!length || length >= MAX_PATH) {
            FreeLibrary(module);
            require(false,"GetModuleFileNameW runtime");
        }
        std::printf("runtime %ls\n", path);
        return module;
    }

    void initialize() {
        DWORD session = 0;
        require(ProcessIdToSessionId(GetCurrentProcessId(),&session) != FALSE, "ProcessIdToSessionId");
        require(session != 0, "interactive session required (session 0 is not presentation evidence)");
        std::printf("START pid=%lu bits=%zu session=%lu size=%dx%d flags=%d\n",
            GetCurrentProcessId(),sizeof(void*)*8,session,width,height,flags);
        LARGE_INTEGER frequency{};
        require(QueryPerformanceFrequency(&frequency) != FALSE,"QueryPerformanceFrequency");
        FILETIME utc{};
        GetSystemTimePreciseAsFileTime(&utc);
        const ULONGLONG filetime=(static_cast<ULONGLONG>(utc.dwHighDateTime)<<32)|utc.dwLowDateTime;
        std::printf("CLOCK qpc=%lld frequency=%lld utc_filetime=%llu queued=%d\n",
            qpc(),frequency.QuadPart,filetime,queued);
        d3d12 = load_runtime(L"d3d12.dll");
        dxgi = load_runtime(L"dxgi.dll");
        const auto create_device = reinterpret_cast<PFN_D3D12_CREATE_DEVICE>(GetProcAddress(d3d12,"D3D12CreateDevice"));
        using CreateFactory = HRESULT(WINAPI*)(UINT,REFIID,void**);
        const auto create_factory = reinterpret_cast<CreateFactory>(GetProcAddress(dxgi,"CreateDXGIFactory2"));
        require(create_device && create_factory,"system D3D12/DXGI entry points");
        CHECK(create_factory(0,IID_PPV_ARGS(&factory)));
        DXGI_ADAPTER_DESC1 selected{};
        for (UINT i = 0; ; ++i) {
            const HRESULT hr = factory->EnumAdapters1(i,&adapter);
            if (hr == DXGI_ERROR_NOT_FOUND) break;
            CHECK(hr);
            CHECK(adapter->GetDesc1(&selected));
            if (!(selected.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) &&
                (std::wcsstr(selected.Description,L"Helios") || std::wcsstr(selected.Description,L"helios"))) break;
            release(adapter,"skipped adapter");
        }
        require(adapter != nullptr,"nonsoftware Helios adapter found");
        std::printf("adapter %ls LUID=%08lx:%08lx\n",selected.Description,
            static_cast<unsigned long>(selected.AdapterLuid.HighPart),selected.AdapterLuid.LowPart);
        CHECK(create_device(adapter,D3D_FEATURE_LEVEL_11_0,IID_PPV_ARGS(&device)));
        const LUID actual = device->GetAdapterLuid();
        require(actual.LowPart == selected.AdapterLuid.LowPart && actual.HighPart == selected.AdapterLuid.HighPart,
            "created device adapter LUID matches Helios");

        instance = GetModuleHandleW(nullptr);
        require(instance != nullptr,"GetModuleHandleW executable");
        WNDCLASSW wc{};
        wc.lpfnWndProc=window_proc; wc.hInstance=instance; wc.lpszClassName=L"HeliosD3D12PresentProbe";
        wc.hCursor=LoadCursorW(nullptr,IDC_ARROW);
        require(wc.hCursor != nullptr,"LoadCursorW");
        window_class=RegisterClassW(&wc);
        require(window_class != 0,"RegisterClassW");
        const DWORD style = flags & 1 ? WS_POPUP : WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        RECT r{0,0,width,height};
        require(AdjustWindowRect(&r,style,FALSE) != FALSE,"AdjustWindowRect");
        window=CreateWindowExW(flags & 1 ? WS_EX_TOPMOST : 0,wc.lpszClassName,
            L"Helios native D3D12 presentation",style,flags & 1 ? 0 : 40,flags & 1 ? 0 : 40,
            r.right-r.left,r.bottom-r.top,nullptr,nullptr,instance,nullptr);
        require(window != nullptr,"CreateWindowExW");
        RECT client{};
        require(GetClientRect(window,&client) && client.right == width && client.bottom == height,
            "physical window client dimensions match swapchain");
        ShowWindow(window,SW_SHOW);
        require(IsWindowVisible(window) != FALSE,"probe window visible");
        CHECK(factory->MakeWindowAssociation(window,DXGI_MWA_NO_ALT_ENTER));

        D3D12_COMMAND_QUEUE_DESC qd{}; qd.Type=D3D12_COMMAND_LIST_TYPE_DIRECT;
        CHECK(device->CreateCommandQueue(&qd,IID_PPV_ARGS(&queue)));
        DXGI_SWAP_CHAIN_DESC1 sd{};
        sd.Width=width; sd.Height=height; sd.Format=DXGI_FORMAT_R8G8B8A8_UNORM;
        sd.SampleDesc.Count=1; sd.BufferUsage=DXGI_USAGE_RENDER_TARGET_OUTPUT;
        if (flags & 2) sd.BufferUsage |= DXGI_USAGE_UNORDERED_ACCESS;
        sd.BufferCount=buffer_count; sd.SwapEffect=DXGI_SWAP_EFFECT_FLIP_DISCARD;
        sd.Scaling=DXGI_SCALING_NONE; sd.AlphaMode=DXGI_ALPHA_MODE_IGNORE;
        CHECK(factory->CreateSwapChainForHwnd(queue,window,&sd,nullptr,nullptr,&swapchain1));
        CHECK(swapchain1->QueryInterface(IID_PPV_ARGS(&swapchain)));
        release(swapchain1,"initial swapchain interface");
        D3D12_DESCRIPTOR_HEAP_DESC hd{}; hd.Type=D3D12_DESCRIPTOR_HEAP_TYPE_RTV; hd.NumDescriptors=buffer_count;
        CHECK(device->CreateDescriptorHeap(&hd,IID_PPV_ARGS(&heap)));
        const UINT stride=device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
        require(stride != 0,"RTV descriptor stride");
        for (UINT i=0; i<buffer_count; ++i) {
            CHECK(swapchain->GetBuffer(i,IID_PPV_ARGS(&buffers[i])));
            const auto desc=buffers[i]->GetDesc();
            require(desc.Width == static_cast<UINT64>(width) && desc.Height == static_cast<UINT>(height) &&
                desc.Format == sd.Format && desc.SampleDesc.Count == 1,"swapchain buffer dimensions and format");
            rtvs[i]=heap->GetCPUDescriptorHandleForHeapStart();
            rtvs[i].ptr += static_cast<SIZE_T>(i)*stride;
            device->CreateRenderTargetView(buffers[i],nullptr,rtvs[i]);
        }
        const auto desc=buffers[0]->GetDesc();
        UINT rows=0; UINT64 row_bytes=0, total=0;
        device->GetCopyableFootprints(&desc,0,1,0,&footprint,&rows,&row_bytes,&total);
        const UINT64 image_bytes=static_cast<UINT64>(footprint.Footprint.RowPitch)*(height-1) +
            static_cast<UINT64>(width)*4;
        require(rows == static_cast<UINT>(height) && row_bytes == static_cast<UINT64>(width)*4 &&
            footprint.Footprint.RowPitch >= row_bytes && footprint.Footprint.Width == static_cast<UINT>(width) &&
            footprint.Footprint.Height == static_cast<UINT>(height) && footprint.Footprint.Depth == 1 &&
            footprint.Footprint.Format == sd.Format && total <= std::numeric_limits<SIZE_T>::max() &&
            footprint.Offset <= total && image_bytes <= total-footprint.Offset,
            "copyable footprint fits mapped RGBA image and process address space");
        readback_size=static_cast<SIZE_T>(total);
        D3D12_HEAP_PROPERTIES hp{}; hp.Type=D3D12_HEAP_TYPE_READBACK;
        D3D12_RESOURCE_DESC rd{}; rd.Dimension=D3D12_RESOURCE_DIMENSION_BUFFER; rd.Width=total; rd.Height=1;
        rd.DepthOrArraySize=1; rd.MipLevels=1; rd.SampleDesc.Count=1; rd.Layout=D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
        const UINT slot_count=queued ? buffer_count : 1;
        for (UINT i=0; i<slot_count; ++i) {
            auto& slot=slots[i];
            CHECK(device->CreateCommittedResource(&hp,D3D12_HEAP_FLAG_NONE,&rd,D3D12_RESOURCE_STATE_COPY_DEST,
                nullptr,IID_PPV_ARGS(&slot.readback)));
            CHECK(device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT,IID_PPV_ARGS(&slot.allocator)));
            CHECK(device->CreateCommandList(0,D3D12_COMMAND_LIST_TYPE_DIRECT,slot.allocator,nullptr,IID_PPV_ARGS(&slot.list)));
            CHECK(slot.list->Close());
        }
        CHECK(device->CreateFence(0,D3D12_FENCE_FLAG_NONE,IID_PPV_ARGS(&fence)));
        event=CreateEventW(nullptr,FALSE,FALSE,nullptr);
        require(event != nullptr,"CreateEventW fence");
        CHECK(device->GetDeviceRemovedReason());
    }

    void wait_value(UINT64 value) {
        CHECK(fence->SetEventOnCompletion(value,event));
        const ULONGLONG deadline=GetTickCount64()+wait_ms;
        while (true) {
            const UINT64 completed=fence->GetCompletedValue();
            require(completed != UINT64_MAX,"fence reports device removal");
            if (completed >= value) break;
            const ULONGLONG now=GetTickCount64();
            require(now < deadline,"GPU fence timed out after 10000 ms");
            const DWORD status=MsgWaitForMultipleObjectsEx(1,&event,static_cast<DWORD>(deadline-now),
                QS_ALLINPUT,MWMO_INPUTAVAILABLE);
            if (status == WAIT_OBJECT_0+1) pump_messages();
            else require(status == WAIT_OBJECT_0,"GPU fence wait (timeout/Win32 failure)");
        }
        CHECK(device->GetDeviceRemovedReason());
    }

    void wait_gpu() {
        // The final drain also queues a fence operation whose lifetime matters.
        in_flight=true;
        CHECK(queue->Signal(fence,++fence_value));
        wait_value(fence_value);
        in_flight=false;
    }

    void retire_slot(UINT slot_index) {
        auto& slot=slots[slot_index];
        if (!slot.completion) return;
        wait_value(slot.completion);
        const D3D12_RANGE range{0,readback_size};
        void* mapped=nullptr;
        CHECK(slot.readback->Map(0,&range,&mapped));
        if (!mapped) {
            const D3D12_RANGE written{0,0}; slot.readback->Unmap(0,&written);
            require(false,"readback Map returned data");
        }
        const auto* pixels=static_cast<const uint8_t*>(mapped)+static_cast<SIZE_T>(footprint.Offset);
        const uint64_t bad=compare_pixels(pixels,footprint.Footprint.RowPitch,width,height,slot.serial,true);
        const D3D12_RANGE written{0,0}; slot.readback->Unmap(0,&written);
        require(bad == 0,"full-frame exact RGBA readback");
        ++checked_frames; checked_pixels += static_cast<uint64_t>(width)*height;
        require(outstanding != 0,"retired slot was outstanding");
        --outstanding;
        if (queued)
            std::printf("RETIRE serial=%llu slot=%u fence=%llu qpc=%lld mismatches=0\n",
                slot.serial,slot_index,slot.completion,qpc());
        slot.completion=0;
    }

    void frame() {
        const UINT index=swapchain->GetCurrentBackBufferIndex();
        require(index < buffer_count,"current backbuffer index");
        const UINT slot_index=queued ? index : 0;
        retire_slot(slot_index);
        auto& slot=slots[slot_index];
        allocator=slot.allocator;
        list=slot.list;
        readback=slot.readback;
        CHECK(allocator->Reset());
        CHECK(list->Reset(allocator,nullptr));
        D3D12_RESOURCE_BARRIER barrier{}; barrier.Type=D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
        barrier.Transition.pResource=buffers[index];
        barrier.Transition.Subresource=D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
        barrier.Transition.StateBefore=D3D12_RESOURCE_STATE_PRESENT;
        barrier.Transition.StateAfter=D3D12_RESOURCE_STATE_RENDER_TARGET;
        list->ResourceBarrier(1,&barrier);
        const uint64_t serial=frames+1;
        pattern(width,height,serial,[&](Rgba c,D3D12_RECT r) {
            const float color[4]={c.r/255.f,c.g/255.f,c.b/255.f,c.a/255.f};
            list->ClearRenderTargetView(rtvs[index],color,1,&r);
        });
        barrier.Transition.StateBefore=D3D12_RESOURCE_STATE_RENDER_TARGET;
        barrier.Transition.StateAfter=D3D12_RESOURCE_STATE_COPY_SOURCE;
        list->ResourceBarrier(1,&barrier);
        D3D12_TEXTURE_COPY_LOCATION dst{}; dst.pResource=readback;
        dst.Type=D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT; dst.PlacedFootprint=footprint;
        D3D12_TEXTURE_COPY_LOCATION src{}; src.pResource=buffers[index];
        src.Type=D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
        list->CopyTextureRegion(&dst,0,0,0,&src,nullptr);
        barrier.Transition.StateBefore=D3D12_RESOURCE_STATE_COPY_SOURCE;
        barrier.Transition.StateAfter=D3D12_RESOURCE_STATE_PRESENT;
        list->ResourceBarrier(1,&barrier);
        CHECK(list->Close());
        ID3D12CommandList* lists[]={list};
        in_flight=true;
        queue->ExecuteCommandLists(1,lists);
        const LONGLONG present_begin=qpc();
        CHECK(swapchain->Present(1,0));
        const LONGLONG present_end=qpc();
        CHECK(queue->Signal(fence,++fence_value));
        slot.completion=fence_value;
        slot.serial=serial;
        ++outstanding;
        max_outstanding=std::max(max_outstanding,outstanding);
        if (queued)
            std::printf("PRESENT serial=%llu buffer=%u fence=%llu begin_qpc=%lld end_qpc=%lld outstanding=%u gpu_completed=%llu\n",
                serial,index,fence_value,present_begin,present_end,outstanding,fence->GetCompletedValue());
        else {
            retire_slot(slot_index);
            in_flight=false;
        }
        ++frames;
        buffers_seen |= 1u << index;
        if (frames == 1 || frames % 60 == 0)
            std::printf("FRAME %llu buffer=%u checked_pixels=%llu mismatches=0 fence=%llu\n",
                frames,index,checked_pixels,fence_value);
    }

    void run(unsigned seconds) {
        const ULONGLONG start=GetTickCount64();
        do {
            pump_messages();
            require(!close_requested,"window closed before requested duration (cancelled)");
            require(IsWindowVisible(window) && !IsIconic(window),"probe window remains visible");
            frame();
            // The final frame can itself cross the duration deadline. Grade
            // cancellation/minimization observed during its fence wait as well.
            pump_messages();
            require(!close_requested,"window closed during presentation (cancelled)");
            require(IsWindowVisible(window) && !IsIconic(window),"probe window remains visible after frame");
        } while (GetTickCount64()-start < static_cast<ULONGLONG>(seconds)*1000);
        // Keep every slot's readback and allocator alive through its own exact
        // completion. A later producer signal is not a consumer-release token.
        for (UINT i=0; i<buffer_count; ++i) retire_slot(i);
        require(checked_frames == frames && outstanding == 0,"every submitted frame readback verified");
        if (queued) require(max_outstanding > 1,"queued control exercised multiple outstanding frame records");
        require(buffers_seen == (1u << buffer_count)-1,"all swapchain backbuffers presented and verified");
        std::printf("RENDER COMPLETE frames=%llu checked_pixels=%llu buffers=0x%x max_outstanding=%u; beginning teardown\n",
            frames,checked_pixels,buffers_seen,max_outstanding);
    }

    bool shutdown() {
        bool ok=true;
        if (queue && fence && event) {
            std::printf("cleanup begin final GPU drain\n");
            try { wait_gpu(); } catch (...) { ok=false; }
            std::printf("cleanup end final GPU drain success=%d\n",ok);
        }
        if (in_flight && device && SUCCEEDED(device->GetDeviceRemovedReason())) {
            // Completion is unknown on a live device. Releasing its resources
            // violates D3D12 lifetime rules; fail the process instead of reusing
            // memory or pretending orderly GPU teardown was established.
            std::printf("FAIL live GPU work did not drain; terminating without unsafe COM releases\n");
            TerminateProcess(GetCurrentProcess(),2);
            std::abort();
        }
        // The current-frame pointers are borrowed aliases of the owning slots.
        list=nullptr; allocator=nullptr; readback=nullptr;
        for (auto& slot : slots) {
            release(slot.list,"command list");
            release(slot.allocator,"command allocator");
            release(slot.readback,"readback buffer");
        }
        for (auto& buffer : buffers) release(buffer,"swapchain buffer");
        release(heap,"RTV heap");
        release(swapchain,"swapchain");
        release(swapchain1,"initial swapchain interface");
        release(queue,"command queue");
        release(fence,"fence");
        if (release(device,"device") != 0) {
            std::printf("FAIL final device reference count is not zero\n"); ok=false;
        }
        release(adapter,"adapter");
        release(factory,"factory");
        if (event) {
            if (!CloseHandle(event)) { std::printf("FAIL CloseHandle fence event: %lu\n",GetLastError()); ok=false; }
            event=nullptr;
        }
        if (window) {
            std::printf("cleanup begin DestroyWindow\n");
            if (!DestroyWindow(window)) { std::printf("FAIL DestroyWindow: %lu\n",GetLastError()); ok=false; }
            window=nullptr;
            std::printf("cleanup end DestroyWindow\n");
        }
        if (window_class && !UnregisterClassW(L"HeliosD3D12PresentProbe",instance)) {
            std::printf("FAIL UnregisterClassW: %lu\n",GetLastError()); ok=false;
        }
        if (dxgi && !FreeLibrary(dxgi)) { std::printf("FAIL FreeLibrary dxgi: %lu\n",GetLastError()); ok=false; }
        if (d3d12 && !FreeLibrary(d3d12)) { std::printf("FAIL FreeLibrary d3d12: %lu\n",GetLastError()); ok=false; }
        return ok;
    }
};

static int number(const wchar_t* text, int low, int high) {
    errno=0; wchar_t* end=nullptr;
    const long n=std::wcstol(text,&end,10);
    require(errno == 0 && end != text && !*end && n >= low && n <= high,"argument range/syntax");
    return static_cast<int>(n);
}
int wmain(int argc, wchar_t** argv) {
    setvbuf(stdout,nullptr,_IONBF,0);
    try {
        if (argc == 2 && !std::wcscmp(argv[1],L"--self-test")) return self_test();
        if (argc == 2 && !std::wcscmp(argv[1],L"--help")) {
            std::printf("Usage: d3d12-present.exe [--queued] [seconds [width height [flags]]]\n"
                "seconds=1..600 (default 15), dimensions=16..4096 (default screen)\n"
                "flags=0..3 (default 1): bit0 borderless/topmost, bit1 swapchain UAV usage\n"
                "--queued: per-backbuffer slots, deferred readback, per-frame Present/QPC records; at most 60 seconds\n"
                "--self-test: CPU-only geometry and grader checks\n"
                "Capture stdout, exit status, and a desktop screenshot while running.\n");
            return 0;
        }
        const bool queued=argc > 1 && !std::wcscmp(argv[1],L"--queued");
        if (queued) { --argc; ++argv; }
        require(argc == 1 || argc == 2 || argc == 4 || argc == 5,"argument count (use --help)");
        const int seconds=argc > 1 ? number(argv[1],1,queued ? 60 : 600) : 15;
        // A launcher/manifest can already have fixed the process awareness.
        // Set our window thread explicitly instead of failing with access denied.
        require(SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != nullptr,
            "SetThreadDpiAwarenessContext physical pixel coordinates");
        const int width=argc > 3 ? number(argv[2],16,4096) : GetSystemMetrics(SM_CXSCREEN);
        const int height=argc > 3 ? number(argv[3],16,4096) : GetSystemMetrics(SM_CYSCREEN);
        const int flags=argc > 4 ? number(argv[4],0,3) : 1;
        require(width >= 16 && width <= 4096 && height >= 16 && height <= 4096,"screen dimensions fit probe bounds");
        Probe probe(width,height,flags,queued);
        bool ok=false;
        try { probe.initialize(); probe.run(seconds); ok=true; }
        catch (const Failure&) {}
        catch (const std::exception& e) { std::printf("FAIL C++ exception: %s\n",e.what()); }
        catch (...) { std::printf("FAIL unexpected exception\n"); }
        if (!probe.shutdown()) ok=false;
        std::printf("%s native presentation/readback/teardown: frames=%llu checked_pixels=%llu buffers=0x%x\n",
            ok ? "PASS" : "FAIL",probe.frames,probe.checked_pixels,probe.buffers_seen);
        return ok ? 0 : 2;
    } catch (const Failure&) { return 2; }
      catch (const std::exception& e) { std::printf("FAIL C++ exception: %s\n",e.what()); return 2; }
}
