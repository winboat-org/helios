// tools/icd_anchor_probe.cpp — stage S4b, the process-global venus-ICD anchor gate.
//
// Loads BOTH Helios user-mode drivers into ONE process and measures the single
// property the anchor exists to guarantee: **one venus ICD module per process,
// and both drivers agree on which one it is.**
//
//   1. D3DKMT finds the Helios adapter LUID (never index 0).
//   2. `D3D11CreateDevice` on that adapter loads the **deployed**
//      `helios_umd.dll` and makes DXVK build a `VkInstance` on the venus ICD
//      (`umd/bridge/dxvk_bridge.cpp:1675` reads the ctx id right after, which is
//      what forces `resolve_helios_icd_module` and therefore the anchor step).
//   3. `LoadLibraryW(helios_umd12.dll)` + `helios_umd12_probe_create_device_v1`
//      makes vkd3d build its OWN `VkInstance` through the cxx bridge
//      (`umd12/bridge/vkd3d_bridge.cpp:169-180` runs the same anchor step).
//   4. THE ASSERTION: the process ended up with exactly one venus ICD module and
//      the anchor published exactly that module. See "WHAT THE ANCHOR QUERY CAN
//      SHOW" below — the shape of this check is not the obvious one, and the
//      reason is mechanical.
//   5. Both venus context ids are **reported, never asserted equal**. See "A
//      CORRECTION TO ARCHITECTURE.md section 6.4" below.
//   6. `-reverse` runs step 3 before step 2, so a pair of runs proves the anchor
//      is order-independent: whichever UMD loads first publishes, the other
//      reconciles against it.
//
// ⛔ Nothing is deployed and nothing is drawn. `OpenAdapter12` still refuses;
// this probe reaches `helios_umd12.dll` only through the three
// `helios_umd12_probe_*_v1` evidence exports (`umd12/src/probe12.rs`), which no
// runtime, loader or ICD resolves by name.
//
// ── WHY THIS PROBE LINKS DXGI AND d3d12_bridge_probe.cpp MUST NOT ────────────
// `umd/build.rs:252-256` states the rule: "a WDDM UMD sits below DXGI and
// implements the DXGI DDI; it must not depend on dxgi.dll." That is a rule about
// the DRIVER. Here the D3D11 device is the *vehicle that loads the driver*, and
// `IDXGIFactory1::EnumAdapters1` + `DXGI_ADAPTER_DESC1::AdapterLuid` is the only
// supported way to hand `D3D11CreateDevice` a specific adapter. The LUID itself
// still comes from D3DKMT, as in `tools/d3d12_bridge_probe.cpp:318`, because
// that is the identification that survives an INF description change.
//
// ── A CORRECTION TO ARCHITECTURE.md section 6.4 (lines 690-693) ──────────────
// §6.4 states the S4b pass criterion as: "both modules must report the same ICD
// path, **both venus context ids must be non-zero and equal**, and
// `IcdAnchorMismatch` must read 0." The middle clause is wrong, and asserting it
// would assert something the ICD cannot do. Evidence, all in-tree:
//
//   * A venus context is minted per **renderer**, i.e. per `VkInstance`:
//     `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:4201-4202` sets both ctx
//     id variables from `helios->ctx_id` immediately after a successful
//     `CTX_CREATE(VENUS)`. Two engines in one process build two instances.
//   * ⭐ MEASURED, already in the tree: `tmp/dx12/gates/G2/logs/test_create_device.log`
//     is ONE process (every vkd3d line is prefixed `2944:`) and shows
//     `vkd3d_instance_init` repeated with
//     `CTX_CREATE(VENUS) ... ctx_id=618, 620, 622, 624, 626, 628, 630, 632, ...`
//     — a new context per instance, monotonically increasing, same pid.
//   * The ICD's own audit comment says so in as many words
//     (`vn_renderer_helios.c:528-538`): `helios_current_ctx_id` is
//     "last-writer-wins across instances ... Ambiguous with two instances".
//
// So the property that actually matters, and the one the anchor provides, is
// that both engines resolve the **same ICD MODULE**. This probe therefore
// asserts the module and *reports* the two context ids, printing what it
// observed so the run itself settles the question. ⛔ Do not "fix" this by
// adding an equality assertion. ⚠ `umd12/src/probe12.rs:138-145` restates §6.4's
// "non-zero and equal" in its doc comment and is wrong for the same reason; this
// probe does not edit it (S4b lane boundary) but the integrator should.
//
// The reported pair is itself evidence. `helios_venus_current_ctx_id` is the
// process-global last-writer-wins value, so:
//   * normal order  (D3D11 then D3D12): it should equal umd12's recorded id;
//   * `-reverse`    (D3D12 then D3D11): it should equal the *D3D11* instance's
//     and therefore DIFFER from umd12's recorded id.
// A pair of runs that shows exactly that has demonstrated two live contexts on
// one ICD module, which is the corrected criterion.
//
// ── HOW THE TWO MODULE HANDLES ARE FOUND, AND WHY THAT WAY ───────────────────
// By replicating the DLLs' own `K32EnumProcessModules` first-hit-wins walk
// (`umd_common/bridge/bridge_icd_anchor.cpp:63-80`), which is the same walk the
// Mesa ICD uses for its exports (`icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:711-732`).
// Rejected alternatives:
//   * `GetModuleHandleW(L"helios_umd.dll")` — ⛔ wrong under ProgramData
//     hotplug, where the loaded module is
//     `C:\ProgramData\HeliosUmd\helios_umd_<16 hex of sha256>.dll`
//     (`tools/hotplug-helios-umd.ps1:75`) and there is no stable name to ask for.
//   * `GetModuleHandleExW(FROM_ADDRESS, <resolved anchor proc>)` — correct but
//     insufficient: it names the module of one proc already in hand, and cannot
//     say how many modules export the name, nor which one is FIRST in the walk
//     order the mechanism actually depends on. Both are exactly what must be
//     measured here.
//
// ── ⚠ WHAT THE ANCHOR QUERY CAN SHOW, AND WHAT IT CANNOT ─────────────────────
// `bridge_icd_anchor.cpp` compiles into BOTH cdylibs (`:4-9`), so each DLL
// carries its own `g_anchor` static (`:43`). Every resolver, in either DLL,
// first runs `find_process_anchor()` (`:63-80`) and calls the copy in the FIRST
// module that exports the name — so exactly ONE copy ever receives a candidate
// and publishes, and the other module's copy stays null for the life of the
// process. That is the mechanism working as designed (`bridge_icd_anchor.h:55-57`:
// "The copy that wins is the one in the module the loader enumerated first;
// that copy's static is the single source of truth, and the other copy is never
// called").
//
// ⛔ Consequently "call both copies with NULL and require both return the same
// non-null pointer" cannot hold: the non-publisher answers null **by
// construction, not by defect**. What is asserted instead — the same invariant
// with the false part removed:
//   (a) the FIRST anchor-exporting module in walk order, the copy every resolver
//       in this process reaches, answers NON-NULL;
//   (b) every other copy answers null, or the SAME pointer. A different non-null
//       answer means two live publishers and is a hard failure;
//   (c) that pointer is a module exporting `helios_venus_memory_alloc_info`
//       (`bridge_icd_anchor.h:73` — that export IS the definition of "is the
//       venus ICD"); and
//   (d) independently of the anchor entirely: exactly ONE loaded module in this
//       process exports that symbol.
// (d) is the raw invariant and is enumeration-order-independent — a count, not a
// first-hit — so it is the one that cannot be argued with; (a)-(c) prove the
// anchor plumbing agrees with it. Every query below passes NULL: a pure query
// publishes nothing (`bridge_icd_anchor.cpp:85-92`), so the probe cannot mask a
// mismatch by publishing on a driver's behalf.
//
// ── IcdAnchorMismatch ────────────────────────────────────────────────────────
// The counter lives in each DLL's copy of the anchor
// (`bridge_icd_anchor.cpp:46`) and is not exported, so a probe cannot read it.
// It is only ever *logged*, on the mismatch path
// (`bridge_icd_anchor.cpp:226-231`), into `C:\ProgramData\Helios\umd-<pid>.log`
// and `umd12-<pid>.log`. The probe's contribution is therefore the pid, printed
// below as `probe pid = <n>`; the RUNNER (`tmp/dx12/build-s4b-anchor.ps1`)
// asserts the string `IcdAnchorMismatch` is ABSENT from this run's block of both
// logs, and that both logs carry the same
// `selected coherent loaded ICD module -> <path>` line.
//
// Build + run (both orders): tmp/dx12/build-s4b-anchor.ps1
//   clang-cl /MD /EHsc /std:c++17 icd_anchor_probe.cpp /link d3d11.lib dxgi.lib gdi32.lib
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <psapi.h>
#include <d3d11.h>
#include <dxgi.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>   // _stricmp
#include <wchar.h>

