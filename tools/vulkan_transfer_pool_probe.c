/* SPDX-License-Identifier: MIT
 * Bounded upload -> device buffer -> optimal image -> readback control.
 * Compare distinct allocations with interleaved buffer/image allocations.
 * Four submissions may be outstanding; fence completion protects every reuse.
 * No WSI, idle wait, benchmark detection, or changes to production defaults.
 * Windows: run in the interactive user's desktop session with a pinned ICD.
 * Usage: vulkan_transfer_pool_probe [--validate] [--mixed|--separate]
 *        [--upload-device-local] [--readback-cached]
 *        [--copy-burst [--batch-regions] [--direct-readback]]
 *        [--init-only] [--expect-icd PATH --expect-icd-sha256 HASH]
 * Windows refuses elevation (the loader ignores ICD overrides there). An
 * expected ICD is checked after enumeration, before device/memory creation.
 * --init-only reports identity/properties without creating a VkDevice.
 * Exit 0: exact bytes and successful completion; 1: failure; 77: unsupported.
 * A timeout stops admission and retains pending resources until completion or
 * device loss. It is never converted to a pass or permission to free backing.
 */
#define _CRT_SECURE_NO_WARNINGS
#include <vulkan/vulkan.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#ifdef _WIN32
#include <windows.h>
#include <tlhelp32.h>
#include <wincrypt.h>
#else
#include <threads.h>
#endif

#define SLOTS 4
#define SHAPES 2
#define ROUNDS 24
#define WAIT_NS UINT64_C(5000000000)
#define TRY(call) do { VkResult r_ = (call); if (r_ != VK_SUCCESS) { \
    fprintf(stderr, "FAIL line=%d %s result=%d\n", __LINE__, #call, r_); \
    failed = 1; goto cleanup; } } while (0)

struct resource {
    VkBuffer buffer;
    VkImage image;
    VkMemoryRequirements req;
    VkDeviceMemory memory;
    VkDeviceSize offset;
};
struct host_buffer { struct resource r; uint32_t *map; };
struct slot {
    struct resource resources[SHAPES * 2];
    struct host_buffer upload, readback;
    VkDeviceMemory pool;
    VkCommandPool commands;
    VkCommandBuffer primary, secondary;
    VkFence fence;
    unsigned serial;
    int pending, initialized;
};
static const VkExtent3D extents[SHAPES] = {{1024, 1024, 1}, {65, 65, 65}};
static unsigned validation_errors, timeout_seen;
static VkMemoryPropertyFlags upload_properties = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT;
static VkMemoryPropertyFlags readback_properties = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT;

#ifdef _WIN32
static int windows_session_allowed(void)
{
    DWORD session = 0, returned = 0;
    HANDLE token = NULL;
    TOKEN_ELEVATION elevation = {0};
    if (!ProcessIdToSessionId(GetCurrentProcessId(), &session) || !session) {
        fprintf(stderr, "REFUSED: Windows graphics probes require an interactive session\n");
        return 0;
    }
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token)) {
        fprintf(stderr, "REFUSED: cannot inspect process elevation\n");
        return 0;
    }
    BOOL queried = GetTokenInformation(token, TokenElevation, &elevation, sizeof(elevation), &returned);
    CloseHandle(token);
    if (!queried || returned != sizeof(elevation) || elevation.TokenIsElevated) {
        fprintf(stderr, "REFUSED: non-elevated token required; Vulkan ignores ICD overrides when elevated\n");
        return 0;
    }
    printf("PROCESS pid=%lu session=%lu elevated=0\n", GetCurrentProcessId(), session);
    return 1;
}

static int file_sha256(const char *path, char hex[65])
{
    HANDLE file = INVALID_HANDLE_VALUE;
    HCRYPTPROV provider = 0;
    HCRYPTHASH hash = 0;
    BYTE data[65536], digest[32];
    DWORD count = 0, length = sizeof(digest);
    int ok = 0;
    file = CreateFileA(path, GENERIC_READ, FILE_SHARE_READ, NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
    if (file == INVALID_HANDLE_VALUE ||
        !CryptAcquireContextA(&provider, NULL, NULL, PROV_RSA_AES, CRYPT_VERIFYCONTEXT) ||
        !CryptCreateHash(provider, CALG_SHA_256, 0, 0, &hash)) goto done;
    for (;;) {
        if (!ReadFile(file, data, sizeof(data), &count, NULL)) goto done;
        if (!count) break;
        if (!CryptHashData(hash, data, count, 0)) goto done;
    }
    if (!CryptGetHashParam(hash, HP_HASHVAL, digest, &length, 0) || length != sizeof(digest)) goto done;
    for (unsigned i = 0; i < sizeof(digest); i++) sprintf(hex + i * 2, "%02x", digest[i]);
    ok = 1;
done:
    if (hash) CryptDestroyHash(hash);
    if (provider) CryptReleaseContext(provider, 0);
    if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
    return ok;
}

static int verify_loaded_icd(const char *expected_path, const char *expected_sha256)
{
    char path[MAX_PATH], digest[65];
    DWORD length = GetFullPathNameA(expected_path, sizeof(path), path, NULL);
    if (!length || length >= sizeof(path)) {
        fprintf(stderr, "REFUSED: invalid expected ICD path\n");
        return 0;
    }
    for (char *p = path; *p; p++) if (*p == '/') *p = '\\';
    HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, GetCurrentProcessId());
    if (snapshot == INVALID_HANDLE_VALUE) {
        fprintf(stderr, "REFUSED: cannot enumerate loaded ICD modules\n");
        return 0;
    }
    MODULEENTRY32 module = {.dwSize = sizeof(module)};
    unsigned matches = 0, unexpected = 0;
    BOOL found = Module32First(snapshot, &module);
    while (found) {
        if (!_stricmp(module.szExePath, path)) matches++;
        else if (!_strnicmp(module.szModule, "vulkan_virtio", 13)) unexpected++;
        if (!_strnicmp(module.szModule, "vulkan_virtio", 13))
            printf("ICD_MODULE path=%s\n", module.szExePath);
        found = Module32Next(snapshot, &module);
    }
    DWORD enumeration_error = GetLastError();
    CloseHandle(snapshot);
    if (enumeration_error != ERROR_NO_MORE_FILES || matches != 1 || unexpected ||
        !file_sha256(path, digest) || _stricmp(digest, expected_sha256)) {
        fprintf(stderr, "REFUSED: loaded ICD identity mismatch expected=%s matches=%u unexpected=%u\n",
                path, matches, unexpected);
        return 0;
    }
    printf("ICD_IDENTITY path=%s sha256=%s verified=1\n", path, digest);
    return 1;
}
#endif

