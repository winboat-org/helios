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

  // UP-5. The **instance-scoped** venus context id, `helios_venus_instance_ctx_id`,
  // also captured at create time on the creating thread.
  //
  // ⛔ This is the one that may be STAMPED into an allocation identity, and
  // `venus_context_id()` above is not — `bridge_icd_anchor.h` says so outright
  // (*"evidence only. Never stamp an identity with this value"*): the
  // process-global static it reads is last-writer-wins across `VkInstance`
  // creations, so a concurrent instance create can replace it in exactly the
  // window between vkd3d's own create and this read. The instance-scoped export
  // returns a `_Thread_local` written by THIS thread's own CTX_CREATE
  // (`vn_renderer_helios.c:648`), which no other thread can touch.
  //
  // ⚠ Both are captured because a DISAGREEMENT is a finding — it means another
  // instance was created concurrently — and one field could not show one. 0 when
  // the ICD is absent or predates the export.
  std::uint32_t venus_instance_context_id() const noexcept;

  // UP-2c/UP-5. The venus identity of the memory an `ID3D12Resource` is bound to.
  //
  // `resource` is an `ID3D12Resource*` carried as an integer for the header
  // isolation reason at the top of this file. BORROWED: no reference is taken and
  // none is released.
  //
  // Two engines answer in sequence and both must, which is why there is one call
  // and not two: `ID3D12DXVKInteropDevice4::GetVulkanResourceMemoryInfo` gives the
  // `{VkDeviceMemory, offset, size, memory type}` the resource's image or buffer is
  // BOUND to, and the anchored venus ICD then turns that `VkDeviceMemory` into the
  // `{res_id, allocationSize, memoryTypeIndex}` a WDDM allocation adopts. A caller
  // that made those two calls itself would have to carry the `VkDeviceMemory`
  // across the cxx seam as an integer for no reason — nothing on the Rust side may
  // do anything with it except hand it back.
  //
  // Every out-param is written on every path, including a `false` return, so a
  // caller reading them after a failure reads zeroes and not stack garbage.
  // `out_status` says WHY a zero identity came back, and the values are seven
  // different findings that must not share a counter — the same discipline as
  // `HELIOS_VKD3D_FENCE_*` above, and for the same reason.
  //
  // ⛔ Keep these values in sync with `bridge12.rs`'s `IdentityStatus`, which maps
  // them by number and counts an unknown value rather than assuming.
  //
  //   0 `HELIOS_VKD3D_IDENTITY_RESOLVED`      — everything answered; the identity is real
  //   1 `HELIOS_VKD3D_IDENTITY_BAD_ARG`       — `resource` was 0, or no device
  //   2 `HELIOS_VKD3D_IDENTITY_NO_INTEROP`    — the engine has no `ID3D12DXVKInteropDevice4`
  //   3 `HELIOS_VKD3D_IDENTITY_ENGINE_REFUSED`— `GetVulkanResourceMemoryInfo` failed (a
  //                                             reserved resource has no bound memory)
  //   4 `HELIOS_VKD3D_IDENTITY_NO_ICD`        — no venus ICD module, or the S4b anchor refused
  //   5 `HELIOS_VKD3D_IDENTITY_NO_EXPORT`     — the anchored ICD predates the memory exports
  //   6 `HELIOS_VKD3D_IDENTITY_ICD_REFUSED`   — the exports ran and answered 0, i.e. the
  //                                             memory has no venus resource (not exported)
  bool resource_venus_identity(std::size_t resource,
                               std::uint64_t* out_vk_memory,
                               std::uint64_t* out_memory_offset,
                               std::uint64_t* out_memory_size,
                               std::uint32_t* out_memory_type_index,
                               std::uint32_t* out_venus_res_id,
                               std::uint64_t* out_venus_alloc_size,
                               std::uint32_t* out_status) const noexcept;

  // UP-5. Hand the venus resource behind `resource`'s memory over to the WDDM
  // allocation that has just adopted it.
  //
  // `helios_venus_memory_transfer_resource_ownership` (`vn_renderer_helios.c:843`)
  // sets `bo->resource_released` so the ICD stops unref'ing the host resource when
  // the `VkDeviceMemory` is freed — the guest KMD's allocation owns it from then on
  // (`create_allocation.rs`'s `adopt_blob_for_allocation`). ⛔ Called only AFTER
  // `pfnAllocateCb` has succeeded: transferring first and then failing the
  // allocation would leave the resource owned by nobody.
  //
  // Returns the res_id it handed over, or 0 (counted and logged) if the interop
  // device, the ICD or the export is unavailable or declined. ⚠ A 0 here after a
  // successful identity read is a REAL defect — the resource would be double-freed
  // at process exit — so the caller must treat it as one.
  std::uint32_t transfer_resource_ownership(std::size_t resource) const noexcept;
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

// The `out_status` values of `resource_venus_identity` above. Declared here, at the
// seam, for the same reason as the fence family: `bridge12.rs` maps them by number.
constexpr std::uint32_t HELIOS_VKD3D_IDENTITY_RESOLVED = 0;
constexpr std::uint32_t HELIOS_VKD3D_IDENTITY_BAD_ARG = 1;
constexpr std::uint32_t HELIOS_VKD3D_IDENTITY_NO_INTEROP = 2;
constexpr std::uint32_t HELIOS_VKD3D_IDENTITY_ENGINE_REFUSED = 3;
constexpr std::uint32_t HELIOS_VKD3D_IDENTITY_NO_ICD = 4;
constexpr std::uint32_t HELIOS_VKD3D_IDENTITY_NO_EXPORT = 5;
constexpr std::uint32_t HELIOS_VKD3D_IDENTITY_ICD_REFUSED = 6;

bool helios_vkd3d_bridge_drain_queue(std::size_t queue,
                                     std::uint64_t* out_wire_fence,
                                     std::uint32_t* out_fence_status) noexcept;
