//! Generic multi-host execution primitives.
//!
//! The rest of this server speaks to ONE host (the `win11` dev VM) through the
//! `Z:\` share model, and it works because that model holds: the tree is shared,
//! sources are edited on the Linux side, and every command is a synchronous
//! `ssh` call. This module answers the questions that model does not:
//!
//!   * **which host** — the dev VM and the `firstheberg2-win` build slave have
//!     different users, workspaces, PowerShells and privilege rules;
//!   * **under which account** — a D3D12/GPU probe run in session 0 produces
//!     *fake* results, while a driver install run in an interactive session can
//!     be killed by a console control event, so the principal is part of the
//!     correctness of the call, not a detail of it;
//!   * **detached or not** — a driver build is 8 minutes and an ssh keepalive
//!     drop kills whatever the session started, so long work has to become a
//!     scheduled task that outlives the connection;
//!   * **where the log went** — a detached task's stdout is discarded unless it
//!     is redirected, so every task this module starts gets a log path back;
//!   * **whether the bytes arrived** — every push/pull is sha256-verified on
//!     both ends rather than trusted.
//!
//! These are the distilled failures of a long session; each one is encoded here
//! so the next agent does not rediscover it. The pure builders (`ssh_args`,
//! `task_wrapper`, `register_task_script`, `shell_body`) carry unit tests,
//! because they are where the escaping bugs live.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rmcp::schemars::{self, JsonSchema};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::{clean_stderr, encode_powershell, ps_single_quote, ExecOutput};

/// A PowerShell single-quoted literal: escape, then wrap. Every value embedded
/// in a payload goes through this — forgetting the wrap (rather than the escape)
/// is the bug the unit tests below pin down.
pub fn lit(value: &str) -> String {
    format!("'{}'", ps_single_quote(value))
}

/// `'<path>' '<arg>' …` — the argument list for `powershell -File`, with the
/// path quoted so a directory with a space works.
///
/// ⛔ ONE TOKEN PER ARGUMENT. `powershell -File` does not parse a comma list or a
/// quoted "flag value" pair: `'-Role vm'` arrives as a single argument and fails
/// with "A parameter cannot be found that matches parameter name 'Role vm'".
/// Callers pass `["-Role", "vm"]`, never `["-Role vm"]`.
pub fn quoted_path_with_args(path: &str, args: &[String]) -> String {
    let mut out = lit(path);
    for a in args {
        out.push(' ');
        out.push_str(&lit(a));
    }
    out
}

/// Extra PATH entries prepended for a build-purpose task on the build slave.
///
/// ⛔ `C:\msys64\usr\bin` is deliberately ABSENT. Its `git.exe` shadows Windows
/// Git and reports a Windows-path repository as "Not a git repository", which a
/// provenance check then (correctly) reads as a dirtied source tree. That cost a
/// full diagnose cycle; the MSYS2 toolchain is reached through `ucrt64`, which
/// is already on the machine PATH.
const SLAVE_BUILD_PATH: &str = r"C:\helios\llvm-22.1.8\bin;C:\helios\cargo\bin;C:\Program Files\PowerShell\7;";
const VM_BUILD_PATH: &str = r"C:\Users\Tibix\.cargo\bin;C:\Program Files\PowerShell\7;";

/// Idempotent `safe.directory` setup for SYSTEM, which is not the owner of the
/// build tree and is therefore refused by git's ownership check.
///
/// ⛔ `git config --global --add safe.directory '*'` is not idempotent: it
/// appends a duplicate on every build, and the duplicates are visible in
/// `git config --get-all` (six after six builds). Check first.
const SAFE_DIRECTORY_SNIPPET: &str = "if (@(& git config --global --get-all safe.directory 2>$null) -notcontains '*') { & git config --global --add safe.directory '*' 2>&1 | Out-Null }\n";

/// What a task started by this module is allowed to assume about its session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    /// Compilation / packaging. Runs as SYSTEM in session 0, with
    /// `safe.directory` set (SYSTEM is not the owner of the tree) and a PATH
    /// that cannot pick up MSYS2's git.
    Build,
    /// Driver or package installation. Runs as SYSTEM so no console control
    /// event from an interactive session can interrupt a display-driver swap
    /// (`STATUS_CONTROL_C_EXIT` at `pnputil /add-driver` is the recorded case).
    Install,
    /// Anything that must observe or drive the real desktop: D3D12 probes, DWM,
    /// benchmarks. Runs as the logged-on interactive user and REFUSES to start
    /// in session 0, because a GPU probe launched from session 0 does not fail —
    /// it reports a plausible, wrong answer.
    Desktop,
    /// Generic privileged work as SYSTEM.
    System,
}

