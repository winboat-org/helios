# Handoff — THE ALIASING DIVERGENCE IS ROOT-CAUSED AND FIXED (guest DXVK deferred-clear hole), via validate-boot + Intel-boot elimination (2026-07-03, fifth session)

## ⚡ FINAL RESULT (read this first — supersedes §1's "prime suspect")

**Root cause of the §4 clears-diverge/copies-propagate divergence: a guest-side DXVK bug.**
`DxvkContext::prepareSharedImages()` flushed deferred clears only for images present in
`m_nonDefaultLayoutImages` — but an image whose ONLY pending work is a deferred clear never
leaves its default (GENERAL) layout and is absent from that list. A `ClearRenderTargetView` +
`Flush` on a shared surface therefore left the clear **deferred in the producer's context
indefinitely** — it only materialized when a later same-context operation (e.g. a CPU
readback) forced it. Every observation fits: clears diverged both directions, copies
propagated (not deferrable), a producer-side readback made clears propagate (it flushed the
deferral), and v1-D's "stomp" (dev1 later reading its OWN older clear after dev2 wrote) was
the deferred clear finally landing over dev2's newer content.

**Eliminated on the way, with instruments:** host compression/fast-clear metadata (divergence
identical on ANV and NVIDIA; identical with LINEAR-tiling images — no aux state exists);
missing `VK_QUEUE_FAMILY_EXTERNAL` barriers (implemented, emission confirmed via debug log —
20 correctly-interleaved release/acquire events — behavior unchanged); VUID-02726-class guest
violations (host validation layers: zero messages on the entire export→import→bind path).

**Fix: dxvk-helios `ecbd8f78`** — `prepareSharedImages` scans `m_deferredClears` itself for
shared-image entries. **Verified: the shared-content probe passes every step** (C0 = clear#2
propagates immediately, D = clear#3 propagates reverse, E = copies) with OPTIMAL tiling on
ANV. The LINEAR-forcing experiment was reverted (its justification was disproven); the
external QFOT release/acquire barriers were KEPT (spec-required availability mechanism for
external memory, not the divergence fix — comment in code says exactly this). Deployed as
UMD 42dfad843610bab1 on the Intel boot; IDD self-converged after a devnode restart
(FAILED_POST_START recovery from deploy-window kills) and is acquiring. NVIDIA re-verification
of the probe still pending (expected same — the bug was guest-side and vendor-independent).

**Remaining owner-visible acceptance: real desktop frames in the LG client** — the §4
divergence was the identified blocker between dwm's composition and the acquired frames.

## ⚡⚡ THE NEW FRONTIER (post-fix, owner-observed): the KMD present nop (audit K-B2)

After the deferred-clear fix went live, the owner saw — for the first time — **a single frame
with real desktop structure** (taskbar geometry with rounded overlay + border shading, black
DX overlays, heavy corruption, red tint), frozen, then black. Instrument chain (18:1xZ):

- IDD healthy: bound swapchain, acquires at dwm cadence (frame 63+), dwm stable, no dumps.
- IDD content sampler: `sampleNonZero=0/357, first=center=0x00000000` at steady state — the
  three rotating IddCx swapchain buffers are ALL-ZERO.
- dwm UMD log: `DXGI Present: #51..#65 src=0x80004540 dst=0x0 copied=false flags=0x2` —
  flip-model presents; the UMD-level copy path has no destination BY DESIGN (flip). Present
  count matches acquire count (the binding is live, not stale).
- The stage that must move pixels (dwm's composition buffer → IddCx swapchain buffer, the
  dxgkrnl BLTQUEUE present on the render adapter) lands in **`kmd_render`'s
  `dxgkddi_present` — which emits a 4-DWORD 'HEPR' NOP DMA (audit K-B2, confirmed in
  source `ddi/display.rs`). Composed pixels are never blitted.**
- The corrupted transition frame's content is consistent with the KMD **GDI executor** (the
  only path that CPU-writes those surfaces today) — explains the GDI-ish look, black DX
  overlays, and plausibly the red tint (GdF format handling; evaluate after pixels flow).

