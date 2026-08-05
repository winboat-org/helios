// d3d12_warp_spy.cpp — the D12-G5 contract-capture proxy.
//
// WHAT THIS IS. `C:\Windows\System32\d3d10warp.dll` exports `OpenAdapter12`: it is
// Microsoft's own D3D12 user-mode display driver, on this exact Windows build. This DLL
// exports `OpenAdapter12` too, forwards to WARP's, and logs the contract the D3D12 runtime
// actually drives — the version negotiation, every `pfnGetCaps(Type, DataSize)` and its
// answer, every `pfnFillDDITable(TableType, TableSize)` pair, and an ordered call trace of
// every table slot the runtime touches. There has never been a public D3D12 UMD and
// Microsoft publishes ~600 auto-generated reference stubs with no Remarks, so this log is
// the only first-hand source that exists (DECISIONS.md H1, GATES.md §4.6).
//
// ⛔ THIS IS NOT THE START OF THE REAL UMD. `DECISIONS.md` §7.1 (R908) is the standing
// record of what unreachable D3D12 scaffolding costs: ~230 lines behind
// `#[allow(unreachable_code)]`, deleted. Nothing here is a driver body — every function is
// a logging thunk over WARP's, and none of it is meant to be ported. `helios_umd12` gets
// its structs from the WDK header through bindgen with `layout_tests(true)`, not from here.
//
// BUILD: build.ps1 (cl + ml64, to a LOCAL C: path — never Z:\, the 9p share fails linker
// and Rust file IO with OS error 87).
//
// THE THUNK MECHANISM. 124 + 75 + 7 = 206 driver-side slots, each a differently typed
// `extern "system"` pointer. §7.3(2) forbids hand-writing D3D12 slot signatures, so the
// generic per-slot thunks are eight instructions of generated assembly
// (`gen_slots.py` -> `spy_thunks.asm`) that touch only R10/R11/flags — all volatile in the
// Microsoft x64 ABI, none of them an argument register — use no stack at all, and tail-jump
// to WARP's real pointer. They never need to know the slot's signature. A handful of slots
// that carry an answer this gate is specifically after (shaders, descriptor handles, heap
// sizes, present) additionally get a *typed* C++ hook, installed over the generic one, that
// calls through the real header typedef.
//
// ⚠ Deviation from DDI_REFERENCE.md §15.2's build sheet, recorded on purpose: that sheet
// specified WinLibs g++ with `__declspec(naked)` + GCC inline asm. GCC has no `naked`
// attribute on x86-64 and `__declspec(naked)` is MSVC syntax, so that recipe cannot build.
// The mechanism is the same idea; the toolchain is cl + ml64, which is also what GATES.md
// §4.6's command block already assumed.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#define _CRT_SECURE_NO_WARNINGS
#include <windows.h>

/* d3dkmddi.h (pulled in by d3d10umddi.h, which d3d12umddi.h includes) uses NTSTATUS,
 * which the user-mode windows.h does not define. Same incantation as the D3D11 UMD's
 * bindgen wrapper, umd/bindgen/d3d10umddi_wrapper.h:14-18. */
#ifndef _NTDEF_
typedef LONG NTSTATUS, *PNTSTATUS;
#endif

#include <d3d12umddi.h>

#include <stddef.h>
#include <stdio.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>

// ---------------------------------------------------------------------------------------
// Layout assertions. The generated assembly indexes these tables by raw byte offset, so a
// header whose shape moved must fail the build, not mislabel the log.
// ---------------------------------------------------------------------------------------
#define SPY_CORE_SLOTS 124
#define SPY_CL_SLOTS 75
#define SPY_QUEUE_SLOTS 7
#define SPY_ADAPTER_SLOTS 8
#define SPY_DXGI_SLOTS 32  // must match gen_slots.py DXGI_SLOTS

static_assert(sizeof(D3D12DDI_DEVICE_FUNCS_CORE_0109) == SPY_CORE_SLOTS * sizeof(void*),
              "DEVICE_FUNCS_CORE_0109 is not 124 pointers - regenerate spy_thunks.asm");
static_assert(sizeof(D3D12DDI_COMMAND_LIST_FUNCS_3D_0108) == SPY_CL_SLOTS * sizeof(void*),
              "COMMAND_LIST_FUNCS_3D_0108 is not 75 pointers - regenerate spy_thunks.asm");
static_assert(sizeof(D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001) == SPY_QUEUE_SLOTS * sizeof(void*),
              "COMMAND_QUEUE_FUNCS_CORE_0001 is not 7 pointers - regenerate spy_thunks.asm");
static_assert(sizeof(D3D12DDI_ADAPTERFUNCS_0109) == SPY_ADAPTER_SLOTS * sizeof(void*),
              "ADAPTERFUNCS_0109 is not 8 pointers");
static_assert(sizeof(D3D12DDI_CORELAYER_DEVICECALLBACKS_0062) == 18 * sizeof(void*),
              "CORELAYER_DEVICECALLBACKS_0062 is not 18 live pointers - the "
              "D3D_UMD_INTERFACE_VERSION gates are not what DECISIONS.md 4.1 counted");

// The tokens this build knows how to decode. Never hand-written (DECISIONS.md §7.2): taken
// from the header's own macros so a moved SDK moves them too.
static const UINT64 kSupported0109 = D3D12DDI_SUPPORTED_0109;
static const UINT64 kSupported0110 = D3D12DDI_SUPPORTED_0110;

// ---------------------------------------------------------------------------------------
// State shared with spy_thunks.asm. Definitions must stay extern "C", global and mutable.
// ---------------------------------------------------------------------------------------
enum : UINT32 {
    kTraceCap = 1u << 20,  // must match TRACE_CAP in gen_slots.py
    kTagCore = 0,
    kTagCl = 1,
    kTagQueue = 2,
    kTagDxgi = 3,
    kTagMark = 0x80,
};

extern "C" {
UINT32 g_spy_trace[kTraceCap];
volatile LONG g_spy_trace_idx;

volatile LONG g_spy_core_hits[SPY_CORE_SLOTS];
volatile LONG g_spy_cl_hits[SPY_CL_SLOTS];
volatile LONG g_spy_queue_hits[SPY_QUEUE_SLOTS];
volatile LONG g_spy_dxgi_hits[SPY_DXGI_SLOTS];

void* g_spy_core_snapshot[SPY_CORE_SLOTS];
void* g_spy_cl_snapshot[SPY_CL_SLOTS];
void* g_spy_queue_snapshot[SPY_QUEUE_SLOTS];
void* g_spy_dxgi_snapshot[SPY_DXGI_SLOTS];
}

// Generated thunk entry points.
#define X(i, name) extern "C" void spy_core_##i();
#include "slots_core_0109.inc"
#undef X
#define X(i, name) extern "C" void spy_cl_##i();
#include "slots_cl_0108.inc"
#undef X
#define X(i, name) extern "C" void spy_queue_##i();
#include "slots_queue_0001.inc"
#undef X
#define X(i, name) extern "C" void spy_dxgi_##i();
#include "slots_dxgi.inc"
#undef X

static void* const kCoreThunks[SPY_CORE_SLOTS] = {
#define X(i, name) (void*)&spy_core_##i,
#include "slots_core_0109.inc"
#undef X
};
static void* const kClThunks[SPY_CL_SLOTS] = {
#define X(i, name) (void*)&spy_cl_##i,
#include "slots_cl_0108.inc"
#undef X
};
static void* const kQueueThunks[SPY_QUEUE_SLOTS] = {
#define X(i, name) (void*)&spy_queue_##i,
#include "slots_queue_0001.inc"
#undef X
};
static void* const kDxgiThunks[SPY_DXGI_SLOTS] = {
#define X(i, name) (void*)&spy_dxgi_##i,
#include "slots_dxgi.inc"
#undef X
};

static const char* const kCoreNames[SPY_CORE_SLOTS] = {
#define X(i, name) #name,
#include "slots_core_0109.inc"
#undef X
};
static const char* const kClNames[SPY_CL_SLOTS] = {
#define X(i, name) #name,
#include "slots_cl_0108.inc"
#undef X
};
static const char* const kQueueNames[SPY_QUEUE_SLOTS] = {
#define X(i, name) #name,
#include "slots_queue_0001.inc"
#undef X
};
static const char* const kAdapterNames[SPY_ADAPTER_SLOTS] = {
#define X(i, name) #name,
#include "slots_adapter_0109.inc"
#undef X
};

