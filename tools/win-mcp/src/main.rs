//! win-mcp — a local stdio MCP server that runs commands and cargo/cargo-make
//! builds on the Helios `win11` dev VM over SSH.
//!
//! Why this exists: raw `ssh win "..."` from the agent suffers from cmd.exe
//! quoting hell and stale-ControlMaster environments. This server wraps the VM
//! with two clean tools and ships commands as base64-UTF16LE PowerShell
//! (`powershell -EncodedCommand`), which is immune to shell quoting and
//! propagates the real exit code.
//!
//! Runs on Linux (the agent's host); the win11 project tree is shared at `Z:\`,
//! so no file tools are needed — the agent edits files directly on the Linux
//! side of the share.

use std::collections::HashMap;
use std::future::Future; // referenced by the #[tool] macro expansion
use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;
use base64::Engine as _;
use rmcp::{
    handler::server::{router::tool::ToolRouter, tool::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt,
};
use serde::Deserialize;
use tokio::process::Command;

mod cli;
mod host;

/// Shared project root on the Windows side (the Z: drive maps the Linux tree).
const PROJECT_DRIVE: &str = "Z:\\";
/// The same tree on the Linux side (where this server runs). Used by tools that
/// edit sources before a build (e.g. the KMD version bump); the robocopy mirror
/// then carries the edits to the Windows build.
///
/// ⛔⛔ **This MUST name the same directory the launcher exports as `Z:\`**
/// (`tools/launch-helios-gtk.sh`'s `HELIOS_SHARE`, which defaults to the repo
/// root). It is the ONE constant here that is not a Windows-side path, so it is
/// the one that silently goes wrong when the tree moves — every other tool in
/// this server addresses the source through `PROJECT_DRIVE`, which follows the
/// share automatically.
///
/// ⚠ It HAS gone wrong: on 2026-09-05 this read `/home/rupansh/helios-vgpu`
/// while `Z:\` was `/home/rupansh/helios-vgpu-dx12`, and `win_build_kmd`
/// reported "KMD version: 22.22.501.0" for a tree whose
/// `driver-version.env` says `22.22.257.0`. `no_bump` made that a false
/// REPORT rather than a false BUILD — the package still stamped the mirrored
/// (correct) version — but a real bump would have edited the *other* tree's
/// file and left this one untouched, i.e. an INF DriverVer that never moved.
///
/// ⇒ overridable by `HELIOS_LINUX_PROJECT_ROOT` so a relocated tree is one env
/// var rather than a recompile, and so this literal cannot be the only thing
/// standing between two checkouts.
fn linux_project_root() -> String {
    std::env::var("HELIOS_LINUX_PROJECT_ROOT")
        .unwrap_or_else(|_| "/home/rupansh/helios-vgpu-dx12".to_string())
}
/// Local build mirror. cargo/wdk build IO fails on the Z:\ 9p share (OS error 87,
/// see windows-drivers-rs#481), so win_cargo robocopy-syncs here and builds on
/// local disk. Edit sources on Linux/Z:\; the mirror is re-synced each build.
const MIRROR_ROOT: &str = "C:\\Users\\Rupansh\\helios-vgpu";
/// libclang location for bindgen (set as LIBCLANG_PATH for cargo builds).
const LIBCLANG_PATH: &str = "C:\\Program Files\\LLVM\\bin";
/// SSH host alias for the dev VM (from ~/.ssh/config).
const SSH_HOST: &str = "win";

/// Mesa venus ICD source — the vendored submodule, on the share. meson/ninja/cl
/// read source straight from here: building Mesa does NOT need the robocopy mirror
/// (validated — meson configures and cl compiles from Z:\ to a local C: build dir;
/// the 9p share is fine for the compiler's READS, unlike cargo/wdk artifact writes).
/// So `win_cargo` EXCLUDES this path from its mirror and Mesa is built via `win_meson`.
const MESA_SRC: &str = "Z:\\icd\\mesa";
/// Local build dir for the Mesa venus ICD (ninja writes artifacts to local disk).
const MESA_BUILD: &str = "C:\\Users\\Rupansh\\helios-mesa-build";
/// VS 2022 x64 dev environment (vcvars). Provides the MSVC archiver/linker
/// (`lib.exe`/`link.exe`) + SDK libs the clang-cl DXVK build links against
/// (icd/win-build/clang-cl-native.ini). Used by `win_dxvk`.
const VCVARS: &str =
    "C:\\Program Files\\Microsoft Visual Studio\\2022\\Community\\VC\\Auxiliary\\Build\\vcvars64.bat";
/// LLVM bin (clang-cl) — the DXVK C++ engine compiles with clang-cl (MSVC ABI).
/// Same directory as LIBCLANG_PATH; named separately for the `win_dxvk` toolchain.
const LLVM_BIN: &str = "C:\\Program Files\\LLVM\\bin";
/// DXVK-helios C++ engine source on the share. The meson build reads a LOCAL git
/// checkout (DXVK_MIRROR), NOT Z:\ directly, so `win_dxvk` robocopy-mirrors the
/// source first, then builds into DXVK_BUILD. The Rust UMD (`win_cargo
/// crate_dir:"umd"`) links the resulting prebuilt `.a` archives.
const DXVK_SRC: &str = "Z:\\dxvk-helios";
const DXVK_MIRROR: &str = "C:\\Users\\Rupansh\\dxvk-helios";
const DXVK_BUILD: &str = "C:\\Users\\Rupansh\\dxvk-build";
/// The meson native file that pins DXVK's compiler to clang-cl/lld-link, and the
/// C forced-include the engine's C sources need. Both are read from `Z:\`
/// deliberately: they live OUTSIDE the `dxvk-helios` subtree, so `DXVK_MIRROR`
/// does not contain them, and `MIRROR_ROOT` is not guaranteed to exist when only
/// the engine is being built. Reading a header off the share is the same thing
/// the Mesa build does for its whole source tree.
const DXVK_NATIVE_FILE: &str = "Z:\\ci\\windows\\clang-cl-native.ini";
const DXVK_C_COMPAT_HEADER: &str = "Z:\\umd\\build-support\\dxvk_c_compat.h";
/// ⛔ **`-Db_vscrt=mt` — the STATIC CRT, and it is load-bearing.**
/// `umd/build.rs`'s header states the coherence rule: DXVK, the cxx shim and the
/// Rust crate must all use the MSVC C++ ABI with the **static** CRT, because a
/// display UMD is loaded into arbitrary application directories and `/MD` lets an
/// app's own MSVCP/VCRUNTIME DLL override the toolset the driver was compiled
/// against. `umd/.cargo/config.toml` sets `crt-static` for the Rust half and
/// `ci/windows/Build-Driver.ps1` asserts the shipped DLL imports no dynamic CRT.
///
/// vkd3d/`umd12` follow the SAME rule (`-Db_vscrt=mt` + `umd12`'s `crt-static`),
/// so both engines' archives match the Rust crate that links them and the
/// installer never needs the VC++ redistributables.
///
/// The flag set is otherwise `ci/windows/Build-Driver.ps1`'s, which is the
/// reference build; `_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH` carries the same
/// caveat recorded there and in `win_vkd3d`.
const DXVK_CPP_ARGS: &str = "/D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH \
     -Wno-deprecated-declarations -Wno-delete-non-abstract-non-virtual-dtor \
     -Wno-unused-private-field -Wno-unused-lambda-capture -Wno-c++20-extensions \
     -Wno-unused-const-variable";

/// vkd3d-proton, built exactly like DXVK: clang-cl + MSVC ABI + `-Db_vscrt=mt`,
/// into static archives that `helios_umd12.dll` links directly.
///
/// ⛔ **Static, not a DLL** — owner decision 2026-08-05 (`DECISIONS.md` D4):
/// *"we are going to statically link vkd3d-proton and not mess with dynamic
/// dlls."* The `helios_vkd3d.dll` shared target is retired and retained only so
/// `D12-G1` stays reproducible.
///
/// Same mirror-then-build shape as DXVK and for the same reason: meson/ninja
/// build IO must not run on the `Z:\` 9p share, and the build dir is kept out of
/// the source tree.
const VKD3D_SRC: &str = "Z:\\vkd3d-proton-helios";
const VKD3D_MIRROR: &str = "C:\\Users\\Rupansh\\vkd3d-proton-helios";
const VKD3D_BUILD: &str = "C:\\Users\\Rupansh\\vkd3d-build";
/// mingw-w64 (WinLibs UCRT gcc 16.1) bin dir — the RECOMMENDED venus toolchain
/// (icd/win-build/mingw-native.ini). gcc compiles venus's GNU-isms natively and
/// builds straight from Z:\. Installed via `winget install BrechtSanders.WinLibs.POSIX.UCRT`.
const MINGW_BIN: &str = "C:\\Users\\Rupansh\\AppData\\Local\\Microsoft\\WinGet\\Packages\\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\\mingw64\\bin";
/// MSBuild used for Looking Glass IDD's WDK/Visual Studio solution.
const MSBUILD: &str =
    "C:\\Program Files\\Microsoft Visual Studio\\2022\\Community\\Msbuild\\Current\\Bin\\MSBuild.exe";

/// The signed KMD driver package `cargo make --makefile Cargo.make.toml` emits.
///
/// This and [`DEFAULT_UMD_DLL`] are the two artifacts a deploy actually ships,
/// and both install tools now pass them to their scripts EXPLICITLY and echo
/// them in the output — see the note on [`WinInstallKmdArgs::package_dir`] for
/// what that prevents. `debug` because cargo-make defaults
/// CARGO_MAKE_CARGO_PROFILE to `dev`; the KMD ships from the dev profile.
const DEFAULT_KMD_PACKAGE_DIR: &str =
    "C:\\Users\\Rupansh\\helios-vgpu\\kmd_render\\target\\debug\\helios_kmd_render_package";
/// The UMD binary a deploy ships. RELEASE, deliberately: the PSC stage measures
/// present-gate timing and the debug profile is opt-level 1 with no LTO, so a
/// debug deploy silently makes every cadence and wake-latency number a
/// measurement of the wrong binary (41st/43rd-session directive).
const DEFAULT_UMD_DLL: &str =
    "C:\\Users\\Rupansh\\helios-vgpu\\umd\\target\\release\\helios_umd.dll";
/// The D3D12 UMD's build path — the value to pass as `umd12_dll` when you want
/// it deployed.
///
/// ⛔ This is a SUGGESTED path, not a default: `win_install_umd` deploys the
/// D3D12 UMD only when `umd12_dll` is passed explicitly. Deploying it rewrites
/// `UserModeDriverName[3]`, a REG_MULTI_SZ dwm resolves at device start, so a
/// routine D3D11 deploy must not be able to acquire a D3D12 path by accident.
/// (`DECISIONS.md` D3; the real slot-3 wiring is stage S5 in
/// `ARCHITECTURE.md` §11, which also lands the `UmdD3D12` kill switch.)
const SUGGESTED_UMD12_DLL: &str =
    "C:\\Users\\Rupansh\\helios-vgpu\\umd12\\target\\release\\helios_umd12.dll";

/// Render one artifact path for the tool's echo line, flagging a non-default.
fn artifact_line(label: &str, value: &str, default: &str) -> String {
    if value == default {
        format!("{label}: {value}\n")
    } else {
        format!("{label}: {value}   (NON-DEFAULT)\n")
    }
}

#[derive(Clone)]
pub struct WinHost {
    tool_router: ToolRouter<WinHost>,
}

#[derive(Debug, Clone)]
pub(crate) struct ExecOutput {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) code: Option<i32>,
    pub(crate) timed_out: bool,
}

/// Encode a PowerShell script as base64 of its UTF-16LE bytes, the input format
/// for `powershell -EncodedCommand`. This avoids all shell quoting concerns.
pub(crate) fn encode_powershell(script: &str) -> String {
    let utf16le: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(utf16le)
}

