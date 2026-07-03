# TOOLCHAIN.md — Build Environment Setup

> **DIRECTION RESET (2026-06-07):** active KMD work is System-class KMDF + DeviceIoControl + Mesa Venus.
> DOD/dxgk build artifacts may remain as archived reference, but the active build should follow `ARCH.md` and
> `SYSTEM_CLASS_REFOCUS_2026_06_07.md`.

## Overview

You need two environments:
1. **Windows 11 Dev VM** — builds and runs the KMD (kernel-mode driver) and ICD
2. **Linux Host** — builds and runs QEMU + virglrenderer (Venus)

The Windows dev VM can be a separate VM from the target VM, or the same one if you're careful. Using separate VMs is strongly recommended.

---

## 1. Linux Host Setup

### 1.1 System Requirements

- Linux kernel **6.13+** (required for KVM page-fault fixes with blob resources)
- Vulkan 1.3-capable GPU with a compliant driver (RADV for AMD, ANV for Intel)
- QEMU **9.2.0+** (first version with upstream Venus support)
- virglrenderer built from source with Venus enabled

Check your kernel:
```bash
uname -r   # must be ≥ 6.13
```

Check Vulkan support:
```bash
vulkaninfo --summary | grep -E "apiVersion|driverVersion"
```

### 1.2 Build virglrenderer with Venus

```bash
# Dependencies (Ubuntu/Debian)
sudo apt install -y \
  meson ninja-build pkg-config \
  libepoxy-dev libgbm-dev \
  libdrm-dev libvulkan-dev \
  libpng-dev cmake

git clone https://gitlab.freedesktop.org/virgl/virglrenderer.git
cd virglrenderer
# Use a recent stable commit or tag — and PIN it. The Venus protocol/capset is
# version-coupled between the guest Venus encoder (Mesa / the Helios ICD) and
# host virglrenderer; record the exact virglrenderer commit + matching Mesa-Venus
# version and bump them together. (mvisor-win-vgpu-driver pins exact Mesa +
# virglrenderer commits for the same reason — see TRANSPORT.md §7.)
meson setup build \
  -Dvenus=true \
  -Dvenus-validate=false \
  -Ddrm-renderers=auto \
  -Dprefix=/usr/local
ninja -C build
sudo ninja -C build install
sudo ldconfig
```

Verify:
```bash
virgl_test_server --help 2>&1 | grep venus
# Should show: --venus   Enable Venus (VirtIO-GPU Vulkan)
```

### 1.3 Build QEMU 9.2+ with virglrenderer

```bash
# If distro QEMU is already ≥ 9.2, you may be able to use it
# But it must find the virglrenderer you just built
qemu-system-x86_64 --version

# If building from source:
sudo apt install -y \
  libglib2.0-dev libpixman-1-dev libssl-dev \
  libslirp-dev libcap-ng-dev libattr1-dev \
  python3-pip python3-setuptools ninja-build

git clone --depth 1 -b v9.2.0 https://gitlab.com/qemu-project/qemu.git
cd qemu
mkdir build && cd build
../configure \
  --prefix=/usr/local \
  --target-list=x86_64-softmmu \
  --enable-kvm \
  --enable-opengl \
  --enable-virglrenderer \
  --enable-gtk
make -j$(nproc)
sudo make install
```

### 1.4 QEMU Launch Command for Development

**Operational rule:** if you change the standalone VM launch command, launcher script, QEMU display/debug
transport, or any environment variable needed by `tools/launch-helios-gtk.sh`, do not try to start the VM
yourself unless the user explicitly asks in that same turn. Tell the user exactly what changed and ask them to
run the VM. This matters because the launcher often needs the user's desktop session, sudo credentials, GPU
environment, and active display state.

```bash
#!/bin/bash
# launch-vm.sh — launch the Windows 11 target VM

DISK="windows11.qcow2"   # your Windows 11 disk image
RAM="16G"
CORES="8"
HOSTMEM="8G"             # virtio-gpu blob memory exposed to guest

qemu-system-x86_64 \
  -enable-kvm \
  -m ${RAM} \
  -smp ${CORES},sockets=1,cores=${CORES},threads=1 \
  -cpu host \
  \
  -drive file=${DISK},if=virtio,format=qcow2 \
  \
  -device virtio-gpu-gl,\
hostmem=${HOSTMEM},\
blob=on,\
venus=on \
  -display gtk,gl=on \
  \
  -device virtio-net-pci,netdev=net0 \
  -netdev user,id=net0,hostfwd=tcp::3389-:3389 \
  \
  -machine q35,accel=kvm \
  -bios /usr/share/OVMF/OVMF_CODE.fd \
  \
  -serial mon:stdio \
  -monitor telnet:127.0.0.1:4444,server,nowait
```