static VKAPI_ATTR VkBool32 VKAPI_CALL
debug_message(VkDebugUtilsMessageSeverityFlagBitsEXT severity,
              VkDebugUtilsMessageTypeFlagsEXT type,
              const VkDebugUtilsMessengerCallbackDataEXT *data, void *user)
{
    (void)type; (void)user;
    if (severity & VK_DEBUG_UTILS_MESSAGE_SEVERITY_ERROR_BIT_EXT) validation_errors++;
    fprintf(stderr, "VALIDATION %s\n", data->pMessage);
    return VK_FALSE;
}

static VkDeviceSize align_size(VkDeviceSize size, VkDeviceSize alignment)
{
    return (size + alignment - 1) / alignment * alignment;
}

static VkDeviceSize bytes(unsigned shape)
{
    return (VkDeviceSize)extents[shape].width * extents[shape].height * extents[shape].depth * 4;
}

static uint32_t pattern(unsigned serial, unsigned index)
{
    uint32_t value = index * 0x9e3779b9u + serial * 0x85ebca6bu;
    value ^= value >> 16;
    return value * 0xc2b2ae35u;
}

static VkResult allocate(VkDevice device, const VkPhysicalDeviceMemoryProperties *props,
                         uint32_t bits, VkMemoryPropertyFlags flags, VkDeviceSize size,
                         const void *next, VkDeviceMemory *memory)
{
    VkMemoryAllocateInfo info = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .pNext = next, .allocationSize = size};
    for (; info.memoryTypeIndex < props->memoryTypeCount; info.memoryTypeIndex++) {
        if ((bits & (1u << info.memoryTypeIndex)) &&
            (props->memoryTypes[info.memoryTypeIndex].propertyFlags & flags) == flags) {
            printf("ALLOC size=%" PRIu64 " type=%u flags=0x%x\n", (uint64_t)size,
                   info.memoryTypeIndex, props->memoryTypes[info.memoryTypeIndex].propertyFlags);
            return vkAllocateMemory(device, &info, NULL, memory);
        }
    }
    return VK_ERROR_FEATURE_NOT_PRESENT;
}

static VkResult host_buffer(VkDevice device, const VkPhysicalDeviceMemoryProperties *props,
                            VkDeviceSize size, VkBufferUsageFlags usage,
                            VkMemoryPropertyFlags properties, struct host_buffer *out)
{
    VkBufferCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = size, .usage = usage};
    VkResult result = vkCreateBuffer(device, &ci, NULL, &out->r.buffer);
    if (result != VK_SUCCESS) return result;
    VkMemoryDedicatedRequirements dedicated = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS};
    VkMemoryRequirements2 req = {.sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2, .pNext = &dedicated};
    VkBufferMemoryRequirementsInfo2 query = {.sType = VK_STRUCTURE_TYPE_BUFFER_MEMORY_REQUIREMENTS_INFO_2,
        .buffer = out->r.buffer};
    vkGetBufferMemoryRequirements2(device, &query, &req);
    out->r.req = req.memoryRequirements;
    VkMemoryDedicatedAllocateInfo owner = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        .buffer = out->r.buffer};
    result = allocate(device, props, out->r.req.memoryTypeBits, properties,
                      out->r.req.size, &owner, &out->r.memory);
    if (result != VK_SUCCESS) return result;
    result = vkBindBufferMemory(device, out->r.buffer, out->r.memory, 0);
    if (result != VK_SUCCESS) return result;
    return vkMapMemory(device, out->r.memory, 0, VK_WHOLE_SIZE, 0, (void **)&out->map);
}

/* No cleanup of an outstanding submission after timeout. Continue waiting only
 * for its own fence, report failure, and stop submitting new work. */
static VkResult drain(VkDevice device, struct slot *slot)
{
    if (!slot->pending) return VK_SUCCESS;
    VkResult result;
    while ((result = vkWaitForFences(device, 1, &slot->fence, VK_TRUE, WAIT_NS)) == VK_TIMEOUT) {
        timeout_seen = 1;
        fprintf(stderr, "FAIL timeout serial=%u; retaining all pending resources\n", slot->serial);
        fflush(stderr);
    }
    if (result == VK_SUCCESS || result == VK_ERROR_DEVICE_LOST) slot->pending = 0;
    return result;
}

