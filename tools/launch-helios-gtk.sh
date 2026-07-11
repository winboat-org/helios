#!/usr/bin/env bash
# tools/launch-helios-gtk.sh — standalone QEMU for win11.
#
# Default mode uses the SDL GL console:
#   bash tools/launch-helios-gtk.sh
#
# GTK GL console:
#   HELIOS_DISPLAY=gtk bash tools/launch-helios-gtk.sh
#
# Looking Glass mode adds KVMFR ivshmem + SPICE input and starts the git-built
# client on the host:
#   HELIOS_DISPLAY=looking-glass bash tools/launch-helios-gtk.sh
#
# Transport overrides:
#   HELIOS_DISPLAY=looking-glass HELIOS_LG_TRANSPORT=spice bash tools/launch-helios-gtk.sh
#
# Ctrl-C requests ACPI shutdown by default. To stop only the LG client and leave
# the VM running:
#   HELIOS_INT_ACTION=client HELIOS_DISPLAY=looking-glass bash tools/launch-helios-gtk.sh
#
# Host client display backend:
#   HELIOS_LG_DISPLAY_SERVER=x11 HELIOS_LG_RENDERER=OpenGL HELIOS_DISPLAY=looking-glass bash tools/launch-helios-gtk.sh
#
# Host GPU used by QEMU/virglrenderer/Venus:
#   HELIOS_QEMU_RENDER_GPU=nvidia HELIOS_DISPLAY=looking-glass bash tools/launch-helios-gtk.sh
#
# Headless QEMU GL scanout with VNC viewer transport:
#   HELIOS_QEMU_RENDER_GPU=nvidia HELIOS_DISPLAY=egl-vnc bash tools/launch-helios-gtk.sh
#
# Override the QEMU binary, for example to compare stock QEMU:
#   HELIOS_QEMU_BIN=/tmp/qemu-ref/build-helios/qemu-system-x86_64 HELIOS_DISPLAY=egl-vnc bash tools/launch-helios-gtk.sh
#
# Override QEMU's data directory for keymaps/ROMs:
#   HELIOS_QEMU_DATADIR=/tmp/qemu-ref/pc-bios HELIOS_QEMU_BIN=/tmp/qemu-ref/build-helios/qemu-system-x86_64 bash tools/launch-helios-gtk.sh
# Out-of-tree modular builds are detected beside HELIOS_QEMU_BIN. Override with:
#   HELIOS_QEMU_MODULE_DIR=/tmp/qemu-ref/build-helios
#
# If another VNC server is holding 5900:
#   HELIOS_VNC_ADDR=127.0.0.1:1 HELIOS_DISPLAY=egl-vnc bash tools/launch-helios-gtk.sh
#
# Temporarily boot without the Helios virtio-gpu PCI device:
#   HELIOS_DISABLE_VIRTIO_GPU=1 HELIOS_DISPLAY=looking-glass bash tools/launch-helios-gtk.sh
#
# Host GPU used by the Looking Glass client:
#   HELIOS_LG_RENDER_GPU=intel HELIOS_DISPLAY=looking-glass bash tools/launch-helios-gtk.sh
#
# Guest CPU topology:
#   HELIOS_SMP=16 HELIOS_SOCKETS=1 HELIOS_CORES=16 HELIOS_THREADS=1 \
#     HELIOS_DISPLAY=looking-glass bash tools/launch-helios-gtk.sh
#
# If another VM is holding 5900:
#   HELIOS_SPICE_PORT=5901 HELIOS_DISPLAY=looking-glass bash tools/launch-helios-gtk.sh
#
# Looking Glass KVMFR defaults to 512 MiB for the normal desktop stream.
# Ensure /dev/kvmfr0 was created with the same size, or override:
#   HELIOS_KVMFR_SIZE=134217728 HELIOS_DISPLAY=looking-glass bash tools/launch-helios-gtk.sh
#
# Kernel debugging over Windows KDCOM for ntoseye:
#   HELIOS_KD_SERIAL=socket HELIOS_DISPLAY=looking-glass bash tools/launch-helios-gtk.sh
# This exposes COM1 at /tmp/ntoseye-kd.sock. Configure the guest with:
#   bcdedit /debug on
#   bcdedit /dbgsettings serial debugport:1 baudrate:115200
set -uo pipefail

