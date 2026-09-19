/* SPDX-License-Identifier: MIT
 * Compare Vulkan and DMA-BUF CPU mappings of the same allocation. No shaders,
 * command buffers, or GPU submissions. Linux only.
 * --cached selects HOST_CACHED system memory instead of host-visible VRAM.
 * --fixed replaces only our own reserved virtual-address range, as QEMU does.
 * --read-only-dmabuf never writes through the DMA-BUF mapping.
 * --vulkan-import also checks a Vulkan import of the very same DMA-BUF payload.
 * --opaque-import instead exports a compatible OPAQUE_FD of the same allocation
 * and imports it with the original size/type; the DMA-BUF mapping stays present.
 * --opaque-only tests an OPAQUE_FD allocation/import without any DMA-BUF mapping.
 * --renderer-import uses virglrenderer's production Vulkan mapper instead;
 * build with HELIOS_VKR_ALLOCATOR_CONTROL and link its native static library.
 * --dedicated and --device-address exercise those allocation contracts.
 */
#ifndef _GNU_SOURCE
#define _GNU_SOURCE 1
#endif
#include <vulkan/vulkan.h>
#include <linux/dma-buf.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdatomic.h>
#include <stdint.h>
#include <inttypes.h>
#include <stdio.h>
#include <string.h>
#include <errno.h>
#ifdef HELIOS_VKR_ALLOCATOR_CONTROL
#include "config.h"
#include "vkr_allocator.h"
#include "virgl_resource.h"
#endif

#define BUFFER_BYTES (12u * 1024u * 1024u)
#define ALLOCATION_BYTES (16u * 1024u * 1024u)
#define TRY(call) do { VkResult r_ = (call); if (r_ != VK_SUCCESS) { \
    fprintf(stderr, "FAIL line=%d %s result=%d\n", __LINE__, #call, r_); \
    result = 1; goto cleanup; } } while (0)

static uint32_t pattern(unsigned serial, unsigned index)
{
    uint32_t v = index * 0x9e3779b9u + serial * 0x85ebca6bu;
    v ^= v >> 16;
    return v * 0xc2b2ae35u;
}

static int sync_fd(int fd, uint64_t flags)
{
    struct dma_buf_sync sync = {.flags = flags};
    int r;
    do r = ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync); while (r < 0 && (errno == EINTR || errno == EAGAIN));
    if (r < 0) fprintf(stderr, "FAIL dma_buf_sync flags=%" PRIu64 " errno=%d\n", flags, errno);
    return r == 0;
}

static unsigned check(const uint32_t *map, unsigned serial, const char *label)
{
    unsigned errors = 0;
    for (unsigned i = 0; i < BUFFER_BYTES / 4; i++) {
        uint32_t expected = pattern(serial, i);
        if (map[i] != expected) {
            if (errors < 8) fprintf(stderr, "FAIL %s byte=%u expected=%08x actual=%08x\n",
                                    label, i * 4, expected, map[i]);
            errors++;
        }
    }
    printf("CHECK %s bytes=%u mismatched_words=%u\n", label, BUFFER_BYTES, errors);
    return errors;
}

