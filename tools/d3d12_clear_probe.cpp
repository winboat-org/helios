// d3d12_clear_probe.cpp -- D12-G8 rung 0: headless pixel correctness through the
// D3D12 DDI, with no swapchain and no present path involved.
//
// `GATES.md` 4.9 names this probe as rung 0 and states why it must not be
// skipped: *"a failure at rung 1 with no rung-0 result cannot be attributed."*
// Rung 1 puts a triangle on the composed desktop, which is DWM, the flip path,
// the KMD's scanout and `helios_paintcap` all at once. This probe is the control
// for every one of those: it renders into an offscreen texture, copies it back to
// CPU memory, and compares an integer. If rung 0 passes and rung 1 does not, the
// defect is in presentation. If rung 0 fails, nothing downstream is worth reading.
//
// ASCII only, on purpose: this file lives on the Z:\ 9p share, which the VM's
// tooling reads as ANSI.
//
// Build (on the VM, from an x64 developer prompt):
//   cl /nologo /EHsc /W4 Z:\tools\d3d12_clear_probe.cpp
//      /Fe:C:\Users\Rupansh\d12g8\clear.exe
//      /link d3d12.lib dxgi.lib dxguid.lib
//
// Usage:
//   clear.exe                        Helios, report only, exit 0
//   clear.exe --expect ok            exit 0 iff the readback pixel is exact
//   clear.exe --adapter warp         the CONTROL arm -- see below
//
// == WHY THERE ARE NO SHADERS IN HERE ==
//
// Deliberate, and it is the same argument `GATES.md` 4.9 makes for running
// `HelloWindow` before `HelloTriangle`: *"it has no shaders at all, so it
// isolates device/queue/present from the DXIL path entirely."* A clear is the
// smallest operation that still produces a pixel a human can check against a
// number, and it needs no PSO, no root signature, no DXIL and no vertex data. So
// a failure here cannot be the shader path, and that is most of what makes the
// result attributable.
//
// == WHAT IT ACTUALLY EXERCISES, which is most of the driver ==
//
//   CreateCommandQueue      -- and with it the WDDM context, which the runtime
//                              permits to be minted NOWHERE ELSE. Until this
//                              probe runs, `QueueContextFailed` reads 0 because
//                              no D3D12 queue has ever existed on this adapter,
//                              not because queue creation works.
//   CreateCommandAllocator  -- pfnCreateCommandPool + the recorder that gives it
//                              a class at first bind
//   CreateCommandList       -- and pfnSetCommandListDDITableCb, whose hRTTable
//                              index has never been exercised either
//   CreateDescriptorHeap, GetCPUDescriptorHandleForHeapStart, CreateRenderTargetView
//   CreateCommittedResource -- pfnCreateHeapAndResource, both arms
//   Reset / Close           -- the list-lifetime pair
//   ClearRenderTargetView   -- the pixel this probe is about
//   ResourceBarrier         -- the legacy arm, which is the one the shipping caps
//                              select
//   CopyTextureRegion       -- with a placed footprint, i.e. the DDI's
//                              texture-copy-location union in both forms
//   ExecuteCommandLists     -- the only submission entry point in the baseline set
//   CreateFence, Signal, SetEventOnCompletion
//   Map / Unmap             -- pfnMapHeap on a READBACK heap
//
// == THE CONTROL ARM ==
//
// `--adapter warp` runs the identical sequence against the Microsoft Basic
// Render Driver. It answers the one question a failing run cannot otherwise
// separate: *is the probe wrong, or is the driver?* WARP is a known-good D3D12
// implementation on this same machine and this same runtime, so a WARP failure is
// a probe bug and a Helios-only failure is a driver bug. Run it once when the
// probe is new or when the result is surprising; it costs a second.
//
// == THE NUMBER ==
//
// The clear colour is `GATES.md` 4.9's, taken from the dx-samples HelloWindow so
// that rung 0 and rung 2 are comparable without a second constant:
//
//     float  { 0.0f, 0.2f, 0.4f, 1.0f }   in DXGI_FORMAT_R8G8B8A8_UNORM
//     8-bit  (0, 51, 102, 255)
//
// 0.2 * 255 = 51.0 and 0.4 * 255 = 102.0 both land exactly on integers, so there
// is no rounding slack to argue about and PASS means EXACT. A +/-1 result is
// reported as a SOFT PASS with the deviation printed, because a one-LSB error is
// a real finding about the format conversion and must not be silently absorbed;
// anything wider fails.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

