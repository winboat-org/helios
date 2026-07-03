# Phase 5 Handover — Mesa venus → Windows ICD over Helios IOCTLs

> **✅ DONE (2026-06-05) — this brief was executed; Phase 5 works end-to-end.** The venus ICD
> (`vn_renderer_helios.c` + the two Mesa edits) is written, committed to the fork, and validated on
> real hardware: `vulkaninfo` shows `driverName venus` (Intel ARL + llvmpipe), `vkCreateDevice` +
> host-visible `vkAllocateMemory`/`vkMapMemory` work, and a `vkCmdFillBuffer`+`vkQueueSubmit`+fence
> round-trips real GPU output. Net deltas vs. this brief: the §2 opcode table had to be corrected
> (a missing `RESOURCE_CREATE_3D` shifted SUBMIT_3D/MAP_BLOB/UNMAP_BLOB — the values are now pinned
> to the `virtio-bindings` crate by a `cargo test`), and `info.max_timeline_count` is **64** not 1
> (§5 said 1, which fails `vkCreateDevice` — every queue needs ring_idx≥1). **The next workstream is
> Phase 4e async submission — see `icd/PHASE4E_ASYNC_HANDOVER.md`.** The rest of this document is kept
> as the historical implementation brief.

**Status: DONE (was: the implementation brief).** Everything below is concrete and
verified against the actual trees (the Helios KMD on `master`, and Mesa at the pinned submodule
commit) so the implementing agent does **not** need to re-derive it. Read this top-to-bottom, then
ARCH.md §5/§6, then start at §6 ("The first three edits").

Related memory (read these too): `mesa-venus-icd-port` (the plan + toolchain), `phase4-blob-plan`
(the blob/MAP_BLOB mechanism + the host-coupling reality), `kmdf-rust-bindings`, `venus-host-blocker`.

---

## 1. Goal & big picture

Port Mesa's **venus** Vulkan driver (`-Dvulkan-drivers=virtio`, `driverName="venus"`) to a **Windows
ICD DLL** whose `vn_renderer` backend talks to the Helios KMD via **`DeviceIoControl` on
`GUID_DEVINTERFACE_HELIOS`** — replacing venus's Linux virtgpu-DRM backend. Everything above
`vn_renderer` (vn_instance/ring/cs, the byte-correct `vn_protocol_driver_*` encoder) is reused
unmodified; virglrenderer's venus **decoder is also Mesa**, so the wire is compatible. This is also
the only way to validate the host-visible blob path (ALLOC_BLOB/MAP_BLOB), because a host-visible
blob must be backed by a venus `vkAllocateMemory` that only the ICD produces.

The port is **one new file + two small edits**:
- NEW `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c` — the IOCTL backend (structural template:
  `vn_renderer_vtest.c`; blob/submit/sync *semantics*: `vn_renderer_virtgpu.c`).
- EDIT `icd/mesa/src/virtio/vulkan/vn_renderer.h` — add a `_WIN32` arm to `vn_renderer_create()`.
- EDIT `icd/mesa/src/virtio/vulkan/meson.build` — compile the new file on Windows + link `setupapi`.
- Plus a `.def` (or `__declspec(dllexport)`) to export the 3 loader symbols on MSVC, and a
  registry-JSON install step.

---

## 2. What's already DONE (the KMD side the backend calls)

The KMD is a System-class KMDF driver, **built, packaged (infverif VALID), installed, on-device
verified: device Code 0, IOCTL channel round-trips, no bugcheck.** It exposes six IOCTLs on
`GUID_DEVINTERFACE_HELIOS`. Single source of truth: `protocol/src/ioctl.rs` (codes/GUID) and
`protocol/src/escape.rs` (payload structs). The ICD redeclares these as C structs (all are
`repr(C)`, padding-free, and begin with a 16-byte `HeliosEscapeHeader{magic='HELS'=0x4845_4C53,
cmd_type, version=1, size}`; the KMD validates magic+version, dispatches on the IOCTL code).

