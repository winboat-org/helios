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
#include "bridge_icd_anchor.h"

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

// ── the two UPSTREAM interop symbols K-F1 drains through ────────────────────
//
// `vkd3d.h:120-121` declares these as
//     VkQueue vkd3d_acquire_vk_queue(ID3D12CommandQueue *queue);
//     void    vkd3d_release_vk_queue(ID3D12CommandQueue *queue);
// inside its `extern "C" {` block (`vkd3d.h:57`, closed at the file's tail),
// with no calling-convention decoration. ⛔ That header cannot be included here
// (see the file banner: it drags in `vulkan.h` plus vkd3d's widl `D3D12_*` types,
// which collide with the SDK's), so the two are redeclared with `void*` standing
// in for `ID3D12CommandQueue*` and for `VkQueue`.
//
// ⚠ That substitution is ABI-identical, not a hope: both are dispatchable
// pointers, `extern "C"` means the symbol name carries no parameter types, and
// the Microsoft x64 convention passes either in RCX and returns either in RAX. ⛔
// It is also why no translation unit may ever include `vkd3d.h` *and* this file's
// declarations — two `extern "C"` declarations of one symbol with different
// parameter types is ill-formed, and the header rule above already forbids it.
//
// ⭐ These are UPSTREAM public interop API, so K-F1 needs no fork patch
// (`KMD_IMPACT.md` §14a.2). They are ordinary archive symbols out of
// `libs/vkd3d/command.c` (`:25555` and `:25591`), which is in this link already.
extern "C" void* vkd3d_acquire_vk_queue(void* queue);
extern "C" void vkd3d_release_vk_queue(void* queue);

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
std::atomic<std::uint32_t> g_vkd3dDrainBadArg{0};          // drain refused: queue == 0
std::atomic<std::uint32_t> g_vkd3dDrainNotAcquired{0};     // acquire returned no VkQueue
std::atomic<std::uint32_t> g_vkd3dQueueFenceZero{0};       // gpu-fence sample yielded 0

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
  // S4b. Read ON THE CREATING THREAD — see `read_venus_ctx_id_now` below.
  std::uint32_t venus_ctx_id = 0;

  ~HeliosVkd3dDeviceImpl() {
    if (d3d12) {
      d3d12->Release();
      d3d12 = nullptr;
    }
  }
};

