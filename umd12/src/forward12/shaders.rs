//! L6b — the shader half of L6: the 14 shader-create slots of
//! `DEVICE_FUNCS_CORE_0109` group (c), and the DXBC container the engine needs
//! and the DDI does not supply.
//!
//! # ⭐⭐ The finding that shapes this whole file
//!
//! The DDI hands `pfnCreate*Shader` a **bare `DxilProgramHeader`** — measured
//! at `D12-G5` and recorded in `DDI_REFERENCE.md` §12.2, three slots in one
//! run, first dwords `00010060 0000010a 4c495844 …`: `(programType << 16) |
//! (major << 4) | minor`, then the length in dwords, then `'DXIL'`. That is the
//! *payload of a DXBC `DXIL` part*, with no container around it.
//!
//! ⛔ **vkd3d cannot consume that.** Every entry point on the engine's shader
//! path parses a container first, and both of them fail on the first dword:
//!
//! * `dxil_spv_parse_dxil_blob` -> `DXILContainerParser::parse_container`
//!   (`subprojects/dxil-spirv/dxil_parser.cpp:435-444`) reads a
//!   `DXIL::ContainerHeader` and `return false` unless its fourcc is `'DXBC'`;
//! * `shader_is_dxil` (`libs/vkd3d-shader/dxbc.c:341-352`) and
//!   `vkd3d_shader_parse_input_signature`
//!   (`libs/vkd3d-shader/vkd3d_shader_main.c:308-313`) both go through
//!   `parse_dxbc`, which rejects a non-`'DXBC'` tag (`dxbc.c:118-122`).
//!
//! ⇒ **this driver must synthesise the container.** `DDI_REFERENCE.md` §12.3
//! left that open — *"if §12.2's UNVERIFIED resolves to 'D3D12 always passes a
//! container', `bridge_dxbc.cpp` is not needed at all"* — and §12.2 then
//! resolved it the other way: **never** a container. So the D3D11 driver's
//! container synthesiser is needed here too, in its D3D12 form.
//!
//! # What the container must carry, and what each part is for
//!
//! | part | consumer | why |
//! |---|---|---|
//! | `DXIL` | dxil-spirv | the bitcode. `parse_dxil` (`dxil_parser.cpp:75-97`) reads a `ProgramHeader` from the part start — which is byte-for-byte what the DDI gave us — and substreams the bitcode at `bitcode_offset` |
//! | `ISG1` | **vkd3d only** | `vkd3d_validate_vertex_input_signature` (`state.c:5279-5310`) matches the input layout's `SemanticName`/`SemanticIndex` against it, and `state.c:5924` takes the matched element's `register_index` as the Vulkan vertex input **location** |
//! | `OSG1` | **vkd3d only** | `vkd3d_validate_shader_io_signatures` (`state.c:5165-5200`) matches consecutive stages by semantic name, index, register, component type and min precision |
//! | `PSG1` | **vkd3d only** | the tessellation patch-constant signature, same matching |
//!
//! ⭐ **dxil-spirv parses the signature parts and then never reads them**:
//! `parse_iosg1` fills `DXILContainerParser::{input,output}_elements`
//! (`dxil_parser.cpp:500-513`) and `dxil_spv_parse_dxil_blob` takes only
//! `get_blob()` and `get_rdat_subobjects()` from the parser
//! (`dxil_spirv_c.cpp:432-441`). Vertex input **locations** therefore come from
//! the DXIL metadata, i.e. `location == startRow == the DDI's `Register``. That
//! is what makes fabricated semantic names safe: they are a matching key
//! between two things this driver writes, and the number that has to be right —
//! the register — is passed through unchanged.
//!
//! # ⭐ Fabricated semantic names — the D3D11 precedent, verbatim
//!
//! The DDI's signature entries carry no name (`D3D12DDIARG_SIGNATURE_ENTRY_0012`
//! is `SystemValue`/`Register`/`Mask`/`Stream`/`RegisterComponentType`/
//! `MinPrecision`), and neither does `D3D12DDIARG_INPUT_ELEMENT_DESC`, which
//! names an `InputRegister` instead. `umd/src/forward/layout.rs:100-166`
//! solved exactly this for D3D11 by fabricating `TEXCOORD` with the register as
//! the semantic index on **both** sides. `pso.rs`'s element-layout translation
//! uses the same convention, so the two agree by construction.
//!
//! ⛔ The one place a fabricated name is NOT enough is the tessellation patch
//! signature: the DDI carries `D3D10_SB_NAME` token values (11..22, one per
//! individual edge/inside factor) while a DXBC signature carries the collapsed
//! `D3D_NAME` reflection values (11..16) plus a semantic index.
//! `umd/bridge/bridge_dxbc.cpp:124-155` records what the absence of that
//! translation cost: *"hull shader tess factors are not declared as SPIR-V
//! TessLevelOuter/Inner built-ins and tessellated draws can disappear."* The
//! table is ported below.

use core::ffi::c_void;

use helios_umd_common::slot::{Boxed, BoxedHandle, DdiHandle, Slot};

use super::pso::{set_error_if_possible, L6_REFUSALS};
use super::tables12::{stage, DeviceCoreTable, Filling};
use crate::ddi12;
use crate::{log_error, note_refusal, trace_line};

helios_umd_common::boxed_handles!(
    crate::ddi12::D3D12DDI_HSHADER => ShaderState,
);

/// How many `log_error!` lines each repeating arm in this file may emit.
///
/// Same shape as `caps12`'s budgets: the shipping default must not turn a
/// per-PSO path into an unbounded writer, and the first few are what triage
/// needs. Per-call detail goes to [`trace_line!`], which is gated on
/// `HKLM\SOFTWARE\Helios!Umd12Trace`.
const LOG_BUDGET: usize = 32;

// ---------------------------------------------------------------------------
// The stage, and the program type that proves the slot
// ---------------------------------------------------------------------------

/// Which `pfnCreate*Shader` slot a handle came from.
///
/// ⛔ **One Rust `extern` fn per stage, and the stage is a compile-time constant
/// at each.** Six of the create slots share one typedef
/// (`PFND3D12DDI_CREATE_SHADER_0026`) and the stage is not a parameter
/// (`DDI_REFERENCE.md` §3.2 (c)); the union arm inside
/// `D3D12DDIARG_CREATE_SHADER_0026::IOSignatures` is selected by the stage and
/// nothing else. `umd/src/forward/shaders.rs:167-180` documents the D3D11 form
/// of the same trap — *"reading a tess block with the 2-word accessor silently
/// returns `n_patch` as the first entry's system value"*.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShaderStage {
    Vertex,
    Pixel,
    Geometry,
    Compute,
    Hull,
    Domain,
    Amplification,
    Mesh,
}

