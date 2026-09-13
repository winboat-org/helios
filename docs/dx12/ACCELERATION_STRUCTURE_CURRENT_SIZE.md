# D3D12 acceleration structure CURRENT_SIZE

Status: implemented (2026-09-13, package `22.22.280.0`). Applies to the
`vkd3d-proton-helios` engine. Related: [STREAM_OUTPUT.md](STREAM_OUTPUT.md) and
the `tiling-buffer` defect, which were exposed by the same admission change but
are unrelated to this one.

## The contract

The DXR functional spec defines the postbuild info type exactly
(`D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_CURRENT_SIZE_DESC`):

> Space used by current acceleration structure or OMM array. **If the
> acceleration structure hasn't had a compaction operation performed on it, this
> size is the same one reported by `GetRaytracingAccelerationStructurePrebuildInfo()`**,
> and if it has been compacted this size is the same reported for postbuild info
> with `D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_COMPACTED_SIZE`.

So `CurrentSizeInBytes` is a D3D12-level value with two exact cases: the
`ResultDataMaxSizeInBytes` the application was handed at prebuild, or the
compacted size. It is not a description of the host's internal layout.

The application allocates `ResultDataMaxSizeInBytes`; a value larger than that
describes memory that does not exist, which is why the native probe checks it:

```c
require(current_size == sizes[i].ResultDataMaxSizeInBytes,
        "uncompacted current/prebuild size agreement");
```

## What the engine did, and why it was wrong

`vkd3d_acceleration_structure_write_postbuild_info` answered the query with
`VK_QUERY_TYPE_ACCELERATION_STRUCTURE_SIZE_KHR` — a Vulkan quantity.
`vkCmdWriteAccelerationStructuresPropertiesKHR` defines it as *"the number of
bytes required by the acceleration structure"*, and RADV satisfies that from the
header field its build writes (`src/amd/vulkan/bvh/header.comp`):

```
compacted_size     = bvh_offset + src.dst_node_offset
serialization_size = compacted_size + align(56 + 8 * instance_count, 128)
size               = serialization_size - 56 - 8 * instance_count
```

i.e. the packed size **plus the 128-byte alignment padding reserved for the
serialization header and its instance-pointer array** (the serialization header
is 56 bytes). That padding is real to RADV but is not part of the structure, so:

* for a structure whose packed size equals the driver's static bound — every
  small AS — the reported size **exceeds** the prebuild requirement;
* for a compacted or cloned structure it **exceeds the allocation the
  application sized from the compacted query**.

## Host measurement

`tools/host_as_size_probe.c` builds the same structures the guest probe builds
directly against RADV, with no Venus, no ICD and no guest in the loop, and prints
the prebuild requirement next to the size queries. Mesa 25.0.7, RX 6600:

```
case                      prebuild  current  compacted  serialization  delta
blas-triangle-1               320      392        320           448      +72
blas-aabb-1                   320      392        320           448      +72
blas-triangle-64            13056     7752       7680          7808    -5304
tlas-instances-2              512      568        512           640      +56
compact-of-blas-triangle-1    320*     392        320            -        +72
clone-of-blas-triangle-1      320*     392        320            -        +72
```

`*` allocated size. The 72 and 56 deltas are exactly the 128-alignment padding
for zero and two instances, and they are the deltas the guest reported. The
64-triangle BLAS also shows the failure in the benign direction: the static bound
is loose there, so the host's packed-plus-padding answer falls *below* the
prebuild size, which D3D12 also does not allow. There is no host query that
returns the prebuild bound after the build, so the number cannot be recovered
from the host — it can only be remembered.

## Fix

The engine records the D3D12 answer per structure and replays it:

* `struct vkd3d_view`'s buffer variant carries `rtas_build_size` (the prebuild
  requirement, 0 = unknown) and `rtas_compacted` (the structure came from a
  `COMPACT` copy), both atomic and both initialised where the AS view is created.
* `vkd3d_acceleration_structure_get_build_sizes` is now the **single** definition
  of a build's storage requirement: `d3d12_device_GetRaytracingAccelerationStructurePrebuildInfo`
  and the build path both call it, with the same converted build info and the
  `RTAS_ALLOW_BLAS_REBUILD_SIZES` adjustment inside, so the number an application
  is given and the number its postbuild query returns cannot disagree.
* `d3d12_command_list_build_raytracing_blas_and_tlas` records that number for the
  destination address. It is captured *before* the OMM VAs are resolved because
  the prebuild path that defines the application-visible value does not resolve
  them.
* `CURRENT_SIZE` then answers with a recorded constant (`vkCmdUpdateBuffer`);
  a compacted or cloned structure follows the host's `COMPACTED_SIZE` query,
  which is the same quantity in both APIs; anything unrecorded keeps the
  previous host-query fallback.

The recorded value is a snapshot of the last build or copy recorded for that
address, exactly like the existing `rtas_kind` state machine: D3D12 requires the
application to synchronize a postbuild query against the build it describes.

## Cost

Each D3D12 acceleration structure build now issues one extra
`vkGetAccelerationStructureBuildSizesKHR`. In Helios that is a synchronous Venus
round trip, where on a native Vulkan driver it is a cheap local call. It is a
correctness requirement rather than a convenience — no host query can reproduce
the prebuild bound — but it is a real per-build cost, so the follow-up if RTAS
build cost ever becomes the limit is a content-keyed cache of prebuild results
(the application has almost always already asked for exactly those inputs).

## Residual gap

A deserialized acceleration structure has no recorded build or copy, so it keeps
the host's size query and inherits RADV's padding. D3D12 does not state the
expected value for a restored structure as precisely as for a built one, the
native probes do not query it, and the fallback is the pre-existing behaviour
rather than a new approximation.

## Acceptance, 2026-09-13 (.280, `ef9c6586`)

The guest raytracing probe, which failed at the size check on `.279`, now passes
it and completes every later stage:

```
SIZE,0,320,320
SIZE,1,320,320
SIZE,2,512,512
PASS,compact_clone_update_source_lifetime
PASS,tlas_restore_completed_before_blas_recording
PASS,serialization_query_after_tlas_restore
PASS,serialize_relocate_tlas_first_deserialize_lifetime
PASS,native_dxr_probe_completed
```

The rest of the native suite is unchanged from `.279`: adapter, allocator,
indirect, indirect-ia, root-signature, sync and draw pass; `stream-output` still
fails `FAIL,counter32 guard changed` (the known DMA-retire ordering defect) and
`tiled` still fails `tiling-buffer` (the known Venus submit-prep crash).