| IOCTL | Value | Method | In / Out struct (escape.rs) |
|---|---|---|---|
| `IOCTL_HELIOS_CTX_CREATE` | `0x0022E400` | BUFFERED | in/out `HeliosEscapeCtxCreate{capset_id→VENUS=4, out_ctx_id}` |
| `IOCTL_HELIOS_CTX_DESTROY` | `0x0022E404` | BUFFERED | in `HeliosEscapeCtxDestroy{ctx_id}` |
| `IOCTL_HELIOS_SUBMIT_VENUS` | `0x0022E409` | **IN_DIRECT** | buffered hdr `HeliosEscapeSubmitVenus{fence_id:u64, ctx_id:u32, buffer_size:u32}` + Venus cs bytes via the input MDL |
| `IOCTL_HELIOS_ALLOC_BLOB` | `0x0022E40C` | BUFFERED | in/out `HeliosEscapeAllocBlob{size:u64, blob_id:u64, blob_flags:u32, blob_mem:u32, ctx_id:u32, out_resource_id:u32}` |
| `IOCTL_HELIOS_MAP_BLOB` | `0x0022E410` | BUFFERED | in/out `HeliosEscapeMapBlob{out_user_va:u64, resource_id:u32}` |
| `IOCTL_HELIOS_WAIT_FENCE` | `0x0022E414` | BUFFERED | in `HeliosEscapeWaitFence{fence_id:u64, timeout_ns:u64}` |

GUID string (for SetupDi / registry): `{C8F84237-CD89-48F5-AFC5-32944524625C}`
(`ioctl.rs:101`; fields Data1=0xC8F84237, Data2=0xCD89, Data3=0x48F5, Data4={AF,C5,32,94,45,24,62,5C}).
Constants the backend needs (`protocol/src/virtio_gpu.rs`): `VIRTIO_GPU_CAPSET_VENUS=4`,
`VIRTIO_GPU_BLOB_MEM_HOST3D=2`, `VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE=1`.

**KMD semantics that shape the backend:**
- **Submit is synchronous.** `SUBMIT_VENUS` blocks until the host fence completes (WAIT_FENCE is
  trivially satisfied; async fences/WDFINTERRUPT are deferred Phase 4e). So `ops.submit` returns
  only after host completion; `ops.wait`/`sync_ops.read` can report "already signaled".
- **MAP_BLOB** maps the host-visible blob's PCI-BAR pages into the **calling process** and returns a
  user VA. It must be called from the same process/handle that opened the device, and serialized
  (the KMD comment + ARCH §12). On failure the KMD's `MmMapLockedPagesSpecifyCache(UserMode)`
  **raises an SEH exception it cannot catch** (a documented hardening TODO — a C `__try` shim);
  bounded today by a 256 MiB per-map cap. Keep maps modest and serialized.
- **`IN_DIRECT` for SUBMIT_VENUS:** the KMD reads the small `HeliosEscapeSubmitVenus` header from the
  buffered system buffer and the Venus bytes from the locked **input MDL** (`WdfRequestRetrieveInputWdmMdl`).
  **Cross-check `kmd/src/ioctl.rs::handle_submit_venus` for the exact Win32 `DeviceIoControl`
  argument mapping** (which pointer carries the header vs. the cs bytes) before wiring `helios_submit`
  — this is the one place the user-mode IN_DIRECT call must match the KMD's buffer retrieval.

---

## 3. The Mesa submodule & fork

- Mesa is vendored as a submodule at **`icd/mesa`**, pinned at `4e8595da21b6` (upstream `main`,
  2026-06-04). `.gitmodules` url → **`https://github.com/rupansh/mesa-helios.git`** (our fork; full
  upstream contributor history preserved). The submodule's local `upstream` remote points at
  `https://gitlab.freedesktop.org/mesa/mesa.git` for future syncs.
- **Our port lands as commits on the fork** (`icd/mesa` branch `main`), then the superproject's
  gitlink is bumped. To sync upstream later: `git -C icd/mesa fetch upstream && git -C icd/mesa
  merge upstream/main` (or rebase our patches).
- ⚠️ **win_cargo mirror does NOT exclude `icd/mesa`** — future `win_cargo kmd/probe` builds will
  robocopy the whole Mesa tree (~thousands of files) every call, slowing them. Fix before heavy
  Phase 5 iteration: add `icd/mesa` (and `icd/mesa/.git`) to the win-mcp mirror's robocopy `/XD`
  exclude list (`tools/win-mcp/src/main.rs`), OR build Mesa from a separate local checkout. The
  Mesa build itself is meson/ninja (not win_cargo); run it from a VS dev shell (§6.4).