/// Escape a string for a PowerShell single-quoted literal (double the quotes).
pub(crate) fn ps_single_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// Keep the last `max` bytes of `s` (on a char boundary), noting truncation.
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…[{} earlier bytes truncated]…\n{}", start, &s[start..])
}

/// Drop SSH-client banners (the post-quantum warning) and any stray CLIXML
/// artifacts so build errors aren't buried in noise.
pub(crate) fn clean_stderr(s: &str) -> String {
    s.lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("** WARNING")
                || t.contains("post-quantum")
                || t.contains("store now, decrypt later")
                || t.contains("openssh.com/pq")
                || t.contains("may need to be upgraded")
                || t.starts_with("#< CLIXML")
                || t.starts_with("<Objs"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse repeated `K=V` tool arguments.
fn parse_env(raw: &Option<Vec<String>>) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for kv in raw.iter().flatten() {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("env entry '{kv}' is not K=V"))?;
        out.insert(k.to_string(), v.to_string());
    }
    Ok(out)
}

fn format_output(o: &ExecOutput) -> String {
    const CAP: usize = 60_000;
    let code = if o.timed_out {
        "TIMEOUT".to_string()
    } else {
        o.code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    };
    format!(
        "exit_code: {code}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        tail(&o.stdout, CAP),
        tail(&o.stderr, CAP),
    )
}