static int verify(VkDevice device, struct slot *slot, VkDeviceSize size)
{
    VkMappedMemoryRange range = {.sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE,
        .memory = slot->readback.r.memory, .size = VK_WHOLE_SIZE};
    VkResult result = vkInvalidateMappedMemoryRanges(device, 1, &range);
    if (result != VK_SUCCESS) { fprintf(stderr, "FAIL invalidate result=%d\n", result); return 0; }
    for (unsigned i = 0; i < size / 4; i++) {
        uint32_t expected = pattern(slot->serial, i);
        if (slot->readback.map[i] != expected) {
            fprintf(stderr, "FAIL serial=%u word=%u expected=%08x actual=%08x\n",
                    slot->serial, i, expected, slot->readback.map[i]);
            return 0;
        }
    }
    printf("CHECK serial=%u bytes=%" PRIu64 " exact=1\n", slot->serial, (uint64_t)size);
    return 1;
}

static void destroy_resource(VkDevice device, struct resource *r, int free_memory)
{
    if (r->buffer) vkDestroyBuffer(device, r->buffer, NULL);
    if (r->image) vkDestroyImage(device, r->image, NULL);
    if (free_memory && r->memory) vkFreeMemory(device, r->memory, NULL);
}

/* The captured loading submission contains 468 288-byte copies at stride 512,
 * then 12079 144-byte copies at stride 256. Test that command density with a
 * pooled mapped source and a nonzero destination binding next to an optimal
 * image. This is a transfer control, not a replay of the benchmark. The
 * destination additionally has TRANSFER_SRC usage for independent readback. */
#define BURST_REGIONS (468u + 12079u)
#define BURST_BYTES (12u * 1024u * 1024u)
#define BURST_SLOTS 2u
#define BURST_ROUNDS 4u
#define BURST_GUARD 0xa5a5a5a5u

static int verify_burst(VkDevice device, struct slot *slot)
{
    VkMappedMemoryRange range = {.sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE,
        .memory = slot->readback.r.memory, .size = VK_WHOLE_SIZE};
    if (vkInvalidateMappedMemoryRanges(device, 1, &range) != VK_SUCCESS) return 0;
    unsigned mismatches = 0, copied_errors = 0, gap_errors = 0;
    for (unsigned i = 0; i < BURST_BYTES / 4; i++) {
        const unsigned offset = i * 4;
        int copied = offset < 468u * 512u ? offset % 512u < 288u :
            offset < 468u * 512u + 12079u * 256u &&
            (offset - 468u * 512u) % 256u < 144u;
        uint32_t expected = copied ? pattern(slot->serial, i) : BURST_GUARD;
        if (slot->readback.map[i] != expected) {
            if (mismatches < 8)
                fprintf(stderr, "FAIL burst serial=%u word=%u copied=%d expected=%08x actual=%08x upload=%08x\n",
                        slot->serial, i, copied, expected, slot->readback.map[i], slot->upload.map[i]);
            mismatches++;
            if (copied) copied_errors++; else gap_errors++;
        }
    }
    if (mismatches) {
        fprintf(stderr, "FAIL burst serial=%u mismatched_words=%u copied_errors=%u gap_errors=%u\n",
                slot->serial, mismatches, copied_errors, gap_errors);
        return 0;
    }
    printf("CHECK burst serial=%u bytes=%u exact=1 gaps=1\n", slot->serial, BURST_BYTES);
    return 1;
}

