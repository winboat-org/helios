/* scanout_timeline_dump.c -- read Helios' bounded scanout causal timeline.
 *
 * This tool never submits work.  Take the two cursors around exactly one
 * Combined run, then dump the closed interval:
 *
 *   before=$(scanout_timeline_dump.exe --cursor)
 *   <run Combined once>
 *   after=$(scanout_timeline_dump.exe --cursor)
 *   scanout_timeline_dump.exe --dump $((before + 1)) "$after" > timeline.csv
 *
 * `--dump` writes only stable CSV to stdout.  Discovery/errors/loss are on
 * stderr so a captured CSV is directly consumable by the timeline oracle.
 *
 * Build (WinLibs g++ on win11):
 *   g++ -O2 -Wall -Wextra -o scanout_timeline_dump.exe \
 *       Z:\tools\scanout_timeline_dump.c -I"Z:\icd\win-build\wdk-include" -lgdi32
 */
#include <windows.h>
#include <d3dkmthk.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define HELIOS_ESCAPE_MAGIC 0x48454C53u /* 'HELS' */
#define HELIOS_ESCAPE_VERSION 1u
#define HELIOS_ESCAPE_QUERY_SCANOUT_TIMELINE 0x0011u
#define HELIOS_SCANOUT_TIMELINE_OP_META 0u
#define HELIOS_SCANOUT_TIMELINE_OP_READ 1u
#define HELIOS_SCANOUT_TIMELINE_TIME_100NS 1u
#define HELIOS_SCANOUT_TIMELINE_BATCH_CAP 32u
#define HELIOS_SCANOUT_TIMELINE_CAPACITY 32768u

struct helios_escape_header {
    uint32_t magic;
    uint32_t cmd_type;
    uint32_t version;
    uint32_t size;
};

/* protocol/src/escape.rs: HeliosScanoutTimelineEvent, exactly 64 bytes. */
struct helios_scanout_timeline_event {
    uint64_t sequence;
    uint64_t timestamp_100ns;
    uint64_t present_epoch;
    uint64_t carried_watermark;
    uint64_t identity;
    uint32_t resource_id;
    uint32_t aux;
    uint32_t kind;
    uint32_t flags;
    uint64_t reserved;
};

/* The fixed escape header is deliberately only 64 bytes.  The 32 entries are
 * trailing bytes in this user buffer, never a KMD stack-resident reply. */
struct helios_escape_query_scanout_timeline {
    struct helios_escape_header hdr;
    uint32_t in_op;
    uint32_t in_count;
    uint64_t in_start_seq;
    uint64_t out_cursor;
    uint64_t out_first_seq;
    uint32_t out_returned;
    uint32_t out_lost;
    uint32_t out_capacity;
    uint32_t out_time_unit;
};

struct timeline_batch {
    struct helios_escape_query_scanout_timeline reply;
    struct helios_scanout_timeline_event events[HELIOS_SCANOUT_TIMELINE_BATCH_CAP];
};

typedef char assert_event_is_64[(sizeof(struct helios_scanout_timeline_event) == 64) ? 1 : -1];
typedef char assert_header_is_64[(sizeof(struct helios_escape_query_scanout_timeline) == 64) ? 1 : -1];

static D3DKMT_HANDLE g_adapter;
static D3DKMT_HANDLE g_device;

static void init_header(struct helios_escape_header *hdr, uint32_t verb, uint32_t size) {
    hdr->magic = HELIOS_ESCAPE_MAGIC;
    hdr->cmd_type = verb;
    hdr->version = HELIOS_ESCAPE_VERSION;
    hdr->size = size;
}

static NTSTATUS escape_on_helios(void *buffer, uint32_t size) {
    D3DKMT_ESCAPE escape_call;
    memset(&escape_call, 0, sizeof(escape_call));
    escape_call.hAdapter = g_adapter;
    escape_call.hDevice = g_device;
    escape_call.Type = D3DKMT_ESCAPE_DRIVERPRIVATE;
    escape_call.pPrivateDriverData = buffer;
    escape_call.PrivateDriverDataSize = size;
    return D3DKMTEscape(&escape_call);
}