int main(int argc, char **argv)
{
    int cached = 0, fixed = 0, read_only = 0, vulkan_import = 0, opaque_import = 0, opaque_only = 0;
    int renderer_import = 0;
    int dedicated_alloc = 0, device_address = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--cached")) cached = 1;
        else if (!strcmp(argv[i], "--fixed")) fixed = 1;
        else if (!strcmp(argv[i], "--read-only-dmabuf")) read_only = 1;
        else if (!strcmp(argv[i], "--vulkan-import")) vulkan_import = 1;
        else if (!strcmp(argv[i], "--opaque-import")) opaque_import = 1;
        else if (!strcmp(argv[i], "--opaque-only")) opaque_import = opaque_only = 1;
        else if (!strcmp(argv[i], "--dedicated")) dedicated_alloc = 1;
        else if (!strcmp(argv[i], "--device-address")) device_address = 1;
        else if (!strcmp(argv[i], "--renderer-import")) {
#ifdef HELIOS_VKR_ALLOCATOR_CONTROL
            renderer_import = opaque_import = opaque_only = 1;
#else
            fprintf(stderr, "UNSUPPORTED: build this control with the renderer allocator\n"); return 77;
#endif
        }
        else { fprintf(stderr, "Unknown argument: %s\n", argv[i]); return 77; }
    }
    if (vulkan_import && opaque_import) {
        fprintf(stderr, "Select only one Vulkan import handle type\n"); return 77;
    }
    if (opaque_only && (fixed || read_only)) {
        fprintf(stderr, "DMA-BUF mapping options cannot be used with --opaque-only\n"); return 77;
    }
    const VkExternalMemoryHandleTypeFlagBits import_type = opaque_import ?
        VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT : VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
    const VkExternalMemoryHandleTypeFlags handle_types = (opaque_only ? 0 : VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT) |
        (opaque_import ? VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT : 0);
    vulkan_import |= opaque_import;
    setvbuf(stdout, NULL, _IONBF, 0);
    int result = 77, fd = -1, import_fd = -1;
    unsigned errors = 0;
    VkInstance instance = VK_NULL_HANDLE;
    VkDevice device = VK_NULL_HANDLE;
    VkDeviceMemory memory = VK_NULL_HANDLE;
    VkDeviceMemory imported_memory = VK_NULL_HANDLE;
    VkBuffer buffer = VK_NULL_HANDLE;
    uint32_t *native = NULL, *external = MAP_FAILED;
    uint32_t *imported = NULL;
    void *reservation = MAP_FAILED;
#ifdef HELIOS_VKR_ALLOCATOR_CONTROL
    struct virgl_resource renderer_resource = {0};
    int renderer_mapped = 0;