static int run_copy_burst(VkPhysicalDevice gpu, VkDevice device, VkQueue queue,
                          unsigned family, int batch_regions, int direct_readback)
{
    struct slot slots[BURST_SLOTS] = {0};
    VkBufferCopy *regions = calloc(BURST_REGIONS, sizeof(*regions));
    if (!regions) return 1;
    for (unsigned i = 0; i < BURST_REGIONS; i++) {
        VkDeviceSize offset = i < 468 ? (VkDeviceSize)i * 512 :
            468u * 512u + (VkDeviceSize)(i - 468) * 256;
        regions[i] = (VkBufferCopy){offset, offset, i < 468 ? 288 : 144};
    }
    VkPhysicalDeviceProperties props;
    VkPhysicalDeviceMemoryProperties memory;
    vkGetPhysicalDeviceProperties(gpu, &props);
    vkGetPhysicalDeviceMemoryProperties(gpu, &memory);
    unsigned completed = 0;
    int failed = 0, unsupported = 0;
    printf("CASE copy_burst calls=%u regions=%u inflight=%u rounds=%u bytes=%u direct_readback=%d\n",
           batch_regions ? 1 : BURST_REGIONS, BURST_REGIONS, BURST_SLOTS, BURST_ROUNDS, BURST_BYTES, direct_readback);
    for (unsigned s = 0; s < BURST_SLOTS; s++) {
        struct slot *slot = &slots[s];
        struct resource *dst = &slot->resources[0], *image = &slot->resources[1];
        struct resource *tail = &slot->resources[2];
        VkBufferCreateInfo buffer = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
            .size = BURST_BYTES, .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT};
        TRY(vkCreateBuffer(device, &buffer, NULL, &slot->upload.r.buffer));
        buffer.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
            VK_BUFFER_USAGE_UNIFORM_BUFFER_BIT;
        TRY(vkCreateBuffer(device, &buffer, NULL, &dst->buffer));
        buffer.size = 65536; buffer.usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT;
        TRY(vkCreateBuffer(device, &buffer, NULL, &tail->buffer));
        VkImageCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
            .imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_R8G8B8A8_UNORM,
            .extent = {128, 128, 1}, .mipLevels = 1, .arrayLayers = 1,
            .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
            .usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT};
        TRY(vkCreateImage(device, &ci, NULL, &image->image));
        struct resource *all[] = {&slot->upload.r, dst, tail, image};
        for (unsigned i = 0; i < 4; i++) {
            VkMemoryDedicatedRequirements dedicated = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS};
            VkMemoryRequirements2 req = {.sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2, .pNext = &dedicated};
            if (all[i]->buffer) {
                VkBufferMemoryRequirementsInfo2 query = {.sType = VK_STRUCTURE_TYPE_BUFFER_MEMORY_REQUIREMENTS_INFO_2,
                    .buffer = all[i]->buffer};
                vkGetBufferMemoryRequirements2(device, &query, &req);
            } else {
                VkImageMemoryRequirementsInfo2 query = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2,
                    .image = all[i]->image};
                vkGetImageMemoryRequirements2(device, &query, &req);
            }
            all[i]->req = req.memoryRequirements;
            if (dedicated.requiresDedicatedAllocation) { unsupported = 1; goto cleanup; }
        }
        tail->offset = align_size(slot->upload.r.req.size, tail->req.alignment);
        VkDeviceSize upload_size = tail->offset + tail->req.size;
        if (upload_size < 16u * 1024u * 1024u) upload_size = 16u * 1024u * 1024u;
        TRY(allocate(device, &memory, slot->upload.r.req.memoryTypeBits & tail->req.memoryTypeBits,
                     upload_properties, upload_size, NULL, &slot->upload.r.memory));
        tail->memory = slot->upload.r.memory;
        TRY(vkBindBufferMemory(device, slot->upload.r.buffer, slot->upload.r.memory, 0));
        TRY(vkBindBufferMemory(device, tail->buffer, tail->memory, tail->offset));
        TRY(vkMapMemory(device, slot->upload.r.memory, 0, VK_WHOLE_SIZE, 0, (void **)&slot->upload.map));
        dst->offset = image->req.size < 262144 ? 262144 : image->req.size;
        dst->offset = align_size(align_size(dst->offset, props.limits.bufferImageGranularity), dst->req.alignment);
        VkDeviceSize pool_size = dst->offset + dst->req.size;
        if (pool_size < 64u * 1024u * 1024u) pool_size = 64u * 1024u * 1024u;
        TRY(allocate(device, &memory, dst->req.memoryTypeBits & image->req.memoryTypeBits,
                     VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, pool_size, NULL, &slot->pool));
        dst->memory = image->memory = slot->pool;
        TRY(vkBindImageMemory(device, image->image, image->memory, 0));
        TRY(vkBindBufferMemory(device, dst->buffer, dst->memory, dst->offset));
        printf("BURST_POOL slot=%u upload=%" PRIu64 " tail_offset=%" PRIu64
               " device=%" PRIu64 " dst_offset=%" PRIu64 " image_req=%" PRIu64 "\n",
               s, (uint64_t)upload_size, (uint64_t)tail->offset, (uint64_t)pool_size,
               (uint64_t)dst->offset, (uint64_t)image->req.size);
        TRY(host_buffer(device, &memory, BURST_BYTES, VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                        readback_properties, &slot->readback));
        VkCommandPoolCreateInfo pool = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = family};
        TRY(vkCreateCommandPool(device, &pool, NULL, &slot->commands));
        VkCommandBufferAllocateInfo alloc = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            .commandPool = slot->commands, .commandBufferCount = 1, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY};
        TRY(vkAllocateCommandBuffers(device, &alloc, &slot->primary));
        VkFenceCreateInfo fence = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
        TRY(vkCreateFence(device, &fence, NULL, &slot->fence));
    }
    for (unsigned serial = 1; serial <= BURST_ROUNDS; serial++) {
        struct slot *slot = &slots[(serial - 1) % BURST_SLOTS];
        if (slot->pending) {
            TRY(drain(device, slot));
            if (!verify_burst(device, slot) || timeout_seen) { failed = 1; goto cleanup; }
            completed++;
        }
        TRY(vkResetCommandPool(device, slot->commands, 0));
        TRY(vkResetFences(device, 1, &slot->fence));
        for (unsigned i = 0; i < BURST_BYTES / 4; i++) slot->upload.map[i] = pattern(serial, i);
        VkMappedMemoryRange range = {.sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE,
            .memory = slot->upload.r.memory, .size = VK_WHOLE_SIZE};
        TRY(vkFlushMappedMemoryRanges(device, 1, &range));
        VkCommandBufferBeginInfo begin = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
        TRY(vkBeginCommandBuffer(slot->primary, &begin));
        VkCommandBuffer cmd = slot->primary;
        VkImageMemoryBarrier image = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .srcAccessMask = slot->initialized ? VK_ACCESS_TRANSFER_WRITE_BIT : 0,
            .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
            .oldLayout = slot->initialized ? VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL : VK_IMAGE_LAYOUT_UNDEFINED,
            .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .image = slot->resources[1].image, .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                             0, 0, NULL, 0, NULL, 1, &image);
        VkClearColorValue clear = {.uint32 = {0, 0, 0, 0}};
        vkCmdClearColorImage(cmd, image.image, image.newLayout, &clear, 1, &image.subresourceRange);
        VkBuffer destination = direct_readback ? slot->readback.r.buffer : slot->resources[0].buffer;
        vkCmdFillBuffer(cmd, destination, 0, BURST_BYTES, BURST_GUARD);
        VkMemoryBarrier barrier = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
            .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT, .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT};
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                             0, 1, &barrier, 0, NULL, 0, NULL);
        if (batch_regions)
            vkCmdCopyBuffer(cmd, slot->upload.r.buffer, destination, BURST_REGIONS, regions);
        else for (unsigned i = 0; i < BURST_REGIONS; i++)
            vkCmdCopyBuffer(cmd, slot->upload.r.buffer, destination, 1, &regions[i]);
        if (!direct_readback) {
            barrier.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
            vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                                 0, 1, &barrier, 0, NULL, 0, NULL);
            VkBufferCopy copy = {0, 0, BURST_BYTES};
            vkCmdCopyBuffer(cmd, slot->resources[0].buffer, slot->readback.r.buffer, 1, &copy);
        }
        barrier.dstAccessMask = VK_ACCESS_HOST_READ_BIT;
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                             0, 1, &barrier, 0, NULL, 0, NULL);
        TRY(vkEndCommandBuffer(cmd));
        VkSubmitInfo submit = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
            .commandBufferCount = 1, .pCommandBuffers = &cmd};
        slot->serial = serial; slot->pending = 1;
        TRY(vkQueueSubmit(queue, 1, &submit, slot->fence));
        slot->initialized = 1;
    }