#include <d3d12.h>
#include <dxgi1_6.h>

#include <cstdio>
#include <cstring>
#include <cwchar>

namespace {

const wchar_t* kHeliosDescription = L"Helios";

// The render target. Small on purpose: the whole point is one exact pixel, and a
// 256x256 R8G8B8A8 surface is 256 KB, which keeps the readback copy trivial while
// still being large enough that the copy has a non-degenerate row pitch to get
// wrong (D3D12_TEXTURE_DATA_PITCH_ALIGNMENT is 256, and 256 * 4 = 1024, so the
// footprint pitch and the natural pitch coincide -- see the note at the readback).
const UINT kWidth = 256;
const UINT kHeight = 256;
const DXGI_FORMAT kFormat = DXGI_FORMAT_R8G8B8A8_UNORM;

const FLOAT kClearColour[4] = {0.0f, 0.2f, 0.4f, 1.0f};
const BYTE kExpectR = 0;
const BYTE kExpectG = 51;
const BYTE kExpectB = 102;
const BYTE kExpectA = 255;

enum class Expect { Report, Ok };
enum class Which { Helios, Warp };

void print_hr(const char* what, HRESULT hr) {
    printf("%-38s hr=0x%08lX\n", what, static_cast<unsigned long>(hr));
}

// Every step reports its own HRESULT and the first failure stops the run. A probe
// that carries on after a failed create reports a second, derived failure and
// hides which step was the real one.
bool step(const char* what, HRESULT hr) {
    print_hr(what, hr);
    return SUCCEEDED(hr);
}

template <typename T>
void release(T*& p) {
    if (p) {
        p->Release();
        p = nullptr;
    }
}

}  // namespace