impl Purpose {
    fn is_system(self) -> bool {
        !matches!(self, Purpose::Desktop)
    }

    /// Parse a `--purpose` value. Rejects anything unknown rather than guessing:
    /// the whole point of the type is that the session rules are never implicit.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "build" => Ok(Purpose::Build),
            "install" => Ok(Purpose::Install),
            "desktop" => Ok(Purpose::Desktop),
            "system" => Ok(Purpose::System),
            other => bail!("unknown purpose '{other}' (build|install|desktop|system)"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Build => "build",
            Purpose::Install => "install",
            Purpose::Desktop => "desktop",
            Purpose::System => "system",
        }
    }
}

/// One addressable Windows host.
#[derive(Clone, Debug)]
pub struct HostSpec {
    /// Stable CLI/MCP name: `vm` or `slave`.
    pub name: &'static str,
    /// `ssh` config alias (overridable by env for a relocated deployment).
    pub ssh_target_env: &'static str,
    pub ssh_target_default: &'static str,
    /// Default working directory on that host.
    pub workspace: &'static str,
    /// Where shipped scripts and task wrappers are staged.
    pub staging: &'static str,
    /// PowerShell used to run payloads (Windows PowerShell on the guest — pwsh
    /// 7 is slave-only — and pwsh 7 on the slave).
    pub powershell: &'static str,
    /// The interactive desktop user, for `Purpose::Desktop`.
    pub interactive_user: &'static str,
    /// PATH prefix for `Purpose::Build`.
    pub build_path: &'static str,
    /// Default purpose when the caller does not name one. The `vm` default is
    /// Desktop (the dangerous mistake there is silently fake GPU results); the
    /// slave default is Build (it has no desktop worth probing).
    pub default_purpose: Purpose,
}

impl HostSpec {
    pub fn ssh_target(&self) -> String {
        std::env::var(self.ssh_target_env).unwrap_or_else(|_| self.ssh_target_default.to_string())
    }
    fn powershell_arg(&self) -> String {
        lit(self.powershell)
    }
}

const VM: HostSpec = HostSpec {
    name: "vm",
    ssh_target_env: "HELIOS_SSH_VM",
    ssh_target_default: "win",
    workspace: r"C:\Users\Tibix",
    staging: r"C:\Users\Tibix\winrun",
    powershell: r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
    interactive_user: "tibix",
    build_path: VM_BUILD_PATH,
    default_purpose: Purpose::Desktop,
};

const SLAVE: HostSpec = HostSpec {
    name: "slave",
    ssh_target_env: "HELIOS_SSH_SLAVE",
    ssh_target_default: "firstheberg2-win",
    workspace: r"C:\src",
    staging: r"C:\src\winrun",
    powershell: r"C:\Program Files\PowerShell\7\pwsh.exe",
    interactive_user: "Docker",
    build_path: SLAVE_BUILD_PATH,
    default_purpose: Purpose::Build,
};

pub fn host_spec(name: &str) -> Result<&'static HostSpec> {
    match name {
        "vm" => Ok(&VM),
        "slave" => Ok(&SLAVE),
        other => bail!("unknown host '{other}' (expected 'vm' or 'slave')"),
    }
}

pub fn host_names() -> [&'static str; 2] {
    ["vm", "slave"]
}

// ── ssh invocation ──────────────────────────────────────────────────────────

/// The user's ssh config, when the SYSTEM one is unreadable.
///
/// ⛔ This is not paranoia. In a container whose `/etc/ssh/ssh_config.d` is not
/// owned by root, OpenSSH 10.2 refuses the *system* config file and exits 255
/// with a single warning line and no other diagnostic — so `ssh host` fails
/// while `ssh -F ~/.ssh/config host` succeeds. Passing `-F` explicitly skips the
/// system file. Detected once, at startup, and cached.
static SSH_CONFIG: OnceLock<Option<String>> = OnceLock::new();

/// Detect (once) whether ssh needs an explicit `-F <user config>`.
pub fn init_ssh_config() {
    SSH_CONFIG.get_or_init(|| {
        if let Ok(p) = std::env::var("HELIOS_SSH_CONFIG") {
            return if p.is_empty() { None } else { Some(p) };
        }
        let home = std::env::var("HOME").ok()?;
        let cfg = format!("{home}/.ssh/config");
        if !Path::new(&cfg).is_file() {
            return None;
        }
        // `ssh -G <target>` parses config and prints the effective config; a
        // rejected system file shows up as this exact warning on stderr.
        for target in ["vm", "slave"] {
            let spec = host_spec(target).ok()?;
            let out = std::process::Command::new("ssh")
                .args(["-G", &spec.ssh_target()])
                .output()
                .ok()?;
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("Bad owner or permissions") {
                return Some(cfg);
            }
        }
        None
    });
}

