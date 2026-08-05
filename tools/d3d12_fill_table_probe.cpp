// d3d12_fill_table_probe.cpp -- does S6-0's pfnFillDDITable honour the runtime's
// SIZE_T, in BOTH directions, and does it leave zero NULL slots?
//
// This is the execution evidence for what PARALLEL.md section 3 calls "the
// single highest-consequence line in S6-0". It needs no adapter, no device and
// no caps answer, which matters: with pfnGetCaps still refusing (L1 has not
// landed), the D3D12 runtime abandons device creation two calls in and never
// reaches pfnFillDDITable at all -- measured at S5, tmp/dx12/gates/G6/RESULT.md.
// So the fill is driven directly, through helios_umd12.dll's
// helios_umd12_probe_fill_ddi_table_v1 export.
//
// WHAT IT PROVES, and why each check is the shape it is.
//
//  1. NO NULL SLOT. Every pointer-sized slot inside the byte count is non-NULL
//     after the fill. A WDDM UMD must fill every slot before returning or the
//     runtime calls through an uninitialised one.
//
//  2. R702, THE DANGEROUS DIRECTION. Ask for size_of(T) - 8 and check that the
//     GUARD BAND past the count is byte-for-byte the poison it was written with.
//     That is the failure the R702 class actually is: 24H2 passed 576 bytes for
//     a 592-byte DRIVERCAPS and the D3D11 driver wrote past it. A test that only
//     checks the filled prefix cannot see it.
//
//  3. THE OTHER DIRECTION. Ask for size_of(T) + 64 and check every slot in the
//     larger buffer is non-NULL -- i.e. a table shape newer than this build's
//     header leaves no hole, because the driver stubs the tail it cannot name.
//
//  4. THE STUBS ARE CALLABLE and they count. Call one slot through its
//     function-pointer type and check it returns 0 and does not fault.
//
//  5. AN UNSERVED TABLE TYPE WRITES NOTHING. Ask for a table type the driver
//     does not serve and check the whole buffer is still poison -- the property
//     DECISIONS.md section 7.4 actually demands of the closed dispatch.
//
// ASCII only, on purpose: this file lives on the Z:\ 9p share.
//
// Build (on the VM, from an x64 developer prompt):
//   cl /nologo /EHsc /W4 Z:\tools\d3d12_fill_table_probe.cpp
//      /Fe:C:\Users\Rupansh\d12s60\filltable.exe /link
//
// Run:
//   filltable.exe [path-to-helios_umd12.dll]
// Default path is the ProgramData hotplug location's newest helios_umd12_*.dll.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

namespace {

typedef long HRESULT_T;
typedef HRESULT_T(__cdecl* PFN_FILL)(int table_type, void* table, size_t table_size, unsigned index);
typedef size_t(__cdecl* PFN_SIZE)(int table_type);
typedef size_t(__cdecl* PFN_SLOT)(size_t);

const unsigned char kPoison = 0xA5;
const size_t kSlot = sizeof(void*);

int g_steps = 0;
int g_failures = 0;

void check(bool ok, const char* what) {
    ++g_steps;
    printf("%02d %-4s %s\n", g_steps, ok ? "OK" : "FAIL", what);
    if (!ok) {
        ++g_failures;
    }
}

// Every byte in [from, to) still holds the poison pattern.
bool untouched(const std::vector<unsigned char>& buf, size_t from, size_t to) {
    for (size_t i = from; i < to; ++i) {
        if (buf[i] != kPoison) {
            printf("     ... byte %zu is 0x%02X, expected 0x%02X\n", i, buf[i], kPoison);
            return false;
        }
    }
    return true;
}

// Every pointer-sized slot in [0, slots) is non-NULL.
bool all_slots_filled(const std::vector<unsigned char>& buf, size_t slots) {
    for (size_t i = 0; i < slots; ++i) {
        void* p = nullptr;
        std::memcpy(&p, buf.data() + i * kSlot, kSlot);
        if (p == nullptr) {
            printf("     ... slot %zu is NULL\n", i);
            return false;
        }
    }
    return true;
}

std::string newest_programdata_umd12() {
    const std::string dir = "C:\\ProgramData\\HeliosUmd\\";
    WIN32_FIND_DATAA fd = {};
    HANDLE h = FindFirstFileA((dir + "helios_umd12_*.dll").c_str(), &fd);
    if (h == INVALID_HANDLE_VALUE) {
        return std::string();
    }
    std::string best;
    FILETIME best_time = {};
    do {
        if (best.empty() || CompareFileTime(&fd.ftLastWriteTime, &best_time) > 0) {
            best = dir + fd.cFileName;
            best_time = fd.ftLastWriteTime;
        }
    } while (FindNextFileA(h, &fd));
    FindClose(h);
    return best;
}

struct Table {
    int type;
    const char* name;
    size_t expect_slots;  // from DECISIONS.md section 4.1, cross-checked
};

}  // namespace

