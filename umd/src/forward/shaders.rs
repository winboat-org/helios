//! Shader creation, and the DXBC signature flattening the >=11.1 DDI needs.
//!
//! The `SigEntry`/`SigHeader`/`SigBlock`/`SigWords` parsers, the three
//! `flatten_*_signatures` walkers, every `create_*_shader[_11_1]`, and the two
//! `stage_set_shader*` macros with their six invocations each.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

// --- Shaders ----------------------------------------------------------------

pub(crate) unsafe fn shader_code_len(code: *const u32) -> usize {
    if code.is_null() {
        return 0;
    }

    // D3D API bytecode is a DXBC container with the total size at byte offset 24.
    // Bound it the same way the SHDR arm below is bounded: the dword at offset
    // 24 is read BEFORE anything is known about the container's real size, and
    // ten call sites build a `from_raw_parts` slice out of the result. Require
    // at least the 32-byte container header and at most 1 << 20 dwords.
    if *code == u32::from_le_bytes(*b"DXBC") {
        let total = *code.add(6) as usize;
        if total < 32 || total > (1 << 20) * core::mem::size_of::<u32>() {
            log_error!("DDI shader_code_len: rejecting DXBC total size {total}");
            return 0;
        }
        return total;
    }

    // D3D UMD callbacks receive raw SHDR/SHEX token streams. The second DWORD
    // is the stream length in DWORDs, including the two-token shader header.
    let dwords = *code.add(1) as usize;
    if dwords < 2 || dwords > (1 << 20) {
        return 0;
    }

    dwords * core::mem::size_of::<u32>()
}

pub(crate) unsafe fn log_shader_code(kind: &str, code: *const u32, len: usize) {
    if code.is_null() {
        log_error!("DDI {kind}: null shader code");
        return;
    }
    let d0 = *code.add(0);
    let d1 = *code.add(1);
    let d2 = *code.add(2);
    let d3 = *code.add(3);
    let is_dxbc = d0 == u32::from_le_bytes(*b"DXBC");
    log_error!(
        "DDI {kind}: shader len={} dxbc={} tokens={:08x} {:08x} {:08x} {:08x}",
        len,
        is_dxbc,
        d0,
        d1,
        d2,
        d3
    );
}

pub(crate) unsafe extern "C" fn calc_size_shader(
    _h: Hdevice,
    _code: *const u32,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) -> u64 {
    8
}

pub(crate) unsafe extern "C" fn create_vertex_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_vertex_shader", code, len);
    if len == 0 {
        log_error!("DDI create_vertex_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let raw = dxvk.create_vertex_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        if SHADER_BIND_LOG_COUNT.first_n(128).is_some() {
            log_error!("DDI create_vertex_shader ok: raw=0x{raw:x} len={len}");
        }
        store_raw_com(h_shader, raw);
        // Keep the bytecode so input layouts can be created lazily (the ISGN
        // supplies the semantic names CreateInputLayout requires).
        dev.owned
            .ia
            .borrow_mut()
            .vs_bytecode
            .insert(raw, bytes.to_vec());
    } else {
        log_error!("DDI create_vertex_shader failed");
    }
}

pub(crate) unsafe extern "C" fn create_pixel_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_pixel_shader", code, len);
    if len == 0 {
        log_error!("DDI create_pixel_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let raw = dxvk.create_pixel_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        if SHADER_BIND_LOG_COUNT.first_n(128).is_some() {
            log_error!("DDI create_pixel_shader ok: raw=0x{raw:x} len={len}");
        }
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_pixel_shader failed");
    }
}

/// Words per signature entry in the bridge wire layout.
///
/// The C++ decoder names the same stride once, as `kSigEntryWords`
/// (`dxvk_bridge.cpp`), and length-validates every incoming block against it.
/// This is the Rust-side half of that constant: it was previously a bare `5`
/// spelled out in eight producer loops, three log dumps and one patch loop.
pub(crate) const SIG_ENTRY_WORDS: usize = 5;

