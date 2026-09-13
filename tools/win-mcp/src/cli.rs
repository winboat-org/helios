//! The command-line front end for the host primitives in [`crate::host`].
//!
//! Why the same binary has an MCP mode and a CLI mode: the primitives are the
//! valuable part, and two consumers want them for different reasons. An MCP
//! client calls them as tools; a CI job, a human, or an agent whose harness only
//! gives it a shell calls them as `win-mcp --cli …`. Duplicating the ssh/task
//! logic in a second place is how the two drift, so there is exactly one
//! implementation and this module is a thin argument parser over it.
//!
//! Every subcommand exits non-zero on failure, so `set -e` in a shell script
//! behaves. `--json` prints a machine-readable object for the subset that has
//! one; `status`, `preflight` and `hostinfo` are JSON already and pass through.

use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::host::{
    self, emit_shipped, host_names, host_spec, sha256_file, Purpose, ScriptRun, TransferReport,
};
use crate::ExecOutput;

const USAGE: &str = "\
win-mcp --cli <command> [options]

Hosts: vm (win11 dev VM), slave (firstheberg2-win build slave)
       overridable with HELIOS_SSH_VM / HELIOS_SSH_SLAVE / HELIOS_SSH_CONFIG

Commands:
  hosts                                   list hosts and the ssh target each resolves to
  hostinfo   --host H                     identity + session facts (JSON)
  preflight  --host H                     toolchain/tree/disk/principal readiness (JSON)
  status     --host H                     one-shot Helios stack status (JSON)
  run        --host H --command <ps>      run an ad-hoc PowerShell snippet
  run-script --host H <local.ps1>         push a .ps1 and run it (--task NAME to detach)
  task start --host H --name N --script C:\\path\\x.ps1
  task status --host H --name N [--log C:\\path\\x.log] [--tail 40]
  task kill  --host H --name N
  push       --host H <local> <remote>    verified transfer, both ends hashed
  pull       --host H <remote> <local>    verified transfer, both ends hashed
  emit       --dir <dir>                  write the shipped .ps1 payloads out to read
  sha256     <local>                      local file digest (same function push uses)

Options:
  --purpose build|install|desktop|system  session/principal rules (default per host)
  --cwd <path>                            working directory for run/run-script
  --env K=V                               extra environment (repeatable)
  --arg <value>                           argument for run-script / task start
  --timeout <secs>                        ssh timeout (default 600)
  --json                                  machine-readable output where available
";

#[derive(Debug, Default)]
struct Args {
    flags: Vec<(String, String)>,
    positionals: Vec<String>,
    json: bool,
}

impl Args {
    fn parse(raw: &[String]) -> Result<Self> {
        let mut a = Args::default();
        let mut i = 0;
        while i < raw.len() {
            let tok = &raw[i];
            if tok == "--json" {
                a.json = true;
                i += 1;
                continue;
            }
            if let Some(name) = tok.strip_prefix("--") {
                let value = raw
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--{name} needs a value"))?;
                a.flags.push((name.to_string(), value.clone()));
                i += 2;
                continue;
            }
            a.positionals.push(tok.clone());
            i += 1;
        }
        Ok(a)
    }
    fn opt(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    fn all(&self, name: &str) -> Vec<String> {
        self.flags
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .collect()
    }
    fn host(&self) -> Result<&'static host::HostSpec> {
        host_spec(self.opt("host").unwrap_or("vm"))
    }
    fn purpose(&self, spec: &host::HostSpec) -> Result<Purpose> {
        match self.opt("purpose") {
            Some(p) => Purpose::parse(p),
            None => Ok(spec.default_purpose),
        }
    }
    fn timeout(&self) -> Result<u64> {
        match self.opt("timeout") {
            Some(t) => Ok(t.parse()?),
            None => Ok(600),
        }
    }
    fn env_pairs(&self) -> Result<Vec<(String, String)>> {
        self.all("env")
            .into_iter()
            .map(|kv| {
                kv.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .ok_or_else(|| anyhow::anyhow!("--env expects K=V, got '{kv}'"))
            })
            .collect()
    }
}

/// Map a remote exit code onto a shell exit code: 0 for success, the remote code
/// when it is shell-legal (so a purpose-refusal 87 stays distinguishable), else 1.
fn exit_code(out: &ExecOutput) -> i32 {
    match out.code {
        Some(0) => 0,
        Some(c) if (1..=255).contains(&c) => c,
        _ => 1,
    }
}

