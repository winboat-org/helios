/*
 * host_as_size_probe — attribute the D3D12 "uncompacted current size" failure
 * to the host Vulkan driver, with no Venus, no ICD and no guest in the loop.
 *
 * Background: the guest D3D12 probe (tools/d3d12_raytracing_probe.cpp) checks
 * the D3D12 contract stated in the DXR functional spec,
 *
 *   D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_CURRENT_SIZE_DESC:
 *   "If the acceleration structure hasn't had a compaction operation performed
 *    on it, this size is the same one reported by
 *    GetRaytracingAccelerationStructurePrebuildInfo()"
 *
 * and the guest stack reports a larger value: 392 vs a 320-byte prebuild for
 * the two BLAS, 568 vs 512 for the TLAS.
 *
 * This probe builds the same structures directly against the host driver's
 * RADV and prints
 *   - vkGetAccelerationStructureBuildSizesKHR().accelerationStructureSize (the
 *     D3D12 prebuild contract's source), and
 *   - VK_QUERY_TYPE_ACCELERATION_STRUCTURE_SIZE_KHR (what the D3D12
 *     CURRENT_SIZE query is translated from),
 * plus the compacted and serialization size queries for context.
 *
 * Host-only diagnostic. Not part of any build; see AGENTS.md tooling policy.
 *
 * Build (inside the WinBoat container, where /dev/dri/renderD128 lives):
 *   gcc -O1 -o /tmp/host_as_size_probe host_as_size_probe.c -lvulkan
 */

