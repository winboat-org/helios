// Probe which D3DKMT synchronization-object forms the Helios render adapter
// accepts. This intentionally avoids D3D/DXGI and talks directly to gdi32 KMT.
//
// Build:
//   cl /EHsc /W4 d3dkmt_sync_probe.cpp /I"Z:\icd\win-build\wdk-include" /link gdi32.lib

#include <windows.h>
#include <d3dkmthk.h>

#include <cstdio>
#include <cstdlib>
#include <cwchar>

static D3DKMT_HANDLE g_adapter;
static D3DKMT_HANDLE g_device;

static void print_status(const char* label, NTSTATUS st) {
  printf("%-52s status=0x%08x\n", label, static_cast<unsigned>(st));
}

static bool probe_helios_escape(D3DKMT_HANDLE adapter) {
  D3DKMT_CREATEDEVICE create_device{};
  create_device.hAdapter = adapter;
  NTSTATUS st = D3DKMTCreateDevice(&create_device);
  if (st != 0)
    return false;

  D3DKMT_CREATECONTEXT create_context{};
  create_context.hDevice = create_device.hDevice;
  st = D3DKMTCreateContext(&create_context);
  if (st != 0) {
    D3DKMT_DESTROYDEVICE destroy{};
    destroy.hDevice = create_device.hDevice;
    D3DKMTDestroyDevice(&destroy);
    return false;
  }

  struct {
    UINT magic;
    UINT cmd_type;
    UINT version;
    UINT size;
    UINT capset_id;
    UINT out_ctx_id;
  } ctx_create{};
  ctx_create.magic = 0x48454c53u;
  ctx_create.cmd_type = 0x0002u;
  ctx_create.version = 1u;
  ctx_create.size = sizeof(ctx_create);
  ctx_create.capset_id = 4u;

  D3DKMT_ESCAPE escape{};
  escape.hAdapter = adapter;
  escape.hDevice = create_device.hDevice;
  escape.Type = D3DKMT_ESCAPE_DRIVERPRIVATE;
  escape.pPrivateDriverData = &ctx_create;
  escape.PrivateDriverDataSize = sizeof(ctx_create);
  st = D3DKMTEscape(&escape);

  D3DKMT_DESTROYCONTEXT destroy_context{};
  destroy_context.hContext = create_context.hContext;
  D3DKMTDestroyContext(&destroy_context);
  D3DKMT_DESTROYDEVICE destroy_device{};
  destroy_device.hDevice = create_device.hDevice;
  D3DKMTDestroyDevice(&destroy_device);
  return st == 0 && ctx_create.out_ctx_id != 0;
}

static bool open_helios() {
  D3DKMT_ENUMADAPTERS2 enum_adapters{};
  NTSTATUS st = D3DKMTEnumAdapters2(&enum_adapters);
  if (st != 0 || enum_adapters.NumAdapters == 0) {
    print_status("D3DKMTEnumAdapters2(count)", st);
    return false;
  }

  auto* adapters = static_cast<D3DKMT_ADAPTERINFO*>(
      std::calloc(enum_adapters.NumAdapters, sizeof(D3DKMT_ADAPTERINFO)));
  if (!adapters)
    return false;

  enum_adapters.pAdapters = adapters;
  st = D3DKMTEnumAdapters2(&enum_adapters);
  if (st != 0) {
    print_status("D3DKMTEnumAdapters2(list)", st);
    std::free(adapters);
    return false;
  }

  D3DKMT_HANDLE chosen = 0;
  LUID chosen_luid{};
  for (UINT i = 0; i < enum_adapters.NumAdapters; ++i) {
    const D3DKMT_HANDLE h = adapters[i].hAdapter;
    D3DKMT_ADAPTERREGISTRYINFO reg{};
    D3DKMT_QUERYADAPTERINFO query{};
    query.hAdapter = h;
    query.Type = KMTQAITYPE_ADAPTERREGISTRYINFO;
    query.pPrivateDriverData = &reg;
    query.PrivateDriverDataSize = sizeof(reg);
    st = D3DKMTQueryAdapterInfo(&query);
    wprintf(L"adapter[%u] h=0x%08x luid=%08x:%08x query=0x%08x name='%s'\n",
            i, h, adapters[i].AdapterLuid.HighPart,
            adapters[i].AdapterLuid.LowPart, static_cast<unsigned>(st),
            st == 0 ? reg.AdapterString : L"<query-failed>");
    if (!chosen && probe_helios_escape(h)) {
      chosen = h;
      chosen_luid = adapters[i].AdapterLuid;
    } else {
      D3DKMT_CLOSEADAPTER close{};
      close.hAdapter = h;
      D3DKMTCloseAdapter(&close);
    }
  }
  std::free(adapters);

  if (!chosen) {
    printf("no Helios adapter found\n");
    return false;
  }

  D3DKMT_CREATEDEVICE create_device{};
  create_device.hAdapter = chosen;
  st = D3DKMTCreateDevice(&create_device);
  if (st != 0) {
    print_status("D3DKMTCreateDevice", st);
    return false;
  }

  g_adapter = chosen;
  g_device = create_device.hDevice;
  printf("opened Helios adapter=0x%08x device=0x%08x luid=%08x:%08x\n",
         g_adapter, g_device, chosen_luid.HighPart, chosen_luid.LowPart);
  return true;
}