namespace {

/// Resolve the process's canonical venus ICD and read its context id, **on the
/// calling thread**.
///
/// ⚠ The thread matters and is not a detail. `helios_venus_instance_ctx_id`
/// ignores its `VkInstance` argument and returns a `_Thread_local`
/// (`icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:639-644`), so an engine
/// that creates its instance on one thread and asks on another gets 0 or
/// another thread's answer. vkd3d creates its `VkInstance` on the calling
/// thread of `vkd3d_create_instance`, which is the thread inside
/// `helios_vkd3d_create_device` — so this is called from there, synchronously,
/// immediately after the device comes back. `ARCHITECTURE.md` §6.4; the D3D11
/// bridge draws the same line at `dxvk_bridge.cpp`'s
/// `read_instance_venus_context_id` right after `new DxvkInstance`.
///
/// Returns false only when the process ANCHOR refused — two venus ICD modules
/// in one process. A ctx id of 0 is not a refusal (an old ICD without the
/// export is a degraded read, not a coherence failure).
bool read_venus_ctx_id_now(std::uint32_t* out) {
  *out = 0;
  void* icd = helios_bridge::find_venus_icd_module();
  if (!icd) {
    umd_log("venus ctx id: no loaded module exports helios_venus_memory_alloc_info");
    return true;
  }
  // ⛔ THE S4b STEP. Both UMDs run this; whichever loaded first published, and
  // a disagreement means this process has two ICD builds live.
  void* canonical = helios_bridge::reconcile_icd_anchor(icd);
  if (!canonical) {
    return false;   // IcdAnchorMismatch already counted and logged
  }
  *out = helios_bridge::read_venus_ctx_id(canonical);
  return true;
}

// ── the venus per-queue GPU-completion fence, resolved ONCE ──────────────────
//
// `helios_venus_queue_gpu_fence(VkQueue, uint64_t*)` — `icd/mesa`'s
// `src/virtio/vulkan/vn_renderer_helios.c:1859`, exported by
// `__declspec(dllexport)` alone (no `.def` entry; the eleven existing
// `helios_venus_*` exports work the same way, proven by the live present-stream
// path). Returns a venus wire fence that retires at HOST GPU COMPLETION of
// everything already submitted to that `VkQueue`, or false with the fence left 0.
//
// ⚠ `void*` stands in for `VkQueue` here rather than including `vulkan.h`, which
// this file has never needed. `VkQueue` is a `VK_DEFINE_HANDLE` — a plain pointer
// — so under `__cdecl` on x64 the parameter is passed in RCX either way. Same
// substitution and same argument as the `vkd3d_acquire_vk_queue` declarations
// above, and it keeps this translation unit free of the Vulkan headers whose
// `D3D12_*`/vkd3d collisions the file banner is about.
// ⛔ The precedent this is modelled on types the handle concretely
// (`umd/bridge/bridge_icd_exports.cpp:542`, `bool (__cdecl*)(VkDevice,
// VkSemaphore, std::uint64_t*)`) because that TU already includes Vulkan. The
// deviation is the include set, not the ABI.
using QueueGpuFenceFn = bool(__cdecl*)(void* /*VkQueue*/, std::uint64_t*);

struct QueueGpuFenceExport {
  QueueGpuFenceFn fn = nullptr;
  std::uint32_t status = HELIOS_VKD3D_FENCE_NO_ICD;
};

/// Resolve the export **once per process**, from the S4b-anchored module.
///
/// ⛔ **Once, and that is a correctness requirement rather than an optimisation.**
/// `find_venus_icd_module()` takes a module reference on every successful call
/// (`GetModuleHandleExA` without `UNCHANGED_REFCOUNT`,
/// `umd_common/bridge/bridge_icd_anchor.cpp:143-146`) and nothing releases it. A
/// per-submit resolution would therefore leak one ICD module reference **per
/// `ExecuteCommandLists`** — the per-object-leak class the 54th session closed,
/// one API generation later and several thousand times faster.
///
/// ⛔ And it resolves through `reconcile_icd_anchor`, never from a bare
/// `find_venus_icd_module`: S4b's rule is one coherent ICD module per process, and
/// a mismatch must REFUSE rather than adopt the published one, because the Vulkan
/// objects this process holds came from whichever image the loader bound. The
/// mismatch path already counts and logs `IcdAnchorMismatch`.
///
/// ⚠ A magic static: C++11 guarantees the initialiser runs exactly once even under
/// concurrent first calls, and every DDI that reaches this is free-threaded.
const QueueGpuFenceExport& queue_gpu_fence_export() {
  static const QueueGpuFenceExport resolved = [] {
    QueueGpuFenceExport out;
    void* candidate = helios_bridge::find_venus_icd_module();
    if (!candidate) {
      umd_log("queue_gpu_fence: no loaded module exports the venus ICD probe "
              "symbol -- the D3D12 submission will carry a 0 boundary");
      out.status = HELIOS_VKD3D_FENCE_NO_ICD;
      return out;
    }
    void* canonical = helios_bridge::reconcile_icd_anchor(candidate);
    if (!canonical) {
      // IcdAnchorMismatch is already counted and logged by the anchor.
      umd_log("queue_gpu_fence: venus ICD anchor mismatch -- refusing to resolve "
              "the fence export; the submission will carry a 0 boundary");
      out.status = HELIOS_VKD3D_FENCE_NO_ICD;
      return out;
    }
    // ⚠ Through `void*` and back: a function pointer is not
    // `reinterpret_cast`-able from `FARPROC` directly without a
    // -Wcast-function-type diagnostic on clang-cl, and the two-step is the
    // conforming spelling.
    out.fn = reinterpret_cast<QueueGpuFenceFn>(reinterpret_cast<void*>(
        GetProcAddress(static_cast<HMODULE>(canonical),
                       "helios_venus_queue_gpu_fence")));
    if (!out.fn) {
      // ⛔ NOT fatal, and not even a refusal: an older ICD image that predates the
      // export is the designed graceful path. The submission still goes, carrying
      // the 0 boundary the record explicitly allows.
      umd_log("queue_gpu_fence: the anchored venus ICD does not export "
              "helios_venus_queue_gpu_fence (older ICD) -- the D3D12 submission "
              "will carry a 0 boundary");
      out.status = HELIOS_VKD3D_FENCE_NO_EXPORT;
      return out;
    }
    umd_log("queue_gpu_fence: resolved helios_venus_queue_gpu_fence from the "
            "anchored venus ICD");
    out.status = HELIOS_VKD3D_FENCE_SAMPLED;
    return out;
  }();
  return resolved;
}

}  // namespace

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

