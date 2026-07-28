// Helios WS1 — name the leaked handle type.
//
// `tools/helios_ownership_soak.cpp` measures that a D3D11CreateDevice/Release
// pair on the Helios adapter leaks 5.99 KERNEL handles per device
// (GetProcessHandleCount), while the WARP control leaks 0.00. That number says
// how many, not *what*. Grepping for CreateEvent on a guess is how a wrong
// answer stays plausible for hours, so this probe names the object type first.
//
// Method: NtQuerySystemInformation(SystemExtendedHandleInformation) filtered to
// this process, snapshotted either side of a small run of device cycles. The
// diff is reported three ways:
//   * per-type counts before/after (type names resolved with NtQueryObject on
//     our own handles, so no privilege beyond the process itself);
//   * every handle present after and absent before, with its type, granted
//     access and kernel object address — the address tells us whether the N
//     leaked handles are N distinct objects or N references to one;
//   * for non-File types, the object's name, which for an Event or Section is
//     usually enough to point straight at the creating call site.
//
// File-type handles are deliberately NOT name-queried: NtQueryObject's name
// path can block indefinitely on a synchronous named pipe with a pending
// operation, and a probe that hangs is worse than one that prints less.
//
// Build + run: tools\helios-handle-types.ps1
//
// Exit codes: 0 = ran, 1 = setup failure.

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <psapi.h>
#include <dxgi1_6.h>
#include <d3d11.h>

#include <cstdio>
#include <cstdlib>
#include <cwchar>
#include <map>
#include <string>
#include <vector>

