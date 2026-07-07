// vk_surface_recreate_probe.cpp — WS2 dcomp-vehicle re-create defect probe
// (28th session): a second VkSurface + swapchain on the SAME hwnd, while the
// first surface is still alive, must build a vehicle (previously it FAILED
// stage='dcomp target/visual' hr=0x88980800 — Windows allows one composition
// target per hwnd and the target cache was per-surface; vkd3d creates a new
// VkSurface for the same hwnd on resize/fullscreen, latching Doom fullscreen
// onto the sw path).
//
// Sequence (mirrors the vkd3d shape):
//   Phase A: window + surface A + swapchain A, present ~F frames (vehicle A
//            goes READY then LIVE — async worker).
//   Phase B: surface B on the SAME hwnd (A still alive!) + swapchain B,
//            present ~F frames. Expected post-fix: vehicle B READY + LIVE
//            (tgt_reuse=1). Pre-fix: FAILED hr=0x88980800.
//   Phase C: destroy swapchain A THEN surface A while B keeps presenting
//            (the shared-visual steal: A's teardown must NOT blank B's
//            content — comp->current_swapchain is B). Present ~F/2 more.
//
// Exit code: 0 = all Vulkan plumbing succeeded (the VERDICT is in
// C:\ProgramData\Helios\helios_icd_diag.log READY/LIVE/FAILED lines for this
// pid + the tgt_reuse counter in the HELIOS_WSI_PERF line); 1 = setup
// failure; 2 = present-path failure.
//
// Build (VM, WinLibs g++ — no vulkan-1.lib import lib needed, link the
// loader by name):
//   g++ -O2 -o C:\Users\Rupansh\helios-probe\vk_surface_recreate_probe.exe \
//       Z:\tools\vk_surface_recreate_probe.cpp -lvulkan-1 -luser32
//   (with -I "C:\VulkanSDK\<ver>\Include" -L "C:\VulkanSDK\<ver>\Lib")
// Env: HELIOS_WSI_DCOMP_PRESENT=1 HELIOS_WSI_PERF=1 HELIOS_WSI_PERF_INTERVAL=100
//      HELIOS_WSI_PERF_FILE=C:\ProgramData\Helios\vk_surface_recreate_perf.txt
// Run from SESSION 1 (schtasks) — session 0 windows never reach dwm.

#define VK_USE_PLATFORM_WIN32_KHR
#include <vulkan/vulkan.h>
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CHECK(expr)                                                          \
   do {                                                                      \
      VkResult _r = (expr);                                                  \
      if (_r != VK_SUCCESS && _r != VK_SUBOPTIMAL_KHR) {                     \
         fprintf(stderr, "FAIL %s -> %d (line %d)\n", #expr, (int)_r,        \
                 __LINE__);                                                  \
         fflush(stderr);                                                     \
         exit(2);                                                            \
      }                                                                      \
   } while (0)

static LRESULT CALLBACK
wnd_proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
   return DefWindowProcA(hwnd, msg, wp, lp);
}

static void
pump_messages(void)
{
   MSG msg;
   while (PeekMessageA(&msg, NULL, 0, 0, PM_REMOVE)) {
      TranslateMessage(&msg);
      DispatchMessageA(&msg);
   }
}

struct chain {
   VkSurfaceKHR surface;
   VkSwapchainKHR swapchain;
   uint32_t image_count;
   VkImage images[16];
};

static VkInstance g_instance;
static VkPhysicalDevice g_phys;
static VkDevice g_device;
static VkQueue g_queue;
static uint32_t g_qfam;
static VkCommandPool g_pool;
static VkCommandBuffer g_cmd;
static VkSemaphore g_sem_acquire, g_sem_render;

