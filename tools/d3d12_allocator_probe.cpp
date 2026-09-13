// The probe builds with /W4 /WX; `fopen` is used for the file-backed diagnostic
// sink (stdout deadlocks the wrapper's undrained pipe, see `tracef`).
#define _CRT_SECURE_NO_WARNINGS
// Native D3D12/runtime fence acceptance. Run in the interactive guest session.
// Build: cl /EHsc /std:c++17 d3d12_sync_probe.cpp d3d12.lib dxgi.lib
// Each case requires both fence ordering and an exact GPU readback pattern.
#include <windows.h>
#include <tlhelp32.h>
#include "d3d12_native_identity.h"
#include <d3d12.h>
#include <dxgi1_6.h>
#include <wrl/client.h>
#include <cstdint>
#include <cstdio>
#include <cstdarg>
#include <cstdlib>
#include <string>
#include <vector>
#include <exception>
#include <utility>

using Microsoft::WRL::ComPtr;
static constexpr UINT words = 4096;
static constexpr UINT bytes = words * sizeof(uint32_t);
static constexpr uint32_t unwritten = 0xa5a5a5a5u;

static void check(HRESULT hr, const char *what) {
    if (FAILED(hr)) { std::fprintf(stderr, "FAIL %s hr=%08lx\n", what, hr); std::_Exit(1); }
}
struct Event {
    HANDLE handle = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    Event() { if (!handle) std::_Exit(2); }
    ~Event() { CloseHandle(handle); }
    void wait() { if (WaitForSingleObject(handle, 10000) != WAIT_OBJECT_0) {
        std::fprintf(stderr, "FAIL completion missing; timeout is not completion\n"); std::_Exit(1);
    } }
};

static void native_modules(bool admitted);

// ⛔ DIAGNOSTICS GO TO A FILE, NEVER TO STDOUT. The wrapper drains the probe's
// stdout only when the process exits, so more than one pipe buffer of output
// BLOCKS the probe: adding ~1k lines of per-epoch output turned a 1.8 s run into
// a 90 s timeout (measured 2026-09-13, twice - first unbuffered, then buffered).
// A file has no such limit, costs no scheduling perturbation, and survives the
// abort path below because that path flushes it.
static FILE *g_trace = nullptr;
static long long g_last_wait_us = -1;
static unsigned g_epoch = 0, g_type = 0;
static void tracef(const char *fmt, ...) {
    if (!g_trace) return;
    va_list ap;
    va_start(ap, fmt);
    std::vfprintf(g_trace, fmt, ap);
    va_end(ap);
}
static ComPtr<ID3D12Device> device() {
    ComPtr<IDXGIFactory4> factory;
    check(CreateDXGIFactory2(0, IID_PPV_ARGS(&factory)), "factory");
    ComPtr<IDXGIAdapter1> adapter;
    for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
        DXGI_ADAPTER_DESC1 desc{};
        check(adapter->GetDesc1(&desc), "adapter description");
        if (desc.VendorId == 0x1af4 && desc.DeviceId == 0x1050) {
            ComPtr<ID3D12Device> result;
            check(D3D12CreateDevice(adapter.Get(), D3D_FEATURE_LEVEL_12_1, IID_PPV_ARGS(&result)), "Helios device");
            std::printf("adapter LUID=%08lx:%08lx\n", desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart);
            std::puts("CAP,NativeFL12_1Admission,00000000");
            native_modules(true);
            return result;
        }
        adapter.Reset();
    }
    std::fprintf(stderr, "FAIL exact Helios PCI adapter absent\n"); std::_Exit(1);
}
static ComPtr<ID3D12Resource> buffer(ID3D12Device *dev, D3D12_HEAP_TYPE heap, D3D12_RESOURCE_STATES state) {
    D3D12_HEAP_PROPERTIES props{}; props.Type = heap;
    props.CreationNodeMask = props.VisibleNodeMask = 1;
    D3D12_RESOURCE_DESC desc{};
    desc.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER; desc.Width = bytes;
    desc.Height = desc.DepthOrArraySize = desc.MipLevels = 1;
    desc.SampleDesc.Count = 1; desc.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    ComPtr<ID3D12Resource> result;
    check(dev->CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, &desc, state, nullptr,
        IID_PPV_ARGS(&result)), "buffer");
    return result;
}
struct Queue {
    ComPtr<ID3D12CommandQueue> queue;
    ComPtr<ID3D12CommandAllocator> allocator;
    ComPtr<ID3D12GraphicsCommandList> list;
    Queue(ID3D12Device *dev, D3D12_COMMAND_LIST_TYPE type) {
        D3D12_COMMAND_QUEUE_DESC desc{}; desc.Type = type;
        check(dev->CreateCommandQueue(&desc, IID_PPV_ARGS(&queue)), "queue");
        check(dev->CreateCommandAllocator(type, IID_PPV_ARGS(&allocator)), "allocator");
        check(dev->CreateCommandList(0, type, allocator.Get(), nullptr, IID_PPV_ARGS(&list)), "list");
    }
    void submit() {
        check(list->Close(), "close list");
        ID3D12CommandList *lists[] = {list.Get()};
        queue->ExecuteCommandLists(1, lists);
    }
};
static ComPtr<ID3D12Fence> fence(ID3D12Device *dev, UINT64 value = 0,
    D3D12_FENCE_FLAGS flags = D3D12_FENCE_FLAG_NONE) {
    ComPtr<ID3D12Fence> result;
    check(dev->CreateFence(value, flags, IID_PPV_ARGS(&result)), "fence");
    return result;
}
static uint32_t pattern(UINT epoch, UINT index) { return 0x9e3779b9u * (epoch + 1) ^ index; }


