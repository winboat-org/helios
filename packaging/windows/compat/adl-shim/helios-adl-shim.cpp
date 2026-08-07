#define WIN32_LEAN_AND_MEAN
#define _WIN32_WINNT 0x0A00

#include <windows.h>

#include <devguid.h>
#include <devpkey.h>
#include <setupapi.h>

#include <cstddef>
#include <cstdint>
#include <cstring>

namespace {

constexpr int kAdlOk = 0;
constexpr int kAdlErrNotInitialized = -2;
constexpr int kAdlErrInvalidParameter = -3;
constexpr int kAdlErrInvalidParameterSize = -4;
constexpr int kAdlErrInvalidAdapterIndex = -5;
constexpr int kAdlAsicDiscrete = 1 << 0;
constexpr int kAmdVendorId = 0x1002;
constexpr int kUnknownPciLocation = -1;
constexpr std::size_t kAdlMaxPath = 256;
constexpr DEVPROPKEY kDeviceBusNumberProperty = {
    {0xa45c254e, 0xdf1c, 0x4efd, {0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0}},
    23};
constexpr DEVPROPKEY kDeviceAddressProperty = {
    {0xa45c254e, 0xdf1c, 0x4efd, {0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0}},
    30};

using AdlMallocCallback = void*(__stdcall*)(int);
using AdlContext = void*;

// Public ADL ABI structures. Keep these local so the compatibility DLL has no
// build-time or runtime dependency on AMD's SDK or driver.
struct AdapterInfo {
    int size;
    int adapter_index;
    char udid[kAdlMaxPath];
    int bus_number;
    int device_number;
    int function_number;
    int vendor_id;
    char adapter_name[kAdlMaxPath];
    char display_name[kAdlMaxPath];
    int present;
    int exists;
    char driver_path[kAdlMaxPath];
    char driver_path_ext[kAdlMaxPath];
    char pnp_string[kAdlMaxPath];
    int os_display_index;
};

struct AdlVersionsInfo {
    char driver_version[kAdlMaxPath];
    char catalyst_version[kAdlMaxPath];
    char catalyst_web_link[kAdlMaxPath];
};

struct AdlVersionsInfoX2 {
    char driver_version[kAdlMaxPath];
    char catalyst_version[kAdlMaxPath];
    char crimson_version[kAdlMaxPath];
    char catalyst_web_link[kAdlMaxPath];
};

static_assert(sizeof(AdapterInfo) == 1572);
static_assert(offsetof(AdapterInfo, bus_number) == 264);
static_assert(offsetof(AdapterInfo, vendor_id) == 276);
static_assert(offsetof(AdapterInfo, adapter_name) == 280);
static_assert(offsetof(AdapterInfo, display_name) == 536);
static_assert(offsetof(AdapterInfo, pnp_string) == 1312);
static_assert(sizeof(AdlVersionsInfo) == 768);
static_assert(sizeof(AdlVersionsInfoX2) == 1024);

INIT_ONCE g_discovery_once = INIT_ONCE_STATIC_INIT;
AdapterInfo g_adapter = {};
bool g_adapter_found = false;
volatile LONG g_adl1_clients = 0;
volatile LONG g_adl2_clients = 0;
int g_adl2_context_cookie = 0;

template <std::size_t Size>
void copy_string(char (&destination)[Size], const char* source) {
    if (!source) {
        destination[0] = '\0';
        return;
    }

    std::size_t index = 0;
    while (index + 1 < Size && source[index] != '\0') {
        destination[index] = source[index];
        ++index;
    }
    destination[index] = '\0';
}

char ascii_lower(char value) {
    if (value >= 'A' && value <= 'Z')
        return static_cast<char>(value + ('a' - 'A'));
    return value;
}

bool contains_ascii_case_insensitive(const char* text, const char* needle) {
    if (!text || !needle || needle[0] == '\0')
        return false;

    for (const char* candidate = text; *candidate != '\0'; ++candidate) {
        const char* left = candidate;
        const char* right = needle;
        while (*left != '\0' && *right != '\0' &&
               ascii_lower(*left) == ascii_lower(*right)) {
            ++left;
            ++right;
        }
        if (*right == '\0')
            return true;
    }
    return false;
}

bool equal_ascii_case_insensitive(const char* left, const char* right) {
    if (!left || !right)
        return false;
    while (*left != '\0' && *right != '\0') {
        if (ascii_lower(*left) != ascii_lower(*right))
            return false;
        ++left;
        ++right;
    }
    return *left == '\0' && *right == '\0';
}

bool get_uint32_property(HDEVINFO devices,
                         SP_DEVINFO_DATA* device,
                         const DEVPROPKEY& key,
                         std::uint32_t* value) {
    DEVPROPTYPE property_type = 0;
    DWORD required_size = 0;
    std::uint32_t property_value = 0;
    if (!SetupDiGetDevicePropertyW(devices,
                                   device,
                                   &key,
                                   &property_type,
                                   reinterpret_cast<PBYTE>(&property_value),
                                   sizeof(property_value),
                                   &required_size,
                                   0)) {
        return false;
    }
    if (property_type != DEVPROP_TYPE_UINT32 || required_size != sizeof(property_value))
        return false;
    *value = property_value;
    return true;
}

void find_display_name(const char* instance_id, char (&display_name)[kAdlMaxPath], int* os_index) {
    DISPLAY_DEVICEA display = {};
    display.cb = sizeof(display);
    for (DWORD index = 0; EnumDisplayDevicesA(nullptr, index, &display, 0); ++index) {
        const bool exact_device = equal_ascii_case_insensitive(display.DeviceID, instance_id);
        const bool active_helios =
            (display.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP) != 0 &&
            contains_ascii_case_insensitive(display.DeviceString, "Helios");
        if (exact_device || active_helios) {
            copy_string(display_name, display.DeviceName);
            *os_index = static_cast<int>(index);
            return;
        }
        display = {};
        display.cb = sizeof(display);
    }
}

BOOL CALLBACK discover_helios_adapter(PINIT_ONCE, PVOID, PVOID*) {
    HDEVINFO devices = SetupDiGetClassDevsA(
        &GUID_DEVCLASS_DISPLAY, nullptr, nullptr, DIGCF_PRESENT);
    if (devices == INVALID_HANDLE_VALUE)
        return TRUE;

    for (DWORD index = 0;; ++index) {
        SP_DEVINFO_DATA device = {};
        device.cbSize = sizeof(device);
        if (!SetupDiEnumDeviceInfo(devices, index, &device))
            break;

        char instance_id[kAdlMaxPath] = {};
        if (!SetupDiGetDeviceInstanceIdA(
                devices, &device, instance_id, sizeof(instance_id), nullptr)) {
            continue;
        }
        if (!contains_ascii_case_insensitive(instance_id, "VEN_1AF4&DEV_1050"))
            continue;

        char description[kAdlMaxPath] = {};
        DWORD property_type = 0;
        if (!SetupDiGetDeviceRegistryPropertyA(devices,
                                               &device,
                                               SPDRP_FRIENDLYNAME,
                                               &property_type,
                                               reinterpret_cast<PBYTE>(description),
                                               sizeof(description),
                                               nullptr)) {
            SetupDiGetDeviceRegistryPropertyA(devices,
                                              &device,
                                              SPDRP_DEVICEDESC,
                                              &property_type,
                                              reinterpret_cast<PBYTE>(description),
                                              sizeof(description),
                                              nullptr);
        }
        if (!contains_ascii_case_insensitive(description, "Helios"))
            continue;

        std::uint32_t bus_number = 0;
        std::uint32_t address = 0;
        if (!get_uint32_property(devices, &device, kDeviceBusNumberProperty, &bus_number) ||
            !get_uint32_property(devices, &device, kDeviceAddressProperty, &address)) {
            continue;
        }

        AdapterInfo result = {};
        result.size = sizeof(result);
        result.adapter_index = 0;
        // Resolve only associates an AMD candidate with OpenCL through the
        // legacy cl_amd_device_attribute_query PCI fields. CLVK deliberately
        // does not expose that AMD-only extension, so GPUDetect records its
        // OpenCL bus and device as unknown (-1). Report the same state through
        // ADL: GPUDetect then joins the records while still using the real PnP
        // identity and display name to associate this candidate with DXGI.
        //
        // Keep querying the real location above. Requiring it prevents a
        // partially enumerated device from becoming a synthetic ADL adapter,
        // and the PnP identity remains the authoritative hardware identity.
        (void)bus_number;
        (void)address;
        result.bus_number = kUnknownPciLocation;
        result.device_number = kUnknownPciLocation;
        result.function_number = kUnknownPciLocation;
        result.vendor_id = kAmdVendorId;
        result.present = 1;
        result.exists = 1;
        result.os_display_index = 0;
        copy_string(result.udid, instance_id);
        copy_string(result.adapter_name, description);
        copy_string(result.pnp_string, instance_id);
        find_display_name(instance_id, result.display_name, &result.os_display_index);

        g_adapter = result;
        g_adapter_found = true;
        break;
    }

    SetupDiDestroyDeviceInfoList(devices);
    return TRUE;
}

bool ensure_discovery() {
    if (!InitOnceExecuteOnce(&g_discovery_once, discover_helios_adapter, nullptr, nullptr))
        return false;
    return g_adapter_found;
}

bool adl1_initialized() {
    return InterlockedCompareExchange(&g_adl1_clients, 0, 0) > 0;
}

bool valid_adapter_index(int adapter_index) {
    return adapter_index == 0 && ensure_discovery();
}

void fill_version_info(AdlVersionsInfo* info) {
    std::memset(info, 0, sizeof(*info));
    copy_string(info->driver_version, "22.22.256.0 Helios compatibility adapter");
    copy_string(info->catalyst_version, "Helios");
}

void fill_version_info(AdlVersionsInfoX2* info) {
    std::memset(info, 0, sizeof(*info));
    copy_string(info->driver_version, "22.22.256.0 Helios compatibility adapter");
    copy_string(info->catalyst_version, "Helios");
    copy_string(info->crimson_version, "Helios");
}

} // namespace

