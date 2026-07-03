// vk_export_alloc_probe.cpp — interrogate the Helios/venus host about EXPORT
// memory allocations (the DWM shared-surface path that fails host-side).
//
// Reproduces what dxvk-helios does for a MISC_SHARED(0x802) texture:
//   VkImage 1896x1030 B8G8R8A8 optimal, usage SAMPLED|COLOR_ATTACHMENT,
//   VkExternalMemoryImageCreateInfo{OPAQUE_FD}
//   vkAllocateMemory + VkExportMemoryAllocateInfo{OPAQUE_FD}
//                    + VkMemoryDedicatedAllocateInfo{image}
// and prints the VkResult of every step, across every compatible memory type,
// plus variations (no dedicated, no export-on-image, host-visible, buffer).
//
// Build (VM):
//   clang-cl /nologo /MD /O2 /I "C:\VulkanSDK\<ver>\Include" Z:\tools\vk_export_alloc_probe.cpp \
//     /Fe:C:\Users\Rupansh\vk_probe.exe /link /LIBPATH:"C:\VulkanSDK\<ver>\Lib" vulkan-1.lib

#include <vulkan/vulkan.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

static const char *rs(VkResult r)
{
  switch (r) {
  case VK_SUCCESS: return "VK_SUCCESS";
  case VK_ERROR_OUT_OF_HOST_MEMORY: return "VK_ERROR_OUT_OF_HOST_MEMORY";
  case VK_ERROR_OUT_OF_DEVICE_MEMORY: return "VK_ERROR_OUT_OF_DEVICE_MEMORY";
  case VK_ERROR_INVALID_EXTERNAL_HANDLE: return "VK_ERROR_INVALID_EXTERNAL_HANDLE";
  case VK_ERROR_INITIALIZATION_FAILED: return "VK_ERROR_INITIALIZATION_FAILED";
  case VK_ERROR_FEATURE_NOT_PRESENT: return "VK_ERROR_FEATURE_NOT_PRESENT";
  case VK_ERROR_FORMAT_NOT_SUPPORTED: return "VK_ERROR_FORMAT_NOT_SUPPORTED";
  case VK_ERROR_DEVICE_LOST: return "VK_ERROR_DEVICE_LOST";
  case VK_ERROR_UNKNOWN: return "VK_ERROR_UNKNOWN";
  default: {
    static char buf[32];
    snprintf(buf, sizeof(buf), "VkResult(%d)", (int)r);
    return buf;
  }
  }
}

struct AllocVariant {
  const char *name;
  bool export_info;   // VkExportMemoryAllocateInfo{OPAQUE_FD}
  bool dedicated;     // VkMemoryDedicatedAllocateInfo{image}
};

static void try_allocs(VkDevice dev, const VkPhysicalDeviceMemoryProperties *mp,
                       VkImage img, const VkMemoryRequirements *reqs,
                       uint32_t type_mask_filter, VkMemoryPropertyFlags want,
                       const char *label)
{
  static const AllocVariant variants[] = {
    { "export+dedicated", true, true },
    { "export only     ", true, false },
    { "dedicated only  ", false, true },
    { "plain           ", false, false },
  };

  for (uint32_t t = 0; t < mp->memoryTypeCount; t++) {
    if (!((reqs->memoryTypeBits & type_mask_filter) & (1u << t)))
      continue;
    VkMemoryPropertyFlags props = mp->memoryTypes[t].propertyFlags;
    if (want && !(props & want))
      continue;

    for (size_t v = 0; v < sizeof(variants) / sizeof(variants[0]); v++) {
      VkMemoryDedicatedAllocateInfo ded = { VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO };
      ded.image = img;

      VkExportMemoryAllocateInfo exp = { VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO };
      exp.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT;

      VkMemoryAllocateInfo ai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO };
      ai.allocationSize = reqs->size;
      ai.memoryTypeIndex = t;

      const void **tail = &ai.pNext;
      if (variants[v].export_info) { *tail = &exp; tail = &exp.pNext; }
      if (variants[v].dedicated)   { *tail = &ded; tail = (const void **)&ded.pNext; }

      VkDeviceMemory mem = VK_NULL_HANDLE;
      VkResult r = vkAllocateMemory(dev, &ai, NULL, &mem);
      printf("  [%s] type=%u props=0x%03x %s -> %s\n", label, t, props,
             variants[v].name, rs(r));
      fflush(stdout);
      if (r == VK_SUCCESS)
        vkFreeMemory(dev, mem, NULL);
    }
  }
}