if [ "${HELIOS_PHASE:-}" != "user" ] && [ "$(id -u)" -ne 0 ]; then
  exec sudo --preserve-env=HELIOS_DISPLAY,HELIOS_QEMU_BIN,HELIOS_QEMU_DATADIR,HELIOS_QEMU_MODULE_DIR,HELIOS_QEMU_RENDER_GPU,HELIOS_DISABLE_VIRTIO_GPU,HELIOS_INTEL_RENDER_NODE,HELIOS_NVIDIA_RENDER_NODE,HELIOS_LG_TRANSPORT,HELIOS_SPICE_PORT,HELIOS_VNC_ADDR,HELIOS_KVMFR_DEV,HELIOS_KVMFR_SIZE,HELIOS_LG_CLIENT,HELIOS_LG_RENDER_GPU,HELIOS_LG_DISPLAY_SERVER,HELIOS_LG_RENDERER,HELIOS_LG_ALLOW_DMA,HELIOS_LG_START_CLIENT,HELIOS_LG_RESTART_CLIENT,HELIOS_LG_CLIENT_DELAY,HELIOS_LG_CLIENT_LOG,HELIOS_INT_ACTION,HELIOS_SMP,HELIOS_SOCKETS,HELIOS_CORES,HELIOS_THREADS,HELIOS_VKR_DEBUG,HELIOS_QEMU_LOG,HELIOS_QEMU_TRACE,HELIOS_KD_SERIAL,HELIOS_KD_SOCKET,DISPLAY,WAYLAND_DISPLAY,GDK_BACKEND,SDL_VIDEODRIVER \
    bash "$0" "$@"
fi

USER_NAME=${SUDO_USER:-rupansh}; USER_UID=$(id -u "$USER_NAME")
DISK=/var/lib/libvirt/images/win11.qcow2; NVRAM=/var/lib/libvirt/qemu/nvram/win11_VARS.fd
SWSRC=/var/lib/libvirt/swtpm/bfe8dc1f-8c5b-435c-8045-1ef3a5c19053/tpm2; TPMDIR=/tmp/helios-tpm; SHARE=/home/rupansh/helios-vgpu
DISPLAY_MODE=${HELIOS_DISPLAY:-sdl}
QEMU_BIN=${HELIOS_QEMU_BIN:-$SHARE/qemu-helios/build-helios/qemu-system-x86_64}
QEMU_DATADIR=${HELIOS_QEMU_DATADIR:-$SHARE/qemu-helios/pc-bios}
QEMU_MODULE_DIR_OVERRIDE=${HELIOS_QEMU_MODULE_DIR:-}
if [ -z "$QEMU_MODULE_DIR_OVERRIDE" ] && [ -e "$(dirname "$QEMU_BIN")/ui-egl-headless.so" ]; then
  QEMU_MODULE_DIR_OVERRIDE=$(dirname "$QEMU_BIN")
fi
if [ ! -x "$QEMU_BIN" ]; then
  echo "missing executable QEMU binary: $QEMU_BIN" >&2
  echo "build qemu-helios/build-helios or set HELIOS_QEMU_BIN explicitly" >&2
  exit 1
