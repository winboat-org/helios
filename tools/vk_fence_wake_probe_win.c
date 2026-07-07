// vk_fence_wake_probe_win.c — GUEST-side venus fence-wake probe: time an
// EMPTY vkQueueSubmit + vkWaitForFences through the Helios/venus stack with
// ZERO WSI involvement (no swapchain, no vsync, nothing in the batch).
// Discriminates "the wire-fence/transport path is slow" from "our WSI
// batches carry legitimate vsync-paced GPU waits" (29th-session question).
//
// vn_WaitForFences observes completion via ring feedback (not the wire
// fence), so pair this run's numbers with the renderer perf summary's
// retire_lat (HELIOS_PERF=1) to split host-completion vs wire-delivery.
//
// Build (win11, WinLibs gcc):
//   gcc -O2 -o Z:\tmp\vk_fence_wake_probe_win.exe Z:\tools\vk_fence_wake_probe_win.c
// Run (session 1, LIMITED — elevated processes ignore VK_DRIVER_FILES):
//   vk_fence_wake_probe_win.exe [gap_ms] [iters]
#include <windows.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

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

typedef VkResult (__stdcall *PFN_vkCreateInstance)(const VkInstanceCreateInfo*, const void*, VkInstance*);
typedef VkResult (__stdcall *PFN_vkEnumeratePhysicalDevices)(VkInstance, u32*, VkPhysicalDevice*);
typedef void (__stdcall *PFN_vkGetPhysicalDeviceProperties)(VkPhysicalDevice, VkPhysicalDeviceProperties*);
typedef VkResult (__stdcall *PFN_vkCreateDevice)(VkPhysicalDevice, const VkDeviceCreateInfo*, const void*, VkDevice*);
typedef void (__stdcall *PFN_vkGetDeviceQueue)(VkDevice, u32, u32, VkQueue*);
typedef VkResult (__stdcall *PFN_vkCreateFence)(VkDevice, const VkFenceCreateInfo*, const void*, VkFence*);
typedef VkResult (__stdcall *PFN_vkResetFences)(VkDevice, u32, const VkFence*);
typedef VkResult (__stdcall *PFN_vkQueueSubmit)(VkQueue, u32, const VkSubmitInfo*, VkFence);
typedef VkResult (__stdcall *PFN_vkWaitForFences)(VkDevice, u32, const VkFence*, u32, u64);

static LARGE_INTEGER g_freq;
static double now_ms(void) {
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    return (double)t.QuadPart * 1000.0 / (double)g_freq.QuadPart;
}

int main(int argc, char** argv) {
    QueryPerformanceFrequency(&g_freq);
    HMODULE lib = LoadLibraryA("vulkan-1.dll");
    if (!lib) { printf("no vulkan-1.dll\n"); return 1; }
#define GET(name) PFN_##name p_##name = (PFN_##name)GetProcAddress(lib, #name); \
    if (!p_##name) { printf("missing %s\n", #name); return 1; }
    GET(vkCreateInstance) GET(vkEnumeratePhysicalDevices) GET(vkGetPhysicalDeviceProperties)
    GET(vkCreateDevice) GET(vkGetDeviceQueue) GET(vkCreateFence) GET(vkResetFences)
    GET(vkQueueSubmit) GET(vkWaitForFences)
#undef GET

    VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO, 0, "fence-wake-probe", 1, "none", 1, (1u<<22)|(1u<<12) };
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, 0, 0, &app, 0, 0, 0, 0 };
    VkInstance inst = 0;
    VkResult r = p_vkCreateInstance(&ici, 0, &inst);
    if (r != 0) { printf("vkCreateInstance=%d\n", r); return 1; }

    u32 n = 0; p_vkEnumeratePhysicalDevices(inst, &n, 0);
    if (!n) { printf("no phys devices\n"); return 1; }
    VkPhysicalDevice devs[8]; if (n > 8) n = 8; p_vkEnumeratePhysicalDevices(inst, &n, devs);
    int pick = -1;
    for (u32 i = 0; i < n; i++) {
        VkPhysicalDeviceProperties pr; memset(&pr, 0, sizeof pr);
        p_vkGetPhysicalDeviceProperties(devs[i], &pr);
        printf("phys[%u]: vendor=0x%04x name=%s\n", i, pr.vendorID, pr.deviceName);
        if (pick < 0 && strstr(pr.deviceName, "Venus")) pick = (int)i;
    }
    if (pick < 0) { printf("no Venus device — wrong ICD?\n"); return 1; }
    printf("using phys[%d]\n", pick);

    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, 0, 0, 0, 1, &prio };
    VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, 0, 0, 1, &qci, 0, 0, 0, 0, 0 };
    VkDevice dev = 0;
    r = p_vkCreateDevice(devs[pick], &dci, 0, &dev);
    if (r != 0) { printf("vkCreateDevice=%d\n", r); return 1; }
    VkQueue q = 0; p_vkGetDeviceQueue(dev, 0, 0, &q);
    VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, 0, 0 };
    VkFence fence = 0; p_vkCreateFence(dev, &fci, 0, &fence);

    VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, 0, 0, 0, 0, 0, 0, 0, 0 };
    int gap_ms = argc > 1 ? atoi(argv[1]) : 30;
    int iters = argc > 2 ? atoi(argv[2]) : 100;

    printf("== empty submit+wait, %dms gaps, %d iters ==\n", gap_ms, iters);
    double worst = 0, total = 0;
    u32 hist[6] = {0}; /* <1,1-3,3-6,6-10,10-20,20+ ms */
    for (int i = 0; i < iters; i++) {
        if (gap_ms) Sleep(gap_ms);
        double t0 = now_ms();
        r = p_vkQueueSubmit(q, 0, 0, fence);
        if (r != 0) { printf("submit=%d\n", r); return 1; }
        r = p_vkWaitForFences(dev, 1, &fence, 1, ~0ull);
        double dt = now_ms() - t0;
        if (r != 0) { printf("wait=%d\n", r); return 1; }
        p_vkResetFences(dev, 1, &fence);
        total += dt; if (dt > worst) worst = dt;
        hist[dt < 1 ? 0 : dt < 3 ? 1 : dt < 6 ? 2 : dt < 10 ? 3 : dt < 20 ? 4 : 5]++;
        if (i < 10 || dt > 5.0)
            printf("iter %3d: %.3f ms\n", i, dt);
    }
    printf("avg=%.3f ms worst=%.3f ms hist_ms[<1,1-3,3-6,6-10,10-20,20+]=%u/%u/%u/%u/%u/%u\n",
           total / iters, worst, hist[0], hist[1], hist[2], hist[3], hist[4], hist[5]);
    return 0;
}
