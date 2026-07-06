// vehicle_flipwait_probe.c — feasibility gate for KERNEL-ENFORCED vehicle flip
// ordering (PSC WS2, 25th session A/B verdict: sw path 200 fps zero stale
// frames; the vehicle/flip path leaks stale frames through the bounded
// usermode gates' timeout paths).
//
// The planned fix queues a dxgkrnl GPU-side WAIT on the venus copy's monitored
// fence ahead of the present packet (D3DKMTWaitForSynchronizationObjectFromGpu)
// so the flip physically cannot execute before the copy completes — no 32 ms
// usermode cap to leak through. That only works if VidSch honors queued
// monitored-fence waits/signals on our SOFTWARE-SCHEDULED render-only adapter.
// This probe proves exactly that, with no rendering:
//
//   1. create monitored fences F (wait target) and G (observer), both 0
//   2. queue on the context:  WAIT(F >= 1)  then  SIGNAL(G = 5)
//   3. G's CPU value must STAY 0 while the wait is unsatisfied
//   4. CPU-signal F = 1  →  G must become 5 (the queue drained in order)
//
// PASS = the ordering primitive works end-to-end; the UMD present-path unit
// can proceed. FAIL shapes: wait/signal escape rejected (NTSTATUS printed) or
// G=5 while F=0 (queued ops NOT ordered — usermode gates stay).
//
// Build (win11, WinLibs gcc):
//   gcc -O2 -o vehicle_flipwait_probe.exe vehicle_flipwait_probe.c \
//       -I Z:\icd\win-build\wdk-include -lgdi32
#include <windows.h>
#include <stdio.h>

#ifndef _NTDEF_
typedef LONG NTSTATUS, *PNTSTATUS;
#endif
#include <d3dkmthk.h>

/* d3dkmthk.h only forward-declares OBJECT_ATTRIBUTES (mingw pulls no ntdef);
 * complete it here (ntdef.h ABI) for D3DKMTShareObjects. */
struct _OBJECT_ATTRIBUTES {
    ULONG Length;
    HANDLE RootDirectory;
    PVOID ObjectName;
    ULONG Attributes;
    PVOID SecurityDescriptor;
    PVOID SecurityQualityOfService;
};

#define HELIOS_ESCAPE_MAGIC 0x48454C53u /* 'HELS' */
#define HELIOS_ESCAPE_VERSION 1u
#define HELIOS_ESCAPE_CTX_CREATE 0x0002u
#define HELIOS_ESCAPE_CTX_DESTROY 0x0003u
#define VIRTIO_GPU_CAPSET_VENUS 4u

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

static D3DKMT_HANDLE g_adapter, g_device, g_context;

static NTSTATUS escape_st(void* buf, UINT size) {
    D3DKMT_ESCAPE esc; memset(&esc, 0, sizeof(esc));
    esc.hAdapter = g_adapter;
    esc.hDevice = g_device;
    esc.Type = D3DKMT_ESCAPE_DRIVERPRIVATE;
    esc.pPrivateDriverData = buf;
    esc.PrivateDriverDataSize = size;
    return D3DKMTEscape(&esc);
}

/* Find the Helios adapter via the versioned CTX_CREATE escape (same pattern as
 * blob_capacity_probe) and create a device + context on it. */
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
            D3DKMT_CREATECONTEXT ctx; memset(&ctx, 0, sizeof(ctx));
            ctx.hDevice = cd.hDevice;
            ctx.NodeOrdinal = 0;
            ctx.EngineAffinity = 0;
            st = D3DKMTCreateContext(&ctx);
            if (st != 0) { printf("CreateContext st=0x%08x\n", (unsigned)st); return 1; }
            g_context = ctx.hContext;
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

