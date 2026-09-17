//! Helios vGPU Setup — self-contained Windows installer.
//!
//! The executable carries the whole bundle appended to its own image (see
//! `archive`). At run time it extracts that payload to a staging directory and
//! drives the tested `Install-Helios.ps1` / `Uninstall-Helios.ps1` logic with the
//! in-box Windows PowerShell, streaming output and a `HELIOS-PROGRESS` protocol
//! into the GUI.
//!
//! Rust owns the process, the container, and the GUI. The install logic stays in
//! the one PowerShell payload shared with WinBoat's `-Automatic` flow; there is
//! deliberately no second implementation to drift from it.
//!
//! CLI:
//!   HeliosSetup.exe                              GUI
//!   HeliosSetup.exe --silent [--automatic] [--log PATH]
//!   HeliosSetup.exe --silent --uninstall [--log PATH]
//!   HeliosSetup.exe --bundle <payloadDir> <out.exe>   CI packing step only

#![windows_subsystem = "windows"]

mod archive;
mod gui;

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use std::os::windows::process::CommandExt;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Install,
    Repair,
    Uninstall,
}

/// Events from the worker thread to the GUI, polled on a timer so no message
/// carries a heap pointer.
pub enum Event {
    Log(String),
    Progress(u8, String),
    Done(i32),
}

pub struct Options {
    pub silent: bool,
    pub uninstall: bool,
    pub automatic: bool,
    pub log: Option<PathBuf>,
}

pub struct Payload {
    pub dir: PathBuf,
    pub temporary: bool,
    pub has_install: bool,
}

pub(crate) fn helios_data_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"));
    base.join("Helios")
}

fn powershell() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32\\WindowsPowerShell\\v1.0\\powershell.exe")
}

/// Resolve the directory holding the payload scripts. A self-contained exe
/// extracts to a per-process staging directory; a scripts-only install (the
/// copy kept in ProgramData) is used in place; a developer running the exe from
/// a bundle folder uses the exe's own directory.
pub fn prepare_payload(exe: &Path) -> io::Result<Payload> {
    if archive::is_self_contained(exe) {
        let staging = helios_data_dir().join(format!("setup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&staging);
        archive::extract_self(exe, &staging)?;
        let has_install = staging.join("Install-Helios.ps1").is_file();
        return Ok(Payload { dir: staging, temporary: true, has_install });
    }
    let stored = helios_data_dir();
    if stored.join("Uninstall-Helios.ps1").is_file() {
        return Ok(Payload { dir: stored, temporary: false, has_install: false });
    }
    let dir = exe.parent().unwrap_or(Path::new(".")).to_path_buf();
    let has_install = dir.join("Install-Helios.ps1").is_file();
    Ok(Payload { dir, temporary: false, has_install })
}

pub fn cleanup_payload(payload: &Payload) {
    if payload.temporary {
        let _ = fs::remove_dir_all(&payload.dir);
    }
}

fn build_command(payload: &Payload, op: Op, automatic: bool) -> Command {
    let install = payload.dir.join("Install-Helios.ps1");
    let uninstall = payload.dir.join("Uninstall-Helios.ps1");
    let mut command = Command::new(powershell());
    command
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .creation_flags(CREATE_NO_WINDOW);
    match op {
        Op::Install => {
            command.arg("-File").arg(install);
            if automatic {
                command.arg("-Automatic");
            } else {
                command.arg("-EnableTestSigning").arg("-ReplaceViogpudo");
            }
        }
        Op::Repair => {
            command.arg("-File").arg(install);
            if automatic {
                command.arg("-Automatic").arg("-Repair");
            } else {
                command.arg("-Repair").arg("-EnableTestSigning").arg("-ReplaceViogpudo");
            }
        }
        Op::Uninstall => {
            command.arg("-File").arg(uninstall);
        }
    }
    command
}

/// Spawn the payload and stream merged stdout/stderr as `Event`s. The returned
/// receiver ends with exactly one `Done(exit_code)`. The caller owns cleaning up
/// the payload directory: the GUI keeps one extraction for the whole process and
/// removes it on exit; silent mode removes it after its single run.
pub fn start_worker(payload: Payload, op: Op, automatic: bool) -> Receiver<Event> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let mut command = build_command(&payload, op, automatic);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = tx.send(Event::Log(format!("[setup] could not start PowerShell: {error}")));
                let _ = tx.send(Event::Done(1));
                return;
            }
        };
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let tx_out = tx.clone();
        let out_thread = thread::spawn(move || pump(stdout, tx_out));
        let tx_err = tx.clone();
        let err_thread = thread::spawn(move || pump(stderr, tx_err));
        let _ = out_thread.join();
        let _ = err_thread.join();
        let code = child.wait().ok().and_then(|status| status.code()).unwrap_or(1);
        let _ = tx.send(Event::Done(code));
    });
    rx
}

fn pump<R: std::io::Read>(reader: R, tx: Sender<Event>) {
    for line in BufReader::new(reader).split(b'\n') {
        let Ok(bytes) = line else { break };
        let text = decode_console(&bytes);
        if let Some((percent, message)) = parse_progress(&text) {
            let _ = tx.send(Event::Progress(percent, message));
        } else {
            let _ = tx.send(Event::Log(text));
        }
    }
}

