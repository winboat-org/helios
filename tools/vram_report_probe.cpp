// Compare the memory numbers exposed by DXGI/VidMm with the Vulkan heaps
// exposed by Venus. Build from a Visual Studio developer command prompt:
//
//   cl /nologo /std:c++17 /O2 /EHsc vram_report_probe.cpp
//      /I%VULKAN_SDK%\Include /Iicd\win-build\wdk-include
//      /Fe:vram_report_probe.exe /link dxgi.lib d3d11.lib gdi32.lib
//      /LIBPATH:%VULKAN_SDK%\Lib vulkan-1.lib
//
// Add `--d3d11-allocs N` to create N shared 4096x4096 RGBA render targets
// (64 MiB each) on Helios and print this process's VidMm usage after every
// allocation. Shared is intentional: private DXVK resources have no paired
// WDDM allocation and therefore cannot exercise VidMm accounting.
// `--hold-seconds N` keeps them alive for host-side VRAM observation.
// `--vulkan-allocs N` performs the same boundary test with 64-MiB native
// Vulkan device-memory allocations and prints the VkResult for each request.
// `--vulkan-nonlocal-allocs N` selects a memory type from a non-device-local
// heap, proving the Mesa tracker charges shared rather than dedicated VidMm.

#define WIN32_LEAN_AND_MEAN
#include <windows.h>

#include <d3d11.h>
#ifndef _NTDEF_
typedef LONG NTSTATUS, *PNTSTATUS;
#endif
#include <d3dkmthk.h>
#include <dxgi1_4.h>
#include <vulkan/vulkan.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>

static double gib(uint64_t bytes) {
  return static_cast<double>(bytes) / (1024.0 * 1024.0 * 1024.0);
}

static void print_bytes(const char *label, uint64_t bytes) {
  std::printf("    %-25s %12llu bytes  (%7.3f GiB)\n", label,
              static_cast<unsigned long long>(bytes), gib(bytes));
}

static const char *heap_flags(VkMemoryHeapFlags flags) {
  return (flags & VK_MEMORY_HEAP_DEVICE_LOCAL_BIT) ? "DEVICE_LOCAL"
                                                   : "non-local";
}

struct RawVidMmUsage {
  uint64_t local = 0;
  uint64_t nonlocal = 0;
  bool valid = false;
};

static RawVidMmUsage query_raw_vidmm_process(const char *label) {
  RawVidMmUsage usage{};
  IDXGIFactory1 *factory = nullptr;
  if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1),
                                reinterpret_cast<void **>(&factory))))
    return usage;

  LUID luid{};
  bool found = false;
  for (UINT index = 0; !found; ++index) {
    IDXGIAdapter1 *adapter = nullptr;
    if (factory->EnumAdapters1(index, &adapter) == DXGI_ERROR_NOT_FOUND)
      break;
    DXGI_ADAPTER_DESC1 desc{};
    if (SUCCEEDED(adapter->GetDesc1(&desc)) && desc.VendorId == 0x1af4 &&
        desc.DeviceId == 0x1050) {
      luid = desc.AdapterLuid;
      found = true;
    }
    adapter->Release();
  }
  factory->Release();
  if (!found)
    return usage;

  for (ULONG segment_id = 1; segment_id <= 2; ++segment_id) {
    D3DKMT_QUERYSTATISTICS segment{};
    segment.Type = D3DKMT_QUERYSTATISTICS_SEGMENT;
    segment.AdapterLuid = luid;
    segment.QuerySegment.SegmentId = segment_id;
    if (D3DKMTQueryStatistics(&segment) != 0)
      return RawVidMmUsage{};

    D3DKMT_QUERYSTATISTICS process{};
    process.Type = D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT;
    process.AdapterLuid = luid;
    process.hProcess = GetCurrentProcess();
    process.QueryProcessSegment.SegmentId = segment_id;
    if (D3DKMTQueryStatistics(&process) != 0)
      return RawVidMmUsage{};

    const uint64_t committed =
        process.QueryResult.ProcessSegmentInformation.BytesCommitted;
    const bool aperture =
        segment.QueryResult.SegmentInformation.Aperture != FALSE;
    std::printf("  %-12s process seg%lu (%s) %8.2f MiB\n", label,
                static_cast<unsigned long>(segment_id),
                aperture ? "non-local" : "local",
                static_cast<double>(committed) / (1024.0 * 1024.0));
    if (aperture)
      usage.nonlocal += committed;
    else
      usage.local += committed;
  }
  usage.valid = true;
  return usage;
}

