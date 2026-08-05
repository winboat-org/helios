# R2 — The D3D12 runtime↔UMD contract: semantics, not just signatures

**Lane:** R2. **Question:** is the D3D12 UMD DDI forwardable into vkd3d-proton's `ID3D12*` COM
objects the way `d3d10umddi` is forwarded into DXVK's `ID3D11Device`, or is it structurally
lower-level in ways that break a 1:1 forward?

**Sources of truth used here, in descending authority:**

1. `tmp/dx12/sdk/d3d12umddi.h` (Windows SDK 10.0.26100.0, 19 031 lines) — cited as `umddi:NNNN`.
2. `tmp/dx12/sdk/d3dkmthk.h`, `d3d12.h` — cited by path + line.
3. **Runtime validation strings extracted from the live `D3D12Core.dll` on the win11 VM**
   (`10.0.26100.8737`, 3 505 480 bytes). These are the *runtime's own words about what the driver
   must do* and are the only conceptual documentation of this DDI that exists. Extraction command
   (run once, read-only, via `win_exec`):
   ```powershell
   $b=[IO.File]::ReadAllBytes("C:\Windows\System32\D3D12Core.dll")
   $s=[Text.Encoding]::ASCII.GetString($b)
   [regex]::Matches($s,"[\x20-\x7e]{16,400}") | %{$_.Value} | Sort-Object -Unique |
     Out-File -Encoding ascii Z:\tmp\dx12\research\d3d12core-strings.txt
   ```
   Result: **25 782** unique strings, saved at
   `/home/rupansh/helios-vgpu/tmp/dx12/research/d3d12core-strings.txt` (cited as `d3d12core:NNNN`,
   the 1-based line in that file). A `Driver|driver|DDI`-filtered subset (270 lines) is at
   `d3d12core-driverstrings.txt`.
4. `vkd3d-proton-helios/` source, `umd/`, `kmd_render/` — cited by path + line.
5. MS Learn — cited by URL. **Finding in itself:** the `d3d12umddi` reference on Learn is
   auto-generated syntax stubs with *no* semantics. `PFND3D12DDI_EXECUTECOMMANDLISTS`
   (<https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3d12umddi/nc-d3d12umddi-pfnd3d12ddi_executecommandlists>)
   is `word_count: 75` and its entire body is the parameter list; there are no Remarks.
   `windows-driver-docs-research-only/` contains **zero** conceptual D3D12-UMD-DDI articles: only
   `display/enhanced-barriers.md`, `display/d3d12-render-passes.md`, `display/work-graphs.md`,
   `display/video-encoding-d3d12*.md` reference `D3D12DDI_*` at all, and each documents one
   feature, not the model. Anything below marked "I infer" is inferred *because there is no
   document to read*, not because I did not look.

---

## 0. Scale of the surface, up front

| Measure | Value | How counted |
|---|---|---|
| Distinct `PFND3D12DDI_*` typedefs in the header | **399** | `grep -o 'PFND3D12DDI_[A-Z0-9_]*' d3d12umddi.h \| sort -u \| wc -l` |
| `typedef struct` in the header | **683** | `grep -c '^typedef struct' d3d12umddi.h` |
| Function pointers in the newest core device table `D3D12DDI_DEVICE_FUNCS_CORE_0109` | **124** | `sed -n '13451,13616p' \| grep -c pfn` |
| Function pointers in the newest 3D command-list table `D3D12DDI_COMMAND_LIST_FUNCS_3D_0108` | **75** | `sed -n '13303,13388p' \| grep -c pfn` |
| Adapter table `D3D12DDI_ADAPTERFUNCS_0109` | **8** (umddi:13640-13650) | read |
| Command-queue table `D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001` | **7**, two of which are `pfnUnused` (umddi:2729-2738) | read |
| Optional DDI tables the runtime can request | **27 enumerators** in `D3D12DDI_TABLE_TYPE` (umddi:2488-2516) | read |

For scale comparison, the live D3D11 UMD implements ~220 DDI functions (DX12.md:375, quoting
ROADMAP.md:3155-3156) across `umd/src/forward/*` at 22 169 lines including the C++ bridge
(`wc -l umd/src/*.rs umd/src/forward/*.rs umd/bridge/*`). A minimum-viable D3D12 core+CL+queue
surface is **~206 entry points** before any optional table.

---

## 1. The object model the runtime imposes, and who owns which memory

### 1.1 Every driver object lives in runtime-allocated memory the driver sized

Identical to D3D11 and therefore already understood by `umd/`. Each object has a
`pfnCalcPrivateXxxSize` returning `SIZE_T` and a `pfnCreateXxx` that receives a handle whose
`pDrvPrivate` points at a runtime buffer of exactly that size. Examples, verbatim:

```c
// umddi:1921-1922
typedef SIZE_T ( APIENTRY* PFND3D12DDI_CALC_PRIVATE_DESCRIPTOR_HEAP_SIZE_0001 )( D3D12DDI_HDEVICE, _In_ CONST D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001* );
typedef HRESULT ( APIENTRY* PFND3D12DDI_CREATE_DESCRIPTOR_HEAP_0001 ) ( D3D12DDI_HDEVICE, _In_ CONST D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001*, D3D12DDI_HDESCRIPTORHEAP );
```

All D3D12 handle types are aliases of, or created by the same macros as, the D3D11 ones —
`d3d12umddi.h` `#include`s `d3d10umddi.h` (umddi:21) and typedefs `D3D12DDI_HDEVICE = D3D10DDI_HDEVICE`
(umddi:25), i.e. a struct with a single `pDrvPrivate` word. **`umd/src/forward/handles.rs:1-60`
already implements exactly this model** (a `Slot<Com<T>>` / `Slot<Boxed<S>>` tagged pointer into
the runtime word, holding "a bare owning COM ptr" for most handle kinds). That module ports to
D3D12 essentially unchanged, and it is the mechanical reason a forward is even conceivable.

`D3D12DDI_HANDLETYPE` (umddi:330-363) enumerates **26** live object classes, from
`D3D12DDI_HT_COMMAND_QUEUE = 19` through `D3D12DDI_HT_0080_VIDEO_ENCODER_HEAP = 49`.

### 1.2 The driver gets BOTH the D3D11-era kernel callbacks and a D3D12-only "core layer" table

```c
// umddi:13618-13636 (D3D12DDIARG_CREATEDEVICE_0109), abridged
typedef struct D3D12DDIARG_CREATEDEVICE_0109
{
    D3D12DDI_HRTDEVICE              hRTDevice;
    UINT                            Interface;
    UINT                            Version;
    CONST D3DDDI_DEVICECALLBACKS*   pKTCallbacks;           // in:  Pointer to runtime callbacks that invoke kernel
    D3D12DDI_HDEVICE                hDrvDevice;
    union { ... CONST struct D3D12DDI_CORELAYER_DEVICECALLBACKS_0062* p12UMCallbacks_0062; };
    D3D12DDI_CREATE_DEVICE_FLAGS    Flags;
    D3D12DDI_GPU_VIRTUAL_ADDRESS_RANGE* pReserveRanges;
    UINT NumReserveRanges;
} D3D12DDIARG_CREATEDEVICE_0109;
```

`pKTCallbacks` is **the same `D3DDDI_DEVICECALLBACKS` the D3D11 UMD already uses** — the bindgen'd
struct in `umd/target/release/build/helios_umd-*/out/d3d10umddi.rs:13127-13193` has 65 members
including `pfnRenderCb`, `pfnSubmitCommandCb`, `pfnPresentCb`, `pfnCreateContextVirtualCb`,
`pfnSignalSynchronizationObjectFromGpu2Cb`, `pfnReserveGpuVirtualAddressCb`,
`pfnMapGpuVirtualAddressCb`, `pfnMakeResidentCb`, `pfnEscapeCb`. **So a D3D12 UMD reaches the
kernel through the identical thunk set Helios' D3D11 UMD already drives.**

`D3D12DDI_CORELAYER_DEVICECALLBACKS_0062` (umddi:8606-8647) adds 18 D3D12-specific callbacks:
`pfnSetErrorCb`, `pfnSetCommandListErrorCb`, `pfnSetCommandListDDITableCb`, `pfnCreateContextCb`,
`pfnCreateContextVirtualCb`, `pfnDestroyContextCb`, `pfnCreatePagingQueueCb`,
`pfnDestroyPagingQueueCb`, `pfnMakeResidentCb`, `pfnEvictCb`, `pfnReclaimAllocations2Cb`,
`pfnOfferAllocationsCb`, `pfnAllocateCb`, `pfnDeallocateCb`,
`pfnCreateSchedulingGroupContextCb`, `pfnCreateSchedulingGroupContextVirtualCb`,
`pfnCreateHwQueueCb`, `pfnQueueBackgroundProcessingWorkCb`.

### 1.3 The driver, not the runtime, creates the WDDM context — and it does so per command queue

Runtime validation string, verbatim:

> `CreateContextCb or CreateContextVirtualCb called outside of queue creation.` — d3d12core:10597

and

> `Driver is not allowed to create a global Hw queue for a context which is owned by a command queue or scheduling group.` — d3d12core:12128
> `Driver targeted HwQueue against context belonging to different queue.` — d3d12core (driverstrings:109)

The `PFND3D12DDI_CREATECONTEXT_CB` signature makes the binding explicit — it takes a **command
queue** runtime handle, not a device handle:

```c
// umddi:2556-2559
typedef _Check_return_ HRESULT(APIENTRY CALLBACK *PFND3D12DDI_CREATECONTEXT_CB)(
    _In_    D3D12DDI_HRTCOMMANDQUEUE hRTCommandQueue,
    _Inout_ D3DDDICB_CREATECONTEXT*
    );
```

**Verified conclusion:** one WDDM context per `ID3D12CommandQueue`, created by the UMD inside
`pfnCreateCommandQueue`. Helios' D3D11 UMD already does the device-scoped equivalent —
`umd/src/device_funcs.rs:1046-1094` `create_runtime_context()` calls `pfnCreateContextCb` with
`NodeOrdinal = 0, EngineAffinity = 0` and stores the returned command-buffer / allocation-list /
patch-list windows. The D3D12 change is *cardinality* (per queue, not per device), not *kind*.

---

## 2. Area-by-area contract analysis

