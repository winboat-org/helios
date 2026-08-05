//! Build script for the Helios D3D12 WDDM UMD.
//!
//! # Stage S3 — and only S3
//!
//! `ARCHITECTURE.md` §11 stages this crate. **This script does exactly one
//! thing: bindgen `d3d12umddi.h`.** It does *not* compile a cxx bridge and does
//! *not* link the vkd3d archives — that is S4, and `DECISIONS.md` §7.1's
//! standing rule is that `OpenAdapter12` stops refusing in the same commit that
//! makes its body reachable, or the body is not written yet.
//!
//! ⛔ When S4 does land, the link set is **measured, not guessed**
//! (`D12-G1` static arm, `tmp/dx12/gates/G1-static/RESULT.md`):
//!
//! ```text
//! C:\Users\Rupansh\vkd3d-build\libs\d3d12core\libhelios_d3d12_static.a
//! cargo:rustc-link-lib=dylib=gdi32
//! ```
//!
//! **One archive** — it is a union carrying every vkd3d / dxil-spirv /
//! dxbc-spirv object — plus `gdi32` for the 12 `__imp_D3DKMT*` that
//! `libs/vkd3d/d3dkmt.c` imports. ⛔ **Never `dxgi`**: `umd/build.rs:239-243`
//! states the rule and the static engine is the first artifact that keeps it.
//!
//! # The deliverable
//!
//! The layout assertions. `layout_tests(true)` makes bindgen emit a
//! compile-time size/alignment/offset check per type, so **if this crate
//! compiles, the D3D12 DDI ABI is machine-checked against the SDK header**.
//! That is the whole point of the stage: `ARCHITECTURE.md` §12 rule 1 —
//! *never hand-transcribe a DDI ABI struct* — and R908 is what ignoring it
//! cost.

use std::env;
use std::path::{Path, PathBuf};

fn def(var: &str, default: &str) -> String {
    env::var(var).unwrap_or_else(|_| default.to_string())
}

/// Pick the highest-versioned MSVC include directory (for the vcruntime/STL
/// headers the SDK headers transitively pull in).
///
/// NOTE: the sort is lexicographic over directory names, not semantic, so a
/// hypothetical `14.9.x` would outrank `14.44.x`. Only one toolset is installed
/// today; set `HELIOS_MSVC_INCLUDE` if that ever stops being true. Same
/// function, same caveat, as `umd/build.rs` — deliberately not shared, because
/// `umd_common` has no `build.rs` and must not acquire one (`DECISIONS.md` D3b:
/// a build script there would drag the WDK into a crate that must also build on
/// Linux).
fn find_msvc_include() -> String {
    if let Ok(v) = env::var("HELIOS_MSVC_INCLUDE") {
        return v;
    }
    let root = Path::new(r"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("include").is_dir())
        .collect();
    versions.sort();
    versions
        .last()
        .map(|p| p.join("include").to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            panic!(
                "no MSVC toolset with an include/ directory under {}; set HELIOS_MSVC_INCLUDE \
                 to the vcruntime/STL include directory",
                root.display()
            )
        })
}

fn generate_d3d12umddi_bindings() {
    let sdk_inc = def(
        "HELIOS_WDK_INCLUDE",
        r"C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0",
    );
    let msvc_inc = find_msvc_include();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    let bindings = bindgen::Builder::default()
        .header("bindgen/d3d12umddi_wrapper.h")
        .clang_args([
            "-target".to_string(),
            "x86_64-pc-windows-msvc".to_string(),
            format!("-I{msvc_inc}"),
            format!(r"-I{sdk_inc}\um"),
            format!(r"-I{sdk_inc}\shared"),
            format!(r"-I{sdk_inc}\ucrt"),
            format!(r"-I{sdk_inc}\winrt"),
        ])
        // The DDI surface. Mirrors `umd/build.rs`'s allowlist shape, retargeted:
        //
        //  * `D3D12DDI.*`          — every DDI struct, enum and arg type, which
        //                            includes the 43-enumerator
        //                            `D3D12DDICAPS_TYPE` (`d3d12umddi.h:94-150`)
        //                            and the three `D3D12DDI_FEATURE_*_106`
        //                            values that live in the SAME enum.
        //  * `PFND3D12DDI.*`       — the ~296 named function-pointer typedefs.
        //                            ⚠ 173 of them are absent from SDK 26100
        //                            despite appearing in DirectX-Specs
        //                            (memory `dx12-specs-mined-74th`); bindgen
        //                            generates what the header HAS, which is
        //                            precisely why this is generated and not
        //                            transcribed.
        //  * `D3DDDI.*` / `D3DKMT.*` — `D3D12DDIARG_CREATEDEVICE_0109` carries a
        //                            `CONST D3DDDI_DEVICECALLBACKS*`
        //                            (`d3d12umddi.h:13623`), the same 65-entry
        //                            table the D3D11 UMD drives, so the kernel
        //                            callback types must come along.
        //  * `DXGI.*`              — the DXGI DDI types the header references.
        //                            ⚠ `D12-G5` measured that this build never
        //                            requests `D3D12DDI_TABLE_TYPE_DXGI`
        //                            (`DDI_REFERENCE.md` §2.3), so these are
        //                            expected to stay unused. Generated anyway:
        //                            an allowlist that omits a type the header
        //                            reaches through produces an opaque blob,
        //                            which is exactly the ABI hole this stage
        //                            exists to close.
        .allowlist_type("D3D12DDI.*")
        .allowlist_type("PFND3D12DDI.*")
        .allowlist_type("D3D12_.*")
        .allowlist_type("D3DDDI.*")
        .allowlist_type("D3DKMT.*")
        .allowlist_type("DXGI_?DDI.*")
        .allowlist_var("D3D12DDI_.*")
        .allowlist_var("D3D12_.*")
        // ⛔ THE DELIVERABLE. Compile-time size/alignment/offset assertions for
        // every generated type. bindgen 0.70 emits them as
        //   const _: () = { ["Offset of field: X::y"][offset_of!(X, y) - N]; };
        // so a mismatch is an E0080 const-evaluation failure during an ordinary
        // `cargo build`, not a `#[test]` that has to be run.
        //
        // ⚠ NEVER drop this to shrink the generated file. `UNVERIFIED-2` names
        // the cost and the ONLY sanctioned mitigation: narrow the allowlist to
        // the implemented DDI versions. The assertions are the reason the crate
        // exists at this stage.
        .layout_tests(true)
        .derive_default(true)
        .generate_comments(false)
        .generate()
        .expect("bindgen failed to generate d3d12umddi bindings");

    bindings
        .write_to_file(out.join("d3d12umddi.rs"))
        .expect("failed to write d3d12umddi.rs");

    println!("cargo:rerun-if-changed=bindgen/d3d12umddi_wrapper.h");
    println!("cargo:rerun-if-env-changed=HELIOS_WDK_INCLUDE");
    // These bindings are generated against this include path, so changing the
    // selection must regenerate them.
    println!("cargo:rerun-if-env-changed=HELIOS_MSVC_INCLUDE");
}

