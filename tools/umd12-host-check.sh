#!/usr/bin/env bash
# tools/umd12-host-check.sh — the LINUX-HOST cross-check for `umd12`.
#
# This is the per-lane inner loop described in `docs/dx12/PARALLEL.md` §7: every
# Rust type, every one of the 214 DDI slot signatures and all 1 904 bindgen
# layout assertions type-check here, on the Linux host, with no VM, no WDK and
# no contention for the single `win11` adapter. `umd12/build.rs` serves the
# committed `umd12/bindgen/cached/d3d12umddi.rs` into `OUT_DIR` when the build
# host is not Windows, which is what makes that possible.
#
# Usage (from anywhere; the script locates the repo itself):
#     tools/umd12-host-check.sh
#     tools/umd12-host-check.sh --message-format short
#     tools/umd12-host-check.sh --clippy -- -D warnings
# `--clippy` (first argument only) runs `cargo clippy` instead of `cargo check`;
# every other argument is forwarded verbatim.
#
# ⭐ The `--clippy` mode is not a convenience. `PARALLEL.md` §10's per-lane merge
# bar requires `-D warnings` clippy over 214 hand-written handlers — that row is
# where `missing_safety_doc` earns its keep — and the bare command in that table
# dies in `link-cplusplus` exactly like the bare `cargo check` did, with an error
# naming `lib.exe` and nothing about clippy. A lane hitting that concludes the
# tree is broken, or quietly drops the row. Both overrides belong in ONE place,
# so §10 points here. Clippy links nothing, so this is not the `build` mode
# forbidden below.
#
# ⛔ `check`/`clippy` ONLY — never `build`, and this script must never grow a `build`
# mode. The two `--config` overrides below tell cargo that the build scripts of
# the `cplusplus` (`link-cplusplus`) and `cxxbridge1` (`cxx`) links keys have
# already been "run", with the given outputs. They therefore do NOT run: there
# is no compiled `cxxbridge1` static library, no `cargo:HEADER` metadata and no
# C++ translation unit in this tree at all. A binary produced here would be
# missing the entire bridge, so a `build` would be a lie — it would link, and it
# would not contain the thing S4 exists to add. The shipping build is
#     tools\umd-check.ps1 -Mode release -Crate umd12
# on the VM, where clang-cl, the MSVC STL and the vkd3d archive actually exist.
#
# ⚠ Equally, a clean run here says nothing about the C++ half. `PARALLEL.md` §7's
# "what it does NOT cover" table is the authority: the cxx bridge's C++
# compilation, the link set, bindgen regeneration from the real SDK header and
# anything that runs are all VM-only.
#
# ⚠ THE TRAP THIS SCRIPT EXISTS FOR. Adding `cxx` to `umd12` pulls in
# `link-cplusplus`, whose build script runs a `cc::Build` probe for the TARGET.
# Cross-compiling to `x86_64-pc-windows-msvc` from Linux there is no MSVC
# toolchain, so it aborts the whole check with:
#     failed to find tool "lib.exe"
# That single failure would have taken the host cross-check away from every lane
# — i.e. eleven agents writing 214 handlers blind instead of against the
# compiler. The fix is cargo's first-class `[target.<triple>.<links>]`
# build-script override, which supplies the build script's output instead of
# running it.
#
# ⛔ And it is passed with `--config` on the COMMAND LINE, never written into
# `.cargo/config.toml`. `CLAUDE.md` (the `CARGO_TARGET_DIR` section) records why
# that file is off limits for platform-specific settings: the Linux host and the
# `win11` VM share this source tree, so `.cargo/config.toml` is read on BOTH
# platforms. An override committed there would also skip cxx's build script on
# the real Windows build — silently producing a `helios_umd12.dll` with no
# bridge object in it. Command line: it applies to this invocation and nothing
# else.
#
# ⚠ UNVERIFIED: the override keys are named after each crate's `links` value
# (`link-cplusplus` → `cplusplus`, `cxx` → `cxxbridge1`). If a future dependency
# bump renames either `links` key, cargo does not warn about an override that
# matches nothing — the build script simply runs again and `lib.exe` comes back.
# The symptom is exactly the error quoted above; the fix is to re-read the two
# crates' `Cargo.toml` `links =` lines.

set -euo pipefail

readonly TARGET_TRIPLE="x86_64-pc-windows-msvc"

# Resolve the repo root from this script's own location, so the script works
# from any cwd (agents run in worktrees; PARALLEL.md §7 recommends `isolation:
# "worktree"`).
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd -- "${script_dir}/.." && pwd)"

# ⚠ `umd12` is a standalone crate with its own `Cargo.lock` — there is no
# workspace manifest at the repo root — so the check runs from the crate
# directory. `CARGO_TARGET_DIR=target/linux` is therefore relative to
# `umd12/`, matching the recipe PARALLEL.md §7 documented and keeping Linux
# artifacts out of the Windows target dir (CLAUDE.md: the two toolchains
# produce incompatible artifacts and must never share one target dir).
readonly CRATE_DIR="${REPO_ROOT}/umd12"

if [[ ! -f "${CRATE_DIR}/Cargo.toml" ]]; then
    echo "umd12-host-check: no crate manifest at ${CRATE_DIR}/Cargo.toml" >&2
    echo "umd12-host-check: is ${REPO_ROOT} the helios-vgpu repo root?" >&2
    exit 1
fi

if command -v rustup >/dev/null 2>&1; then
    if ! rustup target list --installed | grep -qx "${TARGET_TRIPLE}"; then
        echo "umd12-host-check: the ${TARGET_TRIPLE} std is NOT installed." >&2
        echo "umd12-host-check: this check cannot run without it. Install it with:" >&2
        echo "" >&2
        echo "    rustup target add ${TARGET_TRIPLE}" >&2
        echo "" >&2
        exit 1
    fi
else
    # Not fatal: a non-rustup toolchain can still carry the target's std. Say so
    # loudly rather than pretending the precondition was checked — if the target
    # really is missing, cargo's own "can't find crate for `core`" follows
    # immediately.
    echo "umd12-host-check: WARNING - no rustup on PATH; cannot verify that the" >&2
    echo "umd12-host-check: ${TARGET_TRIPLE} std is installed. Continuing." >&2
fi

cd -- "${CRATE_DIR}"

# ⚠ A separate target dir per subcommand. `cargo check` and `cargo clippy` write
# mutually invalidating fingerprints into one directory, so sharing it makes
# every alternation a full rebuild — which on the S6 fan-out is the difference
# between a 7-second loop and a minute-long one.
cargo_cmd="check"
target_dir="target/linux"
if [[ "${1:-}" == "--clippy" ]]; then
    shift
    cargo_cmd="clippy"
    target_dir="target/linux-clippy"
fi

# `"$@"` is forwarded so a lane can add e.g. `--message-format short`,
# `--quiet`, or (with `--clippy`) `-- -D warnings`.
CARGO_TARGET_DIR="${target_dir}" exec cargo "${cargo_cmd}" \
    --target "${TARGET_TRIPLE}" \
    --config "target.${TARGET_TRIPLE}.cplusplus.rustc-link-lib=[]" \
    --config "target.${TARGET_TRIPLE}.cxxbridge1.rustc-cfg=[\"built_with_cargo\"]" \
    "$@"
