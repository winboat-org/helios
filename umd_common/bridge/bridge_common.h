// Declarations every bridge translation unit needs, and nothing else.
//
// Moved from `umd/bridge/bridge_common.h` at stage S1 (`DECISIONS.md` D3b:
// *"if a D3D12 file would contain code that also exists in umd/, that code
// moves to umd_common first and both crates depend on it"*). `helios_umd.dll`'s
// DXVK bridge and `helios_umd12.dll`'s vkd3d bridge include the same file.
//
// T8/R1105 split `dxvk_bridge.cpp` into translation units. The symbols the
// halves share had to leave their anonymous namespaces to be linkable, so they
// now live in a named one; this header is the smallest of the three and is the
// only one `bridge_dxbc.cpp` needs besides its own.
//
// ⚠ Deliberately free of DXVK, vkd3d, COM, Vulkan and WDK includes. That is the
// whole point: `bridge_dxbc.cpp` includes this and `bridge_dxbc.h` and nothing
// else, which makes "the DXBC container synthesis cannot touch the DXVK device
// or the immediate context" a link-time fact rather than a review convention.
// ⛔ Anything needing `LARGE_INTEGER`, COM or an engine type belongs in
// `bridge_util.h` or `bridge_guard.h`, not here.
#pragma once

#include <atomic>
#include <cstdint>

namespace helios_bridge {

/// Append one line to the module's own per-process log, prefixed with the
/// bridge's tag.
///
/// ⛔ **Declared here, defined PER BRIDGE, and that is the point.** The D3D11
/// bridge writes `C:\ProgramData\Helios\umd-<pid>.log` with a `[dxvk-bridge] `
/// prefix; a D3D12 bridge writes `umd12-<pid>.log`. Two drivers appending to
/// one file would interleave unreadably and would break the id-1000 / knob
/// evidence discipline that reads them per module. The Rust side draws the same
/// line — `log::init(basename)` (stage S2).
void umd_log(const char* msg);

/// First `first` occurrences, then every `every`-th. The rate-limit idiom the
/// periodic bridge telemetry uses.
///
/// `inline` here rather than out-of-line in one bridge's `.cpp`: it is four
/// lines of pure arithmetic on a caller-owned atomic, and leaving the
/// definition behind would have made the second bridge either duplicate it or
/// link against the first, both of which D3b forbids.
inline bool bridge_log_budget(std::atomic<std::uint32_t>& counter,
                              std::uint32_t first,
                              std::uint32_t every) {
  const std::uint32_t n = counter.fetch_add(1, std::memory_order_relaxed) + 1;
  return n <= first || (every != 0 && (n % every) == 0);
}

}  // namespace helios_bridge