/// The committed copy of the generated bindings, used to TYPE-CHECK on a host
/// that has no WDK.
///
/// ⭐ This is what lets `cargo check --target x86_64-pc-windows-msvc` run on the
/// **Linux host**, which is the difference between eleven agents writing 214 DDI
/// handlers blind and eleven agents writing them against the real signatures
/// with the compiler answering (`PARALLEL.md` §7).
///
/// ⛔ **It is never used to build a shipping DLL.** On Windows the bindings are
/// regenerated from `d3d12umddi.h` every time and this file is only *compared*
/// against, so a stale cache is loud rather than silent. The SDK header stays
/// the single source of truth.
const CACHED_BINDINGS: &str = "bindgen/cached/d3d12umddi.rs";

/// Refresh the cache from a freshly generated file, and say so.
fn compare_or_refresh_cache(fresh: &Path) {
    let cached = Path::new(CACHED_BINDINGS);
    let fresh_text = std::fs::read_to_string(fresh).unwrap_or_default();
    let cached_text = std::fs::read_to_string(cached).unwrap_or_default();
    if fresh_text == cached_text {
        return;
    }
    // ⚠ Loud, not fatal: a WDK/SDK update legitimately changes the output, and
    // failing the Windows build would block the very machine that can fix it.
    // But it MUST be noticed, because until the cache is refreshed every
    // host-side `cargo check` is type-checking against a different ABI than the
    // one being shipped.
    println!(
        "cargo:warning=helios_umd12: {CACHED_BINDINGS} is STALE ({} bytes cached vs {} generated). \
         Host-side cross-checks are now against a different ABI than this build. Refresh it: \
         copy $OUT_DIR/d3d12umddi.rs over it and commit.",
        cached_text.len(),
        fresh_text.len()
    );
}

fn main() {
    println!("cargo:rerun-if-changed={CACHED_BINDINGS}");

    // ⚠ Two different questions, and conflating them is the bug this shape
    // avoids. `TARGET` is what we are compiling FOR; `cfg!(windows)` here is
    // what the BUILD SCRIPT is running ON. bindgen needs the WDK, which lives
    // on the build host — so the SDK availability question is about the host,
    // not the target.
    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        // Not even targeting Windows: nothing here is meaningful.
        // `src/lib.rs`'s `#[cfg(not(windows))] compile_error!` reports that, and
        // it keys off this same target.
        println!("cargo:warning=helios_umd12: skipping d3d12umddi bindgen on non-Windows target");
        return;
    }

    if cfg!(windows) {
        // The real path: regenerate from the SDK header. Ground truth.
        generate_d3d12umddi_bindings();
        let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("d3d12umddi.rs");
        compare_or_refresh_cache(&out);
        return;
    }

    // Cross-checking from a WDK-less host (the Linux side). Serve the cached
    // generation so `ddi12.rs`'s `include!` resolves and the whole DDI surface
    // type-checks. ⛔ `cargo check` only — this never links a DLL, and
    // `PARALLEL.md` §10 requires the integrator to re-check on the VM.
    let cached = Path::new(CACHED_BINDINGS);
    if !cached.is_file() {
        panic!(
            "helios_umd12: cross-checking for {target} on a host with no WDK, and {CACHED_BINDINGS} \
             is missing. Generate it on the VM (umd-check.ps1 -Crate umd12) and copy \
             $OUT_DIR/d3d12umddi.rs there."
        );
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("d3d12umddi.rs");
    std::fs::copy(cached, &out).expect("failed to stage cached d3d12umddi.rs");
    println!(
        "cargo:warning=helios_umd12: HOST CROSS-CHECK — using {CACHED_BINDINGS}, not the SDK \
         header. Types are checked; nothing is linked and no ABI claim is made here."
    );
}