/* The read-only META verb is the identity probe: it identifies an adapter only
 * if both the transport succeeds and the reply matches the published ABI. */
static int open_helios(void) {
    D3DKMT_ENUMADAPTERS2 adapters;
    memset(&adapters, 0, sizeof(adapters));
    if (D3DKMTEnumAdapters2(&adapters) != 0 || adapters.NumAdapters == 0) {
        fprintf(stderr, "scanout_timeline: D3DKMTEnumAdapters2 failed\n");
        return 0;
    }
    adapters.pAdapters = (D3DKMT_ADAPTERINFO *)calloc(
        adapters.NumAdapters, sizeof(*adapters.pAdapters));
    if (!adapters.pAdapters || D3DKMTEnumAdapters2(&adapters) != 0) {
        fprintf(stderr, "scanout_timeline: adapter enumeration allocation/query failed\n");
        free(adapters.pAdapters);
        return 0;
    }

    for (uint32_t i = 0; i < adapters.NumAdapters; ++i) {
        D3DKMT_CREATEDEVICE create;
        memset(&create, 0, sizeof(create));
        create.hAdapter = adapters.pAdapters[i].hAdapter;
        if (D3DKMTCreateDevice(&create) != 0) {
            D3DKMT_CLOSEADAPTER close;
            memset(&close, 0, sizeof(close));
            close.hAdapter = create.hAdapter;
            D3DKMTCloseAdapter(&close);
            continue;
        }
        g_adapter = create.hAdapter;
        g_device = create.hDevice;

        struct helios_escape_query_scanout_timeline meta;
        memset(&meta, 0, sizeof(meta));
        init_header(&meta.hdr, HELIOS_ESCAPE_QUERY_SCANOUT_TIMELINE, sizeof(meta));
        meta.in_op = HELIOS_SCANOUT_TIMELINE_OP_META;
        NTSTATUS status = escape_on_helios(&meta, sizeof(meta));
        if (status == 0 && meta.out_capacity == HELIOS_SCANOUT_TIMELINE_CAPACITY
            && meta.out_time_unit == HELIOS_SCANOUT_TIMELINE_TIME_100NS) {
            free(adapters.pAdapters);
            return 1;
        }

        D3DKMT_DESTROYDEVICE destroy;
        memset(&destroy, 0, sizeof(destroy));
        destroy.hDevice = g_device;
        D3DKMTDestroyDevice(&destroy);
        D3DKMT_CLOSEADAPTER close;
        memset(&close, 0, sizeof(close));
        close.hAdapter = g_adapter;
        D3DKMTCloseAdapter(&close);
        g_adapter = 0;
        g_device = 0;
    }
    free(adapters.pAdapters);
    fprintf(stderr, "scanout_timeline: no adapter implements Helios timeline escape\n");
    return 0;
}

static void close_helios(void) {
    if (g_device) {
        D3DKMT_DESTROYDEVICE destroy;
        memset(&destroy, 0, sizeof(destroy));
        destroy.hDevice = g_device;
        D3DKMTDestroyDevice(&destroy);
    }
    if (g_adapter) {
        D3DKMT_CLOSEADAPTER close;
        memset(&close, 0, sizeof(close));
        close.hAdapter = g_adapter;
        D3DKMTCloseAdapter(&close);
    }
}

static int get_cursor(uint64_t *out_cursor) {
    struct helios_escape_query_scanout_timeline meta;
    memset(&meta, 0, sizeof(meta));
    init_header(&meta.hdr, HELIOS_ESCAPE_QUERY_SCANOUT_TIMELINE, sizeof(meta));
    meta.in_op = HELIOS_SCANOUT_TIMELINE_OP_META;
    NTSTATUS status = escape_on_helios(&meta, sizeof(meta));
    if (status != 0 || meta.out_capacity != HELIOS_SCANOUT_TIMELINE_CAPACITY
        || meta.out_time_unit != HELIOS_SCANOUT_TIMELINE_TIME_100NS) {
        fprintf(stderr, "scanout_timeline: META failed status=0x%08lx cap=%u time=%u\n",
                (unsigned long)status, meta.out_capacity, meta.out_time_unit);
        return 0;
    }
    *out_cursor = meta.out_cursor;
    return 1;
}