impl ShaderStage {
    /// The name this stage prints under in the log.
    pub(crate) fn name(self) -> &'static str {
        match self {
            ShaderStage::Vertex => "VS",
            ShaderStage::Pixel => "PS",
            ShaderStage::Geometry => "GS",
            ShaderStage::Compute => "CS",
            ShaderStage::Hull => "HS",
            ShaderStage::Domain => "DS",
            ShaderStage::Amplification => "AS",
            ShaderStage::Mesh => "MS",
        }
    }

    /// The DXIL program kind this stage's bytecode must declare in dword 0's
    /// high 16 bits.
    ///
    /// ⭐ Not decoration: `DDI_REFERENCE.md` §12.2 measured *"the type field
    /// matched the slot it arrived on in every case"* across three slots, so a
    /// mismatch is a real finding about which slot the runtime called — the one
    /// cross-check available on data this driver otherwise forwards blind.
    /// Values are DXC's `DXIL::ShaderKind`: Pixel 0, Vertex 1, Geometry 2,
    /// Hull 3, Domain 4, Compute 5, Mesh 13, Amplification 14.
    fn dxil_program_kind(self) -> u32 {
        match self {
            ShaderStage::Pixel => 0,
            ShaderStage::Vertex => 1,
            ShaderStage::Geometry => 2,
            ShaderStage::Hull => 3,
            ShaderStage::Domain => 4,
            ShaderStage::Compute => 5,
            ShaderStage::Mesh => 13,
            ShaderStage::Amplification => 14,
        }
    }

    /// Which arm of `D3D12DDIARG_CREATE_SHADER_0026::IOSignatures` this stage
    /// selects (`DDI_REFERENCE.md` §12.1).
    fn io_arm(self) -> IoArm {
        match self {
            ShaderStage::Hull | ShaderStage::Domain => IoArm::Tessellation,
            ShaderStage::Mesh => IoArm::Mesh,
            _ => IoArm::Standard,
        }
    }
}

/// The union arm the stage selects. Never a value read from the runtime — the
/// DDI carries no tag, which is precisely why this is derived from the slot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum IoArm {
    Standard,
    Tessellation,
    Mesh,
}

// ---------------------------------------------------------------------------
// The payload
// ---------------------------------------------------------------------------

/// What a `D3D12DDI_HSHADER` slot owns.
///
/// ⚠ `pub`, not `pub(crate)`, and it is a compiler rule rather than a design
/// choice: this type is named as `<D3D12DDI_HSHADER as BoxedHandle>::State`,
/// and E0446 forbids a `pub(crate)` type in a public trait impl's associated
/// type. `umd/src/forward/layout.rs:22-32` carries the same note for
/// `LayoutData`. `forward12` is a private module of a `cdylib`, so nothing can
/// name it from outside.
pub struct ShaderState {
    /// Which create slot produced this handle.
    pub(crate) stage: ShaderStage,

    /// The synthesised DXBC container: signature parts plus the `DXIL` part.
    ///
    /// ⭐ **Built at create time, not at PSO time, and owned.** Two reasons,
    /// both load-bearing:
    ///
    /// * the IO signature blocks are `_In_ CONST` pointers into runtime memory
    ///   for the duration of *this* call and there is no documented guarantee
    ///   they outlive it, while `pfnCreatePipelineState` runs later — so the
    ///   signatures have to be consumed here or copied here, and consuming them
    ///   into the container is the smaller of the two;
    /// * `pShaderCode` is the same shape of borrow. `DDI_REFERENCE.md` §12.1
    ///   describes an argument struct the runtime owns; nothing in
    ///   `d3d12umddi.h` says the bytes persist. A PSO built from freed bytecode
    ///   is a crash with no line pointing at this file, so the bytes are copied
    ///   unconditionally.
    pub(crate) container: Box<[u8]>,

    /// The root signature the runtime associated with this shader at create
    /// time.
    ///
    /// ⚠ **Not used to build the PSO.** `D3D12DDIARG_CREATE_PIPELINE_STATE_0099`
    /// carries its own `hRootSignature` and that is the one the pipeline is
    /// built with. This one exists so a disagreement between the two is a
    /// *number* rather than an assumption:
    /// [`crate::forward12::pso::create_pipeline_state`] compares them and bumps
    /// `L6ShaderRootSignatureMismatch`.
    pub(crate) h_root_signature: *mut c_void,
}

// ---------------------------------------------------------------------------
// Bytecode length — ported from the D3D11 driver, bounds and all
// ---------------------------------------------------------------------------

/// The byte length of the bytecode behind `pShaderCode`.
///
/// ⛔ **There is no length parameter anywhere in the shader-create DDIs**
/// (`DDI_REFERENCE.md` §12.2, measured at P1: `pShaderCode` is a bare
/// `*const UINT`). The blob is self-describing and this is the reader.
///
/// Ported from `umd/src/forward/shaders.rs:13-39` **including both bounds
/// checks**, which is what `DDI_REFERENCE.md` §12.2 asks for in as many words:
/// *"Copy it verbatim into `helios_umd12`, including both bounds checks and the
/// log line."* CLAUDE.md's *validate every runtime-supplied size & offset before
/// reading* is the rule they discharge — ten call sites downstream build a slice
/// out of the result.
///
/// ⚠ **The DXBC-container branch is expected to be dead on this DDI** and is
/// kept anyway, with its own counter: §12.2 measured a raw stream in every
/// sample, so a container arriving would be a change in the runtime's behaviour
/// and worth recording rather than assuming away.
///
/// # Safety
/// `code`, when non-null, must address at least two readable `UINT`s, and — if
/// the first is `'DXBC'` — at least seven.
unsafe fn shader_code_len(code: *const u32) -> usize {
    if code.is_null() {
        return 0;
    }

    // D3D API bytecode is a DXBC container with the total size at byte offset
    // 24. Bound it the same way the raw-stream arm below is bounded: the dword
    // at offset 24 is read BEFORE anything is known about the container's real
    // size. Require at least the 32-byte container header and at most 1 << 20
    // dwords.
    // SAFETY: the caller guarantees the first dword is readable.
    if unsafe { *code } == u32::from_le_bytes(*b"DXBC") {
        L6_REFUSALS.shader_dxbc_container_seen.bump();
        // SAFETY: the caller guarantees seven readable dwords when the first is
        // the container tag.
        let total = unsafe { *code.add(6) } as usize;
        if total < 32 || total > (1 << 20) * core::mem::size_of::<u32>() {
            log_error!("shader_code_len: rejecting DXBC total size {total}");
            return 0;
        }
        return total;
    }

    // The live arm. `DDI_REFERENCE.md` §12.2: the runtime hands the DDI a raw
    // stream behind the two-token header, dword 1 is the length in dwords
    // INCLUDING that header, and dword 2 is `'DXIL'`.
    // SAFETY: the caller guarantees two readable dwords.
    let dwords = unsafe { *code.add(1) } as usize;
    // ⚠ The one deviation from the ported source: `umd`'s form is
    // `dwords < 2 || dwords > (1 << 20)`, which this crate's `-D warnings`
    // clippy bar rejects as `manual_range_contains`. The BOUNDS are identical —
    // at least the two-token header, at most 1 Mi dwords — which is what
    // `DDI_REFERENCE.md` §12.2 asked be copied.
    if !(2..=(1usize << 20)).contains(&dwords) {
        return 0;
    }

    dwords * core::mem::size_of::<u32>()
}

