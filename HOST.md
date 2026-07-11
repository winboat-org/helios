# HOST.md — Linux Host Setup and Venus Server

## Overview

The Linux host side of the Helios vGPU project consists of:
1. The in-tree **`qemu-helios` fork** with virtio-gpu-gl, Venus, blob, and
   native OPTIMAL scanout support
2. **virglrenderer** built with Venus support
3. **Host Vulkan driver** (RADV for AMD, ANV for Intel, or NVIDIA proprietary)

You do NOT write a custom host-side daemon for the primary path — virglrenderer handles Venus and
`qemu-helios` handles display export/readback. The `host/` crate is diagnostic only.

The Vulkan renderer remains the proven Venus/virglrenderer stack. The active display path does
depend on the QEMU fork: stock QEMU cannot correctly display the current modifier-less OPTIMAL DWM
primary through egl-headless/GTK/SDL.

---

## 1. Host Requirements Checklist

```bash
# ── Kernel ───────────────────────────────────────────────────────────────────
uname -r
# Required: 6.13+
# If on 6.11-6.12 with Intel CPU, need KVM VMX patch (see TOOLCHAIN.md)

# ── KVM ──────────────────────────────────────────────────────────────────────
ls /dev/kvm
cat /sys/module/kvm/parameters/ignore_msrs  # should be N or 1

# ── udmabuf (for blob zero-copy) ─────────────────────────────────────────────
ls /dev/udmabuf
# If missing: modprobe udmabuf
# If not found: CONFIG_UDMABUF must be enabled in kernel config

# ── Vulkan ───────────────────────────────────────────────────────────────────
vulkaninfo --summary
# Must show: Vulkan 1.3 capable device
# AMD: radv driver, version >= 21.0
# Intel: anv driver
# NVIDIA: NVK (open) or proprietary

# ── QEMU ─────────────────────────────────────────────────────────────────────
./qemu-helios/build-helios/qemu-system-x86_64 --version
# Required for the active display path: the pinned qemu-helios submodule build

# ── virglrenderer ─────────────────────────────────────────────────────────────
pkg-config --modversion virglrenderer
# Build from source with -Dvenus=true (see TOOLCHAIN.md)
```

---

## 2. QEMU Configuration

### 2.1 Generic argument reference

The authoritative launcher is `tools/launch-helios-gtk.sh`. It defaults to the
`qemu-helios/build-helios` binary and automatically selects its module/data
directories. The standalone example below is a generic reference, not the
current win11 VM definition.

```bash
#!/bin/bash
# launch-helios-vm.sh

DISK_IMAGE="$HOME/vms/helios-win11.qcow2"
OVMF_CODE="/usr/share/OVMF/OVMF_CODE_4M.fd"
OVMF_VARS="$HOME/vms/OVMF_VARS.fd"

# Tune these:
VM_RAM="16G"
VM_CPUS="8"
BLOB_MEM="8G"       # host memory exposed as GPU VRAM; max: your RAM - VM RAM

./qemu-helios/build-helios/qemu-system-x86_64 \
  \
  # ── Machine type ──────────────────────────────────────────────────────────
  -machine q35,accel=kvm,smm=on \
  -global driver=cfi.pflash01,property=secure,value=on \
  \
  # ── Firmware ──────────────────────────────────────────────────────────────
  -drive if=pflash,format=raw,unit=0,file=${OVMF_CODE},readonly=on \
  -drive if=pflash,format=raw,unit=1,file=${OVMF_VARS} \
  \
  # ── CPU / Memory ──────────────────────────────────────────────────────────
  -cpu host,+topoext \
  -smp ${VM_CPUS},sockets=1,cores=${VM_CPUS},threads=1 \
  -m ${VM_RAM} \
  \
  # ── Storage ───────────────────────────────────────────────────────────────
  -drive file=${DISK_IMAGE},if=virtio,format=qcow2,aio=native,cache.direct=on \
  \
  # ── Virtio GPU with Venus (the magic) ─────────────────────────────────────
  -device virtio-gpu-gl,\
hostmem=${BLOB_MEM},\
blob=on,\
venus=on,\
id=gpu0 \
  -display gtk,gl=on \
  \
  # ── Network ───────────────────────────────────────────────────────────────
  -device virtio-net-pci,netdev=net0,id=net0 \
  -netdev user,id=net0,\
hostfwd=tcp::3389-:3389,\
hostfwd=tcp::22222-:22 \
  \
  # ── Misc ──────────────────────────────────────────────────────────────────
  -device virtio-rng-pci \
  -device virtio-balloon-pci \
  -usb -device usb-tablet \
  \
  # ── Debug (remove for production) ─────────────────────────────────────────
  -serial mon:stdio \
  -monitor tcp:127.0.0.1:55555,server,nowait \
  -d guest_errors \
  \
  "$@"
```