/// The `-F` config path in use, if any — printed by `--cli hosts` because a
/// wrong decision here is otherwise invisible until a connection fails.
pub fn ssh_config_in_use() -> Option<String> {
    SSH_CONFIG.get().cloned().flatten()
}

/// ssh argv (without the remote command). Pure given the cached `-F` decision.
pub fn ssh_args(target: &str) -> Vec<String> {
    let mut args: Vec<String> = vec!["-o".into(), "BatchMode=yes".into()];
    if let Some(Some(cfg)) = SSH_CONFIG.get() {
        args.push("-F".into());
        args.push(cfg.clone());
    }
    args.push(target.to_string());
    args
}

fn scp_args() -> Vec<String> {
    let mut args: Vec<String> = vec!["-q".into(), "-o".into(), "BatchMode=yes".into()];
    if let Some(Some(cfg)) = SSH_CONFIG.get() {
        args.push("-F".into());
        args.push(cfg.clone());
    }
    args
}

fn remote_scp_path(path: &str) -> String {
    path.replace('\\', "/")
}

// ── command body construction ───────────────────────────────────────────────

/// The PowerShell body that sets the environment, changes directory and runs a
/// command, propagating the native exit code.
pub fn shell_body(command: &str, cwd: &str, env: &[(String, String)], purpose: Purpose, spec: &HostSpec) -> String {
    let mut script = String::from(
        "$ProgressPreference = 'SilentlyContinue';\n$ErrorActionPreference = 'Continue';\n",
    );
    if purpose == Purpose::Desktop {
        script.push_str(
            "if ((Get-Process -Id $PID).SessionId -eq 0) {\n  \
             Write-Output 'winrun: refusing purpose=desktop in session 0 (GPU/desktop results would be fake)';\n  Write-Error 'winrun: refusing purpose=desktop in session 0';\n  exit 87\n}\n",
        );
    }
    if purpose == Purpose::Build {
        script.push_str(SAFE_DIRECTORY_SNIPPET);
        script.push_str(&format!(
            "$env:PATH = {} + [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User');\n",
            lit(spec.build_path)
        ));
    }
    for (k, v) in env {
        script.push_str(&format!("$env:{k} = {};\n", lit(v)));
    }
    script.push_str(&format!("Set-Location -LiteralPath {};\n", lit(cwd)));
    script.push_str(command);
    script.push_str("\n$c = $LASTEXITCODE; if ($null -eq $c) { $c = 0 }; exit $c\n");
    script
}

/// Run a PowerShell body on a host and capture stdout/stderr/exit code.
pub async fn run_body(spec: &HostSpec, script: &str, timeout_secs: u64) -> Result<ExecOutput> {
    let encoded = encode_powershell(script);
    let mut cmd = Command::new("ssh");
    cmd.args(ssh_args(&spec.ssh_target()))
        .arg("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-EncodedCommand")
        .arg(&encoded)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().context("spawning ssh")?;
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
        Err(_) => Ok(ExecOutput {
            stdout: String::new(),
            stderr: format!("command exceeded timeout of {timeout_secs}s and was killed"),
            code: None,
            timed_out: true,
        }),
    }
}

/// Run an ad-hoc PowerShell snippet with cwd/env/purpose applied.
pub async fn run_command(
    spec: &HostSpec,
    command: &str,
    cwd: Option<&str>,
    env: &[(String, String)],
    purpose: Purpose,
    timeout_secs: u64,
) -> Result<ExecOutput> {
    let body = shell_body(
        command,
        cwd.unwrap_or(spec.workspace),
        env,
        purpose,
        spec,
    );
    run_body(spec, &body, timeout_secs).await
}

// ── artifacts ───────────────────────────────────────────────────────────────

/// Result of a verified transfer.
#[derive(Debug, Clone)]
pub struct TransferReport {
    pub local: String,
    pub remote: String,
    pub sha256: String,
    pub size: u64,
    pub verified: bool,
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(&bytes)).to_ascii_uppercase())
}

