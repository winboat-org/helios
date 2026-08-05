// Engine-agnostic helpers shared by the Helios D3D11 and D3D12 bridges.
//
// Moved from `umd/bridge/dxvk_bridge.cpp:121-202` at stage S1
// (`DECISIONS.md` D3b). Nothing here knows about DXVK or vkd3d: it is a
// rate-limited telemetry accumulator, a QPC subtraction, and a scoped COM
// release.
//
// ⚠ Separate from `bridge_common.h` on purpose. That header is deliberately
// free of Windows/COM includes so `bridge_dxbc.cpp` can include it alone and
// "the DXBC container synthesis cannot touch the engine" stays a link-time
// fact. `qpc_elapsed_us` needs `LARGE_INTEGER`, so it cannot live there.
#pragma once

#include <windows.h>

#include <atomic>
#include <cstdint>
#include <optional>

namespace helios_bridge {

// The "atomic total + CAS max + count + log every Nth + reset max" idiom,
// open-coded at two telemetry sites with two different periods.
//
// `bridge_log_budget` (bridge_common.h) is deliberately a SEPARATE, smaller
// helper for the simpler count-and-rate-limit counters -- forcing the two
// shapes together would give both a worse API than either has now.
//
// Per-site extras stay at the sites: `present-gate:` also accumulates
// `s_gateTimeouts`, which it deliberately never resets, plus two failure
// counters. Each site keeps its own log key and format string, so before/after
// numbers stay directly comparable -- which is the whole reason those lines
// exist.
class PeriodicStat {
public:
  struct Sample {
    std::uint32_t n;
    std::uint64_t avg_us;
    std::uint64_t max_us;
  };

  // `period` must be a power of two: the two sites tested `(n & 31u) == 0`
  // and `(n & 127u) == 0`, and keeping the mask form keeps that cadence exact.
  explicit constexpr PeriodicStat(std::uint32_t period) : mask_(period - 1u) {}

  // Record one measurement. Returns the sample to log when this call lands on
  // the period boundary, resetting the running max exactly as both sites did.
  std::optional<Sample> record(std::uint64_t us) {
    total_us_.fetch_add(us, std::memory_order_relaxed);
    std::uint64_t prev_max = max_us_.load(std::memory_order_relaxed);
    while (us > prev_max &&
           !max_us_.compare_exchange_weak(prev_max, us, std::memory_order_relaxed)) {}
    const std::uint32_t n = count_.fetch_add(1, std::memory_order_relaxed) + 1;
    if ((n & mask_) != 0)
      return std::nullopt;
    const Sample sample{
      n,
      total_us_.load(std::memory_order_relaxed) / n,
      max_us_.load(std::memory_order_relaxed),
    };
    max_us_.store(0, std::memory_order_relaxed);
    return sample;
  }

private:
  const std::uint32_t mask_;
  std::atomic<std::uint64_t> total_us_{0};
  std::atomic<std::uint64_t> max_us_{0};
  std::atomic<std::uint32_t> count_{0};
};

// Microseconds between two QueryPerformanceCounter reads. Both telemetry
// sites spelled this division out.
inline std::uint64_t qpc_elapsed_us(const LARGE_INTEGER& freq,
                                    const LARGE_INTEGER& t0,
                                    const LARGE_INTEGER& t1) {
  return std::uint64_t(t1.QuadPart - t0.QuadPart) * 1000000ull
       / std::uint64_t(freq.QuadPart);
}

// Owns one COM reference until it is deliberately released. Non-copyable, so
// the reference cannot be duplicated into a second owner by accident.
//
// ⚠ Only `T::Release()` is required, so this header needs no COM include and
// works equally against DXVK's `ID3D11*` and vkd3d's `ID3D12*`.
template <typename T>
class ComRelease {
public:
  explicit ComRelease(T* ptr) : m_ptr(ptr) {}
  ComRelease(const ComRelease&) = delete;
  ComRelease& operator=(const ComRelease&) = delete;
  ~ComRelease() { reset(); }
  T* get() const { return m_ptr; }
  void reset() {
    if (m_ptr) {
      m_ptr->Release();
      m_ptr = nullptr;
    }
  }
private:
  T* m_ptr = nullptr;
};

}  // namespace helios_bridge
