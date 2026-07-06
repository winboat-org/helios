// blob_map_size_probe.c — find the size at which HELIOS_ESCAPE_MAP_BLOB starts
// failing. Diagnostic for the 2026-07-06 Doom level-load fatal ("Cannot map
// buffer with usage BU_STATIC", MAP_BLOB → STATUS_INSUFFICIENT_RESOURCES one
// second before the crash): the KMD's map path builds ONE MDL per blob
// (IoAllocateMdl + MmMapLockedPagesSpecifyCache), and an MDL's Size member is
// a CSHORT — a single MDL cannot describe more than ((32767 - sizeof(MDL)) /
// sizeof(PFN_NUMBER)) pages (~16 MiB on x64), while the KMD accepts maps up
// to MAX_BLOB_MAP_BYTES = 256 MiB. Every mappable blob in between would fail
// with 0xC000009A.
//
// For each size in the sweep: ALLOC_BLOB(HOST3D, mappable, blob_id=0) →
// MAP_BLOB → report NTSTATUS + user VA (touch first/last byte on success) →
// RELEASE_BLOB. State-neutral.
//
// Build (win11, WinLibs gcc):
//   gcc -O2 -o blob_map_size_probe.exe blob_map_size_probe.c \
//       -I Z:\icd\win-build\wdk-include -lgdi32
#include <windows.h>
#include <stdio.h>

#ifndef _NTDEF_
typedef LONG NTSTATUS, *PNTSTATUS;
#endif
#include <d3dkmthk.h>

#define HELIOS_ESCAPE_MAGIC 0x48454C53u /* 'HELS' */
#define HELIOS_ESCAPE_VERSION 1u
#define HELIOS_ESCAPE_CTX_CREATE 0x0002u
#define HELIOS_ESCAPE_CTX_DESTROY 0x0003u
#define HELIOS_ESCAPE_ALLOC_BLOB 0x0004u
#define HELIOS_ESCAPE_MAP_BLOB 0x0005u
#define HELIOS_ESCAPE_RELEASE_BLOB 0x0008u
#define VIRTIO_GPU_CAPSET_VENUS 4u
#define VIRTIO_GPU_BLOB_MEM_HOST3D 2u
#define VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE 1u

struct helios_escape_header {
    UINT magic, cmd_type, version, size;
};
struct helios_escape_ctx_create {
    struct helios_escape_header hdr;
    UINT capset_id;
    UINT out_ctx_id;
};
struct helios_escape_ctx_destroy {
    struct helios_escape_header hdr;
    UINT ctx_id, padding;
};
struct helios_escape_alloc_blob {
    struct helios_escape_header hdr;
    UINT64 size;
    UINT64 blob_id;
    UINT blob_flags;
    UINT blob_mem;
    UINT ctx_id;
    UINT out_resource_id;
};
struct helios_escape_map_blob {
    struct helios_escape_header hdr;
    UINT64 out_user_va;
    UINT resource_id;
    UINT map_cache;
};
struct helios_escape_release_blob {
    struct helios_escape_header hdr;
    UINT ctx_id, resource_id, flags, padding;
};
/* mirror protocol/src/escape.rs HeliosEscapeQueryStatsV2 (152 bytes, KMD
 * 22.22.55+; older KMDs fill only the 88-byte v1 prefix) */
#define HELIOS_ESCAPE_QUERY_STATS 0x000Au
struct helios_escape_query_stats_v2 {
    struct helios_escape_header hdr;
    UINT64 out_window_used;
    UINT64 out_window_len;
    UINT out_blobs_live, out_blobs_cap, out_blobs_high_water, out_blob_full_rejects;
    UINT out_resources_live, out_resources_cap, out_resources_high_water, out_resource_full_rejects;
    UINT out_contexts_live, out_context_full_drops;
    UINT out_window_range_drops, out_ctrl_timeouts;
    UINT out_take_live_misses, out_adopt_dead_rejects;
    UINT out_fence_events_live, out_fence_events_high_water;
    UINT out_fence_event_registers, out_fence_event_signals;
    UINT out_fence_event_already_complete, out_fence_event_overflows;
    UINT out_fence_event_dup_rejects, out_fence_event_invalid;
    UINT out_fence_event_cancels, out_fence_event_teardown_drops;
    UINT out_mappings_live, out_mappings_cap, out_mappings_high_water;
    UINT out_mapping_full_rejects, out_map_pages_fails, out_window_alloc_rejects;
};

