// Regression probe for zero-area Win32 Vulkan surfaces.
//
// A transient 120x0 client area previously reached the software WSI image
// allocator and produced a zero-sized Venus buffer/memory bind.  The probe
// verifies that capabilities normalize a partial-zero client area to 0x0,
// swapchain creation fails before allocation, and creation recovers after
// resize.  The deliberately stale create requests exercise driver hardening;
// they are not valid application usage.

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
  LOAD_DEVICE_PROC(vkDestroyDevice);

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

  if (!SetWindowPos(hwnd, NULL, 0, 0, 320, 240,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE)) {
    fprintf(stderr, "FAIL SetWindowPos -> %lu\n", GetLastError());
    return 1;
  }
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
