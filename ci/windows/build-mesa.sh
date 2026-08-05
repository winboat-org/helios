#!/usr/bin/env bash
set -euo pipefail

# Every path below is already a Windows path. Without this, MSYS2's argument
# converter rewrites the embedded `-includeD:/...` compiler argument into the
# invalid `-includeD:A:/...` form before Meson sees it.
export MSYS2_ARG_CONV_EXCL='*'

repo_root="$(cygpath -m "${1:?usage: build-mesa.sh REPO_ROOT OUTPUT_DIR [BUILD_DIR]}")"
output_dir="$(cygpath -m "${2:?usage: build-mesa.sh REPO_ROOT OUTPUT_DIR [BUILD_DIR]}")"
build_dir="$(cygpath -m "${3:-C:/helios-mesa-build}")"

mesa_src="${repo_root}/icd/mesa"
native_file="${repo_root}/ci/windows/mingw-native.ini"
compat_header="${repo_root}/icd/win-build/helios_win_compat.h"

rm -rf "${build_dir}"
mkdir -p "${output_dir}"

meson setup "${build_dir}" "${mesa_src}" \
  --native-file "${native_file}" \
  "-Dc_args=-include${compat_header}" \
  -Dvulkan-drivers=virtio \
  -Dgallium-drivers=zink \
  "-Dhelios-wdk-include=${repo_root}/icd/win-build/wdk-include" \
  -Dplatforms=windows \
  -Dvideo-codecs= \
  -Dvulkan-layers= \
  -Degl=disabled \
  -Dgbm=disabled \
  -Dglx=disabled \
  -Dopengl=true \
  -Dgles1=disabled \
  -Dgles2=disabled \
  -Dllvm=disabled \
  -Dshader-cache=disabled \
  -Dzlib=disabled \
  -Dzstd=disabled \
  -Dbuild-tests=false \
  -Dperfetto=false \
  -Dxmlconfig=disabled \
  --buildtype=release

meson compile -C "${build_dir}"

cp "${build_dir}/src/virtio/vulkan/vulkan_virtio.dll" "${output_dir}/"
cp "${build_dir}/src/gallium/targets/wgl/libgallium_wgl.dll" "${output_dir}/"
cp "${build_dir}/src/gallium/targets/libgl-gdi/opengl32.dll" "${output_dir}/opengl32-app-local.dll"

# WGL ICD dependencies are resolved by opengl32.dll and do not reliably search
# the private driver directory. Keep the Mesa ICDs self-contained instead of
# silently shipping a MinGW DLL that the Windows loader will not find.
for dll in "${output_dir}/vulkan_virtio.dll" "${output_dir}/libgallium_wgl.dll"; do
  while read -r dependency; do
    case "${dependency,,}" in
      lib*.dll|zlib1.dll)
        printf 'Unexpected non-system dependency in %s: %s\n' "${dll}" "${dependency}" >&2
        exit 1
        ;;
    esac
  done < <(objdump -p "${dll}" | sed -n 's/^[[:space:]]*DLL Name: //p')
done

mkdir -p "${output_dir}/licenses/mesa"
cp -R "${mesa_src}/licenses/." "${output_dir}/licenses/mesa/"

{
  for dll in "${output_dir}"/*.dll; do
    echo "=== $(basename "${dll}") ==="
    objdump -p "${dll}" | sed -n 's/^[[:space:]]*DLL Name: /DLL Name: /p'
  done
} > "${output_dir}/imports.txt"

printf 'Mesa artifact staged at %s\n' "${output_dir}"