/// Remote sha256 + size for one file, as reported by the host itself.
pub async fn remote_hash(spec: &HostSpec, remote: &str) -> Result<(String, u64)> {
    let cmd = format!(
        "$ErrorActionPreference='Stop'\n\
         try {{\n\
           $f = Get-Item -LiteralPath {p} -ErrorAction Stop\n\
           Write-Output ('WINRUN_SHA=' + (Get-FileHash -LiteralPath {p} -Algorithm SHA256).Hash + ';' + $f.Length)\n\
         }} catch {{\n\
           Write-Output ('WINRUN_SHA_ERROR=' + $_.Exception.Message)\n\
           exit 2\n\
         }}",
        p = lit(remote)
    );
    let out = run_body(spec, &cmd, 300).await?;
    let line = out
        .stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("WINRUN_SHA="))
        .ok_or_else(|| {
            let remote_error = out
                .stdout
                .lines()
                .find_map(|l| l.trim().strip_prefix("WINRUN_SHA_ERROR="))
                .unwrap_or("");
            anyhow::anyhow!(
                "cannot hash {remote} on {}: {}{}",
                spec.name,
                remote_error,
                if out.stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(" ({})", out.stderr.trim())
                }
            )
        })?;
    let (hash, size) = line
        .split_once(';')
        .ok_or_else(|| anyhow::anyhow!("malformed hash marker: {line}"))?;
    Ok((hash.to_ascii_uppercase(), size.trim().parse::<u64>()?))
}

async fn scp(from: &str, to: &str) -> Result<()> {
    let mut cmd = Command::new("scp");
    cmd.args(scp_args())
        .arg(from)
        .arg(to)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = cmd.output().await.context("spawning scp")?;
    if !out.status.success() {
        bail!(
            "scp {from} -> {to} failed: {}",
            clean_stderr(&String::from_utf8_lossy(&out.stderr))
        );
    }
    Ok(())
}

pub async fn ensure_staging(spec: &HostSpec) -> Result<()> {
    let cmd = format!(
        "New-Item -ItemType Directory -Force -Path {} | Out-Null; Write-Output 'WINRUN_STAGING_OK'",
        lit(spec.staging)
    );
    let out = run_body(spec, &cmd, 120).await?;
    if !out.stdout.contains("WINRUN_STAGING_OK") {
        bail!("could not create staging dir {}: {}", spec.staging, out.stderr.trim());
    }
    Ok(())
}

/// Push a local file, verifying the sha256 on both ends.
pub async fn push(spec: &HostSpec, local: &Path, remote: &str) -> Result<TransferReport> {
    let local_sha = sha256_file(local)?;
    let size = std::fs::metadata(local)?.len();
    if let Some(parent) = Path::new(remote).parent() {
        let parent = parent.to_string_lossy().to_string();
        if !parent.is_empty() {
            let cmd = format!("New-Item -ItemType Directory -Force -Path {} | Out-Null", lit(&parent));
            let _ = run_body(spec, &cmd, 120).await?;
        }
    }
    let dest = format!("{}:{}", spec.ssh_target(), remote_scp_path(remote));
    scp(&local.to_string_lossy(), &dest).await?;
    let (remote_sha, remote_size) = remote_hash(spec, remote).await?;
    let verified = remote_sha.eq_ignore_ascii_case(&local_sha) && remote_size == size;
    if !verified {
        bail!(
            "transfer verification failed for {remote}: local {local_sha}/{size} vs remote {remote_sha}/{remote_size}"
        );
    }
    Ok(TransferReport {
        local: local.display().to_string(),
        remote: remote.to_string(),
        sha256: local_sha,
        size,
        verified,
    })
}

/// Pull a remote file, verifying the sha256 on both ends.
pub async fn pull(spec: &HostSpec, remote: &str, local: &Path) -> Result<TransferReport> {
    let (remote_sha, size) = remote_hash(spec, remote).await?;
    if let Some(parent) = local.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let src = format!("{}:{}", spec.ssh_target(), remote_scp_path(remote));
    scp(&src, &local.to_string_lossy()).await?;
    let local_sha = sha256_file(local)?;
    let verified = local_sha.eq_ignore_ascii_case(&remote_sha) && std::fs::metadata(local)?.len() == size;
    if !verified {
        bail!(
            "pull verification failed for {remote}: remote {remote_sha}/{size} vs local {local_sha}/{}",
            std::fs::metadata(local)?.len()
        );
    }
    Ok(TransferReport {
        local: local.display().to_string(),
        remote: remote.to_string(),
        sha256: local_sha,
        size,
        verified,
    })
}

// ── detached tasks ──────────────────────────────────────────────────────────

