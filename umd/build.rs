//! Build script for the Helios WDDM UMD.
//!
//! Compiles the cxx bridge (`bridge/dxvk_bridge.cpp`) that wraps DXVK's C++
//! engine, and links the prebuilt DXVK static libraries into `helios_umd.dll`.
//!
//! DXVK is built separately under clang-cl/meson into a local C: tree (see
//! `GATE5B_D3D_BRINGUP.md`). The default locations match that build; override
//! with `HELIOS_DXVK_SRC` / `HELIOS_DXVK_BUILD` / `HELIOS_CLANG_CL` if needed.
//!
//! Toolchain coherence (critical): DXVK, the cxx shim, and the Rust crate must
//! all use the MSVC C++ ABI with the **dynamic** CRT (`/MD`). DXVK is compiled
//! with clang-cl + `-Db_vscrt=md`; we compile the shim with the same clang-cl so
//! the objects link against one another and against the Rust msvc target.

use std::env;
use std::path::{Path, PathBuf};

fn def(var: &str, default: &str) -> String {
    env::var(var).unwrap_or_else(|_| default.to_string())
}

/// Pick the highest-versioned MSVC include directory (for vcruntime/STL headers
/// that the WDK headers transitively pull in).
///
/// NOTE: the sort is lexicographic over directory names, not semantic, so a
/// hypothetical `14.9.x` would outrank `14.44.x`. Only one toolset is installed
/// today; set `HELIOS_MSVC_INCLUDE` if that ever stops being true.
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
        // Previously this invented a literal version number, which turns a
        // missing or moved toolset into a bindgen failure against a path that
        // was never scanned for. Name the override instead.
        .unwrap_or_else(|| {
            panic!(
                "no MSVC toolset with an include/ directory under {}; set HELIOS_MSVC_INCLUDE \
                 to the vcruntime/STL include directory",
                root.display()
            )
        })
}

/// Fail the build at the point the path is chosen, naming the env var that
/// overrides it.
///
/// Every one of these four defaults is an absolute path baked into this script.
/// Without the check a wrong path surfaces far from its cause — as a clang
/// include error, a missing-archive link error, or a "program not found" from
/// `cc` — and none of those name the variable that would fix it.
fn require_path(env_var: &str, value: &str, dir: bool) {
    let path = Path::new(value);
    let ok = if dir { path.is_dir() } else { path.is_file() };
    if !ok {
        let kind = if dir { "directory" } else { "file" };
        panic!("helios_umd: {env_var} {kind} not found: {value} (override with {env_var})");
    }
}

