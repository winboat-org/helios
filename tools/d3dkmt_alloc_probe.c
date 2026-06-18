// Helios Gate 5a Stage 2 (increment 2a) — D3DKMTCreateAllocation probe.
//
// Opens the Helios WDDM render adapter via D3DKMT (the same path the venus ICD
// uses), brings up a venus context over D3DKMTEscape, then creates a HOST3D
// mappable blob allocation via D3DKMTCreateAllocation carrying a
// HeliosWddmAllocPrivate (kind=SHMEM, blob_id=0). This exercises the KMD's real
// DxgkDdiCreateAllocation -> VirtioGpu::resource_create_blob and answers the
// open ".56 question": does a blob_id=0 HOST3D blob create succeed on the current
// host config, or come back RESP_ERR_UNSPEC? No Lock2/segment work needed.
//
// Build (on win11, vcvars64):
//   cl /EHsc /W3 d3dkmt_alloc_probe.c /I"Z:\icd\win-build\wdk-include" /link gdi32.lib
#include <windows.h>
#include <stdio.h>

#ifndef _NTDEF_
typedef LONG NTSTATUS, *PNTSTATUS;
#endif
#include <d3dkmthk.h>

// ── Helios escape structs (mirror protocol/src/escape.rs) ───────────────────
#define HELIOS_ESCAPE_MAGIC 0x48454C53u /* 'HELS' */
#define HELIOS_ESCAPE_VERSION 1u
#define HELIOS_ESCAPE_CTX_CREATE 0x0002u
#define HELIOS_ESCAPE_ALLOC_BLOB 0x0004u
#define HELIOS_ESCAPE_MAP_BLOB   0x0005u
#define HELIOS_ESCAPE_RELEASE_BLOB 0x0008u
#define VIRTIO_GPU_CAPSET_VENUS 4u

struct helios_escape_header {
    UINT magic, cmd_type, version, size;
};
struct helios_escape_ctx_create {
    struct helios_escape_header hdr;
    UINT capset_id;
    UINT out_ctx_id;
};
// mirror protocol/src/escape.rs (exact field order/sizes)
struct helios_escape_alloc_blob {
    struct helios_escape_header hdr;
    UINT64 size;             // in
    UINT64 blob_id;          // in (0 = scratch shmem)
    UINT blob_flags;         // in
    UINT blob_mem;           // in
    UINT ctx_id;             // in
    UINT out_resource_id;    // out
};
struct helios_escape_map_blob {
    struct helios_escape_header hdr;
    UINT64 out_user_va;      // out
    UINT resource_id;        // in
    UINT map_cache;          // in/out
};
struct helios_escape_release_blob {
    struct helios_escape_header hdr;
    UINT ctx_id, resource_id, flags, padding;
};

// ── Helios WDDM allocation private data (mirror protocol/src/wddm.rs, 48B) ───
#define HELIOS_WDDM_MAGIC 0x4857444Du /* 'HWDM' */
#define HELIOS_WDDM_VERSION 1u
#define HELIOS_WDDM_ALLOC_KIND_SHMEM 0u
#define VIRTIO_GPU_BLOB_MEM_HOST3D 2u
#define VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE 1u

struct helios_wddm_alloc_private {
    UINT64 blob_id;   // 0 = scratch shmem
    UINT64 size;
    UINT magic, version;
    UINT blob_mem, blob_flags;
    UINT ctx_id, map_cache;
    UINT kind, _pad;
};

static D3DKMT_HANDLE g_adapter, g_device, g_context;