extern "C" __declspec(dllexport) int __stdcall
ADL_Main_Control_Create(AdlMallocCallback callback, int) {
    if (!callback)
        return kAdlErrInvalidParameter;
    InterlockedIncrement(&g_adl1_clients);
    ensure_discovery();
    return kAdlOk;
}

extern "C" __declspec(dllexport) int __stdcall ADL_Main_Control_Destroy() {
    if (!adl1_initialized())
        return kAdlErrNotInitialized;
    InterlockedDecrement(&g_adl1_clients);
    return kAdlOk;
}

extern "C" __declspec(dllexport) int __stdcall
ADL_Adapter_NumberOfAdapters_Get(int* number_of_adapters) {
    if (!adl1_initialized())
        return kAdlErrNotInitialized;
    if (!number_of_adapters)
        return kAdlErrInvalidParameter;
    *number_of_adapters = ensure_discovery() ? 1 : 0;
    return kAdlOk;
}

extern "C" __declspec(dllexport) int __stdcall
ADL_Adapter_AdapterInfo_Get(AdapterInfo* info, int input_size) {
    if (!adl1_initialized())
        return kAdlErrNotInitialized;
    if (!info)
        return kAdlErrInvalidParameter;
    if (input_size != static_cast<int>(sizeof(AdapterInfo)))
        return kAdlErrInvalidParameterSize;
    if (!ensure_discovery())
        return kAdlErrInvalidAdapterIndex;
    std::memcpy(info, &g_adapter, sizeof(g_adapter));
    return kAdlOk;
}

