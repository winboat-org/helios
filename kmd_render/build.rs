//! Build script for the Helios KMD.
//!
//! Two jobs:
//!   1. Generate Rust bindings for the WDDM display-miniport DDIs. `wdk-sys`
//!      only generates a fixed set of `ApiSubset`s (Base/Wdf/Gpio/Hid/...) and
//!      has NO display subset and no "add a header" API, so the entire
//!      `dispmprt.h` / `d3dkmddi.h` surface (DRIVER_INITIALIZATION_DATA,
//!      DXGKRNL_INTERFACE, every DXGKARG_*, DxgkInitialize, ...) is missing.
//!      We run our own bindgen over those headers, reusing the clang args /
//!      include paths that `wdk-build` computes via `Builder::wdk_default`.
//!   2. Configure downstream linking against the WDK (`configure_binary_build`).
//!
//! The generated file lands at `$OUT_DIR/dxgk_bindings.rs` and is pulled in by
//! `src/dxgk.rs`.
//!
//! NOTE (first-build tuning): bindgen pulls in every base type the display DDIs
//! transitively reference (DEVICE_OBJECT, LARGE_INTEGER, ...). We blocklist the
//! common ones and redirect them to `wdk_sys` via a `use` prelude so there is a
//! single canonical definition. The blocklist below is a starting set; the first
//! real build against the installed WDK will surface any remaining duplicate-
//! definition conflicts, which get added here. This is expected for layered
//! bindgen and is not a design problem.

use wdk_build::{BuilderExt, Config};

/// Headers that declare the WDDM render-path DDIs we implement.
///
/// `DXGKDDI_INTERFACE_VERSION` is left at the header default (WDDM 3.2 on the
/// 26100 WDK) so every generated struct matches the buffers 24H2 dxgkrnl hands a
/// native driver. We declare the same version in `DriverEntry` (consistent → no
/// BUFFER_TOO_SMALL). An earlier experiment pinned this to WDDM 2.0 (0x5023) to
/// shrink the cap surface, but 24H2 then rejects the UMD revision
/// (STATUS_REVISION_MISMATCH) — a 2.0 adapter is too old for the OS's user-mode
/// driver, so we stay OS-native.
const DXGK_HEADER_CONTENTS: &str = r#"
#include <ntddk.h>
#include <dispmprt.h>
#include <d3dkmddi.h>
"#;

/// Base types that `wdk-sys` already defines. We blocklist them here and import
/// `wdk_sys::*` so DDI code uses one canonical set instead of two incompatible
/// copies. Extend this list as the first build reports conflicts.
const BLOCKLISTED_BASE_TYPES: &[&str] = &[
    "_?DEVICE_OBJECT",
    "_?DRIVER_OBJECT",
    "_?UNICODE_STRING",
    "_?LARGE_INTEGER",
    "_?ULARGE_INTEGER",
    "_?LIST_ENTRY",
    "_?SINGLE_LIST_ENTRY",
    "_?PHYSICAL_ADDRESS",
    "_?GUID",
    "_?KEVENT",
    "_?DISPATCHER_HEADER",
    "_?IRP",
    "_?IO_STACK_LOCATION",
    "_?KDPC",
    "_?KSPIN_LOCK",
    "NTSTATUS",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    generate_dxgk_bindings()?;
    compile_version_resource()?;
    compile_seh_shim();

    // Emit the link configuration for a WDK binary (resolves ntoskrnl, etc.).
    Config::from_env_auto()?.configure_binary_build()?;

    // displib.lib provides DxgkInitialize — the WDDM display-miniport entry that
    // registers our DDI table. wdk-build links the base kernel libs (ntoskrnl,
    // hal, wmilib, ...) but not the display-miniport import lib, so add it here.
    // Its directory is already on the linker search path (km\<ver>\x64).
    println!("cargo:rustc-link-lib=static=displib");
    Ok(())
}

