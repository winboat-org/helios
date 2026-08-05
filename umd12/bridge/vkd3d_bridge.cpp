// Helios D3D12 UMD <-> vkd3d engine bridge implementation.
//
// Wraps the one `ID3D12Device*` the engine hands back behind the opaque
// `HeliosVkd3dDevice`. Structurally this is `umd/bridge/dxvk_bridge.cpp`'s
// create-device path (:1640-1704) with DXVK swapped for vkd3d, which is exactly
// what `DECISIONS.md` D4 says the D3D12 UMD is — but it shares no *code* with
// it: D3b forbids copying from `umd/`, so everything both bridges need already
// lives in `umd_common/bridge/` and is included, not duplicated.
//
// ⭐ The engine is STATICALLY LINKED (D4, flipped by the owner 2026-08-05).
// There is no `helios_vkd3d.dll`, no `LoadLibrary`, no `GetProcAddress` and no
// module pin anywhere in this file. `helios_vkd3d_create_device` and
// `helios_vkd3d_serialize_root_signature` are ARCHIVE symbols pulled out of
// `libhelios_d3d12_static.a` by the linker, declared below and called directly.
// The measured link set is that ONE archive plus `gdi32`
// (`tmp/dx12/gates/G1-static/RESULT.md`) — ⛔ never `dxgi`: a WDDM user-mode
// driver sits BELOW DXGI, and loading dxgi during device creation risks
// re-entering the adapter enumeration that loaded this very DLL
// (`vkd3d-proton-helios/libs/d3d12core/helios_entry.c:19-28`).
//
// ⛔ `HELIOS_BRIDGE_ENGINE_CATCH` is deliberately NOT defined here. It is
// `bridge_guard.h`'s one customization point and exists for `dxvk::DxvkError`,
// which is not a `std::exception`; vkd3d is a C library behind a COM ABI and
// throws nothing, so the generic arms are the whole story.

// ⚠ Guarded, not bare `#define`s as `dxvk_bridge.cpp:8-9` has them: `build.rs`
// passes both on the clang-cl command line for this crate, and a redefinition
// with a different replacement list (`-DNOMINMAX` is `1`, a bare `#define` is
// empty) is a diagnostic. Kept at all so the file still compiles standalone,
// which is how it was syntax-checked on the Linux host.
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>

#include <d3d12.h>

#include "vkd3d_bridge.h"

// ⚠ These two resolve to `umd_common/bridge/`, not to this directory —
// `build.rs` adds it to the include path after `bridge/`, so a same-named header
// in this crate would win (`DECISIONS.md` D3b: one copy of the source, shared
// with the D3D11 bridge).
#include "bridge_common.h"
#include "bridge_guard.h"

#include <atomic>
#include <cstdio>
#include <cstdlib>
#include <mutex>
#include <share.h>

// ── the two engine entry points ─────────────────────────────────────────────
//
// Declared here rather than by including a vkd3d header: `vkd3d.h:43-49` pulls
// in `vulkan.h` and vkd3d's widl `D3D12_*` types, which collide with the SDK's.
// Definitions: `vkd3d-proton-helios/libs/d3d12core/helios_entry.c:112` and
// `:190`. Both are `extern "C"`; a C++-mangled declaration would link against
// nothing and the failure would be a link error, not a runtime one — but only
// if nothing else in the link happens to define the mangled name, so the
// `extern "C"` here is load-bearing, not decorative.
extern "C" HRESULT helios_vkd3d_create_device(LUID adapter_luid, REFIID iid,
                                              void** device);
extern "C" HRESULT helios_vkd3d_serialize_root_signature(
    const D3D12_ROOT_SIGNATURE_DESC* desc, D3D_ROOT_SIGNATURE_VERSION version,
    ID3DBlob** blob, ID3DBlob** error_blob);