static void require(bool condition, const char *what) { if (!condition) { std::fprintf(stderr, "FAIL,%s\n", what); std::_Exit(1); } }
static void module_line(const wchar_t *name, const wchar_t *path)
{
    char utf8_name[256], utf8_path[4 * MAX_PATH];
    require(WideCharToMultiByte(CP_UTF8, 0, name, -1, utf8_name, sizeof(utf8_name), nullptr, nullptr) != 0 &&
            WideCharToMultiByte(CP_UTF8, 0, path, -1, utf8_path, sizeof(utf8_path), nullptr, nullptr) != 0,
            "module path UTF-8 conversion");
    fprintf(stderr, "MODULE,%s,%s\n", utf8_name, utf8_path);
}

static void native_modules(bool admitted)
{
    wchar_t system[MAX_PATH];
    require(GetSystemDirectoryW(system, MAX_PATH) != 0, "GetSystemDirectory");
    HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, GetCurrentProcessId());
    require(snapshot != INVALID_HANDLE_VALUE, "module snapshot");
    bool d3d12 = false, core = false, dxgi = false, umd = false, icd = false, valid = true;
    MODULEENTRY32W entry{};
    entry.dwSize = sizeof(entry);
    for (BOOL more = Module32FirstW(snapshot, &entry); more; more = Module32NextW(snapshot, &entry)) {
        bool *required = nullptr;
        if (!_wcsicmp(entry.szModule, L"d3d12.dll")) required = &d3d12;
        if (!_wcsicmp(entry.szModule, L"D3D12Core.dll")) required = &core;
        if (!_wcsicmp(entry.szModule, L"dxgi.dll")) required = &dxgi;
        if (required) {
            *required = true;
            const std::wstring expected = std::wstring(system) + L"\\" + entry.szModule;
            valid &= _wcsicmp(expected.c_str(), entry.szExePath) == 0;
            module_line(entry.szModule, entry.szExePath);
        }
        if (helios_native_umd12_name(entry.szModule)) {
            valid &= !umd;
            umd = true;
            module_line(entry.szModule, entry.szExePath);
        }
        if (!_wcsnicmp(entry.szModule, L"vulkan_virtio", 13)) {
            icd = true;
            module_line(entry.szModule, entry.szExePath);
        }
        if (!_wcsicmp(entry.szModule, L"helios_vkd3d.dll") || !_wcsicmp(entry.szModule, L"d3d10warp.dll")) valid = false;
    }
    CloseHandle(snapshot);
    fprintf(stderr, "MODULES_PRESENT,d3d12=%u,core=%u,dxgi=%u,umd=%u,icd=%u,admitted=%u\n",
            static_cast<unsigned>(d3d12), static_cast<unsigned>(core), static_cast<unsigned>(dxgi),
            static_cast<unsigned>(umd), static_cast<unsigned>(icd), static_cast<unsigned>(admitted));
    require(valid && d3d12 && dxgi && (!admitted || (core && umd && icd)),
            "native runtime/Helios UMD/ICD module identity");
}


