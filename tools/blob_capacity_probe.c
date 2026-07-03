// blob_capacity_probe.c — measure FREE capacity in the KMD's bounded blob table
// (MAX_BLOBS) by ALLOC_BLOBing 4 KiB HOST3D mappable blobs until the escape
// fails, then releasing them all. Diagnostic for the 2026-07-03 "every new
// venus ring/shmem/export alloc fails guest-side" exhaustion event.
//
// Prints: number of successful allocs (= free slots at start, minus nothing —
// the probe's own ctx/ring do not consume blob slots), and the first failing
// NTSTATUS. Releases every blob it created (verify count), so the probe is
// state-neutral even on early exit paths short of a crash.
//
// Build (on win11, vcvars64):
//   cl /EHsc /W3 blob_capacity_probe.c /I"Z:\icd\win-build\wdk-include" /link gdi32.lib
#include <windows.h>
#include <stdio.h>

#ifndef _NTDEF_
typedef LONG NTSTATUS, *PNTSTATUS;
#endif
#include <d3dkmthk.h>

#define HELIOS_ESCAPE_MAGIC 0x48454C53u /* 'HELS' */
#define HELIOS_ESCAPE_VERSION 1u
#define HELIOS_ESCAPE_CTX_CREATE 0x0002u
#define HELIOS_ESCAPE_ALLOC_BLOB 0x0004u
#define HELIOS_ESCAPE_RELEASE_BLOB 0x0008u
#define HELIOS_ESCAPE_CTX_DESTROY 0x0003u
#define HELIOS_ESCAPE_QUERY_STATS 0x000Au
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
struct helios_escape_release_blob {
    struct helios_escape_header hdr;
    UINT ctx_id, resource_id, flags, padding;
};
/* mirror protocol/src/escape.rs HeliosEscapeQueryStats (88 bytes) */
struct helios_escape_query_stats {
    struct helios_escape_header hdr;
    UINT64 out_window_used;
    UINT64 out_window_len;
    UINT out_blobs_live, out_blobs_cap, out_blobs_high_water, out_blob_full_rejects;
    UINT out_resources_live, out_resources_cap, out_resources_high_water, out_resource_full_rejects;
    UINT out_contexts_live, out_context_full_drops;
    UINT out_window_range_drops, out_ctrl_timeouts;
    UINT out_take_live_misses, out_adopt_dead_rejects;
};

static D3DKMT_HANDLE g_adapter, g_device, g_context;

// Returns the raw NTSTATUS of D3DKMTEscape (0 = success).
static NTSTATUS escape_st(void* buf, UINT size) {
    D3DKMT_ESCAPE esc; memset(&esc, 0, sizeof(esc));
    esc.hAdapter = g_adapter;
    esc.hDevice = g_device;
    esc.Type = D3DKMT_ESCAPE_DRIVERPRIVATE;
    esc.pPrivateDriverData = buf;
    esc.PrivateDriverDataSize = size;
    return D3DKMTEscape(&esc);
}

// Find Helios the way the ICD does: try the versioned CTX_CREATE escape on each
// adapter; only the Helios KMD answers it. (KMTQAITYPE_ADAPTERREGISTRYINFO
// returns STATUS_OBJECT_NAME_NOT_FOUND for every adapter on this build, so the
// old name-string discovery is dead.) On success, leaves the created venus
// context id in *out_ctx.
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
            D3DKMT_CLOSEADAPTER ca; memset(&ca, 0, sizeof(ca)); ca.hAdapter = h; D3DKMTCloseAdapter(&ca);
            continue;
        }
        g_adapter = h;
        g_device = cd.hDevice;

        struct helios_escape_ctx_create cc; memset(&cc, 0, sizeof(cc));
        cc.hdr.magic = HELIOS_ESCAPE_MAGIC; cc.hdr.cmd_type = HELIOS_ESCAPE_CTX_CREATE;
        cc.hdr.version = HELIOS_ESCAPE_VERSION; cc.hdr.size = sizeof(cc);
        cc.capset_id = VIRTIO_GPU_CAPSET_VENUS;
        NTSTATUS est = escape_st(&cc, sizeof(cc));
        printf("adapter[%u] luid=%08x:%08x ctx-probe st=0x%08x\n",
               i, (unsigned)ea.pAdapters[i].AdapterLuid.HighPart,
               (unsigned)ea.pAdapters[i].AdapterLuid.LowPart, (unsigned)est);
        if (est == 0 && cc.out_ctx_id != 0) {
            *out_ctx = cc.out_ctx_id;
            free(ea.pAdapters);
            return 0;
        }

        D3DKMT_DESTROYDEVICE dd; memset(&dd, 0, sizeof(dd)); dd.hDevice = cd.hDevice;
        D3DKMTDestroyDevice(&dd);
        D3DKMT_CLOSEADAPTER ca; memset(&ca, 0, sizeof(ca)); ca.hAdapter = h; D3DKMTCloseAdapter(&ca);
        g_adapter = 0; g_device = 0;
    }
    free(ea.pAdapters);
    printf("no adapter answered the Helios CTX_CREATE escape\n");
    return 1;
}