### 2.1 Device / adapter / queue creation and lifetime — **FORWARDABLE**

`OpenAdapter12` fills `D3D12DDI_ADAPTERFUNCS_0109` (umddi:13640-13650):
`pfnCalcPrivateDeviceSize`, `pfnCreateDevice`, `pfnCloseAdapter`, `pfnGetSupportedVersions`,
`pfnGetCaps`, `pfnGetOptionalDDITables`, `pfnFillDDITable`, `pfnDestroyDevice`. Shape identical to
`D3D10DDIARG_OPENADAPTER` handling in `umd/src/adapter.rs:191-240`, plus three new negotiation
entry points:

- `PFND3D12DDI_GETSUPPORTEDVERSIONS(hAdapter, UINT32* puEntries, UINT64* pSupportedDDIInterfaceVersions)`
  (umddi:2608-2609). The driver returns a **set** of `D3D12DDI_SUPPORTED_xxxx` 64-bit tokens
  (e.g. `D3D12DDI_SUPPORTED_0109`, umddi:13395). This is the lever that lets a new driver
  implement an *older, smaller* DDI revision.
- `PFND3D12DDI_GETOPTIONALDDITTABLES(hAdapter, UINT32*, D3D12DDI_TABLE_REQUEST*)` (umddi:2524-2525).
  Validation string: `PFND3D12DDI_GETOPTIONALDDITTABLES only supports D3D12DDI_TABLE_TYPE_COMMAND_LIST_3D.
  An unsupported table type was requested.` (d3d12core:22785).
- `PFND3D12DDI_FILLDDITTABLE(hAdapter, D3D12DDI_TABLE_TYPE, VOID* pTable, SIZE_T, UINT, D3D12DDI_HRTTABLE)`
  (umddi:2527-2528). **This is the D3D12 replacement for D3D11's "fill the device funcs struct"** —
  the runtime asks for a named table by type and the driver writes function pointers into a
  runtime-supplied buffer *of a runtime-supplied size*. Note the `SIZE_T` — the R702-class hazard
  (24H2 passing 576 B for a 592 B struct, DX12.md/adapter.rs:36-45 lineage) is explicitly
  parameterised here and **must** be honoured.

Device creation gets `D3D12DDI_CREATE_DEVICE_FLAGS` (umddi:2587-2593):
`NONE=0x0`, `DISABLE_IMPLICIT_MGPU=0x1`, `DEBUGGABLE=0x2`.

Queue creation, newest form (umddi:7019-7028):
```c
typedef struct D3D12DDIARG_CREATECOMMANDQUEUE_0050
{
    D3D12DDI_COMMAND_QUEUE_FLAGS          QueueFlags;
    UINT                                  NodeMask;
    D3D12DDI_COMMAND_QUEUE_CREATION_FLAGS QueueCreationFlags;
    D3D12DDI_HSCHEDULINGGROUP_0050        SchedulingGroup; // May be NULL
} D3D12DDIARG_CREATECOMMANDQUEUE_0050;
```

**Forward mapping:** `pfnCreateDevice` → one `vkd3d_create_device()` (`include/vkd3d.h:110`);
`pfnCreateCommandQueue` → one `ID3D12Device::CreateCommandQueue` on that vkd3d device **plus** one
`pfnCreateContextCb`. Shadow state: the `hRTCommandQueue`, the `D3DDDICB_CREATECONTEXT` windows,
and the queue's `D3D12DDI_COMMAND_QUEUE_FLAGS`.

**Risk: LOW.** The lifetime rules are D3D11's.

### 2.2 Command allocators, command lists, bundles — **FORWARDABLE WITH SHADOW STATE**

**The single most important structural answer in this dossier: nothing in the D3D12 UMD DDI hands
the driver a buffer to record into. The driver owns 100 % of the recording memory.**

Evidence — the entire recording-object surface, verbatim:

```c
// umddi:6627-6641 — the "command allocator" at DDI level is a POOL, and its create-args
//                   contain ONE flags word and no memory at all.
typedef enum D3D12DDI_COMMAND_POOL_FLAGS { D3D12DDI_COMMAND_POOL_FLAG_NONE = 0x00000000 } D3D12DDI_COMMAND_POOL_FLAGS;
typedef struct D3D12DDIARG_CREATE_COMMAND_POOL_0040 { D3D12DDI_COMMAND_POOL_FLAGS PoolFlags; } D3D12DDIARG_CREATE_COMMAND_POOL_0040;
typedef SIZE_T ( APIENTRY* PFND3D12DDI_CALC_PRIVATE_COMMAND_POOL_SIZE_0040 )( D3D12DDI_HDEVICE, _In_ CONST D3D12DDIARG_CREATE_COMMAND_POOL_0040* );
typedef HRESULT ( APIENTRY* PFND3D12DDI_CREATE_COMMAND_POOL_0040 ) ( D3D12DDI_HDEVICE, _In_ CONST D3D12DDIARG_CREATE_COMMAND_POOL_0040*, D3D12DDI_HCOMMANDPOOL_0040 );
typedef VOID ( APIENTRY* PFND3D12DDI_DESTROY_COMMAND_POOL_0040 ) ( D3D12DDI_HDEVICE, D3D12DDI_HCOMMANDPOOL_0040 );
typedef VOID ( APIENTRY* PFND3D12DDI_RESET_COMMAND_POOL_0040 ) ( D3D12DDI_HDEVICE, D3D12DDI_HCOMMANDPOOL_0040 );

// umddi:6649-6658 — the RECORDER is the writer; it is pointed at a pool.
typedef struct D3D12DDIARG_CREATE_COMMAND_RECORDER_0040
{
    D3D12DDI_COMMAND_QUEUE_FLAGS QueueFlags;
    D3D12DDI_COMMAND_RECORDER_FLAGS RecorderFlags;
} D3D12DDIARG_CREATE_COMMAND_RECORDER_0040;
typedef VOID ( APIENTRY* PFND3D12DDI_COMMAND_RECORDER_SET_COMMAND_POOL_AS_TARGET_0040 ) ( D3D12DDI_HDEVICE, D3D12DDI_HCOMMANDRECORDER_0040, D3D12DDI_HCOMMANDPOOL_0040 );

// umddi:6615-6625 — the LIST
typedef struct D3D12DDIARG_CREATE_COMMAND_LIST_0040
{
    D3D12DDI_COMMAND_LIST_TYPE   Type;
    D3D12DDI_COMMAND_QUEUE_FLAGS QueueFlags;
    UINT64                       ID;
    D3D12DDI_COMMAND_LIST_FLAGS  CommandListFlags;
    UINT                         NodeMask;
} D3D12DDIARG_CREATE_COMMAND_LIST_0040;

// umddi:6538-6545 — Reset
typedef struct D3D12DDIARG_RESETCOMMANDLIST_0040
{
    D3D12DDI_HCOMMANDRECORDER_0040   hDrvCommandRecorder;
    UINT64                           ID;
    D3D12DDI_COMMAND_LIST_FLAGS      CommandListFlags;
} D3D12DDIARG_RESETCOMMANDLIST_0040;
typedef VOID ( APIENTRY* PFND3D12DDI_RESETCOMMANDLIST_0040 )( D3D12DDI_HCOMMANDLIST, _In_ CONST D3D12DDIARG_RESETCOMMANDLIST_0040*);

// umddi:1750 — Close
typedef VOID ( APIENTRY* PFND3D12DDI_CLOSECOMMANDLIST )( D3D12DDI_HCOMMANDLIST );
```

Notes that matter:

- **Three driver objects, not two.** `ID3D12CommandAllocator` maps to a *command pool* (memory
  owner) at DDI ≥ `0040`; a *command recorder* is a separate object that is aimed at a pool and
  does the writing; a *command list* is the recorded result. At DDI `0003` the older shape was
  `pfnCreateCommandAllocator` / `pfnResetCommandAllocator` (umddi:1741-1744) with
  `D3D12DDIARG_RESETCOMMANDLIST { hDrvCommandAllocator; UINT Slot; UINT64 ID; ... }` (umddi:798-804).
  The `0040` refactor replaced allocator+Slot with recorder+pool.
- `Close` and `Reset` return **VOID**. There is no error path. Failures go through
  `pfnSetCommandListErrorCb(D3D12DDI_HRTCOMMANDLIST, HRESULT)` (umddi:2585).
- `D3D12DDI_COMMAND_LIST_TYPE` has only `DIRECT = 0` and `BUNDLE = 1` (umddi:1425-1429). COMPUTE and
  COPY lists are expressed by `D3D12DDI_COMMAND_QUEUE_FLAGS` on the *list* create args, not by a
  list type. Bundles are executed with `pfnExecuteBundle(hCL, hBundleCL)` (umddi:1767-1768).
- The driver may swap the command list's own DDI table at any time via
  `PFND3D12DDI_SETCOMMANDLISTDDITABLE_CB(D3D12DDI_HRTCOMMANDLIST, D3D12DDI_HRTTABLE)` (umddi:2554),
  and the runtime *requires* it be called at creation:
  > `Driver didn't call pfnSetCommandListDDITableCb or called it with invalid D3D12DDI_HRTTABLE at command list creation, defaulting to stubbed DDIs.` — d3d12core:12105

  This is a genuinely useful mechanism for a forwarder: a "closed" table and an "erroring" table
  can be installed without per-call state checks.

**Forward mapping:** pool → `ID3D12CommandAllocator`; recorder → a shadow struct naming its current
pool; list → `ID3D12GraphicsCommandList`. `pfnResetCommandList(hCL, {hRecorder, ...})` →
`ID3D12GraphicsCommandList::Reset(recorder->pool->allocator, nullptr)`.
`pfnResetCommandPool` → `ID3D12CommandAllocator::Reset`. `pfnCloseCommandList` →
`ID3D12GraphicsCommandList::Close` (HRESULT discarded into `pfnSetCommandListErrorCb`).

**Shadow state required:** recorder→pool binding; list→(recorder, pool) binding at last Reset;
the DDI→API translation of every recorded command (75 CL entry points).

**Risk: MEDIUM.** Not a model mismatch — a volume-of-translation problem, plus one subtlety: the
DDI's PSO/root-signature/descriptor-heap set commands take *driver handles*, which the forwarder
resolves through its own shadow, so every set-state path needs a handle→COM lookup.

