// C++ surface of the Helios UMD <-> DXVK bridge. Included by both the
// cxx-generated glue and dxvk_bridge.cpp.
//
// cxx's generated glue manages `std::unique_ptr<HeliosDxvkDevice>` and therefore
// needs HeliosDxvkDevice to be a COMPLETE type here. We keep the DXVK headers out
// of this (and the glue) via pimpl: HeliosDxvkDevice is a thin complete shell
// holding a unique_ptr to an opaque Impl that owns the DXVK Rc<> objects. The
// destructor is declared here and defined out-of-line in dxvk_bridge.cpp, where
// Impl is complete.
#pragma once

#include <cstdint>
#include <memory>

// Owns the DXVK Rc<DxvkInstance/Adapter/Device>; defined in dxvk_bridge.cpp.
struct HeliosDxvkDeviceImpl;

struct HeliosDxvkDevice {
  HeliosDxvkDevice() noexcept;
  ~HeliosDxvkDevice();
  HeliosDxvkDevice(const HeliosDxvkDevice&) = delete;
  HeliosDxvkDevice& operator=(const HeliosDxvkDevice&) = delete;

  std::unique_ptr<HeliosDxvkDeviceImpl> impl;

  // Raw ID3D11Device* / ID3D11DeviceContext* (as size_t) for the DDI forwarders.
  std::size_t d3d11_device_ptr() const;
  std::size_t d3d11_context_ptr() const;
  std::uint32_t venus_context_id() const;
  bool set_resource_kmt_handles(
      std::size_t d3d11_resource_ptr,
      std::uint32_t local,
      std::uint32_t global) const;
  bool get_resource_memory_info(
      std::size_t d3d11_resource_ptr,
      std::uint64_t* memory,
      std::uint64_t* size,
      std::uint64_t* offset,
      std::uint32_t* resource_id) const;
  // C1 identity: the creating vkAllocateMemory's exact allocationSize and
  // memoryTypeIndex for the resource's backing venus memory (recorded into
  // the WDDM allocation trailer so cross-process openers import with them).
  bool get_resource_alloc_identity(
      std::size_t d3d11_resource_ptr,
      std::uint64_t* venus_alloc_size,
      std::uint32_t* memory_type_index) const;
  bool transfer_resource_ownership(std::size_t d3d11_resource_ptr) const;
  std::size_t open_ddi_texture2d(
      std::uint32_t width,
      std::uint32_t height,
      std::uint32_t format,
      std::uint32_t bind_flags,
      std::uint32_t misc_flags,
      std::uint32_t global,
      std::uint32_t renderer_resource_id,
      std::uint64_t venus_alloc_size,
      std::uint32_t memory_type_index) const;

  // Shader creation wrappers. DXVK may throw dxvk::DxvkError while compiling
  // shader modules; these methods catch it and return 0 so exceptions never
  // cross the D3D UMD ABI.
  std::size_t create_vertex_shader(const std::uint8_t* code, std::size_t len) const;
  std::size_t create_pixel_shader(const std::uint8_t* code, std::size_t len) const;
  std::size_t create_geometry_shader(const std::uint8_t* code, std::size_t len) const;
  // Signature-carrying variant for the >=11.1 DDI, whose typed
  // D3D11_1DDIARG_SIGNATURE_ENTRY2 arrays supply the component types the raw
  // token stream lacks. `kind`: 0 = vertex, 1 = pixel, 2 = geometry.
  // `sig_words` = [n_in, n_out, then (sysval, register, mask, comptype,
  // stream) x n_in, then the same x n_out]; the container gets real
  // ISGN/OSGN chunks so dxbc-spv declares correctly-typed I/O.
  std::size_t create_shader_sig(
      std::uint32_t kind,
      const std::uint8_t* code,
      std::size_t len,
      const std::uint32_t* sig_words,
      std::size_t sig_words_len) const;

  // Flip-model identity rotation (DXGI pfnRotateResourceIdentities): each
  // texture takes the DXVK storage (memory + VkImage + KMT handles) of the
  // NEXT one in the list, the last takes the first's. The swap executes on
  // the CS thread (ordered), no device drain.
  bool rotate_resource_backings(
      const std::size_t* d3d11_resource_ptrs,
      std::size_t count) const;