static int open_helios(void) {
    D3DKMT_ENUMADAPTERS2 ea;
    memset(&ea, 0, sizeof(ea));
    NTSTATUS st = D3DKMTEnumAdapters2(&ea);
    if (st != 0 || ea.NumAdapters == 0) { printf("EnumAdapters2 st=0x%08x n=%u\n", (unsigned)st, ea.NumAdapters); return 1; }
    ea.pAdapters = (D3DKMT_ADAPTERINFO*)calloc(ea.NumAdapters, sizeof(D3DKMT_ADAPTERINFO));
    st = D3DKMTEnumAdapters2(&ea);
    if (st != 0) { printf("EnumAdapters2(2) st=0x%08x\n", (unsigned)st); return 1; }
    D3DKMT_HANDLE chosen = 0;
    for (UINT i = 0; i < ea.NumAdapters; i++) {
        D3DKMT_HANDLE h = ea.pAdapters[i].hAdapter;
        D3DKMT_ADAPTERREGISTRYINFO reg; memset(&reg, 0, sizeof(reg));
        D3DKMT_QUERYADAPTERINFO qai; memset(&qai, 0, sizeof(qai));
        qai.hAdapter = h; qai.Type = KMTQAITYPE_ADAPTERREGISTRYINFO;
        qai.pPrivateDriverData = &reg; qai.PrivateDriverDataSize = sizeof(reg);
        if (chosen == 0 && D3DKMTQueryAdapterInfo(&qai) == 0 && wcsstr(reg.AdapterString, L"Helios")) {
            chosen = h;
        } else {
            D3DKMT_CLOSEADAPTER ca; memset(&ca, 0, sizeof(ca)); ca.hAdapter = h; D3DKMTCloseAdapter(&ca);
        }
    }
    free(ea.pAdapters);
    if (!chosen) { printf("no Helios adapter\n"); return 1; }
    g_adapter = chosen;

    D3DKMT_CREATEDEVICE cd; memset(&cd, 0, sizeof(cd)); cd.hAdapter = chosen;
    st = D3DKMTCreateDevice(&cd);
    if (st != 0) { printf("CreateDevice st=0x%08x\n", (unsigned)st); return 1; }
    g_device = cd.hDevice;

    D3DKMT_CREATECONTEXT cc; memset(&cc, 0, sizeof(cc)); cc.hDevice = cd.hDevice;
    st = D3DKMTCreateContext(&cc);
    if (st != 0) { printf("CreateContext st=0x%08x\n", (unsigned)st); return 1; }
    g_context = cc.hContext;
    printf("opened Helios adapter=0x%x device=0x%x context=0x%x\n",
           (unsigned)g_adapter, (unsigned)g_device, (unsigned)g_context);
    return 0;
}

static int escape(void* buf, UINT size) {
    D3DKMT_ESCAPE esc; memset(&esc, 0, sizeof(esc));
    esc.hAdapter = g_adapter;
    esc.hDevice = g_device; // Stage 1 learned: DRIVERPRIVATE escape needs hDevice
    esc.Type = D3DKMT_ESCAPE_DRIVERPRIVATE;
    esc.pPrivateDriverData = buf;
    esc.PrivateDriverDataSize = size;
    NTSTATUS st = D3DKMTEscape(&esc);
    if (st != 0) { printf("D3DKMTEscape st=0x%08x\n", (unsigned)st); return 1; }
    return 0;
}

