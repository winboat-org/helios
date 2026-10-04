{ pkgs, sources }:
let
  crates = [
    "kmd_render"
    "umd"
    "umd12"
  ];
  vendor =
    crate:
    pkgs.rustPlatform.importCargoLock {
      lockFileContents = builtins.readFile (sources.helios + "/${crate}/Cargo.lock");
      # Actual nix-prefetch-git content hash of the immutable rev in Cargo.lock.
      # All six WDK crates share this source revision; bindgen stays at 0.72.
      outputHashes = pkgs.lib.optionalAttrs (crate == "kmd_render") {
        "wdk-0.4.1" = "sha256-nOQfku4Bg7Y4hiJUeIEy04f1HasgNqR6NK4mM0RUnVs=";
      };
    };
  wdkSource = pkgs.fetchgit {
    url = "https://github.com/microsoft/windows-drivers-rs";
    rev = "36558802149bc92455bf5719fe77f3a829d8580f";
    hash = "sha256-nOQfku4Bg7Y4hiJUeIEy04f1HasgNqR6NK4mM0RUnVs=";
  };
in
pkgs.runCommand "helios-windows-cargo-dependencies" { } (
  "mkdir -p $out/vendor $out/manifests $out/share/licenses\n"
  + pkgs.lib.concatMapStringsSep "\n" (crate: ''
    cp ${vendor crate}/Cargo.lock $out/manifests/${crate}.lock
    cp ${vendor crate}/.cargo/config.toml $out/manifests/${crate}.config.toml
    for package in ${vendor crate}/*; do
      [ -d "$package" ] || continue
      destination="$out/vendor/$(basename "$package")"
      if [ -e "$destination" ]; then
        diff -qr "$package" "$destination"
      else
        cp -rL "$package" "$destination"
      fi
    done
  '') crates
  + ''
    for package in $out/vendor/*; do
      destination="$out/share/licenses/$(basename "$package")"
      mkdir -p "$destination"
      cp "$package/Cargo.toml" "$destination/source-attribution.toml"
      find "$package" -maxdepth 1 -type f \( -iname 'LICENSE*' -o -iname 'COPYING*' -o -iname 'NOTICE*' \) -exec cp '{}' "$destination/" \;
    done
    mkdir -p $out/share/licenses/windows-drivers-rs
    cp ${wdkSource}/LICENSE-* $out/share/licenses/windows-drivers-rs/
  ''
)