**Key flags explained:**
- `blob=on` — enables blob resource support (zero-copy memory between guest and virglrenderer)
- `hostmem=8G` — dedicates 8 GB of host memory as the blob/hostmem region
- `venus=on` — enables Venus capset (Vulkan over virtio-gpu)
- `-display gtk,gl=on` — host display via GTK with OpenGL (required for virglrenderer)

### 1.5 Verify Venus Is Working (Linux Guest First)

Before tackling the Windows driver, verify the stack works end-to-end with a Linux guest:

```bash
# In the Linux guest VM:
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json
vulkaninfo --summary
vkcube   # should render a spinning cube
```

If this works, your host stack (QEMU + virglrenderer + Venus) is correct.

---

## 2. Windows 11 Dev VM Setup

This is where you compile and test the KMD and ICD.

### 2.0 Dev VM (`win11`)

A Windows 11 dev VM named `win11` is reachable via `ssh win` (preconfigured). It was **not** fully provisioned out of the box — only Rust (stable) was present; everything else below had to be installed. The actually-required, verified toolchain:

- **VS 2022 Build Tools** — "Desktop development with C++" (MSVC v143 + Spectre-mitigated x64 libs).
- **WDK** — kit **10.0.26100.0**. Must be a *complete* kit (SDK **and** WDK at the same version): `wdk-build` picks the **highest** installed kit with **no override**, so an incomplete higher kit (e.g. a winget WDK with no matching SDK → missing `specstrings.h`) breaks the build. Keep only complete kits.
- **LLVM 17.0.6** at `C:\Program Files\LLVM\bin`; set `LIBCLANG_PATH` to it for bindgen (LLVM 18 has a bindgen bug). bindgen/LLVM is needed only for active WDF/PCI support or archived dxgk reference builds; the System-class KMDF build should not depend on display DDIs.
- **Rust nightly + `rust-src`** (for `no_std` build-std), target `x86_64-pc-windows-msvc`.
- **cargo-make** — `cargo install --locked cargo-make`.
- **coreutils** are installed (Unix tools like `ls`/`cp`/`grep` work in `win_exec`).
- **SSH:** `ssh win` does not auto-`cd`; the source tree is at `Z:\`. Prefer the `win` MCP server over raw ssh.
- **Shared folder:** the current Linux project folder (`helios-vgpu/`) is shared into the VM and mounted as the **`Z:\`** drive. `ssh win` does **not** auto-`cd` into it — you must **explicitly `cd /d Z:\`** to reach the shared project folder before running any build commands. Commands run there operate on the same source tree you edit on Linux.

This means a typical Windows-side build must first `cd` into `Z:\`:

```bash
ssh win "cd /d Z:\ && cargo make"   # cd into the shared folder, then build on win11
```

The remaining subsections (§2.1–§2.5) document the full from-scratch setup for reference, but most of it is already done on `win11`.

> **IMPORTANT — building on `win11` (updated):**
> - **Rust IO fails on the `Z:\` share.** `cargo`/`cargo make` hit `OS error 87 (The parameter is incorrect)` on artifact copies and warn `could not canonicalize path Z:\`. So the **Cargo target dir must be on local disk**, not the share. Edit source on `Z:\`; build with `CARGO_TARGET_DIR=C:\Users\Rupansh\helios-target\<crate>`.
> - Set it via the **`CARGO_TARGET_DIR` env var** per invocation. Do **NOT** put `target-dir` in a committed `.cargo/config.toml` — Linux reads that file too (it builds shared crates like `protocol/`), and a `C:` path would break it. On Linux use `CARGO_TARGET_DIR=target/linux`.
> - **coreutils are installed** on `win11`: standard Unix tools (`ls`, `cp`, `mv`, `rm`, `cat`, …) work alongside PowerShell.
> - Prefer the **`win` MCP server** (`win_exec` / `win_cargo`) over raw `ssh win "cd /d Z:\ && …"` — it sidesteps cmd.exe quoting and stale-ControlMaster env, and `win_cargo` sets the local target dir + `LIBCLANG_PATH`.

### 2.1 Required Software

Install in this order:

#### Visual Studio 2022
Download from https://visualstudio.microsoft.com/  
Required workloads:
- "Desktop development with C++" (for MSVC toolchain, linker, headers)
- Individual component: "MSVC v143 Spectre-mitigated libs (x64)"

#### Windows Driver Kit (WDK) 22H2
```
https://learn.microsoft.com/en-us/windows-hardware/drivers/download-the-wdk
```
Install the WDK matching your VS 2022. The WDK installs as a VS extension.

Verify: Open VS → Extensions → should show "Windows Driver Kit".

#### LLVM 17.0.6 (not 18 — has a bindgen bug)
```powershell
winget install -i LLVM.LLVM --version 17.0.6 --force
# Select "Add LLVM to PATH" in the GUI
```

Verify:
```powershell
clang --version  # should print 17.0.6
```

#### Rust (nightly channel — required for no_std kernel mode)
```powershell
# Install rustup from https://rustup.rs/
rustup toolchain install nightly
rustup default nightly
rustup component add rust-src
rustup target add x86_64-pc-windows-msvc
```

#### cargo-make
```powershell
cargo install --locked cargo-make --no-default-features --features tls-native
```

#### (Optional) cargo-wdk — driver project scaffolding
```powershell
cargo install cargo-wdk
```

### 2.2 Enable Test Signing

The development KMD needs test signing. On the target VM (where the driver runs):

```powershell
# Run as Administrator in the TARGET VM (not necessarily the dev VM)
bcdedit /set testsigning on
bcdedit /set nointegritychecks on
# Reboot
```

On the dev VM, generate a test certificate:
```powershell
# This is done automatically by cargo-make / wdk-build
# The cert goes to: target/<profile>/package/WDRLocalTestCert.cer
# Install it in the target VM's Trusted Root + Trusted Publishers stores
```

### 2.3 Workspace Setup

```powershell
# Create project
mkdir helios-vgpu
cd helios-vgpu