---

## 4. The `vn_renderer` vtable → Helios IOCTL mapping

`struct vn_renderer { info; ops; shmem_ops; bo_ops; sync_ops; }` (`vn_renderer.h:225-231`). A backend
embeds `struct vn_renderer base` as the **first member** of its private struct and downcasts with a C
cast (exactly as vtest: struct at `vn_renderer_vtest.c:49`, downcast e.g. `:582`). Wire every op like
vtest's tail (`vn_renderer_vtest.c:1073-1097`):

| vtable entry (signature in `vn_renderer.h`) | Helios implementation |
|---|---|
| `ops.destroy` | shmem-cache fini → `CTX_DESTROY{ctx_id}` → `CloseHandle` → free. (vtest `:967`) |
| `ops.submit(vn_renderer_submit{bos[],batches[]})` | **loop `batches[]`**; per batch with `cs_size>0`: `SUBMIT_VENUS{ctx_id, fence_id=monotonic, buffer_size=cs_size}` + `cs_data`. Synchronous → on return mark `batch.syncs[j]` signaled at `sync_values[j]`. `ring_idx` dropped (single ctx/ring — TODO). `bos`/pinning ignored (vtest ignores too). (`vn_renderer.h:79-114`; ref `vtest_vcmd_submit_cmd2 :513`, but you need ONE blob per IOCTL, not vtest's multi-batch framing) |
| `ops.wait(vn_renderer_wait{timeout,syncs,sync_values})` | fast path: if all syncs' stored value ≥ requested, return `VK_SUCCESS` (synchronous submit already completed them). Else `WAIT_FENCE{fence_id, timeout_ns=timeout}`. (`vn_renderer.h:116-124`; ref `vtest_wait :890`) |
| `shmem_ops.create(size)` | `ALLOC_BLOB{blob_mem=HOST3D, blob_flags=USE_MAPPABLE, blob_id=0, size}` → `out_resource_id`, then `MAP_BLOB{resource_id}` → `out_user_va`. Return `vn_renderer_shmem{res_id=out_resource_id, mmap_ptr=(void*)out_user_va, mmap_size≥size}`. (asserts at `vn_renderer.h:284-298`; ref `vtest_shmem_create :816`) |
| `shmem_ops.destroy` | drop the mapping/resource (RESOURCE_UNREF is not yet an IOCTL — for the first cut, track and leave to CTX_DESTROY, or add the IOCTL; see §9). |
| `bo_ops.create_from_device_memory(batch,size,mem_id,flags,external,**out_bo)` | `ALLOC_BLOB{blob_mem=HOST3D, blob_flags=USE_MAPPABLE iff HOST_VISIBLE, **blob_id=mem_id**, size}` → `out_resource_id`. The `batch` param is recent (upstream "venus: let resource_create_blob wait for mem alloc"): the renderer may pass a cs batch to submit *together with* the blob create so the host's `vkAllocateMemory` is ordered before the blob binds — for the first cut, submit `batch` (if non-NULL) via `ops.submit` before/with ALLOC_BLOB. (`vn_renderer.h:148-156`; ref `vtest_bo_create_from_device_memory :747`) |
| `bo_ops.map(bo, placed_addr)` | `MAP_BLOB{resource_id=bo.res_id}` → `out_user_va`; cache in `bo.mmap_ptr` (map at most once). `placed_addr` (fixed-address request) — pass through if set, else ignore. **not thread-safe** per the vtable contract; serialize with the device mutex. (`vn_renderer.h:172`; ref `vtest_bo_map :669`) |
| `bo_ops.flush` / `invalidate` | **no-op** (HOST3D host-coherent, ARCH §5). |
| `bo_ops.create_from_dma_buf`, `export_dma_buf`, `export_sync_file` | **NULL** (vtest NULLs them). |
| `sync_ops.create/destroy/reset/read/write` | guest `fence_id ↔ KEVENT` model. For the synchronous first cut, a sync is a small struct holding a `signaled_value`; `read` returns it, `write/reset` set it, `create` allocs it. `wait` (via `ops.wait`) → `WAIT_FENCE`. (ref `vtest_sync_* :578-649`) |
| `sync_ops.create_from_syncobj`, `export_syncobj` | **NULL** (no external sync). |
| `info` (vn_renderer_info) | **hardcode** — see §5 (Helios has no GET_CAPSET IOCTL). |

Device open (in `vn_renderer_create_helios`, mirroring `vn_renderer_create_vtest :1103`): replace the
vtest socket (`vtest->sock_fd`) with `SetupDiGetClassDevs(&GUID_DEVINTERFACE_HELIOS, NULL, NULL,
DIGCF_PRESENT|DIGCF_DEVICEINTERFACE)` → `SetupDiEnumDeviceInterfaces` → `SetupDiGetDeviceInterfaceDetailW`
→ `CreateFileW(path, GENERIC_READ|GENERIC_WRITE, FILE_SHARE_READ|FILE_SHARE_WRITE, OPEN_EXISTING)`,
storing the `HANDLE` where vtest kept `sock_fd`, guarded by a mutex (vtest `sock_mutex`). Then
`CTX_CREATE{capset_id=VENUS}` (analog of `vtest_vcmd_context_init :1069`) so the single ctx exists
before the first submit. (The `probe/src/main.rs` already has a working SetupDi+CreateFile open in
Rust — mirror its logic in C.)

---

## 5. `vn_renderer_info` — HARDCODE it (the GET_CAPSET gap)

These fields are validated by `vn_instance_init_renderer` (`vn_instance.c:169-236`) **before any host
traffic**; wrong values → `vkCreateInstance` returns a stub (`VK_ERROR_INITIALIZATION_FAILED`) and you
get *no* host log. Both reference backends fill `info` from a `GET_CAPSET(VENUS)` call —
**Helios has no GET_CAPSET IOCTL**, so hardcode (the host already negotiated the venus capset at
CTX_CREATE; over-reported versions are clamped down by the driver):

```c
info.wire_format_version = 1;                         /* MUST == vn_info_wire_format_version() (==1) */
info.vk_xml_version = VK_MAKE_API_VERSION(0,1,3,0);   /* >= 1.1 and clamped down; 1.3 is safe */
info.vk_ext_command_serialization_spec_version = 1;   /* clamped to driver max */
info.vk_mesa_venus_protocol_spec_version = 4;         /* driver knows spec v4 */
info.vk_extension_mask[0] = 0;                        /* bit0 clear => "all extensions supported" (venus_hw.h:40) */
info.max_timeline_count = 1;                          /* single CPU timeline (synchronous model) */
info.has_dma_buf_import = false;
info.has_external_sync = false;
info.has_implicit_fencing = false;
info.has_guest_vram = false;
info.pci.vendor_id = 0x1af4; info.pci.device_id = 0x1050; info.pci.has_bus_info = false;
info.id.has_luid = false;    /* set has_luid + luid later for DXVK D3DKMT interop (Phase 6) */
```
(The exact gate values come from `vn_protocol_driver_info.h` — `wire_format_version`=1,
`vk_xml_version`=`VK_MAKE_API_VERSION(0,1,4,343)`, protocol spec=4 — and the capset struct
`virgl_renderer_capset_venus` is `src/virtio/virtio-gpu/venus_hw.h:29-72`.) The real fix later is a
7th IOCTL `IOCTL_HELIOS_GET_CAPSET` returning `virgl_renderer_capset_venus` from
`VIRTIO_GPU_CMD_GET_CAPSET`; not required for `vkCreateInstance`/`vkEnumeratePhysicalDevices`.

---

## 6. The first three edits (do these in order)

### 6.1 `vn_renderer.h` (lines ~233-261) — add the Windows create arm
Add the prototype (guard `#ifdef _WIN32`) after the `vn_renderer_create_vtest` prototype, and change
the selector inline:
```c
#if defined(_WIN32)
   return vn_renderer_create_helios(instance, alloc, renderer);
#elif defined(HAVE_LIBDRM)
   if (VN_DEBUG(VTEST)) { ... vtest fallback ... }
   return vn_renderer_create_virtgpu(instance, alloc, renderer);
#else
   return vn_renderer_create_vtest(instance, alloc, renderer);
#endif
```
(`_WIN32` is defined by cl and clang-cl; no `-D` needed.) Without this, Windows falls through to
`vn_renderer_create_vtest`, which is not compiled on Windows → link error.

### 6.2 `src/virtio/vulkan/meson.build` (lines ~109-118) — compile the backend on Windows
The current block compiles **no** backend on Windows (`vtest` is `if not with_platform_windows`,
`virtgpu` is `if system_has_kms_drm`). Add:
```meson
if with_platform_windows
  libvn_files += files('vn_renderer_helios.c')
  vn_deps += cc.find_library('setupapi')   # SetupDi*, the device-interface enumeration
  # cfgmgr32 only if you use CM_Get_Device_Interface_List instead of SetupDi*
endif
```
No `meson.options` edit (`virtio` is already a `vulkan-drivers` choice); no `src/meson.build` edit
(the `if with_virtio_vk → subdir('virtio/vulkan')` chain is already correct). WSI (`vn_wsi.c` +
`-DVN_USE_WSI_PLATFORM`) is **already enabled on Windows and links cleanly** (it uses the common
`wsi_common_win32.cpp` via `idep_vulkan_wsi`; needs DirectX-Headers — see §6.4 gates) — do NOT stub it.

### 6.3 Write `vn_renderer_helios.c`
Structurally clone `vn_renderer_vtest.c`: `struct helios { struct vn_renderer base; struct
vn_instance *instance; HANDLE dev; mtx_t dev_mutex; uint32_t ctx_id; struct vn_renderer_shmem_cache
shmem_cache; ... }`; a `helios_init` that opens the device + `CTX_CREATE` + fills `info` (§5) + wires
the four ops tables (§4); and `vn_renderer_create_helios` mirroring `vn_renderer_create_vtest`
(`:1103`). One small `DeviceIoControl` helper. MSVC-compilable (`#include <windows.h>`,
`<setupapi.h>`, `<initguid.h>`+`<cfgmgr32.h>` as needed). Re-declare the Helios structs/IOCTL codes
from §2 (or generate a shared C header from `protocol/` — a future nicety).

### 6.4 Build — VALIDATED 2026-06-04 (configures + compiles from `Z:\`, NO robocopy)

**Build from the share directly — confirmed working.** meson reads `Z:\icd\mesa` and cl compiles to a
local C: build dir; the 9p share is fine for the compiler's reads (only cargo/wdk artifact *writes*
fail on it). **187 objects compiled** (the whole Vulkan runtime + util + WSI incl. `wsi_common_win32.cpp`
+ DirectX-Headers + zlib) before stopping at the venus portability gap (below). So `icd/mesa` is
EXCLUDED from the `win_cargo` mirror and Mesa is built via the new **`win_meson`** MCP tool (runs meson
under vcvars; src `Z:\icd\mesa` → build `C:\Users\Rupansh\helios-mesa-build`).