/* Create a NON-shared monitored fence (initial 0); returns its CPU value VA. */
static NTSTATUS create_monitored(D3DKMT_HANDLE* out_h, volatile UINT64** out_cpu) {
    D3DKMT_CREATESYNCHRONIZATIONOBJECT2 create;
    memset(&create, 0, sizeof(create));
    create.hDevice = g_device;
    create.Info.Type = D3DDDI_MONITORED_FENCE;
    create.Info.MonitoredFence.InitialFenceValue = 0;
    NTSTATUS st = D3DKMTCreateSynchronizationObject2(&create);
    if (st == 0) {
        *out_h = create.hSyncObject;
        *out_cpu = (volatile UINT64*)create.Info.MonitoredFence.FenceValueCPUVirtualAddress;
    }
    return st;
}

int main(void) {
    UINT ctx_id = 0;
    if (open_helios(&ctx_id)) return 1;
    printf("helios open: device=0x%x context=0x%x venus_ctx=%u\n",
           (unsigned)g_device, (unsigned)g_context, ctx_id);

    D3DKMT_HANDLE f = 0, g = 0;
    volatile UINT64 *f_cpu = NULL, *g_cpu = NULL;
    NTSTATUS st = create_monitored(&f, &f_cpu);
    if (st != 0) { printf("create F FAILED st=0x%08x\n", (unsigned)st); return 1; }
    st = create_monitored(&g, &g_cpu);
    if (st != 0) { printf("create G FAILED st=0x%08x\n", (unsigned)st); return 1; }
    printf("fences: F=0x%x cpu=%p (=%llu)  G=0x%x cpu=%p (=%llu)\n",
           (unsigned)f, (void*)f_cpu, (unsigned long long)(f_cpu ? *f_cpu : ~0ull),
           (unsigned)g, (void*)g_cpu, (unsigned long long)(g_cpu ? *g_cpu : ~0ull));

    /* Queue on the context: WAIT(F >= 1), then SIGNAL(G = 5). */
    const UINT64 wait_val = 1, sig_val = 5;
    D3DKMT_WAITFORSYNCHRONIZATIONOBJECTFROMGPU wg;
    memset(&wg, 0, sizeof(wg));
    wg.hContext = g_context;
    wg.ObjectCount = 1;
    wg.ObjectHandleArray = &f;
    wg.MonitoredFenceValueArray = &wait_val;
    st = D3DKMTWaitForSynchronizationObjectFromGpu(&wg);
    printf("WaitFromGpu(F>=1) st=0x%08x\n", (unsigned)st);
    if (st != 0) { printf("VERDICT: FAIL (queued GPU wait rejected)\n"); return 1; }

    D3DKMT_SIGNALSYNCHRONIZATIONOBJECTFROMGPU sg;
    memset(&sg, 0, sizeof(sg));
    sg.hContext = g_context;
    sg.ObjectCount = 1;
    sg.ObjectHandleArray = &g;
    sg.MonitoredFenceValueArray = &sig_val;
    st = D3DKMTSignalSynchronizationObjectFromGpu(&sg);
    printf("SignalFromGpu(G=5) st=0x%08x\n", (unsigned)st);
    if (st != 0) { printf("VERDICT: FAIL (queued GPU signal rejected)\n"); return 1; }

    Sleep(250);
    UINT64 g_before = g_cpu ? *g_cpu : ~0ull;
    printf("G after 250ms with F unsatisfied: %llu (expect 0)\n",
           (unsigned long long)g_before);

    /* Release the wait: CPU-signal F = 1. */
    D3DKMT_SIGNALSYNCHRONIZATIONOBJECTFROMCPU sc;
    memset(&sc, 0, sizeof(sc));
    sc.hDevice = g_device;
    sc.ObjectCount = 1;
    sc.ObjectHandleArray = &f;
    sc.FenceValueArray = &wait_val;
    st = D3DKMTSignalSynchronizationObjectFromCpu(&sc);
    printf("SignalFromCpu(F=1) st=0x%08x\n", (unsigned)st);

    UINT64 g_after = g_cpu ? *g_cpu : ~0ull;
    DWORD waited = 0;
    while (g_after != sig_val && waited < 2000) {
        Sleep(10);
        waited += 10;
        g_after = g_cpu ? *g_cpu : ~0ull;
    }
    printf("G after releasing F: %llu (expect 5) [%lu ms]\n",
           (unsigned long long)g_after, (unsigned long)waited);

    const int held = (g_before == 0);
    const int drained = (g_after == sig_val);
    printf("VERDICT: %s (held=%d drained=%d)\n",
           held && drained ? "PASS — queued monitored-fence waits are honored"
           : !held ? "FAIL — signal executed past the unsatisfied wait"
                   : "FAIL — queue never drained after the fence signal",
           held, drained);

    D3DKMT_DESTROYSYNCHRONIZATIONOBJECT ds;
    memset(&ds, 0, sizeof(ds));
    ds.hSyncObject = f;
    (void)D3DKMTDestroySynchronizationObject(&ds);
    ds.hSyncObject = g;
    (void)D3DKMTDestroySynchronizationObject(&ds);

    /* ── Cross-device shape (the real UMD topology): the copy fence is created
     * on the ICD's kernel device; the present packet rides the RUNTIME's
     * context. Test: fence F2 on device A (g_device), wait queued on a context
     * of a SECOND device B — first with the raw handle, then via the
     * NT-name share → OpenSyncObjectFromNtHandle2(B) reopen (the WS1 #4
     * consumer pattern the UMD would reuse). */
    printf("-- cross-device --\n");
    D3DKMT_CREATEDEVICE cd2; memset(&cd2, 0, sizeof(cd2)); cd2.hAdapter = g_adapter;
    st = D3DKMTCreateDevice(&cd2);
    if (st != 0) { printf("CreateDevice(B) st=0x%08x\n", (unsigned)st); return 1; }
    D3DKMT_CREATECONTEXT cc2; memset(&cc2, 0, sizeof(cc2));
    cc2.hDevice = cd2.hDevice;
    cc2.NodeOrdinal = 0;
    st = D3DKMTCreateContext(&cc2);
    if (st != 0) { printf("CreateContext(B) st=0x%08x\n", (unsigned)st); return 1; }

    /* F2: NT-shared monitored fence on device A (the ICD's creation shape). */
    D3DKMT_CREATESYNCHRONIZATIONOBJECT2 create2;
    memset(&create2, 0, sizeof(create2));
    create2.hDevice = g_device;
    create2.Info.Type = D3DDDI_MONITORED_FENCE;
    create2.Info.Flags.Shared = 1;
    create2.Info.Flags.NtSecuritySharing = 1;
    create2.Info.MonitoredFence.InitialFenceValue = 0;
    st = D3DKMTCreateSynchronizationObject2(&create2);
    if (st != 0) { printf("create F2 (NT-shared) FAILED st=0x%08x\n", (unsigned)st); return 1; }
    D3DKMT_HANDLE f2 = create2.hSyncObject;

    D3DKMT_HANDLE g2 = 0; volatile UINT64* g2_cpu = NULL;
    /* G2 (observer) on device B, non-shared. */
    D3DKMT_HANDLE save_dev = g_device;
    g_device = cd2.hDevice;
    st = create_monitored(&g2, &g2_cpu);
    g_device = save_dev;
    if (st != 0) { printf("create G2 FAILED st=0x%08x\n", (unsigned)st); return 1; }

    /* (a) raw cross-device handle: wait on B's context for A's fence. */
    const UINT64 wait2 = 1, sig2 = 7;
    memset(&wg, 0, sizeof(wg));
    wg.hContext = cc2.hContext;
    wg.ObjectCount = 1;
    wg.ObjectHandleArray = &f2;
    wg.MonitoredFenceValueArray = &wait2;
    NTSTATUS raw_st = D3DKMTWaitForSynchronizationObjectFromGpu(&wg);
    printf("cross-device RAW WaitFromGpu st=0x%08x\n", (unsigned)raw_st);

    if (raw_st != 0) {
        /* (b) reopen via the NT share on device B (the consumer pattern). */
        HANDLE nt = NULL;
        OBJECT_ATTRIBUTES oa; memset(&oa, 0, sizeof(oa));
        oa.Length = sizeof(oa);
        st = D3DKMTShareObjects(1, &f2, &oa, GENERIC_ALL, &nt);
        printf("ShareObjects(F2) st=0x%08x nt=%p\n", (unsigned)st, nt);
        if (st == 0 && nt) {
            D3DKMT_OPENSYNCOBJECTFROMNTHANDLE2 open2; memset(&open2, 0, sizeof(open2));
            open2.hNtHandle = nt;
            open2.hDevice = cd2.hDevice;
            st = D3DKMTOpenSyncObjectFromNtHandle2(&open2);
            printf("OpenSyncObjectFromNtHandle2(B) st=0x%08x hSync=0x%x\n",
                   (unsigned)st, (unsigned)open2.hSyncObject);
            if (st == 0) {
                D3DKMT_HANDLE f2b = open2.hSyncObject;
                memset(&wg, 0, sizeof(wg));
                wg.hContext = cc2.hContext;
                wg.ObjectCount = 1;
                wg.ObjectHandleArray = &f2b;
                wg.MonitoredFenceValueArray = &wait2;
                raw_st = D3DKMTWaitForSynchronizationObjectFromGpu(&wg);
                printf("cross-device REOPENED WaitFromGpu st=0x%08x\n", (unsigned)raw_st);
            }
            CloseHandle(nt);
        }
    }
    if (raw_st != 0) {
        printf("CROSS-DEVICE VERDICT: FAIL (no wait shape accepted)\n");
        return 1;
    }

    memset(&sg, 0, sizeof(sg));
    sg.hContext = cc2.hContext;
    sg.ObjectCount = 1;
    sg.ObjectHandleArray = &g2;
    sg.MonitoredFenceValueArray = &sig2;
    st = D3DKMTSignalSynchronizationObjectFromGpu(&sg);
    printf("cross-device SignalFromGpu(G2=7) st=0x%08x\n", (unsigned)st);

    Sleep(250);
    UINT64 g2_before = g2_cpu ? *g2_cpu : ~0ull;

    /* Release: CPU-signal F2 = 1 on its OWNING device A. */
    memset(&sc, 0, sizeof(sc));
    sc.hDevice = g_device;
    sc.ObjectCount = 1;
    sc.ObjectHandleArray = &f2;
    sc.FenceValueArray = &wait2;
    st = D3DKMTSignalSynchronizationObjectFromCpu(&sc);
    printf("cross-device SignalFromCpu(F2=1) st=0x%08x\n", (unsigned)st);

    UINT64 g2_after = g2_cpu ? *g2_cpu : ~0ull;
    waited = 0;
    while (g2_after != sig2 && waited < 2000) {
        Sleep(10);
        waited += 10;
        g2_after = g2_cpu ? *g2_cpu : ~0ull;
    }
    printf("CROSS-DEVICE VERDICT: %s (held=%llu drained=%llu after %lu ms)\n",
           (g2_before == 0 && g2_after == sig2) ? "PASS" : "FAIL",
           (unsigned long long)g2_before, (unsigned long long)g2_after,
           (unsigned long)waited);

    struct helios_escape_ctx_destroy cds; memset(&cds, 0, sizeof(cds));
    cds.hdr.magic = HELIOS_ESCAPE_MAGIC; cds.hdr.cmd_type = HELIOS_ESCAPE_CTX_DESTROY;
    cds.hdr.version = HELIOS_ESCAPE_VERSION; cds.hdr.size = sizeof(cds);
    cds.ctx_id = ctx_id;
    (void)escape_st(&cds, sizeof(cds));
    return held && drained ? 0 : 1;
}
