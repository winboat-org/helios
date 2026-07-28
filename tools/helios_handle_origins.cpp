// Helios WS1 — name the CALL SITE behind each leaked handle.
//
// `helios_handle_types.cpp` established the mechanism: `helios_umd.dll`,
// `vulkan-1.dll` and the venus ICD are loaded and unloaded once per D3D11
// device, and each module's process-lifetime state is released by nothing on
// unload, so six kernel handles are stranded per device. Pinning attributed
// them to a module (File -> helios_umd; 2 Event + 2 Semaphore + 1 Thread ->
// the ICD). This names the function that created each one.
//
// Method, which needs no debugger and no symbols to be useful:
//   1. warm up once to discover which modules are transient, then LoadLibrary
//      every one of them so the set is stable and hookable;
//   2. patch each loaded module's IAT for the handle-minting kernel32 entry
//      points, matching by RESOLVED ADDRESS rather than by name so the
//      api-set aliasing (kernel32 vs KernelBase vs api-ms-win-core-*) is
//      handled without enumerating spellings;
//   3. create and release ONE device. Every handle minted in that window is
//      recorded with RtlCaptureStackBackTrace; CloseHandle un-records it;
//   4. anything still live after the device is gone is, by construction,
//      exactly what leaks per device when the modules are NOT pinned. Print
//      its stack.
//
// Frames print as `module+0xRVA` always, plus a symbol when dbghelp resolves
// one. The ICD is built by mingw gcc and carries DWARF, which dbghelp cannot
// read, so its frames resolve to module+RVA — feed those to `addr2line -e`
// against the same binary.
//
// Build + run: tools\helios-handle-origins.ps1
//
// Exit codes: 0 = ran, 1 = setup failure.

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <psapi.h>
#include <dbghelp.h>
#include <dxgi1_6.h>
#include <d3d11.h>

#include <cstdio>
#include <cstdlib>
#include <cwchar>
#include <string>
#include <vector>