// ---- the three umd12 evidence exports (S4 section 3.3 + S4b) ----------------
// ⛔ The LUID crosses as two SCALARS, exactly as `tools/d3d12_bridge_probe.cpp:104`
// declares it: `winnt.h` is `typedef struct _LUID { DWORD LowPart; LONG HighPart; }`,
// so low is unsigned and high is signed, and parameters cannot be reordered by
// an ABI the way struct fields can be misdeclared.
typedef HRESULT      (*PFN_UMD12_CREATE)(unsigned int luid_low, int luid_high,
                                         void **out_bridge, void **out_device);
typedef unsigned int (*PFN_UMD12_CTXID)(void *bridge);
typedef void         (*PFN_UMD12_DESTROY)(void *bridge);

// `umd-check.ps1 -Mode release -Crate umd12` builds in the local mirror with
// CARGO_TARGET_DIR = <mirror>\umd12\target (tools/umd-check.ps1:83), so the
// release cdylib lands here. Overridable by the first non-flag argument.
static const wchar_t *DEFAULT_UMD12_DLL =
        L"C:\\Users\\Rupansh\\helios-vgpu\\umd12\\target\\release\\helios_umd12.dll";

// `bridge_icd_anchor.h:60`. Exported by BOTH cdylibs; that is the mechanism.
static const char *ANCHOR_EXPORT = "helios_icd_anchor_v1";
// `bridge_icd_anchor.h:73`. ⛔ ONE spelling, shared with both resolvers — a
// second spelling is a subtler version of the divergence the anchor prevents.
static const char *ICD_PROBE_EXPORT = "helios_venus_memory_alloc_info";
// `vn_renderer_helios.c:635-638`. ⚠ This is the PROCESS-GLOBAL, last-writer-wins
// value (`static uint32_t helios_current_ctx_id`, `:541`) — NOT the thread-local
// one. The thread-local is `helios_venus_instance_ctx_id`'s
// `helios_calling_thread_ctx_id` (`:548`, `:645-649`). Both are set from the
// same `helios->ctx_id` at CTX_CREATE (`:4201-4202`), which is why a bridge
// reading either synchronously on its creating thread gets the right answer.
static const char *ICD_CURRENT_CTX_EXPORT = "helios_venus_current_ctx_id";

