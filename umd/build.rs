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
        .unwrap_or_else(|| format!(r"{}\14.44.35207\include", root.display()))
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
        // Keep the generated module lean and stable across runs.
        .layout_tests(false)
        .derive_default(true)
        .generate_comments(false)
        .generate()
        .expect("bindgen failed to generate d3d10umddi bindings");

    bindings
        .write_to_file(out.join("d3d10umddi.rs"))
        .expect("failed to write d3d10umddi.rs");

    println!("cargo:rerun-if-changed=bindgen/d3d10umddi_wrapper.h");
    println!("cargo:rerun-if-env-changed=HELIOS_WDK_INCLUDE");
}

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        // The UMD is Windows-only; allow `cargo check` on Linux to no-op so the
        // crate doesn't pull clang-cl / DXVK on the host.
        println!("cargo:warning=helios_umd: skipping DXVK bridge on non-Windows target");
        return;
    }

    let dxvk_src = def("HELIOS_DXVK_SRC", r"C:\Users\Rupansh\dxvk-helios");
    let dxvk_build = def("HELIOS_DXVK_BUILD", r"C:\Users\Rupansh\dxvk-build");
    let clang_cl = def("HELIOS_CLANG_CL", r"C:\Program Files\LLVM\bin\clang-cl.exe");

    generate_d3d10umddi_bindings();

    // --- Compile the cxx bridge shim with clang-cl (matches DXVK's ABI) -------
    let mut build = cxx_build::bridge("src/bridge.rs");
    build
        .file("bridge/dxvk_bridge.cpp")
        .compiler(&clang_cl)
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
        "setupapi", "gdi32", "user32", "ole32", "oleaut32", "version", "advapi32",
        "shell32", "cfgmgr32",
    ] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }

    println!("cargo:rerun-if-changed=bridge/dxvk_bridge.cpp");
    println!("cargo:rerun-if-changed=bridge/dxvk_bridge.h");
    println!("cargo:rerun-if-changed=src/bridge.rs");
    println!("cargo:rerun-if-env-changed=HELIOS_DXVK_SRC");
    println!("cargo:rerun-if-env-changed=HELIOS_DXVK_BUILD");
}