/// One line per shader create: length, container-ness, and the first four
/// dwords.
///
/// Ported from `umd/src/forward/shaders.rs:41-59`. §12.2 calls it *"the matching
/// instrument"* for [`shader_code_len`] and it is what turned a guess about the
/// blob format into a measurement.
///
/// # Safety
/// `code`, when non-null, must address at least four readable `UINT`s.
unsafe fn log_shader_code(stage: ShaderStage, code: *const u32, len: usize) {
    if code.is_null() {
        log_error!("Create{}Shader: null shader code", stage.name());
        return;
    }
    // SAFETY: the caller guarantees four readable dwords.
    let (d0, d1, d2, d3) = unsafe { (*code, *code.add(1), *code.add(2), *code.add(3)) };
    let is_dxbc = d0 == u32::from_le_bytes(*b"DXBC");
    let n = L6_REFUSALS.shader_creates.get();
    if n <= LOG_BUDGET {
        log_error!(
            "Create{}Shader: len={len} dxbc={is_dxbc} tokens={d0:08x} {d1:08x} {d2:08x} {d3:08x} \
             (x{n})",
            stage.name(),
        );
    } else {
        trace_line!(
            "Create{}Shader: len={len} dxbc={is_dxbc} tokens={d0:08x} {d1:08x} {d2:08x} {d3:08x}",
            stage.name(),
        );
    }
}

// ---------------------------------------------------------------------------
// The DXBC container writer
// ---------------------------------------------------------------------------

/// The largest signature this driver will encode.
///
/// Ported with its reasoning from `umd/bridge/bridge_dxbc.cpp:93-97`: D3D has at
/// most 32 input/output registers per stage, a few hundred is generous, and the
/// bound exists so the length arithmetic below **cannot be satisfied by a
/// wrapped sum** — it is the only thing between a runtime-supplied count and the
/// indexing that follows.
const MAX_SIGNATURE_ENTRIES: usize = 512;

/// The fixed part of a DXBC container header: tag, 16-byte digest, version,
/// total size, part count (`dxil-spirv`'s `DXIL::ContainerHeader`,
/// `dxil.hpp:35-43`; vkd3d's `VKD3D_DXBC_HEADER_SIZE`).
const DXBC_HEADER_SIZE: usize = 32;

/// A signature part to place ahead of the code part.
struct SignaturePart<'a> {
    /// Four bytes, one of `ISG1` / `OSG1` / `PSG1`.
    ///
    /// ⛔ **The `1` forms, not `ISGN`/`OSGN`/`PCSG`.** vkd3d accepts both
    /// (`dxbc.c:253`, `:268`, `:283`) but dxil-spirv's `DXIL::FourCC` names only
    /// `ISG1`/`OSG1`/`PSG1` (`dxil.hpp:82-84`) and its `parse_iosg1` reads the
    /// 8-dword form. Emitting the 6-dword `ISGN` under an `ISG1` tag, or the
    /// `1` layout under an `ISGN` tag, is a silent misparse in one of the two
    /// readers; emitting `ISG1` with the 8-dword layout is what a real DXC
    /// container looks like and is parsed correctly by both.
    tag: &'static [u8; 4],
    entries: &'a [ddi12::D3D12DDIARG_SIGNATURE_ENTRY_0012],
}

/// Bytes per `ISG1`/`OSG1`/`PSG1` element: stream, name offset, semantic index,
/// system value, component type, register, mask, min precision.
///
/// Cross-checked against **both** readers: vkd3d's `shader_parse_signature`
/// reads exactly these eight in this order when `has_stream_index` and
/// `has_min_precision` are set (`dxbc.c:207-240`), and dxil-spirv's
/// `parse_iosg1` reads the same eight (`dxil_parser.cpp:99-140`).
const SIGNATURE_ENTRY_BYTES: usize = 32;

/// What one DDI signature entry becomes in a DXBC signature part.
struct EncodedEntry {
    /// NUL-terminated; only ever one of the three constants below.
    name: &'static [u8],
    semantic_index: u32,
    system_value: u32,
}

const NAME_TEXCOORD: &[u8] = b"TEXCOORD\0";
const NAME_TESS_FACTOR: &[u8] = b"SV_TessFactor\0";
const NAME_INSIDE_TESS_FACTOR: &[u8] = b"SV_InsideTessFactor\0";

/// Translate one DDI signature entry's system value into the pair a DXBC
/// signature carries.
///
/// ⭐ Ported from `umd/bridge/bridge_dxbc.cpp:124-155`, table and comment both.
/// For a non-patch part the two enumerations agree — the assertions below pin
/// that claim to the generated header rather than to this sentence — so the
/// answer is `TEXCOORD<register>` with the system value passed through. For a
/// patch part the DDI's per-factor `D3D10_SB_NAME` values 11..22 must collapse
/// onto the `D3D_NAME` values 11..16 plus a semantic index, or hull-shader tess
/// factors never become `TessLevelOuter`/`TessLevelInner` built-ins.
fn encode_entry(is_patch_part: bool, system_value: u32, register: u32) -> EncodedEntry {
    let passthrough = EncodedEntry {
        name: NAME_TEXCOORD,
        semantic_index: register,
        system_value,
    };
    if !is_patch_part {
        return passthrough;
    }
    // `D3D_NAME_FINAL_QUAD_EDGE_TESSFACTOR` 11, `…QUAD_INSIDE…` 12,
    // `…TRI_EDGE…` 13, `…TRI_INSIDE…` 14, `…LINE_DETAIL…` 15,
    // `…LINE_DENSITY…` 16.
    match system_value {
        11 => EncodedEntry { name: NAME_TESS_FACTOR, semantic_index: 0, system_value: 11 },
        12 => EncodedEntry { name: NAME_TESS_FACTOR, semantic_index: 1, system_value: 11 },
        13 => EncodedEntry { name: NAME_TESS_FACTOR, semantic_index: 2, system_value: 11 },
        14 => EncodedEntry { name: NAME_TESS_FACTOR, semantic_index: 3, system_value: 11 },
        15 => EncodedEntry { name: NAME_INSIDE_TESS_FACTOR, semantic_index: 0, system_value: 12 },
        16 => EncodedEntry { name: NAME_INSIDE_TESS_FACTOR, semantic_index: 1, system_value: 12 },
        17 => EncodedEntry { name: NAME_TESS_FACTOR, semantic_index: 0, system_value: 13 },
        18 => EncodedEntry { name: NAME_TESS_FACTOR, semantic_index: 1, system_value: 13 },
        19 => EncodedEntry { name: NAME_TESS_FACTOR, semantic_index: 2, system_value: 13 },
        20 => EncodedEntry { name: NAME_INSIDE_TESS_FACTOR, semantic_index: 0, system_value: 14 },
        21 => EncodedEntry { name: NAME_TESS_FACTOR, semantic_index: 0, system_value: 15 },
        22 => EncodedEntry { name: NAME_INSIDE_TESS_FACTOR, semantic_index: 0, system_value: 16 },
        _ => passthrough,
    }
}