namespace {

// ---- ntdll surface -------------------------------------------------------
// Not in any public header; the shapes below are the documented-by-use x64
// layouts and are size-asserted where a wrong guess would silently misparse.

typedef struct _HELIOS_UNICODE_STRING {
    USHORT Length;
    USHORT MaximumLength;
    PWSTR  Buffer;
} HELIOS_UNICODE_STRING;

typedef struct _HELIOS_SYSTEM_HANDLE_ENTRY_EX {
    PVOID     Object;
    ULONG_PTR UniqueProcessId;
    ULONG_PTR HandleValue;
    ULONG     GrantedAccess;
    USHORT    CreatorBackTraceIndex;
    USHORT    ObjectTypeIndex;
    ULONG     HandleAttributes;
    ULONG     Reserved;
} HELIOS_SYSTEM_HANDLE_ENTRY_EX;

static_assert(sizeof(HELIOS_SYSTEM_HANDLE_ENTRY_EX) == 40,
              "SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX must be 40 bytes on x64");

typedef struct _HELIOS_SYSTEM_HANDLE_INFORMATION_EX {
    ULONG_PTR                     NumberOfHandles;
    ULONG_PTR                     Reserved;
    HELIOS_SYSTEM_HANDLE_ENTRY_EX Handles[1];
} HELIOS_SYSTEM_HANDLE_INFORMATION_EX;

// `NTSTATUS` is not declared by windows.h alone (it lives in winternl.h /
// ntstatus.h, neither of which we want dragged in beside the DXGI headers).
using HeliosNtStatus = LONG;

constexpr ULONG kSystemExtendedHandleInformation = 64;
constexpr ULONG kObjectNameInformation = 1;
constexpr ULONG kObjectTypeInformation = 2;
constexpr HeliosNtStatus kStatusInfoLengthMismatch = static_cast<HeliosNtStatus>(0xC0000004L);

using PfnNtQuerySystemInformation = HeliosNtStatus(NTAPI*)(ULONG, PVOID, ULONG, PULONG);
using PfnNtQueryObject = HeliosNtStatus(NTAPI*)(HANDLE, ULONG, PVOID, ULONG, PULONG);

PfnNtQuerySystemInformation g_querySystem = nullptr;
PfnNtQueryObject            g_queryObject = nullptr;

bool load_ntdll() {
    HMODULE ntdll = GetModuleHandleW(L"ntdll.dll");
    if (!ntdll)
        return false;
    g_querySystem = reinterpret_cast<PfnNtQuerySystemInformation>(
        GetProcAddress(ntdll, "NtQuerySystemInformation"));
    g_queryObject = reinterpret_cast<PfnNtQueryObject>(GetProcAddress(ntdll, "NtQueryObject"));
    return g_querySystem && g_queryObject;
}

// ---- handle snapshot -----------------------------------------------------

struct HandleRecord {
    ULONG_PTR value = 0;
    PVOID     object = nullptr;
    ULONG     grantedAccess = 0;
    USHORT    typeIndex = 0;
};

// Type names by ObjectTypeIndex, resolved lazily and cached: NtQueryObject is
// a syscall per handle and the same few indices repeat thousands of times.
std::map<USHORT, std::wstring> g_typeNames;

const wchar_t* type_name(USHORT index, HANDLE h) {
    auto it = g_typeNames.find(index);
    if (it != g_typeNames.end())
        return it->second.c_str();
    std::wstring name = L"(index ";
    name += std::to_wstring(index);
    name += L")";
    alignas(16) unsigned char buf[2048] = {};
    ULONG needed = 0;
    if (g_queryObject(h, kObjectTypeInformation, buf, sizeof(buf), &needed) >= 0) {
        auto* us = reinterpret_cast<HELIOS_UNICODE_STRING*>(buf);
        if (us->Buffer && us->Length)
            name.assign(us->Buffer, us->Length / sizeof(wchar_t));
    }
    return g_typeNames.emplace(index, std::move(name)).first->second.c_str();
}

// Object name, for the types where asking cannot block. Returns an empty
// string when the object is unnamed (the common case) or the query fails.
std::wstring object_name(HANDLE h, const wchar_t* type) {
    // The name query only blocks on a File whose underlying object is a
    // synchronous pipe or a character device with an operation pending.
    // GetFileType answers that without touching the object's queue, so a
    // disk file — the interesting case — can still be named.
    if (_wcsicmp(type, L"File") == 0 && GetFileType(h) != FILE_TYPE_DISK)
        return L"(non-disk file; name not queried)";
    alignas(16) unsigned char buf[2048] = {};
    ULONG needed = 0;
    if (g_queryObject(h, kObjectNameInformation, buf, sizeof(buf), &needed) < 0)
        return L"";
    auto* us = reinterpret_cast<HELIOS_UNICODE_STRING*>(buf);
    if (!us->Buffer || !us->Length)
        return L"";
    return std::wstring(us->Buffer, us->Length / sizeof(wchar_t));
}

bool snapshot(std::vector<HandleRecord>* out) {
    out->clear();
    const ULONG_PTR self = static_cast<ULONG_PTR>(GetCurrentProcessId());
    ULONG size = 1u << 20;
    std::vector<unsigned char> buf;
    for (int attempt = 0; attempt < 16; ++attempt) {
        buf.assign(size, 0);
        ULONG needed = 0;
        HeliosNtStatus st =
            g_querySystem(kSystemExtendedHandleInformation, buf.data(), size, &needed);
        if (st == kStatusInfoLengthMismatch) {
            size = (needed > size) ? (needed + (needed / 4)) : (size * 2);
            continue;
        }
        if (st < 0) {
            std::printf("FAIL: NtQuerySystemInformation status 0x%08lX\n",
                        static_cast<unsigned long>(st));
            return false;
        }
        auto* info = reinterpret_cast<HELIOS_SYSTEM_HANDLE_INFORMATION_EX*>(buf.data());
        for (ULONG_PTR i = 0; i < info->NumberOfHandles; ++i) {
            const HELIOS_SYSTEM_HANDLE_ENTRY_EX& e = info->Handles[i];
            if (e.UniqueProcessId != self)
                continue;
            HandleRecord r;
            r.value = e.HandleValue;
            r.object = e.Object;
            r.grantedAccess = e.GrantedAccess;
            r.typeIndex = e.ObjectTypeIndex;
            out->push_back(r);
        }
        return true;
    }
    std::printf("FAIL: handle table kept growing past %lu bytes\n", size);
    return false;
}

void histogram(const std::vector<HandleRecord>& snap, std::map<std::wstring, unsigned>* out) {
    out->clear();
    for (const HandleRecord& r : snap)
        (*out)[type_name(r.typeIndex, reinterpret_cast<HANDLE>(r.value))] += 1;
}

// ---- the device cycle under test ----------------------------------------
// Deliberately IDENTICAL to helios_ownership_soak.cpp:254-270 — the same
// D3D11CreateDevice(adapter) then Release() pair, so this probe types the
// handles that harness counts, not a different workload's.

IDXGIAdapter1* find_adapter(IDXGIFactory1* factory, const wchar_t* match) {
    IDXGIAdapter1* adapter = nullptr;
    for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
        DXGI_ADAPTER_DESC1 desc{};
        adapter->GetDesc1(&desc);
        if (wcsstr(desc.Description, match) != nullptr) {
            std::wprintf(L"adapter: \"%s\"\n", desc.Description);
            return adapter;  // caller releases
        }
        adapter->Release();
    }
    return nullptr;
}

