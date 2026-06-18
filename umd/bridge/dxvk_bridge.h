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
};

// Create a DXVK instance + logical device on the Helios venus adapter.
// Returns nullptr on failure. Matches the cxx bridge signature in src/bridge.rs.
std::unique_ptr<HeliosDxvkDevice> helios_dxvk_create_device(
    std::uint32_t luid_low,
    std::int32_t  luid_high);