// ⛔ The three claims `encode_entry` and the writer below rest on, pinned to the
// generated header instead of to a comment. Each says "the DDI's enumeration
// and the DXBC signature's enumeration are the same integers here", which is
// exactly the kind of assertion `ARCHITECTURE.md` §12 rule 1 exists to force.
const _: () = assert!(
    ddi12::D3D10_SB_NAME_D3D10_SB_NAME_UNDEFINED == 0
        && ddi12::D3D10_SB_NAME_D3D10_SB_NAME_POSITION == 1
        && ddi12::D3D10_SB_NAME_D3D10_SB_NAME_VERTEX_ID == 6
        && ddi12::D3D10_SB_NAME_D3D10_SB_NAME_PRIMITIVE_ID == 7
        && ddi12::D3D10_SB_NAME_D3D10_SB_NAME_INSTANCE_ID == 8
        && ddi12::D3D10_SB_NAME_D3D10_SB_NAME_SAMPLE_INDEX == 10,
    "D3D10_SB_NAME must be value-identical to D3D_NAME over the non-patch range"
);
const _: () = assert!(
    ddi12::D3D10_SB_NAME_D3D11_SB_NAME_FINAL_QUAD_U_EQ_0_EDGE_TESSFACTOR == 11
        && ddi12::D3D10_SB_NAME_D3D11_SB_NAME_FINAL_LINE_DENSITY_TESSFACTOR == 22,
    "the patch-signature collapse table above is keyed on D3D10_SB_NAME's own values"
);
const _: () = assert!(
    ddi12::D3D10_SB_REGISTER_COMPONENT_TYPE_D3D10_SB_REGISTER_COMPONENT_UNKNOWN == 0
        && ddi12::D3D10_SB_REGISTER_COMPONENT_TYPE_D3D10_SB_REGISTER_COMPONENT_UINT32 == 1
        && ddi12::D3D10_SB_REGISTER_COMPONENT_TYPE_D3D10_SB_REGISTER_COMPONENT_SINT32 == 2
        && ddi12::D3D10_SB_REGISTER_COMPONENT_TYPE_D3D10_SB_REGISTER_COMPONENT_FLOAT32 == 3,
    "D3D10_SB_REGISTER_COMPONENT_TYPE must be value-identical to D3D_REGISTER_COMPONENT_TYPE"
);
const _: () = assert!(
    ddi12::D3D11_SB_OPERAND_MIN_PRECISION_D3D11_SB_OPERAND_MIN_PRECISION_DEFAULT == 0
        && ddi12::D3D11_SB_OPERAND_MIN_PRECISION_D3D11_SB_OPERAND_MIN_PRECISION_FLOAT_16 == 1
        && ddi12::D3D11_SB_OPERAND_MIN_PRECISION_D3D11_SB_OPERAND_MIN_PRECISION_FLOAT_2_8 == 2
        && ddi12::D3D11_SB_OPERAND_MIN_PRECISION_D3D11_SB_OPERAND_MIN_PRECISION_SINT_16 == 4
        && ddi12::D3D11_SB_OPERAND_MIN_PRECISION_D3D11_SB_OPERAND_MIN_PRECISION_UINT_16 == 5,
    "D3D11_SB_OPERAND_MIN_PRECISION must be value-identical to D3D_MIN_PRECISION"
);

/// Append a little-endian `u32`.
fn put32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Build the body of one `ISG1`/`OSG1`/`PSG1` part — everything after the
/// 8-byte part header.
///
/// Layout, from vkd3d's `shader_parse_signature` (`dxbc.c:175-248`): element
/// count, the constant `8` (the offset of the first element), the elements, then
/// the name bytes, padded to a dword. Name offsets are relative to the body
/// start, which is what `shader_get_string(data, …, name_offset)` indexes.
fn build_signature_body(part: &SignaturePart<'_>) -> Vec<u8> {
    let is_patch_part = part.tag == b"PSG1";
    let count = part.entries.len();
    let name_base = 8 + count * SIGNATURE_ENTRY_BYTES;

    let mut encoded = Vec::with_capacity(count);
    let mut name_offsets = Vec::with_capacity(count);
    let mut names_size = 0usize;
    for e in part.entries {
        let enc = encode_entry(is_patch_part, e.SystemValue as u32, e.Register);
        name_offsets.push(name_base + names_size);
        names_size += enc.name.len();
        encoded.push(enc);
    }

    let mut body = Vec::with_capacity(name_base + names_size + 3);
    put32(&mut body, count as u32);
    put32(&mut body, 8);
    for (i, e) in part.entries.iter().enumerate() {
        let enc = &encoded[i];
        // The mask is 4 bits of xyzw. The high byte is the read/write mask,
        // which the D3D11 bridge sets to the same value
        // (`bridge_dxbc.cpp:222`): a signature this driver synthesises has no
        // finer information, and vkd3d compares whole `mask` words between
        // stages, so both halves must be derived the same way on both sides.
        let mask = u32::from(e.Mask) & 0xF;
        // ⚠ `UNKNOWN` (0) means the token stream did not type the register.
        // Default it to float32, matching `bridge_dxbc.cpp:218-220` and the D3D
        // convention for untyped registers. ⛔ Do NOT default it to 0: the D3D11
        // driver's note records that typing a SINT input as float32 was
        // VUID-Input-08733 UB that rasterized nothing.
        let component_type = if e.RegisterComponentType == 0 {
            3
        } else {
            e.RegisterComponentType as u32
        };
        put32(&mut body, u32::from(e.Stream));
        put32(&mut body, name_offsets[i] as u32);
        put32(&mut body, enc.semantic_index);
        put32(&mut body, enc.system_value);
        put32(&mut body, component_type);
        put32(&mut body, e.Register);
        put32(&mut body, mask | (mask << 8));
        put32(&mut body, e.MinPrecision as u32);
    }
    for enc in &encoded {
        body.extend_from_slice(enc.name);
    }
    while body.len() % 4 != 0 {
        body.push(0);
    }
    body
}