namespace helios_bridge {

// ── named counters ──────────────────────────────────────────────────────────
//
// CLAUDE.md operating rule 2: every skipped or refused path gets a named
// counter, so "it silently did nothing" is never a possible reading of a log.
// These are process-local atomics rather than registry counters because this is
// user mode and the log is per-process and per-pid anyway; each is also logged
// at the moment it increments, with its running value.
std::atomic<std::uint32_t> g_vkd3dCreateDeviceFailed{0};   // engine returned a failure HRESULT
std::atomic<std::uint32_t> g_vkd3dCreateDeviceNullOut{0};  // engine returned S_OK with a null device
std::atomic<std::uint32_t> g_vkd3dSerializeBadArg{0};      // serialize refused: null desc/blob_out

// ── this DLL's `umd_log` ────────────────────────────────────────────────────
//
// ⛔ `bridge_common.h:35` declares `umd_log` and every bridge DEFINES its own.
// `helios_umd.dll` writes `umd-<pid>.log` with a `[dxvk-bridge] ` prefix; this
// one writes `umd12-<pid>.log` with `[vkd3d-bridge] `. Two drivers appending to
// one file would interleave unreadably and would break the per-module evidence
// discipline that reads them.
//
// Magic static, and a fixed `char[]` rather than the `std::string` the D3D11
// bridge's version holds (`dxvk_bridge.cpp:190-200`): this function is called
// from `bridge_guard`'s `catch (const std::exception&)` arm, which is reachable
// on `std::bad_alloc`, and an allocation inside a bad_alloc handler can throw
// again. Same rule as the guard's own arms.
static const char* umd_log_file() {
  struct LogPath { char buf[MAX_PATH]; };
  static const LogPath path = [] {
    LogPath p{};
    CreateDirectoryA("C:\\ProgramData\\Helios", nullptr);
    _snprintf_s(p.buf, sizeof(p.buf), _TRUNCATE,
                "C:\\ProgramData\\Helios\\umd12-%lu.log",
                (unsigned long)GetCurrentProcessId());
    return p;
  }();
  return path.buf;
}

void umd_log(const char* msg) {
  // ⛔ `_fsopen` with `_SH_DENYNO`, NEVER `fopen_s`. `fopen_s` opens
  // `_SH_SECURE` (deny-sharing); the Rust side of this same DLL holds a
  // persistent handle to this very file (`helios_umd_common::log`), so every
  // `fopen_s` here would fail and ALL bridge logging would silently vanish.
  // That already happened once on the D3D11 side — found in the 18th session,
  // where the DriverStore UMD contained the strings and the logs contained no
  // `[dxvk-bridge] ` lines at all (`dxvk_bridge.cpp:202-214`).
  FILE* f = _fsopen(umd_log_file(), "a", _SH_DENYNO);
  if (f) {
    fprintf(f, "[vkd3d-bridge] %s\n", msg);
    fclose(f);
  }
}

}  // namespace helios_bridge

using helios_bridge::umd_log;

// ── the pimpl ───────────────────────────────────────────────────────────────
//
// ⛔ No `FreeLibrary` and no module pin, unlike the shape a DLL-hosted engine
// would need: under D4-static the engine's code is in `helios_umd12.dll`'s own
// image, so there is no module whose lifetime could end under a live device.
struct HeliosVkd3dDeviceImpl {
  ID3D12Device* d3d12 = nullptr;

  ~HeliosVkd3dDeviceImpl() {
    if (d3d12) {
      d3d12->Release();
      d3d12 = nullptr;
    }
  }
};

HeliosVkd3dDevice::HeliosVkd3dDevice() noexcept = default;

// Defined out-of-line here, where `HeliosVkd3dDeviceImpl` is complete — that is
// the whole point of the declaration in the header.
HeliosVkd3dDevice::~HeliosVkd3dDevice() = default;

std::size_t HeliosVkd3dDevice::d3d12_device_ptr() const noexcept {
  // BORROWED. No `AddRef`: the caller is looking at the reference this object
  // owns and must not release it. ⚠ Adopting this on the Rust side into an
  // owning `ID3D12Device` is a double release at drop, and the crash lands
  // nowhere near here.
  return impl ? reinterpret_cast<std::size_t>(impl->d3d12) : 0;
}

