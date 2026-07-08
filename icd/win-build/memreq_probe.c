/*
 * memreq_probe.c — pin the IMPORTED-OPTIMAL-image memory-requirement asymmetry.
 *
 * Enabling VK_EXT_image_drm_format_modifier + VK_EXT_external_memory_dma_buf on
 * the DXVK device inflated the HOST-reported memory requirement of imported
 * OPTIMAL shared images: a 1896x1030 B8G8R8A8_UNORM surface's EXPORT side needs
 * 7811520 bytes (= 1896*4*1030, tight linear) but the IMPORT-side reconstruction
 * now needs 8773632 (~+12%). DXVK's undersize-import guard then refuses the bind
 * and the desktop goes blank.
 *
 * This probe creates the SAME image under a set of create-info configs that each
 * toggle ONE knob (MUTABLE_FORMAT flag / VkImageFormatListCreateInfo / usage bits
 * / external-memory chaining) and prints vkGetImageMemoryRequirements2 for each,
 * to identify which single knob flips 7811520 <-> 8773632.
 *
 * Device exts: ONLY VK_KHR_external_memory + VK_KHR_external_memory_fd (the venus
 * renderer_handle_type=OPAQUE_FD path). No dma_buf / drm_format_modifier at the
 * device level — every config here is OPTIMAL tiling + OPAQUE_FD external.
 *
 * Build (mingw, on win11):
 *   gcc -O2 -o C:\Users\Rupansh\memreq_probe.exe \
 *       Z:\icd\win-build\memreq_probe.c -IZ:\icd\mesa\include
 * Run (against the installed Helios ICD):
 *   $env:VK_DRIVER_FILES="C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json"
 *   .\memreq_probe.exe
 * Output is written to BOTH stdout and C:\Users\Rupansh\memreq_out.txt (the
 * session-0 SSH pipe does not surface mingw stderr; a file is the reliable sink).
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdarg.h>
#include <string.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#define ILOAD(name) (PFN_##name)(void *) gipa(inst, #name)

static FILE *g_log;
/* dual sink: stdout + a log file (session-0 SSH pipe drops mingw output) */
static void
outf(const char *fmt, ...)
{
   char buf[512];
   va_list ap;
   va_start(ap, fmt);
   vsnprintf(buf, sizeof buf, fmt, ap);
   va_end(ap);
   fputs(buf, stdout);
   fflush(stdout);
   if (g_log) { fputs(buf, g_log); fflush(g_log); }
}

struct cfg {
   const char *name;
   VkImageUsageFlags usage;
   VkImageCreateFlags flags;
   int with_list;     /* chain VkImageFormatListCreateInfo{UNORM,SRGB} */
   int with_external; /* chain VkExternalMemoryImageCreateInfo (fd or dmabuf) */
   VkImageTiling tiling;
   int modifier;      /* use DRM_FORMAT_MODIFIER(LINEAR)+DMA_BUF external path */
   const char *note;
};