# KMD — kernel-mode driver (KMDF System-class, no_std)
cargo new kmd --lib
cd kmd
```

**`kmd/Cargo.toml`:**
```toml
[package]
name = "helios_kmd"
version = "0.1.0"
edition = "2021"
build = "build.rs"

[lib]
crate-type = ["cdylib"]

[package.metadata.wdk.driver-model]
driver-type = "KMDF"
kmdf-version-major = 1
target-kmdf-version-minor = 33
# driver-type = "KMDF" flips the generated INF Class from Display to System
# {4d36e97d-e325-11ce-bfc1-08002be10318}.

[dependencies]
wdk = "0.4.0"
wdk-sys = "0.5.0"
wdk-alloc = "0.4.0"
wdk-panic = "0.4.0"

[build-dependencies]
wdk-build = "0.4.0"

[profile.dev]
panic = "abort"
lto = "thin"
opt-level = 1

[profile.release]
panic = "abort"
lto = true
opt-level = 3
codegen-units = 1

[features]
default = []
nightly = ["wdk/nightly", "wdk-sys/nightly"]
```

**`kmd/build.rs`:**
```rust
fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::Config::from_env_auto()?.configure_binary_build();
    Ok(())
}
```

**`kmd/Cargo.make.toml`:**
```toml
extend = "target/rust-driver-makefile.toml"
[config]
load_script = '''
#!@rust
//! ```cargo
//! [dependencies]
//! wdk-build = "0.4.0"
//! ```
#![allow(unused_doc_comments)]
wdk_build::cargo_make::load_rust_driver_makefile()?
'''
```

**`.cargo/config.toml`:**
```toml
[build]
rustflags = ["-C", "target-feature=+crt-static"]

[target.x86_64-pc-windows-msvc]
rustflags = [
    "-C", "target-feature=+crt-static",
    "-Z", "sanitizer=address",   # remove for release
]
```

Build the skeleton:
```powershell
# From the kmd/ directory, in a VS 2022 Developer Command Prompt
cargo make
# Should produce: target/debug/package/helios_kmd.inf + helios_kmd.sys
```

### 2.4 ICD Setup

```powershell
cd ../
cargo new icd --lib
```

**`icd/Cargo.toml`:**
```toml
[package]
name = "helios_icd"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]   # produces helios_icd.dll

