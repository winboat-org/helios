/*
 * helios_scanout_usage_probe.c — validate §6.1 (SCANOUT_DRM_MODIFIER_DESIGN.md).
 *
 * The DWM scan-out primary must be a DRM_FORMAT_MOD_LINEAR + DMA_BUF-exportable
 * BGRA image (helios_vk_present.c proved this works for TRANSFER_DST usage). The
 * open question that forks the whole implementation: does the host support that
 * modifier+dmabuf image with the FULL render-target usage a DWM primary needs
 * (COLOR_ATTACHMENT | SAMPLED | TRANSFER_*), or only TRANSFER_DST?
 *
 *   - full usage supported  -> ZERO-COPY: mark the DXVK primary as a modifier
 *                              DMA_BUF image directly.
 *   - only TRANSFER_DST     -> COPY-PATH: render into an OPTIMAL RT, blit into a
 *                              TRANSFER_DST modifier scan-out image per present.
 *
 * This only runs vkGetPhysicalDeviceImageFormatProperties2 (a physical-device
 * query — needs neither a VkDevice nor the dma_buf device ext enabled), so it is
 * safe to run headless in session 0. It prints a table of usage-combo results.
 *
 * Build (mingw, on win11):
 *   gcc -O2 -o C:\Users\Rupansh\helios_scanout_usage_probe.exe \
 *       Z:\icd\win-build\helios_scanout_usage_probe.c -IZ:\icd\mesa\include
 * Run:  $env:VN_DEBUG="init"; .\helios_scanout_usage_probe.exe
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#define DRM_FORMAT_MOD_LINEAR 0ULL

#define ILOAD(name) (PFN_##name)(void *) gipa(inst, #name)

struct usage_case {
   const char *name;
   VkImageUsageFlags usage;
};

/* Query BGRA + DRM_FORMAT_MODIFIER(LINEAR) + DMA_BUF for one usage set. */
static void
probe_usage(PFN_vkGetPhysicalDeviceImageFormatProperties2 pImgFmt2,
            VkPhysicalDevice phys, const char *label, VkImageUsageFlags usage,
            uint64_t modifier)
{
   VkPhysicalDeviceImageDrmFormatModifierInfoEXT mi = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT,
      .drmFormatModifier = modifier,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   VkPhysicalDeviceExternalImageFormatInfo ext = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
      .pNext = &mi,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
   };
   VkPhysicalDeviceImageFormatInfo2 fi = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
      .pNext = &ext,
      .format = VK_FORMAT_B8G8R8A8_UNORM,
      .type = VK_IMAGE_TYPE_2D,
      .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
      .usage = usage,
      .flags = 0,
   };
   VkExternalImageFormatProperties efp = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES
   };
   VkImageFormatProperties2 p2 = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2, .pNext = &efp
   };
   VkResult r = pImgFmt2(phys, &fi, &p2);
   uint32_t feats = efp.externalMemoryProperties.externalMemoryFeatures;
   int exportable = !!(feats & VK_EXTERNAL_MEMORY_FEATURE_EXPORTABLE_BIT);
   fprintf(stderr,
           "  %-38s usage=0x%04x => r=%d feats=0x%x%s%s  %s\n",
           label, usage, r, feats,
           exportable ? " EXPORTABLE" : "",
           (feats & VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT) ? " DEDICATED_ONLY" : "",
           (r == VK_SUCCESS) ? "SUPPORTED" : "unsupported");
   fflush(stderr);
}

int
main(void)
{
   HMODULE vk = LoadLibraryW(L"vulkan-1.dll");
   if (!vk) { fprintf(stderr, "no vulkan-1.dll\n"); return 1; }
   PFN_vkGetInstanceProcAddr gipa =
      (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(vk, "vkGetInstanceProcAddr");
   PFN_vkCreateInstance pCreateInstance =
      (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");

   const VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "helios_scanout_usage_probe",
      .apiVersion = VK_API_VERSION_1_1,
   };
   const VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkInstance inst = VK_NULL_HANDLE;
   if (pCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) {
      fprintf(stderr, "vkCreateInstance failed\n"); return 2;
   }

   PFN_vkEnumeratePhysicalDevices pEnum = ILOAD(vkEnumeratePhysicalDevices);
   PFN_vkGetPhysicalDeviceProperties pProps = ILOAD(vkGetPhysicalDeviceProperties);
   PFN_vkGetPhysicalDeviceImageFormatProperties2 pImgFmt2 =
      ILOAD(vkGetPhysicalDeviceImageFormatProperties2);

   uint32_t count = 0;
   pEnum(inst, &count, NULL);
   VkPhysicalDevice devs[8];
   if (count > 8) count = 8;
   pEnum(inst, &count, devs);
   const char *want_gpu = getenv("HELIOS_GPU");
   if (!want_gpu) want_gpu = "Intel";
   VkPhysicalDevice phys = devs[0];
   for (uint32_t i = 0; i < count; i++) {
      VkPhysicalDeviceProperties pp;
      pProps(devs[i], &pp);
      fprintf(stderr, "  device[%u]: %s\n", i, pp.deviceName);
      if (strstr(pp.deviceName, want_gpu)) phys = devs[i];
   }
   VkPhysicalDeviceProperties props;
   pProps(phys, &props);
   fprintf(stderr, "Using device: \"%s\"\n\n", props.deviceName);

   /* Usage sets, weakest to the full DWM-primary set. The DWM primary needs
    * COLOR_ATTACHMENT (RT) + SAMPLED (compose) + TRANSFER_* (copies). */
   const VkImageUsageFlags DST = VK_IMAGE_USAGE_TRANSFER_DST_BIT;
   const VkImageUsageFlags SRC = VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
   const VkImageUsageFlags COL = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
   const VkImageUsageFlags SMP = VK_IMAGE_USAGE_SAMPLED_BIT;

   struct usage_case cases[] = {
      { "TRANSFER_DST (proven recipe)",        DST },
      { "TRANSFER_DST|SRC",                    DST | SRC },
      { "COLOR_ATTACHMENT",                    COL },
      { "COLOR_ATTACHMENT|TRANSFER_DST|SRC",   COL | DST | SRC },
      { "COLOR_ATTACHMENT|SAMPLED",            COL | SMP },
      { "FULL (COL|SAMPLED|TRANSFER_DST|SRC)", COL | SMP | DST | SRC },
   };

   fprintf(stderr, "== DRM_FORMAT_MOD_LINEAR + DMA_BUF, BGRA ==\n");
   for (uint32_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++)
      probe_usage(pImgFmt2, phys, cases[i].name, cases[i].usage, DRM_FORMAT_MOD_LINEAR);

   fprintf(stderr, "\nDECISION: if FULL is SUPPORTED+EXPORTABLE -> ZERO-COPY primary.\n"
                   "          if only TRANSFER_DST -> COPY-PATH (blit per present).\n");
   return 0;
}
