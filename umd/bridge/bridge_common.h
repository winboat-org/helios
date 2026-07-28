// Declarations every bridge translation unit needs, and nothing else.
//
// T8/R1105 split `dxvk_bridge.cpp` into translation units. The symbols the
// halves share had to leave their anonymous namespaces to be linkable, so they
// now live in a named one; this header is the smallest of the three and is the
// only one `bridge_dxbc.cpp` needs besides its own.
//
// ⚠ Deliberately free of DXVK, COM, Vulkan and WDK includes. That is the whole
// point: `bridge_dxbc.cpp` includes this and `bridge_dxbc.h` and nothing else,
// which makes "the DXBC container synthesis cannot touch the DXVK device or the
// immediate context" a link-time fact rather than a review convention.
#pragma once

#include <atomic>
#include <cstdint>

namespace helios_bridge {

/// Append one line to `C:\ProgramData\Helios\umd-<pid>.log`, prefixed
/// `[dxvk-bridge] `. Defined in `dxvk_bridge.cpp`.
void umd_log(const char* msg);

/// First `first` occurrences, then every `every`-th. The rate-limit idiom the
/// periodic bridge telemetry uses. Defined in `dxvk_bridge.cpp`.
bool bridge_log_budget(std::atomic<std::uint32_t>& counter,
                       std::uint32_t first,
                       std::uint32_t every);

}  // namespace helios_bridge