std::unique_ptr<HeliosVkd3dDevice> helios_vkd3d_bridge_create_device(
    std::uint32_t luid_low, std::int32_t luid_high) {
  // ── process-global env configuration, exactly once ────────────────────────
  //
  // `std::call_once` and not "do it on every create": `_putenv_s` is not safe
  // against a concurrent `getenv`, and one process makes several devices. That
  // is R824 on the D3D11 side (`dxvk_bridge.cpp:1600-1612`) — the same hazard
  // reached the same way, not shared code.
  static std::once_flag s_envOnce;
  std::call_once(s_envOnce, [] {
    // ⭐ Set `VKD3D_LOG_FILE` only if it is ABSENT. vkd3d defaults its log to
    // stderr, which is a black hole in `dwm.exe` — so an unset variable must be
    // given a real file. But a gate script that already set one must WIN: if
    // this overwrote it, the gate's log would move out from under the script
    // that is reading it, and the run would look silent.
    //
    // ⛔ `VKD3D_DEBUG` and `VKD3D_CONFIG` are deliberately NOT set here. They
    // change engine behaviour and verbosity; a driver that pins them removes
    // the operator's only lever and makes every measurement configuration-blind.
    const char* existing = std::getenv("VKD3D_LOG_FILE");
    char chosen[MAX_PATH] = {};
    if (existing && existing[0]) {
      _snprintf_s(chosen, sizeof(chosen), _TRUNCATE, "%s", existing);
    } else {
      CreateDirectoryA("C:\\ProgramData\\Helios", nullptr);
      _snprintf_s(chosen, sizeof(chosen), _TRUNCATE,
                  "C:\\ProgramData\\Helios\\umd12-%lu-vkd3d.log",
                  (unsigned long)GetCurrentProcessId());
      _putenv_s("VKD3D_LOG_FILE", chosen);
    }

    char msg[MAX_PATH + 96];
    std::snprintf(msg, sizeof(msg),
                  "vkd3d env configured once: VKD3D_LOG_FILE=%s (%s)", chosen,
                  (existing && existing[0]) ? "pre-set, kept" : "set by bridge");
    umd_log(msg);
  });

  // ⚠ The sentinel is written with its explicit type. `bridge_guard` deduces
  // `R` from the ERROR VALUE ALONE — the body's return type is not a deduction
  // context — so a sentinel of the wrong type silently decides what every
  // SUCCESS value is converted to on the way out. Commit `ead692e`: a bare `0`
  // against a `std::size_t` body deduced `R = int` and truncated every returned
  // pointer to 32 bits; dwm and LogonUI crash-looped at cold boot and nothing
  // warned. The `static_assert` in `umd_common/bridge/bridge_guard.h` is the
  // fix — ⛔ never defeat it with a cast; if it fires, the sentinel is wrong.
  // (Deliberately no line number: it is the only `static_assert` in any bridge
  // directory, so `grep` finds it and a pointer cannot go stale. This comment
  // cited `:94` for about an hour, until the same commit's edit to that header
  // pushed the assert to `:103`.)
  return helios_bridge::bridge_guard(
      "helios_vkd3d_bridge_create_device", std::unique_ptr<HeliosVkd3dDevice>{},
      [&]() -> std::unique_ptr<HeliosVkd3dDevice> {
        auto out = std::make_unique<HeliosVkd3dDevice>();
        out->impl = std::make_unique<HeliosVkd3dDeviceImpl>();

        LUID luid;
        // `LUID` is `{ DWORD LowPart; LONG HighPart; }` — unsigned low, signed
        // high. The two halves are carried separately across the cxx seam
        // because cxx has no `LUID`, and this is the one place their order is
        // reassembled.
        luid.LowPart = luid_low;
        luid.HighPart = luid_high;

        // ⛔ `__uuidof(ID3D12Device)`, not `IID_ID3D12Device`: the latter is a
        // `dxguid.lib` symbol, and the measured link set is one archive plus
        // `gdi32` and nothing else (`G1-static/RESULT.md`).
        ID3D12Device* dev = nullptr;
        const HRESULT hr = helios_vkd3d_create_device(
            luid, __uuidof(ID3D12Device), reinterpret_cast<void**>(&dev));
        if (FAILED(hr)) {
          const std::uint32_t n =
              helios_bridge::g_vkd3dCreateDeviceFailed.fetch_add(
                  1, std::memory_order_relaxed) + 1;
          char msg[192];
          std::snprintf(msg, sizeof(msg),
                        "helios_vkd3d_create_device(luid %08x:%08x) failed hr=0x%08lx "
                        "(Vkd3dCreateDeviceFailed=%u)",
                        (unsigned)luid_high, (unsigned)luid_low,
                        (unsigned long)hr, n);
          umd_log(msg);
          return std::unique_ptr<HeliosVkd3dDevice>{};
        }
        if (!dev) {
          // S_OK with a null out-pointer is an engine contract violation, not a
          // failure we can act on — but it is exactly the shape that would
          // otherwise become a null deref two layers up, so it is counted and
          // refused here.
          const std::uint32_t n =
              helios_bridge::g_vkd3dCreateDeviceNullOut.fetch_add(
                  1, std::memory_order_relaxed) + 1;
          char msg[160];
          std::snprintf(msg, sizeof(msg),
                        "helios_vkd3d_create_device returned S_OK with a null device "
                        "(Vkd3dCreateDeviceNullOut=%u)", n);
          umd_log(msg);
          return std::unique_ptr<HeliosVkd3dDevice>{};
        }

        // `Impl` now owns the one reference; its destructor releases it, on
        // every exit path including an exception unwinding out of the guard.
        out->impl->d3d12 = dev;

        char msg[160];
        std::snprintf(msg, sizeof(msg),
                      "ID3D12Device created OK on luid %08x:%08x (static vkd3d engine)",
                      (unsigned)luid_high, (unsigned)luid_low);
        umd_log(msg);
        return out;
      });
}