### 2.3 `ExecuteCommandLists` — **FORWARDABLE WITH SHADOW STATE**, and the KMD contract is the real work

```c
// umddi:1735-1739
typedef VOID ( APIENTRY* PFND3D12DDI_EXECUTECOMMANDLISTS ) (
    D3D12DDI_HCOMMANDQUEUE,
    UINT Count,
     _In_reads_(Count) CONST D3D12DDI_HCOMMANDLIST* pCommandLists
    );
```

Full path, with what is verified at each hop:

| Hop | Who | Evidence |
|---|---|---|
| `ID3D12CommandQueue::ExecuteCommandLists` | app | `d3d12.h` |
| validation, then `pfnExecuteCommandLists(hDrvQueue, Count, pDrvLists)` | D3D12Core runtime | umddi:1735, umddi:2731 (`D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001.pfnExecuteCommandLists`) |
| build/complete a DMA buffer and submit it | **the UMD** | see below |
| `pfnSubmitCommandCb` / `pfnRenderCb` (runtime thunk) → `D3DKMTSubmitCommand` / `D3DKMTRender` | runtime thunk into kernel | d3d12core:11939-11944, d3d12core:23015/23018 |
| `DxgkDdiSubmitCommandVirtual` (Helios is GpuMmu ⇒ virtual, not legacy) | Helios KMD | `kmd_render/src/ddi/submit_command.rs:725-760` |

**That the UMD is the party that submits is directly evidenced by the runtime's own validation of
what the *driver* put in the submit structure:**

> `D3DDDICB_SUBMITCOMMAND::NumPrimaries is too large. Only half the available array may be used by driver.` — d3d12core:11944
> `D3DDDICB_SUBMITCOMMAND::BroadcastContextCount is too large.` — d3d12core:11943
> `D3DDDICB_SUBMITCOMMAND::BroadcastContext array must contain contexts that are all associated with the same command queue.` — d3d12core:11942
> `D3DDDICB_RENDER::BroadcastContext array must contain contexts that are all associated with the same command queue.` — d3d12core:11940
> `Reserved flags given to RenderCb` / `Reserved flags given to SubmitCommandToHwQueueCb` — d3d12core:23015, 23018

corroborated by MS Learn for the callback itself:

> "**pfnSubmitCommandCb** is used to submit command buffers on contexts that support graphics
> processing unit (GPU) virtual addressing. These contexts generate commands directly from user
> mode, manage their own command buffer pool and don't make use of allocation or patch location
> list. … Since DMA buffer are built directly by the user mode driver and submitted to the GPU
> without modification …"
> — <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dumddi/nc-d3dumddi-pfnd3dddi_submitcommandcb>

**Can a UMD that does its real work out-of-band satisfy this?** Helios' D3D11 UMD *already does
exactly that* and it is the strongest single piece of evidence in this dossier:

- `umd/src/forward/present.rs:795` obtains `pfnRenderCb` and `:960` obtains `pfnPresentCb`; the
  comment at `:940` records "The DXGI DDI requires `pfnRenderCb` to precede `pfnPresentCb`".
  The DMA buffer carries no GPU commands — DXVK already did the work over Vulkan/venus.
- On the kernel side, `kmd_render/src/ddi/submit_command.rs:720-724` states the contract verbatim:

  > "There is no guest GPU to program (the host owns the real MMU; venus addresses by resource id —
  > the actual work rides the venus Escape channel), but since C3/M3.4 **the fence is NOT lied
  > about: it queues behind the venus work outstanding at submit time and completes from the
  > interrupt DPC.**"

- And the UMD can name a *specific* completion point rather than "everything outstanding": the
  present path writes a watermark into the DMA buffer's private data that the KMD decodes
  (`decode_virtual_present_fence` → `decode_present_fence`,
  `kmd_render/src/ddi/submit_command.rs:504, 628-646`), backed by an ICD private export that
  registers a Vulkan timeline semaphore as a monotonic stream:
  `umd/bridge/bridge_icd_exports.h:37-42` `venus_register_present_stream(VkDevice, VkSemaphore, uint64_t* out_cookie)`.

So the answer is **yes, with a named mechanism that already exists**: at `pfnExecuteCommandLists`
the forwarder (a) forwards to vkd3d's queue, (b) obtains a monotonic completion watermark for that
submission, (c) submits an otherwise-empty DMA buffer on the queue's WDDM context whose private
data carries the watermark, and the KMD completes the DMA fence only when the host has reached it.

**Shadow state required:** per-queue WDDM context + its command/allocation/patch windows; per-queue
monotonic watermark; the vkd3d↔Vulkan-timeline plumbing.

**Risk: HIGH — but concentrated in one place**, namely getting the watermark out of vkd3d
(§4.3 below).

Two caps the runtime queries here and validates:
`D3D12DDICAPS_TYPE_EXECUTECOMMANDLISTS_PARALLELISM = 1069, // pData = BOOL` (umddi:128) and
`D3D12DDICAPS_TYPE_0023_UMD_BASED_COMMAND_QUEUE_PRIORITY = 1062` (umddi:118) — the latter has an
explicit failure string: `Driver did not correctly respond to
D3D12DDICAPS_TYPE_0023_UMD_BASED_COMMAND_QUEUE_PRIORITY caps query.` (d3d12core:12097).

### 2.4 Fences — **FORWARDABLE WITH SHADOW STATE (mechanism), but the load-bearing semantics are UNVERIFIED**

This is the area the assignment called out, so it gets the most care.

**What the header says — verbatim, all of it:**

```c
// umddi:1575-1598
typedef struct D3D12DDI_FENCE_PLACEMENT
{
    D3DGPU_VIRTUAL_ADDRESS BaseAddress;
} D3D12DDI_FENCE_PLACEMENT;

typedef enum D3D12DDI_FENCE_FLAGS
{
    D3D12DDI_FENCE_FLAG_NONE           = 0x0,
    D3D12DDI_FENCE_FLAG_BOTTOM_OF_PIPE = 0x1,
} D3D12DDI_FENCE_FLAGS;

typedef struct D3D12DDI_FENCE
{
    D3D12DDI_FENCE_PLACEMENT FenceValue;
    D3D12DDI_FENCE_PLACEMENT FenceMonitoredValue;
    D3D12DDI_FENCE_FLAGS Flags;
} D3D12DDI_FENCE;

typedef struct D3D12DDIARG_CREATE_FENCE
{
    UINT FenceCount;
    _Field_size_(FenceCount) D3D12DDI_FENCE* Fences;
} D3D12DDIARG_CREATE_FENCE;

// umddi:1786-1787
typedef SIZE_T ( APIENTRY* PFND3D12DDI_CALCPRIVATEFENCESIZE )( D3D12DDI_HDEVICE, _In_ CONST D3D12DDIARG_CREATE_FENCE* );
typedef HRESULT ( APIENTRY* PFND3D12DDI_CREATEFENCE )( D3D12DDI_HDEVICE, D3D12DDI_HFENCE, _In_ CONST D3D12DDIARG_CREATE_FENCE* );

// umddi:2712-2720 — the ONLY two fence operations, and they live on the QUEUE table
typedef struct D3D12DDIARG_FENCE_OPERATION
{
    D3D12DDI_HFENCE Fence;
    UINT64 Value;
    UINT PhysicalAdapterMask; // Out: The set of adapters to broadcast the operation to
} D3D12DDIARG_FENCE_OPERATION;

typedef void( APIENTRY* PFND3D12DDI_SIGNAL_FENCE )( D3D12DDI_HCOMMANDQUEUE, D3D12DDIARG_FENCE_OPERATION*);
typedef void( APIENTRY* PFND3D12DDI_WAIT_FOR_FENCE )( D3D12DDI_HCOMMANDQUEUE, D3D12DDIARG_FENCE_OPERATION*);
```

**Verified facts:**

1. **The driver is never given a `D3DKMT_HANDLE` for a D3D12 fence.** `D3D12DDIARG_CREATE_FENCE`
   carries only GPU virtual addresses. The `FenceValue`/`FenceMonitoredValue` GPU VA pair is
   exactly the WDDM monitored-fence shape: `d3dkmthk.h:1707-1708`
   ```c
   D3DKMT_PTR(VOID*,       FenceValueCPUVirtualAddress);           // out: Read-only mapping of the fence value for the CPU
   D3DKMT_ALIGN64 D3DGPU_VIRTUAL_ADDRESS FenceValueGPUVirtualAddress; // out: Read/write mapping of the fence value for the GPU
   ```
   The runtime keeps the CPU mapping and the handle; the driver gets the GPU mapping only.
2. **`ID3D12Fence::Signal` (the CPU signal) has no DDI at all.** `grep -n "FROM_CPU\|FromCpu\|FROMCPU"
   tmp/dx12/sdk/d3d12umddi.h` returns **zero** hits. Every fence entry point in the header is
   queue-scoped. Therefore CPU signal is executed entirely by the runtime
   (`D3DKMTSignalSynchronizationObjectFromCpu`, whose name is present in D3D12Core's own strings —
   d3d12core, `SignalSynchronizationObjectFromCpu`).
3. **The `FenceCount`/array shape is the LDA/multi-physical-adapter split**, matching
   `PhysicalAdapterMask` and `pfnGetImplicitPhysicalAdapterMask` (umddi:2710) /
   `pfnQueryNodeMap` (umddi:2724).
4. D3D12 fences are backed by monitored fences or WDDM3.1 native fences, per the runtime's own
   wording: `... must be either monitored fences with GPU access or native fences.` (d3d12core:20702).
   Helios does not implement any of `DxgkDdiCreateNativeFence` / `SignalMonitoredFence` /
   `SetNativeFenceLogBuffer` (DX12.md §3.5), so the monitored-fence-on-a-software-scheduled-context
   path is the only one available.

**What I infer, and why:** `PhysicalAdapterMask` is documented in the header as **`// Out: The set
of adapters to broadcast the operation to`**. An *out* parameter on a *void* function whose name is
"the set of adapters to broadcast to" only makes sense if the caller — the runtime — performs the
broadcast. Combined with fact 1 (the driver has no kernel handle to signal with), I infer:
**`pfnSignalFence`/`pfnWaitForFence` tell the driver to order its own pipeline around a fence
operation and to report the adapter mask; the runtime then performs the kernel-side
`D3DKMTSignalSynchronizationObjectFromGpu*` / `WaitFromGpu` against the driver's context.**
Confidence: high, but it is an inference. **UNVERIFIED — settling experiment in §5.**

