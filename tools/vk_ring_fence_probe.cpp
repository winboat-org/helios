// vk_ring_fence_probe.cpp — WS1 #4 evidence probe: does a win32-external-
// semaphore signal ride a ring_idx>=1 wire fence that retires at HOST GPU
// COMPLETION (virglrenderer vkr queue sync thread), not at decode?
//
// Sequence (all on the Helios/venus ICD):
//   1. Calibrate a GPU workload (vkCmdFillBuffer xN on a device-local buffer)
//      to a few hundred ms, timed via a plain VkFence wait (= GPU completion
//      via the feedback path) -> T_gpu.
//   2. Create an exportable TIMELINE semaphore sem1 (OPAQUE_WIN32 NT), export
//      its handle, re-import it as sem2 (payload VN_SYNC_TYPE_IMPORTED_WIN32_SYNC
//      -> waits go through helios_wait -> shared WDDM monitored fence: the
//      exact consumer path LGIdd will use).
//   3. t0: submit the workload signaling sem1=1. vn submits the signal as a
//      fence-only ring_idx>=1 SUBMIT_VENUS batch (vkWaitRingSeqnoMESA cs) and
//      the ICD retire thread signals the WDDM fence when the wire fence
//      retires.
//   4. EARLY probe: wait sem2>=1 with a SHORT timeout right after submit.
//      Expected: VK_TIMEOUT (the GPU work is still running). An instant
//      VK_SUCCESS = early signal (decode-time retirement or blind-signal) =
//      the bug this whole workstream kills.
//   5. FULL wait: wait sem2>=1 with a long timeout; elapsed-since-t0 must be
//      ~= T_gpu. That proves the full chain: INFO_RING_IDX honored by
//      QEMU/virglrenderer -> per-ring GPU-completion fence -> KMD in-flight
//      table -> WAIT_FENCE -> retire thread -> monitored fence -> consumer.
//
// Exit code 0 = chain proven; 1 = setup failure; 2 = semantics failure.
//
// Build (VM):
//   clang-cl /nologo /MD /O2 /I "C:\VulkanSDK\<ver>\Include" Z:\tools\vk_ring_fence_probe.cpp \
//     /Fe:C:\Users\Rupansh\helios-probe\vk_ring_fence_probe.exe /link /LIBPATH:"C:\VulkanSDK\<ver>\Lib" vulkan-1.lib

#define VK_USE_PLATFORM_WIN32_KHR
#include <vulkan/vulkan.h>
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static double qpc_ms(LARGE_INTEGER a, LARGE_INTEGER b, LARGE_INTEGER f)
{
  return (double)(b.QuadPart - a.QuadPart) * 1000.0 / (double)f.QuadPart;
}

