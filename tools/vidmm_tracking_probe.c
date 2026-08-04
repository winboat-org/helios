// Prove Helios' VidMm-only tracking allocation contract without involving Mesa.
//
// The probe creates one or more HELIOS_WDDM_ALLOC_KIND_TRACKING allocations,
// makes them resident, and prints the current-process segment usage before and
// after. Pass `nonlocal` as the third argument to exercise the shared/aperture
// budget; the default exercises local/dedicated. The KMD creates a one-page
// identity resource for each object; the full-size bytes remain owned by Venus
// on the host.
//
// Build from an MSVC developer command prompt:
//   cl /nologo /W4 /O2 tools\vidmm_tracking_probe.c \
//      /Iicd\win-build\wdk-include /Fe:vidmm_tracking_probe.exe /link gdi32.lib
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>

#ifndef _NTDEF_
typedef LONG NTSTATUS, *PNTSTATUS;
#endif
#include <d3dkmthk.h>

#ifndef STATUS_PENDING
#define STATUS_PENDING ((NTSTATUS)0x00000103L)
#endif

#define HELIOS_WDDM_MAGIC 0x4857444Du /* 'HWDM' */
#define HELIOS_WDDM_VERSION 1u
#define HELIOS_WDDM_ALLOC_KIND_TRACKING 3u
#define HELIOS_WDDM_BLOB_FLAG_NONLOCAL_TRACKING 0x40000000u
#define HELIOS_WDDM_ALLOC_KIND_SHMEM 0u
#define VIRTIO_GPU_BLOB_MEM_HOST3D 2u
#define VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE 1u
#define HELIOS_ESCAPE_MAGIC 0x48454C53u /* 'HELS' */
#define HELIOS_ESCAPE_VERSION 1u
#define HELIOS_ESCAPE_CTX_CREATE 0x0002u
#define HELIOS_ESCAPE_CTX_DESTROY 0x0003u
#define VIRTIO_GPU_CAPSET_VENUS 4u

struct helios_escape_header {
    UINT magic;
    UINT cmd_type;
    UINT version;
    UINT size;
};

struct helios_escape_ctx_create {
    struct helios_escape_header hdr;
    UINT capset_id;
    UINT out_ctx_id;
};

struct helios_escape_ctx_destroy {
    struct helios_escape_header hdr;
    UINT ctx_id;
    UINT padding;
};

struct helios_wddm_alloc_private {
    UINT64 blob_id;
    UINT64 size;
    UINT magic;
    UINT version;
    UINT blob_mem;
    UINT blob_flags;
    UINT ctx_id;
    UINT map_cache;
    UINT kind;
    UINT adopt_resource_id;
};

static D3DKMT_HANDLE g_adapter;
static D3DKMT_HANDLE g_device;
static D3DKMT_HANDLE g_context;
static D3DKMT_HANDLE g_paging_queue;
static D3DKMT_HANDLE g_paging_sync;
static LUID g_adapter_luid;
static UINT g_venus_ctx;
static int g_backed_control;
static int g_nonlocal_tracking;
static int g_shared_tracking;
static UINT64 g_last_global_committed;
static volatile LONG g_cookie_serial;

#define CROSSPROC_RESULT_MAGIC 0x43505452u /* 'CPTR' */

struct crossproc_result {
    UINT magic;
    UINT status;
    D3DKMT_HANDLE global_share;
};

static void close_adapter_handle(D3DKMT_HANDLE handle) {
    D3DKMT_CLOSEADAPTER close_adapter;
    memset(&close_adapter, 0, sizeof(close_adapter));
    close_adapter.hAdapter = handle;
    (void)D3DKMTCloseAdapter(&close_adapter);
}

static NTSTATUS escape_on(D3DKMT_HANDLE adapter, D3DKMT_HANDLE device,
                          void *data, UINT size) {
    D3DKMT_ESCAPE escape;
    memset(&escape, 0, sizeof(escape));
    escape.hAdapter = adapter;
    escape.hDevice = device;
    escape.Type = D3DKMT_ESCAPE_DRIVERPRIVATE;
    escape.pPrivateDriverData = data;
    escape.PrivateDriverDataSize = size;
    return D3DKMTEscape(&escape);
}