**What this means for Helios.** The good news is that the WDDM-side semantics Helios needs are
*already proven on this stack*:

- `DxgkDdiSubmitCommandVirtual` completes a fence only after the venus work outstanding at submit
  time (`kmd_render/src/ddi/submit_command.rs:720-724`, quoted above), so the D3D12 fence's ordering
  guarantee ("signalled after the GPU work in preceding ExecuteCommandLists") reduces to Helios'
  existing wire-fence contract.
- VidSch's software scheduler already honours a queued `WAIT(F>=1)` before a `SIGNAL(G=5)` on this
  adapter — `tools/vehicle_flipwait_probe.c`, recorded in DX12.md:316-318.

The **make-or-break unknown** is: with no guest GPU writing the fence VA, does dxgkrnl's software
monitored-fence path update the value the runtime reads at `FenceValueCPUVirtualAddress`? On a real
GPU the *hardware* writes the fence VA and the KMD raises
`DXGK_INTERRUPT_MONITORED_FENCE_SIGNALED`. Helios has no such writer. If dxgkrnl updates the value
itself when it retires a queued monitored-fence signal packet on a software-scheduled context, the
architecture works untouched. If it requires a memory write plus a KMD interrupt notification, then
Helios' KMD must gain a monitored-fence notification path. **This is the highest-value single
experiment in the whole D3D12 investigation; see §5.1.**

**Risk: HIGH, and it is the risk that decides the strategy.**

### 2.5 Descriptor heaps — **FORWARDABLE** (cleanest surprise in the dossier)

```c
// umddi:808-832
typedef enum D3D12DDI_DESCRIPTOR_HEAP_TYPE { CBV_SRV_UAV, SAMPLER, RTV, DSV, NUM_TYPES } ...;
typedef enum D3D12DDI_DESCRIPTOR_HEAP_FLAGS { NONE=0x0, CPU_VISIBLE=0x1, SHADER_VISIBLE=0x2 } ...;
typedef struct D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001
{ D3D12DDI_DESCRIPTOR_HEAP_TYPE Type; UINT NumDescriptors; D3D12DDI_DESCRIPTOR_HEAP_FLAGS Flags; UINT NodeMask; } ...;

// umddi:1415-1423 — both handles are single opaque scalars
typedef struct D3D12DDI_CPU_DESCRIPTOR_HANDLE { SIZE_T ptr; } D3D12DDI_CPU_DESCRIPTOR_HANDLE;
typedef struct D3D12DDI_GPU_DESCRIPTOR_HANDLE { UINT64  ptr; } D3D12DDI_GPU_DESCRIPTOR_HANDLE;

// umddi:1925-1927 — the driver chooses the stride and both heap-start values
typedef UINT ( APIENTRY* PFND3D12DDI_GET_DESCRIPTOR_SIZE_IN_BYTES ) ( D3D12DDI_HDEVICE, D3D12DDI_DESCRIPTOR_HEAP_TYPE );
typedef D3D12DDI_CPU_DESCRIPTOR_HANDLE ( APIENTRY* PFND3D12DDI_GET_CPU_DESCRIPTOR_HANDLE_FOR_HEAP_START ) ( D3D12DDI_HDEVICE, D3D12DDI_HDESCRIPTORHEAP);
typedef D3D12DDI_GPU_DESCRIPTOR_HANDLE ( APIENTRY* PFND3D12DDI_GET_GPU_DESCRIPTOR_HANDLE_FOR_HEAP_START ) ( D3D12DDI_HDEVICE, D3D12DDI_HDESCRIPTORHEAP);
```

**Answers to the assignment's questions:**

- *Who allocates the heap memory?* **The driver, entirely.** `D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001`
  contains no pointer and no size, and there is no callback that hands the driver descriptor
  storage. `pfnCalcPrivateDescriptorHeapSize` sizes only the *object*; a real driver allocates the
  descriptor array itself (typically via `pfnAllocateCb` for shader-visible heaps).
- *What does the driver write into a CPU descriptor handle?* Whatever it likes. The view-creation
  DDIs take a destination CPU handle and are `VOID`:
  `pfnCreateShaderResourceView(hDevice, CONST D3D12DDIARG_CREATE_SHADER_RESOURCE_VIEW_0002*, D3D12DDI_CPU_DESCRIPTOR_HANDLE DestDescriptor)`
  (umddi:1885), and likewise CBV/UAV/RTV/DSV/Sampler (umddi:1894-1898).
- *What is a GPU descriptor handle at the DDI level?* An opaque `UINT64` the driver minted. It is
  handed back at `pfnSetGraphicsRootDescriptorTable(hCL, UINT RootParameterIndex, D3D12DDI_GPU_DESCRIPTOR_HANDLE BaseDescriptor)`
  (umddi:1941) and, for the clear-UAV paths, as a `(GPU handle in current heap, CPU handle)` pair
  (umddi:2007-2032).
- *How would a driver that forwards to vkd3d shadow this?* **It need not shadow it at all.** Because
  both handle values are driver-chosen opaque scalars, the forwarder can create a matching
  `ID3D12DescriptorHeap` on the vkd3d device and return **vkd3d's own handle values verbatim**:
  `vkd3d-proton-helios/libs/vkd3d/resource.c:9146-9167` returns `heap->cpu_va` and `heap->gpu_va`
  directly, and `libs/vkd3d/device.c:6505-6512` returns vkd3d's own increment size. Descriptor
  *arithmetic* done by the runtime/app (base + i*stride) then lands on vkd3d's own arithmetic,
  because it is the same stride.

`pfnCopyDescriptors` / `pfnCopyDescriptorsSimple` (umddi:1900-1918) map straight onto
`ID3D12Device::CopyDescriptors`.

**Caps the runtime validates here** (must be answered consistently or device creation fails):
> `Driver's MaxViewDescriptorHeapSize is too small` (d3d12core, driverstrings:115)
> `Driver's MaxSamplerDescriptorHeapSize is too small` (driverstrings:113)
> `Driver's MaxSamplerDescriptorHeapSizeWithStaticSamplers is too small or larger than MaxSamplerDescriptorHeapSize` (driverstrings:114)

**Risk: LOW-MEDIUM.** The one real hazard is ABI, not semantics: the DDI returns
`D3D12DDI_CPU_DESCRIPTOR_HANDLE` **by value** (MSVC returns a 1-scalar POD in RAX), whereas
vkd3d's C implementation uses the GCC/MinGW "return struct via hidden pointer" convention
(`D3D12_CPU_DESCRIPTOR_HANDLE * STDMETHODCALLTYPE d3d12_descriptor_heap_GetCPUDescriptorHandleForHeapStart(iface, D3D12_CPU_DESCRIPTOR_HANDLE *descriptor)`,
resource.c:9146-9147). This is the same defect class as the 52nd-session UMD crash
(`bridge_guard` deduced `R=int` from a bare `0` and truncated `size_t` returns) and must be handled
explicitly in the bridge, not assumed.

### 2.6 Resources, heaps, placed/reserved resources, GPU virtual addresses — **FORWARDABLE WITH SHADOW STATE; the GPU-VA question is the second load-bearing unknown**

Creation is a *single fused* DDI — heap and resource together (umddi:13438-13445):

```c
typedef HRESULT ( APIENTRY* PFND3D12DDI_CREATEHEAPANDRESOURCE_0109)(
    D3D12DDI_HDEVICE, _In_opt_ CONST D3D12DDIARG_CREATEHEAP_0001*, D3D12DDI_HHEAP, D3D12DDI_HRTRESOURCE,
    _In_opt_ CONST D3D12DDIARG_CREATERESOURCE_0109*, _In_opt_ CONST D3D12DDI_CLEAR_VALUES*,
    D3D12DDI_HPROTECTEDRESOURCESESSION_0030, D3D12DDI_HRESOURCE );
```

Either argument may be NULL: heap-only (`CreateHeap`), resource-only (`CreatePlacedResource` /
`CreateReservedResource`), or both (`CreateCommittedResource`). The runtime hard-requires the whole
family:
> `Driver set pfnCreateHeapAndResource to NULL.` / `pfnDestroyHeapAndResource` / `pfnOpenHeapAndResource` /
> `pfnCalcPrivateHeapAndResourceSizes` / `pfnCalcPrivateOpenedHeapAndResourceSizes` /
> `pfnCheckResourceAllocationInfo` / `pfnCheckExistingResourceAllocationInfo` /
> `pfnCheckSubresourceInfo` / `pfnCopyBufferRegion` — d3d12core:12170-12178
> `Driver must set pfnMapHeap and pfnUnmapHeap to non-NULL.` — d3d12core:12129

`D3D12DDIARG_CREATERESOURCE_0109` (umddi:13413-13436) hands the driver `Width/Height/DepthOrArraySize/
MipLevels/DXGI_FORMAT/SampleDesc/Layout/Flags/InitialBarrierLayout/pRowMajorLayout/
SamplerFeedbackMipRegion/NumCastableFormats/pCastableFormats/CreateAtVirtualAddress` and, first
field, `D3D12DDIARG_BUFFER_PLACEMENT ReuseBufferGPUVA` (umddi:441-447).

**Kernel identity is mandatory in at least three places, so a "pure passthrough with no
`pfnAllocateCb`" is not viable:**

- `typedef D3DKMT_HANDLE ( APIENTRY* PFND3D12DDI_CHECKRESOURCEALLOCATIONHANDLE )( D3D12DDI_HDEVICE, D3D10DDI_HRESOURCE );`
  (umddi:2992)
- `PFND3D12DDI_GET_DEBUG_ALLOCATION_INFO_0014` must return
  `D3D12DDI_DEBUG_KMT_ALLOCATION_INFO_0014 { UINT32 PhysicalAdapterIndex; D3DKMT_HANDLE hAllocation; UINT64 Offset; UINT64 Size; }`
  (umddi:3890-3905).