**The exact working `meson setup` (corrected from the draft above — `gallium-va`/`gallium-vdpau` are
gone, folded into `-Dvideo-codecs=`):**
```
meson setup C:\Users\Rupansh\helios-mesa-build Z:\icd\mesa ^
  -Dvulkan-drivers=virtio -Dgallium-drivers= -Dplatforms=windows -Dvideo-codecs= ^
  -Dvulkan-layers= -Degl=disabled -Dgbm=disabled -Dglx=disabled -Dopengl=false ^
  -Dgles1=disabled -Dgles2=disabled -Dllvm=disabled -Dshader-cache=disabled ^
  -Dbuild-tests=false -Dperfetto=false --buildtype=debugoptimized
```
`-Degl=disabled` is REQUIRED (else `src/egl/meson.build` errors on `libgallium_wgl` since gallium is
empty). `-Dvulkan-drivers=virtio` is mandatory (`auto`→`[]` on Windows, meson.build:282). Via `win_meson`:
`win_meson(["setup","C:\\Users\\Rupansh\\helios-mesa-build","Z:\\icd\\mesa","-Dvulkan-drivers=virtio",...])`
then `win_meson([])` (defaults to `compile -C` the build dir).

**Configure gates ACTUALLY hit + how they were cleared (all done on win11 already):**
1. Python deps in the meson interpreter: `pip install mako pyyaml packaging setuptools` (3.12 dropped
   `distutils` → `packaging` is required; mako + pyyaml for codegen). ✅ installed.