#define CHECK(expr)                                                          \
  do {                                                                       \
    VkResult _r = (expr);                                                    \
    if (_r != VK_SUCCESS) {                                                  \
      printf("FAIL %s = %d at line %d\n", #expr, (int)_r, __LINE__);         \
      return 1;                                                              \
    }                                                                        \
  } while (0)

int main()
{
  LARGE_INTEGER freq;
  QueryPerformanceFrequency(&freq);

  VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO };
  app.pApplicationName = "vk_ring_fence_probe";
  app.apiVersion = VK_API_VERSION_1_2;
  VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO };
  ici.pApplicationInfo = &app;
  VkInstance inst;
  CHECK(vkCreateInstance(&ici, NULL, &inst));

  uint32_t ndev = 0;
  CHECK(vkEnumeratePhysicalDevices(inst, &ndev, NULL));
  if (!ndev) { printf("FAIL no physical devices\n"); return 1; }
  VkPhysicalDevice devs[8];
  if (ndev > 8) ndev = 8;
  CHECK(vkEnumeratePhysicalDevices(inst, &ndev, devs));

  // Pick the venus device (driverID MESA_VENUS), else device 0.
  VkPhysicalDevice pd = devs[0];
  for (uint32_t i = 0; i < ndev; i++) {
    VkPhysicalDeviceDriverProperties drv = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES };
    VkPhysicalDeviceProperties2 p2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, &drv };
    vkGetPhysicalDeviceProperties2(devs[i], &p2);
    printf("device[%u] %s driver=%s (id %d)\n", i, p2.properties.deviceName,
           drv.driverName, (int)drv.driverID);
    if (drv.driverID == VK_DRIVER_ID_MESA_VENUS) pd = devs[i];
  }

  // External-semaphore capability check (timeline + OPAQUE_WIN32).
  {
    VkSemaphoreTypeCreateInfo tci = { VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO };
    tci.semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE;
    VkPhysicalDeviceExternalSemaphoreInfo esi = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_SEMAPHORE_INFO, &tci };
    esi.handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT;
    VkExternalSemaphoreProperties esp = { VK_STRUCTURE_TYPE_EXTERNAL_SEMAPHORE_PROPERTIES };
    vkGetPhysicalDeviceExternalSemaphoreProperties(pd, &esi, &esp);
    printf("timeline OPAQUE_WIN32 features=0x%x\n", esp.externalSemaphoreFeatures);
    if (!(esp.externalSemaphoreFeatures & VK_EXTERNAL_SEMAPHORE_FEATURE_EXPORTABLE_BIT)) {
      printf("FAIL timeline semaphore not exportable\n");
      return 1;
    }
  }

  uint32_t nqf = 0;
  vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, NULL);
  VkQueueFamilyProperties qfp[16];
  if (nqf > 16) nqf = 16;
  vkGetPhysicalDeviceQueueFamilyProperties(pd, &nqf, qfp);
  uint32_t qfam = ~0u;
  for (uint32_t i = 0; i < nqf; i++) {
    if (qfp[i].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT)) { qfam = i; break; }
  }
  if (qfam == ~0u) { printf("FAIL no gfx/compute queue family\n"); return 1; }

  const float prio = 1.0f;
  VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO };
  qci.queueFamilyIndex = qfam;
  qci.queueCount = 1;
  qci.pQueuePriorities = &prio;
  const char *dev_exts[] = {
    VK_KHR_EXTERNAL_SEMAPHORE_WIN32_EXTENSION_NAME,
  };
  VkPhysicalDeviceVulkan12Features f12 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES };
  f12.timelineSemaphore = VK_TRUE;
  VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, &f12 };
  dci.queueCreateInfoCount = 1;
  dci.pQueueCreateInfos = &qci;
  dci.enabledExtensionCount = 1;
  dci.ppEnabledExtensionNames = dev_exts;
  VkDevice dev;
  CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
  VkQueue queue;
  vkGetDeviceQueue(dev, qfam, 0, &queue);

  PFN_vkGetSemaphoreWin32HandleKHR pGetSemWin32 =
    (PFN_vkGetSemaphoreWin32HandleKHR)vkGetDeviceProcAddr(dev, "vkGetSemaphoreWin32HandleKHR");
  PFN_vkImportSemaphoreWin32HandleKHR pImportSemWin32 =
    (PFN_vkImportSemaphoreWin32HandleKHR)vkGetDeviceProcAddr(dev, "vkImportSemaphoreWin32HandleKHR");
  if (!pGetSemWin32 || !pImportSemWin32) {
    printf("FAIL win32 semaphore entrypoints missing\n");
    return 1;
  }

  // Workload buffer: device-local, transfer-dst.
  const VkDeviceSize BUF_SIZE = 128ull * 1024 * 1024;
  VkBufferCreateInfo bci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO };
  bci.size = BUF_SIZE;
  bci.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT;
  VkBuffer buf;
  CHECK(vkCreateBuffer(dev, &bci, NULL, &buf));
  VkMemoryRequirements mr;
  vkGetBufferMemoryRequirements(dev, buf, &mr);
  VkPhysicalDeviceMemoryProperties mp;
  vkGetPhysicalDeviceMemoryProperties(pd, &mp);
  uint32_t mt = ~0u;
  for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
    if ((mr.memoryTypeBits & (1u << i)) &&
        (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) { mt = i; break; }
  }
  if (mt == ~0u) { printf("FAIL no device-local memory type\n"); return 1; }
  VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO };
  mai.allocationSize = mr.size;
  mai.memoryTypeIndex = mt;
  VkDeviceMemory mem;
  CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
  CHECK(vkBindBufferMemory(dev, buf, mem, 0));

  VkCommandPoolCreateInfo cpci = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO };
  cpci.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
  cpci.queueFamilyIndex = qfam;
  VkCommandPool pool;
  CHECK(vkCreateCommandPool(dev, &cpci, NULL, &pool));
  VkCommandBufferAllocateInfo cbai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO };
  cbai.commandPool = pool;
  cbai.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
  cbai.commandBufferCount = 1;
  VkCommandBuffer cb;
  CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb));

  VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
  VkFence fence;
  CHECK(vkCreateFence(dev, &fci, NULL, &fence));

  // record `fills` x fill(whole buffer) with a barrier between each so they
  // serialize (a fill flood without barriers may overlap and finish early).
  VkMemoryBarrier mb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER };
  mb.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
  mb.dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