- `pfnAllocateCb` takes `D3D12DDICB_ALLOCATE_0022 { pPrivateDriverData; PrivateDriverDataSize; hResource; D3DKMT_HANDLE hKMResource; NumAllocations; D3D12DDI_ALLOCATION_INFO_0022* }`
  (umddi:4841-4849), i.e. the driver mints kernel allocations exactly the way the D3D11 UMD does today.
  Validation: `Reserved fields in D3D12DDI_ALLOCATION_INFO_0022 were not zero.` (d3d12core:23007).

**GPU virtual addresses.** `typedef UINT64 D3D12DDI_GPU_VIRTUAL_ADDRESS;` (umddi:92). The runtime
asks the driver for a resource's VA:
`typedef D3D12DDI_GPU_VIRTUAL_ADDRESS ( APIENTRY* PFND3D12DDI_CHECKRESOURCEVIRTUALADDRESS )( D3D12DDI_HDEVICE, D3D12DDI_HRESOURCE );`
(umddi:2476), and the VA then travels through root descriptors
(`PFND3D12DDI_SET_ROOT_BUFFER_VIEW(..., D3D12DDI_GPU_VIRTUAL_ADDRESS BufferLocation)`, umddi:1959),
IB/VB/SO views (umddi:1963-1989), and indirect-argument buffers. Caps:
`D3D12DDICAPS_TYPE_GPUVA_CAPS = 1009` → `D3D12DDI_GPUVA_CAPS_0004 { UINT MaxGPUVirtualAddressBitsPerResource; }`
(umddi:250-257). The runtime validates it:
> `Driver set MaxGPUVirtualAddressBitsPerResource to 0.` — d3d12core:12169
> `FL12.2+ driver incorrectly did not report at least 40 bits of GPU virtual address bits` — d3d12core:13157

Helios reports a 40-bit GPU VA (`kmd_render/src/ddi/gpummu.rs:44-65`, per DX12.md:290-292), which
clears that bar exactly.

**The gap.** DX12.md §3.4 is right that guest GPU VA on this stack is decorative — but *for a
forwarding UMD the guest VA space is not what would be used at all*. vkd3d's
`ID3D12Resource::GetGPUVirtualAddress` returns `resource->res.va`
(`vkd3d-proton-helios/libs/vkd3d/resource.c:2656-2663`), which is a Vulkan **buffer device
address** in the *host* GPU's address space, obtained through venus. A forwarder would return
those from `pfnCheckResourceVirtualAddress` and never call `pfnReserveGpuVirtualAddressCb` /
`pfnMapGpuVirtualAddressCb`. Whether the D3D12 runtime and its debug layer accept a VA space the
driver never obtained from the kernel is **UNVERIFIED** (§5.3). The header offers one hook that
suggests the runtime *does* care about VA placement in at least one mode:
`D3D12DDIARG_CREATEDEVICE_0109.pReserveRanges / NumReserveRanges` (umddi:13634-13635) and
`D3D12DDI_RECREATE_AT_TIER` + `CreateAtVirtualAddress` (umddi:13397-13435) — but
`RECREATE_AT_TIER_NOT_SUPPORTED = 0` is a legal answer.

**Reserved (tiled) resources** additionally require `pfnUpdateTileMappings` / `pfnCopyTileMappings`
— note they live on the **command queue** table (umddi:2734-2735, 1852-…), i.e. they are immediate
operations, not recorded ones. vkd3d implements them on `ID3D12CommandQueue`, so the mapping is
1:1.

**Risk: HIGH** (kernel-allocation identity + VA acceptance), **but not obviously fatal**.

### 2.7 Residency / MakeResident / Evict / budgets — **FORWARDABLE (mostly trivially)**

Both directions exist. Driver-side DDIs (umddi:1842-1850):

```c
typedef HRESULT ( APIENTRY* PFND3D12DDI_MAKERESIDENT_0001 )( D3D12DDI_HDEVICE, D3D12DDIARG_MAKERESIDENT_0001* );
typedef HRESULT ( APIENTRY* PFND3D12DDI_EVICT2 )( D3D12DDI_HDEVICE, CONST D3D12DDIARG_EVICT* );
typedef HRESULT ( APIENTRY* PFND3D12DDI_OFFERRESOURCES )( D3D12DDI_HDEVICE, CONST D3D12DDIARG_OFFERRESOURCES* );
typedef HRESULT ( APIENTRY* PFND3D12DDI_RECLAIMRESOURCES_0001 )( D3D12DDI_HDEVICE, D3D12DDIARG_RECLAIMRESOURCES_0001* );
```

with the paging-fence protocol spelled out in the args (umddi:494-514):
```c
    _Field_size_(NumAdapters) UINT64* pPagingFenceValue;    // out: Fence to wait on
    UINT WaitMask;                                          // out: Bit "i" is set if PagingFenceValue[i] is valid.  Only if MakeResident returns E_PENDING.
```

Callback-side (umddi:2531-2551): `pfnMakeResidentCb`, `pfnEvictCb`, `pfnReclaimAllocations2Cb`,
`pfnOfferAllocationsCb`, all taking a `D3D12DDI_HRTPAGINGQUEUE` created via
`pfnCreatePagingQueueCb`. **Helios' D3D11 UMD already creates the WDDM 2.x paging queue** —
`umd/src/device_funcs.rs:1101-1130` `create_runtime_paging_queue()`.

Budgets are answered through the optional downlevel table (umddi:18326-18349):
```c
typedef struct D3D12DDI_QUERY_VIDEO_MEMORY_INFO_0054 { UINT64 Budget; UINT64 CurrentUsage; } ...;
typedef void (APIENTRY* PFND3D12DDI_QUERY_VIDEO_MEMORY_INFO_0054)(
    D3D12DDI_HDEVICE, UINT NodeIndex, D3D12DDI_MEMORY_SEGMENT_GROUP_0054, _Out_ D3D12DDI_QUERY_VIDEO_MEMORY_INFO_0054*);
```
`D3D12DDI_MEMORY_SEGMENT_GROUP_0054 { LOCAL, NON_LOCAL }` (umddi:18320-18324).

**Risk: LOW.** `DriverManagesResidency` is not set by the Helios KMD (DX12.md:245-247), so VidMm
owns residency and the driver's MakeResident/Evict can be honest thin forwards to the callbacks or
to `S_OK` for a UMA-style claim. The one trap is the `E_PENDING` + paging-fence protocol, which
must not be faked.

### 2.8 Root signatures, PSOs, PSO libraries, state objects — **FORWARDABLE WITH SHADOW STATE (re-serialization required)**

**Root signatures arrive PARSED, not as a blob.** `D3D12DDIARG_CREATE_ROOT_SIGNATURE_0001 { CONST
D3D12DDI_ROOT_SIGNATURE* pRootSignature; UINT NodeMask; }` (umddi:1409-1413), where
`D3D12DDI_ROOT_SIGNATURE` (umddi:1397-1407) is a structure of parameters, static samplers and
flags. **vkd3d's `ID3D12Device::CreateRootSignature` takes a serialized DXBC `RTS0` blob**
(`vkd3d-proton-helios/libs/vkd3d/device.c:6514-6531`). So the forwarder must **re-serialize**.
That is feasible: `vkd3d_serialize_root_signature(const D3D12_ROOT_SIGNATURE_DESC*, version, blob,
error_blob)` exists at `vkd3d-proton-helios/include/vkd3d.h:129` and
`libs/vkd3d/vkd3d_main.c:453`, layered on `vkd3d_shader_serialize_root_signature`
(`libs/vkd3d-shader/dxbc.c:1384`). ⚠ **It is not exported from the Windows `d3d12core.dll`** — see
§4.1.

**Shaders arrive per-stage as DXBC containers with a root-signature handle and full IO signatures:**
```c
// umddi:2209-2212
typedef SIZE_T ( APIENTRY* PFND3D12DDI_CALC_PRIVATE_SHADER_SIZE )(
    D3D12DDI_HDEVICE, _In_reads_(pShaderCode[1]) CONST UINT* pShaderCode, D3D12DDI_HROOTSIGNATURE, _In_ CONST D3D12DDIARG_STAGE_IO_SIGNATURES* );
```
`pShaderCode[1]` is the DXBC container's dword-1 size field — the same convention the D3D11 UMD
already parses in `umd/bridge/bridge_dxbc.cpp`. `D3D12DDIARG_STAGE_IO_SIGNATURES` (umddi:2089-2125)
carries the full input/output signature "to assist in the event input/output registers need to be
reordered during shader compilation" — a forwarder ignores it, because vkd3d recovers signatures
from the container.

**PSOs are assembled from handles, not descs:**
```c
// umddi:11952-11978, abridged
typedef struct D3D12DDIARG_CREATE_PIPELINE_STATE_0099
{
    D3D12DDI_HSHADER hComputeShader, hVertexShader, hPixelShader, hDomainShader, hHullShader, hGeometryShader;
    D3D12DDI_HROOTSIGNATURE hRootSignature;
    D3D12DDI_HBLENDSTATE hBlendState;  UINT SampleMask;
    D3D12DDI_HRASTERIZERSTATE hRasterizerState;  D3D12DDI_HDEPTHSTENCILSTATE hDepthStencilState;
    D3D12DDI_HELEMENTLAYOUT hElementLayout;
    D3D12DDI_INDEX_BUFFER_STRIP_CUT_VALUE IBStripCutValue;
    D3D12DDI_PRIMITIVE_TOPOLOGY_TYPE PrimitiveTopologyType;
    UINT NumRenderTargets; DXGI_FORMAT RTVFormats[8]; DXGI_FORMAT DSVFormat;
    DXGI_SAMPLE_DESC SampleDesc; UINT NodeMask;
    D3D12DDI_LIBRARY_REFERENCE_0010 LibraryReference; D3D12DDI_VIEW_INSTANCING_DESC ViewInstancingDesc;
    D3D12DDI_HSHADER hMeshShader, hAmplificationShader; D3D12DDI_PIPELINE_STATE_FLAGS Flags;
} D3D12DDIARG_CREATE_PIPELINE_STATE_0099;
```

So blend / rasterizer / depth-stencil / element-layout are **separate driver objects** created
earlier (`pfnCreateBlendState`, `pfnCreateRasterizerState_0102`, `pfnCreateDepthStencilState_0095`,
`pfnCreateElementLayout_0010`), and the PSO references them by handle. vkd3d wants them inline in
`D3D12_GRAPHICS_PIPELINE_STATE_DESC`. **Shadow state:** each of those handles stores its DDI desc;
`pfnCreatePipelineState` reassembles a `D3D12_GRAPHICS_PIPELINE_STATE_DESC` (translating
`D3D12DDI_BLEND` (umddi:2740-2763) → `D3D12_BLEND`, etc.) plus the retained shader bytecode blobs.