/// A `-File` payload writes with the console code page, not necessarily UTF-8.
/// Decode as UTF-8 when it is valid and fall back to the ANSI code page, so an
/// ASCII progress marker or a non-ASCII path is never mangled.
fn decode_console(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.trim_end_matches('\r').to_string();
    }
    use windows_sys::Win32::Globalization::MultiByteToWideChar;
    unsafe {
        let length =
            MultiByteToWideChar(0, 0, bytes.as_ptr(), bytes.len() as i32, std::ptr::null_mut(), 0);
        let mut wide = vec![0u16; length.max(0) as usize];
        MultiByteToWideChar(0, 0, bytes.as_ptr(), bytes.len() as i32, wide.as_mut_ptr(), length);
        String::from_utf16_lossy(&wide).trim_end_matches('\r').to_string()
    }
}

fn parse_progress(line: &str) -> Option<(u8, String)> {
    let rest = line.trim_start().strip_prefix("HELIOS-PROGRESS")?;
    let mut parts = rest.trim_start().splitn(2, ' ');
    let percent = parts.next()?.parse::<i32>().ok()?.clamp(0, 100) as u8;
    let message = parts.next().unwrap_or("").to_string();
    Some((percent, message))
}

fn write_summary(code: i32, log: &Path) {
    // Attach to the caller's console so a CLI user is told the external step.
    let text = match code {
        0 => "Helios: completed.".to_string(),
        3010 => "Helios: a reboot is required to finish. Restart Windows, then re-run if prompted."
            .to_string(),
        2 => "Helios: this stored installer can only uninstall; re-run with --uninstall.".to_string(),
        other => format!("Helios: failed (exit code {other}). Log: {}", log.display()),
    };
    gui::console_line(&text);
}

fn run_silent(exe: &Path, options: &Options) -> i32 {
    let payload = match prepare_payload(exe) {
        Ok(payload) => payload,
        Err(error) => {
            eprintln!("Helios: could not prepare the payload: {error}");
            return 1;
        }
    };
    if !options.uninstall && !payload.has_install {
        let log = options
            .log
            .clone()
            .unwrap_or_else(|| helios_data_dir().join("logs").join("setup.log"));
        if let Some(parent) = log.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&log, "Helios: this stored installer can only uninstall; re-run with --uninstall.\r\n");
        cleanup_payload(&payload);
        write_summary(2, &log);
        return 2;
    }
    let op = if options.uninstall {
        Op::Uninstall
    } else if options.automatic {
        // Let the payload's -Automatic mode choose between a fresh install and
        // the post-reboot Verify/Complete step from the recorded state; a
        // -Repair run would reinstall and never reach `finished`.
        Op::Install
    } else if payload.has_install && helios_data_dir().join("install-state.json").is_file() {
        Op::Repair
    } else {
        Op::Install
    };
    let log = options.log.clone().unwrap_or_else(|| {
        let dir = helios_data_dir().join("logs");
        let _ = fs::create_dir_all(&dir);
        dir.join("setup.log")
    });
    let file = match File::create(&log) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("Helios: could not create {}: {error}", log.display());
            cleanup_payload(&payload);
            return 1;
        }
    };
    let stderr = file.try_clone();
    let mut command = build_command(&payload, op, options.automatic);
    command.stdout(Stdio::from(file));
    if let Ok(stderr) = stderr {
        command.stderr(Stdio::from(stderr));
    }
    let code = match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("Helios: could not run the payload: {error}");
            cleanup_payload(&payload);
            return 1;
        }
    };
    cleanup_payload(&payload);
    write_summary(code, &log);
    code
}

fn parse_args() -> (Options, Option<(PathBuf, PathBuf)>) {
    let argv: Vec<String> = std::env::args().collect();
    let mut options = Options { silent: false, uninstall: false, automatic: false, log: None };
    let mut bundle = None;
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--silent" | "-s" => options.silent = true,
            "--uninstall" | "/uninstall" => options.uninstall = true,
            "--automatic" | "/automatic" => options.automatic = true,
            "--log" | "/log" if i + 1 < argv.len() => {
                options.log = Some(PathBuf::from(&argv[i + 1]));
                i += 1;
            }
            "--bundle" if i + 2 < argv.len() => {
                bundle = Some((PathBuf::from(&argv[i + 1]), PathBuf::from(&argv[i + 2])));
                i += 2;
            }
            "--bundle" => {
                // A packaging misconfiguration must fail, not fall through to the
                // GUI/silent install path with the operands silently ignored.
                eprintln!("Helios: --bundle requires <payloadDir> <outExe>");
                std::process::exit(2);
            }
            _ => {}
        }
        i += 1;
    }
    (options, bundle)
}

fn main() {
    let (options, bundle) = parse_args();
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("HeliosSetup.exe"));

    if let Some((payload_dir, out)) = bundle {
        match archive::build_bundle(&exe, &payload_dir, &out) {
            Ok(()) => {
                gui::console_line(&format!("Helios: packed {} -> {}", payload_dir.display(), out.display()));
            }
            Err(error) => {
                gui::console_line(&format!("Helios: packing failed: {error}"));
                std::process::exit(1);
            }
        }
        return;
    }

    if options.silent {
        let code = run_silent(&exe, &options);
        std::process::exit(code);
    }

    std::process::exit(gui::run(&exe, options.automatic));
}
