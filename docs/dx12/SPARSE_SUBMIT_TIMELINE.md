# Venus sparse submits and the feedback timeline counters

Status: fixed in package `22.22.281.0` (parent `7bd1231`), Mesa fork
`913491b0394`. Related: [SPARSE_COMPATIBILITY.md](SPARSE_COMPATIBILITY.md)
(the D3D11/tiled *backing* story, a different layer) and
[ACCELERATION_STRUCTURE_CURRENT_SIZE.md](ACCELERATION_STRUCTURE_CURRENT_SIZE.md)
(the other defect the FL12 admission change exposed).

## Symptom

The native D3D12 `tiled` probe advanced from `BLOCKED77` to running
(`CAP,TiledResourcesTier,2`) and then died at its first case:

```
ADAPTER,1af4,1050,00000000:00007bd1,Helios vGPU
CAP,TiledResourcesTier,2
BEGIN,tiled,tiling-buffer
<process exit 0xC0000005>
```

A WER minidump localised the fault to the guest ICD, not the engine and not the
KMD: `vulkan_virtio.dll` (the shipped DLL still carries its symbol table, so no
PDB was needed) RVA `0x379205` = `vn_queue_submission_prepare + 0x325`. The
faulting instruction was a load through a pointer field of a batch record; the
`0xC0000005` was a read inside the submit-preparation path, i.e. the ICD was
preparing a submission it did not understand.

## The submit that does it

With the fork's submit-shape tracer on (`HELIOS_SUBMIT_SHAPE_TRACE=1`, log
`C:\ProgramData\Helios\helios_icd_diag.log`) the failing submission logs as:

```
HELIOS submit-shape #n begin type=BIND_SPARSE_INFO batches=1 ... 
HELIOS submit-shape #n batch=0 waits=1 cmds=0 signals=1 wait0={...} sig0={...}
```

— a `VkBindSparseInfo` (batch type 7) with exactly one wait and one signal, both
a valid timeline semaphore handle, and then **no `submit-entry` line**: the
process died inside preparation, before forwarding. Instrumented statement marks
in the submit path ended at `fb:slot-nonnull`.

That shape is produced by vkd3d-proton's sparse-binding flush
(`libs/vkd3d/command.c`, `d3d12_command_queue_flush_bind_sparse`), which is how a
D3D12 reserved-resource tile mapping reaches the host:

```c
wait_semaphore_value = queue->submission_timeline_count++;
timeline_info.sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO;
timeline_info.waitSemaphoreValueCount = timeline_info.signalSemaphoreValueCount = 1;
timeline_info.pWaitSemaphoreValues = &wait_semaphore_value;
timeline_info.pSignalSemaphoreValues = &queue->submission_timeline_count;
bind_sparse_info.sType = VK_STRUCTURE_TYPE_BIND_SPARSE_INFO;
bind_sparse_info.pNext = &timeline_info;
bind_sparse_info.pWaitSemaphores = bind_sparse_info.pSignalSemaphores = &queue->submission_timeline;
bind_sparse_info.waitSemaphoreCount = bind_sparse_info.signalSemaphoreCount = 1;
...
vkQueueBindSparse(vk_queue_sparse, 1, &bind_sparse_info, ...);
```

The wait and the signal are the *same* semaphore: the queue's own submission
timeline. That is the semaphore the ICD must read a counter from.

## The defect

`vn_QueueBindSparse` (`icd/mesa/src/virtio/vulkan/vn_queue.c`) routes the batch
through `vn_queue_submission_prepare` → `vn_queue_submission_count_batch_feedback`.
For a signal semaphore carrying a Helios feedback slot, that function needs the
timeline counter — either to attach a feedback command (`queue->can_feedback`) or
to suspend feedback (`!queue->can_feedback`, the path a dedicated sparse-binding
queue takes, since it has none of graphics/compute/transfer). Both arms call:

* `vn_get_wait_semaphore_counter(submit, batch_index, 0)`
* `vn_get_signal_semaphore_counter(submit, batch_index, signal_index)`

and both of those switches had cases for `VK_STRUCTURE_TYPE_SUBMIT_INFO` and
`VK_STRUCTURE_TYPE_SUBMIT_INFO_2` only:

```c
   case VK_STRUCTURE_TYPE_SUBMIT_INFO_2:
      return submit->submit2_batches[batch_index].pSignalSemaphoreInfos[sem_index].value;
   default:
      UNREACHABLE("unexpected batch type");
   }
```

`UNREACHABLE` is not an error return. In a release build it is
`__builtin_unreachable()` — the compiler is entitled to assume the default case
cannot be reached, so the sparse batch does not get a clean failure; it gets
whatever the optimizer laid out, which is the observed bad load. The neighbouring
helpers (`vn_get_wait_semaphore_count`, `vn_get_signal_semaphore_count`,
`vn_get_wait_semaphore`, `vn_get_signal_semaphore`) all already handled
`VK_STRUCTURE_TYPE_BIND_SPARSE_INFO`; only the two *timeline counter* accessors,
added later by the feedback work, were missing it.

## The fix

`913491b0394` adds the missing case to both accessors, reading the values from
the same `pNext` struct the Vulkan spec puts them in:

```c
   case VK_STRUCTURE_TYPE_BIND_SPARSE_INFO: {
      const struct VkTimelineSemaphoreSubmitInfo *timeline_sem_info =
         vk_find_struct_const(submit->sparse_batches[batch_index].pNext,
                              TIMELINE_SEMAPHORE_SUBMIT_INFO);
      return timeline_sem_info->pSignalSemaphoreValues[sem_index];
   }
```

17 lines, two cases, no other change. It is the shape upstream Mesa carries:
current upstream `src/virtio/vulkan/vn_queue.c` handles
`VK_STRUCTURE_TYPE_BIND_SPARSE_INFO` in `vn_get_signal_semaphore_counter`
(fetched 2026-09-13); upstream's submission struct is single-batch, this fork's
is batched, so the port is mechanical and the fork now matches upstream
semantics. The counter read is only reached when the batch carries a
`VkTimelineSemaphoreSubmitInfo`, which the vkd3d path above always attaches —
the same assumption the pre-existing `VK_STRUCTURE_TYPE_SUBMIT_INFO` arm already
makes.

⚠ Why our tree reaches the case at all: upstream asserts that a sparse batch
never takes the feedback path (`assert(submit->batch_type !=
VK_STRUCTURE_TYPE_BIND_SPARSE_INFO)` in `vn_queue_submission_count_batch_feedback`).
This fork turned that assert into an `if` guard — the sparse batch skips the
*command* feedback loop (a `VkBindSparseInfo` has no command buffers) but still
enters the *semaphore* feedback arm, because Helios binds a feedback slot to the
engine's queue timeline semaphores and asks them for attribution. So the case is
reachable here by construction, not by a validation-layer hole.

## Verification