static VkImage make_image(VkDevice dev, uint32_t w, uint32_t h, bool external,
                          VkResult *out)
{
  VkExternalMemoryImageCreateInfo ext = { VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO };
  ext.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT;

  VkImageCreateInfo ici = { VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO };
  ici.pNext = external ? &ext : NULL;
  ici.imageType = VK_IMAGE_TYPE_2D;
  ici.format = VK_FORMAT_B8G8R8A8_UNORM;
  ici.extent = { w, h, 1 };
  ici.mipLevels = 1;
  ici.arrayLayers = 1;
  ici.samples = VK_SAMPLE_COUNT_1_BIT;
  ici.tiling = VK_IMAGE_TILING_OPTIMAL;
  ici.usage = VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
              VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT;
  ici.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
  ici.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;

  VkImage img = VK_NULL_HANDLE;
  *out = vkCreateImage(dev, &ici, NULL, &img);
  return img;
}

int main(void)
{
  VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO };
  app.pApplicationName = "vk_export_alloc_probe";
  app.apiVersion = VK_API_VERSION_1_1;

  VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO };
  ici.pApplicationInfo = &app;

  VkInstance inst;
  VkResult r = vkCreateInstance(&ici, NULL, &inst);
  printf("vkCreateInstance -> %s\n", rs(r));
  if (r != VK_SUCCESS) return 1;

  uint32_t n = 0;
  vkEnumeratePhysicalDevices(inst, &n, NULL);
  VkPhysicalDevice pds[8];
  if (n > 8) n = 8;
  vkEnumeratePhysicalDevices(inst, &n, pds);
  printf("physical devices: %u\n", n);

  VkPhysicalDevice pd = VK_NULL_HANDLE;
  for (uint32_t i = 0; i < n; i++) {
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(pds[i], &props);
    printf("  [%u] %s\n", i, props.deviceName);
    if (strstr(props.deviceName, "Venus") || strstr(props.deviceName, "Virtio"))
      pd = pds[i];
  }
  if (!pd) { printf("no venus device\n"); return 1; }

  // External buffer/image capability queries, as the app sees them.
  {
    VkPhysicalDeviceExternalBufferInfo bi = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_BUFFER_INFO };
    bi.usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT;
    bi.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT;
    VkExternalBufferProperties bp = { VK_STRUCTURE_TYPE_EXTERNAL_BUFFER_PROPERTIES };
    vkGetPhysicalDeviceExternalBufferProperties(pd, &bi, &bp);
    printf("external buffer OPAQUE_FD: features=0x%x export-from-import=0x%x compat=0x%x\n",
           bp.externalMemoryProperties.externalMemoryFeatures,
           bp.externalMemoryProperties.exportFromImportedHandleTypes,
           bp.externalMemoryProperties.compatibleHandleTypes);

    VkPhysicalDeviceExternalImageFormatInfo eifi = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO };
    eifi.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT;
    VkPhysicalDeviceImageFormatInfo2 ifi = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2 };
    ifi.pNext = &eifi;
    ifi.format = VK_FORMAT_B8G8R8A8_UNORM;
    ifi.type = VK_IMAGE_TYPE_2D;
    ifi.tiling = VK_IMAGE_TILING_OPTIMAL;
    ifi.usage = VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT;
    VkExternalImageFormatProperties eifp = { VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES };
    VkImageFormatProperties2 ifp = { VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2 };
    ifp.pNext = &eifp;
    r = vkGetPhysicalDeviceImageFormatProperties2(pd, &ifi, &ifp);
    printf("external image  OPAQUE_FD: query=%s features=0x%x compat=0x%x\n", rs(r),
           eifp.externalMemoryProperties.externalMemoryFeatures,
           eifp.externalMemoryProperties.compatibleHandleTypes);
  }

  VkPhysicalDeviceMemoryProperties mp;
  vkGetPhysicalDeviceMemoryProperties(pd, &mp);
  printf("memory types: %u\n", mp.memoryTypeCount);
  for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
    printf("  type %u: props=0x%03x heap=%u\n", i, mp.memoryTypes[i].propertyFlags,
           mp.memoryTypes[i].heapIndex);

  // Which external-memory-relevant extensions does the ICD actually expose?
  static const char *want_exts[] = {
    VK_KHR_EXTERNAL_MEMORY_EXTENSION_NAME,
    VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
    VK_KHR_DEDICATED_ALLOCATION_EXTENSION_NAME,
    VK_KHR_GET_MEMORY_REQUIREMENTS_2_EXTENSION_NAME,
    "VK_EXT_external_memory_dma_buf",
    "VK_KHR_external_memory_win32",
  };
  const char *enable_exts[8];
  uint32_t enable_count = 0;
  {
    uint32_t extn = 0;
    vkEnumerateDeviceExtensionProperties(pd, NULL, &extn, NULL);
    VkExtensionProperties *exts =
        (VkExtensionProperties *)calloc(extn, sizeof(*exts));
    vkEnumerateDeviceExtensionProperties(pd, NULL, &extn, exts);
    printf("device extensions: %u total\n", extn);
    for (size_t w = 0; w < sizeof(want_exts) / sizeof(want_exts[0]); w++) {
      bool found = false;
      for (uint32_t i = 0; i < extn; i++) {
        if (!strcmp(exts[i].extensionName, want_exts[w])) { found = true; break; }
      }
      printf("  %-36s %s\n", want_exts[w], found ? "PRESENT" : "absent");
      if (found)
        enable_exts[enable_count++] = want_exts[w];
    }
    free(exts);
  }

  float prio = 1.0f;
  VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO };
  qci.queueFamilyIndex = 0;
  qci.queueCount = 1;
  qci.pQueuePriorities = &prio;

  VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO };
  dci.queueCreateInfoCount = 1;
  dci.pQueueCreateInfos = &qci;
  dci.enabledExtensionCount = enable_count;
  dci.ppEnabledExtensionNames = enable_exts;

  VkDevice dev;
  r = vkCreateDevice(pd, &dci, NULL, &dev);
  printf("vkCreateDevice(%u present exts) -> %s\n", enable_count, rs(r));
  if (r != VK_SUCCESS) return 1;

  const struct { uint32_t w, h; } sizes[] = { { 1896, 1030 }, { 1896, 48 } };
  for (int s = 0; s < 2; s++) {
    for (int external = 1; external >= 0; external--) {
      VkResult cr;
      VkImage img = make_image(dev, sizes[s].w, sizes[s].h, external != 0, &cr);
      char label[64];
      snprintf(label, sizeof(label), "%ux%u %s", sizes[s].w, sizes[s].h,
               external ? "ext-img" : "plain-img");
      printf("%s: vkCreateImage -> %s\n", label, rs(cr));
      if (cr != VK_SUCCESS)
        continue;

      VkMemoryDedicatedRequirements dedr = { VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS };
      VkMemoryRequirements2 mr2 = { VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2 };
      mr2.pNext = &dedr;
      VkImageMemoryRequirementsInfo2 iri = { VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2 };
      iri.image = img;
      vkGetImageMemoryRequirements2(dev, &iri, &mr2);
      printf("%s: reqs size=%llu align=%llu typeBits=0x%x prefersDedicated=%u requiresDedicated=%u\n",
             label, (unsigned long long)mr2.memoryRequirements.size,
             (unsigned long long)mr2.memoryRequirements.alignment,
             mr2.memoryRequirements.memoryTypeBits,
             dedr.prefersDedicatedAllocation, dedr.requiresDedicatedAllocation);

      try_allocs(dev, &mp, img, &mr2.memoryRequirements, ~0u, 0, label);
      vkDestroyImage(dev, img, NULL);
    }
  }

  printf("done\n");
  vkDestroyDevice(dev, NULL);
  vkDestroyInstance(inst, NULL);
  return 0;
}