/// Generate Rust types for the d3d10umddi DDI (device-funcs tables, the
/// CREATEDEVICE/OPENADAPTER arg structs, the runtime callback tables) from the
/// WDK header. The 152-entry D3D11DDI_DEVICEFUNCS table is far too large to
/// hand-transcribe with correct PFN signatures, so we bindgen it.
fn generate_d3d10umddi_bindings() {
    let sdk_inc = def(
        "HELIOS_WDK_INCLUDE",
        r"C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0",
    );
    let msvc_inc = find_msvc_include();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    let bindings = bindgen::Builder::default()
        .header("bindgen/d3d10umddi_wrapper.h")
        .clang_args([
            "-target".to_string(),
            "x86_64-pc-windows-msvc".to_string(),
            format!("-I{msvc_inc}"),
            format!(r"-I{sdk_inc}\um"),
            format!(r"-I{sdk_inc}\shared"),
            format!(r"-I{sdk_inc}\ucrt"),
            format!(r"-I{sdk_inc}\winrt"),
        ])
        // The DDI surface: device-funcs tables, adapter funcs, arg structs,
        // caps, the DXGI base DDI, and the kernel/runtime callback tables.
        .allowlist_type("D3D1[012].*")
        .allowlist_type("D3DWDDM2.*")
        .allowlist_type("D3DDDI.*")
        .allowlist_type("DXGI_?DDI.*")
        .allowlist_type("PFND3D1.*")
        .allowlist_var("D3D1[012].*_DDI_.*")
        .allowlist_var("D3DWDDM.*")
        // Emit bindgen's per-type size/alignment/offset assertions. These are
        // what make R802's deletion of the hand-transcribed ABI structs safe:
        // the generated module becomes self-checking against the WDK headers it
        // was produced from -- currently 817 size, 815 alignment and 4704 field
        // offsets across 818 types.
        //
        // These are COMPILE-TIME. bindgen 0.70 emits them as
        //   const _: () = { ["Offset of field: X::y"][offset_of!(X, y) - N]; };
        // so a mismatch is an E0080 const-evaluation failure during an ordinary
        // `cargo build`, not a `#[test]` that has to be run. (Verified by
        // deliberately corrupting one D3D10DDIARG_CREATEDEVICE offset and
        // confirming the build fails; REFACTOR_REVIEW.md R802 predicts the
        // older `#[test] fn bindgen_test_layout_*` form, which this bindgen
        // version no longer produces.)
        //
        // The cost is generated-file size (~1.1 MB, 43k lines) and a slower
        // cold build, which is why they were originally off. That is worth
        // paying for a driver whose ABI is defined by someone else's headers.
        .layout_tests(true)
        .derive_default(true)
        .generate_comments(false)
        .generate()
        .expect("bindgen failed to generate d3d10umddi bindings");

    bindings
        .write_to_file(out.join("d3d10umddi.rs"))
        .expect("failed to write d3d10umddi.rs");

    println!("cargo:rerun-if-changed=bindgen/d3d10umddi_wrapper.h");
    println!("cargo:rerun-if-env-changed=HELIOS_WDK_INCLUDE");
    // Same reason as HELIOS_WDK_INCLUDE: these bindings are generated against
    // this include path, so changing the selection must regenerate them.
    println!("cargo:rerun-if-env-changed=HELIOS_MSVC_INCLUDE");
}

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        // The crate is Windows-only. This guard exists so the build script does
        // not go looking for clang-cl or the DXVK archives on a non-Windows
        // host; it is NOT a path to a working host build. `src/lib.rs`'s
        // `#[cfg(not(windows))] compile_error!` is what reports that, and it
        // keys off the same target this branch tests. The WDK headers are not
        // present on the host, so `src/ddi.rs`'s bindgen output cannot be
        // generated there and a cfg-gated build would type-check nothing useful.
        println!("cargo:warning=helios_umd: skipping DXVK bridge on non-Windows target");
        return;
    }

    let dxvk_src = def("HELIOS_DXVK_SRC", r"C:\Users\Rupansh\dxvk-helios");
    let dxvk_build = def("HELIOS_DXVK_BUILD", r"C:\Users\Rupansh\dxvk-build");
    let clang_cl = def("HELIOS_CLANG_CL", r"C:\Program Files\LLVM\bin\clang-cl.exe");
    let archiver = def("HELIOS_MSVC_LIB", r"C:\Program Files\LLVM\bin\llvm-lib.exe");

    // The module doc above calls the C++ ABI / CRT agreement critical, and then
    // the build declared no dependency on the compiler that decides it. `cc` and
    // `cxx-build` do not add a rerun edge for a compiler supplied via
    // `.compiler()`, so swapping HELIOS_CLANG_CL or HELIOS_MSVC_LIB left the
    // previously built helios_dxvk_bridge.lib — compiled against the previous
    // MSVC STL — to be relinked against freshly built DXVK archives (which DO
    // have rerun-if-changed), giving mismatched std::string / std::mutex layouts
    // across the cxx boundary inside one DLL. That is heap corruption at
    // runtime, guarded by prose. Declaring the identity as a build input turns a
    // changed *selection* into a rebuild.
    //
    // It does NOT catch an in-place LLVM upgrade; a generated toolchain
    // fingerprint (resolved `clang-cl --version` + MSVC include dir, with
    // rerun-if-changed on it) is the stronger fix and is a separate follow-up.
    println!("cargo:rerun-if-env-changed=HELIOS_CLANG_CL");
    println!("cargo:rerun-if-env-changed=HELIOS_MSVC_LIB");

    require_path("HELIOS_DXVK_SRC", &dxvk_src, true);
    require_path("HELIOS_DXVK_BUILD", &dxvk_build, true);
    require_path("HELIOS_CLANG_CL", &clang_cl, false);
    require_path("HELIOS_MSVC_LIB", &archiver, false);

    generate_d3d10umddi_bindings();

    // --- Compile the cxx bridge shim with clang-cl (matches DXVK's ABI) -------
    let mut build = cxx_build::bridge("src/bridge.rs");
    build
        .file("bridge/dxvk_bridge.cpp")
        // T8/R1105: extra TUs inherit every include and define from this same
        // cc::Build, so there is no flag duplication to drift.
        .file("bridge/bridge_dxbc.cpp")
        .file("bridge/bridge_icd_exports.cpp")
        .compiler(&clang_cl)
        .archiver(&archiver)
        .std("c++17")
        // DXVK (and our shim) use C++ exceptions; cxx-build disables them by default.
        .flag("/EHsc")
        .include("bridge")
        .include(format!(r"{dxvk_src}\src"))
        .include(format!(r"{dxvk_src}\src\dxvk"))
        .include(format!(r"{dxvk_src}\src\d3d11"))
        .include(format!(r"{dxvk_src}\subprojects\dxbc-spirv"))
        .include(format!(r"{dxvk_src}\include"))
        .include(format!(r"{dxvk_src}\include\vulkan\include"))
        .include(format!(r"{dxvk_src}\include\spirv\include"))
        // Generated headers (version.h / buildenv.h) live at the meson build root.
        .include(&dxvk_build)
        // Suppresses the MSVC STL's own #error when the clang-cl version falls
        // outside the STL's supported-compiler window. Deliberately accepted:
        // removing it hard-fails the only working build. It is a runtime-risk
        // acknowledgement, not a fix — the ABI still rests on the two objects
        // agreeing, which nothing here can prove.
        .define("_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH", None)
        .define("NOMINMAX", None)
        .define("WIN32_LEAN_AND_MEAN", None)
        .define("_WIN32_WINNT", "0x0A00")
        .define("_CRT_SECURE_NO_WARNINGS", None);
    build.compile("helios_dxvk_bridge");

    // --- Link the prebuilt DXVK static libraries -----------------------------
    // These are MS-format COFF archives (meson archiver = lib.exe) with a `.a`
    // name, so we pass full paths as link args rather than relying on Rust's
    // `static=NAME` -> `NAME.lib` name resolution.
    let libs = [
        // DXVK's full D3D11 COM implementation (must precede libdxvk so its
        // engine references resolve against the dxvk archive).
        format!(r"{dxvk_build}\src\d3d11\libhelios_d3d11_static.a"),
        format!(r"{dxvk_build}\src\dxvk\libdxvk.a"),
        format!(r"{dxvk_build}\subprojects\dxbc-spirv\libdxbc_spv.a"),
        format!(r"{dxvk_build}\src\spirv\libspirv.a"),
        format!(r"{dxvk_build}\src\util\libutil.a"),
        format!(r"{dxvk_build}\src\wsi\libwsi.a"),
        format!(r"{dxvk_build}\src\vulkan\libvkcommon.a"),
        format!(r"{dxvk_build}\subprojects\libdisplay-info\libdisplay-info.a"),
    ];
    for p in &libs {
        println!("cargo:rustc-link-arg-cdylib={p}");
        println!("cargo:rerun-if-changed={p}");
    }

    // System libraries DXVK's engine/WSI depend on.
    // NOTE: deliberately NOT linking system dxgi. A WDDM UMD sits below DXGI and
    // implements the DXGI DDI; it must not depend on dxgi.dll. DXVK's only
    // dxgi.dll call (CreateDXGIFactory1) is in d3d11_main.cpp's exported d3d11.dll
    // entry points, which we never reference (we build D3D11DXGIDevice directly),
    // so that object is never pulled out of the static archive.
    for lib in [
        "setupapi", "gdi32", "user32", "ole32", "oleaut32", "version", "advapi32", "shell32",
        "cfgmgr32",
    ] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }

    println!("cargo:rerun-if-changed=bridge/dxvk_bridge.cpp");
    println!("cargo:rerun-if-changed=bridge/dxvk_bridge.h");
    println!("cargo:rerun-if-changed=src/bridge.rs");
    println!("cargo:rerun-if-env-changed=HELIOS_DXVK_SRC");
    println!("cargo:rerun-if-env-changed=HELIOS_DXVK_BUILD");
}
