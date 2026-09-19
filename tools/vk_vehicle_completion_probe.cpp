// Regression: a delayed producer must keep the vehicle source unavailable,
// then become acquirable after the helper copy completes. No queue/device-idle
// waits. Run in the interactive session with async WSI and the vehicle enabled.
// Build: x86_64-w64-mingw32-g++ -std=c++17 -O2 -static -Iicd/mesa/include
//        tools/vk_vehicle_completion_probe.cpp -luser32 -ladvapi32 -o <probe.exe>
// Usage: probe.exe <exact ICD DLL>. Require this PID's vehicle LIVE and
// copy-pending/completed diagnostics as well as PASS; GDI cannot pass the gate.
// --teardown-close / --teardown-resize cancel the window while the delayed
// copy is pending. Application work is fenced before destroying the chain;
// require matching helper retirement and no leaked objects in the host trace.
#define VK_USE_PLATFORM_WIN32_KHR
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>
#include <windows.h>
#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>

#define REQUIRE(expr) do { if (!(expr)) { \
  std::fprintf(stderr, "FAIL line %d: %s\n", __LINE__, #expr); std::exit(1); } } while (0)
#define CHECK(expr) do { VkResult r = (expr); if (r != VK_SUCCESS) { \
  std::fprintf(stderr, "FAIL line %d: %s -> %d\n", __LINE__, #expr, r); std::exit(1); } } while (0)
#define INSTANCE_PROC(name) auto name = reinterpret_cast<PFN_##name>(get(instance, #name)); REQUIRE(name)
#define DEVICE_PROC(name) auto name = reinterpret_cast<PFN_##name>(vkGetDeviceProcAddr(device, #name)); REQUIRE(name)

static LRESULT CALLBACK window_proc(HWND h, UINT m, WPARAM w, LPARAM l) {
  return DefWindowProcA(h, m, w, l);
}

static void pump() {
  MSG message;
  while (PeekMessageA(&message, nullptr, 0, 0, PM_REMOVE)) {
    TranslateMessage(&message);
    DispatchMessageA(&message);
  }
}

int main(int argc, char** argv) {
  const bool close_pending = argc == 3 && !std::strcmp(argv[2], "--teardown-close");
  const bool resize_pending = argc == 3 && !std::strcmp(argv[2], "--teardown-resize");
  const bool teardown = close_pending || resize_pending;
  const bool pattern = argc == 7 && !std::strcmp(argv[2], "--present-pattern");
  const bool queued = pattern || (argc == 7 && !std::strcmp(argv[2], "--present-queued"));
  const bool loop = queued || (argc == 7 && !std::strcmp(argv[2], "--present-loop"));
  if (argc != 2 && !loop && !teardown) {
    std::fprintf(stderr, "usage: %s ICD [--teardown-close|--teardown-resize] | ICD [--present-loop|--present-queued|--present-pattern srgb|unorm WIDTH HEIGHT FRAMES]\n", argv[0]);
    return 2;
  }
  auto positive = [](const char* text, unsigned max) {
    char* end = nullptr;
    const auto value = std::strtoul(text, &end, 10);
    REQUIRE(end && !*end && value > 0 && value <= max);
    return uint32_t(value);
  };
  const auto width = loop ? positive(argv[4], 4096) : 640u;
  const auto height = loop ? positive(argv[5], 4096) : 480u;
  const auto frames = loop ? positive(argv[6], 10000) : 0u;
  REQUIRE(!pattern || (width >= 16 && height >= 20));
  REQUIRE(!loop || (!std::strcmp(argv[3], "srgb") || !std::strcmp(argv[3], "unorm")));
  const auto wanted_format = loop && !std::strcmp(argv[3], "srgb")
    ? VK_FORMAT_B8G8R8A8_SRGB : VK_FORMAT_B8G8R8A8_UNORM;
  DWORD session = 0;
  REQUIRE(ProcessIdToSessionId(GetCurrentProcessId(), &session) && session != 0);
  HANDLE token = nullptr;
  REQUIRE(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token));
  TOKEN_ELEVATION elevation = {};
  DWORD token_size = 0;
  REQUIRE(GetTokenInformation(token, TokenElevation, &elevation, sizeof(elevation), &token_size));
  CloseHandle(token);
  REQUIRE(!elevation.TokenIsElevated);
  std::printf("vehicle completion probe pid=%lu session=%lu\n", GetCurrentProcessId(), session);
  std::fflush(stdout);
  HMODULE module = LoadLibraryA(argv[1]);
  REQUIRE(module);
  char actual_path[MAX_PATH], expected_path[MAX_PATH];
  const auto actual_len = GetModuleFileNameA(module, actual_path, MAX_PATH);
  const auto expected_len = GetFullPathNameA(argv[1], MAX_PATH, expected_path, nullptr);
  REQUIRE(actual_len && actual_len < MAX_PATH && expected_len && expected_len < MAX_PATH);
  REQUIRE(!_stricmp(actual_path, expected_path));
  std::printf("loaded_icd=%s\n", actual_path);
  auto get = reinterpret_cast<PFN_vkGetInstanceProcAddr>(
    reinterpret_cast<void*>(GetProcAddress(module, "vk_icdGetInstanceProcAddr")));
  REQUIRE(get);
  auto vkCreateInstance = reinterpret_cast<PFN_vkCreateInstance>(get(nullptr, "vkCreateInstance"));
  REQUIRE(vkCreateInstance);
  VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO };
  app.pApplicationName = "helios vehicle completion probe";
  app.apiVersion = VK_API_VERSION_1_2;
  const char* instance_extensions[] = { "VK_KHR_surface", "VK_KHR_win32_surface" };
  VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO };
  ici.pApplicationInfo = &app;
  ici.enabledExtensionCount = 2;
  ici.ppEnabledExtensionNames = instance_extensions;
  VkInstance instance = VK_NULL_HANDLE;
  CHECK(vkCreateInstance(&ici, nullptr, &instance));
  INSTANCE_PROC(vkCreateWin32SurfaceKHR);
  INSTANCE_PROC(vkEnumeratePhysicalDevices);
  INSTANCE_PROC(vkGetPhysicalDeviceQueueFamilyProperties);
  INSTANCE_PROC(vkGetPhysicalDeviceSurfaceSupportKHR);
  INSTANCE_PROC(vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
  INSTANCE_PROC(vkGetPhysicalDeviceSurfaceFormatsKHR);
  INSTANCE_PROC(vkGetPhysicalDeviceMemoryProperties);
  INSTANCE_PROC(vkCreateDevice);
  INSTANCE_PROC(vkGetDeviceProcAddr);
  INSTANCE_PROC(vkDestroySurfaceKHR);
  INSTANCE_PROC(vkDestroyInstance);

  WNDCLASSA wc = {};
  wc.lpfnWndProc = window_proc;
  wc.hInstance = GetModuleHandleA(nullptr);
  wc.lpszClassName = "HeliosVehicleCompletionProbe";
  REQUIRE(RegisterClassA(&wc));
  HWND hwnd = CreateWindowExA(0, wc.lpszClassName, "Helios copy completion", WS_POPUP | WS_VISIBLE,
    0, 0, width, height, nullptr, nullptr, wc.hInstance, nullptr);
  REQUIRE(hwnd);
  pump();
  VkWin32SurfaceCreateInfoKHR sci = { VK_STRUCTURE_TYPE_WIN32_SURFACE_CREATE_INFO_KHR };
  sci.hinstance = wc.hInstance;
  sci.hwnd = hwnd;
  VkSurfaceKHR surface = VK_NULL_HANDLE;
  CHECK(vkCreateWin32SurfaceKHR(instance, &sci, nullptr, &surface));
  uint32_t count = 0;
  CHECK(vkEnumeratePhysicalDevices(instance, &count, nullptr));
  REQUIRE(count);
  std::vector<VkPhysicalDevice> devices(count);
  CHECK(vkEnumeratePhysicalDevices(instance, &count, devices.data()));
  VkPhysicalDevice physical = VK_NULL_HANDLE;
  uint32_t family = UINT32_MAX;
  for (auto candidate : devices) {
    uint32_t n = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(candidate, &n, nullptr);
    std::vector<VkQueueFamilyProperties> families(n);
    vkGetPhysicalDeviceQueueFamilyProperties(candidate, &n, families.data());
    for (uint32_t i = 0; i < n; ++i) {
      VkBool32 present = VK_FALSE;
      CHECK(vkGetPhysicalDeviceSurfaceSupportKHR(candidate, i, surface, &present));
      if (present && (families[i].queueFlags & VK_QUEUE_GRAPHICS_BIT)) {
        physical = candidate;
        family = i;
        break;
      }
    }
    if (physical) break;
  }
  REQUIRE(physical);
  float priority = 1.0f;
  VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO };
  qci.queueFamilyIndex = family;
  qci.queueCount = 1;
  qci.pQueuePriorities = &priority;
  const char* device_extensions[] = { "VK_KHR_swapchain" };
  VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO };
  dci.queueCreateInfoCount = 1;
  dci.pQueueCreateInfos = &qci;
  dci.enabledExtensionCount = 1;
  dci.ppEnabledExtensionNames = device_extensions;
  VkDevice device = VK_NULL_HANDLE;
  CHECK(vkCreateDevice(physical, &dci, nullptr, &device));
  DEVICE_PROC(vkGetDeviceQueue);
  DEVICE_PROC(vkCreateSwapchainKHR); DEVICE_PROC(vkDestroySwapchainKHR);
  DEVICE_PROC(vkGetSwapchainImagesKHR); DEVICE_PROC(vkAcquireNextImageKHR);
  DEVICE_PROC(vkCreateSemaphore); DEVICE_PROC(vkDestroySemaphore);
  DEVICE_PROC(vkCreateFence); DEVICE_PROC(vkDestroyFence); DEVICE_PROC(vkResetFences); DEVICE_PROC(vkWaitForFences); DEVICE_PROC(vkGetFenceStatus);
  DEVICE_PROC(vkCreateBuffer); DEVICE_PROC(vkDestroyBuffer); DEVICE_PROC(vkGetBufferMemoryRequirements);
  DEVICE_PROC(vkAllocateMemory); DEVICE_PROC(vkFreeMemory); DEVICE_PROC(vkBindBufferMemory); DEVICE_PROC(vkCmdFillBuffer);
  DEVICE_PROC(vkCreateCommandPool); DEVICE_PROC(vkDestroyCommandPool);
  DEVICE_PROC(vkAllocateCommandBuffers); DEVICE_PROC(vkResetCommandBuffer);
  DEVICE_PROC(vkBeginCommandBuffer); DEVICE_PROC(vkEndCommandBuffer);
  DEVICE_PROC(vkCmdPipelineBarrier); DEVICE_PROC(vkCmdClearColorImage);
  DEVICE_PROC(vkCreateImageView); DEVICE_PROC(vkDestroyImageView);
  DEVICE_PROC(vkCreateRenderPass); DEVICE_PROC(vkDestroyRenderPass);
  DEVICE_PROC(vkCreateFramebuffer); DEVICE_PROC(vkDestroyFramebuffer);
  DEVICE_PROC(vkCmdBeginRenderPass); DEVICE_PROC(vkCmdEndRenderPass);
  DEVICE_PROC(vkCmdClearAttachments);
  DEVICE_PROC(vkQueueSubmit); DEVICE_PROC(vkQueuePresentKHR); DEVICE_PROC(vkDestroyDevice);
  VkQueue queue = VK_NULL_HANDLE;
  vkGetDeviceQueue(device, family, 0, &queue);
  VkSurfaceCapabilitiesKHR caps;
  CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(physical, surface, &caps));
  const auto usage = loop ? VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT : VK_IMAGE_USAGE_TRANSFER_DST_BIT;
  REQUIRE(caps.supportedUsageFlags & usage);
  CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(physical, surface, &count, nullptr));
  std::vector<VkSurfaceFormatKHR> formats(count);
  CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(physical, surface, &count, formats.data()));
  auto format = std::find_if(formats.begin(), formats.end(), [&](auto f) { return f.format == wanted_format; });
  REQUIRE(format != formats.end());
  VkSwapchainCreateInfoKHR ci = { VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR };
  ci.surface = surface;
  ci.minImageCount = std::max(caps.minImageCount, 3u);
  REQUIRE(!caps.maxImageCount || ci.minImageCount <= caps.maxImageCount);
  ci.imageFormat = format->format;
  ci.imageColorSpace = format->colorSpace;
  ci.imageExtent = caps.currentExtent;
  ci.imageArrayLayers = 1;
  ci.imageUsage = usage;
  ci.imageSharingMode = VK_SHARING_MODE_EXCLUSIVE;
  ci.preTransform = caps.currentTransform;
  ci.compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR;
  ci.presentMode = VK_PRESENT_MODE_FIFO_KHR;
  ci.clipped = VK_TRUE;
  VkSwapchainKHR chain = VK_NULL_HANDLE;
  CHECK(vkCreateSwapchainKHR(device, &ci, nullptr, &chain));
  CHECK(vkGetSwapchainImagesKHR(device, chain, &count, nullptr));
  std::vector<VkImage> images(count);
  CHECK(vkGetSwapchainImagesKHR(device, chain, &count, images.data()));
  VkRenderPass pass = VK_NULL_HANDLE;
  std::vector<VkImageView> views(count, VK_NULL_HANDLE);
  std::vector<VkFramebuffer> framebuffers(count, VK_NULL_HANDLE);
  std::vector<bool> initialized(count, false);
  if (loop) {
    REQUIRE(width >= 16 && height >= 8);
    REQUIRE(caps.currentExtent.width == width && caps.currentExtent.height == height);
    VkAttachmentDescription attachment = {};
    attachment.format = wanted_format;
    attachment.samples = VK_SAMPLE_COUNT_1_BIT;
    attachment.loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR;
    attachment.storeOp = VK_ATTACHMENT_STORE_OP_STORE;
    attachment.stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE;
    attachment.stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE;
    attachment.initialLayout = attachment.finalLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL;
    VkAttachmentReference color_ref = {0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
    VkSubpassDescription subpass = {};
    subpass.pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS;
    subpass.colorAttachmentCount = 1;
    subpass.pColorAttachments = &color_ref;
    VkRenderPassCreateInfo rp = {VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO};
    rp.attachmentCount = 1; rp.pAttachments = &attachment;
    rp.subpassCount = 1; rp.pSubpasses = &subpass;
    CHECK(vkCreateRenderPass(device, &rp, nullptr, &pass));
    for (uint32_t i = 0; i < count; i++) {
      VkImageViewCreateInfo view = {VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO};
      view.image = images[i]; view.viewType = VK_IMAGE_VIEW_TYPE_2D;
      view.format = wanted_format;
      view.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
      CHECK(vkCreateImageView(device, &view, nullptr, &views[i]));
      VkFramebufferCreateInfo fb = {VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO};
      fb.renderPass = pass; fb.attachmentCount = 1; fb.pAttachments = &views[i];
      fb.width = width; fb.height = height; fb.layers = 1;
      CHECK(vkCreateFramebuffer(device, &fb, nullptr, &framebuffers[i]));
    }
  }
  std::vector<VkSemaphore> rendered(count);
  VkSemaphoreCreateInfo sem_ci = { VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
  const uint32_t slot_count = queued ? std::min(count, 3u) : 1u;
  std::vector<VkSemaphore> acquired_slots(slot_count);
  for (auto& semaphore : acquired_slots) CHECK(vkCreateSemaphore(device, &sem_ci, nullptr, &semaphore));
  for (auto& semaphore : rendered) CHECK(vkCreateSemaphore(device, &sem_ci, nullptr, &semaphore));
  // Finite GPU work, all submitted before Present's binary semaphore wait.
  // A future host-signaled timeline dependency would violate Present's
  // dependent-signal submission contract and could deadlock this probe.
  VkBufferCreateInfo buffer_ci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO };
  buffer_ci.size = 256ull * 1024 * 1024;
  buffer_ci.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT;
  buffer_ci.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
  VkBuffer work_buffer = VK_NULL_HANDLE;
  VkDeviceMemory work_memory = VK_NULL_HANDLE;
  if (!loop) {
  CHECK(vkCreateBuffer(device, &buffer_ci, nullptr, &work_buffer));
  VkMemoryRequirements requirements;
  vkGetBufferMemoryRequirements(device, work_buffer, &requirements);
  VkPhysicalDeviceMemoryProperties memory_properties;
  vkGetPhysicalDeviceMemoryProperties(physical, &memory_properties);
  VkMemoryAllocateInfo memory_ci = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO };
  memory_ci.allocationSize = requirements.size;
  memory_ci.memoryTypeIndex = UINT32_MAX;
  for (uint32_t i = 0; i < memory_properties.memoryTypeCount; ++i) {
    if ((requirements.memoryTypeBits & (1u << i)) &&
        (memory_properties.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) {
      memory_ci.memoryTypeIndex = i;
      break;
    }
  }
  REQUIRE(memory_ci.memoryTypeIndex != UINT32_MAX);
  CHECK(vkAllocateMemory(device, &memory_ci, nullptr, &work_memory));
  CHECK(vkBindBufferMemory(device, work_buffer, work_memory, 0));
  }
  VkFenceCreateInfo fence_ci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
  std::vector<VkFence> submitted_slots(slot_count);
  std::vector<bool> pending_slots(slot_count, false);
  for (auto& fence : submitted_slots) CHECK(vkCreateFence(device, &fence_ci, nullptr, &fence));
  const VkFence submitted = submitted_slots[0]; // Delayed single-slot control.
  VkFence acquired_fence;
  CHECK(vkCreateFence(device, &fence_ci, nullptr, &acquired_fence));
  VkCommandPoolCreateInfo pool_ci = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO };
  pool_ci.queueFamilyIndex = family;
  pool_ci.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
  VkCommandPool pool;
  CHECK(vkCreateCommandPool(device, &pool_ci, nullptr, &pool));
  VkCommandBufferAllocateInfo alloc = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO };
  alloc.commandPool = pool;
  alloc.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
  alloc.commandBufferCount = slot_count;
  std::vector<VkCommandBuffer> command_slots(slot_count);
  CHECK(vkAllocateCommandBuffers(device, &alloc, command_slots.data()));
  uint32_t max_unsignaled = 0;

  auto present = [&](bool delayed, uint32_t frame) {
    const uint32_t slot = frame % slot_count;
    const auto cmd = command_slots[slot];
    const auto acquired = acquired_slots[slot];
    const auto submitted = submitted_slots[slot];
    // A command buffer and its acquire semaphore are reusable only after
    // their own submission has completed. Other slots remain in flight.
    if (pending_slots[slot]) {
      CHECK(vkWaitForFences(device, 1, &submitted, VK_TRUE, 5000000000ull));
      pending_slots[slot] = false;
    }
    uint32_t index = UINT32_MAX;
    CHECK(vkAcquireNextImageKHR(device, chain, 5000000000ull, acquired, VK_NULL_HANDLE, &index));
    CHECK(vkResetCommandBuffer(cmd, 0));
    VkCommandBufferBeginInfo begin = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
    CHECK(vkBeginCommandBuffer(cmd, &begin));
    if (delayed) {
      VkMemoryBarrier order = { VK_STRUCTURE_TYPE_MEMORY_BARRIER };
      order.srcAccessMask = order.dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
      for (uint32_t i = 0; i < 2048; ++i) {
        vkCmdFillBuffer(cmd, work_buffer, 0, buffer_ci.size, i);
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
          0, 1, &order, 0, nullptr, 0, nullptr);
      }
    }
    VkImageMemoryBarrier barrier = { VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER };
    barrier.dstAccessMask = loop ? VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT : VK_ACCESS_TRANSFER_WRITE_BIT;
    barrier.oldLayout = loop && initialized[index] ? VK_IMAGE_LAYOUT_PRESENT_SRC_KHR : VK_IMAGE_LAYOUT_UNDEFINED;
    barrier.newLayout = loop ? VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL : VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
    barrier.srcQueueFamilyIndex = barrier.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    barrier.image = images[index];
    barrier.subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 };
    const VkPipelineStageFlags render_stage = loop ? VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT : VK_PIPELINE_STAGE_TRANSFER_BIT;
    vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, render_stage, 0, 0, nullptr, 0, nullptr, 1, &barrier);
    VkClearColorValue color = {};
    color.float32[0] = float(frame % 60) / 60;
    color.float32[1] = 0.4f;
    color.float32[3] = 1.0f;
    if (loop) {
      VkClearValue clear = {}; clear.color = color;
      VkRenderPassBeginInfo begin_pass = {VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO};
      begin_pass.renderPass = pass; begin_pass.framebuffer = framebuffers[index];
      begin_pass.renderArea.extent = {width, height};
      begin_pass.clearValueCount = 1; begin_pass.pClearValues = &clear;
      vkCmdBeginRenderPass(cmd, &begin_pass, VK_SUBPASS_CONTENTS_INLINE);
      // A spatial pattern exposes image-address/layout errors that a uniform
      // clear can hide. Binary channel values survive sRGB/UNORM transfers.
      // The bottom serial identifies the exact expected pattern in a capture.
      if (pattern) {
        for (uint32_t y = 0; y < 12; ++y) {
          for (uint32_t x = 0; x < 16; ++x) {
            uint32_t bits = ((x + 1) * 0x45d9f3bu) ^ ((y + 7) * 0x27d4eb2du) ^ frame;
            bits ^= bits >> 16;
            VkClearAttachment cell = {};
            cell.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT;
            cell.clearValue.color = {{float(bits & 1), float((bits >> 1) & 1),
                                      float((bits >> 2) & 1), 1.0f}};
            VkClearRect rect = {};
            rect.rect.offset = {int32_t(width * x / 16), int32_t((height - 8) * y / 12)};
            rect.rect.extent = {width * (x + 1) / 16 - width * x / 16,
                               (height - 8) * (y + 1) / 12 - (height - 8) * y / 12};
            rect.layerCount = 1;
            vkCmdClearAttachments(cmd, 1, &cell, 1, &rect);
          }
        }
      }
      // 16-bit white/black serial at the bottom, immune to sRGB conversion.
      for (uint32_t bit = 0; bit < 16; bit++) {
        VkClearAttachment tag = {};
        tag.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT;
        const float value = (frame & (1u << bit)) ? 1.0f : 0.0f;
        tag.clearValue.color = {{value, value, value, 1.0f}};
        VkClearRect rect = {};
        rect.rect.offset = {int32_t(width * bit / 16), int32_t(height - 8)};
        rect.rect.extent = {width * (bit + 1) / 16 - width * bit / 16, 8};
        rect.layerCount = 1;
        vkCmdClearAttachments(cmd, 1, &tag, 1, &rect);
      }
      vkCmdEndRenderPass(cmd);
    } else {
      vkCmdClearColorImage(cmd, images[index], VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &color, 1, &barrier.subresourceRange);
    }
    barrier.srcAccessMask = barrier.dstAccessMask;
    barrier.dstAccessMask = 0;
    barrier.oldLayout = barrier.newLayout;
    barrier.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
    vkCmdPipelineBarrier(cmd, render_stage, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, nullptr, 0, nullptr, 1, &barrier);
    CHECK(vkEndCommandBuffer(cmd));
    VkPipelineStageFlags stage = render_stage;
    VkSubmitInfo submit = { VK_STRUCTURE_TYPE_SUBMIT_INFO };
    submit.waitSemaphoreCount = 1;
    submit.pWaitSemaphores = &acquired;
    submit.pWaitDstStageMask = &stage;
    submit.commandBufferCount = 1;
    submit.pCommandBuffers = &cmd;
    submit.signalSemaphoreCount = 1;
    submit.pSignalSemaphores = &rendered[index];
    CHECK(vkResetFences(device, 1, &submitted));
    CHECK(vkQueueSubmit(queue, 1, &submit, submitted));
    pending_slots[slot] = true;
    uint32_t unsignaled = 0;
    for (uint32_t i = 0; i < slot_count; ++i) {
      if (!pending_slots[i]) continue;
      const VkResult status = vkGetFenceStatus(device, submitted_slots[i]);
      REQUIRE(status == VK_SUCCESS || status == VK_NOT_READY);
      unsignaled += status == VK_NOT_READY;
    }
    max_unsignaled = std::max(max_unsignaled, unsignaled);
    VkPresentInfoKHR pi = { VK_STRUCTURE_TYPE_PRESENT_INFO_KHR };
    pi.waitSemaphoreCount = 1;
    pi.pWaitSemaphores = &rendered[index];
    pi.swapchainCount = 1;
    pi.pSwapchains = &chain;
    pi.pImageIndices = &index;
    CHECK(vkQueuePresentKHR(queue, &pi));
    initialized[index] = true;
    if (!delayed && !queued) {
      CHECK(vkWaitForFences(device, 1, &submitted, VK_TRUE, 5000000000ull));
      pending_slots[slot] = false;
    }
    pump();
    return index;
  };

  bool timeout_proven = false;
  if (loop) {
    std::printf("present-loop queued=%u slots=%u format=%u usage=0x%x extent=%ux%u frames=%u images=%u\n", queued, slot_count, wanted_format, usage, width, height, frames, count);
    const auto start = GetTickCount64();
    for (uint32_t frame = 0; frame < frames; frame++) {
      REQUIRE(GetTickCount64() - start < 60000);
      const auto index = present(false, frame);
      if (frame % 32 == 0) {
        std::printf("frame=%u index=%u elapsed_ms=%llu\n", frame, index, static_cast<unsigned long long>(GetTickCount64() - start));
        std::fflush(stdout);
      }
      if (!queued) Sleep(10);
    }
    for (uint32_t i = 0; i < slot_count; ++i)
      if (pending_slots[i]) CHECK(vkWaitForFences(device, 1, &submitted_slots[i], VK_TRUE, 5000000000ull));
    // Drain only this chain's image reads: acquire each distinct image after
    // its mandatory WSI copy-completion guard, without queue/device-idle.
    std::vector<bool> drained(count, false);
    for (uint32_t i = 0; i < count; i++) {
      uint32_t index = UINT32_MAX;
      CHECK(vkAcquireNextImageKHR(device, chain, 5000000000ull, VK_NULL_HANDLE, acquired_fence, &index));
      CHECK(vkWaitForFences(device, 1, &acquired_fence, VK_TRUE, 5000000000ull));
      CHECK(vkResetFences(device, 1, &acquired_fence));
      REQUIRE(!drained[index]); drained[index] = true;
    }
    std::printf("completed frames=%u max_observed_unsignaled_submissions=%u\n", frames, max_unsignaled);
    std::puts("PASS bounded present-loop (require configured path, visible serial and host fault evidence)");
  } else {
  const auto warmup_end = GetTickCount64() + 10000;
  uint32_t frame = 0;
  do { present(false, frame++); Sleep(10); } while (GetTickCount64() < warmup_end);
  std::printf("warmup frames=%u; submitting finite delayed frame\n", frame);
  // This single chain starts its WSI producer at zero and increments once
  // per successful Present. Bind diagnostic acceptance to this frame only.
  std::printf("delayed_producer=%llu\n", static_cast<unsigned long long>(frame) + 1);
  std::fflush(stdout);
  const auto delayed_start = GetTickCount64();
  const uint32_t source = present(true, frame);
  std::vector<bool> held(count, false);
  for (uint32_t i = 1; i < count; ++i) {
    uint32_t index = UINT32_MAX;
    CHECK(vkAcquireNextImageKHR(device, chain, 5000000000ull, VK_NULL_HANDLE, acquired_fence, &index));
    CHECK(vkWaitForFences(device, 1, &acquired_fence, VK_TRUE, 5000000000ull));
    CHECK(vkResetFences(device, 1, &acquired_fence));
    REQUIRE(index != source && !held[index]);
    held[index] = true;
  }
  Sleep(80);
  const VkResult source_status = vkGetFenceStatus(device, submitted);
  REQUIRE(source_status == VK_SUCCESS || source_status == VK_NOT_READY);
  uint32_t index = UINT32_MAX;
  VkResult acquire_status = vkAcquireNextImageKHR(device, chain, 0, VK_NULL_HANDLE, acquired_fence, &index);
  REQUIRE(acquire_status == VK_NOT_READY || acquire_status == VK_SUCCESS);
  const bool initially_unavailable = acquire_status == VK_NOT_READY;
  if (initially_unavailable) {
    acquire_status = vkAcquireNextImageKHR(device, chain, 50000000, VK_NULL_HANDLE, acquired_fence, &index);
    REQUIRE(acquire_status == VK_TIMEOUT || acquire_status == VK_SUCCESS);
  }
  timeout_proven = source_status == VK_NOT_READY && initially_unavailable && acquire_status == VK_TIMEOUT;
  if (acquire_status == VK_SUCCESS) {
    REQUIRE(index == source);
  }
  std::printf("%s: source wait observed for %llu ms\n",
    timeout_proven ? "PASS pending" : "INCONCLUSIVE pending (work completed during observation)",
    static_cast<unsigned long long>(GetTickCount64() - delayed_start));
  if (teardown && timeout_proven) {
    // Change window lifetime while the helper's exact read is still pending.
    // All waits have already been submitted; no host-signal dependency or
    // queue/device-idle call is introduced to manufacture this condition.
    if (close_pending) REQUIRE(DestroyWindow(hwnd));
    else REQUIRE(SetWindowPos(hwnd, nullptr, 0, 0, width + 16, height + 16,
                              SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE));
    pump();
    std::printf("lifecycle_cancel=%s producer=%llu elapsed_ms=%llu\n",
      close_pending ? "close" : "resize", static_cast<unsigned long long>(frame) + 1,
      static_cast<unsigned long long>(GetTickCount64() - delayed_start));
    std::fflush(stdout);
  }
  CHECK(vkWaitForFences(device, 1, &submitted, VK_TRUE, 5000000000ull));
  if (teardown && timeout_proven) {
    std::puts("application submission complete; destroying cancelled swapchain (helper proof required)");
  } else {
    if (acquire_status != VK_SUCCESS)
      CHECK(vkAcquireNextImageKHR(device, chain, 5000000000ull, VK_NULL_HANDLE, acquired_fence, &index));
    CHECK(vkWaitForFences(device, 1, &acquired_fence, VK_TRUE, 5000000000ull));
    REQUIRE(index == source);
    std::puts("PASS completion: the retained source became acquirable");
  }
  }

  for (auto fb : framebuffers) if (fb) vkDestroyFramebuffer(device, fb, nullptr);
  for (auto view : views) if (view) vkDestroyImageView(device, view, nullptr);
  if (pass) vkDestroyRenderPass(device, pass, nullptr);
  const auto destroy_start = GetTickCount64();
  vkDestroySwapchainKHR(device, chain, nullptr);
  if (teardown) {
    std::printf("swapchain_destroy_ms=%llu\n", static_cast<unsigned long long>(GetTickCount64() - destroy_start));
    std::fflush(stdout);
  }
  vkDestroyCommandPool(device, pool, nullptr);
  if (work_buffer) vkDestroyBuffer(device, work_buffer, nullptr);
  if (work_memory) vkFreeMemory(device, work_memory, nullptr);
  vkDestroyFence(device, acquired_fence, nullptr);
  for (auto fence : submitted_slots) vkDestroyFence(device, fence, nullptr);
  for (auto semaphore : rendered) vkDestroySemaphore(device, semaphore, nullptr);
  for (auto semaphore : acquired_slots) vkDestroySemaphore(device, semaphore, nullptr);
  vkDestroyDevice(device, nullptr);
  vkDestroySurfaceKHR(instance, surface, nullptr);
  vkDestroyInstance(instance, nullptr);
  if (IsWindow(hwnd)) DestroyWindow(hwnd);
  FreeLibrary(module);
  if (teardown && timeout_proven)
    std::puts("LIFECYCLE INPUT COMPLETE (require matching helper retirement and host destruction evidence)");
  else if (!loop)
    std::puts(timeout_proven ? "PASS (require matching vehicle LIVE and pending/completed diagnostics)" : "INCONCLUSIVE");
  return loop || timeout_proven ? 0 : 3;
}