#define MAX_PROBE 512

static void query_stats(const char* label) {
    struct helios_escape_query_stats qs; memset(&qs, 0, sizeof(qs));
    qs.hdr.magic = HELIOS_ESCAPE_MAGIC; qs.hdr.cmd_type = HELIOS_ESCAPE_QUERY_STATS;
    qs.hdr.version = HELIOS_ESCAPE_VERSION; qs.hdr.size = sizeof(qs);
    NTSTATUS st = escape_st(&qs, sizeof(qs));
    if (st != 0) {
        printf("[%s] QUERY_STATS st=0x%08x (old KMD without the verb?)\n", label, (unsigned)st);
        return;
    }
    printf("[%s] blobs=%u/%u hw=%u rejects=%u | resources=%u/%u hw=%u rejects=%u | "
           "ctx=%u drops=%u | window=%llu/%llu MiB range-drops=%u | "
           "ctrl-timeouts=%u take-misses=%u adopt-dead=%u\n",
           label,
           qs.out_blobs_live, qs.out_blobs_cap, qs.out_blobs_high_water, qs.out_blob_full_rejects,
           qs.out_resources_live, qs.out_resources_cap, qs.out_resources_high_water,
           qs.out_resource_full_rejects,
           qs.out_contexts_live, qs.out_context_full_drops,
           (unsigned long long)(qs.out_window_used >> 20),
           (unsigned long long)(qs.out_window_len >> 20),
           qs.out_window_range_drops,
           qs.out_ctrl_timeouts, qs.out_take_live_misses, qs.out_adopt_dead_rejects);
}

int main(void) {
    UINT ctx_id = 0;
    NTSTATUS st;
    if (open_helios(&ctx_id)) return 1;
    printf("CTX_CREATE ok ctx_id=%u\n", ctx_id);
    query_stats("before");

    static UINT rids[MAX_PROBE];
    UINT got = 0;
    NTSTATUS first_fail = 0;
    for (UINT i = 0; i < MAX_PROBE; i++) {
        struct helios_escape_alloc_blob ab; memset(&ab, 0, sizeof(ab));
        ab.hdr.magic = HELIOS_ESCAPE_MAGIC; ab.hdr.cmd_type = HELIOS_ESCAPE_ALLOC_BLOB;
        ab.hdr.version = HELIOS_ESCAPE_VERSION; ab.hdr.size = sizeof(ab);
        ab.size = 4096; ab.blob_id = 0; ab.ctx_id = ctx_id;
        ab.blob_mem = VIRTIO_GPU_BLOB_MEM_HOST3D;
        ab.blob_flags = VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
        st = escape_st(&ab, sizeof(ab));
        if (st != 0 || ab.out_resource_id == 0) {
            first_fail = st;
            break;
        }
        rids[got++] = ab.out_resource_id;
    }
    printf("FREE BLOB SLOTS: %u (first failing NTSTATUS=0x%08x%s)\n",
           got, (unsigned)first_fail,
           got == MAX_PROBE ? ", probe cap reached — table not full" : "");

    UINT released = 0;
    for (UINT i = 0; i < got; i++) {
        struct helios_escape_release_blob rb; memset(&rb, 0, sizeof(rb));
        rb.hdr.magic = HELIOS_ESCAPE_MAGIC; rb.hdr.cmd_type = HELIOS_ESCAPE_RELEASE_BLOB;
        rb.hdr.version = HELIOS_ESCAPE_VERSION; rb.hdr.size = sizeof(rb);
        rb.ctx_id = ctx_id; rb.resource_id = rids[i];
        if (escape_st(&rb, sizeof(rb)) == 0) released++;
    }
    printf("released %u/%u\n", released, got);
    query_stats("after");

    struct helios_escape_ctx_destroy cd; memset(&cd, 0, sizeof(cd));
    cd.hdr.magic = HELIOS_ESCAPE_MAGIC; cd.hdr.cmd_type = HELIOS_ESCAPE_CTX_DESTROY;
    cd.hdr.version = HELIOS_ESCAPE_VERSION; cd.hdr.size = sizeof(cd);
    cd.ctx_id = ctx_id;
    escape_st(&cd, sizeof(cd));
    return 0;
}