fn print_exec(out: &ExecOutput) {
    let code = if out.timed_out {
        "TIMEOUT".to_string()
    } else {
        out.code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
    };
    println!("exit={code}");
    if !out.stdout.trim().is_empty() {
        println!("--- stdout ---\n{}", out.stdout.trim_end());
    }
    if !out.stderr.trim().is_empty() {
        println!("--- stderr ---\n{}", out.stderr.trim_end());
    }
}

fn print_transfer(t: &TransferReport) {
    println!(
        "{} -> {}  {} bytes  sha256={}  verified={}",
        t.local, t.remote, t.size, t.sha256, t.verified
    );
}

/// Returns the process exit code.
pub async fn run(raw: Vec<String>) -> i32 {
    match dispatch(raw).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("win-mcp: {e:#}");
            1
        }
    }
}

async fn dispatch(raw: Vec<String>) -> Result<i32> {
    let Some(command) = raw.first() else {
        println!("{USAGE}");
        return Ok(2);
    };
    // Parse everything AFTER the subcommand: otherwise the subcommand itself is
    // collected as the first positional and every subcommand with a positional
    // argument (run-script <path>, push <local> <remote>, task status) reads it
    // one token to the right.
    let args = Args::parse(&raw[1..])?;
    if command == "--help" || command == "-h" || command == "help" {
        println!("{USAGE}");
        return Ok(0);
    }

    match (command.as_str(), args.positionals.as_slice()) {
        ("hosts", _) => {
            println!(
                "ssh config: {}",
                crate::host::ssh_config_in_use().unwrap_or_else(|| "(system config accepted)".into())
            );
            for name in host_names() {
                let spec = host_spec(name)?;
                println!(
                    "{:<6} ssh={:<18} workspace={:<28} staging={:<30} default_purpose={}",
                    spec.name,
                    spec.ssh_target(),
                    spec.workspace,
                    spec.staging,
                    spec.default_purpose.as_str(),
                );
            }
            Ok(0)
        }

        ("hostinfo", _) => {
            let spec = args.host()?;
            println!("{}", host::hostinfo(spec).await?);
            Ok(0)
        }

        ("preflight", _) => {
            let spec = args.host()?;
            println!("{}", host::preflight(spec).await?);
            Ok(0)
        }

        ("status", _) => {
            let spec = args.host()?;
            println!("{}", host::status(spec).await?);
            Ok(0)
        }

        ("run", _) => {
            let spec = args.host()?;
            let command = args
                .opt("command")
                .ok_or_else(|| anyhow::anyhow!("run needs --command '<powershell>'"))?;
            let purpose = args.purpose(spec)?;
            let out = host::run_command(
                spec,
                command,
                args.opt("cwd"),
                &args.env_pairs()?,
                purpose,
                args.timeout()?,
            )
            .await?;
            print_exec(&out);
            Ok(exit_code(&out))
        }

        ("run-script", rest) => {
            let spec = args.host()?;
            let local = rest
                .first()
                .ok_or_else(|| anyhow::anyhow!("run-script needs a local .ps1 path"))?;
            let purpose = args.purpose(spec)?;
            let task = args.opt("task").map(|s| s.to_string());
            let log = args.opt("log").map(|s| s.to_string());
            let run = host::run_script(
                spec,
                &PathBuf::from(local),
                &args.all("arg"),
                purpose,
                task.as_deref(),
                log.as_deref(),
                args.timeout()?,
            )
            .await?;
            match run {
                ScriptRun::Sync {
                    transfer,
                    remote,
                    output,
                } => {
                    print_transfer(&transfer);
                    println!("ran {remote} as purpose={}", purpose.as_str());
                    print_exec(&output);
                    Ok(exit_code(&output))
                }
                ScriptRun::Task { transfer, task } => {
                    print_transfer(&transfer);
                    println!(
                        "started task {} state={} log={}",
                        task.name, task.state, task.log
                    );
                    println!(
                        "poll with: win-mcp --cli task status --host {} --name {} --log {}",
                        args.opt("host").unwrap_or("vm"),
                        task.name,
                        task.log
                    );
                    Ok(0)
                }
            }
        }

        ("task", rest) => {
            let spec = args.host()?;
            match rest.first().map(|s| s.as_str()).unwrap_or("") {
                "start" => {
                    let name = args
                        .opt("name")
                        .ok_or_else(|| anyhow::anyhow!("task start needs --name"))?;
                    let script = args
                        .opt("script")
                        .ok_or_else(|| anyhow::anyhow!("task start needs --script <remote .ps1>"))?;
                    let purpose = args.purpose(spec)?;
                    let started = host::task_start(
                        spec,
                        name,
                        script,
                        &args.all("arg"),
                        purpose,
                        args.opt("log"),
                    )
                    .await?;
                    println!(
                        "task={} state={} purpose={} log={}",
                        started.name,
                        started.state,
                        purpose.as_str(),
                        started.log
                    );
                    if args.json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "task": started.name,
                                "state": started.state,
                                "log": started.log,
                            })
                        );
                    }
                    Ok(0)
                }
                "status" => {
                    let name = args
                        .opt("name")
                        .ok_or_else(|| anyhow::anyhow!("task status needs --name"))?;
                    let log = args
                        .opt("log")
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("{}\\{name}.log", spec.staging));
                    let tail: usize = args.opt("tail").unwrap_or("40").parse()?;
                    let st = host::task_status(spec, name, &log, tail).await?;
                    if args.json {
                        // serde_json, not format!: a Windows log path carries
                        // backslashes that make hand-built JSON invalid.
                        println!(
                            "{}",
                            serde_json::json!({
                                "exists": st.exists,
                                "state": st.state,
                                "last_result": st.last_result,
                                "log_exists": st.log_exists,
                                "exit_marker": st.exit_marker,
                                "log": log,
                            })
                        );
                    } else {
                        println!(
                            "task={name} exists={} state={} last_result={} log_exists={} exit_marker={}",
                            st.exists,
                            st.state,
                            st.last_result.map(|v| v.to_string()).unwrap_or("-".into()),
                            st.log_exists,
                            st.exit_marker.map(|v| v.to_string()).unwrap_or("-".into())
                        );
                        if !st.log_tail.is_empty() {
                            println!("--- log tail ({log}) ---\n{}", st.log_tail);
                        }
                    }
                    // A MISSING exit marker is NOT success: the desktop guard
                    // refuses before the payload runs, and a task whose principal
                    // never started writes no marker either. Success is exactly
                    // "finished, last result 0, marker 0". Running gets its own code
                    // so a poller can tell "wait" from "failed".
                    if !st.exists {
                        return Ok(1);
                    }
                    if st.state.eq_ignore_ascii_case("running") {
                        return Ok(2);
                    }
                    Ok(if st.exit_marker == Some(0) && st.last_result == Some(0) { 0 } else { 1 })
                }
                "kill" => {
                    let name = args
                        .opt("name")
                        .ok_or_else(|| anyhow::anyhow!("task kill needs --name"))?;
                    host::task_kill(spec, name).await?;
                    println!("killed {name}");
                    Ok(0)
                }
                other => bail!("unknown task action '{other}' (start|status|kill)"),
            }
        }

        ("push", rest) => {
            let spec = args.host()?;
            let (local, remote) = match rest {
                [l, r, ..] => (l, r),
                _ => bail!("push needs <local> <remote>"),
            };
            print_transfer(&host::push(spec, &PathBuf::from(local), remote).await?);
            Ok(0)
        }

        ("pull", rest) => {
            let spec = args.host()?;
            let (remote, local) = match rest {
                [r, l, ..] => (r, l),
                _ => bail!("pull needs <remote> <local>"),
            };
            let report = host::pull(spec, remote, &PathBuf::from(local)).await?;
            // Read the direction the way a human does: source -> destination.
            println!(
                "{} -> {}  {} bytes  sha256={}  verified={}",
                report.remote, report.local, report.size, report.sha256, report.verified
            );
            Ok(0)
        }

        ("emit", _) => {
            let dir = args
                .opt("dir")
                .ok_or_else(|| anyhow::anyhow!("emit needs --dir <dir>"))?;
            for p in emit_shipped(&PathBuf::from(dir))? {
                println!("{}", p.display());
            }
            Ok(0)
        }

        ("sha256", rest) => {
            let local = rest
                .first()
                .ok_or_else(|| anyhow::anyhow!("sha256 needs a local path"))?;
            println!("{}", sha256_file(&PathBuf::from(local))?);
            Ok(0)
        }

        (other, _) => {
            bail!("unknown command '{other}' (try --help)");
        }
    }
}