struct SpyTable {
    const char* const* names;
    UINT32 count;
    volatile LONG* hits;
};
static const SpyTable kTables[4] = {
    {kCoreNames, SPY_CORE_SLOTS, g_spy_core_hits},
    {kClNames, SPY_CL_SLOTS, g_spy_cl_hits},
    {kQueueNames, SPY_QUEUE_SLOTS, g_spy_queue_hits},
    {nullptr, SPY_DXGI_SLOTS, g_spy_dxgi_hits},  // DXGI slot names are not in this header
};
static const char* const kTableTag[4] = {"core", "cl", "queue", "dxgi"};

// ---------------------------------------------------------------------------------------
// Symbolic names for the two enums the log must print in decimal AND symbolically.
// ---------------------------------------------------------------------------------------
struct CapName {
    UINT32 value;
    const char* name;
    int deprecated;
};
static const CapName kCaps[] = {
#define CAP(v, n, d) {v, #n, d},
#include "caps_types.inc"
#undef CAP
};
static const UINT32 kCapCount = (UINT32)(sizeof(kCaps) / sizeof(kCaps[0]));

struct TblName {
    UINT32 value;
    const char* name;
};
static const TblName kTableNames[] = {
#define TBL(v, n) {v, #n},
#include "table_types.inc"
#undef TBL
};
static const UINT32 kTableNameCount = (UINT32)(sizeof(kTableNames) / sizeof(kTableNames[0]));

static const char* cap_name(UINT32 v) {
    for (UINT32 i = 0; i < kCapCount; ++i)
        if (kCaps[i].value == v) return kCaps[i].name;
    return "??? NOT A D3D12DDICAPS_TYPE ENUMERATOR IN SDK 10.0.26100.0";
}
static const char* table_name(UINT32 v) {
    for (UINT32 i = 0; i < kTableNameCount; ++i)
        if (kTableNames[i].value == v) return kTableNames[i].name;
    return "??? NOT A D3D12DDI_TABLE_TYPE ENUMERATOR";
}

// ---------------------------------------------------------------------------------------
// Log. One line per event, flushed per line, with a sequence number that is also pushed
// into the call-trace ring so the two orderings can be merged at dump time.
// ---------------------------------------------------------------------------------------
static CRITICAL_SECTION g_log_lock;
static FILE* g_log;
static volatile LONG g_log_seq;
static LARGE_INTEGER g_qpc_freq, g_qpc_start;

static void spy_trace_push(UINT32 code) {
    LONG i = InterlockedExchangeAdd(&g_spy_trace_idx, 1);
    if ((UINT32)i < kTraceCap) g_spy_trace[i] = code;
}

static void logf(const char* fmt, ...) {
    LONG seq = InterlockedIncrement(&g_log_seq);
    spy_trace_push((kTagMark << 24) | ((UINT32)seq & 0x00FFFFFF));
    if (!g_log) return;
    LARGE_INTEGER now;
    QueryPerformanceCounter(&now);
    double ms = g_qpc_freq.QuadPart
                    ? (double)(now.QuadPart - g_qpc_start.QuadPart) * 1000.0
                          / (double)g_qpc_freq.QuadPart
                    : 0.0;
    EnterCriticalSection(&g_log_lock);
    fprintf(g_log, "[%06ld %9.3fms t%04lu] ", seq, ms, GetCurrentThreadId());
    va_list ap;
    va_start(ap, fmt);
    vfprintf(g_log, fmt, ap);
    va_end(ap);
    fputc('\n', g_log);
    fflush(g_log);
    LeaveCriticalSection(&g_log_lock);
}

static void log_hex(const char* label, const void* p, size_t n) {
    if (!p) {
        logf("%s = NULL", label);
        return;
    }
    char buf[3 * 64 + 1];
    size_t k = n > 64 ? 64 : n;
    const unsigned char* b = (const unsigned char*)p;
    for (size_t i = 0; i < k; ++i) sprintf(buf + 3 * i, "%02x ", b[i]);
    buf[3 * k] = 0;
    logf("%s [%zu bytes, first %zu] %s", label, n, k, buf);
}

// Named refusal counters. CLAUDE.md rule 2: every skipped or refused path gets a name.
static struct {
    LONG warp_load_failed;
    LONG warp_wrong_path;
    LONG warp_no_export;
    LONG unknown_version;
    LONG table_truncated;
    LONG table_refilled;
    LONG unknown_table;
    LONG corelayer_arm_unknown;
    LONG gate_knob_off;
    LONG gate_wrong_process;
} g_refuse;