typedef void *       (*PFN_ICD_ANCHOR)(void *candidate);
typedef unsigned int (*PFN_VENUS_CURRENT_CTX_ID)(void);

static int g_step = 0;
static int g_failures = 0;

#define STEP(fmt, ...)      printf("[%02d] " fmt "\n", ++g_step, __VA_ARGS__)
#define FAILSTEP(fmt, ...)  do { printf("[%02d] FAIL: " fmt "\n", ++g_step, __VA_ARGS__); g_failures++; } while (0)

static bool check(HRESULT hr, const char *what)
{
    if (SUCCEEDED(hr)) { printf("[%02d] ok    %s\n", ++g_step, what); return true; }
    printf("[%02d] FAIL  %s -> hr=0x%08lx\n", ++g_step, what, (unsigned long)hr);
    g_failures++;
    return false;
}

// One log line per assertion, same shape as `check` so a failure is attributable
// to one step rather than to "the probe".
static bool require_that(bool cond, const char *what)
{
    if (cond) { printf("[%02d] ok    %s\n", ++g_step, what); return true; }
    printf("[%02d] FAIL  %s\n", ++g_step, what);
    g_failures++;
    return false;
}

// ---- D3DKMT adapter enumeration --------------------------------------------
//
// Hand-declared rather than `#include <d3dkmthk.h>`: that header is WDK-only and
// this probe builds against the plain SDK, the same reason
// `tools/adapter_type_probe.cpp:8` gives. ⚠ These shapes are ABI; they are
// transcribed unchanged from `tools/d3d12_bridge_probe.cpp:259-306`, which
// transcribed them from the staged `tmp/dx12/sdk/d3dkmthk.h` (SDK 10.0.26100.0).
// They are read-only queries; nothing here is handed to a driver.
typedef LONG KMT_NTSTATUS;
typedef UINT D3DKMT_HANDLE;

struct KmtAdapterInfo {
    D3DKMT_HANDLE hAdapter;
    LUID          AdapterLuid;
    ULONG         NumOfSources;
    BOOL          bPresentMoveRegionsPreferred;
};
struct KmtEnumAdapters2 {
    ULONG            NumAdapters;
    KmtAdapterInfo  *pAdapters;
};
struct KmtQueryAdapterInfo {
    D3DKMT_HANDLE hAdapter;
    UINT          Type;
    VOID         *pPrivateDriverData;
    UINT          PrivateDriverDataSize;
};
struct KmtCloseAdapter { D3DKMT_HANDLE hAdapter; };
struct KmtUmdFileNameInfo {
    UINT  Version;                  // KMTUMDVERSION, d3dkmthk.h:1830
    WCHAR UmdFileName[MAX_PATH];
};
union KmtAdapterType {
    struct {
        UINT RenderSupported       : 1;
        UINT DisplaySupported      : 1;
        UINT SoftwareDevice        : 1;
        UINT PostDevice            : 1;
        UINT HybridDiscrete        : 1;
        UINT HybridIntegrated      : 1;
        UINT IndirectDisplayDevice : 1;
        UINT Paravirtualized       : 1;
        UINT Rest                  : 24;
    };
    UINT Value;
};
static const UINT KMTQAITYPE_UMDRIVERNAME = 1;
static const UINT KMTQAITYPE_ADAPTERTYPE  = 15;
static const UINT KMTUMDVERSION_DX11      = 2;
// ⛔ KMTQAITYPE_ADAPTERREGISTRYINFO is deliberately NOT queried: measured
// 2026-08-05 it fails on EVERY adapter on this box
// (`tools/d3d12_bridge_probe.cpp:354-356`), so a probe that reported the adapter
// by its registry description would report "(unavailable)" for all of them.