fi
QEMU_RENDER_GPU=${HELIOS_QEMU_RENDER_GPU:-intel}
DISABLE_VIRTIO_GPU=${HELIOS_DISABLE_VIRTIO_GPU:-0}
INTEL_RENDER_NODE=${HELIOS_INTEL_RENDER_NODE:-/dev/dri/renderD129}
NVIDIA_RENDER_NODE=${HELIOS_NVIDIA_RENDER_NODE:-/dev/dri/renderD128}
LG_TRANSPORT=${HELIOS_LG_TRANSPORT:-kvmfr}
SPICE_PORT=${HELIOS_SPICE_PORT:-5900}
VNC_ADDR=${HELIOS_VNC_ADDR:-127.0.0.1:0}
KVMFR_DEV=${HELIOS_KVMFR_DEV:-/dev/kvmfr0}
KVMFR_SIZE=${HELIOS_KVMFR_SIZE:-536870912}
LG_CLIENT=${HELIOS_LG_CLIENT:-$SHARE/LookingGlass/client/build/looking-glass-client}
LG_RENDER_GPU=${HELIOS_LG_RENDER_GPU:-default}
LG_DISPLAY_SERVER=${HELIOS_LG_DISPLAY_SERVER:-wayland}
LG_RENDERER=${HELIOS_LG_RENDERER:-EGL}
LG_ALLOW_DMA=${HELIOS_LG_ALLOW_DMA:-no}
LG_START_CLIENT=${HELIOS_LG_START_CLIENT:-yes}
LG_RESTART_CLIENT=${HELIOS_LG_RESTART_CLIENT:-no}
LG_CLIENT_DELAY=${HELIOS_LG_CLIENT_DELAY:-12}
LG_CLIENT_LOG=${HELIOS_LG_CLIENT_LOG:-/tmp/helios-looking-glass-client.log}
INT_ACTION=${HELIOS_INT_ACTION:-shutdown}
SMP=${HELIOS_SMP:-16}
SOCKETS=${HELIOS_SOCKETS:-1}
CORES=${HELIOS_CORES:-16}
THREADS=${HELIOS_THREADS:-1}
KD_SERIAL=${HELIOS_KD_SERIAL:-pty}
KD_SOCKET=${HELIOS_KD_SOCKET:-/tmp/ntoseye-kd.sock}
if [ "${HELIOS_PHASE:-}" != "user" ]; then
  [ "$(id -u)" -eq 0 ] || { echo "run with sudo"; exit 1; }
  USER_PHASE_PID=
  cleanup() {
    trap - EXIT INT TERM
    if [ -n "${USER_PHASE_PID:-}" ]; then
      kill "$USER_PHASE_PID" 2>/dev/null || true
      wait "$USER_PHASE_PID" 2>/dev/null || true
    fi
    ip link del heltap0 2>/dev/null||true
    chown libvirt-qemu:libvirt-qemu "$DISK" "$NVRAM" 2>/dev/null||true
  }
  handle_root_int() {
    if [ "$INT_ACTION" = "shutdown" ]; then
      echo '>>> SIGINT received; requesting clean VM shutdown. <<<'
      if [ -n "${USER_PHASE_PID:-}" ]; then
        kill -INT "$USER_PHASE_PID" 2>/dev/null || true
        wait "$USER_PHASE_PID" 2>/dev/null || true
        USER_PHASE_PID=
      fi
      cleanup
      exit 130
    fi
    echo '>>> SIGINT received; leaving VM running. <<<'
  }
  trap cleanup EXIT TERM
  trap handle_root_int INT
  chown "$USER_NAME" "$DISK" "$NVRAM"
  if [ "$DISPLAY_MODE" = "looking-glass" ] && [ "$LG_TRANSPORT" = "kvmfr" ]; then
    [ -e "$KVMFR_DEV" ] || { echo "missing $KVMFR_DEV (load kvmfr first)"; exit 1; }
    chown "$USER_NAME" "$KVMFR_DEV" 2>/dev/null || true
  fi
  mkdir -p "$TPMDIR/state"; cp -a "$SWSRC"/. "$TPMDIR/state/" 2>/dev/null || echo "WARN fresh TPM"
  chown -R "$USER_NAME" "$TPMDIR"
  ip link del heltap0 2>/dev/null||true; ip tuntap add dev heltap0 mode tap user "$USER_NAME"
  if ! ip link show virbr0 >/dev/null 2>&1; then
    virsh -c qemu:///system net-start default >/dev/null 2>&1 || true
  fi
  ip link show virbr0 >/dev/null 2>&1 || { echo "missing virbr0 (system libvirt default network is not active)"; exit 1; }
  ip link set heltap0 master virbr0 && ip link set heltap0 up
  sudo -u "$USER_NAME" env HELIOS_PHASE=user XDG_RUNTIME_DIR=/run/user/$USER_UID \
    DISPLAY=${DISPLAY:-:0} WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-wayland-1} GDK_BACKEND=${GDK_BACKEND:-wayland,x11,*} SDL_VIDEODRIVER=${SDL_VIDEODRIVER:-wayland} \
    HELIOS_DISPLAY="$DISPLAY_MODE" HELIOS_QEMU_RENDER_GPU="$QEMU_RENDER_GPU" HELIOS_DISABLE_VIRTIO_GPU="$DISABLE_VIRTIO_GPU" HELIOS_INTEL_RENDER_NODE="$INTEL_RENDER_NODE" HELIOS_NVIDIA_RENDER_NODE="$NVIDIA_RENDER_NODE" HELIOS_LG_TRANSPORT="$LG_TRANSPORT" HELIOS_SPICE_PORT="$SPICE_PORT" HELIOS_VNC_ADDR="$VNC_ADDR" HELIOS_KVMFR_DEV="$KVMFR_DEV" \
    HELIOS_QEMU_BIN="$QEMU_BIN" HELIOS_QEMU_DATADIR="$QEMU_DATADIR" HELIOS_QEMU_MODULE_DIR="$QEMU_MODULE_DIR_OVERRIDE" \
    HELIOS_KVMFR_SIZE="$KVMFR_SIZE" HELIOS_LG_CLIENT="$LG_CLIENT" \
    HELIOS_LG_RENDER_GPU="$LG_RENDER_GPU" HELIOS_LG_DISPLAY_SERVER="$LG_DISPLAY_SERVER" HELIOS_LG_RENDERER="$LG_RENDERER" HELIOS_LG_ALLOW_DMA="$LG_ALLOW_DMA" \
    HELIOS_LG_START_CLIENT="$LG_START_CLIENT" HELIOS_LG_RESTART_CLIENT="$LG_RESTART_CLIENT" \
    HELIOS_LG_CLIENT_DELAY="$LG_CLIENT_DELAY" HELIOS_LG_CLIENT_LOG="$LG_CLIENT_LOG" \
    HELIOS_INT_ACTION="$INT_ACTION" HELIOS_SMP="$SMP" HELIOS_SOCKETS="$SOCKETS" HELIOS_CORES="$CORES" HELIOS_THREADS="$THREADS" \
    HELIOS_VKR_DEBUG="${HELIOS_VKR_DEBUG:-}" HELIOS_QEMU_LOG="${HELIOS_QEMU_LOG:-}" HELIOS_QEMU_TRACE="${HELIOS_QEMU_TRACE:-}" \
    HELIOS_KD_SERIAL="$KD_SERIAL" HELIOS_KD_SOCKET="$KD_SOCKET" \
    bash "$0" &
  USER_PHASE_PID=$!
  while kill -0 "$USER_PHASE_PID" 2>/dev/null; do
    wait "$USER_PHASE_PID"
    USER_PHASE_STATUS=$?
    if [ "$USER_PHASE_STATUS" -eq 130 ] && [ "$INT_ACTION" != "shutdown" ]; then
      continue
    fi
    break
  done
  USER_PHASE_PID=
  exit "$USER_PHASE_STATUS"
