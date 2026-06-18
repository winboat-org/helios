// Helios UMD <-> DXVK engine bridge implementation.
//
// Wraps DXVK's DxvkInstance/DxvkAdapter/DxvkDevice behind the opaque
// HeliosDxvkDevice. The DXVK engine references a frontend-provided
// `Logger::s_instance` global (normally defined in src/d3d11/d3d11_main.cpp,
// which we do not build) — we provide it here.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

#include <cstdio>
#include <cstdlib>
#include <exception>

#include "dxvk_bridge.h"

#include "dxvk_instance.h"
#include "dxvk_adapter.h"
#include "dxvk_device.h"
#include "../src/util/util_error.h"

namespace dxvk {
  // Frontend-provided global the DXVK engine links against. The string is the
  // log file name DXVK writes engine diagnostics to.
  Logger Logger::s_instance("helios_umd_dxvk.log");
}

namespace {
  void umd_log(const char* msg) {
    FILE* f = nullptr;
    if (fopen_s(&f, "C:\\Windows\\Temp\\helios_umd.log", "a") == 0 && f) {
      fprintf(f, "[dxvk-bridge] %s\n", msg);
      fclose(f);
    }
  }
}

// Opaque to the public header / cxx glue; owns the DXVK Rc<> objects.
struct HeliosDxvkDeviceImpl {
  dxvk::Rc<dxvk::DxvkInstance> instance;
  dxvk::Rc<dxvk::DxvkAdapter>  adapter;
  dxvk::Rc<dxvk::DxvkDevice>   device;
};

// Out-of-line ctor/dtor, defined where HeliosDxvkDeviceImpl is complete so the
// header (and the cxx glue) need no DXVK headers.
HeliosDxvkDevice::HeliosDxvkDevice() noexcept = default;
HeliosDxvkDevice::~HeliosDxvkDevice() = default;

std::unique_ptr<HeliosDxvkDevice> helios_dxvk_create_device(
    std::uint32_t luid_low,
    std::int32_t  luid_high) {
  // Force selection of the Helios venus device if other ICDs are present.
  _putenv_s("DXVK_FILTER_DEVICE_NAME", "Virtio-GPU Venus");

  try {
    auto out = std::make_unique<HeliosDxvkDevice>();
    out->impl = std::make_unique<HeliosDxvkDeviceImpl>();
    auto& d = *out->impl;

    d.instance = new dxvk::DxvkInstance(dxvk::DxvkInstanceFlags());

    if (luid_low != 0 || luid_high != 0) {
      LUID luid;
      luid.LowPart  = luid_low;
      luid.HighPart = luid_high;
      d.adapter = d.instance->findAdapterByLuid(&luid);
      if (d.adapter == nullptr)
        umd_log("findAdapterByLuid found nothing; falling back to adapter 0");
    }

    if (d.adapter == nullptr)
      d.adapter = d.instance->enumAdapters(0);

    if (d.adapter == nullptr) {
      umd_log("no Vulkan adapter enumerated (venus ICD not present?)");
      return nullptr;
    }

    d.device = d.adapter->createDevice();
    if (d.device == nullptr) {
      umd_log("DxvkAdapter::createDevice returned null");
      return nullptr;
    }

    umd_log("DxvkDevice created on venus adapter OK");
    return out;
  } catch (const dxvk::DxvkError& e) {
    umd_log(("DxvkError: " + e.message()).c_str());
    return nullptr;
  } catch (const std::exception& e) {
    umd_log(e.what());
    return nullptr;
  } catch (...) {
    umd_log("unknown C++ exception in helios_dxvk_create_device");
    return nullptr;
  }
}