Pipeline libraries (umddi:13565-13571: `pfnCalcPrivatePipelineLibrarySize`,
`pfnCreatePipelineLibrary`, `pfnAddPipelineStateToLibrary`, `pfnCalcSerializedLibrarySize`,
`pfnSerializeLibrary`) map to `ID3D12PipelineLibrary`. State objects / DXR (umddi:13590-13599) map
to `ID3D12StateObject` and are a large second tranche.

**Risk: MEDIUM-HIGH.** No model mismatch; a lot of struct translation plus one genuine
reconstruction (root signature re-serialization), which is exactly the class of work the R802
bindgen discipline exists to make safe.

### 2.9 Barriers and resource state — **FORWARDABLE**

**The driver sees barriers; the runtime does not resolve them.** Both generations are present in
one table:

```c
// umddi:4802-4816 — legacy
typedef struct D3D12DDIARG_RESOURCE_BARRIER_0022
{
    D3D12DDI_RESOURCE_BARRIER_TYPE    Type;      // TRANSITION | ALIASING | UAV | 0022_RANGED (umddi:1477-1483)
    D3D12DDI_RESOURCE_BARRIER_FLAGS   Flags;     // NONE | BEGIN_ONLY | END_ONLY | ATOMIC_COPY | ALIASING (umddi:1505-1512)
    union { D3D12DDI_RESOURCE_TRANSITION_BARRIER_0003 Transition;
            D3D12DDI_RESOURCE_RANGED_BARRIER_0022     Ranged;
            D3D12DDI_RESOURCE_UAV_BARRIER             UAV; };
} D3D12DDIARG_RESOURCE_BARRIER_0022;
typedef VOID ( APIENTRY* PFND3D12DDI_RESOURCEBARRIER_0022 )( D3D12DDI_HCOMMANDLIST, UINT Count, _In_reads_(Count) CONST D3D12DDIARG_RESOURCE_BARRIER_0022* );
```
plus enhanced barriers `pfnBarrier` (`PFND3D12DDI_BARRIER_0088` / `_0094`, present in
`D3D12DDI_COMMAND_LIST_FUNCS_3D_0108` at umddi:13380). Support is opt-in:
> "A driver indicates support by setting the **EnhancedBarriersSupported** member of
> **D3D12DDI_D3D12_OPTIONS_DATA_0089** to TRUE."
> — `windows-driver-docs-research-only/windows-driver-docs-pr/display/enhanced-barriers.md:34`

Resources also carry an `InitialBarrierLayout` at creation (umddi:13425) and legacy
`InitialResourceState` in the open path (umddi:491).

Both map 1:1 onto vkd3d's `ID3D12GraphicsCommandList::ResourceBarrier` and
`ID3D12GraphicsCommandList7::Barrier`.

**Risk: LOW.**

### 2.10 Multi-queue / COPY / COMPUTE and the miniport's engine nodes — **FORWARDABLE, degraded**

At the DDI, queue class is a flag, not a node:
```c
// umddi:1435-1448
D3D12DDI_COMMAND_QUEUE_FLAG_NONE=0, _3D=0x1, _COMPUTE=0x2, _COPY=0x4, _PAGING=0x8,
_0022_VIDEO_DECODE=0x10, _0022_VIDEO_PROCESS=0x20, _0053_VIDEO_ENCODE=0x40
```
The mapping from flag to WDDM node is **the driver's choice**, expressed in the
`D3DDDICB_CREATECONTEXT{VIRTUAL}.NodeOrdinal` it passes at queue creation. Helios advertises
exactly one node, `DXGK_ENGINE_TYPE_3D`, `NbAsymetricProcessingNodes = 1`
(DX12.md:207-208 citing `kmd_render/src/ddi/query_adapter_info.rs:1254-1278, 456-464`). Multiple
contexts on one node is legal WDDM; it costs parallelism, not correctness — dxgkrnl time-slices
them. So COPY/COMPUTE queues would exist and work, serialized behind 3D at the WDDM level (though
vkd3d may still get real host-side parallelism, since the real work is out-of-band).

Two caps must be answered honestly or the runtime complains:
> `Driver did not correctly respond to D3D12DDICAPS_TYPE_0050_HARDWARE_SCHEDULING_CAPS caps query.` — d3d12core:12098

and Helios must report **no** hardware scheduling, because `DxgkDdiCreateHwQueue` returns
`STATUS_NOT_SUPPORTED` and records `HwQRef` (DX12.md:214-217 citing
`kmd_render/src/ddi/scheduler.rs:180-187`). The runtime enforces the consequence:
> `Driver didn't provide any HwQueues for a hardware scheduling command queue present.` — d3d12core:12106

`pfnCalcPrivateSchedulingGroupSize` / `pfnCreateSchedulingGroup` / `pfnDestroySchedulingGroup`
(umddi:13579-13581) belong to the same hardware-scheduling family and can be refused consistently.

**Risk: MEDIUM** (a caps-honesty risk of exactly the class CLAUDE.md/DX12.md §5.5 warns about:
`SupportDirectFlip=1` and `FlipImmediateMmIo`).

### 2.11 Debug layer / SDK layers interactions the driver must tolerate — **must be designed in from day one**

- `D3D12DDI_CREATE_DEVICE_FLAG_DEBUGGABLE = 0x2` (umddi:2591) arrives on `CalcPrivateDeviceSize`
  **and** `CreateDevice`, so the *private size* may legitimately differ between debug and retail.
- `pfnGetDebugAllocationInfo` (umddi:3898-3905) must map any `D3D12DDI_HANDLE_AND_TYPE` to
  `{ VA infos, KMT allocation infos }`.
- **The caps gauntlet is a hard gate, not advice.** The runtime aborts device creation on any of:
  `Driver did not respond to D3D12DDICAPS_TYPE_D3D12_OPTIONS caps query.` /
  `... D3D12DDICAPS_TYPE_ARCHITECTURE_INFO ...` / `... D3D12DDICAPS_TYPE_SHADER ...` /
  `Driver did not report any supported shader models in D3D12DDICAPS_TYPE_0011_SHADER_MODELS caps query` /
  `Driver doesn't respond to D3D12DDICAPS_MEMORY_ARCHITECTURE Caps.` /
  `Driver failed D3D12DDICAPS_TEXTURE_LAYOUT or D3D12DDICAPS_TEXTURE_LAYOUT_SETS Caps.` /
  `Driver did not set valid WaveLaneCountMin/Max or TotalLaneCount via D3D12DDICAPS_TYPE_SHADER caps query`
  (d3d12core:12099-12112). There are **~12 distinct "Driver filled out an invalid value in
  D3D12DDI_D3D12_OPTIONS_DATA::<Tier>"** strings (d3d12core:12113-12127) and a whole family of
  shader-model coupling rules — e.g.
  `Drivers that support raytracing must expose shader model 6.3.`,
  `Drivers that support mesh shader 1.0 must expose shader model 6.5.`,
  `Drivers that expose AtomicInt64OnTypedResource, AtomicInt64OnGroupShared,
  AtomicInt64OnDescriptorHeapResource, DerivativesInMeshAndAmplificationShaders or WaveMMATier
  must expose shader model 6.6.` (driverstrings:116, and eight sibling rules at 117-124).
  **Every tier is a contract the runtime cross-checks.**
- `D3D12DDICAPS_TYPE` has **48 enumerators** (umddi:94-150) spanning `1000..1091`.
- Device removal on error is the runtime's response to `pfnSetErrorCb`:
  `Removing device due to bad UMD error.` / `Removing device due to driver error.` /
  `Removing device due to driver-reported app error.` (d3d12core:22986-22988).
- `D3D12DDICAPS_TYPE_3DPIPELINESUPPORT` uses `D3D12DDI_3DPIPELINELEVEL` (umddi:2924-2933):
  `1_0_GENERIC=1, 1_0_CORE=2, 11_0=10, 11_1=11, 12_0=12, 12_1=13, 12_2=14`, and the
  `_0081` variant is an in/out negotiation
  (`HighestRuntimeSupportedFeatureLevel` in, `MaximumDriverSupportedFeatureLevel` out — umddi:10418-10420).

**Risk: MEDIUM.** This is where the "advertising a capability that is not backed is a lie the OS
acts on" landmine (DX12.md §5.5) is densest.

### 2.12 Present (out of lane, recorded for R7)

D3D12 present *does* reach the driver, on the **command list** table:
```c
// umddi:7250-7251
typedef VOID ( APIENTRY* PFND3D12DDI_PRESENT_0051 ) ( D3D12DDI_HCOMMANDLIST, D3D12DDI_HCOMMANDQUEUE, _In_ CONST D3D12DDIARG_PRESENT_0001*,
    _Out_ D3D12DDI_PRESENT_0051*, _Out_opt_ D3D12DDI_PRESENT_CONTEXTS_0051*, _Out_opt_ D3D12DDI_PRESENT_HWQUEUES_0051* );
```
`D3D12DDIARG_PRESENT_0001` (umddi:1630-1644) is nearly the D3D11 `DXGI_DDI_ARG_PRESENT`
(`phSurfacesToPresent`, `hDstResource`, `Flags`, `FlipInterval`, `VidPnSourceID`, dirty rects,
private driver data). The driver **outputs** the KM allocation handles and the context(s)
(`D3D12DDI_PRESENT_0051 { BroadcastSrcAllocation[]; BroadcastDstAllocation[]; AddedGpuWork;
BackBufferMultiplicity; SyncIntervalOverrideValid; SyncIntervalOverride; }`, umddi:7226-7235;
`D3D12DDI_PRESENT_CONTEXTS_0051 { HANDLE hContext; ... }`, umddi:7237-7242). Runtime validations:
`Driver provided too many contexts for present.` (d3d12core:12130), `Driver set invalid sync
interval override.` (d3d12core:12168). **This is materially better news than DX12.md §2(b)
suggests: a D3D12 UMD gets the destination surface hand-off that a bare Vulkan ICD never gets.**
Handed to R7.

---