#define SPY_REFUSE(field, ...)                 \
    do {                                       \
        InterlockedIncrement(&g_refuse.field); \
        logf("REFUSE " #field ": " __VA_ARGS__); \
    } while (0)

// ---------------------------------------------------------------------------------------
// Mutation arms. Off unless HELIOS_D12SPY_MUTATE names one. Each arm answers a specific
// question and every one of them is logged at load and again at the moment it fires.
//   range  - force an out-of-range tier into D3D12_OPTIONS (does the runtime range-check?)
//   cross  - raytracing tier 1.1 while the shader-model list is clamped to 6.0 (the
//            runtime's own string says "Drivers that support raytracing must expose shader
//            model 6.3."): does it validate the caps set as ONE contract? (Q2, §15 #13)
//   sm65   - clamp the reported shader models to 6.5 (7.17)
//   capfail- return E_INVALIDARG for D3D12DDICAPS_TYPE_OPTIONS_0110 (§11.2's UNVERIFIED:
//            what does the runtime do with a failing HRESULT for a cap it asked for?)
//   tier   - answer a LEGAL but different ResourceBindingTier (HELIOS_D12SPY_TIERVAL,
//            default 2). The control for `range`: it separates "the runtime clamps an
//            illegal tier" from "the runtime ignores the driver's answer entirely".
//   forcever- overwrite pfnGetSupportedVersions' answer with D3D12DDI_SUPPORTED_0110.
//            On the Helios adapter (which declares WDDM 2.1) WARP offers exactly ONE
//            version and it is a D3D11-era token, so D3D12CreateDevice fails. This arm
//            separates the two possible causes: WARP's own adapter-derived policy, or the
//            D3D12 RUNTIME refusing a D3D12 DDI version on a WDDM 2.1 adapter. Which one
//            it is decides whether P3 is blocked on raising kmd_render's WddmSurface.
// ---------------------------------------------------------------------------------------
enum SpyMutate { kMutNone = 0, kMutRange, kMutCross, kMutSm65, kMutCapFail, kMutTier,
                 kMutForceVer };
static SpyMutate g_mutate = kMutNone;
static const char* g_mutate_name = "none";

// ---------------------------------------------------------------------------------------
// The real WARP, and the negotiation state observed from it.
// ---------------------------------------------------------------------------------------
static HMODULE g_warp;
static D3D12DDI_ADAPTERFUNCS_0109 g_adapter_real;
static D3D12DDI_DEVICE_FUNCS_CORE_0109 g_core_real;
static D3D12DDI_COMMAND_LIST_FUNCS_3D_0108 g_cl_real;
static D3D12DDI_CORELAYER_DEVICECALLBACKS_0062 g_corelayer_real;
static bool g_corelayer_captured;
static UINT64 g_versions[256];  // WARP reports 77 on this build; 64 truncated it
static UINT32 g_version_count;
static UINT64 g_negotiated;  // the (Interface<<32)|Version pair the runtime came back with
static bool g_negotiated_is_0109;
static volatile LONG g_present_count;
static volatile LONG g_dumped;
static LONG g_dump_after_presents = 3;

static void spy_dump(const char* why);

// ---------------------------------------------------------------------------------------
// Load the real WARP.
//
// ⚠ A bare LoadLibrary("d3d10warp.dll") from a DLL *named* d3d10warp.dll re-enters itself,
// and neither LOAD_LIBRARY_SEARCH_SYSTEM32 nor a full path fixes it: the loader's
// already-loaded check matches on BASE NAME, so it hands back the module with that name
// that is already in the process — us (DECISIONS.md P-A, §6.1). The only load that is safe
// is a copy under a DIFFERENT base name, taken from System32 by build.ps1, loaded by full
// path, and then verified with GetModuleFileNameW. Without the verification step, "I loaded
// myself and every log line is a lie" is indistinguishable from success.
// ---------------------------------------------------------------------------------------
static bool spy_load_warp() {
    wchar_t self[MAX_PATH], want[MAX_PATH], got[MAX_PATH];
    HMODULE hself = nullptr;
    GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                           | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                       (LPCWSTR)&spy_load_warp, &hself);
    DWORD n = GetModuleFileNameW(hself, self, MAX_PATH);
    if (!n || n >= MAX_PATH) {
        SPY_REFUSE(warp_load_failed, "GetModuleFileNameW(self) failed, gle=%lu",
                   GetLastError());
        return false;
    }
    logf("spy module = %ls", self);

    // <our directory>\d3d10warp_real.dll — build.ps1 copies System32's there and records
    // its SHA-256 next to the log.
    wcscpy_s(want, MAX_PATH, self);
    wchar_t* slash = wcsrchr(want, L'\\');
    if (!slash) {
        SPY_REFUSE(warp_load_failed, "self path has no directory separator");
        return false;
    }
    wcscpy_s(slash + 1, MAX_PATH - (slash + 1 - want), L"d3d10warp_real.dll");

    g_warp = LoadLibraryExW(want, nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
    if (!g_warp) {
        SPY_REFUSE(warp_load_failed, "LoadLibraryExW(%ls) gle=%lu", want, GetLastError());
        return false;
    }
    if (!GetModuleFileNameW(g_warp, got, MAX_PATH) || _wcsicmp(got, want) != 0) {
        SPY_REFUSE(warp_wrong_path, "asked for %ls, got %ls", want, got);
        FreeLibrary(g_warp);
        g_warp = nullptr;
        return false;
    }
    if (g_warp == hself) {
        SPY_REFUSE(warp_wrong_path, "resolved to the spy itself");
        g_warp = nullptr;
        return false;
    }
    logf("real WARP = %ls (base %p)", got, (void*)g_warp);
    return true;
}

// ---------------------------------------------------------------------------------------
// Adapter-table thunks. All 8 are typed, because the adapter table IS the negotiation and
// its argument structs are exactly what this gate exists to read.
// ---------------------------------------------------------------------------------------
static void note_adapter(UINT32 slot) { spy_trace_push((3u << 24) | 0x00800000u | slot); }

static HRESULT APIENTRY spy_GetSupportedVersions(D3D12DDI_HADAPTER hAdapter,
                                                 UINT32* puEntries,
                                                 UINT64* pVersions) {
    note_adapter(3);
    logf("pfnGetSupportedVersions ENTER puEntries=%p (*=%u) pVersions=%p  <-- %s call",
         (void*)puEntries, puEntries ? *puEntries : 0, (void*)pVersions,
         pVersions ? "FILL" : "COUNT");
    HRESULT hr = g_adapter_real.pfnGetSupportedVersions(hAdapter, puEntries, pVersions);
    logf("pfnGetSupportedVersions -> 0x%08lx, *puEntries=%u", (unsigned long)hr,
         puEntries ? *puEntries : 0);
    if (SUCCEEDED(hr) && puEntries && pVersions) {
        UINT32 n = *puEntries;
        if (n > 256) n = 256;
        g_version_count = n;
        for (UINT32 i = 0; i < n; ++i) {
            g_versions[i] = pVersions[i];
            logf("  version[%u] = 0x%016llx  (Interface=0x%08x Version=0x%08x)", i,
                 (unsigned long long)pVersions[i], (unsigned)(pVersions[i] >> 32),
                 (unsigned)(pVersions[i] & 0xFFFFFFFFull));
        }
    }
    // DDI_REFERENCE.md sec.15.4's version-floor probe. ⛔ Only the FILL answer is edited:
    // WARP must see BOTH of its own calls unmodified, or (a) forcing the COUNT to 1 makes
    // its own FILL return ERROR_INSUFFICIENT_BUFFER (0x8007007A) for a 77-entry list, and
    // (b) skipping the call entirely crashes it at 0xC0000005 later -- this is where WARP
    // initialises the state pfnCalcPrivateDeviceSize needs. Shrinking the count on the way
    // OUT is safe: the runtime's buffer is already big enough.
    if (g_mutate == kMutForceVer && SUCCEEDED(hr) && puEntries && pVersions && *puEntries) {
        char v[32] = "";
        UINT64 tok = kSupported0110;
        if (GetEnvironmentVariableA("HELIOS_D12SPY_VER", v, sizeof(v)))
            tok = _strtoui64(v, nullptr, 16);
        logf("MUTATE forcever: replacing the %u-entry answer with exactly 0x%016llx",
             *puEntries, (unsigned long long)tok);
        pVersions[0] = tok;
        *puEntries = 1;
        g_versions[0] = tok;
        g_version_count = 1;
    }
    return hr;
}

static void log_caps_answer(const D3D12DDIARG_GETCAPS* a, HRESULT hr) {
    logf("pfnGetCaps Type=%u (%s) DataSize=%u pInfo=%p%s pData=%p -> 0x%08lx",
         (unsigned)a->Type, cap_name((UINT32)a->Type), a->DataSize, a->pInfo,
         a->pInfo ? "" : " (NULL)", a->pData, (unsigned long)hr);
    if (a->pInfo) log_hex("  pInfo", a->pInfo, 8);
    if (a->pData && SUCCEEDED(hr)) log_hex("  pData", a->pData, a->DataSize);
}

static HRESULT APIENTRY spy_GetCaps(D3D12DDI_HADAPTER hAdapter,
                                    CONST D3D12DDIARG_GETCAPS* pArgs) {
    note_adapter(4);
    if (!pArgs) {
        logf("pfnGetCaps with NULL pArgs (!)");
        return g_adapter_real.pfnGetCaps(hAdapter, pArgs);
    }
    if (g_mutate == kMutCapFail && pArgs->Type == D3D12DDICAPS_TYPE_OPTIONS_0110) {
        logf("MUTATE capfail: refusing Type=%u (%s) with E_INVALIDARG without calling WARP",
             (unsigned)pArgs->Type, cap_name((UINT32)pArgs->Type));
        return E_INVALIDARG;
    }
    HRESULT hr = g_adapter_real.pfnGetCaps(hAdapter, pArgs);
    log_caps_answer(pArgs, hr);

    if (SUCCEEDED(hr) && pArgs->pData) {
        if (pArgs->Type == D3D12DDICAPS_TYPE_D3D12_OPTIONS
            && pArgs->DataSize == sizeof(D3D12DDI_D3D12_OPTIONS_DATA_0089)) {
            D3D12DDI_D3D12_OPTIONS_DATA_0089* o =
                (D3D12DDI_D3D12_OPTIONS_DATA_0089*)pArgs->pData;
            logf("  OPTIONS: ResourceBindingTier=%u TiledResourcesTier=%u "
                 "ConservativeRasterTier=%u ResourceHeapTier=%u RenderPassTier=%u "
                 "RaytracingTier=%u MeshShaderTier=%u SamplerFeedbackTier=%u "
                 "EnhancedBarriers=%d",
                 (unsigned)o->ResourceBindingTier, (unsigned)o->TiledResourcesTier,
                 (unsigned)o->ConservativeRasterizationTier, (unsigned)o->ResourceHeapTier,
                 (unsigned)o->RenderPassTier, (unsigned)o->RaytracingTier,
                 (unsigned)o->MeshShaderTier, (unsigned)o->SamplerFeedbackTier,
                 (int)o->EnhancedBarriersSupported);
            if (g_mutate == kMutTier) {
                char v[16] = "2";
                GetEnvironmentVariableA("HELIOS_D12SPY_TIERVAL", v, sizeof(v));
                logf("MUTATE tier: ResourceBindingTier %u -> %s (legal value)",
                     (unsigned)o->ResourceBindingTier, v);
                o->ResourceBindingTier = (D3D12DDI_RESOURCE_BINDING_TIER)atoi(v);
            }
            if (g_mutate == kMutRange) {
                logf("MUTATE range: ResourceBindingTier %u -> 99 (illegal)",
                     (unsigned)o->ResourceBindingTier);
                o->ResourceBindingTier = (D3D12DDI_RESOURCE_BINDING_TIER)99;
            }
            if (g_mutate == kMutCross) {
                logf("MUTATE cross: RaytracingTier %u -> 1.1 while shader models are "
                     "clamped to 6.0",
                     (unsigned)o->RaytracingTier);
                o->RaytracingTier = D3D12DDI_RAYTRACING_TIER_1_1;
            }
        } else if (pArgs->Type == D3D12DDICAPS_TYPE_0011_SHADER_MODELS
                   && pArgs->DataSize == sizeof(D3D12DDI_D3D12_SHADER_MODELS_DATA_0011)) {
            D3D12DDI_D3D12_SHADER_MODELS_DATA_0011* s =
                (D3D12DDI_D3D12_SHADER_MODELS_DATA_0011*)pArgs->pData;
            UINT n = (s->pNumShaderModelsSupported) ? *s->pNumShaderModelsSupported : 0;
            logf("  SHADER_MODELS: count=%u", n);
            for (UINT i = 0; i < n && s->pShaderModelsSupported && i < 32; ++i)
                logf("    [%u] 0x%08x  (%u.%u %s)", i,
                     (unsigned)s->pShaderModelsSupported[i],
                     (unsigned)((s->pShaderModelsSupported[i] >> 16) & 0xFFFF),
                     (unsigned)((s->pShaderModelsSupported[i] & 0xFFFF) / 0x10),
                     ((s->pShaderModelsSupported[i] & 0xF) == 5) ? "RELEASE"
                                                                 : "EXPERIMENTAL?");
            if ((g_mutate == kMutSm65 || g_mutate == kMutCross) && n
                && s->pShaderModelsSupported && s->pNumShaderModelsSupported) {
                UINT cap = (g_mutate == kMutSm65) ? D3D12DDI_SHADER_MODEL_6_5_RELEASE_0071
                                                  : D3D12DDI_SHADER_MODEL_6_0_RELEASE_0011;
                UINT keep = 0;
                for (UINT i = 0; i < n; ++i)
                    if ((UINT)s->pShaderModelsSupported[i] <= cap)
                        s->pShaderModelsSupported[keep++] = s->pShaderModelsSupported[i];
                logf("MUTATE %s: shader-model list %u -> %u entries (ceiling 0x%08x)",
                     g_mutate_name, n, keep, cap);
                *s->pNumShaderModelsSupported = keep;
            }
        } else if (pArgs->Type == D3D12DDICAPS_TYPE_3DPIPELINESUPPORT
                   && pArgs->DataSize >= sizeof(UINT)) {
            logf("  3DPIPELINESUPPORT: level = %u", *(const UINT*)pArgs->pData);
        }
    }
    return hr;
}

static HRESULT APIENTRY spy_GetOptionalDDITables(D3D12DDI_HADAPTER hAdapter,
                                                 UINT32* puEntries,
                                                 D3D12DDI_TABLE_REQUEST* pReq) {
    note_adapter(5);
    logf("pfnGetOptionalDDITables ENTER *puEntries=%u pReq=%p", puEntries ? *puEntries : 0,
         (void*)pReq);
    HRESULT hr = g_adapter_real.pfnGetOptionalDDITables(hAdapter, puEntries, pReq);
    logf("pfnGetOptionalDDITables -> 0x%08lx, *puEntries=%u", (unsigned long)hr,
         puEntries ? *puEntries : 0);
    if (SUCCEEDED(hr) && puEntries && pReq)
        for (UINT32 i = 0; i < *puEntries && i < 32; ++i)
            logf("  request[%u] tableType=%u (%s) numTables=%u", i,
                 (unsigned)pReq[i].tableType, table_name((UINT32)pReq[i].tableType),
                 pReq[i].numTables);
    return hr;
}

// ---------------------------------------------------------------------------------------
// Typed hooks over individual core / command-list slots. Each one exists to answer a
// specific numbered question; they are installed *after* the generic thunks, so a slot with
// a typed hook is traced by the hook rather than by the assembly stub.
// ---------------------------------------------------------------------------------------
static void note_core(size_t slot) {
    InterlockedIncrement(&g_spy_core_hits[slot]);
    spy_trace_push((kTagCore << 24) | (UINT32)slot);
}
static void note_cl(size_t slot) {
    InterlockedIncrement(&g_spy_cl_hits[slot]);
    spy_trace_push((kTagCl << 24) | (UINT32)slot);
}
#define CORE_SLOT(member) (offsetof(D3D12DDI_DEVICE_FUNCS_CORE_0109, member) / sizeof(void*))
#define CL_SLOT(member) \
    (offsetof(D3D12DDI_COMMAND_LIST_FUNCS_3D_0108, member) / sizeof(void*))

// Q1 / §15 #14: the DDI passes no length parameter anywhere, so the blob must be
// self-describing. Dump the first eight dwords: 0x43425844 ('DXBC') in dword 0 means a
// container (size at dword 6); anything else means a raw SM4/SM5 token stream whose
// dword 1 is the length in dwords (DDI_REFERENCE.md §12.2).
static void log_shader_code(const char* who, CONST D3D12DDIARG_CREATE_SHADER_0026* a) {
    if (!a || !a->pShaderCode) {
        logf("%s: pShaderCode = NULL", who);
        return;
    }
    const UINT* c = a->pShaderCode;
    logf("%s: dwords %08x %08x %08x %08x %08x %08x %08x %08x | %s | hRootSignature=%p "
         "Flags=0x%x IOSig=%p",
         who, c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7],
         (c[0] == 0x43425844u) ? "DXBC CONTAINER (size at dword[6])"
                               : "raw token stream (len at dword[1])",
         a->hRootSignature.pDrvPrivate, (unsigned)a->Flags,
         (const void*)a->IOSignatures.Standard);
}