static int open_helios(void) {
    D3DKMT_ENUMADAPTERS2 enumerate;
    D3DKMT_ADAPTERINFO *adapters = NULL;
    D3DKMT_HANDLE chosen = 0;
    NTSTATUS status;
    UINT i;

    memset(&enumerate, 0, sizeof(enumerate));
    status = D3DKMTEnumAdapters2(&enumerate);
    if (status != 0 || enumerate.NumAdapters == 0) {
        printf("EnumAdapters2(size) failed: status=0x%08x count=%u\n",
               (unsigned)status, enumerate.NumAdapters);
        return 1;
    }

    adapters = (D3DKMT_ADAPTERINFO *)calloc(enumerate.NumAdapters,
                                             sizeof(*adapters));
    if (!adapters) {
        puts("out of memory enumerating adapters");
        return 1;
    }
    enumerate.pAdapters = adapters;
    status = D3DKMTEnumAdapters2(&enumerate);
    if (status != 0) {
        printf("EnumAdapters2(data) failed: status=0x%08x\n", (unsigned)status);
        free(adapters);
        return 1;
    }

    // KMTQAITYPE_ADAPTERREGISTRYINFO is not implemented consistently on the
    // Windows build used by this VM. Probe the versioned Helios escape instead;
    // other display KMDs reject it. Destroy the temporary Venus context before
    // retaining the matching KMT device.
    for (i = 0; i < enumerate.NumAdapters; ++i) {
        D3DKMT_CREATEDEVICE create_device;
        struct helios_escape_ctx_create create_context;

        memset(&create_device, 0, sizeof(create_device));
        create_device.hAdapter = adapters[i].hAdapter;
        status = D3DKMTCreateDevice(&create_device);
        if (status == 0) {
            memset(&create_context, 0, sizeof(create_context));
            create_context.hdr.magic = HELIOS_ESCAPE_MAGIC;
            create_context.hdr.cmd_type = HELIOS_ESCAPE_CTX_CREATE;
            create_context.hdr.version = HELIOS_ESCAPE_VERSION;
            create_context.hdr.size = sizeof(create_context);
            create_context.capset_id = VIRTIO_GPU_CAPSET_VENUS;
            status = escape_on(adapters[i].hAdapter, create_device.hDevice,
                               &create_context, sizeof(create_context));
            printf("adapter[%u] luid=%08x:%08x Helios probe=0x%08x\n", i,
                   (unsigned)adapters[i].AdapterLuid.HighPart,
                   (unsigned)adapters[i].AdapterLuid.LowPart, (unsigned)status);
            if (!chosen && status == 0 && create_context.out_ctx_id != 0) {
                chosen = adapters[i].hAdapter;
                g_device = create_device.hDevice;
                g_adapter_luid = adapters[i].AdapterLuid;
                g_venus_ctx = create_context.out_ctx_id;
                continue;
            }

            {
                D3DKMT_DESTROYDEVICE destroy_device;
                memset(&destroy_device, 0, sizeof(destroy_device));
                destroy_device.hDevice = create_device.hDevice;
                (void)D3DKMTDestroyDevice(&destroy_device);
            }
        }
        close_adapter_handle(adapters[i].hAdapter);
    }
    free(adapters);
    if (!chosen) {
        puts("no Helios adapter found");
        return 1;
    }
    g_adapter = chosen;

    if (!g_device) {
        D3DKMT_CREATEDEVICE create_device;
        memset(&create_device, 0, sizeof(create_device));
        create_device.hAdapter = g_adapter;
        status = D3DKMTCreateDevice(&create_device);
        if (status != 0) {
            printf("CreateDevice failed: status=0x%08x\n", (unsigned)status);
            return 1;
        }
        g_device = create_device.hDevice;
    }

    {
        D3DKMT_CREATECONTEXTVIRTUAL create_context;
        memset(&create_context, 0, sizeof(create_context));
        create_context.hDevice = g_device;
        create_context.NodeOrdinal = 0;
        status = D3DKMTCreateContextVirtual(&create_context);
        if (status != 0) {
            printf("CreateContext failed: status=0x%08x\n", (unsigned)status);
            return 1;
        }
        g_context = create_context.hContext;
    }

    {
        D3DKMT_CREATEPAGINGQUEUE create_queue;
        memset(&create_queue, 0, sizeof(create_queue));
        create_queue.hDevice = g_device;
        create_queue.Priority = D3DDDI_PAGINGQUEUE_PRIORITY_NORMAL;
        status = D3DKMTCreatePagingQueue(&create_queue);
        if (status != 0) {
            printf("CreatePagingQueue failed: status=0x%08x\n", (unsigned)status);
            return 1;
        }
        g_paging_queue = create_queue.hPagingQueue;
        g_paging_sync = create_queue.hSyncObject;
    }

    printf("opened Helios adapter=0x%x device=0x%x paging_queue=0x%x\n",
           (unsigned)g_adapter, (unsigned)g_device, (unsigned)g_paging_queue);
    return 0;
}

static int query_budget(const char *label, UINT64 *usage_out) {
    D3DKMT_QUERYVIDEOMEMORYINFO query;
    NTSTATUS status;

    memset(&query, 0, sizeof(query));
    query.hProcess = GetCurrentProcess();
    query.hAdapter = g_adapter;
    query.MemorySegmentGroup = g_nonlocal_tracking
                                   ? D3DKMT_MEMORY_SEGMENT_GROUP_NON_LOCAL
                                   : D3DKMT_MEMORY_SEGMENT_GROUP_LOCAL;
    status = D3DKMTQueryVideoMemoryInfo(&query);
    if (status != 0) {
        printf("%s: QueryVideoMemoryInfo failed: status=0x%08x\n", label,
               (unsigned)status);
        return 1;
    }
    printf("%-16s usage=%10.2f MiB  budget=%10.2f MiB  reservation=%10.2f MiB\n",
           label, (double)query.CurrentUsage / (1024.0 * 1024.0),
           (double)query.Budget / (1024.0 * 1024.0),
           (double)query.CurrentReservation / (1024.0 * 1024.0));
    if (usage_out) {
        *usage_out = query.CurrentUsage;
    }
    return 0;
}