*Localisation* was done against a diagnostic build of the same 17-line change
deployed as an out-of-tree ICD
(`C:\ProgramData\Helios\icd-diag\`, registry swap under
`HKLM\SOFTWARE\Khronos\Vulkan\Drivers`, restored afterwards): the `tiled` case
then reported `PASS,tiled,tiling-buffer,case-completed` with `TILING,total,4`
instead of dying, and `CAP,TiledResourcesTier,2` unchanged. That DLL is not
byte-identical to the shipped one (the build embeds the source revision), so the
shipped artifact was rebuilt from the committed pin and re-tested as the package.

*Package* `22.22.281.0` (sha256
`922b9f0f3c64719066b44731ddcf5f8240647652790d2c7b09fe69a6e66c3a72`, 44 manifest
entries) installed as `oem28.inf`, Code 0, five driver images verified against the
manifest, DWM reloaded on the new `helios_umd.dll` and the new packaged ICD. Two
independent native `tiled` runs, each in its own child process:

```
pass 1: BLOCKED no-output-msaa, BLOCKED tiled-format-caps,
        PASS tiling-buffer, PASS tiling-2d, BLOCKED tiling-3d,
        PASS mappings, PASS copy-mappings, PASS copy-tiles-2d,
        FAIL copy-tiles-predicated
pass 2: BLOCKED no-output-msaa, BLOCKED tiled-format-caps,
        PASS tiling-buffer, PASS tiling-2d, BLOCKED tiling-3d,
        PASS mappings, PASS copy-mappings, PASS copy-tiles-2d,
        PASS copy-tiles-predicated, FAIL copy-tiles-msaa4x
```

`tiling-buffer` — the case that faulted — passes in both, with the same
`TILING,total,4` / `SUBRESOURCE,0,4,1,1,0` lines and
`PASS,tiled,tiling-buffer,case-completed`. The rest of the native suite is
unchanged: `adapter`, `indirect`, `indirect-ia`, `root-signature`, `raytracing`,
`sync` and `draw` (x64 and x86, 28/28 steps) pass; `no-output-msaa` and
`tiled-format-caps` remain BLOCKED on the absent D3D12 debug layer (`0x887a002d`)
and `tiling-3d` remains BLOCKED.

⚠ Failures that remain are **not** this change, but they are not all one thing
either. The two probes that carried the known open defect — `stream-output` and, as
this session showed, `allocator`, both content checks read back after a fence wait —
are **fixed as of `.288`** (`allocator` 55/55; `stream-output` 23/25 with two residual
undiagnosed failures). ⛔ The defect's original description, *"the D3D12 DMA packet
retires on Venus worker completion, not on host GPU completion"*, was **measured false**
on `.287` (`WfBStrm=2739`, `WfBReb=0`: the packet blocks on the registered stream and is
released by real GPU completion). The untruthfulness was in the engine's queue-signal
path, which waited on a **cached** submission-timeline value that could already be
retired. Full account: `KMD_IMPACT.md` §14a.2. Both probes fail intermittently with text
that varies run to run:

| probe | runs | failures | text |
|---|---|---|---|
| `stream-output` | 10 | 5 | `counter32 guard changed`, `NULL SO padding/tail overwritten`, `32-bit counter guards overwritten` |
| `allocator` | 10 | 4 | `GPU epoch pattern` (readback held a previous epoch of the 256-epoch reuse loop) |

The rate is batch-dependent — one 12-run batch of
`allocator`/`stream-output`/`raytracing` (4 each) had zero failures while the next
had six — which is what a fence that can advance before host completion looks like
from the outside. `allocator`'s 256-epoch loop is the sharpest oracle of the set
(exact content, known epoch, one second per run) and is worth keeping for the K-F
verification.

The `tiled` probe's own later cases are intermittent as well and are *not*
attributed by this work: `copy-tiles-predicated` failed pass 1 (and in the
diagnostic run) and passed pass 2, while `copy-tiles-msaa4x` failed pass 2 with
`MSAA CopyTiles sample roundtrip mismatch`. Both are content mismatches read back
after a fence wait, so they are candidates for the same hazard, but that is not
established here; `SPARSE_COMPATIBILITY.md` remains the governing record for what
tiled behaviour is claimed.

Cleanup note: the guest-side diagnostic ICD
(`C:\ProgramData\Helios\icd-diag\`) and its manifest are left out of the
registered driver list; only the packaged `.281` manifest
(`...\runtime\mesa\helios_vulkan.json`) is registered.

## Residual

The fix removes the crash; it does not by itself establish sparse semantics.
`SPARSE_COMPATIBILITY.md` remains the governing record for what tiled-resource
behaviour is and is not claimed on this stack, and the owner's 2026-09-12 scope
("do not spend further time on complete sparse semantics") is unchanged.