/// One entry of a flattened shader signature, in the bridge's wire order.
///
/// The tessellation D3D11 (non-11.1) producer has no component type or stream
/// — the older D3D10 entry shape does not carry them — and passes zeros so the
/// bridge's DXBC writer takes its UNKNOWN-component fallback. That is the only
/// legitimate zero pair; going through this constructor is what stops it being
/// confused with a five-element literal that dropped a field.
#[derive(Clone, Copy)]
pub(crate) struct SigEntry {
    pub(crate) sysval: u32,
    pub(crate) register_: u32,
    pub(crate) mask: u32,
    pub(crate) comptype: u32,
    pub(crate) stream: u32,
}

impl SigEntry {
    pub(crate) fn as_words(self) -> [u32; SIG_ENTRY_WORDS] {
        [
            self.sysval,
            self.register_,
            self.mask,
            self.comptype,
            self.stream,
        ]
    }
}

/// Which header a flattened signature block carries.
///
/// The two shapes are NOT interchangeable and the arity is the whole hazard:
/// the stage block is `[n_in, n_out, entries…]` (validated in the bridge at
/// `sig_words_len == 2 + (n_in + n_out) * kSigEntryWords`), the tessellation
/// block is `[n_in, n_out, n_patch, entries…]` (`3 + …`). Reading a tess block
/// with the 2-word accessors silently returns `n_patch` as the first entry's
/// system value. Making the header a constructor choice is what removes that.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SigHeader {
    Stage,
    Tess,
}

impl SigHeader {
    pub(crate) const fn words(self) -> usize {
        match self {
            SigHeader::Stage => 2,
            SigHeader::Tess => 3,
        }
    }
}

/// A borrowed flattened signature block: a header of counts followed by
/// fixed-width entries. The read side of the layout, shared by the log dumps
/// and the input-variant patcher.
#[derive(Clone, Copy)]
pub(crate) struct SigBlock<'a> {
    pub(crate) words: &'a [u32],
    pub(crate) header: SigHeader,
}

impl<'a> SigBlock<'a> {
    /// A `[n_in, n_out, …]` block. `None` if the header itself is missing.
    pub(crate) fn stage(words: &'a [u32]) -> Option<Self> {
        Self::new(words, SigHeader::Stage)
    }

    /// A `[n_in, n_out, n_patch, …]` block. `None` if the header is missing.
    pub(crate) fn tess(words: &'a [u32]) -> Option<Self> {
        Self::new(words, SigHeader::Tess)
    }

    pub(crate) fn new(words: &'a [u32], header: SigHeader) -> Option<Self> {
        (words.len() >= header.words()).then_some(Self { words, header })
    }

    pub(crate) fn n_in(self) -> usize {
        self.words[0] as usize
    }

    pub(crate) fn n_out(self) -> usize {
        self.words[1] as usize
    }

    /// `None` on a stage block, which has no patch-constant group.
    pub(crate) fn n_patch(self) -> Option<usize> {
        match self.header {
            SigHeader::Stage => None,
            SigHeader::Tess => Some(self.words[2] as usize),
        }
    }

    /// Entry `i` of the group that starts after `preceding` entries, or `None`
    /// if the block is shorter than its header claims. Never a partial entry.
    pub(crate) fn entry(self, preceding: usize, i: usize) -> Option<SigEntry> {
        let base = entry_base(self.header, preceding, i);
        let w = self.words.get(base..base + SIG_ENTRY_WORDS)?;
        Some(SigEntry {
            sysval: w[0],
            register_: w[1],
            mask: w[2],
            comptype: w[3],
            stream: w[4],
        })
    }

    pub(crate) fn input(self, i: usize) -> Option<SigEntry> {
        self.entry(0, i)
    }
}

/// Word index of entry `i` of the group that starts after `preceding` entries.
///
/// The `2 + i * 5` / `3 + (n_in + i) * 5` arithmetic, once.
pub(crate) fn entry_base(header: SigHeader, preceding: usize, i: usize) -> usize {
    header.words() + (preceding + i) * SIG_ENTRY_WORDS
}

