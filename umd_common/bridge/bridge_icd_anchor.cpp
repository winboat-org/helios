// The process-global venus-ICD anchor. See `bridge_icd_anchor.h` for the whole
// argument; this file is the mechanism.
//
// ⛔ **Compiled into BOTH cdylibs** (`umd/build.rs` and `umd12/build.rs` each
// list it as an extra `.file()`). That is not duplication in D3b's sense — it is
// ONE source compiled twice, exactly like `bridge_guard.h`'s template, and it is
// what makes both DLLs export the name. Each copy carries its own publisher
// static; only the copy in the first module the loader enumerated is ever
// called, and that copy's static is the process's single source of truth.

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <psapi.h>       // declares K32EnumProcessModules (the impl is in kernel32)
#include <tlhelp32.h>

#include <atomic>
#include <cstdio>

#include "bridge_common.h"       // umd_log
#include "bridge_icd_anchor.h"

// ⚠ `K32EnumProcessModules`, not `EnumProcessModules` from `psapi.lib`. The
// K32-prefixed forms live in `kernel32.dll` on every Windows the driver targets
// (`_WIN32_WINNT >= 0x0600`), so this adds no import library to either cdylib's
// link line — and `umd12`'s link set is a *measured* minimum of one archive plus
// `gdi32` (`tmp/dx12/gates/G1-static/RESULT.md`), which a new `psapi` would
// silently grow.
//
// It is also the SAME call the Mesa ICD's own first-hit-wins walk uses
// (`icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:711-732`). Using a different
// enumeration here would mean the two walks could disagree about module order,
// which is the one property this mechanism rests on.

namespace {

/// The published module. Written exactly once per process, by whichever copy of
/// `helios_icd_anchor_v1` the loader put first.
std::atomic<void*> g_anchor{nullptr};

/// `IcdAnchorMismatch`. Expected 0 forever.
std::atomic<unsigned int> g_icdAnchorMismatch{0};

/// Sticky: once two ICD modules have been seen, this process never trusts the
/// venus identity plumbing again.
std::atomic<bool> g_poisoned{false};

/// Find `helios_icd_anchor_v1` the way the ICD finds *its* exports: walk every
/// loaded module, first hit wins.
///
/// ⚠ A fixed 1024-entry buffer, matching the ICD's own
/// (`wsi_common_win32.cpp:711-732`). If a process ever exceeds it,
/// `K32EnumProcessModules` reports the *needed* size and the count below is
/// clamped — so the walk covers a prefix of load order, which still contains the
/// earliest-loaded UMD. Truncation cannot make two callers disagree, because
/// both walk the same prefix.
using PfnAnchor = void* (*)(void*);

PfnAnchor find_process_anchor() {
  HMODULE modules[1024];
  DWORD needed = 0;
  if (!K32EnumProcessModules(GetCurrentProcess(), modules, sizeof(modules),
                             &needed)) {
    return nullptr;
  }
  DWORD count = needed / sizeof(HMODULE);
  if (count > (DWORD)(sizeof(modules) / sizeof(modules[0]))) {
    count = (DWORD)(sizeof(modules) / sizeof(modules[0]));
  }
  for (DWORD i = 0; i < count; i++) {
    if (FARPROC fn = GetProcAddress(modules[i], "helios_icd_anchor_v1")) {
      return reinterpret_cast<PfnAnchor>(reinterpret_cast<void*>(fn));
    }
  }
  return nullptr;
}

}  // namespace

extern "C" void* helios_icd_anchor_v1(void* candidate) {
  if (!candidate) {
    // Pure query. ⚠ Must NOT publish: a caller asking "who is canonical?"
    // before anyone has resolved one would otherwise publish null and, with a
    // plain compare-exchange against null, leave the slot still empty — the
    // difference is only visible if this ever becomes a non-null sentinel, so
    // the early return states the intent rather than relying on that.
    return g_anchor.load(std::memory_order_acquire);
  }
  void* expected = nullptr;
  if (g_anchor.compare_exchange_strong(expected, candidate,
                                       std::memory_order_release,
                                       std::memory_order_acquire)) {
    return candidate;   // we are the publisher
  }
  return expected;      // someone published first; theirs wins
}