static int query_raw_statistics(const char *label, UINT64 *usage_out) {
    D3DKMT_QUERYSTATISTICS query;
    NTSTATUS status;
    ULONG segment_id;
    UINT64 raw_tracking_usage = 0;
    int selected_segment_found = 0;
    int query_failed = 0;

    g_last_global_committed = 0;

    for (segment_id = 1; segment_id <= 2; ++segment_id) {
        int is_local_segment = 0;
        UINT64 global_committed = 0;
        int have_global_statistics = 0;

        memset(&query, 0, sizeof(query));
        query.Type = D3DKMT_QUERYSTATISTICS_SEGMENT;
        query.AdapterLuid = g_adapter_luid;
        query.QuerySegment.SegmentId = segment_id;
        status = D3DKMTQueryStatistics(&query);
        if (status == 0) {
            const D3DKMT_QUERYSTATISTICS_SEGMENT_INFORMATION *segment =
                &query.QueryResult.SegmentInformation;
            is_local_segment = segment->Aperture == 0;
            global_committed = segment->BytesCommitted;
            have_global_statistics = 1;
            printf("%-16s raw seg%lu committed=%10.2f MiB resident=%10.2f MiB "
                   "allocs=%lu/%lu aperture=%lu\n",
                   label, segment_id,
                   (double)segment->BytesCommitted / (1024.0 * 1024.0),
                   (double)segment->BytesResident / (1024.0 * 1024.0),
                   (unsigned long)segment->Memory.AllocsCommitted,
                   (unsigned long)segment->Memory.AllocsResident,
                   (unsigned long)segment->Aperture);
        } else {
            printf("%-16s raw seg%lu query failed: status=0x%08x\n", label,
                   segment_id, (unsigned)status);
            query_failed = 1;
        }

        memset(&query, 0, sizeof(query));
        query.Type = D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT;
        query.AdapterLuid = g_adapter_luid;
        query.hProcess = GetCurrentProcess();
        query.QueryProcessSegment.SegmentId = segment_id;
        status = D3DKMTQueryStatistics(&query);
        if (status == 0) {
            const D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT_INFORMATION *process =
                &query.QueryResult.ProcessSegmentInformation;
            printf("%-16s process seg%lu committed=%10.2f MiB allocations=%lu\n",
                   label, segment_id,
                   (double)process->BytesCommitted / (1024.0 * 1024.0),
                   (unsigned long)process->VideoMemory.AllocsCommitted);
            if (is_local_segment != g_nonlocal_tracking) {
                if (have_global_statistics) {
                    g_last_global_committed = global_committed;
                    selected_segment_found = 1;
                }
                raw_tracking_usage += process->BytesCommitted;
            }
        } else {
            printf("%-16s process seg%lu query failed: status=0x%08x\n", label,
                   segment_id, (unsigned)status);
            query_failed = 1;
        }
    }

    memset(&query, 0, sizeof(query));
    query.Type = D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT_GROUP;
    query.AdapterLuid = g_adapter_luid;
    query.hProcess = GetCurrentProcess();
    query.QueryProcessSegmentGroup = g_nonlocal_tracking
                                         ? D3DKMT_MEMORY_SEGMENT_GROUP_NON_LOCAL
                                         : D3DKMT_MEMORY_SEGMENT_GROUP_LOCAL;
    status = D3DKMTQueryStatistics(&query);
    if (status == 0) {
        const D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT_GROUP_INFORMATION *group =
            &query.QueryResult.ProcessSegmentGroupInformation;
        printf("%-16s process %s requested=%10.2f MiB usage=%10.2f MiB\n",
               label, g_nonlocal_tracking ? "nonlocal" : "local",
               (double)group->Requested / (1024.0 * 1024.0),
               (double)group->Usage / (1024.0 * 1024.0));
    } else {
        printf("%-16s process group query failed: status=0x%08x\n", label,
               (unsigned)status);
    }
    // QueryVideoMemoryInfo and PROCESS_SEGMENT_GROUP both report zero for this
    // stand-alone KMT probe on the current Windows 11 build, even though the
    // per-segment statistics (and Task Manager) account the bytes correctly.
    // Use the selected process-segment total as the authoritative raw counter
    // for the probe's pass/fail decision.
    if (usage_out)
        *usage_out = raw_tracking_usage;
    return query_failed || !selected_segment_found;
}

static int wait_for_global_delta(const char *label, UINT64 baseline,
                                 UINT64 expected) {
    const ULONGLONG deadline = GetTickCount64() + 2000;
    for (;;) {
        UINT64 unused = 0;
        UINT64 delta;
        if (query_raw_statistics(label, &unused))
            return 1;
        delta = g_last_global_committed >= baseline
                    ? g_last_global_committed - baseline : 0;
        if (delta >= expected && delta <= expected + 1024ull * 1024ull)
            return 0;
        if (GetTickCount64() >= deadline)
            return 1;
        Sleep(25);
    }
}

static int wait_for_cleanup(UINT64 raw_before, UINT64 global_before,
                            int check_global, UINT64 *raw_after) {
    const ULONGLONG deadline = GetTickCount64() + 2000;
    for (;;) {
        if (query_raw_statistics("destroyed", raw_after))
            return 1;
        if (*raw_after <= raw_before + 1024ull * 1024ull &&
            (!check_global || g_last_global_committed <=
                                  global_before + 16ull * 1024ull * 1024ull))
            return 0;
        if (GetTickCount64() >= deadline)
            return 1;
        Sleep(25);
    }
}