## 3. Comparison with the D3D11 path Helios already satisfies

| Aspect | D3D11 (`d3d10umddi`) as Helios uses it | D3D12 (`d3d12umddi`) |
|---|---|---|
| Object private memory | runtime-allocated, driver-sized | **same** (umddi:1921-1922 pattern) |
| Handle encoding | `pDrvPrivate` word; Helios stores a bare COM ptr (`umd/src/forward/handles.rs:1-60`) | **same** (`D3D12DDI_HDEVICE = D3D10DDI_HDEVICE`, umddi:25) |
| DDI ≈ API isomorphism | very high — that is why the DXVK forward works | **high for command recording, LOW for the object graph**: PSO from handles, root signature parsed, descriptor heaps driver-owned, heap+resource fused |
| Who owns the WDDM context | UMD, one per device (`umd/src/device_funcs.rs:1046-1094`) | UMD, **one per command queue** (d3d12core:10597) |
| Who submits | UMD (`pfnRenderCb`, `umd/src/forward/present.rs:795`) | UMD (`pfnSubmitCommandCb`/`pfnRenderCb`, d3d12core:11939-11944) |
| Real work location | out-of-band via DXVK → Vulkan → venus; DMA buffer is ceremonial | can be **identical** via vkd3d → Vulkan → venus |
| GPU completion → WDDM fence | KMD queues the DMA fence behind outstanding venus work (`kmd_render/src/ddi/submit_command.rs:720-724`); exact-boundary watermark decoded from DMA private data (`:504, 628-646`) | **the same machinery is what a D3D12 fence needs** |
| Caps | one contract validated at `CDevice::LLOCompleteLayerConstruction` (`umd/src/caps.rs:30-43`) | **48 caps types**, ~60 distinct validation failures, tier/shader-model cross-checks |
| Error reporting | HRESULT returns + `pfnSetErrorCb` | many DDIs are `VOID`; errors go through `pfnSetErrorCb` / `pfnSetCommandListErrorCb` only |

---

## 4. Practical blockers specific to bridging to vkd3d-proton

### 4.1 vkd3d's Windows DLL exports nothing usable — the fork must build it as a library

`vkd3d-proton-helios/libs/d3d12core/d3d12core.def`, verbatim, in full:
```
LIBRARY d3d12core.dll

EXPORTS
    D3D12GetInterface
    D3D12SDKVersion DATA PRIVATE
```
and `libs/d3d12/*.def` exports only the eight public `D3D12*` API entry points. So **none** of
`vkd3d_create_device`, `vkd3d_get_vk_device`, `vkd3d_acquire_vk_queue`,
`vkd3d_serialize_root_signature` (`include/vkd3d.h:104-142`) is reachable from a separate DLL.

The DXVK precedent is the answer: `dxvk-helios/` is **linked into `helios_umd.dll`** behind the cxx
bridge, not consumed as `d3d11.dll`. The vkd3d analogue is to link the static `libvkd3d` into
`helios_umd.dll` and drive it through `vkd3d_create_device()`. That is a fork change to
`vkd3d-proton-helios` build files — and DX12.md §1.3 records that the submodule's checked-out
`origin` is upstream, not the `.gitmodules` fork URL, so **the push remote must be wired up first**.

### 4.2 The interop surface exists and is adequate

`include/vkd3d.h:104-142` provides `vkd3d_create_instance`, `vkd3d_create_device`,
`vkd3d_get_vk_device(ID3D12Device*)`, `vkd3d_get_vk_physical_device`,
`vkd3d_get_vk_queue_family_index/index/flags`, `vkd3d_acquire_vk_queue(ID3D12CommandQueue*)` /
`vkd3d_release_vk_queue`, `vkd3d_lock_vk_queue`/`unlock`, `vkd3d_resource_incref/decref`,
`vkd3d_serialize_root_signature`, `vkd3d_serialize_versioned_root_signature`,
`vkd3d_get_vk_format` / `vkd3d_get_dxgi_format`. Combined with the Helios ICD's private export
`venus_register_present_stream(VkDevice, VkSemaphore, uint64_t* out_cookie)`
(`umd/bridge/bridge_icd_exports.h:37-42`), **the pieces needed to turn a vkd3d submission into a
KMD-visible watermark are all present in the tree today**.

vkd3d also already talks D3DKMT on Windows for adapter identity:
`libs/vkd3d/d3dkmt.c:25-41` (`d3d12_device_open_kmt` → `D3DKMTOpenAdapterFromLuid` +
`D3DKMTCreateDevice`).

### 4.3 The one piece with no existing answer: a per-`ExecuteCommandLists` completion watermark

vkd3d's queue is asynchronous (`d3d12_command_queue_ExecuteCommandLists`,
`libs/vkd3d/command.c:22764`, with an internal submission thread). To emit a watermark the
forwarder must either (a) `ID3D12CommandQueue::Signal` an internal `ID3D12Fence` after each
forwarded ECL and translate that fence's Vulkan timeline into the ICD present-stream cookie, or
(b) reach the `VkQueue` through `vkd3d_acquire_vk_queue` and signal an extra timeline semaphore
itself. Neither exists; (a) is the smaller change and keeps vkd3d's ordering guarantees.

### 4.4 ABI: struct-return convention

Noted in §2.5: `PFND3D12DDI_GET_{CPU,GPU}_DESCRIPTOR_HANDLE_FOR_HEAP_START` return the handle
struct **by value** (umddi:1926-1927); vkd3d's C implementation returns via hidden pointer
(`libs/vkd3d/resource.c:9146-9167`). Same class as the 52nd-session `bridge_guard` truncation bug.

---

## 5. UNVERIFIED items, each with the experiment that settles it

### 5.1 ★ Does a WDDM monitored fence advance on this adapter without any GPU-side write?

**Why it matters:** every `ID3D12Fence` is a monitored fence; the D3D12 runtime reads its value
from `FenceValueCPUVirtualAddress` (`d3dkmthk.h:1707`) and from `D3DKMTWaitForSynchronizationObjectFromCpu`.
Helios has no guest GPU to write `FenceValueGPUVirtualAddress`, and the KMD implements neither
`DxgkDdiSignalMonitoredFence` nor the native-fence family (DX12.md §3.5). If dxgkrnl's software
scheduler does **not** write the value itself when it retires a queued monitored-fence signal
packet on a software-scheduled context, then *every* strategy-(a) D3D12 fence hangs, and the whole
"native D3D12 UMD" strategy dies here.

**Settling experiment (no D3D12 code needed, ~half a day):** extend
`tools/vehicle_flipwait_probe.c` (already proves VidSch honours a queued `WAIT(F>=1)` before
`SIGNAL(G=5)` on this adapter, per DX12.md:316-318) into a standalone D3DKMT probe under `tools/`
that:
1. `D3DKMTCreateDevice` + `D3DKMTCreateContextVirtual` on the Helios adapter (NodeOrdinal 0);
2. `D3DKMTCreateSynchronizationObject2` with a **monitored fence**, capturing
   `FenceValueCPUVirtualAddress` / `FenceValueGPUVirtualAddress`;
3. `D3DKMTSubmitCommand` an empty DMA buffer, then
   `D3DKMTSignalSynchronizationObjectFromGpu` for value 1 on that context;
4. poll `*FenceValueCPUVirtualAddress` and `D3DKMTWaitForSynchronizationObjectFromCpu`.
**Pass:** the CPU-visible value reaches 1 without anything writing the GPU VA.
**Fail:** it never advances ⇒ Helios' KMD needs a monitored-fence notification path
(`DXGKCB_NOTIFY_INTERRUPT` with `DXGK_INTERRUPT_MONITORED_FENCE_SIGNALED`) before D3D12 is possible
at all. Run in session 1 via a cloned scheduled task (memory: 60th session).

### 5.2 Does the runtime, not the driver, perform the kernel signal/wait for `pfnSignalFence`/`pfnWaitForFence`?

**Current status:** inferred (high confidence) from (a) the driver never receiving a
`D3DKMT_HANDLE` for the fence (umddi:1594-1598) and (b) `PhysicalAdapterMask` being documented as
`// Out: The set of adapters to broadcast the operation to` (umddi:2716).
**Settling experiment:** once `OpenAdapter12` returns a table, implement `pfnSignalFence` as a
counter-only no-op that sets `PhysicalAdapterMask = 1`, and take a
`Microsoft-Windows-DxgKrnl` all-keywords ETW slice (recipe in ROADMAP.md, and the same provider
that found the WS2 present-queue stall) around one `ID3D12CommandQueue::Signal`. If
`SignalSynchronizationObjectFromGpu` packets appear on the queue's context with no driver call
between, the runtime does it. Cheaper alternative that needs no D3D12 code: run any D3D12 app on
**WARP** (`C:\Windows\System32\d3d10warp.dll`, which does export `OpenAdapter12` — verified by
string scan) and take the same ETW slice; WARP is a UMD whose DDI traffic is real even though its
execution is software.

### 5.3 Will the runtime accept GPU virtual addresses the driver never obtained from the kernel?

**Why it matters:** a forwarder would return vkd3d's Vulkan BDA from
`pfnCheckResourceVirtualAddress` (umddi:2476) and never call `pfnReserveGpuVirtualAddressCb` /
`pfnMapGpuVirtualAddressCb`.
**Settling experiment:** report `MaxGPUVirtualAddressBitsPerResource = 40`
(matching `kmd_render/src/ddi/gpummu.rs`), return BDAs, and run a D3D12 sample from
`dx-samples-research-only/Samples/Desktop/` **with the D3D12 debug layer enabled**
(`D3D12_FEATURE_...`, `d3d12SDKLayers.dll`), watching for `MaxGPUVirtualAddressBitsPerResource
error` (present as a string in D3D12Core, d3d12core:22509) and for any GPU-VA validation break.
If the debug layer independently tracks VA ranges per resource, the addresses only have to be
self-consistent and in range — which BDAs are.

### 5.4 What is the oldest `D3D12DDI_SUPPORTED_xxxx` this Windows build accepts?