static int dump_closed_interval(uint64_t first, uint64_t last) {
    puts("sequence,timestamp_100ns,present_epoch,carried_watermark,identity,resource_id,aux,kind,flags");
    for (uint64_t start = first; start <= last;) {
        uint64_t remaining = last - start + 1;
        uint32_t count = remaining < HELIOS_SCANOUT_TIMELINE_BATCH_CAP
            ? (uint32_t)remaining : HELIOS_SCANOUT_TIMELINE_BATCH_CAP;
        struct timeline_batch batch;
        memset(&batch, 0, sizeof(batch));
        uint32_t bytes = (uint32_t)(sizeof(batch.reply)
            + (size_t)count * sizeof(batch.events[0]));
        init_header(&batch.reply.hdr, HELIOS_ESCAPE_QUERY_SCANOUT_TIMELINE, bytes);
        batch.reply.in_op = HELIOS_SCANOUT_TIMELINE_OP_READ;
        batch.reply.in_count = count;
        batch.reply.in_start_seq = start;
        NTSTATUS status = escape_on_helios(&batch, bytes);
        if (status != 0) {
            fprintf(stderr, "scanout_timeline: READ start=%" PRIu64 " status=0x%08lx\n",
                    start, (unsigned long)status);
            return 0;
        }
        if (batch.reply.out_lost) {
            fprintf(stderr, "scanout_timeline: lost=%u before/during start=%" PRIu64 "\n",
                    batch.reply.out_lost, start);
        }
        for (uint32_t i = 0; i < batch.reply.out_returned; ++i) {
            const struct helios_scanout_timeline_event *e = &batch.events[i];
            printf("%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64 ",%" PRIu64
                   ",%u,%u,%u,%u\n",
                   e->sequence, e->timestamp_100ns, e->present_epoch,
                   e->carried_watermark, e->identity, e->resource_id, e->aux,
                   e->kind, e->flags);
        }
        /* The KMD advances its attempt through `count` sequence numbers after
         * clamping an overwritten first sequence.  Use out_first_seq rather
         * than returned count so a reported loss cannot make this loop repeat
         * a slot forever.  The closed `last` cap keeps the output one run. */
        if (batch.reply.out_first_seq == 0
            || batch.reply.out_first_seq > UINT64_MAX - count) {
            fprintf(stderr, "scanout_timeline: invalid READ progress\n");
            return 0;
        }
        start = batch.reply.out_first_seq + count;
    }
    return 1;
}

int main(int argc, char **argv) {
    if (argc != 2 && argc != 4) {
        fprintf(stderr, "usage: %s --cursor | --dump FIRST_SEQ LAST_SEQ\n", argv[0]);
        return 2;
    }
    if (!open_helios()) {
        return 1;
    }

    int ok = 0;
    if (argc == 2 && strcmp(argv[1], "--cursor") == 0) {
        uint64_t cursor;
        if (get_cursor(&cursor)) {
            /* Machine-readable, intentionally no label: suitable for command
             * substitution in the before/run/after recipe above. */
            printf("%" PRIu64 "\n", cursor);
            ok = 1;
        }
    } else if (argc == 4 && strcmp(argv[1], "--dump") == 0) {
        char *end_first = NULL;
        char *end_last = NULL;
        uint64_t first = _strtoui64(argv[2], &end_first, 0);
        uint64_t last = _strtoui64(argv[3], &end_last, 0);
        if (!*argv[2] || !*argv[3] || *end_first || *end_last || first == 0 || last < first) {
            fprintf(stderr, "scanout_timeline: FIRST_SEQ and LAST_SEQ must form a nonzero closed interval\n");
        } else {
            ok = dump_closed_interval(first, last);
        }
    } else {
        fprintf(stderr, "usage: %s --cursor | --dump FIRST_SEQ LAST_SEQ\n", argv[0]);
    }
    close_helios();
    return ok ? 0 : 1;
}