cleanup:
    for (unsigned s = 0; s < BURST_SLOTS; s++) {
        struct slot *slot = &slots[s];
        int pending = slot->pending;
        VkResult result = drain(device, slot);
        if (result != VK_SUCCESS) failed = 1;
        else if (pending && !failed) {
            if (!verify_burst(device, slot)) failed = 1;
            else completed++;
        }
        while (slot->pending) {
            fprintf(stderr, "FAIL burst ambiguous wait=%d; retaining resources\n", result);
            fflush(stderr);
#ifdef _WIN32
            Sleep(1000);
#else
            const struct timespec delay = {1, 0};
            thrd_sleep(&delay, NULL);
#endif
            result = drain(device, slot);
        }
    }
    for (unsigned s = 0; s < BURST_SLOTS; s++) {
        struct slot *slot = &slots[s];
        if (slot->fence) vkDestroyFence(device, slot->fence, NULL);
        if (slot->commands) vkDestroyCommandPool(device, slot->commands, NULL);
        for (unsigned i = 0; i < 3; i++) destroy_resource(device, &slot->resources[i], 0);
        if (slot->pool) vkFreeMemory(device, slot->pool, NULL);
        if (slot->upload.map) vkUnmapMemory(device, slot->upload.r.memory);
        if (slot->readback.map) vkUnmapMemory(device, slot->readback.r.memory);
        destroy_resource(device, &slot->upload.r, 1);
        destroy_resource(device, &slot->readback.r, 1);
    }
    free(regions);
    int result = unsupported ? 77 : failed || timeout_seen || validation_errors || completed != BURST_ROUNDS ? 1 : 0;
    printf("RESULT copy_burst batch=%d exit=%d completed=%u bytes_checked=%" PRIu64
           " validation_errors=%u timeouts=%u\n", batch_regions, result, completed,
           (uint64_t)completed * BURST_BYTES, validation_errors, timeout_seen);
    return result;
}