/// An owned flattened signature block. Holds the exact `Vec<u32>` handed across
/// the cxx bridge.
pub(crate) struct SigWords {
    pub(crate) words: Vec<u32>,
    pub(crate) header: SigHeader,
}

impl SigWords {
    /// `[n_in, n_out]`, the >=11.1 stage-shader header.
    pub(crate) fn stage(n_in: u32, n_out: u32) -> Self {
        Self {
            words: vec![n_in, n_out],
            header: SigHeader::Stage,
        }
    }

    /// `[n_in, n_out, n_patch]`, the tessellation header.
    pub(crate) fn tess(n_in: u32, n_out: u32, n_patch: u32) -> Self {
        Self {
            words: vec![n_in, n_out, n_patch],
            header: SigHeader::Tess,
        }
    }

    /// Re-adopt a stage block previously produced by [`SigWords::stage`] and
    /// stored as raw words (`ia.vs_sig_words`).
    pub(crate) fn adopt_stage(words: Vec<u32>) -> Option<Self> {
        (words.len() >= SigHeader::Stage.words()).then_some(Self {
            words,
            header: SigHeader::Stage,
        })
    }

    pub(crate) fn push(&mut self, entry: SigEntry) {
        self.words.extend_from_slice(&entry.as_words());
    }

    pub(crate) fn block(&self) -> SigBlock<'_> {
        SigBlock {
            words: &self.words,
            header: self.header,
        }
    }

    pub(crate) fn n_in(&self) -> usize {
        self.block().n_in()
    }

    /// Overwrite input entry `i`'s component type. Returns false if the block
    /// is too short, which the caller treats as "leave it alone".
    pub(crate) fn set_input_comptype(&mut self, i: usize, comptype: u32) -> bool {
        let base = entry_base(self.header, 0, i);
        match self.words.get_mut(base + 3) {
            Some(slot) => {
                *slot = comptype;
                true
            }
            None => false,
        }
    }

    /// Replace the whole input group, keeping the groups that follow it. Used
    /// when a shader arrived through the legacy untyped create and its inputs
    /// must be synthesized wholesale from the bound layout.
    pub(crate) fn replace_inputs(&mut self, entries: &[SigEntry]) {
        let header = self.header.words();
        let old_end = (header + self.n_in() * SIG_ENTRY_WORDS).min(self.words.len());
        let tail = self.words.split_off(old_end);
        self.words.truncate(header);
        self.words[0] = entries.len() as u32;
        for e in entries {
            self.words.extend_from_slice(&e.as_words());
        }
        self.words.extend_from_slice(&tail);
    }

    pub(crate) fn into_words(self) -> Vec<u32> {
        self.words
    }
}

/// Flatten a >=11.1 typed signature block into the bridge wire layout:
/// [n_in, n_out, (sysval, register, mask, comptype, stream) x n_in, same x
/// n_out]. The ENTRY2 arm is the one the >=11.1 runtime fills.
pub(crate) unsafe fn flatten_stage_io_signatures(
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
) -> Vec<u32> {
    if sig.is_null() {
        return SigWords::stage(0, 0).into_words();
    }
    let s = &*sig;
    let p_in = s.__bindgen_anon_1.pInputSignature;
    let p_out = s.__bindgen_anon_2.pOutputSignature;
    let n_in = if p_in.is_null() {
        0
    } else {
        s.NumInputSignatureEntries
    };
    let n_out = if p_out.is_null() {
        0
    } else {
        s.NumOutputSignatureEntries
    };
    let mut words = SigWords::stage(n_in, n_out);
    for i in 0..n_in as usize {
        let e = &*p_in.add(i);
        words.push(SigEntry {
            sysval: e.SystemValue as u32,
            register_: e.Register,
            mask: e.Mask as u32,
            comptype: e.RegisterComponentType as u32,
            stream: e.Stream as u32,
        });
    }
    for i in 0..n_out as usize {
        let e = &*p_out.add(i);
        words.push(SigEntry {
            sysval: e.SystemValue as u32,
            register_: e.Register,
            mask: e.Mask as u32,
            comptype: e.RegisterComponentType as u32,
            stream: e.Stream as u32,
        });
    }
    words.into_words()
}