/// Compile the SEH shim for `MmMapLockedPagesSpecifyCache(UserMode)` (which
/// raises on failure — un-catchable from no_std Rust). Kernel-appropriate
/// flags: `/Zl` omits default-CRT lib records from the object (rustc drives
/// the kernel link; msvcrt must not be pulled in) and `/GS-` avoids
/// `__security_cookie` references. The `__C_specific_handler` reference the
/// `__try/__except` emits resolves from ntoskrnl.lib, already on the link.
fn compile_seh_shim() {
    cc::Build::new()
        .file("src/seh_shim.c")
        .flag("/Zl")
        .flag("/GS-")
        .compile("helios_seh_shim");
    println!("cargo:rerun-if-changed=src/seh_shim.c");
}

fn compile_version_resource() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    let rc_path = out_dir.join("helios_kmd_render_version.rc");
    let res_path = out_dir.join("helios_kmd_render_version.res");

    std::fs::write(
        &rc_path,
        r#"1 VERSIONINFO
FILEVERSION 22,22,52,0
PRODUCTVERSION 22,22,52,0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0
FILEOS 0x00040004L
FILETYPE 0x00000003L
FILESUBTYPE 0x00000004L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "Helios Project\0"
            VALUE "FileDescription", "Helios vGPU WDDM render miniport\0"
            VALUE "FileVersion", "22.22.52.0\0"
            VALUE "InternalName", "helios_kmd_render.sys\0"
            VALUE "OriginalFilename", "helios_kmd_render.sys\0"
            VALUE "ProductName", "Helios vGPU\0"
            VALUE "ProductVersion", "22.22.52.0\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#,
    )?;

    let rc = find_windows_sdk_tool("rc.exe");
    let status = std::process::Command::new(&rc)
        .arg("/nologo")
        .arg(format!("/fo{}", res_path.display()))
        .arg(&rc_path)
        .status()?;
    if !status.success() {
        return Err(format!("{} failed with {status}", rc.display()).into());
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-link-arg={}", res_path.display());
    Ok(())
}

fn find_windows_sdk_tool(name: &str) -> std::path::PathBuf {
    if let (Ok(sdk_dir), Ok(sdk_ver)) = (
        std::env::var("WindowsSdkDir"),
        std::env::var("WindowsSDKVersion"),
    ) {
        let candidate = std::path::Path::new(&sdk_dir)
            .join("bin")
            .join(sdk_ver.trim_end_matches('\\'))
            .join("x64")
            .join(name);
        if candidate.exists() {
            return candidate;
        }
    }

    let candidate =
        std::path::Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64")
            .join(name);
    if candidate.exists() {
        return candidate;
    }

    name.into()
}

fn generate_dxgk_bindings() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::env::var("OUT_DIR")?;
    let out_path = std::path::Path::new(&out_dir).join("dxgk_bindings.rs");

    // `wdk_default` seeds the builder with the correct target triple, kernel-mode
    // defines (_KERNEL_MODE, _AMD64_, ...) and WDK/SDK include paths.
    let mut builder = bindgen::Builder::wdk_default(Config::from_env_auto()?)?
        .header_contents("helios-dxgk-input.h", DXGK_HEADER_CONTENTS)
        // Generate only the display surface; base types come from wdk_sys.
        .allowlist_type("DXGK.*")
        .allowlist_type("_?DRIVER_INITIALIZATION_DATA")
        .allowlist_type("D3DKMT_.*")
        .allowlist_type("D3DDDI_.*")
        .allowlist_type("DXGK_.*")
        .allowlist_function("Dxgk.*")
        .allowlist_var("DXGK.*")
        .allowlist_var("D3DKMDT_.*")
        .allowlist_var("KMT_.*")
        // Re-export the base types from wdk_sys so blocklisted references resolve.
        // (The allow(...) lints are applied as an outer attribute on the `bindings`
        // module in src/dxgk.rs — an inner attribute here is illegal under include!.)
        .raw_line("pub use wdk_sys::*;");

    for ty in BLOCKLISTED_BASE_TYPES {
        builder = builder.blocklist_type(ty);
    }

    builder.generate()?.write_to_file(&out_path)?;

    Ok(())
}