static void wait_done(ID3D12Device *dev, ID3D12Fence *done, UINT64 value) {
    Event event;
    check(done->SetEventOnCompletion(value, event.handle), "completion event");
    // ⛔ The wait DURATION is the datum that separates the two candidate causes of
    // `FAIL,GPU epoch pattern`: a wait that returns in microseconds while the GPU
    // copy takes milliseconds means the fence advanced at submission; a wait that
    // tracks the GPU means the fence is truthful and the readback path is at fault.
    // (Same question the UMD's `Umd12FenceSignalDelayUs` doc poses; that arm is
    // unobservable because the runtime never enters the fence DDIs.)
    LARGE_INTEGER freq, t0, t1;
    QueryPerformanceFrequency(&freq);
    QueryPerformanceCounter(&t0);
    event.wait();
    QueryPerformanceCounter(&t1);
    // ⛔ NO PER-EPOCH I/O: writing a line per epoch perturbs the very race under
    // test (24/24 passes with per-epoch tracing against ~1-in-6 failures without).
    // The value is kept in memory and reported only from the failure path.
    g_last_wait_us = (long long)((t1.QuadPart - t0.QuadPart) * 1000000 / freq.QuadPart);
    require(done->GetCompletedValue() == value, "exact authenticated completion");
    check(dev->GetDeviceRemovedReason(), "device health");
}
static void write_pattern(ID3D12Resource *upload, UINT epoch) {
    void *data = nullptr;
    D3D12_RANGE empty{};
    check(upload->Map(0, &empty, &data), "map upload");
    for (UINT i = 0; i < words; ++i) static_cast<uint32_t *>(data)[i] = pattern(epoch, i);
    upload->Unmap(0, nullptr);
}
static void verify(ID3D12Resource *readback, UINT epoch) {
    void *data = nullptr;
    D3D12_RANGE range{0, bytes}, empty{};
    check(readback->Map(0, &range, &data), "map readback");
    uint32_t *w = static_cast<uint32_t *>(data);
    UINT bad = 0, first = 0;
    for (UINT i = 0; i < words; ++i) {
        if (w[i] != pattern(epoch, i)) { if (!bad) first = i; ++bad; }
    }
    if (bad) {
        // ⛔ WHICH EPOCH'S DATA IS ACTUALLY THERE, and does it arrive late?
        // `pattern(e,0) = 0x9e3779b9 * (e+1)` carries the epoch in its first word,
        // so the observed content can be NAMED instead of merely called wrong:
        // "one epoch behind" is a CPU/GPU reuse race on the shared upload buffer,
        // garbage is a copy/binding fault, and a correct re-read after a delay is
        // "the GPU had not finished" - three different fixes.
        const uint32_t got0 = w[0];
        const long long observed = static_cast<long long>(got0 / 0x9e3779b9u) - 1;
        tracef("MISMATCH,type=%u,epoch=%u,bad=%u,first=%u,got0=0x%08x,want0=0x%08x,observed_epoch=%lld,exact=%d,last_wait_us=%lld\n",
               g_type, epoch, bad, first, got0, pattern(epoch, 0), observed,
               (got0 % 0x9e3779b9u) == 0 ? 1 : 0, g_last_wait_us);
        Sleep(100);
        UINT bad_after = 0;
        for (UINT i = 0; i < words; ++i)
            if (w[i] != pattern(epoch, i)) ++bad_after;
        tracef("REREAD,epoch=%u,bad_after_100ms=%u,got0_after=0x%08x,observed_after=%lld\n",
               epoch, bad_after, w[0], static_cast<long long>(w[0] / 0x9e3779b9u) - 1);
        std::fflush(g_trace);
    }
    readback->Unmap(0, &empty);
    require(!bad, "GPU epoch pattern");
}
static void run_reuse(ID3D12Device *dev, D3D12_COMMAND_LIST_TYPE type) {
    Queue q(dev, type);
    auto upload = buffer(dev, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    auto gpu = buffer(dev, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COPY_DEST);
    auto readback = buffer(dev, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    auto done = fence(dev);
    for (UINT epoch = 1; epoch <= 256; ++epoch) {
        write_pattern(upload.Get(), epoch);
        q.list->CopyBufferRegion(gpu.Get(), 0, upload.Get(), 0, bytes);
        D3D12_RESOURCE_BARRIER b{};
        b.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
        b.Transition = {gpu.Get(), D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                       D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE};
        q.list->ResourceBarrier(1, &b);
        q.list->CopyBufferRegion(readback.Get(), 0, gpu.Get(), 0, bytes);
        std::swap(b.Transition.StateBefore, b.Transition.StateAfter);
        q.list->ResourceBarrier(1, &b);
        g_epoch = epoch;
        g_type = unsigned(type);
        q.submit();
        check(q.queue->Signal(done.Get(), epoch), "queue completion");
        wait_done(dev, done.Get(), epoch);
        // Reset immediately after completion, before CPU readback work gives the
        // independent engine retirement thread extra scheduling opportunity.
        check(q.allocator->Reset(), "completed allocator reset");
        check(q.list->Reset(q.allocator.Get(), nullptr), "list reset");
        verify(readback.Get(), epoch);
    }
    check(q.list->Close(), "close final empty list");
    std::printf("PASS,allocator_reuse,type=%u,epochs=256,words=%u\n", unsigned(type), words);
}
static void run_shared_allocator(ID3D12Device *dev) {
    Queue a(dev, D3D12_COMMAND_LIST_TYPE_DIRECT), b(dev, D3D12_COMMAND_LIST_TYPE_DIRECT);
    check(b.list->Close(), "close second list");
    b.allocator = a.allocator;
    auto up = buffer(dev, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    auto rb1 = buffer(dev, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    auto rb2 = buffer(dev, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    auto done1 = fence(dev), done2 = fence(dev);
    // Recording two lists simultaneously on one allocator is invalid. Close
    // each before resetting/recording the other.
    for (UINT epoch = 1; epoch <= 64; ++epoch) {
        write_pattern(up.Get(), epoch + 300);
        a.list->CopyBufferRegion(rb1.Get(), 0, up.Get(), 0, bytes);
        a.submit();
        check(b.list->Reset(a.allocator.Get(), nullptr), "record second shared list");
        b.list->CopyBufferRegion(rb2.Get(), 0, up.Get(), 0, bytes);
        b.submit();
        check(a.queue->Signal(done1.Get(), epoch), "first queue completion");
        check(b.queue->Signal(done2.Get(), epoch), "second queue completion");
        wait_done(dev, done1.Get(), epoch); wait_done(dev, done2.Get(), epoch);
        check(a.allocator->Reset(), "both queues completed allocator reset");
        check(a.list->Reset(a.allocator.Get(), nullptr), "first shared list reset");
        verify(rb1.Get(), epoch + 300); verify(rb2.Get(), epoch + 300);
    }
    check(a.list->Close(), "close final shared list");
    std::puts("PASS,shared_allocator_two_queues,epochs=64");
}
static void run_pending_list_lifetime(ID3D12Device *dev) {
    Queue q(dev, D3D12_COMMAND_LIST_TYPE_DIRECT);
    auto up = buffer(dev, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    auto rb = buffer(dev, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    auto gate = fence(dev), done = fence(dev);
    write_pattern(up.Get(), 999);
    write_pattern(rb.Get(), 0); // sentinel differs at every word
    check(q.queue->Wait(gate.Get(), 1), "pending queue gate");
    q.list->CopyBufferRegion(rb.Get(), 0, up.Get(), 0, bytes);
    q.submit();
    check(q.queue->Signal(done.Get(), 1), "pending queue signal");
    Event event;
    check(done->SetEventOnCompletion(1, event.handle), "pending completion event");
    require(WaitForSingleObject(event.handle, 100) == WAIT_TIMEOUT && done->GetCompletedValue() == 0,
            "unsignaled dependency holds completion");
    verify(rb.Get(), 0);
    // List Reset with a different allocator is legal while the old execution
    // is pending. Never call allocator Reset on storage still in GPU use.
    ComPtr<ID3D12CommandAllocator> replacement;
    check(dev->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&replacement)), "replacement allocator");
    check(q.list->Reset(replacement.Get(), nullptr), "pending list reset");
    check(q.list->Close(), "close replacement list");
    q.allocator.Reset(); // release all public references to submitted storage
    check(gate->Signal(1), "release queue gate");
    wait_done(dev, done.Get(), 1);
    verify(rb.Get(), 999);
    std::puts("PASS,pending_list_reset_allocator_release");
}
int main() {
    // ⛔ STDOUT STAYS BUFFERED (an unbuffered `WAIT` per epoch perturbs the very
    // race under test: measured 2026-09-13, 1.8 s -> 11.7 s and three 90 s
    // timeouts), but the abort path FLUSHES first. `std::_Exit` does not, which is
    // why the failing run's own `stdout.txt` was 0 bytes and its per-type PASS
    // lines and timings were unrecoverable.
    g_trace = std::fopen("allocator-timing.log", "w");
    // No exception may unwind submitted resource owners before completion.
    std::set_terminate([] { if (g_trace) std::fflush(g_trace); std::fflush(stdout); std::_Exit(125); });
    auto dev = device();
    run_reuse(dev.Get(), D3D12_COMMAND_LIST_TYPE_DIRECT);
    run_reuse(dev.Get(), D3D12_COMMAND_LIST_TYPE_COMPUTE);
    run_reuse(dev.Get(), D3D12_COMMAND_LIST_TYPE_COPY);
    run_shared_allocator(dev.Get());
    run_pending_list_lifetime(dev.Get());
    std::puts("PASS,native_allocator_probe_completed");
    return 0;
}
