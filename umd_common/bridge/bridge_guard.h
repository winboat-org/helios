// The one catch triple both Helios bridges route every cxx entry point through.
//
// Moved from `umd/bridge/dxvk_bridge.cpp:270-325` at stage S1
// (`DECISIONS.md` D3b). ⛔ **This file exists so that a second guard template is
// never written from scratch.** `ARCHITECTURE.md` §12 rule 4 records what that
// costs: commit `ead692e`, a bare `0` sentinel against a `std::size_t` body
// deduced `R = int` and truncated every returned pointer to 32 bits; dwm and
// LogonUI crash-looped at cold boot and nothing warned. The assert below is
// the fix, and this is the check that it is still the only one:
//
//     grep -rnE '^[[:space:]]*static_assert\(' umd/bridge umd12/bridge umd_common/bridge
//
// must return exactly one hit — the `static_assert` further down this file.
//
// ⚠ TWO earlier spellings of this check were BROKEN, and both looked fine:
//   * the bare word `static_assert` matched this very comment and the pointer
//     left behind in `dxvk_bridge.cpp` — it reported 3 and read as a
//     regression;
//   * adding the trailing `(` did NOT fix that, because both of those comments
//     quote the paren too. It still reported 3, right up until S4 measured it.
// The anchor is what actually works: a real `static_assert` starts its line
// (after indentation), a quoted one is preceded by `//`.
//
// ⛔ And do NOT reach for `git grep` here. It skips untracked files, so a
// freshly added `umd12/bridge/` — which is exactly when a second guard is most
// likely to appear — reads as **0** and the check passes by being blind.
#pragma once

#include <cstdio>
#include <exception>
#include <type_traits>

#include "bridge_common.h"   // umd_log

// ── The one engine-specific customization point ─────────────────────────────
//
// DXVK throws `dxvk::DxvkError`, which is not a `std::exception`, so the D3D11
// bridge needs an arm for it BEFORE the generic ones. vkd3d is a C library
// behind a COM ABI and throws nothing, so the D3D12 bridge needs no arm at all.
//
// A bridge that wants one defines this macro to a complete `catch` clause
// before including this header:
//
//     #define HELIOS_BRIDGE_ENGINE_CATCH(what)                        \
//       catch (const dxvk::DxvkError&) {                              \
//         char msg[160];                                              \
//         std::snprintf(msg, sizeof(msg), "%s: DxvkError", what);     \
//         ::helios_bridge::umd_log(msg);                              \
//       }
//     #include "bridge_guard.h"
//
// ⚠ Why a macro and not a template parameter or a rethrow hook: both of those
// change the generated code. A `template <typename EngineError = ...>` moves the
// arm's message off the specific type and touches every one of the ~20 call
// sites; a `catch (...) { if (!engine_describe(what)) ... }` hook adds a
// rethrow. S1 is a MOVE — behaviour change here is a defect, not an
// improvement — so the macro was chosen precisely because it expands to the
// identical try/catch chain the D3D11 bridge had before.
//
// ⛔ The arm it expands to must obey the same rule as the generic ones below:
// **it must not allocate.** Fixed `char[]` + `snprintf` only.
#ifndef HELIOS_BRIDGE_ENGINE_CATCH
#define HELIOS_BRIDGE_ENGINE_CATCH(what) /* no engine-specific exception type */
#endif

namespace helios_bridge {

// cxx emits EVERY generated C++ shim `noexcept` (verified verbatim in the
// checked-in generated artifact, bridge.rs.cc), so an exception escaping a
// bridge method is std::terminate — dwm.exe dies instead of the DDI returning
// a failure. Seven methods had no handler at all, and every one of them
// reaches code that allocates (find_helios_icd_export ->
// discover_vulkan_icd_manifests builds a std::vector<std::string>, runs
// ifstream/ostringstream over the manifest and concatenates strings; the
// now-retired present_flip_wait_setup additionally took a lock_guard,
// make_shared and constructed a std::thread). Defect class: a recoverable
// resource failure escalated to unconditional death of the compositor.
//
// R1014(4): this is the ONLY catch triple in the D3D11 bridge. The other nine
// were hand-written copies whose DxvkError arm built a std::string, which is
// the bug the paragraph below describes -- so folding them in was a
// robustness fix, not only a dedupe.
//
// Making this the only path that can return the sentinel collapses "error
// sentinel" and "escaped exception" into one code path. The compiler cannot
// prove a body is exception-free, so that is the honest limit of the
// guarantee.
//
// The catch arms must not allocate: a std::string built inside a
// std::bad_alloc handler can throw again. Fixed char[] + snprintf only —
// which is also why DxvkError::message() (returns std::string) is not called
// in the engine arm.
//
// `R` is deduced from `on_error` ALONE — the body's return type is not a
// deduction context — so the sentinel silently decides the type every
// SUCCESS value is converted to on the way out. A bare `0` against a
// `std::size_t` body deduced `R = int` and truncated every returned pointer
// to 32 bits: that shipped in T7 and access-violated dwm and LogonUI at the
// first `VSSetShader`. The static_assert makes the mismatch a compile error
// instead of a pointer that looks plausible in a log line.
template <typename R, typename Fn>
R bridge_guard(const char* what, R on_error, Fn&& fn) noexcept {
  static_assert(std::is_same_v<R, decltype(fn())>,
                "bridge_guard's error value must have the guarded body's exact "
                "return type; otherwise the success path is converted too");
  try {
    return fn();
  }
  HELIOS_BRIDGE_ENGINE_CATCH(what)
  catch (const std::exception& e) {
    char msg[256];
    std::snprintf(msg, sizeof(msg), "%s: exception: %s", what, e.what());
    umd_log(msg);
  } catch (...) {
    char msg[160];
    std::snprintf(msg, sizeof(msg), "%s: unknown exception", what);
    umd_log(msg);
  }
  return on_error;
}

}  // namespace helios_bridge