fi
# ---- user phase (desktop user, native Wayland) ----
pkill -f 'virtiofsd.*helios-tpm' 2>/dev/null||true; pkill -f 'swtpm.*helios-tpm' 2>/dev/null||true
VIRTIOFSD_PID=
LG_CLIENT_PID=
QEMU_PID=

qmp_cmd() {
  command -v socat >/dev/null 2>&1 || return 1
  [ -S "$TPMDIR/mon.sock" ] || return 1
  printf '%s\n%s\n' '{"execute":"qmp_capabilities"}' "$1" | socat - "UNIX-CONNECT:$TPMDIR/mon.sock" >/dev/null 2>&1
}

shutdown_qemu() {
  [ -n "${QEMU_PID:-}" ] || return 0
  kill -0 "$QEMU_PID" 2>/dev/null || return 0

  echo '>>> Requesting guest shutdown <<<'
  if qmp_cmd '{"execute":"system_powerdown"}'; then
    for _ in $(seq 1 45); do
      kill -0 "$QEMU_PID" 2>/dev/null || return 0
      sleep 1
    done
  fi

  echo '>>> Guest did not stop, asking QEMU to quit <<<'
  qmp_cmd '{"execute":"quit"}' || true
  for _ in $(seq 1 10); do
    kill -0 "$QEMU_PID" 2>/dev/null || return 0
    sleep 1
  done

  echo '>>> QEMU did not quit, terminating process <<<'
  kill "$QEMU_PID" 2>/dev/null || true
  for _ in $(seq 1 5); do
    kill -0 "$QEMU_PID" 2>/dev/null || return 0
    sleep 1
  done
  kill -KILL "$QEMU_PID" 2>/dev/null || true
}

cleanup_user() {
  trap - EXIT TERM
  trap - INT
  if [ -n "${LG_CLIENT_PID:-}" ]; then
    kill "$LG_CLIENT_PID" 2>/dev/null || true
    wait "$LG_CLIENT_PID" 2>/dev/null || true
  fi
  shutdown_qemu
  if [ -n "${QEMU_PID:-}" ]; then
    wait "$QEMU_PID" 2>/dev/null || true
  fi
  if [ -n "${VIRTIOFSD_PID:-}" ]; then
    kill "$VIRTIOFSD_PID" 2>/dev/null || true
    wait "$VIRTIOFSD_PID" 2>/dev/null || true
  fi
  pkill -f 'swtpm.*helios-tpm' 2>/dev/null || true
}
handle_user_int() {
  if [ "$INT_ACTION" = "shutdown" ]; then
    cleanup_user
    exit 130
  fi
  if [ -n "${LG_CLIENT_PID:-}" ]; then
    echo '>>> SIGINT received; stopping Looking Glass client supervisor, VM remains running. <<<'
    kill "$LG_CLIENT_PID" 2>/dev/null || true
    wait "$LG_CLIENT_PID" 2>/dev/null || true
    LG_CLIENT_PID=
  else
    echo '>>> SIGINT received; VM remains running. <<<'
  fi
}
trap cleanup_user EXIT TERM
trap handle_user_int INT

/usr/lib/virtiofsd --shared-dir "$SHARE" --socket-path "$TPMDIR/fs.sock" --tag helios-vgpu --sandbox none &
VIRTIOFSD_PID=$!
/usr/bin/swtpm socket --tpmstate dir="$TPMDIR/state" --ctrl type=unixio,path="$TPMDIR/swtpm-sock" --tpm2 --daemon --pid file="$TPMDIR/swtpm.pid"
sleep 1

qemu_display_args=()
qemu_lg_args=()
qemu_gpu_args=()
qemu_vnc_args=()
qemu_trace_args=()
qemu_env_prefix=(env)
qemu_serial_args=()
lg_client_args=()
lg_client_env_prefix=(env)
qemu_egl_headless=egl-headless