typedef KMT_NTSTATUS (WINAPI *PFN_KmtEnumAdapters2)(KmtEnumAdapters2 *);
typedef KMT_NTSTATUS (WINAPI *PFN_KmtQueryAdapterInfo)(KmtQueryAdapterInfo *);
typedef KMT_NTSTATUS (WINAPI *PFN_KmtCloseAdapter)(const KmtCloseAdapter *);

// Find the Helios adapter's LUID and the D3D11 UMD path the kernel serves for
// it. Never assume index 0 (GATES.md G1 trap).
//
// ⭐ Identify by the UMD the kernel serves, not by a marketing string:
// KMTQAITYPE_UMDRIVERNAME indexed by KMTUMDVERSION is the exact mechanism
// `DECISIONS.md` D3 rests on, so "the adapter whose D3D11 UMD is helios_umd*" is
// a direct statement about the driver under test, and it survives an INF
// description change. The returned path is also what step "d3d11 umd module"
// below is cross-checked against, which is how a run notices it loaded a
// different copy of the UMD than the one the runner inspected (memory 7th: a
// stale DriverStore UMD looked like a fixed one for a whole session).
static bool find_helios_luid(LUID *out, wchar_t *umd_out, size_t umd_cch)
{
    HMODULE gdi = LoadLibraryA("gdi32.dll");
    if (!gdi) { printf("       LoadLibraryA(gdi32.dll) -> %lu\n", GetLastError()); return false; }

    auto EnumAdapters2    = (PFN_KmtEnumAdapters2)   GetProcAddress(gdi, "D3DKMTEnumAdapters2");
    auto QueryAdapterInfo = (PFN_KmtQueryAdapterInfo)GetProcAddress(gdi, "D3DKMTQueryAdapterInfo");
    auto CloseAdapter     = (PFN_KmtCloseAdapter)    GetProcAddress(gdi, "D3DKMTCloseAdapter");
    if (!EnumAdapters2 || !QueryAdapterInfo || !CloseAdapter) {
        printf("       D3DKMT entry points missing from gdi32.dll\n");
        return false;
    }

    KmtEnumAdapters2 ea = {};
    if (EnumAdapters2(&ea) < 0 || ea.NumAdapters == 0) {
        printf("       D3DKMTEnumAdapters2(count) failed or reported 0 adapters\n");
        return false;
    }
    ea.pAdapters = (KmtAdapterInfo *)calloc(ea.NumAdapters, sizeof(KmtAdapterInfo));
    if (!ea.pAdapters) return false;
    if (EnumAdapters2(&ea) < 0) { free(ea.pAdapters); return false; }

    bool found = false;
    for (ULONG i = 0; i < ea.NumAdapters; ++i) {
        KmtAdapterInfo &a = ea.pAdapters[i];

        KmtAdapterType t = {};
        KmtQueryAdapterInfo qt = { a.hAdapter, KMTQAITYPE_ADAPTERTYPE, &t, sizeof(t) };
        QueryAdapterInfo(&qt);

        KmtUmdFileNameInfo umd = {};
        umd.Version = KMTUMDVERSION_DX11;
        KmtQueryAdapterInfo qu = { a.hAdapter, KMTQAITYPE_UMDRIVERNAME, &umd, sizeof(umd) };
        const KMT_NTSTATUS us = QueryAdapterInfo(&qu);

        printf("       adapter[%lu] luid=%08lx:%08lx type=0x%08x{Render=%u Display=%u Sw=%u Paravirt=%u}"
               " umd[dx11]=%ls\n",
               (unsigned long)i,
               (unsigned long)a.AdapterLuid.HighPart, (unsigned long)a.AdapterLuid.LowPart,
               t.Value, t.RenderSupported, t.DisplaySupported, t.SoftwareDevice, t.Paravirtualized,
               us >= 0 ? umd.UmdFileName : L"(no umd name)");

        // SoftwareDevice excludes WARP / Basic Render Driver. The substring is
        // `helios_umd` and not `helios_umd.dll` on purpose: ProgramData hotplug
        // names the deployed copy `helios_umd_<hash>.dll`
        // (`tools/hotplug-helios-umd.ps1:75`).
        if (!found && us >= 0 && !t.SoftwareDevice && t.RenderSupported &&
            wcsstr(umd.UmdFileName, L"helios_umd") != nullptr) {
            *out = a.AdapterLuid;
            wcsncpy_s(umd_out, umd_cch, umd.UmdFileName, _TRUNCATE);
            found = true;
        }

        KmtCloseAdapter c = { a.hAdapter };
        CloseAdapter(&c);
    }
    free(ea.pAdapters);
    return found;
}

