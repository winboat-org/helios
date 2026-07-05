# VENUS_KMD_ALLOC_SPEC.md — minimal venus client in the KMD (self-allocate host-visible memory)

**Purpose (2026-06-21):** let `kmd_render` self-allocate a HOST_VISIBLE|HOST_COHERENT
`VkDeviceMemory` via venus at device-init, create+map a HOST3D blob over it, and report
it as a **BAR-backed, VidMm-registerable, CPU-writable page-table segment** (Option A for
the `0x10E:0x49` gate — VidMm drops system-RAM segments, so the page-table segment must be
device-BAR memory backed by real host memory).

Extracted (citation-verified) from `icd/mesa/src/virtio/{vulkan,venus-protocol}`. Implement
exactly. Wire primitives: LE; `u64`/`size_t`/`VkDeviceSize`/offset = 8 B; `u32`/`VkResult`/
`VkStructureType`/`VkFlags`/`VkCommandTypeEXT` = 4 B; `simple_pointer`/`array_size` = 8 B u64
(present=1, absent/NULL=0); stream 4-byte aligned; empty pNext = one 8-byte zero.

## THE key simplification
**All Vulkan handles are GUEST-assigned monotonic u64 ids** (counter from 1; 0=NULL),
EXCEPT `VkPhysicalDevice` which is **host-assigned** (must be decoded from the
`vkEnumeratePhysicalDevices` reply). So you pick the `VkDeviceMemory` id and **reuse it
verbatim as the virtio-gpu `blob_id`** — no need to decode the alloc reply except its
`VkResult`. (`vn_AllocateMemory` sets `mem->base.id = vn_get_next_obj_id()`; that id →
`mem_id` → `blob_id` of ALLOC_BLOB. `vn_device_memory.c` / `vn_renderer_helios.c`.)
`has_guest_vram=false` on Helios → blob is created lazily: send `vkAllocateMemory` FIRST,
THEN `RESOURCE_CREATE_BLOB(blob_id=memory_id)` + `RESOURCE_MAP_BLOB`.

## Ring (vn_ring) — fixed offsets (header fields 64-byte aligned)
head@0 (u32, host-written, read Acquire) · tail@64 (u32, you write SeqCst/full-fence) ·
status@128 (u32; IDLE=0x1 FATAL=0x2 ALIVE=0x4; clear via fetch_and(~mask)) ·
buffer@192 size **131072** (128 KiB, pow2) · extra@131264 size 4 (guest never touches) ·
**shmem_size = 131268 (0x200C4)**. head/tail are FREE-RUNNING u32 byte counters; bufoff =
counter & (buffer_size-1); occupancy = tail-head; preserve u32 wrap. Alloc = HOST3D blob
(`blob_mem=2`, `blob_flags=USE_MAPPABLE=1`, `blob_id=0`, size=131268) + MAP_BLOB +
**kernel-map** the gpa (`MmMapIoSpaceEx`, cache per `map_blob_prepare.map_cache`); memset 0.

## Submit / reply / completion
- Command header: `u32 VkCommandTypeEXT | u32 flags` (flags bit 0x1 = GENERATE_REPLY).
- Producer: wait `cur+size-head <= buffer_size`; memcpy into buffer (split at wrap); cur+=size;
  store tail=cur (**SeqCst**); seqno = post-write cur; if status&IDLE → `vkNotifyRingMESA`.
- Completion: poll head (Acquire) until `(i32)(head - seqno) >= 0`. Abort on status&FATAL.
- Reply: write `vkSetReplyCommandStreamMESA` (178) FIRST pointing at a reply shmem
  (`u32 178|u32 0|u64 1|{u32 reply_res_id,u64 off,u64 size}`), then the real cmd (flags=1),
  publish, poll, then decode from reply_shmem. Reply = `[u32 echoed cmdtype][returns...]`
  (NO flags word). Create/alloc reply: `[i32 VkResult][u64 1][u64 handle id]`. Void (mem
  props): no VkResult. New reply shmem → do a roundtrip first (251 submit-seqno / 252
  wait-seqno, direct) so the host maps it. **Validate every host-written count/offset before
  use (untrusted).**
- Direct (bootstrap, via `submit_venus`, NOT the ring): 188 CreateRing, 190 NotifyRing,
  178? (no — 178 is in-ring), 251 SubmitVirtqueueSeqno, 252 WaitVirtqueueSeqno.

