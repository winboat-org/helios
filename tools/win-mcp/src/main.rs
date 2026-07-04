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

/// Shared project root on the Windows side (the Z: drive maps the Linux tree).
const PROJECT_DRIVE: &str = "Z:\\";
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
/// mingw-w64 (WinLibs UCRT gcc 16.1) bin dir — the RECOMMENDED venus toolchain
/// (icd/win-build/mingw-native.ini). gcc compiles venus's GNU-isms natively and
/// builds straight from Z:\. Installed via `winget install BrechtSanders.WinLibs.POSIX.UCRT`.
const MINGW_BIN: &str = "C:\\Users\\Rupansh\\AppData\\Local\\Microsoft\\WinGet\\Packages\\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\\mingw64\\bin";
/// MSBuild used for Looking Glass IDD's WDK/Visual Studio solution.
const MSBUILD: &str =
    "C:\\Program Files\\Microsoft Visual Studio\\2022\\Community\\Msbuild\\Current\\Bin\\MSBuild.exe";

#[derive(Clone)]
pub struct WinHost {
    tool_router: ToolRouter<WinHost>,
}

struct ExecOutput {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    timed_out: bool,
}

/// Encode a PowerShell script as base64 of its UTF-16LE bytes, the input format
/// for `powershell -EncodedCommand`. This avoids all shell quoting concerns.
fn encode_powershell(script: &str) -> String {
    let utf16le: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(utf16le)
}

/// Escape a string for a PowerShell single-quoted literal (double the quotes).
fn ps_single_quote(s: &str) -> String {
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
fn clean_stderr(s: &str) -> String {
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
    /// Crate directory relative to the project root, e.g. "kmd" or "icd".
    /// The working directory becomes Z:\<crate_dir>.
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
struct WinInstallUmdArgs {
    /// Flags passed through to `tools\hotplug-helios-umd.ps1`. Empty = the script's
    /// default ProgramData hotplug (leaves the adapter running; new D3D processes
    /// pick up the DLL, existing ones keep the old one until the device restarts).
    /// Common sets: ["-PlanOnly"] (dry run); ["-KillUmdUsers","-RestartDevice",
    /// "-NoProbe"] (force existing UMD users + an adapter restart to load the new
    /// DLL now). See HELIOS_DRIVER_DEPLOYMENT.md.
    #[serde(default)]
    args: Vec<String>,
    /// Timeout in seconds. Defaults to 600.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

fn ps_join_path(root: &str, rel: &str) -> String {
    if root.ends_with(['\\', '/']) {
        format!("{root}{rel}")
    } else {
        format!("{root}\\{rel}")
    }
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
        description = "Sync the project to the local build mirror and run cargo (or cargo make) there. The Z:\\ share cannot host cargo/wdk build IO (OS error 87), so this robocopy-mirrors Z:\\ -> C:\\Users\\Rupansh\\helios-vgpu (excluding target/, all .git, and the vendored Mesa submodule at icd/mesa — Mesa is a meson/C ICD built separately via win_meson straight from the share, not through this mirror) and builds inside the mirror with LIBCLANG_PATH set for bindgen. Edit sources on the Linux/Z:\\ side — the mirror is re-synced on every call. crate_dir is relative to the project root (e.g. \"kmd\"); args is the cargo argv (e.g. [\"make\",\"--makefile\",\"Cargo.make.toml\"] or [\"build\"])."
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
            "robocopy {PROJECT_DRIVE} {MIRROR_ROOT} /MIR /XJ /XD target .git \"{MESA_SRC}\" dxvk dxvk-research-only vkd3d-proton virtio-research-only-3d windows-driver-docs-research-only /NFL /NDL /NJH /NJS /NP /R:1 /W:1\n\
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
        description = "Build the DXVK-helios C++ engine (the UMD's render backend) on win11 with the clang-cl (MSVC ABI) toolchain. Mirrors the source Z:\\dxvk-helios -> the local git checkout C:\\Users\\Rupansh\\dxvk-helios (the meson build reads the LOCAL copy, NOT the Z:\\ share) with robocopy, then runs meson in the pre-configured build dir C:\\Users\\Rupansh\\dxvk-build. CRITICAL and the reason this tool exists: it prepends LLVM (clang-cl) to PATH BEFORE calling vcvars64 (which supplies MSVC lib.exe/link.exe). The reverse order silently drops the MSVC archiver because cmd expands %PATH% at PARSE time, and the archive step then fails 'CreateProcess failed'. Empty `args` defaults to `compile -C <build dir>` (the common rebuild-after-edit). After this, relink the UMD with `win_cargo crate_dir:\"umd\" args:[\"build\"]` (its build.rs reruns on the changed .a archives), then deploy with win_install_umd. Edit DXVK sources on the Linux/Z:\\ side; the mirror re-syncs on every call."
    )]
    async fn win_dxvk(&self, Parameters(a): Parameters<WinDxvkArgs>) -> String {
        let meson_args = if a.args.is_empty() {
            format!("compile -C {DXVK_BUILD}")
        } else {
            a.args.join(" ")
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
             cmd /c 'set \"PATH={LLVM_BIN};%PATH%\" && call \"{VCVARS}\" && meson {meson_args}'"
        );
        match run_ssh(&command, None, &HashMap::new(), a.timeout_secs.unwrap_or(1800)).await {
            Ok(o) => format_output(&o),
            Err(e) => format!("error launching ssh: {e}"),
        }
    }

    #[tool(
        description = "Install/hotplug the Helios UMD (helios_umd.dll) on win11 by running tools\\hotplug-helios-umd.ps1 via `powershell -NoProfile -ExecutionPolicy Bypass -File`. The -ExecutionPolicy Bypass is REQUIRED: the VM's machine ExecutionPolicy is Restricted, so invoking the .ps1 any other way silently no-ops (no output, no deploy). Build the UMD first: `win_cargo crate_dir:\"umd\" args:[\"build\"]`. The script copies the build to C:\\ProgramData\\HeliosUmd, rewrites the display software key (UserModeDriverName), syncs the active DriverStore copy, verifies the destination SHA256 against the source, and prints final state. Pass script flags via `args`: [] = default ProgramData hotplug; [\"-PlanOnly\"] = dry run; [\"-KillUmdUsers\",\"-RestartDevice\",\"-NoProbe\"] = force existing UMD users off + an adapter restart so the new DLL loads immediately. NOTE: dxgkrnl caches the UMD path at DEVICE start, so without -RestartDevice (or a reboot) already-running processes keep the old DLL. See HELIOS_DRIVER_DEPLOYMENT.md."
    )]
    async fn win_install_umd(&self, Parameters(a): Parameters<WinInstallUmdArgs>) -> String {
        let extra = a.args.join(" ");
        // Nested `powershell -ExecutionPolicy Bypass -File` — REQUIRED because the
        // machine ExecutionPolicy is Restricted, so a bare `& script.ps1` (which is
        // effectively what run_ssh's outer -EncodedCommand shell would do) silently
        // fails to run the .ps1. Running it as a child process also makes the
        // script's Write-Host output visible (captured as the child's stdout),
        // unlike same-process Write-Host which goes to an uncaptured host stream.
        let command = format!(
            "& powershell -NoProfile -ExecutionPolicy Bypass -File '{PROJECT_DRIVE}tools\\hotplug-helios-umd.ps1' {extra}"
        );
        match run_ssh(&command, None, &HashMap::new(), a.timeout_secs.unwrap_or(600)).await {
            Ok(o) => format_output(&o),
            Err(e) => format!("error launching ssh: {e}"),
        }
    }
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

#[tokio::main]
async fn main() -> Result<()> {
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