static int create_tracking(UINT64 size, D3DKMT_HANDLE *allocation_out,
                           D3DKMT_HANDLE *resource_out,
                           D3DKMT_HANDLE *global_out) {
    struct helios_wddm_alloc_private private_data;
    D3DDDI_ALLOCATIONINFO2 allocation_info;
    D3DKMT_CREATEALLOCATION create;
    NTSTATUS status;

    memset(&private_data, 0, sizeof(private_data));
    private_data.size = size;
    private_data.magic = HELIOS_WDDM_MAGIC;
    private_data.version = HELIOS_WDDM_VERSION;
    private_data.kind = HELIOS_WDDM_ALLOC_KIND_TRACKING;
    private_data.ctx_id = g_venus_ctx;
    if (!g_backed_control) {
        private_data.map_cache = GetTickCount() ^ GetCurrentProcessId() ^
                                 (UINT)InterlockedIncrement(&g_cookie_serial);
        if (private_data.map_cache == 0)
            private_data.map_cache = 1;
    }
    if (g_nonlocal_tracking) {
        private_data.blob_flags = HELIOS_WDDM_BLOB_FLAG_NONLOCAL_TRACKING;
    }
    if (g_backed_control) {
        private_data.kind = HELIOS_WDDM_ALLOC_KIND_SHMEM;
        private_data.blob_mem = VIRTIO_GPU_BLOB_MEM_HOST3D;
        private_data.blob_flags = VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
        private_data.ctx_id = g_venus_ctx;
    }

    memset(&allocation_info, 0, sizeof(allocation_info));
    allocation_info.pPrivateDriverData = &private_data;
    allocation_info.PrivateDriverDataSize = sizeof(private_data);

    memset(&create, 0, sizeof(create));
    create.hDevice = g_device;
    create.Flags.CreateResource = 1;
    create.Flags.CreateShared = g_shared_tracking != 0;
    // The one-page KMD identity resource has no application-visible content.
    create.Flags.AllowNotZeroed = 1;
    create.NumAllocations = 1;
    // CreateAllocation2's advanced-scheduling path consumes ALLOCATIONINFO2
    // (and returns the allocation's GPU VA there). Passing the legacy layout
    // silently creates a non-virtualized object whose residency is charged to
    // the paging aperture on current Windows.
    create.pAllocationInfo2 = &allocation_info;
    // Microsoft documents CreateAllocation2 as the client-driver entry point,
    // including specifically for a stand-alone video-memory allocation.
    status = D3DKMTCreateAllocation2(&create);
    if (status != 0) {
        printf("CreateAllocation(%llu bytes) failed: status=0x%08x\n",
               (unsigned long long)size, (unsigned)status);
        return 1;
    }
    *allocation_out = allocation_info.hAllocation;
    *resource_out = create.hResource;
    *global_out = create.hGlobalShare;
    printf("  tracking hResource=0x%x hAllocation=0x%x hGlobal=0x%x gpuva=0x%llx\n",
           (unsigned)create.hResource,
           (unsigned)allocation_info.hAllocation,
           (unsigned)create.hGlobalShare,
           (unsigned long long)allocation_info.GpuVirtualAddress);
    if (g_shared_tracking && create.hGlobalShare == 0) {
        D3DKMT_DESTROYALLOCATION destroy;
        puts("CreateShared returned no global handle");
        memset(&destroy, 0, sizeof(destroy));
        destroy.hDevice = g_device;
        destroy.hResource = create.hResource;
        (void)D3DKMTDestroyAllocation(&destroy);
        *allocation_out = 0;
        *resource_out = 0;
        return 1;
    }
    return 0;
}