**Why it matters:** it sets the floor on the surface a first implementation must cover — DDI `0003`
has a 105-ish-entry core table with no mesh shaders, no work graphs, no state objects, no enhanced
barriers; DDI `0109` has 124 entries and drags in every tier.
**Settling experiment:** implement only `OpenAdapter12` + `pfnGetSupportedVersions` returning a
single old token (e.g. `D3D12DDI_SUPPORTED_0040`) and `pfnGetCaps` refusing everything; call
`D3D12CreateDevice` and read the runtime's ETW / `SetErrorCb` reason. This is a ~200-line
experiment and it is the **cheapest** way to size the whole project. ⚠ It must be written so that
`OpenAdapter12` stops refusing in the *same commit* that makes the code reachable (DX12.md §5.1,
R908).

### 5.5 Does the runtime tolerate a DMA buffer with zero GPU commands at `ExecuteCommandLists`?

**Current status:** strongly suggested by the D3D11 path already doing it
(`umd/src/forward/present.rs:795`, `kmd_render/src/ddi/submit_command.rs:720-724`), but never
demonstrated for D3D12.
**Settling experiment:** covered as a by-product of 5.4 + a trivial clear-only sample; the
observable is that `DxgkDdiSubmitCommandVirtual`'s counters move
(`tools/kmd-counter-snapshot.ps1` diff, verified to move *this boot*) and no `0x119 Arg1=2`
bugcheck occurs.

### 5.6 Do `pfnFillDDITable`'s size argument and the runtime's table structs match bindgen's?

**Current status:** unverified for D3D12; the D3D11 precedent (R702: 24H2 passing 576 B for a
592 B `DRIVERCAPS`) says this *will* bite.
**Settling experiment:** log `SIZE_T` at every `pfnFillDDITable` call and compare against
`size_of::<D3D12DDI_DEVICE_FUNCS_CORE_xxxx>()` from a bindgen'd header with layout assertions
(R802 discipline). Never write past the runtime-supplied size.

### 5.7 Ordinal→name map for D3D12Core's 96 delay-imported D3DKMT entry points

I established that `D3D12Core.dll` delay-imports **96 functions by ordinal** from
`ext-ms-win-dx-d3dkmt-dxcore-l1-1-{0,1,3,4,5}.dll` (PE delay-import directory parsed on the VM;
90 from `-l1-1-0` alone) and statically imports **no** `gdi32`/D3DKMT symbols. I could **not**
resolve those ordinals: neither `gdi32.dll` (995 named exports) nor `gdi32full.dll` (1 007) matches
the ordinal numbering, because the API-set virtual DLLs export by ordinal only.
**Settling experiment:** resolve the API set with `ApiSetQueryApiSetPresenceEx` or read
`C:\Windows\System32\apisetschema.dll`'s namespace to find the host, then map ordinals against the
host's export table. Worth doing only if §5.2's ETW answer is ambiguous.

---

## 6. Verdict table

| # | Area | Forwardable? | Shadow state the Helios UMD must keep | Risk |
|---|---|---|---|---|
| 1 | Device / adapter / queue creation & lifetime | **FORWARDABLE** | vkd3d `ID3D12Device`; per-queue `{ID3D12CommandQueue, WDDM hContext, cmd/alloc/patch windows, queue flags, hRTCommandQueue}`; paging queue | LOW |
| 2 | Command allocators / pools / recorders / lists / bundles | **FORWARDABLE WITH SHADOW STATE** | pool→`ID3D12CommandAllocator`; recorder→current pool; list→`ID3D12GraphicsCommandList` + last-Reset recorder; per-list DDI-table identity for `pfnSetCommandListDDITableCb` | MEDIUM (volume: 75 CL entry points) |
| 3 | `ExecuteCommandLists` + kernel submission | **FORWARDABLE WITH SHADOW STATE** | per-queue monotonic watermark; DMA private-data marker layout shared with `kmd_render`; empty-DMA-buffer submit bookkeeping | HIGH (concentrated in §4.3) |
| 4 | Fences (`ID3D12Fence`, queue signal/wait, shared) | **FORWARDABLE WITH SHADOW STATE — conditional on §5.1** | fence handle → `{GPU VAs, per-adapter mask, internal vkd3d ID3D12Fence, last requested value}` | **HIGH — decides the strategy** |
| 5 | Descriptor heaps | **FORWARDABLE** | heap handle → `ID3D12DescriptorHeap` only; handle *values* pass through unchanged | LOW-MEDIUM (struct-return ABI) |
| 6 | Resources / heaps / placed & reserved / GPU VA | **FORWARDABLE WITH SHADOW STATE** | resource handle → `{ID3D12Resource, D3DKMT_HANDLE from pfnAllocateCb, VA, DDI create-args}`; heap handle → `ID3D12Heap` | HIGH (KM identity + VA acceptance, §5.3) |
| 7 | Residency / MakeResident / Evict / budgets | **FORWARDABLE** | paging-queue handle; per-allocation residency bookkeeping only if `E_PENDING` is ever returned | LOW |
| 8 | Root signatures, PSOs, PSO libraries, state objects | **FORWARDABLE WITH SHADOW STATE** | shader blobs per handle; blend/rasterizer/DS/element-layout descs per handle; **re-serialized root-signature blob** per handle | MEDIUM-HIGH |
| 9 | Barriers and resource state | **FORWARDABLE** | none beyond the resource handle map | LOW |
| 10 | Multi-queue COPY / COMPUTE, engine nodes | **FORWARDABLE, degraded to one node** | queue-flags→NodeOrdinal policy (all → node 0) | MEDIUM (caps honesty) |
| 11 | Debug layer / SDK layers | **must be designed in** | debug-mode private sizes; `HANDLE_AND_TYPE` → `{D3DKMT_HANDLE, offset, size}` map for `pfnGetDebugAllocationInfo` | MEDIUM |
| — | Present (R7's lane; recorded) | **FORWARDABLE**, and better than the bare-Vulkan path | present-context + KM allocation handles per swapchain buffer | (R7) |

---

## 7. Overall verdict

**A "`d3d12umddi` frontend → vkd3d-proton `ID3D12` COM" bridge is architecturally possible but
costly, and it is gated on one unresolved kernel question, not on the DDI shape.**

The reasoning:

1. **The DDI is not lower-level than D3D11 in the way that would break a forward.** The thing that
   would kill it — the runtime handing the driver a buffer and demanding hardware command packets
   in it — **does not happen**. Every recording object (`D3D12DDIARG_CREATE_COMMAND_POOL_0040` is
   *one flags word*, umddi:6633-6636) is driver-private, and `pfnExecuteCommandLists` returns
   `VOID` and takes only driver handles (umddi:1735-1739). The driver is free to record into a
   `ID3D12GraphicsCommandList` it owns and submit an empty DMA buffer — which is **precisely what
   Helios' D3D11 UMD does today**, with the KMD-side honesty already engineered in
   (`kmd_render/src/ddi/submit_command.rs:720-724`: "the fence is NOT lied about: it queues behind
   the venus work outstanding at submit time").

2. **The object graph, not the command stream, is where the cost is.** Root signatures arrive
   parsed and must be re-serialized; PSOs arrive as handle bundles and must be reassembled;
   heaps and resources are fused into one DDI; descriptor heaps are entirely driver-owned. None of
   these is a *model* mismatch — each is a translation with a known target. The volume is
   ~206 entry points across three mandatory tables plus a 48-type caps contract with ~60 distinct
   runtime-enforced consistency rules, against the ~220-function D3D11 forward that took this
   project from Gate 5b to a composited desktop.

3. **The gate is §5.1.** Whether a monitored fence can advance on this adapter with no GPU-side
   write is a question about `dxgkrnl` + `kmd_render`, not about vkd3d or the DDI, and it is
   answerable *today* with a D3DKMT probe and no D3D12 code. If the answer is no, strategy (a)
   requires new KMD fence machinery before any UMD work is worth starting — and note that
   **strategy (b) (vkd3d as `d3d12.dll` over the Vulkan ICD) is not affected by it at all**, since
   nothing in that path creates a monitored fence.

4. **Practically, the DXVK precedent transfers but the packaging does not.** vkd3d-proton's Windows
   DLLs export only `D3D12GetInterface` (`libs/d3d12core/d3d12core.def`), so a bridge requires
   linking `libvkd3d` into `helios_umd.dll` and a fork with a working push remote (DX12.md §1.3).
   That is the same shape as `dxvk-helios/`, so it is known work, but it is work.

**Confidence: MEDIUM-HIGH on the DDI-shape verdicts** (they are read directly from the header and
corroborated by the runtime's own validation strings), **LOW-MEDIUM on the end-to-end verdict**,
because §5.1, §5.3 and §5.4 are unanswered and any one of them can move the cost by a large factor.

**What would change my mind, in each direction:**

- *Toward "dead end":* §5.1 failing (monitored fences do not advance without a GPU writer) **and**
  the KMD notification path proving expensive; or §5.3 failing in a way that forces the driver to
  own a guest VA space that maps to venus ids — DX12.md §3.4 is right that that would be a large,
  novel subsystem with no host counterpart.
- *Toward "architecturally sound":* §5.4 showing the runtime still accepts an early DDI revision
  (say `0040`), which would cut the mandatory surface by roughly a third and remove work graphs,
  state objects, enhanced barriers, mesh shaders and sampler feedback from the first milestone;
  plus §5.1 passing. In that world the bridge is a bigger DXVK-shaped job, not a different kind of
  job.
- *Toward "don't do it at all":* the D0/D1 experiments in DX12.md §4 showing upstream
  vkd3d-proton already runs and presents on the Helios ICD. Strategy (b) then delivers D3D12 with
  **zero** DDI work, and the only thing a native UMD buys is `D3D12CreateDevice` working for
  unmodified apps without DLL replacement. That is a real benefit — but it should be paid for
  knowingly, after (b) is measured, not before.

---

## 8. Artifacts produced by this lane

- `/home/rupansh/helios-vgpu/tmp/dx12/research/R2-runtime-contract.md` — this file.
- `/home/rupansh/helios-vgpu/tmp/dx12/research/d3d12core-strings.txt` — 25 782 unique ASCII strings
  extracted from `C:\Windows\System32\D3D12Core.dll` (10.0.26100.8737). **The only conceptual
  documentation of the D3D12 UMD DDI contract that exists.** Read-only extraction; nothing on the
  VM was modified.
- `/home/rupansh/helios-vgpu/tmp/dx12/research/d3d12core-driverstrings.txt` — the 270-line
  `Driver|driver|DDI` subset.