/// The wrapper a scheduled task runs: redirects the payload to a log and
/// appends the exit code as a marker, so a detached run is still readable.
///
/// Pure — unit tested, because the escaping and the log path are what go wrong.
pub fn task_wrapper(payload: &str, log: &str, purpose: Purpose, spec: &HostSpec) -> String {
    let mut s = String::from("$ErrorActionPreference='Continue'\n$ProgressPreference='SilentlyContinue'\n");
    if purpose == Purpose::Desktop {
        s.push_str(&format!(
            "if ((Get-Process -Id $PID).SessionId -eq 0) {{\n  \
             Add-Content -LiteralPath {} -Value 'winrun: refusing purpose=desktop in session 0';\n  exit 87\n}}\n",
            lit(log)
        ));
    }
    if purpose == Purpose::Build {
        s.push_str(SAFE_DIRECTORY_SNIPPET);
        s.push_str(&format!(
            "$env:PATH = '{}' + [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')\n",
            ps_single_quote(spec.build_path)
        ));
    }
    s.push_str(&format!(
        "& {} -NoProfile -ExecutionPolicy Bypass -File {} *> {}\n",
        spec.powershell_arg(),
        payload,
        lit(log)
    ));
    s.push_str(&format!(
        "$c = $LASTEXITCODE; if ($null -eq $c) {{ $c = 0 }}\nAdd-Content -LiteralPath {} -Value ('WINRUN_EXIT=' + $c)\nexit $c\n",
        lit(log)
    ));
    s
}

/// The encoded snippet that registers and starts the task under the principal
/// its purpose demands. Pure.
pub fn register_task_script(spec: &HostSpec, name: &str, wrapper: &str, purpose: Purpose) -> String {
    let principal = if purpose.is_system() {
        "New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest".to_string()
    } else {
        format!(
            "New-ScheduledTaskPrincipal -UserId {} -LogonType Interactive -RunLevel Highest",
            lit(spec.interactive_user)
        )
    };
    let name_lit = lit(name);
    let argument = lit(&format!(
        r#"-NoProfile -ExecutionPolicy Bypass -File "{wrapper}""#
    ));
    format!(
        "$ErrorActionPreference='Stop'\n\
         Unregister-ScheduledTask -TaskName {name} -Confirm:$false -ErrorAction SilentlyContinue\n\
         $action = New-ScheduledTaskAction -Execute {exe} -Argument {argument} -WorkingDirectory {wd}\n\
         $principal = {principal}\n\
         Register-ScheduledTask -TaskName {name} -Action $action -Principal $principal -Force | Out-Null\n\
         Start-ScheduledTask -TaskName {name}\n\
         Start-Sleep -Seconds 3\n\
         $t = Get-ScheduledTask -TaskName {name}\n\
         Write-Output ('WINRUN_TASK=' + $t.State)\n",
        name = name_lit,
        exe = lit(spec.powershell),
        argument = argument,
        wd = lit(spec.staging),
        principal = principal,
    )
}

/// A started (or finished) detached task.
#[derive(Debug, Clone)]
pub struct TaskStart {
    pub name: String,
    pub log: String,
    pub state: String,
}

/// Start `payload` (a remote .ps1 path) as a detached scheduled task.
///
/// `payload_args` are appended to the `-File` invocation. Returns the task name
/// and the log path; the caller polls [`task_status`].
pub async fn task_start(
    spec: &HostSpec,
    name: &str,
    payload: &str,
    payload_args: &[String],
    purpose: Purpose,
    log: Option<&str>,
) -> Result<TaskStart> {
    ensure_staging(spec).await?;
    let log = log
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{}\\{}.log", spec.staging, name));
    let payload_cmd = quoted_path_with_args(payload, payload_args);
    let wrapper = task_wrapper(&payload_cmd, &log, purpose, spec);

    let local = std::env::temp_dir().join(format!("winrun-{name}-task.ps1"));
    std::fs::write(&local, wrapper.as_bytes())?;
    let remote_wrapper = format!("{}\\{}-task.ps1", spec.staging, name);
    push(spec, &local, &remote_wrapper).await?;
    let _ = std::fs::remove_file(&local);

    let script = register_task_script(spec, name, &remote_wrapper, purpose);
    let out = run_body(spec, &script, 180).await?;
    let state = out
        .stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("WINRUN_TASK="))
        .unwrap_or("unknown")
        .to_string();
    if state == "unknown" {
        bail!("task registration produced no state: {} {}", out.stdout.trim(), out.stderr.trim());
    }
    Ok(TaskStart {
        name: name.to_string(),
        log,
        state,
    })
}

#[derive(Debug, Clone, Default)]
pub struct TaskStatus {
    pub exists: bool,
    pub state: String,
    pub last_result: Option<i64>,
    pub log_exists: bool,
    pub log_tail: String,
    pub exit_marker: Option<i64>,
}