static int open_shared_tracking(D3DKMT_HANDLE global,
                                UINT64 expected_size,
                                D3DKMT_HANDLE *allocation_out,
                                D3DKMT_HANDLE *resource_out) {
    D3DKMT_QUERYRESOURCEINFO query;
    D3DKMT_OPENRESOURCE open;
    D3DDDI_OPENALLOCATIONINFO allocation_info;
    void *runtime_data = NULL;
    void *resource_private = NULL;
    void *total_private = NULL;
    NTSTATUS status;

    memset(&query, 0, sizeof(query));
    query.hDevice = g_device;
    query.hGlobalShare = global;
    status = D3DKMTQueryResourceInfo(&query);
    if (status != 0 || query.NumAllocations != 1) {
        printf("QueryResourceInfo(global=0x%x) failed: status=0x%08x allocs=%u\n",
               (unsigned)global, (unsigned)status, query.NumAllocations);
        return 1;
    }
    runtime_data = query.PrivateRuntimeDataSize
                       ? calloc(1, query.PrivateRuntimeDataSize) : NULL;
    resource_private = query.ResourcePrivateDriverDataSize
                           ? calloc(1, query.ResourcePrivateDriverDataSize) : NULL;
    total_private = query.TotalPrivateDriverDataSize
                        ? calloc(1, query.TotalPrivateDriverDataSize) : NULL;
    if ((query.PrivateRuntimeDataSize && !runtime_data) ||
        (query.ResourcePrivateDriverDataSize && !resource_private) ||
        (query.TotalPrivateDriverDataSize && !total_private)) {
        goto failed;
    }
    if (query.PrivateRuntimeDataSize) {
        query.pPrivateRuntimeData = runtime_data;
        status = D3DKMTQueryResourceInfo(&query);
        if (status != 0) {
            printf("QueryResourceInfo(data) failed: status=0x%08x\n",
                   (unsigned)status);
            goto failed;
        }
    }

    memset(&allocation_info, 0, sizeof(allocation_info));
    memset(&open, 0, sizeof(open));
    open.hDevice = g_device;
    open.hGlobalShare = global;
    open.NumAllocations = 1;
    open.pOpenAllocationInfo = &allocation_info;
    open.pPrivateRuntimeData = runtime_data;
    open.PrivateRuntimeDataSize = query.PrivateRuntimeDataSize;
    open.pResourcePrivateDriverData = resource_private;
    open.ResourcePrivateDriverDataSize = query.ResourcePrivateDriverDataSize;
    open.pTotalPrivateDriverDataBuffer = total_private;
    open.TotalPrivateDriverDataBufferSize = query.TotalPrivateDriverDataSize;
    status = D3DKMTOpenResource(&open);
    if (status == 0 && open.hResource != 0 &&
        allocation_info.hAllocation != 0) {
        struct helios_wddm_alloc_private private_data;
        memset(&private_data, 0, sizeof(private_data));
        if (allocation_info.PrivateDriverDataSize >= sizeof(private_data) &&
            allocation_info.pPrivateDriverData != NULL) {
            memcpy(&private_data, allocation_info.pPrivateDriverData,
                   sizeof(private_data));
        }
        if (private_data.magic != HELIOS_WDDM_MAGIC ||
            private_data.version != HELIOS_WDDM_VERSION ||
            private_data.kind != HELIOS_WDDM_ALLOC_KIND_TRACKING ||
            private_data.size != expected_size || private_data.map_cache == 0) {
            puts("opened tracker identity does not match the expected allocation");
            status = (NTSTATUS)0xc000000du;
        }
    }
    if (status != 0 || open.hResource == 0 || allocation_info.hAllocation == 0) {
        if (open.hResource != 0) {
            D3DKMT_DESTROYALLOCATION destroy;
            memset(&destroy, 0, sizeof(destroy));
            destroy.hDevice = g_device;
            destroy.hResource = open.hResource;
            (void)D3DKMTDestroyAllocation(&destroy);
        }
        printf("OpenResource(global=0x%x) failed: status=0x%08x resource=0x%x allocation=0x%x\n",
               (unsigned)global, (unsigned)status, (unsigned)open.hResource,
               (unsigned)allocation_info.hAllocation);
        goto failed;
    }
    *resource_out = open.hResource;
    *allocation_out = allocation_info.hAllocation;
    printf("  opened   hResource=0x%x hAllocation=0x%x hGlobal=0x%x\n",
           (unsigned)open.hResource, (unsigned)allocation_info.hAllocation,
           (unsigned)global);
    free(runtime_data);
    free(resource_private);
    free(total_private);
    return 0;

failed:
    free(runtime_data);
    free(resource_private);
    free(total_private);
    return 1;
}

static int make_resident(D3DKMT_HANDLE *allocations, UINT count) {
    D3DDDI_MAKERESIDENT make;
    UINT *priorities;
    NTSTATUS status;

    priorities = (UINT *)calloc(count, sizeof(*priorities));
    if (!priorities) {
        return 1;
    }
    for (UINT i = 0; i < count; ++i) {
        priorities[i] = D3DDDI_ALLOCATIONPRIORITY_MAXIMUM;
    }
    memset(&make, 0, sizeof(make));
    make.hPagingQueue = g_paging_queue;
    make.NumAllocations = count;
    make.AllocationList = allocations;
    make.PriorityList = priorities;
    status = D3DKMTMakeResident(&make);
    free(priorities);
    printf("MakeResident:      status=0x%08x resident=%u fence=%llu trim=%llu\n",
           (unsigned)status, make.NumAllocations,
           (unsigned long long)make.PagingFenceValue,
           (unsigned long long)make.NumBytesToTrim);
    if (status != 0 && status != STATUS_PENDING) {
        return 1;
    }

    if (make.PagingFenceValue != 0) {
        D3DKMT_WAITFORSYNCHRONIZATIONOBJECTFROMCPU wait;
        UINT64 value = make.PagingFenceValue;

        memset(&wait, 0, sizeof(wait));
        wait.hDevice = g_device;
        wait.ObjectCount = 1;
        wait.ObjectHandleArray = &g_paging_sync;
        wait.FenceValueArray = &value;
        status = D3DKMTWaitForSynchronizationObjectFromCpu(&wait);
        if (status != 0) {
            printf("paging-fence wait failed: status=0x%08x value=%llu\n",
                   (unsigned)status, (unsigned long long)value);
            return 1;
        }
    }
    return 0;
}

static void evict_and_destroy(D3DKMT_HANDLE *allocations,
                              D3DKMT_HANDLE *resources, UINT count) {
    if (count != 0) {
        D3DKMT_EVICT evict;
        D3DKMT_DESTROYALLOCATION destroy;
        NTSTATUS status;

        memset(&evict, 0, sizeof(evict));
        evict.hDevice = g_device;
        evict.NumAllocations = count;
        evict.AllocationList = allocations;
        status = D3DKMTEvict(&evict);
        printf("Evict:             status=0x%08x trim=%llu\n", (unsigned)status,
               (unsigned long long)evict.NumBytesToTrim);

        for (UINT i = 0; i < count; ++i) {
            memset(&destroy, 0, sizeof(destroy));
            destroy.hDevice = g_device;
            destroy.hResource = resources[i];
            status = D3DKMTDestroyAllocation(&destroy);
            printf("DestroyAllocation[%u]: status=0x%08x\n", i,
                   (unsigned)status);
        }
    }
}

