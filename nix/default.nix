{
  pkgs,
  sources,
  dependencies ? { },
  target ? "protocol",
  configuration ? "release",
  toolchain ? { },
  schemaVersion ? 1,
}:
assert toolchain == { };
assert schemaVersion == 1;
if target == "development-package" then
  {
    backend = "devbox";
    purpose = "build";
    architecture = "x64";
    crt = "mt";
    commands = [
      [
        "powershell.exe"
        "-NoProfile"
        "-ExecutionPolicy"
        "Bypass"
        "-File"
        "@heliosSourceDirectory@/nix/Package-Guest.ps1"
        "-Specification"
        "@specification@"
      ]
    ];
    outputs = [ "bundle/manifest.json" ];
    preserveDirectories = [ "bundle" ];
    requirements = [
      "verified-native-component-manifests"
      "baseline-test-signing-certificate"
    ];
  }
else if target == "guest-x64" || target == "guest-x86" then
  let
    native = target == "guest-x64";
  in
  {
    backend = "devbox";
    purpose = "build";
    crt = "mt";
    architecture = if target == "guest-x86" then "x86" else "x64";
    commands = [
      [
        "powershell.exe"
        "-NoProfile"
        "-ExecutionPolicy"
        "Bypass"
        "-File"
        "@heliosSourceDirectory@/nix/Build-Guest.ps1"
        "-Specification"
        "@specification@"
      ]
    ];
    outputs = [
      "package/helios_umd32.dll"
      "package/helios_umd12_32.dll"
    ]
    ++ pkgs.lib.optionals native [
      "package/helios_umd.dll"
      "package/helios_umd12.dll"
      "package/helios_kmd_render.sys"
      "package/helios_kmd_render.inf"
      "package/helios_kmd_render.cat"
      "package/helios-dev-test.cer"
    ];
    outputArchitectures = {
      "package/helios_umd32.dll" = "x86";
      "package/helios_umd12_32.dll" = "x86";
    };
    requirements = [
      "MSVC-v143"
      "SDK-and-WDK-10.0.26100.0"
      "LLVM-22.1.8"
      "bindgen-0.72"
      "Rust-nightly-2026-07-14"
      "local-CARGO_TARGET_DIR"
      "paired-static-CRT-DXVK-vkd3d"
    ];
  }
else
  assert target == "protocol";
  pkgs.rustPlatform.buildRustPackage {
    pname = "helios-protocol";
    version = "0.1.0";
    src = sources.helios;
    cargoRoot = "protocol";
    buildAndTestSubdir = "protocol";
    cargoLock.lockFile = sources.helios + "/protocol/Cargo.lock";
    cargoBuildType = configuration;
    cargoTestType = configuration;
    doCheck = true;
    installPhase = ''
      mkdir -p $out/share/helios-protocol
      cp -r protocol/src $out/share/helios-protocol/
      mkdir -p $out/lib
      find target -name 'libhelios_protocol*.rlib' -exec cp {} $out/lib/ \;
      mkdir -p $out/share/licenses/helios-protocol
      cp protocol/Cargo.toml $out/share/licenses/helios-protocol/source-attribution.toml
      echo 'The source snapshot has no declared root or protocol license. Distribution requires owner license clarification.' > $out/share/licenses/helios-protocol/NOTICE
    '';
  }