#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CHECK(expr)                                                                                 \
    do {                                                                                            \
        VkResult r_ = (expr);                                                                       \
        if (r_ != VK_SUCCESS) {                                                                     \
            fprintf(stderr, "FAIL %s -> VkResult %d (line %d)\n", #expr, (int)r_, __LINE__);        \
            exit(1);                                                                                \
        }                                                                                           \
    } while (0)

#define DEVPROC(name) ((PFN_##name)vkGetDeviceProcAddr(dev, #name))

static PFN_vkGetAccelerationStructureBuildSizesKHR p_GetASBuildSizes;
static PFN_vkCreateAccelerationStructureKHR p_CreateAS;
static PFN_vkGetAccelerationStructureDeviceAddressKHR p_GetASAddress;
static PFN_vkCmdBuildAccelerationStructuresKHR p_CmdBuildAS;
static PFN_vkCmdWriteAccelerationStructuresPropertiesKHR p_CmdWriteASProps;
static PFN_vkCmdCopyAccelerationStructureKHR p_CmdCopyAS;
static PFN_vkDestroyAccelerationStructureKHR p_DestroyAS;
static PFN_vkGetBufferDeviceAddressKHR p_GetBufAddress;

struct buf {
    VkBuffer vk;
    VkDeviceMemory mem;
    void *map;
    VkDeviceAddress addr;
    VkDeviceSize size;
};

static uint32_t find_memory_type(VkPhysicalDevice pdev, uint32_t bits, VkMemoryPropertyFlags want)
{
    VkPhysicalDeviceMemoryProperties mp;
    uint32_t i;

    vkGetPhysicalDeviceMemoryProperties(pdev, &mp);
    for (i = 0; i < mp.memoryTypeCount; i++) {
        if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
            return i;
    }
    fprintf(stderr, "no memory type for bits %#x want %#x\n", bits, (unsigned)want);
    exit(1);
}

static struct buf make_buf(VkDevice dev, VkPhysicalDevice pdev, VkDeviceSize size,
        VkBufferUsageFlags usage)
{
    struct buf b;
    VkMemoryRequirements req;
    VkMemoryAllocateFlagsInfo flags_info;
    VkMemoryAllocateInfo alloc;
    VkBufferCreateInfo ci;

    memset(&b, 0, sizeof(b));
    memset(&ci, 0, sizeof(ci));
    ci.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO;
    ci.size = size;
    ci.usage = usage;
    ci.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
    CHECK(vkCreateBuffer(dev, &ci, NULL, &b.vk));

    vkGetBufferMemoryRequirements(dev, b.vk, &req);

    memset(&flags_info, 0, sizeof(flags_info));
    flags_info.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_FLAGS_INFO;
    flags_info.flags = VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT;

    memset(&alloc, 0, sizeof(alloc));
    alloc.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
    alloc.pNext = &flags_info;
    alloc.allocationSize = req.size;
    alloc.memoryTypeIndex = find_memory_type(pdev, req.memoryTypeBits,
            VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
    CHECK(vkAllocateMemory(dev, &alloc, NULL, &b.mem));
    CHECK(vkBindBufferMemory(dev, b.vk, b.mem, 0));
    CHECK(vkMapMemory(dev, b.mem, 0, VK_WHOLE_SIZE, 0, &b.map));

    b.size = size;
    if (usage & VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT)
        b.addr = p_GetBufAddress(dev, &(VkBufferDeviceAddressInfo){
                VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_INFO, NULL, b.vk});
    return b;
}

/* One acceleration structure we build and then query the sizes of. */
struct rtas {
    const char *name;
    VkAccelerationStructureTypeKHR type;
    VkAccelerationStructureBuildGeometryInfoKHR build;
    VkAccelerationStructureBuildRangeInfoKHR range;
    VkAccelerationStructureGeometryKHR geom;
    uint32_t primitive_count;
    struct buf storage;
    struct buf scratch;
    VkAccelerationStructureKHR vk;
    VkDeviceSize build_size; /* what vkGetAccelerationStructureBuildSizesKHR said */
};

static void rtas_alloc(struct rtas *a, VkDevice dev, VkPhysicalDevice pdev, VkDeviceSize scratch_align)
{
    VkAccelerationStructureBuildSizesInfoKHR sizes;
    VkAccelerationStructureCreateInfoKHR ci;

    a->build.pGeometries = &a->geom;
    a->build.geometryCount = 1;

    memset(&sizes, 0, sizeof(sizes));
    sizes.sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_BUILD_SIZES_INFO_KHR;
    p_GetASBuildSizes(dev, VK_ACCELERATION_STRUCTURE_BUILD_TYPE_DEVICE_KHR, &a->build,
            &a->primitive_count, &sizes);
    a->build_size = sizes.accelerationStructureSize;

    a->storage = make_buf(dev, pdev, sizes.accelerationStructureSize,
            VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_STORAGE_BIT_KHR |
            VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT);
    a->scratch = make_buf(dev, pdev, sizes.buildScratchSize + scratch_align,
            VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT);

    memset(&ci, 0, sizeof(ci));
    ci.sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_CREATE_INFO_KHR;
    ci.buffer = a->storage.vk;
    ci.offset = 0;
    ci.size = sizes.accelerationStructureSize;
    ci.type = a->type;
    CHECK(p_CreateAS(dev, &ci, NULL, &a->vk));

    a->build.dstAccelerationStructure = a->vk;
    a->build.pGeometries = &a->geom;
    a->build.geometryCount = 1;
    a->build.scratchData.deviceAddress =
            (a->scratch.addr + scratch_align - 1) & ~(VkDeviceAddress)(scratch_align - 1);
}

static int cmp_u64(const void *a, const void *b)
{
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y ? 1 : 0;
}

/* Query one property of one acceleration structure in its own submission and
 * return the value. Batched multi-AS writes returned cross-contaminated values
 * on this driver, so this is deliberately the least ambiguous form. */
static uint64_t measure_as(VkQueue queue, VkCommandBuffer cb, VkCommandBufferBeginInfo *bi,
        VkQueryPool pool, VkAccelerationStructureKHR as, VkQueryType type,
        struct buf *out, void *qmap)
{
    VkMemoryBarrier barrier;
    uint64_t value;

    CHECK(vkResetCommandBuffer(cb, 0));
    bi->flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    CHECK(vkBeginCommandBuffer(cb, bi));

    /* The build/copy is in an earlier submission; make its device writes
     * visible to the query. */
    memset(&barrier, 0, sizeof(barrier));
    barrier.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER;
    barrier.srcAccessMask = VK_ACCESS_ACCELERATION_STRUCTURE_WRITE_BIT_KHR;
    barrier.dstAccessMask = VK_ACCESS_ACCELERATION_STRUCTURE_READ_BIT_KHR;
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ACCELERATION_STRUCTURE_BUILD_BIT_KHR,
            VK_PIPELINE_STAGE_ACCELERATION_STRUCTURE_BUILD_BIT_KHR, 0, 1, &barrier, 0, NULL, 0, NULL);

    vkCmdResetQueryPool(cb, pool, 0, 1);
    p_CmdWriteASProps(cb, 1, &as, type, pool, 0);
    vkCmdCopyQueryPoolResults(cb, pool, 0, 1, out->vk, 0, sizeof(uint64_t),
            VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WAIT_BIT);

    CHECK(vkEndCommandBuffer(cb));
    CHECK(vkQueueSubmit(queue, 1, &(VkSubmitInfo){
            VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 0, NULL, NULL, 1, &cb, 0, NULL}, VK_NULL_HANDLE));
    CHECK(vkQueueWaitIdle(queue));
    memcpy(&value, qmap, sizeof(value));
    return value;
}

/* Create an acceleration structure of an explicit size over fresh storage, the
 * way a D3D12 app allocates for a CLONE/COMPACT destination. */
static void create_as_of_size(VkDevice dev, VkPhysicalDevice pdev, VkDeviceSize size,
        VkAccelerationStructureTypeKHR type, struct buf *storage, VkAccelerationStructureKHR *as)
{
    VkAccelerationStructureCreateInfoKHR ci;

    *storage = make_buf(dev, pdev, size,
            VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_STORAGE_BIT_KHR |
            VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT);

    memset(&ci, 0, sizeof(ci));
    ci.sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_CREATE_INFO_KHR;
    ci.buffer = storage->vk;
    ci.size = size;
    ci.type = type;
    CHECK(p_CreateAS(dev, &ci, NULL, as));
}

int main(void)
{
    VkPhysicalDeviceRayTracingMaintenance1FeaturesKHR maint1;
    VkPhysicalDeviceAccelerationStructureFeaturesKHR accel_feat;
    VkPhysicalDeviceAccelerationStructurePropertiesKHR accel_props;
    VkPhysicalDeviceBufferDeviceAddressFeatures bda_feat;
    VkPhysicalDeviceProperties2 props2;
    VkPhysicalDeviceFeatures2 feat2;
    VkPhysicalDeviceProperties props;
    VkPhysicalDevice pdev = VK_NULL_HANDLE;
    VkPhysicalDeviceMemoryProperties mp;
    struct rtas cases[4];
    VkQueueFamilyProperties *qf;
    VkPhysicalDevice *devs;
    VkQueryPool query_pool[3];
    struct buf query_buf;
    VkDeviceQueueCreateInfo qci;
    VkCommandBufferBeginInfo bi;
    VkCommandPool cmd_pool;
    VkCommandBuffer cb;
    VkDevice dev;
    VkInstance inst;
    VkQueue queue;
    int qfi = -1;
    uint32_t dev_count, qf_count, i, n;
    const char *want_ext[] = {
        VK_KHR_ACCELERATION_STRUCTURE_EXTENSION_NAME,
        VK_KHR_RAY_TRACING_MAINTENANCE_1_EXTENSION_NAME,
        VK_KHR_DEFERRED_HOST_OPERATIONS_EXTENSION_NAME,
    };
    float prio = 1.0f;
    uint64_t results[4][3];
    VkDeviceSize scratch_align;
    VkResult r;
    void *qmap;

    CHECK(vkCreateInstance(&(VkInstanceCreateInfo){
            VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL, 0,
            &(VkApplicationInfo){VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL,
                    "host_as_size_probe", 1, "host_as_size_probe", 1, VK_API_VERSION_1_3},
            0, NULL, 0, NULL}, NULL, &inst));

    CHECK(vkEnumeratePhysicalDevices(inst, &dev_count, NULL));
    devs = calloc(dev_count, sizeof(*devs));
    CHECK(vkEnumeratePhysicalDevices(inst, &dev_count, devs));

    for (i = 0; i < dev_count && pdev == VK_NULL_HANDLE; i++) {
        uint32_t ec;
        VkExtensionProperties *ep;

        CHECK(vkEnumerateDeviceExtensionProperties(devs[i], NULL, &ec, NULL));
        ep = calloc(ec, sizeof(*ep));
        CHECK(vkEnumerateDeviceExtensionProperties(devs[i], NULL, &ec, ep));
        for (n = 0; n < sizeof(want_ext) / sizeof(want_ext[0]); n++) {
            uint32_t k;
            int found = 0;
            for (k = 0; k < ec; k++)
                if (!strcmp(ep[k].extensionName, want_ext[n]))
                    found = 1;
            if (!found)
                break;
        }
        free(ep);
        if (n == sizeof(want_ext) / sizeof(want_ext[0]))
            pdev = devs[i];
    }
    if (pdev == VK_NULL_HANDLE) {
        fprintf(stderr, "no device with acceleration structure + maintenance1\n");
        return 1;
    }

    vkGetPhysicalDeviceProperties(pdev, &props);
    printf("device,%s,driverVersion,%u.%u.%u\n", props.deviceName,
            VK_API_VERSION_MAJOR(props.driverVersion), VK_API_VERSION_MINOR(props.driverVersion),
            VK_API_VERSION_PATCH(props.driverVersion));

    memset(&accel_feat, 0, sizeof(accel_feat));
    accel_feat.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_FEATURES_KHR;
    memset(&maint1, 0, sizeof(maint1));
    maint1.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_TRACING_MAINTENANCE_1_FEATURES_KHR;
    accel_feat.pNext = &maint1;
    memset(&bda_feat, 0, sizeof(bda_feat));
    bda_feat.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_BUFFER_DEVICE_ADDRESS_FEATURES;
    maint1.pNext = &bda_feat;

    memset(&feat2, 0, sizeof(feat2));
    feat2.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2;
    feat2.pNext = &accel_feat;
    vkGetPhysicalDeviceFeatures2(pdev, &feat2);
    if (!accel_feat.accelerationStructure || !maint1.rayTracingMaintenance1 || !bda_feat.bufferDeviceAddress) {
        fprintf(stderr, "missing required features (as=%d maint1=%d bda=%d)\n",
                accel_feat.accelerationStructure, maint1.rayTracingMaintenance1, bda_feat.bufferDeviceAddress);
        return 1;
    }

    memset(&accel_props, 0, sizeof(accel_props));
    accel_props.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_PROPERTIES_KHR;
    memset(&props2, 0, sizeof(props2));
    props2.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2;
    props2.pNext = &accel_props;
    vkGetPhysicalDeviceProperties2(pdev, &props2);
    scratch_align = accel_props.minAccelerationStructureScratchOffsetAlignment;
    if (scratch_align == 0)
        scratch_align = 1;
    printf("scratch_alignment,%llu\n", (unsigned long long)scratch_align);

    CHECK(vkEnumeratePhysicalDevices(inst, &dev_count, NULL));
    vkGetPhysicalDeviceQueueFamilyProperties(pdev, &qf_count, NULL);
    qf = calloc(qf_count, sizeof(*qf));
    vkGetPhysicalDeviceQueueFamilyProperties(pdev, &qf_count, qf);
    for (i = 0; i < qf_count; i++) {
        if ((qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) && (qf[i].queueFlags & VK_QUEUE_COMPUTE_BIT)) {
            qfi = (int)i;
            break;
        }
    }
    if (qfi < 0) {
        fprintf(stderr, "no graphics+compute queue family\n");
        return 1;
    }

    memset(&qci, 0, sizeof(qci));
    qci.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO;
    qci.queueFamilyIndex = (uint32_t)qfi;
    qci.queueCount = 1;
    qci.pQueuePriorities = &prio;

    CHECK(vkCreateDevice(pdev, &(VkDeviceCreateInfo){
            VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, &feat2, 0,
            1, &qci, 0, NULL,
            (uint32_t)(sizeof(want_ext) / sizeof(want_ext[0])), want_ext,
            NULL}, NULL, &dev));
    vkGetDeviceQueue(dev, (uint32_t)qfi, 0, &queue);

    p_GetASBuildSizes = DEVPROC(vkGetAccelerationStructureBuildSizesKHR);
    p_CreateAS = DEVPROC(vkCreateAccelerationStructureKHR);
    p_GetASAddress = DEVPROC(vkGetAccelerationStructureDeviceAddressKHR);
    p_CmdBuildAS = DEVPROC(vkCmdBuildAccelerationStructuresKHR);
    p_CmdWriteASProps = DEVPROC(vkCmdWriteAccelerationStructuresPropertiesKHR);
    p_CmdCopyAS = DEVPROC(vkCmdCopyAccelerationStructureKHR);
    p_DestroyAS = DEVPROC(vkDestroyAccelerationStructureKHR);
    p_GetBufAddress = DEVPROC(vkGetBufferDeviceAddressKHR);
    if (!p_GetBufAddress)
        p_GetBufAddress = DEVPROC(vkGetBufferDeviceAddress);
    if (!p_GetASBuildSizes || !p_CreateAS || !p_GetASAddress || !p_CmdBuildAS ||
            !p_CmdWriteASProps || !p_DestroyAS || !p_GetBufAddress) {
        fprintf(stderr, "missing device entry points\n");
        return 1;
    }

    memset(cases, 0, sizeof(cases));

    /* Case 0: one triangle, mirroring the guest probe's BLAS 0. */
    {
        struct buf verts = make_buf(dev, pdev, 3 * 12,
                VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_BIT_KHR |
                VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT);
        float v[9] = {-1, -1, 0, 1, -1, 0, 0, 1, 0};
        memcpy(verts.map, v, sizeof(v));

        cases[0].name = "blas-triangle-1";
        cases[0].type = VK_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL_KHR;
        cases[0].geom = (VkAccelerationStructureGeometryKHR){
            .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_KHR,
            .geometryType = VK_GEOMETRY_TYPE_TRIANGLES_KHR,
            .geometry = {.triangles = {
                .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_TRIANGLES_DATA_KHR,
                .vertexFormat = VK_FORMAT_R32G32B32_SFLOAT,
                .vertexData = {.deviceAddress = verts.addr},
                .vertexStride = 12,
                .maxVertex = 3,
                .indexType = VK_INDEX_TYPE_NONE_KHR}},
            .flags = VK_GEOMETRY_OPAQUE_BIT_KHR};
        cases[0].primitive_count = 1;
        cases[0].build = (VkAccelerationStructureBuildGeometryInfoKHR){
            VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_BUILD_GEOMETRY_INFO_KHR, NULL,
            VK_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL_KHR,
            VK_BUILD_ACCELERATION_STRUCTURE_PREFER_FAST_TRACE_BIT_KHR |
            VK_BUILD_ACCELERATION_STRUCTURE_ALLOW_COMPACTION_BIT_KHR,
            VK_BUILD_ACCELERATION_STRUCTURE_MODE_BUILD_KHR};
        cases[0].range = (VkAccelerationStructureBuildRangeInfoKHR){1, 0, 0, 0};
    }

    /* Case 1: one AABB, mirroring the guest probe's BLAS 1. */
    {
        struct buf aabb = make_buf(dev, pdev, 24,
                VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_BIT_KHR |
                VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT);
        float b[6] = {-1, -1, 0, 1, 1, 0.5f};
        memcpy(aabb.map, b, sizeof(b));

        cases[1].name = "blas-aabb-1";
        cases[1].type = VK_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL_KHR;
        cases[1].geom = (VkAccelerationStructureGeometryKHR){
            .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_KHR,
            .geometryType = VK_GEOMETRY_TYPE_AABBS_KHR,
            .geometry = {.aabbs = {
                .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_AABBS_DATA_KHR,
                .data = {.deviceAddress = aabb.addr},
                .stride = 24}},
            .flags = VK_GEOMETRY_OPAQUE_BIT_KHR};
        cases[1].primitive_count = 1;
        cases[1].build = (VkAccelerationStructureBuildGeometryInfoKHR){
            VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_BUILD_GEOMETRY_INFO_KHR, NULL,
            VK_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL_KHR,
            VK_BUILD_ACCELERATION_STRUCTURE_PREFER_FAST_TRACE_BIT_KHR |
            VK_BUILD_ACCELERATION_STRUCTURE_ALLOW_COMPACTION_BIT_KHR,
            VK_BUILD_ACCELERATION_STRUCTURE_MODE_BUILD_KHR};
        cases[1].range = (VkAccelerationStructureBuildRangeInfoKHR){1, 0, 0, 0};
    }

    /* Case 2: 64 triangles — a case where the packed size can fall below the
     * driver's static upper bound, to see whether the two answers diverge the
     * other way. */
    {
        struct buf verts = make_buf(dev, pdev, 64 * 3 * 12,
                VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_BIT_KHR |
                VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT);
        float *v = verts.map;
        for (i = 0; i < 64; i++) {
            v[i * 9 + 0] = (float)i;       v[i * 9 + 1] = 0.0f;      v[i * 9 + 2] = 0.0f;
            v[i * 9 + 3] = (float)i + 1.0f; v[i * 9 + 4] = 0.0f;     v[i * 9 + 5] = 0.0f;
            v[i * 9 + 6] = (float)i;       v[i * 9 + 7] = 0.5f;      v[i * 9 + 8] = 0.0f;
        }

        cases[2].name = "blas-triangle-64";
        cases[2].type = VK_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL_KHR;
        cases[2].geom = (VkAccelerationStructureGeometryKHR){
            .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_KHR,
            .geometryType = VK_GEOMETRY_TYPE_TRIANGLES_KHR,
            .geometry = {.triangles = {
                .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_TRIANGLES_DATA_KHR,
                .vertexFormat = VK_FORMAT_R32G32B32_SFLOAT,
                .vertexData = {.deviceAddress = verts.addr},
                .vertexStride = 12,
                .maxVertex = 64 * 3,
                .indexType = VK_INDEX_TYPE_NONE_KHR}},
            .flags = VK_GEOMETRY_OPAQUE_BIT_KHR};
        cases[2].primitive_count = 64;
        cases[2].build = (VkAccelerationStructureBuildGeometryInfoKHR){
            VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_BUILD_GEOMETRY_INFO_KHR, NULL,
            VK_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL_KHR,
            VK_BUILD_ACCELERATION_STRUCTURE_PREFER_FAST_TRACE_BIT_KHR |
            VK_BUILD_ACCELERATION_STRUCTURE_ALLOW_COMPACTION_BIT_KHR,
            VK_BUILD_ACCELERATION_STRUCTURE_MODE_BUILD_KHR};
        cases[2].range = (VkAccelerationStructureBuildRangeInfoKHR){64, 0, 0, 0};
    }

    for (i = 0; i < 3; i++)
        rtas_alloc(&cases[i], dev, pdev, scratch_align);

    /* Case 3: a two-instance TLAS over cases 0 and 1, array-of-pointers. */
    {
        struct buf instances = make_buf(dev, pdev, 2 * sizeof(VkAccelerationStructureInstanceKHR),
                VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_BIT_KHR |
                VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT);
        struct buf ptrs = make_buf(dev, pdev, 2 * sizeof(VkDeviceAddress),
                VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_BIT_KHR |
                VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT);
        VkAccelerationStructureInstanceKHR *inst = instances.map;
        VkDeviceAddress *pa = ptrs.map;

        for (i = 0; i < 2; i++) {
            memset(&inst[i], 0, sizeof(inst[i]));
            inst[i].transform.matrix[0][0] = 1.0f;
            inst[i].transform.matrix[1][1] = 1.0f;
            inst[i].transform.matrix[2][2] = 1.0f;
            inst[i].transform.matrix[0][3] = i ? 3.0f : 0.0f;
            inst[i].instanceCustomIndex = i;
            inst[i].mask = 1;
            inst[i].instanceShaderBindingTableRecordOffset = 0;
            inst[i].accelerationStructureReference =
                    p_GetASAddress(dev, &(VkAccelerationStructureDeviceAddressInfoKHR){
                            VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_DEVICE_ADDRESS_INFO_KHR,
                            NULL, cases[i].vk});
            pa[i] = instances.addr + i * sizeof(VkAccelerationStructureInstanceKHR);
        }

        cases[3].name = "tlas-instances-2";
        cases[3].type = VK_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL_KHR;
        cases[3].geom = (VkAccelerationStructureGeometryKHR){
            .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_KHR,
            .geometryType = VK_GEOMETRY_TYPE_INSTANCES_KHR,
            .geometry = {.instances = {
                .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_INSTANCES_DATA_KHR,
                .arrayOfPointers = VK_TRUE,
                .data = {.deviceAddress = ptrs.addr}}},
            .flags = VK_GEOMETRY_OPAQUE_BIT_KHR};
        cases[3].primitive_count = 2;
        cases[3].build = (VkAccelerationStructureBuildGeometryInfoKHR){
            VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_BUILD_GEOMETRY_INFO_KHR, NULL,
            VK_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL_KHR,
            VK_BUILD_ACCELERATION_STRUCTURE_PREFER_FAST_TRACE_BIT_KHR |
            VK_BUILD_ACCELERATION_STRUCTURE_ALLOW_UPDATE_BIT_KHR,
            VK_BUILD_ACCELERATION_STRUCTURE_MODE_BUILD_KHR};
        cases[3].range = (VkAccelerationStructureBuildRangeInfoKHR){2, 0, 0, 0};
        rtas_alloc(&cases[3], dev, pdev, scratch_align);
    }

    CHECK(vkCreateCommandPool(dev, &(VkCommandPoolCreateInfo){
            VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, NULL,
            VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, (uint32_t)qfi}, NULL, &cmd_pool));
    CHECK(vkAllocateCommandBuffers(dev, &(VkCommandBufferAllocateInfo){
            VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, NULL, cmd_pool,
            VK_COMMAND_BUFFER_LEVEL_PRIMARY, 1}, &cb));

    for (i = 0; i < 4; i++) {
        cases[i].build.dstAccelerationStructure = cases[i].vk;
        cases[i].build.pGeometries = &cases[i].geom;
        cases[i].build.geometryCount = 1;
    }

    /* One query pool per type, each holding a single query, and one AS queried
     * per submission: a batched multi-AS/multi-type query in a single command
     * buffer returned cross-contaminated values on this driver, so measure in
     * the least ambiguous way possible. */
    n = 4;
    {
        const VkQueryType types[3] = {
            VK_QUERY_TYPE_ACCELERATION_STRUCTURE_SIZE_KHR,
            VK_QUERY_TYPE_ACCELERATION_STRUCTURE_COMPACTED_SIZE_KHR,
            VK_QUERY_TYPE_ACCELERATION_STRUCTURE_SERIALIZATION_SIZE_KHR,
        };
        for (i = 0; i < 3; i++)
            CHECK(vkCreateQueryPool(dev, &(VkQueryPoolCreateInfo){
                    VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO, NULL, 0,
                    types[i], 1, 0}, NULL, &query_pool[i]));
    }
    query_buf = make_buf(dev, pdev, sizeof(uint64_t), VK_BUFFER_USAGE_TRANSFER_DST_BIT);

    memset(&bi, 0, sizeof(bi));
    bi.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
    bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    CHECK(vkBeginCommandBuffer(cb, &bi));

    {
        VkAccelerationStructureKHR handles[4];
        const VkAccelerationStructureBuildRangeInfoKHR *ranges[4];
        VkAccelerationStructureBuildGeometryInfoKHR builds[4];
        VkMemoryBarrier barrier;

        for (i = 0; i < 4; i++) {
            handles[i] = cases[i].vk;
            ranges[i] = &cases[i].range;
            builds[i] = cases[i].build;
        }
        p_CmdBuildAS(cb, 4, builds, ranges);

        memset(&barrier, 0, sizeof(barrier));
        barrier.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER;
        barrier.srcAccessMask = VK_ACCESS_ACCELERATION_STRUCTURE_WRITE_BIT_KHR;
        barrier.dstAccessMask = VK_ACCESS_ACCELERATION_STRUCTURE_READ_BIT_KHR;
        vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_ACCELERATION_STRUCTURE_BUILD_BIT_KHR,
                VK_PIPELINE_STAGE_ACCELERATION_STRUCTURE_BUILD_BIT_KHR, 0, 1, &barrier, 0, NULL, 0, NULL);
    }

    CHECK(vkEndCommandBuffer(cb));
    CHECK(vkQueueSubmit(queue, 1, &(VkSubmitInfo){
            VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 0, NULL, NULL, 1, &cb, 0, NULL}, VK_NULL_HANDLE));
    CHECK(vkQueueWaitIdle(queue));

    CHECK(vkMapMemory(dev, query_buf.mem, 0, VK_WHOLE_SIZE, 0, &qmap));

    {
        const VkQueryType types[3] = {
            VK_QUERY_TYPE_ACCELERATION_STRUCTURE_SIZE_KHR,
            VK_QUERY_TYPE_ACCELERATION_STRUCTURE_COMPACTED_SIZE_KHR,
            VK_QUERY_TYPE_ACCELERATION_STRUCTURE_SERIALIZATION_SIZE_KHR,
        };
        uint32_t t;

        printf("case,prebuild_as_size,current_size_query,compacted_size_query,serialization_size_query,delta\n");
        for (i = 0; i < n; i++) {
            for (t = 0; t < 3; t++)
                results[i][t] = measure_as(queue, cb, &bi, query_pool[t], cases[i].vk, types[t],
                        &query_buf, qmap);

            printf("%s,%llu,%llu,%llu,%llu,%lld\n", cases[i].name,
                    (unsigned long long)cases[i].build_size,
                    (unsigned long long)results[i][0],
                    (unsigned long long)results[i][1],
                    (unsigned long long)results[i][2],
                    (long long)((int64_t)results[i][0] - (int64_t)cases[i].build_size));
        }
    }

    /* D3D12 also has to answer CURRENT_SIZE for structures that came from a
     * CLONE or a COMPACT copy: a clone of an uncompacted structure must report
     * the source's prebuild size, a compacted structure must report the
     * compacted size. Measure what the host says for both. */
    {
        struct buf compact_storage, clone_storage;
        VkAccelerationStructureKHR compact_as, clone_as;
        VkCopyAccelerationStructureInfoKHR copy;
        uint64_t compact_size = results[0][1];
        uint64_t clone_size = cases[0].build_size;
        uint64_t got_size, got_compacted;

        create_as_of_size(dev, pdev, compact_size, VK_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL_KHR,
                &compact_storage, &compact_as);
        create_as_of_size(dev, pdev, clone_size, VK_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL_KHR,
                &clone_storage, &clone_as);

        for (i = 0; i < 2; i++) {
            CHECK(vkResetCommandBuffer(cb, 0));
            bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
            CHECK(vkBeginCommandBuffer(cb, &bi));

            memset(&copy, 0, sizeof(copy));
            copy.sType = VK_STRUCTURE_TYPE_COPY_ACCELERATION_STRUCTURE_INFO_KHR;
            copy.src = cases[0].vk;
            copy.dst = i ? clone_as : compact_as;
            copy.mode = i ? VK_COPY_ACCELERATION_STRUCTURE_MODE_CLONE_KHR
                          : VK_COPY_ACCELERATION_STRUCTURE_MODE_COMPACT_KHR;
            p_CmdCopyAS(cb, &copy);

            CHECK(vkEndCommandBuffer(cb));
            CHECK(vkQueueSubmit(queue, 1, &(VkSubmitInfo){
                    VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 0, NULL, NULL, 1, &cb, 0, NULL}, VK_NULL_HANDLE));
            CHECK(vkQueueWaitIdle(queue));

            got_size = measure_as(queue, cb, &bi, query_pool[0], i ? clone_as : compact_as,
                    VK_QUERY_TYPE_ACCELERATION_STRUCTURE_SIZE_KHR, &query_buf, qmap);
            got_compacted = measure_as(queue, cb, &bi, query_pool[1], i ? clone_as : compact_as,
                    VK_QUERY_TYPE_ACCELERATION_STRUCTURE_COMPACTED_SIZE_KHR, &query_buf, qmap);

            printf("%s,allocated=%llu,current_size_query=%llu,compacted_size_query=%llu,source_prebuild=%llu,source_compacted=%llu\n",
                    i ? "clone-of-blas-triangle-1" : "compact-of-blas-triangle-1",
                    (unsigned long long)(i ? clone_size : compact_size),
                    (unsigned long long)got_size, (unsigned long long)got_compacted,
                    (unsigned long long)cases[0].build_size, (unsigned long long)compact_size);
        }
    }

    {
        uint32_t bad = 0;
        for (i = 0; i < n; i++)
            if (results[i][0] > cases[i].build_size)
                bad++;
        printf("VERDICT,host_current_size_exceeds_prebuild,%u_of_%u\n", bad, n);
    }

    (void)cmp_u64;
    (void)mp;
    return 0;
}