### 2.2 Why These Options Matter

| Option | Explanation |
|--------|-------------|
| `blob=on` | Enables virtio-gpu blob resources (shared zero-copy memory between guest and virglrenderer). Required for Venus performance and for mapping VkDeviceMemory into guest address space. |
| `hostmem=8G` | Allocates 8 GB of host RAM as the blob pool. This is what Venus uses as "GPU VRAM". Set to 50–75% of available host RAM minus VM RAM. |
| `venus=on` | Enables the Venus capset in QEMU, so the guest driver can create Venus contexts. |
| `gl=on` | Enables OpenGL rendering context for virglrenderer on the host. Required for virglrenderer to initialize. |
| `virtio-gpu-gl` | The GL-accelerated virtio-gpu device variant (not `virtio-gpu` which is software only). |
| `q35` | PCIe machine type. Required for MSI-X interrupts (virtio modern). |

### 2.3 Kernel Parameters (Host)

Add to `/etc/default/grub`:
```
GRUB_CMDLINE_LINUX="... intel_iommu=on iommu=pt"
```
(IOMMU passthrough mode helps with DMA performance even without passthrough.)

For AMD:
```
GRUB_CMDLINE_LINUX="... amd_iommu=on iommu=pt"
```

---

## 3. virglrenderer Configuration

### 3.1 Environment Variables

Set these before launching QEMU:

```bash
# NOTE: VIRGL_DEBUG (=venus / =verbose / etc.) does NOT produce readable logs in
# the venus render-server setup — venus runs in the virgl_render_server
# child process whose stderr is not captured (user-confirmed 2026-06-06). Do not
# rely on it for host-side venus tracing.

# Force specific host Vulkan device (useful for multi-GPU hosts)
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/radeon_icd.x86_64.json
# Or for NVIDIA:
# export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/nvidia_icd.json

# For NVIDIA proprietary driver — may need this:
# export __NV_PRIME_RENDER_OFFLOAD=1
```

### 3.2 Checking Venus is Active

After starting the VM and before the Helios KMDF driver loads in the guest, you can check using the vtest server to verify virglrenderer + Venus works:

```bash
# Stop QEMU, then test venus standalone:
cd virglrenderer-build/
./vtest/virgl_test_server --venus &
VN_DEBUG=vtest VK_DRIVER_FILES=/path/to/mesa/icd.json vulkaninfo
```

If `vulkaninfo` shows a device from the Venus test server, your virglrenderer build is correct.

---

## 4. Performance Tuning

### 4.1 CPU Pinning

Pin QEMU vCPUs to physical cores (reduce scheduling overhead):

```bash
# Get QEMU PID after start
QEMU_PID=$(pgrep -x qemu-system-x86_64 | tail -1)

# Pin vCPU threads to physical cores 4-11 (leave 0-3 for host)
VCPU_THREADS=$(ls /proc/$QEMU_PID/task/)
CORE=4
for TID in $VCPU_THREADS; do
    taskset -cp $CORE $TID 2>/dev/null
    CORE=$((CORE + 1))
done
```

### 4.2 Huge Pages

Using huge pages reduces TLB pressure for large blob memory:

```bash
# Allocate 8 GB in 2 MB huge pages (4096 pages of 2MB)
echo 4096 > /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages

# Tell QEMU to use huge pages for VM memory:
# Add to QEMU command: -mem-prealloc -mem-path /dev/hugepages
```

### 4.3 Isolate Host GPU for Venus

If you have multiple GPUs, dedicate one to Venus:
```bash
# Check available GPUs:
vulkaninfo --summary | grep "GPU"

# Set virglrenderer to use GPU 1 (index):
export MESA_VK_DEVICE_SELECT=1
```

### 4.4 virglrenderer Thread Configuration