namespace helios_bridge {

// ── the module walk ─────────────────────────────────────────────────────────
//
// Moved from `umd/bridge/bridge_icd_exports.cpp`'s
// `find_helios_icd_in_loaded_modules` (`DECISIONS.md` D3b, stage S4b), because
// `umd12` needs exactly this and would otherwise carry a second copy — which is
// the divergence `ARCHITECTURE.md` §6.4 exists to prevent, one level up from
// the one the original comment describes.
//
// ⚠ A MOVE, not a rewrite: same export probe, same `GetModuleHandleExA`
// reference-hold, same log line. `umd`'s behaviour must be unchanged by S4b
// apart from the added anchor step.

namespace {
constexpr const char* kHeliosVenusCurrentCtxId = "helios_venus_current_ctx_id";

/// KMD-assigned context ids are small monotonically allocated integers. A value
/// such as `0xcccccc00` means the export decoded the wrong handle/object, not a
/// real Venus context. Verbatim from `bridge_icd_exports.cpp`'s
/// `plausible_venus_context_id`.
bool plausible_venus_context_id(unsigned int ctx) {
  return ctx != 0 && ctx < 0x01000000u;
}

std::atomic<unsigned int> g_venusCtxImplausible{0};
}  // namespace

void* find_venus_icd_module() {
  HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, GetCurrentProcessId());
  if (snapshot == INVALID_HANDLE_VALUE) {
    return nullptr;
  }
  MODULEENTRY32 module = {};
  module.dwSize = sizeof(module);
  if (Module32First(snapshot, &module)) {
    do {
      if (GetProcAddress(module.hModule, kVenusIcdProbeExport)) {
        // Hold one reference for the process-wide export table. Looking up
        // each export independently can mix two ICD versions and call a
        // function with a foreign VkDeviceMemory/VkInstance handle.
        HMODULE held = nullptr;
        if (GetModuleHandleExA(0, module.szExePath, &held)) {
          char msg[512];
          std::snprintf(msg, sizeof(msg),
            "selected coherent loaded ICD module -> %s", module.szExePath);
          umd_log(msg);
          CloseHandle(snapshot);
          return held;
        }
      }
    } while (Module32Next(snapshot, &module));
  }
  CloseHandle(snapshot);
  return nullptr;
}

unsigned int read_venus_ctx_id(void* module) {
  if (!module) {
    return 0;
  }
  using Fn = unsigned int(__cdecl*)();
  auto fn = reinterpret_cast<Fn>(reinterpret_cast<void*>(
      GetProcAddress(static_cast<HMODULE>(module), kHeliosVenusCurrentCtxId)));
  if (!fn) {
    umd_log("venus ctx id: helios_venus_current_ctx_id export unavailable");
    return 0;
  }
  const unsigned int ctx = fn();
  if (ctx && !plausible_venus_context_id(ctx)) {
    // ⛔ Return 0 rather than a wrong value, so the caller's loud "no venus
    // context" path fires instead of a silent wrong stamp. Same policy, and the
    // same reasoning, as `read_current_venus_context_id`'s in `umd`.
    const unsigned int n =
        g_venusCtxImplausible.fetch_add(1, std::memory_order_relaxed) + 1;
    char msg[160];
    std::snprintf(msg, sizeof(msg),
                  "venus ctx export returned implausible ctx_id=%u -> 0 (x%u)",
                  ctx, n);
    umd_log(msg);
    return 0;
  }
  return ctx;
}

void* reconcile_icd_anchor(void* candidate) {
  if (!candidate) {
    return nullptr;
  }

  PfnAnchor anchor = find_process_anchor();
  if (!anchor) {
    // ⚠ Not a refusal, and deliberately uncounted. This module exports the
    // symbol itself, so the only way the walk misses it is
    // `K32EnumProcessModules` failing outright — at which point there is no
    // second UMD to disagree with either, and the local candidate IS the
    // process's answer. Degrading to "trust myself" is strictly safer than
    // refusing on a diagnostic call's failure.
    umd_log("icd anchor: K32EnumProcessModules found no helios_icd_anchor_v1; "
            "using this module's own ICD resolution");
    return candidate;
  }

  void* canonical = anchor(candidate);
  if (canonical == candidate) {
    return candidate;
  }

  // ⛔ REFUSE. Do not adopt `canonical`: see `bridge_icd_anchor.h` — this
  // module's Vulkan objects came from whichever ICD the *Vulkan loader* bound,
  // and interrogating a different module with them is the foreign-handle bug
  // the anchor exists to prevent.
  const unsigned int n =
      g_icdAnchorMismatch.fetch_add(1, std::memory_order_relaxed) + 1;
  g_poisoned.store(true, std::memory_order_release);

  char canonical_path[MAX_PATH] = "<unresolvable>";
  char candidate_path[MAX_PATH] = "<unresolvable>";
  GetModuleFileNameA(static_cast<HMODULE>(canonical), canonical_path,
                     sizeof(canonical_path));
  GetModuleFileNameA(static_cast<HMODULE>(candidate), candidate_path,
                     sizeof(candidate_path));

  char msg[2 * MAX_PATH + 160];
  std::snprintf(msg, sizeof(msg),
                "IcdAnchorMismatch=%u REFUSING: this module resolved venus ICD "
                "%s but the process anchor published %s - two ICD builds in one "
                "process would mix foreign VkDeviceMemory/VkInstance handles",
                n, candidate_path, canonical_path);
  umd_log(msg);
  return nullptr;
}

bool icd_anchor_poisoned() {
  return g_poisoned.load(std::memory_order_acquire);
}

unsigned int icd_anchor_mismatch_count() {
  return g_icdAnchorMismatch.load(std::memory_order_relaxed);
}

}  // namespace helios_bridge
