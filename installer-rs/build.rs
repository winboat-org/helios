// Embed the icon and the (elevation/DPI) manifest via the Windows SDK resource
// compiler. Rust does not do this itself, and a missing manifest means the
// self-contained exe would not request administrator.
use std::path::PathBuf;
use std::process::Command;

fn find_rc() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("HELIOS_RC_DIR") {
        let candidate = PathBuf::from(dir).join("rc.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for base in [std::env::var_os("ProgramFiles(x86)"), std::env::var_os("ProgramFiles")]
        .into_iter()
        .flatten()
    {
        let kits = PathBuf::from(base).join("Windows Kits").join("10").join("bin");
        let Ok(entries) = std::fs::read_dir(&kits) else { continue };
        let mut versions: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        versions.sort();
        versions.reverse();
        for version in versions {
            for arch in ["x64", "x86"] {
                let candidate = version.join(arch).join("rc.exe");
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn main() {
    println!("cargo:rerun-if-changed=installer.rc");
    println!("cargo:rerun-if-changed=src/installer.manifest");
    if std::env::var("CARGO_CFG_WINDOWS").is_err() {
        return;
    }
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    let res = format!("{out}\\installer.res");
    let rc = find_rc().expect(
        "rc.exe from the Windows SDK is required to embed the installer manifest; set HELIOS_RC_DIR",
    );

    // rc.exe has no default include path; without the SDK um/shared/ucrt dirs it
    // cannot find windows.h. Derive them from the rc.exe location (…\Windows
    // Kits\10\bin\<version>\x64\rc.exe), falling back to the SDK env vars.
    let mut command = Command::new(&rc);
    command.args(["/nologo", "/i", "src"]);
    for include in sdk_includes(&rc) {
        command.arg("/i").arg(include);
    }
    let status = command
        .arg("/fo")
        .arg(&res)
        .arg("installer.rc")
        .status()
        .expect("run rc.exe");
    if !status.success() {
        panic!("rc.exe failed to compile installer.rc");
    }
    println!("cargo:rustc-link-arg={res}");
}

fn sdk_includes(rc: &PathBuf) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    // From the rc.exe path: <kit>\bin\<version>\<arch>\rc.exe
    let components: Vec<_> = rc.components().collect();
    if let Some(bin) = components.iter().position(|c| c.as_os_str().eq_ignore_ascii_case("bin")) {
        let kit: PathBuf = components[..bin].iter().collect();
        if let Some(version) = components.get(bin + 1) {
            roots.push(kit.join("Include").join(version));
        }
    }
    if let (Some(dir), Some(version)) =
        (std::env::var_os("WindowsSdkDir"), std::env::var_os("WindowsSDKVersion"))
    {
        let version = version.to_string_lossy().trim_end_matches('\\').to_string();
        roots.push(PathBuf::from(dir).join("Include").join(version));
    }
    let mut includes = Vec::new();
    for root in roots {
        for part in ["um", "shared", "ucrt", "winrt", "cppwinrt"] {
            let candidate = root.join(part);
            if candidate.is_dir() {
                includes.push(candidate);
            }
        }
    }
    includes
}