#define SPY_CREATE_SHADER_HOOK(NAME, MEMBER)                                            \
    static VOID APIENTRY NAME(D3D12DDI_HDEVICE hDevice,                                 \
                              CONST D3D12DDIARG_CREATE_SHADER_0026* pArgs,              \
                              D3D12DDI_HSHADER hShader) {                               \
        note_core(CORE_SLOT(MEMBER));                                                   \
        log_shader_code(#MEMBER, pArgs);                                                \
        g_core_real.MEMBER(hDevice, pArgs, hShader);                                    \
    }

SPY_CREATE_SHADER_HOOK(spy_CreateVertexShader, pfnCreateVertexShader)
SPY_CREATE_SHADER_HOOK(spy_CreatePixelShader, pfnCreatePixelShader)
SPY_CREATE_SHADER_HOOK(spy_CreateGeometryShader, pfnCreateGeometryShader)
SPY_CREATE_SHADER_HOOK(spy_CreateComputeShader, pfnCreateComputeShader)
SPY_CREATE_SHADER_HOOK(spy_CreateHullShader, pfnCreateHullShader)
SPY_CREATE_SHADER_HOOK(spy_CreateDomainShader, pfnCreateDomainShader)
SPY_CREATE_SHADER_HOOK(spy_CreateAmplificationShader, pfnCreateAmplificationShader)
SPY_CREATE_SHADER_HOOK(spy_CreateMeshShader, pfnCreateMeshShader)

static SIZE_T APIENTRY spy_CalcPrivateShaderSize(D3D12DDI_HDEVICE hDevice,
                                                 CONST D3D12DDIARG_CREATE_SHADER_0026* a) {
    note_core(CORE_SLOT(pfnCalcPrivateShaderSize));
    log_shader_code("pfnCalcPrivateShaderSize", a);
    SIZE_T n = g_core_real.pfnCalcPrivateShaderSize(hDevice, a);
    logf("pfnCalcPrivateShaderSize -> %zu", n);
    return n;
}

// H3 / §9.6: both descriptor handles are opaque driver-chosen scalars returned BY VALUE,
// and the stride is the driver's choice too. Reading WARP's actual numbers is what turns
// "a forwarder can pass vkd3d's handles straight through" from a plan into a checked one.
static UINT APIENTRY spy_GetDescriptorSizeInBytes(D3D12DDI_HDEVICE hDevice,
                                                  D3D12DDI_DESCRIPTOR_HEAP_TYPE type) {
    note_core(CORE_SLOT(pfnGetDescriptorSizeInBytes));
    UINT n = g_core_real.pfnGetDescriptorSizeInBytes(hDevice, type);
    logf("pfnGetDescriptorSizeInBytes(type=%u) -> %u", (unsigned)type, n);
    return n;
}
static D3D12DDI_CPU_DESCRIPTOR_HANDLE APIENTRY
spy_GetCPUDescriptorHandleForHeapStart(D3D12DDI_HDEVICE hDevice,
                                       D3D12DDI_HDESCRIPTORHEAP h) {
    note_core(CORE_SLOT(pfnGetCPUDescriptorHandleForHeapStart));
    D3D12DDI_CPU_DESCRIPTOR_HANDLE r =
        g_core_real.pfnGetCPUDescriptorHandleForHeapStart(hDevice, h);
    logf("pfnGetCPUDescriptorHandleForHeapStart(heap=%p) -> ptr=0x%zx (8-byte POD, RAX)",
         h.pDrvPrivate, (size_t)r.ptr);
    return r;
}
static D3D12DDI_GPU_DESCRIPTOR_HANDLE APIENTRY
spy_GetGPUDescriptorHandleForHeapStart(D3D12DDI_HDEVICE hDevice,
                                       D3D12DDI_HDESCRIPTORHEAP h) {
    note_core(CORE_SLOT(pfnGetGPUDescriptorHandleForHeapStart));
    D3D12DDI_GPU_DESCRIPTOR_HANDLE r =
        g_core_real.pfnGetGPUDescriptorHandleForHeapStart(hDevice, h);
    logf("pfnGetGPUDescriptorHandleForHeapStart(heap=%p) -> ptr=0x%llx", h.pDrvPrivate,
         (unsigned long long)r.ptr);
    return r;
}

// §9.7: heap and resource creation are fused, and the two argument pointers are
// independently nullable (committed / placed / heap-only). Log which arm each call is.
static D3D12DDI_HEAP_AND_RESOURCE_SIZES APIENTRY spy_CalcPrivateHeapAndResourceSizes(
    D3D12DDI_HDEVICE hDevice, CONST D3D12DDIARG_CREATEHEAP_0001* pHeap,
    CONST D3D12DDIARG_CREATERESOURCE_0109* pRes,
    D3D12DDI_HPROTECTEDRESOURCESESSION_0030 hSession) {
    note_core(CORE_SLOT(pfnCalcPrivateHeapAndResourceSizes));
    D3D12DDI_HEAP_AND_RESOURCE_SIZES s =
        g_core_real.pfnCalcPrivateHeapAndResourceSizes(hDevice, pHeap, pRes, hSession);
    logf("pfnCalcPrivateHeapAndResourceSizes(heap=%s res=%s) -> {Heap=%zu Resource=%zu} "
         "(16-byte struct return: hidden pointer)",
         pHeap ? "yes" : "NULL", pRes ? "yes" : "NULL", s.Heap, s.Resource);
    return s;
}
static HRESULT APIENTRY spy_CreateHeapAndResource(
    D3D12DDI_HDEVICE hDevice, CONST D3D12DDIARG_CREATEHEAP_0001* pHeap, D3D12DDI_HHEAP hHeap,
    D3D12DDI_HRTRESOURCE hRTRes, CONST D3D12DDIARG_CREATERESOURCE_0109* pRes,
    CONST D3D12DDI_CLEAR_VALUES* pClear, D3D12DDI_HPROTECTEDRESOURCESESSION_0030 hSession,
    D3D12DDI_HRESOURCE hRes) {
    note_core(CORE_SLOT(pfnCreateHeapAndResource));
    HRESULT hr = g_core_real.pfnCreateHeapAndResource(hDevice, pHeap, hHeap, hRTRes, pRes,
                                                      pClear, hSession, hRes);
    logf("pfnCreateHeapAndResource(heap=%s res=%s clear=%s) -> 0x%08lx  [%s]",
         pHeap ? "yes" : "NULL", pRes ? "yes" : "NULL", pClear ? "yes" : "NULL",
         (unsigned long)hr,
         (pHeap && pRes) ? "COMMITTED" : (pRes ? "PLACED" : "HEAP-ONLY"));
    return hr;
}

static HRESULT APIENTRY spy_CreateRootSignature(D3D12DDI_HDEVICE hDevice,
                                                CONST D3D12DDIARG_CREATE_ROOT_SIGNATURE_0100* a,
                                                D3D12DDI_HROOTSIGNATURE h) {
    note_core(CORE_SLOT(pfnCreateRootSignature));
    HRESULT hr = g_core_real.pfnCreateRootSignature(hDevice, a, h);
    logf("pfnCreateRootSignature Version=%u NodeMask=0x%x pRootSignature=%p -> 0x%08lx "
         "(H3: arrives PARSED, not as an RTS0 blob)",
         a ? (unsigned)a->Version : 0, a ? a->NodeMask : 0,
         a ? (const void*)a->pRootSignature_1_2 : nullptr, (unsigned long)hr);
    return hr;
}

// The frame boundary. Also the dump trigger, so a sample that never exits still yields its
// first-frame trace.
static VOID APIENTRY spy_Present(D3D12DDI_HCOMMANDLIST hCL, D3D12DDI_HCOMMANDQUEUE hQ,
                                 CONST D3D12DDIARG_PRESENT_0001* a,
                                 D3D12DDI_PRESENT_0051* pOut,
                                 D3D12DDI_PRESENT_CONTEXTS_0051* pCtx,
                                 D3D12DDI_PRESENT_HWQUEUES_0051* pHwQ) {
    note_cl(CL_SLOT(pfnPresent));
    LONG frame = InterlockedIncrement(&g_present_count);
    logf("=== FRAME %ld: pfnPresent hCL=%p hQueue=%p surfaces=%u dst=%p flags=0x%x "
         "flipInterval=%u vidpn=%u dirty=%u privSize=%u priv=%p optimizeForComposition=%d "
         "outCtx=%p outHwQ=%p",
         frame, hCL.pDrvPrivate, hQ.pDrvPrivate, a ? a->SurfacesToPresent : 0,
         a ? a->hDstResource.pDrvPrivate : nullptr, a ? (unsigned)a->Flags.Value : 0,
         a ? (unsigned)a->FlipInterval : 0, a ? a->VidPnSourceID : 0, a ? a->DirtyRects : 0,
         a ? a->PrivateDriverDataSize : 0, a ? a->pPrivateDriverData : nullptr,
         a ? (int)a->OptimizeForComposition : 0, (void*)pCtx, (void*)pHwQ);
    g_cl_real.pfnPresent(hCL, hQ, a, pOut, pCtx, pHwQ);
    if (pOut) log_hex("  pfnPresent OUT (D3D12DDI_PRESENT_0051)", pOut, sizeof(*pOut));
    if (frame == g_dump_after_presents) spy_dump("present-threshold");
}

// ---------------------------------------------------------------------------------------
// Corelayer callbacks — the runtime->driver direction. Wrapping them is how §15 #9
// (how a driver obtains a second table for pfnSetCommandListDDITableCb) and #7 (where
// recording memory comes from) get observed rather than inferred.
// ---------------------------------------------------------------------------------------
static VOID APIENTRY spy_cb_SetError(D3D12DDI_HRTDEVICE hRT, HRESULT hr) {
    logf("CB pfnSetErrorCb(hRTDevice=%p, 0x%08lx)  <-- WARP is reporting a device error",
         hRT.handle, (unsigned long)hr);
    g_corelayer_real.pfnSetErrorCb(hRT, hr);
}
static VOID APIENTRY spy_cb_SetCommandListError(D3D12DDI_HRTCOMMANDLIST hRT, HRESULT hr) {
    logf("CB pfnSetCommandListErrorCb(hRTCommandList=%p, 0x%08lx)", hRT.handle,
         (unsigned long)hr);
    g_corelayer_real.pfnSetCommandListErrorCb(hRT, hr);
}
static VOID APIENTRY spy_cb_SetCommandListDDITable(D3D12DDI_HRTCOMMANDLIST hRT,
                                                   D3D12DDI_HRTTABLE hTable) {
    logf("CB pfnSetCommandListDDITableCb(hRTCommandList=%p, hRTTable=%p)  <-- #9", hRT.handle,
         hTable.handle);
    g_corelayer_real.pfnSetCommandListDDITableCb(hRT, hTable);
}

// ---------------------------------------------------------------------------------------
// Table installation.
// ---------------------------------------------------------------------------------------
static void install_generic(void** table, size_t slots_in_buffer, void* const* thunks,
                            void** snapshot, const char* const* names, const char* tag,
                            size_t known) {
    size_t n = slots_in_buffer < known ? slots_in_buffer : known;
    size_t nulls = 0;
    for (size_t i = 0; i < n; ++i) {
        snapshot[i] = table[i];
        // ⛔ Preserve NULLs. The runtime may test a slot for NULL to detect an unsupported
        // feature; replacing one with a thunk would answer "supported" on WARP's behalf.
        // The NULL set is also the answer to §15 #2 (which slots may legally be NULL).
        if (table[i]) {
            table[i] = thunks[i];
        } else {
            ++nulls;
            if (names) logf("  %s[%zu] %s = NULL in WARP's fill", tag, i, names[i]);
        }
    }
    logf("  %s: installed %zu thunks, %zu NULL slots left alone", tag, n - nulls, nulls);
}

static void install_typed(void** table, size_t slots_in_buffer, size_t slot, void* fn,
                          const char* name) {
    if (slot >= slots_in_buffer) {
        logf("  typed hook %s SKIPPED: slot %zu beyond the runtime's buffer (%zu slots)",
             name, slot, slots_in_buffer);
        return;
    }
    if (!table[slot]) {
        logf("  typed hook %s SKIPPED: WARP left the slot NULL", name);
        return;
    }
    table[slot] = fn;
}

static HRESULT APIENTRY spy_FillDDITable(D3D12DDI_HADAPTER hAdapter,
                                         D3D12DDI_TABLE_TYPE tableType, VOID* pTable,
                                         SIZE_T tableSize, UINT fifth,
                                         D3D12DDI_HRTTABLE hRTTable) {
    note_adapter(6);
    logf("pfnFillDDITable TableType=%u (%s) TableSize=%zu (=%zu slots) fifthUINT=%u "
         "hRTTable=%p pTable=%p",
         (unsigned)tableType, table_name((UINT32)tableType), tableSize,
         tableSize / sizeof(void*), fifth, hRTTable.handle, pTable);

    HRESULT hr = g_adapter_real.pfnFillDDITable(hAdapter, tableType, pTable, tableSize,
                                               fifth, hRTTable);
    logf("pfnFillDDITable -> 0x%08lx", (unsigned long)hr);
    if (FAILED(hr) || !pTable) return hr;

    void** t = (void**)pTable;
    size_t slots = tableSize / sizeof(void*);

    switch (tableType) {
        case D3D12DDI_TABLE_TYPE_DEVICE_CORE: {
            if (tableSize != sizeof(D3D12DDI_DEVICE_FUNCS_CORE_0109))
                SPY_REFUSE(table_truncated,
                           "DEVICE_CORE TableSize=%zu, sizeof(_0109)=%zu -> the runtime "
                           "negotiated a different core-table version",
                           tableSize, sizeof(D3D12DDI_DEVICE_FUNCS_CORE_0109));
            if (g_spy_core_snapshot[0]) SPY_REFUSE(table_refilled, "DEVICE_CORE refilled");
            install_generic(t, slots, kCoreThunks, g_spy_core_snapshot, kCoreNames, "core",
                            SPY_CORE_SLOTS);
            memset(&g_core_real, 0, sizeof(g_core_real));
            memcpy(&g_core_real, g_spy_core_snapshot,
                   (slots < SPY_CORE_SLOTS ? slots : SPY_CORE_SLOTS) * sizeof(void*));
#define TYPED_CORE(member, fn) \
    install_typed(t, slots, CORE_SLOT(member), (void*)&fn, #member)
            TYPED_CORE(pfnCalcPrivateShaderSize, spy_CalcPrivateShaderSize);
            TYPED_CORE(pfnCreateVertexShader, spy_CreateVertexShader);
            TYPED_CORE(pfnCreatePixelShader, spy_CreatePixelShader);
            TYPED_CORE(pfnCreateGeometryShader, spy_CreateGeometryShader);
            TYPED_CORE(pfnCreateComputeShader, spy_CreateComputeShader);
            TYPED_CORE(pfnCreateHullShader, spy_CreateHullShader);
            TYPED_CORE(pfnCreateDomainShader, spy_CreateDomainShader);
            TYPED_CORE(pfnCreateAmplificationShader, spy_CreateAmplificationShader);
            TYPED_CORE(pfnCreateMeshShader, spy_CreateMeshShader);
            TYPED_CORE(pfnGetDescriptorSizeInBytes, spy_GetDescriptorSizeInBytes);
            TYPED_CORE(pfnGetCPUDescriptorHandleForHeapStart,
                       spy_GetCPUDescriptorHandleForHeapStart);
            TYPED_CORE(pfnGetGPUDescriptorHandleForHeapStart,
                       spy_GetGPUDescriptorHandleForHeapStart);
            TYPED_CORE(pfnCalcPrivateHeapAndResourceSizes,
                       spy_CalcPrivateHeapAndResourceSizes);
            TYPED_CORE(pfnCreateHeapAndResource, spy_CreateHeapAndResource);
            TYPED_CORE(pfnCreateRootSignature, spy_CreateRootSignature);
#undef TYPED_CORE
            break;
        }
        case D3D12DDI_TABLE_TYPE_COMMAND_LIST_3D: {
            if (tableSize != sizeof(D3D12DDI_COMMAND_LIST_FUNCS_3D_0108))
                SPY_REFUSE(table_truncated,
                           "COMMAND_LIST_3D TableSize=%zu, sizeof(_0108)=%zu", tableSize,
                           sizeof(D3D12DDI_COMMAND_LIST_FUNCS_3D_0108));
            if (g_spy_cl_snapshot[0])
                SPY_REFUSE(table_refilled,
                           "COMMAND_LIST_3D refilled (hRTTable=%p) - a second command-list "
                           "table instance, the multiplicity of #3/#9",
                           hRTTable.handle);
            install_generic(t, slots, kClThunks, g_spy_cl_snapshot, kClNames, "cl",
                            SPY_CL_SLOTS);
            memset(&g_cl_real, 0, sizeof(g_cl_real));
            memcpy(&g_cl_real, g_spy_cl_snapshot,
                   (slots < SPY_CL_SLOTS ? slots : SPY_CL_SLOTS) * sizeof(void*));
            install_typed(t, slots, CL_SLOT(pfnPresent), (void*)&spy_Present, "pfnPresent");
            break;
        }
        case D3D12DDI_TABLE_TYPE_COMMAND_QUEUE_3D:
            if (tableSize != sizeof(D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001))
                SPY_REFUSE(table_truncated, "COMMAND_QUEUE_3D TableSize=%zu, sizeof=%zu",
                           tableSize, sizeof(D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001));
            install_generic(t, slots, kQueueThunks, g_spy_queue_snapshot, kQueueNames,
                            "queue", SPY_QUEUE_SLOTS);
            break;
        case D3D12DDI_TABLE_TYPE_DXGI:
            // §2.3: the struct is not in d3d12umddi.h. 168 bytes = DXGI1_4 (21 members),
            // 176 = DXGI1_5 or DXGI1_6_1 (22). The observed TableSize IS the answer to #4.
            logf("  DXGI table: %zu bytes = %zu slots -> %s", tableSize, slots,
                 tableSize == 168   ? "DXGI1_4_DDI_BASE_FUNCTIONS (21 members)"
                 : tableSize == 176 ? "DXGI1_5 or DXGI1_6_1 (22 members)"
                                    : "NOT one of the seven dxgiddi.h candidates");
            install_generic(t, slots, kDxgiThunks, g_spy_dxgi_snapshot, nullptr, "dxgi",
                            SPY_DXGI_SLOTS);
            break;
        default:
            SPY_REFUSE(unknown_table,
                       "table type %u (%s) is not one of the four a baseline device needs; "
                       "left unhooked",
                       (unsigned)tableType, table_name((UINT32)tableType));
            break;
    }
    return hr;
}

static SIZE_T APIENTRY spy_CalcPrivateDeviceSize(D3D12DDI_HADAPTER hAdapter,
                                                 CONST D3D12DDIARG_CALCPRIVATEDEVICESIZE* a) {
    note_adapter(0);
    logf("pfnCalcPrivateDeviceSize Interface=0x%08x Version=0x%08x Flags=0x%x", a->Interface,
         a->Version, (unsigned)a->Flags);
    SIZE_T n = g_adapter_real.pfnCalcPrivateDeviceSize(hAdapter, a);
    logf("pfnCalcPrivateDeviceSize -> %zu", n);
    return n;
}

static HRESULT APIENTRY spy_CreateDevice(D3D12DDI_HADAPTER hAdapter,
                                         CONST D3D12DDIARG_CREATEDEVICE_0109* a) {
    note_adapter(1);
    // §1.5's load-bearing inference: that Interface is the high 32 bits of the negotiated
    // D3D12DDI_SUPPORTED_* token and Version the low 32. Check it against the list WARP
    // actually returned instead of assuming it.
    UINT64 pair = ((UINT64)a->Interface << 32) | (UINT64)a->Version;
    const char* match = "NO MATCH in pfnGetSupportedVersions' list";
    for (UINT32 i = 0; i < g_version_count; ++i)
        if (g_versions[i] == pair) match = "MATCHES a token WARP reported";
    g_negotiated = pair;
    g_negotiated_is_0109 = (pair == kSupported0109);
    logf("pfnCreateDevice hRTDevice=%p Interface=0x%08x Version=0x%08x -> pair 0x%016llx "
         "%s%s%s",
         a->hRTDevice.handle, a->Interface, a->Version, (unsigned long long)pair, match,
         (pair == kSupported0109) ? " == D3D12DDI_SUPPORTED_0109" : "",
         (pair == kSupported0110) ? " == D3D12DDI_SUPPORTED_0110" : "");
    logf("  Flags=0x%x hDrvDevice=%p pKTCallbacks=%p p12UMCallbacks=%p NumReserveRanges=%u "
         "pReserveRanges=%p",
         (unsigned)a->Flags, a->hDrvDevice.pDrvPrivate, (const void*)a->pKTCallbacks,
         (const void*)a->p12UMCallbacks, a->NumReserveRanges, (const void*)a->pReserveRanges);
    for (UINT i = 0; i < a->NumReserveRanges && i < 16; ++i)
        logf("    reserveRange[%u] start=0x%016llx size=%llu", i,
             (unsigned long long)a->pReserveRanges[i].StartAddress,
             (unsigned long long)a->pReserveRanges[i].SizeInBytes);

    // Wrap the corelayer callbacks. ⚠ The union arm is chosen by an exhaustive match on the
    // negotiated token, never by an `else` that assumes the largest arm — that is exactly
    // the adapter.rs:36-45 landmine (a 376..392-byte out-of-bounds write into the runtime's
    // heap). If the token is not one this build knows, pass the arg through untouched.
    D3D12DDIARG_CREATEDEVICE_0109 mine = *a;
    static D3D12DDI_CORELAYER_DEVICECALLBACKS_0062 wrapped;
    if (a->p12UMCallbacks_0062 && (pair == kSupported0109 || pair == kSupported0110)) {
        if (!g_corelayer_captured) {
            g_corelayer_real = *a->p12UMCallbacks_0062;
            g_corelayer_captured = true;
            const void* const* p = (const void* const*)&g_corelayer_real;
            for (int i = 0; i < 18; ++i)
                logf("  corelayer[%d] = %p", i, p[i]);
        }
        wrapped = g_corelayer_real;
        if (g_corelayer_real.pfnSetErrorCb) wrapped.pfnSetErrorCb = spy_cb_SetError;
        if (g_corelayer_real.pfnSetCommandListErrorCb)
            wrapped.pfnSetCommandListErrorCb = spy_cb_SetCommandListError;
        if (g_corelayer_real.pfnSetCommandListDDITableCb)
            wrapped.pfnSetCommandListDDITableCb = spy_cb_SetCommandListDDITable;
        mine.p12UMCallbacks_0062 = &wrapped;
    } else if (a->p12UMCallbacks) {
        SPY_REFUSE(corelayer_arm_unknown,
                   "negotiated token 0x%016llx is not one this build decodes; passing "
                   "p12UMCallbacks through unwrapped",
                   (unsigned long long)pair);
    }

    HRESULT hr = g_adapter_real.pfnCreateDevice(hAdapter, &mine);
    logf("pfnCreateDevice -> 0x%08lx", (unsigned long)hr);
    return hr;
}

static VOID APIENTRY spy_DestroyDevice(D3D12DDI_HDEVICE hDevice) {
    note_adapter(7);
    logf("pfnDestroyDevice(%p)  <-- on the ADAPTER table, not the device table",
         hDevice.pDrvPrivate);
    spy_dump("pfnDestroyDevice");
    g_adapter_real.pfnDestroyDevice(hDevice);
}

static HRESULT APIENTRY spy_CloseAdapter(D3D12DDI_HADAPTER hAdapter) {
    note_adapter(2);
    logf("pfnCloseAdapter(%p)", hAdapter.pDrvPrivate);
    spy_dump("pfnCloseAdapter");
    return g_adapter_real.pfnCloseAdapter(hAdapter);
}

// ---------------------------------------------------------------------------------------
// The dump: per-slot hit counts and the ordered call trace.
// ---------------------------------------------------------------------------------------
static void dump_hits(const char* tag, const SpyTable& t) {
    UINT32 touched = 0;
    for (UINT32 i = 0; i < t.count; ++i)
        if (t.hits[i]) ++touched;
    logf("HITS %s: %u of %u slots called", tag, touched, t.count);
    for (UINT32 i = 0; i < t.count; ++i)
        if (t.hits[i])
            logf("  %s[%3u] %-58s %ld", tag, i, t.names ? t.names[i] : "(dxgi slot)",
                 t.hits[i]);
}

static void spy_dump(const char* why) {
    if (InterlockedExchange(&g_dumped, 1)) {
        logf("(dump already taken; %s ignored)", why);
        return;
    }
    logf("======== DUMP (%s) ========", why);
    logf("mutate arm = %s; frames presented = %ld", g_mutate_name, g_present_count);
    logf("refusals: warp_load_failed=%ld warp_wrong_path=%ld warp_no_export=%ld "
         "unknown_version=%ld table_truncated=%ld table_refilled=%ld unknown_table=%ld "
         "corelayer_arm_unknown=%ld",
         g_refuse.warp_load_failed, g_refuse.warp_wrong_path, g_refuse.warp_no_export,
         g_refuse.unknown_version, g_refuse.table_truncated, g_refuse.table_refilled,
         g_refuse.unknown_table, g_refuse.corelayer_arm_unknown);
    for (int i = 0; i < 3; ++i) dump_hits(kTableTag[i], kTables[i]);
    dump_hits(kTableTag[3], kTables[3]);

    LONG total = g_spy_trace_idx;
    logf("TRACE: %ld events (%s)", total,
         (UINT32)total > kTraceCap ? "ring saturated; the first 1Mi are kept" : "complete");
    UINT32 n = (UINT32)total < kTraceCap ? (UINT32)total : kTraceCap;
    if (g_log) {
        EnterCriticalSection(&g_log_lock);
        for (UINT32 i = 0; i < n; ++i) {
            UINT32 e = g_spy_trace[i];
            UINT32 tag = e >> 24, idx = e & 0x00FFFFFF;
            if (tag == kTagMark)
                fprintf(g_log, "TRACE %6u  -> log line #%u\n", i, idx);
            else if (tag == 3 && (e & 0x00800000u))
                fprintf(g_log, "TRACE %6u  adapter[%u] %s\n", i, idx & 0xFF,
                        (idx & 0xFF) < SPY_ADAPTER_SLOTS ? kAdapterNames[idx & 0xFF] : "?");
            else if (tag < 4 && idx < kTables[tag].count)
                fprintf(g_log, "TRACE %6u  %s[%u] %s\n", i, kTableTag[tag], idx,
                        kTables[tag].names ? kTables[tag].names[idx] : "(dxgi slot)");
            else
                fprintf(g_log, "TRACE %6u  <malformed 0x%08x>\n", i, e);
        }
        fflush(g_log);
        LeaveCriticalSection(&g_log_lock);
    }
    logf("======== END DUMP ========");
}

// ---------------------------------------------------------------------------------------
// Init and the exports.
// ---------------------------------------------------------------------------------------
static void spy_init_once() {
    static volatile LONG done;
    if (InterlockedExchange(&done, 1)) return;
    InitializeCriticalSection(&g_log_lock);
    QueryPerformanceFrequency(&g_qpc_freq);
    QueryPerformanceCounter(&g_qpc_start);

    char path[MAX_PATH];
    DWORD n = GetEnvironmentVariableA("HELIOS_D12SPY_LOG", path, MAX_PATH);
    char full[MAX_PATH + 64];
    if (n && n < MAX_PATH) {
        sprintf(full, "%s.%lu.log", path, GetCurrentProcessId());
    } else {
        // ⚠ C:\ProgramData\Helios already exists and is writable from both sessions (the
        // DXVK logs live there). It contains a junction loop — never -Recurse it.
        CreateDirectoryA("C:\\ProgramData\\Helios", nullptr);
        sprintf(full, "C:\\ProgramData\\Helios\\d3d12_spy-%lu.log", GetCurrentProcessId());
    }
    g_log = fopen(full, "a");

    char mut[64];
    if (GetEnvironmentVariableA("HELIOS_D12SPY_MUTATE", mut, sizeof(mut))) {
        if (!_stricmp(mut, "range")) { g_mutate = kMutRange; g_mutate_name = "range"; }
        else if (!_stricmp(mut, "cross")) { g_mutate = kMutCross; g_mutate_name = "cross"; }
        else if (!_stricmp(mut, "sm65")) { g_mutate = kMutSm65; g_mutate_name = "sm65"; }
        else if (!_stricmp(mut, "capfail")) { g_mutate = kMutCapFail; g_mutate_name = "capfail"; }
        else if (!_stricmp(mut, "tier")) { g_mutate = kMutTier; g_mutate_name = "tier"; }
        else if (!_stricmp(mut, "forcever")) { g_mutate = kMutForceVer; g_mutate_name = "forcever"; }
        else g_mutate_name = "UNKNOWN (ignored)";
    }
    char dumpn[32];
    if (GetEnvironmentVariableA("HELIOS_D12SPY_DUMP_AFTER_PRESENTS", dumpn, sizeof(dumpn)))
        g_dump_after_presents = atol(dumpn);

    char exe[MAX_PATH] = {0};
    GetModuleFileNameA(nullptr, exe, MAX_PATH);
    logf("==== d3d12_warp_spy (D12-G5) pid=%lu session=? exe=%s", GetCurrentProcessId(),
         exe);
    logf("log=%s mutate=%s dumpAfterPresents=%ld", full, g_mutate_name,
         g_dump_after_presents);
    logf("sizeof: ADAPTERFUNCS_0109=%zu CORE_0109=%zu CL_0108=%zu QUEUE_0001=%zu "
         "CORELAYER_0062=%zu",
         sizeof(D3D12DDI_ADAPTERFUNCS_0109), sizeof(D3D12DDI_DEVICE_FUNCS_CORE_0109),
         sizeof(D3D12DDI_COMMAND_LIST_FUNCS_3D_0108),
         sizeof(D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001),
         sizeof(D3D12DDI_CORELAYER_DEVICECALLBACKS_0062));
    logf("D3D12DDI_SUPPORTED_0109=0x%016llx _0110=0x%016llx",
         (unsigned long long)kSupported0109, (unsigned long long)kSupported0110);
}

// The Route B gate. Route B points the Helios adapter's UserModeDriverName[3] at this DLL,
// and ⚠ dwm.exe calls OpenAdapter12 on that adapter in production (DECISIONS.md §7.13).
// Two conditions, both required, both defaulting to REFUSE:
//   1. HKLM\SOFTWARE\Helios!UmdD3D12Spy == 1 — the same shape as D11's UmdD3D12 kill
//      switch, read once per process so a running dwm keeps its behaviour.
//   2. the process is the named workload — so even with the knob on, dwm's OpenAdapter12
//      gets DXGI_ERROR_UNSUPPORTED, i.e. BIT-IDENTICAL to what helios_umd.dll returns
//      today (umd/src/adapter.rs:177-189). The experiment cannot change the compositor.
// ⛔ DXGI_ERROR_UNSUPPORTED (0x887A0004), never DXGI_ERROR_DRIVER_INTERNAL_ERROR — the
// latter is recorded by the runtime and ETW as a driver fault (DECISIONS.md §7.5).
static bool spy_gate_open() {
    DWORD on = 0, sz = sizeof(on), type = 0;
    HKEY k;
    if (RegOpenKeyExA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", 0, KEY_QUERY_VALUE, &k)
        == ERROR_SUCCESS) {
        RegQueryValueExA(k, "UmdD3D12Spy", nullptr, &type, (LPBYTE)&on, &sz);
        RegCloseKey(k);
    }
    if (on != 1) {
        SPY_REFUSE(gate_knob_off, "HKLM\\SOFTWARE\\Helios!UmdD3D12Spy = %lu, not 1", on);
        return false;
    }
    char exe[MAX_PATH] = {0};
    GetModuleFileNameA(nullptr, exe, MAX_PATH);
    const char* base = strrchr(exe, '\\');
    base = base ? base + 1 : exe;
    char want[64] = "spy_workload.exe";
    GetEnvironmentVariableA("HELIOS_D12SPY_PROC", want, sizeof(want));
    if (_stricmp(base, want) != 0) {
        SPY_REFUSE(gate_wrong_process, "process is %s, gate allows only %s", base, want);
        return false;
    }
    return true;
}

extern "C" HRESULT APIENTRY OpenAdapter12(D3D12DDIARG_OPENADAPTER* pArgs) {
    spy_init_once();
    if (!spy_gate_open()) return DXGI_ERROR_UNSUPPORTED;
    logf("OpenAdapter12 ENTER hRTAdapter=%p pAdapterCallbacks=%p pAdapterFuncs=%p",
         pArgs ? pArgs->hRTAdapter.handle : nullptr,
         pArgs ? (const void*)pArgs->pAdapterCallbacks : nullptr,
         pArgs ? (void*)pArgs->pAdapterFuncs : nullptr);
    if (!pArgs) return E_INVALIDARG;

    if (!g_warp && !spy_load_warp())
        return DXGI_ERROR_UNSUPPORTED;  // never 0x887A0020: §14.3(5)

    typedef HRESULT(APIENTRY * PFN_OPEN12)(D3D12DDIARG_OPENADAPTER*);
    PFN_OPEN12 real = (PFN_OPEN12)GetProcAddress(g_warp, "OpenAdapter12");
    if (!real) {
        SPY_REFUSE(warp_no_export, "d3d10warp_real.dll has no OpenAdapter12");
        return DXGI_ERROR_UNSUPPORTED;
    }

    HRESULT hr = real(pArgs);
    logf("WARP OpenAdapter12 -> 0x%08lx, hAdapter=%p, pAdapterFuncs=%p",
         (unsigned long)hr, pArgs->hAdapter.pDrvPrivate, (void*)pArgs->pAdapterFuncs);
    if (FAILED(hr) || !pArgs->pAdapterFuncs) return hr;

    // The runtime's buffer is D3D12DDI_ADAPTERFUNCS_0109-shaped: 8 pointers, and the only
    // difference from the base form is pfnCreateDevice's argument type (§1.3).
    D3D12DDI_ADAPTERFUNCS_0109* f = (D3D12DDI_ADAPTERFUNCS_0109*)pArgs->pAdapterFuncs;
    void* const* p = (void* const*)f;
    for (int i = 0; i < SPY_ADAPTER_SLOTS; ++i)
        logf("  adapter[%d] %-28s = %p%s", i, kAdapterNames[i], p[i],
             p[i] ? "" : "   <-- NULL");
    g_adapter_real = *f;

    if (f->pfnCalcPrivateDeviceSize) f->pfnCalcPrivateDeviceSize = spy_CalcPrivateDeviceSize;
    if (f->pfnCreateDevice) f->pfnCreateDevice = spy_CreateDevice;
    if (f->pfnCloseAdapter) f->pfnCloseAdapter = spy_CloseAdapter;
    if (f->pfnGetSupportedVersions) f->pfnGetSupportedVersions = spy_GetSupportedVersions;
    if (f->pfnGetCaps) f->pfnGetCaps = spy_GetCaps;
    if (f->pfnGetOptionalDDITables) f->pfnGetOptionalDDITables = spy_GetOptionalDDITables;
    if (f->pfnFillDDITable) f->pfnFillDDITable = spy_FillDDITable;
    if (f->pfnDestroyDevice) f->pfnDestroyDevice = spy_DestroyDevice;
    logf("adapter table hooked");
    return hr;
}

// Pass-throughs so an app-local copy of this DLL does not break D3D10/11 on WARP. Both take
// a single pointer argument, so a void* forwarder is ABI-identical without dragging in the
// D3D10 argument structs.
typedef HRESULT(APIENTRY* PFN_OPEN_PASSTHROUGH)(void*);
static HRESULT passthrough(const char* name, void* pArgs) {
    spy_init_once();
    if (!g_warp && !spy_load_warp()) return DXGI_ERROR_UNSUPPORTED;
    PFN_OPEN_PASSTHROUGH real = (PFN_OPEN_PASSTHROUGH)GetProcAddress(g_warp, name);
    if (!real) {
        SPY_REFUSE(warp_no_export, "d3d10warp_real.dll has no %s", name);
        return DXGI_ERROR_UNSUPPORTED;
    }
    HRESULT hr = real(pArgs);
    logf("%s (pass-through) -> 0x%08lx", name, (unsigned long)hr);
    return hr;
}
extern "C" HRESULT APIENTRY OpenAdapter(void* pArgs) {
    return passthrough("OpenAdapter", pArgs);
}
extern "C" HRESULT APIENTRY OpenAdapter10_2(void* pArgs) {
    return passthrough("OpenAdapter10_2", pArgs);
}

BOOL APIENTRY DllMain(HMODULE, DWORD reason, LPVOID reserved) {
    // Nothing but the barest bookkeeping here: DllMain runs under the loader lock.
    if (reason == DLL_PROCESS_DETACH && reserved == nullptr && g_log) {
        // Orderly FreeLibrary (not process teardown): the log is still safe to touch.
        spy_dump("DLL_PROCESS_DETACH");
    }
    return TRUE;
}