static void shutdown_kmt(void) {
    if (g_venus_ctx) {
        struct helios_escape_ctx_destroy destroy_venus;
        memset(&destroy_venus, 0, sizeof(destroy_venus));
        destroy_venus.hdr.magic = HELIOS_ESCAPE_MAGIC;
        destroy_venus.hdr.cmd_type = HELIOS_ESCAPE_CTX_DESTROY;
        destroy_venus.hdr.version = HELIOS_ESCAPE_VERSION;
        destroy_venus.hdr.size = sizeof(destroy_venus);
        destroy_venus.ctx_id = g_venus_ctx;
        (void)escape_on(g_adapter, g_device, &destroy_venus,
                        sizeof(destroy_venus));
    }
    if (g_context) {
        D3DKMT_DESTROYCONTEXT destroy_context;
        memset(&destroy_context, 0, sizeof(destroy_context));
        destroy_context.hContext = g_context;
        (void)D3DKMTDestroyContext(&destroy_context);
    }
    if (g_paging_queue) {
        D3DDDI_DESTROYPAGINGQUEUE destroy_queue;
        memset(&destroy_queue, 0, sizeof(destroy_queue));
        destroy_queue.hPagingQueue = g_paging_queue;
        (void)D3DKMTDestroyPagingQueue(&destroy_queue);
    }
    if (g_device) {
        D3DKMT_DESTROYDEVICE destroy_device;
        memset(&destroy_device, 0, sizeof(destroy_device));
        destroy_device.hDevice = g_device;
        (void)D3DKMTDestroyDevice(&destroy_device);
    }
    if (g_adapter) {
        close_adapter_handle(g_adapter);
    }
}

static int write_crossproc_result(HANDLE pipe, UINT status,
                                  D3DKMT_HANDLE global_share) {
    struct crossproc_result result;
    DWORD written = 0;

    memset(&result, 0, sizeof(result));
    result.magic = CROSSPROC_RESULT_MAGIC;
    result.status = status;
    result.global_share = global_share;
    return !WriteFile(pipe, &result, sizeof(result), &written, NULL) ||
           written != sizeof(result);
}

static int read_crossproc_result(HANDLE pipe, HANDLE child,
                                 struct crossproc_result *result) {
    const ULONGLONG deadline = GetTickCount64() + 30000;

    for (;;) {
        DWORD available = 0;
        DWORD read = 0;
        if (!PeekNamedPipe(pipe, NULL, 0, NULL, &available, NULL)) {
            printf("PeekNamedPipe failed: error=%lu\n", GetLastError());
            return 1;
        }
        if (available >= sizeof(*result)) {
            return !ReadFile(pipe, result, sizeof(*result), &read, NULL) ||
                   read != sizeof(*result);
        }
        if (WaitForSingleObject(child, 10) == WAIT_OBJECT_0) {
            if (!PeekNamedPipe(pipe, NULL, 0, NULL, &available, NULL) ||
                available < sizeof(*result)) {
                puts("creator child exited before publishing its result");
                return 1;
            }
        }
        if (GetTickCount64() >= deadline) {
            puts("timed out waiting for creator child result");
            return 1;
        }
    }
}

static int run_creator_child(UINT64 mib, HANDLE result_pipe,
                             HANDLE release_event) {
    D3DKMT_HANDLE allocation = 0;
    D3DKMT_HANDLE resource = 0;
    D3DKMT_HANDLE global_share = 0;
    int failed = 0;

    g_shared_tracking = 1;
    if (open_helios() ||
        create_tracking(mib * 1024ull * 1024ull, &allocation, &resource,
                        &global_share) ||
        make_resident(&allocation, 1)) {
        failed = 1;
    }
    if (write_crossproc_result(result_pipe, failed ? 1u : 0u, global_share)) {
        failed = 1;
    }
    CloseHandle(result_pipe);

    if (!failed && WaitForSingleObject(release_event, 30000) != WAIT_OBJECT_0) {
        puts("creator child timed out waiting for importer");
        failed = 1;
    }
    CloseHandle(release_event);
    if (allocation && resource) {
        evict_and_destroy(&allocation, &resource, 1);
    }
    shutdown_kmt();
    return failed ? 1 : 0;
}