// ---- the loaded-module walk -------------------------------------------------
//
// ⚠ `K32EnumProcessModules`, not `EnumProcessModules` from `psapi.lib`: the
// K32-prefixed forms live in `kernel32.dll` on every Windows the driver targets,
// so this adds no import library — the same choice, for the same reason, as
// `bridge_icd_anchor.cpp:27-37`, and it is the SAME enumeration the anchor
// resolvers use, so this probe observes the order they depend on.
//
// ⚠ `find_venus_icd_module` (`bridge_icd_anchor.cpp:130-157`) walks with
// `TH32CS_SNAPMODULE` instead. That difference cannot matter to the assertion
// below, because the assertion is a COUNT (exactly one module exports the ICD
// probe symbol) and a count is enumeration-order-independent. If the count were
// ever >1 the two walks could disagree about which is first — which is precisely
// the hazard, and precisely why the count is the assertion.
struct ModuleRef {
    HMODULE module;
    char    path[MAX_PATH];
};

static const DWORD kMaxModules = 1024;   // matches the ICD's own fixed buffer

static DWORD collect_exporters(const char *sym, ModuleRef *out, DWORD cap)
{
    HMODULE mods[kMaxModules];
    DWORD needed = 0;
    if (!K32EnumProcessModules(GetCurrentProcess(), mods, sizeof(mods), &needed)) {
        printf("       K32EnumProcessModules -> %lu\n", GetLastError());
        return 0;
    }
    DWORD count = needed / (DWORD)sizeof(HMODULE);
    if (count > kMaxModules) count = kMaxModules;   // prefix of load order; see above

    DWORD hits = 0;
    for (DWORD i = 0; i < count && hits < cap; ++i) {
        if (!GetProcAddress(mods[i], sym)) continue;
        out[hits].module = mods[i];
        out[hits].path[0] = '\0';
        GetModuleFileNameA(mods[i], out[hits].path, (DWORD)sizeof(out[hits].path));
        hits++;
    }
    return hits;
}

// ---- process state shared by the two stages ---------------------------------
// File-static so the two stages can run in either order and the assertion block
// afterwards is identical in both.
static IDXGIFactory1       *g_factory;
static IDXGIAdapter1       *g_dxgi_adapter;
static ID3D11Device        *g_d3d11;
static ID3D11DeviceContext *g_d3d11_ctx;

static HMODULE            g_umd12;
static PFN_UMD12_CREATE   g_umd12_create;
static PFN_UMD12_CTXID    g_umd12_ctxid;
static PFN_UMD12_DESTROY  g_umd12_destroy;
static void              *g_umd12_bridge;
static void              *g_umd12_device;      // BORROWED; see below

// ── stage: the D3D11 vehicle ────────────────────────────────────────────────
// This is what loads the DEPLOYED helios_umd.dll and makes DXVK build a
// VkInstance on the venus ICD. ⛔ The device is kept alive until after the
// assertions: `helios_umd.dll` is loaded and unloaded ONCE PER D3D11 DEVICE
// (measured, `umd_common/src/log.rs:197`), so releasing it early would
// unload the very module the anchor walk has to find.
static bool stage_d3d11(const LUID &luid)
{
    if (!check(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void **)&g_factory),
               "CreateDXGIFactory1")) return false;

    for (UINT i = 0;; ++i) {
        IDXGIAdapter1 *a = nullptr;
        HRESULT er = g_factory->EnumAdapters1(i, &a);
        if (er == DXGI_ERROR_NOT_FOUND) break;
        // ⚠ Not `while (EnumAdapters1(...) != DXGI_ERROR_NOT_FOUND)`: any other
        // failure leaves `a` null and that loop dereferences it.
        if (FAILED(er) || !a) {
            printf("       EnumAdapters1(%u) -> hr=0x%08lx (stopping)\n", i, (unsigned long)er);
            break;
        }
        DXGI_ADAPTER_DESC1 d = {};
        a->GetDesc1(&d);
        const bool match = d.AdapterLuid.LowPart == luid.LowPart &&
                           d.AdapterLuid.HighPart == luid.HighPart;
        printf("       dxgi[%u] luid=%08lx:%08lx \"%ls\"%s\n", i,
               (unsigned long)d.AdapterLuid.HighPart, (unsigned long)d.AdapterLuid.LowPart,
               d.Description, match ? "  <= LUID match" : "");
        if (match && !g_dxgi_adapter) { g_dxgi_adapter = a; g_dxgi_adapter->AddRef(); }
        a->Release();
    }

    if (!g_dxgi_adapter) {
        // ⛔ Never fall back to adapter 0 or to a description match: a D3D11
        // device on the WRONG adapter would load a different UMD and every
        // assertion below would then be measuring some other driver.
        FAILSTEP("no DXGI adapter with luid=%08lx:%08lx - the D3DKMT LUID has no DXGI twin",
                 (unsigned long)luid.HighPart, (unsigned long)luid.LowPart);
        return false;
    }
    STEP("DXGI adapter matched by LUID (AdapterLuid, not by description)%s", "");

    const D3D_FEATURE_LEVEL levels[] = { D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0 };
    D3D_FEATURE_LEVEL achieved = (D3D_FEATURE_LEVEL)0;
    // D3D_DRIVER_TYPE_UNKNOWN is REQUIRED when an explicit adapter is supplied.
    HRESULT hr = D3D11CreateDevice(g_dxgi_adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0,
                                   levels, ARRAYSIZE(levels), D3D11_SDK_VERSION,
                                   &g_d3d11, &achieved, &g_d3d11_ctx);
    if (!check(hr, "D3D11CreateDevice(Helios adapter) - loads helios_umd.dll, builds a VkInstance"))
        return false;
    STEP("D3D11 device up, featureLevel=0x%04x device=%p context=%p",
         (unsigned)achieved, (void *)g_d3d11, (void *)g_d3d11_ctx);
    return true;
}

