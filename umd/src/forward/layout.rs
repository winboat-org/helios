//! Input layouts and the vertex-shader input variant cache.
//!
//! `isgn_lookup`, `build_layout_signature_blob`, the element-layout DDIs,
//! `bind_input_layout`, the VS input-variant resolver, and the IA vertex and
//! index buffer setters.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

/// One d3d10umddi input element, kept until the bound VS is known (the DDI gives
/// us the VS input *register*, not a semantic name, so we resolve names lazily).
pub(crate) struct DdiInputElement {
    pub(crate) input_slot: u32,
    pub(crate) aligned_byte_offset: u32,
    pub(crate) format: i32,
    pub(crate) input_slot_class: u32,
    pub(crate) instance_step_rate: u32,
    pub(crate) input_register: u32,
}

/// Element-layout data, Box'd and stashed in the CreateElementLayout handle.
pub(crate) struct LayoutData {
    pub(crate) elements: Vec<DdiInputElement>,
}

/// Parse a vertex shader's DXBC `ISGN` (input signature) chunk and return the
/// (semantic name, semantic index) of the element bound to input `register`.
/// `ID3D11Device::CreateInputLayout` needs semantic names, but the DDI only
/// passes the register index, so we recover the names from the shader bytecode.
pub(crate) unsafe fn isgn_lookup(dxbc: &[u8], register: u32) -> Option<(std::ffi::CString, u32)> {
    if dxbc.len() < 32 || &dxbc[0..4] != b"DXBC" {
        return None;
    }
    let chunk_count = u32::from_le_bytes(dxbc[28..32].try_into().ok()?) as usize;
    for i in 0..chunk_count {
        let off_pos = 32 + i * 4;
        if off_pos + 4 > dxbc.len() {
            return None;
        }
        let coff = u32::from_le_bytes(dxbc[off_pos..off_pos + 4].try_into().ok()?) as usize;
        if coff + 8 > dxbc.len() || &dxbc[coff..coff + 4] != b"ISGN" {
            continue;
        }
        let data = coff + 8; // skip FourCC + chunk size
        if data + 8 > dxbc.len() {
            return None;
        }
        let elem_count = u32::from_le_bytes(dxbc[data..data + 4].try_into().ok()?) as usize;
        for e in 0..elem_count {
            let ep = data + 8 + e * 24;
            if ep + 24 > dxbc.len() {
                return None;
            }
            let name_off = u32::from_le_bytes(dxbc[ep..ep + 4].try_into().ok()?) as usize;
            let sem_index = u32::from_le_bytes(dxbc[ep + 4..ep + 8].try_into().ok()?);
            let reg = u32::from_le_bytes(dxbc[ep + 16..ep + 20].try_into().ok()?);
            if reg == register {
                let nstart = data + name_off;
                // Every other offset in this function is checked; this one was
                // not, and `&v[a..a]` with `a > len` is out of bounds in Rust —
                // a panic in a DDI is a silent graphics deadlock.
                if nstart >= dxbc.len() {
                    return None;
                }
                let mut nend = nstart;
                while nend < dxbc.len() && dxbc[nend] != 0 {
                    nend += 1;
                }
                let name = std::ffi::CString::new(&dxbc[nstart..nend]).ok()?;
                return Some((name, sem_index));
            }
        }
        return None;
    }
    None
}

// R423 note: the review asks for a host-target unit test asserting that
// `isgn_lookup` returns None (rather than panicking) for a `name_off` past the
// chunk. It is not addable in T2: this crate is `crate-type = ["cdylib"]`, so
// `cargo test` has no lib target, and adding `rlib` is not enough — the test
// harness then fails to link because build.rs passes the DXVK static archives
// through `cargo:rustc-link-arg-cdylib`, and this cargo rejects
// `rustc-link-arg-tests` as an invalid instruction. Switching them to a plain
// `rustc-link-arg` would change the SHIPPING cdylib's link line, which is not a
// trade worth making for one test. The analogue of T0's host-testable
// `kmd_logic` crate does not exist for the UMD; creating one is a file-split
// change and belongs with T8. The bounds check itself is in `isgn_lookup`.