static int run(VkPhysicalDevice gpu, VkDevice device, VkQueue queue, unsigned family, int mixed)
{
    struct slot slots[SLOTS] = {0};
    VkPhysicalDeviceProperties properties;
    VkPhysicalDeviceMemoryProperties memory;
    vkGetPhysicalDeviceProperties(gpu, &properties);
    vkGetPhysicalDeviceMemoryProperties(gpu, &memory);
    VkSemaphore timeline = VK_NULL_HANDLE;
    int failed = 0, unsupported = 0;
    unsigned completed = 0;
    const VkDeviceSize total = bytes(0) + bytes(1);
    VkSemaphoreTypeCreateInfo type = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO,
        .semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE};
    VkSemaphoreCreateInfo sem = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, .pNext = &type};
    printf("CASE mode=%s inflight=%u rounds=%u granularity=%" PRIu64 "\n",
           mixed ? "mixed" : "separate", SLOTS, ROUNDS, (uint64_t)properties.limits.bufferImageGranularity);
    printf("HOST_MEMORY upload_required=0x%x readback_required=0x%x\n",
           upload_properties, readback_properties);
    TRY(vkCreateSemaphore(device, &sem, NULL, &timeline));
    for (unsigned s = 0; s < SLOTS; s++) {
        struct slot *slot = &slots[s];
        uint32_t common_bits = UINT32_MAX;
        VkDeviceSize pool_size = 0;
        for (unsigned i = 0; i < SHAPES * 2; i++) {
            struct resource *r = &slot->resources[i];
            unsigned shape = i / 2;
            VkMemoryDedicatedRequirements dedicated = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS};
            VkMemoryRequirements2 req = {.sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2, .pNext = &dedicated};
            if (!(i & 1)) {
                VkBufferCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                    .size = bytes(shape), .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
                        VK_BUFFER_USAGE_TRANSFER_DST_BIT | VK_BUFFER_USAGE_STORAGE_BUFFER_BIT};
                TRY(vkCreateBuffer(device, &ci, NULL, &r->buffer));
                VkBufferMemoryRequirementsInfo2 query = {.sType = VK_STRUCTURE_TYPE_BUFFER_MEMORY_REQUIREMENTS_INFO_2,
                    .buffer = r->buffer};
                vkGetBufferMemoryRequirements2(device, &query, &req);
            } else {
                VkImageCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                    .imageType = shape ? VK_IMAGE_TYPE_3D : VK_IMAGE_TYPE_2D,
                    .format = VK_FORMAT_R8G8B8A8_UNORM, .extent = extents[shape], .mipLevels = 1,
                    .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
                    .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT};
                TRY(vkCreateImage(device, &ci, NULL, &r->image));
                VkImageMemoryRequirementsInfo2 query = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2,
                    .image = r->image};
                vkGetImageMemoryRequirements2(device, &query, &req);
            }
            r->req = req.memoryRequirements;
            if (mixed && dedicated.requiresDedicatedAllocation) {
                fprintf(stderr, "UNSUPPORTED mixed resource=%u requires dedicated memory\n", i);
                unsupported = 1; goto cleanup;
            }
            common_bits &= r->req.memoryTypeBits;
            /* Adjacent linear/optimal resources share no granularity page. All
             * sizes come from actual requirements, never the image byte count. */
            pool_size = align_size(pool_size, properties.limits.bufferImageGranularity);
            r->offset = align_size(pool_size, r->req.alignment);
            pool_size = r->offset + r->req.size;
            printf("RESOURCE slot=%u index=%u kind=%s bytes=%" PRIu64 " req=%" PRIu64
                   " align=%" PRIu64 " bits=0x%x offset=%" PRIu64 "\n", s, i,
                   r->buffer ? "buffer" : "image", (uint64_t)bytes(shape), (uint64_t)r->req.size,
                   (uint64_t)r->req.alignment, r->req.memoryTypeBits, (uint64_t)(mixed ? r->offset : 0));
        }
        if (mixed) {
            VkResult result = allocate(device, &memory, common_bits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT,
                                        pool_size, NULL, &slot->pool);
            if (result == VK_ERROR_FEATURE_NOT_PRESENT) { unsupported = 1; goto cleanup; }
            TRY(result);
        }
        for (unsigned i = 0; i < SHAPES * 2; i++) {
            struct resource *r = &slot->resources[i];
            if (mixed) r->memory = slot->pool;
            else {
                r->offset = 0;
                VkMemoryDedicatedAllocateInfo owner = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
                    .buffer = r->buffer, .image = r->image};
                TRY(allocate(device, &memory, r->req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT,
                             r->req.size, &owner, &r->memory));
            }
            if (r->buffer) TRY(vkBindBufferMemory(device, r->buffer, r->memory, r->offset));
            else TRY(vkBindImageMemory(device, r->image, r->memory, r->offset));
        }
        TRY(host_buffer(device, &memory, total, VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
                        upload_properties, &slot->upload));
        TRY(host_buffer(device, &memory, total, VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                        readback_properties, &slot->readback));
        VkCommandPoolCreateInfo pool = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = family};
        TRY(vkCreateCommandPool(device, &pool, NULL, &slot->commands));
        VkCommandBufferAllocateInfo alloc = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            .commandPool = slot->commands, .commandBufferCount = 1, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY};
        TRY(vkAllocateCommandBuffers(device, &alloc, &slot->primary));
        alloc.level = VK_COMMAND_BUFFER_LEVEL_SECONDARY;
        TRY(vkAllocateCommandBuffers(device, &alloc, &slot->secondary));
        VkFenceCreateInfo fence = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
        TRY(vkCreateFence(device, &fence, NULL, &slot->fence));
    }
    for (unsigned serial = 1; serial <= ROUNDS; serial++) {
        struct slot *slot = &slots[(serial - 1) % SLOTS];
        if (slot->pending) {
            TRY(drain(device, slot));
            if (!verify(device, slot, total) || timeout_seen) { failed = 1; goto cleanup; }
            completed++;
        }
        TRY(vkResetCommandPool(device, slot->commands, 0));
        TRY(vkResetFences(device, 1, &slot->fence));
        for (unsigned i = 0; i < total / 4; i++) slot->upload.map[i] = pattern(serial, i);
        VkMappedMemoryRange range = {.sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE,
            .memory = slot->upload.r.memory, .size = VK_WHOLE_SIZE};
        TRY(vkFlushMappedMemoryRanges(device, 1, &range));
        VkCommandBufferInheritanceInfo inheritance = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_INHERITANCE_INFO};
        VkCommandBufferBeginInfo begin = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT, .pInheritanceInfo = &inheritance};
        TRY(vkBeginCommandBuffer(slot->secondary, &begin));
        VkCommandBuffer cmd = slot->secondary;
        VkDeviceSize offset = 0;
        for (unsigned shape = 0; shape < SHAPES; shape++) {
            VkBuffer buffer = slot->resources[shape * 2].buffer;
            VkImage image = slot->resources[shape * 2 + 1].image;
            VkBufferCopy copy = {offset, 0, bytes(shape)};
            if (serial & 1) {
                VkBufferCopy2 region = {.sType = VK_STRUCTURE_TYPE_BUFFER_COPY_2,
                    .srcOffset = offset, .size = bytes(shape)};
                VkCopyBufferInfo2 ci = {.sType = VK_STRUCTURE_TYPE_COPY_BUFFER_INFO_2,
                    .srcBuffer = slot->upload.r.buffer, .dstBuffer = buffer, .regionCount = 1, .pRegions = &region};
                vkCmdCopyBuffer2(cmd, &ci);
            } else vkCmdCopyBuffer(cmd, slot->upload.r.buffer, buffer, 1, &copy);
            VkBufferMemoryBarrier buffer_barrier = {.sType = VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER,
                .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT, .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
                .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                .buffer = buffer, .size = VK_WHOLE_SIZE};
            VkImageMemoryBarrier image_barrier = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                .srcAccessMask = slot->initialized ? VK_ACCESS_TRANSFER_READ_BIT : 0,
                .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                .oldLayout = slot->initialized ? VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL : VK_IMAGE_LAYOUT_UNDEFINED,
                .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, .image = image,
                .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
            vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                                 0, 0, NULL, 1, &buffer_barrier, 1, &image_barrier);
            VkBufferImageCopy region = {.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
                .imageExtent = extents[shape]};
            vkCmdCopyBufferToImage(cmd, buffer, image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &region);
            image_barrier.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
            image_barrier.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
            image_barrier.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
            image_barrier.newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
            vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                                 0, 0, NULL, 0, NULL, 1, &image_barrier);
            region.bufferOffset = offset;
            vkCmdCopyImageToBuffer(cmd, image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, slot->readback.r.buffer, 1, &region);
            offset += bytes(shape);
        }
        VkMemoryBarrier host = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
            .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT, .dstAccessMask = VK_ACCESS_HOST_READ_BIT};
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                             0, 1, &host, 0, NULL, 0, NULL);
        TRY(vkEndCommandBuffer(cmd));
        begin.pInheritanceInfo = NULL;
        TRY(vkBeginCommandBuffer(slot->primary, &begin));
        vkCmdExecuteCommands(slot->primary, 1, &slot->secondary);
        TRY(vkEndCommandBuffer(slot->primary));
        uint64_t wait_value = serial - 1, signal_value = serial;
        /* Retain the whole slot even if submit returns an ambiguous error. */
        slot->serial = serial; slot->pending = 1;
        if (serial & 1) {
            VkCommandBufferSubmitInfo command = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_SUBMIT_INFO,
                .commandBuffer = slot->primary};
            VkSemaphoreSubmitInfo wait = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_SUBMIT_INFO,
                .semaphore = timeline, .value = wait_value, .stageMask = VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT};
            VkSemaphoreSubmitInfo signal = wait;
            signal.value = signal_value;
            VkSubmitInfo2 submit = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO_2,
                .waitSemaphoreInfoCount = 1, .pWaitSemaphoreInfos = &wait,
                .commandBufferInfoCount = 1, .pCommandBufferInfos = &command,
                .signalSemaphoreInfoCount = 1, .pSignalSemaphoreInfos = &signal};
            TRY(vkQueueSubmit2(queue, 1, &submit, slot->fence));
        } else {
            VkPipelineStageFlags stage = VK_PIPELINE_STAGE_ALL_COMMANDS_BIT;
            VkTimelineSemaphoreSubmitInfo values = {.sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO,
                .waitSemaphoreValueCount = 1, .pWaitSemaphoreValues = &wait_value,
                .signalSemaphoreValueCount = 1, .pSignalSemaphoreValues = &signal_value};
            VkSubmitInfo submit = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .pNext = &values,
                .waitSemaphoreCount = 1, .pWaitSemaphores = &timeline, .pWaitDstStageMask = &stage,
                .commandBufferCount = 1, .pCommandBuffers = &slot->primary,
                .signalSemaphoreCount = 1, .pSignalSemaphores = &timeline};
            TRY(vkQueueSubmit(queue, 1, &submit, slot->fence));
        }
        slot->initialized = 1;
    }
