# helios build interface

`default.nix` accepts schemaVersion 1, `pkgs`, explicit `sources`,
`dependencies`, `target`, `configuration` (release/debug) and `toolchain`.
It returns a derivation for native/cross outputs or a devbox dispatch record
for ABI constrained Windows targets. Dependencies are immutable Nix output
paths. Sources are exported snapshots, including selected gitlink contents.
Only locked toolchains are accepted (`toolchain = {}`); nonempty overrides
are refused. No recipe downloads dependencies during compilation.

The committed devenv inputs/lock match the environment workspace. From this
checkout run `devenv shell -- wb-component-build /path/to/spec.json`. The JSON
spec supplies system, schemaVersion, sources (`id: {path: ..., narHash: "sha256-..."}`), dependencies
(`id: /nix/store/...`), target and configuration explicitly. Nix therefore never
assumes the location of another checkout. The environment's `wb build` prepares
these snapshots and records full source/toolchain/artifact provenance.

Guest dispatch records require Stage 4's local disk mirror and durable elevated
backend. Build and runtime verification are recorded separately; MinGW outputs
cannot substitute for an MSVC static engine. Licenses and debug symbols must accompany
exported artifacts.

Windows dispatch uses `Build-Guest.ps1` and the exact offline closure from
`windows-dependencies.nix`. It builds native and WoW64 UMD11/UMD12 against
separately verified static engine artifacts. Fresh bindgen 0.72 output uses the
provisioned LLVM 22.1.8 and retains layout assertions. Cached binding comparison
warnings remain separate from the fresh native compilation.

The KMD uses an owned local source copy with Cargo's ordinary target layout so
the pinned wdk-build can discover its lockfile above OUT_DIR. Packaging checks
matched-kit tools first, uses the kit's x86 Inf2Cat for the amd64 driver, and
signs the SYS and CAT with the verified development guest certificate. Resource
and INF versions come from the same Helios metadata. The environment workspace
retains complete component manifests, symbols and native acceptance evidence.

`Package-Guest.ps1` combines verified driver, both Mesa architectures and CLVK
artifacts into a development install package with the four existing Windows
installer scripts. It preserves licenses, PDBs, source identities and exact file
hashes. This builds no installer executable and does not replace release CI.