#endif
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "dmabuf-cpu-map-control", .apiVersion = device_address ? VK_API_VERSION_1_2 : VK_API_VERSION_1_1};
    VkInstanceCreateInfo ic = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
    TRY(vkCreateInstance(&ic, NULL, &instance));
    uint32_t count = 1;
    VkPhysicalDevice gpu;
    TRY(vkEnumeratePhysicalDevices(instance, &count, &gpu));
    if (!count) goto cleanup;
    VkPhysicalDeviceProperties props;
    VkPhysicalDeviceMemoryProperties types;
    vkGetPhysicalDeviceProperties(gpu, &props);
    vkGetPhysicalDeviceMemoryProperties(gpu, &types);
    printf("DEVICE %s vendor=%04x device=%04x driver=0x%x fixed=%d cached=%d read_only_dmabuf=%d import_type=0x%x\n",
           props.deviceName, props.vendorID, props.deviceID, props.driverVersion,
           fixed, cached, read_only, vulkan_import ? import_type : 0);
    VkQueueFamilyProperties families[32]; count = 32;
    vkGetPhysicalDeviceQueueFamilyProperties(gpu, &count, families);
    uint32_t family = 0;
    while (family < count && !families[family].queueCount) family++;
    if (family == count) goto cleanup;
    const VkBufferUsageFlags usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
        (device_address ? VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT : 0);
    VkPhysicalDeviceExternalBufferInfo info = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_BUFFER_INFO,
        .usage = usage, .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
    for (unsigned h = 0; h < (opaque_import && !opaque_only ? 2u : 1u); h++) {
        info.handleType = h || opaque_only ? VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT : VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
        VkExternalBufferProperties external_props = {.sType = VK_STRUCTURE_TYPE_EXTERNAL_BUFFER_PROPERTIES};
        vkGetPhysicalDeviceExternalBufferProperties(gpu, &info, &external_props);
        VkExternalMemoryProperties properties = external_props.externalMemoryProperties;
        VkExternalMemoryFeatureFlags flags = properties.externalMemoryFeatures;
        printf("EXTERNAL handle=0x%x features=0x%x compatible=0x%x\n",
               info.handleType, flags, properties.compatibleHandleTypes);
        if (!(flags & VK_EXTERNAL_MEMORY_FEATURE_EXPORTABLE_BIT) ||
            ((flags & VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT) && !dedicated_alloc) ||
            (properties.compatibleHandleTypes & handle_types) != handle_types ||
            (vulkan_import && info.handleType == import_type && !(flags & VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT))) {
            fprintf(stderr, "UNSUPPORTED external buffer handle combination\n"); goto cleanup;
        }
    }
    float priority = 1;
    VkDeviceQueueCreateInfo qc = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = family, .queueCount = 1, .pQueuePriorities = &priority};
    const char *extensions[] = {VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME, VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME};
    VkDeviceCreateInfo dc = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1, .pQueueCreateInfos = &qc,
        .enabledExtensionCount = 2, .ppEnabledExtensionNames = extensions};
    VkPhysicalDeviceBufferDeviceAddressFeatures address_feature = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_BUFFER_DEVICE_ADDRESS_FEATURES};
    if (device_address) {
        VkPhysicalDeviceFeatures2 features = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
            .pNext = &address_feature};
        vkGetPhysicalDeviceFeatures2(gpu, &features);
        if (!address_feature.bufferDeviceAddress) goto cleanup;
        address_feature.bufferDeviceAddressCaptureReplay = VK_FALSE;
        address_feature.bufferDeviceAddressMultiDevice = VK_FALSE;
        dc.pNext = &address_feature;
    }
    TRY(vkCreateDevice(gpu, &dc, NULL, &device));
    VkExternalMemoryBufferCreateInfo ext = {.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
        .handleTypes = handle_types};
    VkBufferCreateInfo bc = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .pNext = &ext,
        .size = BUFFER_BYTES, .usage = usage};
    TRY(vkCreateBuffer(device, &bc, NULL, &buffer));
    VkMemoryDedicatedRequirements dedicated = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS};
    VkMemoryRequirements2 req = {.sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2, .pNext = &dedicated};
    VkBufferMemoryRequirementsInfo2 query = {.sType = VK_STRUCTURE_TYPE_BUFFER_MEMORY_REQUIREMENTS_INFO_2,
        .buffer = buffer};
    vkGetBufferMemoryRequirements2(device, &query, &req);
    if ((dedicated.requiresDedicatedAllocation && !dedicated_alloc) ||
        req.memoryRequirements.size > ALLOCATION_BYTES) goto cleanup;
    VkMemoryPropertyFlags want = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT |
        (cached ? VK_MEMORY_PROPERTY_HOST_CACHED_BIT : VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
    unsigned type = 0;
    while (type < types.memoryTypeCount && (!(req.memoryRequirements.memoryTypeBits & (1u << type)) ||
           (types.memoryTypes[type].propertyFlags & want) != want)) type++;
    if (type == types.memoryTypeCount) goto cleanup;
    printf("ALLOC size=%u type=%u flags=0x%x\n", ALLOCATION_BYTES, type, types.memoryTypes[type].propertyFlags);
    VkExportMemoryAllocateInfo export = {.sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
        .handleTypes = handle_types};
    VkMemoryDedicatedAllocateInfo dedicated_info = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        .buffer = buffer, .pNext = &export};
    VkMemoryAllocateFlagsInfo allocation_flags = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_FLAGS_INFO,
        .flags = VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT,
        .pNext = dedicated_alloc ? (void *)&dedicated_info : (void *)&export};
    VkMemoryAllocateInfo alloc = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &export,
        .allocationSize = ALLOCATION_BYTES, .memoryTypeIndex = type};
    if (dedicated_alloc) alloc.pNext = &dedicated_info;
    if (device_address) alloc.pNext = &allocation_flags;
    printf("CONTRACT dedicated=%d device_address=%d requirement=%" PRIu64 " allocation=%u\n",
           dedicated_alloc, device_address, (uint64_t)req.memoryRequirements.size, ALLOCATION_BYTES);
    TRY(vkAllocateMemory(device, &alloc, NULL, &memory));
    TRY(vkBindBufferMemory(device, buffer, memory, 0));
    TRY(vkMapMemory(device, memory, 0, VK_WHOLE_SIZE, 0, (void **)&native));
    PFN_vkGetMemoryFdKHR get_fd = (PFN_vkGetMemoryFdKHR)vkGetDeviceProcAddr(device, "vkGetMemoryFdKHR");
    if (!get_fd) { result = 1; goto cleanup; }
    VkMemoryGetFdInfoKHR get = {.sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
        .memory = memory, .handleType = opaque_only ? VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT : VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
    TRY(get_fd(device, &get, &fd));
    off_t fd_bytes = opaque_only ? ALLOCATION_BYTES : lseek(fd, 0, SEEK_END);
    if (fd_bytes < ALLOCATION_BYTES) { result = 1; goto cleanup; }
    if (vulkan_import && !renderer_import) {
        if (opaque_import) {
            /* OPAQUE_FD imports use the original allocation size/type. Every
             * handle type was queried and declared on buffer and memory. */
            get.handleType = import_type;
            TRY(get_fd(device, &get, &import_fd));
        } else {
            PFN_vkGetMemoryFdPropertiesKHR get_properties =
                (PFN_vkGetMemoryFdPropertiesKHR)vkGetDeviceProcAddr(device, "vkGetMemoryFdPropertiesKHR");
            if (!get_properties) { result = 1; goto cleanup; }
            VkMemoryFdPropertiesKHR fd_properties = {.sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR};
            TRY(get_properties(device, import_type, fd, &fd_properties));
            printf("IMPORT memory_type_bits=0x%x type=%u\n", fd_properties.memoryTypeBits, type);
            if (!(fd_properties.memoryTypeBits & (1u << type))) {
                fprintf(stderr, "UNSUPPORTED original host-visible type absent from DMA-BUF import properties\n");
                goto cleanup;
            }
            /* Vulkan consumes the duplicate on success. The original stays
             * owned here for mmap and CPU-access ioctls until cleanup. */
            import_fd = fcntl(fd, F_DUPFD_CLOEXEC, 0);
            if (import_fd < 0) { result = 1; goto cleanup; }
        }
        VkImportMemoryFdInfoKHR import = {.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
            .handleType = import_type, .fd = import_fd};
        VkMemoryAllocateInfo import_alloc = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            .pNext = &import, .allocationSize = ALLOCATION_BYTES, .memoryTypeIndex = type};
        TRY(vkAllocateMemory(device, &import_alloc, NULL, &imported_memory));
        import_fd = -1;
        TRY(vkMapMemory(device, imported_memory, 0, VK_WHOLE_SIZE, 0, (void **)&imported));
    }