// ── stage: helios_umd12.dll ─────────────────────────────────────────────────
static bool stage_umd12(const wchar_t *dll_path, const LUID &luid)
{
    // ⚠ The resolved path is printed because more than one copy of a UMD exists
    // on disk (build tree, DriverStore, ProgramData hotplug): loading one and
    // reasoning about another is the mistake that made a stale UMD look fixed.
    g_umd12 = LoadLibraryW(dll_path);
    if (!g_umd12) {
        FAILSTEP("LoadLibraryW(%ls) -> GetLastError=%lu", dll_path, GetLastError());
        return false;
    }
    wchar_t resolved[MAX_PATH] = {};
    GetModuleFileNameW(g_umd12, resolved, MAX_PATH);
    STEP("LoadLibraryW(helios_umd12.dll) ok, base=%p", (void *)g_umd12);
    printf("       loaded from = %ls\n", resolved);

    g_umd12_create  = (PFN_UMD12_CREATE) GetProcAddress(g_umd12, "helios_umd12_probe_create_device_v1");
    g_umd12_ctxid   = (PFN_UMD12_CTXID)  GetProcAddress(g_umd12, "helios_umd12_probe_venus_context_id_v1");
    g_umd12_destroy = (PFN_UMD12_DESTROY)GetProcAddress(g_umd12, "helios_umd12_probe_destroy_device_v1");
    if (!g_umd12_create || !g_umd12_ctxid || !g_umd12_destroy) {
        FAILSTEP("GetProcAddress: create=%p ctxid=%p destroy=%p (a null one means this DLL predates S4b)",
                 (void *)g_umd12_create, (void *)g_umd12_ctxid, (void *)g_umd12_destroy);
        return false;
    }
    STEP("three umd12 probe exports resolved (create=%p venus_ctx_id=%p destroy=%p)",
         (void *)g_umd12_create, (void *)g_umd12_ctxid, (void *)g_umd12_destroy);

    HRESULT hr = g_umd12_create((unsigned int)luid.LowPart, (int)luid.HighPart,
                                &g_umd12_bridge, &g_umd12_device);
    if (!check(hr, "helios_umd12_probe_create_device_v1 - vkd3d builds its own VkInstance"))
        return false;
    // ⛔ `g_umd12_device` is a BORROWED `ID3D12Device*`: the bridge keeps the
    // owning reference (`umd12/src/probe12.rs:68-71`). Its refcount is never
    // touched here - no AddRef, therefore no Release - so the bridge's single
    // reference stays balanced and `..._destroy_device_v1` is the only teardown.
    STEP("umd12 bridge=%p device=%p (BORROWED - refcount untouched)",
         g_umd12_bridge, g_umd12_device);
    return true;
}

