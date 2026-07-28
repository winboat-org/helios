// The Helios venus ICD export surface: the loader spelunking's public results.
//
// T8/R1105. Only the six readers `dxvk_bridge.cpp` actually calls are declared
// here; the manifest discovery, the JSON parsing, the resolve-once export table
// and the `helios_icd_export<Fn>` template stay private to
// `bridge_icd_exports.cpp`, which is the reachability reduction the split buys.
//
// ⚠ INCLUDE ORDER: this header names `VkInstance` and `VkDeviceMemory` but does
// NOT include a Vulkan header of its own. DXVK ships its own loader shim
// (`src/vulkan/vulkan_loader.h`) which must be the first Vulkan header a TU
// sees; pulling `<vulkan/vulkan.h>` in ahead of it breaks `dxvk_device_info.h`
// and `dxvk_presenter.h` with nine hard errors. Include this AFTER the DXVK
// headers.
#pragma once

#include <cstdint>

namespace helios_bridge {

/// The venus context id the ICD reports for the CALLING THREAD. Fallback only.
std::uint32_t read_current_venus_context_id();

/// The venus context id scoped to one `VkInstance` -- the correct question, and
/// the one the dcomp vehicle needs when a process holds several instances.
/// Falls back to `read_current_venus_context_id` when the ICD is too old to
/// export the instance-scoped entry point.
std::uint32_t read_instance_venus_context_id(VkInstance instance);

std::uint64_t venus_memory_id_from_handle(VkDeviceMemory memory);
std::uint32_t venus_memory_resource_id_from_handle(VkDeviceMemory memory);
std::uint32_t venus_memory_transfer_resource_ownership(VkDeviceMemory memory);
bool venus_memory_alloc_info_from_handle(VkDeviceMemory memory,
                                        std::uint64_t* alloc_size,
                                        std::uint32_t* memory_type_index);

}  // namespace helios_bridge
