// vk_fence_wake_probe.c — measure host NVIDIA fence-completion wake latency
// for an EMPTY vkQueueSubmit on an idle vs hot queue. Isolates the vkr
// sync-thread leg (WaitForFences on the empty marker submit) of the Helios
// guest's 10-20ms wire-fence retirement mystery.
//
// Build: gcc -O2 -o vk_fence_wake_probe vk_fence_wake_probe.c -ldl
// Minimal hand-declared Vulkan ABI (no headers on this host).
#include <dlfcn.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

typedef uint32_t u32; typedef uint64_t u64; typedef int32_t VkResult;
typedef void* VkInstance; typedef void* VkPhysicalDevice; typedef void* VkDevice;
typedef void* VkQueue; typedef u64 VkFence;

#define VK_STRUCTURE_TYPE_APPLICATION_INFO 0
#define VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO 1
#define VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO 2
#define VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO 3
#define VK_STRUCTURE_TYPE_SUBMIT_INFO 4
#define VK_STRUCTURE_TYPE_FENCE_CREATE_INFO 8

typedef struct { u32 sType; const void* pNext; const char* pApplicationName; u32 applicationVersion; const char* pEngineName; u32 engineVersion; u32 apiVersion; } VkApplicationInfo;
typedef struct { u32 sType; const void* pNext; u32 flags; const VkApplicationInfo* pApplicationInfo; u32 enabledLayerCount; const char* const* ppEnabledLayerNames; u32 enabledExtensionCount; const char* const* ppEnabledExtensionNames; } VkInstanceCreateInfo;
typedef struct { u32 sType; const void* pNext; u32 flags; u32 queueFamilyIndex; u32 queueCount; const float* pQueuePriorities; } VkDeviceQueueCreateInfo;
typedef struct { u32 sType; const void* pNext; u32 flags; u32 queueCreateInfoCount; const VkDeviceQueueCreateInfo* pQueueCreateInfos; u32 enabledLayerCount; const char* const* ppEnabledLayerNames; u32 enabledExtensionCount; const char* const* ppEnabledExtensionNames; const void* pEnabledFeatures; } VkDeviceCreateInfo;
typedef struct { u32 sType; const void* pNext; u32 flags; } VkFenceCreateInfo;
typedef struct { u32 sType; const void* pNext; u32 waitSemaphoreCount; const void* pWaitSemaphores; const void* pWaitDstStageMask; u32 commandBufferCount; const void* pCommandBuffers; u32 signalSemaphoreCount; const void* pSignalSemaphores; } VkSubmitInfo;
typedef struct { u32 apiVersion; u32 driverVersion; u32 vendorID; u32 deviceID; u32 deviceType; char deviceName[256]; uint8_t rest[4096]; } VkPhysicalDeviceProperties;

typedef VkResult (*PFN_vkCreateInstance)(const VkInstanceCreateInfo*, const void*, VkInstance*);
typedef VkResult (*PFN_vkEnumeratePhysicalDevices)(VkInstance, u32*, VkPhysicalDevice*);
typedef void (*PFN_vkGetPhysicalDeviceProperties)(VkPhysicalDevice, VkPhysicalDeviceProperties*);
typedef VkResult (*PFN_vkCreateDevice)(VkPhysicalDevice, const VkDeviceCreateInfo*, const void*, VkDevice*);
typedef void (*PFN_vkGetDeviceQueue)(VkDevice, u32, u32, VkQueue*);
typedef VkResult (*PFN_vkCreateFence)(VkDevice, const VkFenceCreateInfo*, const void*, VkFence*);
typedef VkResult (*PFN_vkResetFences)(VkDevice, u32, const VkFence*);
typedef VkResult (*PFN_vkQueueSubmit)(VkQueue, u32, const VkSubmitInfo*, VkFence);
typedef VkResult (*PFN_vkWaitForFences)(VkDevice, u32, const VkFence*, u32, u64);