static int run_cross_process_test(UINT64 mib, UINT64 global_before) {
    SECURITY_ATTRIBUTES security;
    STARTUPINFOA startup;
    PROCESS_INFORMATION process = {0};
    struct crossproc_result result;
    D3DKMT_HANDLE opened_allocation = 0;
    D3DKMT_HANDLE opened_resource = 0;
    HANDLE read_pipe = NULL;
    HANDLE write_pipe = NULL;
    HANDLE release_event = NULL;
    char executable[MAX_PATH];
    char command[MAX_PATH + 128];
    DWORD child_exit = 1;
    UINT64 expected = mib * 1024ull * 1024ull;
    int failed = 0;

    memset(&security, 0, sizeof(security));
    security.nLength = sizeof(security);
    security.bInheritHandle = TRUE;
    if (!CreatePipe(&read_pipe, &write_pipe, &security, 0) ||
        !SetHandleInformation(read_pipe, HANDLE_FLAG_INHERIT, 0)) {
        puts("failed to create cross-process result pipe");
        failed = 1;
        goto cleanup;
    }
    release_event = CreateEventA(&security, TRUE, FALSE, NULL);
    if (!release_event) {
        puts("failed to create cross-process release event");
        failed = 1;
        goto cleanup;
    }
    if (!GetModuleFileNameA(NULL, executable, sizeof(executable))) {
        puts("failed to resolve probe executable path");
        failed = 1;
        goto cleanup;
    }

    snprintf(command, sizeof(command),
             "\"%s\" --creator-child %llu %llu %llu", executable,
             (unsigned long long)mib,
             (unsigned long long)(UINT_PTR)write_pipe,
             (unsigned long long)(UINT_PTR)release_event);
    memset(&startup, 0, sizeof(startup));
    startup.cb = sizeof(startup);
    if (!CreateProcessA(NULL, command, NULL, NULL, TRUE, 0, NULL, NULL,
                        &startup, &process)) {
        printf("CreateProcess failed: error=%lu\n", GetLastError());
        failed = 1;
        goto cleanup;
    }
    CloseHandle(write_pipe);
    write_pipe = NULL;

    memset(&result, 0, sizeof(result));
    if (read_crossproc_result(read_pipe, process.hProcess, &result) ||
        result.magic != CROSSPROC_RESULT_MAGIC ||
        result.status != 0 || result.global_share == 0) {
        printf("creator child failed: magic=0x%x status=%u global=0x%x\n",
               result.magic, result.status,
               (unsigned)result.global_share);
        failed = 1;
    }
    if (!failed &&
        (open_shared_tracking(result.global_share, mib * 1024ull * 1024ull,
                              &opened_allocation,
                              &opened_resource) ||
         make_resident(&opened_allocation, 1))) {
        failed = 1;
    }
    if (!failed) {
        if (wait_for_global_delta("cross-open", global_before, expected)) {
            puts("FAIL: cross-process open changed the adapter's one global charge");
            failed = 1;
        }
    }

    SetEvent(release_event);
    if (WaitForSingleObject(process.hProcess, 30000) != WAIT_OBJECT_0) {
        puts("creator child timed out during teardown; terminating it");
        (void)TerminateProcess(process.hProcess, 1);
        (void)WaitForSingleObject(process.hProcess, 5000);
        failed = 1;
    }
    if (!GetExitCodeProcess(process.hProcess, &child_exit) || child_exit != 0) {
        printf("creator child did not exit cleanly: code=%lu\n", child_exit);
        failed = 1;
    }
    if (opened_allocation && opened_resource) {
        if (wait_for_global_delta("creator-exited", global_before, expected)) {
            puts("FAIL: imported tracker did not survive its creator process");
            failed = 1;
        }
        evict_and_destroy(&opened_allocation, &opened_resource, 1);
        opened_allocation = 0;
        opened_resource = 0;
    }
cleanup:
    if (process.hProcess &&
        WaitForSingleObject(process.hProcess, 0) == WAIT_TIMEOUT) {
        (void)TerminateProcess(process.hProcess, 1);
        (void)WaitForSingleObject(process.hProcess, 5000);
    }
    if (process.hThread)
        CloseHandle(process.hThread);
    if (process.hProcess)
        CloseHandle(process.hProcess);
    if (release_event) {
        SetEvent(release_event);
        CloseHandle(release_event);
    }
    if (write_pipe) {
        CloseHandle(write_pipe);
    }
    if (read_pipe) {
        CloseHandle(read_pipe);
    }
    if (opened_allocation && opened_resource) {
        evict_and_destroy(&opened_allocation, &opened_resource, 1);
    }
    return failed;
}