// ── the assertion ───────────────────────────────────────────────────────────
static void assert_one_icd_module(void)
{
    ModuleRef anchors[16] = {};
    const DWORD n_anchor = collect_exporters(ANCHOR_EXPORT, anchors, ARRAYSIZE(anchors));

    STEP("%lu loaded module(s) export %s (walk order = load order)",
         (unsigned long)n_anchor, ANCHOR_EXPORT);
    for (DWORD i = 0; i < n_anchor; ++i)
        printf("       anchor[%lu] %s%s\n", (unsigned long)i, anchors[i].path,
               anchors[i].module == g_umd12 ? "   <= helios_umd12.dll" : "");

    // Both UMDs must be present, or the run is not the two-engine process S4b is
    // about. A deployed helios_umd.dll that predates S4b exports nothing here,
    // and that is the likeliest cause of a count of 1.
    bool have_umd12 = false;
    const ModuleRef *d3d11_umd = nullptr;
    for (DWORD i = 0; i < n_anchor; ++i) {
        if (anchors[i].module == g_umd12) have_umd12 = true;
        else if (!d3d11_umd)              d3d11_umd = &anchors[i];
    }
    require_that(have_umd12, "helios_umd12.dll exports helios_icd_anchor_v1");
    if (require_that(d3d11_umd != nullptr,
                     "a second, non-umd12 module exports helios_icd_anchor_v1 "
                     "(the deployed helios_umd.dll; a miss means it predates S4b)")) {
        // Machine-greppable: the runner cross-checks this against the DLL it ran
        // dumpbin /EXPORTS on, so the export assertion cannot land on a copy the
        // process never loaded.
        printf("       d3d11 umd module = %s\n", d3d11_umd->path);
    }
    if (n_anchor == 0) {
        FAILSTEP("no module exports %s - nothing to reconcile; the rest of this "
                 "probe cannot mean anything", ANCHOR_EXPORT);
        return;
    }

    // (a) + (b): query every copy with NULL. ⛔ NULL is a pure query and
    // publishes nothing (`bridge_icd_anchor.cpp:85-92`), so the probe cannot
    // paper over a mismatch by publishing on a driver's behalf. Exactly one copy
    // is ever the publisher (see the header); the rest answer null by
    // construction, and only a DIFFERENT non-null answer is a defect.
    void *published = nullptr;
    const char *publisher_path = "(none)";
    bool split_brain = false;
    for (DWORD i = 0; i < n_anchor; ++i) {
        auto anchor = (PFN_ICD_ANCHOR)GetProcAddress(anchors[i].module, ANCHOR_EXPORT);
        void *answer = anchor ? anchor(nullptr) : nullptr;
        printf("       anchor[%lu] query(NULL) -> %p   %s\n", (unsigned long)i, answer,
               anchors[i].path);
        if (!answer) continue;
        if (!published) { published = answer; publisher_path = anchors[i].path; }
        else if (answer != published) split_brain = true;
    }
    require_that(published != nullptr,
                 "the process anchor has published a venus ICD module "
                 "(non-null from the first copy in walk order)");
    require_that(!split_brain,
                 "no two anchor copies published DIFFERENT modules "
                 "(a split answer is two live publishers)");
    if (published)
        printf("       published by = %s\n", publisher_path);

    // (c) the published module really is the venus ICD, by the one definition
    // both resolvers use.
    char icd_path[MAX_PATH] = "(none)";
    if (published) {
        GetModuleFileNameA((HMODULE)published, icd_path, sizeof(icd_path));
        printf("       icd module = %s\n", icd_path);
        require_that(GetProcAddress((HMODULE)published, ICD_PROBE_EXPORT) != nullptr,
                     "the published module exports helios_venus_memory_alloc_info "
                     "(bridge_icd_anchor.h:73 - that export IS the definition)");
    }

    // (d) THE RAW INVARIANT, measured independently of the anchor: one venus ICD
    // module per process. A count, so it does not depend on enumeration order.
    ModuleRef icds[16] = {};
    const DWORD n_icd = collect_exporters(ICD_PROBE_EXPORT, icds, ARRAYSIZE(icds));
    for (DWORD i = 0; i < n_icd; ++i)
        printf("       icd[%lu] %s\n", (unsigned long)i, icds[i].path);
    require_that(n_icd == 1,
                 "exactly ONE loaded module exports helios_venus_memory_alloc_info "
                 "(the invariant, independent of the anchor plumbing)");
    if (n_icd >= 1 && published)
        require_that(icds[0].module == (HMODULE)published,
                     "the anchor published the same module a fresh first-hit-wins "
                     "walk selects");

    // ---- the two context ids: REPORTED, never asserted equal ----------------
    // See the correction at the top of this file. Asserting equality here would
    // assert something the ICD cannot do.
    const unsigned int umd12_ctx = (g_umd12_ctxid && g_umd12_bridge)
                                       ? g_umd12_ctxid(g_umd12_bridge) : 0;
    printf("       umd12 venus ctx id = %u\n", umd12_ctx);

    unsigned int icd_ctx = 0;
    bool have_icd_ctx = false;
    if (published) {
        auto cur = (PFN_VENUS_CURRENT_CTX_ID)GetProcAddress((HMODULE)published,
                                                            ICD_CURRENT_CTX_EXPORT);
        if (cur) { icd_ctx = cur(); have_icd_ctx = true; }
    }
    printf("       icd process-global ctx id = %u%s\n", icd_ctx,
           have_icd_ctx ? "" : "  (export unavailable)");
    STEP("venus context ids REPORTED (umd12=%u, icd last-writer=%u) - not asserted equal",
         umd12_ctx, icd_ctx);

    printf("\n"
           "       NOTE (this run's own evidence about ARCHITECTURE.md section 6.4)\n"
           "       6.4 asks for \"both venus context ids non-zero and EQUAL\". Each\n"
           "       engine builds its own VkInstance and the ICD mints a context per\n"
           "       instance (vn_renderer_helios.c:4201-4202), so two engines in one\n"
           "       process are EXPECTED to hold two different ids. The id printed\n"
           "       above as \"icd process-global\" is helios_current_ctx_id, which is\n"
           "       last-writer-wins across instances (vn_renderer_helios.c:534-538),\n"
           "       so it names whichever engine created its instance LAST:\n"
           "         normal order  (D3D11 then D3D12): expect it to equal umd12's;\n"
           "         -reverse      (D3D12 then D3D11): expect it to DIFFER from\n"
           "                       umd12's, because the D3D11 instance came later.\n"
           "       A -reverse run that prints two different non-zero ids on ONE ICD\n"
           "       module has settled the question: same module, two contexts.\n"
           "\n"
           "       NOTE (IcdAnchorMismatch)\n"
           "       The counter is a static inside each DLL's copy of the anchor\n"
           "       (bridge_icd_anchor.cpp:46) and is not exported, so no probe can\n"
           "       read it. It is only logged, and only on the mismatch path\n"
           "       (bridge_icd_anchor.cpp:226-231). Expected value: 0, i.e. the\n"
           "       string \"IcdAnchorMismatch\" ABSENT from this run's block of both\n"
           "       C:\\ProgramData\\Helios\\umd-<pid>.log and umd12-<pid>.log. The\n"
           "       runner (tmp/dx12/build-s4b-anchor.ps1) asserts that, plus one\n"
           "       identical \"selected coherent loaded ICD module ->\" line in each.\n");
}