/// Run a PowerShell command on the VM and capture stdout/stderr/exit code.
async fn run_ssh(
    command: &str,
    cwd: Option<&str>,
    env: &HashMap<String, String>,
    timeout_secs: u64,
) -> Result<ExecOutput> {
    // Build a PowerShell script: set env, cd, run, then propagate the exit code.
    // SilentlyContinue on progress stops PowerShell from CLIXML-serializing its
    // "Preparing modules…" progress records into our captured stderr.
    let mut script = String::from(
        "$ProgressPreference = 'SilentlyContinue';\n$ErrorActionPreference = 'Continue';\n",
    );
    for (k, v) in env {
        script.push_str(&format!("$env:{k} = '{}';\n", ps_single_quote(v)));
    }
    let dir = cwd.unwrap_or(PROJECT_DRIVE);
    script.push_str(&format!(
        "Set-Location -LiteralPath '{}';\n",
        ps_single_quote(dir)
    ));
    script.push_str(command);
    // For native programs $LASTEXITCODE holds the code; cmdlets leave it null.
    script.push_str("\n$c = $LASTEXITCODE; if ($null -eq $c) { $c = 0 }; exit $c\n");

    let encoded = encode_powershell(&script);

    let mut cmd = Command::new("ssh");
    cmd.arg(SSH_HOST)
        .arg("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-EncodedCommand")
        .arg(&encoded)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = cmd.spawn()?;
    match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await {
        Ok(out) => {
            let out = out?;
            Ok(ExecOutput {
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: clean_stderr(&String::from_utf8_lossy(&out.stderr)),
                code: out.status.code(),
                timed_out: false,
            })
        }
        // Timed out: the wait_with_output future is dropped, and kill_on_drop
        // terminates the ssh child (and thus the remote powershell).
        Err(_) => Ok(ExecOutput {
            stdout: String::new(),
            stderr: format!("command exceeded timeout of {timeout_secs}s and was killed"),
            code: None,
            timed_out: true,
        }),
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WinExecArgs {
    /// PowerShell command/script to run on the Windows 11 dev VM (win11).
    command: String,
    /// Working directory on Windows. Defaults to Z:\ (the shared project root).
    #[serde(default)]
    cwd: Option<String>,
    /// Extra environment variables to set before running the command.
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    /// Timeout in seconds. Defaults to 600.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WinCargoArgs {
    /// Crate directory relative to the project root, e.g. "kmd_render" or
    /// "umd". The working directory becomes Z:\<crate_dir>. The active Rust
    /// crates are kmd_render, umd, protocol and kmd_logic — "kmd" is the
    /// ARCHIVED System-class driver and "icd" is a meson/C project built by
    /// win_meson, not by cargo.
    crate_dir: String,
    /// Arguments passed to cargo, e.g. ["make","--makefile","Cargo.make.toml"]
    /// or ["build","--release"].
    args: Vec<String>,
    /// Timeout in seconds. Defaults to 1800 (driver builds are slow).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WinMesonArgs {
    /// meson argv to run under the VS dev environment. Examples:
    ///   ["setup", "C:\\Users\\Rupansh\\helios-mesa-build", "Z:\\icd\\mesa",
    ///    "-Dvulkan-drivers=virtio", "-Dgallium-drivers=", "-Dplatforms=windows", ...]
    ///   ["compile", "-C", "C:\\Users\\Rupansh\\helios-mesa-build"]
    /// Empty defaults to `compile -C <the standard Mesa build dir>`. Args must be
    /// space-free or pre-quoted (they are joined verbatim).
    #[serde(default)]
    args: Vec<String>,
    /// Timeout in seconds. Defaults to 1800 (mesa builds are slow).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WinLookingGlassArgs {
    /// Source root on the Windows VM. Defaults to Z:\. Override if the shared
    /// tree is exposed at another path.
    #[serde(default)]
    source_root: Option<String>,
    /// CMake configure arguments. If empty, the tool configures LookingGlass/host
    /// with the default Ninja + RelWithDebInfo + USE_NVFBC=OFF settings.
    #[serde(default)]
    configure_args: Vec<String>,
    /// CMake build arguments after `cmake --build <build_dir>`. If empty, builds
    /// the default target.
    #[serde(default)]
    build_args: Vec<String>,
    /// Build directory on the Windows VM. Defaults to
    /// C:\Users\Rupansh\helios-lookingglass-host-build.
    #[serde(default)]
    build_dir: Option<String>,
    /// If true, skip CMake configure and only run the build step.
    #[serde(default)]
    no_configure: bool,
    /// Timeout in seconds. Defaults to 1800.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WinLookingGlassIddArgs {
    /// Source root on the Windows VM. Defaults to Z:\. Override if the shared
    /// tree is exposed at another path.
    #[serde(default)]
    source_root: Option<String>,
    /// MSBuild arguments after the solution path. If empty, builds Release|x64
    /// with RunInfVerif=false.
    #[serde(default)]
    msbuild_args: Vec<String>,
    /// MSBuild path. Defaults to the VS 2022 Community MSBuild installation.
    #[serde(default)]
    msbuild_path: Option<String>,
    /// If true, only sync the local mirror and Mesa Vulkan headers; do not build.
    #[serde(default)]
    sync_only: bool,
    /// Timeout in seconds. Defaults to 1800.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WinDxvkArgs {
    /// meson argv run in the DXVK build dir. Empty defaults to
    /// `compile -C C:\Users\Rupansh\dxvk-build` (the common case: rebuild after a
    /// source edit). Reconfiguration is rare (the build dir is already set up with
    /// the clang-cl native file); pass a full `["setup", ...]` argv only if needed.
    #[serde(default)]
    args: Vec<String>,
    /// Skip the Z:\dxvk-helios -> local-checkout source mirror and build only.
    /// Default false (always mirror first, so share edits are picked up).
    #[serde(default)]
    no_sync: bool,
    /// Timeout in seconds. Defaults to 1800 (a header change recompiles many TUs).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WinVkd3dArgs {
    /// meson argv run for the vkd3d build. **Empty is the normal case** and does
    /// the right thing: it configures the build dir with the canonical clang-cl
    /// setup if it is not configured yet, then compiles. Pass an explicit argv
    /// (e.g. `["setup","--reconfigure", ...]`) only to override.
    #[serde(default)]
    args: Vec<String>,
    /// Skip the Z:\vkd3d-proton-helios -> local-checkout source mirror and build
    /// only. Default false (always mirror first, so share edits are picked up).
    #[serde(default)]
    no_sync: bool,
    /// Force `meson setup --wipe` before building. Use after changing a meson
    /// option or the compiler; a plain reconfigure does not always pick those up.
    #[serde(default)]
    wipe: bool,
    /// Timeout in seconds. Defaults to 3600 — vkd3d is seven repositories and a
    /// cold clang-cl build of dxil-spirv + SPIRV-Tools is slow.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WinInstallUmdArgs {
    /// The UMD binary to deploy. Defaults to the RELEASE build
    /// (umd\target\release\helios_umd.dll) — see `DEFAULT_UMD_DLL` for why a
    /// debug deploy invalidates every timing number. Always passed to the script
    /// as `-UmdDll` and echoed in the output, so which binary shipped is a fact
    /// in the transcript rather than an inherited script default.
    #[serde(default)]
    umd_dll: Option<String>,
    /// The D3D12 UMD binary (`helios_umd12.dll`) to deploy alongside the D3D11
    /// one, taking `UserModeDriverName` slot 3 (`DECISIONS.md` D3).
    ///
    /// ⛔ OMIT IT and no D3D12 UMD is deployed — the registry writes are then
    /// bit-identical to a pre-D3D12 deploy. There is deliberately NO default,
    /// because deploying it changes what dwm resolves at device start. Pass
    /// `SUGGESTED_UMD12_DLL` (umd12\target\release\helios_umd12.dll) to opt in.
    ///
    /// Build it first: `win_cargo crate_dir:"umd12" args:["build","--release"]`,
    /// or both UMDs at once via `tools\umd-check.ps1 -Mode release`.
    ///
    /// ⚠ ProgramData mode only, and since 2026-09-05 that is a HOTPLUG
    /// convenience rather than the way D3D12 ships: stage S5's INF change has
    /// landed, so the signed DriverStore package now carries helios_umd12.dll
    /// and registers `UserModeDriverName[3]` itself — that copy survives a cold
    /// boot, this override does not. Use `win_install_kmd` (which requires
    /// `umd12_dll`) for a real deploy; use this to swap the DLL without
    /// reinstalling the package.
    #[serde(default)]
    umd12_dll: Option<String>,
    /// The KMD package dir the DriverStore/PackageUpgrade modes read. Defaults to
    /// `DEFAULT_KMD_PACKAGE_DIR`. Irrelevant to the default ProgramData mode, but
    /// passed and echoed anyway so the two install tools report the same pair.
    #[serde(default)]
    package_dir: Option<String>,
    /// Flags passed through to `tools\hotplug-helios-umd.ps1`. Empty = the script's
    /// default ProgramData hotplug (leaves the adapter running; new D3D processes
    /// pick up the DLL, existing ones keep the old one until the device restarts).
    /// Common sets: ["-PlanOnly"] (dry run); ["-KillUmdUsers","-RestartDevice",
    /// "-NoProbe"] (force existing UMD users + an adapter restart to load the new
    /// DLL now). Do NOT pass -UmdDll/-PackageDir here — use the fields above; a
    /// duplicate named argument is a PowerShell parameter-binding error.
    /// See HELIOS_DRIVER_DEPLOYMENT.md.
    #[serde(default)]
    args: Vec<String>,
    /// Timeout in seconds. Defaults to 600.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WinBuildKmdArgs {
    /// Explicit new version as "a.b.c.d" (e.g. "22.22.52.0"). Default: bump the
    /// third component of the current version by one.
    #[serde(default)]
    version: Option<String>,
    /// Rebuild at the CURRENT version without bumping (e.g. after a failed
    /// build of an already-bumped tree). Coherence across the three sites is
    /// still verified.
    #[serde(default)]
    no_bump: bool,
    /// Extra build environment, including explicit HELIOS_DXVK_BUILD_X86 and
    /// HELIOS_VKD3D_BUILD_X86 directories for already-built x86 engines.
    /// LIBCLANG_PATH defaults to the dev VM toolchain. CARGO_TARGET_DIR is
    /// always the KMD mirror's local target directory, overriding this map.
    /// Names are case insensitive; duplicate names with different casing fail.
    /// Install the i686 Rust target and PowerShell 7 before WoW64 packaging.
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    /// Timeout in seconds. Defaults to 1800.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WinInstallKmdArgs {
    /// Gracefully reboot the guest after a successful install (RECOMMENDED,
    /// default true): a new KMD image only loads at boot — without the reboot
    /// the device sits in CM_PROB_FAILED_POST_START limbo on the old image.
    /// Set false to install now and let the user reboot later.
    #[serde(default)]
    restart_vm: Option<bool>,
    /// ⛔ **REQUIRED, no default.** The signed driver package to publish —
    /// normally the one `win_build_kmd` writes,
    /// `kmd_render\target\debug\helios_kmd_render_package`.
    ///
    /// ⚠ WHY THERE IS NO DEFAULT (2026-09-05, superseding the 2026-07-27 R614
    /// note that made these merely *echoed*): these explicit paths are the
    /// artifacts a deploy actually ships, and echoing an inherited default is
    /// not the same as choosing it. Twice now a wrong binary reached the
    /// DriverStore because nobody had to name it — R614's Defender-blocked
    /// stale DEBUG `helios_umd.dll`, and on 2026-09-05 a DEBUG
    /// `helios_umd12.dll` that `cargo make` staged and the install script never
    /// refreshed. A default is an answer nobody had to think about; for the
    /// binaries that reach the DriverStore, the caller states them or the call
    /// is refused. The script enforces the same rule independently.
    package_dir: String,
    /// ⛔ **REQUIRED, no default.** The D3D11 UMD to publish alongside the KMD;
    /// use the RELEASE build (`umd\target\release\helios_umd.dll`) unless you
    /// are deliberately shipping a debug binary.
    ///
    /// This is load-bearing, not cosmetic: `install-helios-kmd.ps1`'s
    /// `Sync-HeliosPackageUmd` overwrites whatever cargo-make staged in the
    /// package with THIS file BEFORE the catalog is generated and signed — so
    /// this argument, not the packaging task, decides which UMD reaches the
    /// DriverStore.
    umd_dll: String,
    /// ⛔ **REQUIRED, no default.** The D3D12 UMD
    /// (`umd12\target\release\helios_umd12.dll`).
    ///
    /// ⭐ The INF's `CopyFiles` carries `helios_umd12.dll` and registers it at
    /// `UserModeDriverName` slot 3, so it ships in the DriverStore package like
    /// any other file — this is NOT the `win_install_umd` ProgramData override,
    /// and it DOES survive a cold boot.
    ///
    /// ⛔ It is synced into the package on the same terms as `umd_dll` and
    /// BEFORE the catalog is generated, which is the only moment it can be
    /// corrected: the DriverStore copy is catalog-signed, so overwriting the
    /// file afterwards breaks the signature instead of fixing the binary.
    umd12_dll: String,
    /// Explicit x86 D3D11 artifact, normally
    /// `umd\target\i686-pc-windows-msvc\release\helios_umd.dll`.
    /// The installer stages it as helios_umd32.dll. Required with umd12_32_dll
    /// whenever the package INF declares UserModeDriverNameWoW; omit both only
    /// for an older native-only package. There is no x64 fallback.
    #[serde(default)]
    umd32_dll: Option<String>,
    /// Explicit x86 D3D12 artifact, normally
    /// `umd12\target\i686-pc-windows-msvc\release\helios_umd12.dll`.
    /// Staged as helios_umd12_32.dll. Required with umd32_dll for WoW64 packages.
    #[serde(default)]
    umd12_32_dll: Option<String>,
    /// Extra flags passed through to `tools\install-helios-kmd.ps1` in
    /// addition to the always-passed -AllowRebootRequired (e.g. ["-PlanOnly"],
    /// ["-BinaryOnly"], ["-SkipSign"]). Do NOT pass
    /// -PackageDir/-UmdDll/-Umd12Dll/-Umd32Dll/-Umd12_32Dll here (including
    /// colon notation) — use the fields above; a duplicate
    /// named argument is a PowerShell parameter-binding error.
    #[serde(default)]
    args: Vec<String>,
    /// Timeout in seconds for the install step. Defaults to 900.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// PowerShell accepts `-Name value`, `-Name:value`, and unambiguous prefixes.
/// A passthrough artifact flag must not bind an explicit field a second time.
fn script_argument_is(token: &str, expected: &str) -> bool {
    let token = token.trim_matches(['\'', '"']);
    let name = token.split_once(':').map_or(token, |(name, _)| name);
    name.len() > 1
        && expected
            .get(..name.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name))
}

fn has_script_argument(args: &[String], names: &[&str]) -> bool {
    args.iter()
        .flat_map(|arg| arg.split_whitespace())
        .any(|token| names.iter().any(|name| script_argument_is(token, name)))
}

/// A successful preview must never be followed by the default reboot. Honor
/// explicit false when PowerShell supports it; suppress reboot conservatively
/// for other colon spellings rather than treating a preview as an install.
fn script_switch_enabled(args: &[String], name: &str) -> bool {
    args.iter()
        .flat_map(|arg| arg.split_whitespace())
        .find(|token| script_argument_is(token, name))
        .is_some_and(|token| {
            let value = token
                .split_once(':')
                .map(|(_, value)| value.trim_matches(['\'', '"']));
            !value.is_some_and(|value| {
                value.eq_ignore_ascii_case("$false")
                    || value.eq_ignore_ascii_case("false")
                    || value == "0"
            })
        })
}

fn kmd_build_environment(
    extra: Option<HashMap<String, String>>,
) -> Result<HashMap<String, String>, String> {
    // Windows environment names are case insensitive; assigning differently
    // cased keys in HashMap order would make engine selection nondeterministic.
    let mut env = HashMap::new();
    for (key, value) in extra.unwrap_or_default() {
        let key = key.to_ascii_uppercase();
        if env.insert(key.clone(), value).is_some() {
            return Err(format!("duplicate Windows environment variable: {key}"));
        }
    }
    env.entry("LIBCLANG_PATH".to_string())
        .or_insert_with(|| LIBCLANG_PATH.to_string());
    // Never inherit an unrelated target directory or the Z: share.
    env.insert(
        "CARGO_TARGET_DIR".to_string(),
        format!("{MIRROR_ROOT}\\kmd_render\\target"),
    );
    Ok(env)
}

fn kmd_install_script(a: &WinInstallKmdArgs) -> Result<String, String> {
    if has_script_argument(
        &a.args,
        &[
            "-PackageDir",
            "-UmdDll",
            "-Umd12Dll",
            "-Umd32Dll",
            "-Umd12_32Dll",
        ],
    ) {
        return Err("artifact flags must use package_dir / umd_dll / umd12_dll / umd32_dll / umd12_32_dll fields, not args (including -Name:value)".to_string());
    }
    if a.umd32_dll.is_some() != a.umd12_32_dll.is_some() {
        return Err(
            "supply both umd32_dll and umd12_32_dll, or omit both for a native-only package"
                .to_string(),
        );
    }
    let mut paths = vec![
        ("PackageDir", a.package_dir.as_str()),
        ("UmdDll", a.umd_dll.as_str()),
        ("Umd12Dll", a.umd12_dll.as_str()),
    ];
    if let Some(path) = a.umd32_dll.as_deref() {
        paths.push(("Umd32Dll", path));
    }
    if let Some(path) = a.umd12_32_dll.as_deref() {
        paths.push(("Umd12_32Dll", path));
    }
    let mut command =
        format!("& '{PROJECT_DRIVE}tools\\install-helios-kmd.ps1' -AllowRebootRequired");
    for (name, value) in paths {
        if value.trim().is_empty() {
            return Err(format!(
                "{name} requires an explicit nonempty artifact path"
            ));
        }
        command.push_str(&format!(" -{name} '{}'", ps_single_quote(value)));
    }
    if !a.args.is_empty() {
        command.push_str(&format!(" {}", a.args.join(" ")));
    }
    Ok(command)
}

fn kmd_install_command(a: &WinInstallKmdArgs) -> Result<String, String> {
    let command = kmd_install_script(a)?;
    // Keep the child process and policy bypass, but bind script parameters
    // inside PowerShell: powershell.exe 5.1 -File cannot pass switch booleans.
    // Catch script/binding failures explicitly so no failure requests a reboot.
    let script = format!(
        "$ProgressPreference = 'SilentlyContinue'\n$ErrorActionPreference = 'Stop'\n\
         try {{\n{command}\nexit 0\n}} catch {{\n[Console]::Error.WriteLine($_.ToString())\nexit 1\n}}"
    );
    Ok(format!(
        "& powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {}",
        encode_powershell(&script)
    ))
}

/// Bump (or verify) the KMD version at its single source of truth,
/// `kmd_render/driver-version.env`. `build.rs` renders the .sys
/// FILEVERSION/PRODUCTVERSION numerics and the FileVersion/ProductVersion
/// strings from one parse of that file, and `Cargo.make.toml` reads it through
/// cargo-make's top-level `env_files` for the stampinf `-v` that stamps the INF
/// DriverVer.
///
/// This function is a convenience bumper, NOT the coherence gate. A mismatch
/// between the INF DriverVer and the image FILEVERSION fails AddAdapter with
/// 0xc0000182 (FAILED_ADD), and that gate lives in `kmd_render/build.rs`
/// (`verify_version_wiring`) so it also covers a hand-run
/// `cargo make --makefile Cargo.make.toml` — building must not depend on this
/// server. All this does is parse, validate four components, and rewrite the one
/// line. Returns (old_version, new_version) as dotted strings.
fn bump_kmd_version(explicit: Option<&str>, no_bump: bool) -> Result<(String, String), String> {
    bump_kmd_version_at(&linux_project_root(), explicit, no_bump)
}

/// The one line that carries the version.
const KMD_VERSION_KEY: &str = "HELIOS_KMD_VERSION=";

fn bump_kmd_version_at(
    root: &str,
    explicit: Option<&str>,
    no_bump: bool,
) -> Result<(String, String), String> {
    let version_path = format!("{root}/kmd_render/driver-version.env");
    let version_file =
        std::fs::read_to_string(&version_path).map_err(|e| format!("read {version_path}: {e}"))?;

    // Current version: parse `HELIOS_KMD_VERSION=a.b.c.d` from driver-version.env.
    let cur_raw = version_file
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix(KMD_VERSION_KEY))
        .ok_or("no HELIOS_KMD_VERSION line in kmd_render/driver-version.env")?
        .trim();
    let cur: Vec<u32> = cur_raw
        .split('.')
        .map(|p| p.trim().parse::<u32>())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("unparsable HELIOS_KMD_VERSION: {e}"))?;
    if cur.len() != 4 {
        return Err(format!(
            "HELIOS_KMD_VERSION has {} fields, expected 4",
            cur.len()
        ));
    }
    let cur_dotted = format!("{}.{}.{}.{}", cur[0], cur[1], cur[2], cur[3]);

    if no_bump {
        return Ok((cur_dotted.clone(), cur_dotted));
    }

    let new: Vec<u32> = match explicit {
        Some(v) => {
            let parts: Vec<u32> = v
                .split('.')
                .map(|p| p.parse::<u32>())
                .collect::<Result<_, _>>()
                .map_err(|e| format!("bad explicit version {v:?}: {e}"))?;
            if parts.len() != 4 {
                return Err(format!("explicit version {v:?} must be a.b.c.d"));
            }
            parts
        }
        None => vec![cur[0], cur[1], cur[2] + 1, cur[3]],
    };
    let new_dotted = format!("{}.{}.{}.{}", new[0], new[1], new[2], new[3]);
    if new_dotted == cur_dotted {
        return Err(format!("new version equals current ({cur_dotted})"));
    }

    // Rewrite only the HELIOS_KMD_VERSION line, so the file's comments survive a
    // bump and a version string appearing inside one is not rewritten.
    let mut rewritten = false;
    let mut new_version_file = String::with_capacity(version_file.len());
    for line in version_file.lines() {
        if !rewritten && line.trim().starts_with(KMD_VERSION_KEY) {
            new_version_file.push_str(KMD_VERSION_KEY);
            new_version_file.push_str(&new_dotted);
            rewritten = true;
        } else {
            new_version_file.push_str(line);
        }
        new_version_file.push('\n');
    }
    if !rewritten {
        return Err(format!(
            "no HELIOS_KMD_VERSION line to rewrite in {version_path}"
        ));
    }
    std::fs::write(&version_path, new_version_file)
        .map_err(|e| format!("write {version_path}: {e}"))?;
    Ok((cur_dotted, new_dotted))
}