2. **pkgconf NOT needed** — meson falls back to CMake, which found+built DirectX-Headers from its wrap
   (`subprojects/DirectX-Headers.wrap`, auto-downloaded) and zlib from its wrap. ✅
3. No Vulkan-Headers gate hit (vk.xml is vendored; the lite runtime supplies headers). ✅
4. No Rust/bindgen (venus is pure C with this option set). ✅
`meson setup` returns 0 cleanly.

### 6.5 ✅ venus compiles on Windows — SOLVED (validated 2026-06-05)

venus had never been built on Windows (vtest excluded, virtgpu Linux-only). MSVC `cl` fails on venus's
GNU-isms (C11 atomics need `/experimental:c11atomics`; `void*` pointer arithmetic → `C2036` all over
`vn_cs.h`/`vn_ring.h` + the *generated* `venus-protocol/vn_protocol_driver_*.h`; `pid_t`). **gcc and clang
accept all of those natively** (`void*` arithmetic + C11 `_Atomic` are GNU extensions), so the fix is the
toolchain, not patching venus. **Both mingw-w64 gcc 16.1 and clang-cl 17 compile 100% of venus (every
`vn_*.c` + the generated protocol headers) with ZERO edits to the Mesa tree**, reaching the link step
(only the expected undefineds remain: `vn_renderer_create_vtest` = the unwritten backend, and the
SPIR-V→NIR `vtn_*` from `vk_util.c`).

