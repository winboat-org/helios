// Guest-side (win11) validation for the DRM-modifier scan-out primary path.
// Runs THROUGH the Helios venus ICD and actually CREATES the scan-out image the
// DWM primary will be: BGRA + VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT(LINEAR) +
// DMA_BUF external export + dedicated memory + bind + subresource-layout query.
//
// Purpose: with the Option-B scoped bypass in the ICD, does the HOST vkr accept
// a DMA_BUF + explicit-modifier image on this NVIDIA host? A loud Vk error here
// means vkr can't forward explicit modifiers (pivot to Intel); success means the
// full path is viable and the reboot/SDL test is worth it. This tests the one
// unknown the icd/mesa tree can't answer statically.
//
// Build (win11): gcc -O2 -o C:\Users\Rupansh\helios_scanout_create_probe.exe \
//   Z:\icd\win-build\helios_scanout_create_probe.c -IZ:\icd\mesa\include -lvulkan-1
// (use whatever import lib name resolves; loader is vulkan-1.dll)
#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include <vulkan/vulkan_core.h>

#define CK(expr) do { VkResult _r = (expr); printf("  %-52s r=%d%s\n", #expr, _r, _r ? "  <== FAIL" : ""); } while (0)

static const char* feat_str(VkExternalMemoryFeatureFlags f, char* b) {
   b[0]=0;
   if (f & VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT) strcat(b,"DEDICATED_ONLY ");
   if (f & VK_EXTERNAL_MEMORY_FEATURE_EXPORTABLE_BIT)     strcat(b,"EXPORTABLE ");
   if (f & VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT)     strcat(b,"IMPORTABLE ");
   if (!f) strcat(b,"(none)");
   return b;
}

int main(void) {
   const uint32_t W = 1920, H = 1080;
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO };
   VkInstance inst;
   if (vkCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) { printf("vkCreateInstance FAILED\n"); return 1; }

   uint32_t n = 0; vkEnumeratePhysicalDevices(inst, &n, NULL);
   if (!n) { printf("NO physical devices (enumerate returned 0)\n"); return 2; }
   VkPhysicalDevice pds[8]; if (n>8) n=8; vkEnumeratePhysicalDevices(inst, &n, pds);
   VkPhysicalDevice pd = pds[0];
   VkPhysicalDeviceProperties props; vkGetPhysicalDeviceProperties(pd, &props);
   printf("device[0] = %s\n", props.deviceName);

   // ---- 1. caps query for the exact scan-out image ----
   VkPhysicalDeviceExternalImageFormatInfo eii = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
   uint64_t mod = 0; // DRM_FORMAT_MOD_LINEAR
   VkPhysicalDeviceImageDrmFormatModifierInfoEXT mi = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT,
      .drmFormatModifier = mod, .sharingMode = VK_SHARING_MODE_EXCLUSIVE, .pNext = &eii };
   VkPhysicalDeviceImageFormatInfo2 ifi = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2, .pNext = &mi,
      .format = VK_FORMAT_B8G8R8A8_UNORM, .type = VK_IMAGE_TYPE_2D,
      .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
      .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_SAMPLED_BIT
             | VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT };
   VkExternalImageFormatProperties efp = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES };
   VkImageFormatProperties2 ifp = { .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2, .pNext = &efp };
   VkResult qr = vkGetPhysicalDeviceImageFormatProperties2(pd, &ifi, &ifp);
   char b[128];
   printf("1. caps query (BGRA, MODIFIER(LINEAR), DMA_BUF, full usage): r=%d feats=[%s]\n",
          qr, feat_str(efp.externalMemoryProperties.externalMemoryFeatures, b));

   // ---- 2. create device with the scan-out extensions ----
   const char* devExts[] = {
      VK_KHR_EXTERNAL_MEMORY_EXTENSION_NAME,
      VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
      VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
      VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME,
   };
   float pri = 1.0f;
   VkDeviceQueueCreateInfo q = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = 0, .queueCount = 1, .pQueuePriorities = &pri };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &q,
      .enabledExtensionCount = 4, .ppEnabledExtensionNames = devExts };
   VkDevice dev;
   printf("2. vkCreateDevice (external_memory + fd + dma_buf + drm_format_modifier):\n");
   CK(vkCreateDevice(pd, &dci, NULL, &dev));
   if (!dev) { printf("   device NULL -> abort\n"); return 3; }

   // ---- 3. create the DMA_BUF + explicit-modifier image ----
   VkImageDrmFormatModifierListCreateInfoEXT modList = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
      .drmFormatModifierCount = 1, .pDrmFormatModifiers = &mod };
   VkExternalMemoryImageCreateInfo emi = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
      .pNext = &modList,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
   VkImageCreateInfo ci = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .pNext = &emi,
      .imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
      .extent = { W, H, 1 }, .mipLevels = 1, .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
      .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_SAMPLED_BIT
             | VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE, .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
   VkImage img = VK_NULL_HANDLE;
   printf("3. vkCreateImage (DMA_BUF + MODIFIER(LINEAR)):\n");
   CK(vkCreateImage(dev, &ci, NULL, &img));
   if (!img) { printf("   image NULL -> abort (vkr rejected the modifier image)\n"); return 4; }

   // ---- 4. allocate dedicated DMA_BUF export memory + bind ----
   VkMemoryRequirements mr; vkGetImageMemoryRequirements(dev, img, &mr);
   VkPhysicalDeviceMemoryProperties mp; vkGetPhysicalDeviceMemoryProperties(pd, &mp);
   int mti = -1, dl = -1;
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++) if (mr.memoryTypeBits & (1u<<i)) {
      if (mti < 0) mti = (int)i;
      if (dl < 0 && (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) dl = (int)i;
   }
   if (dl >= 0) mti = dl;
   printf("4. mem: size=%llu memoryTypeBits=0x%x -> type %d (device_local=%s)\n",
          (unsigned long long)mr.size, mr.memoryTypeBits, mti,
          (mti>=0 && (mp.memoryTypes[mti].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) ? "yes":"no");
   VkMemoryDedicatedAllocateInfo ded = { .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO, .image = img };
   VkExportMemoryAllocateInfo exp = { .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
      .pNext = &ded, .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &exp,
      .allocationSize = mr.size, .memoryTypeIndex = (uint32_t)mti };
   VkDeviceMemory mem = VK_NULL_HANDLE;
   CK(vkAllocateMemory(dev, &mai, NULL, &mem));
   if (mem) CK(vkBindImageMemory(dev, img, mem, 0));

   // ---- 5. subresource layout (MEMORY_PLANE_0) -> the real stride/offset ----
   if (mem) {
      VkImageSubresource sub = { .aspectMask = VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT, .mipLevel = 0, .arrayLayer = 0 };
      VkSubresourceLayout sl; memset(&sl, 0, sizeof sl);
      vkGetImageSubresourceLayout(dev, img, &sub, &sl);
      printf("5. subresource layout MEMORY_PLANE_0: offset=%llu rowPitch=%llu size=%llu\n",
             (unsigned long long)sl.offset, (unsigned long long)sl.rowPitch, (unsigned long long)sl.size);
      printf("   (expect rowPitch >= %u; width*4 = %u)\n", W*4, W*4);
   }

   printf("\nRESULT: if steps 3+4+5 all r=0, vkr accepts the DMA_BUF+modifier scan-out image on this host.\n");
   return 0;
}