if [ "$QEMU_RENDER_GPU" = "intel" ]; then
  [ -e "$INTEL_RENDER_NODE" ] || { echo "missing Intel render node $INTEL_RENDER_NODE"; exit 1; }
  qemu_env_prefix=(
    env
    __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json
    __GLX_VENDOR_LIBRARY_NAME=mesa
    DRI_PRIME=pci-0000_00_02_0
    MESA_LOADER_DRIVER_OVERRIDE=iris
  )
  qemu_egl_headless="egl-headless,rendernode=$INTEL_RENDER_NODE"
elif [ "$QEMU_RENDER_GPU" != "default" ]; then
  if [ "$QEMU_RENDER_GPU" = "nvidia" ]; then
    [ -e "$NVIDIA_RENDER_NODE" ] || { echo "missing NVIDIA render node $NVIDIA_RENDER_NODE"; exit 1; }
    if ! nvidia-smi -L >/dev/null 2>&1; then
      echo "NVIDIA driver is not healthy enough for QEMU/Venus (nvidia-smi failed)"
      exit 1
    fi
    qemu_env_prefix=(
      env
      __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/10_nvidia.json
      __GLX_VENDOR_LIBRARY_NAME=nvidia
      __VK_LAYER_NV_optimus=NVIDIA_only
      VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/nvidia_icd.json
      GBM_BACKEND=nvidia-drm
    )
    qemu_egl_headless="egl-headless,rendernode=$NVIDIA_RENDER_NODE"
  else
    echo "unknown HELIOS_QEMU_RENDER_GPU=$QEMU_RENDER_GPU (expected default, intel, or nvidia)"
    exit 1
  fi
fi

case "$DISPLAY_MODE" in
  sdl)
    qemu_display_args=(-display sdl,gl=on)
    ;;
  gtk)
    qemu_display_args=(-display gtk,gl=on)
    ;;
  spice-app)
    qemu_display_args=(-display spice-app,gl=on)
    ;;
  looking-glass|egl-vnc|vnc)
    # These modes are finalized after their transport-specific arguments.
    ;;
  *)
    echo "unknown HELIOS_DISPLAY=$DISPLAY_MODE (expected sdl, gtk, spice-app, looking-glass, egl-vnc, or vnc)"
    exit 1
    ;;
esac

case "$KD_SERIAL" in
  pty)
    qemu_serial_args=(
      -chardev pty,id=charserial0
      -device '{"driver":"isa-serial","chardev":"charserial0","id":"serial0","index":0}'
    )
    ;;
  socket)
    rm -f "$KD_SOCKET"
    qemu_serial_args=(
      -chardev "socket,id=charserial0,path=$KD_SOCKET,server=on,wait=off"
      -device '{"driver":"isa-serial","chardev":"charserial0","id":"serial0","index":0}'
    )
    echo ">>> KD serial socket: $KD_SOCKET (COM1) <<<"
    ;;
  none)
    qemu_serial_args=()
    ;;
  *)
    echo "unknown HELIOS_KD_SERIAL=$KD_SERIAL (expected pty, socket, or none)"
    exit 1
    ;;
esac