int main(int argc, char** argv) {
    std::string dll = argc > 1 ? argv[1] : newest_programdata_umd12();
    if (dll.empty()) {
        printf("FAIL: no helios_umd12 DLL given and none found in C:\\ProgramData\\HeliosUmd\\\n");
        return 2;
    }
    printf("module: %s\n\n", dll.c_str());

    HMODULE m = LoadLibraryA(dll.c_str());
    if (!m) {
        printf("FAIL: LoadLibrary failed, GetLastError=%lu\n", GetLastError());
        return 2;
    }
    PFN_FILL fill = (PFN_FILL)GetProcAddress(m, "helios_umd12_probe_fill_ddi_table_v1");
    PFN_SIZE table_size = (PFN_SIZE)GetProcAddress(m, "helios_umd12_probe_ddi_table_size_v1");
    if (!fill || !table_size) {
        printf("FAIL: exports missing (fill=%p size=%p)\n", (void*)fill, (void*)table_size);
        return 2;
    }

    // DECISIONS.md section 4.1's canonical counts. The probe asks the DLL for
    // the BYTE size (so an SDK pin move cannot make this probe agree with a
    // previous header) and checks the derived slot count against these.
    const Table tables[] = {
        {0, "DEVICE_FUNCS_CORE_0109", 124},
        {1, "COMMAND_LIST_FUNCS_3D_0108", 75},
        {2, "COMMAND_QUEUE_FUNCS_CORE_0001", 7},
    };

    for (const Table& t : tables) {
        printf("\n-- %s --\n", t.name);
        const size_t bytes = table_size(t.type);
        char msg[256];

        _snprintf_s(msg, sizeof(msg), _TRUNCATE, "%s: header size %zu B = %zu slots (want %zu)",
                    t.name, bytes, bytes / kSlot, t.expect_slots);
        check(bytes != 0 && bytes % kSlot == 0 && bytes / kSlot == t.expect_slots, msg);
        if (bytes == 0) {
            continue;
        }

        // --- 1. exact size: every slot filled, nothing past it -------------
        {
            const size_t guard = 128;
            std::vector<unsigned char> buf(bytes + guard, kPoison);
            HRESULT_T hr = fill(t.type, buf.data(), bytes, 0);
            _snprintf_s(msg, sizeof(msg), _TRUNCATE, "%s: fill(exact) hr=0x%08lX",
                        t.name, (unsigned long)hr);
            check(hr == 0, msg);
            _snprintf_s(msg, sizeof(msg), _TRUNCATE, "%s: exact -> all %zu slots non-NULL",
                        t.name, bytes / kSlot);
            check(all_slots_filled(buf, bytes / kSlot), msg);
            _snprintf_s(msg, sizeof(msg), _TRUNCATE,
                        "%s: exact -> %zu-byte guard band untouched", t.name, guard);
            check(untouched(buf, bytes, bytes + guard), msg);

            // --- 4. a stub is callable and returns 0 -----------------------
            PFN_SLOT slot0 = nullptr;
            std::memcpy(&slot0, buf.data(), kSlot);
            size_t rv = slot0 ? slot0(0) : 1;
            _snprintf_s(msg, sizeof(msg), _TRUNCATE, "%s: slot 0 stub returned %zu (want 0)",
                        t.name, rv);
            check(slot0 != nullptr && rv == 0, msg);
        }

        // --- 2. R702: the runtime's count is SHORTER than our struct -------
        if (bytes > kSlot) {
            const size_t asked = bytes - kSlot;
            const size_t guard = 128;
            std::vector<unsigned char> buf(bytes + guard, kPoison);
            HRESULT_T hr = fill(t.type, buf.data(), asked, 0);
            _snprintf_s(msg, sizeof(msg), _TRUNCATE, "%s: fill(short=%zu) hr=0x%08lX",
                        t.name, asked, (unsigned long)hr);
            check(hr == 0, msg);
            _snprintf_s(msg, sizeof(msg), _TRUNCATE,
                        "%s: short -> all %zu asked slots non-NULL", t.name, asked / kSlot);
            check(all_slots_filled(buf, asked / kSlot), msg);
            // THE R702 CHECK. Not one byte past the count the caller gave.
            _snprintf_s(msg, sizeof(msg), _TRUNCATE,
                        "%s: short -> bytes %zu..%zu UNTOUCHED (R702)", t.name, asked,
                        bytes + guard);
            check(untouched(buf, asked, bytes + guard), msg);
        }

        // --- 3. the runtime's count is LONGER than our struct --------------
        {
            const size_t asked = bytes + 64;
            const size_t guard = 128;
            std::vector<unsigned char> buf(asked + guard, kPoison);
            HRESULT_T hr = fill(t.type, buf.data(), asked, 0);
            _snprintf_s(msg, sizeof(msg), _TRUNCATE, "%s: fill(long=%zu) hr=0x%08lX",
                        t.name, asked, (unsigned long)hr);
            check(hr == 0, msg);
            _snprintf_s(msg, sizeof(msg), _TRUNCATE,
                        "%s: long -> all %zu slots non-NULL incl. the unknown tail",
                        t.name, asked / kSlot);
            check(all_slots_filled(buf, asked / kSlot), msg);
            _snprintf_s(msg, sizeof(msg), _TRUNCATE,
                        "%s: long -> %zu-byte guard band untouched", t.name, guard);
            check(untouched(buf, asked, asked + guard), msg);
        }
    }

    // --- 5. an unserved table type writes NOTHING --------------------------
    printf("\n-- refusals --\n");
    {
        const size_t bytes = 992;
        std::vector<unsigned char> buf(bytes, kPoison);
        // 3 is D3D12DDI_TABLE_TYPE_DXGI: a real enumerator, deliberately not
        // served (D12-G5 measured that this runtime never requests it).
        HRESULT_T hr = fill(3, buf.data(), bytes, 0);
        check(hr != 0, "unserved table type 3 (DXGI) is refused");
        check(untouched(buf, 0, bytes), "unserved table type 3 wrote NOTHING");
    }
    {
        const size_t bytes = 992;
        std::vector<unsigned char> buf(bytes, kPoison);
        HRESULT_T hr = fill(9999, buf.data(), bytes, 0);
        check(hr != 0, "unknown table type 9999 is refused");
        check(untouched(buf, 0, bytes), "unknown table type 9999 wrote NOTHING");
    }
    {
        HRESULT_T hr = fill(0, nullptr, 992, 0);
        check(hr != 0, "null table pointer is refused");
    }
    {
        std::vector<unsigned char> buf(64, kPoison);
        HRESULT_T hr = fill(0, buf.data(), 0, 0);
        check(hr != 0, "zero table size is refused");
        check(untouched(buf, 0, buf.size()), "zero table size wrote NOTHING");
    }

    printf("\n%d steps, %d failures\n", g_steps, g_failures);
    // Deliberately NOT FreeLibrary: the DLL's process-lifetime log handle is
    // released in DllMain(DLL_PROCESS_DETACH), and letting process exit do it
    // keeps the log file open for the lines the checks above produced.
    return g_failures == 0 ? 0 : 1;
}
