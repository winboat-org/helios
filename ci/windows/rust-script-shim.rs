use std::collections::hash_map::DefaultHasher;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("helios rust-script shim: {message}");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let real_executable = match env::var_os("HELIOS_RUST_SCRIPT_REAL") {
        Some(path) if Path::new(&path).is_file() => path,
        _ => return fail("HELIOS_RUST_SCRIPT_REAL does not name rust-script.exe"),
    };

    let arguments: Vec<OsString> = env::args_os().skip(1).collect();
    let script_index = arguments.iter().position(|argument| {
        let path = Path::new(argument);
        path.extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
            && path.is_file()
    });

    // Preserve version checks, cache management, and future non-script modes.
    let Some(script_index) = script_index else {
        return run(&real_executable, &arguments);
    };

    let source = {
        let path = Path::new(&arguments[script_index]);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            match env::current_dir() {
                Ok(directory) => directory.join(path),
                Err(error) => return fail(format!("cannot resolve script path: {error}")),
            }
        }
    };

    let short_root = env::var_os("HELIOS_RUST_SCRIPT_SHORT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\rs"));
    if let Err(error) = fs::create_dir_all(&short_root) {
        return fail(format!("cannot create {}: {error}", short_root.display()));
    }

    let contents = match fs::read(&source) {
        Ok(contents) => contents,
        Err(error) => return fail(format!("cannot read {}: {error}", source.display())),
    };
    let mut hasher = DefaultHasher::new();
    contents.hash(&mut hasher);
    let short_script = short_root.join(format!("{:016x}.rs", hasher.finish()));
    if let Err(error) = fs::write(&short_script, contents) {
        return fail(format!("cannot write {}: {error}", short_script.display()));
    }

    let has_base_path = arguments
        .iter()
        .any(|argument| argument == OsStr::new("--base-path"));
    let mut forwarded = Vec::with_capacity(arguments.len() + 2);
    for (index, argument) in arguments.iter().enumerate() {
        if index == script_index {
            if !has_base_path {
                forwarded.push(OsString::from("--base-path"));
                forwarded.push(
                    source
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .as_os_str()
                        .to_owned(),
                );
            }
            forwarded.push(short_script.as_os_str().to_owned());
        } else {
            forwarded.push(argument.clone());
        }
    }

    eprintln!(
        "helios rust-script shim: {} -> {}",
        source.display(),
        short_script.display()
    );
    run(&real_executable, &forwarded)
}

fn run(executable: &OsStr, arguments: &[OsString]) -> ExitCode {
    match Command::new(executable).args(arguments).status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(error) => fail(format!("cannot launch {}: {error}", Path::new(executable).display())),
    }
}