std::int32_t helios_vkd3d_bridge_serialize_root_signature(
    std::size_t desc, std::uint32_t version,
    std::size_t* blob_out, std::size_t* err_out) noexcept {
  // ⚠ `std::int32_t(0x80004005)` — `E_FAIL` spelled with its explicit type, not
  // a bare literal. Same `ead692e` class as the create path above: the sentinel
  // alone deduces `R`. Written as the constant rather than `E_FAIL` so the
  // type is visible at a glance; `E_FAIL` is `_HRESULT_TYPEDEF_(0x80004005L)`.
  return helios_bridge::bridge_guard(
      "helios_vkd3d_bridge_serialize_root_signature", std::int32_t(0x80004005),
      [&]() -> std::int32_t {
        if (!desc || !blob_out) {
          const std::uint32_t n =
              helios_bridge::g_vkd3dSerializeBadArg.fetch_add(
                  1, std::memory_order_relaxed) + 1;
          char msg[160];
          std::snprintf(msg, sizeof(msg),
                        "serialize_root_signature refused: desc=%p blob_out=%p "
                        "(Vkd3dSerializeBadArg=%u)",
                        (void*)desc, (void*)blob_out, n);
          umd_log(msg);
          // E_INVALIDARG — the caller's argument is wrong, which is a distinct
          // report from the guard's E_FAIL sentinel.
          return std::int32_t(0x80070057);
        }

        // Zero both outs before the call: the engine writes them on success,
        // and a caller that reads an untouched `err_out` after a failure would
        // otherwise release stack garbage.
        *blob_out = 0;
        if (err_out) *err_out = 0;

        ID3DBlob* blob = nullptr;
        ID3DBlob* err = nullptr;
        const HRESULT hr = helios_vkd3d_serialize_root_signature(
            reinterpret_cast<const D3D12_ROOT_SIGNATURE_DESC*>(desc),
            static_cast<D3D_ROOT_SIGNATURE_VERSION>(version), &blob, &err);

        // Both blobs are OWNED by the caller from here on. `err` is handed back
        // even on success (the engine may emit warnings) and is dropped if the
        // caller did not ask for it — releasing it rather than leaking, because
        // this path can run per-PSO.
        *blob_out = reinterpret_cast<std::size_t>(blob);
        if (err_out) {
          *err_out = reinterpret_cast<std::size_t>(err);
        } else if (err) {
          err->Release();
        }
        return static_cast<std::int32_t>(hr);
      });
}