## vkCreateRingMESA (188, direct) — register the ring
`u32 188|u32 0 | u64 ring_id | u64 1 [ i32 1000384000(RING_CREATE_INFO_MESA) | u64 0(pNext;
monitor optional) | u32 0(flags) | u32 <ring res_id> | u64 0(off) | u64 131268(size) | u64
1000000(idleTimeout ns) | u64 0(head) | u64 64(tail) | u64 128(status) | u64 192(buffer) |
u64 131072(bufferSize) | u64 131264(extra) | u64 4(extraSize) ]`.

## Command sequence (cmd-type ids) + encodings
1. **vkCreateRingMESA (188, direct)** — above.
2. **vkCreateInstance (0, reply)**: `u32 0|u32 1 | u64 1[ i32 1|u64 0|u32 0|u64 0|u32 0|u64 0|
   u32 0|u64 0 ] | u64 0 | u64 1[ u64 instance_id ]`. Check VkResult.
3. **vkEnumeratePhysicalDevices (2, reply)** — DECODE physDev id. Count: `u32 2|u32 1|u64
   instance_id|u64 1|u32 0|u64 0`. Array(1): `u32 2|u32 1|u64 instance_id|u64 1|u32 1|u64 1|
   u64 0`. Reply `[i32 2][i32 VkResult][u64 1][u32 count][u64 N][u64 id×N]` → physDev=id[0].
4. **vkGetPhysicalDeviceMemoryProperties (8, reply)**: `u32 8|u32 1|u64 physDev|u64 1|u64 32|
   u64 16`. Reply (no VkResult) `[i32 8][u64 1][u32 typeCount][u64 32][(u32 propFlags,u32
   heapIdx)×32][u32 heapCount][u64 16][(u64 size,u32 flags)×16]`. Pick first i<typeCount with
   propFlags & (HOST_VISIBLE 0x2 | HOST_COHERENT 0x4) both set.
5. **vkCreateDevice (11, reply)**: `u32 11|u32 1 | u64 physDev | u64 1[ i32 3|u64 0|u32 0|u32
   1|u64 1[ i32 2|u64 0|u32 0|u32 0|u32 1|u64 1|f32 1.0(0x3F800000) ]|u32 0|u64 0|u32 0|u64 0|
   u64 0 ] | u64 0 | u64 1[ u64 device_id ]`. Check VkResult. (If it fails, host may require a
   `VkDeviceQueueTimelineInfoMESA` pNext (ring_idx≥1) on the queue-create — verify live.)
6. **vkAllocateMemory (21, reply)**: `u32 21|u32 1 | u64 device_id | u64 1[ i32 5|u64 0|u64
   allocationSize|u32 memoryTypeIndex ] | u64 0 | u64 1[ u64 memory_id ]`. **memory_id is yours
   = the blob_id.** Check VkResult.
7. `resource_create_blob(ctx, HOST3D=2, USE_MAPPABLE=1, blob_id=memory_id, size)` →
   page-table res_id; `map_blob_prepare(res_id)` → gpa. Report a CpuVisible MEMORY segment
   with `CpuTranslatedAddress=gpa`, Size=size as the page-table segment (`MEMORY_SEGMENT_ID=2`).

## Skippable / hardcode
- `vkEnumerateInstanceVersion`: skip (Helios hardcodes wire_format_version=1).
- mem-props query: index is host-GPU-dependent — query, don't hardcode.
- alloc reply: only VkResult needed (you own memory_id).
- No export/dedicated pNext needed on `VkMemoryAllocateInfo` for a plain HOST_VISIBLE alloc.

## Verify-live flags
1. `VkDeviceQueueTimelineInfoMESA` pNext on vkCreateDevice queue-create (may be required).
2. vkCreateRingMESA monitor pNext (try NULL first).
3. Reply-shmem roundtrip before first decode.
4. Kernel cache type for the ring (host-visible venus mem is typically WB/cached).
5. Host reply-write/head-advance contract (virglrenderer decoder not in tree) — confirm the
   first roundtrip live.

Closest working reference: `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c` (ring shmem
create, submit, wait/seqno, bo-create-from-device-memory blob_id) + `vn_ring.c` + the
`venus-protocol/vn_protocol_driver_*.h` encoders.
