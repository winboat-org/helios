# Handoff — Helios IDD black frames (2026-06-25)

> **⛔ SUPERSEDED (2026-06-26) — read `FABLE5_HANDOFF.md`.** This predates the M1 WDDM-3.2 raise and
> the finding that the blocker is the IDD failing its PnP post-start (Code 43), not black-frame
> delivery. Historical only.

Paste the **PROMPT** section into a fresh session.

---

## PROMPT

Continue the Helios WDDM + Looking Glass IDD bring-up in `/home/rupansh/helios-vgpu`.
Goal (unchanged, locked): DWM composites the whole Windows desktop **on the Helios WDDM render
adapter** (venus → host GPU) and the Looking Glass IDD displays those composed frames in the
Looking Glass client. Do not pivot to per-app venus.

**Read first:** the `venus-enum-adapter-probe-regression` memory (it has the full chain of this
session's findings, verbatim), then this doc. Treat Microsoft docs as authoritative.

### Current verified state (live this session)
- **"No frames" was fixed** (1-line ICD adapter-probe fix): venus enumerates, `D3D11CreateDevice
  (Helios)=S_OK`, the IDD acquires `1920x1080` frames again. Persists across reboot.
- **Frames flow but are BLACK**, and the cause is now precisely root-caused (NOT sync, NOT the
  keyed mutex — those were ruled out with hard evidence):
  - DWM composes the desktop on Helios into venus resources **`res_id 52/54/55`, 1952x1088**,
    UMD/DXVK-created on venus **ctx=8** — confirmed to have real pixels (`helios_umd.log`:
    `DXGI Present sample ... nonzero=241/366`).
  - The IDD reads a **different** venus resource: **`res_id 147`, 1920x1080**, **KMD-self-backed**
    on the KMD's internal venus **ctx=2** (via `GetStandardAllocationDriverData`). **Nothing ever
    renders into 147** — it appears nowhere in the UMD render log. The IDD's readback samples it
    all-zero (`looking-glass-idd.txt`: `sampleNonZero=0/360`).
  - So DWM's composed pixels (52/54/55) are never delivered into the surface the IDD reads (147).
- **Ruled out, with evidence:**
  - `DxgkDdiPresent` is **never called** (unconditional KMD breadcrumb `PBcall` stayed absent
    while DWM presented). MS docs: it targets "the current primary of the device"; Helios is
    render-only (no VidPN source / no primary) so the OS never invokes it. ⇒ a KMD present-blit
    is impossible.
  - The keyed-mutex / Win32-external-semaphore emulation (ICD+DXVK) is broken 3 independent ways
    (legacy `D3DDDI_FENCE` vs monitored-fence reads; DXVK GPU-ordering arm dropped when
    `m_fence->kmtLocal()==0`; venus signal/wait never reach a host VkSemaphore;
    `kmtLocal()` published as the shared handle → `0x887A0026` abandoned). This explains the
    `0x887A0026` error but is **secondary** — it's a SYNC layer, and the BLACK is a
    SHARING/DELIVERY problem (147 is never filled regardless of sync).

### The decision (committed): fix path (a) — eliminate the proxy / unify the venus resource
The UMD's `create_resource(tex2d)` (`umd/src/forward.rs`) **always** mints a fresh DXVK VkImage
(the "proxy", 52/54/55) and has the KMD adopt it. There IS an import path —
`open_ddi_texture2d(w,h,fmt,bind,misc,hKMResource,renderer_resource_id)` (`forward.rs:1121-1156`)
imports a KMD-backed venus resid into DXVK and only falls back to a fresh proxy texture if it
returns 0 — but it is **never exercised** for these surfaces (0 `open_resource ddi-shared`
log lines). DWM renders into a throwaway proxy that's never delivered to 147.

Pursue **(a)**: make DWM render **directly into the same venus resource the IDD reads** (zero-copy),
by routing the IddCx swapchain backbuffer / DWM's composition render target through the
venus-import path instead of a proxy. This addresses the root and avoids re-implementing
cross-process present/copy/sync. Do **not** pursue (b) a present-copy unless (a) proves
infeasible.

### First tasks (ordered)
1. **Make the IDD process's UMD log visible — this is the make-or-break unknown.** The IDD's D3D
   device opens surface 147 in a restricted IddCx host process whose `helios_umd` logging is
   currently invisible (no IDD-process lines in `C:\Windows\Temp\helios_umd.log` at all — likely a
   write-permission/context issue). Make the UMD log to a path the IDD process can write
   (e.g. `C:\ProgramData\Helios\umd-<pid>.log`, world-writable) or via ETW. Then read whether the
   IDD opens 147 via `open_ddi_texture2d` and whether that import **succeeds** (`ddi-shared ok`) or
   **falls back to a proxy** (`ddi-shared failed; falling back to metadata texture`).
2. **If the import falls back / fails:** fix it so DXVK can use the KMD-self-backed venus resource
   (147) as a real render/read target. NOTE the KMD self-backs standard allocations as a mappable
   **HOST3D blob** (`create_allocation.rs`, `GetStandardAllocationDriverData`); DXVK importing it
   as a render-target/SRV `VkImage` may require the backing to be an **image-capable** venus
   resource (look at `VkImportMemoryResourceInfoMESA` / `helios_bo_create_from_resource_id` in
   `icd/mesa/.../vn_renderer_helios.c` and `dxvk-helios/src/dxvk/dxvk_image.cpp`).
3. **Ensure DWM's composition RT and the IddCx swapchain backbuffer resolve to the SAME venus
   resid.** Confirm whether the 1952x1088 (composition) vs 1920x1080 (monitor) difference is just
   alignment padding (then unify) or a genuinely larger composition surface (then a scaled blit is
   unavoidable → fallback (b)).
4. Re-verify the IDD reads non-zero (`looking-glass-idd.txt` `sampleNonZero` > 0) and confirm with
   the user that the Looking Glass client shows the desktop.

### Diagnostics already in place (KMD `.99`, uncommitted, READ-ONLY — never alter behavior)
- `diag::record_named` / `record_named_bytes`: fixed-name registry values (survive the flooding
  `S<idx>` ring). Read live from `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`.
- `dxgkddi_present` writes `PBcall/PBflag/PBcnt/PBalst` + `PBsrc/PBdst/...` (present hook trace).
- **Allocation ring `AE0..AE7`** (`record_alloc_event` in `create_allocation.rs`): `AE{n}r`=resid,
  `AE{n}d`=(w<<16|h), `AE{n}c`=ctx (bit31=open). Captures one boot's surface map. This is how
  147/52/54/55 were correlated.
- `present_alloc_info` / `gpu::blob_lookup` helpers (resolve a present/alloc handle → resid+dims).
- Consider reverting these read-only diags before committing, or keep them gated.

### Key files
- `umd/src/forward.rs` — `create_resource` (proxy mint), `open_ddi_texture2d` import path
  (~1121), `dxgi_present` (~4855, `hDst=0`).
- `umd/bridge/dxvk_bridge.{h,cpp}` + `umd/src/bridge.rs` — `open_ddi_texture2d`,
  `get_resource_memory_info`, `transfer_resource_ownership`.
- `kmd_render/src/ddi/create_allocation.rs` — `GetStandardAllocationDriverData` self-back,
  adopt-vs-self-back logic, `AllocationContext`.
- `icd/mesa/src/virtio/vulkan/vn_renderer_helios.c` — `VkImportMemoryResourceInfoMESA` /
  `helios_bo_create_from_resource_id`, the (broken) WDDM sync emulation.
- `dxvk-helios/src/dxvk/dxvk_image.cpp` — `DxvkKeyedMutex`, shared-image import.
- `LookingGlass/idd/LGIdd/CSwapChainProcessor.cpp` — acquire + D3D11 readback (`SwapChainNewFrameD3D11`),
  `IddCxSwapChainInSystemMemory` (a keyed-mutex-free system-memory mode exists; B-option fallback).

### Build / deploy / VM gotchas (hard-won this session)
- **ICD:** `win_meson ["compile","-C","C:\\Users\\Rupansh\\helios-mesa-build"]` (reads `Z:\icd\mesa`
  direct); deploy = copy `...\helios-mesa-build\src\virtio\vulkan\vulkan_virtio.dll` over the
  manifest-referenced `C:\ProgramData\HeliosVulkan\vulkan_virtio-879f56b158e4.dll`. No reboot.
- **KMD:** bump version 4× in `kmd_render/build.rs` + `Cargo.make.toml` (currently `.99`),
  `win_cargo kmd_render ["make","--makefile","Cargo.make.toml"]`, then
  `Z:\tools\install-helios-kmd.ps1`, then **REBOOT** (`shutdown /r /t 0 /f`) — the install warns a
  reboot is the reliable activation path, and a PnP restart of Helios **wedges the IDD swapchain**
  (`keyed mutex abandoned` → stuck `ReplugMonitor`); only a full guest reboot recovers it. Each
  cycle ≈ 3-4 min; minimize KMD churn.
- **UMD:** rebuild + redeploy is reboot-free (hotplug script) — prefer iterating on the UMD/ICD.
- Boot is slow (~90-150s); SSH `No route to host` during boot is normal, not a crash. Verify with
  `(Get-CimInstance Win32_OperatingSystem).LastBootUpTime` + System event 41 (only logs UNCLEAN
  shutdowns) to distinguish a reboot from a BSOD.
- Display/monitor state: use WMI / session-1 probes, NOT SSH session-0 APIs (misleading).
- `DxgkDdiPresent` is PASSIVE_LEVEL; `RtlWriteRegistryValue` (diag) is fine there. Do NOT add
  registry writes to DISPATCH-level DDIs (submit_command) — use atomics.
- The tree has a large uncommitted pile across KMD/ICD/UMD/DXVK/IDD (~3000+ lines). Do NOT commit.

### Negative results — do not repeat
- KMD present-blit in `dxgkddi_present` (DDI never fires on render-only Helios).
- Fixing the keyed-mutex sync to fix BLACK (it's a delivery/sharing problem, not sync; the keyed
  mutex only governs `0x887A0026`).
- Disabling `DECLARE_CROSS_ADAPTER_RESOURCE` (regresses the venus/D3DKMT escape path).
- A standalone QEMU launch; ask the user to (re)launch the VM if launcher/transport changes.