namespace {

// ---- recorded handle origins --------------------------------------------

constexpr unsigned kMaxFrames = 14;
constexpr unsigned kMaxRecords = 8192;

struct Origin {
    HANDLE      handle = nullptr;
    const char* kind = nullptr;
    USHORT      frameCount = 0;
    void*       frames[kMaxFrames] = {};
    bool        live = false;
};

Origin           g_records[kMaxRecords];
unsigned         g_recordCount = 0;
CRITICAL_SECTION g_lock;
bool             g_recording = false;

void record(HANDLE h, const char* kind) {
    if (!g_recording || !h || h == INVALID_HANDLE_VALUE)
        return;
    void*  frames[kMaxFrames] = {};
    // Skip frame 0 (this function) and 1 (the hook thunk).
    const USHORT n = RtlCaptureStackBackTrace(2, kMaxFrames, frames, nullptr);
    EnterCriticalSection(&g_lock);
    if (g_recordCount < kMaxRecords) {
        Origin& o = g_records[g_recordCount++];
        o.handle = h;
        o.kind = kind;
        o.frameCount = n;
        o.live = true;
        for (USHORT i = 0; i < n; ++i)
            o.frames[i] = frames[i];
    }
    LeaveCriticalSection(&g_lock);
}

void unrecord(HANDLE h) {
    if (!g_recording || !h)
        return;
    EnterCriticalSection(&g_lock);
    // Reverse order: a reused handle value's newest record is the live one.
    for (unsigned i = g_recordCount; i-- > 0;) {
        if (g_records[i].handle == h && g_records[i].live) {
            g_records[i].live = false;
            break;
        }
    }
    LeaveCriticalSection(&g_lock);
}

// ---- hook thunks ---------------------------------------------------------
// Each calls the real entry point through the address the IAT held before the
// patch, so a module we failed to hook still works and a hooked module's
// behaviour is unchanged apart from the bookkeeping.

using PfnCreateEventW = HANDLE(WINAPI*)(LPSECURITY_ATTRIBUTES, BOOL, BOOL, LPCWSTR);
using PfnCreateEventA = HANDLE(WINAPI*)(LPSECURITY_ATTRIBUTES, BOOL, BOOL, LPCSTR);
using PfnCreateEventExW = HANDLE(WINAPI*)(LPSECURITY_ATTRIBUTES, LPCWSTR, DWORD, DWORD);
using PfnCreateSemaphoreA = HANDLE(WINAPI*)(LPSECURITY_ATTRIBUTES, LONG, LONG, LPCSTR);
using PfnCreateSemaphoreW = HANDLE(WINAPI*)(LPSECURITY_ATTRIBUTES, LONG, LONG, LPCWSTR);
using PfnCreateSemaphoreExW =
    HANDLE(WINAPI*)(LPSECURITY_ATTRIBUTES, LONG, LONG, LPCWSTR, DWORD, DWORD);
using PfnCreateFileW = HANDLE(WINAPI*)(LPCWSTR, DWORD, DWORD, LPSECURITY_ATTRIBUTES, DWORD, DWORD,
                                       HANDLE);
using PfnCreateFileA = HANDLE(WINAPI*)(LPCSTR, DWORD, DWORD, LPSECURITY_ATTRIBUTES, DWORD, DWORD,
                                       HANDLE);
using PfnDuplicateHandle = BOOL(WINAPI*)(HANDLE, HANDLE, HANDLE, LPHANDLE, DWORD, BOOL, DWORD);
using PfnCreateThread = HANDLE(WINAPI*)(LPSECURITY_ATTRIBUTES, SIZE_T, LPTHREAD_START_ROUTINE,
                                        LPVOID, DWORD, LPDWORD);
using PfnCloseHandle = BOOL(WINAPI*)(HANDLE);

PfnCreateEventW       g_realCreateEventW = nullptr;
PfnCreateEventA       g_realCreateEventA = nullptr;
PfnCreateEventExW     g_realCreateEventExW = nullptr;
PfnCreateSemaphoreA   g_realCreateSemaphoreA = nullptr;
PfnCreateSemaphoreW   g_realCreateSemaphoreW = nullptr;
PfnCreateSemaphoreExW g_realCreateSemaphoreExW = nullptr;
PfnCreateFileW        g_realCreateFileW = nullptr;
PfnCreateFileA        g_realCreateFileA = nullptr;
PfnDuplicateHandle    g_realDuplicateHandle = nullptr;
PfnCreateThread       g_realCreateThread = nullptr;
PfnCloseHandle        g_realCloseHandle = nullptr;

HANDLE WINAPI hookCreateEventW(LPSECURITY_ATTRIBUTES sa, BOOL manual, BOOL init, LPCWSTR name) {
    HANDLE h = g_realCreateEventW(sa, manual, init, name);
    record(h, "CreateEventW");
    return h;
}
HANDLE WINAPI hookCreateEventA(LPSECURITY_ATTRIBUTES sa, BOOL manual, BOOL init, LPCSTR name) {
    HANDLE h = g_realCreateEventA(sa, manual, init, name);
    record(h, "CreateEventA");
    return h;
}
HANDLE WINAPI hookCreateEventExW(LPSECURITY_ATTRIBUTES sa, LPCWSTR name, DWORD flags,
                                 DWORD access) {
    HANDLE h = g_realCreateEventExW(sa, name, flags, access);
    record(h, "CreateEventExW");
    return h;
}
// The ANSI spellings are not redundant: the venus ICD is built by mingw gcc,
// whose winpthreads and CRT are compiled without UNICODE, so `CreateSemaphore`
// there IS `CreateSemaphoreA`. Hooking only the wide forms silently missed
// both leaked semaphores on this probe's first complete run.
HANDLE WINAPI hookCreateSemaphoreA(LPSECURITY_ATTRIBUTES sa, LONG init, LONG max, LPCSTR name) {
    HANDLE h = g_realCreateSemaphoreA(sa, init, max, name);
    record(h, "CreateSemaphoreA");
    return h;
}
HANDLE WINAPI hookCreateSemaphoreW(LPSECURITY_ATTRIBUTES sa, LONG init, LONG max, LPCWSTR name) {
    HANDLE h = g_realCreateSemaphoreW(sa, init, max, name);
    record(h, "CreateSemaphoreW");
    return h;
}
HANDLE WINAPI hookCreateSemaphoreExW(LPSECURITY_ATTRIBUTES sa, LONG init, LONG max, LPCWSTR name,
                                     DWORD flags, DWORD access) {
    HANDLE h = g_realCreateSemaphoreExW(sa, init, max, name, flags, access);
    record(h, "CreateSemaphoreExW");
    return h;
}
HANDLE WINAPI hookCreateFileW(LPCWSTR name, DWORD access, DWORD share, LPSECURITY_ATTRIBUTES sa,
                              DWORD disp, DWORD flags, HANDLE tmpl) {
    HANDLE h = g_realCreateFileW(name, access, share, sa, disp, flags, tmpl);
    record(h, "CreateFileW");
    return h;
}
HANDLE WINAPI hookCreateFileA(LPCSTR name, DWORD access, DWORD share, LPSECURITY_ATTRIBUTES sa,
                              DWORD disp, DWORD flags, HANDLE tmpl) {
    HANDLE h = g_realCreateFileA(name, access, share, sa, disp, flags, tmpl);
    record(h, "CreateFileA");
    return h;
}
BOOL WINAPI hookDuplicateHandle(HANDLE srcProc, HANDLE src, HANDLE dstProc, LPHANDLE dst,
                                DWORD access, BOOL inherit, DWORD options) {
    BOOL ok = g_realDuplicateHandle(srcProc, src, dstProc, dst, access, inherit, options);
    if (ok && dst)
        record(*dst, "DuplicateHandle");
    return ok;
}
HANDLE WINAPI hookCreateThread(LPSECURITY_ATTRIBUTES sa, SIZE_T stack,
                               LPTHREAD_START_ROUTINE start, LPVOID param, DWORD flags,
                               LPDWORD id) {
    HANDLE h = g_realCreateThread(sa, stack, start, param, flags, id);
    record(h, "CreateThread");
    return h;
}
BOOL WINAPI hookCloseHandle(HANDLE h) {
    unrecord(h);
    return g_realCloseHandle(h);
}

// ---- IAT patching --------------------------------------------------------

struct Target {
    const char*        name;
    void*              hook;
    void**             realSlot;
    std::vector<void*> addresses;  // every alias this export resolves to
};

std::vector<Target> g_targets;

void collect_addresses(Target* t) {
    // The same export is reachable through kernel32, KernelBase and several
    // api-set stubs, and different modules import different ones. Matching the
    // IAT by resolved address covers all of them without a spelling list.
    static const wchar_t* kProviders[] = {
        L"kernel32.dll",
        L"KernelBase.dll",
        L"api-ms-win-core-synch-l1-2-0.dll",
        L"api-ms-win-core-file-l1-2-0.dll",
        L"api-ms-win-core-handle-l1-1-0.dll",
        L"api-ms-win-core-processthreads-l1-1-1.dll",
    };
    for (const wchar_t* p : kProviders) {
        HMODULE m = GetModuleHandleW(p);
        if (!m)
            m = LoadLibraryW(p);
        if (!m)
            continue;
        void* a = reinterpret_cast<void*>(GetProcAddress(m, t->name));
        if (!a)
            continue;
        bool dup = false;
        for (void* seen : t->addresses)
            if (seen == a)
                dup = true;
        if (!dup)
            t->addresses.push_back(a);
    }
}

// The provider modules must NOT be patched. kernel32!CreateFileW is a thin
// forwarder that reaches KernelBase through kernel32's OWN import table, so
// patching that slot makes the real function we call re-enter the hook —
// unbounded recursion, which is exactly how the first run of this probe died
// (0xC00000FD, STATUS_STACK_OVERFLOW, before printing a single record). A
// depth guard does not help: the recursion is in the call chain, not in the
// bookkeeping.
bool is_provider(HMODULE mod) {
    wchar_t base[MAX_PATH] = {};
    if (!GetModuleBaseNameW(GetCurrentProcess(), mod, base, MAX_PATH))
        return true;  // unknown: refuse rather than risk the recursion
    static const wchar_t* kExact[] = { L"ntdll.dll", L"kernel32.dll", L"KernelBase.dll",
                                       L"kernel.appcore.dll" };
    for (const wchar_t* e : kExact)
        if (_wcsicmp(base, e) == 0)
            return true;
    return _wcsnicmp(base, L"api-ms-win-", 11) == 0 || _wcsnicmp(base, L"ext-ms-win-", 11) == 0;
}

unsigned patch_module(HMODULE mod) {
    if (is_provider(mod))
        return 0;
    unsigned patched = 0;
    auto* dos = reinterpret_cast<IMAGE_DOS_HEADER*>(mod);
    if (IsBadReadPtr(dos, sizeof(*dos)) || dos->e_magic != IMAGE_DOS_SIGNATURE)
        return 0;
    auto* nt = reinterpret_cast<IMAGE_NT_HEADERS*>(reinterpret_cast<BYTE*>(mod) + dos->e_lfanew);
    if (nt->Signature != IMAGE_NT_SIGNATURE)
        return 0;
    const IMAGE_DATA_DIRECTORY& dir =
        nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
    if (!dir.VirtualAddress || !dir.Size)
        return 0;
    auto* imp =
        reinterpret_cast<IMAGE_IMPORT_DESCRIPTOR*>(reinterpret_cast<BYTE*>(mod) + dir.VirtualAddress);
    for (; imp->Name; ++imp) {
        if (!imp->FirstThunk)
            continue;
        auto* thunk =
            reinterpret_cast<IMAGE_THUNK_DATA*>(reinterpret_cast<BYTE*>(mod) + imp->FirstThunk);
        for (; thunk->u1.Function; ++thunk) {
            void** slot = reinterpret_cast<void**>(&thunk->u1.Function);
            for (Target& t : g_targets) {
                bool match = false;
                for (void* a : t.addresses)
                    if (*slot == a)
                        match = true;
                if (!match)
                    continue;
                DWORD old = 0;
                if (!VirtualProtect(slot, sizeof(void*), PAGE_READWRITE, &old))
                    continue;
                *slot = t.hook;
                VirtualProtect(slot, sizeof(void*), old, &old);
                ++patched;
            }
        }
    }
    return patched;
}

unsigned patch_all_modules() {
    unsigned patched = 0;
    HMODULE mods[1024];
    DWORD   needed = 0;
    if (!EnumProcessModules(GetCurrentProcess(), mods, sizeof(mods), &needed))
        return 0;
    const DWORD n = needed / sizeof(HMODULE);
    for (DWORD i = 0; i < n; ++i)
        patched += patch_module(mods[i]);
    return patched;
}

void install_hooks() {
    g_targets = {
        { "CreateEventW", reinterpret_cast<void*>(&hookCreateEventW),
          reinterpret_cast<void**>(&g_realCreateEventW), {} },
        { "CreateEventA", reinterpret_cast<void*>(&hookCreateEventA),
          reinterpret_cast<void**>(&g_realCreateEventA), {} },
        { "CreateEventExW", reinterpret_cast<void*>(&hookCreateEventExW),
          reinterpret_cast<void**>(&g_realCreateEventExW), {} },
        { "CreateSemaphoreA", reinterpret_cast<void*>(&hookCreateSemaphoreA),
          reinterpret_cast<void**>(&g_realCreateSemaphoreA), {} },
        { "CreateSemaphoreW", reinterpret_cast<void*>(&hookCreateSemaphoreW),
          reinterpret_cast<void**>(&g_realCreateSemaphoreW), {} },
        { "CreateSemaphoreExW", reinterpret_cast<void*>(&hookCreateSemaphoreExW),
          reinterpret_cast<void**>(&g_realCreateSemaphoreExW), {} },
        { "CreateFileW", reinterpret_cast<void*>(&hookCreateFileW),
          reinterpret_cast<void**>(&g_realCreateFileW), {} },
        { "CreateFileA", reinterpret_cast<void*>(&hookCreateFileA),
          reinterpret_cast<void**>(&g_realCreateFileA), {} },
        { "DuplicateHandle", reinterpret_cast<void*>(&hookDuplicateHandle),
          reinterpret_cast<void**>(&g_realDuplicateHandle), {} },
        { "CreateThread", reinterpret_cast<void*>(&hookCreateThread),
          reinterpret_cast<void**>(&g_realCreateThread), {} },
        { "CloseHandle", reinterpret_cast<void*>(&hookCloseHandle),
          reinterpret_cast<void**>(&g_realCloseHandle), {} },
    };
    for (Target& t : g_targets) {
        collect_addresses(&t);
        *t.realSlot = t.addresses.empty() ? nullptr : t.addresses[0];
        if (t.addresses.empty())
            std::printf("  WARNING: %s resolved to no address; not hooked\n", t.name);
    }
    const unsigned patched = patch_all_modules();
    std::printf("  patched %u IAT slots\n", patched);
}

// ---- reporting -----------------------------------------------------------

void print_frame(void* addr) {
    HMODULE mod = nullptr;
    wchar_t base[MAX_PATH] = L"?";
    ULONG_PTR rva = 0;
    if (GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                               GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                           reinterpret_cast<LPCWSTR>(addr), &mod) &&
        mod) {
        GetModuleBaseNameW(GetCurrentProcess(), mod, base, MAX_PATH);
        rva = reinterpret_cast<ULONG_PTR>(addr) - reinterpret_cast<ULONG_PTR>(mod);
    }
    alignas(16) unsigned char symBuf[sizeof(SYMBOL_INFO) + 512] = {};
    auto* sym = reinterpret_cast<SYMBOL_INFO*>(symBuf);
    sym->SizeOfStruct = sizeof(SYMBOL_INFO);
    sym->MaxNameLen = 511;
    DWORD64 disp = 0;
    const bool named = SymFromAddr(GetCurrentProcess(), reinterpret_cast<DWORD64>(addr), &disp,
                                  sym) != FALSE;
    std::wprintf(L"      %-30s +0x%06llX", base, static_cast<unsigned long long>(rva));
    if (named)
        std::printf("  %s+0x%llX", sym->Name, static_cast<unsigned long long>(disp));
    std::printf("\n");
}

