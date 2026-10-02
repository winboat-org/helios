#!/usr/bin/env bash
# Build the paired protocol/renderer locally; never install into /usr or restart QEMU.
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo"
build="$repo/target/linux"
jobs=${HELIOS_BUILD_JOBS:-8}
if [[ -x "$build/venus-protocol-tools/bin/python3" ]]; then
    export PATH="$build/venus-protocol-tools/bin:$PATH"
fi
python3 -c 'import mako, yaml' || {
    echo 'Install Python Mako and PyYAML, or create target/linux/venus-protocol-tools with those packages.' >&2
    exit 1
}
for project in venus-protocol virglrenderer; do
    [[ -f "$repo/$project/meson.build" ]] || {
        echo "Missing $project submodule. Initialize the recorded gitlink first." >&2
        exit 1
    }
done
setup() {
    local directory=$1 source=$2
    shift 2
    if [[ -f "$directory/build.ninja" ]]; then
        meson setup --reconfigure "$directory" "$source" "$@"
    else
        meson setup "$directory" "$source" "$@"
    fi
}
setup "$build/venus-protocol-build" venus-protocol \
    --prefix="$build/venus-protocol-install" --libdir=lib
ninja -C "$build/venus-protocol-build" -j "$jobs"
meson test -C "$build/venus-protocol-build" --print-errorlogs
meson install -C "$build/venus-protocol-build"

# Mesa carries generated driver headers; build the renderer from the very same
# protocol source. Copy only driver outputs, never renderer headers or XML.
python3 - "$build/venus-protocol-build" "$repo/icd/mesa/src/virtio/venus-protocol" <<'PY'
import pathlib, shutil, sys
source, destination = map(pathlib.Path, sys.argv[1:])
headers = sorted(source.glob('vn_protocol_driver*.h'))
if not headers or not destination.is_dir():
    raise SystemExit('Missing generated headers or Mesa submodule')
for header in headers:
    target = destination / header.name
    if not target.exists() or target.read_bytes() != header.read_bytes():
        shutil.copyfile(header, target)
print(f'Mesa driver headers synchronized: {len(headers)}')
PY
export PKG_CONFIG_PATH="$build/venus-protocol-install/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
setup "$build/virglrenderer-build" virglrenderer \
    --prefix="$build/virglrenderer-install" --libdir=lib --buildtype=release \
    -Dvenus=true -Dtests=true
ninja -C "$build/virglrenderer-build" -j "$jobs"
meson test -C "$build/virglrenderer-build" venus_queue_sync --print-errorlogs
meson install -C "$build/virglrenderer-build"
echo 'Local renderer installed. See TOOLCHAIN.md for full QEMU restart, loaded host hashes and paired guest verification.'
