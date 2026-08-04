// Prove Helios' VidMm-only tracking allocation contract without involving Mesa.
//
// The probe creates one or more HELIOS_WDDM_ALLOC_KIND_TRACKING allocations,
// makes them resident, and prints the current-process local-memory usage before
// and after. The KMD creates a one-page identity resource for each object; the
// full-size bytes remain owned by Venus on the host.
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

static int query_local(const char *label, UINT64 *usage_out) {
    D3DKMT_QUERYVIDEOMEMORYINFO query;
    NTSTATUS status;

    memset(&query, 0, sizeof(query));
    query.hProcess = GetCurrentProcess();
    query.hAdapter = g_adapter;
    query.MemorySegmentGroup = D3DKMT_MEMORY_SEGMENT_GROUP_LOCAL;
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

static UINT64 query_raw_statistics(const char *label) {
    D3DKMT_QUERYSTATISTICS query;
    NTSTATUS status;
    ULONG segment_id;
    UINT64 process_local_usage = 0;

    for (segment_id = 1; segment_id <= 2; ++segment_id) {
        memset(&query, 0, sizeof(query));
        query.Type = D3DKMT_QUERYSTATISTICS_SEGMENT;
        query.AdapterLuid = g_adapter_luid;
        query.QuerySegment.SegmentId = segment_id;
        status = D3DKMTQueryStatistics(&query);
        if (status == 0) {
            const D3DKMT_QUERYSTATISTICS_SEGMENT_INFORMATION *segment =
                &query.QueryResult.SegmentInformation;
            printf("%-16s raw seg%lu committed=%10.2f MiB resident=%10.2f MiB "
                   "allocs=%lu/%lu\n",
                   label, segment_id,
                   (double)segment->BytesCommitted / (1024.0 * 1024.0),
                   (double)segment->BytesResident / (1024.0 * 1024.0),
                   (unsigned long)segment->Memory.AllocsCommitted,
                   (unsigned long)segment->Memory.AllocsResident);
        } else {
            printf("%-16s raw seg%lu query failed: status=0x%08x\n", label,
                   segment_id, (unsigned)status);
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
        } else {
            printf("%-16s process seg%lu query failed: status=0x%08x\n", label,
                   segment_id, (unsigned)status);
        }
    }

    memset(&query, 0, sizeof(query));
    query.Type = D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT_GROUP;
    query.AdapterLuid = g_adapter_luid;
    query.hProcess = GetCurrentProcess();
    query.QueryProcessSegmentGroup = D3DKMT_MEMORY_SEGMENT_GROUP_LOCAL;
    status = D3DKMTQueryStatistics(&query);
    if (status == 0) {
        const D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT_GROUP_INFORMATION *group =
            &query.QueryResult.ProcessSegmentGroupInformation;
        printf("%-16s process local requested=%10.2f MiB usage=%10.2f MiB\n",
               label, (double)group->Requested / (1024.0 * 1024.0),
               (double)group->Usage / (1024.0 * 1024.0));
        process_local_usage = group->Usage;
    } else {
        printf("%-16s process group query failed: status=0x%08x\n", label,
               (unsigned)status);
    }
    return process_local_usage;
}

static int create_tracking(UINT64 size, D3DKMT_HANDLE *allocation_out,
                           D3DKMT_HANDLE *resource_out) {
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
    printf("  tracking hResource=0x%x hAllocation=0x%x gpuva=0x%llx\n",
           (unsigned)create.hResource,
           (unsigned)allocation_info.hAllocation,
           (unsigned long long)allocation_info.GpuVirtualAddress);
    return 0;
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

int main(int argc, char **argv) {
    UINT count = 4;
    UINT64 mib = 64;
    D3DKMT_HANDLE *allocations = NULL;
    D3DKMT_HANDLE *resources = NULL;
    UINT created = 0;
    UINT64 reported_before = 0;
    UINT64 reported_resident = 0;
    UINT64 reported_after = 0;
    UINT64 raw_before = 0;
    UINT64 raw_resident = 0;
    UINT64 raw_after = 0;
    int failed = 0;

    if (argc > 1) {
        count = (UINT)strtoul(argv[1], NULL, 0);
    }
    if (argc > 2) {
        mib = _strtoui64(argv[2], NULL, 0);
    }
    if (argc > 3 && strcmp(argv[3], "backed") == 0) {
        g_backed_control = 1;
    }
    if (count == 0 || count > 1024 || mib == 0 || mib > 4096) {
        fprintf(stderr, "usage: %s [allocation-count 1..1024] [MiB 1..4096]\n",
                argv[0]);
        return 2;
    }

    allocations = (D3DKMT_HANDLE *)calloc(count, sizeof(*allocations));
    resources = (D3DKMT_HANDLE *)calloc(count, sizeof(*resources));
    if (!allocations || !resources) {
        puts("out of memory allocating handle list");
        return 1;
    }
    if (open_helios()) {
        failed = 1;
        goto cleanup;
    }
    if (query_local("baseline", &reported_before)) {
        failed = 1;
        goto cleanup;
    }
    raw_before = query_raw_statistics("baseline");

    for (created = 0; created < count; ++created) {
        if (create_tracking(mib * 1024ull * 1024ull, &allocations[created],
                            &resources[created])) {
            failed = 1;
            break;
        }
    }
    printf("created %u tracking allocations of %llu MiB\n", created,
           (unsigned long long)mib);
    (void)query_local("created", NULL);
    (void)query_raw_statistics("created");
    if (!failed && make_resident(allocations, created)) {
        failed = 1;
    }
    if (!failed && query_local("resident", &reported_resident)) {
        failed = 1;
    }
    raw_resident = query_raw_statistics("resident");
    if (!failed) {
        UINT64 expected = (UINT64)created * mib * 1024ull * 1024ull;
        UINT64 delta = raw_resident >= raw_before ? raw_resident - raw_before : 0;
        printf("resident delta:   %.2f MiB (expected at least %.2f MiB)\n",
               (double)delta / (1024.0 * 1024.0),
               (double)expected / (1024.0 * 1024.0));
        if (delta < expected) {
            puts("FAIL: raw process-local usage did not include every tracking allocation");
            failed = 1;
        }
    }

cleanup:
    evict_and_destroy(allocations, resources, created);
    if (g_adapter) {
        Sleep(50);
        (void)query_local("destroyed", &reported_after);
        raw_after = query_raw_statistics("destroyed");
        if (raw_after > raw_before + 1024ull * 1024ull) {
            puts("FAIL: raw process-local usage did not return to baseline");
            failed = 1;
        }
    }
    shutdown_kmt();
    free(allocations);
    free(resources);
    puts(failed ? "VIDMM TRACKING PROBE: FAIL" : "VIDMM TRACKING PROBE: PASS");
    return failed ? 1 : 0;
}