#ifdef HELIOS_VKR_ALLOCATOR_CONTROL
    if (renderer_import) {
        VkPhysicalDeviceIDProperties ids = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES};
        VkPhysicalDeviceProperties2 props2 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
            .pNext = &ids};
        vkGetPhysicalDeviceProperties2(gpu, &props2);
        renderer_resource.res_id = 1;
        renderer_resource.fd_type = VIRGL_RESOURCE_FD_OPAQUE;
        renderer_resource.fd = fd; /* The mapper duplicates; this control retains ownership. */
        memcpy(renderer_resource.vulkan_info.device_uuid, ids.deviceUUID, VK_UUID_SIZE);
        memcpy(renderer_resource.vulkan_info.driver_uuid, ids.driverUUID, VK_UUID_SIZE);
        renderer_resource.vulkan_info.allocation_size = ALLOCATION_BYTES;
        renderer_resource.vulkan_info.memory_type_index = type;
        uint64_t mapped_size = 0;
        int rc = vkr_allocator_resource_map(&renderer_resource, (void **)&imported, &mapped_size);
        if (rc) { fprintf(stderr, "FAIL renderer_map=%d\n", rc); result = 1; goto cleanup; }
        renderer_mapped = 1;
        if (mapped_size != ALLOCATION_BYTES) { result = 1; goto cleanup; }
        printf("RENDERER_MAP size=%" PRIu64 " original_type=%u separate_device=1\n", mapped_size, type);
    }
