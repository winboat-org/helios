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

// `order_mode` of HeliosDxvkDevice::present_frame_gate. The values are the
// `HKLM\SOFTWARE\Helios!PresentOrder` registry values, spelled here so the
// wire meaning of the knob lives beside the code that branches on it.
inline constexpr std::uint32_t kPresentOrderComplete = 0;
inline constexpr std::uint32_t kPresentOrderSubmitted = 1;

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
  // Opt-in queue-feed attribution. The timestamp is zero when tracing is off,
  // so the ordinary callback path performs no clock read or atomic update.
  std::uint64_t feed_trace_timestamp_ns() const noexcept;
  void feed_trace_render_callback(std::uint64_t duration_ns) const noexcept;
  void feed_trace_present_callback(std::uint64_t duration_ns) const noexcept;
  // BUILD_2 recycle handoff. The deferred-context address is borrowed; the
  // command-list address transfers its one owned IC hCL reference on true.
  // The bridge gives it only to that exact deferred context's bounded cache;
  // the list's origin check rejects a cross-DC handoff, and false means the
  // caller still owns and releases the command-list reference.
  bool recycle_deferred_command_list(
      std::size_t deferred_context_ptr,
      std::size_t command_list_ptr) const noexcept;
  // Marks a freshly-created, UMD-private deferred context as eligible for
  // DXVK's DDI-only logical Finish(FALSE) reset. The borrowed pointer must
  // originate from this device's CreateDeferredContext call.
  bool enable_deferred_context_ddi_logical_reset(
      std::size_t deferred_context_ptr) const noexcept;
  bool set_resource_kmt_handles(
      std::size_t d3d11_resource_ptr,
      std::uint32_t local,
      std::uint32_t global) const noexcept;
  bool get_resource_memory_info(
      std::size_t d3d11_resource_ptr,
      std::uint64_t* memory,
      std::uint64_t* size,
      std::uint64_t* offset,
      std::uint32_t* resource_id) const noexcept;
  // C1 identity: the creating vkAllocateMemory's exact allocationSize and
  // memoryTypeIndex for the resource's backing venus memory (recorded into
  // the WDDM allocation trailer so cross-process openers import with them).
  bool get_resource_alloc_identity(
      std::size_t d3d11_resource_ptr,
      std::uint64_t* venus_alloc_size,
      std::uint32_t* memory_type_index) const noexcept;
  bool transfer_resource_ownership(std::size_t d3d11_resource_ptr) const noexcept;
  std::size_t open_ddi_texture2d(
      std::uint32_t width,
      std::uint32_t height,
      std::uint32_t format,
      std::uint32_t bind_flags,
      std::uint32_t misc_flags,
      std::uint32_t global,
      std::uint32_t renderer_resource_id,
      std::uint64_t venus_alloc_size,
      std::uint32_t memory_type_index,
      bool scanout_linear,
      bool linear_scanout_target,
      bool cross_context_optimal) const;

  // Create the DWM scan-out primary as a dedicated OPTIMAL,
  // DMA_BUF-exportable image (via the D3D11_HELIOS_CREATE_INFO marker) and
  // report logical scanout metadata for exact host reconstruction. Returns an
  // owned ID3D11Resource* (as usize), or 0.
  std::size_t create_ddi_scanout_texture2d(
      std::uint32_t width,
      std::uint32_t height,
      std::uint32_t format,
      std::uint32_t bind_flags,
      std::uint32_t misc_flags,
      std::uint64_t* out_row_pitch,
      std::uint64_t* out_offset) const;

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
  // Signature-carrying tessellation shader create. `kind`: 0 = hull,
  // 1 = domain. `sig_words` = [n_in, n_out, n_patch, then entries for each
  // group as (sysval, register, mask, comptype, stream)]. D3D11 tessellation
  // DDI signatures do not carry component type/stream, so the Rust side passes
  // zeros there and the DXBC wrapper uses the same fallback rules as the 11.1
  // shader path.
  std::size_t create_tess_shader_sig(
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

  // Cross-process present ordering, PRODUCER side. Records a signal on this
  // device's named present timeline at the presented frame's GPU completion and
  // publishes (resid -> pid, fenceId, value), so a consumer compositing this
  // surface waits on the GPU instead of us blocking the CPU here. Call once per
  // present, BEFORE the gate, with the presented source resource.
  //
  // Returns the existing slot-publication success.  On success it also writes
  // the registered stream correlation when it is representable; all three
  // outputs remain zero when the private ICD export/KMD stream is unavailable
  // or the full timeline value exceeds u32, so the Rust side retains its gate.
  bool publish_present_order(std::size_t d3d11_resource_ptr,
                             std::uint32_t* out_ctx_id,
                             std::uint32_t* out_value32,
                             std::uint64_t* out_cookie) const;

  // D4a scanout acquire: hand the per-device KMD read-retirement event to the
  // DXVK device's signaler thread (DxvkHeliosScanoutAcquire). `event_handle`
  // is a usermode auto-reset event HANDLE as a machine word, never 0. The UMD
  // keeps ownership of the handle and closes it in DestroyDevice only after
  // this bridge device has dropped — the signaler joins inside ~DxvkDevice, so
  // the handle always outlives its last waiter. Returns false if no live DXVK
  // device.
  bool set_scanout_acquire_event(std::size_t event_handle) const noexcept;

  // Present-path ordering gate. `order_mode` selects WHAT is waited for:
  //
  //   kPresentOrderSubmitted — wait until the frame's Venus work has reached
  //     vkQueueSubmit (D3D11ImmediateContext::HeliosWaitFrameSubmitted). This
  //     is the KMD's actual requirement: pfnRenderCb then samples a watermark
  //     that covers the frame. `timeout_us` is unused; the wait is on guest CPU
  //     threads only and always reports true.
  //   kPresentOrderComplete — additionally wait, bounded by `timeout_us`, for
  //     the GPU to finish it (HeliosWaitFrameComplete). Returns false on
  //     timeout/error, and a timeout means the present is published with work
  //     still outstanding — the stale-frame window this gate exists to close.
  bool present_frame_gate(std::uint32_t timeout_us, std::uint32_t order_mode) const;

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

  // D4b snapshot ring: record an image-level copy of the presented primary
  // (src) into a snapshot-ring image (dst) on the open command list, BEFORE
  // the present-time Flush, so the copy rides frame N's own command stream —
  // ordered after the frame's draws and before anything of frame N+1 by queue
  // order alone (no waits, no stalls). Unlike present_vehicle_copy there is no
  // staging-alias substitution: both operands are this device's own DXVK
  // images (the source is never an import), so the copy-time consumer
  // present-wait no-ops and the full-extent OPTIMAL->OPTIMAL same-format copy
  // takes DxvkContext::copyImageHw. Returns 0 on success, 1 on a (copied,
  // loud) geometry mismatch — the caller must then SKIP the descriptor
  // substitution (a partially-filled snapshot must never be bound) — and
  // negative on failure, where the caller presents exactly as today.
  std::int32_t present_snapshot_copy(
      std::size_t dst_resource_ptr,
      std::size_t src_resource_ptr,
      bool windowed_blt_reservation) const;

  std::size_t create_hull_shader(const std::uint8_t* code, std::size_t len) const;
  std::size_t create_domain_shader(const std::uint8_t* code, std::size_t len) const;
  std::size_t create_compute_shader(const std::uint8_t* code, std::size_t len) const;
};

// Create a DXVK instance + logical device on the Helios venus adapter.
// Returns nullptr on failure. Matches the cxx bridge signature in src/bridge.rs.
std::unique_ptr<HeliosDxvkDevice> helios_dxvk_create_device(
    std::uint32_t luid_low,
    std::int32_t  luid_high);