static void destroy_sync(D3DKMT_HANDLE sync) {
  if (!sync)
    return;
  D3DKMT_DESTROYSYNCHRONIZATIONOBJECT destroy{};
  destroy.hSyncObject = sync;
  print_status("  destroy", D3DKMTDestroySynchronizationObject(&destroy));
}

static void probe_monitored(const char* label, UINT flags_value, UINT engine_affinity) {
  D3DKMT_CREATESYNCHRONIZATIONOBJECT2 create{};
  create.hDevice = g_device;
  create.Info.Type = D3DDDI_MONITORED_FENCE;
  create.Info.Flags.Value = flags_value;
  create.Info.MonitoredFence.InitialFenceValue = 0;
  create.Info.MonitoredFence.EngineAffinity = engine_affinity;
  NTSTATUS st = D3DKMTCreateSynchronizationObject2(&create);
  print_status(label, st);
  printf("  h=0x%08x shared=0x%08x cpu=%p gpu=0x%llx flags=0x%08x engine=%u\n",
         create.hSyncObject, create.Info.SharedHandle,
         create.Info.MonitoredFence.FenceValueCPUVirtualAddress,
         static_cast<unsigned long long>(
             create.Info.MonitoredFence.FenceValueGPUVirtualAddress),
         flags_value, engine_affinity);
  destroy_sync(create.hSyncObject);
}

static void probe_fence(const char* label, UINT flags_value) {
  D3DKMT_CREATESYNCHRONIZATIONOBJECT2 create{};
  create.hDevice = g_device;
  create.Info.Type = D3DDDI_FENCE;
  create.Info.Flags.Value = flags_value;
  create.Info.Fence.FenceValue = 0;
  NTSTATUS st = D3DKMTCreateSynchronizationObject2(&create);
  print_status(label, st);
  printf("  h=0x%08x shared=0x%08x flags=0x%08x\n",
         create.hSyncObject, create.Info.SharedHandle, flags_value);
  destroy_sync(create.hSyncObject);
}

static void probe_cpu_notification(const char* label, UINT flags_value) {
  HANDLE event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
  D3DKMT_CREATESYNCHRONIZATIONOBJECT2 create{};
  create.hDevice = g_device;
  create.Info.Type = D3DDDI_CPU_NOTIFICATION;
  create.Info.Flags.Value = flags_value;
  create.Info.CPUNotification.Event = event;
  NTSTATUS st = D3DKMTCreateSynchronizationObject2(&create);
  print_status(label, st);
  printf("  h=0x%08x shared=0x%08x event=%p flags=0x%08x\n",
         create.hSyncObject, create.Info.SharedHandle, event, flags_value);
  destroy_sync(create.hSyncObject);
  CloseHandle(event);
}

int main() {
  if (!open_helios())
    return 1;

  D3DDDI_SYNCHRONIZATIONOBJECT_FLAGS f{};

  f.Value = 0;
  probe_monitored("monitored private engine=0", f.Value, 0);
  probe_monitored("monitored private engine=1", f.Value, 1);

  f.Value = 0;
  f.Shared = 1;
  probe_monitored("monitored shared-kmt engine=0", f.Value, 0);
  probe_monitored("monitored shared-kmt engine=1", f.Value, 1);

  f.Value = 0;
  f.Shared = 1;
  f.NtSecuritySharing = 1;
  probe_monitored("monitored shared-nt engine=0", f.Value, 0);
  probe_monitored("monitored shared-nt engine=1", f.Value, 1);

  f.Value = 0;
  f.NoGPUAccess = 1;
  probe_monitored("monitored nogpu private", f.Value, 0);

  f.Value = 0;
  probe_fence("legacy fence private", f.Value);
  f.Shared = 1;
  probe_fence("legacy fence shared-kmt", f.Value);
  f.NtSecuritySharing = 1;
  probe_fence("legacy fence shared-nt", f.Value);

  f.Value = 0;
  probe_cpu_notification("cpu notification private", f.Value);
  f.SignalByKmd = 1;
  probe_cpu_notification("cpu notification signal-by-kmd", f.Value);

  if (g_device) {
    D3DKMT_DESTROYDEVICE destroy{};
    destroy.hDevice = g_device;
    print_status("D3DKMTDestroyDevice", D3DKMTDestroyDevice(&destroy));
  }
  if (g_adapter) {
    D3DKMT_CLOSEADAPTER close{};
    close.hAdapter = g_adapter;
    print_status("D3DKMTCloseAdapter", D3DKMTCloseAdapter(&close));
  }
  return 0;
}
