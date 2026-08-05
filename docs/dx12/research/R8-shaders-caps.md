# R8 — Shaders (DXIL), caps tiers, and feature levels for D3D12 on Helios

**Lane:** R8. **Date:** 2026-08-05. **Status:** research only; nothing built, nothing installed.

Evidence key used throughout:
- **[HDR]** = the Windows SDK header at `tmp/dx12/sdk/*.h` says so, with a line number.
- **[VKD]** = `vkd3d-proton-helios/` source says so, with a line number.
- **[HEL]** = live Helios tree says so, with a line number.
- **[ICD]** = the guest `vulkaninfo` capture at `tmp/dx12/research/guest-vulkaninfo-full.txt` says so.
- **[MS]** = Microsoft docs / public spec, with a URL.
- **[INFER]** = my reasoning from the above. Always labelled.
- **UNVERIFIED** = I could not settle it; the settling read/experiment is given inline.

Header provenance: `tmp/dx12/sdk/d3d12umddi.h`, 19 031 lines, Windows SDK 10.0.26100.0
(`D3D12DDI_MINOR_HEADER_VERSION 3`, `d3d12umddi.h:19`). vkd3d-proton at submodule pin
`2c7ba22c` (upstream master, unmodified — DX12.md §1.3).

---

## 0. The five findings that drive everything else

1. **vkd3d-proton on the Helios venus ICD would cap at Shader Model 6.0, not 6.8** — one
   property, `shaderDenormPreserveFloat32`, gates SM 6.2, and every higher SM is chained off
   6.2. §4.3. This is derivable *today* from a `vulkaninfo` capture and vkd3d's own source; no
   build required.
2. **Because SM caps at 6.0, `max_feature_level` caps at FL 12_1**, not 12_2 — FL 12_2 requires
   SM ≥ 6.5 in `d3d12_device_caps_init_feature_level` (`device.c:10572`). §4.4.
3. **The Helios D3D11 shader path cannot be reused for D3D12 at all.** The in-tree compiler is
   `dxbc-spirv`, which has no DXIL support of any kind (§2.3). The D3D12 DXIL compiler is
   `dxil-spirv`, a vkd3d-proton subproject that is **not checked out** in this tree (§3.1).
4. **The current DirectX-Graphics-Samples tree is DXIL-only**: all **178** shader compile steps
   in `Samples/Desktop` `.vcxproj` files invoke `dxc.exe -T…_6_x`; the only project that uses
   `fxc.exe` is `D3D12On7`. An "SM 5.1 floor" D3D12 device runs essentially none of them. §5.
5. **The D3D12 DDI hands the driver shader bytecode with no length anywhere.** `grep
   BytecodeLength d3d12umddi.h` returns zero hits, while the API-side `D3D12_SHADER_BYTECODE`
   carries `SIZE_T BytecodeLength` (`d3d12.h:2196-2200`). The driver must derive the size from
   the blob. §1.2.

---

## 1. Shader bytecode: what the D3D12 UMD DDI actually receives

### 1.1 The `CreateShader` family and its argument struct

There are three generations of the shader-creation DDI in this header.

**Generation 0003 — bare parameters** (`d3d12umddi.h:2209-2225`), verbatim:

```c
typedef SIZE_T ( APIENTRY* PFND3D12DDI_CALC_PRIVATE_SHADER_SIZE )(
    D3D12DDI_HDEVICE, _In_reads_(pShaderCode[1]) CONST UINT* pShaderCode, D3D12DDI_HROOTSIGNATURE, _In_ CONST D3D12DDIARG_STAGE_IO_SIGNATURES* );
typedef VOID ( APIENTRY* PFND3D12DDI_CREATE_SHADER_0003 )(
    D3D12DDI_HDEVICE, _In_reads_(pShaderCode[1]) CONST UINT* pShaderCode, D3D12DDI_HROOTSIGNATURE, D3D12DDI_HSHADER, _In_ CONST D3D12DDIARG_STAGE_IO_SIGNATURES*, D3D12DDI_CREATE_SHADER_FLAGS );
typedef VOID ( APIENTRY* PFND3D12DDI_CREATE_COMPUTE_SHADER_0003 )(
    D3D12DDI_HDEVICE, _In_reads_(pShaderCode[1]) CONST UINT* pShaderCode, D3D12DDI_HROOTSIGNATURE, D3D12DDI_HSHADER, D3D12DDI_CREATE_SHADER_FLAGS );
typedef VOID ( APIENTRY* PFND3D12DDI_CREATE_TESS_SHADER_0003 )(
    D3D12DDI_HDEVICE, _In_reads_(pShaderCode[1]) CONST UINT* pShaderCode, D3D12DDI_HROOTSIGNATURE, D3D12DDI_HSHADER, _In_ CONST D3D12DDIARG_TESSELLATION_IO_SIGNATURES*, D3D12DDI_CREATE_SHADER_FLAGS );
```

**Generation 0010 — argument struct** (`d3d12umddi.h:3269-3280`), verbatim:

```c
typedef struct D3D12DDIARG_CREATE_SHADER_0010
{
    D3D12DDI_HROOTSIGNATURE hRootSignature;
    CONST UINT* pShaderCode;
    union
    {
        CONST D3D12DDIARG_STAGE_IO_SIGNATURES* Standard;
        CONST D3D12DDIARG_TESSELLATION_IO_SIGNATURES* Tessellation;
    } IOSignatures;
    D3D12DDI_CREATE_SHADER_FLAGS Flags;
    D3D12DDI_LIBRARY_REFERENCE_0010 LibraryReference;
} D3D12DDIARG_CREATE_SHADER_0010;
```

**Generation 0026 — adds mesh signatures and a cache hash** (`d3d12umddi.h:5538-5551`), verbatim:

```c
typedef struct D3D12DDIARG_CREATE_SHADER_0026
{
    D3D12DDI_HROOTSIGNATURE hRootSignature;
    CONST UINT* pShaderCode;
    union
    {
        CONST D3D12DDIARG_STAGE_IO_SIGNATURES* Standard;
        CONST D3D12DDIARG_TESSELLATION_IO_SIGNATURES* Tessellation;
        CONST D3D12DDIARG_MESH_IO_SIGNATURES* Mesh;
    } IOSignatures;
    D3D12DDI_CREATE_SHADER_FLAGS Flags;
    D3D12DDI_LIBRARY_REFERENCE_0010 LibraryReference;
    D3D12DDI_SHADERCACHE_HASH ShaderCodeHash;
} D3D12DDIARG_CREATE_SHADER_0026;
```

Entry points (`d3d12umddi.h:5562-5568`): `PFND3D12DDI_CALC_PRIVATE_SHADER_SIZE_0026`,
`PFND3D12DDI_CREATE_SHADER_0026`, plus the geometry-shader-with-stream-output pair. Note the
D3D11 two-call idiom survives unchanged: *CalcPrivate…Size* then *Create…* into runtime-owned
memory, exactly as `umd/src/forward/shaders.rs:61-67` does for D3D11 (`calc_size_shader`
returns a constant 8).

`D3D12DDI_CREATE_SHADER_FLAGS` (`d3d12umddi.h:2201-2207`) has exactly three values:
`_NONE = 0x0`, `_ENABLE_SHADER_TRACING = 0x1`, `_DISABLE_OPTIMIZATION_0024 = 0x2`.

`D3D12DDI_SHADERCACHE_HASH` (`d3d12umddi.h:4243-4246`) is `BYTE Hash[16]` — a *cache key* handed
to the driver for use with `pfnShaderCacheGetValueCb` / `pfnShaderCacheStoreValueCb`
(`d3d12umddi.h:4248-4270`). It is **not** the DXIL validator hash and carries no security
meaning at the DDI.

### 1.2 There is no length. Anywhere.

```
$ grep -n "BytecodeLength\|SHADER_BYTECODE" tmp/dx12/sdk/d3d12umddi.h
(no output)
$ grep -n "typedef struct D3D12_SHADER_BYTECODE" -A 4 tmp/dx12/sdk/d3d12.h
2196:typedef struct D3D12_SHADER_BYTECODE
2197-    {
2198-    _Field_size_bytes_full_(BytecodeLength)  const void *pShaderBytecode;
2199-    SIZE_T BytecodeLength;
2200-    } 	D3D12_SHADER_BYTECODE;
```

The application hands D3D12 a pointer **and** a length; the runtime forwards only the pointer.
The same is true of `D3D12DDI_DXIL_LIBRARY_DESC_0054` (`d3d12umddi.h:7820-7825`):

```c
typedef struct D3D12DDI_DXIL_LIBRARY_DESC_0054
{
    CONST UINT*  pDXILLibrary;
    UINT NumExports; // Optional, if 0 all exports in the library/collection will be surfaced
    _In_reads_(NumExports) D3D12DDI_EXPORT_DESC_0054* pExports;
} D3D12DDI_DXIL_LIBRARY_DESC_0054;
```

whereas its API counterpart `D3D12_DXIL_LIBRARY_DESC` (`d3d12.h:14354-14359`) holds a full
`D3D12_SHADER_BYTECODE`.

**[INFER, high confidence]** The blob is therefore *self-describing*, and the only two
self-describing D3D shader encodings are:
- a raw SM4/SM5 token stream, where `dword[0]` is the version token and `dword[1]` is the
  length **in dwords** — which is exactly what the `_In_reads_(pShaderCode[1])` SAL annotation
  on the 0003 generation encodes (`d3d12umddi.h:2210`); and
