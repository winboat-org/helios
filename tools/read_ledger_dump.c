/* read_ledger_dump.c -- read Helios' mapped scanout READ LEDGER.
 *
 * This is a diagnostic-only consumer of HELIOS_ESCAPE_MAP_READ_LEDGER.  It
 * creates one D3DKMT device solely to issue PROBE, MAP, and UNMAP, then reads
 * the KMD-owned read-only page.  It never submits graphics work or changes a
 * driver/runtime knob.
 *
 * stdout is stable CSV; adapter discovery and failures go to stderr:
 *
 *   slot,resid,generation,issued,retired,reclaimed,slot_overflow
 *   0,123,17,7,7,0,0
 *
 * `reclaimed` is set when the final acquire-read of `resid` or `generation`
 * differs from the first. In that case the counters are not attributed to the
 * first claim: a same-resid generation change is a re-claim, not the same read.
 *
 * Build (WinLibs g++ on win11):
 *   g++ -O2 -Wall -Wextra -o read_ledger_dump.exe \
 *       Z:\\tools\\read_ledger_dump.c -I"Z:\\icd\\win-build\\wdk-include" -lgdi32
 */
#include <windows.h>
#include <d3dkmthk.h>
#include <inttypes.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Mirrors protocol/src/escape.rs.  Keep this tool ABI-only: it does not share
 * a build with the driver, so the assertions below intentionally fail loudly
 * if C packing drifts from Rust's repr(C) structs. */
#define HELIOS_ESCAPE_MAGIC 0x48454C53u /* 'HELS' */
#define HELIOS_ESCAPE_VERSION 1u
#define HELIOS_ESCAPE_MAP_READ_LEDGER 0x000Eu

#define HELIOS_READ_LEDGER_MAGIC 0x4C524C48u /* 'HLRL' */
#define HELIOS_READ_LEDGER_VERSION 2u
#define HELIOS_READ_LEDGER_SLOTS 65u

#define HELIOS_SCANOUT_ACQ_OP_PROBE 0u
#define HELIOS_SCANOUT_ACQ_OP_MAP 1u
#define HELIOS_SCANOUT_ACQ_OP_UNMAP 2u
#define HELIOS_SCANOUT_ACQ_OK 0u
#define HELIOS_SCANOUT_ACQ_PROBE_ACK 1u
#define HELIOS_SCANOUT_ACQ_NOT_FOUND 2u
#define HELIOS_SCANOUT_CAP_READ_LEDGER (1u << 0)

struct helios_escape_header {
    uint32_t magic;
    uint32_t cmd_type;
    uint32_t version;
    uint32_t size;
};

/* protocol/src/escape.rs: HeliosEscapeMapReadLedger, exactly 40 bytes. */
struct helios_escape_map_read_ledger {
    struct helios_escape_header hdr;
    uint64_t out_user_va;
    uint32_t op;
    uint32_t out_size;
    uint32_t out_state;
    uint32_t pad;
};

/* protocol/src/escape.rs: HeliosReadLedgerSlot, exactly 32 bytes. */
struct helios_read_ledger_slot {
    uint32_t resid;
    uint32_t pad0;
    uint64_t generation;
    uint64_t issued;
    uint64_t retired;
};

/* protocol/src/escape.rs: HeliosReadLedgerPage, exactly 2112 bytes. The
 * escape maps a whole 4 KiB page; only this defined prefix is consumed. */
struct helios_read_ledger_page {
    uint32_t magic;
    uint32_t version;
    uint32_t slot_count;
    uint32_t reserved0;
    struct helios_read_ledger_slot slots[HELIOS_READ_LEDGER_SLOTS];
    uint32_t slot_overflow;
    uint32_t reserved1[3];
};

typedef char assert_escape_header_is_16[
    (sizeof(struct helios_escape_header) == 16) ? 1 : -1];
typedef char assert_map_escape_is_40[
    (sizeof(struct helios_escape_map_read_ledger) == 40) ? 1 : -1];
typedef char assert_map_va_offset_is_16[
    (offsetof(struct helios_escape_map_read_ledger, out_user_va) == 16) ? 1 : -1];
typedef char assert_ledger_slot_is_32[
    (sizeof(struct helios_read_ledger_slot) == 32) ? 1 : -1];
typedef char assert_ledger_generation_offset_is_8[
    (offsetof(struct helios_read_ledger_slot, generation) == 8) ? 1 : -1];
typedef char assert_ledger_page_is_2112[
    (sizeof(struct helios_read_ledger_page) == 2112) ? 1 : -1];
typedef char assert_ledger_slots_offset_is_16[
    (offsetof(struct helios_read_ledger_page, slots) == 16) ? 1 : -1];
typedef char assert_ledger_overflow_offset_is_2096[
    (offsetof(struct helios_read_ledger_page, slot_overflow) == 2096) ? 1 : -1];