The only out-of-tree glue is **one force-included header, `icd/win-build/helios_win_compat.h`** (NOT in
the Mesa submodule), providing `pid_t`, the clang-cl interlocked-intrinsic aliases, and the
`sync_wait`/`sync_valid_fd` libsync stubs — each block self-gating. (The sync stubs are PLACEHOLDERS for
the IOCTL `WAIT_FENCE` path the backend will add.)

**RECOMMENDED: mingw-w64 gcc** (`icd/win-build/mingw-native.ini`). Why it beats clang-cl:
- Builds **straight from `Z:\`** (no robocopy) — matches `win_meson`. clang-cl must build from a **local
  C: source mirror** (clang's `#include`-once dedup needs stable inode identity the 9p share lacks → the
  earlier `#include nested too deeply` on zlib was *this*, not a zlib/venus bug).
- **One** forced-include (gcc has pid_t/atomics/interlocked natively); clang-cl additionally needs
  `-D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH` (MSVC 14.44 STL gates Clang≥19) + the SDK `rc.exe` pinned
  (not `llvm-rc`). Two of clang-cl's needs retire only by moving to LLVM≥19.
- Install (done on win11): `winget install --id BrechtSanders.WinLibs.POSIX.UCRT --exact` → gcc 16.1.0,
  native Windows target (`x86_64-w64-mingw32`), bundles pkgconf/ninja/widl/dlltool/windres.

**Build (mingw) — via the `win_meson` MCP tool (configured for mingw + the shim):**
```
win_meson(["setup","C:\\Users\\Rupansh\\helios-mesa-build","Z:\\icd\\mesa",
  "--native-file","Z:\\icd\\win-build\\mingw-native.ini",
  "-Dc_args=-includeZ:\\icd\\win-build\\helios_win_compat.h", <the §6.4 option set>])
win_meson([])   # compile
```
The configure gates from §6.4 still apply (mako/pyyaml/packaging/setuptools; CMake builds DirectX-Headers
+ zlib from wraps). **Deployment note (mingw):** static-link the runtime
(`-static-libgcc -static-libstdc++ -Wl,-Bstatic,-lwinpthread`) so `vulkan_virtio.dll` has no mingw-runtime
DLL deps; the Vulkan loader then loads it like any ICD (the exported `vk_icd*` symbols are the C ABI,
toolchain-agnostic — DXVK/VKD3D in Phase 6 are unaffected). clang-cl alternative: `icd/win-build/clang-cl-native.ini`.