/// Build a minimal DXBC container with a synthetic `ISGN` chunk for the given
/// input registers, followed by the raw SM4/SM5 token stream (SHDR/SHEX).
///
/// The DDI hands shaders to the driver as raw token streams with no DXBC
/// container, so there are no semantic names to recover for
/// `ID3D11Device::CreateInputLayout`. Names are only a matching key between
/// the layout descs and the shader signature, and DXVK resolves an element's
/// vertex-input LOCATION from the matched signature entry's register — which
/// is also how dxbc-spv assigns locations (`dcl_input v[r]` → location r) when
/// it compiles the container-less shader. So a fabricated "TEXCOORD<r>" per
/// register keeps both sides consistent.
pub(crate) fn build_layout_signature_blob(registers: &[u32], tokens: &[u8]) -> Vec<u8> {
    const NAME: &[u8] = b"TEXCOORD\0";
    let entry_count = registers.len();
    let entries_size = entry_count * 24;
    let name_off = 8 + entries_size; // offsets are relative to chunk-data start
    let isgn_len_unpadded = name_off + NAME.len();
    let isgn_len = (isgn_len_unpadded + 3) & !3;

    // Code chunk tag from the version token (major >= 5 uses SHEX).
    let version_token = if tokens.len() >= 4 {
        u32::from_le_bytes(tokens[0..4].try_into().unwrap())
    } else {
        0
    };
    let code_tag: &[u8; 4] = if ((version_token >> 4) & 0xF) >= 5 {
        b"SHEX"
    } else {
        b"SHDR"
    };

    // DXBC header (32) + 2 chunk offsets (8).
    let isgn_chunk_off = 32 + 8;
    let code_chunk_off = isgn_chunk_off + 8 + isgn_len;
    let total = code_chunk_off + 8 + tokens.len();

    let mut blob = vec![0u8; total];
    blob[0..4].copy_from_slice(b"DXBC");
    // bytes 4..20: checksum left zero (DXVK does not verify it)
    blob[20..24].copy_from_slice(&1u32.to_le_bytes());
    blob[24..28].copy_from_slice(&(total as u32).to_le_bytes());
    blob[28..32].copy_from_slice(&2u32.to_le_bytes());
    blob[32..36].copy_from_slice(&(isgn_chunk_off as u32).to_le_bytes());
    blob[36..40].copy_from_slice(&(code_chunk_off as u32).to_le_bytes());

    blob[isgn_chunk_off..isgn_chunk_off + 4].copy_from_slice(b"ISGN");
    blob[isgn_chunk_off + 4..isgn_chunk_off + 8].copy_from_slice(&(isgn_len as u32).to_le_bytes());
    let data = isgn_chunk_off + 8;
    blob[data..data + 4].copy_from_slice(&(entry_count as u32).to_le_bytes());
    blob[data + 4..data + 8].copy_from_slice(&8u32.to_le_bytes());
    for (i, reg) in registers.iter().enumerate() {
        let ep = data + 8 + i * 24;
        blob[ep..ep + 4].copy_from_slice(&(name_off as u32).to_le_bytes());
        blob[ep + 4..ep + 8].copy_from_slice(&reg.to_le_bytes()); // semantic index = register
        blob[ep + 8..ep + 12].copy_from_slice(&0u32.to_le_bytes()); // no system value
        blob[ep + 12..ep + 16].copy_from_slice(&3u32.to_le_bytes()); // float32
        blob[ep + 16..ep + 20].copy_from_slice(&reg.to_le_bytes());
        blob[ep + 20] = 0x0F; // mask
        blob[ep + 21] = 0x0F; // read/write mask
    }
    blob[data + name_off..data + name_off + NAME.len()].copy_from_slice(NAME);

    blob[code_chunk_off..code_chunk_off + 4].copy_from_slice(code_tag);
    blob[code_chunk_off + 4..code_chunk_off + 8]
        .copy_from_slice(&(tokens.len() as u32).to_le_bytes());
    blob[code_chunk_off + 8..code_chunk_off + 8 + tokens.len()].copy_from_slice(tokens);
    blob
}