  // Present-path frame-completion gate: bounded wait until the current
  // flush's submission completes on the GPU (see
  // D3D11ImmediateContext::HeliosWaitFrameComplete). Returns true when the
  // frame completed within the timeout, false on timeout/error.
  bool present_frame_gate(std::uint32_t timeout_us) const;

  // WS1 #4 producer: lazily create the device-wide NAMED present fence
  // (Global\HeliosPresentFence_<pid>, permissive DACL — the IddCx consumer
  // runs as another principal), record a signal of value = ++present counter
  // on the frame's OPEN command list (rides the caller's following Flush;
  // the ICD retires it at host GPU completion), and publish
  // (resid -> pid, value) for the presented src/dst resources in the shared
  // present-sync table. NO wait happens on the present thread.
  // Returns the published value, or 0 when unavailable (no venus resid on
  // the resources, table unavailable, or named-fence creation failed —
  // failure disables the path loudly and permanently for this device, and
  // the caller keeps using the bounded present gate instead).
  std::uint64_t present_sync_publish(
      std::size_t src_resource_ptr,
      std::size_t dst_resource_ptr) const;

  // Name discriminator of this device's WS1 #4 named present fence
  // (Global\HeliosPresentFence_<pid>_<id>), 0 until the first successful
  // present_sync_publish created it (or when the path is disabled). The
  // dcomp-vehicle recycle gate pairs it with publish's returned value so
  // the ICD can import the fence by name and gate image reuse at acquire.
  std::uint32_t present_sync_fence_id() const;

  // Kernel flip-wait plumbing (25th session): the UMD created a monitored
  // fence on the RUNTIME's kernel device (the only device scope the present
  // context's queued GPU waits accept) and hands over the runtime's
  // pfnSignalSynchronizationObjectFromCpuCb (raw fn address), the runtime
  // device handle, the fence handle, and its CPU value VA. The bridge owns
  // the signal side: present_flip_wait_arm enqueues "CPU-signal the flip
  // fence to flip_value when the present fence reaches target_value" on the
  // fence-waiter thread, and a watchdog thread unwedges a stalled chain by
  // signaling the flip fence forward (bounded failure = today's stale-frame
  // semantics, never a forever-parked present context).
  bool present_flip_wait_setup(
      std::size_t signal_cb,
      std::size_t h_rt_device,
      std::uint32_t h_fence,
      std::size_t fence_cpu_va) const;
  bool present_flip_wait_arm(
      std::uint64_t target_value,
      std::uint64_t flip_value) const;

  // Dcomp present vehicle (road 4 unit 2): record an image-level copy of the
  // imported ICD frame (src) into the vehicle backbuffer texture (dst) on
  // the open command list. Sources the import's LIVE storage (the
  // direct-bind staging alias for device-local imports) so no
  // refresh-arming is needed, and DxvkContext::copyImage fires the bounded
  // copy-time consumer present-wait for the imported source — the published
  // (resid -> fence, value) slot orders the copy against the producing
  // ICD's GPU writes. Returns 0 on success, 1 on a (copied, loud) geometry
  // mismatch, negative on failure — the caller must fail the present loudly
  // rather than flip a stale backbuffer.
  std::int32_t present_vehicle_copy(
      std::size_t dst_resource_ptr,
      std::size_t src_resource_ptr) const;

  std::size_t create_hull_shader(const std::uint8_t* code, std::size_t len) const;
  std::size_t create_domain_shader(const std::uint8_t* code, std::size_t len) const;
  std::size_t create_compute_shader(const std::uint8_t* code, std::size_t len) const;
};

// Create a DXVK instance + logical device on the Helios venus adapter.
// Returns nullptr on failure. Matches the cxx bridge signature in src/bridge.rs.
std::unique_ptr<HeliosDxvkDevice> helios_dxvk_create_device(
    std::uint32_t luid_low,
    std::int32_t  luid_high);