So the venus-compile blocker is **closed**; the genuine remaining Phase 5 work is writing
`vn_renderer_helios.c` (defines `vn_renderer_create`, replacing the vtest backend symbol) and resolving
the `vtn_*` SPIR-V→NIR link dep (link libvtn, or confirm the IOCTL transport path doesn't need
`vk_spec_info_to_nir_spirv`). The KMD/IOCTL side is done and waiting.

---

## 7. DLL exports (MSVC) + ICD registration

- **Exports:** the loader finds 3 symbols by `GetProcAddr` on the DLL: `vk_icdGetInstanceProcAddr`
  (defined `vn_icd.c:15`), `vk_icdNegotiateLoaderICDInterfaceVersion` + `vk_icdGetPhysicalDeviceProcAddr`
  (defined in the shared runtime `src/vulkan/runtime/vk_instance.c:565,582`, linked in; ICD interface
  version is hardcoded **7**). On Linux these are exported via `-Bsymbolic`+visibility; **on MSVC you
  must add a `.def` file** (or `__declspec(dllexport)`) listing those 3 names, wired into the
  `shared_library('vulkan_virtio', ..., vs_module_defs: 'vn_icd.def')`. If they aren't exported,
  `vulkaninfo` reports the ICD as failing to load with no other diagnostic.
- **Artifact:** `vulkan_virtio.dll` (Windows `libname_prefix=''`). Mesa auto-generates the manifest
  JSON via the `virtio_icd` / `virtio_devenv_icd` custom_targets (`src/virtio/vulkan/meson.build:20-54`).
  For the dev loop, `meson devenv -C <build>` exports `VK_DRIVER_FILES` → `virtio_devenv_icd.<cpu>.json`
  (library_path = the just-built DLL) — fastest `vulkaninfo` smoke test.
- **Registration (deploy):** write a `REG_DWORD` under `HKLM\SOFTWARE\Khronos\Vulkan\Drivers` whose
  **name** is the absolute path to the manifest JSON, data `0` (enabled). JSON:
  `{"file_format_version":"1.0.0","ICD":{"library_path":"<abs path to vulkan_virtio.dll>","api_version":"1.3"}}`.
  The KMDF universal INF cannot write this — an ICD installer/postinstall script does it (independent
  of the PnP device). Precedent: lavapipe + SwiftShader enumerate via exactly this with no display
  adapter. (Or just set `VK_DRIVER_FILES`/`VK_ICD_FILENAMES` env for testing.)
- **Helios installer:** use `Z:\tools\install-helios-icd.ps1 -PlanOnly -NoSmoke`, then
  `Z:\tools\install-helios-icd.ps1` after every Mesa ICD rebuild. It copies the DLL to a
  content-hashed ProgramData path (`vulkan_virtio-<hash>.dll`), atomically rewrites
  `C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json`, validates the JSON, removes stale
  Helios/Virtio registry entries, and registers that ProgramData JSON under the Khronos Vulkan
  Drivers key. Old hashed DLLs are retained unless `-PruneOld` is passed, which avoids overwrite
  failures when a previous ICD DLL is still mapped.
- The WDDM UMD/DXVK bridge follows those same ICD manifests to locate Helios-specific ICD exports
  (`helios_venus_current_ctx_id`, memory id/resource id helpers). It no longer carries a hardcoded
  list of `vulkan_virtio*.dll` names, so the manifest installed by this script is the single source
  of truth for both normal Vulkan apps and the UMD helper path.

---

## 8. Test order / milestones

