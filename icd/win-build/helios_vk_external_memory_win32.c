/* Focused VK_KHR_external_memory_win32 probe for the Helios Windows ICD.
 *
 * Build in the WinBoat guest:
 *   gcc -O2 -o C:\Users\tibix\helios_vk_external_memory_win32.exe \
 *     Z:\icd\win-build\helios_vk_external_memory_win32.c \
 *     -IZ:\icd\mesa\include
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>

#define VK_USE_PLATFORM_WIN32_KHR
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#define LOAD_I(name)                                                        \
   PFN_##name name = (PFN_##name)gipa(instance, #name);                     \
   if (!(name)) {                                                           \
      fprintf(stderr, "FAIL: missing %s\n", #name);                       \
      return 10;                                                            \
   }

#define LOAD_D(name)                                                        \
   PFN_##name name = (PFN_##name)gdpa(device, #name);                       \
   if (!(name)) {                                                           \
      fprintf(stderr, "FAIL: missing %s\n", #name);                       \
      return 11;                                                            \
   }

static const char *
result_name(VkResult result)
{
   switch (result) {
   case VK_SUCCESS: return "VK_SUCCESS";
   case VK_ERROR_EXTENSION_NOT_PRESENT: return "VK_ERROR_EXTENSION_NOT_PRESENT";
   case VK_ERROR_INVALID_EXTERNAL_HANDLE: return "VK_ERROR_INVALID_EXTERNAL_HANDLE";
   case VK_ERROR_OUT_OF_HOST_MEMORY: return "VK_ERROR_OUT_OF_HOST_MEMORY";
   case VK_ERROR_OUT_OF_DEVICE_MEMORY: return "VK_ERROR_OUT_OF_DEVICE_MEMORY";
   case VK_ERROR_DEVICE_LOST: return "VK_ERROR_DEVICE_LOST";
   default: return "other VkResult";
   }
}

static bool
has_extension(const VkExtensionProperties *extensions,
              uint32_t count,
              const char *name)
{
   for (uint32_t i = 0; i < count; i++) {
      if (!strcmp(extensions[i].extensionName, name))
         return true;
   }
   return false;
}

static uint32_t
choose_memory_type(const VkPhysicalDeviceMemoryProperties *props,
                   uint32_t type_bits)
{
   for (uint32_t i = 0; i < props->memoryTypeCount; i++) {
      if ((type_bits & (1u << i)) &&
          (props->memoryTypes[i].propertyFlags &
           VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT))
         return i;
   }
   for (uint32_t i = 0; i < props->memoryTypeCount; i++) {
      if (type_bits & (1u << i))
         return i;
   }
   return UINT32_MAX;
}

int
main(void)
{
   HMODULE loader = LoadLibraryW(L"vulkan-1.dll");
   if (!loader) {
      fprintf(stderr, "FAIL: LoadLibrary(vulkan-1.dll), error %lu\n",
              (unsigned long)GetLastError());
      return 1;
   }

   PFN_vkGetInstanceProcAddr gipa =
      (PFN_vkGetInstanceProcAddr)(void *)
         GetProcAddress(loader, "vkGetInstanceProcAddr");
   PFN_vkCreateInstance create_instance =
      gipa ? (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance") : NULL;
   if (!create_instance) {
      fprintf(stderr, "FAIL: no vkCreateInstance\n");
      return 2;
   }

   const VkApplicationInfo app_info = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "helios_vk_external_memory_win32",
      .apiVersion = VK_API_VERSION_1_1,
   };
   const VkInstanceCreateInfo instance_info = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app_info,
   };
   VkInstance instance = VK_NULL_HANDLE;
   VkResult result = create_instance(&instance_info, NULL, &instance);
   if (result != VK_SUCCESS) {
      fprintf(stderr, "FAIL: vkCreateInstance: %d (%s)\n", result,
              result_name(result));
      return 3;
   }

   LOAD_I(vkDestroyInstance);
   LOAD_I(vkEnumeratePhysicalDevices);
   LOAD_I(vkGetPhysicalDeviceProperties);
   LOAD_I(vkGetPhysicalDeviceMemoryProperties);
   LOAD_I(vkGetPhysicalDeviceQueueFamilyProperties);
   LOAD_I(vkEnumerateDeviceExtensionProperties);
   LOAD_I(vkGetPhysicalDeviceExternalBufferProperties);
   LOAD_I(vkGetPhysicalDeviceImageFormatProperties2);
   LOAD_I(vkCreateDevice);

   uint32_t physical_count = 0;
   result = vkEnumeratePhysicalDevices(instance, &physical_count, NULL);
   if (result != VK_SUCCESS || !physical_count) {
      fprintf(stderr, "FAIL: no Vulkan physical device\n");
      return 4;
   }
   VkPhysicalDevice *physical_devices =
      calloc(physical_count, sizeof(*physical_devices));
   vkEnumeratePhysicalDevices(instance, &physical_count, physical_devices);
   VkPhysicalDevice physical_device = physical_devices[0];
   free(physical_devices);

   VkPhysicalDeviceProperties physical_props;
   vkGetPhysicalDeviceProperties(physical_device, &physical_props);
   fprintf(stderr, "device: %s\n", physical_props.deviceName);

   uint32_t extension_count = 0;
   vkEnumerateDeviceExtensionProperties(physical_device, NULL,
                                        &extension_count, NULL);
   VkExtensionProperties *extensions =
      calloc(extension_count, sizeof(*extensions));
   vkEnumerateDeviceExtensionProperties(physical_device, NULL,
                                        &extension_count, extensions);
   const bool have_win32 =
      has_extension(extensions, extension_count,
                    VK_KHR_EXTERNAL_MEMORY_WIN32_EXTENSION_NAME);
   free(extensions);
   fprintf(stderr, "%s: %s\n",
           VK_KHR_EXTERNAL_MEMORY_WIN32_EXTENSION_NAME,
           have_win32 ? "present" : "MISSING");
   if (!have_win32)
      return 5;

   VkPhysicalDeviceExternalBufferInfo external_buffer_info = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_BUFFER_INFO,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT,
      .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
               VK_BUFFER_USAGE_TRANSFER_DST_BIT,
   };
   VkExternalBufferProperties external_buffer_props = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_BUFFER_PROPERTIES,
   };
   vkGetPhysicalDeviceExternalBufferProperties(
      physical_device, &external_buffer_info, &external_buffer_props);
   const VkExternalMemoryProperties *external_props =
      &external_buffer_props.externalMemoryProperties;
   fprintf(stderr, "OPAQUE_WIN32 buffer: features=0x%x compatible=0x%x "
                   "exportFromImported=0x%x\n",
           external_props->externalMemoryFeatures,
           external_props->compatibleHandleTypes,
           external_props->exportFromImportedHandleTypes);
   const VkExternalMemoryFeatureFlags required_features =
      VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT |
      VK_EXTERNAL_MEMORY_FEATURE_EXPORTABLE_BIT;
   if ((external_props->externalMemoryFeatures & required_features) !=
          required_features ||
       !(external_props->compatibleHandleTypes &
         VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT) ||
       !(external_props->exportFromImportedHandleTypes &
         VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT)) {
      fprintf(stderr, "FAIL: incomplete OPAQUE_WIN32 capabilities\n");
      return 6;
   }

   const VkPhysicalDeviceExternalImageFormatInfo external_image_info = {
      .sType =
         VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT,
   };
   const VkPhysicalDeviceImageFormatInfo2 image_format_info = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
      .pNext = &external_image_info,
      .format = VK_FORMAT_R8G8B8A8_UNORM,
      .type = VK_IMAGE_TYPE_2D,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT |
               VK_IMAGE_USAGE_TRANSFER_DST_BIT |
               VK_IMAGE_USAGE_SAMPLED_BIT,
   };
   VkExternalImageFormatProperties external_image_props = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES,
   };
   VkImageFormatProperties2 image_format_props = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2,
      .pNext = &external_image_props,
   };
   result = vkGetPhysicalDeviceImageFormatProperties2(
      physical_device, &image_format_info, &image_format_props);
   const VkExternalMemoryProperties *image_external_props =
      &external_image_props.externalMemoryProperties;
   fprintf(stderr, "OPAQUE_WIN32 image: result=%d (%s) features=0x%x "
                   "compatible=0x%x exportFromImported=0x%x\n",
           result, result_name(result),
           image_external_props->externalMemoryFeatures,
           image_external_props->compatibleHandleTypes,
           image_external_props->exportFromImportedHandleTypes);
   if (result != VK_SUCCESS ||
       (image_external_props->externalMemoryFeatures & required_features) !=
          required_features ||
       !(image_external_props->compatibleHandleTypes &
         VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT) ||
       !(image_external_props->exportFromImportedHandleTypes &
         VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT)) {
      fprintf(stderr, "FAIL: incomplete OPAQUE_WIN32 image capabilities\n");
      return 22;
   }

   uint32_t queue_count = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(physical_device, &queue_count,
                                            NULL);
   VkQueueFamilyProperties *queue_props =
      calloc(queue_count, sizeof(*queue_props));
   vkGetPhysicalDeviceQueueFamilyProperties(physical_device, &queue_count,
                                            queue_props);
   uint32_t queue_family = UINT32_MAX;
   for (uint32_t i = 0; i < queue_count; i++) {
      if (queue_props[i].queueCount) {
         queue_family = i;
         break;
      }
   }
   free(queue_props);
   if (queue_family == UINT32_MAX)
      return 7;

   const float priority = 1.0f;
   const VkDeviceQueueCreateInfo queue_info = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = queue_family,
      .queueCount = 1,
      .pQueuePriorities = &priority,
   };
   const char *device_extensions[] = {
      VK_KHR_EXTERNAL_MEMORY_WIN32_EXTENSION_NAME,
   };
   const VkDeviceCreateInfo device_info = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &queue_info,
      .enabledExtensionCount = 1,
      .ppEnabledExtensionNames = device_extensions,
   };
   VkDevice device = VK_NULL_HANDLE;
   result = vkCreateDevice(physical_device, &device_info, NULL, &device);
   if (result != VK_SUCCESS) {
      fprintf(stderr, "FAIL: vkCreateDevice: %d (%s)\n", result,
              result_name(result));
      return 8;
   }

   PFN_vkGetDeviceProcAddr gdpa =
      (PFN_vkGetDeviceProcAddr)gipa(instance, "vkGetDeviceProcAddr");
   LOAD_D(vkDestroyDevice);
   LOAD_D(vkCreateBuffer);
   LOAD_D(vkDestroyBuffer);
   LOAD_D(vkGetBufferMemoryRequirements);
   LOAD_D(vkAllocateMemory);
   LOAD_D(vkFreeMemory);
   LOAD_D(vkBindBufferMemory);
   LOAD_D(vkCreateImage);
   LOAD_D(vkDestroyImage);
   LOAD_D(vkGetImageMemoryRequirements);
   LOAD_D(vkBindImageMemory);
   LOAD_D(vkGetMemoryWin32HandleKHR);
   LOAD_D(vkGetMemoryWin32HandlePropertiesKHR);

   VkPhysicalDeviceMemoryProperties memory_props;
   vkGetPhysicalDeviceMemoryProperties(physical_device, &memory_props);

   const VkExternalMemoryBufferCreateInfo external_create = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT,
   };
   const VkBufferCreateInfo buffer_info = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .pNext = &external_create,
      .size = 64 * 1024,
      .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
               VK_BUFFER_USAGE_TRANSFER_DST_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   VkBuffer export_buffer = VK_NULL_HANDLE;
   VkBuffer import_buffer = VK_NULL_HANDLE;
   VkBuffer named_import_buffer = VK_NULL_HANDLE;
   result = vkCreateBuffer(device, &buffer_info, NULL, &export_buffer);
   if (result == VK_SUCCESS)
      result = vkCreateBuffer(device, &buffer_info, NULL, &import_buffer);
   if (result == VK_SUCCESS)
      result = vkCreateBuffer(device, &buffer_info, NULL,
                              &named_import_buffer);
   if (result != VK_SUCCESS) {
      fprintf(stderr, "FAIL: vkCreateBuffer: %d (%s)\n", result,
              result_name(result));
      return 12;
   }

   VkMemoryRequirements requirements;
   vkGetBufferMemoryRequirements(device, export_buffer, &requirements);
   const uint32_t memory_type =
      choose_memory_type(&memory_props, requirements.memoryTypeBits);
   if (memory_type == UINT32_MAX)
      return 13;

   wchar_t shared_name[128];
   swprintf(shared_name, sizeof(shared_name) / sizeof(shared_name[0]),
            L"Local\\HeliosVkExternalMemory-%lu",
            (unsigned long)GetCurrentProcessId());
   const VkExportMemoryWin32HandleInfoKHR export_win32_info = {
      .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_WIN32_HANDLE_INFO_KHR,
      .pAttributes = NULL,
      .dwAccess = GENERIC_ALL,
      .name = shared_name,
   };
   const VkExportMemoryAllocateInfo export_allocate = {
      .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
      .pNext = &export_win32_info,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT,
   };
   const VkMemoryAllocateInfo export_memory_info = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &export_allocate,
      .allocationSize = requirements.size,
      .memoryTypeIndex = memory_type,
   };
   VkDeviceMemory export_memory = VK_NULL_HANDLE;
   result = vkAllocateMemory(device, &export_memory_info, NULL,
                             &export_memory);
   fprintf(stderr, "export allocation: %d (%s), size=%llu type=%u\n",
           result, result_name(result),
           (unsigned long long)requirements.size, memory_type);
   if (result != VK_SUCCESS)
      return 14;
   result = vkBindBufferMemory(device, export_buffer, export_memory, 0);
   if (result != VK_SUCCESS)
      return 15;

   const VkMemoryGetWin32HandleInfoKHR get_handle_info = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_GET_WIN32_HANDLE_INFO_KHR,
      .memory = export_memory,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT,
   };
   HANDLE shared_handle = NULL;
   result = vkGetMemoryWin32HandleKHR(device, &get_handle_info,
                                      &shared_handle);
   fprintf(stderr, "get NT handle: %d (%s), handle=%p\n", result,
           result_name(result), shared_handle);
   if (result != VK_SUCCESS || !shared_handle)
      return 16;

   VkExportMemoryAllocateInfo reexport_allocate = {
      .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT,
   };
   VkImportMemoryWin32HandleInfoKHR import_handle_info = {
      .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_WIN32_HANDLE_INFO_KHR,
      .pNext = &reexport_allocate,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT,
      .handle = shared_handle,
   };
   const VkMemoryAllocateInfo import_memory_info = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &import_handle_info,
      .allocationSize = requirements.size,
      .memoryTypeIndex = memory_type,
   };
   VkDeviceMemory import_memory = VK_NULL_HANDLE;
   result = vkAllocateMemory(device, &import_memory_info, NULL,
                             &import_memory);
   /* OPAQUE_WIN32 import does not transfer handle ownership.  Close it now to
    * prove that the imported WDDM allocation retained its own payload ref. */
   CloseHandle(shared_handle);
   shared_handle = NULL;
   fprintf(stderr, "import allocation: %d (%s)\n", result,
           result_name(result));
   if (result != VK_SUCCESS)
      return 17;
   result = vkBindBufferMemory(device, import_buffer, import_memory, 0);
   if (result != VK_SUCCESS)
      return 18;

   const VkMemoryGetWin32HandleInfoKHR reexport_info = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_GET_WIN32_HANDLE_INFO_KHR,
      .memory = import_memory,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT,
   };
   HANDLE reexported_handle = NULL;
   result = vkGetMemoryWin32HandleKHR(device, &reexport_info,
                                      &reexported_handle);
   fprintf(stderr, "re-export imported allocation: %d (%s), handle=%p\n",
           result, result_name(result), reexported_handle);
   if (result != VK_SUCCESS || !reexported_handle)
      return 19;
   CloseHandle(reexported_handle);

   const VkImportMemoryWin32HandleInfoKHR import_name_info = {
      .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_WIN32_HANDLE_INFO_KHR,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT,
      .handle = NULL,
      .name = shared_name,
   };
   const VkMemoryAllocateInfo import_name_memory_info = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &import_name_info,
      .allocationSize = requirements.size,
      .memoryTypeIndex = memory_type,
   };
   VkDeviceMemory named_import_memory = VK_NULL_HANDLE;
   result = vkAllocateMemory(device, &import_name_memory_info, NULL,
                             &named_import_memory);
   fprintf(stderr, "named import allocation: %d (%s)\n", result,
           result_name(result));
   if (result != VK_SUCCESS)
      return 20;
   result = vkBindBufferMemory(device, named_import_buffer,
                               named_import_memory, 0);
   if (result != VK_SUCCESS)
      return 21;

   vkDestroyBuffer(device, named_import_buffer, NULL);
   vkFreeMemory(device, named_import_memory, NULL);
   vkDestroyBuffer(device, import_buffer, NULL);
   vkFreeMemory(device, import_memory, NULL);
   vkDestroyBuffer(device, export_buffer, NULL);
   vkFreeMemory(device, export_memory, NULL);

   const VkExternalMemoryImageCreateInfo external_image_create = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT,
   };
   const VkImageCreateInfo image_info = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .pNext = &external_image_create,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM,
      .extent = { 64, 64, 1 },
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT |
               VK_IMAGE_USAGE_TRANSFER_DST_BIT |
               VK_IMAGE_USAGE_SAMPLED_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   VkImage export_image = VK_NULL_HANDLE;
   VkImage import_image = VK_NULL_HANDLE;
   result = vkCreateImage(device, &image_info, NULL, &export_image);
   if (result == VK_SUCCESS)
      result = vkCreateImage(device, &image_info, NULL, &import_image);
   fprintf(stderr, "external image creation: %d (%s)\n", result,
           result_name(result));
   if (result != VK_SUCCESS)
      return 23;

   VkMemoryRequirements image_requirements;
   vkGetImageMemoryRequirements(device, export_image, &image_requirements);
   const uint32_t image_memory_type =
      choose_memory_type(&memory_props, image_requirements.memoryTypeBits);
   if (image_memory_type == UINT32_MAX)
      return 24;

   const VkMemoryDedicatedAllocateInfo export_image_dedicated = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
      .image = export_image,
   };
   const VkExportMemoryAllocateInfo export_image_allocate = {
      .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
      .pNext = &export_image_dedicated,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT,
   };
   const VkMemoryAllocateInfo export_image_memory_info = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &export_image_allocate,
      .allocationSize = image_requirements.size,
      .memoryTypeIndex = image_memory_type,
   };
   VkDeviceMemory export_image_memory = VK_NULL_HANDLE;
   result = vkAllocateMemory(device, &export_image_memory_info, NULL,
                             &export_image_memory);
   if (result == VK_SUCCESS)
      result = vkBindImageMemory(device, export_image, export_image_memory, 0);
   fprintf(stderr, "external image export allocation/bind: %d (%s)\n",
           result, result_name(result));
   if (result != VK_SUCCESS)
      return 25;

   const VkMemoryGetWin32HandleInfoKHR get_image_handle_info = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_GET_WIN32_HANDLE_INFO_KHR,
      .memory = export_image_memory,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT,
   };
   HANDLE shared_image_handle = NULL;
   result = vkGetMemoryWin32HandleKHR(device, &get_image_handle_info,
                                      &shared_image_handle);
   if (result != VK_SUCCESS || !shared_image_handle)
      return 26;

   const VkMemoryDedicatedAllocateInfo import_image_dedicated = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
      .image = import_image,
   };
   const VkImportMemoryWin32HandleInfoKHR import_image_handle_info = {
      .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_WIN32_HANDLE_INFO_KHR,
      .pNext = &import_image_dedicated,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT,
      .handle = shared_image_handle,
   };
   const VkMemoryAllocateInfo import_image_memory_info = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &import_image_handle_info,
      .allocationSize = image_requirements.size,
      .memoryTypeIndex = image_memory_type,
   };
   VkDeviceMemory import_image_memory = VK_NULL_HANDLE;
   result = vkAllocateMemory(device, &import_image_memory_info, NULL,
                             &import_image_memory);
   CloseHandle(shared_image_handle);
   if (result == VK_SUCCESS)
      result = vkBindImageMemory(device, import_image, import_image_memory, 0);
   fprintf(stderr, "external image handle import/bind: %d (%s)\n", result,
           result_name(result));
   if (result != VK_SUCCESS)
      return 27;

   vkDestroyImage(device, import_image, NULL);
   vkFreeMemory(device, import_image_memory, NULL);
   vkDestroyImage(device, export_image, NULL);
   vkFreeMemory(device, export_image_memory, NULL);
   vkDestroyDevice(device, NULL);
   vkDestroyInstance(instance, NULL);
   FreeLibrary(loader);
   fprintf(stderr, "PASS: OPAQUE_WIN32 buffer/image handle/name paths\n");
   return 0;
}