if [ "$DISPLAY_MODE" = "looking-glass" ]; then
  if [ "$LG_TRANSPORT" = "kvmfr" ]; then
    qemu_display_args=(-display "$qemu_egl_headless")
    qemu_lg_args=(
      -spice port="$SPICE_PORT",addr=127.0.0.1,disable-ticketing=on,image-compression=off
      -chardev spicevmc,id=charchannel0,name=vdagent
      -device '{"driver":"virtserialport","bus":"virtio-serial0.0","nr":1,"chardev":"charchannel0","id":"channel0","name":"com.redhat.spice.0"}'
      -device '{"driver":"ivshmem-plain","id":"shmem0","memdev":"looking-glass"}'
      -object "{\"qom-type\":\"memory-backend-file\",\"id\":\"looking-glass\",\"mem-path\":\"$KVMFR_DEV\",\"size\":$KVMFR_SIZE,\"share\":true}"
    )
    lg_client_args=(
      app:shmFile="$KVMFR_DEV"
      app:allowDMA="$LG_ALLOW_DMA"
      spice:input=yes
      spice:host=127.0.0.1
      spice:port="$SPICE_PORT"
      spice:display=no
      win:size=1920x1080
      win:setGuestRes=no
      win:disableWaitingMessage=yes
    )
  elif [ "$LG_TRANSPORT" = "spice" ]; then
    qemu_display_args=(-display "$qemu_egl_headless")
    qemu_lg_args=(
      -spice port="$SPICE_PORT",addr=127.0.0.1,disable-ticketing=on,image-compression=off
      -chardev spicevmc,id=charchannel0,name=vdagent
      -device '{"driver":"virtserialport","bus":"virtio-serial0.0","nr":1,"chardev":"charchannel0","id":"channel0","name":"com.redhat.spice.0"}'
    )
    lg_client_args=(
      app:lgmp=no
      app:allowDMA=no
      spice:input=yes
      spice:host=127.0.0.1
      spice:port="$SPICE_PORT"
      spice:display=yes
      win:size=1920x1080
      win:setGuestRes=no
      win:disableWaitingMessage=yes
    )
  else
    echo "unknown HELIOS_LG_TRANSPORT=$LG_TRANSPORT (expected kvmfr or spice)"
    exit 1
  fi

  if [ "$LG_RENDER_GPU" = "intel" ]; then
    lg_client_env=(
      __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json
      __GLX_VENDOR_LIBRARY_NAME=mesa
      DRI_PRIME=pci-0000_00_02_0
      MESA_LOADER_DRIVER_OVERRIDE=iris
    )
  else
    lg_client_env=()
  fi

  if [ "$LG_DISPLAY_SERVER" = "x11" ]; then
    lg_client_env_prefix=(env -u WAYLAND_DISPLAY)
    lg_client_env+=(
      DISPLAY="${DISPLAY:-:0}"
      GDK_BACKEND=x11
      SDL_VIDEODRIVER=x11
    )
    if [ "$LG_RENDERER" = "auto" ]; then
      LG_RENDERER=OpenGL
    fi
  elif [ "$LG_DISPLAY_SERVER" = "wayland" ]; then
    lg_client_env+=(
      WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}"
      GDK_BACKEND=wayland
      SDL_VIDEODRIVER=wayland
    )
  elif [ "$LG_DISPLAY_SERVER" != "auto" ]; then
    echo "unknown HELIOS_LG_DISPLAY_SERVER=$LG_DISPLAY_SERVER (expected x11, wayland, or auto)"
    exit 1
  fi

  if [ "$LG_RENDERER" != "auto" ]; then
    lg_client_args+=(app:renderer="$LG_RENDERER")
  fi
  if [ "$LG_RENDERER" = "EGL" ]; then
    lg_client_args+=(egl:multisample=no)
  fi

  if [ "$LG_START_CLIENT" = "yes" ]; then
    (
      sleep "$LG_CLIENT_DELAY"
      restart_count=0
      while true; do
        echo ">>> Starting Looking Glass client (log: $LG_CLIENT_LOG) <<<"
        start_time=$(date +%s)
        {
          printf '\n=== Looking Glass client start %s ===\n' "$(date --iso-8601=seconds)"
          "${lg_client_env_prefix[@]}" "${lg_client_env[@]}" "$LG_CLIENT" "${lg_client_args[@]}"
          status=$?
          printf '=== Looking Glass client exited status=%s %s ===\n' "$status" "$(date --iso-8601=seconds)"
        } >>"$LG_CLIENT_LOG" 2>&1
        [ "$LG_RESTART_CLIENT" = "yes" ] || exit "$status"

        run_time=$(( $(date +%s) - start_time ))
        if [ "$run_time" -lt 5 ]; then
          restart_count=$((restart_count + 1))
          if [ "$restart_count" -ge 5 ]; then
            echo ">>> Looking Glass client failed quickly $restart_count times; not restarting. See $LG_CLIENT_LOG <<<"
            exit "$status"
          fi
          sleep $((restart_count * 2))
        else
          restart_count=0
          sleep 2
        fi
      done
    ) &
    LG_CLIENT_PID=$!
  fi
fi

if [ "$DISPLAY_MODE" = "egl-vnc" ] || [ "$DISPLAY_MODE" = "vnc" ]; then
  qemu_display_args=(-display "$qemu_egl_headless")
  qemu_vnc_args=(-vnc "$VNC_ADDR")
  echo ">>> VNC display: $VNC_ADDR (127.0.0.1:0 is TCP 5900) <<<"
fi

if [ "$DISABLE_VIRTIO_GPU" = "1" ] || [ "$DISABLE_VIRTIO_GPU" = "yes" ]; then
  echo ">>> HELIOS_DISABLE_VIRTIO_GPU=$DISABLE_VIRTIO_GPU: omitting virtio-gpu-gl-pci device <<<"
  QEMU_GPU_LABEL="virtio-gpu disabled"
else
  qemu_gpu_args=(
    -device
    '{"driver":"virtio-gpu-gl-pci","id":"ua-heliosgpu","max_outputs":1,"bus":"pci.8","addr":"0x0","venus":true,"blob":true,"hostmem":8589934592,"max_hostmem":8589934592}'
  )
  QEMU_GPU_LABEL="virtio-gpu-gl-pci"
fi

