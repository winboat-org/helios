# Phase 4e Handover — Async submission (non-blocking SUBMIT_VENUS)

**Status: NOT STARTED (design only).** Read `phase4e-async-submit` + `phase5-ctx-attach` memories,
then ARCH.md §5/§13, then this. Everything here is verified against the trees on `master` (git
`28c7eb1` + the doc commit).

---

## 0. The prompt (for the next agent)

Continue Helios. Phase 5 is DONE — the Mesa venus ICD works end-to-end (vulkaninfo `driverName
venus`, vkCreateDevice, host-visible vkAllocateMemory/vkMapMemory, and `vkCmdFillBuffer`+
`vkQueueSubmit`+fence+readback = `0xDEADBEEF`, all on the real Intel ARL iGPU). Your task is **Phase
4e: make SUBMIT_VENUS asynchronous** (it blocks today), then the roadmap is: optimal vkcube present
(not a GDI hack) → DXVK → VKD3D-Proton.

**Build/test ONLY via the `win` MCP** (win11 VM): `win_cargo kmd make` (KMD), `win_meson compile`
(ICD), `win_exec` (PowerShell, elevated). Rebind the KMD after a build: `devcon update
"C:\Users\Rupansh\helios-vgpu\kmd\target\debug\helios_kmd_package\helios_kmd.inf" "PCI\VEN_1AF4&DEV_1050"`
(devcon at `C:\Program Files (x86)\Windows Kits\10\Tools\10.0.26100.0\x64\devcon.exe`). ICD registered
in `HKLM\SOFTWARE\Khronos\Vulkan\Drivers` (REG_DWORD = abs path to
`...\helios-mesa-mingw\src\virtio\vulkan\virtio_devenv_icd.x86_64.json`, data 0; the elevated SSH
session makes the loader ignore VK_DRIVER_FILES → must use the registry). mingw bin (for the ICD's
libwinpthread + to build test EXEs):
`C:\Users\Rupansh\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_*\mingw64\bin`.
Host = Arch, no sudo for you — ask the user for host-side QEMU/render-server logs when needed. A working venus
reference guest: `ssh ubuntu@localhost -p 2222`. virgl/qemu sources at `/tmp/virgl-src`, `/tmp/qemu-virgl.c`.

**Test gate:** `C:\Users\Rupansh\helios_vk_exec.exe` → `vkQueueWaitIdle => 0 OK` (today it's `-13`
`VK_ERROR_UNKNOWN`). Then re-run `helios_vk_{smoke,dev,exec}.exe` + `vulkaninfoSDK.exe --summary`
(Vulkan SDK at `C:\VulkanSDK\1.4.350.0\Bin`) for no regression. Rebuild test EXEs:
`gcc -O2 -o C:\Users\Rupansh\helios_vk_exec.exe Z:\icd\win-build\helios_vk_exec.c -IZ:\icd\mesa\include`.

---

## 1. Why async, and the honest caveat

`kmd/src/virtio/gpu.rs::submit_venus` is SYNCHRONOUS: `control.add_notify_wait_pop(...)` adds the
descriptors, notifies, then **spin-polls the used ring until that command completes**. That is the
synchronous-submit deadlock class (a submit whose completion depends on a later submit hangs the
single-threaded channel) and the suspected cause of the `vkQueueWaitIdle` gap.

Observed during Phase-5 debugging (keep in mind): every `SUBMIT_VENUS` carries `cs_size=24` — it is
just a **ring doorbell**; the real venus commands live in the ring shmem the host reads directly.
Blocking on each 24-byte doorbell may be exactly what perturbs the host's async ring/ffb processing.