extern "C" __declspec(dllexport) int __stdcall
ADL_Adapter_ASICFamilyType_Get(int adapter_index, int* asic_types, int* valid_bits) {
    if (!adl1_initialized())
        return kAdlErrNotInitialized;
    if (!asic_types || !valid_bits)
        return kAdlErrInvalidParameter;
    if (!valid_adapter_index(adapter_index))
        return kAdlErrInvalidAdapterIndex;
    *asic_types = kAdlAsicDiscrete;
    *valid_bits = kAdlAsicDiscrete;
    return kAdlOk;
}

extern "C" __declspec(dllexport) int __stdcall
ADL_Graphics_Versions_Get(AdlVersionsInfo* info) {
    if (!adl1_initialized())
        return kAdlErrNotInitialized;
    if (!info)
        return kAdlErrInvalidParameter;
    fill_version_info(info);
    return kAdlOk;
}

extern "C" __declspec(dllexport) int __stdcall
ADL_Adapter_Primary_Get(int* primary_adapter_index) {
    if (!adl1_initialized())
        return kAdlErrNotInitialized;
    if (!primary_adapter_index)
        return kAdlErrInvalidParameter;
    if (!ensure_discovery())
        return kAdlErrInvalidAdapterIndex;
    *primary_adapter_index = 0;
    return kAdlOk;
}

extern "C" __declspec(dllexport) int __stdcall
ADL2_Main_Control_Create(AdlMallocCallback callback, int, AdlContext* context) {
    if (!callback || !context)
        return kAdlErrInvalidParameter;
    *context = &g_adl2_context_cookie;
    InterlockedIncrement(&g_adl2_clients);
    ensure_discovery();
    return kAdlOk;
}

extern "C" __declspec(dllexport) int __stdcall
ADL2_Main_Control_Destroy(AdlContext context) {
    if (context != &g_adl2_context_cookie ||
        InterlockedCompareExchange(&g_adl2_clients, 0, 0) <= 0) {
        return kAdlErrNotInitialized;
    }
    InterlockedDecrement(&g_adl2_clients);
    return kAdlOk;
}

extern "C" __declspec(dllexport) int __stdcall
ADL2_Graphics_VersionsX2_Get(AdlContext context, AdlVersionsInfoX2* info) {
    if (context != &g_adl2_context_cookie)
        return kAdlErrNotInitialized;
    if (!info)
        return kAdlErrInvalidParameter;
    fill_version_info(info);
    return kAdlOk;
}

extern "C" __declspec(dllexport) void* __stdcall
ADL2_Main_Control_GetProcAddress(AdlContext context, void* module, char* procedure_name) {
    if (context != &g_adl2_context_cookie || !module || !procedure_name)
        return nullptr;
    return reinterpret_cast<void*>(
        GetProcAddress(static_cast<HMODULE>(module), procedure_name));
}