// ---- the workload --------------------------------------------------------

IDXGIAdapter1* find_adapter(IDXGIFactory1* factory, const wchar_t* match) {
    IDXGIAdapter1* adapter = nullptr;
    for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
        DXGI_ADAPTER_DESC1 desc{};
        adapter->GetDesc1(&desc);
        if (wcsstr(desc.Description, match) != nullptr) {
            std::wprintf(L"adapter: \"%s\"\n", desc.Description);
            return adapter;
        }
        adapter->Release();
    }
    return nullptr;
}

ID3D11Device* create_device(IDXGIAdapter1* adapter) {
    ID3D11Device*     device = nullptr;
    D3D_FEATURE_LEVEL got{};
    const D3D_FEATURE_LEVEL levels[] = { D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1,
                                         D3D_FEATURE_LEVEL_10_0 };
    if (FAILED(D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0, levels,
                                 ARRAYSIZE(levels), D3D11_SDK_VERSION, &device, &got, nullptr)))
        return nullptr;
    return device;
}

std::vector<std::wstring> module_paths() {
    std::vector<std::wstring> paths;
    HMODULE                   mods[1024];
    DWORD                     needed = 0;
    if (!EnumProcessModules(GetCurrentProcess(), mods, sizeof(mods), &needed))
        return paths;
    const DWORD n = needed / sizeof(HMODULE);
    for (DWORD i = 0; i < n; ++i) {
        wchar_t path[MAX_PATH] = {};
        if (GetModuleFileNameW(mods[i], path, MAX_PATH))
            paths.push_back(path);
    }
    return paths;
}