cleanup:
    for (unsigned s = 0; s < SLOTS; s++) {
        struct slot *slot = &slots[s];
        int pending = slot->pending;
        VkResult result = drain(device, slot);
        if (result != VK_SUCCESS) failed = 1;
        else if (pending && !failed) {
            if (!verify(device, slot, total)) failed = 1;
            else completed++;
        }
        /* Unexpected wait failures do not grant permission to free work. */
        while (slot->pending) {
            fprintf(stderr, "FAIL ambiguous wait=%d; retaining resources\n", result);
            fflush(stderr);
#ifdef _WIN32
            Sleep(1000);
#else
            const struct timespec delay = {1, 0};
            thrd_sleep(&delay, NULL);
#endif
            result = drain(device, slot);
        }
    }
    for (unsigned s = 0; s < SLOTS; s++) {
        struct slot *slot = &slots[s];
        if (slot->fence) vkDestroyFence(device, slot->fence, NULL);
        if (slot->commands) vkDestroyCommandPool(device, slot->commands, NULL);
        for (unsigned i = 0; i < SHAPES * 2; i++) destroy_resource(device, &slot->resources[i], !mixed);
        if (slot->pool) vkFreeMemory(device, slot->pool, NULL);
        if (slot->upload.map) vkUnmapMemory(device, slot->upload.r.memory);
        if (slot->readback.map) vkUnmapMemory(device, slot->readback.r.memory);
        destroy_resource(device, &slot->upload.r, 1);
        destroy_resource(device, &slot->readback.r, 1);
    }
    if (timeline) vkDestroySemaphore(device, timeline, NULL);
    int result = unsupported ? 77 : failed || timeout_seen || validation_errors || completed != ROUNDS ? 1 : 0;
    printf("RESULT mode=%s exit=%d completed=%u bytes_checked=%" PRIu64 " validation_errors=%u timeouts=%u\n",
           mixed ? "mixed" : "separate", result, completed, (uint64_t)(completed * total), validation_errors, timeout_seen);
    return result;
}

int main(int argc, char **argv)
{
    int validate = 0, first = 0, last = 1, copy_burst = 0, batch_regions = 0, direct_readback = 0;
    int init_only = 0;
    const char *expected_icd = NULL, *expected_icd_sha256 = NULL;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--validate")) validate = 1;
        else if (!strcmp(argv[i], "--mixed")) first = last = 1;
        else if (!strcmp(argv[i], "--separate")) first = last = 0;
        else if (!strcmp(argv[i], "--upload-device-local"))
            upload_properties |= VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
        else if (!strcmp(argv[i], "--readback-cached"))
            readback_properties |= VK_MEMORY_PROPERTY_HOST_CACHED_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
        else if (!strcmp(argv[i], "--copy-burst")) copy_burst = 1;
        else if (!strcmp(argv[i], "--batch-regions")) batch_regions = 1;
        else if (!strcmp(argv[i], "--direct-readback")) direct_readback = 1;
        else if (!strcmp(argv[i], "--init-only")) init_only = 1;
        else if (!strcmp(argv[i], "--expect-icd") && i + 1 < argc) expected_icd = argv[++i];
        else if (!strcmp(argv[i], "--expect-icd-sha256") && i + 1 < argc) expected_icd_sha256 = argv[++i];
        else { fprintf(stderr, "Unknown option: %s\n", argv[i]); return 77; }
    }
    if (!copy_burst && (batch_regions || direct_readback)) {
        fprintf(stderr, "Copy-burst options require --copy-burst\n"); return 77;
    }
    if (!!expected_icd != !!expected_icd_sha256 ||
        (expected_icd_sha256 && (strlen(expected_icd_sha256) != 64 ||
         strspn(expected_icd_sha256, "0123456789abcdefABCDEF") != 64))) {
        fprintf(stderr, "Expected ICD requires a path and a 64-digit SHA256\n"); return 77;
    }
    setvbuf(stdout, NULL, _IONBF, 0);