/// Assemble a DXBC container out of the signature parts and the DDI's bytecode.
///
/// The bytecode becomes the `DXIL` part **verbatim**: it already is a
/// `DXIL::ProgramHeader` followed by the bitcode, which is exactly what
/// `DXILContainerParser::parse_dxil` expects at a part's first byte
/// (`dxil_parser.cpp:75-97`).
///
/// ⚠ The checksum is left zero. vkd3d says so in as many words —
/// `dxbc.c:124` is literally `WARN("Ignoring DXBC checksum.\n")` — and
/// dxil-spirv never reads the digest field at all. The DXIL **validator** hash
/// is a different thing entirely and lives inside the bitcode, where the D3D12
/// runtime already checked it before this driver was called
/// (`DDI_REFERENCE.md` §12.4).
fn build_dxbc_container(parts: &[SignaturePart<'_>], code: &[u8]) -> Box<[u8]> {
    let bodies: Vec<Vec<u8>> = parts.iter().map(build_signature_body).collect();

    let part_count = bodies.len() + 1;
    let mut offset = DXBC_HEADER_SIZE + part_count * 4;
    let mut part_offsets = Vec::with_capacity(part_count);
    for body in &bodies {
        part_offsets.push(offset);
        offset += 8 + body.len();
    }
    part_offsets.push(offset);
    let total = offset + 8 + code.len();

    let mut blob = Vec::with_capacity(total);
    blob.extend_from_slice(b"DXBC");
    blob.extend_from_slice(&[0u8; 16]);
    // Two `u16`s, major 1 and minor 0. vkd3d's `parse_dxbc` reads them as one
    // dword and rejects anything but `0x00000001` (`dxbc.c:127-133`).
    put32(&mut blob, 1);
    put32(&mut blob, total as u32);
    put32(&mut blob, part_count as u32);
    for off in &part_offsets {
        put32(&mut blob, *off as u32);
    }
    for (part, body) in parts.iter().zip(bodies.iter()) {
        blob.extend_from_slice(part.tag);
        put32(&mut blob, body.len() as u32);
        blob.extend_from_slice(body);
    }
    blob.extend_from_slice(b"DXIL");
    put32(&mut blob, code.len() as u32);
    blob.extend_from_slice(code);

    debug_assert_eq!(blob.len(), total);
    blob.into_boxed_slice()
}

// ---------------------------------------------------------------------------
// Reading the DDI's IO signature blocks
// ---------------------------------------------------------------------------

/// Borrow one signature array out of the runtime's block, refusing a count this
/// driver will not encode.
///
/// # Safety
/// `ptr`, when `count` is non-zero, must address `count` live
/// `D3D12DDIARG_SIGNATURE_ENTRY_0012`s for the duration of the call.
unsafe fn signature_slice<'a>(
    ptr: *const ddi12::D3D12DDIARG_SIGNATURE_ENTRY_0012,
    count: ddi12::UINT,
) -> &'a [ddi12::D3D12DDIARG_SIGNATURE_ENTRY_0012] {
    let count = count as usize;
    if ptr.is_null() || count == 0 {
        return &[];
    }
    if count > MAX_SIGNATURE_ENTRIES {
        note_refusal(&L6_REFUSALS.shader_signature_count_refused);
        log_error!("shader signature: refusing {count} entries (max {MAX_SIGNATURE_ENTRIES})");
        return &[];
    }
    // SAFETY: non-null and bounded per the checks above; the caller guarantees
    // the array is live and `count`-long.
    unsafe { core::slice::from_raw_parts(ptr, count) }
}

/// The signature parts for one stage, read out of the union arm the stage
/// selects.
///
/// ⛔ The arm comes from `stage`, never from the data. There is no tag in
/// `D3D12DDIARG_CREATE_SHADER_0026::IOSignatures` — reading `Tessellation` on a
/// vertex shader would walk two pointers past the end of the runtime's block.
///
/// # Safety
/// `io` must be the `IOSignatures` union of a live
/// `D3D12DDIARG_CREATE_SHADER_0026`, and the pointer in the arm `stage` selects
/// must be null or address a live block of that arm's type.
unsafe fn signature_parts<'a>(
    stage: ShaderStage,
    io: &ddi12::D3D12DDIARG_CREATE_SHADER_0026__bindgen_ty_1,
) -> Vec<SignaturePart<'a>> {
    let mut parts = Vec::with_capacity(3);
    match stage.io_arm() {
        IoArm::Standard => {
            // SAFETY: the caller guarantees this arm is the live one for this
            // stage; every arm is a pointer at offset 0 (machine-checked by the
            // bindgen layout assertions), so reading it is defined.
            let p = unsafe { io.Standard };
            if p.is_null() {
                return parts;
            }
            // SAFETY: non-null per the check, live per the caller.
            let s = unsafe { &*p };
            // SAFETY: every arm of these two unions is a pointer at offset 0.
            let (inputs, outputs) = unsafe { (s.__bindgen_anon_1.pInputSignature, s.__bindgen_anon_2.pOutputSignature) };
            // SAFETY: the runtime owns both arrays for this call and states
            // their lengths in the same struct.
            let input = unsafe { signature_slice(inputs, s.NumInputSignatureEntries) };
            // SAFETY: as above.
            let output = unsafe { signature_slice(outputs, s.NumOutputSignatureEntries) };
            if !input.is_empty() {
                parts.push(SignaturePart { tag: b"ISG1", entries: input });
            }
            if !output.is_empty() {
                parts.push(SignaturePart { tag: b"OSG1", entries: output });
            }
        }
        IoArm::Tessellation => {
            // SAFETY: as the Standard arm — the stage selects the arm.
            let p = unsafe { io.Tessellation };
            if p.is_null() {
                return parts;
            }
            // SAFETY: non-null per the check, live per the caller.
            let s = unsafe { &*p };
            // SAFETY: every arm of these three unions is a pointer at offset 0.
            let (inputs, outputs, patch) = unsafe {
                (
                    s.__bindgen_anon_1.pInputSignature,
                    s.__bindgen_anon_2.pOutputSignature,
                    s.__bindgen_anon_3.pPatchConstantSignature,
                )
            };
            // SAFETY: the runtime owns all three arrays for this call and
            // states their lengths in the same struct.
            let input = unsafe { signature_slice(inputs, s.NumInputSignatureEntries) };
            // SAFETY: as above.
            let output = unsafe { signature_slice(outputs, s.NumOutputSignatureEntries) };
            // SAFETY: as above.
            let pc = unsafe { signature_slice(patch, s.NumPatchConstantSignatureEntries) };
            if !input.is_empty() {
                parts.push(SignaturePart { tag: b"ISG1", entries: input });
            }
            if !output.is_empty() {
                parts.push(SignaturePart { tag: b"OSG1", entries: output });
            }
            if !pc.is_empty() {
                parts.push(SignaturePart { tag: b"PSG1", entries: pc });
            }
        }
        IoArm::Mesh => {
            // SAFETY: as the Standard arm — the stage selects the arm.
            let p = unsafe { io.Mesh };
            if p.is_null() {
                return parts;
            }
            // SAFETY: non-null per the check, live per the caller.
            let s = unsafe { &*p };
            // ⚠ A mesh block carries **two output** signatures and no input
            // one: per-vertex and per-primitive. Only the vertex outputs are
            // encoded as `OSG1`, because a DXBC container has one output
            // signature part and the per-primitive set has no chunk to live in.
            // Counted rather than dropped silently.
            // SAFETY: the runtime owns the array for this call and states its
            // length in the same struct.
            let vertex = unsafe {
                signature_slice(s.pVertexOutputSignature, s.NumVertexOutputSignatureEntries)
            };
            if s.NumPrimitiveOutputSignatureEntries != 0 {
                note_refusal(&L6_REFUSALS.mesh_primitive_signature_dropped);
            }
            if !vertex.is_empty() {
                parts.push(SignaturePart { tag: b"OSG1", entries: vertex });
            }
        }
    }
    parts
}

