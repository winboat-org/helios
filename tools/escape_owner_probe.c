// escape_owner_probe.c — the T1b gate instrument for the escape trust boundary.
//
// Exercises, in order:
//   1. QUERY_STATS dump (v1 + v2) — the "counters byte-identical across a
//      session" half of the gate, and the context_full_drops reading R312's
//      untracked-context policy depends on.
//   2. A bad-magic escape        -> must be refused (ESCAPE_BAD_HEADER).
//   3. An unknown verb (0x0007)  -> must be refused (ESCAPE_UNKNOWN_VERB).
//   4. RELEASE_BLOB with hDevice = NULL against the LIVE DWM PRIMARY's resource
//      id (read from QUERY_SCANOUT) -> must be refused (EscNoDev), with the
//      primary still live afterwards and DWM still composing.
//   5. CTX_DESTROY for a context owned by ANOTHER device -> must be refused
//      (EscCtxOwn), with the victim context still usable.
//
// ⚠ TESTS 4 AND 5 ARE DESTRUCTIVE ON A PRE-T1b KMD. Before 22.22.180.0, owner 0
// matched every blob the KMD had adopted for a WDDM allocation, so test 4 would
// unmap+unref the DWM primary behind the live allocation's back (host "invalid
// res_id" -> CS error -> DWM kill). Run them only against a KMD that carries
// R311/R312; that is the point of the test.
//
// Build (WinLibs g++ on win11 — no clang-cl on the box):
//   g++ -O2 -o C:\Users\Rupansh\helios-probe\escape_owner_probe.exe ^
//       Z:\tools\escape_owner_probe.c -I"Z:\icd\win-build\wdk-include" -lgdi32
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
#define HELIOS_ESCAPE_PRESENT_BLOB 0x0007u /* defined in protocol, never dispatched */
#define HELIOS_ESCAPE_RELEASE_BLOB 0x0008u
#define HELIOS_ESCAPE_QUERY_STATS 0x000Au
#define HELIOS_ESCAPE_QUERY_SCANOUT 0x000Bu
#define VIRTIO_GPU_CAPSET_VENUS 4u

/* winnt.h defines these as DWORD; we compare against NTSTATUS. */
#undef STATUS_INVALID_PARAMETER
#undef STATUS_NOT_IMPLEMENTED
#undef STATUS_INVALID_DEVICE_REQUEST
#define STATUS_INVALID_PARAMETER ((NTSTATUS)0xC000000DL)
#define STATUS_NOT_IMPLEMENTED ((NTSTATUS)0xC0000002L)
#define STATUS_INVALID_DEVICE_REQUEST ((NTSTATUS)0xC0000010L)

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
struct helios_escape_release_blob {
    struct helios_escape_header hdr;
    UINT ctx_id, resource_id, flags, padding;
};
/* protocol/src/escape.rs HeliosEscapeQueryStats (v1, 88 bytes) */
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
/* HeliosEscapeQueryScanout */
struct helios_escape_query_scanout {
    struct helios_escape_header hdr;
    UINT64 out_alloc_size;
    UINT out_resource_id, out_width, out_height, out_dxgi_format;
    UINT out_pitch, out_plane_offset, out_memory_type_index, out_generation;
    UINT reserved[2];
};

static D3DKMT_HANDLE g_adapter, g_device_a, g_device_b;

static NTSTATUS escape_on(D3DKMT_HANDLE device, void* buf, UINT size) {
    D3DKMT_ESCAPE esc;
    memset(&esc, 0, sizeof(esc));
    esc.hAdapter = g_adapter;
    esc.hDevice = device; /* 0 = the forgeable owner value this probe tests */
    esc.Type = D3DKMT_ESCAPE_DRIVERPRIVATE;
    esc.pPrivateDriverData = buf;
    esc.PrivateDriverDataSize = size;
    return D3DKMTEscape(&esc);
}

static void hdr_init(struct helios_escape_header* h, UINT verb, UINT size) {
    h->magic = HELIOS_ESCAPE_MAGIC;
    h->cmd_type = verb;
    h->version = HELIOS_ESCAPE_VERSION;
    h->size = size;
}

/* Find Helios the way the ICD does: only its KMD answers the CTX_CREATE escape.
   Leaves two devices open on the adapter (A = victim, B = attacker). */