echo ">>> QEMU ($DISPLAY_MODE, $QEMU_GPU_LABEL, host GPU: $QEMU_RENDER_GPU, bin: $QEMU_BIN, datadir: $QEMU_DATADIR). Z:\\ = repo. SSH 192.168.122.120 <<<"
# Capture QEMU + virgl_render_server (vkr_log) stderr; render-server diagnostics
# like "failed to look up object" / "mem fd export failed" land here. Optional
# HELIOS_VKR_DEBUG (e.g. "validate" / "all") enables vkr debugging incl. host-side
# Vulkan validation layers in the render server.
QEMU_LOG="${HELIOS_QEMU_LOG:-/tmp/helios-qemu-stderr.log}"
: >"$QEMU_LOG"
echo ">>> QEMU/render-server stderr tee'd to $QEMU_LOG <<<"
if [ -n "${HELIOS_VKR_DEBUG:-}" ]; then
  qemu_env_prefix+=("VKR_DEBUG=${HELIOS_VKR_DEBUG}")
fi
if [ -n "$QEMU_MODULE_DIR_OVERRIDE" ]; then
  qemu_env_prefix+=("QEMU_MODULE_DIR=$QEMU_MODULE_DIR_OVERRIDE")
fi
if [ -n "${HELIOS_QEMU_TRACE:-}" ]; then
  for trace_event in $HELIOS_QEMU_TRACE; do
    qemu_trace_args+=(-trace "$trace_event")
  done