static void
chain_create(struct chain *c, HWND hwnd)
{
   VkWin32SurfaceCreateInfoKHR sci = {
      VK_STRUCTURE_TYPE_WIN32_SURFACE_CREATE_INFO_KHR};
   sci.hinstance = GetModuleHandleA(NULL);
   sci.hwnd = hwnd;
   CHECK(vkCreateWin32SurfaceKHR(g_instance, &sci, NULL, &c->surface));

   VkBool32 supported = VK_FALSE;
   CHECK(vkGetPhysicalDeviceSurfaceSupportKHR(g_phys, g_qfam, c->surface,
                                              &supported));
   if (!supported) {
      fprintf(stderr, "FAIL surface not presentable on family %u\n", g_qfam);
      exit(1);
   }

   VkSurfaceCapabilitiesKHR caps;
   CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(g_phys, c->surface, &caps));

   VkSwapchainCreateInfoKHR ci = {
      VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR};
   ci.surface = c->surface;
   ci.minImageCount = caps.minImageCount > 3 ? caps.minImageCount : 3;
   ci.imageFormat = VK_FORMAT_B8G8R8A8_UNORM;
   ci.imageColorSpace = VK_COLOR_SPACE_SRGB_NONLINEAR_KHR;
   ci.imageExtent = caps.currentExtent;
   ci.imageArrayLayers = 1;
   ci.imageUsage = VK_IMAGE_USAGE_TRANSFER_DST_BIT |
                   VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
   ci.imageSharingMode = VK_SHARING_MODE_EXCLUSIVE;
   ci.preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR;
   ci.compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR;
   ci.presentMode = VK_PRESENT_MODE_FIFO_KHR;
   ci.clipped = VK_TRUE;
   CHECK(vkCreateSwapchainKHR(g_device, &ci, NULL, &c->swapchain));

   c->image_count = 0;
   CHECK(vkGetSwapchainImagesKHR(g_device, c->swapchain, &c->image_count,
                                 NULL));
   if (c->image_count > 16)
      c->image_count = 16;
   CHECK(vkGetSwapchainImagesKHR(g_device, c->swapchain, &c->image_count,
                                 c->images));
   printf("chain %p: swapchain %ux%u images=%u\n", (void *)c,
          caps.currentExtent.width, caps.currentExtent.height,
          c->image_count);
   fflush(stdout);
}

static void
chain_destroy(struct chain *c)
{
   vkDeviceWaitIdle(g_device);
   vkDestroySwapchainKHR(g_device, c->swapchain, NULL);
   vkDestroySurfaceKHR(g_instance, c->surface, NULL);
   memset(c, 0, sizeof(*c));
}

/* One cleared frame; serialized with a queue-wait-idle (correctness probe,
 * not a perf one). */
static void
present_frame(struct chain *c, uint32_t frame)
{
   uint32_t idx = 0;
   CHECK(vkAcquireNextImageKHR(g_device, c->swapchain, 2000ull * 1000 * 1000,
                               g_sem_acquire, VK_NULL_HANDLE, &idx));

   VkCommandBufferBeginInfo bi = {
      VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
   bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
   CHECK(vkResetCommandBuffer(g_cmd, 0));
   CHECK(vkBeginCommandBuffer(g_cmd, &bi));

   VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
   VkImageMemoryBarrier to_dst = {VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER};
   to_dst.srcAccessMask = 0;
   to_dst.dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
   to_dst.oldLayout = VK_IMAGE_LAYOUT_UNDEFINED;
   to_dst.newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
   to_dst.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
   to_dst.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
   to_dst.image = c->images[idx];
   to_dst.subresourceRange = range;
   vkCmdPipelineBarrier(g_cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                        VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL,
                        1, &to_dst);

   /* Animated color ramp so a frozen display is visually obvious. */
   VkClearColorValue color;
   color.float32[0] = (float)((frame * 5) % 256) / 255.0f;
   color.float32[1] = (float)((frame * 3) % 256) / 255.0f;
   color.float32[2] = (float)((frame * 7) % 256) / 255.0f;
   color.float32[3] = 1.0f;
   vkCmdClearColorImage(g_cmd, c->images[idx],
                        VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &color, 1,
                        &range);

   VkImageMemoryBarrier to_present = to_dst;
   to_present.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
   to_present.dstAccessMask = 0;
   to_present.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
   to_present.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
   vkCmdPipelineBarrier(g_cmd, VK_PIPELINE_STAGE_TRANSFER_BIT,
                        VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0,
                        NULL, 1, &to_present);
   CHECK(vkEndCommandBuffer(g_cmd));

   VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_TRANSFER_BIT;
   VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO};
   si.waitSemaphoreCount = 1;
   si.pWaitSemaphores = &g_sem_acquire;
   si.pWaitDstStageMask = &wait_stage;
   si.commandBufferCount = 1;
   si.pCommandBuffers = &g_cmd;
   si.signalSemaphoreCount = 1;
   si.pSignalSemaphores = &g_sem_render;
   CHECK(vkQueueSubmit(g_queue, 1, &si, VK_NULL_HANDLE));

   VkPresentInfoKHR pi = {VK_STRUCTURE_TYPE_PRESENT_INFO_KHR};
   pi.waitSemaphoreCount = 1;
   pi.pWaitSemaphores = &g_sem_render;
   pi.swapchainCount = 1;
   pi.pSwapchains = &c->swapchain;
   pi.pImageIndices = &idx;
   CHECK(vkQueuePresentKHR(g_queue, &pi));
   CHECK(vkQueueWaitIdle(g_queue));

   pump_messages();
}

