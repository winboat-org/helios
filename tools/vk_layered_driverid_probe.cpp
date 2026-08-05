// tools/vk_layered_driverid_probe.cpp — settles DECISIONS.md H5 / SUBSTRATE.md §7.
//
// Chains VkPhysicalDeviceLayeredApiPropertiesListKHR -> ...LayeredApiVulkanPropertiesKHR ->
// VkPhysicalDeviceDriverProperties exactly as vkd3d-proton does at
// vkd3d-proton-helios/libs/vkd3d/device.c:2323-2343, and prints the nested driverID.
//
// The answer decides the D3D12 ceiling:
//   nested driverID == VK_DRIVER_ID_NVIDIA_PROPRIETARY (4)  => vkd3d's swizzle (device.c:2657-2664)
//        fires before the shader-model caps init (:11599), the NVIDIA denorm exemption at :10699
//        applies, and SM 6.6+ / FL 12_2 are reachable.
//   nested driverID == 0 or MESA_VENUS                      => the ceiling is SM 6.0 / FL 12_1 and
//        the vkd3d fork gets its first patch (SUBSTRATE.md §7.4).
//
// Read-only: creates an instance, queries properties, exits. No device, no driver change.
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <string.h>

static const char *driver_id_name(uint32_t id)
{
    switch (id) {
    case 0:                                   return "NONE (no layered driver reported)";
    case VK_DRIVER_ID_NVIDIA_PROPRIETARY:     return "NVIDIA_PROPRIETARY";
    case VK_DRIVER_ID_MESA_VENUS:             return "MESA_VENUS";
    case VK_DRIVER_ID_AMD_PROPRIETARY:        return "AMD_PROPRIETARY";
    case VK_DRIVER_ID_AMD_OPEN_SOURCE:        return "AMD_OPEN_SOURCE";
    case VK_DRIVER_ID_MESA_RADV:              return "MESA_RADV";
    case VK_DRIVER_ID_INTEL_PROPRIETARY_WINDOWS: return "INTEL_PROPRIETARY_WINDOWS";
    case VK_DRIVER_ID_INTEL_OPEN_SOURCE_MESA: return "INTEL_OPEN_SOURCE_MESA";
    default:                                  return "other";
    }
}

// Does this physical device advertise VK_KHR_maintenance7? vkd3d only builds the layered chain
// when it does (device.c:2323), so a "0" answer is only attributable with this printed alongside.
static bool has_maintenance7(VkPhysicalDevice pd)
{
    uint32_t count = 0;
    if (vkEnumerateDeviceExtensionProperties(pd, NULL, &count, NULL) != VK_SUCCESS || !count)
        return false;
    VkExtensionProperties *props = new VkExtensionProperties[count];
    bool found = false;
    if (vkEnumerateDeviceExtensionProperties(pd, NULL, &count, props) == VK_SUCCESS) {
        for (uint32_t i = 0; i < count && !found; i++)
            found = strcmp(props[i].extensionName, VK_KHR_MAINTENANCE_7_EXTENSION_NAME) == 0;
    }
    delete[] props;
    return found;
}

int main(void)
{
    VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO };
    app.pApplicationName = "vk_layered_driverid_probe";
    app.apiVersion = VK_API_VERSION_1_3;              /* the same floor vkd3d uses */
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO };
    ici.pApplicationInfo = &app;
    VkInstance inst = VK_NULL_HANDLE;
    VkResult vr = vkCreateInstance(&ici, NULL, &inst);
    if (vr != VK_SUCCESS) { printf("vkCreateInstance failed: %d\n", (int)vr); return 1; }

    uint32_t n = 0;
    vkEnumeratePhysicalDevices(inst, &n, NULL);
    if (!n) { printf("ZERO physical devices (32-bit ICD? see SUBSTRATE.md S2)\n"); return 1; }
    VkPhysicalDevice pd[8];
    if (n > 8) n = 8;
    vkEnumeratePhysicalDevices(inst, &n, pd);

    for (uint32_t i = 0; i < n; i++) {
        VkPhysicalDeviceDriverProperties real = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES };
        VkPhysicalDeviceLayeredApiVulkanPropertiesKHR vkl = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_LAYERED_API_VULKAN_PROPERTIES_KHR };
        /* NOTE: vkl.properties.sType is deliberately left 0 — vkd3d leaves it 0 too
         * (device.c:2318-2321 memsets, :2338-2342 sets only the outer sType), and venus
         * does not read it (vn_physical_device.c:2240). Match the engine, not the spec:
         * a probe that sets it could succeed where vkd3d fails, which is the worst possible
         * outcome for a probe whose whole job is to predict vkd3d. */
        vkl.properties.pNext = &real;                 /* <- the NESTED properties2, as vkd3d does */
        VkPhysicalDeviceLayeredApiPropertiesKHR layer = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_LAYERED_API_PROPERTIES_KHR };
        layer.pNext = &vkl;
        VkPhysicalDeviceLayeredApiPropertiesListKHR list = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_LAYERED_API_PROPERTIES_LIST_KHR };
        list.layeredApiCount = 1; list.pLayeredApis = &layer;
        VkPhysicalDeviceDriverProperties top = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES };
        top.pNext = &list;
        VkPhysicalDeviceProperties2 p2 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2 };
        p2.pNext = &top;
        vkGetPhysicalDeviceProperties2(pd[i], &p2);

        const bool m7 = has_maintenance7(pd[i]);
        printf("pd[%u] %s\n", i, p2.properties.deviceName);
        printf("  VK_KHR_maintenance7 = %s   (vkd3d builds the layered chain only when present)\n",
               m7 ? "PRESENT" : "ABSENT");
        printf("  top    driverID = %u (%s) driverName=%s\n",
               (unsigned)top.driverID, driver_id_name((uint32_t)top.driverID), top.driverName);
        printf("  layeredApiCount = %u  layerVendorID=0x%04x layerDeviceID=0x%04x layerName=%s\n",
               list.layeredApiCount, layer.vendorID, layer.deviceID, layer.deviceName);
        printf("  NESTED driverID = %u (%s) driverName=%s\n",
               (unsigned)real.driverID, driver_id_name((uint32_t)real.driverID), real.driverName);
        printf("  ==> vkd3d %s swizzle driverID  ==>  SM %s\n",
               real.driverID ? "WILL" : "will NOT",
               real.driverID == VK_DRIVER_ID_NVIDIA_PROPRIETARY ? "6.6+ (FL 12_2)" : "6.0 (FL 12_1)");
    }
    return 0;
}