static int open_helios(UINT* out_ctx_a) {
    D3DKMT_ENUMADAPTERS2 ea;
    memset(&ea, 0, sizeof(ea));
    if (D3DKMTEnumAdapters2(&ea) != 0 || ea.NumAdapters == 0) {
        printf("EnumAdapters2 failed\n");
        return 1;
    }
    ea.pAdapters = (D3DKMT_ADAPTERINFO*)calloc(ea.NumAdapters, sizeof(D3DKMT_ADAPTERINFO));
    if (D3DKMTEnumAdapters2(&ea) != 0) {
        printf("EnumAdapters2(2) failed\n");
        return 1;
    }
    for (UINT i = 0; i < ea.NumAdapters; i++) {
        D3DKMT_HANDLE h = ea.pAdapters[i].hAdapter;
        D3DKMT_CREATEDEVICE cd;
        memset(&cd, 0, sizeof(cd));
        cd.hAdapter = h;
        if (D3DKMTCreateDevice(&cd) != 0) {
            D3DKMT_CLOSEADAPTER ca;
            memset(&ca, 0, sizeof(ca));
            ca.hAdapter = h;
            D3DKMTCloseAdapter(&ca);
            continue;
        }
        g_adapter = h;
        g_device_a = cd.hDevice;

        struct helios_escape_ctx_create cc;
        memset(&cc, 0, sizeof(cc));
        hdr_init(&cc.hdr, HELIOS_ESCAPE_CTX_CREATE, sizeof(cc));
        cc.capset_id = VIRTIO_GPU_CAPSET_VENUS;
        NTSTATUS est = escape_on(g_device_a, &cc, sizeof(cc));
        if (est == 0 && cc.out_ctx_id != 0) {
            *out_ctx_a = cc.out_ctx_id;
            D3DKMT_CREATEDEVICE cd2;
            memset(&cd2, 0, sizeof(cd2));
            cd2.hAdapter = h;
            if (D3DKMTCreateDevice(&cd2) == 0) {
                g_device_b = cd2.hDevice;
            }
            printf("helios adapter luid=%08x:%08x deviceA=%#x deviceB=%#x ctxA=%u\n",
                   (unsigned)ea.pAdapters[i].AdapterLuid.HighPart,
                   (unsigned)ea.pAdapters[i].AdapterLuid.LowPart, (unsigned)g_device_a,
                   (unsigned)g_device_b, cc.out_ctx_id);
            free(ea.pAdapters);
            return 0;
        }
        D3DKMT_DESTROYDEVICE dd;
        memset(&dd, 0, sizeof(dd));
        dd.hDevice = cd.hDevice;
        D3DKMTDestroyDevice(&dd);
        D3DKMT_CLOSEADAPTER ca;
        memset(&ca, 0, sizeof(ca));
        ca.hAdapter = h;
        D3DKMTCloseAdapter(&ca);
        g_adapter = 0;
        g_device_a = 0;
    }
    free(ea.pAdapters);
    printf("no adapter answered the Helios CTX_CREATE escape\n");
    return 1;
}

static int dump_stats(const char* label, struct helios_escape_query_stats* out) {
    struct helios_escape_query_stats qs;
    memset(&qs, 0, sizeof(qs));
    hdr_init(&qs.hdr, HELIOS_ESCAPE_QUERY_STATS, sizeof(qs));
    NTSTATUS st = escape_on(g_device_a, &qs, sizeof(qs));
    if (st != 0) {
        printf("[%s] QUERY_STATS st=0x%08x\n", label, (unsigned)st);
        return 1;
    }
    printf("[%s] blobs=%u/%u hw=%u rej=%u | resources=%u/%u hw=%u rej=%u | "
           "contexts=%u drops=%u | window=%llu/%llu rangedrops=%u | ctrl_timeouts=%u "
           "take_live_misses=%u adopt_dead=%u\n",
           label, qs.out_blobs_live, qs.out_blobs_cap, qs.out_blobs_high_water,
           qs.out_blob_full_rejects, qs.out_resources_live, qs.out_resources_cap,
           qs.out_resources_high_water, qs.out_resource_full_rejects, qs.out_contexts_live,
           qs.out_context_full_drops, (unsigned long long)qs.out_window_used,
           (unsigned long long)qs.out_window_len, qs.out_window_range_drops, qs.out_ctrl_timeouts,
           qs.out_take_live_misses, qs.out_adopt_dead_rejects);
    if (out) {
        *out = qs;
    }
    return 0;
}

static int query_scanout(struct helios_escape_query_scanout* qs) {
    memset(qs, 0, sizeof(*qs));
    hdr_init(&qs->hdr, HELIOS_ESCAPE_QUERY_SCANOUT, sizeof(*qs));
    NTSTATUS st = escape_on(g_device_a, qs, sizeof(*qs));
    printf("QUERY_SCANOUT st=0x%08x resid=%u %ux%u pitch=%u gen=%u\n", (unsigned)st,
           qs->out_resource_id, qs->out_width, qs->out_height, qs->out_pitch, qs->out_generation);
    return st != 0;
}