#define RECORD(fills)                                                        \
  do {                                                                       \
    VkCommandBufferBeginInfo bi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO }; \
    CHECK(vkResetCommandBuffer(cb, 0));                                      \
    CHECK(vkBeginCommandBuffer(cb, &bi));                                    \
    for (uint32_t i = 0; i < (fills); i++) {                                 \
      vkCmdFillBuffer(cb, buf, 0, BUF_SIZE, 0xA5A5A500u + i);                \
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT,               \
                           VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 1, &mb, 0,     \
                           NULL, 0, NULL);                                   \
    }                                                                        \
    CHECK(vkEndCommandBuffer(cb));                                           \
  } while (0)

  // ── 1. calibrate ──────────────────────────────────────────────────────────
  uint32_t fills = 32;
  double t_gpu_ms = 0;
  for (int attempt = 0; attempt < 6; attempt++) {
    RECORD(fills);
    VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO };
    si.commandBufferCount = 1;
    si.pCommandBuffers = &cb;
    LARGE_INTEGER a, b;
    QueryPerformanceCounter(&a);
    CHECK(vkQueueSubmit(queue, 1, &si, fence));
    CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 30ull * 1000000000ull));
    QueryPerformanceCounter(&b);
    CHECK(vkResetFences(dev, 1, &fence));
    t_gpu_ms = qpc_ms(a, b, freq);
    printf("calibrate fills=%u -> %.1f ms\n", fills, t_gpu_ms);
    if (t_gpu_ms >= 180.0 && t_gpu_ms <= 600.0) break;
    double scale = 300.0 / (t_gpu_ms > 1.0 ? t_gpu_ms : 1.0);
    if (scale > 32.0) scale = 32.0;
    uint32_t next = (uint32_t)((double)fills * scale);
    if (next < 1) next = 1;
    if (next == fills) break;
    fills = next;
  }
  if (t_gpu_ms < 60.0) {
    printf("FAIL could not calibrate a >=60ms workload (got %.1f ms) — timing would be noise\n", t_gpu_ms);
    return 1;
  }

  // ── 2. exportable timeline sem1, export, import as sem2 ──────────────────
  VkSemaphoreTypeCreateInfo tci = { VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO };
  tci.semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE;
  tci.initialValue = 0;
  VkExportSemaphoreCreateInfo esci = { VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO, &tci };
  esci.handleTypes = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT;
  VkSemaphoreCreateInfo sci = { VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, &esci };
  VkSemaphore sem1;
  CHECK(vkCreateSemaphore(dev, &sci, NULL, &sem1));

  VkSemaphoreGetWin32HandleInfoKHR ghi = { VK_STRUCTURE_TYPE_SEMAPHORE_GET_WIN32_HANDLE_INFO_KHR };
  ghi.semaphore = sem1;
  ghi.handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT;
  HANDLE h = NULL;
  CHECK(pGetSemWin32(dev, &ghi, &h));
  printf("exported sem1 NT handle %p\n", h);

  VkSemaphoreTypeCreateInfo tci2 = tci;
  VkSemaphoreCreateInfo sci2 = { VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, &tci2 };
  VkSemaphore sem2;
  CHECK(vkCreateSemaphore(dev, &sci2, NULL, &sem2));
  VkImportSemaphoreWin32HandleInfoKHR ihi = { VK_STRUCTURE_TYPE_IMPORT_SEMAPHORE_WIN32_HANDLE_INFO_KHR };
  ihi.semaphore = sem2;
  ihi.handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT;
  ihi.handle = h; // ownership passes to the driver
  CHECK(pImportSemWin32(dev, &ihi));
  printf("imported as sem2 (consumer path: helios_wait -> WDDM monitored fence)\n");

  // ── 3. submit workload signaling sem1=1 ───────────────────────────────────
  RECORD(fills);
  uint64_t signal_val = 1;
  VkTimelineSemaphoreSubmitInfo tssi = { VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO };
  tssi.signalSemaphoreValueCount = 1;
  tssi.pSignalSemaphoreValues = &signal_val;
  VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, &tssi };
  si.commandBufferCount = 1;
  si.pCommandBuffers = &cb;
  si.signalSemaphoreCount = 1;
  si.pSignalSemaphores = &sem1;
  LARGE_INTEGER t0, t1, t2, t3;
  QueryPerformanceCounter(&t0);
  CHECK(vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
  QueryPerformanceCounter(&t1);
  printf("submit returned in %.2f ms (workload ~%.0f ms in flight)\n",
         qpc_ms(t0, t1, freq), t_gpu_ms);

  // ── 4. early consumer probe: MUST time out while the GPU is busy ─────────
  uint64_t wait_val = 1;
  VkSemaphoreWaitInfo swi = { VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO };
  swi.semaphoreCount = 1;
  swi.pSemaphores = &sem2;
  swi.pValues = &wait_val;
  const uint64_t early_ns = 20ull * 1000 * 1000; // 20 ms << T_gpu
  VkResult early = vkWaitSemaphores(dev, &swi, early_ns);
  QueryPerformanceCounter(&t2);
  printf("early consumer wait (20ms budget): %s after %.1f ms\n",
         early == VK_TIMEOUT ? "VK_TIMEOUT (correct — no early signal)"
                             : (early == VK_SUCCESS ? "VK_SUCCESS (EARLY SIGNAL — BUG)" : "error"),
         qpc_ms(t1, t2, freq));

  // ── 5. full consumer wait: must complete at ~T_gpu after submit ──────────
  VkResult full = vkWaitSemaphores(dev, &swi, 30ull * 1000000000ull);
  QueryPerformanceCounter(&t3);
  double wait_total_ms = qpc_ms(t1, t3, freq);
  printf("full consumer wait: %s, %.1f ms after submit (T_gpu %.1f ms)\n",
         full == VK_SUCCESS ? "VK_SUCCESS" : "FAILED", wait_total_ms, t_gpu_ms);

  int rc = 0;
  if (early == VK_SUCCESS) {
    printf("VERDICT: EARLY SIGNAL — ring fence retired before GPU completion\n");
    rc = 2;
  } else if (full != VK_SUCCESS) {
    printf("VERDICT: consumer wait never completed — retire chain broken\n");
    rc = 2;
  } else if (wait_total_ms < 0.5 * t_gpu_ms) {
    printf("VERDICT: wait returned at %.0f%% of T_gpu — suspicious early retirement\n",
           100.0 * wait_total_ms / t_gpu_ms);
    rc = 2;
  } else {
    printf("VERDICT: PROVEN — consumer wait tracked GPU completion (%.0f%% of T_gpu)\n",
           100.0 * wait_total_ms / t_gpu_ms);
  }

  // Producer-side cross-check on a second value.
  {
    RECORD(fills);
    uint64_t v2 = 2;
    tssi.pSignalSemaphoreValues = &v2;
    LARGE_INTEGER a, b;
    QueryPerformanceCounter(&a);
    CHECK(vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
    uint64_t wv = 2;
    VkSemaphoreWaitInfo swi1 = { VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO };
    swi1.semaphoreCount = 1;
    swi1.pSemaphores = &sem1;
    swi1.pValues = &wv;
    VkResult r = vkWaitSemaphores(dev, &swi1, 30ull * 1000000000ull);
    QueryPerformanceCounter(&b);
    printf("producer wait sem1>=2: %s in %.1f ms (T_gpu %.1f ms)\n",
           r == VK_SUCCESS ? "VK_SUCCESS" : "FAILED", qpc_ms(a, b, freq), t_gpu_ms);
  }

  vkDeviceWaitIdle(dev);
  vkDestroySemaphore(dev, sem1, NULL);
  vkDestroySemaphore(dev, sem2, NULL);
  vkDestroyFence(dev, fence, NULL);
  vkDestroyCommandPool(dev, pool, NULL);
  vkDestroyBuffer(dev, buf, NULL);
  vkFreeMemory(dev, mem, NULL);
  vkDestroyDevice(dev, NULL);
  vkDestroyInstance(inst, NULL);
  printf("done rc=%d\n", rc);
  return rc;
}