static uint64_t positive_delta(uint64_t after, uint64_t before) {
  return after >= before ? after - before : 0;
}

static void print_vidmm_group(IDXGIAdapter3 *adapter,
                              DXGI_MEMORY_SEGMENT_GROUP group,
                              const char *name) {
  DXGI_QUERY_VIDEO_MEMORY_INFO info{};
  const HRESULT hr = adapter->QueryVideoMemoryInfo(0, group, &info);
  if (FAILED(hr)) {
    std::printf("    %-25s Query failed: 0x%08lx\n", name,
                static_cast<unsigned long>(hr));
    return;
  }
  std::printf("    %s VidMm budget\n", name);
  print_bytes("Budget", info.Budget);
  print_bytes("CurrentUsage", info.CurrentUsage);
  print_bytes("CurrentReservation", info.CurrentReservation);
  print_bytes("AvailableForReservation", info.AvailableForReservation);
}

static void report_dxgi() {
  IDXGIFactory1 *factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1),
                                  reinterpret_cast<void **>(&factory));
  if (FAILED(hr)) {
    std::printf("DXGI: CreateDXGIFactory1 failed: 0x%08lx\n",
                static_cast<unsigned long>(hr));
    return;
  }

  std::puts("DXGI / VidMm");
  for (UINT index = 0;; ++index) {
    IDXGIAdapter1 *adapter = nullptr;
    hr = factory->EnumAdapters1(index, &adapter);
    if (hr == DXGI_ERROR_NOT_FOUND)
      break;
    if (FAILED(hr)) {
      std::printf("  EnumAdapters1(%u) failed: 0x%08lx\n", index,
                  static_cast<unsigned long>(hr));
      break;
    }

    DXGI_ADAPTER_DESC1 desc{};
    hr = adapter->GetDesc1(&desc);
    if (FAILED(hr)) {
      std::printf("  adapter %u GetDesc1 failed: 0x%08lx\n", index,
                  static_cast<unsigned long>(hr));
      adapter->Release();
      continue;
    }

    std::printf("  [%u] %ls\n", index, desc.Description);
    std::printf("    PCI vendor/device         %04x:%04x\n", desc.VendorId,
                desc.DeviceId);
    print_bytes("DedicatedVideoMemory", desc.DedicatedVideoMemory);
    print_bytes("DedicatedSystemMemory", desc.DedicatedSystemMemory);
    print_bytes("SharedSystemMemory", desc.SharedSystemMemory);

    IDXGIAdapter3 *adapter3 = nullptr;
    hr = adapter->QueryInterface(__uuidof(IDXGIAdapter3),
                                 reinterpret_cast<void **>(&adapter3));
    if (FAILED(hr)) {
      std::printf("    IDXGIAdapter3 unavailable: 0x%08lx\n",
                  static_cast<unsigned long>(hr));
    } else {
      print_vidmm_group(adapter3, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, "LOCAL");
      print_vidmm_group(adapter3, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL,
                        "NON_LOCAL");
      adapter3->Release();
    }
    adapter->Release();
  }
  factory->Release();
}