pub(crate) unsafe extern "C" fn calc_size_element_layout(
    _h: Hdevice,
    _a: *const ddi::D3D10DDIARG_CREATEELEMENTLAYOUT,
) -> u64 {
    8
}

pub(crate) unsafe extern "C" fn create_element_layout(
    _h: Hdevice,
    arg: *const ddi::D3D10DDIARG_CREATEELEMENTLAYOUT,
    h_el: ddi::D3D10DDI_HELEMENTLAYOUT,
    _hrt: ddi::D3D10DDI_HRTELEMENTLAYOUT,
) {
    clear_handle(h_el);
    let a = &*arg;
    let mut elems = Vec::with_capacity(a.NumElements as usize);
    for i in 0..a.NumElements as usize {
        let e = &*a.pVertexElements.add(i);
        elems.push(DdiInputElement {
            input_slot: e.InputSlot,
            aligned_byte_offset: e.AlignedByteOffset,
            format: e.Format as i32,
            input_slot_class: e.InputSlotClass as u32,
            instance_step_rate: e.InstanceDataStepRate,
            input_register: e.InputRegister,
        });
    }
    // The element-layout slot is a fourth payload kind: a Box this driver
    // allocated, but stored as a bare `usize` and then used as the identity key
    // of `ShaderCaches::layout_cache`. R803's finding does not list it; it is the
    // same latent confusion as the resource and RTV slots.
    let Some(slot) = boxed_slot(h_el) else {
        return;
    };
    slot.store(LayoutData { elements: elems });
}