static void usage(void)
{
    printf("usage: icd_anchor_probe [path\\to\\helios_umd12.dll] [-reverse]\n"
           "  -reverse   create the D3D12 device BEFORE the D3D11 one, so a pair of\n"
           "             runs proves the anchor is load-order independent.\n");
}

int main(int argc, char **argv)
{
    const wchar_t *dll_path = DEFAULT_UMD12_DLL;
    wchar_t dll_buf[MAX_PATH];
    bool reverse = false;

    for (int i = 1; i < argc; ++i) {
        if (_stricmp(argv[i], "-reverse") == 0 || _stricmp(argv[i], "/reverse") == 0) {
            reverse = true;
        } else if (_stricmp(argv[i], "-h") == 0 || _stricmp(argv[i], "-help") == 0 ||
                   _stricmp(argv[i], "/?") == 0) {
            usage();
            return 0;
        } else {
            MultiByteToWideChar(CP_ACP, 0, argv[i], -1, dll_buf, MAX_PATH);
            dll_path = dll_buf;
        }
    }

    printf("icd_anchor_probe - S4b venus-ICD anchor gate (%s order)\n",
           reverse ? "REVERSE: D3D12 first" : "normal: D3D11 first");
    printf("       umd12 dll = %ls\n", dll_path);
    // The pid is this probe's only handle on IcdAnchorMismatch: the counter is
    // never exported, only logged into C:\ProgramData\Helios\umd-<pid>.log and
    // umd12-<pid>.log, which the runner then reads. ⚠ Those files are
    // APPEND-ONLY and pids are reused across boots, so the runner must take only
    // the block after the last "UMD module:" line (umd_common/src/log.rs:115-118).
    printf("       probe pid = %lu\n", (unsigned long)GetCurrentProcessId());

    // ---- 1. the adapter LUID -------------------------------------------------
    LUID luid = {};
    wchar_t umd_name[MAX_PATH] = L"(none)";
    if (!find_helios_luid(&luid, umd_name, ARRAYSIZE(umd_name))) {
        FAILSTEP("no adapter served by helios_umd* found via D3DKMT%s", "");
        return 1;
    }
    STEP("Helios adapter luid=%08lx:%08lx", (unsigned long)luid.HighPart,
         (unsigned long)luid.LowPart);
    printf("       kernel says dx11 umd = %ls\n", umd_name);

    // ---- 2/3. the two engines, in the requested order -------------------------
    // ⛔ Both stages must succeed before the assertion means anything: with one
    // engine the anchor is trivially self-consistent (bridge_icd_anchor.h:29-32)
    // and a PASS would prove nothing about the mixed process, which is the only
    // case S4b exists for.
    bool ok = true;
    if (reverse) {
        ok = stage_umd12(dll_path, luid) && ok;
        ok = stage_d3d11(luid) && ok;
    } else {
        ok = stage_d3d11(luid) && ok;
        ok = stage_umd12(dll_path, luid) && ok;
    }

    if (ok) {
        // ---- 4/5. the assertion + the reported context ids -------------------
        assert_one_icd_module();
    } else {
        FAILSTEP("one of the two engines did not come up; skipping the anchor "
                 "assertion rather than reporting a single-engine PASS%s", "");
    }

    printf("\nS4b anchor %s - %d failure(s) across %d steps (%s order)\n",
           g_failures ? "FAIL" : "PASS", g_failures, g_step,
           reverse ? "reverse" : "normal");

    // ---- 6. teardown ---------------------------------------------------------
    // Reverse order of creation. A hang or a crash here is a real finding: the
    // UMDs tear devices down constantly (memory 54th, the six-handles-per-device
    // leak).
    // ⛔ No FreeLibrary(g_umd12): the anchor publisher may live in that module,
    // and unloading it while helios_umd.dll is still resolving would manufacture
    // exactly the mismatch this probe exists to rule out. The process is about
    // to exit; there is nothing to reclaim and something to break.
    if (g_umd12_destroy && g_umd12_bridge) g_umd12_destroy(g_umd12_bridge);
    if (g_d3d11_ctx)     g_d3d11_ctx->Release();
    if (g_d3d11)         g_d3d11->Release();
    if (g_dxgi_adapter)  g_dxgi_adapter->Release();
    if (g_factory)       g_factory->Release();

    return g_failures ? 1 : 0;
}