#ifdef _WIN32
    if (!windows_session_allowed()) return 77;
#else
    if (expected_icd) { fprintf(stderr, "ICD identity pinning is a Windows probe option\n"); return 77; }
#endif
    int failed = 0, result = 77;
    VkInstance instance = VK_NULL_HANDLE;
    VkDevice device = VK_NULL_HANDLE;
    VkDebugUtilsMessengerEXT messenger = VK_NULL_HANDLE;
    PFN_vkDestroyDebugUtilsMessengerEXT destroy_debug = NULL;
    const char *layer = "VK_LAYER_KHRONOS_validation", *extension = VK_EXT_DEBUG_UTILS_EXTENSION_NAME;
    VkDebugUtilsMessengerCreateInfoEXT debug = {.sType = VK_STRUCTURE_TYPE_DEBUG_UTILS_MESSENGER_CREATE_INFO_EXT,
        .messageSeverity = VK_DEBUG_UTILS_MESSAGE_SEVERITY_WARNING_BIT_EXT | VK_DEBUG_UTILS_MESSAGE_SEVERITY_ERROR_BIT_EXT,
        .messageType = VK_DEBUG_UTILS_MESSAGE_TYPE_GENERAL_BIT_EXT | VK_DEBUG_UTILS_MESSAGE_TYPE_VALIDATION_BIT_EXT |
                       VK_DEBUG_UTILS_MESSAGE_TYPE_PERFORMANCE_BIT_EXT, .pfnUserCallback = debug_message};
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "transfer-pool-control",
        .apiVersion = VK_API_VERSION_1_3};
    VkInstanceCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app,
        .pNext = validate ? &debug : NULL, .enabledLayerCount = validate ? 1 : 0, .ppEnabledLayerNames = &layer,
        .enabledExtensionCount = validate ? 1 : 0, .ppEnabledExtensionNames = &extension};
    TRY(vkCreateInstance(&ci, NULL, &instance));
    if (validate) {
        PFN_vkCreateDebugUtilsMessengerEXT create_debug = (PFN_vkCreateDebugUtilsMessengerEXT)vkGetInstanceProcAddr(instance, "vkCreateDebugUtilsMessengerEXT");
        destroy_debug = (PFN_vkDestroyDebugUtilsMessengerEXT)vkGetInstanceProcAddr(instance, "vkDestroyDebugUtilsMessengerEXT");
        if (!create_debug || !destroy_debug) goto cleanup;
        TRY(create_debug(instance, &debug, NULL, &messenger));
    }
    uint32_t count = 1;
    VkPhysicalDevice gpu;
    TRY(vkEnumeratePhysicalDevices(instance, &count, &gpu));
    if (!count) goto cleanup;
#ifdef _WIN32
    if (expected_icd && !verify_loaded_icd(expected_icd, expected_icd_sha256)) goto cleanup;
#endif
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(gpu, &props);
    printf("DEVICE %s vendor=%04x device=%04x api=%u driver=%u\n", props.deviceName,
           props.vendorID, props.deviceID, props.apiVersion, props.driverVersion);
    if (init_only) {
        result = validation_errors ? 1 : 0;
        printf("RESULT init_only exit=%d device_created=0 submissions=0 validation_errors=%u\n",
               result, validation_errors);
        goto cleanup;
    }
    VkPhysicalDeviceVulkan13Features f13 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES};
    VkPhysicalDeviceVulkan12Features f12 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES, .pNext = &f13};
    VkPhysicalDeviceFeatures2 features = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, .pNext = &f12};
    vkGetPhysicalDeviceFeatures2(gpu, &features);
    if (!f12.timelineSemaphore || !f13.synchronization2 || props.apiVersion < VK_API_VERSION_1_3) goto cleanup;
    memset(&f12, 0, sizeof(f12)); memset(&f13, 0, sizeof(f13));
    f12.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES;
    f12.pNext = &f13; f12.timelineSemaphore = VK_TRUE;
    f13.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES; f13.synchronization2 = VK_TRUE;
    VkQueueFamilyProperties families[32]; count = 32;
    vkGetPhysicalDeviceQueueFamilyProperties(gpu, &count, families);
    unsigned family = 0;
    for (; family < count; family++) if (families[family].queueCount && (families[family].queueFlags & VK_QUEUE_GRAPHICS_BIT)) break;
    if (family == count) goto cleanup;
    const float priority = 1.0f;
    VkDeviceQueueCreateInfo queue_info = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = family, .queueCount = 1, .pQueuePriorities = &priority};
    VkDeviceCreateInfo device_info = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f12,
        .queueCreateInfoCount = 1, .pQueueCreateInfos = &queue_info};
    TRY(vkCreateDevice(gpu, &device_info, NULL, &device));
    VkQueue queue; vkGetDeviceQueue(device, family, 0, &queue);
    result = 0;
    if (copy_burst) result = run_copy_burst(gpu, device, queue, family, batch_regions, direct_readback);
    for (int mixed = first; !copy_burst && mixed <= last; mixed++) {
        result = run(gpu, device, queue, family, mixed);
        if (result) break;
    }
cleanup:
    if (device) vkDestroyDevice(device, NULL);
    if (messenger) destroy_debug(instance, messenger, NULL);
    if (instance) vkDestroyInstance(instance, NULL);
    return failed ? 1 : result;
}
