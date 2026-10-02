{ pkgs, ... }:
{
  packages = [
    pkgs.nix
    pkgs.git
    pkgs.meson
    pkgs.ninja
    pkgs.cmake
    pkgs.pkg-config
    pkgs.python3
    pkgs.nixfmt
  ];
  env.WB_NIXPKGS = toString pkgs.path;
  scripts.wb-component-build.exec = ''
    exec nix build --out-link result --file ./nix/standalone.nix --argstr nixpkgsPath "$WB_NIXPKGS" --argstr specification "$1"
  '';
}