static double now_ms(void) {
    struct timespec ts; clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

int main(int argc, char** argv) {
    void* lib = dlopen("libvulkan.so.1", RTLD_NOW);
    if (!lib) { printf("no libvulkan: %s\n", dlerror()); return 1; }
    PFN_vkCreateInstance pCreateInstance = (PFN_vkCreateInstance)dlsym(lib, "vkCreateInstance");
    PFN_vkEnumeratePhysicalDevices pEnum = (PFN_vkEnumeratePhysicalDevices)dlsym(lib, "vkEnumeratePhysicalDevices");
    PFN_vkGetPhysicalDeviceProperties pProps = (PFN_vkGetPhysicalDeviceProperties)dlsym(lib, "vkGetPhysicalDeviceProperties");
    PFN_vkCreateDevice pCreateDevice = (PFN_vkCreateDevice)dlsym(lib, "vkCreateDevice");
    PFN_vkGetDeviceQueue pGetQueue = (PFN_vkGetDeviceQueue)dlsym(lib, "vkGetDeviceQueue");
    PFN_vkCreateFence pCreateFence = (PFN_vkCreateFence)dlsym(lib, "vkCreateFence");
    PFN_vkResetFences pResetFences = (PFN_vkResetFences)dlsym(lib, "vkResetFences");
    PFN_vkQueueSubmit pQueueSubmit = (PFN_vkQueueSubmit)dlsym(lib, "vkQueueSubmit");
    PFN_vkWaitForFences pWaitForFences = (PFN_vkWaitForFences)dlsym(lib, "vkWaitForFences");
    if (!pCreateInstance || !pEnum || !pCreateDevice || !pGetQueue || !pCreateFence || !pQueueSubmit || !pWaitForFences || !pResetFences) {
        printf("missing vk entrypoints\n"); return 1;
    }

    VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO, 0, "fence-wake-probe", 1, "none", 1, (1u<<22)|(1u<<12) /*1.1.0*/ };
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, 0, 0, &app, 0, 0, 0, 0 };
    VkInstance inst = 0;
    VkResult r = pCreateInstance(&ici, 0, &inst);
    if (r != 0) { printf("vkCreateInstance=%d\n", r); return 1; }

    u32 n = 0; pEnum(inst, &n, 0);
    if (!n) { printf("no phys devices\n"); return 1; }
    VkPhysicalDevice devs[8]; if (n > 8) n = 8; pEnum(inst, &n, devs);
    int pick = 0;
    for (u32 i = 0; i < n; i++) {
        VkPhysicalDeviceProperties pr; memset(&pr, 0, sizeof pr);
        pProps(devs[i], &pr);
        printf("phys[%u]: vendor=0x%04x name=%s\n", i, pr.vendorID, pr.deviceName);
        if (pr.vendorID == 0x10de) pick = (int)i;
    }
    printf("using phys[%d]\n", pick);

    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, 0, 0, 0 /*family 0 = graphics*/, 1, &prio };
    VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, 0, 0, 1, &qci, 0, 0, 0, 0, 0 };
    VkDevice dev = 0;
    r = pCreateDevice(devs[pick], &dci, 0, &dev);
    if (r != 0) { printf("vkCreateDevice=%d\n", r); return 1; }
    VkQueue q = 0; pGetQueue(dev, 0, 0, &q);
    VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, 0, 0 };
    VkFence fence = 0; pCreateFence(dev, &fci, 0, &fence);

    VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, 0, 0, 0, 0, 0, 0, 0, 0 };
    int idle_gap_ms = argc > 1 ? atoi(argv[1]) : 100;
    int iters = argc > 2 ? atoi(argv[2]) : 30;

    printf("== mode: %dms gaps, %d iters (idle-queue wake latency) ==\n", idle_gap_ms, iters);
    double worst = 0, total = 0;
    for (int i = 0; i < iters; i++) {
        if (idle_gap_ms) usleep(idle_gap_ms * 1000);
        double t0 = now_ms();
        r = pQueueSubmit(q, 0, 0, fence);
        if (r != 0) { printf("submit=%d\n", r); return 1; }
        r = pWaitForFences(dev, 1, &fence, 1, ~0ull);
        double dt = now_ms() - t0;
        if (r != 0) { printf("wait=%d\n", r); return 1; }
        pResetFences(dev, 1, &fence);
        total += dt; if (dt > worst) worst = dt;
        printf("iter %2d: submit+wait = %.3f ms\n", i, dt);
    }
    printf("avg=%.3f ms worst=%.3f ms\n", total / iters, worst);
    return 0;
}