static D3DKMT_HANDLE g_adapter, g_device;
static NTSTATUS escape_st(void* buf, UINT size);

static void query_stats_v2(const char* label) {
    struct helios_escape_query_stats_v2 qs; memset(&qs, 0, sizeof(qs));
    qs.hdr.magic = HELIOS_ESCAPE_MAGIC; qs.hdr.cmd_type = HELIOS_ESCAPE_QUERY_STATS;
    qs.hdr.version = HELIOS_ESCAPE_VERSION; qs.hdr.size = sizeof(qs);
    NTSTATUS st = escape_st(&qs, sizeof(qs));
    if (st != 0) {
        printf("[%s] QUERY_STATS st=0x%08x\n", label, (unsigned)st);
        return;
    }
    printf("[%s] blobs=%u/%u window=%llu/%llu MiB | mappings=%u/%u hw=%u "
           "full-rejects=%u map-pages-fails=%u window-rejects=%u | "
           "fence-events live=%u hw=%u reg=%u sig=%u imm=%u ovf=%u dup=%u inv=%u can=%u tear=%u\n",
           label, qs.out_blobs_live, qs.out_blobs_cap,
           (unsigned long long)(qs.out_window_used >> 20),
           (unsigned long long)(qs.out_window_len >> 20),
           qs.out_mappings_live, qs.out_mappings_cap, qs.out_mappings_high_water,
           qs.out_mapping_full_rejects, qs.out_map_pages_fails, qs.out_window_alloc_rejects,
           qs.out_fence_events_live, qs.out_fence_events_high_water,
           qs.out_fence_event_registers, qs.out_fence_event_signals,
           qs.out_fence_event_already_complete, qs.out_fence_event_overflows,
           qs.out_fence_event_dup_rejects, qs.out_fence_event_invalid,
           qs.out_fence_event_cancels, qs.out_fence_event_teardown_drops);
}

static NTSTATUS escape_st(void* buf, UINT size) {
    D3DKMT_ESCAPE esc; memset(&esc, 0, sizeof(esc));
    esc.hAdapter = g_adapter;
    esc.hDevice = g_device;
    esc.Type = D3DKMT_ESCAPE_DRIVERPRIVATE;
    esc.pPrivateDriverData = buf;
    esc.PrivateDriverDataSize = size;
    return D3DKMTEscape(&esc);
}

static int open_helios(UINT* out_ctx) {
    D3DKMT_ENUMADAPTERS2 ea;
    memset(&ea, 0, sizeof(ea));
    NTSTATUS st = D3DKMTEnumAdapters2(&ea);
    if (st != 0 || ea.NumAdapters == 0) { printf("EnumAdapters2 st=0x%08x n=%u\n", (unsigned)st, ea.NumAdapters); return 1; }
    ea.pAdapters = (D3DKMT_ADAPTERINFO*)calloc(ea.NumAdapters, sizeof(D3DKMT_ADAPTERINFO));
    st = D3DKMTEnumAdapters2(&ea);
    if (st != 0) { printf("EnumAdapters2(2) st=0x%08x\n", (unsigned)st); return 1; }

    for (UINT i = 0; i < ea.NumAdapters; i++) {
        D3DKMT_HANDLE h = ea.pAdapters[i].hAdapter;
        D3DKMT_CREATEDEVICE cd; memset(&cd, 0, sizeof(cd)); cd.hAdapter = h;
        if (D3DKMTCreateDevice(&cd) != 0) {
            D3DKMT_CLOSEADAPTER ca; memset(&ca, 0, sizeof(ca)); ca.hAdapter = h; (void)D3DKMTCloseAdapter(&ca);
            continue;
        }
        g_adapter = h;
        g_device = cd.hDevice;

        struct helios_escape_ctx_create cc; memset(&cc, 0, sizeof(cc));
        cc.hdr.magic = HELIOS_ESCAPE_MAGIC; cc.hdr.cmd_type = HELIOS_ESCAPE_CTX_CREATE;
        cc.hdr.version = HELIOS_ESCAPE_VERSION; cc.hdr.size = sizeof(cc);
        cc.capset_id = VIRTIO_GPU_CAPSET_VENUS;
        NTSTATUS est = escape_st(&cc, sizeof(cc));
        if (est == 0 && cc.out_ctx_id != 0) {
            *out_ctx = cc.out_ctx_id;
            free(ea.pAdapters);
            return 0;
        }

        D3DKMT_DESTROYDEVICE dd; memset(&dd, 0, sizeof(dd)); dd.hDevice = cd.hDevice;
        (void)D3DKMTDestroyDevice(&dd);
        D3DKMT_CLOSEADAPTER ca; memset(&ca, 0, sizeof(ca)); ca.hAdapter = h; (void)D3DKMTCloseAdapter(&ca);
        g_adapter = 0; g_device = 0;
    }
    free(ea.pAdapters);
    printf("no adapter answered the Helios CTX_CREATE escape\n");
    return 1;
}