bool contains(const std::vector<std::wstring>& v, const std::wstring& n) {
    for (const std::wstring& s : v)
        if (_wcsicmp(s.c_str(), n.c_str()) == 0)
            return true;
    return false;
}

}  // namespace

int main(int argc, char** argv) {
    std::setvbuf(stdout, nullptr, _IONBF, 0);
    InitializeCriticalSection(&g_lock);

    const wchar_t* adapterMatch = L"Helios";
    if (argc > 1 && argv[1][0] == 'w')
        adapterMatch = L"Basic Render";

    IDXGIFactory1* factory = nullptr;
    if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), reinterpret_cast<void**>(&factory)))) {
        std::printf("FAIL: CreateDXGIFactory1\n");
        return 1;
    }
    IDXGIAdapter1* adapter = find_adapter(factory, adapterMatch);
    if (!adapter) {
        std::wprintf(L"FAIL: no adapter matching \"%s\"\n", adapterMatch);
        factory->Release();
        return 1;
    }

    // 1. discover the transient modules
    const std::vector<std::wstring> pre = module_paths();
    ID3D11Device*                   warm = create_device(adapter);
    if (!warm) {
        std::printf("FAIL: D3D11CreateDevice\n");
        adapter->Release();
        factory->Release();
        return 1;
    }
    const std::vector<std::wstring> live = module_paths();
    warm->Release();
    const std::vector<std::wstring> post = module_paths();

    // 2. pin them, so the set is stable and its IATs stay patched
    std::printf("\n--- pinning transient modules ---\n");
    for (const std::wstring& m : live) {
        if (contains(pre, m) || contains(post, m))
            continue;
        const size_t       slash = m.find_last_of(L'\\');
        const std::wstring b = slash == std::wstring::npos ? m : m.substr(slash + 1);
        std::wprintf(L"  %-34s %s\n", b.c_str(), LoadLibraryW(m.c_str()) ? L"pinned" : L"FAILED");
    }

    // 3. hook, then run ONE device cycle with recording on
    std::printf("\n--- installing IAT hooks ---\n");
    install_hooks();

    SymSetOptions(SYMOPT_DEFERRED_LOADS | SYMOPT_UNDNAME);
    SymInitialize(GetCurrentProcess(), nullptr, TRUE);

    g_recording = true;
    ID3D11Device* d = create_device(adapter);
    if (d)
        d->Release();
    g_recording = false;

    // 4. whatever the device left behind is what leaks when unpinned
    std::printf("\n--- handles minted during one device and STILL LIVE after it ---\n");
    unsigned leaked = 0;
    EnterCriticalSection(&g_lock);
    for (unsigned i = 0; i < g_recordCount; ++i) {
        const Origin& o = g_records[i];
        if (!o.live)
            continue;
        ++leaked;
        std::printf("\n  [%u] %s -> handle 0x%04llX\n", leaked, o.kind,
                    static_cast<unsigned long long>(reinterpret_cast<ULONG_PTR>(o.handle)));
        for (USHORT f = 0; f < o.frameCount; ++f)
            print_frame(o.frames[f]);
    }
    LeaveCriticalSection(&g_lock);
    std::printf("\n  %u still-live of %u recorded mints\n", leaked, g_recordCount);

    adapter->Release();
    factory->Release();
    return 0;
}