// ---------------------------------------------------------------------------
// The create path, shared by all nine create slots
// ---------------------------------------------------------------------------

/// Null a shader handle's slot word.
///
/// ⛔ **Every create runs this first**, before anything can fail. It is the
/// discipline `DDI_REFERENCE.md` §12.1 names — *"failure is reported by leaving
/// the handle cleared"* — and the shape
/// `umd/src/forward/shaders.rs:68-100` already implements for D3D11: clear
/// first, never fabricate a handle.
///
/// # Safety
/// `h_shader.pDrvPrivate`, when non-null, must address the machine word the
/// paired `pfnCalcPrivateShaderSize` sized.
unsafe fn clear_shader_handle(h_shader: ddi12::D3D12DDI_HSHADER) {
    // SAFETY: the caller guarantees the slot is the runtime-allocated word.
    if let Some(slot) = unsafe { shader_slot(h_shader) } {
        // SAFETY: as above; clearing touches only the word.
        unsafe { slot.clear() };
    }
}

/// The slot behind a shader handle, typed with [`ShaderState`] by the handle's
/// own `BoxedHandle` impl rather than by this call site (R803).
///
/// # Safety
/// Same precondition as `Slot::from_priv`.
unsafe fn shader_slot(
    h: ddi12::D3D12DDI_HSHADER,
) -> Option<Slot<Boxed<<ddi12::D3D12DDI_HSHADER as BoxedHandle>::State>>> {
    // SAFETY: forwarded unchanged; the caller carries the precondition.
    unsafe { Slot::from_priv(h.drv_private()) }
}

/// Borrow the state behind a shader handle.
///
/// ⛔ **The D3D12 soundness argument is re-derived here and NOT inherited from
/// D3D11.** `umd_common::slot`'s `Slot::get` states plainly that the
/// `CUseCountedObject` first-created/last-destroyed ordering is established for
/// D3D11 and *"NOT established"* for D3D12, and `PARALLEL.md` §9.4 forbids
/// inheriting the claim. This function therefore does **not** call `get` — it
/// takes the narrower route `slot.rs` offers for exactly this case, `ptr()`,
/// and carries the lifetime itself:
///
/// * the returned reference is **not** `'static`. It borrows for as long as the
///   caller's binding lives, and every caller is one DDI invocation;
/// * the only path that frees the box is [`destroy_shader`], and the only
///   readers are [`crate::forward12::pso::create_pipeline_state`] and the log.
///   A runtime that could run `pfnDestroyShader` concurrently with a
///   `pfnCreatePipelineState` naming the same `hShader` would be destroying an
///   object while passing it as an argument — which is not a race this driver
///   can defend against and not one any D3D12 runtime can implement, since the
///   *runtime* owns both handles and the ordering between them;
/// * ⚠ concurrent **reads** across free-threaded workers are permitted by `&`
///   and are the expected case. [`ShaderState`] is immutable after
///   construction, so there is deliberately no `&mut` path and no interior
///   mutability to protect.
///
/// # Safety
/// `h` must be a live handle a create slot in this file stored into and
/// [`destroy_shader`] has not been called on, and the returned reference must
/// not outlive the DDI call that obtained it.
pub(crate) unsafe fn shader<'a>(h: ddi12::D3D12DDI_HSHADER) -> Option<&'a ShaderState> {
    // SAFETY: forwarded; the caller carries the precondition.
    let p = unsafe { shader_slot(h)?.ptr() };
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null per the check, and the argument above establishes that
    // no teardown can overlap this borrow.
    Some(unsafe { &*p })
}