std::uint32_t HeliosVkd3dDevice::venus_context_id() const noexcept {
  // Already read, on the creating thread, at device-create time. ⛔ Do NOT
  // re-read it here: this accessor can be called from any thread and the ICD's
  // export is thread-local, so a lazy read would answer 0 or another thread's
  // context on every caller but the first.
  return impl ? impl->venus_ctx_id : 0;
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

        // ⛔ S4b: on THIS thread, before returning. The ICD's ctx-id export is
        // thread-local, and vkd3d created its `VkInstance` on this thread
        // inside the call above.
        std::uint32_t ctx = 0;
        if (!read_venus_ctx_id_now(&ctx)) {
          // The anchor refused: two venus ICD modules in one process. Refusing
          // device creation is `ARCHITECTURE.md` §6.4's stated behaviour, and
          // it is the right one — a device built here would hand foreign
          // handles across the two ICDs and fail much later, somewhere else.
          // `IcdAnchorMismatch` is already counted and logged.
          umd_log("REFUSING ID3D12Device: venus ICD anchor mismatch");
          return std::unique_ptr<HeliosVkd3dDevice>{};
        }
        out->impl->venus_ctx_id = ctx;

        char msg[192];
        std::snprintf(msg, sizeof(msg),
                      "ID3D12Device created OK on luid %08x:%08x venus ctx_id=%u "
                      "(static vkd3d engine)",
                      (unsigned)luid_high, (unsigned)luid_low, ctx);
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

bool helios_vkd3d_bridge_drain_queue(std::size_t queue,
                                     std::uint64_t* out_wire_fence,
                                     std::uint32_t* out_fence_status) noexcept {
  // ⛔ Cleared FIRST, before anything that can fail or throw. A caller that reads
  // an untouched pair after a false return would otherwise read stack garbage as a
  // GPU boundary — and a garbage fence is the one input the KMD clamps but cannot
  // reject (`HeliosD3D12SubmitCmd`'s "safety of a guest-supplied fence" note).
  if (out_wire_fence) *out_wire_fence = 0;
  if (out_fence_status) *out_fence_status = HELIOS_VKD3D_FENCE_NO_ICD;

  // ⚠ `false` as the sentinel, and its type is `bool` at a glance — the
  // `ead692e` rule from the two entry points above: `bridge_guard` deduces `R`
  // from the ERROR VALUE ALONE, and the `static_assert` in
  // `umd_common/bridge/bridge_guard.h` is what makes a mismatch a compile error.
  return helios_bridge::bridge_guard(
      "helios_vkd3d_bridge_drain_queue", false, [&]() -> bool {
        if (!queue) {
          const std::uint32_t n =
              helios_bridge::g_vkd3dDrainBadArg.fetch_add(
                  1, std::memory_order_relaxed) + 1;
          char msg[128];
          std::snprintf(msg, sizeof(msg),
                        "drain_queue refused: queue=0 (Vkd3dDrainBadArg=%u)", n);
          umd_log(msg);
          return false;
        }
        void* q = reinterpret_cast<void*>(queue);

        // ⭐⭐ ACQUIRE + RELEASE AROUND NOTHING **IS** THE DRAIN, and this is not
        // a trick: it is verbatim what upstream vkd3d does for the same purpose.
        // `libs/vkd3d/swapchain.c:490-498`,
        // `dxgi_vk_swap_chain_drain_queue`, with its own comment —
        //
        //     /* This functions as a DRAIN of the D3D12 queue.
        //      * All CPU operations that were queued must have been submitted to
        //      * Vulkan now. */
        //     if (vkd3d_acquire_vk_queue(&chain->queue->ID3D12CommandQueue_iface))
        //         vkd3d_release_vk_queue(&chain->queue->ID3D12CommandQueue_iface);
        //
        // — including the `if`, which is why the null test below exists rather
        // than an unconditional pair.
        //
        // ⚠ **What the null arm actually means, since it is not obvious and it is
        // not harmless.** `vkd3d_queue_acquire` returns `VK_NULL_HANDLE` only when
        // `pthread_mutex_lock` on the vkd3d_queue itself FAILS
        // (`command.c:333-347`; the `vk_queue` is `assert`ed non-null). By then
        // `d3d12_command_queue_acquire_serialized` has already taken `queue_lock`
        // and the skipped `release` never drops it — so that arm leaks a lock and
        // the next submission on this queue would block forever. ⛔ It is
        // nevertheless what upstream does, verbatim, and this bridge deliberately
        // does not "improve" on it: releasing after a failed acquire would unlock a
        // mutex this thread does not hold, which is worse. A pthread mutex lock
        // failing here is a corrupted-mutex / EDEADLK class of event; the counter
        // and the log line are what make it visible instead of a mystery hang.
        //
        // What acquire does: `d3d12_command_queue_acquire_serialized`
        // (`libs/vkd3d/command.c:25202-25218`) pushes a `VKD3D_SUBMISSION_DRAIN`
        // marker and `pthread_cond_wait`s until the worker thread's
        // `queue_drain_count` catches up — i.e. until the worker has SUBMITTED
        // everything enqueued before this call. It then takes the vkd3d_queue
        // mutex and returns the `VkQueue`, leaving BOTH locks held.
        //
        // ⚠ RELEASE IS NOT FREE, and callers must know it: `vkd3d_release_vk_queue`
        // (`command.c:25591-25620`) issues a real `vkQueueSubmit2` of an empty
        // batch that signals the queue's `submission_timeline` and bumps
        // `last_submission_timeline_value`, before dropping both locks. That is
        // the documented interop contract — *"Need to increment the submission
        // counter here so that fence signals and waits behave as expected in an
        // interop scenario"* — so it is correct, not incidental, but it does mean
        // one extra empty `vkQueueSubmit2` per drain. ⛔ Do NOT try to avoid it
        // with `vkd3d_unlock_vk_queue`: that releases only the vkd3d_queue mutex
        // and would leave `queue_lock` held forever.
        void* vk_queue = vkd3d_acquire_vk_queue(q);
        if (!vk_queue) {
          const std::uint32_t n =
              helios_bridge::g_vkd3dDrainNotAcquired.fetch_add(
                  1, std::memory_order_relaxed) + 1;
          char msg[160];
          std::snprintf(msg, sizeof(msg),
                        "drain_queue: acquire_vk_queue(%p) returned no VkQueue "
                        "(Vkd3dDrainNotAcquired=%u)", q, n);
          umd_log(msg);
          return false;
        }

        // ⛔⛔ THE FENCE IS SAMPLED **HERE**, AND THE POSITION IS THE WHOLE
        // CORRECTNESS OF IT. Two properties, both of which only this line's
        // placement can provide — nothing inside the ICD export can check either:
        //
        //   * **After the drain.** The `VKD3D_SUBMISSION_DRAIN` above has already
        //     completed, so every `vkQueueSubmit` for the work this packet is meant
        //     to cover has reached the host driver and is inside the boundary the
        //     export reports. Reading a LARGER ring seqno than needed is harmless —
        //     it over-orders. Reading a **stale smaller** one is the only way to get
        //     a fence covering less work than the caller believes, and that is
        //     exactly what sampling before the drain would produce.
        //   * **While the queue is still held.** Both of vkd3d's locks are ours
        //     until the release below, so no other thread can push a submission in
        //     between and make the boundary describe a moving target.
        //
        // ⚠ **The release's OWN submit is deliberately not covered**, and a reader
        // who spots that gap must not "fix" it. `vkd3d_release_vk_queue` issues an
        // empty `vkQueueSubmit2` signalling vkd3d's internal `submission_timeline`
        // (see the note above it): that batch carries no application work, so a
        // boundary excluding it is correct — and moving the sample after the release
        // would read the queue with no lock held, racing the very submissions the
        // drain just serialised.
        if (out_wire_fence && out_fence_status) {
          const QueueGpuFenceExport& e = queue_gpu_fence_export();
          if (!e.fn) {
            // NO_ICD or NO_EXPORT, decided and logged once at resolution time.
            *out_fence_status = e.status;
          } else if (!e.fn(vk_queue, out_wire_fence) || *out_wire_fence == 0) {
            // ⛔ `|| == 0` as well as the bool: the export documents that it always
            // writes the out-param and leaves 0 on every refusal, so a `true` with a
            // 0 fence would be a contract break — and treating it as a sample would
            // put a "no boundary" record on the wire labelled as a real one.
            *out_wire_fence = 0;
            *out_fence_status = HELIOS_VKD3D_FENCE_REFUSED;
            const std::uint32_t n =
                helios_bridge::g_vkd3dQueueFenceZero.fetch_add(
                    1, std::memory_order_relaxed) + 1;
            if (n <= 8 || (n % 4096) == 0) {
              char msg[192];
              std::snprintf(msg, sizeof(msg),
                            "queue_gpu_fence(%p) declined -- submitting a 0 "
                            "boundary (Vkd3dQueueFenceZero=%u)", vk_queue, n);
              umd_log(msg);
            }
          } else {
            *out_fence_status = HELIOS_VKD3D_FENCE_SAMPLED;
          }
        }

        // ⛔ ONE release, on every path that acquired. The sample above adds no
        // early return by construction — it cannot `return`, only write its
        // out-params — because skipping this call leaves BOTH of vkd3d's locks held
        // and the next submission on this queue blocks forever. (The null-acquire arm
        // above already leaks `queue_lock`; see its note. That is upstream's shape
        // and this is not a second instance of it.)
        vkd3d_release_vk_queue(q);
        return true;
      });
}