/// Poll a task's state and log. `log` is the path returned by [`task_start`].
pub async fn task_status(spec: &HostSpec, name: &str, log: &str, tail_lines: usize) -> Result<TaskStatus> {
    let cmd = format!(
        "$ErrorActionPreference='Continue'\n\
         $t = Get-ScheduledTask -TaskName {name} -ErrorAction SilentlyContinue\n\
         if ($null -eq $t) {{ Write-Output 'WINRUN_EXISTS=0' }} else {{\n\
           $i = Get-ScheduledTaskInfo -TaskName {name}\n\
           Write-Output 'WINRUN_EXISTS=1'\n\
           Write-Output ('WINRUN_STATE=' + $t.State)\n\
           Write-Output ('WINRUN_RESULT=' + $i.LastTaskResult)\n\
         }}\n\
         if (Test-Path -LiteralPath {log}) {{\n\
           Write-Output 'WINRUN_LOG=1'\n\
           Write-Output 'WINRUN_TAIL_BEGIN'\n\
           Get-Content -LiteralPath {log} -Tail {n}\n\
           Write-Output 'WINRUN_TAIL_END'\n\
           $m = Select-String -LiteralPath {log} -Pattern 'WINRUN_EXIT=(\\d+)' | Select-Object -Last 1\n\
           if ($m) {{ Write-Output ('WINRUN_EXIT=' + $m.Matches[0].Groups[1].Value) }}\n\
         }} else {{ Write-Output 'WINRUN_LOG=0' }}\n",
        name = lit(name),
        log = lit(log),
        n = tail_lines,
    );
    let out = run_body(spec, &cmd, 300).await?;
    let text = out.stdout;
    let mut st = TaskStatus::default();
    for line in text.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("WINRUN_EXISTS=") {
            st.exists = v.trim() == "1";
        } else if let Some(v) = l.strip_prefix("WINRUN_STATE=") {
            st.state = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix("WINRUN_RESULT=") {
            st.last_result = v.trim().parse().ok();
        } else if let Some(v) = l.strip_prefix("WINRUN_LOG=") {
            st.log_exists = v.trim() == "1";
        } else if let Some(v) = l.strip_prefix("WINRUN_EXIT=") {
            st.exit_marker = v.trim().parse().ok();
        }
    }
    if let (Some(a), Some(b)) = (text.find("WINRUN_TAIL_BEGIN"), text.find("WINRUN_TAIL_END")) {
        if b > a {
            st.log_tail = text[a + "WINRUN_TAIL_BEGIN".len()..b].trim().to_string();
        }
    }
    Ok(st)
}

/// Unregister a task (idempotent).
pub async fn task_kill(spec: &HostSpec, name: &str) -> Result<()> {
    let cmd = format!(
        "Unregister-ScheduledTask -TaskName {} -Confirm:$false -ErrorAction SilentlyContinue; Write-Output 'WINRUN_KILLED'",
        lit(name)
    );
    let out = run_body(spec, &cmd, 120).await?;
    if !out.stdout.contains("WINRUN_KILLED") {
        bail!("could not unregister {}: {}", name, out.stderr.trim());
    }
    Ok(())
}

// ── host introspection ──────────────────────────────────────────────────────

/// Ship and run `payload` (a local .ps1) either synchronously or as a task.
pub async fn run_script(
    spec: &HostSpec,
    local: &Path,
    args: &[String],
    purpose: Purpose,
    detached: Option<&str>,
    log: Option<&str>,
    timeout_secs: u64,
) -> Result<ScriptRun> {
    ensure_staging(spec).await?;
    let name = local
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "script.ps1".to_string());
    let remote = format!("{}\\{}", spec.staging, name);
    let report = push(spec, local, &remote).await?;
    match detached {
        Some(task) => {
            let started = task_start(spec, task, &remote, args, purpose, log).await?;
            Ok(ScriptRun::Task {
                transfer: report,
                task: started,
            })
        }
        None => {
            let command = format!(
                "& {} -NoProfile -ExecutionPolicy Bypass -File {}",
                spec.powershell_arg(),
                quoted_path_with_args(&remote, args)
            );
            let out = run_command(spec, &command, None, &[], purpose, timeout_secs).await?;
            Ok(ScriptRun::Sync {
                transfer: report,
                remote,
                output: out,
            })
        }
    }
}

#[derive(Debug, Clone)]
pub enum ScriptRun {
    Sync {
        transfer: TransferReport,
        remote: String,
        output: ExecOutput,
    },
    Task {
        transfer: TransferReport,
        task: TaskStart,
    },
}

const PREFLIGHT_PS: &str = include_str!("../preflight.ps1");
const STATUS_PS: &str = include_str!("../status.ps1");
const HOSTINFO_PS: &str = include_str!("../hostinfo.ps1");