int main(int argc, char **argv) {
    UINT count = 4;
    UINT64 mib = 64;
    D3DKMT_HANDLE *allocations = NULL;
    D3DKMT_HANDLE *resources = NULL;
    D3DKMT_HANDLE *global_shares = NULL;
    D3DKMT_HANDLE *opened_allocations = NULL;
    D3DKMT_HANDLE *opened_resources = NULL;
    UINT created = 0;
    UINT64 raw_before = 0;
    UINT64 raw_resident = 0;
    UINT64 raw_after = 0;
    UINT64 global_before = 0;
    int cross_process = 0;
    int failed = 0;

    if (argc == 5 && strcmp(argv[1], "--creator-child") == 0) {
        UINT64 child_mib = _strtoui64(argv[2], NULL, 0);
        HANDLE result_pipe = (HANDLE)(UINT_PTR)_strtoui64(argv[3], NULL, 0);
        HANDLE release_event = (HANDLE)(UINT_PTR)_strtoui64(argv[4], NULL, 0);
        return run_creator_child(child_mib, result_pipe, release_event);
    }

    if (argc > 4) {
        failed = 1;
    }
    if (argc > 1) {
        count = (UINT)strtoul(argv[1], NULL, 0);
    }
    if (argc > 2) {
        mib = _strtoui64(argv[2], NULL, 0);
    }
    if (argc > 3 && strcmp(argv[3], "backed") == 0) {
        g_backed_control = 1;
    } else if (argc > 3 && strcmp(argv[3], "nonlocal") == 0) {
        g_nonlocal_tracking = 1;
    } else if (argc > 3 && strcmp(argv[3], "shared") == 0) {
        g_shared_tracking = 1;
    } else if (argc > 3 && strcmp(argv[3], "crossproc") == 0) {
        g_shared_tracking = 1;
        cross_process = 1;
    } else if (argc > 3) {
        failed = 1;
    }
    if (failed || count == 0 || count > 1024 || mib == 0 || mib > 4096 ||
        (cross_process && count != 1)) {
        fprintf(stderr,
                "usage: %s [allocation-count 1..1024] [MiB 1..4096] "
                "[nonlocal|backed|shared|crossproc]\n"
                "       crossproc requires allocation-count=1\n",
                argv[0]);
        return 2;
    }

    allocations = (D3DKMT_HANDLE *)calloc(count, sizeof(*allocations));
    resources = (D3DKMT_HANDLE *)calloc(count, sizeof(*resources));
    global_shares = (D3DKMT_HANDLE *)calloc(count, sizeof(*global_shares));
    opened_allocations = (D3DKMT_HANDLE *)calloc(count, sizeof(*opened_allocations));
    opened_resources = (D3DKMT_HANDLE *)calloc(count, sizeof(*opened_resources));
    if (!allocations || !resources || !global_shares || !opened_allocations ||
        !opened_resources) {
        puts("out of memory allocating handle list");
        failed = 1;
        goto cleanup;
    }
    if (open_helios()) {
        failed = 1;
        goto cleanup;
    }
    if (query_budget("baseline", NULL)) {
        failed = 1;
        goto cleanup;
    }
    if (query_raw_statistics("baseline", &raw_before)) {
        failed = 1;
        goto cleanup;
    }
    global_before = g_last_global_committed;

    if (cross_process) {
        failed = run_cross_process_test(mib, global_before);
        goto cleanup;
    }

    for (created = 0; created < count; ++created) {
        if (create_tracking(mib * 1024ull * 1024ull, &allocations[created],
                            &resources[created], &global_shares[created])) {
            failed = 1;
            break;
        }
    }
    printf("created %u tracking allocations of %llu MiB\n", created,
           (unsigned long long)mib);
    (void)query_budget("created", NULL);
    if (query_raw_statistics("created", NULL))
        failed = 1;
    if (!failed && make_resident(allocations, created)) {
        failed = 1;
    }
    if (!failed && query_budget("resident", NULL)) {
        failed = 1;
    }
    if (query_raw_statistics("resident", &raw_resident))
        failed = 1;
    if (!failed) {
        UINT64 expected = (UINT64)created * mib * 1024ull * 1024ull;
        UINT64 delta = raw_resident >= raw_before ? raw_resident - raw_before : 0;
        printf("resident delta:   %.2f MiB (expected %.2f MiB)\n",
               (double)delta / (1024.0 * 1024.0),
               (double)expected / (1024.0 * 1024.0));
        const UINT64 tolerance = 1024ull * 1024ull;
        if (delta < expected || delta > expected + tolerance) {
            puts("FAIL: raw process usage did not match the tracking allocation total");
            failed = 1;
        }
    }
    if (!failed && g_shared_tracking) {
        UINT opened = 0;
        for (; opened < created; ++opened) {
            if (open_shared_tracking(global_shares[opened],
                                     mib * 1024ull * 1024ull,
                                     &opened_allocations[opened],
                                     &opened_resources[opened])) {
                failed = 1;
                break;
            }
        }
        if (!failed && make_resident(opened_allocations, opened)) {
            failed = 1;
        }
        if (!failed) {
            UINT64 expected = (UINT64)created * mib * 1024ull * 1024ull;
            if (wait_for_global_delta("shared-open", global_before, expected)) {
                puts("FAIL: opening the shared tracker changed the adapter's one global charge");
                failed = 1;
            }
        }
        // Close the creator-side handles first. The opened references must keep
        // the same global allocations charged until their own cleanup below.
        evict_and_destroy(allocations, resources, created);
        memset(allocations, 0, count * sizeof(*allocations));
        memset(resources, 0, count * sizeof(*resources));
        if (!failed) {
            UINT64 expected = (UINT64)created * mib * 1024ull * 1024ull;
            if (wait_for_global_delta("owner-closed", global_before, expected)) {
                puts("FAIL: shared tracker did not survive its creator handle");
                failed = 1;
            }
        }
        evict_and_destroy(opened_allocations, opened_resources, opened);
        memset(opened_allocations, 0, count * sizeof(*opened_allocations));
        memset(opened_resources, 0, count * sizeof(*opened_resources));
        created = 0;
    }

cleanup:
    evict_and_destroy(allocations, resources, created);
    if (g_adapter) {
        (void)query_budget("destroyed", NULL);
        if (wait_for_cleanup(raw_before, global_before, g_shared_tracking,
                             &raw_after)) {
            puts("FAIL: process/adapter usage did not return to baseline");
            failed = 1;
        }
    }
    shutdown_kmt();
    free(allocations);
    free(resources);
    free(global_shares);
    free(opened_allocations);
    free(opened_resources);
    puts(failed ? "VIDMM TRACKING PROBE: FAIL" : "VIDMM TRACKING PROBE: PASS");
    return failed ? 1 : 0;
}
