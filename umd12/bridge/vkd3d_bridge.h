// C++ surface of the Helios D3D12 UMD <-> vkd3d engine bridge. Included by both
// the cxx-generated glue (`umd12/src/bridge12.rs`) and `vkd3d_bridge.cpp`.
//
// Same shape as `umd/bridge/dxvk_bridge.h:1-30`, and for the same reason: cxx's
// generated glue manages `std::unique_ptr<HeliosVkd3dDevice>`, so
// `HeliosVkd3dDevice` must be a COMPLETE type here — while the engine's own
// headers must not be. Pimpl is what buys both: this file is a thin shell
// holding a `unique_ptr` to an opaque `Impl` that owns the `ID3D12Device*`, and
// `~HeliosVkd3dDevice()` is DECLARED here and DEFINED out-of-line in the `.cpp`
// where `Impl` is complete. Declaring it here is not decoration — without it the
// compiler emits the destructor inline in every translation unit that includes
// this header, and `unique_ptr<Impl>`'s deleter then requires a complete `Impl`.
//
// ⛔ No engine, Vulkan, COM or D3D12 header may appear in this file. The
// D3D12 device pointer therefore crosses as a `std::size_t`, not an
// `ID3D12Device*`. Two independent reasons, both hard:
//   * `vkd3d-proton-helios/include/vkd3d.h:43-49` drags in `vulkan.h` plus
//     vkd3d's own widl-generated `D3D12_*` types, which then collide with the
//     Windows SDK's. The `D12-G1` static gate proved the SDK `<d3d12.h>` alone
//     is sufficient to link and run against the archive
//     (`tmp/dx12/gates/G1-static/RESULT.md`), so no vkd3d header is ever needed
//     on this side of the seam.
//   * cxx's `usize` maps to `std::size_t`; a COM pointer type in the bridge
//     signature would need a matching opaque type on the Rust side for no gain.
#pragma once

#include <cstdint>
#include <memory>

// Owns the `ID3D12Device*`; defined in vkd3d_bridge.cpp.
struct HeliosVkd3dDeviceImpl;

struct HeliosVkd3dDevice {
  HeliosVkd3dDevice() noexcept;
  ~HeliosVkd3dDevice();
  HeliosVkd3dDevice(const HeliosVkd3dDevice&) = delete;
  HeliosVkd3dDevice& operator=(const HeliosVkd3dDevice&) = delete;

  std::unique_ptr<HeliosVkd3dDeviceImpl> impl;

  // BORROWED — the bridge keeps the owning reference. 0 if not created.
  // The caller must NOT `Release()` this, and on the Rust side must not let a
  // `windows::ID3D12Device` own it (that is a double release at drop).
  //
  // ⚠ There is deliberately NO owned counterpart at S4. Nothing yet takes a
  // reference out of the bridge, and an uncalled accessor would have to carry
  // `#[allow(dead_code)]` on the Rust side, which `PARALLEL.md` §10's merge
  // checklist forbids on hand-written lines (R908). When S6 gains its first
  // owning caller it adds `std::size_t d3d12_device_addref() const noexcept`
  // here — `AddRef` then return the pointer, 0 and a named counter if absent —
  // paired with exactly one `from_raw` on the Rust side. `bridge12.rs`'s module
  // doc carries the same note so neither half is re-derived alone.
  std::size_t d3d12_device_ptr() const noexcept;

  // S4b. The venus context id this device's `VkInstance` belongs to, captured
  // at create time ON THE CREATING THREAD (the ICD's export is thread-local).
  // 0 when the ICD is absent or too old to export it — which is a degraded
  // read, not a failure; an anchor MISMATCH refuses device creation outright
  // and no `HeliosVkd3dDevice` exists to ask.
  std::uint32_t venus_context_id() const noexcept;
};

// ⛔ NOT named `helios_vkd3d_create_device`: that C symbol is DEFINED in the
// statically linked engine archive (`vkd3d-proton-helios/libs/d3d12core/
// helios_entry.c:112`) and, under D4-static, that archive is in this very link.
// Naming the bridge entry point the same thing would be a duplicate-symbol
// error at best and — because the engine symbol is `extern "C"` while this one
// is C++-mangled — a silently different function at worst.
std::unique_ptr<HeliosVkd3dDevice> helios_vkd3d_bridge_create_device(
    std::uint32_t luid_low, std::int32_t luid_high);

// Stateless forward to the engine's second entry point (`helios_entry.c:190`).
// `desc` is a `const D3D12_ROOT_SIGNATURE_DESC*` carried as an integer for the
// same header-isolation reason as above; `blob_out`/`err_out` receive OWNED
// `ID3DBlob*` values that the caller releases. Returns the engine's HRESULT.
// Never throws: the body runs inside the shared `bridge_guard`.
std::int32_t helios_vkd3d_bridge_serialize_root_signature(
    std::size_t desc, std::uint32_t version,
    std::size_t* blob_out, std::size_t* err_out) noexcept;

// K-F1 (`docs/dx12/KMD_IMPACT.md` §14a.2). Drain one `ID3D12CommandQueue`'s vkd3d
// submission worker, and — while the queue is still held — sample the venus wire
// fence that retires at host GPU completion of everything now submitted to it.
//
// ⭐ The drain is a CPU-side wait for SUBMISSION, NOT for GPU completion. That
// distinction is the whole reason this call is permitted where
// `FENCE-BRIDGE-DESIGN.md`'s design A is rejected: it costs no CPU/GPU overlap.
// The fence sample is not a wait at all — it reads a boundary and returns.
//
// `queue` is an `ID3D12CommandQueue*` carried as an integer, for the same
// header-isolation reason as `d3d12_device_ptr` above. BORROWED — no reference is
// taken and none is released. Returns false and counts if `queue` is 0 or if the
// engine declined to hand over the Vulkan queue. Never throws: `bridge_guard`.
//
// `out_wire_fence` / `out_fence_status`: pass **both or neither**. Non-null asks
// for the fence and both are always written before this returns. A **0 fence is a
// legal outcome**, not an error — it is `HeliosD3D12SubmitCmd`'s documented "order
// against nothing" arm — so the status is what says WHY, and the four values are
// four different findings that must not share a counter:
//
//   0 `HELIOS_VKD3D_FENCE_SAMPLED`    — a non-zero fence; the boundary is real
//   1 `HELIOS_VKD3D_FENCE_NO_ICD`     — no venus ICD module in this process, or the
//                                       S4b anchor refused (two ICD images live)
//   2 `HELIOS_VKD3D_FENCE_NO_EXPORT`  — the module predates
//                                       `helios_venus_queue_gpu_fence`
//   3 `HELIOS_VKD3D_FENCE_REFUSED`    — the export ran and declined (ring 0, a
//                                       handle it could not decode, no ctx, ...)
//
// ⛔ Keep these values in sync with `bridge12.rs`'s `FenceStatus`, which maps them
// by number and counts an unknown value rather than assuming.
constexpr std::uint32_t HELIOS_VKD3D_FENCE_SAMPLED = 0;
constexpr std::uint32_t HELIOS_VKD3D_FENCE_NO_ICD = 1;
constexpr std::uint32_t HELIOS_VKD3D_FENCE_NO_EXPORT = 2;
constexpr std::uint32_t HELIOS_VKD3D_FENCE_REFUSED = 3;

bool helios_vkd3d_bridge_drain_queue(std::size_t queue,
                                     std::uint64_t* out_wire_fence,
                                     std::uint32_t* out_fence_status) noexcept;