pub(crate) unsafe extern "C" fn destroy_element_layout(
    h: Hdevice,
    h_el: ddi::D3D10DDI_HELEMENTLAYOUT,
) {
    let Some(slot) = boxed_slot(h_el) else {
        return;
    };
    // The slot word doubles as this layout's identity in `layout_cache`, so it
    // is read before the box is taken.
    let p = slot.word();
    if p != 0 {
        let mut owned = std::collections::HashSet::new();
        if let Some(dev) = helios_device(h) {
            dev.owned
                .caches_lock()
                .layout_cache
                .retain(|&(layout, _), cached| {
                    if layout == p {
                        if *cached != 0 {
                            owned.insert(*cached);
                        }
                        false
                    } else {
                        true
                    }
                });
            let _ = dev.owned.bindings.current_layout.compare_exchange(
                p,
                0,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        for cached in &owned {
            // SAFETY: each removed cache value owns one CreateInputLayout
            // reference, adopted here and released exactly once.
            drop(IUnknown::from_raw(*cached as *mut c_void));
        }
        trace_line!(
            "DDI DestroyElementLayout: layout=0x{:x} released_cached={}",
            p,
            owned.len()
        );
        drop(slot.take());
    }
}

pub(crate) unsafe extern "C" fn ia_set_input_layout(
    h: Hdevice,
    h_el: ddi::D3D10DDI_HELEMENTLAYOUT,
) {
    if let Some(dev) = helios_device(h) {
        let p = match boxed_slot(h_el) {
            Some(slot) => slot.word(),
            None => 0,
        };
        dev.owned.bindings.current_layout.store(p, Ordering::Relaxed);
    }
}

/// Lazily create + bind the `ID3D11InputLayout` for the current (element layout,
/// VS) pair, resolving element semantic names from the VS input signature.
pub(crate) unsafe fn bind_input_layout(h: Hdevice) {
    let Some(dev) = helios_device(h) else {
        return;
    };
    let (lp, vp) = {
        let bindings = &dev.owned.bindings;
        (
            bindings.current_layout.load(Ordering::Relaxed),
            bindings.current_vs.load(Ordering::Relaxed),
        )
    };
    if lp == 0 || vp == 0 {
        if SHADER_BIND_LOG_COUNT.first_n(256).is_some() {
            log_error!(
                "DDI bind_input_layout skipped: layout=0x{:x} vs=0x{:x}",
                lp,
                vp
            );
        }
        return;
    }
    let cached = dev.owned.caches_lock().layout_cache.get(&(lp, vp)).copied();
    let il_raw = match cached {
        Some(p) => p,
        None => {
            let bytecode = match dev.owned.caches_lock().vs_bytecode.get(&vp) {
                Some(b) => b.clone(),
                None => {
                    log_error!(
                        "DDI bind_input_layout skipped: missing VS bytecode layout=0x{:x} vs=0x{:x}",
                        lp, vp
                    );
                    return;
                }
            };
            let layout = &*(lp as *const LayoutData);
            let is_dxbc = bytecode.len() >= 4 && &bytecode[0..4] == b"DXBC";
            // Reserve so the CString store never reallocates (the descs below
            // borrow raw pointers into it for the CreateInputLayout call).
            let mut names: Vec<std::ffi::CString> = Vec::with_capacity(layout.elements.len());
            let mut descs: Vec<D3D11_INPUT_ELEMENT_DESC> =
                Vec::with_capacity(layout.elements.len());
            let mut registers: Vec<u32> = Vec::with_capacity(layout.elements.len());
            for el in &layout.elements {
                let (name, sem_index) = if is_dxbc {
                    // Real container: recover the shader's own semantic names.
                    match isgn_lookup(&bytecode, el.input_register) {
                        Some(v) => v,
                        None => {
                            log_error!(
                                "DDI bind_input_layout: no ISGN entry for input_register={} fmt={} slot={} offset={}",
                                el.input_register, el.format, el.input_slot, el.aligned_byte_offset
                            );
                            continue;
                        }
                    }
                } else {
                    // Raw DDI token stream: no ISGN exists. Fabricate
                    // TEXCOORD<register> and pair it with a synthetic ISGN in
                    // the blob below so name-matching resolves to the register.
                    (
                        std::ffi::CString::new("TEXCOORD").unwrap(),
                        el.input_register,
                    )
                };
                names.push(name);
                let name_ptr = names.last().unwrap().as_ptr() as *const u8;
                if !registers.contains(&el.input_register) {
                    registers.push(el.input_register);
                }
                descs.push(D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: PCSTR(name_ptr),
                    SemanticIndex: sem_index,
                    Format: DXGI_FORMAT(el.format),
                    InputSlot: el.input_slot,
                    AlignedByteOffset: el.aligned_byte_offset,
                    InputSlotClass: D3D11_INPUT_CLASSIFICATION(el.input_slot_class as i32),
                    InstanceDataStepRate: el.instance_step_rate,
                });
            }
            if descs.is_empty() {
                log_error!(
                    "DDI bind_input_layout skipped: empty descs elements={} vs_len={}",
                    layout.elements.len(),
                    bytecode.len()
                );
                return;
            }
            let signature_blob;
            let blob_for_layout: &[u8] = if is_dxbc {
                &bytecode
            } else {
                signature_blob = build_layout_signature_blob(&registers, &bytecode);
                &signature_blob
            };
            let Some(device) = d3d11_device(h) else {
                return;
            };
            let mut il: Option<ID3D11InputLayout> = None;
            match device.CreateInputLayout(&descs, blob_for_layout, Some(&mut il)) {
                Ok(()) => match il {
                    Some(l) => {
                        let raw = l.into_raw() as usize;
                        log_error!(
                            "DDI CreateInputLayout ok: layout=0x{:x} vs=0x{:x} elems={} raw=0x{:x}",
                            lp,
                            vp,
                            descs.len(),
                            raw
                        );
                        // Insert-race arm: under FREETHREADED caps two threads
                        // can miss and create concurrently. The loser releases
                        // its layout and binds the winner's — never leak, never
                        // orphan a cached entry. The COM release happens after
                        // the caches guard drops.
                        let (chosen, loser) = {
                            let mut caches = dev.owned.caches_lock();
                            match caches.layout_cache.entry((lp, vp)) {
                                std::collections::hash_map::Entry::Occupied(e) => {
                                    (*e.get(), Some(raw))
                                }
                                std::collections::hash_map::Entry::Vacant(e) => {
                                    e.insert(raw);
                                    (raw, None)
                                }
                            }
                        };
                        if let Some(loser) = loser {
                            drop(IUnknown::from_raw(loser as *mut c_void));
                        }
                        chosen
                    }
                    None => return,
                },
                Err(e) => {
                    log_error!("CreateInputLayout failed: {e:?}");
                    return;
                }
            }
        }
    };
    if let Some(context) = d3d11_context(h) {
        let il = ManuallyDrop::new(ID3D11InputLayout::from_raw(il_raw as *mut c_void));
        context.IASetInputLayout(&*il);
    }

    // VUID-Input-08733: the DDI never provides shader-input component types
    // (RegisterComponentType arrives 0/UNKNOWN — verified against both dwm's
    // SM4 shaders and the SM5 draw probe), so compiled VS inputs default to
    // float32 while layouts may bind SINT/UINT vertex formats: vertex-fetch
    // UB (dwm binds R16G16_SINT TEXCOORDs; the garbage is the prime Xid-109
    // suspect). The INPUT LAYOUT is the ground truth for the numeric class —
    // any (layout, VS) pair the runtime allows to bind matched the app's
    // original input signature — so bind a variant recompiled with the
    // layout's classes whenever any attribute is non-float.
    resolve_vs_input_variant(h, lp, vp);
}

/// Numeric class of a DXGI vertex format for Vulkan's vertex-input contract,
/// as a DXBC ISGN component type: 1 = UINT, 2 = SINT, 3 = FLOAT (covers
/// FLOAT/UNORM/SNORM — all float-class at the fetch).
pub(crate) fn dxgi_vertex_class(format: i32) -> u32 {
    match format {
        // *_UINT: R32G32B32A32, R32G32B32, R16G16B16A16, R32G32, R10G10B10A2,
        // R8G8B8A8, R16G16, R32, R8G8, R16, R8
        3 | 7 | 12 | 17 | 25 | 30 | 36 | 42 | 50 | 57 | 62 => 1,
        // *_SINT: same families
        4 | 8 | 14 | 18 | 32 | 38 | 43 | 52 | 59 | 64 => 2,
        _ => 3,
    }
}

/// Component mask of a DXGI vertex format (for synthesized ISGN entries).
pub(crate) fn dxgi_vertex_mask(format: i32) -> u32 {
    match format {
        1..=4 | 9..=14 | 19..=32 => 0xf,    // 4-component families
        5..=8 => 0x7,                       // R32G32B32
        15..=18 | 33..=38 | 48..=52 => 0x3, // 2-component families
        _ => 0x1,                           // scalars and the rest
    }
}

/// Pick (and lazily compile) the vertex-shader variant whose declared input
/// component types match the bound layout's format classes, then bind it.
/// All-float layouts (the overwhelmingly common case) bind the original.
pub(crate) unsafe fn resolve_vs_input_variant(h: Hdevice, lp: usize, vp: usize) {
    let Some(dev) = helios_device(h) else {
        return;
    };
    let layout = &*(lp as *const LayoutData);
    // (register, class, mask) per input register, merging multi-element
    // registers (matrix-style attributes span elements, same class).
    let mut classes: Vec<(u32, u32, u32)> = Vec::new();
    let mut any_nonfloat = false;
    for el in &layout.elements {
        let class = dxgi_vertex_class(el.format);
        let mask = dxgi_vertex_mask(el.format);
        if class != 3 {
            any_nonfloat = true;
        }
        if let Some(entry) = classes.iter_mut().find(|c| c.0 == el.input_register) {
            entry.1 = entry.1.max(class);
            entry.2 |= mask;
        } else {
            classes.push((el.input_register, class, mask));
        }
    }

    let desired = if !any_nonfloat {
        vp
    } else {
        // FNV-1a over (register, class) pairs = the variant cache key.
        let mut key: u64 = 0xcbf2_9ce4_8422_2325;
        for &(r, c, _) in &classes {
            key = (key ^ (((r as u64) << 8) | c as u64)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        let cached = dev.owned.caches_lock().vs_variants.get(&(vp, key)).copied();
        let variant = match cached {
            Some(v) => v,
            None => {
                let v = create_vs_input_variant(dev, vp, &classes);
                // Insert-race arm (FREETHREADED): keep the winner, release the
                // loser's shader after the guard drops.
                let (chosen, loser) = {
                    let mut caches = dev.owned.caches_lock();
                    match caches.vs_variants.entry((vp, key)) {
                        std::collections::hash_map::Entry::Occupied(e) => (*e.get(), v),
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(v);
                            (v, 0)
                        }
                    }
                };
                if loser != 0 {
                    drop(IUnknown::from_raw(loser as *mut c_void));
                }
                chosen
            }
        };
        if variant != 0 {
            variant
        } else {
            vp
        }
    };

    if dev.owned.bindings.bound_vs_com.load(Ordering::Relaxed) == desired {
        return;
    }
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let s = ManuallyDrop::new(ID3D11VertexShader::from_raw(desired as *mut c_void));
    context.VSSetShader(&*s, None);
    dev.owned.bindings.bound_vs_com.store(desired, Ordering::Relaxed);
    if SHADER_BIND_LOG_COUNT.first_n(256).is_some() {
        trace_line!("DDI VS input-class variant bound: vs=0x{vp:x} -> 0x{desired:x}");
    }
}

/// Recompile a vertex shader with its synthesized ISGN component types taken
/// from the bound input layout. Returns 0 on failure (caller falls back to
/// the original shader — no worse than the pre-variant behaviour).
pub(crate) unsafe fn create_vs_input_variant(
    dev: &HeliosDevice,
    vp: usize,
    classes: &[(u32, u32, u32)],
) -> usize {
    let (bytecode, mut words) = {
        let caches = dev.owned.caches_lock();
        let Some(b) = caches.vs_bytecode.get(&vp) else {
            log_error!("VS variant: no bytecode for vs=0x{vp:x}");
            return 0;
        };
        if b.len() >= 4 && &b[0..4] == b"DXBC" {
            // Real container: its own ISGN already carries real types.
            return 0;
        }
        let w = caches
            .vs_sig_words
            .get(&vp)
            .cloned()
            .and_then(SigWords::adopt_stage)
            .unwrap_or_else(|| SigWords::stage(0, 0));
        (b.clone(), w)
    };

    let n_in = words.n_in();
    if n_in > 0 {
        // Patch the DDI-provided entries' component types from the layout.
        for i in 0..n_in {
            let Some(e) = words.block().input(i) else {
                break;
            };
            let class = match classes.iter().find(|c| c.0 == e.register_) {
                Some(&(_, class, _)) => class,
                None if e.comptype == 0 => 3,
                None => continue,
            };
            words.set_input_comptype(i, class);
        }
    } else {
        // Shader arrived through the legacy untyped create: synthesize the
        // input entries wholesale from the layout (extra entries for unused
        // registers are declared-then-eliminated by the compiler).
        let synthesized: Vec<SigEntry> = classes
            .iter()
            .map(|&(reg, class, mask)| SigEntry {
                sysval: 0,
                register_: reg,
                mask,
                comptype: class,
                stream: 0,
            })
            .collect();
        words.replace_inputs(&synthesized);
    }

    let dxvk = &dev.dxvk;
    let words = words.into_words();
    let raw = dxvk.create_shader_sig(
        0,
        bytecode.as_ptr(),
        bytecode.len(),
        words.as_ptr(),
        words.len(),
    );
    log_error!(
        "VS input-class variant: vs=0x{vp:x} classes={:?} -> raw=0x{raw:x}",
        classes.iter().map(|c| (c.0, c.1)).collect::<Vec<_>>()
    );
    raw
}

pub(crate) unsafe extern "C" fn ia_set_vertex_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    buffers: *const ddi::D3D10DDI_HRESOURCE,
    strides: *const u32,
    offsets: *const u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let mut bufs: Vec<Option<ID3D11Buffer>> = Vec::with_capacity(num as usize);
    for i in 0..num as usize {
        let h_buf = *buffers.add(i);
        bufs.push(load_resource(h_buf).and_then(|r| (*r).cast::<ID3D11Buffer>().ok()));
    }
    if let Some(dev) = helios_device(h) {
        if start == 0 && num != 0 {
            let bindings = &dev.owned.bindings;
            bindings.current_vb0.store(
                bufs.first()
                    .and_then(|b| b.as_ref())
                    .map(|b| b.as_raw() as usize)
                    .unwrap_or(0),
                Ordering::Relaxed,
            );
            bindings.current_vb0_stride.store(
                if strides.is_null() { 0 } else { *strides },
                Ordering::Relaxed,
            );
            bindings.current_vb0_offset.store(
                if offsets.is_null() { 0 } else { *offsets },
                Ordering::Relaxed,
            );
        }
    }
    let n = IA_BIND_LOG_COUNT.next();
    if n < 128 || num == 0 {
        let first_stride = if num != 0 && !strides.is_null() {
            *strides
        } else {
            0
        };
        let first_offset = if num != 0 && !offsets.is_null() {
            *offsets
        } else {
            0
        };
        let first_raw = bufs
            .first()
            .and_then(|b| b.as_ref())
            .map(|b| b.as_raw() as usize)
            .unwrap_or(0);
        trace_line!(
            "DDI IASetVertexBuffers start={} num={} first=0x{:x} stride={} offset={}",
            start,
            num,
            first_raw,
            first_stride,
            first_offset
        );
    }
    context.IASetVertexBuffers(
        start,
        num,
        Some(bufs.as_ptr()),
        Some(strides),
        Some(offsets),
    );
}

pub(crate) unsafe extern "C" fn ia_set_index_buffer(
    h: Hdevice,
    h_buf: ddi::D3D10DDI_HRESOURCE,
    format: ddi::DXGI_FORMAT,
    offset: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let buf = load_resource(h_buf).and_then(|r| (*r).cast::<ID3D11Buffer>().ok());
    if let Some(dev) = helios_device(h) {
        let bindings = &dev.owned.bindings;
        bindings.current_ib.store(
            buf.as_ref().map(|b| b.as_raw() as usize).unwrap_or(0),
            Ordering::Relaxed,
        );
        bindings.current_ib_format.store(format as u32, Ordering::Relaxed);
        bindings.current_ib_offset.store(offset, Ordering::Relaxed);
    }
    if IA_BIND_LOG_COUNT.first_n(128).is_some() {
        trace_line!(
            "DDI IASetIndexBuffer raw=0x{:x} fmt={} offset={}",
            buf.as_ref().map(|b| b.as_raw() as usize).unwrap_or(0),
            format as u32,
            offset
        );
    }
    context.IASetIndexBuffer(buf.as_ref(), DXGI_FORMAT(format as i32), offset);
}