fn ps_join_path(root: &str, rel: &str) -> String {
    if root.ends_with(['\\', '/']) {
        format!("{root}{rel}")
    } else {
        format!("{root}\\{rel}")
    }
}

// ── generic multi-host tool arguments ───────────────────────────────────────
//
// These are the transport-agnostic layer: `host` selects which machine, and
// `purpose` selects the session/privilege rules (see `host::Purpose`). The
// domain tools above stay VM/Z:-specific; anything that has to work on the build
// slave as well goes through these.

#[derive(Deserialize, schemars::JsonSchema)]
struct HostOnlyArgs {
    /// Which host: "vm" (win11 dev VM) or "slave" (firstheberg2-win build slave).
    host: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct HostRunArgs {
    /// Which host: "vm" or "slave".
    host: String,
    /// PowerShell to run on that host.
    command: String,
    /// Working directory. Defaults to the host's workspace (C:\Users\Tibix on the
    /// VM, C:\src on the slave).
    cwd: Option<String>,
    /// Session/privilege rules: build | install | desktop | system. Defaults to
    /// "desktop" on the VM and "build" on the slave. "desktop" runs as the
    /// interactive user and REFUSES to run in session 0, because a GPU probe
    /// started there reports plausible but fake results.
    purpose: Option<String>,
    /// Extra environment variables as K=V. Repeatable.
    env: Option<Vec<String>>,
    /// ssh timeout in seconds (default 600).
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct HostRunScriptArgs {
    /// Which host: "vm" or "slave".
    host: String,
    /// Path to a .ps1 ON THE LINUX SIDE. It is pushed to the host's staging dir
    /// with a verified sha256 and then run there — so no quoting of the script
    /// body is involved anywhere.
    script: String,
    /// Arguments passed to the script.
    args: Option<Vec<String>>,
    /// Session/privilege rules: build | install | desktop | system.
    purpose: Option<String>,
    /// Start it DETACHED as a scheduled task with this name instead of blocking
    /// on ssh. Required for anything longer than a couple of minutes: an ssh
    /// keepalive drop kills a synchronous remote process.
    task: Option<String>,
    /// Log path for a detached task. Defaults to <staging>\<task>.log.
    log: Option<String>,
    /// ssh timeout in seconds for the synchronous case (default 600).
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct HostTaskArgs {
    /// Which host: "vm" or "slave".
    host: String,
    /// start | status | kill.
    action: String,
    /// Scheduled task name. Use a stable Helios-prefixed name so the next agent
    /// can find it.
    name: String,
    /// For action=start: remote path of the .ps1 to run. Use win_host_push or
    /// win_host_run_script (with task=) to get it there.
    script: Option<String>,
    /// For action=start: arguments for the script.
    args: Option<Vec<String>>,
    /// For action=start: build | install | desktop | system.
    purpose: Option<String>,
    /// Log path the task writes to (defaults to <staging>\<name>.log). Pass the
    /// same value back to action=status to read it.
    log: Option<String>,
    /// For action=status: how many log lines back to return (default 40).
    tail_lines: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct HostPushArgs {
    /// Which host: "vm" or "slave".
    host: String,
    /// Local (Linux) file to send.
    local: String,
    /// Destination path on the host, e.g. C:\src\out\pkg.zip.
    remote: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct HostPullArgs {
    /// Which host: "vm" or "slave".
    host: String,
    /// Remote (Windows) file to fetch.
    remote: String,
    /// Local (Linux) destination path.
    local: String,
}

#[tool_router]
impl WinHost {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Run a PowerShell command on the Windows 11 dev VM (win11) over SSH and return exit_code, stdout, and stderr. The Helios project tree is shared at Z:\\ (same files as the Linux side). Prefer this over raw ssh: it avoids cmd.exe quoting issues and uses a fresh environment."
    )]
    async fn win_exec(&self, Parameters(a): Parameters<WinExecArgs>) -> String {
        let env = a.env.unwrap_or_default();
        match run_ssh(
            &a.command,
            a.cwd.as_deref(),
            &env,
            a.timeout_secs.unwrap_or(600),
        )
        .await
        {
            Ok(o) => format_output(&o),
            Err(e) => format!("error launching ssh: {e}"),
        }
    }

    #[tool(
        description = "Sync the project to the local build mirror and run cargo (or cargo make) there. The Z:\\ share cannot host cargo/wdk build IO (OS error 87), so this robocopy-mirrors Z:\\ -> C:\\Users\\Rupansh\\helios-vgpu (excluding target/, all .git, and the vendored Mesa submodule at icd/mesa — Mesa is a meson/C ICD built separately via win_meson straight from the share, not through this mirror) and builds inside the mirror with LIBCLANG_PATH set for bindgen. Edit sources on the Linux/Z:\\ side — the mirror is re-synced on every call. crate_dir is relative to the project root (e.g. \"kmd_render\" or \"umd\"; the active Rust crates are kmd_render, umd, umd_common, umd12, protocol and kmd_logic — \"kmd\" is the ARCHIVED System-class driver and \"icd\" is meson/C, built by win_meson); args is the cargo argv (e.g. [\"make\",\"--makefile\",\"Cargo.make.toml\"] or [\"build\"]). THE TWO UMDs: \"umd\" is helios_umd.dll (D3D11, DXVK) and \"umd12\" is helios_umd12.dll (D3D12, vkd3d — enabled by default; UmdD3D12=0 disables new processes); both take \"umd_common\" as a path rlib, so it is never built on its own. To build BOTH with the rustc-only diagnostic filter, prefer `win_exec` on tools\\umd-check.ps1 -Mode release, which also avoids the ~115 clang warnings a raw umd build emits. vkd3d-proton-helios is built separately with win_vkd3d; helios_umd12.dll statically links its archives (DECISIONS.md D4)."
    )]
    async fn win_cargo(&self, Parameters(a): Parameters<WinCargoArgs>) -> String {
        let mut env = HashMap::new();
        env.insert("LIBCLANG_PATH".to_string(), LIBCLANG_PATH.to_string());
        // 1) mirror the tree to local disk (cargo/wdk IO fails on the share),
        // 2) cd into the crate in the mirror, 3) build with the local default target.
        //
        // robocopy /XD excludes: cargo `target` dirs, every `.git` DIRECTORY (which
        // also covers the Mesa submodule's multi-GB history under the superproject's
        // .git/modules/icd/mesa), AND the whole vendored Mesa tree at MESA_SRC. Mesa
        // is a meson/C ICD, never a cargo build, so it has no business in this mirror;
        // excluding it keeps every kmd/probe build fast. Mesa is built separately,
        // straight from the share, via `win_meson` (no robocopy — validated).
        let command = format!(
            "robocopy {PROJECT_DRIVE} {MIRROR_ROOT} /MIR /XJ /XD target .git \"{MESA_SRC}\" dxvk dxvk-research-only vkd3d-proton vkd3d-proton-helios virtio-research-only-3d windows-driver-docs-research-only /NFL /NDL /NJH /NJS /NP /R:1 /W:1\n\
             $robocopyExit = $LASTEXITCODE\n\
             if ($robocopyExit -ge 8) {{ \"win_cargo: robocopy mirror sync failed (exit $robocopyExit)\"; exit $robocopyExit }}\n\
             Set-Location -LiteralPath '{MIRROR_ROOT}\\{}'\n\
             cargo {}",
            a.crate_dir,
            a.args.join(" "),
        );
        match run_ssh(&command, None, &env, a.timeout_secs.unwrap_or(1800)).await {
            Ok(o) => format_output(&o),
            Err(e) => format!("error launching ssh: {e}"),
        }
    }

    #[tool(
        description = "Build vkd3d-proton (the D3D12 engine) on win11 with the clang-cl (MSVC ABI) toolchain, producing the STATIC archives helios_umd12.dll links. ⛔ STATIC, NOT A DLL — owner decision 2026-08-05 (DECISIONS.md D4): \"we are going to statically link vkd3d-proton and not mess with dynamic dlls\". This is the exact shape win_dxvk uses for DXVK, and for the same reasons: it mirrors Z:\\vkd3d-proton-helios -> the local checkout C:\\Users\\Rupansh\\vkd3d-proton-helios (meson build IO must not run on the Z:\\ 9p share) and builds into C:\\Users\\Rupansh\\vkd3d-build. CRITICAL and the reason this tool exists rather than a raw win_exec: it prepends LLVM (clang-cl) to PATH BEFORE calling vcvars64, because cmd expands %PATH% at PARSE time — the reverse order silently drops MSVC's lib.exe/link.exe and the archive step dies with 'CreateProcess failed'. It also pins CC/CXX=clang-cl, which is how meson picks clang-cl over cl.exe (matching the DXVK build dir, verified: compiler id clang-cl 17.0.6, linker lld-link, b_vscrt=mt). Empty `args` is the normal case: it configures the build dir with the canonical setup if unconfigured, then compiles. The artifact set to link is libs/d3d12core/libhelios_d3d12_static.a plus libs/vkd3d/libvkd3d-proton.a, libs/vkd3d-common/libvkd3d_common.a, libs/vkd3d-shader/libvkd3d-shader.a, subprojects/dxil-spirv/libdxil-spirv.a, subprojects/dxil-spirv/libdxbc_spv_module.a and subprojects/dxil-spirv/subprojects/dxbc-spirv/libdxbc_spv.a. ⚠ helios_d3d12_static deliberately excludes libs/d3d12core/main.c, the only object in the engine that references CreateDXGIFactory1 — so the static arm has NO dxgi.dll import, which the retired DLL arm did have. Toolchain deps are all present on win11: widl from the WinLibs UCRT mingw64 bin, glslangValidator from the Vulkan SDK, meson/ninja from Python 3.12. Prerequisite: the nested submodules must be initialised (git submodule update --init --recursive inside the submodule) or meson will not configure."
    )]
    async fn win_vkd3d(&self, Parameters(a): Parameters<WinVkd3dArgs>) -> String {
        // Same mirror-then-build flow as win_dxvk. /XD+/XF .git skip all git
        // metadata (the submodule .git dirs AND the .git pointer files), so the
        // local checkout's git state is never touched, and the meson BUILD dir is
        // outside the source tree so the mirror stays clean.
        let sync = if a.no_sync {
            "\"win_vkd3d: no_sync — skipping source mirror\"".to_string()
        } else {
            format!(
                "robocopy {VKD3D_SRC} {VKD3D_MIRROR} /MIR /XJ /XD .git /XF .git /NFL /NDL /NJH /NJS /NP /R:1 /W:1\n\
                 $rc = $LASTEXITCODE\n\
                 if ($rc -ge 8) {{ \"win_vkd3d: robocopy vkd3d source mirror failed (exit $rc)\"; exit $rc }}"
            )
        };
        // The canonical configure. `--buildtype release` + `-Db_vscrt=mt` match
        // DXVK exactly, which is what keeps the two engines' CRT and C++ ABI
        // compatible with the Rust msvc target that links them both.
        // enable_tests stays OFF: the conformance suite is built by the mingw
        // cross arm on the Linux host (D12-G0/G2), not here.
        //
        // ⛔ `_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH` is REQUIRED, not optional.
        // MSVC 14.44's <yvals_core.h> hard-asserts "expected Clang 19.0.0 or
        // newer" and the installed clang-cl is 17.0.6, so without it the
        // dxbc-spirv objects fail to compile at all (verified: 143-target build
        // goes from `ninja: build stopped` to clean). This is the same define
        // `umd/build.rs` already applies to the DXVK bridge shim, with the same
        // caveat recorded there: it is a runtime-risk acknowledgement, not a fix
        // — the ABI still rests on the objects agreeing, which nothing here can
        // prove. Removing it hard-fails the only working build.
        let stl = "-D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH";
        let setup = format!(
            "meson setup {VKD3D_BUILD} {VKD3D_MIRROR} --buildtype release -Db_vscrt=mt \
             -Denable_tests=false \"-Dcpp_args={stl}\" \"-Dc_args={stl}\""
        );
        let meson_cmd = if !a.args.is_empty() {
            format!("meson {}", a.args.join(" "))
        } else if a.wipe {
            format!("{setup} --wipe && meson compile -C {VKD3D_BUILD}")
        } else {
            // Configure only when not configured yet, so the common call is a
            // plain incremental compile.
            format!(
                "(if not exist \"{VKD3D_BUILD}\\build.ninja\" ({setup}) ) && meson compile -C {VKD3D_BUILD}"
            )
        };
        // ⛔ LLVM before vcvars64 — see win_dxvk. CC/CXX pin clang-cl; without
        // them meson picks cl.exe from vcvars and the archives come out with a
        // different C++ ABI from DXVK's.
        let command = format!(
            "{sync}\n\
             cmd /c 'set \"PATH={LLVM_BIN};%PATH%\" && set \"CC=clang-cl\" && set \"CXX=clang-cl\" && call \"{VCVARS}\" && {meson_cmd}'"
        );
        match run_ssh(
            &command,
            None,
            &HashMap::new(),
            a.timeout_secs.unwrap_or(3600),
        )
        .await
        {
            Ok(o) => format_output(&o),
            Err(e) => format!("error launching ssh: {e}"),
        }
    }

    #[tool(
        description = "Build the Mesa venus Vulkan ICD on win11 with the RECOMMENDED mingw-w64 gcc toolchain. Mesa is read straight from the Z:\\ share at Z:\\icd\\mesa and built into the LOCAL dir C:\\Users\\Rupansh\\helios-mesa-build (validated: gcc compiles 100% of venus from Z:\\ to link, zero Mesa edits). `args` is the meson argv. To CONFIGURE, pass the native file + the compat forced-include + the option set, e.g.: [\"setup\",\"C:\\\\Users\\\\Rupansh\\\\helios-mesa-build\",\"Z:\\\\icd\\\\mesa\",\"--native-file\",\"Z:\\\\icd\\\\win-build\\\\mingw-native.ini\",\"-Dc_args=-includeZ:\\\\icd\\\\win-build\\\\helios_win_compat.h\",\"-Dvulkan-drivers=virtio\",\"-Dgallium-drivers=\",\"-Dplatforms=windows\",\"-Dvideo-codecs=\",\"-Dvulkan-layers=\",\"-Degl=disabled\",\"-Dgbm=disabled\",\"-Dglx=disabled\",\"-Dopengl=false\",\"-Dgles1=disabled\",\"-Dgles2=disabled\",\"-Dllvm=disabled\",\"-Dshader-cache=disabled\",\"-Dbuild-tests=false\",\"-Dperfetto=false\",\"--buildtype=debugoptimized\"]. To BUILD, [\"compile\",\"-C\",\"C:\\\\Users\\\\Rupansh\\\\helios-mesa-build\"]; empty args defaults to compiling that dir. The mingw bin is prepended to PATH; no vcvars (mingw is self-contained). The clang-cl alternative (icd/win-build/clang-cl-native.ini) needs a local C: source mirror — drive it via win_exec. See icd/PHASE5_HANDOVER.md §6."
    )]
    async fn win_meson(&self, Parameters(a): Parameters<WinMesonArgs>) -> String {
        // Default to compiling the standard Mesa ICD build dir.
        let meson_args = if a.args.is_empty() {
            format!("compile -C {MESA_BUILD}")
        } else {
            a.args.join(" ")
        };
        // Recommended toolchain = mingw-w64 gcc: prepend its bin to PATH so gcc +
        // its helpers resolve (the --native-file the caller passes pins the actual
        // compilers). No vcvars — mingw ships its own Windows headers/libs and gcc
        // ignores the MSVC INCLUDE/LIB env. meson reads Mesa source from Z:\ directly
        // (no robocopy); ninja artifacts go to the local C: build dir. cmd expands
        // %PATH% at parse time, which is correct here (single prepend, no prior env mutation).
        let command = format!("cmd /c 'set \"PATH={MINGW_BIN};%PATH%\" && meson {meson_args}'");
        match run_ssh(
            &command,
            None,
            &HashMap::new(),
            a.timeout_secs.unwrap_or(1800),
        )
        .await
        {
            Ok(o) => format_output(&o),
            Err(e) => format!("error launching ssh: {e}"),
        }
    }

    #[tool(
        description = "Sync the project to the local Windows mirror and build the Looking Glass Windows host server from LookingGlass\\host. This mirrors Z:\\ -> C:\\Users\\Rupansh\\helios-vgpu with robocopy (excluding target/, all .git dirs, and icd\\mesa), then builds from local disk into C:\\Users\\Rupansh\\helios-lookingglass-host-build using mingw-w64 gcc + Ninja. Edit sources on the Linux/Z:\\ side; the mirror is re-synced on every call. Empty args configure with Ninja RelWithDebInfo USE_NVFBC=OFF and build the default target."
    )]
    async fn win_looking_glass(&self, Parameters(a): Parameters<WinLookingGlassArgs>) -> String {
        let source_root = a.source_root.unwrap_or_else(|| PROJECT_DRIVE.to_string());
        let lg_source = ps_join_path(&source_root, "LookingGlass");
        let build_dir = a
            .build_dir
            .unwrap_or_else(|| "C:\\Users\\Rupansh\\helios-lookingglass-host-build".to_string());
        let lg_src = format!("{MIRROR_ROOT}\\LookingGlass\\host");

        let configure = if a.no_configure {
            String::new()
        } else if a.configure_args.is_empty() {
            format!(
                "cmake -S \"{lg_src}\" -B \"{build_dir}\" -G Ninja -DCMAKE_BUILD_TYPE=RelWithDebInfo -DUSE_NVFBC=OFF"
            )
        } else {
            format!(
                "cmake -S \"{lg_src}\" -B \"{build_dir}\" {}",
                a.configure_args.join(" ")
            )
        };
        let build = if a.build_args.is_empty() {
            format!("cmake --build \"{build_dir}\"")
        } else {
            format!("cmake --build \"{build_dir}\" {}", a.build_args.join(" "))
        };
        let command = format!(
            "if (!(Test-Path -LiteralPath '{source_root}')) {{ \"win_looking_glass: source root not found: {source_root}\"; exit 3 }}\n\
             robocopy \"{lg_source}\" {MIRROR_ROOT}\\LookingGlass /MIR /XJ /XD .git build x64 Debug Release /NFL /NDL /NJH /NJS /NP /R:1 /W:1\n\
             $robocopyExit = $LASTEXITCODE\n\
             if ($robocopyExit -ge 8) {{ \"win_looking_glass: robocopy LookingGlass sync failed (exit $robocopyExit)\"; exit $robocopyExit }}\n\
             if (!(Test-Path -LiteralPath '{MIRROR_ROOT}\\LookingGlass\\host')) {{ \"win_looking_glass: LookingGlass\\host missing after sync\"; exit 2 }}\n\
             $env:PATH = '{MINGW_BIN};' + $env:PATH\n\
             {}\n\
             if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}\n\
             {}\n",
            if configure.is_empty() {
                "\"win_looking_glass: skipping configure\"".to_string()
            } else {
                configure
            },
            build,
        );

        match run_ssh(
            &command,
            None,
            &HashMap::new(),
            a.timeout_secs.unwrap_or(1800),
        )
        .await
        {
            Ok(o) => format_output(&o),
            Err(e) => format!("error launching ssh: {e}"),
        }
    }

    #[tool(
        description = "Sync the project to the local Windows mirror and build the Looking Glass IDD WDK driver from LookingGlass\\idd\\LGIdd.sln. This mirrors Z:\\ -> C:\\Users\\Rupansh\\helios-vgpu with robocopy (excluding target/, all .git dirs, and icd\\mesa), copies only icd\\mesa\\include\\vulkan into the mirror for Vulkan headers, then builds the IDD solution from local NTFS with MSBuild. Edit sources on the Linux/Z:\\ side; the mirror is re-synced on every call. Empty msbuild_args builds Release|x64 with RunInfVerif=false."
    )]
    async fn win_looking_glass_idd(
        &self,
        Parameters(a): Parameters<WinLookingGlassIddArgs>,
    ) -> String {
        let source_root = a.source_root.unwrap_or_else(|| PROJECT_DRIVE.to_string());
        let lg_source = ps_join_path(&source_root, "LookingGlass");
        let mesa_vulkan_src = ps_join_path(&source_root, "icd\\mesa\\include\\vulkan");
        let msbuild = a.msbuild_path.unwrap_or_else(|| MSBUILD.to_string());
        let sln = format!("{MIRROR_ROOT}\\LookingGlass\\idd\\LGIdd.sln");
        let msbuild_args = if a.msbuild_args.is_empty() {
            "/p:Configuration=Release /p:Platform=x64 /p:RunInfVerif=false /m /v:minimal"
                .to_string()
        } else {
            a.msbuild_args.join(" ")
        };
        let build = if a.sync_only {
            "\"win_looking_glass_idd: sync_only requested; skipping MSBuild\"\n$global:LASTEXITCODE = 0"
                .to_string()
        } else {
            format!(
                "if (!(Test-Path -LiteralPath '{msbuild}')) {{ \"win_looking_glass_idd: MSBuild not found: {msbuild}\"; exit 4 }}\n\
                 & '{msbuild}' '{sln}' {msbuild_args}\n"
            )
        };

        let command = format!(
            "if (!(Test-Path -LiteralPath '{source_root}')) {{ \"win_looking_glass_idd: source root not found: {source_root}\"; exit 3 }}\n\
             robocopy \"{lg_source}\" {MIRROR_ROOT}\\LookingGlass /MIR /XJ /XD .git build x64 Debug Release /NFL /NDL /NJH /NJS /NP /R:1 /W:1\n\
             $robocopyExit = $LASTEXITCODE\n\
             if ($robocopyExit -ge 8) {{ \"win_looking_glass_idd: robocopy LookingGlass sync failed (exit $robocopyExit)\"; exit $robocopyExit }}\n\
             New-Item -ItemType Directory -Force -Path '{MIRROR_ROOT}\\icd\\mesa\\include\\vulkan' | Out-Null\n\
             robocopy \"{mesa_vulkan_src}\" {MIRROR_ROOT}\\icd\\mesa\\include\\vulkan /MIR /XJ /NFL /NDL /NJH /NJS /NP /R:1 /W:1\n\
             $robocopyExit = $LASTEXITCODE\n\
             if ($robocopyExit -ge 8) {{ \"win_looking_glass_idd: Vulkan header sync failed (exit $robocopyExit)\"; exit $robocopyExit }}\n\
             if (!(Test-Path -LiteralPath '{sln}')) {{ \"win_looking_glass_idd: LGIdd.sln missing after sync: {sln}\"; exit 2 }}\n\
             {build}"
        );

        match run_ssh(
            &command,
            None,
            &HashMap::new(),
            a.timeout_secs.unwrap_or(1800),
        )
        .await
        {
            Ok(o) => format_output(&o),
            Err(e) => format!("error launching ssh: {e}"),
        }
    }

    #[tool(
        description = "Build the DXVK-helios C++ engine (the UMD's render backend) on win11 with the clang-cl (MSVC ABI) toolchain. Mirrors the source Z:\\dxvk-helios -> the local git checkout C:\\Users\\Rupansh\\dxvk-helios (the meson build reads the LOCAL copy, NOT the Z:\\ share) with robocopy, then runs meson in the build dir C:\\Users\\Rupansh\\dxvk-build. CRITICAL and the reason this tool exists: it prepends LLVM (clang-cl) to PATH BEFORE calling vcvars64 (which supplies MSVC lib.exe/link.exe). The reverse order silently drops the MSVC archiver because cmd expands %PATH% at PARSE time, and the archive step then fails 'CreateProcess failed'. Empty `args` is the normal case and does the right thing: it configures the build dir with the canonical setup if it is not configured yet, then compiles. ⛔ That canonical setup carries `-Db_vscrt=mt` — the STATIC CRT — which must match `umd/.cargo/config.toml`'s `crt-static`; vkd3d/umd12 also use the STATIC CRT, so both engines' flags agree and the installer needs no VC++ redist (see DXVK_CPP_ARGS). After this, relink the UMD with `win_cargo crate_dir:\"umd\" args:[\"build\"]` (its build.rs reruns on the changed .a archives), then deploy with win_install_umd. Edit DXVK sources on the Linux/Z:\\ side; the mirror re-syncs on every call."
    )]
    async fn win_dxvk(&self, Parameters(a): Parameters<WinDxvkArgs>) -> String {
        // The canonical configure, kept HERE rather than only in the transcript of
        // whoever last ran it. Before this existed, `args: []` was a bare
        // `meson compile` that failed outright if DXVK_BUILD was missing, and the
        // setup line — including the load-bearing `-Db_vscrt=mt` — lived only in
        // `ci/windows/Build-Driver.ps1` under CI-specific paths. A build dir that
        // has to be reconstructed by hand from a CI script is a flag set that can
        // silently come back WRONG, and a CRT mismatch does not fail the build; it
        // fails in an app's process later. Same shape as `win_vkd3d`.
        let setup = format!(
            "meson setup {DXVK_BUILD} {DXVK_MIRROR} --native-file {DXVK_NATIVE_FILE} \
             --buildtype release -Db_vscrt=mt \"-Dcpp_args={DXVK_CPP_ARGS}\" \
             \"-Dc_args=/FI{DXVK_C_COMPAT_HEADER}\" -Denable_d3d8=false -Denable_d3d9=false \
             -Denable_d3d10=false -Denable_d3d11=true -Denable_dxgi=true"
        );
        // ⚠ Full command, NOT an argv tail: this used to interpolate into a
        // `meson {args}` template, which cannot express "configure, then compile"
        // (two meson invocations). Same shape as `win_vkd3d`'s `meson_cmd`.
        let meson_cmd = if a.args.is_empty() {
            // Configure only when not configured yet, so the common call stays a
            // plain incremental compile.
            format!(
                "(if not exist \"{DXVK_BUILD}\\build.ninja\" ({setup}) ) && \
                 meson compile -C {DXVK_BUILD}"
            )
        } else {
            format!("meson {}", a.args.join(" "))
        };
        // Mirror the DXVK source share -> local checkout. /XD+/XF .git skip all git
        // metadata (submodule .git dirs AND the submodule .git pointer files), so
        // the local checkouts' git state is never touched. The meson BUILD dir is
        // separate (DXVK_BUILD), so the source tree carries no build artifacts.
        let sync = if a.no_sync {
            "\"win_dxvk: no_sync — skipping source mirror\"".to_string()
        } else {
            format!(
                "robocopy {DXVK_SRC} {DXVK_MIRROR} /MIR /XJ /XD .git /XF .git /NFL /NDL /NJH /NJS /NP /R:1 /W:1\n\
                 $rc = $LASTEXITCODE\n\
                 if ($rc -ge 8) {{ \"win_dxvk: robocopy DXVK source mirror failed (exit $rc)\"; exit $rc }}"
            )
        };
        // clang-cl (LLVM) MUST precede `call vcvars64`: cmd expands %PATH% at PARSE
        // time, so `set PATH=LLVM;%PATH%` placed AFTER vcvars captures the
        // pre-vcvars PATH and discards vcvars' MSVC bin (lib.exe/link.exe vanish ->
        // "CreateProcess failed" at the archive step). Order: LLVM first, then
        // vcvars (prepends MSVC), then meson. clang-cl resolves from LLVM, the
        // archiver/linker from MSVC.
        let command = format!(
            "{sync}\n\
             cmd /c 'set \"PATH={LLVM_BIN};%PATH%\" && call \"{VCVARS}\" && {meson_cmd}'"
        );
        match run_ssh(
            &command,
            None,
            &HashMap::new(),
            a.timeout_secs.unwrap_or(1800),
        )
        .await
        {
            Ok(o) => format_output(&o),
            Err(e) => format!("error launching ssh: {e}"),
        }
    }

    #[tool(
        description = "Hotplug native x64 Helios UMDs using tools\\hotplug-helios-umd.ps1. Build release artifacts first; umd_dll and package_dir are always forwarded and printed. Optional umd12_dll also replaces native DX12; omitting it preserves the installed DX12 slot. WoW64 registration is preserved. Default ProgramData mode copies to C:\\ProgramData\\HeliosUmd, updates registration and verifies hashes; it never edits DriverStore. For a persistent signed-package upgrade, use win_install_kmd. Pass -PlanOnly through args for inspection, or -KillUmdUsers -RestartDevice -NoProbe for activation. Runtime path caching may retain an old module after restart; verify loaded module paths and hashes in fresh processes. Do not duplicate artifact parameters through args. The tool uses PowerShell ExecutionPolicy Bypass."
    )]
    async fn win_install_umd(&self, Parameters(a): Parameters<WinInstallUmdArgs>) -> String {
        let umd_dll = a.umd_dll.as_deref().unwrap_or(DEFAULT_UMD_DLL);
        let package_dir = a.package_dir.as_deref().unwrap_or(DEFAULT_KMD_PACKAGE_DIR);
        // No default for the D3D12 UMD, by design — see `SUGGESTED_UMD12_DLL`.
        let umd12_dll = a.umd12_dll.as_deref();
        // The artifacts are named in the OUTPUT, not just in the command line, so
        // "which UMD did that deploy ship?" is answerable from the transcript.
        // The D3D12 line is emitted either way, so "was D3D12 deployed?" is a
        // fact in the transcript rather than an absence to be inferred.
        let header = format!(
            "{}{}{}",
            artifact_line("UMD  ", umd_dll, DEFAULT_UMD_DLL),
            match umd12_dll {
                Some(p) => artifact_line("UMD12", p, SUGGESTED_UMD12_DLL),
                None =>
                    "UMD12: (unchanged; pass `umd12_dll` to replace native slot 3)\n"
                        .to_string(),
            },
            artifact_line("pkg  ", package_dir, DEFAULT_KMD_PACKAGE_DIR),
        );
        if a.args.iter().any(|f| {
            f.eq_ignore_ascii_case("-UmdDll")
                || f.eq_ignore_ascii_case("-PackageDir")
                || f.eq_ignore_ascii_case("-Umd12Dll")
        }) {
            return format!(
                "{header}\nwin_install_umd: REFUSED — -UmdDll / -Umd12Dll / -PackageDir passed \
                 through `args` would bind twice against the explicit ones. Use the `umd_dll` / \
                 `umd12_dll` / `package_dir` fields instead."
            );
        }
        // Only pass -Umd12Dll when asked: the script treats an empty string as
        // "not deployed", but not passing it at all keeps the two paths textually
        // distinct in the transcript.
        let umd12_arg = match umd12_dll {
            Some(p) => format!(" -Umd12Dll '{p}'"),
            None => String::new(),
        };
        let extra = a.args.join(" ");
        // Nested `powershell -ExecutionPolicy Bypass -File` — REQUIRED because the
        // machine ExecutionPolicy is Restricted, so a bare `& script.ps1` (which is
        // effectively what run_ssh's outer -EncodedCommand shell would do) silently
        // fails to run the .ps1. Running it as a child process also makes the
        // script's Write-Host output visible (captured as the child's stdout),
        // unlike same-process Write-Host which goes to an uncaptured host stream.
        let command = format!(
            "& powershell -NoProfile -ExecutionPolicy Bypass -File '{PROJECT_DRIVE}tools\\hotplug-helios-umd.ps1' \
             -UmdDll '{umd_dll}'{umd12_arg} -PackageDir '{package_dir}' {extra}"
        );
        match run_ssh(
            &command,
            None,
            &HashMap::new(),
            a.timeout_secs.unwrap_or(600),
        )
        .await
        {
            Ok(o) => format!("{header}{}", format_output(&o)),
            Err(e) => format!("{header}error launching ssh: {e}"),
        }
    }

    #[tool(
        description = "Bump the Helios KMD (kmd_render) version and build the signed driver package. The version lives at ONE site, kmd_render/driver-version.env (HELIOS_KMD_VERSION): build.rs renders the .sys FILEVERSION/PRODUCTVERSION numerics and FileVersion/ProductVersion strings from one parse of it, and Cargo.make.toml reads it via cargo-make env_files for the stampinf -v that stamps the INF DriverVer — an INF/FILEVERSION mismatch is FAILED_ADD 0xc0000182. Coherence is enforced by kmd_render/build.rs itself (verify_version_wiring), so a hand-run `cargo make --makefile Cargo.make.toml` is covered too — this tool is only a convenience bumper. It bumps the third component by default (or uses `version`, or `no_bump` to rebuild the current version), edits the Linux-side source, then mirrors + runs `cargo make --makefile Cargo.make.toml` on the VM (build, inf2cat, test-sign, package). Output starts with the old -> new version line; the package lands in kmd_render\\target\\debug\\helios_kmd_render_package on the VM mirror. WoW64 prerequisites: build the x86 DXVK and vkd3d engines first, pass their HELIOS_DXVK_BUILD_X86 and HELIOS_VKD3D_BUILD_X86 directories in `env`, install the i686-pc-windows-msvc Rust target and put PowerShell 7 (pwsh) on PATH. The engine helpers win_dxvk/win_vkd3d are native-only; ci/windows/Build-Driver.ps1 is the complete x64/x86 build path. CARGO_TARGET_DIR is always the local KMD mirror target directory. Deploy with win_install_kmd. The version edit persists on the Linux tree — commit it with the change it ships."
    )]
    async fn win_build_kmd(&self, Parameters(a): Parameters<WinBuildKmdArgs>) -> String {
        let env = match kmd_build_environment(a.env) {
            Ok(env) => env,
            Err(error) => return format!("win_build_kmd: REFUSED — {error}"),
        };
        let (old_v, new_v) = match bump_kmd_version(a.version.as_deref(), a.no_bump) {
            Ok(v) => v,
            Err(e) => return format!("win_build_kmd: version bump failed: {e}"),
        };
        let header = if a.no_bump {
            format!("KMD version: {old_v} (no bump, coherence verified)\n")
        } else {
            format!("KMD version: {old_v} -> {new_v}\n")
        };
        // Same mirror-then-build flow as win_cargo (see its comments) with the
        // kmd_render package makefile.
        let command = format!(
            "robocopy {PROJECT_DRIVE} {MIRROR_ROOT} /MIR /XJ /XD target .git \"{MESA_SRC}\" dxvk dxvk-research-only vkd3d-proton vkd3d-proton-helios virtio-research-only-3d windows-driver-docs-research-only /NFL /NDL /NJH /NJS /NP /R:1 /W:1\n\
             $robocopyExit = $LASTEXITCODE\n\
             if ($robocopyExit -ge 8) {{ \"win_build_kmd: robocopy mirror sync failed (exit $robocopyExit)\"; exit $robocopyExit }}\n\
             Set-Location -LiteralPath '{MIRROR_ROOT}\\kmd_render'\n\
             cargo make --makefile Cargo.make.toml"
        );
        match run_ssh(&command, None, &env, a.timeout_secs.unwrap_or(1800)).await {
            Ok(o) => format!("{header}{}", format_output(&o)),
            Err(e) => format!("{header}error launching ssh: {e}"),
        }
    }

    #[tool(
        description = "Install the complete Helios graphics package using tools\\install-helios-kmd.ps1 with ExecutionPolicy Bypass. Build first with win_build_kmd. Explicit package_dir, umd_dll and umd12_dll are required; also supply umd32_dll and umd12_32_dll when the INF registers WoW64. Artifact paths are printed, and images are synchronized before catalog signing. Do not duplicate artifact parameters through args. Default restart_vm=true reboots the guest after successful installation; restart_vm=false defers it. -PlanOnly suppresses deployment and reboot. Test VM restarts have standing authorization in AGENTS.md. After reconnecting, verify the active version, PnP Code 0, loaded UMD paths and visible rendering. A WinBoat guest reboot may stop its container; restart that container if needed. See HELIOS_DRIVER_DEPLOYMENT.md."
    )]
    async fn win_install_kmd(&self, Parameters(a): Parameters<WinInstallKmdArgs>) -> String {
        // Native paths stay required. The optional x86 fields preserve old
        // package compatibility; the script enforces their INF dependency.
        let header = format!(
            "pkg      : {}\nUMD      : {}\nUMD12    : {}\nUMD32    : {}\nUMD12_32 : {}\n",
            a.package_dir,
            a.umd_dll,
            a.umd12_dll,
            a.umd32_dll
                .as_deref()
                .unwrap_or("(not supplied; native-only package)"),
            a.umd12_32_dll
                .as_deref()
                .unwrap_or("(not supplied; native-only package)"),
        );
        let command = match kmd_install_command(&a) {
            Ok(command) => command,
            Err(error) => return format!("{header}win_install_kmd: REFUSED — {error}"),
        };
        let install = match run_ssh(
            &command,
            None,
            &HashMap::new(),
            a.timeout_secs.unwrap_or(900),
        )
        .await
        {
            Ok(o) => o,
            Err(e) => return format!("{header}error launching ssh: {e}"),
        };
        let installed_ok = install.code == Some(0) && !install.timed_out;
        let mut out = format!("{header}{}", format_output(&install));

        let plan_only = script_switch_enabled(&a.args, "-PlanOnly");
        if a.restart_vm.unwrap_or(true) && !plan_only {
            if installed_ok {
                let reboot = run_ssh(
                    "shutdown /r /t 5 /c 'Helios KMD activation reboot'",
                    None,
                    &HashMap::new(),
                    30,
                )
                .await;
                out.push_str(match reboot {
                    Ok(o) if o.code == Some(0) => {
                        "\n\nwin_install_kmd: REBOOT ISSUED (shutdown /r /t 5) — the guest \
                         goes down in ~5 s and SSH returns in 1-3 min. Poll with win_exec, \
                         then verify DriverVersion and CM_PROB_NONE."
                    }
                    _ => {
                        "\n\nwin_install_kmd: install OK but the reboot command FAILED — \
                         reboot the guest manually to activate the new KMD."
                    }
                });
            } else {
                out.push_str(
                    "\n\nwin_install_kmd: install did not exit 0 — SKIPPING the reboot. \
                     Inspect the output above; the DriverStore backup path is printed there.",
                );
            }
        } else if installed_ok && !plan_only {
            out.push_str(
                "\n\nwin_install_kmd: installed WITHOUT reboot (restart_vm=false) — the new \
                 KMD activates on the next boot; until then the device may sit in \
                 FAILED_POST_START limbo on the old image.",
            );
        }
        out
    }

    // ── generic multi-host tools ────────────────────────────────────────────
    //
    // Why these exist next to the domain tools: the domain tools encode the VM's
    // Z:\ share model, which the build slave does not have, and none of them can
    // start detached work or report where a log went. Everything learned the hard
    // way about principals, sessions, detached tasks and verified transfers lives
    // in `host.rs` and is reached through these.

    #[tool(
        description = "One-shot status of the Helios stack on a host, as JSON. Use this FIRST in a session instead of hand-writing probes: it answers what package is installed and whether it is the loaded one (install-state, PnP status/problem, KMD service image + hash, every driver image hash, the registered Vulkan manifest, DWM's loaded graphics modules with hashes, pending-reboot flags, evidence dirs, free space, and any Helios scheduled tasks). On the slave it reports the tree HEAD and submodule pins, warm build dirs, staged artifacts with package zip hashes, recent logs and running build processes."
    )]
    async fn win_host_status(&self, Parameters(a): Parameters<HostOnlyArgs>) -> String {
        match host::host_spec(&a.host) {
            Ok(spec) => match host::status(spec).await {
                Ok(json) => json,
                Err(e) => format!("error: {e:#}"),
            },
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(
        description = "Preflight a host before starting work: are git/python/ninja/meson/cmake/cargo/clang-cl/MSVC present and where, is PowerShell 7 installed, is MSYS2 complete, do the expected workspaces exist, is the repo HEAD readable under THIS account (with the safe.directory setting), how much disk is free, and which Helios tasks are registered. Answers in one call what otherwise gets discovered eight minutes into a build. Returns JSON."
    )]
    async fn win_host_preflight(&self, Parameters(a): Parameters<HostOnlyArgs>) -> String {
        match host::host_spec(&a.host) {
            Ok(spec) => match host::preflight(spec).await {
                Ok(json) => json,
                Err(e) => format!("error: {e:#}"),
            },
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(
        description = "Identity and session facts for a host: hostname, account, session id, admin, PowerShell version, OS build, which interactive sessions exist, whether explorer/dwm are running there, and free space. Cheap, and worth calling whenever a result looks wrong — a probe in session 0 does not fail, it lies."
    )]
    async fn win_host_info(&self, Parameters(a): Parameters<HostOnlyArgs>) -> String {
        match host::host_spec(&a.host) {
            Ok(spec) => match host::hostinfo(spec).await {
                Ok(json) => json,
                Err(e) => format!("error: {e:#}"),
            },
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(
        description = "Run an ad-hoc PowerShell snippet on either host (vm or slave) with the session rules made explicit via purpose: build (SYSTEM + safe.directory + a PATH that cannot pick up MSYS2's git), install (SYSTEM), desktop (the interactive user, refusing session 0), system. Use win_exec for the Z:\\-share VM work you already have commands for; use this when the target may be the build slave or when the principal matters."
    )]
    async fn win_host_run(&self, Parameters(a): Parameters<HostRunArgs>) -> String {
        let spec = match host::host_spec(&a.host) {
            Ok(s) => s,
            Err(e) => return format!("error: {e:#}"),
        };
        let purpose = match a.purpose.as_deref().map(host::Purpose::parse).transpose() {
            Ok(p) => p.unwrap_or(spec.default_purpose),
            Err(e) => return format!("error: {e:#}"),
        };
        let env: Vec<(String, String)> = match parse_env(&a.env) {
            Ok(v) => v.into_iter().collect(),
            Err(e) => return format!("error: {e:#}"),
        };
        match host::run_command(
            spec,
            &a.command,
            a.cwd.as_deref(),
            &env,
            purpose,
            a.timeout_secs.unwrap_or(600),
        )
        .await
        {
            Ok(o) => format!("purpose={} {}", purpose.as_str(), format_output(&o)),
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(
        description = "Push a local (Linux) .ps1 to a host and run it there — same content, both hosts, no shell quoting anywhere. Pass task=<name> to start it DETACHED as a scheduled task and get the log path back (use this for anything long: an ssh keepalive drop kills a synchronous remote process, which is how a build silently dies). The push is sha256-verified on both ends."
    )]
    async fn win_host_run_script(&self, Parameters(a): Parameters<HostRunScriptArgs>) -> String {
        let spec = match host::host_spec(&a.host) {
            Ok(s) => s,
            Err(e) => return format!("error: {e:#}"),
        };
        let purpose = match a.purpose.as_deref().map(host::Purpose::parse).transpose() {
            Ok(p) => p.unwrap_or(spec.default_purpose),
            Err(e) => return format!("error: {e:#}"),
        };
        let args = a.args.clone().unwrap_or_default();
        match host::run_script(
            spec,
            std::path::Path::new(&a.script),
            &args,
            purpose,
            a.task.as_deref(),
            a.log.as_deref(),
            a.timeout_secs.unwrap_or(600),
        )
        .await
        {
            Ok(host::ScriptRun::Sync { transfer, remote, output }) => format!(
                "pushed {} bytes to {remote} (sha256 {}), purpose={}\n{}",
                transfer.size,
                transfer.sha256,
                purpose.as_str(),
                format_output(&output)
            ),
            Ok(host::ScriptRun::Task { transfer, task }) => format!(
                "pushed {} bytes (sha256 {}); task {} state={} purpose={}\nlog: {}\npoll with win_host_task action=status name={} log={}",
                transfer.size,
                transfer.sha256,
                task.name,
                task.state,
                purpose.as_str(),
                task.log,
                task.name,
                task.log
            ),
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(
        description = "Detached-task lifecycle: action=start (name + script, plus optional args/purpose/log), action=status (name + optional log/tail_lines: returns state, LastTaskResult, whether the log exists, its tail, and the WINRUN_EXIT marker the wrapper appends), action=kill. Long work belongs here rather than in a synchronous ssh call."
    )]
    async fn win_host_task(&self, Parameters(a): Parameters<HostTaskArgs>) -> String {
        let spec = match host::host_spec(&a.host) {
            Ok(s) => s,
            Err(e) => return format!("error: {e:#}"),
        };
        let log = a
            .log
            .clone()
            .unwrap_or_else(|| format!("{}\\{}.log", spec.staging, a.name));
        match a.action.as_str() {
            "start" => {
                let Some(script) = a.script.as_deref() else {
                    return "error: action=start needs script (a remote .ps1 path)".to_string();
                };
                let purpose = match a.purpose.as_deref().map(host::Purpose::parse).transpose() {
                    Ok(p) => p.unwrap_or(spec.default_purpose),
                    Err(e) => return format!("error: {e:#}"),
                };
                let args = a.args.clone().unwrap_or_default();
                match host::task_start(spec, &a.name, script, &args, purpose, Some(&log)).await {
                    Ok(t) => format!(
                        "task={} state={} purpose={} log={}",
                        t.name,
                        t.state,
                        purpose.as_str(),
                        t.log
                    ),
                    Err(e) => format!("error: {e:#}"),
                }
            }
            "status" => {
                match host::task_status(spec, &a.name, &log, a.tail_lines.unwrap_or(40)).await {
                    Ok(st) => format!(
                        "task={} exists={} state={} last_result={} log_exists={} exit_marker={}\n--- log tail ({log}) ---\n{}",
                        a.name,
                        st.exists,
                        st.state,
                        st.last_result.map(|v| v.to_string()).unwrap_or("-".into()),
                        st.log_exists,
                        st.exit_marker.map(|v| v.to_string()).unwrap_or("-".into()),
                        st.log_tail
                    ),
                    Err(e) => format!("error: {e:#}"),
                }
            }
            "kill" => match host::task_kill(spec, &a.name).await {
                Ok(()) => format!("killed task {}", a.name),
                Err(e) => format!("error: {e:#}"),
            },
            other => format!("error: unknown action '{other}' (start|status|kill)"),
        }
    }

    #[tool(
        description = "Copy a local file to a host with the sha256 verified on BOTH ends (size too). Use it instead of raw scp for packages and artifacts: an unchecked transfer that silently truncates is the expensive kind of failure."
    )]
    async fn win_host_push(&self, Parameters(a): Parameters<HostPushArgs>) -> String {
        match host::host_spec(&a.host) {
            Ok(spec) => match host::push(spec, std::path::Path::new(&a.local), &a.remote).await {
                Ok(t) => format!(
                    "{} -> {}  {} bytes  sha256={}  verified={}",
                    t.local, t.remote, t.size, t.sha256, t.verified
                ),
                Err(e) => format!("error: {e:#}"),
            },
            Err(e) => format!("error: {e:#}"),
        }
    }

    #[tool(
        description = "Copy a file from a host to Linux with the sha256 verified on BOTH ends. Use it for evidence and logs (task stdout, probe archives, WER dumps)."
    )]
    async fn win_host_pull(&self, Parameters(a): Parameters<HostPullArgs>) -> String {
        match host::host_spec(&a.host) {
            Ok(spec) => match host::pull(spec, &a.remote, std::path::Path::new(&a.local)).await {
                Ok(t) => format!(
                    "{} -> {}  {} bytes  sha256={}  verified={}",
                    t.remote, t.local, t.size, t.sha256, t.verified
                ),
                Err(e) => format!("error: {e:#}"),
            },
            Err(e) => format!("error: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::bump_kmd_version_at;

    fn install_args() -> super::WinInstallKmdArgs {
        super::WinInstallKmdArgs {
            restart_vm: Some(false),
            package_dir: r"C:\build\package".into(),
            umd_dll: r"C:\build\native\helios_umd.dll".into(),
            umd12_dll: r"C:\build\native\helios_umd12.dll".into(),
            umd32_dll: None,
            umd12_32_dll: None,
            args: vec!["-PlanOnly".into()],
            timeout_secs: None,
        }
    }

    #[test]
    fn kmd_install_explicit_architecture_paths() {
        let mut args = install_args();
        let native = super::kmd_install_script(&args).unwrap();
        assert!(!native.contains("-Umd32Dll"));
        assert!(!native.contains("-Umd12_32Dll"));
        args.umd32_dll = Some(r"C:\Chef's builds\i686\helios_umd.dll".into());
        assert!(
            super::kmd_install_command(&args).is_err(),
            "incomplete x86 pair accepted"
        );
        args.umd12_32_dll = Some(r"C:\Chef's builds\i686\helios_umd12.dll".into());
        let wow64 = super::kmd_install_script(&args).unwrap();
        assert!(wow64.contains(r"-Umd32Dll 'C:\Chef''s builds\i686\helios_umd.dll'"));
        assert!(wow64.contains(r"-Umd12_32Dll 'C:\Chef''s builds\i686\helios_umd12.dll'"));
        args.umd12_32_dll = Some("   ".into());
        assert!(
            super::kmd_install_command(&args).is_err(),
            "empty x86 path accepted"
        );
    }

    #[test]
    fn kmd_install_refuses_passthrough_artifacts() {
        for flag in [
            "-PackageDir",
            "-UmdDll",
            "-Umd12Dll",
            "-Umd32Dll",
            "-Umd12_32Dll",
            "-Umd12_32D",
        ] {
            for arg in [
                flag.to_string(),
                format!("{}:C:\\wrong.dll", flag.to_lowercase()),
                format!("-PlanOnly {flag} 'C:\\wrong.dll'"),
            ] {
                let mut args = install_args();
                args.args = vec![arg];
                assert!(
                    super::kmd_install_command(&args).is_err(),
                    "passthrough accepted: {:?}",
                    args.args
                );
            }
        }
    }

    #[test]
    fn kmd_preview_switch_never_requests_reboot() {
        for arg in [
            "-PlanOnly",
            "-planonly:$true",
            "-PlanO:True",
            "-PlanOnly:'$true'",
        ] {
            assert!(
                super::script_switch_enabled(&[arg.into()], "-PlanOnly"),
                "{arg}"
            );
        }
        for arg in [
            "-PlanOnly:$false",
            "-PlanOnly:False",
            "-PlanOnly:0",
            "-RestartDevice",
        ] {
            assert!(
                !super::script_switch_enabled(&[arg.into()], "-PlanOnly"),
                "{arg}"
            );
        }
    }

    #[test]
    fn kmd_install_child_binds_switches_inside_powershell() {
        use base64::Engine as _;
        for switch in ["-PlanOnly:$true", "-PlanOnly:$false"] {
            let mut args = install_args();
            args.args = vec![switch.into()];
            let command = super::kmd_install_command(&args).unwrap();
            let encoded = command
                .strip_prefix("& powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand ")
                .expect("must bind switches inside PowerShell, not powershell.exe -File");
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap();
            let wide: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            let script = String::from_utf16(&wide).unwrap();
            assert!(script.contains(&super::kmd_install_script(&args).unwrap()));
            assert!(script.contains(switch));
            assert!(script.contains("$ErrorActionPreference = 'Stop'"));
            assert!(script.contains("exit 0\n} catch {"));
            assert!(script.contains("exit 1"));
        }
    }

    #[test]
    fn kmd_build_preserves_explicit_engines_on_local_target() {
        let extra = std::collections::HashMap::from([
            (
                "helios_dxvk_build_x86".into(),
                r"C:\engines\dxvk-x86".into(),
            ),
            (
                "HELIOS_VKD3D_BUILD_X86".into(),
                r"C:\engines\vkd3d-x86".into(),
            ),
            ("Libclang_Path".into(), r"C:\llvm22\bin".into()),
            ("cargo_target_dir".into(), r"Z:\unsafe-target".into()),
        ]);
        let env = super::kmd_build_environment(Some(extra)).unwrap();
        assert_eq!(env["HELIOS_DXVK_BUILD_X86"], r"C:\engines\dxvk-x86");
        assert_eq!(env["HELIOS_VKD3D_BUILD_X86"], r"C:\engines\vkd3d-x86");
        assert_eq!(env["LIBCLANG_PATH"], r"C:\llvm22\bin");
        assert_eq!(
            env["CARGO_TARGET_DIR"],
            format!("{}\\kmd_render\\target", super::MIRROR_ROOT)
        );
        assert_eq!(
            super::kmd_build_environment(None).unwrap()["LIBCLANG_PATH"],
            super::LIBCLANG_PATH
        );
        assert!(env.keys().all(|key| key == &key.to_ascii_uppercase()));
        assert!(
            super::kmd_build_environment(Some(std::collections::HashMap::from([
                ("LIBCLANG_PATH".into(), r"C:\first\bin".into()),
                ("libclang_path".into(), r"C:\second\bin".into()),
            ])))
            .is_err()
        );
    }

    #[test]
    fn kmd_install_schema_keeps_native_package_compatibility() {
        let schema = super::schemars::schema_for!(super::WinInstallKmdArgs);
        let object = schema.as_object().unwrap();
        let required = object["required"].as_array().unwrap();
        let properties = object["properties"].as_object().unwrap();
        for field in ["package_dir", "umd_dll", "umd12_dll"] {
            assert!(required.iter().any(|value| value == field));
        }
        for field in ["umd32_dll", "umd12_32_dll"] {
            assert!(properties.contains_key(field));
            assert!(!required.iter().any(|value| value == field));
        }
        let build_schema = super::schemars::schema_for!(super::WinBuildKmdArgs);
        let build = build_schema.as_object().unwrap();
        assert!(build["properties"].as_object().unwrap().contains_key("env"));
        assert!(build.get("required").is_none_or(|required| {
            !required
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "env")
        }));
    }

    /// Copy the real kmd_render version-site files into a temp root and
    /// exercise verify (no_bump), auto-bump, and explicit-bump against them.
    #[test]
    fn kmd_version_bump_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("winmcp-bump-test-{}", std::process::id()));
        let kmd = tmp.join("kmd_render");
        std::fs::create_dir_all(&kmd).unwrap();
        for f in ["driver-version.env", "build.rs", "Cargo.make.toml"] {
            std::fs::copy(
                // The checkout this test was compiled from, not the deployment
                // constant: the test's subject is the bump logic, and the tree it
                // runs from must exist wherever the suite is run.
                format!("{}/../../kmd_render/{f}", env!("CARGO_MANIFEST_DIR")),
                kmd.join(f),
            )
            .unwrap();
        }
        let root = tmp.to_str().unwrap();

        // Verify-only: real tree must be coherent.
        let (old_v, same) = bump_kmd_version_at(root, None, true).unwrap();
        assert_eq!(old_v, same);

        // Auto-bump increments the third component.
        let (from, to) = bump_kmd_version_at(root, None, false).unwrap();
        assert_eq!(from, old_v);
        let f: Vec<u32> = from.split('.').map(|p| p.parse().unwrap()).collect();
        let t: Vec<u32> = to.split('.').map(|p| p.parse().unwrap()).collect();
        assert_eq!((t[0], t[1], t[2], t[3]), (f[0], f[1], f[2] + 1, f[3]));

        // The bumped tree is coherent at the new version; no old remnants.
        let (v2, _) = bump_kmd_version_at(root, None, true).unwrap();
        assert_eq!(v2, to);
        // The single source now carries only the new version, and the bump left
        // the file's comments intact.
        let env = std::fs::read_to_string(kmd.join("driver-version.env")).unwrap();
        assert!(env.contains(&format!("HELIOS_KMD_VERSION={to}")), "{env}");
        assert!(
            !env.contains(&format!("HELIOS_KMD_VERSION={from}")),
            "{env}"
        );
        assert!(env.contains('#'), "bump dropped the file's comments: {env}");
        // A bump must not need to touch any other file. The cross-file coherence
        // gate is kmd_render/build.rs::verify_version_wiring, not this tool, so
        // that a hand-run `cargo make` is covered too; all this asserts is that
        // no other file carries a version literal to keep in step.
        let build_rs = std::fs::read_to_string(kmd.join("build.rs")).unwrap();
        assert!(
            !build_rs.contains(&from),
            "build.rs regained a version literal"
        );
        assert!(
            !build_rs.contains(&to),
            "build.rs regained a version literal"
        );
        let make = std::fs::read_to_string(kmd.join("Cargo.make.toml")).unwrap();
        assert!(
            !make.contains(&to),
            "Cargo.make.toml regained a version literal"
        );
        assert!(make.contains("\"-v\", \"${HELIOS_KMD_VERSION}\""), "{make}");

        // Explicit version.
        let (_, v3) = bump_kmd_version_at(root, Some("30.0.1.2"), false).unwrap();
        assert_eq!(v3, "30.0.1.2");

        // Bad explicit version is rejected.
        assert!(bump_kmd_version_at(root, Some("30.0.1"), false).is_err());

        // A malformed single source is rejected in verify mode, before anything
        // is written — this is the case that used to reach install as
        // FAILED_ADD 0xc0000182.
        std::fs::write(
            kmd.join("driver-version.env"),
            "HELIOS_KMD_VERSION=22.22.176\n",
        )
        .unwrap();
        let err = bump_kmd_version_at(root, None, true).unwrap_err();
        assert!(err.contains("expected 4"), "{err}");
        std::fs::write(kmd.join("driver-version.env"), "# no version here\n").unwrap();
        let err = bump_kmd_version_at(root, None, true).unwrap_err();
        assert!(err.contains("no HELIOS_KMD_VERSION line"), "{err}");

        std::fs::remove_dir_all(&tmp).unwrap();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // One ssh-config decision per process, before anything connects: on a host
    // whose system ssh_config is unreadable, plain `ssh host` exits 255 and
    // `ssh -F ~/.ssh/config host` works. Cached here so the CLI and the server
    // make the same choice.
    host::init_ssh_config();

    // CLI mode: the same primitives the MCP tools expose, driven from a shell so
    // CI scripts, humans and shell-only agents can use them too.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(|a| a == "--cli").unwrap_or(false) {
        let code = cli::run(argv[1..].to_vec()).await;
        std::process::exit(code);
    }

    // Drop any stale SSH ControlMaster so the first build picks up the current
    // machine environment (PATH/vars updated by recent toolchain installs).
    let _ = Command::new("ssh")
        .args(["-O", "exit", SSH_HOST])
        .output()
        .await;

    let service = WinHost::new()
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
}

#[tool_handler]
impl ServerHandler for WinHost {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Runs commands and cargo/cargo-make builds on the Helios win11 dev VM. \
                 The project source is shared at Z:\\ (identical to the Linux tree), so \
                 edit files on Linux and build here."
                    .to_string(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

}
