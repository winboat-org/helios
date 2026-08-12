#define WIN32_LEAN_AND_MEAN
#include <windows.h>

#include <stdint.h>
#include <string.h>
#include <wchar.h>

#include "resolve-opencl-properties.h"
#include "opencl-forwarders.inc"

typedef struct _cl_context *cl_context;
typedef struct _cl_device_id *cl_device_id;
typedef uint32_t cl_uint;
typedef int32_t cl_int;

#define HELIOS_CL_INVALID_OPERATION (-59)

typedef cl_context(__stdcall *real_cl_create_context_fn)(
    const helios_cl_context_property *, cl_uint, const cl_device_id *,
    void(__stdcall *)(const char *, const void *, size_t, void *), void *,
    cl_int *);

static INIT_ONCE g_initialize_once = INIT_ONCE_STATIC_INIT;
static real_cl_create_context_fn g_real_cl_create_context;
static int g_filter_mixed_context;
extern IMAGE_DOS_HEADER __ImageBase;

static HMODULE load_app_local_real_opencl(void) {
  wchar_t path[32768];
  DWORD length;
  wchar_t *separator;

  length = GetModuleFileNameW((HMODULE)&__ImageBase, path,
                              (DWORD)(sizeof(path) / sizeof(path[0])));
  if (length == 0 || length >= (DWORD)(sizeof(path) / sizeof(path[0]))) {
    return NULL;
  }
  separator = wcsrchr(path, L'\\');
  if (separator == NULL) {
    return NULL;
  }
  ++separator;
  if ((size_t)(separator - path) + wcslen(L"OpenCL_real.dll") + 1U >
      sizeof(path) / sizeof(path[0])) {
    return NULL;
  }
  wcscpy_s(separator, (size_t)(path + (sizeof(path) / sizeof(path[0])) - separator),
           L"OpenCL_real.dll");
  return LoadLibraryW(path);
}

static BOOL CALLBACK initialize_proxy(PINIT_ONCE once, PVOID parameter,
                                      PVOID *context) {
  HMODULE real_opencl;
  FARPROC address;
  wchar_t enabled[2];

  (void)once;
  (void)parameter;
  (void)context;

  real_opencl = load_app_local_real_opencl();
  if (real_opencl != NULL) {
    address = GetProcAddress(real_opencl, "clCreateContext");
    _Static_assert(sizeof(address) == sizeof(g_real_cl_create_context),
                   "function pointer size mismatch");
    memcpy(&g_real_cl_create_context, &address, sizeof(address));
  }

  g_filter_mixed_context =
      GetEnvironmentVariableW(L"HELIOS_RESOLVE_OPENCL_MIXED_CONTEXT_COMPAT",
                              enabled, (DWORD)(sizeof(enabled) / sizeof(enabled[0]))) ==
          1 &&
      enabled[0] == L'1';
  return TRUE;
}

__declspec(dllexport) cl_context __stdcall clCreateContext(
    const helios_cl_context_property *properties, cl_uint num_devices,
    const cl_device_id *devices,
    void(__stdcall *notify)(const char *, const void *, size_t, void *),
    void *user_data, cl_int *errcode_ret) {
  helios_cl_context_property filtered[HELIOS_RESOLVE_FILTER_CAPACITY];
  const helios_cl_context_property *forwarded = properties;
  cl_int error = HELIOS_CL_INVALID_OPERATION;
  cl_context result = NULL;

  InitOnceExecuteOnce(&g_initialize_once, initialize_proxy, NULL, NULL);
  (void)helios_prepare_resolve_properties(
      g_filter_mixed_context, properties, filtered,
      sizeof(filtered) / sizeof(filtered[0]), &forwarded);

  if (g_real_cl_create_context != NULL) {
    result = g_real_cl_create_context(forwarded, num_devices, devices, notify,
                                      user_data, &error);
  }
  if (errcode_ret != NULL) {
    *errcode_ret = error;
  }
  return result;
}