struct helios_device {
    D3DKMT_HANDLE adapter;
    D3DKMT_HANDLE device;
};

static void init_header(struct helios_escape_header *hdr, uint32_t size) {
    hdr->magic = HELIOS_ESCAPE_MAGIC;
    hdr->cmd_type = HELIOS_ESCAPE_MAP_READ_LEDGER;
    hdr->version = HELIOS_ESCAPE_VERSION;
    hdr->size = size;
}

static NTSTATUS ledger_escape(const struct helios_device *device, void *buffer,
                              uint32_t size) {
    D3DKMT_ESCAPE call;
    memset(&call, 0, sizeof(call));
    call.hAdapter = device->adapter;
    call.hDevice = device->device;
    call.Type = D3DKMT_ESCAPE_DRIVERPRIVATE;
    call.pPrivateDriverData = buffer;
    call.PrivateDriverDataSize = size;
    return D3DKMTEscape(&call);
}

static void close_device(struct helios_device *device) {
    if (device->device) {
        D3DKMT_DESTROYDEVICE destroy;
        memset(&destroy, 0, sizeof(destroy));
        destroy.hDevice = device->device;
        NTSTATUS status = D3DKMTDestroyDevice(&destroy);
        if (status != 0) {
            fprintf(stderr, "read_ledger: D3DKMTDestroyDevice failed status=0x%08lx\n",
                    (unsigned long)status);
        }
        device->device = 0;
    }
    if (device->adapter) {
        D3DKMT_CLOSEADAPTER close;
        memset(&close, 0, sizeof(close));
        close.hAdapter = device->adapter;
        NTSTATUS status = D3DKMTCloseAdapter(&close);
        if (status != 0) {
            fprintf(stderr, "read_ledger: D3DKMTCloseAdapter failed status=0x%08lx\n",
                    (unsigned long)status);
        }
        device->adapter = 0;
    }
}

/* The identity test requires both the documented PROBE acknowledgement and
 * the READ_LEDGER capability bit.  A successful unknown adapter escape is not
 * enough to select it. */
static int supports_read_ledger(const struct helios_device *device) {
    struct helios_escape_map_read_ledger probe;
    NTSTATUS status;

    memset(&probe, 0, sizeof(probe));
    init_header(&probe.hdr, (uint32_t)sizeof(probe));
    probe.op = HELIOS_SCANOUT_ACQ_OP_PROBE;
    status = ledger_escape(device, &probe, (uint32_t)sizeof(probe));
    return status == 0
        && probe.out_state == HELIOS_SCANOUT_ACQ_PROBE_ACK
        && (probe.out_size & HELIOS_SCANOUT_CAP_READ_LEDGER) != 0;
}

/* Enumerate adapters, retaining the sole opened device only for the adapter
 * whose probe confirms the exact Helios read-ledger contract. */
static int open_helios(struct helios_device *out) {
    D3DKMT_ENUMADAPTERS2 adapters;
    memset(&adapters, 0, sizeof(adapters));
    if (D3DKMTEnumAdapters2(&adapters) != 0 || adapters.NumAdapters == 0) {
        fprintf(stderr, "read_ledger: D3DKMTEnumAdapters2 failed\n");
        return 0;
    }
    adapters.pAdapters = (D3DKMT_ADAPTERINFO *)calloc(
        adapters.NumAdapters, sizeof(*adapters.pAdapters));
    if (!adapters.pAdapters || D3DKMTEnumAdapters2(&adapters) != 0) {
        fprintf(stderr, "read_ledger: adapter enumeration allocation/query failed\n");
        free(adapters.pAdapters);
        return 0;
    }

    for (uint32_t i = 0; i < adapters.NumAdapters; ++i) {
        D3DKMT_CREATEDEVICE create;
        struct helios_device candidate;
        memset(&create, 0, sizeof(create));
        memset(&candidate, 0, sizeof(candidate));
        create.hAdapter = adapters.pAdapters[i].hAdapter;
        if (D3DKMTCreateDevice(&create) != 0) {
            D3DKMT_CLOSEADAPTER close;
            memset(&close, 0, sizeof(close));
            close.hAdapter = create.hAdapter;
            NTSTATUS close_status = D3DKMTCloseAdapter(&close);
            if (close_status != 0) {
                fprintf(stderr,
                        "read_ledger: close after CreateDevice failure status=0x%08lx\n",
                        (unsigned long)close_status);
            }
            continue;
        }
        candidate.adapter = create.hAdapter;
        candidate.device = create.hDevice;
        if (supports_read_ledger(&candidate)) {
            *out = candidate;
            free(adapters.pAdapters);
            return 1;
        }
        close_device(&candidate);
    }
    free(adapters.pAdapters);
    fprintf(stderr, "read_ledger: no adapter implements Helios read-ledger escape\n");
    return 0;
}