int
main(void)
{
   g_log = fopen("C:\\Users\\Rupansh\\memreq_out.txt", "w");

   HMODULE vk = LoadLibraryW(L"vulkan-1.dll");
   if (!vk) { outf("no vulkan-1.dll\n"); return 1; }
   PFN_vkGetInstanceProcAddr gipa =
      (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(vk, "vkGetInstanceProcAddr");
   PFN_vkCreateInstance pCreateInstance =
      (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");

   const VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "memreq_probe",
      .apiVersion = VK_API_VERSION_1_1,
   };
   const VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkInstance inst = VK_NULL_HANDLE;
   if (pCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) {
      outf("vkCreateInstance failed\n"); return 2;
   }

   PFN_vkEnumeratePhysicalDevices pEnum = ILOAD(vkEnumeratePhysicalDevices);
   PFN_vkGetPhysicalDeviceProperties pProps = ILOAD(vkGetPhysicalDeviceProperties);
   PFN_vkCreateDevice pCreateDevice = ILOAD(vkCreateDevice);
   PFN_vkDestroyDevice pDestroyDevice = ILOAD(vkDestroyDevice);
   PFN_vkCreateImage pCreateImage = ILOAD(vkCreateImage);
   PFN_vkDestroyImage pDestroyImage = ILOAD(vkDestroyImage);
   PFN_vkGetImageMemoryRequirements2 pGetMemReq2 =
      ILOAD(vkGetImageMemoryRequirements2);

   uint32_t count = 0;
   pEnum(inst, &count, NULL);
   if (!count) { outf("NO physical devices\n"); return 3; }
   VkPhysicalDevice devs[8];
   if (count > 8) count = 8;
   pEnum(inst, &count, devs);
   for (uint32_t i = 0; i < count; i++) {
      VkPhysicalDeviceProperties pp;
      pProps(devs[i], &pp);
      outf("  device[%u]: %s\n", i, pp.deviceName);
   }
   VkPhysicalDevice phys = devs[0]; /* device[0] = Virtio-GPU Venus */
   VkPhysicalDeviceProperties props;
   pProps(phys, &props);
   outf("Using device[0]: \"%s\"\n\n", props.deviceName);

   /* device: external_memory + fd (OPAQUE_FD venus path) plus dma_buf +
    * drm_format_modifier so the tiling-comparison configs below can run. Per the
    * N_noext result, enabling exts on the device does not change per-image reqs
    * unless the corresponding create-info is chained. */
   const char *devExts[] = {
      VK_KHR_EXTERNAL_MEMORY_EXTENSION_NAME,
      VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
      VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
      VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME,
   };
   float pri = 1.0f;
   VkDeviceQueueCreateInfo q = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = 0, .queueCount = 1, .pQueuePriorities = &pri };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1, .pQueueCreateInfos = &q,
      .enabledExtensionCount = 4, .ppEnabledExtensionNames = devExts };
   VkDevice dev = VK_NULL_HANDLE;
   VkResult dr = pCreateDevice(phys, &dci, NULL, &dev);
   if (dr != VK_SUCCESS || !dev) {
      outf("vkCreateDevice failed r=%d\n", dr); return 4;
   }

   const uint32_t W = 1896, H = 1030;
   outf("image: %ux%u B8G8R8A8_UNORM OPTIMAL 1mip 1layer 1sample EXCLUSIVE\n",
        W, H);
   outf("  tight linear (W*4*H) = %u\n\n", W * 4 * H);

   /* usage bit reference (numeric values are what actually matters):
    *   TRANSFER_SRC=0x1 TRANSFER_DST=0x2 SAMPLED=0x4 STORAGE=0x8 COLOR_ATT=0x10
    *   0x17 = COLOR_ATTACHMENT|SAMPLED|TRANSFER_DST|TRANSFER_SRC
    *   0x06 = SAMPLED|TRANSFER_DST                                            */
   const VkImageTiling OPT = VK_IMAGE_TILING_OPTIMAL;
   const VkImageTiling LIN = VK_IMAGE_TILING_LINEAR;
   struct cfg cfgs[] = {
      /* OPTIMAL tiling: the 4 hypothesized create-info knobs */
      { "E_full",   0x17, VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT, 1, 1, OPT, 0,
        "DXVK EXPORT create-info (mutable + list + external OPAQUE_FD)" },
      { "I_min",    0x17, 0,                                  0, 1, OPT, 0,
        "lossy IMPORT reconstruction (no mutable, no list)" },
      { "I_mut",    0x17, VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT, 0, 1, OPT, 0,
        "mutable but no list (isolates the list)" },
      { "I_list",   0x17, VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT, 1, 1, OPT, 0,
        "== E_full (sanity dup)" },
      { "I_usage6", 0x06, 0,                                  0, 1, OPT, 0,
        "reduced usage 0x6 (isolates usage)" },
      { "N_noext",  0x17, VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT, 1, 0, OPT, 0,
        "no external chaining at all (isolates external-info)" },
      /* tiling comparison: what actually produces the 7811520 tight backing? */
      { "L_lin17",  0x17, 0,                                  0, 1, LIN, 0,
        "LINEAR tiling, full usage 0x17" },
      { "L_lin06",  0x06, 0,                                  0, 1, LIN, 0,
        "LINEAR tiling, reduced usage 0x6" },
      { "M_modlin", 0x17, 0,                                  0, 1, OPT, 1,
        "DRM_FORMAT_MODIFIER(LINEAR) + DMA_BUF (DXVK scanout export path)" },
   };

   VkFormat viewFmts[2] = { VK_FORMAT_B8G8R8A8_UNORM, VK_FORMAT_B8G8R8A8_SRGB };

   outf("%-9s %-8s %-6s %-8s %-5s %-4s  %-12s %-10s %-12s\n",
        "config", "tiling", "usage", "flags", "list", "ext", "size", "alignment", "memTypeBits");
   outf("--------------------------------------------------------------------------------------------------\n");

   uint64_t modLinear = 0; /* DRM_FORMAT_MOD_LINEAR */
   for (uint32_t i = 0; i < sizeof(cfgs) / sizeof(cfgs[0]); i++) {
      struct cfg *c = &cfgs[i];

      VkImageFormatListCreateInfo listCI = {
         .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_LIST_CREATE_INFO,
         .viewFormatCount = 2, .pViewFormats = viewFmts };
      VkImageDrmFormatModifierListCreateInfoEXT modCI = {
         .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
         .drmFormatModifierCount = 1, .pDrmFormatModifiers = &modLinear };
      VkExternalMemoryImageCreateInfo extCI = {
         .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
         .handleTypes = c->modifier
            ? VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT
            : VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT };

      const void *chain = NULL;
      if (c->with_list)     { listCI.pNext = chain; chain = &listCI; }
      if (c->modifier)      { modCI.pNext  = chain; chain = &modCI; }
      if (c->with_external) { extCI.pNext  = chain; chain = &extCI; }

      VkImageCreateInfo ci = {
         .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
         .pNext = chain,
         .flags = c->flags,
         .imageType = VK_IMAGE_TYPE_2D,
         .format = VK_FORMAT_B8G8R8A8_UNORM,
         .extent = { W, H, 1 },
         .mipLevels = 1, .arrayLayers = 1,
         .samples = VK_SAMPLE_COUNT_1_BIT,
         .tiling = c->modifier ? VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT
                               : c->tiling,
         .usage = c->usage,
         .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
         .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };

      const char *tilstr = c->modifier ? "MOD_LIN"
                         : (c->tiling == VK_IMAGE_TILING_LINEAR ? "LINEAR" : "OPTIMAL");

      VkImage img = VK_NULL_HANDLE;
      VkResult r = pCreateImage(dev, &ci, NULL, &img);
      if (r != VK_SUCCESS || !img) {
         outf("%-9s %-8s 0x%02x   0x%-6x %-5d %-4d  vkCreateImage FAILED r=%d  (%s)\n",
              c->name, tilstr, c->usage, c->flags, c->with_list, c->with_external, r, c->note);
         continue;
      }

      VkImageMemoryRequirementsInfo2 ri = {
         .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2, .image = img };
      VkMemoryRequirements2 mr2 = {
         .sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2 };
      pGetMemReq2(dev, &ri, &mr2);

      outf("%-9s %-8s 0x%02x   0x%-6x %-5d %-4d  %-12llu 0x%-8llx 0x%-10x  %s\n",
           c->name, tilstr, c->usage, c->flags, c->with_list, c->with_external,
           (unsigned long long)mr2.memoryRequirements.size,
           (unsigned long long)mr2.memoryRequirements.alignment,
           mr2.memoryRequirements.memoryTypeBits,
           c->note);

      pDestroyImage(dev, img, NULL);
   }

   outf("\nInterpretation:\n"
        "  Compare E_full vs I_min: which knob (flags/list/usage/ext) toggled the\n"
        "  size between 7811520 (tight) and 8773632 (inflated). The knob whose\n"
        "  presence/absence moves the number is the culprit the import path must match.\n");

   pDestroyDevice(dev, NULL);
   return 0;
}