[dependencies]
# Vulkan bindings
ash = "0.38"              # Vulkan types/enums
# Windows user-mode APIs  
windows = { version = "0.58", features = [
    "Win32_Graphics_Direct3D",
    "Win32_System_Memory",
]}
# Serialization for Venus
bytemuck = { version = "1", features = ["derive"] }
```

### 2.5 Deploying to the Target VM

Use a network share or WinRM to copy files to the target VM. Then:

```powershell
# On target VM (as Administrator):
pnputil /add-driver helios_kmd.inf /install
# Or using devcon:
devcon install helios_kmd.inf "PCI\VEN_1AF4&DEV_1050"
```

Check Device Manager → the device should appear under "System devices" (System
class {4d36e97d-e325-11ce-bfc1-08002be10318}), NOT under "Display adapters". There
is no display adapter and no Code 43 — this is a System-class KMDF function driver,
not a WDDM miniport.

Verify the device interface is reachable from user mode (this is how the ICD finds
the KMD): `SetupDiGetClassDevs(&GUID_DEVINTERFACE_HELIOS, ...)` →
`SetupDiEnumDeviceInterfaces` → `SetupDiGetDeviceInterfaceDetail` → `CreateFile` on
the returned device path should succeed.

Check for errors:
```powershell
Get-WinEvent -LogName System | Where-Object {$_.ProviderName -eq "helios_kmd"} | Select-Object -First 20
```

---

## 3. Debugging Setup

### WinDbg Kernel Debugging (Host ↔ Target VM)

The same launch-command rule applies to kernel debugging. If adding or changing a KD transport requires a QEMU
argument or `tools/launch-helios-gtk.sh` environment change, document the exact command and
ask the user to run/restart the VM. Configure guest BCD and build tools from automation, but leave VM launch to
the user after launch-command changes.

On the target VM:
```powershell
bcdedit /debug on
bcdedit /dbgsettings net hostip:192.168.x.x port:50001 key:1.1.1.1
```

On the dev machine, open WinDbg and connect:
```
File → Attach to Kernel → Net → Port: 50001, Key: 1.1.1.1
```

Useful WinDbg commands for KMDF driver debugging:
```
!wdfkd                       # load the WDF debugger extension
!wdfkd.wdfldr                # show loaded WDF drivers / framework versions
!wdfkd.wdfdevice             # inspect our WDFDEVICE (context, queues, interrupts)
lm m helios*                 # check driver is loaded
!devnode 0 1 "PCI\VEN_1AF4"  # find our device node
.reload /f helios_kmd.sys    # load symbols
```

### DbgPrint Viewing (simpler — no kernel debugger needed)

Use [DebugView](https://learn.microsoft.com/en-us/sysinternals/downloads/debugview) from SysInternals in the target VM. It captures `DbgPrint` / `KdPrint` output.

In Rust (via wdk-sys):
```rust
use wdk_sys::ntddk::KdPrint;
// KdPrint is a macro that calls DbgPrint in debug builds
// Usage:
unsafe { KdPrint!("Helios: adapter started\n\0"); }
```

### virglrenderer Logging (Host Side)

> ⚠️ **`VIRGL_DEBUG` does NOT reliably produce readable logs** with the venus render-server:
> venus runs in the `virgl_render_server` child whose stderr may not be captured. See HOST.md
> §5.1. Use QEMU `-d guest_errors` for `RESP_ERR_*`; for venus traces, capture the
> render-server child's stderr directly or build virglrenderer with logging.

---

## 4. Version Compatibility Matrix

| Component | Minimum | Recommended | Notes |
|-----------|---------|-------------|-------|
| Linux kernel | 6.13 | Latest stable | Blob resource KVM fixes |
| QEMU | 9.2.0 | Latest | Venus upstreamed in 9.2 |
| virglrenderer | 1.1.0 | Latest | Build from source with -Dvenus=true |
| Mesa (Linux guest test) | 24.2 | Latest | Venus ICD |
| WDK | 10.0.26100.0 | 10.0.26100.0 | For KMDF/WDF (KMDF 1.33) |
| VS | 2022 | 2022 | Earlier versions may work |
| LLVM | 17.0.6 | 17.0.6 | 18 has bindgen bug, avoid |
| Rust | nightly-2024-11+ | Latest nightly | 2024 edition |
| windows-drivers-rs | 0.4.x / 0.5.x | Latest | wdk = 0.4, wdk-sys = 0.5 |

---

## 5. Common Build Failures

### "cannot find -lntoskrnl"
The WDK is not on PATH or VS Developer Command Prompt was not used.  
Fix: Build inside "x64 Native Tools Command Prompt for VS 2022".

### bindgen fails with LLVM error
LLVM 18 has a known bug. Downgrade to 17.0.6.

### KMD loads but crashes on start
Check IRQL. A common mistake is calling pageable functions at DISPATCH_LEVEL during virtqueue init. Use `KeGetCurrentIrql()` assertions in debug.