1. **First milestone — host logs the first `vkCreateInstance`.** Set `VN_DEBUG=init` (bit 0,
   `vn_common.h:118`) to get the ICD's "connected to renderer"/"wire format version" lines
   (`vn_instance.c:215`); confirm on the host from the standalone QEMU/render-server log. Minimum
   to reach here: `info` (§5, validated before host traffic) + `shmem_ops.create`
   (the 128 KiB ring, `vn_instance.c:142`/`vn_ring.c:294` — needs a genuinely host-coherent mapping) +
   `ops.submit` (`vkCreateRingMESA` carries the ring's `resourceId`) + `ops.wait`/`sync_ops`.
2. **`vulkaninfo`** → `driverName "venus"`, exactly one physical device.
3. **Offscreen `vkcube --headless`** (no WSI). Needs `bo_ops.create_from_device_memory` + `bo_ops.map`
   working end-to-end (the real ALLOC_BLOB/MAP_BLOB validation).
4. WSI / windowed present — deferred (ARCH §8: GDI StretchDIBits readback). Phase 6 = DXVK/VKD3D.

---

## 9. Risks / open items (in priority order)

1. **`VK_RING_STATUS_FATAL_BIT_MESA` → `abort()`** (`vn_ring.c:458`). If `shmem_ops.create` returns a
   bad res_id or a non-coherent mapping (host never sees the cs), or the ring's `resourceId` is wrong,
   the ICD process aborts. This is the most likely first-run failure and ties to the **still-unvalidated
   `ALLOC_BLOB(blob_id=0)`+`MAP_BLOB` host path**: phase4-blob-plan recorded the host **rejecting** a
   standalone `ALLOC_BLOB(HOST3D,USE_MAPPABLE,blob_id=0)` with Win32 1117 — but that was **without a
   live venus context**. The ring shmem is allocated **after** `CTX_CREATE`, and the capset flag
   `supports_blob_id_0` (`venus_hw.h:35`) governs exactly this. **Re-verify: does a HOST3D mappable
   blob with `blob_id=0` get accepted once the venus context exists?** If not, the ring needs a
   different backing (e.g. a guest-shmem blob, `VIRTIO_GPU_BLOB_MEM_GUEST`) — a KMD change.
2. **MAP_BLOB user-mapping on this WDK** — the KMD maps PCI-BAR (I/O-space) pages into user space with
   `MmMapLockedPagesSpecifyCache(UserMode)` over a hand-built `MDL_IO_SPACE` MDL. This is implemented
   but **only end-to-end-validated here**. If it bugchecks/raises, the fallback (ARCH §12, mvisor
   pattern) is `MmMapIoSpaceEx`-to-kernel + a section view — a KMD change. The UserMode-map SEH
   (no SEH in the no_std KMD) is a known hardening TODO (a C `__try` shim).
3. **Synchronous-submit deadlock risk** — `ops.submit` blocks to completion; a venus command whose
   completion depends on a *later* command (e.g. `vkWaitSemaphores` ordering, the `allow_vk_wait_syncs`
   capset flag) can hang a single-threaded synchronous channel. Unlikely headless; a real risk for
   vkcube/DXVK. The proper fix is async fences (Phase 4e: WDFINTERRUPT + the `fence_id→KEVENT` DPC,
   blocked on the legacy-INTx `STATUS_DEVICE_POWER_FAILURE` question).
4. **DLL exports** — see §7; missing `.def` = silent ICD-load failure.
5. **`max_timeline_count=1` / `ring_idx`** — keep at 1 until the KMD has real multi-timeline fences;
   advertising more binds ring_idx values the KMD ignores → silent mis-fencing.
6. **GET_CAPSET / RESOURCE_UNREF gaps** — `info` is hardcoded (§5); shmem/bo destroy can't fully free
   host resources without a RESOURCE_UNREF IOCTL (leak until CTX_DESTROY for the first cut). Add the
   IOCTLs when moving past bring-up.

---

## 10. Key file pointers

- Backend to write: `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c`.
- Template / reference: `vn_renderer_vtest.c` (structure), `vn_renderer_virtgpu.c` (blob/submit/sync
  semantics), `vn_renderer.h` (vtable), `vn_renderer_util.c/.h`.
- Edits: `vn_renderer.h:233-261`, `src/virtio/vulkan/meson.build:109-118`, a `vn_icd.def`.
- Contract sources: `vn_instance.c` (init/ring/info gates), `vn_ring.c` (ring/cs/abort), `vn_icd.c`
  (exports), `src/virtio/virtio-gpu/venus_hw.h` (capset struct), `vn_protocol_driver_info.h` (version gates).
- Helios side: `protocol/src/{ioctl.rs,escape.rs,virtio_gpu.rs}`, `kmd/src/ioctl.rs` (esp.
  `handle_submit_venus` for the IN_DIRECT mapping, `handle_map_blob`), `probe/src/main.rs` (a working
  SetupDi+CreateFile+DeviceIoControl example in Rust), ARCH.md §3/§5/§6.