/// Flatten a D3D11 tessellation signature block into the bridge wire layout:
/// [n_in, n_out, n_patch, entries...]. The D3D11 tessellation DDI uses the
/// older D3D10 signature entry shape, so component type and stream are not
/// available; pass zeros and let the bridge's DXBC signature writer use its
/// existing UNKNOWN-component fallback.
pub(crate) unsafe fn flatten_tess_io_signatures(
    sig: *const ddi::D3D11DDIARG_TESSELLATION_IO_SIGNATURES,
) -> Vec<u32> {
    if sig.is_null() {
        return SigWords::tess(0, 0, 0).into_words();
    }
    let s = &*sig;
    let p_in = s.pInputSignature;
    let p_out = s.pOutputSignature;
    let p_patch = s.pPatchConstantSignature;
    let n_in = if p_in.is_null() {
        0
    } else {
        s.NumInputSignatureEntries
    };
    let n_out = if p_out.is_null() {
        0
    } else {
        s.NumOutputSignatureEntries
    };
    let n_patch = if p_patch.is_null() {
        0
    } else {
        s.NumPatchConstantSignatureEntries
    };
    // The D3D10 entry shape carries no component type and no stream: zeros here
    // are the documented UNKNOWN-component fallback, not dropped fields.
    let d3d10_entry = |e: &ddi::D3D10DDIARG_SIGNATURE_ENTRY| SigEntry {
        sysval: e.SystemValue as u32,
        register_: e.Register,
        mask: e.Mask as u32,
        comptype: 0,
        stream: 0,
    };
    let mut words = SigWords::tess(n_in, n_out, n_patch);
    for i in 0..n_in as usize {
        words.push(d3d10_entry(&*p_in.add(i)));
    }
    for i in 0..n_out as usize {
        words.push(d3d10_entry(&*p_out.add(i)));
    }
    for i in 0..n_patch as usize {
        words.push(d3d10_entry(&*p_patch.add(i)));
    }
    words.into_words()
}

/// Flatten a >=11.1 tessellation signature block into the bridge wire layout:
/// [n_in, n_out, n_patch, entries...]. The 11.1 tessellation callbacks use
/// ENTRY2, so register component type and stream are available just like the
/// non-tessellation 11.1 shader creates.
pub(crate) unsafe fn flatten_tess_io_signatures_11_1(
    sig: *const ddi::D3D11_1DDIARG_TESSELLATION_IO_SIGNATURES,
) -> Vec<u32> {
    if sig.is_null() {
        return SigWords::tess(0, 0, 0).into_words();
    }
    let s = &*sig;
    let p_in = s.__bindgen_anon_1.pInputSignature;
    let p_out = s.__bindgen_anon_2.pOutputSignature;
    let p_patch = s.__bindgen_anon_3.pPatchConstantSignature;
    let n_in = if p_in.is_null() {
        0
    } else {
        s.NumInputSignatureEntries
    };
    let n_out = if p_out.is_null() {
        0
    } else {
        s.NumOutputSignatureEntries
    };
    let n_patch = if p_patch.is_null() {
        0
    } else {
        s.NumPatchConstantSignatureEntries
    };
    let entry_11_1 = |e: &ddi::D3D11_1DDIARG_SIGNATURE_ENTRY2| SigEntry {
        sysval: e.SystemValue as u32,
        register_: e.Register,
        mask: e.Mask as u32,
        comptype: e.RegisterComponentType as u32,
        stream: e.Stream as u32,
    };
    let mut words = SigWords::tess(n_in, n_out, n_patch);
    for i in 0..n_in as usize {
        words.push(entry_11_1(&*p_in.add(i)));
    }
    for i in 0..n_out as usize {
        words.push(entry_11_1(&*p_out.add(i)));
    }
    for i in 0..n_patch as usize {
        words.push(entry_11_1(&*p_patch.add(i)));
    }
    words.into_words()
}