static bool report_d3d11_allocations(unsigned allocation_count,
                                     unsigned hold_seconds) {
  IDXGIFactory1 *factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1),
                                  reinterpret_cast<void **>(&factory));
  if (FAILED(hr)) {
    std::printf("\nD3D11 allocation test: factory creation failed: 0x%08lx\n",
                static_cast<unsigned long>(hr));
    return false;
  }

  IDXGIAdapter1 *adapter = nullptr;
  for (UINT index = 0;; ++index) {
    IDXGIAdapter1 *candidate = nullptr;
    hr = factory->EnumAdapters1(index, &candidate);
    if (hr == DXGI_ERROR_NOT_FOUND)
      break;
    if (FAILED(hr))
      break;
    DXGI_ADAPTER_DESC1 desc{};
    if (SUCCEEDED(candidate->GetDesc1(&desc)) && desc.VendorId == 0x1af4 &&
        desc.DeviceId == 0x1050) {
      adapter = candidate;
      break;
    }
    candidate->Release();
  }
  if (!adapter) {
    std::puts("\nD3D11 allocation test: Helios adapter not found");
    factory->Release();
    return false;
  }

  IDXGIAdapter3 *adapter3 = nullptr;
  hr = adapter->QueryInterface(__uuidof(IDXGIAdapter3),
                               reinterpret_cast<void **>(&adapter3));
  if (FAILED(hr)) {
    std::printf("\nD3D11 allocation test: IDXGIAdapter3 unavailable: 0x%08lx\n",
                static_cast<unsigned long>(hr));
    adapter->Release();
    factory->Release();
    return false;
  }

  static const D3D_FEATURE_LEVEL levels[] = {
      D3D_FEATURE_LEVEL_11_1,
      D3D_FEATURE_LEVEL_11_0,
      D3D_FEATURE_LEVEL_10_1,
      D3D_FEATURE_LEVEL_10_0,
  };
  ID3D11Device *device = nullptr;
  ID3D11DeviceContext *context = nullptr;
  D3D_FEATURE_LEVEL level{};
  hr = D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0, levels,
                         static_cast<UINT>(sizeof(levels) / sizeof(levels[0])),
                         D3D11_SDK_VERSION, &device, &level, &context);
  std::printf("\nD3D11 / VidMm allocation test\n");
  std::printf("  D3D11CreateDevice          0x%08lx (FL 0x%04x)\n",
              static_cast<unsigned long>(hr), static_cast<unsigned>(level));
  if (FAILED(hr)) {
    adapter3->Release();
    adapter->Release();
    factory->Release();
    return false;
  }

  const RawVidMmUsage before = query_raw_vidmm_process("before");
  std::puts("  Before render-target allocations");
  print_vidmm_group(adapter3, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, "LOCAL");

  std::vector<ID3D11Texture2D *> textures;
  std::vector<ID3D11RenderTargetView *> views;
  textures.reserve(allocation_count);
  views.reserve(allocation_count);
  const D3D11_TEXTURE2D_DESC desc = {
      4096,
      4096,
      1,
      1,
      DXGI_FORMAT_R8G8B8A8_UNORM,
      {1, 0},
      D3D11_USAGE_DEFAULT,
      D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
      0,
      D3D11_RESOURCE_MISC_SHARED,
  };
  const FLOAT clear[4] = {0.125f, 0.25f, 0.5f, 1.0f};
  for (unsigned i = 0; i < allocation_count; ++i) {
    ID3D11Texture2D *texture = nullptr;
    hr = device->CreateTexture2D(&desc, nullptr, &texture);
    if (FAILED(hr)) {
      std::printf("  allocation %u CreateTexture2D failed: 0x%08lx\n", i + 1,
                  static_cast<unsigned long>(hr));
      break;
    }
    ID3D11RenderTargetView *view = nullptr;
    hr = device->CreateRenderTargetView(texture, nullptr, &view);
    if (FAILED(hr)) {
      std::printf("  allocation %u CreateRenderTargetView failed: 0x%08lx\n",
                  i + 1, static_cast<unsigned long>(hr));
      texture->Release();
      break;
    }
    context->ClearRenderTargetView(view, clear);
    context->Flush();
    textures.push_back(texture);
    views.push_back(view);
    std::printf("  After %u x 64 MiB render target(s)\n", i + 1);
    print_vidmm_group(adapter3, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, "LOCAL");
  }

  const RawVidMmUsage resident = query_raw_vidmm_process("resident");
  const uint64_t expected = textures.size() * 64ull * 1024ull * 1024ull;
  const uint64_t local_delta = positive_delta(resident.local, before.local);
  const uint64_t nonlocal_delta =
      positive_delta(resident.nonlocal, before.nonlocal);
  const uint64_t tolerance = 1024ull * 1024ull;
  std::printf("  dedicated VidMm delta     %.2f MiB (expected %.2f MiB)\n",
              static_cast<double>(local_delta) / (1024.0 * 1024.0),
              static_cast<double>(expected) / (1024.0 * 1024.0));
  std::printf("  shared VidMm delta        %.2f MiB\n",
              static_cast<double>(nonlocal_delta) / (1024.0 * 1024.0));
  const bool passed =
      before.valid && resident.valid && textures.size() == allocation_count &&
      local_delta >= expected && local_delta <= expected + tolerance &&
      nonlocal_delta <= tolerance;
  std::printf("  D3D11 VIDMM ACCOUNTING: %s\n", passed ? "PASS" : "FAIL");

  if (hold_seconds) {
    std::printf("  Holding %zu render targets for %u second(s)\n",
                textures.size(), hold_seconds);
    std::fflush(stdout);
    Sleep(hold_seconds * 1000);
  }

  for (ID3D11RenderTargetView *view : views)
    view->Release();
  for (ID3D11Texture2D *texture : textures)
    texture->Release();
  context->Flush();
  context->Release();
  device->Release();
  adapter3->Release();
  adapter->Release();
  factory->Release();
  return passed;
}