**Next session = implement the real present copy (C3/K-A2/K-B2):** at `DxgkDdiPresent`
(or the submit path), execute an actual src-allocation → dst-allocation copy host-side via
venus (both allocations carry venus resids; identical creation parameters make a raw
blob-to-blob byte copy image-correct on the same host device — or do it properly with
imported VkBuffers + vkCmdCopyBuffer in the KMD's venus context), then retire the fence.
Verify with the IDD sampler (`sampleNonZero` > 0 and hash changing per frame), then owner
eyes. Note `PBcall/PBflag/PBcnt` named diag values already exist in `dxgkddi_present` for
confirming call shape — but the registry diag ring burns out in minutes (the C-class tracer
strip is still pending), so rely on the atomics/CollectDbgInfo HDBG report instead.

---

# Session record below (validate boot first, then the Intel boot that closed it)

Continues `HANDOFF_BLOBTABLE_ALIASING_2026_07_03.md` (its §4 leads were this session's brief).
Deployed: KMD 22.22.42.0 (unchanged), UMD b89f9918 (unchanged), **LGIdd 16.41.16.666**
(LookingGlass `846a7edd` — the §3 frame-held replug gate — devcon-installed this session).

## 0. The clean cold boot (16:22 IST, pre-validate) — §6b checklist results

1. **Self-convergence PASSED — first fully zero-touch cold boot.** First binding stale →
   stale-binding watchdog replugged at +10 s → second binding stable, acquires at dwm cadence
   (67 frames by boot+20 min), zero manual actions, zero kill dumps, all devnodes Code 0.
   After the LGIdd 16.41.16.666 devcon deploy (device restart), the IDD self-converged AGAIN
   in 34 s. Two consecutive self-convergences on the C5 state machine.
2. **C1 boot hole did NOT fire** (host log clean through boot+20 min). Intermittent — keep the
   rotated-log discipline.
3. **Blob table healthy**: 132/8192 at steady state, zero rejects, zero ctrl-timeouts.
4. **Aliasing repro confirmed cold** (identical A/B pass, C stale, D own-clears-only, E copy
   propagates). Owner did not watch the client at bind time on this boot.

## 1. §4 LEAD 1 RESULT (the headline): the sharing path is spec-clean AND still diverges

**Static analysis** (all verified in source this session): the external+dedicated shape is
plumbed end-to-end — DXVK chains `VkMemoryDedicatedAllocateInfo` on both sides (creator:
dedicated→`VkExportMemoryAllocateInfo`; opener: dedicated→`VkImportMemoryResourceInfoMESA`,
`dxvk_memory.cpp:1201`), the ICD forwards the full pNext chain and `fix_alloc_info` preserves
dedicated while rewriting handle types to DMA_BUF, vkr (`vkr_device_memory.c:246`) translates
res-info→fd-info in place preserving the chain, and the venus wire encoder serializes both
structs. The `meta_bind=0xa8` vs `open_bind=0x28` log difference is a DDI-vs-API namespace
artifact (both sides feed DXVK 0x28 through `api_bind_flags`) — NOT a usage divergence.

**Dynamic confirmation (validate boot, host vulkan-validation-layers 1.4.350.1-1 active):**
`HeliosSharedProbe` re-run under validation reproduced the divergence EXACTLY (C stale, D
own-clears, E propagates) while the host log recorded **ZERO validation messages for the
probe's entire export → import → bind → clear → readback path** — no vkAllocateMemory, no
bind, no external-memory, no image-create complaint. **The VUID-02726-class hypothesis for
the sharing path is dead**; the guest emits spec-legal Vulkan.

**Prime suspect now: missing `VK_QUEUE_FAMILY_EXTERNAL` release/acquire barriers.**
`grep -rn QUEUE_FAMILY_EXTERNAL dxvk-helios/` → zero hits. DXVK never emits the external
queue-family ownership transfers that the spec requires for cross-instance memory
consistency on external images (shared images do stay in `VK_IMAGE_LAYOUT_GENERAL` —
`d3d11_texture.cpp:241` skips OptimizeLayout for shared — but layout alone is not the
availability mechanism). On native Windows this is masked: the WDDM driver coordinates
shared-allocation compression below the API. On our Linux dma_buf path nothing does.
Validation CANNOT flag a missing external barrier (it cannot know two images alias), which
is consistent with a clean log + divergence. The clears-diverge/copies-propagate signature
matches metadata-encoded clears never being made available to the second instance.

**Fix shape (C7.2-aligned, not started):** bracket shared-image producer work with a release
barrier to `VK_QUEUE_FAMILY_EXTERNAL` (at flush / keyed-mutex release / after RT writes) and
consumer work with an acquire from it — in DXVK's backend for images with
`DxvkSharedHandleMode != None`. This also converges with the §4c ICD-side
`VK_KHR_external_memory_win32` emulation: one owner for external-memory correctness.
**Discriminator still pending: the Intel/ANV comparison (owner relaunch)** — if ANV passes
without barriers, NVIDIA metadata is confirmed as the differentiator; the barrier fix is
required either way (spec), but Intel tells us whether it will be sufficient on NVIDIA.

## 2. NEW VUID classes surfaced by the validate boot (real bugs, NOT the aliasing mechanism)

1. **`VUID-VkGraphicsPipelineCreateInfo-Input-08733` (storm, every dwm-style worker):**
   `R16G16_SINT`/`R32_SINT` vertex attribute formats vs `float32` SPIR-V input variables
   (`v0_xy`…`v4_x`). dwm's composition quads use SINT vertex layouts; the UMD's synthetic
   input-signature blobs (`forward.rs:4605-4673`, audit C-class) type everything as float →
   dxbc-spv declares float inputs → UB per draw. NVIDIA currently renders it correctly
   (owner saw a correct desktop in transient frames), but this is exactly the
   works-until-a-driver-update class the audit§C7 bans. Fix: carry true component types into
   the synthetic signatures (the DDI signature entries lack types; derive from the shader's
   IL input declarations in dxbc-spv, or declare typeless-compatible inputs).
2. **`VUID-VkDescriptorBufferInfo-buffer-02999` (storm):** null-descriptor UNIFORM_BUFFER
   writes with `range != VK_WHOLE_SIZE` (48/128/256/4000). Even with nullDescriptor enabled,
   range must be WHOLE_SIZE. DXVK null-uniform-buffer path. Plus
   `VUID-vkCmdBindVertexBuffers2-pBuffers-04112` (null vertex buffer with nonzero offset) —
   same family.
3. **`VUID-vkCmdDraw-imageLayout-00344` (twice):** image known in `GENERAL` sampled through
   a descriptor declaring `SHADER_READ_ONLY_OPTIMAL` ("t0"). A shared/GENERAL image being
   sampled by a pipeline whose descriptors assume the optimized layout — layout-discipline
   mismatch in the shared-image consumer path; worth chasing with the barrier work (same
   code region).

## 3. Validate-boot IDD kill: SetDevice overran the frame-release deadline (environment-induced)

Under validation everything host-side is ~an order slower. Boot sequence: OS assigned the
swapchain, dwm presented immediately, the processor thread sat inside
`IddCxSwapChainSetDevice → IddSwapChain::Open` for 25+ s (stale-binding watchdog fired and
was correctly DEFERRED by the binding-in-flight gate the whole time) → IddCx's kernel
watchdog fired `ReportBugcheckForSwapChainTimeoutDriverDidNotReleaseFrame` → WUDFHost
terminated (twice: PID-1592 16:49:01, PID-1576 16:53:47 — full stacks via cdb on the
PID-1576 dump; thread 12 = SetDevice in flight, thread 4 = the IddCx bugcheck report) →
devnode `FAILED_POST_START`, client loops "Waiting for the host to restart".

This is a **different §3 variant** from the departure-mid-hold one fixed this session
(LGIdd 846a7edd gates ReplugMonitor on m_frameHeld): here NO frame was ever acquired — the
deadline ran against a bring-up that could not finish in time. The gate cannot and should
not help. Disposition: environment-induced under validation slowness (the same 22 s
SetDevice was observed in normal boot churn without a kill — the deadline only bites when
dwm has already presented). Options if validate boots need a live IDD later: IddCx debug
control to relax the watchdog for instrumented boots only (documented debug knob, not a
production change), or accept no-display on validate boots — the §4 probe does not need the
IDD at all (proven this session).

## 4. Ops learned

- The shared-content probe is IDD-independent: two D3D11 devices in one process. Validate
  boots can gather §4 evidence with the IDD dead.
- cdb (`C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe`) + `.symopt+0x40` +
  `!analyze -v; ~*k` symbolizes the WUDFHost dumps directly on the guest.
- Validation output goes to the same `/tmp/helios-qemu-stderr.log` tee; vkr prefixes worker
  pid per guest process; bracket experiments by line count (`wc -l` before/after).
- The validation layer's `duplicate_message_limit` (10) silences repeat VUIDs per worker —
  early lines are the complete class inventory.
- pgrep against `qemu-system-x86_64` without `-f` fails (comm truncates to 15 chars) — use
  `pgrep -f qemu-system`.

## 5. State to carry forward

- LGIdd 16.41.16.666 deployed + committed (LookingGlass `846a7edd`); IDD currently
  FAILED_POST_START on the validate boot (expected; recovers on a normal relaunch).
- Host log rotations: `.2026-07-03-session4` (pre-cold-boot), `.2026-07-03-session5-coldboot`
  (the clean cold boot), current = the validate boot.
- Next actions, in order: (1) **Intel/ANV comparison relaunch** (owner:
  `HELIOS_QEMU_RENDER_GPU` unset/intel, validate optional) → run `HeliosSharedProbe`; (2) if
  ANV passes → implement the external-barrier release/acquire in DXVK for shared images (and
  decide §4c win32-emulation as the vehicle); (3) UMD synthetic-signature component types
  (§2.1); (4) DXVK null-descriptor range + null-VB offset cleanups (§2.2); (5) chase the
  00344 layout mismatch alongside the barrier work; (6) still open from before: §5 residual
  C1 boot hole, P2/C6 linear GDI mismatches, KMD diag-tracer strip.

## 7. Copy-paste prompt for the next session

> You are continuing the Helios vGPU project in /home/rupansh/helios-vgpu. Read
> `HELIOS_FIRST_PRINCIPLES_AUDIT.md` (contracts C1–C7, hack inventory), then
> `HANDOFF_VALIDATE_BOOT_2026_07_03.md` in full — start with its ⚡ and ⚡⚡ sections.
> STATE: the §4 shared-surface aliasing divergence is ROOT-CAUSED AND FIXED — it was a
> guest DXVK bug (prepareSharedImages never flushed deferred clears of shared images that
> stayed in GENERAL layout; fix = dxvk-helios `ecbd8f78`, scans m_deferredClears directly;
> the shared-content probe passes every step with OPTIMAL tiling). Host-metadata theories
> are DEAD (eliminated by instrument: validation-clean path on NVIDIA, identical divergence
> on ANV, identical with LINEAR tiling, QFOT external barriers emitted-and-ineffective; the
> barriers were KEPT as the spec external-memory availability contract, the LINEAR forcing
> was reverted). After the fix went live the owner saw — first time ever — a frame with
> real desktop structure (taskbar geometry, black DX overlays, red tint, corruption,
> frozen, then black): content consistent with the KMD GDI executor, the only path that
> writes those surfaces today.
>
> THE FRONTIER (⚡⚡, evidence chain complete): **the KMD present nop (audit K-B2)**.
> dwm flip-presents (`DXGI Present dst=0x0 copied=false` at exactly the IDD's acquire
> cadence), the three rotating IddCx swapchain buffers sample ALL-ZERO
> (`sampleNonZero=0/357` in the IDD log), and the stage that must move pixels — dxgkrnl's
> BLTQUEUE present executed on Helios — is `kmd_render/src/ddi/display.rs::dxgkddi_present`,
> which emits a 4-DWORD 'HEPR' NOP DMA. Note its current shape: present flags bit (1<<2)
> returns SUCCESS early, and src/dst allocation lists are already read into atomics
> (PBcall/PBflag/PBcnt named diag values exist, but the registry diag ring burns out in
> minutes — use the DISPATCH-safe atomics / the HDBG CollectDbgInfo report instead).
>
> TASK (C3/K-A2/K-B2, work it first): implement the REAL present copy. At DxgkDdiPresent
> (or its submit path), execute an actual src-allocation → dst-allocation copy host-side
> via venus — both allocations carry venus resids in the KMD's allocation state; with
> identical creation parameters a raw blob→blob byte copy is image-correct on one host
> device, or do it properly with imported VkBuffers (VkImportMemoryResourceInfoMESA) +
> vkCmdCopyBuffer in the KMD's own venus context — then retire the fence. Beware the
> audit's K-A3 constraint (no unbounded DISPATCH-level virtio waits under the spinlock; the
> bounded-poll + poison-latch infrastructure from P0 exists). Acceptance ladder: (1) IDD
> sampler shows sampleNonZero > 0 and a per-frame-changing sampleHash; (2) the owner
> watches the live desktop (moving cursor, dragging windows) in the LG client, sustained.
> Only (2) closes the milestone. Then evaluate the red tint / corruption (suspect the GDI
> executor GdF format handling and/or 10bpc IddCx path) against real content.
>
> Deployed right now (Intel boot, `HELIOS_QEMU_RENDER_GPU` default): KMD 22.22.42.0, UMD
> hash 42dfad843610bab1 (dxvk ecbd8f78 linked), LGIdd 16.41.16.666 (LookingGlass 846a7edd,
> frame-held replug gate), ICD unchanged. Everything is committed: parent through 755d59a,
> dxvk-helios ecbd8f78, LookingGlass 846a7edd. Also open, in rough order: NVIDIA re-verify
> of the shared-content probe (expected pass — the bug was guest-side); the SetDevice-
> overrun DriverDidNotReleaseFrame kill variant (bring-up exceeding IddCx's frame deadline
> — hit under validation slowness and during deploy churn; the frameHeld gate correctly
> does not cover it); §5 residual C1 boot hole (did NOT fire on the last cold boot; keep
> rotated-clean host logs); UMD synthetic input-signature component types (dwm binds
> R16G16_SINT vertex formats against float32 SPIR-V inputs — Input-08733 storm, NVIDIA
> tolerates today = C7 time bomb, forward.rs:4605); DXVK null-descriptor range/offset
> violations (buffer-02999/04112); the imageLayout-00344 GENERAL-vs-READ_ONLY sampled-image
> mismatch; the KMD diag-tracer strip (registry ring burns its 3000 cap in minutes); P2/C6
> linear GDI-surface import-size mismatches.
>
> Ops: KMD version bump before every deploy (kmd_render build.rs + Cargo.make.toml), build
> via win_cargo, devcon at `C:\Program Files (x86)\Windows Kits\10\Tools\10.0.26100.0\x64\devcon.exe`
> (instance IDs need the `@` prefix for devcon restart), UMD hotplug AFTER KMD install via
> `tools/hotplug-helios-umd.ps1 -Mode ProgramData -KillUmdUsers -NoProbe -RestartDevice
> -UmdDll ...\umd\target\release\helios_umd.dll`. dxvk rebuild: edit on Z:, copy changed
> files to C:\Users\Rupansh\dxvk-helios, `meson compile -C C:\Users\Rupansh\dxvk-build`
> with `C:\Program Files\LLVM\bin` AND MSVC `...\bin\Hostx64\x64` on PATH (vcvars64 alone
> lacks clang-cl), purge `...\helios-vgpu\umd\target\release\.fingerprint\helios_umd-*`,
> `win_cargo umd build --release`, hotplug. LGIdd builds via win_looking_glass_idd (stampinf
> auto-versions; the InfVerif x86-DLL error is non-fatal). Probes run via schtasks /IT
> (HeliosSharedProbe, HeliosBlobTruthProbe, HeliosBlobProbe); outputs in
> C:\Users\Rupansh\helios-probe\. IDD log: `C:\ProgramData\Looking Glass (IDD)\
> looking-glass-idd.txt` (acquires + the per-30-frames content sampler). dwm/UMD logs in
> C:\ProgramData\Helios\ (umd-<pid>.log; `DXGI Present` lines). Host venus log
> /tmp/helios-qemu-stderr.log (rotate before boots; vkr lines carry no timestamps). QMP at
> /tmp/helios-tpm/mon.sock. Each Helios device restart churns the dwm/IDD pairing and can
> kill WUDFHost (deploy-window collateral); recover the devnode with devcon restart
> `@ROOT\DISPLAY\0000` and let the C5 watchdog reconverge — but batch deploys to minimize
> restarts. WUDFHost dumps: cdb `!analyze -v; ~*k` with `.symopt+0x40` works on-guest.
> The overseer's standing directive: no hacks, no kick rituals, loud failure over fake
> success; only owner-visible LG-client output closes milestones. Ask before cold boots or
> VM relaunches.