int main(int argc, char** argv) {
    Expect expect = Expect::Report;
    Which which = Which::Helios;

    for (int i = 1; i < argc; ++i) {
        if (std::strcmp(argv[i], "--expect") == 0 && i + 1 < argc) {
            ++i;
            if (std::strcmp(argv[i], "ok") == 0) {
                expect = Expect::Ok;
            } else {
                printf("FAIL: --expect takes 'ok', got '%s'\n", argv[i]);
                return 2;
            }
        } else if (std::strcmp(argv[i], "--adapter") == 0 && i + 1 < argc) {
            ++i;
            if (std::strcmp(argv[i], "helios") == 0) {
                which = Which::Helios;
            } else if (std::strcmp(argv[i], "warp") == 0) {
                which = Which::Warp;
            } else {
                printf("FAIL: --adapter takes 'helios' or 'warp', got '%s'\n", argv[i]);
                return 2;
            }
        } else {
            printf("FAIL: unrecognised argument '%s'\n", argv[i]);
            return 2;
        }
    }

    printf("d3d12_clear_probe: adapter=%s expect=%s\n",
           which == Which::Warp ? "WARP (control)" : "Helios",
           expect == Expect::Ok ? "ok" : "report");
    printf("target %ux%u R8G8B8A8_UNORM, clear {%.1f, %.1f, %.1f, %.1f} -> (%u, %u, %u, %u)\n\n",
           kWidth, kHeight,
           kClearColour[0], kClearColour[1], kClearColour[2], kClearColour[3],
           kExpectR, kExpectG, kExpectB, kExpectA);

    IDXGIFactory4* factory = nullptr;
    if (!step("CreateDXGIFactory2", CreateDXGIFactory2(0, IID_PPV_ARGS(&factory)))) {
        return 2;
    }

    IDXGIAdapter1* adapter = nullptr;
    DXGI_ADAPTER_DESC1 adapter_desc = {};
    if (which == Which::Warp) {
        IDXGIAdapter* warp = nullptr;
        if (!step("EnumWarpAdapter", factory->EnumWarpAdapter(IID_PPV_ARGS(&warp)))) {
            release(factory);
            return 2;
        }
        HRESULT qi = warp->QueryInterface(IID_PPV_ARGS(&adapter));
        release(warp);
        if (!step("QueryInterface(IDXGIAdapter1)", qi)) {
            release(factory);
            return 2;
        }
        adapter->GetDesc1(&adapter_desc);
    } else {
        for (UINT i = 0;; ++i) {
            IDXGIAdapter1* candidate = nullptr;
            if (factory->EnumAdapters1(i, &candidate) == DXGI_ERROR_NOT_FOUND) {
                break;
            }
            DXGI_ADAPTER_DESC1 desc = {};
            candidate->GetDesc1(&desc);
            if (!adapter && std::wcsstr(desc.Description, kHeliosDescription) != nullptr) {
                adapter = candidate;
                adapter_desc = desc;
                continue;
            }
            candidate->Release();
        }
        if (!adapter) {
            printf("FAIL: no adapter whose description contains '%ws'\n", kHeliosDescription);
            release(factory);
            return 2;
        }
    }
    printf("%-38s %ws (luid=%08lX:%08lX)\n\n",
           "adapter",
           adapter_desc.Description,
           static_cast<unsigned long>(adapter_desc.AdapterLuid.HighPart),
           static_cast<unsigned long>(adapter_desc.AdapterLuid.LowPart));

    // Everything below is one straight line of D3D12 with no branches, so the
    // printed HRESULT trace IS the list of DDIs that were reached. A gate reading
    // this file wants to know how far it got, not only whether it finished.
    ID3D12Device* device = nullptr;
    ID3D12CommandQueue* queue = nullptr;
    ID3D12CommandAllocator* allocator = nullptr;
    ID3D12GraphicsCommandList* list = nullptr;
    ID3D12DescriptorHeap* rtv_heap = nullptr;
    ID3D12Resource* target = nullptr;
    ID3D12Resource* readback = nullptr;
    ID3D12Fence* fence = nullptr;
    HANDLE fence_event = nullptr;
    int rc = 2;

    do {
        if (!step("D3D12CreateDevice(FL 11_0)",
                  D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&device)))) {
            break;
        }

        // The queue, and with it the WDDM context. This is the first D3D12 queue
        // this adapter has ever been asked for.
        D3D12_COMMAND_QUEUE_DESC queue_desc = {};
        queue_desc.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
        queue_desc.Flags = D3D12_COMMAND_QUEUE_FLAG_NONE;
        if (!step("CreateCommandQueue(DIRECT)",
                  device->CreateCommandQueue(&queue_desc, IID_PPV_ARGS(&queue)))) {
            break;
        }
        if (!step("CreateCommandAllocator(DIRECT)",
                  device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT,
                                                 IID_PPV_ARGS(&allocator)))) {
            break;
        }
        if (!step("CreateCommandList(DIRECT)",
                  device->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, allocator,
                                            nullptr, IID_PPV_ARGS(&list)))) {
            break;
        }

        D3D12_DESCRIPTOR_HEAP_DESC heap_desc = {};
        heap_desc.Type = D3D12_DESCRIPTOR_HEAP_TYPE_RTV;
        heap_desc.NumDescriptors = 1;
        heap_desc.Flags = D3D12_DESCRIPTOR_HEAP_FLAG_NONE;
        if (!step("CreateDescriptorHeap(RTV)",
                  device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&rtv_heap)))) {
            break;
        }

        D3D12_HEAP_PROPERTIES default_heap = {};
        default_heap.Type = D3D12_HEAP_TYPE_DEFAULT;
        D3D12_RESOURCE_DESC target_desc = {};
        target_desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
        target_desc.Width = kWidth;
        target_desc.Height = kHeight;
        target_desc.DepthOrArraySize = 1;
        target_desc.MipLevels = 1;
        target_desc.Format = kFormat;
        target_desc.SampleDesc.Count = 1;
        target_desc.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
        target_desc.Flags = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;

        // ⚠ The optimized clear value MUST match the colour actually cleared to,
        // or the debug layer flags it and some drivers take a slow path. Passing
        // it also exercises the DDI's D3D12DDI_CLEAR_VALUE, which a null would
        // skip entirely.
        D3D12_CLEAR_VALUE clear_value = {};
        clear_value.Format = kFormat;
        std::memcpy(clear_value.Color, kClearColour, sizeof(kClearColour));

        // Created directly in RENDER_TARGET state, so the clear needs no barrier
        // before it. One barrier in this probe, not two, and the one that is here
        // is the one the copy requires.
        if (!step("CreateCommittedResource(target)",
                  device->CreateCommittedResource(&default_heap, D3D12_HEAP_FLAG_NONE,
                                                  &target_desc,
                                                  D3D12_RESOURCE_STATE_RENDER_TARGET,
                                                  &clear_value, IID_PPV_ARGS(&target)))) {
            break;
        }

        // The footprint the copy must use. ⛔ Asked for rather than computed: the
        // row pitch is the runtime's answer and hard-coding `width * 4` is how a
        // readback silently reads the wrong bytes on any format or alignment this
        // probe did not anticipate.
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT footprint = {};
        UINT64 readback_bytes = 0;
        UINT rows = 0;
        UINT64 row_bytes = 0;
        device->GetCopyableFootprints(&target_desc, 0, 1, 0, &footprint, &rows, &row_bytes,
                                      &readback_bytes);
        printf("%-38s pitch=%u rows=%u rowBytes=%llu total=%llu\n",
               "GetCopyableFootprints",
               footprint.Footprint.RowPitch, rows,
               static_cast<unsigned long long>(row_bytes),
               static_cast<unsigned long long>(readback_bytes));
        if (readback_bytes == 0 || footprint.Footprint.RowPitch == 0 || rows == 0) {
            printf("FAIL: GetCopyableFootprints returned a degenerate footprint.\n");
            break;
        }

        D3D12_HEAP_PROPERTIES readback_heap = {};
        readback_heap.Type = D3D12_HEAP_TYPE_READBACK;
        D3D12_RESOURCE_DESC readback_desc = {};
        readback_desc.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
        readback_desc.Width = readback_bytes;
        readback_desc.Height = 1;
        readback_desc.DepthOrArraySize = 1;
        readback_desc.MipLevels = 1;
        readback_desc.Format = DXGI_FORMAT_UNKNOWN;
        readback_desc.SampleDesc.Count = 1;
        readback_desc.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
        if (!step("CreateCommittedResource(readback)",
                  device->CreateCommittedResource(&readback_heap, D3D12_HEAP_FLAG_NONE,
                                                  &readback_desc,
                                                  D3D12_RESOURCE_STATE_COPY_DEST, nullptr,
                                                  IID_PPV_ARGS(&readback)))) {
            break;
        }

        D3D12_CPU_DESCRIPTOR_HANDLE rtv = rtv_heap->GetCPUDescriptorHandleForHeapStart();
        printf("%-38s ptr=0x%016llX increment=%u\n",
               "RTV heap start",
               static_cast<unsigned long long>(rtv.ptr),
               device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV));
        if (rtv.ptr == 0) {
            // vkd3d always assigns a real host pointer to a heap's cpu_va, so a
            // zero here is a truncation across the by-value struct return and not
            // the engine's answer. Named because it is the one failure this whole
            // handle scheme was flagged as vulnerable to.
            printf("FAIL: GetCPUDescriptorHandleForHeapStart returned 0 -- the by-value\n");
            printf("      struct return is truncating. See descriptors.rs HAZARD 1.\n");
            break;
        }
        device->CreateRenderTargetView(target, nullptr, rtv);

        // ---- record ----
        list->ClearRenderTargetView(rtv, kClearColour, 0, nullptr);

        D3D12_RESOURCE_BARRIER barrier = {};
        barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
        barrier.Transition.pResource = target;
        barrier.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
        barrier.Transition.StateBefore = D3D12_RESOURCE_STATE_RENDER_TARGET;
        barrier.Transition.StateAfter = D3D12_RESOURCE_STATE_COPY_SOURCE;
        list->ResourceBarrier(1, &barrier);

        D3D12_TEXTURE_COPY_LOCATION dst = {};
        dst.pResource = readback;
        dst.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
        dst.PlacedFootprint = footprint;
        D3D12_TEXTURE_COPY_LOCATION src = {};
        src.pResource = target;
        src.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
        src.SubresourceIndex = 0;
        list->CopyTextureRegion(&dst, 0, 0, 0, &src, nullptr);

        if (!step("Close", list->Close())) {
            break;
        }

        // ---- submit and wait ----
        ID3D12CommandList* lists[] = {list};
        queue->ExecuteCommandLists(1, lists);

        if (!step("CreateFence", device->CreateFence(0, D3D12_FENCE_FLAG_NONE,
                                                    IID_PPV_ARGS(&fence)))) {
            break;
        }
        fence_event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
        if (!fence_event) {
            printf("FAIL: CreateEventW returned NULL (GetLastError=%lu)\n", GetLastError());
            break;
        }
        if (!step("CommandQueue::Signal", queue->Signal(fence, 1))) {
            break;
        }
        if (!step("SetEventOnCompletion", fence->SetEventOnCompletion(1, fence_event))) {
            break;
        }
        // ⚠ Bounded. An unbounded wait on a driver under bring-up is a hung gate
        // with no output, which is strictly worse than a reported timeout -- and a
        // frozen run is a defect to root-cause, never a retry.
        const DWORD kTimeoutMs = 20000;
        DWORD waited = WaitForSingleObject(fence_event, kTimeoutMs);
        printf("%-38s %s (completed=%llu)\n",
               "WaitForSingleObject",
               waited == WAIT_OBJECT_0 ? "signalled" : (waited == WAIT_TIMEOUT ? "TIMEOUT" : "?"),
               static_cast<unsigned long long>(fence->GetCompletedValue()));
        if (waited != WAIT_OBJECT_0) {
            printf("FAIL: the GPU did not complete within %lu ms. This is a defect to\n",
                   static_cast<unsigned long>(kTimeoutMs));
            printf("      root-cause, not a run to retry.\n");
            break;
        }

        // ---- read back ----
        void* mapped = nullptr;
        D3D12_RANGE read_range = {0, static_cast<SIZE_T>(readback_bytes)};
        if (!step("Map(readback)", readback->Map(0, &read_range, &mapped)) || !mapped) {
            break;
        }

        const BYTE* base = static_cast<const BYTE*>(mapped);
        const BYTE* pixel0 = base;
        printf("\npixel(0,0)  = (%u, %u, %u, %u)\n", pixel0[0], pixel0[1], pixel0[2], pixel0[3]);
        printf("expected    = (%u, %u, %u, %u)\n", kExpectR, kExpectG, kExpectB, kExpectA);

        // ⭐ Not only pixel 0. A clear that wrote one pixel, or wrote the first row
        // and stopped, or ignored the row pitch, all pass a single-pixel check.
        // Sweeping the whole surface costs nothing here and turns three different
        // partial-write bugs into a count.
        UINT64 exact = 0;
        UINT64 near_miss = 0;
        UINT64 wrong = 0;
        BYTE first_wrong[4] = {0, 0, 0, 0};
        UINT first_wrong_x = 0;
        UINT first_wrong_y = 0;
        bool have_first_wrong = false;
        for (UINT y = 0; y < kHeight; ++y) {
            const BYTE* row = base + static_cast<UINT64>(y) * footprint.Footprint.RowPitch;
            for (UINT x = 0; x < kWidth; ++x) {
                const BYTE* p = row + static_cast<UINT64>(x) * 4;
                const int dr = static_cast<int>(p[0]) - static_cast<int>(kExpectR);
                const int dg = static_cast<int>(p[1]) - static_cast<int>(kExpectG);
                const int db = static_cast<int>(p[2]) - static_cast<int>(kExpectB);
                const int da = static_cast<int>(p[3]) - static_cast<int>(kExpectA);
                if (dr == 0 && dg == 0 && db == 0 && da == 0) {
                    ++exact;
                } else if (dr >= -1 && dr <= 1 && dg >= -1 && dg <= 1 && db >= -1 && db <= 1 &&
                           da >= -1 && da <= 1) {
                    ++near_miss;
                } else {
                    ++wrong;
                    if (!have_first_wrong) {
                        first_wrong[0] = p[0];
                        first_wrong[1] = p[1];
                        first_wrong[2] = p[2];
                        first_wrong[3] = p[3];
                        first_wrong_x = x;
                        first_wrong_y = y;
                        have_first_wrong = true;
                    }
                }
            }
        }
        const UINT64 total = static_cast<UINT64>(kWidth) * kHeight;
        printf("surface     = %llu exact, %llu within +/-1, %llu wrong (of %llu)\n",
               static_cast<unsigned long long>(exact),
               static_cast<unsigned long long>(near_miss),
               static_cast<unsigned long long>(wrong),
               static_cast<unsigned long long>(total));
        if (have_first_wrong) {
            printf("first wrong at (%u, %u) = (%u, %u, %u, %u)\n",
                   first_wrong_x, first_wrong_y,
                   first_wrong[0], first_wrong[1], first_wrong[2], first_wrong[3]);
        }

        D3D12_RANGE written = {0, 0};
        readback->Unmap(0, &written);

        if (exact == total) {
            printf("\nPASS: every one of %llu pixels is exactly (%u, %u, %u, %u).\n",
                   static_cast<unsigned long long>(total), kExpectR, kExpectG, kExpectB, kExpectA);
            rc = 0;
        } else if (wrong == 0) {
            printf("\nSOFT PASS: %llu pixels are within +/-1 of the expected value and none is\n",
                   static_cast<unsigned long long>(near_miss));
            printf("           further out. A one-LSB deviation is a real finding about the\n");
            printf("           format conversion -- record it in notes.md, do not absorb it.\n");
            rc = 0;
        } else {
            printf("\nFAIL: %llu pixels are outside +/-1. This is a colour-pipeline defect,\n",
                   static_cast<unsigned long long>(wrong));
            printf("      not tolerance. Do NOT widen the tolerance to make it pass.\n");
            rc = 1;
        }
    } while (false);

    if (fence_event) {
        CloseHandle(fence_event);
    }
    release(fence);
    release(readback);
    release(target);
    release(rtv_heap);
    release(list);
    release(allocator);
    release(queue);
    release(device);
    release(adapter);
    release(factory);

    if (expect == Expect::Report) {
        printf("\nRESULT: rc would be %d (no --expect given, reporting only)\n", rc);
        return 0;
    }
    if (rc != 0) {
        printf("\nRead C:\\ProgramData\\Helios\\umd12-<pid>.log after the last 'UMD module:'\n");
        printf("line: 'D3D12 DDI refusals:' names the step and 'D3D12 noop DDI hits:' names\n");
        printf("every slot that is still a counting noop.\n");
    }
    return rc;
}