/* The WinLibs g++ build uses GCC's acquire-load primitive.  It reads the
 * read-only mapping without an interlocked RMW (which would be invalid on the
 * page), and pairs with the KMD's Release stores. */
static uint32_t read_acquire_u32(const volatile uint32_t *word) {
    return __atomic_load_n(word, __ATOMIC_ACQUIRE);
}

static uint64_t read_acquire_u64(const volatile uint64_t *word) {
    return __atomic_load_n(word, __ATOMIC_ACQUIRE);
}

static int map_page(const struct helios_device *device,
                    const volatile struct helios_read_ledger_page **out_page,
                    int *out_mapped) {
    struct helios_escape_map_read_ledger map;
    NTSTATUS status;

    memset(&map, 0, sizeof(map));
    init_header(&map.hdr, (uint32_t)sizeof(map));
    map.op = HELIOS_SCANOUT_ACQ_OP_MAP;
    status = ledger_escape(device, &map, (uint32_t)sizeof(map));
    /* A successful MAP owns a KMD mapping even if a later local ABI check
     * rejects its reply, so mark it before those checks for cleanup. */
    *out_mapped = status == 0 && map.out_state == HELIOS_SCANOUT_ACQ_OK;
    if (!*out_mapped || map.out_user_va == 0 || map.out_size < sizeof(**out_page)) {
        fprintf(stderr,
                "read_ledger: MAP failed status=0x%08lx state=%u va=0x%llx size=%u\n",
                (unsigned long)status, map.out_state,
                (unsigned long long)map.out_user_va, map.out_size);
        return 0;
    }

    *out_page = (const volatile struct helios_read_ledger_page *)(uintptr_t)map.out_user_va;
    if (read_acquire_u32(&(*out_page)->magic) != HELIOS_READ_LEDGER_MAGIC
        || read_acquire_u32(&(*out_page)->version) != HELIOS_READ_LEDGER_VERSION
        || read_acquire_u32(&(*out_page)->slot_count) != HELIOS_READ_LEDGER_SLOTS) {
        fprintf(stderr, "read_ledger: mapped page ABI validation failed\n");
        return 0;
    }
    return 1;
}

/* UNMAP is attempted after every successful MAP, including ABI rejection, so
 * the diagnostic cannot leave an owner-keyed mapping behind. */
static int unmap_page(const struct helios_device *device) {
    struct helios_escape_map_read_ledger unmap;
    NTSTATUS status;

    memset(&unmap, 0, sizeof(unmap));
    init_header(&unmap.hdr, (uint32_t)sizeof(unmap));
    unmap.op = HELIOS_SCANOUT_ACQ_OP_UNMAP;
    status = ledger_escape(device, &unmap, (uint32_t)sizeof(unmap));
    if (status != 0 || (unmap.out_state != HELIOS_SCANOUT_ACQ_OK
                        && unmap.out_state != HELIOS_SCANOUT_ACQ_NOT_FOUND)) {
        fprintf(stderr, "read_ledger: UNMAP failed status=0x%08lx state=%u\n",
                (unsigned long)status, unmap.out_state);
        return 0;
    }
    return 1;
}

static void print_page(const volatile struct helios_read_ledger_page *page) {
    uint32_t slot_overflow = read_acquire_u32(&page->slot_overflow);
    puts("slot,resid,generation,issued,retired,reclaimed,slot_overflow");
    for (uint32_t i = 0; i < HELIOS_READ_LEDGER_SLOTS; ++i) {
        const volatile struct helios_read_ledger_slot *slot = &page->slots[i];
        uint32_t resid = read_acquire_u32(&slot->resid);
        uint64_t generation = read_acquire_u64(&slot->generation);
        uint64_t issued = read_acquire_u64(&slot->issued);
        uint64_t retired = read_acquire_u64(&slot->retired);
        uint64_t final_generation = read_acquire_u64(&slot->generation);
        uint32_t final_resid = read_acquire_u32(&slot->resid);
        uint32_t reclaimed = resid == 0 || generation == 0
            || final_resid != resid || final_generation != generation;

        if (reclaimed) {
            resid = 0;
            generation = 0;
            issued = 0;
            retired = 0;
        }

        printf("%u,%u,%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%u,%u\n",
               i, resid, generation, issued, retired,
               reclaimed, slot_overflow);
    }
}

int main(void) {
    struct helios_device device;
    const volatile struct helios_read_ledger_page *page = NULL;
    int mapped = 0;
    int ok = 0;

    memset(&device, 0, sizeof(device));
    if (!open_helios(&device)) {
        return 1;
    }
    if (map_page(&device, &page, &mapped)) {
        print_page(page);
        ok = 1;
    }
    if (mapped && !unmap_page(&device)) {
        ok = 0;
    }
    close_device(&device);
    return ok ? 0 : 1;
}