- a DXBC container (`'DXBC'` magic, 16-byte digest, version dword, **total byte size at offset
  24**, chunk count, chunk offset table) — the only form a DXIL blob ever takes, since DXIL
  ships as a `DXIL` chunk inside a DXBC container (`vkd3d-proton-helios/libs/vkd3d-shader/dxbc.c:39`
  `#define TAG_DXIL MAKE_TAG('D','X','I','L')`, and the container walker at `dxbc.c:100-170`).

Helios' D3D11 UMD already implements *both* discriminators in one function —
`umd/src/forward/shaders.rs:13-39`, `shader_code_len()`: `'DXBC'` magic ⇒ read the total size
at dword 6 (byte 24); otherwise ⇒ `dword[1] * 4`. **That function is directly reusable for
D3D12 and its two bounds checks (`total < 32 || total > (1<<20)*4`, `dwords < 2 || dwords >
1<<20`) are exactly the validation a D3D12 UMD needs on the same untrusted input.**

⚠ **UNVERIFIED:** whether the D3D12 runtime ever hands the driver a *raw* DXIL bitstream rather
than a DXBC container, and whether SM 5.1 DXBC arrives as a token stream (D3D11 shape) or as a
container. Neither the header nor
<https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3d12umddi/ns-d3d12umddi-d3d12ddiarg_create_shader_0026>
(fetched 2026-08-05; it says only "Pointer to the shader code") settles it.
**Settling experiment:** make `OpenAdapter12` succeed with a device-funcs table whose
`pfnCreateShader` does nothing but log the first 8 dwords and return, then run
`Samples/Desktop/D3D12HelloWorld/src/HelloTriangle` (already SM 6.0, §5) via a cloned scheduled
task. First dword `0x43425844` ⇒ container. This is a ~1-day experiment and it must be paired
with an honest caps answer, or R908 repeats (DX12.md §5.1).

### 1.3 The IO-signature structs — and the header's own admission that DXBC→DXIL converters exist

`D3D12DDIARG_STAGE_IO_SIGNATURES` (`d3d12umddi.h:2089-2125`),
`D3D12DDIARG_TESSELLATION_IO_SIGNATURES` (`:2127-2169`) and `D3D12DDIARG_MESH_IO_SIGNATURES`
(`:2171-2199`) carry the same "union of all registers, a superset of what this shader uses"
contract as D3D11's. The entry struct gained a field in 0012 (`d3d12umddi.h:2078-2087`),
verbatim:

```c
typedef struct D3D12DDIARG_SIGNATURE_ENTRY_0012
{
    D3D10_SB_NAME SystemValue; // D3D10_SB_NAME_UNDEFINED if the particular entry doesn't have a system name.
    UINT Register;
    BYTE Mask;// (D3D10_SB_OPERAND_4_COMPONENT_MASK >> 4), meaning 4 LSBs are xyzw respectively
    BYTE Stream; // This field was inserted in _0012 and will not break old drivers since it doesn't change struct size.
                 // It is used to help drivers that use a DXBC->DXIL converter, for GS output signatures
    D3D10_SB_REGISTER_COMPONENT_TYPE RegisterComponentType;
    D3D11_SB_OPERAND_MIN_PRECISION   MinPrecision;
} D3D12DDIARG_SIGNATURE_ENTRY_0012;
```

Two things follow, both load-bearing for Helios:

1. Microsoft explicitly expects some D3D12 drivers to implement **only** a DXIL backend and to
   run incoming DXBC through a converter (`DxbcConverter`, shipped in DXC). That is the exact
   architectural posture a Helios D3D12 UMD over vkd3d-shader would be in: vkd3d-shader's own
   `vkd3d_shader_compile_dxbc` comment says "**Shader models 4 through 6.x are handled
   externally through dxil-spirv**" (`libs/vkd3d-shader/vkd3d_shader_main.c:212`).
2. The `SystemValue`, `RegisterComponentType` and `MinPrecision` fields are the same
   `D3D10_SB_*` / `D3D11_SB_*` token enums Helios already flattens for D3D11
   (`umd/src/forward/shaders.rs:150-166`, `SigEntry { sysval, register_, mask, comptype, stream }`).
   The D3D12 `_0012` layout matches Helios' 5-word wire entry field-for-field, including the
   `Stream` byte Helios already carries and currently logs-and-drops
   (`umd/bridge/bridge_dxbc.cpp:212-219`).

### 1.4 Validation and signing: not the driver's job

- **DXBC container checksum.** vkd3d-proton does not check it: `dxbc.c:124` is literally
  `WARN("Ignoring DXBC checksum.\n"); skip_dword_unknown(&ptr, 4);`. Helios' D3D11 bridge, by
  contrast, *computes* one when it synthesises a container
  (`umd/bridge/bridge_dxbc.cpp:303-304`, `dxbc_spv::dxbc::hashDxbcBinary`) because DXVK's
  container reader is downstream. Neither is a security check.