**Caveat (verify, don't assume):** venus polls fence-feedback slots **straight from mapped memory**
(`vn_get_fence_status` → `vn_feedback_get_status`), NOT via our `WAIT_FENCE`. Coherency of those
slots is fine (proved by `helios_vk_poll.c`: a GPU write became visible to a CPU spin-poll). So
making the KMD non-blocking may or may not retire `vkQueueWaitIdle` on its own — measure it. If async
alone doesn't fix it, the fallback is `VN_PERF=no_fence_feedback` (confirmed working) made the ICD
default, since a synchronous/near-synchronous backend gains nothing from feedback.

---

## 2. Should this use Rust `async`/`await` in no_std? — **NO. Not a design fit.**

The word "async" here means **the I/O-completion model** (non-blocking submit + deferred completion),
**not** Rust-language `async fn`/`.await`. Do **not** introduce a futures executor. Reasons:

1. **No runtime.** `async`/`await` desugars to `Future`s that need an executor to poll. A `no_std`
   KMDF driver has none; you'd hand-roll one for zero benefit.
2. **The completion source is an interrupt/DPC, not a pollable future.** The natural kernel primitive
   is a `KEVENT` signaled from a DPC — which is exactly what `kmd/src/fence.rs::FenceTable` already
   is (`register`/`signal`/`wait_and_remove` over `KeInitializeEvent`/`KeSetEvent`/
   `KeWaitForSingleObject`). Wrapping that in a `Future` is strictly more code.
3. **IRQL boundaries don't map to wakers.** The flow is submit (PASSIVE/DISPATCH) → ISR (DIRQL) → DPC
   (DISPATCH) → wake the waiter (PASSIVE). You cannot `.await` across IRQLs; `KeWaitForSingleObject`
   on an event is the sanctioned cross-IRQL handoff.
4. **WDF is callback/handle-based by construction.** Idiomatic kernel async I/O = non-blocking submit
   + DPC completion + event wait. That already IS the `FenceTable`/`WAIT_FENCE` design. It is "async"
   in the I/O sense without any language `async`.

**Verdict:** implement with the native kernel primitives — non-blocking `add`/`notify`, an owned
in-flight buffer pool, `pop_used` drained from a DPC (or polled), and the existing `FenceTable`
KEVENTs. Keep functions plain (`fn`, not `async fn`). (If a future host daemon or a userland
component ever needs concurrency, `async` Rust with `tokio` is fine **there** — std, not the KMD.)

---

## 3. Design (poll-first; interrupt-driven is the follow-up)

**KMD `submit_venus(ctx_id, fence_id, ring_idx, venus: DmaBuffer)`** (take ownership of the venus
buffer, not a borrow):
- Build per-submit **owned** buffers — a `hdr` (`VirtioGpuCmdSubmit`, 32 B) + `resp` (`VirtioGpuCtrlHdr`)
  — in a small `DmaBuffer` (NOT the shared `self.scratch`, which can't be reused while in flight).
- `let token = unsafe { self.control.add(&[hdr, venus_bytes], &mut [resp]) }?;` then `if
  should_notify() { transport.notify(0) }`. Store `InFlight { token, fence_id, hdr_meta: DmaBuffer,
  venus: DmaBuffer }` in a fixed in-flight pool (`[Option<InFlight>; N]`, no_std). `FenceTable::register(fence_id)`.
  **Return — do not wait.**
- **Drain** completed entries at the start of every submit AND inside `WAIT_FENCE`: control-queue
  completions are in-order, so FIFO. `while can_pop() { pop_used(front.token, inputs, outputs)?;
  check resp OK; FenceTable::signal(front.fence_id); free the slot }`. Reconstruct the `inputs`/`outputs`
  slices for `pop_used` from the stored buffers (raw pointers, mirroring the existing scratch pattern,
  to dodge the borrow checker while the buffers live in `self`).
- **Backpressure:** if the pool/queue is full, spin-drain (`can_pop`) until a slot frees.

**KMD `handle_wait_fence`** (kmd/src/ioctl.rs): drain the used ring under the virtio lock, then
`adapter.fences.wait_and_remove(fence_id, timeout_ns)` (FenceTable is DONE + correct, just unwired).

**ICD `vn_renderer_helios.c`:** stop the premature `sync->val = sync_values[j]` in `helios_submit` /
`helios_bo_create_from_device_memory` (correct only when submit is synchronous). Record the batch's
fence_id on its syncs; `helios_wait` (ops.wait) issues `WAIT_FENCE(fence_id, timeout)` then sets
`sync->val`. **Verify** which syncs venus actually waits on via ops.wait vs. memory polling — the ring
seqno + fence feedback are memory-polled and independent of `WAIT_FENCE`.

**Buffer lifecycle is the crux:** `add` only borrows the buffers for the call, but the device DMAs
their **physical pages** until `pop_used`, so the owned `DmaBuffer`s must persist in the pool until
popped. `pop_used(token, inputs, outputs)` needs the same slices reconstructed from the stored buffers.

**virtio-drivers 0.13 API** (`queue.rs`): `unsafe fn add(inputs:&[&[u8]], outputs:&mut[&mut[u8]]) ->
Result<u16>` (token), `should_notify()`, `can_pop()`, `unsafe fn pop_used(token, inputs, outputs) ->
Result<u32>`. `CTRL_QUEUE_SIZE=64`. `DmaBuffer::{new(len), as_slice, as_mut_slice}` (hal.rs).

---

## 4. Interrupt-driven completion (AFTER poll-first works) + the blocker

Replace the WAIT_FENCE/submit-time poll with a real interrupt: `control.set_dev_notify(true)` in
`gpu.rs::init` (false today); ISR reads the virtio **ISR-status register at DIRQL** to claim+ack
(needs a transport-independent pointer it can read **without** the DISPATCH-level virtio lock — see
`interrupt.rs` module docs); DPC pops the used ring + `FenceTable::signal`. `interrupt.rs` already has
the ISR/DPC skeleton (ISR returns FALSE, dev-notify suppressed).

**BLOCKER (real, deferred):** creating a `WDFINTERRUPT` made the device fail D0 with
`STATUS_DEVICE_POWER_FAILURE` (`pnp.rs:135-143`). KMDF connects+enables the interrupt at D0 and it
failed on this device's interrupt assignment — likely **MSI-X vs INTx**: virtio-modern prefers MSI-X;
`virtio-drivers` `PciTransport`'s interrupt handling and KMDF's `WDFINTERRUPT` config must agree, and
the translated resource list (`evt_device_prepare_hardware`'s `_resources_translated`, currently
ignored) must be parsed to match. Device `MSISupported` was empty in the registry Interrupt Management
key. Resolve this before the interrupt path; **poll-first sidesteps it entirely.**

---

## 5. Test/commit discipline

Keep the tree green: KMD must `infverif VALID`; rebind with devcon; the gate is `helios_vk_exec`
`vkQueueWaitIdle => 0`, then no regression on smoke/dev/exec/vulkaninfo. Commit the KMD (protocol +
`gpu.rs` + `ioctl.rs`) and the ICD (submodule) as scoped commits, bumping the `icd/mesa` gitlink.
Don't commit until it works (the project rule). Everything is currently committed + clean at `28c7eb1`.