static bool supports_memory_budget(VkPhysicalDevice physical_device) {
  uint32_t count = 0;
  if (vkEnumerateDeviceExtensionProperties(physical_device, nullptr, &count,
                                           nullptr) != VK_SUCCESS)
    return false;
  VkExtensionProperties *extensions = new VkExtensionProperties[count];
  const VkResult result = vkEnumerateDeviceExtensionProperties(
      physical_device, nullptr, &count, extensions);
  bool found = false;
  if (result == VK_SUCCESS) {
    for (uint32_t i = 0; i < count; ++i) {
      if (std::strcmp(extensions[i].extensionName,
                      VK_EXT_MEMORY_BUDGET_EXTENSION_NAME) == 0) {
        found = true;
        break;
      }
    }
  }
  delete[] extensions;
  return found;
}

static void report_vulkan() {
  VkApplicationInfo app{VK_STRUCTURE_TYPE_APPLICATION_INFO};
  app.pApplicationName = "Helios VRAM report probe";
  app.apiVersion = VK_API_VERSION_1_1;
  VkInstanceCreateInfo create{VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
  create.pApplicationInfo = &app;
  VkInstance instance = VK_NULL_HANDLE;
  VkResult result = vkCreateInstance(&create, nullptr, &instance);
  if (result != VK_SUCCESS) {
    std::printf("\nVulkan: vkCreateInstance failed: %d\n", result);
    return;
  }

  uint32_t count = 0;
  result = vkEnumeratePhysicalDevices(instance, &count, nullptr);
  if (result != VK_SUCCESS || count == 0) {
    std::printf("\nVulkan: no physical devices (result %d)\n", result);
    vkDestroyInstance(instance, nullptr);
    return;
  }
  VkPhysicalDevice *devices = new VkPhysicalDevice[count];
  result = vkEnumeratePhysicalDevices(instance, &count, devices);
  if (result != VK_SUCCESS) {
    std::printf("\nVulkan: enumerate failed: %d\n", result);
    delete[] devices;
    vkDestroyInstance(instance, nullptr);
    return;
  }

  std::puts("\nVulkan / Venus");
  for (uint32_t device_index = 0; device_index < count; ++device_index) {
    VkPhysicalDeviceProperties properties{};
    vkGetPhysicalDeviceProperties(devices[device_index], &properties);
    std::printf("  [%u] %s\n", device_index, properties.deviceName);
    std::printf("    vendor/device             %04x:%04x\n",
                properties.vendorID, properties.deviceID);

    const bool has_budget = supports_memory_budget(devices[device_index]);
    VkPhysicalDeviceMemoryBudgetPropertiesEXT budget{
        VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MEMORY_BUDGET_PROPERTIES_EXT};
    VkPhysicalDeviceMemoryProperties2 memory{
        VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MEMORY_PROPERTIES_2};
    memory.pNext = has_budget ? &budget : nullptr;
    vkGetPhysicalDeviceMemoryProperties2(devices[device_index], &memory);
    std::printf("    VK_EXT_memory_budget      %s\n",
                has_budget ? "yes" : "no");
    for (uint32_t heap = 0; heap < memory.memoryProperties.memoryHeapCount;
         ++heap) {
      const VkMemoryHeap &h = memory.memoryProperties.memoryHeaps[heap];
      std::printf("    heap %u (%s)\n", heap, heap_flags(h.flags));
      print_bytes("Size", h.size);
      if (has_budget) {
        print_bytes("Budget", budget.heapBudget[heap]);
        print_bytes("Usage", budget.heapUsage[heap]);
      }
    }
  }

  delete[] devices;
  vkDestroyInstance(instance, nullptr);
}

static bool report_vulkan_allocations(unsigned allocation_count,
                                      unsigned hold_seconds,
                                      bool device_local) {
  VkApplicationInfo app{VK_STRUCTURE_TYPE_APPLICATION_INFO};
  app.pApplicationName = "Helios Vulkan allocation probe";
  app.apiVersion = VK_API_VERSION_1_1;
  VkInstanceCreateInfo create{VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
  create.pApplicationInfo = &app;
  VkInstance instance = VK_NULL_HANDLE;
  VkResult result = vkCreateInstance(&create, nullptr, &instance);
  if (result != VK_SUCCESS) {
    std::printf("\nVulkan allocation test: vkCreateInstance failed: %d\n",
                result);
    return false;
  }

  uint32_t device_count = 0;
  result = vkEnumeratePhysicalDevices(instance, &device_count, nullptr);
  std::vector<VkPhysicalDevice> physical_devices(device_count);
  if (result == VK_SUCCESS && device_count) {
    result = vkEnumeratePhysicalDevices(instance, &device_count,
                                        physical_devices.data());
  }
  VkPhysicalDevice physical = VK_NULL_HANDLE;
  for (VkPhysicalDevice candidate : physical_devices) {
    VkPhysicalDeviceProperties properties{};
    vkGetPhysicalDeviceProperties(candidate, &properties);
    if (properties.deviceType != VK_PHYSICAL_DEVICE_TYPE_CPU) {
      physical = candidate;
      break;
    }
  }
  if (result != VK_SUCCESS || physical == VK_NULL_HANDLE) {
    std::printf("\nVulkan allocation test: accelerated device not found (%d)\n",
                result);
    vkDestroyInstance(instance, nullptr);
    return false;
  }

  uint32_t queue_count = 0;
  vkGetPhysicalDeviceQueueFamilyProperties(physical, &queue_count, nullptr);
  std::vector<VkQueueFamilyProperties> queues(queue_count);
  vkGetPhysicalDeviceQueueFamilyProperties(physical, &queue_count,
                                           queues.data());
  uint32_t queue_family = UINT32_MAX;
  for (uint32_t i = 0; i < queue_count; ++i) {
    if (queues[i].queueCount) {
      queue_family = i;
      break;
    }
  }

  VkPhysicalDeviceMemoryProperties memory{};
  vkGetPhysicalDeviceMemoryProperties(physical, &memory);
  uint32_t memory_type = UINT32_MAX;
  uint32_t memory_heap = UINT32_MAX;
  for (uint32_t i = 0; i < memory.memoryTypeCount; ++i) {
    const uint32_t heap = memory.memoryTypes[i].heapIndex;
    const bool heap_is_device_local =
        (memory.memoryHeaps[heap].flags & VK_MEMORY_HEAP_DEVICE_LOCAL_BIT) != 0;
    if (heap_is_device_local == device_local) {
      memory_type = i;
      memory_heap = heap;
      break;
    }
  }
  if (queue_family == UINT32_MAX || memory_type == UINT32_MAX) {
    std::printf("\nVulkan allocation test: no usable queue or %s memory\n",
                device_local ? "device-local" : "non-local");
    vkDestroyInstance(instance, nullptr);
    return false;
  }

  const float priority = 1.0f;
  VkDeviceQueueCreateInfo queue{VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO};
  queue.queueFamilyIndex = queue_family;
  queue.queueCount = 1;
  queue.pQueuePriorities = &priority;
  VkDeviceCreateInfo device_create{VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO};
  device_create.queueCreateInfoCount = 1;
  device_create.pQueueCreateInfos = &queue;
  VkDevice device = VK_NULL_HANDLE;
  result = vkCreateDevice(physical, &device_create, nullptr, &device);
  std::printf("\nVulkan %s device-memory allocation test\n",
              device_local ? "device-local" : "non-local");
  std::printf("  vkCreateDevice             %d\n", result);
  if (result != VK_SUCCESS) {
    vkDestroyInstance(instance, nullptr);
    return false;
  }

  const RawVidMmUsage before = query_raw_vidmm_process("before");

  std::vector<VkDeviceMemory> allocations;
  allocations.reserve(allocation_count);
  VkMemoryAllocateInfo allocate{VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
  allocate.allocationSize = 64ull * 1024 * 1024;
  allocate.memoryTypeIndex = memory_type;
  std::printf("  memory type / heap         %u / %u (%s)\n", memory_type,
              memory_heap, device_local ? "DEVICE_LOCAL" : "non-local");
  for (unsigned i = 0; i < allocation_count; ++i) {
    VkDeviceMemory allocation = VK_NULL_HANDLE;
    result = vkAllocateMemory(device, &allocate, nullptr, &allocation);
    std::printf("  allocation %u x 64 MiB     %d\n", i + 1, result);
    if (result != VK_SUCCESS)
      break;
    allocations.push_back(allocation);
  }

  const RawVidMmUsage resident = query_raw_vidmm_process("resident");
  const uint64_t expected = allocations.size() * allocate.allocationSize;
  const uint64_t selected_delta =
      device_local ? positive_delta(resident.local, before.local)
                   : positive_delta(resident.nonlocal, before.nonlocal);
  const uint64_t other_delta =
      device_local ? positive_delta(resident.nonlocal, before.nonlocal)
                   : positive_delta(resident.local, before.local);
  const uint64_t tolerance = 1024ull * 1024ull;
  std::printf("  selected VidMm delta      %.2f MiB (expected %.2f MiB)\n",
              static_cast<double>(selected_delta) / (1024.0 * 1024.0),
              static_cast<double>(expected) / (1024.0 * 1024.0));
  std::printf("  other VidMm delta         %.2f MiB\n",
              static_cast<double>(other_delta) / (1024.0 * 1024.0));
  bool passed =
      before.valid && resident.valid &&
      allocations.size() == allocation_count && selected_delta >= expected &&
      selected_delta <= expected + tolerance && other_delta <= tolerance;

  if (hold_seconds) {
    std::printf("  Holding %zu allocations for %u second(s)\n",
                allocations.size(), hold_seconds);
    std::fflush(stdout);
    Sleep(hold_seconds * 1000);
  }
  for (VkDeviceMemory allocation : allocations)
    vkFreeMemory(device, allocation, nullptr);
  const RawVidMmUsage destroyed = query_raw_vidmm_process("destroyed");
  if (!destroyed.valid || destroyed.local > before.local + tolerance ||
      destroyed.nonlocal > before.nonlocal + tolerance)
    passed = false;
  std::printf("  VULKAN VIDMM ACCOUNTING: %s\n", passed ? "PASS" : "FAIL");
  vkDestroyDevice(device, nullptr);
  vkDestroyInstance(instance, nullptr);
  return passed;
}

int main(int argc, char **argv) {
  unsigned d3d11_allocations = 0;
  unsigned vulkan_allocations = 0;
  unsigned vulkan_nonlocal_allocations = 0;
  unsigned hold_seconds = 0;
  bool passed = true;
  for (int i = 1; i + 1 < argc; i += 2) {
    if (std::strcmp(argv[i], "--d3d11-allocs") == 0)
      d3d11_allocations =
          static_cast<unsigned>(std::strtoul(argv[i + 1], nullptr, 10));
    else if (std::strcmp(argv[i], "--vulkan-allocs") == 0)
      vulkan_allocations =
          static_cast<unsigned>(std::strtoul(argv[i + 1], nullptr, 10));
    else if (std::strcmp(argv[i], "--vulkan-nonlocal-allocs") == 0)
      vulkan_nonlocal_allocations =
          static_cast<unsigned>(std::strtoul(argv[i + 1], nullptr, 10));
    else if (std::strcmp(argv[i], "--hold-seconds") == 0)
      hold_seconds =
          static_cast<unsigned>(std::strtoul(argv[i + 1], nullptr, 10));
  }
  MEMORYSTATUSEX status{};
  status.dwLength = sizeof(status);
  if (GlobalMemoryStatusEx(&status)) {
    std::puts("Guest system memory");
    print_bytes("PhysicalTotal", status.ullTotalPhys);
    print_bytes("PhysicalAvailable", status.ullAvailPhys);
    std::puts("");
  }
  report_dxgi();
  report_vulkan();
  if (d3d11_allocations)
    passed &= report_d3d11_allocations(d3d11_allocations, hold_seconds);
  if (vulkan_allocations)
    passed &= report_vulkan_allocations(vulkan_allocations, hold_seconds, true);
  if (vulkan_nonlocal_allocations)
    passed &= report_vulkan_allocations(vulkan_nonlocal_allocations,
                                        hold_seconds, false);
  return passed ? 0 : 1;
}