Venus in virglrenderer runs in a separate process/thread per context. For best performance:

```bash
# virglrenderer uses its own thread pool for Venus
# The number of threads defaults to nproc; this is usually fine.
# To limit: (future option, check virglrenderer docs)
```

---

## 5. Venus Protocol Verification

### 5.1 Host-side venus tracing — VIRGL_DEBUG does NOT work here

> ⚠️ **`VIRGL_DEBUG` (any value) may produce no readable logs in this setup**.
> venus runs in the separate `virgl_render_server` child process whose stderr /
> `VIRGL_DEBUG` output is not necessarily captured. The QEMU log still
> captures QEMU-level `LOG_GUEST_ERROR` (`-d guest_errors`, e.g. a `RESP_ERR_*`), but
> not virglrenderer/venus decode traces. For host-side venus diagnostics, capture the
> render-server child's stderr directly or build virglrenderer with logging — not
> `VIRGL_DEBUG`.

### 5.2 Host Vulkan API Validation

During development, you can run virglrenderer with the Vulkan validation layers:

```bash
export VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation
# Install validation layers: 
# sudo apt install vulkan-validationlayers
```

This will catch Venus encoding errors that result in invalid Vulkan calls on the host.

---

## 6. `host/` Crate (Diagnostics Tool)

The `host/` crate provides a Rust binary that can:
- Connect to a running QEMU's monitor socket
- Query virtio-gpu state
- Dump in-flight Venus command buffers (for debugging encoding issues)
- Report performance metrics (fence latency, ring buffer utilization)

```rust
// host/src/main.rs — skeleton

use std::os::unix::net::UnixStream;

fn main() {
    let monitor = std::env::args().nth(1)
        .unwrap_or_else(|| "127.0.0.1:55555".to_string());

    println!("Helios Host Diagnostic Tool");
    println!("Connecting to QEMU monitor: {}", monitor);

    // TODO: implement QEMU monitor protocol (HMP or QMP)
    // to query virtio-gpu state, fence values, etc.
}
```

This is a low-priority component. Focus on the KMD and ICD first.

---

## 7. Known Issues & Workarounds

### AMD GPU: Transparent Huge Pages conflict

On AMD with older RADV versions, the `CONFIG_TRANSPARENT_HUGEPAGE` kernel option can cause issues with Venus blob memory. If you see memory corruption or hangs:

```bash
# Disable THP for the QEMU process:
echo madvise > /sys/kernel/mm/transparent_hugepage/enabled
# Or globally (not recommended):
echo never > /sys/kernel/mm/transparent_hugepage/enabled
```

### Intel GPU: PAT coherency (kernel < 6.16)

On Intel with kernel < 6.11, apply the KVM VMX PAT patch. On kernel 6.11-6.15, you may need:
```bash
# Check QEMU version for opt-out support:
# QEMU must pass KVM_X86_QUIRK_IGNORE_GUEST_PAT = 0 to KVM
# This is handled in newer QEMU builds
```

### NVIDIA proprietary driver

NVIDIA's proprietary driver has known issues with Venus (`VkDeviceMemory` export). Use NVIDIA open-source NVK driver if available, or test with AMD/Intel first.

### Missing `/dev/udmabuf`

```bash
sudo modprobe udmabuf
# Make permanent:
echo "udmabuf" >> /etc/modules-load.d/helios.conf
```

---

## 8. Performance Targets

| Scenario | Target | Notes |
|----------|--------|-------|
| Triangle benchmark (host) | 100% | Baseline |
| Triangle benchmark (Venus) | 40–60% | Goal |
| vkcube framerate | ≥30 FPS at 1080p | Basic render |
| DXVK synthetic (3DMark equivalent) | 40–50% of native | Long-term goal |
| Command submission latency | < 1ms | Fence to completion |
| Memory bandwidth (blob) | ≥50% of PCIe bandwidth | Zero-copy path |

QEMU has a default 100fps cap on fence polling for display output. This does NOT affect 3D command submission — only applies to scanout. Your 3D pipeline throughput is not limited to 100fps.

Source of the cap (QEMU code): `hw/display/virtio-gpu-virgl.c` function `virtio_gpu_virgl_fence_poll` uses a 10ms timer. This only fires when there's a pending display flush, not for pure compute/3D.