/* The vehicle build is ASYNC and slow (nested D3D11CreateDevice on the
 * Helios UMD is seconds) — frame-count phases on the fast sw path finish
 * before the vehicle exists (first probe run: chain A cancelled mid-build,
 * the defect never exercised). Phases are wall-clock, long enough for
 * READY + first-present LIVE. */
static uint32_t
present_for_seconds(struct chain *c, uint32_t seconds, uint32_t frame_base)
{
   ULONGLONG deadline = GetTickCount64() + (ULONGLONG)seconds * 1000;
   uint32_t f = 0;
   while (GetTickCount64() < deadline) {
      present_frame(c, frame_base + f++);
      Sleep(10);
   }
   return f;
}

int
main(int argc, char **argv)
{
   const uint32_t phase_secs = argc > 1 ? (uint32_t)atoi(argv[1]) : 12;

   printf("vk_surface_recreate_probe pid=%lu phase_secs=%u\n",
          (unsigned long)GetCurrentProcessId(), phase_secs);
   fflush(stdout);

   WNDCLASSA wc = {};
   wc.lpfnWndProc = wnd_proc;
   wc.hInstance = GetModuleHandleA(NULL);
   wc.lpszClassName = "HeliosVkRecreateProbe";
   wc.hbrBackground = (HBRUSH)GetStockObject(BLACK_BRUSH);
   RegisterClassA(&wc);
   HWND hwnd = CreateWindowExA(0, wc.lpszClassName,
                               "helios vk_surface_recreate_probe",
                               WS_OVERLAPPEDWINDOW | WS_VISIBLE, 80, 80, 640,
                               480, NULL, NULL, wc.hInstance, NULL);
   if (!hwnd) {
      fprintf(stderr, "FAIL CreateWindowExA\n");
      return 1;
   }
   pump_messages();

   const char *inst_exts[] = {"VK_KHR_surface", "VK_KHR_win32_surface"};
   VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO};
   app.pApplicationName = "vk_surface_recreate_probe";
   app.apiVersion = VK_API_VERSION_1_1;
   VkInstanceCreateInfo ici = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
   ici.pApplicationInfo = &app;
   ici.enabledExtensionCount = 2;
   ici.ppEnabledExtensionNames = inst_exts;
   CHECK(vkCreateInstance(&ici, NULL, &g_instance));

   uint32_t ndev = 1;
   VkResult er = vkEnumeratePhysicalDevices(g_instance, &ndev, &g_phys);
   if ((er != VK_SUCCESS && er != VK_INCOMPLETE) || ndev < 1) {
      fprintf(stderr, "FAIL no physical device (%d)\n", (int)er);
      return 1;
   }
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(g_phys, &props);
   printf("device: %s\n", props.deviceName);
   fflush(stdout);

   uint32_t nfam = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(g_phys, &nfam, NULL);
   VkQueueFamilyProperties fams[16];
   if (nfam > 16)
      nfam = 16;
   vkGetPhysicalDeviceQueueFamilyProperties(g_phys, &nfam, fams);
   g_qfam = UINT32_MAX;
   for (uint32_t i = 0; i < nfam; i++) {
      if (fams[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
         g_qfam = i;
         break;
      }
   }
   if (g_qfam == UINT32_MAX) {
      fprintf(stderr, "FAIL no graphics queue family\n");
      return 1;
   }

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO};
   qci.queueFamilyIndex = g_qfam;
   qci.queueCount = 1;
   qci.pQueuePriorities = &prio;
   const char *dev_exts[] = {"VK_KHR_swapchain"};
   VkDeviceCreateInfo dci = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO};
   dci.queueCreateInfoCount = 1;
   dci.pQueueCreateInfos = &qci;
   dci.enabledExtensionCount = 1;
   dci.ppEnabledExtensionNames = dev_exts;
   CHECK(vkCreateDevice(g_phys, &dci, NULL, &g_device));
   vkGetDeviceQueue(g_device, g_qfam, 0, &g_queue);

   VkCommandPoolCreateInfo pci = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
   pci.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
   pci.queueFamilyIndex = g_qfam;
   CHECK(vkCreateCommandPool(g_device, &pci, NULL, &g_pool));
   VkCommandBufferAllocateInfo cai = {
      VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
   cai.commandPool = g_pool;
   cai.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
   cai.commandBufferCount = 1;
   CHECK(vkAllocateCommandBuffers(g_device, &cai, &g_cmd));
   VkSemaphoreCreateInfo semci = {VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
   CHECK(vkCreateSemaphore(g_device, &semci, NULL, &g_sem_acquire));
   CHECK(vkCreateSemaphore(g_device, &semci, NULL, &g_sem_render));

   /* Phase A */
   struct chain a = {}, b = {};
   uint32_t n;
   printf("phase A: surface+chain A\n");
   fflush(stdout);
   chain_create(&a, hwnd);
   n = present_for_seconds(&a, phase_secs, 0);
   printf("phase A done (%u presents)\n", n);
   fflush(stdout);

   /* Phase B: NEW surface, SAME hwnd, surface A still alive (the vkd3d
    * resize/fullscreen shape that failed 0x88980800 pre-fix). */
   printf("phase B: NEW surface+chain B on the SAME hwnd (A alive)\n");
   fflush(stdout);
   chain_create(&b, hwnd);
   n = present_for_seconds(&b, phase_secs, 100000);
   printf("phase B done (%u presents)\n", n);
   fflush(stdout);

   /* Phase C: tear down A while B is the bound content — B must survive. */
   printf("phase C: destroying chain+surface A under live B\n");
   fflush(stdout);
   chain_destroy(&a);
   n = present_for_seconds(&b, phase_secs / 2, 200000);
   printf("phase C done (%u presents)\n", n);
   fflush(stdout);

   chain_destroy(&b);
   vkDestroySemaphore(g_device, g_sem_acquire, NULL);
   vkDestroySemaphore(g_device, g_sem_render, NULL);
   vkDestroyCommandPool(g_device, g_pool, NULL);
   vkDestroyDevice(g_device, NULL);
   vkDestroyInstance(g_instance, NULL);
   DestroyWindow(hwnd);

   printf("PROBE PASS (plumbing) — verdict: helios_icd_diag.log READY/LIVE "
          "lines for pid=%lu, tgt_reuse in the perf line\n",
          (unsigned long)GetCurrentProcessId());
   fflush(stdout);
   return 0;
}
