// Regression probe for Win32 Vulkan surface extent changes.
//
// A transient 120x0 client area previously reached the software WSI image
// allocator and produced a zero-sized Venus buffer/memory bind.  The probe
// verifies that capabilities normalize a partial-zero client area to 0x0,
// swapchain creation fails before allocation, live swapchains become
// OUT_OF_DATE after resize or a zero-area transition, and creation recovers
// after restore.  The deliberately stale create requests exercise driver
// hardening; they are not valid application usage.

#define VK_USE_PLATFORM_WIN32_KHR
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>
#include <windows.h>

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#define CHECK_VK(expr)                                                         \
  do {                                                                         \
    VkResult result = (expr);                                                  \
    if (result != VK_SUCCESS) {                                                \
      fprintf(stderr, "FAIL %s -> %d (line %d)\n", #expr, (int)result,         \
              __LINE__);                                                       \
      exit(1);                                                                 \
    }                                                                          \
  } while (0)

#define LOAD_INSTANCE_PROC(name)                                               \
  PFN_##name name = (PFN_##name)(void *)get_instance_proc(instance, #name);    \
  if (!name) {                                                                 \
    fprintf(stderr, "FAIL missing instance proc %s\n", #name);                 \
    return 1;                                                                  \
  }

#define LOAD_DEVICE_PROC(name)                                                 \
  PFN_##name name = (PFN_##name)(void *)get_device_proc(device, #name);        \
  if (!name) {                                                                 \
    fprintf(stderr, "FAIL missing device proc %s\n", #name);                   \
    return 1;                                                                  \
  }

static LRESULT CALLBACK window_proc(HWND hwnd, UINT message, WPARAM wparam,
                                    LPARAM lparam) {
  return DefWindowProcA(hwnd, message, wparam, lparam);
}

static void expect_extent(const char *name, VkExtent2D actual, uint32_t width,
                          uint32_t height) {
  if (actual.width != width || actual.height != height) {
    fprintf(stderr, "FAIL %s is %ux%u, expected %ux%u\n", name, actual.width,
            actual.height, width, height);
    exit(1);
  }
}

static void expect_result(const char *name, VkResult actual,
                          VkResult expected) {
  if (actual != expected) {
    fprintf(stderr, "FAIL %s -> %d, expected %d\n", name, (int)actual,
            (int)expected);
    exit(1);
  }
}

static void resize_client(HWND hwnd, uint32_t width, uint32_t height) {
  if (!SetWindowPos(hwnd, NULL, 0, 0, width, height,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE)) {
    fprintf(stderr, "FAIL SetWindowPos -> %lu\n", GetLastError());
    exit(1);
  }

  RECT rect;
  if (!GetClientRect(hwnd, &rect)) {
    fprintf(stderr, "FAIL GetClientRect -> %lu\n", GetLastError());
    exit(1);
  }
  expect_extent("resized client", {(uint32_t)(rect.right - rect.left),
                                    (uint32_t)(rect.bottom - rect.top)},
                width, height);
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s <vulkan ICD DLL>\n", argv[0]);
    return 2;
  }

  HMODULE icd = LoadLibraryA(argv[1]);
  if (!icd) {
    fprintf(stderr, "FAIL LoadLibraryA(%s) -> %lu\n", argv[1], GetLastError());
    return 1;
  }
  PFN_vkGetInstanceProcAddr get_instance_proc =
      (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(
          icd, "vk_icdGetInstanceProcAddr");
  if (!get_instance_proc) {
    fprintf(stderr, "FAIL missing vk_icdGetInstanceProcAddr\n");
    return 1;
  }
  PFN_vkCreateInstance vkCreateInstance =
      (PFN_vkCreateInstance)(void *)get_instance_proc(VK_NULL_HANDLE,
                                                      "vkCreateInstance");
  if (!vkCreateInstance) {
    fprintf(stderr, "FAIL missing vkCreateInstance\n");
    return 1;
  }

  HINSTANCE hinstance = GetModuleHandleA(NULL);
  WNDCLASSA window_class = {};
  window_class.lpfnWndProc = window_proc;
  window_class.hInstance = hinstance;
  window_class.lpszClassName = "HeliosVkZeroExtentProbe";
  if (!RegisterClassA(&window_class) &&
      GetLastError() != ERROR_CLASS_ALREADY_EXISTS) {
    fprintf(stderr, "FAIL RegisterClassA -> %lu\n", GetLastError());
    return 1;
  }

  /* WS_POPUP makes the requested outer and client dimensions identical. */
  HWND hwnd = CreateWindowExA(0, window_class.lpszClassName,
                              "helios vk zero-extent probe", WS_POPUP, 0, 0,
                              120, 0, NULL, NULL, hinstance, NULL);
  if (!hwnd) {
    fprintf(stderr, "FAIL CreateWindowExA -> %lu\n", GetLastError());
    return 1;
  }

  RECT rect;
  if (!GetClientRect(hwnd, &rect)) {
    fprintf(stderr, "FAIL GetClientRect -> %lu\n", GetLastError());
    return 1;
  }
  printf("zero-area client: %ldx%ld\n", rect.right - rect.left,
         rect.bottom - rect.top);
  if (rect.right - rect.left <= 0 || rect.bottom - rect.top != 0) {
    fprintf(
        stderr,
        "FAIL probe could not create the required partial-zero client area\n");
    return 1;
  }

  const char *instance_extensions[] = {
      VK_KHR_SURFACE_EXTENSION_NAME,
      VK_KHR_WIN32_SURFACE_EXTENSION_NAME,
  };
  VkApplicationInfo app_info = {};
  app_info.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
  app_info.pApplicationName = "vk_zero_extent_probe";
  app_info.apiVersion = VK_API_VERSION_1_1;
  VkInstanceCreateInfo instance_info = {};
  instance_info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
  instance_info.pApplicationInfo = &app_info;
  instance_info.enabledExtensionCount = 2;
  instance_info.ppEnabledExtensionNames = instance_extensions;

  VkInstance instance = VK_NULL_HANDLE;
  CHECK_VK(vkCreateInstance(&instance_info, NULL, &instance));

  LOAD_INSTANCE_PROC(vkCreateWin32SurfaceKHR);
  LOAD_INSTANCE_PROC(vkDestroySurfaceKHR);
  LOAD_INSTANCE_PROC(vkDestroyInstance);
  LOAD_INSTANCE_PROC(vkEnumeratePhysicalDevices);
  LOAD_INSTANCE_PROC(vkGetPhysicalDeviceProperties);
  LOAD_INSTANCE_PROC(vkGetPhysicalDeviceQueueFamilyProperties);
  LOAD_INSTANCE_PROC(vkGetPhysicalDeviceSurfaceSupportKHR);
  LOAD_INSTANCE_PROC(vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
  LOAD_INSTANCE_PROC(vkGetPhysicalDeviceSurfaceFormatsKHR);
  LOAD_INSTANCE_PROC(vkCreateDevice);
  LOAD_INSTANCE_PROC(vkGetDeviceProcAddr);

  VkWin32SurfaceCreateInfoKHR surface_info = {};
  surface_info.sType = VK_STRUCTURE_TYPE_WIN32_SURFACE_CREATE_INFO_KHR;
  surface_info.hinstance = hinstance;
  surface_info.hwnd = hwnd;
  VkSurfaceKHR surface = VK_NULL_HANDLE;
  CHECK_VK(vkCreateWin32SurfaceKHR(instance, &surface_info, NULL, &surface));

  uint32_t physical_count = 0;
  CHECK_VK(vkEnumeratePhysicalDevices(instance, &physical_count, NULL));
  if (physical_count == 0) {
    fprintf(stderr, "FAIL no Vulkan physical device\n");
    return 1;
  }
  VkPhysicalDevice *physical_devices =
      (VkPhysicalDevice *)calloc(physical_count, sizeof(*physical_devices));
  CHECK_VK(
      vkEnumeratePhysicalDevices(instance, &physical_count, physical_devices));

  VkPhysicalDevice physical_device = VK_NULL_HANDLE;
  uint32_t queue_family = UINT32_MAX;
  for (uint32_t device_index = 0; device_index < physical_count;
       device_index++) {
    uint32_t queue_count = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(physical_devices[device_index],
                                             &queue_count, NULL);
    VkQueueFamilyProperties *queues =
        (VkQueueFamilyProperties *)calloc(queue_count, sizeof(*queues));
    vkGetPhysicalDeviceQueueFamilyProperties(physical_devices[device_index],
                                             &queue_count, queues);
    for (uint32_t i = 0; i < queue_count; i++) {
      VkBool32 present = VK_FALSE;
      CHECK_VK(vkGetPhysicalDeviceSurfaceSupportKHR(
          physical_devices[device_index], i, surface, &present));
      if (present && (queues[i].queueFlags & VK_QUEUE_GRAPHICS_BIT)) {
        physical_device = physical_devices[device_index];
        queue_family = i;
        break;
      }
    }
    free(queues);
    if (physical_device != VK_NULL_HANDLE)
      break;
  }
  free(physical_devices);
  if (queue_family == UINT32_MAX) {
    fprintf(stderr, "FAIL no graphics/present queue family\n");
    return 1;
  }

  VkPhysicalDeviceProperties properties;
  vkGetPhysicalDeviceProperties(physical_device, &properties);
  printf("device: %s\n", properties.deviceName);

  VkSurfaceCapabilitiesKHR capabilities;
  CHECK_VK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(physical_device, surface,
                                                     &capabilities));
  expect_extent("currentExtent", capabilities.currentExtent, 0, 0);
  expect_extent("minImageExtent", capabilities.minImageExtent, 0, 0);
  expect_extent("maxImageExtent", capabilities.maxImageExtent, 0, 0);

  uint32_t format_count = 0;
  CHECK_VK(vkGetPhysicalDeviceSurfaceFormatsKHR(physical_device, surface,
                                                &format_count, NULL));
  if (format_count == 0) {
    fprintf(stderr, "FAIL surface has no formats\n");
    return 1;
  }
  VkSurfaceFormatKHR *formats =
      (VkSurfaceFormatKHR *)calloc(format_count, sizeof(*formats));
  CHECK_VK(vkGetPhysicalDeviceSurfaceFormatsKHR(physical_device, surface,
                                                &format_count, formats));

  float priority = 1.0f;
  VkDeviceQueueCreateInfo queue_info = {};
  queue_info.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO;
  queue_info.queueFamilyIndex = queue_family;
  queue_info.queueCount = 1;
  queue_info.pQueuePriorities = &priority;
  const char *device_extensions[] = {VK_KHR_SWAPCHAIN_EXTENSION_NAME};
  VkDeviceCreateInfo device_info = {};
  device_info.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
  device_info.queueCreateInfoCount = 1;
  device_info.pQueueCreateInfos = &queue_info;
  device_info.enabledExtensionCount = 1;
  device_info.ppEnabledExtensionNames = device_extensions;
  VkDevice device = VK_NULL_HANDLE;
  CHECK_VK(vkCreateDevice(physical_device, &device_info, NULL, &device));
  PFN_vkGetDeviceProcAddr get_device_proc = vkGetDeviceProcAddr;
  LOAD_DEVICE_PROC(vkCreateSwapchainKHR);
  LOAD_DEVICE_PROC(vkDestroySwapchainKHR);
  LOAD_DEVICE_PROC(vkGetSwapchainImagesKHR);
  LOAD_DEVICE_PROC(vkAcquireNextImageKHR);
  LOAD_DEVICE_PROC(vkCreateSemaphore);
  LOAD_DEVICE_PROC(vkDestroySemaphore);
  LOAD_DEVICE_PROC(vkCreateCommandPool);
  LOAD_DEVICE_PROC(vkDestroyCommandPool);
  LOAD_DEVICE_PROC(vkAllocateCommandBuffers);
  LOAD_DEVICE_PROC(vkBeginCommandBuffer);
  LOAD_DEVICE_PROC(vkEndCommandBuffer);
  LOAD_DEVICE_PROC(vkCmdPipelineBarrier);
  LOAD_DEVICE_PROC(vkGetDeviceQueue);
  LOAD_DEVICE_PROC(vkQueueSubmit);
  LOAD_DEVICE_PROC(vkQueuePresentKHR);
  LOAD_DEVICE_PROC(vkQueueWaitIdle);
  LOAD_DEVICE_PROC(vkDestroyDevice);

  VkQueue queue = VK_NULL_HANDLE;
  vkGetDeviceQueue(device, queue_family, 0, &queue);

  VkSwapchainCreateInfoKHR swapchain_info = {};
  swapchain_info.sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR;
  swapchain_info.surface = surface;
  swapchain_info.minImageCount = capabilities.minImageCount;
  swapchain_info.imageFormat = formats[0].format;
  swapchain_info.imageColorSpace = formats[0].colorSpace;
  swapchain_info.imageExtent = {320, 240}; /* Deliberately stale/nonzero. */
  swapchain_info.imageArrayLayers = 1;
  swapchain_info.imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
  swapchain_info.imageSharingMode = VK_SHARING_MODE_EXCLUSIVE;
  swapchain_info.preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR;
  swapchain_info.compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR;
  swapchain_info.presentMode = VK_PRESENT_MODE_FIFO_KHR;
  swapchain_info.clipped = VK_TRUE;

  VkSwapchainKHR swapchain = VK_NULL_HANDLE;
  VkResult result =
      vkCreateSwapchainKHR(device, &swapchain_info, NULL, &swapchain);
  expect_result("zero-area vkCreateSwapchainKHR", result,
                VK_ERROR_INITIALIZATION_FAILED);
  printf("zero-area swapchain rejected with VK_ERROR_INITIALIZATION_FAILED\n");

  resize_client(hwnd, 320, 240);
  CHECK_VK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(physical_device, surface,
                                                     &capabilities));
  expect_extent("restored currentExtent", capabilities.currentExtent, 320, 240);
  expect_extent("restored minImageExtent", capabilities.minImageExtent, 320,
                240);
  expect_extent("restored maxImageExtent", capabilities.maxImageExtent, 320,
                240);

  swapchain_info.imageExtent = {319, 240};
  result = vkCreateSwapchainKHR(device, &swapchain_info, NULL, &swapchain);
  expect_result("stale-size vkCreateSwapchainKHR", result,
                VK_ERROR_INITIALIZATION_FAILED);

  swapchain_info.imageExtent = capabilities.currentExtent;
  swapchain_info.minImageCount = capabilities.minImageCount;
  CHECK_VK(vkCreateSwapchainKHR(device, &swapchain_info, NULL, &swapchain));
  printf("restored swapchain created at %ux%u\n",
         capabilities.currentExtent.width, capabilities.currentExtent.height);

  VkSemaphoreCreateInfo semaphore_info = {};
  semaphore_info.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO;
  VkSemaphore semaphore = VK_NULL_HANDLE;
  CHECK_VK(vkCreateSemaphore(device, &semaphore_info, NULL, &semaphore));

  resize_client(hwnd, 400, 300);

  uint32_t image_index = UINT32_MAX;
  result = vkAcquireNextImageKHR(device, swapchain, 0, semaphore,
                                 VK_NULL_HANDLE, &image_index);
  expect_result("resized vkAcquireNextImageKHR", result,
                VK_ERROR_OUT_OF_DATE_KHR);
  printf("live resize invalidated the old swapchain\n");
  vkDestroySemaphore(device, semaphore, NULL);

  resize_client(hwnd, 320, 240);
  CHECK_VK(vkCreateSemaphore(device, &semaphore_info, NULL, &semaphore));
  image_index = UINT32_MAX;
  result = vkAcquireNextImageKHR(device, swapchain, 0, semaphore,
                                 VK_NULL_HANDLE, &image_index);
  expect_result("restored-size old vkAcquireNextImageKHR", result,
                VK_ERROR_OUT_OF_DATE_KHR);
  printf("old swapchain remained invalid after restoring its original size\n");
  vkDestroySemaphore(device, semaphore, NULL);
  vkDestroySwapchainKHR(device, swapchain, NULL);

  resize_client(hwnd, 400, 300);
  CHECK_VK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(physical_device, surface,
                                                     &capabilities));
  expect_extent("resized currentExtent", capabilities.currentExtent, 400, 300);
  swapchain_info.imageExtent = capabilities.currentExtent;
  swapchain_info.minImageCount = capabilities.minImageCount;
  CHECK_VK(vkCreateSwapchainKHR(device, &swapchain_info, NULL, &swapchain));
  CHECK_VK(vkCreateSemaphore(device, &semaphore_info, NULL, &semaphore));
  VkSemaphore present_ready = VK_NULL_HANDLE;
  CHECK_VK(
      vkCreateSemaphore(device, &semaphore_info, NULL, &present_ready));

  image_index = UINT32_MAX;
  CHECK_VK(vkAcquireNextImageKHR(device, swapchain, 0, semaphore,
                                 VK_NULL_HANDLE, &image_index));

  uint32_t swapchain_image_count = 0;
  CHECK_VK(vkGetSwapchainImagesKHR(device, swapchain, &swapchain_image_count,
                                   NULL));
  VkImage *swapchain_images =
      (VkImage *)calloc(swapchain_image_count, sizeof(*swapchain_images));
  if (!swapchain_images) {
    fprintf(stderr, "FAIL allocating swapchain image list\n");
    return 1;
  }
  CHECK_VK(vkGetSwapchainImagesKHR(device, swapchain, &swapchain_image_count,
                                   swapchain_images));

  VkCommandPoolCreateInfo command_pool_info = {};
  command_pool_info.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO;
  command_pool_info.queueFamilyIndex = queue_family;
  VkCommandPool command_pool = VK_NULL_HANDLE;
  CHECK_VK(vkCreateCommandPool(device, &command_pool_info, NULL,
                               &command_pool));

  VkCommandBufferAllocateInfo command_buffer_info = {};
  command_buffer_info.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO;
  command_buffer_info.commandPool = command_pool;
  command_buffer_info.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
  command_buffer_info.commandBufferCount = 1;
  VkCommandBuffer command_buffer = VK_NULL_HANDLE;
  CHECK_VK(vkAllocateCommandBuffers(device, &command_buffer_info,
                                     &command_buffer));

  VkCommandBufferBeginInfo begin_info = {};
  begin_info.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
  begin_info.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
  CHECK_VK(vkBeginCommandBuffer(command_buffer, &begin_info));

  VkImageMemoryBarrier present_barrier = {};
  present_barrier.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER;
  present_barrier.oldLayout = VK_IMAGE_LAYOUT_UNDEFINED;
  present_barrier.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
  present_barrier.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
  present_barrier.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
  present_barrier.image = swapchain_images[image_index];
  present_barrier.subresourceRange.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT;
  present_barrier.subresourceRange.baseMipLevel = 0;
  present_barrier.subresourceRange.levelCount = 1;
  present_barrier.subresourceRange.baseArrayLayer = 0;
  present_barrier.subresourceRange.layerCount = 1;
  vkCmdPipelineBarrier(command_buffer, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                       VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0,
                       NULL, 1, &present_barrier);
  CHECK_VK(vkEndCommandBuffer(command_buffer));

  VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT;
  VkSubmitInfo submit_info = {};
  submit_info.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO;
  submit_info.waitSemaphoreCount = 1;
  submit_info.pWaitSemaphores = &semaphore;
  submit_info.pWaitDstStageMask = &wait_stage;
  submit_info.commandBufferCount = 1;
  submit_info.pCommandBuffers = &command_buffer;
  submit_info.signalSemaphoreCount = 1;
  submit_info.pSignalSemaphores = &present_ready;
  CHECK_VK(vkQueueSubmit(queue, 1, &submit_info, VK_NULL_HANDLE));
  CHECK_VK(vkQueueWaitIdle(queue));

  resize_client(hwnd, 420, 310);

  VkResult per_swapchain_result = VK_SUCCESS;
  VkPresentInfoKHR present_info = {};
  present_info.sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR;
  present_info.waitSemaphoreCount = 1;
  present_info.pWaitSemaphores = &present_ready;
  present_info.swapchainCount = 1;
  present_info.pSwapchains = &swapchain;
  present_info.pImageIndices = &image_index;
  present_info.pResults = &per_swapchain_result;
  result = vkQueuePresentKHR(queue, &present_info);
  expect_result("resized vkQueuePresentKHR", result,
                VK_ERROR_OUT_OF_DATE_KHR);
  expect_result("resized vkQueuePresentKHR pResults", per_swapchain_result,
                VK_ERROR_OUT_OF_DATE_KHR);
  CHECK_VK(vkQueueWaitIdle(queue));
  printf("live resize invalidated an acquired image at present\n");
  vkDestroyCommandPool(device, command_pool, NULL);
  free(swapchain_images);
  vkDestroySemaphore(device, present_ready, NULL);
  vkDestroySemaphore(device, semaphore, NULL);
  vkDestroySwapchainKHR(device, swapchain, NULL);

  CHECK_VK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(physical_device, surface,
                                                     &capabilities));
  expect_extent("present-resized currentExtent", capabilities.currentExtent,
                420, 310);
  swapchain_info.imageExtent = capabilities.currentExtent;
  swapchain_info.minImageCount = capabilities.minImageCount;
  CHECK_VK(vkCreateSwapchainKHR(device, &swapchain_info, NULL, &swapchain));
  CHECK_VK(vkCreateSemaphore(device, &semaphore_info, NULL, &semaphore));

  resize_client(hwnd, 120, 0);

  image_index = UINT32_MAX;
  result = vkAcquireNextImageKHR(device, swapchain, 0, semaphore,
                                 VK_NULL_HANDLE, &image_index);
  expect_result("zero-area vkAcquireNextImageKHR", result,
                VK_ERROR_OUT_OF_DATE_KHR);
  printf("zero-area resize invalidated the old swapchain\n");
  vkDestroySemaphore(device, semaphore, NULL);
  vkDestroySwapchainKHR(device, swapchain, NULL);

  CHECK_VK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(physical_device, surface,
                                                     &capabilities));
  expect_extent("zero-area currentExtent", capabilities.currentExtent, 0, 0);

  resize_client(hwnd, 360, 260);

  CHECK_VK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(physical_device, surface,
                                                     &capabilities));
  expect_extent("final currentExtent", capabilities.currentExtent, 360, 260);
  swapchain_info.imageExtent = capabilities.currentExtent;
  swapchain_info.minImageCount = capabilities.minImageCount;
  CHECK_VK(vkCreateSwapchainKHR(device, &swapchain_info, NULL, &swapchain));
  printf("swapchain creation recovered after restore at %ux%u\n",
         capabilities.currentExtent.width, capabilities.currentExtent.height);

  vkDestroySwapchainKHR(device, swapchain, NULL);
  vkDestroyDevice(device, NULL);
  free(formats);
  vkDestroySurfaceKHR(instance, surface, NULL);
  vkDestroyInstance(instance, NULL);
  DestroyWindow(hwnd);
  FreeLibrary(icd);
  printf("PASS\n");
  return 0;
}