ID3D11Device* create_device(IDXGIAdapter1* adapter) {
    ID3D11Device* device = nullptr;
    D3D_FEATURE_LEVEL got{};
    const D3D_FEATURE_LEVEL levels[] = { D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1,
                                         D3D_FEATURE_LEVEL_10_0 };
    HRESULT hr = D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0, levels,
                                   ARRAYSIZE(levels), D3D11_SDK_VERSION, &device, &got, nullptr);
    if (FAILED(hr))
        return nullptr;
    return device;
}

}  // namespace

int main(int argc, char** argv) {
    std::setvbuf(stdout, nullptr, _IONBF, 0);

    unsigned cycles = 5;
    const wchar_t* adapterMatch = L"Helios";
    // argv[3]: a module-name prefix to hold loaded across the run ("all" for
    // every transient module). See the pinning block below.
    static wchar_t pinBuf[MAX_PATH] = {};
    const wchar_t* pinMatch = nullptr;
    std::vector<std::wstring> transientPaths;
    if (argc > 1) cycles = strtoul(argv[1], nullptr, 10);
    if (argc > 2 && argv[2][0] == 'w') adapterMatch = L"Basic Render";  // WARP control
    if (argc > 3 && argv[3][0]) {
        MultiByteToWideChar(CP_ACP, 0, argv[3], -1, pinBuf, MAX_PATH);
        pinMatch = pinBuf;
    }

    if (!load_ntdll()) {
        std::printf("FAIL: ntdll exports\n");
        return 1;
    }
    std::printf("helios handle-type probe: cycles=%u\n", cycles);

    IDXGIFactory1* factory = nullptr;
    if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), reinterpret_cast<void**>(&factory)))) {
        std::printf("FAIL: CreateDXGIFactory1\n");
        return 1;
    }
    IDXGIAdapter1* adapter = find_adapter(factory, adapterMatch);
    if (!adapter) {
        std::wprintf(L"FAIL: no adapter matching \"%s\" enumerated\n", adapterMatch);
        factory->Release();
        return 1;
    }

    // Warm up exactly as the soak does: the first device pulls in helios_umd,
    // DXVK and the venus ICD, and those one-time handles are not the leak.
    {
        ID3D11Device* warm = create_device(adapter);
        if (!warm) {
            std::printf("FAIL: D3D11CreateDevice on the selected adapter\n");
            adapter->Release();
            factory->Release();
            return 1;
        }
        warm->Release();
    }

    // ---- is the UMD's module graph torn down per device? ------------------
    // The soak's `modules +0` is sampled only BETWEEN cycles, so it is equally
    // consistent with "nothing loads" and with "loads and unloads inside every
    // cycle". Those have opposite implications, so ask directly, around one
    // cycle, at all three points.
    {
        auto module_paths = []() {
            std::vector<std::wstring> paths;
            HMODULE mods[1024];
            DWORD needed = 0;
            if (!EnumProcessModules(GetCurrentProcess(), mods, sizeof(mods), &needed))
                return paths;
            const DWORD n = needed / sizeof(HMODULE);
            for (DWORD i = 0; i < n; ++i) {
                wchar_t path[MAX_PATH] = {};
                if (GetModuleFileNameW(mods[i], path, MAX_PATH))
                    paths.push_back(path);
            }
            return paths;
        };
        auto has = [](const std::vector<std::wstring>& v, const std::wstring& n) {
            for (const std::wstring& s : v)
                if (_wcsicmp(s.c_str(), n.c_str()) == 0)
                    return true;
            return false;
        };
        auto base_of = [](const std::wstring& p) {
            const size_t slash = p.find_last_of(L'\\');
            return slash == std::wstring::npos ? p : p.substr(slash + 1);
        };

        const std::vector<std::wstring> pre = module_paths();
        ID3D11Device* d = create_device(adapter);
        const std::vector<std::wstring> live = module_paths();
        if (d)
            d->Release();
        const std::vector<std::wstring> post = module_paths();

        std::printf("\n--- modules loaded for one device and UNLOADED with it ---\n");
        for (const std::wstring& m : live) {
            if (!has(pre, m) && !has(post, m)) {
                transientPaths.push_back(m);
                std::wprintf(L"  %s\n", base_of(m).c_str());
            }
        }
        std::printf("  (%zu transient; %zu -> %zu -> %zu modules)\n", transientPaths.size(),
                    pre.size(), live.size(), post.size());
        for (const std::wstring& m : live) {
            if (!has(pre, m) && has(post, m))
                std::wprintf(L"  RETAINED: %s\n", base_of(m).c_str());
        }

        // ---- pin, to attribute the leak to a module --------------------
        // Each transient module's process-lifetime statics are destroyed by
        // nothing when the module unloads: a Rust `static OnceLock<File>` and a
        // C++ magic static both outlive DllMain's detach, and the loader does
        // not close handles a module opened. Holding a module loaded therefore
        // subtracts exactly its own never-released statics from the per-device
        // figure, which attributes the leak without a single hook.
        if (pinMatch) {
            std::printf("\n--- pinning modules matching \"%ls\" ---\n", pinMatch);
            for (const std::wstring& m : transientPaths) {
                const std::wstring b = base_of(m);
                if (_wcsicmp(pinMatch, L"all") != 0 &&
                    _wcsnicmp(b.c_str(), pinMatch, wcslen(pinMatch)) != 0)
                    continue;
                HMODULE h = LoadLibraryW(m.c_str());
                std::wprintf(L"  %-32s %s\n", b.c_str(), h ? L"pinned" : L"LoadLibrary FAILED");
            }
        }
    }

    // ---- TLS/FLS index high-water ----------------------------------------
    // TlsAlloc returns the LOWEST free index, so it reads out how many slots
    // are permanently taken. A process is capped at 1088 TLS slots and 128 FLS
    // slots; a DLL graph that is loaded and unloaded per device while leaking
    // its per-instance slots would exhaust them at a few hundred devices --
    // which is the scale at which this harness's sibling fail-fasts in
    // ucrtbase (0xC0000409, i.e. __fastfail, not a stack overrun).
    auto probe_slots = [](const char* label) {
        DWORD t = TlsAlloc();
        DWORD f = FlsAlloc(nullptr);
        std::printf("%-24s TlsAlloc->%lu  FlsAlloc->%lu\n", label, static_cast<unsigned long>(t),
                    static_cast<unsigned long>(f));
        if (t != TLS_OUT_OF_INDEXES) TlsFree(t);
        if (f != FLS_OUT_OF_INDEXES) FlsFree(f);
    };
    std::printf("\n--- slot high-water ---\n");
    probe_slots("before device cycles:");

    std::vector<HandleRecord> before;
    if (!snapshot(&before)) {
        adapter->Release();
        factory->Release();
        return 1;
    }

    unsigned failures = 0;
    for (unsigned i = 0; i < cycles; ++i) {
        ID3D11Device* d = create_device(adapter);
        if (!d) {
            ++failures;
            continue;
        }
        d->Release();
    }

    std::vector<HandleRecord> after;
    if (!snapshot(&after)) {
        adapter->Release();
        factory->Release();
        return 1;
    }
    probe_slots("after device cycles:");

    std::map<std::wstring, unsigned> hBefore, hAfter;
    histogram(before, &hBefore);
    histogram(after, &hAfter);

    std::printf("\nhandles: before=%zu after=%zu delta=%+d over %u cycles (%.2f/device), failures=%u\n",
                before.size(), after.size(),
                static_cast<int>(after.size()) - static_cast<int>(before.size()), cycles,
                cycles ? (static_cast<double>(static_cast<int>(after.size()) -
                                             static_cast<int>(before.size())) / cycles)
                       : 0.0,
                failures);

    std::printf("\n--- per-type counts (only types that moved) ---\n");
    for (const auto& kv : hAfter) {
        auto it = hBefore.find(kv.first);
        const unsigned wasCount = (it == hBefore.end()) ? 0 : it->second;
        if (wasCount != kv.second)
            std::wprintf(L"  %-24s %5u -> %-5u  %+d\n", kv.first.c_str(), wasCount, kv.second,
                         static_cast<int>(kv.second) - static_cast<int>(wasCount));
    }
    for (const auto& kv : hBefore) {
        if (hAfter.find(kv.first) == hAfter.end())
            std::wprintf(L"  %-24s %5u -> 0      %+d\n", kv.first.c_str(), kv.second,
                         -static_cast<int>(kv.second));
    }

    // Handle values are reused, so "new" means a value not live before. The
    // object address distinguishes N leaked objects from N refs to one.
    std::printf("\n--- handles live after and not before ---\n");
    unsigned printed = 0;
    for (const HandleRecord& a : after) {
        bool seen = false;
        for (const HandleRecord& b : before) {
            if (b.value == a.value) {
                seen = true;
                break;
            }
        }
        if (seen)
            continue;
        if (++printed > 80) {
            std::printf("  ... (further entries suppressed)\n");
            break;
        }
        const wchar_t* t = type_name(a.typeIndex, reinterpret_cast<HANDLE>(a.value));
        std::wstring name = object_name(reinterpret_cast<HANDLE>(a.value), t);
        std::wprintf(L"  h=0x%04llX  %-16s access=0x%08lX obj=%p  %s\n",
                     static_cast<unsigned long long>(a.value), t,
                     static_cast<unsigned long>(a.grantedAccess), a.object, name.c_str());
    }
    std::printf("  (%u new handles listed)\n", printed);

    adapter->Release();
    factory->Release();
    return 0;
}