fi
setsid "${qemu_env_prefix[@]}" "$QEMU_BIN" \
  -L \
  "$QEMU_DATADIR" \
  -name \
  guest=win11,debug-threads=on \
  -blockdev \
  '{"driver":"file","filename":"/usr/share/edk2/x64/OVMF_CODE.4m.fd","node-name":"libvirt-pflash0-storage","auto-read-only":true,"discard":"unmap"}' \
  -blockdev \
  '{"node-name":"libvirt-pflash0-format","read-only":true,"driver":"raw","file":"libvirt-pflash0-storage"}' \
  -blockdev \
  '{"driver":"file","filename":"/var/lib/libvirt/qemu/nvram/win11_VARS.fd","node-name":"libvirt-pflash1-storage","read-only":false}' \
  -machine \
  pc-q35-11.0,usb=off,vmport=off,smm=on,dump-guest-core=off,memory-backend=pc.ram,pflash0=libvirt-pflash0-format,pflash1=libvirt-pflash1-storage,hpet=off,acpi=on \
  -accel \
  kvm,honor-guest-pat=on \
  -cpu \
  host,migratable=on,hv-time=on,hv-relaxed=on,hv-vapic=on,hv-spinlocks=0x1fff,hv-vpindex=on,hv-runtime=on,hv-synic=on,hv-stimer=on,hv-frequencies=on,hv-tlbflush=on,hv-ipi=on,hv-evmcs=on,hv-avic=on \
  -m \
  size=33554432k \
  -object \
  '{"qom-type":"memory-backend-memfd","id":"pc.ram","share":true,"x-use-canonical-path-for-ramblock-id":false,"size":34359738368}' \
  -overcommit \
  mem-lock=off \
  -smp \
  "$SMP,sockets=$SOCKETS,cores=$CORES,threads=$THREADS" \
  -uuid \
  bfe8dc1f-8c5b-435c-8045-1ef3a5c19053 \
  -no-user-config \
  -nodefaults \
  -chardev \
  socket,id=charmonitor,path=/tmp/helios-tpm/mon.sock,server=on,wait=off \
  -mon \
  chardev=charmonitor,id=monitor,mode=control \
  -rtc \
  base=localtime,driftfix=slew \
  -global \
  kvm-pit.lost_tick_policy=delay \
  -global \
  ICH9-LPC.disable_s3=1 \
  -global \
  ICH9-LPC.disable_s4=1 \
  -boot \
  strict=on \
  -device \
  '{"driver":"pcie-root-port","port":16,"chassis":1,"id":"pci.1","bus":"pcie.0","multifunction":true,"addr":"0x2"}' \
  -device \
  '{"driver":"pcie-root-port","port":17,"chassis":2,"id":"pci.2","bus":"pcie.0","addr":"0x2.0x1"}' \
  -device \
  '{"driver":"pcie-root-port","port":18,"chassis":3,"id":"pci.3","bus":"pcie.0","addr":"0x2.0x2"}' \
  -device \
  '{"driver":"pcie-root-port","port":19,"chassis":4,"id":"pci.4","bus":"pcie.0","addr":"0x2.0x3"}' \
  -device \
  '{"driver":"pcie-root-port","port":20,"chassis":5,"id":"pci.5","bus":"pcie.0","addr":"0x2.0x4"}' \
  -device \
  '{"driver":"pcie-root-port","port":21,"chassis":6,"id":"pci.6","bus":"pcie.0","addr":"0x2.0x5"}' \
  -device \
  '{"driver":"pcie-root-port","port":22,"chassis":7,"id":"pci.7","bus":"pcie.0","addr":"0x2.0x6"}' \
  -device \
  '{"driver":"pcie-root-port","port":23,"chassis":8,"id":"pci.8","bus":"pcie.0","addr":"0x2.0x7"}' \
  -device \
  '{"driver":"pcie-root-port","port":24,"chassis":9,"id":"pci.9","bus":"pcie.0","multifunction":true,"addr":"0x3"}' \
  -device \
  '{"driver":"pcie-root-port","port":25,"chassis":10,"id":"pci.10","bus":"pcie.0","addr":"0x3.0x1"}' \
  -device \
  '{"driver":"pcie-root-port","port":26,"chassis":11,"id":"pci.11","bus":"pcie.0","addr":"0x3.0x2"}' \
  -device \
  '{"driver":"pcie-root-port","port":27,"chassis":12,"id":"pci.12","bus":"pcie.0","addr":"0x3.0x3"}' \
  -device \
  '{"driver":"pcie-root-port","port":28,"chassis":13,"id":"pci.13","bus":"pcie.0","addr":"0x3.0x4"}' \
  -device \
  '{"driver":"pcie-root-port","port":29,"chassis":14,"id":"pci.14","bus":"pcie.0","addr":"0x3.0x5"}' \
  -device \
  '{"driver":"pcie-root-port","port":30,"chassis":15,"id":"pci.15","bus":"pcie.0","addr":"0x3.0x6"}' \
  -device \
  '{"driver":"pcie-pci-bridge","id":"pci.16","bus":"pci.5","addr":"0x0"}' \
  -device \
  '{"driver":"qemu-xhci","p2":15,"p3":15,"id":"usb","bus":"pci.2","addr":"0x0"}' \
  -device \
  '{"driver":"virtio-scsi-pci","id":"scsi0","bus":"pci.9","addr":"0x0"}' \
  -device \
  '{"driver":"virtio-serial-pci","id":"virtio-serial0","bus":"pci.3","addr":"0x0"}' \
  "${qemu_lg_args[@]}" \
  -blockdev \
  '{"driver":"file","filename":"/var/lib/libvirt/images/win11.qcow2","node-name":"libvirt-1-storage","auto-read-only":true,"discard":"unmap"}' \
  -blockdev \
  '{"node-name":"libvirt-1-format","read-only":false,"driver":"qcow2","file":"libvirt-1-storage"}' \
  -device \
  '{"driver":"ide-hd","bus":"ide.0","drive":"libvirt-1-format","id":"sata0-0-0","bootindex":1}' \
  -chardev \
  socket,id=chr-vu-fs0,path=/tmp/helios-tpm/fs.sock \
  -device \
  '{"driver":"vhost-user-fs-pci","id":"fs0","chardev":"chr-vu-fs0","tag":"helios-vgpu","bus":"pci.7","addr":"0x0"}' \
  -netdev \
  tap,id=hostnet0,ifname=heltap0,script=no,downscript=no \
  -device \
  '{"driver":"virtio-net-pci","netdev":"hostnet0","id":"net0","mac":"52:54:00:2e:4b:35","bus":"pci.1","addr":"0x0"}' \
  "${qemu_serial_args[@]}" \
  -chardev \
  socket,id=chrtpm,path=/tmp/helios-tpm/swtpm-sock \
  -tpmdev \
  emulator,id=tpm-tpm0,chardev=chrtpm \
  -device \
  '{"driver":"tpm-crb","tpmdev":"tpm-tpm0","id":"tpm0"}' \
  -device \
  '{"driver":"usb-tablet","id":"input2","bus":"usb.0","port":"1"}' \
  "${qemu_gpu_args[@]}" \
  -global \
  ICH9-LPC.noreboot=off \
  -watchdog-action \
  reset \
  "${qemu_vnc_args[@]}" \
  -device \
  '{"driver":"virtio-balloon-pci","id":"balloon0","bus":"pci.4","addr":"0x0"}' \
  -d \
  guest_errors \
  -sandbox \
  off \
  -msg \
  timestamp=on \
  "${qemu_trace_args[@]}" \
  "${qemu_display_args[@]}" 2> >(tee -a "$QEMU_LOG" >&2) &
QEMU_PID=$!
while kill -0 "$QEMU_PID" 2>/dev/null; do
  if [ -n "${LG_CLIENT_PID:-}" ] && ! kill -0 "$LG_CLIENT_PID" 2>/dev/null; then
    wait "$LG_CLIENT_PID" 2>/dev/null
    RUN_STATUS=$?
    LG_CLIENT_PID=
    echo '>>> Looking Glass client exited; requesting clean VM shutdown. <<<'
    shutdown_qemu
    if [ -n "${QEMU_PID:-}" ]; then
      wait "$QEMU_PID" 2>/dev/null || true
      QEMU_PID=
    fi
    break
  fi

  sleep 1
done
if [ -n "${QEMU_PID:-}" ]; then
  wait "$QEMU_PID"
  RUN_STATUS=$?
fi
QEMU_PID=
exit "$RUN_STATUS"