#endif
    if (!opaque_only) {
        if (fixed) {
            reservation = mmap(NULL, ALLOCATION_BYTES, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (reservation == MAP_FAILED) { result = 1; goto cleanup; }
        }
        external = mmap(fixed ? reservation : NULL, ALLOCATION_BYTES, PROT_READ | (read_only ? 0 : PROT_WRITE),
                        MAP_SHARED | (fixed ? MAP_FIXED : 0), fd, 0);
        if (external == MAP_FAILED) { fprintf(stderr, "FAIL mmap errno=%d\n", errno); result = 1; goto cleanup; }
        reservation = MAP_FAILED; /* external owns this mapping now. */
        printf("MAPPINGS native=%p dmabuf=%p fd_size=%" PRId64 "\n", (void *)native, (void *)external, (int64_t)fd_bytes);
    } else {
        printf("MAPPING native=%p opaque_only=1\n", (void *)native);
    }
    if (imported) printf("MAPPING vulkan_import=%p\n", (void *)imported);
    for (unsigned i = 0; i < BUFFER_BYTES / 4; i++) native[i] = pattern(1, i);
    atomic_thread_fence(memory_order_seq_cst);
    VkMappedMemoryRange range = {.sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE, .memory = memory, .size = VK_WHOLE_SIZE};
    VkMappedMemoryRange imported_range = {.sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE,
        .memory = imported_memory, .size = VK_WHOLE_SIZE};
    TRY(vkFlushMappedMemoryRanges(device, 1, &range));
    errors += check(native, 1, "native-self");
    if (imported) {
        if (imported_memory) TRY(vkInvalidateMappedMemoryRanges(device, 1, &imported_range));
        errors += check(imported, 1, "native-to-vulkan-import");
    }
    if (opaque_only) {
        for (unsigned i = 0; i < BUFFER_BYTES / 4; i++) imported[i] = pattern(2, i);
        atomic_thread_fence(memory_order_seq_cst);
        /* Every selected type is HOST_COHERENT. The production mapper owns
         * its VkDevice/Memory, so it requires no explicit cache operation. */
        if (imported_memory) TRY(vkFlushMappedMemoryRanges(device, 1, &imported_range));
        TRY(vkInvalidateMappedMemoryRanges(device, 1, &range));
        errors += check(native, 2, "vulkan-import-to-native");
    } else {
        if (!sync_fd(fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ)) { result = 1; goto cleanup; }
        errors += check(external, 1, "native-to-dmabuf");
        if (!sync_fd(fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ)) { result = 1; goto cleanup; }
        if (!read_only) {
            if (!sync_fd(fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_WRITE)) { result = 1; goto cleanup; }
            for (unsigned i = 0; i < BUFFER_BYTES / 4; i++) external[i] = pattern(2, i);
            atomic_thread_fence(memory_order_seq_cst);
            if (!sync_fd(fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE)) { result = 1; goto cleanup; }
            TRY(vkInvalidateMappedMemoryRanges(device, 1, &range));
            errors += check(native, 2, "dmabuf-to-native");
            if (imported) {
                TRY(vkInvalidateMappedMemoryRanges(device, 1, &imported_range));
                errors += check(imported, 2, "dmabuf-to-vulkan-import");
            }
        }
    }
    result = errors ? 1 : 0;
cleanup:
#ifdef HELIOS_VKR_ALLOCATOR_CONTROL
    if (renderer_mapped && vkr_allocator_resource_unmap(&renderer_resource)) result = 1;
    vkr_allocator_fini();
#endif
    if (external != MAP_FAILED) munmap(external, ALLOCATION_BYTES);
    if (reservation != MAP_FAILED) munmap(reservation, ALLOCATION_BYTES);
    if (fd >= 0) close(fd);
    if (import_fd >= 0) close(import_fd);
    if (imported && imported_memory) vkUnmapMemory(device, imported_memory);
    if (imported_memory) vkFreeMemory(device, imported_memory, NULL);
    if (native) vkUnmapMemory(device, memory);
    if (buffer) vkDestroyBuffer(device, buffer, NULL);
    if (memory) vkFreeMemory(device, memory, NULL);
    if (device) vkDestroyDevice(device, NULL);
    if (instance) vkDestroyInstance(instance, NULL);
    printf("RESULT exit=%d mismatched_words=%u gpu_submissions=0 read_only_dmabuf=%d\n", result, errors, read_only);
    return result;
}