int main(void) {
    if (open_helios()) return 1;

    // venus context up
    struct helios_escape_ctx_create cc; memset(&cc, 0, sizeof(cc));
    cc.hdr.magic = HELIOS_ESCAPE_MAGIC; cc.hdr.cmd_type = HELIOS_ESCAPE_CTX_CREATE;
    cc.hdr.version = HELIOS_ESCAPE_VERSION; cc.hdr.size = sizeof(cc);
    cc.capset_id = VIRTIO_GPU_CAPSET_VENUS;
    if (escape(&cc, sizeof(cc))) return 1;
    UINT ctx_id = cc.out_ctx_id;
    printf("CTX_CREATE(VENUS) ok ctx_id=%u\n", ctx_id);

    // ── Gate 5a Stage 2b: the zero-copy BAR path over D3DKMTEscape ──────────
    // ALLOC_BLOB(blob_id=0, HOST3D mappable) -> MAP_BLOB -> sentinel on user VA.
    struct helios_escape_alloc_blob ab; memset(&ab, 0, sizeof(ab));
    ab.hdr.magic = HELIOS_ESCAPE_MAGIC; ab.hdr.cmd_type = HELIOS_ESCAPE_ALLOC_BLOB;
    ab.hdr.version = HELIOS_ESCAPE_VERSION; ab.hdr.size = sizeof(ab);
    ab.size = 4096; ab.blob_id = 0; ab.ctx_id = ctx_id;
    ab.blob_mem = VIRTIO_GPU_BLOB_MEM_HOST3D;
    ab.blob_flags = VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
    printf("alloc_blob sizeof=%zu (expect 48)\n", sizeof(ab));
    if (escape(&ab, sizeof(ab))) { printf("ALLOC_BLOB escape failed\n"); }
    else {
        UINT rid = ab.out_resource_id;
        printf("ALLOC_BLOB ok resource_id=%u\n", rid);

        struct helios_escape_map_blob mb; memset(&mb, 0, sizeof(mb));
        mb.hdr.magic = HELIOS_ESCAPE_MAGIC; mb.hdr.cmd_type = HELIOS_ESCAPE_MAP_BLOB;
        mb.hdr.version = HELIOS_ESCAPE_VERSION; mb.hdr.size = sizeof(mb);
        mb.resource_id = rid;
        printf("map_blob sizeof=%zu (expect 32)\n", sizeof(mb));
        if (escape(&mb, sizeof(mb))) { printf("MAP_BLOB escape failed\n"); }
        else {
            printf("MAP_BLOB ok user_va=0x%llx map_cache=%u\n",
                   (unsigned long long)mb.out_user_va, mb.map_cache);
            if (mb.out_user_va) {
                volatile unsigned* p = (volatile unsigned*)(UINT_PTR)mb.out_user_va;
                *p = 0xCAFEF00D;            // write a sentinel into the host-visible blob
                unsigned rb = *p;           // read it back through the BAR mapping
                printf("  BAR sentinel write/read = 0x%08x (%s)\n",
                       rb, rb == 0xCAFEF00D ? "OK" : "MISMATCH");
            }
        }

        struct helios_escape_release_blob rb2; memset(&rb2, 0, sizeof(rb2));
        rb2.hdr.magic = HELIOS_ESCAPE_MAGIC; rb2.hdr.cmd_type = HELIOS_ESCAPE_RELEASE_BLOB;
        rb2.hdr.version = HELIOS_ESCAPE_VERSION; rb2.hdr.size = sizeof(rb2);
        rb2.ctx_id = ctx_id; rb2.resource_id = rid;
        printf("RELEASE_BLOB %s\n", escape(&rb2, sizeof(rb2)) ? "failed" : "ok");
    }

    // (legacy) create a HOST3D mappable blob via D3DKMTCreateAllocation (kind=SHMEM, blob_id=0)
    struct helios_wddm_alloc_private priv; memset(&priv, 0, sizeof(priv));
    priv.magic = HELIOS_WDDM_MAGIC; priv.version = HELIOS_WDDM_VERSION;
    priv.kind = HELIOS_WDDM_ALLOC_KIND_SHMEM; priv.ctx_id = ctx_id;
    priv.blob_id = 0; priv.size = 4096;
    priv.blob_mem = VIRTIO_GPU_BLOB_MEM_HOST3D;
    priv.blob_flags = VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
    printf("HeliosWddmAllocPrivate sizeof=%zu (expect 48)\n", sizeof(priv));

    D3DDDI_ALLOCATIONINFO ai; memset(&ai, 0, sizeof(ai));
    ai.pPrivateDriverData = &priv;
    ai.PrivateDriverDataSize = sizeof(priv);

    D3DKMT_CREATEALLOCATION ca; memset(&ca, 0, sizeof(ca));
    ca.hDevice = g_device;
    ca.NumAllocations = 1;
    ca.pAllocationInfo = &ai;

    NTSTATUS st = D3DKMTCreateAllocation(&ca);
    printf("D3DKMTCreateAllocation st=0x%08x hAllocation=0x%x\n",
           (unsigned)st, (unsigned)ai.hAllocation);

    if (st == 0 && ai.hAllocation) {
        // Stage 2b probe: try to CPU-map the blob via D3DKMTLock2.
        D3DKMT_LOCK2 lk; memset(&lk, 0, sizeof(lk));
        lk.hDevice = g_device;
        lk.hAllocation = ai.hAllocation;
        NTSTATUS lst = D3DKMTLock2(&lk);
        printf("D3DKMTLock2 st=0x%08x pData=%p\n", (unsigned)lst, lk.pData);
        if (lst == 0 && lk.pData) {
            volatile unsigned* p = (volatile unsigned*)lk.pData;
            *p = 0xDEADBEEF;             // write a sentinel into the host-visible blob
            unsigned rb = *p;            // read it back
            printf("  sentinel write/read = 0x%08x\n", rb);
            D3DKMT_UNLOCK2 ul; memset(&ul, 0, sizeof(ul));
            ul.hDevice = g_device; ul.hAllocation = ai.hAllocation;
            printf("D3DKMTUnlock2 st=0x%08x\n", (unsigned)D3DKMTUnlock2(&ul));
        }

        D3DKMT_HANDLE h = ai.hAllocation;
        D3DKMT_DESTROYALLOCATION da; memset(&da, 0, sizeof(da));
        da.hDevice = g_device; da.phAllocationList = &h; da.AllocationCount = 1;
        NTSTATUS dst = D3DKMTDestroyAllocation(&da);
        printf("D3DKMTDestroyAllocation st=0x%08x\n", (unsigned)dst);
    }
    return 0;
}
