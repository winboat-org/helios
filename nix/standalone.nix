{ nixpkgsPath, specification }:
let
  spec = builtins.fromJSON (builtins.readFile specification);
  pkgs = import (builtins.toPath nixpkgsPath) { system = spec.system; };
  sources = builtins.mapAttrs (
    name: source:
    builtins.path {
      path = builtins.toPath source.path;
      name = "${name}-source";
      sha256 = source.narHash;
    }
  ) spec.sources;
  dependencies = builtins.mapAttrs (_: path: builtins.toPath path) (spec.dependencies or { });
in
import ./default.nix {
  inherit pkgs sources dependencies;
  inherit (spec) target configuration schemaVersion;
}