async fn run_shipped(
    spec: &HostSpec,
    name: &str,
    script: &str,
    args: &[String],
    timeout_secs: u64,
) -> Result<String> {
    ensure_staging(spec).await?;
    let local = std::env::temp_dir().join(format!("winrun-{}-{}", spec.name, name));
    std::fs::write(&local, script.as_bytes())?;
    let remote = format!("{}\\{}", spec.staging, name);
    push(spec, &local, &remote).await?;
    let _ = std::fs::remove_file(&local);
    let command = format!(
        "& {} -NoProfile -ExecutionPolicy Bypass -File {}",
        spec.powershell_arg(),
        quoted_path_with_args(&remote, args)
    );
    let out = run_command(spec, &command, None, &[], Purpose::System, timeout_secs).await?;
    if !out.stdout.contains("WINRUN_JSON_BEGIN") {
        bail!(
            "{} on {} produced no JSON envelope (exit {:?}):\n{}\n{}",
            name,
            spec.name,
            out.code,
            out.stdout.trim(),
            out.stderr.trim()
        );
    }
    Ok(extract_json(&out.stdout))
}

/// Pull the JSON out of the `WINRUN_JSON_BEGIN/END` envelope the host scripts
/// emit, so their own progress output can never corrupt the payload.
pub fn extract_json(stdout: &str) -> String {
    match (stdout.find("WINRUN_JSON_BEGIN"), stdout.find("WINRUN_JSON_END")) {
        (Some(a), Some(b)) if b > a => stdout[a + "WINRUN_JSON_BEGIN".len()..b].trim().to_string(),
        _ => stdout.trim().to_string(),
    }
}

/// One-shot capability report for a host (JSON text).
pub async fn preflight(spec: &HostSpec) -> Result<String> {
    run_shipped(spec, "preflight.ps1", PREFLIGHT_PS, &[], 300).await
}

/// One-shot Helios stack status for a host (JSON text).
pub async fn status(spec: &HostSpec) -> Result<String> {
    run_shipped(
        spec,
        "status.ps1",
        STATUS_PS,
        &["-Role".to_string(), spec.name.to_string()],
        600,
    )
    .await
}

/// Raw host identity and session facts (JSON text).
pub async fn hostinfo(spec: &HostSpec) -> Result<String> {
    run_shipped(spec, "hostinfo.ps1", HOSTINFO_PS, &[], 300).await
}

/// Absolute path of the shipped status script, for humans who want to read it.
pub fn shipped_scripts() -> Vec<(&'static str, &'static str)> {
    vec![
        ("preflight.ps1", PREFLIGHT_PS),
        ("status.ps1", STATUS_PS),
        ("hostinfo.ps1", HOSTINFO_PS),
    ]
}