int main(void) {
    UINT ctx_id = 0;
    if (open_helios(&ctx_id)) return 1;
    printf("CTX_CREATE ok ctx_id=%u\n", ctx_id);
    query_stats_v2("before");

    static const UINT64 sizes_mb[] = { 1, 8, 15, 16, 17, 24, 31, 32, 33, 48, 64, 128, 256 };
    for (UINT i = 0; i < sizeof(sizes_mb) / sizeof(sizes_mb[0]); i++) {
        const UINT64 bytes = sizes_mb[i] << 20;

        struct helios_escape_alloc_blob ab; memset(&ab, 0, sizeof(ab));
        ab.hdr.magic = HELIOS_ESCAPE_MAGIC; ab.hdr.cmd_type = HELIOS_ESCAPE_ALLOC_BLOB;
        ab.hdr.version = HELIOS_ESCAPE_VERSION; ab.hdr.size = sizeof(ab);
        ab.size = bytes; ab.blob_id = 0; ab.ctx_id = ctx_id;
        ab.blob_mem = VIRTIO_GPU_BLOB_MEM_HOST3D;
        ab.blob_flags = VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
        NTSTATUS st = escape_st(&ab, sizeof(ab));
        if (st != 0 || ab.out_resource_id == 0) {
            printf("%4llu MiB: ALLOC_BLOB FAILED st=0x%08x\n",
                   (unsigned long long)sizes_mb[i], (unsigned)st);
            continue;
        }

        struct helios_escape_map_blob mb; memset(&mb, 0, sizeof(mb));
        mb.hdr.magic = HELIOS_ESCAPE_MAGIC; mb.hdr.cmd_type = HELIOS_ESCAPE_MAP_BLOB;
        mb.hdr.version = HELIOS_ESCAPE_VERSION; mb.hdr.size = sizeof(mb);
        mb.resource_id = ab.out_resource_id;
        st = escape_st(&mb, sizeof(mb));
        if (st == 0 && mb.out_user_va != 0) {
            volatile UCHAR* p = (volatile UCHAR*)(ULONG_PTR)mb.out_user_va;
            UCHAR first = p[0], last = p[bytes - 1];
            printf("%4llu MiB: MAP OK va=0x%llx cache=%u first=%02x last=%02x\n",
                   (unsigned long long)sizes_mb[i],
                   (unsigned long long)mb.out_user_va, mb.map_cache, first, last);
        } else {
            printf("%4llu MiB: MAP FAILED st=0x%08x\n",
                   (unsigned long long)sizes_mb[i], (unsigned)st);
        }

        struct helios_escape_release_blob rb; memset(&rb, 0, sizeof(rb));
        rb.hdr.magic = HELIOS_ESCAPE_MAGIC; rb.hdr.cmd_type = HELIOS_ESCAPE_RELEASE_BLOB;
        rb.hdr.version = HELIOS_ESCAPE_VERSION; rb.hdr.size = sizeof(rb);
        rb.ctx_id = ctx_id; rb.resource_id = ab.out_resource_id;
        st = escape_st(&rb, sizeof(rb));
        if (st != 0)
            printf("%4llu MiB: RELEASE FAILED st=0x%08x (leak!)\n",
                   (unsigned long long)sizes_mb[i], (unsigned)st);
    }

    /* Phase 2 — concurrent-mapping headroom: map-and-HOLD 4 MiB blobs until
     * MAP_BLOB refuses. The count of successful concurrent maps bounds the
     * free slots in the KMD's adapter-global user-mapping table
     * (MAX_MAPPINGS) / remaining window space; a level load bursts many
     * concurrent maps, so low headroom here = the Doom BU_STATIC fatal. */
    printf("-- concurrent map-and-hold (4 MiB each) --\n");
    enum { HOLD_MAX = 300 };
    static UINT held[HOLD_MAX];
    UINT held_n = 0;
    NTSTATUS first_map_fail = 0;
    for (UINT i = 0; i < HOLD_MAX; i++) {
        struct helios_escape_alloc_blob ab; memset(&ab, 0, sizeof(ab));
        ab.hdr.magic = HELIOS_ESCAPE_MAGIC; ab.hdr.cmd_type = HELIOS_ESCAPE_ALLOC_BLOB;
        ab.hdr.version = HELIOS_ESCAPE_VERSION; ab.hdr.size = sizeof(ab);
        ab.size = 4ull << 20; ab.blob_id = 0; ab.ctx_id = ctx_id;
        ab.blob_mem = VIRTIO_GPU_BLOB_MEM_HOST3D;
        ab.blob_flags = VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
        NTSTATUS st = escape_st(&ab, sizeof(ab));
        if (st != 0 || ab.out_resource_id == 0) {
            printf("held=%u: ALLOC_BLOB refused st=0x%08x\n", held_n, (unsigned)st);
            first_map_fail = st;
            break;
        }
        struct helios_escape_map_blob mb; memset(&mb, 0, sizeof(mb));
        mb.hdr.magic = HELIOS_ESCAPE_MAGIC; mb.hdr.cmd_type = HELIOS_ESCAPE_MAP_BLOB;
        mb.hdr.version = HELIOS_ESCAPE_VERSION; mb.hdr.size = sizeof(mb);
        mb.resource_id = ab.out_resource_id;
        st = escape_st(&mb, sizeof(mb));
        if (st != 0 || mb.out_user_va == 0) {
            printf("held=%u: MAP_BLOB refused st=0x%08x\n", held_n, (unsigned)st);
            first_map_fail = st;
            /* release the unmapped blob before stopping */
            struct helios_escape_release_blob rb; memset(&rb, 0, sizeof(rb));
            rb.hdr.magic = HELIOS_ESCAPE_MAGIC; rb.hdr.cmd_type = HELIOS_ESCAPE_RELEASE_BLOB;
            rb.hdr.version = HELIOS_ESCAPE_VERSION; rb.hdr.size = sizeof(rb);
            rb.ctx_id = ctx_id; rb.resource_id = ab.out_resource_id;
            (void)escape_st(&rb, sizeof(rb));
            break;
        }
        held[held_n++] = ab.out_resource_id;
    }
    if (first_map_fail == 0)
        printf("held=%u: probe cap reached without refusal\n", held_n);
    for (UINT i = 0; i < held_n; i++) {
        struct helios_escape_release_blob rb; memset(&rb, 0, sizeof(rb));
        rb.hdr.magic = HELIOS_ESCAPE_MAGIC; rb.hdr.cmd_type = HELIOS_ESCAPE_RELEASE_BLOB;
        rb.hdr.version = HELIOS_ESCAPE_VERSION; rb.hdr.size = sizeof(rb);
        rb.ctx_id = ctx_id; rb.resource_id = held[i];
        NTSTATUS st = escape_st(&rb, sizeof(rb));
        if (st != 0)
            printf("release[%u] FAILED st=0x%08x (leak!)\n", i, (unsigned)st);
    }
    printf("released %u held blobs\n", held_n);
    query_stats_v2("after");

    struct helios_escape_ctx_destroy cds; memset(&cds, 0, sizeof(cds));
    cds.hdr.magic = HELIOS_ESCAPE_MAGIC; cds.hdr.cmd_type = HELIOS_ESCAPE_CTX_DESTROY;
    cds.hdr.version = HELIOS_ESCAPE_VERSION; cds.hdr.size = sizeof(cds);
    cds.ctx_id = ctx_id;
    (void)escape_st(&cds, sizeof(cds));
    return 0;
}