- **DXIL validator hash.** **[MS]** "The DirectX runtime validates the hash on each shader by
  computing the hash from DXIL and comparing the computed value against the value written in
  the shader binary."
  (<https://devblogs.microsoft.com/directx/open-sourcing-dxil-validator-hash/>). Unsigned
  bytecode fails at `ID3D12Device::CreateComputeShader`/`CreatePipelineState` with *"Compute
  Shader is corrupt or in an unrecognized format"* unless experimental shader models +
  Developer Mode are on; a later Agility SDK added a literal `BYPASS` hash value
  (`01010101010101010101010101010101`) that skips verification, and `PREVIEW_BYPASS` for
  preview shader models, which still needs Developer Mode +
  `D3D12ExperimentalShaderModels`
  (<https://github.com/microsoft/hlsl-specs/blob/main/proposals/infra/INF-0004-validator-hashing.md>).
- **Therefore:** the check happens in `d3d12core.dll` **before** the UMD is called. A Helios
  D3D12 UMD has **no signing obligation and no validation obligation** — it may assume the blob
  reached it only because the runtime accepted the hash. It must still bounds-check every
  offset it reads out of the blob (CLAUDE.md rule: "Validate every runtime/guest-supplied size
  & offset before reading"), because a *correct* hash says nothing about a container being
  well-formed against the driver's parser.
- **Corollary for strategy (b) (vkd3d-proton as `d3d12.dll`/`d3d12core.dll`):** replacing
  `d3d12core.dll` removes the hash check entirely; vkd3d never computes it (`dxbc.c:124`) and
  `vkd3d_compute_dxbc_checksum` is used only on the *write* path when serialising a root
  signature (`dxbc.c:1425`). So the Proton model sidesteps validator hashing altogether. That is
  a *convenience*, not a correctness win: it means a Helios-under-vkd3d run and a
  Helios-under-real-D3D12 run do not see the same shader-acceptance behaviour, so a conformance
  claim from one does not transfer to the other.

---

## 2. What Helios does today for D3D11 — the contract the D3D12 equivalent must match in spirit

### 2.1 The shape

D3D11's runtime hands the UMD a **raw SM4/SM5 token stream** plus separate IO-signature structs.
DXVK's compiler wants a **DXBC container** with `ISGN`/`OSGN`/`PCSG` chunks. Helios bridges the
gap by *synthesising a container in the UMD*:

- `umd/src/forward/shaders.rs:13-39` — `shader_code_len()`: discriminate container vs token
  stream, bound both.
- `umd/src/forward/shaders.rs:68-100` — `create_vertex_shader`: `clear_handle` → resolve device
  → length → `core::slice::from_raw_parts` → `dxvk.create_vertex_shader(ptr, len)` → on success
  `store_raw_com`, and cache the bytecode so input layouts can be built lazily from the ISGN.
  On failure: `log_error!` and leave the handle cleared — **no panic, ever** (CLAUDE.md
  invariant).
- `umd/src/forward/shaders.rs:129-186` — the wire format for flattened signatures:
  `SIG_ENTRY_WORDS = 5`, `SigEntry { sysval, register_, mask, comptype, stream }`, and
  `SigHeader::{Stage(2 words), Tess(3 words)}`. The doc comment at `:167-180` records why the
  arity is a constructor choice: reading a tess block with the 2-word accessor "silently
  returns `n_patch` as the first entry's system value".
- `umd/bridge/bridge_dxbc.cpp` (406 lines) — container synthesis and *nothing else*, compiled
  without `dxvk_instance.h`/`dxvk_device.h`/`d3d11_device.h` so that "the signature encoder
  cannot touch the DXVK device" is a link-time fact (`bridge_dxbc.cpp:1-13`):
  - `append_signature_chunk` (`:165-241`) writes one 24-bytes-per-entry `ISGN`/`OSGN`/`PCSG`
    chunk; semantic names are synthesised as `"TEXCOORD"` + register index because "names are
    only a matching key"; the load-bearing fields are register, mask, system value and
    **component type**, and the comment at `:161-164` records what happens without the last
    one: "dwm binds R16G16_SINT vertex data against shaders whose ISGN declares SINT inputs —
    typing them float32 was VUID-Input-08733 UB that rasterized nothing."
  - `encode_signature_entry` (`:124-155`) translates D3D11 DDI tess-factor `D3D10_SB_NAME`
    token values 11..22 into the collapsed `D3D_NAME` reflection values dxbc-spv expects,
    "without this translation, hull shader tess factors are not declared as SPIR-V
    TessLevelOuter/Inner built-ins and tessellated draws can disappear."
  - `build_dxbc_container<N>` (`:264-311`) — 32-byte file header, `N+1` chunk offset table,
    signature chunks, code chunk (`"SHEX"` for SM5+, `"SHDR"` for SM4, `dxbc_code_tag()` at
    `:313-315`), then the MD5 stamp via `dxbc_spv::dxbc::hashDxbcBinary`. `N` is a *template*
    parameter so the offset table has no runtime bound to get wrong.
  - Refusal discipline: `signature_count_ok` (`:98-110`) bounds entry counts at 512 and
    increments the named atomic `g_signatureCountRefused`, logging `"… REFUSED: signature entry
    count %u exceeds %u (x%u)"`. That is the CLAUDE.md "every refusal gets a named counter"
    rule, in the shader path.
  - Optional forensics: `HKLM\SOFTWARE\Helios\ShaderBytecodeDumpPath` dumps every blob as
    `shader-<pid>-<seq>-<stage>-<form>-<len>.dxbc` (`:39-83`). **This knob is the single
    cheapest D3D12 bring-up instrument in the tree** — the D3D12 equivalent should exist from
    commit 1.

### 2.2 The contract, stated precisely

> The UMD receives an opaque, length-less D3D bytecode pointer plus a runtime-owned description
> of the stage IO signature; it derives the length from the blob, bounds it, converts to
> whatever the engine's compiler consumes, and returns a driver handle. Failure is a cleared
> handle plus a logged error plus a named counter — never a panic, never a fabricated handle.

The D3D12 version of that sentence is identical except that "whatever the engine's compiler
consumes" is SPIR-V-from-DXIL rather than SPIR-V-from-DXBC.

### 2.3 ⚠ The D3D11 compiler cannot be reused

`dxvk-helios/subprojects/dxbc-spirv/` contains `dxbc/` (`dxbc_converter.cpp`, `dxbc_parser.cpp`,
`dxbc_signature.cpp`, …), `sm3/`, `ir/`, `spirv/`. A recursive case-insensitive grep for `dxil`
across its headers and sources returns **zero hits**. There is no DXIL front end in the D3D11
stack, and there is no plausible incremental path to one — DXIL is LLVM 3.7 bitcode, not a
token stream. **Any D3D12 story on this tree requires `dxil-spirv` (or DXC's `DxbcConverter` +
`dxil-spirv`), full stop.**

---

## 3. What vkd3d-shader supports

### 3.1 The whole library is 5 557 lines and contains no shader compiler

```
$ wc -l vkd3d-proton-helios/libs/vkd3d-shader/*.c *.h
    99 checksum.c        # DXBC container MD5 (write path only)
  1879 dxbc.c            # container walk, signature chunks, root-signature (de)serialise
  2474 dxil.c            # the dxil-spirv binding layer: option/remap plumbing
  1001 vkd3d_shader_main.c
   104 vkd3d_shader_private.h
```

`libs/vkd3d-shader/meson.build` links exactly `checksum.c`, `dxil.c`, `dxbc.c`,
`vkd3d_shader_main.c`, `3rdparty/md5/md5.c`, against `dxil_spirv_dep`.

The dispatch (`vkd3d_shader_main.c:196-215`), verbatim:

```c
    is_dxil = shader_is_dxil(dxbc->code, dxbc->size);
    /* Shader models 4 through 6.x are handled externally through dxil-spirv. */
    spirv->meta.hash = 0;
    return vkd3d_shader_compile_dxil(dxbc, spirv, spirv_debug, shader_interface_info, compile_args, is_dxil);
```

`shader_is_dxil` (`dxbc.c:341-351`) just walks the container looking for a `DXIL` chunk. **Both
DXBC-TPF (SM 4/5.1) and DXIL (SM 6.x) go to the same external converter**; there is no TPF→SPIR-V
path inside vkd3d-proton. (Upstream *vkd3d* — the WineHQ project — does have a TPF compiler;
vkd3d-**proton** does not. Do not confuse the two.)

⚠ **`vkd3d-proton-helios/subprojects/dxil-spirv/` is an empty directory** (`ls -la` shows a
0-entry dir, `.gitmodules` declares it as
`https://github.com/HansKristian-Work/dxil-spirv`). So the DXIL compiler itself is **not
readable in this tree** and vkd3d-proton **cannot be built** until the submodule is
initialised. Anything in this section about dxil-spirv's internals is therefore inference from
its consumer, not a read of it. `khronos/Vulkan-Headers` and `khronos/SPIRV-Headers` are also
submodules.

### 3.2 Shader models and stages vkd3d-proton claims

Shader models: 5_1 through 6_9 are enumerated in the override table
(`libs/vkd3d/device.c:10604-10614`); the auto-detected ladder tops out at
`D3D_SHADER_MODEL_6_8` (`device.c:10820`), with 6_9 reachable only through
`VKD3D_SHADER_MODEL=6_9`.

Stages, from the caps code:
- Graphics VS/HS/DS/GS/PS and compute: baseline.
- **Mesh + amplification**: `d3d12_device_determine_mesh_shader_tier` (`device.c:10052-10059`)
  → `D3D12_MESH_SHADER_TIER_1` iff `meshShader && taskShader`.
- **Raytracing**: `d3d12_device_determine_ray_tracing_tier` (`device.c:9905…`) gated on
  `VK_KHR_acceleration_structure` / `ray_tracing_pipeline` / RTAS vertex-buffer format support
  (`d3d12_device_supports_rtas_formats`, `device.c:9885-9903`).
- **Work graphs**: `libs/vkd3d/workgraphs.c` exists; the cap is
  `D3D12DDI_WORK_GRAPHS_TIER` in the DDI (`d3d12umddi.h:10537-10541`) and
  `options1->WorkGraphsTier` in vkd3d.

### 3.3 The SPIR-V surface dxil-spirv is driven to emit

`libs/vkd3d-shader/dxil.c` is essentially one big "which `dxil_spv_option_*` do we turn on"
function. The options set (`vkd3d_dxil_converter_set_options`, `dxil.c:776-1330`, and
`vkd3d_dxil_converter_set_quirks`, `:696-774`) names, one-for-one, the Vulkan features the
generated SPIR-V will depend on. The load-bearing ones:

| dxil-spirv option (`dxil.c` line) | Vulkan feature it commits the SPIR-V to |
|---|---|
| `DXIL_SPV_OPTION_PHYSICAL_STORAGE_BUFFER` (`:836`) | `bufferDeviceAddress` |
| `DXIL_SPV_OPTION_BINDLESS_CBV_SSBO_EMULATION` (`:824`) | descriptor indexing / update-after-bind |
| `DXIL_SPV_OPTION_SCALAR_BLOCK_LAYOUT` (`:1048`) | `scalarBlockLayout` |
| `DXIL_SPV_OPTION_COMPUTE_SHADER_DERIVATIVES` (`:781`) | `VK_KHR_compute_shader_derivatives` |
| `DXIL_SPV_OPTION_SHADER_DEMOTE_TO_HELPER` (`:1019`) | `shaderDemoteToHelperInvocation` |
| `DXIL_SPV_OPTION_TYPED_UAV_READ_WITHOUT_FORMAT` (`:1028`) | `shaderStorageImageReadWithoutFormat` |
| `DXIL_SPV_OPTION_BARYCENTRIC_KHR` (`:1069`) | `VK_KHR_fragment_shader_barycentric` |
| `DXIL_SPV_OPTION_SUBGROUP_PARTITIONED_NV` (`:1129`) | `VK_NV_shader_subgroup_partitioned` |
| `DXIL_SPV_OPTION_QUAD_CONTROL_RECONVERGENCE` (`:1139`) | `VK_KHR_shader_quad_control` + maximal reconvergence |
| `DXIL_SPV_OPTION_RAW_ACCESS_CHAINS_NV` (`:1151`) | `VK_NV_raw_access_chains` — **absent from the Helios ICD** |
| `DXIL_SPV_OPTION_MIN_PRECISION_NATIVE_16BIT` (`:1102`) | `shaderFloat16` / `shaderInt16` |
| `DXIL_SPV_OPTION_DENORM_PRESERVE_SUPPORT` (`:782`) | `VK_KHR_shader_float_controls` denorm |
| `DXIL_SPV_OPTION_FLOAT8_SUPPORT` (`:783`) | `VK_EXT_shader_float8` |
| `DXIL_SPV_OPTION_OPACITY_MICROMAP` (`:1162`) | `VK_EXT_opacity_micromap` |
| `DXIL_SPV_OPTION_SBT_DESCRIPTOR_SIZE_LOG2` (`:921`) | ray-tracing pipeline SBT layout |

**How vkd3d keeps that honest** — `d3d12_device_validate_shader_meta`
(`libs/vkd3d/device.c:11670-11790`) re-reads the **emitted SPIR-V** for `OpCapability` tokens
(`vkd3d_shader_extract_feature_meta`, `vkd3d_shader_main.c:750-830`, switching on
`SpvCapabilityInt64`, `SpvCapabilityFloat64`, `SpvCapabilityStencilExportEXT`,
`SpvCapabilitySparseResidency`, `SpvCapabilityFragmentFullyCoveredEXT`,
`SpvCapabilityFragmentBarycentricKHR`, `SpvCapabilityInt64Atomics`, `SpvCapabilityInt64ImageEXT`,
`SpvCapabilityFloat16` + the 16-bit storage caps, `SpvCapabilityShaderViewportIndexLayerEXT`)
and **fails PSO creation** if the shader needs something the reported caps say is off. Eleven
such checks; each one is a clean `return false`, not a hang. **This is the reference
implementation of DX12.md §5.5's "advertise only what is backed" rule, and it is worth reading
before writing the Helios equivalent (§6).**

---

## 4. The caps contract

### 4.1 The DDI surface, by the numbers

- `D3D12DDICAPS_TYPE` (`d3d12umddi.h:94-153`) has **44** enumerators, values 1000–1091.
- **6** versions of `D3D12DDI_SHADER_CAPS_*` (`:2907, 3515, 4036, 6843, 10442, 10516`).
- **17** versions of `D3D12DDI_D3D12_OPTIONS_DATA_00NN` (`:741 … 11079`), after which Microsoft
  changed convention — the comment at `:11122-11126` says: *"New options DDIs use a new NNNN
  version number and add new caps without inheriting the caps from the previous version. This
  is done to avoid bloating one caps struct indefinitely, like what happened with
  `D3D12DDICAPS_TYPE_D3D12_OPTIONS`."* — plus **9** standalone `D3D12DDI_OPTIONS_DATA_00NN`
  (0090, 0091, 0093, 0098, 0101, 0102, 0103, 0109, 0110).

A driver answers `pfnGetCaps` with the version the runtime asks for; the version negotiation
happens in the adapter's `D3D12DDI_SUPPORTED_*` handshake (e.g.
`#define D3D12DDI_SUPPORTED_0026 ((((UINT64)D3D12DDI_INTERFACE_VERSION_R2) << 32) |
(((UINT64)D3D12DDI_BUILD_VERSION_0026) << 16))`, `:5524`).

### 4.2 The caps that matter, with semantics

**Feature level — `D3D12DDICAPS_TYPE_3DPIPELINESUPPORT` (1007).** The header comment at
`d3d12umddi.h:2922-2923` is the whole semantic and it is the **opposite of D3D11's**:

```
// D3D12DDICAPS_TYPE_3DPIPELINESUPPORT
// For D3D12, drivers only report the maximum level they support
```

`D3D12DDI_3DPIPELINELEVEL` (`:2924-2933`): `1_0_GENERIC = 1`, `1_0_CORE = 2`, `11_0 = 10`,
`11_1 = 11`, `12_0 = 12`, `12_1 = 13`, `12_2 = 14`.
⚠ Helios' *D3D11* caps site treats 3DPIPELINESUPPORT as a **bitmask**
(`umd/src/caps.rs:46-47` "a BITMASK of supported levels", `:57-65`: `LVL_11_0 = 1 << 2`,
`LVL_12_0 = 1 << 7`, `FL11_PIPELINE_MASK` = their OR; and `:208` repeats it), which is correct for
D3D11 (memory: 30th session, "3DPIPELINESUPPORT is a BITMASK") and **wrong for D3D12**.
Reporting `FL11_PIPELINE_MASK = 0x8F` into the D3D12 slot would be read as "level 143". This is
precisely the class of error R908 deleted.

A second, negotiated form exists: `D3D12DDICAPS_TYPE_0081_3DPIPELINESUPPORT1` (1074),
`D3D12DDI_3DPIPELINESUPPORT1_DATA_0081` (`:10416-10420`), which is **in/out**:
```c
    D3D12DDI_3DPIPELINELEVEL HighestRuntimeSupportedFeatureLevel; // input
    D3D12DDI_3DPIPELINELEVEL MaximumDriverSupportedFeatureLevel;  // output
```

**Shader models — `D3D12DDICAPS_TYPE_0011_SHADER_MODELS` (1012).**
`D3D12DDI_D3D12_SHADER_MODELS_DATA_0011` (`:3503-3507`) is a *count + array* out-param, not a
single value: the driver writes the list of `D3D12DDI_SHADER_MODEL` values it supports. The
enum (`:3478-3500`) encodes major/minor plus a release/experimental discriminator in the low
byte: `5_1_RELEASE = 0x00050015`, `6_0_EXPERIMENTAL = 0x00060000`, `6_0_RELEASE = 0x00060005`,
… `6_8_RELEASE = 0x00060085`, `6_9_EXPERIMENTAL = 0x00060090`.

**Shader feature caps — `D3D12DDICAPS_TYPE_SHADER` (1004).** Latest version
`D3D12DDI_SHADER_CAPS_0084` (`:10516-10535`): `MinPrecision` (bitmask of
`D3D12DDI_SHADER_MIN_PRECISION_{NONE,10_BIT,16_BIT}`, `:2898-2904`), `DoubleOps`,
`ShaderSpecifiedStencilRef`, `TypedUAVLoadAdditionalFormats`, `ROVs`, `WaveOps`,
`WaveLaneCountMin`, `WaveLaneCountMax`, `TotalLaneCount`, `Int64Ops`, `Native16BitOps`,
`AtomicInt64OnTypedResource`, `AtomicInt64OnGroupShared`,
`DerivativesInMeshAndAmplificationShaders`, `WaveMMATier`,
`AtomicInt64OnDescriptorHeapResource`.

**Binding / heap / tiling / conservative-raster tiers** — `D3D12DDICAPS_TYPE_D3D12_OPTIONS`
(1006). Enums at `d3d12umddi.h:694-739`:
`D3D12DDI_RESOURCE_BINDING_TIER_{1,2,3}`,
`D3D12DDI_CONSERVATIVE_RASTERIZATION_TIER_{NOT_SUPPORTED,1,2,3}`,
`D3D12DDI_TILED_RESOURCES_TIER_{NOT_SUPPORTED,1,2,3}`,
`D3D12DDI_CROSS_NODE_SHARING_TIER_{NOT_SUPPORTED,1_EMULATED,1,2,0041_3}`,
`D3D12DDI_RESOURCE_HEAP_TIER_{1,2}`. The newest aggregate,
`D3D12DDI_D3D12_OPTIONS_DATA_0089` (`:11079-11112`), adds 27 more fields including
`DepthBoundsTestSupported`, `ProgrammableSamplePositionsTier`, `ViewInstancingTier`,
`BarycentricsSupported`, `RenderPassTier`, `RaytracingTier`, `VariableShadingRateTier`,
`MeshShaderTier`, `SamplerFeedbackTier`, `DriverManagedShaderCachePresent`,
`EnhancedBarriersSupported`.

**GPU VA — `D3D12DDICAPS_TYPE_GPUVA_CAPS` (1009).** `D3D12DDI_GPUVA_CAPS_0004`
(`:254-257`) is one field, `UINT MaxGPUVirtualAddressBitsPerResource`. vkd3d hardcodes 40 with
an `/* XXX */` (`device.c:10183`) — which happens to be exactly Helios' declared GPU VA width
(`kmd_render/src/ddi/gpummu.rs:44-65`, 40-bit VA — DX12.md §3.4).

**Root signature — no version 1.0 at the DDI.** `D3D12DDI_ROOT_SIGNATURE_VERSION`
(`:3743-3747`) has only `_1_1 = 0x2` and `_1_2 = 0x3`, and `D3D12DDIARG_CREATE_ROOT_SIGNATURE_0013`
(`:3749-3758`) hands the driver a **parsed** `CONST D3D12DDI_ROOT_SIGNATURE_0013*`, not the
serialised `RTS0` blob. **[INFER]** the runtime up-converts 1.0 → 1.1 and parses for the driver.
⚠ This is a genuine impedance mismatch for any "vkd3d behind the UMD DDI" plan: vkd3d parses
`RTS0` itself (`vkd3d_shader_parse_root_signature_v_1_0` / `_v_1_2` /
`_v_1_2_from_raw_payload`, `vkd3d_shader_main.c:639-701`), so a UMD would have to **re-serialise**
the DDI's parsed struct back into an `RTS0` blob (vkd3d can write one — `dxbc.c:1019-1045`
writes `TAG_DXBC` + `TAG_RTS0` and stamps it with `vkd3d_compute_dxbc_checksum`) or bypass
vkd3d's parser entirely.

**Scheduling / parallelism caps — the two Helios must get right.**
- `D3D12DDICAPS_TYPE_0050_HARDWARE_SCHEDULING_CAPS` (1067):
  `D3D12DDICAPS_HARDWARE_SCHEDULING_CAPS_0050 { UINT ComputeQueuesPer3DQueue; }` with the
  comment "*0 means don't use scheduling groups*" (`:7004-7008`). Helios must report **0**:
  `DxgkDdiCreateHwQueue` returns `STATUS_NOT_SUPPORTED` and records `HwQRef`
  (`kmd_render/src/ddi/scheduler.rs:180-187`, via DX12.md §3.1).
- `D3D12DDICAPS_TYPE_EXECUTECOMMANDLISTS_PARALLELISM` (1069), `pData = BOOL`
  (`d3d12umddi.h:128`). See §6.

### 4.3 ★ What vkd3d-proton would report on the Helios venus ICD — the shader-model result

`d3d12_device_caps_init_shader_model` (`libs/vkd3d/device.c:10640-10826`). The gate for SM 6.0
(`:10665-10670`) is:

| requirement | source line | Helios ICD | verdict |
|---|---|---|---|
| `subgroupSize >= 4` | `device.c:10665` | 32 (`vulkaninfo:673`) | ✅ |
| `subgroupSupportedOperations ⊇ {BASIC, VOTE, ARITHMETIC, BALLOT, SHUFFLE, QUAD}` | `:10646-10652, 10666` | all 6 present, plus SHUFFLE_RELATIVE/CLUSTERED/ROTATE/ROTATE_CLUSTERED/PARTITIONED_EXT (`vulkaninfo:689-700`) | ✅ |
| `subgroupSupportedStages ⊇ {COMPUTE, FRAGMENT}` | `:10654-10656, 10667` | 14 stages (`vulkaninfo:674`) | ✅ |
| `scalarBlockLayout \|\| uniformBufferStandardLayout` | `:10668` | both true (`vulkaninfo:1662,1664`) | ✅ |
| `shaderInt16` | `:10669` | true (`vulkaninfo:1269`) | ✅ |

⇒ **SM 6.0 enabled** (`device.c:10679`).

The gate for **SM 6.2** (`device.c:10694-10711`), verbatim:

```c
        denorm_behavior = device->device_info.vulkan_1_2_properties.denormBehaviorIndependence !=
                VK_SHADER_FLOAT_CONTROLS_INDEPENDENCE_NONE;
        if (denorm_behavior)
        {
            if (device->device_info.vulkan_1_2_properties.driverID != VK_DRIVER_ID_NVIDIA_PROPRIETARY)
            {
                denorm_behavior = device->device_info.vulkan_1_2_properties.shaderDenormFlushToZeroFloat32 &&
                        device->device_info.vulkan_1_2_properties.shaderDenormPreserveFloat32;
            }
        }
```

The Helios guest ICD reports (`tmp/dx12/research/guest-vulkaninfo-full.txt`):

```
711:	driverID                                             = DRIVER_ID_MESA_VENUS
712:	driverName                                           = venus
713:	driverInfo                                           = Mesa 26.2.0-devel (git-f023e5ce48)
719:	denormBehaviorIndependence                           = SHADER_FLOAT_CONTROLS_INDEPENDENCE_ALL
725:	shaderDenormPreserveFloat32                          = false
728:	shaderDenormFlushToZeroFloat32                       = false
```

`driverID != VK_DRIVER_ID_NVIDIA_PROPRIETARY` ⇒ the NVIDIA exemption does **not** apply ⇒
`denorm_behavior = false && false = false` ⇒ **`max_shader_model` stays at
`D3D_SHADER_MODEL_6_0`.** SM 6.3, 6.5, 6.6, 6.7 and 6.8 are each conditioned on the *previous*
value (`device.c:10716, 10735, 10759, 10794, 10817`), so the whole ladder is dead above 6.0.

**Why this is the venus wrapper's fault and not the host's:** the venus ICD passes
`VkPhysicalDeviceVulkan12Properties` straight through from the host
(`icd/mesa/src/virtio/venus-protocol/vn_protocol_driver_device.h:26289-26295` decodes
`denormBehaviorIndependence` and `shaderDenormPreserveFloat32` verbatim from the host's reply),
so the `false`s are the *host NVIDIA driver's own* values — the same `false`s vkd3d deliberately
ignores on bare metal, because (`device.c:10693-10695`) "*shaderDenorm handling appears to work
just fine on NV, despite the properties struct saying otherwise. Assume that this is just a
driver oversight, since otherwise we cannot expose SM 6.2 there*". Helios loses SM 6.2 **only
because the driverID string says `venus` instead of `nvidia`.**

Three candidate fixes, cheapest first:
1. **Measure it first, for free:** set `VKD3D_SHADER_MODEL=6_8` in the app environment
   (`d3d12_device_caps_shader_model_override`, `device.c:10591-10638`) and see whether anything
   actually breaks. This is a read-only A/B on a knob upstream already ships.
2. **Fork vkd3d-proton** to extend the exemption at `device.c:10700` to
   `VK_DRIVER_ID_MESA_VENUS`. ⚠ Only sound while the host is NVIDIA; venus over a host driver
   with real FTZ/preserve semantics would then be a lie. If taken, it must be conditioned on
   something venus can actually observe about the host, or gated behind an explicit Helios knob.
   **This would be the first real content of the `vkd3d-proton-helios` fork** — DX12.md §1.3
   records that no document explains what the fork was for.
3. **Fix it in `icd/mesa`** if venus is (verifiably) able to honour the SPIR-V float-controls
   execution modes on the host. **UNVERIFIED** whether it can. Settling read:
   `icd/mesa/src/virtio/vulkan/` for any float-controls handling, then a SPIR-V test that sets
   `DenormPreserve` on fp32 and checks a denormal survives a round trip.

⚠ **UNVERIFIED, and it matters:** whether `d3d12_device_caps_init_shader_model` runs *after*
`VK_LAYER_OBS_HOOK` and friends perturb the reported properties. The capture at
`guest-vulkaninfo-full.txt:1-2` shows an OBS layer loaded in the guest. **Settling experiment:**
re-capture `vulkaninfo` with `VK_LOADER_LAYERS_DISABLE=*`.

### 4.4 The rest of the caps table, derived

Every other input vkd3d needs is present in the guest ICD. The mapping, feature-by-feature:

| vkd3d cap (`device.c`) | derived from | Helios ICD evidence | value on Helios |
|---|---|---|---|
| `ResourceBindingTier` (`:10177`) | **hardcoded** `TIER_3` | — | **Tier 3** |
| `TiledResourcesTier` (`:9845-9868`) | sparse features + sparse queue family | `sparseBinding/ResidencyAliased/Buffer/Image2D/Image3D` all true (`vi:1272-1280`), `residencyStandard2D/3DBlockShape` true, `residencyAlignedMipSize` false, `residencyNonResidentStrict` true (`vi:445-449`), `shaderResourceResidency`+`MinLod` true (`vi:1270-1271`), `filterMinmaxSingleComponentFormats` true (`vi:772`), 6 queue families carry `QUEUE_SPARSE_BINDING_BIT` (`vi:1103-1143`) | **Tier 4** (the `TIER_4` return at `device.c:9867`) |
| `ConservativeRasterizationTier` (`:9870-9884`) | `VK_EXT_conservative_rasterization` + 2 props | ext present (`vi:937`), `degenerateTrianglesRasterized` true (`vi:482`), `fullyCoveredFragmentShaderInputVariable` true (`vi:484`) | **Tier 3** |
| `ROVsSupported` (`:10180`) | `fragmentShaderPixelInterlock && …SampleInterlock` | both true (`vi:1425-1426`) | **TRUE** |
| `OutputMergerLogicOp` (`:10173`) | `features.logicOp` | true (`vi:1236`) | **TRUE** |
| `TypedUAVLoadAdditionalFormats` (`:10010-10047`) | 18 DXGI formats must report `FORMAT_SUPPORT2_UAV_TYPED_LOAD` | not directly readable from vulkaninfo | **UNVERIFIED**, expected TRUE on Blackwell. Settling: run vkd3d and read `D3D12_FEATURE_D3D12_OPTIONS` |
| `ResourceHeapTier` (`:9983-10008`) | `bufferImageGranularity <= 64 KiB` + memory-type mask intersection + `pageableDeviceLocalMemory` OR fallback-domain masks | `bufferImageGranularity = 0x400` (1 KiB, `vi:294`) ✅; **`VK_EXT_pageable_device_local_memory` ABSENT** | **UNVERIFIED** (Tier 1 or 2 depending on the fallback-domain mask). Settling: the same read |
| `PSSpecifiedStencilRefSupported` (`:10178`) | `VK_EXT_shader_stencil_export` | **ABSENT from the ICD** | **FALSE** ⇒ any shader emitting `SV_StencilRef` fails PSO creation at `device.c:11719-11724` |
| `MinPrecisionSupport` (`:10174`, `:10149-10153`) | `shaderFloat16 && shaderInt16` | true/true (`vi:1638, 1269`) | **16_BIT** |
| `Native16BitShaderOpsSupported` (`:10300`, `:10133-10147`) | `shaderFloat16 && shaderInt16 && storageBuffer16BitAccess && shaderDenormPreserveFloat16 && denormIndep != NONE && minStorageBufferOffsetAlignment <= 16` | all true; `storageBuffer16BitAccess` true (`vi:1616`), `shaderDenormPreserveFloat16` **true** (`vi:724`), `minStorageBufferOffsetAlignment = 0x10` (`vi:362`) | **TRUE** (but unreachable in practice: it needs SM 6.2) |
| `DoublePrecisionFloatShaderOps` (`:10169-10171`) | `shaderFloat64 && shaderDenormPreserveFloat64 && denormIndep != NONE` (NV-exempt) | `shaderFloat64` true (`vi:1267`), **`shaderDenormPreserveFloat64` false** (`vi:726`), driverID venus ⇒ no exemption (`device.c:10163-10166`) | **FALSE** ⇒ any fp64 shader fails PSO creation (`device.c:11678-11683`). *Same root cause as §4.3.* |
| `WaveOps` (`:10197`) | `max_shader_model >= 6.0` | ✅ | **TRUE** |
| `WaveLaneCountMin/Max` (`:10198-10199`) | `vulkan_1_3_properties.min/maxSubgroupSize` | 32 / 32 (`vi:783-784`) | **32 / 32** |
| `TotalLaneCount` (`:10226-10227`) | no AMD/NV props ext ⇒ `32 * subgroupSize` with a `WARN` | venus exposes neither `VK_AMD_shader_core_properties` nor `VK_NV_shader_sm_builtins` | **1024, and wrong** (a real RTX PRO 6000 has ~24 k lanes). See §6. |
| `Int64ShaderOps` (`:10231`) | `shaderInt64` | true (`vi:1268`) | **TRUE** |
| `DepthBoundsTestSupported` (`:10241`) | `features.depthBounds` | true (`vi:1242`) | **TRUE** |
| `ProgrammableSamplePositionsTier` (`:10243`) | hardcoded NOT_SUPPORTED | — | **NOT_SUPPORTED** |
| `ViewInstancingTier` (`:10264-10274`) | multiview + geom + tess + shaderOutputLayer/ViewportIndex | all true (`vi:1621-1622, 1675-1676`) | **Tier 2** |
| `VPAndRTArrayIndexFromAnyShader…` (`:10187-10189`) | `shaderOutputViewportIndex && shaderOutputLayer` | true/true | **TRUE** |
| `MeshShaderTier` (`:10052-10059`) | `meshShader && taskShader` | true/true (`vi:1462-1463`) | **Tier 1** (unreachable: mesh shaders need SM 6.5) |
| `MaxGPUVirtualAddressBitsPerResource` (`:10183`) | hardcoded 40 | — | **40** |
| `CrossNodeSharingTier` (`:10185`) | hardcoded NOT_SUPPORTED | — | **NOT_SUPPORTED** |
| `StandardSwizzle64KBSupported` (`:10184`) | hardcoded FALSE | — | **FALSE** |

**Feature level** (`d3d12_device_caps_init_feature_level`, `device.c:10549-10589`):

- 11_0: unconditional floor (`:10555`).
- 11_1 (`:10557-10560`): `OutputMergerLogicOp` ✅ + `vertexPipelineStoresAndAtomics` ✅
  (`vi:1253`) + `maxPerStageDescriptorStorageBuffers >= 64` (1 048 576, `vi:299`) ✅ +
  `maxPerStageDescriptorStorageImages >= 64` (1 048 576, `vi:301`) ✅ → **11_1**.
- 12_0 (`:10562-10566`): tiled ≥ Tier 2 ✅ + binding ≥ Tier 2 ✅ + `TypedUAVLoadAdditionalFormats`
  (expected ✅) → **12_0**.
- 12_1 (`:10568-10570`): ROVs ✅ + conservative ≥ Tier 1 ✅ → **12_1**.
- 12_2 (`:10572-10583`): needs `max_shader_model >= D3D_SHADER_MODEL_6_5` → **❌ blocked by §4.3**
  (and would also need `RaytracingTier ≥ 1_1`, `VariableShadingRateTier ≥ 2`, `MeshShaderTier ≥ 1`,
  `SamplerFeedbackTier ≥ 0_9`, `Int64ShaderOps`, `DepthBoundsTestSupported`,
  `CopyQueueTimestampQueriesSupported`, binding Tier 3, conservative Tier 3, tiled Tier 3).

**⇒ predicted Helios D3D12 profile under vkd3d-proton, today: FL 12_1, SM 6.0, Resource Binding
Tier 3, Tiled Resources Tier 4, Conservative Raster Tier 3, Heap Tier 1-or-2, ROVs yes, fp64 no,
native-16-bit unreachable, stencil-ref no, DXR/mesh/sampler-feedback present in Vulkan but
unreachable through the SM gate.** Every entry is derived, not measured — see §7 for the probe
that measures it.

---

## 5. The minimum viable D3D12 device

### 5.1 The nominal floor

`D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_11_0, …)` with:
- `3DPIPELINESUPPORT` = `D3D12DDI_3DPIPELINELEVEL_11_0` (10),
- `ResourceBindingTier` 1, `ResourceHeapTier` 1, `TiledResourcesTier` NOT_SUPPORTED,
- `ConservativeRasterizationTier` NOT_SUPPORTED, `ROVs` FALSE,
- shader-model list = `{ D3D12DDI_SHADER_MODEL_5_1_RELEASE_0011 }` (0x00050015).

Below FL 11_0 there are only `1_0_GENERIC` and `1_0_CORE` (`d3d12umddi.h:2926-2927`) —
compute-only / "D3D12 Core 1.0" profiles paired with
`D3D12DDICAPS_TYPE_0033_ADAPTER_COMPUTE_ONLY` (1066, `:123`). ⚠ The deleted R908 body reported
`3DPIPELINELEVEL_1_0_CORE` (DX12.md §1.2) — i.e. the *compute-only* level. Do not resurrect that
value by copy-paste.

What that floor demands of the Vulkan driver, per vkd3d's own hard requirements
(`libs/vkd3d/device.c:3428-3463`, each an `ERR(… "This is required." )` that fails device
creation):
`samplerMirrorClampToEdge`, `VK_EXT_robustness2` **with** `nullDescriptor`,
`shaderDrawParameters`, push descriptors, and `VK_KHR_maintenance5` **and** `maintenance6`.
Plus, for the always-Tier-3 binding claim, full descriptor indexing with update-after-bind. The
Helios ICD carries `maintenance1..7` (`vi:1044-1050`), `VK_EXT_robustness2` (`vi:984`) and
`VK_EXT_mutable_descriptor_type` (`vi:971`); the individual feature booleans are lane R12's to
confirm.

### 5.2 ⚠ The floor is a fiction for the sample corpus in this tree

```
$ grep -rhno "dxc.exe -nologo -T[a-z]*_[0-9]_[0-9]" --include=*.vcxproj \
    dx-samples-research-only/Samples/Desktop | sed 's/.*-T//' | sort | uniq -c | sort -rn
     74 ps_6_0
     70 vs_6_0
     14 ms_6_5
     10 ps_6_3
      4 cs_6_0
      4 as_6_5
      2 gs_6_0
$ grep -rl "fxc.exe" --include=*.vcxproj dx-samples-research-only/Samples/Desktop
dx-samples-research-only/Samples/Desktop/D3D12On7/src/D3D12On7.vcxproj
```

Even `D3D12HelloTriangle` — the canonical first-frame sample — builds with
`dxc.exe -nologo -Tvs_6_0` / `-Tps_6_0`
(`dx-samples-research-only/Samples/Desktop/D3D12HelloWorld/src/HelloTriangle/D3D12HelloTriangle.vcxproj:147-150`)
and loads the results as precompiled `.cso`
(`D3D12HelloTriangle.cpp:161-162`, `ReadDataFromFile(…"shaders_VSMain.cso")`). It uses
`D3D_ROOT_SIGNATURE_VERSION_1` (`:150`) and `D3D_FEATURE_LEVEL_11_0` (`:63, 74`).

**Consequence for planning:** "FL 11_0 + Tier 1 + SM 5.1" is a valid *DDI* floor, but it is not
a runnable milestone. The first meaningful bring-up target is **FL 11_0 + SM 6.0** — which the
Helios substrate already reaches (§4.3/§4.4) and which is exactly what vkd3d-proton would
report unmodified.

### 5.3 Simplest-first sample ladder (all in `dx-samples-research-only/Samples/Desktop/`)

| # | Sample | What it exercises | Shader |
|---|---|---|---|
| 0 | `D3D12HelloWorld/src/HelloWindow` | device + command queue + swapchain + `ClearRenderTargetView` + Present. **No shaders at all** — the ideal first D3D12 target because it separates device/queue/present from the shader path entirely. | none |
| 1 | `D3D12HelloWorld/src/HelloTriangle` | root signature (empty, `D3D_ROOT_SIGNATURE_VERSION_1`), input layout, graphics PSO, one draw | vs/ps_6_0 |
| 2 | `D3D12HelloWorld/src/HelloConstBuffers` | CBV, descriptor heap, per-frame updates | vs/ps_6_0 |
| 3 | `D3D12HelloWorld/src/HelloTexture` | SRV, upload heap, `CopyTextureRegion`, sampler | vs/ps_6_0 |
| 4 | `D3D12HelloWorld/src/HelloFrameBuffering` | fences, frame pacing — the first thing to stress Helios' monitored-fence story (DX12.md §3.5) | vs/ps_6_0 |
| 5 | `D3D12HelloWorld/src/HelloBundles` | bundle command lists | vs/ps_6_0 |
| 6 | `D3D12nBodyGravity` / `D3D12Multithreading` | compute queue + multi-threaded recording — the first thing that meets the one-node KMD (DX12.md §3.1) | cs/vs/ps_6_0 |
| 7 | `D3D12SM6WaveIntrinsics` | wave ops, `WaveLaneCountMin/Max`, `TotalLaneCount` | 6_0 |
| 8 | `D3D12ReservedResources` | tiled/reserved resources — the tier claim vs the decorative page tables (DX12.md §3.3) | 6_0 |
| 9 | `D3D12Residency` | `MakeResident`/`Evict`/`QueryVideoMemoryInfo` vs Helios' two-segment topology (DX12.md §3.2) | 6_0 |
| — | `D3D12MeshShaders`, `D3D12Raytracing`, `HelloWorkGraphs`, `HelloGenericPrograms` | **out of reach** at SM 6.0 (they need ms/as_6_5, lib_6_3, SM 6.8) | 6_3–6_8 |

`D3D12On7` is the only fxc/SM5 sample and is a Windows-7 compatibility path — not a useful gate.

---

## 6. The advertise-only-what-is-backed rule, D3D12 edition

DX12.md §5.5 states the rule with two D3D11-era proofs: `SupportDirectFlip = 1` made DWM stop
compositing an eligible visual while every fence stayed green
(`kmd_render/src/ddi/query_adapter_info.rs:439-455`), and `FlipImmediateMmIo` opted into a
contract requiring the flip to be complete when `SetVidPnSourceAddress` returns — at DIRQL,
where a virtio round-trip is illegal — costing 80 dropped binds and 145 of 1245 present markers
writing the buffer already on screen (`:377-388`).

D3D12's tiered caps are worse than D3D11's booleans in one specific way: **most of them change
what the application computes, not what the runtime calls.** An over-reported tier does not
produce a refused DDI call; it produces an app that lays out memory or indexes descriptors on a
promise the driver then breaks, silently.

Ranked by "silent corruption" rather than "clean failure":

**Tier A — corrupt pixels or memory, no error anywhere.**

1. `StandardSwizzle64KBSupported` — at the DDI, `D3D12DDI_TEXTURE_LAYOUT_CAPS_0026`
   (`d3d12umddi.h:5529-5535`) field `Supports64KStandardSwizzle` (`:5533`), with
   `D3D12DDI_D3D12_OPTIONS_DATA_0089.Deterministic64KBUndefinedSwizzle` (`:11095`) alongside. If
   TRUE, applications write texture tiles **CPU-side** in the standard 64 KiB swizzle and expect
   the GPU to read them back identically. On Helios the real layout is chosen host-side by
   venus/NVIDIA and is not knowable to the guest. Over-reporting this yields garbage texels with
   no error path at all. vkd3d hardcodes it FALSE (`device.c:10184`) — copy that.
2. `TiledResourcesTier ≥ 1` without a real tile-mapping backend. D3D12 reserved resources are
   defined by GPU-VA remapping; Helios' page tables are **explicitly decorative**
   (`kmd_render/src/ddi/gpummu.rs:1-14`: "the host GPU owns the real MMU, so the guest page
   tables are *decorative* — their content is never read by any hardware"). Reads from
   non-resident tiles are required to return zero; if the mapping is a no-op, they return
   whatever was there. Also note `UpdateTileMappings` has no failure return the app can see.
3. `ROVsSupported` = TRUE without real fragment-shader interlock. Rasterizer-ordered views
   promise deterministic per-pixel ordering; without it, blended/OIT results are
   non-deterministically wrong and **frame-rate dependent** — the hardest possible bug to
   attribute, and this project has already burned four sessions on a frame-rate-dependent
   visual defect (memory: 58th, "0ab-B scales with FRAME RATE").
4. `ConservativeRasterizationTier ≥ 3` = `SV_InnerCoverage` is meaningful. Wrong coverage is
   silently wrong geometry.
5. `TypedUAVLoadAdditionalFormats` = TRUE for a format the backend cannot type-load: garbage
   loads, no error.
6. `WaveLaneCountMin`/`WaveLaneCountMax`/`TotalLaneCount`. ⚠ **This is a live, already-known
   wrong value on Helios**: vkd3d falls back to `TotalLaneCount = 32 * subgroupSize` = **1024**
   with a `WARN`/`FIXME` (`device.c:10226-10233`) because venus exposes neither
   `VK_AMD_shader_core_properties` nor `VK_NV_shader_sm_builtins`. Applications that size
   dispatches or persistent-thread pools off `TotalLaneCount` will under-occupy the GPU by
   ~24×. That is a *performance* lie, not a correctness one — but it is exactly the kind of
   number that will later be blamed on the transport.
7. `ResourceHeapTier 2` when the heap cannot actually hold all three resource categories:
   aliased placed resources overlap. vkd3d derives it from memory-type masks
   (`device.c:9983-10008`) precisely to avoid this.

**Tier B — hangs, TDRs, or scheduler bugchecks.**

8. `D3D12DDICAPS_TYPE_EXECUTECOMMANDLISTS_PARALLELISM` (1069, `pData = BOOL`,
   `d3d12umddi.h:128`). **[INFER]** TRUE tells the runtime it may drive `ExecuteCommandLists`
   concurrently on multiple threads against the same device. Helios has one 3D node
   (`kmd_render/src/ddi/query_adapter_info.rs:1254-1278`, `NbAsymetricProcessingNodes = 1` at
   `:456-464`, DX12.md §3.1) and a single-context submit path. Report **FALSE** until proven.
   ⚠ **UNVERIFIED** — this is the one cap in the list whose exact contract I could not find
   documented. **Settling read:** the WDK docs for `D3D12DDICAPS_TYPE_EXECUTECOMMANDLISTS_PARALLELISM`,
   or an ETW `Microsoft-Windows-DxgKrnl` trace showing concurrent `QueuePacket` submits when it
   is set.
9. `D3D12DDICAPS_HARDWARE_SCHEDULING_CAPS_0050.ComputeQueuesPer3DQueue` ≠ 0
   (`d3d12umddi.h:7004-7008`). Non-zero opts into scheduling groups
   (`D3D12DDIARG_CREATESCHEDULINGGROUP_0050`, `:7010-7017`), which lands on hardware queues that
   `DxgkDdiCreateHwQueue` refuses (`kmd_render/src/ddi/scheduler.rs:180-187`). The KMD's refusal
   is *at queue creation* specifically to avoid the "succeed at create, fail at submit" VidSch
   `0x119`/Arg1=2 bugcheck (DX12.md §5.2). Report **0**.
10. `EnhancedBarriersSupported` (`D3D12DDI_D3D12_OPTIONS_DATA_0089`, `d3d12umddi.h:11111`).
    Opting in changes the *entire* barrier vocabulary the runtime uses; a driver that silently
    treats enhanced barriers as legacy loses synchronisation, which on this stack means a
    Venus-side write/read race with no guest-visible error. Report **FALSE** until the barrier
    path is real.
11. `RenderPassTier` above what the backend honours: tile-based load/store ops get dropped.
12. Shader-model over-report. Claiming SM 6.6 when the DXIL→SPIR-V path cannot express, say,
    64-bit atomics means the PSO either fails (good) or compiles to something that races (bad).
    vkd3d's `d3d12_device_validate_shader_meta` (`device.c:11670-11790`) exists exactly to make
    this the "good" case, by re-reading the emitted SPIR-V's `OpCapability` list and failing PSO
    creation. **Any native Helios D3D12 UMD must implement the same back-check.**

**Tier C — clean failure, therefore safe-ish.** `MeshShaderTier`, `RaytracingTier`,
`SamplerFeedbackTier`, `WorkGraphsTier`, `VariableShadingRateTier`: an app that asks for these
without support fails at `CreateStateObject`/`CreatePipelineState` and usually has a fallback.

**The Helios-specific version of DX12.md §5.5, in one sentence:** *for D3D12, the caps that
must be under-reported are the ones that change what the application writes into memory —
swizzle, tiling, heap tier, typed-UAV formats and lane counts — because on this stack the guest
does not own the layout and cannot detect the mismatch; the caps that may be over-reported
"safely" are the ones that produce an HRESULT.*

⚠ **UNVERIFIED:** whether the D3D12 runtime cross-validates the caps set as one contract the way
`CDevice::LLOCompleteLayerConstruction` does for D3D11 (`umd/src/caps.rs:39-42` records that a
partial edit is rejected with `DXGI_ERROR_UNSUPPORTED`). The existence of the in/out
`D3D12DDICAPS_TYPE_0081_3DPIPELINESUPPORT1` negotiation (`d3d12umddi.h:10416-10420`) suggests
some cross-validation, but I found no statement that e.g. claiming FL 12_0 with
`TiledResourcesTier 1` is rejected. **Settling experiment:** the logging-`OpenAdapter12` shim of
§1.2, answering deliberately inconsistent caps and reading the ETW
`Microsoft-Windows-DxgKrnl` `AzureTriage` reason (recipe in ROADMAP.md).

---

## 7. A caps bring-up ladder

Each rung: what to turn on, the probe that proves it, the sample that exercises it. Rungs S0–S2
are **reads and measurements only** and require no D3D12 DDI code — they answer DX12.md §2's
questions 1 and 2 for this lane specifically.

| Rung | Enable | Probe that proves it | Sample / test |
|---|---|---|---|
| **S0** | *nothing* — build the pinned `vkd3d-proton-helios` (needs `git submodule update --init subprojects/dxil-spirv khronos/*`; §3.1 says all three are empty) and dump the caps it computes on the guest ICD | new `tools/d3d12_caps_probe.cpp`: `D3D12CreateDevice` + `CheckFeatureSupport` for `D3D12_OPTIONS`, `OPTIONS1..21`, `SHADER_MODEL`, `FEATURE_LEVELS`, `ARCHITECTURE1`, `D3D12_FEATURE_FORMAT_SUPPORT` over the whole DXGI format table. Print one line per cap. | none — this is a read |
| **S1** | Confirm or refute §4.3's SM 6.0 prediction | S0's `D3D12_FEATURE_SHADER_MODEL`. Then re-run with `VKD3D_SHADER_MODEL=6_8` and diff the *rest* of the caps (FL should jump 12_1 → 12_2) | none |
| **S2** | Confirm the Vulkan-side inputs are what the table in §4.4 assumes | `vulkaninfo` re-captured with `VK_LOADER_LAYERS_DISABLE=*`, diffed against `tmp/dx12/research/guest-vulkaninfo-full.txt` | none |
| **S3** | **FL 11_0, no shaders** | `helios_paintcap` → `Z:\tmp\screen_copy.png` showing the sample's clearing colour cycle; `tools/kmd-counter-snapshot.ps1` diff with all failure counters at 0; `tools/umd-gate-surface.ps1 -AllProcesses` clean | `D3D12HelloWorld/src/HelloWindow` |
| **S4** | **SM 6.0 + root signature 1.1 + graphics PSO** | same screenshot gate; plus a shader-blob dump (the D3D12 analogue of `HKLM\SOFTWARE\Helios\ShaderBytecodeDumpPath`, §2.1) whose first dword is checked to settle §1.2's UNVERIFIED | `HelloTriangle` |
| **S5** | **Descriptor heaps: CBV → SRV/sampler** | S0 probe re-run + pixel-diff of the rendered texture against a golden PNG | `HelloConstBuffers`, then `HelloTexture` |
| **S6** | **Fences / frame pacing** | `tools/vnc_frame_probe.py` + `tools/vnc_scanout_correlate.py` black-frame percentage, compared against the D3D11 0ab-A/B/C numbers in ROADMAP.md | `HelloFrameBuffering` |
| **S7** | **Compute queue + bundles** | `tools/kmd-counter-snapshot.ps1`: `HwQRef` must **not** move (proving nothing tried a hardware queue); ETW `DxgKrnl` slice showing all packets on node 0 | `HelloBundles`, `D3D12nBodyGravity` |
| **S8** | **`ResourceBindingTier` ≥ 2 claim** | S0 probe; plus a descriptor-heap stress that exceeds Tier-1 limits | `D3D12DynamicIndexing` |
| **S9** | **`WaveOps` + lane counts** — and fix the `TotalLaneCount = 1024` lie (§6 item 6) | S0 probe reports `WaveLaneCountMin/Max/TotalLaneCount`; compare against the host's real SM count | `D3D12SM6WaveIntrinsics` |
| **S10** | **`TypedUAVLoadAdditionalFormats`, `ROVsSupported`, `ConservativeRasterizationTier`** — each only after a per-cap pixel test | golden-image diff per cap; each refusal counted | `D3D12ExecuteIndirect`, `D3D12VariableRateShading` |
| **S11** | **`TiledResourcesTier`** — last, and only with the reserved-resource semantics actually proven (zero reads from unmapped tiles) | `D3D12ReservedResources` visual + a dedicated unmapped-tile-reads-zero probe | `D3D12ReservedResources` |
| **S12** | **`ResourceHeapTier 2` / residency** | `tools/vram_report_probe.cpp` extended to D3D12 `QueryVideoMemoryInfo` (DX12.md §3.2 already names this) | `D3D12Residency`, `D3D12SmallResources` |
| **S13** | **SM 6.2+** — only after §4.3 is resolved at its root (ICD or fork), never by an env var in a shipped configuration | S0 probe + fp16/fp64 numerical tests | `D3D12MeshShaders` becomes reachable only at SM 6.5 |

Rungs S3 onward are only meaningful under a *chosen* strategy (DX12.md §2 (a) vs (b)); under (b)
they are vkd3d-proton runs, under (a) they are UMD milestones. The rung order is the same either
way, which is the point: **it is a caps ladder, not a code ladder.**

---

## 8. Load-bearing facts other lanes must not contradict

1. `vkd3d-proton-helios/subprojects/dxil-spirv` is an **empty directory**; vkd3d-proton cannot
   be built from this tree as-is. Same for `khronos/Vulkan-Headers`, `khronos/SPIRV-Headers`.
2. vkd3d-proton contains **no** DXBC-TPF→SPIR-V compiler; DXBC *and* DXIL both go to dxil-spirv
   (`vkd3d_shader_main.c:212`).
3. `dxvk-helios/subprojects/dxbc-spirv` contains **no** DXIL support (zero grep hits).
4. The D3D12 UMD DDI passes shader bytecode with **no length parameter anywhere**
   (`grep BytecodeLength d3d12umddi.h` → nothing).
5. `D3D12DDICAPS_TYPE_3DPIPELINESUPPORT` for D3D12 is a **maximum level**, not a bitmask
   (`d3d12umddi.h:2922-2923`) — the opposite of the D3D11 cap Helios already implements.
6. The D3D12 DDI has **no root signature version 1.0** (`d3d12umddi.h:3743-3747`) and hands the
   driver a **parsed** root signature (`:3749-3758`), whereas vkd3d parses the serialised blob.
7. The Helios guest ICD reports `driverID = DRIVER_ID_MESA_VENUS` with
   `shaderDenormPreserveFloat32 = false` and `shaderDenormFlushToZeroFloat32 = false`; vkd3d's
   SM-6.2 gate (`device.c:10694-10711`) exempts only `VK_DRIVER_ID_NVIDIA_PROPRIETARY`.
8. `VK_EXT_shader_stencil_export`, `VK_EXT_pageable_device_local_memory`,
   `VK_EXT_descriptor_buffer`, `VK_KHR_maintenance8`, `VK_NV_raw_access_chains` are **absent**
   from the guest ICD; `VK_EXT_conservative_rasterization`, `VK_EXT_fragment_shader_interlock`,
   `VK_EXT_mesh_shader`, `VK_KHR_ray_tracing_pipeline`, `VK_KHR_acceleration_structure`,
   `VK_KHR_shader_maximal_reconvergence`, `VK_KHR_shader_quad_control`,
   `VK_KHR_compute_shader_derivatives`, `VK_KHR_maintenance1..7` are **present**.
9. The DXIL validator hash is verified by the **D3D12 runtime**, before the driver
   ([devblogs.microsoft.com/directx/open-sourcing-dxil-validator-hash/](https://devblogs.microsoft.com/directx/open-sourcing-dxil-validator-hash/)).
   A driver neither signs nor validates. Replacing `d3d12core.dll` with vkd3d removes the check.
10. 174 of 178 shader-compile steps in `dx-samples-research-only/Samples/Desktop` use
    `dxc -T*_6_x`; only `D3D12On7` uses `fxc`.

## 9. Open questions, each with its settling experiment

| # | Question | Settling experiment |
|---|---|---|
| Q1 | Does the D3D12 runtime hand the UMD a DXBC container or a raw stream, per shader model? | Logging-only `OpenAdapter12` + `pfnCreateShader` that dumps the first 8 dwords; run `HelloTriangle` from a cloned scheduled task. §1.2 |
| Q2 | Does the runtime cross-validate the D3D12 caps set as one contract (the D3D11 `LLOCompleteLayerConstruction` analogue)? | Same shim, answering deliberately inconsistent caps; read ETW `Microsoft-Windows-DxgKrnl` → `AzureTriage`. §6 |
| Q3 | Exact contract of `D3D12DDICAPS_TYPE_EXECUTECOMMANDLISTS_PARALLELISM`. | WDK doc read, or ETW trace of concurrent `QueuePacket` submits with the cap set. §6 item 8 |
| Q4 | Can venus honour SPIR-V fp32 `DenormPreserve`/`DenormFlushToZero` execution modes despite the host reporting `false`? | Read `icd/mesa/src/virtio/vulkan/` for float-controls handling; then a SPIR-V test that round-trips a denormal. §4.3 fix 3 |
| Q5 | `TypedUAVLoadAdditionalFormats` and `ResourceHeapTier` on this ICD. | `tools/d3d12_caps_probe.cpp` (rung S0) — not derivable from `vulkaninfo`. §4.4 |
| Q6 | Is the `guest-vulkaninfo-full.txt` capture perturbed by `VK_LAYER_OBS_HOOK` (present at line 1-2)? | Re-capture with `VK_LOADER_LAYERS_DISABLE=*` and diff. §4.3 |
| Q7 | Does a WDDM 2.1 adapter (`kmd_render/src/ddi/wddm_surface.rs:64`) constrain the shader models a D3D12 UMD may report? The WDDM history table says WDDM 2.1 = "Shader Model 6.0" (`windows-driver-docs-pr/display/windows-vista-display-driver-model-design-guide.md:36`) and WDDM 3.2 = SM 6.8 (`:45`). | Read the WDK's `DXGKDDI_INTERFACE_VERSION` ↔ shader-model requirements; then have the caps shim report SM 6.5 at WDDM 2.1 and see whether the runtime accepts it. This interacts with the `E_NOTIMPL`/MPO3 reason WDDM 3.2 is unselected (DX12.md §3.4) |
| Q8 | Does `dxil-spirv` (uncheckable here) require SPIR-V/Vulkan features beyond the option list in §3.3? | `git submodule update --init subprojects/dxil-spirv` and read it; or run rung S0 and read the `INFO`/`WARN` lines vkd3d emits at device init |

---

## 10. Direct implications for the Helios D3D12 plan

1. **§4.3 is a free, decisive datum for DX12.md §2's decision** and it favours neither strategy —
   it is a *substrate* fact. Both (a) and (b) inherit the SM 6.0 ceiling, because both go through
   dxil-spirv over the same venus ICD. Record it in DX12.md §2 as evidence item 2 ("what does the
   venus ICD actually expose") already partially answered, with numbers.
2. **The first D3D12 milestone should be `HelloWindow`, not `HelloTriangle`** — it isolates
   device/queue/present from shaders entirely, and the shader path is where the missing
   dxil-spirv submodule bites.
3. **The `vkd3d-proton-helios` fork now has a candidate purpose** (DX12.md §1.3 says none is
   recorded): extending the SM-6.2 denorm exemption to `VK_DRIVER_ID_MESA_VENUS`. That is a
   two-line change with real semantics and a real risk (§4.3 fix 2). It should be taken only
   after fix 1 (`VKD3D_SHADER_MODEL=6_8`) has measured what actually breaks, and it must carry
   the evidence in a comment at the change site — CLAUDE.md's "a knob's default is a decision"
   rule applies to a forked constant exactly as it does to a registry knob.
4. **Reuse `shader_code_len()` verbatim.** `umd/src/forward/shaders.rs:13-39` already implements
   the exact container-vs-token discrimination and bounds-checking a D3D12 UMD needs on a
   length-less blob. Do not re-derive it.
5. **Port `ShaderBytecodeDumpPath` to D3D12 in the first shader commit.**
   `umd/bridge/bridge_dxbc.cpp:39-83` is the cheapest instrument for settling Q1 and for every
   later shader defect.
6. **Copy `d3d12_device_validate_shader_meta`'s posture, not just its list.** vkd3d re-reads the
   *emitted* SPIR-V's `OpCapability` set and fails PSO creation when a shader needs something the
   reported caps disclaim (`libs/vkd3d/device.c:11670-11790`). That is the mechanism that turns
   an over-reported cap from silent corruption into a clean HRESULT, and it is the concrete
   answer to DX12.md §5.5 for shaders.
7. **Three caps must be pinned to their conservative values from the first commit**, with the
   reason in the comment: `ComputeQueuesPer3DQueue = 0`, `EXECUTECOMMANDLISTS_PARALLELISM =
   FALSE`, `StandardSwizzle64KBSupported = FALSE`. Each has a named KMD reality behind it
   (one node / one context / host-owned layout), and each is a Tier-A or Tier-B hazard in §6.
8. **`TotalLaneCount = 1024` is already wrong today**, whichever strategy wins, and it will look
   like a Helios performance defect. File it in ROADMAP.md now, with `device.c:10226-10233` as
   the citation, so the first person who sees a 24× under-occupied dispatch does not spend a
   session on it.