/// Copy the shipped PowerShell payloads into a directory (used by `--emit`).
pub fn emit_shipped(dir: &Path) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let mut out = Vec::new();
    for (name, body) in shipped_scripts() {
        let p = dir.join(name);
        std::fs::write(&p, body)?;
        out.push(p);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> &'static HostSpec {
        host_spec(name).unwrap()
    }

    #[test]
    fn ssh_args_default_to_no_explicit_config_when_undetected() {
        let args = ssh_args("example-host");
        assert_eq!(args[0], "-o");
        assert_eq!(args[1], "BatchMode=yes");
        assert_eq!(args.last().unwrap(), "example-host");
    }

    #[test]
    fn shell_body_sets_cwd_env_and_task_guard() {
        let body = shell_body(
            "Write-Output hi",
            r"C:\work dir",
            &[("K".into(), "v'q".into())],
            Purpose::Desktop,
            spec("vm"),
        );
        assert!(body.contains(r"Set-Location -LiteralPath 'C:\work dir';"));
        assert!(body.contains(r"$env:K = 'v''q';"));
        // Desktop refuses session 0 rather than reporting fake GPU results.
        assert!(body.contains("SessionId -eq 0"));
        assert!(body.contains("exit 87"));
        assert!(body.trim_end().ends_with("exit $c"));
    }

    #[test]
    fn build_purpose_injects_safe_directory_and_avoids_msys2_git() {
        let body = shell_body("cargo build", r"C:\src", &[], Purpose::Build, spec("slave"));
        assert!(body.contains("safe.directory"));
        assert!(body.contains(r"C:\helios\llvm-22.1.8\bin"));
        assert!(!body.contains(r"msys64\usr\bin"));
    }

    #[test]
    fn build_purpose_on_vm_does_not_use_slave_toolchain() {
        let body = shell_body("cargo build", r"C:\Users\Tibix", &[], Purpose::Build, spec("vm"));
        assert!(!body.contains("helios\\llvm-22.1.8"));
        assert!(body.contains(r"C:\Users\Tibix\.cargo\bin"));
    }

    #[test]
    fn task_wrapper_redirects_to_log_and_records_exit() {
        let w = task_wrapper(
            r"C:\src\winrun\build.ps1 -x 1",
            r"C:\src\winrun\build.log",
            Purpose::Build,
            spec("slave"),
        );
        assert!(w.contains("*>"), "the wrapper must redirect both streams to the log");
        assert!(w.contains("WINRUN_EXIT="));
        assert!(w.contains("safe.directory"));
        // The log path is single-quoted, so spaces survive.
        assert!(w.contains(r"Add-Content -LiteralPath 'C:\src\winrun\build.log'"));
        assert!(w.contains(r"*> 'C:\src\winrun\build.log'"));
    }

    #[test]
    fn desktop_task_wrapper_refuses_session_0() {
        let w = task_wrapper("x", r"C:\l.log", Purpose::Desktop, spec("vm"));
        assert!(w.contains("refusing purpose=desktop in session 0"));
        assert!(w.contains("exit 87"));
    }

    #[test]
    fn system_purposes_register_as_system_and_desktop_as_the_user() {
        let sys = register_task_script(spec("slave"), "T", r"C:\w.ps1", Purpose::Build);
        assert!(sys.contains("UserId 'SYSTEM'"));
        assert!(sys.contains("ServiceAccount"));
        let desk = register_task_script(spec("vm"), "T", r"C:\w.ps1", Purpose::Desktop);
        assert!(desk.contains("UserId 'tibix'"));
        assert!(desk.contains("Interactive"));
        // Registration always returns a state marker the caller can parse.
        assert!(sys.contains("WINRUN_TASK="));
        // The wrapper path is double-quoted INSIDE the -Argument string, which
        // is itself a single-quoted literal.
        assert!(sys.contains(r#"-File "C:\w.ps1""#), "{sys}");
        let spaced = register_task_script(
            spec("vm"),
            "T2",
            r"C:\Users\Some User\winrun\x.ps1",
            Purpose::Desktop,
        );
        assert!(spaced.contains(r#"-File "C:\Users\Some User\winrun\x.ps1""#), "{spaced}");
        assert!(spaced.contains(r"WorkingDirectory 'C:\Users\Tibix\winrun'"), "{spaced}");
    }

    #[test]
    fn paths_with_spaces_are_quoted_everywhere_they_are_embedded() {
        // Regression: ps_single_quote escapes but does not wrap, so a log or
        // payload path with a space used to end the argument and silently break
        // the task (the redirection target became a bare token).
        let payload = quoted_path_with_args(
            r"C:\Program Files\Helios\task.ps1",
            &["-Mode".to_string(), "run".to_string()],
        );
        assert_eq!(
            payload,
            r"'C:\Program Files\Helios\task.ps1' '-Mode' 'run'"
        );
        let w = task_wrapper(&payload, r"C:\Program Files\Helios\task.log", Purpose::Build, spec("slave"));
        assert!(w.contains(r"-File 'C:\Program Files\Helios\task.ps1' '-Mode' 'run'"));
        assert!(w.contains(r"*> 'C:\Program Files\Helios\task.log'"));
        assert!(w.contains(r"Add-Content -LiteralPath 'C:\Program Files\Helios\task.log'"));
    }

    #[test]
    fn file_arguments_are_one_token_each() {
        // Regression: `-File` does not split "flag value" pairs, so a caller that
        // passes "-Role vm" as one element gets a parameter-binding error.
        let argv = quoted_path_with_args(r"C:\s.ps1", &["-Role".into(), "vm".into()]);
        assert_eq!(argv, r"'C:\s.ps1' '-Role' 'vm'");
        assert!(!argv.contains("'-Role vm'"));
    }

    #[test]
    fn json_envelope_is_extracted_from_surrounding_noise() {
        let out = "preparing modules\nWINRUN_JSON_BEGIN\n{\"a\":1}\nWINRUN_JSON_END\ntrailing\n";
        assert_eq!(extract_json(out), "{\"a\":1}");
    }

    #[test]
    fn remote_scp_paths_use_forward_slashes() {
        assert_eq!(remote_scp_path(r"C:\src\winrun\x.ps1"), "C:/src/winrun/x.ps1");
    }

    #[test]
    fn shipped_scripts_are_embedded_and_emit() {
        let names: Vec<&str> = shipped_scripts().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["preflight.ps1", "status.ps1", "hostinfo.ps1"]);
        for (_, body) in shipped_scripts() {
            assert!(body.contains("WINRUN_JSON_BEGIN"), "payload must emit a JSON envelope");
        }
        let dir = std::env::temp_dir().join("winrun-emit-test");
        let written = emit_shipped(&dir).unwrap();
        assert_eq!(written.len(), 3);
    }

    #[test]
    fn sha256_matches_known_vector() {
        let p = std::env::temp_dir().join("winrun-sha-test.bin");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD".to_ascii_uppercase()
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unknown_host_is_an_error_not_a_default() {
        assert!(host_spec("win11").is_err());
        assert!(host_spec("").is_err());
    }
}