pub(crate) unsafe fn log_tess_sig_summary(name: &str, sig_words: &[u32]) {
    let Some(block) = SigBlock::tess(sig_words) else {
        return;
    };
    let n_in = block.n_in();
    let n_out = block.n_out();
    let n_patch = block.n_patch().unwrap_or(0);
    let mut dump = format!("DDI {name} tess sig counts: in={n_in} out={n_out} patch={n_patch}");
    for (tag, count, preceding) in [
        ("i", n_in, 0usize),
        ("o", n_out, n_in),
        ("p", n_patch, n_in + n_out),
    ] {
        for i in 0..count.min(4) {
            let Some(e) = block.entry(preceding, i) else {
                break;
            };
            dump.push_str(&format!(
                " {tag}[r{} m0x{:x} sv{}]",
                e.register_, e.mask, e.sysval
            ));
        }
    }
    log_error!("{dump}");
}

/// Shared body for the >=11.1 typed shader creates. `kind`: 0 = vertex,
/// 1 = pixel, 2 = geometry (bridge convention). The typed signatures carry
/// the component types the raw token stream cannot express — without them
/// dxbc-spv declared every input float32 while dwm binds R16G16_SINT vertex
/// data (VUID-Input-08733 UB: garbage positions, nothing rasterized).
pub(crate) unsafe fn create_shader_11_1_common(
    h: Hdevice,
    kind: u32,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
    name: &str,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code(name, code, len);
    if len == 0 {
        log_error!("DDI {name} failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let sig_words = flatten_stage_io_signatures(sig);
    {
        // Evidence line for the Input-08733 investigation: dump each input
        // entry's (register, mask, component type) — comptype 0 (UNKNOWN)
        // falls back to float32 in the bridge, which is UB against SINT
        // vertex formats.
        let block = SigBlock::stage(&sig_words);
        let n_in = block.map_or(0, |b| b.n_in());
        let mut dump = format!("DDI {name} sig entries:");
        for i in 0..n_in.min(8) {
            let Some(e) = block.and_then(|b| b.input(i)) else {
                break;
            };
            dump.push_str(&format!(
                " [r{} m0x{:x} t{}]",
                e.register_, e.mask, e.comptype
            ));
        }
        log_error!("{dump}");
    }
    let raw = dxvk.create_shader_sig(
        kind,
        bytes.as_ptr(),
        bytes.len(),
        sig_words.as_ptr(),
        sig_words.len(),
    );
    if raw != 0 {
        if SHADER_BIND_LOG_COUNT.first_n(128).is_some() {
            log_error!(
                "DDI {name} ok: raw=0x{raw:x} len={len} sig_in={} sig_out={}",
                sig_words[0],
                sig_words[1]
            );
        }
        store_raw_com(h_shader, raw);
        if kind == 0 {
            // Keep the bytecode so input layouts can be created lazily, and
            // the signature words so input-class variants can be recompiled
            // against the bound layout (resolve_vs_input_variant).
            let mut ia = dev.owned.ia.borrow_mut();
            ia.vs_bytecode.insert(raw, bytes.to_vec());
            ia.vs_sig_words.insert(raw, sig_words);
        }
    } else {
        log_error!("DDI {name} failed");
    }
}

pub(crate) unsafe extern "C" fn create_vertex_shader_11_1(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
) {
    create_shader_11_1_common(h, 0, code, h_shader, sig, "create_vertex_shader_11_1");
}

pub(crate) unsafe extern "C" fn create_pixel_shader_11_1(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
) {
    create_shader_11_1_common(h, 1, code, h_shader, sig, "create_pixel_shader_11_1");
}

pub(crate) unsafe extern "C" fn create_geometry_shader_11_1(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11_1DDIARG_STAGE_IO_SIGNATURES,
) {
    create_shader_11_1_common(h, 2, code, h_shader, sig, "create_geometry_shader_11_1");
}

pub(crate) unsafe extern "C" fn create_geometry_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_geometry_shader", code, len);
    if len == 0 {
        log_error!("DDI create_geometry_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let raw = dxvk.create_geometry_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_geometry_shader failed");
    }
}

pub(crate) unsafe extern "C" fn calc_size_geometry_shader_so(
    _h: Hdevice,
    _arg: *const ddi::D3D11DDIARG_CREATEGEOMETRYSHADERWITHSTREAMOUTPUT,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) -> u64 {
    8
}

pub(crate) unsafe extern "C" fn create_geometry_shader_so(
    h: Hdevice,
    arg: *const ddi::D3D11DDIARG_CREATEGEOMETRYSHADERWITHSTREAMOUTPUT,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    _sig: *const ddi::D3D10DDIARG_STAGE_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    if arg.is_null() {
        return;
    }
    let Some(dev) = helios_device(h) else {
        return;
    };
    let a = &*arg;
    let len = shader_code_len(a.pShaderCode);
    log_shader_code("create_geometry_shader_so", a.pShaderCode, len);
    if len == 0 {
        log_error!("DDI create_geometry_shader_so failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(a.pShaderCode as *const u8, len);
    // Stream-output declarations need semantic names that are not present in the
    // compact DDI declaration. Create a plain GS for now; DWM's composition path
    // should not depend on SO capture.
    //
    // The consequence when an app DOES depend on it: SOSetTargets binds buffers
    // that are never written, DrawAuto reads zero vertices, and the app renders
    // nothing. Counted so that failure has a name. R911.
    note_ddi_refusal(&DDI_REFUSALS.gs_so_declaration_dropped);
    let dxvk = &dev.dxvk;
    let raw = dxvk.create_geometry_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_geometry_shader_so failed");
    }
}

pub(crate) unsafe extern "C" fn calc_size_tess_shader(
    _h: Hdevice,
    _code: *const u32,
    _sig: *const ddi::D3D11DDIARG_TESSELLATION_IO_SIGNATURES,
) -> u64 {
    8
}

pub(crate) unsafe extern "C" fn calc_size_tess_shader_11_1(
    _h: Hdevice,
    _code: *const u32,
    _sig: *const ddi::D3D11_1DDIARG_TESSELLATION_IO_SIGNATURES,
) -> u64 {
    8
}

pub(crate) unsafe extern "C" fn create_hull_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11DDIARG_TESSELLATION_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_hull_shader", code, len);
    if len == 0 {
        log_error!("DDI create_hull_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let sig_words = flatten_tess_io_signatures(sig);
    log_tess_sig_summary("create_hull_shader", &sig_words);
    let mut raw = dxvk.create_tess_shader_sig(
        0,
        bytes.as_ptr(),
        bytes.len(),
        sig_words.as_ptr(),
        sig_words.len(),
    );
    if raw == 0 {
        note_ddi_refusal(&DDI_REFUSALS.tess_sig_fallback);
        log_error!("DDI create_hull_shader signature path failed; falling back to raw bytecode");
        raw = dxvk.create_hull_shader(bytes.as_ptr(), bytes.len());
    }
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_hull_shader failed");
    }
}

pub(crate) unsafe extern "C" fn create_hull_shader_11_1(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11_1DDIARG_TESSELLATION_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_hull_shader_11_1", code, len);
    if len == 0 {
        log_error!("DDI create_hull_shader_11_1 failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let sig_words = flatten_tess_io_signatures_11_1(sig);
    log_tess_sig_summary("create_hull_shader_11_1", &sig_words);
    let mut raw = dxvk.create_tess_shader_sig(
        0,
        bytes.as_ptr(),
        bytes.len(),
        sig_words.as_ptr(),
        sig_words.len(),
    );
    if raw == 0 {
        note_ddi_refusal(&DDI_REFUSALS.tess_sig_fallback);
        log_error!(
            "DDI create_hull_shader_11_1 signature path failed; falling back to raw bytecode"
        );
        raw = dxvk.create_hull_shader(bytes.as_ptr(), bytes.len());
    }
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_hull_shader_11_1 failed");
    }
}

pub(crate) unsafe extern "C" fn create_domain_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11DDIARG_TESSELLATION_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_domain_shader", code, len);
    if len == 0 {
        log_error!("DDI create_domain_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let sig_words = flatten_tess_io_signatures(sig);
    log_tess_sig_summary("create_domain_shader", &sig_words);
    let mut raw = dxvk.create_tess_shader_sig(
        1,
        bytes.as_ptr(),
        bytes.len(),
        sig_words.as_ptr(),
        sig_words.len(),
    );
    if raw == 0 {
        note_ddi_refusal(&DDI_REFUSALS.tess_sig_fallback);
        log_error!("DDI create_domain_shader signature path failed; falling back to raw bytecode");
        raw = dxvk.create_domain_shader(bytes.as_ptr(), bytes.len());
    }
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_domain_shader failed");
    }
}

pub(crate) unsafe extern "C" fn create_domain_shader_11_1(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
    sig: *const ddi::D3D11_1DDIARG_TESSELLATION_IO_SIGNATURES,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_domain_shader_11_1", code, len);
    if len == 0 {
        log_error!("DDI create_domain_shader_11_1 failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let sig_words = flatten_tess_io_signatures_11_1(sig);
    log_tess_sig_summary("create_domain_shader_11_1", &sig_words);
    let mut raw = dxvk.create_tess_shader_sig(
        1,
        bytes.as_ptr(),
        bytes.len(),
        sig_words.as_ptr(),
        sig_words.len(),
    );
    if raw == 0 {
        note_ddi_refusal(&DDI_REFUSALS.tess_sig_fallback);
        log_error!(
            "DDI create_domain_shader_11_1 signature path failed; falling back to raw bytecode"
        );
        raw = dxvk.create_domain_shader(bytes.as_ptr(), bytes.len());
    }
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_domain_shader_11_1 failed");
    }
}

pub(crate) unsafe extern "C" fn create_compute_shader(
    h: Hdevice,
    code: *const u32,
    h_shader: ddi::D3D10DDI_HSHADER,
    _hrt: ddi::D3D10DDI_HRTSHADER,
) {
    clear_handle(h_shader);
    let Some(dev) = helios_device(h) else {
        return;
    };
    let len = shader_code_len(code);
    log_shader_code("create_compute_shader", code, len);
    if len == 0 {
        log_error!("DDI create_compute_shader failed: unknown shader length");
        return;
    }
    let bytes = core::slice::from_raw_parts(code as *const u8, len);
    let dxvk = &dev.dxvk;
    let raw = dxvk.create_compute_shader(bytes.as_ptr(), bytes.len());
    if raw != 0 {
        store_raw_com(h_shader, raw);
    } else {
        log_error!("DDI create_compute_shader failed");
    }
}

pub(crate) unsafe extern "C" fn destroy_shader(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
    let raw = handle_com_raw(h_shader);
    if raw != 0 {
        if let Some(dev) = helios_device(h) {
            let mut owned = std::collections::HashSet::new();
            let was_vertex_shader = {
                let mut ia = dev.owned.ia.borrow_mut();
                let had_bytecode = ia.vs_bytecode.remove(&raw).is_some();
                let had_signature = ia.vs_sig_words.remove(&raw).is_some();
                let was_vertex_shader = had_bytecode || had_signature;
                if was_vertex_shader {
                    ia.vs_variants.retain(|&(vs, _), variant| {
                        if vs == raw {
                            if *variant != 0 {
                                owned.insert(*variant);
                            }
                            false
                        } else {
                            true
                        }
                    });
                    ia.layout_cache.retain(|&(_, vs), layout| {
                        if vs == raw {
                            if *layout != 0 {
                                owned.insert(*layout);
                            }
                            false
                        } else {
                            true
                        }
                    });
                    if ia.current_vs == raw {
                        ia.current_vs = 0;
                    }
                    if ia.bound_vs_com == raw || owned.contains(&ia.bound_vs_com) {
                        ia.bound_vs_com = 0;
                    }
                }
                was_vertex_shader
            };
            for cached in &owned {
                // SAFETY: the cache owns the COM reference returned by its
                // Create* operation. Removing the entry transfers that one
                // reference here for release.
                drop(IUnknown::from_raw(*cached as *mut c_void));
            }
            if was_vertex_shader {
                trace_line!(
                    "DDI DestroyShader: VS raw=0x{:x} released_cached={}",
                    raw,
                    owned.len()
                );
            }
        }
    }
    release_com(h_shader);
}

/// Generate one stage's `pfn*SetShader` entry point.
///
/// The DDI table demands six distinct `extern "C"` symbols, so this is a macro
/// expansion and not a shared function — there is no type-level guarantee to
/// win here. What it wins is that the fourteen shared lines and the ONE
/// stage-specific line stop being indistinguishable to a reviewer: `vs` alone
/// also writes `ia.bound_vs_com` (read by the input-variant recompiler), and
/// in six copy-pasted bodies that asymmetry looked exactly like the five
/// others. `also_set:` is now the only visible difference.
macro_rules! stage_set_shader {
    ($name:ident, $tag:literal, $current:ident, $com:ty, $method:ident
     $(, also_set: $extra:ident)?) => {
        pub(crate) unsafe extern "C" fn $name(h: Hdevice, h_shader: ddi::D3D10DDI_HSHADER) {
            let com = handle_com_raw(h_shader);
            if let Some(dev) = helios_device(h) {
                let mut ia = dev.owned.ia.borrow_mut();
                ia.$current = com;
                $(ia.$extra = com;)?
            }
            if SHADER_SET_LOG_COUNT.first_n(512).is_some() {
                trace_line!(concat!("DDI ", $tag, "SetShader raw=0x{:x}"), com);
            }
            let Some(context) = d3d11_context(h) else {
                return;
            };
            match load_com::<$com>(h_shader) {
                Some(s) => context.$method(&*s, None),
                None => context.$method(None, None),
            }
        }
    };
}

stage_set_shader!(
    vs_set_shader, "VS", current_vs, ID3D11VertexShader, VSSetShader,
    // The one real asymmetry in the family: the input-variant recompiler
    // (`resolve_vs_input_variant`) reads this to find the bound VS.
    also_set: bound_vs_com
);
stage_set_shader!(
    ps_set_shader,
    "PS",
    current_ps,
    ID3D11PixelShader,
    PSSetShader
);
stage_set_shader!(
    gs_set_shader,
    "GS",
    current_gs,
    ID3D11GeometryShader,
    GSSetShader
);
stage_set_shader!(
    hs_set_shader,
    "HS",
    current_hs,
    ID3D11HullShader,
    HSSetShader
);
stage_set_shader!(
    ds_set_shader,
    "DS",
    current_ds,
    ID3D11DomainShader,
    DSSetShader
);
stage_set_shader!(
    cs_set_shader,
    "CS",
    current_cs,
    ID3D11ComputeShader,
    CSSetShader
);

/// Generate one stage's `pfn*SetShaderWithIfaces` entry point.
///
/// Class instances (dynamic shader linkage) are not implemented, so every one
/// of these forwards to the plain setter and ignores the three interface
/// arguments. Keeping that as six identical hand-written bodies made it look
/// like six independent decisions.
macro_rules! stage_set_shader_with_ifaces {
    ($name:ident, $plain:ident) => {
        pub(crate) unsafe extern "C" fn $name(
            h: Hdevice,
            h_shader: ddi::D3D10DDI_HSHADER,
            _num_class_instances: u32,
            _class_instance_ids: *const u32,
            _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
        ) {
            $plain(h, h_shader);
        }
    };
}

stage_set_shader_with_ifaces!(ps_set_shader_with_ifaces, ps_set_shader);
stage_set_shader_with_ifaces!(vs_set_shader_with_ifaces, vs_set_shader);
stage_set_shader_with_ifaces!(gs_set_shader_with_ifaces, gs_set_shader);
stage_set_shader_with_ifaces!(hs_set_shader_with_ifaces, hs_set_shader);
stage_set_shader_with_ifaces!(ds_set_shader_with_ifaces, ds_set_shader);
stage_set_shader_with_ifaces!(cs_set_shader_with_ifaces, cs_set_shader);

// --- Output-merger / rasterizer state setters -------------------------------