/// The body every `pfnCreate*Shader` shares: validate, copy, wrap, store.
///
/// # Safety
/// `arg` must point at a live `D3D12DDIARG_CREATE_SHADER_0026` whose
/// `pShaderCode` and IO signature block are live for the call, and `h_shader`
/// must carry the machine word `pfnCalcPrivateShaderSize` sized.
unsafe fn create_shader_common(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_SHADER_0026,
    h_shader: ddi12::D3D12DDI_HSHADER,
    stage: ShaderStage,
) {
    // SAFETY: the caller guarantees the slot; clearing touches only the word.
    unsafe { clear_shader_handle(h_shader) };
    L6_REFUSALS.shader_creates.bump();

    if arg.is_null() {
        note_refusal(&L6_REFUSALS.shader_bad_arg);
        // SAFETY: `h_device` is the device handle the runtime passed to this
        // DDI; `set_error_if_possible` re-validates it.
        unsafe { set_error_if_possible(h_device, helios_umd_common::hr::E_INVALIDARG) };
        return;
    }
    // SAFETY: non-null per the check; the DDI declares it `_In_ CONST`.
    let a = unsafe { &*arg };

    // SAFETY: `pShaderCode` is the runtime's `_In_` bytecode pointer;
    // `shader_code_len` reads at most seven dwords and only after checking the
    // first, which is the bound `DDI_REFERENCE.md` §12.2 requires.
    let len = unsafe { shader_code_len(a.pShaderCode) };
    // SAFETY: as above — four dwords, and the function null-checks first.
    unsafe { log_shader_code(stage, a.pShaderCode, len) };
    if len == 0 {
        note_refusal(&L6_REFUSALS.shader_length_unknown);
        // SAFETY: as the null-arg arm above.
        unsafe { set_error_if_possible(h_device, helios_umd_common::hr::E_INVALIDARG) };
        return;
    }

    // SAFETY: `len` came from the blob's own self-description and was bounded
    // by `shader_code_len` to at most 4 MiB; the runtime declares `pShaderCode`
    // readable for the length the blob states.
    let code = unsafe { core::slice::from_raw_parts(a.pShaderCode.cast::<u8>(), len) };

    // ⭐ The one cross-check available on data this driver otherwise forwards
    // blind: dword 0's high 16 bits are the DXIL program kind, and
    // `DDI_REFERENCE.md` §12.2 measured it matching the slot in every sample.
    let program_kind = u32::from_le_bytes([code[0], code[1], code[2], code[3]]) >> 16;
    if program_kind != stage.dxil_program_kind() {
        L6_REFUSALS.shader_program_kind_mismatch.bump();
        let n = L6_REFUSALS.shader_program_kind_mismatch.get();
        if n <= LOG_BUDGET {
            log_error!(
                "Create{}Shader: bytecode declares DXIL program kind {program_kind}, expected {} \
                 (x{n})",
                stage.name(),
                stage.dxil_program_kind(),
            );
        }
    }

    // SAFETY: the caller guarantees the IO signature block is live, and the arm
    // is selected by `stage` rather than by any runtime-supplied tag.
    let parts = unsafe { signature_parts(stage, &a.IOSignatures) };
    let container = build_dxbc_container(&parts, code);
    trace_line!(
        "Create{}Shader: container={} bytes, {} signature part(s), code={len}",
        stage.name(),
        container.len(),
        parts.len(),
    );

    let Some(slot) = (
        // SAFETY: the caller guarantees `h_shader` carries the runtime-allocated
        // machine word.
        unsafe { shader_slot(h_shader) }
    ) else {
        note_refusal(&L6_REFUSALS.shader_bad_arg);
        // SAFETY: as the null-arg arm above.
        unsafe { set_error_if_possible(h_device, helios_umd_common::hr::E_INVALIDARG) };
        return;
    };
    // SAFETY: the slot is the word `pfnCalcPrivateShaderSize` sized, and it was
    // cleared at the top of this function, so nothing is being overwritten.
    unsafe {
        slot.store(ShaderState {
            stage,
            container,
            h_root_signature: a.hRootSignature.pDrvPrivate,
        })
    };
}

// ---------------------------------------------------------------------------
// The 14 slots
// ---------------------------------------------------------------------------

/// `pfnCalcPrivateShaderSize`, and — the same typedef, a second slot —
/// `pfnCalcPrivateTessellationShaderSize`.
///
/// ⭐ One machine word, exactly as the D3D11 driver answers for every one of its
/// shader slots (`umd/src/forward/shaders.rs:62-70`, `-> 8`). The word holds a
/// `Box<ShaderState>`; the bytecode and the synthesised container are this
/// driver's allocation and not the runtime's, because their size is not known
/// until `pfnCreate*Shader` reads the blob — and `CalcPrivate*Size` is called
/// before it.
///
/// # Safety
/// Trivially safe: neither argument is dereferenced. `unsafe` because the DDI
/// typedef is.
unsafe extern "C" fn calc_private_shader_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _arg: *const ddi12::D3D12DDIARG_CREATE_SHADER_0026,
) -> ddi12::SIZE_T {
    core::mem::size_of::<*mut c_void>() as ddi12::SIZE_T
}

/// `pfnCalcPrivateMeshShaderSize` — the same answer, a distinct slot.
///
/// # Safety
/// As [`calc_private_shader_size`].
unsafe extern "C" fn calc_private_mesh_shader_size(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _arg: *const ddi12::D3D12DDIARG_CREATE_SHADER_0026,
) -> ddi12::SIZE_T {
    core::mem::size_of::<*mut c_void>() as ddi12::SIZE_T
}

/// `pfnCalcPrivateGeometryShaderWithStreamOutput`.
///
/// # Safety
/// As [`calc_private_shader_size`].
unsafe extern "C" fn calc_private_geometry_shader_with_stream_output(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    _arg: *const ddi12::D3D12DDIARG_CREATE_GEOMETRY_SHADER_WITH_STREAM_OUTPUT_0026,
) -> ddi12::SIZE_T {
    core::mem::size_of::<*mut c_void>() as ddi12::SIZE_T
}

/// Emit one `pfnCreate*Shader` slot for `$stage`.
///
/// ⛔ The stage is baked into each generated function rather than passed: six of
/// these share one DDI typedef and the union arm inside the argument struct is
/// selected by which slot the runtime called (`DDI_REFERENCE.md` §12.1). A
/// single handler taking a stage parameter could not be installed in six slots.
macro_rules! create_shader_slot {
    ($name:ident, $stage:expr, $doc:literal) => {
        #[doc = $doc]
        ///
        /// # Safety
        /// `arg` must point at a live `D3D12DDIARG_CREATE_SHADER_0026` whose
        /// `pShaderCode` and IO signature block are live for the call, and
        /// `h_shader` must carry the machine word
        /// `pfnCalcPrivateShaderSize` sized.
        unsafe extern "C" fn $name(
            h_device: ddi12::D3D12DDI_HDEVICE,
            arg: *const ddi12::D3D12DDIARG_CREATE_SHADER_0026,
            h_shader: ddi12::D3D12DDI_HSHADER,
        ) {
            // SAFETY: forwarded unchanged; the caller's guarantees above are
            // exactly `create_shader_common`'s preconditions.
            unsafe { create_shader_common(h_device, arg, h_shader, $stage) }
        }
    };
}

create_shader_slot!(
    create_vertex_shader,
    ShaderStage::Vertex,
    "`pfnCreateVertexShader`."
);
create_shader_slot!(
    create_pixel_shader,
    ShaderStage::Pixel,
    "`pfnCreatePixelShader`."
);
create_shader_slot!(
    create_geometry_shader,
    ShaderStage::Geometry,
    "`pfnCreateGeometryShader`."
);
create_shader_slot!(
    create_compute_shader,
    ShaderStage::Compute,
    "`pfnCreateComputeShader`."
);
create_shader_slot!(
    create_hull_shader,
    ShaderStage::Hull,
    "`pfnCreateHullShader`. Reads the `Tessellation` arm of `IOSignatures`."
);
create_shader_slot!(
    create_domain_shader,
    ShaderStage::Domain,
    "`pfnCreateDomainShader`. Reads the `Tessellation` arm of `IOSignatures`."
);
create_shader_slot!(
    create_amplification_shader,
    ShaderStage::Amplification,
    "`pfnCreateAmplificationShader`. Reads the `Standard` arm."
);
create_shader_slot!(
    create_mesh_shader,
    ShaderStage::Mesh,
    "`pfnCreateMeshShader`. Reads the `Mesh` arm."
);