int main(int argc, char** argv) {
    int destructive = (argc > 1 && strcmp(argv[1], "--attack") == 0);
    UINT ctx_a = 0;
    if (open_helios(&ctx_a) != 0) {
        return 1;
    }

    struct helios_escape_query_stats before;
    dump_stats("start", &before);

    /* --- 2. bad magic --- */
    struct helios_escape_ctx_destroy bad;
    memset(&bad, 0, sizeof(bad));
    hdr_init(&bad.hdr, HELIOS_ESCAPE_CTX_DESTROY, sizeof(bad));
    bad.hdr.magic = 0xDEADBEEFu;
    bad.ctx_id = ctx_a;
    NTSTATUS st = escape_on(g_device_a, &bad, sizeof(bad));
    printf("bad-magic escape       st=0x%08x %s\n", (unsigned)st,
           st == STATUS_INVALID_PARAMETER ? "(PASS: refused)" : "(FAIL: expected INVALID_PARAMETER)");

    /* --- 3. unknown verb --- */
    struct helios_escape_header unk;
    hdr_init(&unk, HELIOS_ESCAPE_PRESENT_BLOB, sizeof(unk));
    st = escape_on(g_device_a, &unk, sizeof(unk));
    printf("unknown verb 0x0007    st=0x%08x %s\n", (unsigned)st,
           st == STATUS_NOT_IMPLEMENTED ? "(PASS: refused)" : "(FAIL: expected NOT_IMPLEMENTED)");

    if (!destructive) {
        printf("\nSkipping the ownership attacks (pass --attack to run them; they are\n"
               "DESTRUCTIVE against a pre-22.22.180.0 KMD).\n");
    } else {
        /* --- 4. owner==0 RELEASE_BLOB against the live DWM primary --- */
        struct helios_escape_query_scanout sc;
        if (query_scanout(&sc) == 0 && sc.out_resource_id != 0) {
            struct helios_escape_release_blob rb;
            memset(&rb, 0, sizeof(rb));
            hdr_init(&rb.hdr, HELIOS_ESCAPE_RELEASE_BLOB, sizeof(rb));
            rb.ctx_id = ctx_a; /* nonzero: the KMD rejects ctx_id 0 outright */
            rb.resource_id = sc.out_resource_id;
            st = escape_on(0 /* hDevice = NULL: the forged owner */, &rb, sizeof(rb));
            printf("owner=0 RELEASE_BLOB(primary resid=%u) st=0x%08x %s\n", sc.out_resource_id,
                   (unsigned)st,
                   st == STATUS_INVALID_PARAMETER ? "(PASS: refused)"
                                                  : "(FAIL: the KMD accepted a forged owner)");
            struct helios_escape_query_scanout after_sc;
            if (query_scanout(&after_sc) == 0) {
                printf("primary after attack   resid=%u %s\n", after_sc.out_resource_id,
                       after_sc.out_resource_id == sc.out_resource_id
                           ? "(PASS: unchanged)"
                           : "(FAIL: the primary was destroyed)");
            }
        } else {
            printf("no live primary published; skipping the owner=0 attack\n");
        }

        /* --- 5. cross-device CTX_DESTROY --- */
        if (g_device_b != 0) {
            struct helios_escape_ctx_destroy cd;
            memset(&cd, 0, sizeof(cd));
            hdr_init(&cd.hdr, HELIOS_ESCAPE_CTX_DESTROY, sizeof(cd));
            cd.ctx_id = ctx_a; /* device A's context, destroyed from device B */
            st = escape_on(g_device_b, &cd, sizeof(cd));
            printf("cross-device CTX_DESTROY(ctx=%u) st=0x%08x %s\n", ctx_a, (unsigned)st,
                   st == STATUS_INVALID_DEVICE_REQUEST
                       ? "(PASS: refused)"
                       : "(FAIL: one device destroyed another's context)");
        }
    }

    struct helios_escape_query_stats after;
    dump_stats("end", &after);
    printf("\ndelta: blobs_live %+d  contexts_live %+d  context_full_drops %+d\n",
           (int)after.out_blobs_live - (int)before.out_blobs_live,
           (int)after.out_contexts_live - (int)before.out_contexts_live,
           (int)after.out_context_full_drops - (int)before.out_context_full_drops);

    /* Clean up our own context so the probe stays state-neutral. */
    struct helios_escape_ctx_destroy cd;
    memset(&cd, 0, sizeof(cd));
    hdr_init(&cd.hdr, HELIOS_ESCAPE_CTX_DESTROY, sizeof(cd));
    cd.ctx_id = ctx_a;
    st = escape_on(g_device_a, &cd, sizeof(cd));
    printf("own CTX_DESTROY        st=0x%08x %s\n", (unsigned)st,
           st == 0 ? "(PASS: owner keeps its rights)" : "(FAIL: owner-scoping is too strict)");

    if (g_device_b) {
        D3DKMT_DESTROYDEVICE dd;
        memset(&dd, 0, sizeof(dd));
        dd.hDevice = g_device_b;
        D3DKMTDestroyDevice(&dd);
    }
    D3DKMT_DESTROYDEVICE dd;
    memset(&dd, 0, sizeof(dd));
    dd.hDevice = g_device_a;
    D3DKMTDestroyDevice(&dd);
    D3DKMT_CLOSEADAPTER ca;
    memset(&ca, 0, sizeof(ca));
    ca.hAdapter = g_adapter;
    D3DKMTCloseAdapter(&ca);
    return 0;
}
