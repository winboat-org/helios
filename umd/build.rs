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

fn def(var: &str, default: &str) -> String {
    env::var(var).unwrap_or_else(|_| default.to_string())
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