/// `pfnCreateGeometryShaderWithStreamOutput`.
///
/// ⚠ **The geometry shader is created; the stream-output declaration is
/// DROPPED and counted.** That is the same answer the shipping D3D11 driver
/// gives (`umd/src/forward/shaders.rs:676-684`,
/// `DdiRefusals::gs_so_declaration_dropped`) and for the same reason, restated
/// for D3D12:
///
/// `D3D12_STREAM_OUTPUT_DESC` entries are matched against the GS output
/// signature by `SemanticName`/`SemanticIndex`, and this file **fabricates**
/// those names. Fabricating them on the SO side too would be self-consistent
/// for real entries — but a D3D12 SO declaration also encodes **gap** entries
/// (`SemanticName == NULL`), and `D3D12DDIARG_STREAM_OUTPUT_DECLARATION_ENTRY`
/// has no field this driver has evidence for as the gap marker. Guessing it
/// mis-lays-out the whole SO buffer with no error anywhere.
///
/// The consequence when an app depends on SO, spelled out because a counter
/// without one is not readable: `SOSetTargets` binds buffers that are never
/// written and `DrawAuto` reads zero vertices, so the app renders nothing.
/// `GsStreamOutputDropped` is what makes that a number instead of a silence.
///
/// # Safety
/// `arg` must point at a live
/// `D3D12DDIARG_CREATE_GEOMETRY_SHADER_WITH_STREAM_OUTPUT_0026`, and `h_shader`
/// must carry the machine word the paired calc-size slot sized.
unsafe extern "C" fn create_geometry_shader_with_stream_output(
    h_device: ddi12::D3D12DDI_HDEVICE,
    arg: *const ddi12::D3D12DDIARG_CREATE_GEOMETRY_SHADER_WITH_STREAM_OUTPUT_0026,
    h_shader: ddi12::D3D12DDI_HSHADER,
) {
    if arg.is_null() {
        // SAFETY: the caller guarantees the slot word.
        unsafe { clear_shader_handle(h_shader) };
        note_refusal(&L6_REFUSALS.shader_bad_arg);
        // SAFETY: `h_device` is this DDI's device handle.
        unsafe { set_error_if_possible(h_device, helios_umd_common::hr::E_INVALIDARG) };
        return;
    }
    note_refusal(&L6_REFUSALS.gs_stream_output_dropped);
    // SAFETY: non-null per the check; `CreateShader` is its first member and is
    // the same `_In_ CONST` block every other create slot receives.
    let inner = unsafe { core::ptr::addr_of!((*arg).CreateShader) };
    // SAFETY: forwarded unchanged — `inner` points inside the runtime's live
    // argument struct, so it satisfies `create_shader_common`'s precondition.
    unsafe { create_shader_common(h_device, inner, h_shader, ShaderStage::Geometry) }
}

/// `pfnDestroyShader`.
///
/// # Safety
/// `h_shader` must be a handle a create slot in this file stored into, and must
/// not be destroyed twice.
unsafe extern "C" fn destroy_shader(
    _h_device: ddi12::D3D12DDI_HDEVICE,
    h_shader: ddi12::D3D12DDI_HSHADER,
) {
    // SAFETY: the caller guarantees a live slot; `take` empties it, so a second
    // destroy finds `None` rather than double-freeing.
    let Some(slot) = (unsafe { shader_slot(h_shader) }) else {
        note_refusal(&L6_REFUSALS.shader_bad_arg);
        return;
    };
    // SAFETY: as above. The `Box<ShaderState>` returned owns the container
    // allocation and nothing else; dropping it frees exactly what
    // `create_shader_common` allocated.
    drop(unsafe { slot.take() });
}

/// Install L6's 14 shader device-core slots.
///
/// Chain position: `PsoSlots` -> `ShaderSlots` on the device-core table.
///
/// ⚠ Reached through [`crate::forward12::pso::install_shaders`], because
/// `tables12` is a shared file and the chain link it already names lives in
/// `pso.rs` (`PARALLEL.md` §5: a lane's diff against the sequencer is empty).
pub(crate) fn install(
    mut filling: Filling<'_, DeviceCoreTable, stage::PsoSlots>,
) -> Filling<'_, DeviceCoreTable, stage::ShaderSlots> {
    let table = filling.table();
    table.pfnCalcPrivateShaderSize = Some(calc_private_shader_size);
    table.pfnCreateVertexShader = Some(create_vertex_shader);
    table.pfnCreatePixelShader = Some(create_pixel_shader);
    table.pfnCreateGeometryShader = Some(create_geometry_shader);
    table.pfnCreateComputeShader = Some(create_compute_shader);
    table.pfnCalcPrivateGeometryShaderWithStreamOutput =
        Some(calc_private_geometry_shader_with_stream_output);
    table.pfnCreateGeometryShaderWithStreamOutput = Some(create_geometry_shader_with_stream_output);
    // ⚠ The SAME typedef as `pfnCalcPrivateShaderSize`
    // (`PFND3D12DDI_CALC_PRIVATE_SHADER_SIZE_0026`) and a DISTINCT slot
    // (`DDI_REFERENCE.md` §3.2 (c)). Filling it with the same function is
    // correct — the answer does not depend on the stage — and leaving it NULL
    // would not be.
    table.pfnCalcPrivateTessellationShaderSize = Some(calc_private_shader_size);
    table.pfnCreateHullShader = Some(create_hull_shader);
    table.pfnCreateDomainShader = Some(create_domain_shader);
    table.pfnDestroyShader = Some(destroy_shader);
    table.pfnCreateAmplificationShader = Some(create_amplification_shader);
    table.pfnCreateMeshShader = Some(create_mesh_shader);
    table.pfnCalcPrivateMeshShaderSize = Some(calc_private_mesh_shader_size);
    filling.advance()
}

// ⛔ **No `REFUSALS` array here, and that is deliberate.** L6 is one lane across
// two files and `lib.rs` names exactly one set per lane
// (`forward12::pso::REFUSALS`), so every counter this file bumps —
// `L6ShaderCreates`, `L6ShaderBadArg`, `L6ShaderLengthUnknown`,
// `L6ShaderDxbcContainerSeen`, `L6ShaderProgramKindMismatch`,
// `L6ShaderSignatureCountRefused`, `L6MeshPrimitiveSignatureDropped`,
// `L6GsStreamOutputDropped`, plus `L6SetErrorNoDevice` / `L6SetErrorCbAbsent`
// through `pso::set_error_if_possible` — is declared beside the rest of L6's in
// `pso.rs`, inside the one array `lib.rs` prints. A second array in this file
// would be counters in no summary at all, which is T5's *"an instrument nothing
// can read is not an instrument"* in its most literal form.
//
// ⚠ Two more counters are keyed on state THIS file stores and are bumped in
// `pso.rs` because that is where the comparison happens:
// `L6ShaderStageMismatch` and `L6ShaderRootSignatureMismatch`, both read out of
// [`ShaderState`] by `pso::bytecode_of`.
