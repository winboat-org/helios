# REFACTOR_REVIEW.md — Phase-1 review of `kmd_render/` and `umd/`

**Date:** 2026-07-23 · **Baseline:** KMD `22.22.142.0`, branch `wddm` · **Charter:** `REFACTOR_HANDOFF.md` Phase 1

This is the Phase-1 deliverable of the behavior-preserving quality refactor: one review
document containing dependency-ordered, atomic recommendations for `kmd_render/` (WDDM 3.2
render+display miniport, no_std Rust) and `umd/` (D3D11 UMD, d3d10umddi frontend bridged via
cxx to dxvk-helios). No production code was modified in producing it.

## Method and honesty statement

- **Finding pass (complete):** 16 parallel adversarial reviewers — 11 scoped (per subsystem,
  including three splitting `umd/src/forward.rs` by range) and 5 cross-cutting sweeps
  (legacy paths, duplication, unsafe/lock-order, concurrency/timeouts, error paths). A 17th
  sweep (dedicated telemetry) failed to launch; telemetry coverage comes from the scoped
  reviewers and other sweeps, which produced 12 telemetry findings — the axis is covered,
  but less redundantly than the others. Output: **169 findings + 187 minor notes**, every
  finding anchored to file:line evidence.
- **Adversarial verification (truncated):** each finding was assigned an independent
  skeptic instructed to refute it (factual / liveness / behavior-safety / cosmetic-guarantee
  checks). **21 of 169 verdicts completed** before an API session limit killed the rest:
  **3 CONFIRMED, 18 MODIFIED, 0 REFUTED.** Verifier corrections are reproduced verbatim in
  the affected entries and are **authoritative over the original claim**. The remaining 148
  findings are marked **UNVERIFIED**; the 0% refutation rate among completed verdicts is
  encouraging but does not transfer — the very first implementation step for any UNVERIFIED
  entry is to re-verify its cited lines against the code.
- **Dedup, ordering, and this document:** performed by the lead reviewer from the finder
  JSON and completed verdicts. 169 raw findings collapse into **20 defect items (Part I)**
  and **77 dependency-ordered recommendations (Part II, tranches 1–7)**; duplicate reports
  are folded into their canonical entry and listed there. Independent convergence (up to 8
  reviewers reporting the same item) is called out where it exists — it is the strongest
  confidence signal available for unverified entries.

## Frozen baseline — no recommendation may break these

- DWM renders directly into the exact Windows-designated OPTIMAL primary; there is no
  guest primary-to-scanout copy.
- KMD refresh markers capture a Venus wire-fence watermark under the `WddmNotifyGuard`
  lock-order proof and are consumed by the used-ring DPC.
- The UMD bounded 10 ms condition-variable frame-completion gate is a safety contract
  (normal completion wakes immediately; ~0.48 ms steady average) — a KEEP under the
  timeout doctrine, never a polling hack.
- QEMU reconstructs the modifier-less OPTIMAL image without changing the virtio-gpu
  protocol ABI.
- `ScanoutDiag` is absent and must remain absent during primary tests.
- Kernel invariants (CLAUDE.md): no `diag::record`/pageable code above PASSIVE; no
  allocation or spin-waits in ISR/DPC; per-arm validation of every guest-supplied
  size/offset; no panics in DDI paths; blob-window offsets below the VidMm reserve belong
  to dxgkrnl; the SupportsCpuHostAperture segment reports LAST; KMD version bumps touch
  all three sites; Venus commands flush before fence signal.

**Timeout doctrine** (applied throughout): a bounded timeout around a real
event/fence/condvar wait is a safety contract and is kept; an arbitrary delay used to make
ordering appear correct is a hack and is flagged. Every timeout-touching entry states its
classification.

## Regression gate (every implementation tranche)

KMD + release-UMD builds and format/diff checks; healthy device state and expected
driver/UMD binding; `ScanoutDiag` absent, `VpSA=1`, `ScSet=1`; visible desktop,
idle-to-active responsiveness, rapid cursor motion without trails, no unprompted DWM
crash; no new present-gate steady-state timeouts, control timeouts, or ring failures;
DComp cadence near the 63 fps baseline; same-boot QEMU evidence of the actual OPTIMAL DWM
primary. UMD-only tranches deploy via adapter restart; any KMD change needs the three-site
version bump and a guest reboot (request before use).

## Execution order and rationale

**Part I (defects)** are owner-decision items, not refactor steps — each changes
observable behavior or fixes unsoundness. **Part II** runs in tranche order:

1. **Legacy-path removal** — deleting dormant machinery first shrinks every later diff.
2. **Hot-path telemetry containment** — small, measurable, independent of structure.
3. **File splits (pure moves)** — structure before content; `--color-moved` reviewable.
4. **Dedup/consolidation** — small diffs inside the new structure.
5. **Static guarantees: constants and newtypes** — mechanical foundations.
6. **Static guarantees: typestate/RAII/sealed interfaces** — the structural core, built
   on tranches 3–5.
7. **Concurrency and wait-structure** — last, closest to the frozen contracts.

Entries are numbered D1–D20 and R1–R77 in execution order. Per-entry `Dependencies` refine
the tranche order; anything unlisted depends only on its tranche predecessors.


---

## Part I — Defects and error-contract corrections (owner decision required)

These are verified or convergently-reported behavior bugs, soundness holes, and fake-success error paths. They are **not** refactor steps: each changes observable behavior (or fixes undefined behavior) and needs an owner call on whether/when to land, per the loud-failure doctrine. None may be silently folded into a refactor commit. Convention below: a defect fix that returns an error where success was returned before must be tested against the DDI's documented legal return set and the runtime's reaction (device-removal risk) before landing.

### D1. DxgkDdiSetVidPnSourceAddress performs registry writes before (and inside) its own raised-IRQL guard, violating the 'no diag::record above PASSIVE' invariant

- **Category:** defect · **Reported by:** `xc-errors/vidpn-irql-registry-writes`
- **Merged duplicate reports (4):** `kmd-display/svsa-raised-irql-registry-writes` — SetVidPnSourceAddress performs registry-write telemetry before and inside its raised-IRQL gate; `xc-concurrency/vidpn-addr-dispatch-registry` — SetVidPnSourceAddress performs registry-writing diagnostics before its IRQL gate — diag I/O reachable at DISPATCH; `xc-legacy/scirq-registry-write-above-passive` — SetVidPnSourceAddress performs PASSIVE-only registry writes on its raised-IRQL (MMIO flip) path; `xc-unsafe/setvidpn-raised-irql-registry-write` — SetVidPnSourceAddress writes the registry (PASSIVE-only) on its own raised-IRQL detection path
- **Files:** `kmd_render/src/ddi/display.rs`, `kmd_render/src/diag.rs`
- **Symbols:** `dxgkddi_set_vidpn_source_address`, `diag::record_named_bytes`, `diag::record`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 5 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** The DDI writes `VpSA` (line 647, trace_tick), `ScSrc/ScWH/ScDir` (683-685, trace_tick) and — exactly on the raised-IRQL branch — `ScIrq` (690) via RtlWriteRegistryValue BEFORE/AT the `KeGetCurrentIrql() != 0` check at 688. record_named* is explicitly NOT DiagLevel-gated (diag.rs:32-34 'failure counters must stay loud'). The code itself anticipates raised-IRQL arrivals (it counts them as ScIrq and skips the bind), so on any DISPATCH-level flip call the driver performs PASSIVE-only registry I/O at raised IRQL — the exact invariant CLAUDE.md marks BSOD/silent-deadlock. `diag::record` at 633/635 is only saved by the DiagLevel=0 default.

**Evidence.** display.rs:647 `crate::diag::record_named_bytes(b"VpSA", source_address_n);` ... :683-685 `ScSrc/ScWH/ScDir` ... :688-691 `let irql = unsafe { KeGetCurrentIrql() }; if irql != 0 { crate::diag::record_named_bytes(b"ScIrq", irql as u32); return STATUS_SUCCESS; }`. diag.rs:12-13 'RtlWriteRegistryValue requires PASSIVE_LEVEL'; diag.rs:32-34 'record_named* are NOT gated here'. CLAUDE.md invariant row 1.

**Recommendation.** Move the IRQL gate to the very top of the DDI; below it, latch raised-IRQL arrivals in DISPATCH-safe atomics flushed from a PASSIVE point (the existing cpu_host_aperture ChIq pattern, dumped in commit_vidpn via diag_dump_cpu_host_atomics). All rec_named/record calls move after the gate. Telemetry-only change; bind behavior identical.

**Risk.** Low: telemetry relocation only. Must keep the ScIrq observability (as an atomic + PASSIVE flush) so a raised-IRQL skip remains loud.

**Atomic commit boundary.** Single commit touching only dxgkddi_set_vidpn_source_address plus one atomic + one line in an existing PASSIVE dump.

**Validation.** KMD build + reboot; VpSA=1/ScSet=1, visible desktop, cursor, 63 fps cadence; ScIrq-equivalent atomic visible via the PASSIVE dump; no new gate timeouts.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** 'record_named* only at PASSIVE' is enforced per call site by comments; nothing stops a DISPATCH-capable DDI from calling it (this one does).
1. **Compile-time representation:** Give diag::record/record_named* a `PassiveToken` parameter (zero-sized proof constructed only in known-PASSIVE entry points), or split diag into passive-only and dispatch-safe (atomic) halves in separate modules so DISPATCH code cannot name the registry writers.
1. **Smallest atomic migration:** First fix this DDI (behavior fix); token refactor of diag callers can follow file-by-file.
1. **Remaining `unsafe` preconditions:** Token issuance at DDI entry still relies on knowing each DDI's documented IRQL; the compiler cannot verify dxgkrnl's actual calling IRQL.
1. **Regression test proving preserved behavior:** Same-boot desktop + flip stress (window drag); verify ScIrq atomic increments only when raised-IRQL flips occur and no registry writes happen on that path.

**Lead-reviewer note.** Reported independently by FIVE reviewers — the strongest convergence in the whole review. This is a live violation of the 'no diag::record above PASSIVE' key invariant on the SetVidPnSourceAddress path (the exact-primary flip DDI). Fix belongs conceptually with tranche 2 (telemetry containment) but should land first among the telemetry changes.


### D2. Input-layout/VS caches keyed by freed raw addresses are never purged on destroy (ABA => wrong layout/shader bound)

- **Category:** defect · **Reported by:** `umd-forward-c/ia-cache-aba-stale-pointer-keys`
- **Merged duplicate reports (1):** `umd-forward-b/destroy-shader-stale-ia-caches` — destroy_shader leaves IaState maps keyed by the freed COM pointer: unbounded growth plus ABA stale input-layout on address reuse
- **Files:** `umd/src/forward.rs`, `umd/src/device_funcs.rs`
- **Symbols:** `bind_input_layout`, `resolve_vs_input_variant`, `create_vs_input_variant`, `destroy_element_layout`, `destroy_shader`, `IaState::layout_cache`, `IaState::vs_variants`, `IaState::vs_bytecode`
- **Verification:** **MODIFIED** (severity medium) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** IaState caches are keyed by raw heap addresses: layout_cache by (LayoutData Box ptr, VS COM ptr), vs_variants by (VS COM ptr, class key), vs_bytecode/vs_sig_words by VS COM ptr. destroy_element_layout (forward.rs:6989-6998) drops the LayoutData Box and destroy_shader (forward.rs:3723-3725) releases the COM ref, but NEITHER purges the cache entries keyed by those now-freed addresses. bind_input_layout hits layout_cache at 7031 and resolve_vs_input_variant hits vs_variants at 7207 by address equality only. Cached ID3D11InputLayout / variant-VS COM pointers in the map values are also never released until device teardown (leak per churned layout/shader).

**Evidence.** forward.rs:6995 'drop(Box::from_raw(p as *mut LayoutData));' with no cache purge; 3723-3725 'destroy_shader ... release_com(h_shader.pDrvPrivate);' only; 7031 'layout_cache.get(&(lp, vp))' and 7120 'layout_cache.insert((lp, vp), raw)'; 7207 'vs_variants.get(&(vp, key))'; 7045/7180 '&*(lp as *const LayoutData)'. device_funcs.rs:166-168 'Cache of created input layouts keyed by (layout_ptr, vs_ptr)'. Failure: destroy layout+VS, allocator reuses both addresses for a NEW pair -> 7031 hit returns the IL built for the DEAD layout -> silently wrong vertex fetch.

**Recommendation.** Bug fix, kept separate from refactors: purge on destroy — destroy_element_layout removes all (lp, *) layout_cache keys (releasing the owned IL COM raw values) and clears ia.current_layout if it equals lp; destroy_shader removes vs_bytecode/vs_sig_words entries for the COM ptr, all (*, vp) layout_cache keys, and all (vp, *) vs_variants entries (releasing variant COM refs). Follow-up static form: generation-tagged newtype keys (LayoutId/ShaderId minted at create, stored beside the raw ptr) so an address reuse can never alias a live key.

**Risk.** Purge code must not release COM refs still bound in DXVK's context (DXVK holds its own refs via IASetInputLayout/VSSetShader, so releasing our cache ref is safe); getting the key direction wrong could evict live entries (cache-miss recreation is correct-but-slower, so failure mode is benign).

**Atomic commit boundary.** One commit: purge-on-destroy in destroy_element_layout + destroy_shader (with COM releases). Generation-tagged key newtypes as a separate later commit.

**Validation.** Release UMD build; run a layout/shader churn workload (dxvk-tests or a D3D11 sample that recreates input layouts); verify no HANDLE_MISS/CreateInputLayout failures, visible desktop, DComp ~63 fps, no new gate timeouts; add a targeted probe that destroys+recreates a layout and asserts the draw output changes with the new layout.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Cache keys are raw addresses assumed to identify live objects; permitted invalid sequence: destroy A, create B at same address, lookup hits A's cached derivative for B.
1. **Compile-time representation:** Newtype LayoutId(u64)/ShaderId(u64) from a monotonic per-device counter minted in create_*, stored in the handle private alongside the pointer; caches keyed by Id so address reuse cannot alias; owned cache values wrapped in a struct whose Drop releases the COM ref.
1. **Smallest atomic migration:** IaState key types + the four create/destroy/lookup sites; no DDI ABI change.
1. **Remaining `unsafe` preconditions:** Deref of current_layout/current_vs pointers during draw still rests on the D3D11 runtime's destroy-while-bound prohibition; cannot be encoded across the C DDI boundary.
1. **Regression test proving preserved behavior:** Layout churn probe (create/bind/draw/destroy loop) produces identical pixels before/after; existing selftest_triangle stays PASS.

**Verifier corrections (authoritative).** 1) Leak scope: cached IL/variant COM refs are never released even at device teardown — ddi_destroy_device (device_funcs.rs:394-413) drop_in_place's HeliosDevice and dropping HashMap<_,usize> values does not Release(); only process exit reclaims (finding said "until device teardown"). 2) ABA precondition overstated in evidence: a single address reuse suffices — stale lp with a still-live vp (layout_cache), or stale vp with a still-live lp / same class key (layout_cache, vs_variants); "reuses both addresses" is not required. 3) vs_bytecode/vs_sig_words are leak-only, not ABA-vulnerable: create_vertex_shader overwrites entries at the reused key (forward.rs:3101, 3402-3403) and they are only consulted via a live current_vs; the ABA maps are exactly layout_cache and vs_variants. 4) Recommendation incomplete: the purge in destroy_shader must also reset ia.bound_vs_com when it equals vp or any released variant pointer — the `bound_vs_com == desired` short-circuit at forward.rs:7223 can spuriously match a reused address and skip VSSetShader, leaving the wrong shader bound. 5) Implementation notes: destroy_shader must capture the COM raw before release_com nulls the slot to use it as the purge key, and needs helios_device(h) (h is currently ignored; it is available at all call sites including 6523/6524 and 6816/6817, where the com-keyed purge is a harmless no-op for non-VS stages).


### D3. query_get_data discards GetData's not-ready status, so the runtime sees every query as complete with unwritten data

- **Category:** defect · **Reported by:** `umd-forward-b/query-getdata-swallows-still-drawing`
- **Files:** `umd/src/forward.rs`
- **Symbols:** `query_get_data`, `set_runtime_error`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** query_get_data (5845-5861) calls `let _ = context.GetData(&async_, Some(data), data_size, flags);`. The d3d10umddi contract for pfnQueryGetData is: a void return means the data is valid; a query still in flight must be reported through pfnSetErrorCb with DXGI_DDI_ERR_WASSTILLDRAWING so the runtime returns S_FALSE to the app. DXVK's GetData returns S_FALSE (Ok in windows-rs) without writing *data when not ready, and returns Err on real failure; both are discarded. Apps polling occlusion/timestamp/event queries therefore receive immediate 'success' with stale/uninitialized output — wrong frame pacing, broken occlusion culling, garbage timestamps — with no counter and no log. Also the load_com/context-miss early returns (5852-5857) report implicit success the same way.

**Evidence.** umd/src/forward.rs:5858-5860 "if let Ok(async_) = (*q).cast::<ID3D11Asynchronous>() { let _ = context.GetData(&async_, Some(data), data_size, flags); }" — result discarded, no set_runtime_error anywhere in the function; contrast the documented purpose of set_runtime_error at :400-419 and device_funcs.rs:99-101.

**Recommendation.** Defect fix, own commit: propagate not-ready via set_runtime_error(h, DXGI_DDI_ERR_WASSTILLDRAWING) when GetData returns S_FALSE (windows-rs: inspect the raw HRESULT, since S_FALSE maps to Ok), and a legal error HRESULT on Err or missing handle; add a named counter for each path per the loud-failure rule. Note windows-rs GetData collapses S_FALSE into Ok(()) — use the raw HRESULT variant or check hr.0 to distinguish; a wrapper that merely forwards Ok would relocate the bug, not fix it.

**Risk.** Medium: apps that accidentally depended on instant query completion will now spin until real completion — that is correct D3D11 behavior, but pacing-sensitive workloads (3DMark) should be re-measured.

**Atomic commit boundary.** One commit in query_get_data plus the counter.

**Validation.** dxvk-tests query tests pass; a D3D11_QUERY_OCCLUSION probe returns S_FALSE while in flight then real counts; DOOM/3DMark fps unchanged or improved; new WASSTILLDRAWING counter moves under load and stops when idle.


### D4. read_config_dword uses RTL_QUERY_REGISTRY_DIRECT without TYPECHECK — a mistyped (REG_SZ) knob value causes kernel stack corruption / arbitrary write

- **Category:** defect · **Reported by:** `kmd-core/read-config-dword-typecheck`
- **Files:** `kmd_render/src/diag.rs`
- **Symbols:** `read_config_dword`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** diag.rs:142-160 builds an RTL_QUERY_REGISTRY_TABLE with Flags = RTL_QUERY_REGISTRY_DIRECT only and EntryContext pointing at a 4-byte stack `u32`. Per the RtlQueryRegistryValues contract, DIRECT with a string-typed value treats EntryContext as a UNICODE_STRING descriptor (16 bytes on x64): it reads Length/MaximumLength/Buffer from adjacent stack garbage and writes the string through that pointer. The hazard is acknowledged only by comment (diag.rs:135-137: "The value MUST be REG_DWORD (RTL_QUERY_REGISTRY_DIRECT without TYPECHECK interprets string data as a UNICODE_STRING buffer...)").

**Evidence.** diag.rs:118 "const RTL_QUERY_REGISTRY_DIRECT: u32 = 0x20;" (no TYPECHECK const exists); :143-145 "table[0].Flags = RTL_QUERY_REGISTRY_DIRECT; table[0].Name = ...; table[0].EntryContext = (&mut value as *mut u32).cast();"; :146-147 "DefaultType/DefaultData stay zero"; :135-137 comment documenting the UNICODE_STRING hazard instead of closing it. Reachable from every knob read at AddAdapter/StartDevice (start_device.rs:52,126-128; query_adapter_info.rs:451,470,596-597).

**Recommendation.** Add RTL_QUERY_REGISTRY_TYPECHECK (0x100) to Flags and set DefaultType = REG_DWORD << RTL_QUERY_REGISTRY_TYPECHECK_SHIFT (24) so a wrongly-typed value is rejected and `value` stays at `default`. Behavior-identical for every correctly-typed knob; converts a memory-corruption path into the documented fall-back-to-default path. Report separately from the config-module refactor.

**Risk.** None to correct configs. Requires the target OS to honor TYPECHECK (present since Win8; the 24H2 guest qualifies).

**Atomic commit boundary.** One-line-ish commit to read_config_dword (flags + DefaultType).

**Validation.** Targeted: `reg add ...\helios_kmd_render /v GdiAccelMode /t REG_SZ /d 1` then `pnputil /restart-device` — adapter must start normally with the default (GdiM=1 diag record), no bugcheck; delete the value afterwards. Plus normal boot regression gate.


### D5. DEFECT: VirtioGpu::drop writes device-status 0 but never waits for the reset to read back 0 before freeing all in-flight DMA buffers

- **Category:** defect · **Reported by:** `kmd-transport-gpu/drop-missing-reset-completion-wait`
- **Files:** `kmd_render/src/virtio/gpu.rs`
- **Symbols:** `VirtioGpu::drop`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Drop's last statement is `self.transport.set_status(DeviceStatus::empty())` (gpu.rs:2419), after which the struct's fields — including `inflight`/`parked` Vec<InFlight> owning device-visible DmaBuffers, and the VirtQueue ring memory — are freed. The virtio 1.2 spec requires the driver to poll device_status until it reads 0 before assuming reset is complete; `init` does exactly this bounded poll after its own reset (gpu.rs:850-853) but Drop does not. If the device (QEMU's virtio-gpu with the asynchronous virglrenderer/venus worker) has not finished quiescing when the write returns, it can still DMA into ring/command pages that MmFreeContiguousMemory has already released and the kernel has reused.

**Evidence.** gpu.rs:2408-2419 "// Quiesce the device (resets queues) so it stops touching the rings and the in-flight/parked entry buffers we are about to free." followed only by `self.transport.set_status(DeviceStatus::empty());` — no read-back. Contrast gpu.rs:849-853: `transport.set_status(DeviceStatus::empty()); // reset` then `while !transport.get_status().is_empty() && spins < 100_000`. hal.rs:101-106 DmaBuffer::drop frees with MmFreeContiguousMemory immediately as fields drop.

**Recommendation.** Mirror init's bounded status poll in Drop (PASSIVE context is guaranteed — set_virtio documents drop happens at PASSIVE outside the lock): after set_status(empty()), spin/sleep-bounded until get_status().is_empty() or a bounded budget expires; on budget expiry, log + count (new named counter, e.g. RESET_ACK_TIMEOUTS) and proceed — the leak-vs-corrupt tradeoff on a wedged device should at minimum be loud.

**Risk.** The fix is additive on a cold path (StopDevice/StartDevice-failure/Unload). Not fixing risks a rare use-after-free DMA scribble on device restart (`pnputil /restart-device`) or driver upgrade — memory-corruption class, hard to attribute afterwards.

**Atomic commit boundary.** One small commit in VirtioGpu::drop only.

**Validation.** Adapter restart cycles (pnputil /restart-device xN) + full reboot with no pool corruption under driver verifier; counter stays 0 on a healthy host; StartDevice-after-StopDevice still succeeds (BAR cache reuse path unchanged).


### D6. DEFECT (latent): cap-structure reads silently truncate offsets >0xFF to u8 — a virtio capability placed at cfg offset >=0xEC would read its 64-bit fields from the wrong registers

- **Category:** defect · **Reported by:** `kmd-transport-gpu/cfg-offset-u8-truncation`
- **Files:** `kmd_render/src/virtio/gpu.rs`, `kmd_render/src/virtio/config.rs`
- **Symbols:** `cfg_read32`, `scan_host_visible_window`, `map_isr_status_register`, `DxgkConfigAccess::read_word`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** `cfg_read32` routes through the virtio-drivers `ConfigurationAccess::read_word` whose `register_offset` is u8 (config.rs:36), doing `off as u8` (gpu.rs:247). The comment claims "PCI config space is 256 bytes, so the `as u8` truncation is lossless" (gpu.rs:238-239) — but the cap walk reads up to `cap + 20` (gpu.rs:298-301) and `cap` can legally be up to 0xFC, so `cap+20 = 0x110` truncates to 0x10 and reads BAR0 instead of the capability's length-high dword. The host-visible window base/len would then be silently garbage (wrong GPA handed to VidMm/blob mapping) rather than failing. Unreachable with QEMU's current low-offset cap layout, but it is a device-layout-dependent silent-wrong-read in the path that establishes the CPU-visible BAR segment geometry.

**Evidence.** gpu.rs:237-239 "`off` is held in a `u16` ... so the `cap + 20` cap-structure reads never overflow the `u8` arithmetic; PCI config space is 256 bytes, so the `as u8` truncation is lossless." — contradicted by gpu.rs:247 `off as u8` with gpu.rs:300-301 `cfg_read32(access, cap + 16)` / `cfg_read32(access, cap + 20)` where cap <= 0xFC (gpu.rs:289 `& 0xFC`). config.rs:36 `fn read_word(&self, _device_function: DeviceFunction, register_offset: u8)` vs config.rs:43-50 the underlying callback takes `register_offset as ULONG`.

**Recommendation.** Add a KMD-local wide accessor that bypasses the u8 trait bottleneck — DxgkCbReadDeviceSpace takes a ULONG offset (config.rs:47), so a `cfg_read32_wide(&DxgkConfigAccess, off: u16)` on the concrete type is trivial — and use it for all cap-structure reads; additionally bound the walk (`cap as usize + 20 < 256` else skip/log). Behavior identical on every current layout.

**Risk.** Not fixing: a future QEMU/machine-type change relocating vendor caps high in config space would produce a corrupt HostVisibleWindow base — blob maps to a bogus GPA, VidMm segment reported over garbage — with no failure signal (violates the validate-every-offset invariant and loud-failure rule).

**Atomic commit boundary.** One commit: wide accessor + the two cap walks converted + bound check.

**Validation.** Boot with unchanged DpInf/0x0B00_0005 diag records; host-visible window base/len identical to current boot (visible via QUERY_STATS window_len); desktop + MAP_BLOB workloads unchanged.


### D7. Four diag breadcrumb codes (0x0120-0x0123) are each assigned to two unrelated failure sites, making bring-up triage evidence ambiguous

- **Category:** defect · **Reported by:** `kmd-venus/diag-code-collisions`
- **Merged duplicate reports (1):** `xc-duplication/venus-diag-code-collision` — Defect: venus.rs diag breadcrumb codes 0x0120-0x0123 are each used by two different functions, making S-ring traces ambiguous
- **Files:** `kmd_render/src/virtio/venus.rs`
- **Symbols:** `diag`, `create_linear_scanout_image`, `create_fence`, `wait_for_fence`, `allocate_export_image_memory`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** diag(0x0120) is emitted by both create_linear_scanout_image cmd-mismatch (line 941) and create_fence null-handle (line 1584); 0x0121 by create_linear_scanout_image result!=0 (949) and create_fence echoed-id mismatch (1589); 0x0122 by create_linear_scanout_image null ids (954) and wait_for_fences cmd-mismatch (1613); 0x0123 by allocate_export_image_memory cmd-mismatch (1141) and wait_for_fences result!=0 (1618). The 0x0D00_00xx breadcrumb ring is this driver's primary black-screen triage tool (evidence discipline: counters are read across boots); a collided code sends the investigation to the wrong function.

**Evidence.** venus.rs:941 'diag(0x0120);' (create_linear_scanout_image) vs :1584 'diag(0x0120);' (create_fence); :949 vs :1589 'diag(0x0121);'; :954 vs :1613 'diag(0x0122);'; :1141 'diag(0x0123);' (allocate_export_image_memory) vs :1618 'diag(0x0123);' (wait_for_fence). Sorted grep of 'diag(0x01' confirms these are the only duplicates.

**Recommendation.** Renumber the four later-added colliding sites (create_fence/wait_for_fence sites predate or postdate the scanout ones — pick the scanout-linear set to keep since session notes reference SdgL* stages, and move fence codes to a fresh unused range, e.g. 0x0140+). Structurally, the ring-call-dedup finding removes hand-assigned per-site codes by deriving the code from the command id + failure kind in one place; land the renumber first as the immediate defect fix.

**Risk.** Historical S-ring dumps referencing old fence-path codes become ambiguous retroactively — acceptable; document the renumber in ROADMAP.md tooling notes.

**Atomic commit boundary.** One-line-per-site commit renumbering the four colliding codes.

**Validation.** grep proves every diag literal in venus.rs unique; boot + forced-failure not required (codes are write-only); standard regression gate unaffected.


### D8. enum_cofunc_modality leaks the created source/target mode set when pfnAssignSourceModeSet/pfnAssignTargetModeSet fails

- **Category:** defect · **Reported by:** `kmd-display/vidpn-assign-failure-modeset-leak`
- **Files:** `kmd_render/src/ddi/vidpn.rs`
- **Symbols:** `enum_cofunc_modality`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** On the unpinned-source branch the code releases the acquired set, creates a new one, adds the single mode, then assigns. Failure handling releases the set after acquire/create/add failures (fp=13/14/15), but on assign failure it only breaks: vidpn.rs:539-543 'status = unsafe { assign_src(h_vidpn, source_id, h_set) }; if !ok(status) { fp = 16; break; }' — h_set is never released. The target branch mirrors the leak at vidpn.rs:606-610 (fp=26). Per the WDDM contract (and the Microsoft KMDOD sample), ownership transfers only on successful assign; on failure the driver must call pfnReleaseSourceModeSet/pfnReleaseTargetModeSet. The reference leaks inside the OS VidPn object until it is destroyed.

**Evidence.** vidpn.rs:539-543 'status = unsafe { assign_src(h_vidpn, source_id, h_set) }; if !ok(status) { fp = 16; break; }' — no release before break, unlike vidpn.rs:534-537 (fp=15) which does 'let _ = unsafe { release_src(h_vidpn, h_set) };'. Same asymmetry at vidpn.rs:606-610 (fp=26) vs 600-604 (fp=25).

**Recommendation.** Report as a behavior bug, kept separate from refactors: add '_ = release_src(h_vidpn, h_set);' / release_tgt before the fp=16/fp=26 breaks (matching the fp=15/fp=25 shape). The RAII-guard refactor (vidpn-raii-modeset-guards) then makes this class unrepresentable.

**Risk.** Low severity: assign failures are rare and the leak is bounded by VidPn lifetime, but checked dxgkrnl/verifier can flag the unreleased reference and repeated mode-set retries could accumulate during a hostile negotiation loop.

**Atomic commit boundary.** One two-line commit adding the missing releases at the two break sites.

**Validation.** KMD build; reboot; mode negotiation unchanged: VpECr=0, VpCN=1, VpCP=1, no VpECf=16/26 breadcrumbs, desktop visible.

**Lead-reviewer note.** R52 (VidPn RAII guards) makes this class unrepresentable; the minimal leak fix can land first as a one-commit defect fix, then R52 subsumes it.


### D9. bar_transfer validates the blob arm but never the MDL arm; bar_fill silently truncates while VIRTUAL_FILL refuses loudly

- **Category:** defect · **Reported by:** `kmd-alloc/paging-mdl-arm-validation`
- **Files:** `kmd_render/src/ddi/build_paging_buffer.rs`
- **Symbols:** `bar_transfer`, `bar_fill`, `mdl_system_va`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** bar_transfer bounds-checks blob_off+bytes against the blob length (250-252, 277-279) but copies `src.add(mdl_off)` / `dst.add(mdl_off)` for TransferSize bytes with NO check of the MDL's described byte count (MmGetMdlByteCount is never consulted; mdl_system_va only maps). mdl_off itself is a heuristic: `((MdlOffset as u64) << 12) + (TransferOffset & 0xFFF)` with the comment 'Validated post-boot via PgTs/PgTd' — a contract verified by side-effect counters, not code. Separately, classic FILL truncates silently: `let n = fill_len.min(len)` (325) counts BAR_FILLS success, while VIRTUAL_FILL refuses out-of-bounds loudly via BAR_ERR_BOUNDS (488-491) — asymmetric per-arm validation, the RenderGdi ~48%-drop class.

**Evidence.** build_paging_buffer.rs:233-236 `// MdlOffset is in pages ... Validated post-boot via PgTs/PgTd` / `let mdl_off = ((t.MdlOffset as u64) << 12) + (t.TransferOffset as u64 & 0xFFF);`; :256-261 `copy_nonoverlapping(src.add(mdl_off as usize), dst.add(blob_off as usize), bytes as usize)` — only `blob_off.saturating_add(bytes) > len` checked (:249-252), no MDL length check anywhere in file; :325 `let n = fill_len.min(len); fill_pattern(dst, n as usize, pattern);` vs :488-491 `if off.saturating_add(fill_len) > len { BAR_ERR_BOUNDS.fetch_add(1,...); return; }`.

**Recommendation.** Before each copy, validate `mdl_off + bytes <= MmGetMdlByteCount(mdl)` (and bytes != 0); on violation increment BAR_ERR_BOUNDS (or a new BAR_ERR_MDLBOUNDS) and skip the copy — identical to the existing blob-side refusal. Make classic FILL match VIRTUAL_FILL: refuse (counted) instead of .min() truncation. Keep the mdl_off derivation but assert its assumption (TransferOffset low bits == MDL sub-page phase expectation) behind the same counter. Behavior identical for all well-formed VidMm input.

**Risk.** Low: VidMm is a trusted kernel producer and transfers are whole-allocation in practice, so the new refusal paths should never fire; if one does, the op degrades to the pre-existing null-engine outcome (loud counter) rather than an OOB kernel copy.

**Atomic commit boundary.** One commit in build_paging_buffer.rs: add MDL-arm bounds checks + loud FILL refusal.

**Validation.** KMD build + reboot; eviction/re-commit exercise (open+close heavy app, lock/unlock cycles); PgTi/PgTo/PgFn advance as before; new bounds counters stay 0 across a full session; standard visible-desktop gate.


### D10. OpenAllocation transport-error path skips the unwind of already-handed-out opens; CreateAllocation failure leaks the ResourceContext box

- **Category:** defect · **Reported by:** `kmd-alloc/open-create-partial-failure-leaks`
- **Merged duplicate reports (1):** `xc-errors/open-allocation-err-arm-leak` — DxgkDdiOpenAllocation's transport-error arm returns failure without unwinding OpenAllocationContext boxes already handed out in the same call
- **Files:** `kmd_render/src/ddi/create_allocation.rs`
- **Symbols:** `dxgkddi_open_allocation`, `dxgkddi_create_allocation`, `ResourceContext`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** In dxgkddi_open_allocation the liveness gate has two failure arms: Ok(false) unwinds opens 0..i (frees each Box, nulls hDeviceSpecificAllocation, 1239-1249) before returning; but Err(_de) (transport error) returns STATUS_DEVICE_NOT_READY at 1252-1255 with NO unwind — the OpenAllocationContext boxes for entries 0..i leak and their handles stay populated in a failed call dxgkrnl will not CloseAllocation for. In dxgkddi_create_allocation, when Flags.Resource is set a ResourceContext is boxed into args.hResource (1107-1110); if create_one later fails, the unwind loop (1127-1134) frees prior allocations but never frees/nulls args.hResource before returning (1135) — leaked, since dxgkrnl does not call DestroyAllocation for a failed create.

**Evidence.** create_allocation.rs:1238-1250 Ok(false) arm: `for j in 0..i { ... drop(Box::from_raw(prev.hDeviceSpecificAllocation ...)); prev.hDeviceSpecificAllocation = null_mut(); } return STATUS_INVALID_PARAMETER;` vs :1252-1255 `Err(_de) => { crate::diag::record(0x0C02_00E5); return STATUS_DEVICE_NOT_READY; }` (no loop). :1107-1110 `let resource = Box::new(ResourceContext {...}); args.hResource = Box::into_raw(resource) as HANDLE;`; failure return :1135 `return status;` — unwind loop 1127-1134 touches only pAllocationInfo entries, never hResource.

**Recommendation.** Hoist the existing Ok(false) unwind loop into a helper and run it on the Err(_de) arm too; in dxgkddi_create_allocation's failure return, reclaim the ResourceContext (Box::from_raw) and reset args.hResource to its original value. Verify against the DDI contract text that dxgkrnl performs no destroy callback after a failed create/open (reference drivers free per-resource context on this path).

**Risk.** Minimal: both paths fire only on rare failures (virtio transport down mid-open; per-allocation create failure). Freeing our own just-created boxes on the documented driver-cleanup path cannot affect the success path.

**Atomic commit boundary.** One commit: shared unwind helper + hResource reclaim on the create failure return.

**Validation.** KMD builds; normal boot unchanged (paths never taken); optional fault-injection via a debug knob forcing create_one failure on Nth allocation, then verify no pool leak growth (poolmon/verifier tag) across repeated failures and that dxgkrnl proceeds without 0x13B/handle complaints.


### D11. dxgi_rotate_resource_identities returns S_OK after refusing or failing the rotation, desynchronizing runtime and driver identity maps

- **Category:** error-path · **Reported by:** `umd-forward-c/rotate-fake-success`
- **Files:** `umd/src/forward.rs`
- **Symbols:** `dxgi_rotate_resource_identities`
- **Verification:** **CONFIRMED** (severity medium) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** All three failure arms return 0 (success): null resource handle (8697-8700), untracked resource (8702-8705), and DXVK backing-rotation failure (8715-8718 'backing rotation FAILED'). The runtime proceeds believing resource[i] took resource[i+1]'s identity while the driver kept the old mapping — subsequent flip presents report the wrong hSrcAllocation and the wrong present_private resource_id, i.e. exactly the two-of-three-black-buffers class this function's own doc block (8666-8677) says the old stub caused. There is also no named counter, only log lines, violating the 'every skip/refusal gets a counter' rule.

**Evidence.** forward.rs:8715-8718 'if !rotated { log_line("DXGI RotateResourceIdentities: backing rotation FAILED"); return 0; }'; 8702-8705 'untracked resource ... return 0'; contrast doc at 8674-8677: 'The old Flush-only stub pinned dwm's composition to ONE allocation ... (black IDD output).' — the same skew this fake-success recreates on failure.

**Recommendation.** Add a named atomic counter per refusal arm (surfaced in the periodic present log line) — behavior-preserving, land now. Separately (owner-gated, since it changes observable behavior): verify the legal failure return set for pfnRotateResourceIdentities against the WDK contract and return the documented error instead of 0 so dxgkrnl/DXGI can recover, per loud-failure-over-fake-success; keep as its own commit with an A/B note.

**Risk.** Returning an error the runtime treats as device-removal would be worse than today's skew; hence counter-first, return-change only after contract verification. Counter addition is zero-risk.

**Atomic commit boundary.** One commit for counters; a separate reviewed commit for the return-code change.

**Validation.** Counter stays 0 across a normal boot + flip-model workloads (it should never fire on the healthy path); if it moves, that is a live defect surfaced. Desktop visible, rotation log line ('rotated N resources') cadence unchanged.


### D12. Failed Create* DDIs never call pfnSetErrorCb: runtime sees S_OK with a null driver handle (fake success)

- **Category:** error-path · **Reported by:** `umd-forward-a/create-ddi-error-suppression`
- **Merged duplicate reports (1):** `umd-forward-b/void-create-ddis-silent-failure` — All void create DDIs in scope report success to the runtime after failure — pfnSetErrorCb exists for exactly this and is never called
- **Files:** `umd/src/forward.rs`
- **Symbols:** `create_resource`, `finish_wddm_tex2d`, `create_vertex_shader`, `create_pixel_shader`, `set_runtime_error`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Header admits it: lines 8-10 "Errors on VOID-returning Create* are dropped for now (TODO...) — a failed create leaves a null handle." set_runtime_error (400-419) exists and open_resource uses it correctly (2006, 2027, 2088), but: CreateBuffer failure only logs (1717); CreateTexture2D failure only logs (1859); the direct scan-out primary create failure logs "SCAN-OUT PRIMARY CREATE FAILED" and returns with a null handle and no runtime error (1798-1806) — DWM's CreateTexture2D succeeds against a dead primary; CreateTexture3D (1927), create_rtv (2217), create_dsv (2402), vertex/pixel shader failures (3103, 3138) likewise. Additionally finish_wddm_tex2d treats allocate_wddm_resource failure (hr!=0 → (0,0), 1476-1480) identically to "no allocation needed" and stores the resource with allocation=0 — for a pPrimaryDesc resource that is a partial failure reported as success (flips will later fail with no attribution).

**Evidence.** forward.rs:8-10 TODO comment; :1717 `Err(e) => log_line(&format!("DDI create_resource(buffer) failed: {e:?}"))` (no set_runtime_error); :1798-1806 "do NOT fall back ... SCAN-OUT PRIMARY CREATE FAILED ... -> no primary" then plain return; :1476-1480 `if hr == 0 { (h_allocation, alloc.hKMResource) } else { (0, 0) }` conflated with the not-needed (0,0) at :1268-1270; :1608-1616 store_resource proceeds with allocation=0; contrast :2006 `set_runtime_error(h, E_FAIL)` in open_resource.

**Recommendation.** Route every Create*/open failure through set_runtime_error with a documented HRESULT (E_FAIL/E_OUTOFMEMORY), and make finish_wddm_tex2d distinguish alloc-failure from alloc-not-needed for primary/shared resources (fail the create). This changes only failure-path behavior — currently undefined (runtime dereferences a null pDrvPrivate) — success paths are untouched. Add a per-cause counter per Operating Rule 2.

**Risk.** Low-medium: some callers (DWM) may retry or fall back differently once creates fail loudly; that is the contractual behavior and matches the repo's loud-failure doctrine. Verify no existing workload depends on the silent-null behavior.

**Atomic commit boundary.** One commit: thread set_runtime_error through the create/alloc failure arms in forward.rs (no signature changes).

**Validation.** Release build; desktop + DWM stability unchanged (success paths untouched); inject a forced create failure in a test app and confirm the API call returns the error instead of crashing; regression gate items (VpSA/ScSet, cadence, no DWM crash).

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** "Every void Create* DDI must either store a valid payload or report via pfnSetErrorCb" is enforced by nothing; the permitted invalid sequence is: create fails → function returns → runtime treats the call as S_OK → later DDI dereferences the null/zero payload.
1. **Compile-time representation:** A #[must_use] CreateOutcome proof token produced at DDI entry, consumed by exactly one of store_*(token, payload) or fail(token, hr) (which calls pfnSetErrorCb); dropping it unconsumed is a compile-time must_use warning promoted to deny.
1. **Smallest atomic migration:** One commit introducing the token in the create/open family only (create_resource, open_resource, views, shaders); other DDIs untouched.
1. **Remaining `unsafe` preconditions:** The token cannot prove the stored COM pointer is live — that stays a bridge trust boundary; and Drop-based enforcement in extern "C" fns must avoid unwinding (use explicit consumption, not panicking Drop).
1. **Regression test proving preserved behavior:** Existing workloads (DWM boot, dxvk-tests) run with zero new SetErrorCb hits; forced-failure unit exercise returns the HRESULT to the API caller.

**Lead-reviewer note.** Two independent reports covering different line ranges of forward.rs — the pfnSetErrorCb omission is file-wide. Fix once, uniformly, after the R14 split (or before, as one sweep) — but as its own commit; changing create-failure semantics needs runtime-reaction testing per DDI.


### D13. with_virtio failure (transport torn down) is mapped to success in two wait paths: ctrl_roundtrip returns Ok with a never-written response, wait_fence reports Complete

- **Category:** error-path · **Reported by:** `kmd-transport-ctrl/transport-gone-reports-success`
- **Files:** `kmd_render/src/virtio/ctrl.rs`
- **Symbols:** `ctrl_roundtrip`, `wait_fence`, `SyncWaitBlock::copy_resp`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Both timeout-cancel paths use '.unwrap_or(true)' on with_virtio, so DriverError::DeviceNotFound (virtio slot is None — device stopped/torn down) is folded into 'already completed'. ctrl_roundtrip then executes block.copy_resp(resp_out) on a block whose done bit was never set — resp_out receives the initial zeros — and returns Ok(()). wait_fence's cancel path likewise returns WaitFenceOutcome::Complete, while its own prepare path maps the same with_virtio error to Invalid (ctrl.rs:1508) — inconsistent classification of the identical condition. Reachability today is negligible (dxgkrnl serializes teardown against escapes, and every ctrl_roundtrip caller re-validates resp_is_ok, converting the zeroed response to DeviceError), so this is an error-path refactor, not a live defect — but the function's Ok is not backed by any completion proof, which is exactly the fragility the sync-wait typestate would make unrepresentable.

**Evidence.** ctrl.rs:281-293: 'let already_done = adapter.with_virtio(|v| { v.drain_used(); v.abandon_sync(token, block_ptr) }).unwrap_or(true); if !already_done { ... return Err(VirtioError::Timeout); } block.copy_resp(resp_out); Ok(())' — the unwrap_or(true) arm reaches copy_resp with done never set. ctrl.rs:1539-1547: 'let completed = adapter.with_virtio(|v| { v.drain_used(); v.fence_wait_cancel(block_ptr) }).unwrap_or(true); if completed { WaitFenceOutcome::Complete }' vs ctrl.rs:1508 'Err(_) => return WaitFenceOutcome::Invalid, // transport gone'. adapter.rs:1021-1023: with_virtio errs only when the virtio slot is None. gpu.rs:561 'only valid once is_done'.

**Recommendation.** Map with_virtio Err explicitly: ctrl_roundtrip -> Err(VirtioError::DeviceError); wait_fence cancel -> WaitFenceOutcome::Invalid (matching its prepare arm). Alternatively (and preferably) let sync-wait-typestate land first: with copy_resp gated behind a Completed proof, these arms cannot compile in their current shape and must be written honestly.

**Risk.** Low: changes outcomes only in the transport-torn-down race, which no current caller can reach with different observable behavior (all re-validate the response; escape teardown is serialized). Confirm the ICD treats a late Invalid identically to its current TimedOut/Invalid handling (venus.rs:2142-2146 already lumps them).

**Dependencies.** prefer landing after sync-wait-typestate, which subsumes the ctrl_roundtrip half

**Atomic commit boundary.** One small commit changing the two unwrap_or(true) arms and their doc comments.

**Validation.** KMD build; regression gate; device-restart cycle (pnputil /restart-device) with an active present workload — no new escape failures, CTRL_TIMEOUT_COUNT stays 0, clean adapter restart.


### D14. init's device-reset poll has no failure branch: after 100k iterations it proceeds to feature negotiation against a device that never acknowledged reset

- **Category:** error-path · **Reported by:** `kmd-transport-gpu/init-reset-timeout-silent-fallthrough`
- **Files:** `kmd_render/src/virtio/gpu.rs`
- **Symbols:** `VirtioGpu::init`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** gpu.rs:850-853: `let mut spins = 0u32; while !transport.get_status().is_empty() && spins < 100_000 { spins += 1; }` — when the bound is hit the loop simply exits and init continues to ACKNOWLEDGE/DRIVER/feature negotiation with a device still mid-reset; negotiation results against such a device are undefined and any failure will surface later as an inscrutable FeatureRejected/DeviceError (or worse, a half-live transport). The bound is also an iteration count of MMIO reads, not a time budget (the sibling GET_DISPLAY_INFO poll at least documents its ~1 s calibration, gpu.rs:70-74, and DOES fail with DeviceError on expiry, gpu.rs:921-926). Timeout-doctrine classification: the bounded poll itself is a KEEP — it is the spec-mandated bring-up wait with no interrupt available yet (the ISR-status VA is not even mapped until later in init); the flaw is solely the missing failure branch and the uncalibrated unit.

**Evidence.** gpu.rs:849-855: `transport.set_status(DeviceStatus::empty()); // reset` / `while !transport.get_status().is_empty() && spins < 100_000 { spins += 1; }` / `transport.set_status(DeviceStatus::ACKNOWLEDGE);` — no error return between the loop and the next status write. Contrast the failing sibling poll gpu.rs:922-925: `if spins >= CTRL_POLL_SPINS { return Err(VirtioError::DeviceError); }`. mod.rs:29-30: errors exist so StartDevice "can fail loudly (and distinguishably)".

**Recommendation.** Give the reset poll the same shape as the GET_DISPLAY_INFO poll: on bound expiry return `Err(VirtioError::DeviceError)` (StartDevice then fails loudly with STATUS_IO_DEVICE_ERROR instead of limping), and express the bound as a time budget (KeQueryPerformanceCounter or KeDelayExecutionThread 1 ms slices at PASSIVE) with a named constant documenting the spec requirement. Behavior on every healthy boot is unchanged.

**Risk.** Minimal — only a broken/wedged device hits the new branch, and failing StartDevice is the documented contract (mod.rs:29-30 "fail loudly ... rather than leaving a half-initialized adapter").

**Atomic commit boundary.** One-line-scale commit inside init.

**Validation.** Normal boot unchanged (device state healthy, desktop visible); no new StartDevice failures across reboot + restart-device cycles.


### D15. Writer's fixed 512-byte buffer uses unchecked slice indexing — an oversized encode panics inside a DDI path; the sizing comment is stale

- **Category:** static-guarantee · **Reported by:** `kmd-venus/writer-capacity-panic`
- **Files:** `kmd_render/src/virtio/venus.rs`
- **Symbols:** `Writer`, `Writer::u32`, `Writer::u64`, `Writer::bytes_padded`, `MAX_CMD_BYTES`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Writer::u32/u64/bytes_padded index `self.buf[self.len..self.len + n]` with no capacity check (377-411); overflow is an out-of-bounds panic. Callers run inside DxgkDdiStartDevice, DxgkDdiCreateAllocation, and SetVidPnSourceAddress — per the Key Invariants table, 'a panic in any DDI = silent graphics deadlock'. The guard is a comment whose sizing claim is stale: 'The largest is vkCreateDevice (~120 bytes); 512 is comfortable headroom' (194-196) — the full-tier vkCreateDevice encode now carries five padded extension strings (148 bytes of names + 40 bytes of length words alone, 2737-2743, 2798-2804), totaling ≈330 bytes. Today no runtime input controls stream length, so this is a latent trap armed by the next extension-list or command addition, exactly the edit this refactor stage will perform.

**Evidence.** venus.rs:377-380 'fn u32(&mut self, v: u32) { let b = v.to_le_bytes(); self.buf[self.len..self.len + 4].copy_from_slice(&b); self.len += 4; }' — no bound check; :404-411 bytes_padded same pattern. Stale claim :194-196 '/// Maximum venus stream we build for any single direct/ring command. The largest is `vkCreateDevice` (~120 bytes); 512 is comfortable headroom.' vs :2798-2804 'w.u32(exts.len() as u32); w.u64(exts.len() as u64); for ext in exts { w.u64(ext.len() as u64); w.bytes_padded(ext); }' with EXT_FULL's five names (:2737-2743) padding to 148 bytes.

**Recommendation.** Make Writer overflow non-panicking and compile-visible: either (a) a latching writer — writes past capacity set an `overflow` flag, all writes become no-ops, and as_slice() becomes `finish() -> Result<&[u8], VirtioError>` (every call site already returns Result); or (b) const-generic capacity with per-command const upper-bound assertions for the fixed-shape streams. (a) is the smaller trusted boundary. Fix the stale comment with the computed full-tier size either way.

**Risk.** finish() plumbing touches every encoder — do it with (or after) ring-call-dedup so there is one send boundary instead of ~30.

**Dependencies.** R23 (ring-call-dedup)

**Atomic commit boundary.** One commit: Writer latch + finish() at the single ring_call/direct-submit boundary + comment fix.

**Validation.** Host-side codec test: encoding MAX_CMD_BYTES+1 bytes returns Err, no panic; golden-byte equality for all real commands; standard visual gate.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** 'Every encoded command fits in 512 bytes' is enforced by nothing; the invalid sequence is any future command/extension growth pushing len past 512 → slice panic inside a DDI → silent graphics deadlock.
1. **Compile-time representation:** Latching fallible writer whose only output is Result (panic-free by construction), or const-asserted per-command capacity bounds.
1. **Smallest atomic migration:** Writer type + the single send boundary post-dedup.
1. **Remaining `unsafe` preconditions:** None new; the compiler cannot bound dynamically-composed streams without const shapes — the latch converts that to a counted, legal error return.
1. **Regression test proving preserved behavior:** Codec overflow test (Err, no panic); golden-byte equality; boot bring-up diag sequence unchanged.

**Lead-reviewer note.** Direct violation of the 'no panic in any DDI-reachable path' key invariant.


### D16. Four expect/unwrap panic sites in KMD release paths encode loop-carried buffer ownership as Option

- **Category:** static-guarantee · **Reported by:** `xc-unsafe/kmd-panic-sites-loop-ownership`
- **Files:** `kmd_render/src/virtio/ctrl.rs`, `kmd_render/src/ddi/create_allocation.rs`
- **Symbols:** `ctrl_roundtrip`, `submit_venus_async`, `submit_primary_scanout_copy`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** ctrl.rs:258 and :1416/:1419 use meta_slot.take().expect("meta returned on every retry path") inside enqueue-retry loops; create_allocation.rs:372 uses prepared.take().unwrap(). All are logically unreachable today, but each is a KeBugCheck (wdk_panic) in a DDI path — the Key Invariant says a panic in any DDI is a silent graphics deadlock and release paths must never panic. The reachable-by-refactor risk is real: any future edit to the retry arms that forgets to restore the Option turns into a bugcheck instead of a compile error.

**Evidence.** ctrl.rs:258 "let m = meta_slot.take().expect(\"meta returned on every retry path\");" (same pattern at :1416 and :1419 for meta+venus buffers); create_allocation.rs:372 "let old = prepared.take().unwrap();". lib.rs:17-21: wdk_panic supplies the KeBugCheck panic handler — "We never want to panic in release".

**Recommendation.** Restructure so ownership is loop-carried by value: let mut m = meta; loop { match enqueue(m) { Ok(t) => break t, Err((m_back, QueueFull)) => { m = m_back; backoff(); } Err((_, e)) => return Err(e) } } — no Option, no expect. For create_allocation.rs:372, the preceding map-check already proves Some; replace with if-let/let-else so the proof is structural. Zero behavior change.

**Risk.** Trivial.

**Atomic commit boundary.** One commit covering all four sites.

**Validation.** KMD builds; grep confirms no expect(/unwrap() on DDI-reachable paths (allowlist unwrap_or*); boot + escape traffic normal (QfRet/CtOut counters behave).

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** 'The buffer Option is Some on every loop iteration' is asserted at runtime; the invalid state is an Err arm that forgets to restore it after a future edit.
1. **Compile-time representation:** Move-based loop-carried ownership (the enqueue API already returns the buffer in its error type — use it directly); let-else for the checked-Some case.
1. **Smallest atomic migration:** All four sites in one commit; no API changes.
1. **Remaining `unsafe` preconditions:** None.
1. **Regression test proving preserved behavior:** Existing enqueue-backpressure behavior under load: QUEUE_FULL_RETRIES advances, no CTRL_TIMEOUT regression, no bugcheck.

**Lead-reviewer note.** Same invariant class as D15; the four expect/unwrap sites should become counted error returns.


### D17. open_resource fabricates a 1x1 BGRA meta when the trailer is missing — silent wrong-geometry alias instead of loud failure

- **Category:** error-path · **Reported by:** `umd-forward-a/open-meta-1x1-fallback`
- **Files:** `umd/src/forward.rs`
- **Symbols:** `open_resource`, `read_alloc_meta`, `read_open_identity`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** open_resource fails loudly when the identity record is absent (2001-2008, per audit U-B2), but when the identity is present and only the meta trailer is missing it silently defaults to `meta.unwrap_or(HeliosWddmAllocMeta { width: 1, height: 1, format: 21, pitch: 4, ... })` (2009-2020) and builds a 1x1 A8R8G8B8 alias of the real surface — draws succeed, content is garbage. Both current writers (UMD RuntimeAllocPrivate 88 bytes; KMD create_allocation.rs:1410 private+meta) always append the 40-byte meta, so this arm is unreachable dead-but-dangerous code, in the same family as the parse-only legacy StandardAllocMetaV1/V2 arms (132-151, 310-343).

**Evidence.** forward.rs:2009-2020 `let meta = meta.unwrap_or(HeliosWddmAllocMeta { width: 1, height: 1, format: 21, pitch: 4, ... })`; contrast the loud arm :2001-2007 "no venus identity record ... -> E_FAIL" and its comment :1996-2000 "draws 'succeeded' and the shared content stayed black forever (audit U-B2). Fail loudly instead"; legacy arms :310-343; KMD always writes full meta: kmd_render/src/ddi/create_allocation.rs:1410 `(size_of::<HeliosWddmAllocPrivate>() + size_of::<HeliosWddmAllocMeta>()) as u32`.

**Recommendation.** Make the ddi-shared open arm require the meta: change read_open_identity to return an exhaustive enum (IdentityWithMeta / IdentityOnly) and have open_resource treat IdentityOnly as E_FAIL with a named counter, matching the identity-missing arm. Fold the V1/V2 legacy-trailer arms into the same review: with version-locked KMD+UMD (INF pairs them, KMD deploys require reboot) they are dead; remove after owner confirmation.

**Risk.** Low: the arm is unreachable with the paired 142 KMD; if some producer does hit it, current behavior is silently-wrong rendering, so failing loudly is the safer contract.

**Atomic commit boundary.** One commit in the open_resource/read_open_identity area (can precede or follow the split).

**Validation.** Release build; boot + open-heavy workload (DWM shared surfaces, dxvk-tests) with zero hits on the new counter; regression gate items unchanged.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** "A ddi-shared open must carry creator geometry" is enforced only by writers happening to append the trailer; the permitted invalid state is a live venus identity paired with fabricated 1x1 geometry that renders wrong forever.
1. **Compile-time representation:** read_open_identity returns enum OpenRecord { WithMeta(HeliosWddmOpenIdentity, HeliosWddmAllocMeta), IdentityOnly(HeliosWddmOpenIdentity) }; the texture-building arm pattern-matches WithMeta so the compiler forbids constructing an alias without geometry.
1. **Smallest atomic migration:** read_open_identity + its single caller open_resource, one commit.
1. **Remaining `unsafe` preconditions:** The unaligned reads from runtime-supplied private data stay unsafe; sizes are already checked per-arm (303, 355) and cannot be encoded further.
1. **Regression test proving preserved behavior:** Open-heavy same-boot run (DWM + a shared-surface app) with the IdentityOnly counter at 0 and unchanged desktop.


### D18. CreateDevice validates late (leaking DXVK device + kernel context on E_FAIL) and returns S_OK when the WDDM context creation fails

- **Category:** error-path · **Reported by:** `umd-core/create-device-partial-failure-paths`
- **Merged duplicate reports (1):** `xc-errors/umd-create-device-partial-init` — UMD create_device reports E_FAIL after partially constructing the device (HeliosDevice written, runtime context created) without rollback
- **Files:** `umd/src/lib.rs`, `umd/src/device_funcs.rs`
- **Symbols:** `create_device`, `create_runtime_context`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Two partial-failure shapes in create_device: (1) the HeliosDevice is constructed in the runtime's private memory and create_runtime_context creates a WDDM kernel context (lib.rs:708-747) BEFORE the p_device_funcs null check at lib.rs:751-754 — on that E_FAIL the runtime never calls pfnDestroyDevice for a failed create, so the in-place HeliosDevice is never dropped: the DXVK device (UniquePtr moved into the struct) and the kernel context both leak. (2) create_runtime_context only records state 'if hr == 0' (device_funcs.rs:440-448) and returns unit; create_device then unconditionally proceeds to report success (lib.rs:793-796) — a device with h_context null that cannot make WDDM submissions is handed to the runtime as S_OK, and each downstream path must rediscover the hole (e.g. forward.rs:7634 disable 'no runtime callbacks/context').

**Evidence.** lib.rs:704-707 hDrvDevice checked after 'let dxvk = bridge::ffi::helios_dxvk_create_device(0, 0);' (696); lib.rs:708-747 'core::ptr::write(create.h_drv_device as *mut HeliosDevice, ...)' then 'device_funcs::create_runtime_context(...)'; lib.rs:751-754 'if create.p_device_funcs.is_null() { ... return E_FAIL; }' — post-construction, no drop_in_place/context destroy. device_funcs.rs:440-448 'if hr == 0 { dev.h_context = arg.hContext; ... }' — failure falls through silently; lib.rs:794-795 '"  CreateDevice -> S_OK (DXVK device + D3D11 funcs table installed)"'.

**Recommendation.** Hoist all argument validation (h_drv_device, p_device_funcs, dxgi table pointer) to the top of create_device before creating the DXVK device or constructing anything — pure reorder, behavior-preserving for every input the real runtime sends. For (2), decide with the owner: either propagate CreateContext failure as E_FAIL (a behavior change confined to an already-broken path, matching 'loud failure over fake success'), or keep S_OK but add a named counter (e.g. UmdCtxCreateFail) so the partial state is visible; today it is only a log line.

**Risk.** Low for the reorder (real runtimes always pass non-null tables; the only caller exercising nulls is the selftest). The E_FAIL-on-context-failure half changes failure-path behavior and needs owner sign-off per the behavior-preserving charter.

**Dependencies.** R1 (remove-temporary-selftest)

**Atomic commit boundary.** Commit 1: hoist validation above construction (pure reorder). Commit 2 (owner-gated): propagate CreateContext failure or add the named counter.

**Validation.** Release UMD build; dwm + app device creation S_OK with identical log sequence; forced-failure unit check (synthetic args with null p_device_funcs) no longer constructs the device; no new counters moving on a healthy boot; visible desktop.


### D19. ensure_linear_copy_target_ready reports success on later calls even if its one-time layout-init submission failed

- **Category:** error-path · **Reported by:** `kmd-venus/ensure-target-partial-failure`
- **Files:** `kmd_render/src/virtio/venus.rs`
- **Symbols:** `VenusClient::ensure_linear_copy_target_ready`, `VenusClient::queue_submit_command_buffer`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** The function publishes copy_target_image_id/pool/cmd_buf BEFORE the init queue-submit (1793-1796, deliberately — the comment explains retaining the pool is the safe side of an ambiguous transport failure). But if queue_submit_command_buffer fails with a non-fatal error (host VkResult != 0 → diag 0x011D returns Err without latching `fatal`, 1690-1693), the target is now published as ready while the PREINITIALIZED→GENERAL+external transition may never have executed. Every subsequent call short-circuits at 1736-1737 (`if self.copy_target_image_id == target_image_id { return Ok(()); }`) — success after partial failure. Copies then run against an image in the wrong layout, and the host samples garbage or the venus decoder rejects the barrier chain, with no breadcrumb pointing at the missed init.

**Evidence.** venus.rs:1789-1796 '// Publish lifetime before queue submit: if transport failure makes the submission result ambiguous, retaining the pool is always safe ... self.copy_target_image_id = target_image_id; ... self.queue_submit_command_buffer(adapter, command_buffer_id, 0)' — Err propagates but state stays published. venus.rs:1736-1738 'if self.copy_target_image_id == target_image_id { return Ok(()); }'. Non-fatal failure mode: venus.rs:1690-1693 'if r.read_i32()? != 0 { diag(0x011D); return Err(VirtioError::DeviceError); }' (fatal latch is only set by ring waits, 613-622).

**Recommendation.** Behavior-preserving restructure: replace the zero-sentinel field trio with a tri-state (Unset / Ready / InitFailed) — publish InitFailed (still retaining pool/cmd_buf ids for safe teardown, preserving the ambiguity rationale) when the init submit errors, and have subsequent ensure calls on InitFailed return DeviceError with a dedicated diag code instead of Ok. This only changes an already-failing error path from silent fake success to loud failure, per the 'no fake success' operating rule; the happy path is untouched.

**Risk.** None to the direct-scanout primary (this code is the non-direct fallback); the fallback path converts a latent wrong-layout copy into a clean STATUS_DEVICE_NOT_READY.

**Dependencies.** R57 (prepared-copy-typestate)

**Atomic commit boundary.** One commit inside the CopyTargetState enum introduction (same state machine).

**Validation.** Code inspection + forced-failure unit of the state machine if codec tests exist; regression gate: fallback copy path still works (windowed/BLT primary scenario shows desktop), direct path untouched (VpSA=1/ScSet=1, ScCpy=2 zero-copy breadcrumb on primary boot).

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** 'Target is initialized' is inferred from copy_target_image_id != 0, which is set before the initializing submission is known to have been accepted; the invalid sequence is ensure→submit-fails→ensure→Ok→submit copies against an uninitialized-layout image.
1. **Compile-time representation:** CopyTargetState enum where Ready is constructed only from a successful submit result, and InitFailed has no transition to Ready for the same client lifetime.
1. **Smallest atomic migration:** VenusClient field change only; no caller signature changes.
1. **Remaining `unsafe` preconditions:** Transport-ambiguous submits (Err after possible enqueue) are inherently unknowable — InitFailed models 'unknown, treat as failed' which is the loud-failure policy, not proof.
1. **Regression test proving preserved behavior:** Fallback-path desktop scenario unchanged; no new CpCpy/ScCpy error codes during normal boots; the new failure code appears only under injected submit failure.


### D20. StartDevice/StopDevice hold `&mut AdapterContext` while live ISR/DPC paths concurrently hold `&AdapterContext` — shared-xor-mutable violated by construction

- **Category:** unsafe-contract · **Reported by:** `kmd-core/adapter-lifecycle-aliasing`
- **Files:** `kmd_render/src/ddi/start_device.rs`, `kmd_render/src/adapter.rs`, `kmd_render/src/ddi/interrupt.rs`
- **Symbols:** `dxgkddi_start_device`, `dxgkddi_stop_device`, `AdapterContext`, `drain_used_and_complete`, `vsync_dpc_routine`, `setup_bar_segment`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** start_device.rs:117 forms `&mut AdapterContext` and holds it across the whole body. At :162 `adapter.set_virtio(Some(gpu))` publishes the transport, after which the INTx ISR/used-ring DPC can run and take `&AdapterContext` (interrupt.rs:37) reading `adapter.dxgkrnl.as_ref()` (interrupt.rs:63) — while StartDevice keeps mutating through the same `&mut`: `bar_segment` (:168), `venus_ctx_id`/`page_table_window` (:201-213), `display_w/h`+`edid` (:249-253). StopDevice repeats it (:316 `&mut`, :331-349 mutations) while a queued DPC can still be draining. The vsync DPC (:295) likewise reads `dxgkrnl`/`display_half` as `&`. This is Rust aliasing UB regardless of whether the racy fields happen to be disjoint today; safety rests on comments ("Written once during the (serialized) StartDevice lifecycle DDI", adapter.rs:84-85), not types.

**Evidence.** start_device.rs:117 "let adapter = unsafe { &mut *(miniport_device_context as *mut AdapterContext) };"; :162 "adapter.set_virtio(Some(gpu));" then :168 "adapter.bar_segment = setup_bar_segment(adapter);", :249-253 `display_w/display_h/edid` writes. interrupt.rs:37 "pub(crate) fn drain_used_and_complete(adapter: &AdapterContext)"; :63 "if let Some(dxgkrnl) = adapter.dxgkrnl.as_ref()". start_device.rs:295 DPC "let adapter = unsafe { &*(context as *const AdapterContext) };", :299 reads `adapter.dxgkrnl`. stop_device :316 `&mut`, :331-334 "adapter.set_venus_client(None); adapter.page_table_window = None; adapter.venus_ctx_id = 0;" while a pre-flush-queued used-ring DPC may run. adapter.rs:84-85 invariant stated only as comment.

**Recommendation.** Behavior-preserving: change every lifecycle DDI to take `&AdapterContext`. Move the StartDevice-written fields into explicit interior-mutable write-once cells (a tiny `WriteOnce<T>`/`UnsafeCell` wrapper with Release publish + Acquire read accessors) for the DPC-visible set (`dxgkrnl`, `bar_segment`, `page_table_window`, `venus_ctx_id`, `display_w/h`, `edid`, knob fields), or stage population in a local `AdapterBoot` value and publish before any interrupt-visible state goes live. Keep the existing publish order exactly (dxgkrnl saved before transport init; bar_segment before segment queries).

**Risk.** Signature churn across start/stop/setup_bar_segment and field-access sites; a missed mutation site becomes a compile error (self-checking). Must not reorder the dxgkrnl-save / set_virtio / init_vsync sequence or the frozen refresh pipeline changes behavior.

**Atomic commit boundary.** One commit: flip start/stop signatures to `&AdapterContext` and wrap all fields they mutate, together — a partial conversion leaves a mixed `&mut`/`&` state that is still UB.

**Validation.** KMD builds both platforms; cold boot to visible desktop; `pnputil /restart-device` stop/start cycle healthy (StopDevice→StartDevice re-runs); ScanoutDiag absent; VpSA=1/ScSet=1; cursor responsive; ~63 fps DComp cadence; no new CtOut/WtOut/ring failures this boot.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** "dxgkrnl/bar_segment/etc. are written only during serialized lifecycle DDIs and read elsewhere" is a comment; the invalid sequence — DPC reads `dxgkrnl` while StopDevice/StartDevice holds `&mut` over the same struct — compiles today and is UB.
1. **Compile-time representation:** All DDIs take `&AdapterContext`; lifecycle-written fields become `WriteOnce<T>`/atomic-publish cells with safe Acquire readers, so no `&mut AdapterContext` ever exists after AddDevice boxing; mutation authority is the cell API, not exclusivity.
1. **Smallest atomic migration:** Single commit converting the two lifecycle DDI signatures plus every field they mutate.
1. **Remaining `unsafe` preconditions:** The `*mut c_void → AdapterContext` cast at each DDI entry (dxgkrnl round-trip contract) cannot be encoded; nor can dxgkrnl's serialization of Start/Stop against each other — both stay as trusted-boundary // SAFETY comments.
1. **Regression test proving preserved behavior:** Same-boot restart-device cycle plus cold boot with visible desktop, VpSA=1/ScSet=1, and unchanged IrqN/DpcN/RfDone counter progression.

**Lead-reviewer note.** Soundness defect (shared-xor-mutable violated by construction between StartDevice/StopDevice and live ISR/DPC paths), not a proven misbehavior. The structural cure — partitioning AdapterContext into interior-mutability cells with an explicit lifecycle typestate — should be designed together with R44/R48/R51 rather than patched ad hoc.



---

## Part II, Tranche 1 — Legacy-path removal

Deleting or sealing dead and dormant bring-up-era machinery first shrinks every later tranche. All removals must be behavior-preserving under **default** knob values; knobs with operational kill-switch value are kept and documented, not deleted. Liveness of anything claimed dead must be re-proven (callers + knob defaults) at implementation time — most entries here are unverified.

**Regression-gate emphasis after this tranche:** visible desktop + cursor + VpSA=1/ScSet=1 + no new gate timeouts; for R2 additionally the ICD-side vehicle counters named in the entry.

### R1. TEMPORARY Gate-5b selftest export (~120 lines in lib.rs + ~590 lines in forward.rs) still shipped after its stated removal condition was met

- **Category:** legacy-removal · **Reported by:** `umd-core/remove-temporary-selftest`
- **Merged duplicate reports (2):** `umd-core/remove-dead-d3d12-scaffolding` — OpenAdapter12 dead body + ~350 lines of unreachable D3D12 DDI scaffolding compiled into the shipping UMD; `xc-legacy/umd-diag-probes` — UMD bring-up probe suite still compiled into the production DLL: self-marked-TEMPORARY selftests + HLSL compiler, unreachable D3D12 scaffolding, env-gated present readback/BMP-dump/force-opaque probes
- **Files:** `umd/src/lib.rs`, `umd/src/forward.rs`
- **Symbols:** `helios_umd_selftest`, `selftest_offscreen_clear`, `selftest_triangle`, `selftest_cb_readback`, `selftest_triangle_cb`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 3 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** lib.rs:252-255 marks helios_umd_selftest 'TEMPORARY (Gate 5b bring-up)... Remove once the DDI path is validated end-to-end.' The DDI path has been validated (hardware-accelerated desktop milestone met 2026-07-05; PSC stage). The export synthesizes a CreateDevice with null pKTCallbacks/hRTDevice (lib.rs:307-322), which is why create_runtime_context must tolerate null callbacks, and drives forward.rs selftest_* (6239-6830, ~590 lines) that duplicate render/readback logic in the production DLL. Repo-wide grep finds no other caller (tools/, .ps1, .md all clean; only the untracked umd_clean copy).

**Evidence.** lib.rs:252-256 '/// TEMPORARY (Gate 5b bring-up): out-of-band smoke test of the DXVK bridge... Remove once the DDI path is validated end-to-end.'; lib.rs:307-311 synthetic args with 'p_kt_callbacks: core::ptr::null()'; forward.rs:6239 'pub unsafe fn selftest_offscreen_clear', 6399 'selftest_triangle', 6534 'selftest_cb_readback', 6610 'selftest_triangle_cb'. Grep for helios_umd_selftest across repo: only umd/src/lib.rs and the git-ignored umd_clean/ copy.

**Recommendation.** Delete helios_umd_selftest and forward::selftest_* in one commit, or move them behind a non-default cargo feature (e.g. `selftest`) if the owner still uses rundll32-style smoke tests. Keep create_runtime_context's defensive null checks (they are legal-DDI hygiene regardless).

**Risk.** Low. Only risk is an undocumented guest-side scheduled task invoking the export; grep of tools/ and docs found none, but the owner should confirm no schtasks references it before the commit lands.

**Dependencies.** owner confirms no guest schtasks/tooling invokes helios_umd_selftest

**Atomic commit boundary.** One commit removing the export + the four forward.rs selftest fns; a separate follow-up may tighten CreateDevice arg validation once the synthetic caller is gone.

**Validation.** Release UMD build; DLL size drop; win_install_umd + adapter restart; visible desktop; dwm device create S_OK in umd-<pid>.log; noop counters unchanged; VpSA=1/ScSet=1; DComp cadence ~63fps.

**Lead-reviewer note.** Do R1 before the R14/R15 splits: ~1100 lines of selftest/HLSL/D3D12/probe code simply disappear instead of being moved. The env-gated present readback/BMP probes overlap R12 — coordinate so each block is handled exactly once (delete here; cache-and-seal in R12 only what survives).


### R2. Vehicle kwait + present-sync publish machinery is dormant under default knobs yet still executes setup side effects per device in the live present path

- **Category:** legacy-removal · **Reported by:** `umd-forward-c/vehicle-kwait-dormant-machinery`
- **Merged duplicate reports (4):** `xc-legacy/kwait-publish-dead` — The vehicle kernel-flip-wait (kwait) + present-result chain is unreachable under default knobs (PresentSyncPublish=0) yet still creates a monitored fence and arms bridge state per device; `xc-unsafe/iddcx-publish-kwait-legacy` — Default-off IddCx named-fence publish + vehicle kwait machinery still compiled into every present; `xc-errors/legacy-vehicle-present-isolation` — Legacy dcomp-vehicle/IddCx present machinery is still compiled into the active dxgi_present flow; isolate it so it cannot enter the exact-primary path; `xc-concurrency/present-path-sealed-enum` — Legacy vehicle/IddCx/copy-era present machinery is interleaved with the direct-primary path in dxgi_present, routed by scattered booleans and a process-name heuristic
- **Files:** `umd/src/forward.rs`, `umd/src/lib.rs`
- **Symbols:** `flip_wait_setup`, `vehicle_present_prepare`, `dxgi_present`, `present_sync_publish_enabled`, `vehicle_kernel_flip_wait`
- **Verification:** **CONFIRMED** (severity low) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** Three ordering mechanisms coexist in dxgi_present: kernel flip-wait (arm block 8436-8474), WS1#4 publish/acquire-gate, and the bounded CPU gate. lib.rs:1260-1263 documents PresentSyncPublish as 'Legacy IddCx producer switch... Absent = 0 because the real Helios display adapter has no cross-process IDD consumer'. With publish off, vehicle_present_prepare returns sync_value=0, so the kwait arm condition 'is_vehicle_present && sync_value != 0' (8436) is never true and take_present_result always misses — yet flip_wait_setup(dev) still runs per vehicle present (7885) purely to compute a kwait_ordered flag consumed only by the disabled publish call: on first vehicle present it creates a runtime monitored fence and starts the bridge watchdog (7648-7677) that nothing will ever wait on. The vehicle itself is still live (mesa wsi_common_win32.cpp:876-877 resolves helios_umd_set_present_source), so this is dormant machinery inside an active flow, not dead code.

**Evidence.** forward.rs:7885 'let kwait_ordered = flip_wait_setup(dev);' unconditional; 7888 'if present_sync_publish_enabled() {' gates the only consumer; 8436 'if is_vehicle_present && sync_value != 0' gates the only kernel wait; lib.rs:1261-1263 'Absent = 0 because the real Helios display adapter has no cross-process IDD consumer.'; icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:876-877 still resolves 'helios_umd_set_present_source'.

**Recommendation.** Behavior-preserving step: hoist the publish-enabled check so flip_wait_setup is only invoked when a publish can actually produce a nonzero sync_value (guard 7885 with present_sync_publish_enabled()); the disabled-path fence/watchdog side effect disappears while every enabled configuration is unchanged. Then, module-seal the whole kwait+publish+result complex inside forward/vehicle.rs behind one entry point. Actual deletion of the kwait/acquire-gate path requires owner sign-off plus live counter evidence (EXT_KWAIT_ARMED/EXT_PRESENTS across a boot with real Vulkan workloads and with PresentSyncPublish=1 A/B) — record in ROADMAP.md, do not delete in this phase.

**Risk.** If any deployment sets PresentSyncPublish=1 (ICD acquire-gate perf path from the 25th-28th sessions), the guard must not change its behavior — the hoisted condition must exactly reproduce 'publish enabled => kwait_ordered as today'. Deleting instead of sealing would break that configuration.

**Dependencies.** R14 (forward-split-modules); external: owner decision + live counter evidence before any deletion

**Atomic commit boundary.** Commit 1: guard flip_wait_setup call with present_sync_publish_enabled(). Commit 2: move/seal vehicle module (covered by split).

**Validation.** Default-knob boot: vehicle presents (vkcube/DComp probe) still ~63 fps, no 'flip-kwait READY' log line (proves fence no longer created), EXT_FLIP_GATE_TIMEOUTS steady; PresentSyncPublish=1 A/B boot: 'flip-kwait READY' appears and kwait_armed counts as before.

**Verifier corrections (authoritative).** Minor refinements (do not change the verdict): (1) the watchdog thread start is in umd/bridge/dxvk_bridge.cpp:2068 (reached via the bridge call at forward.rs:7668), not within forward.rs:7648-7677 itself — forward.rs 7648-7666 creates the monitored fence; (2) "still runs per vehicle present" should read "called per vehicle present; the heavy side effects (fence + watchdog) fire once per device via the flip_wait_state cache (forward.rs:7619-7622)", matching the title's per-device wording; (3) add to evidence: the default-config log at forward.rs:7678-7681 falsely claims "vehicle flips are kernel-ordered... CPU gate retired for this device" — the CPU gate actually serves every present since kernel_wait_armed can never be set; (4) add to validation: EXT_RESULT_MISSES / "get_present_result: none pending" cadence must be unchanged (ICD serial-wait fallback identical).

**Lead-reviewer note.** Verified CONFIRMED. Commit 1 (gate flip_wait_setup on present_sync_publish_enabled()) is proven behavior-preserving and removes one inert kernel fence + one watchdog thread per vehicle-presenting device plus an affirmatively false log line. Full deletion of the kwait/publish chain is owner-gated with counter A/B. The module-isolation half of the merged reports composes with the R14 split (vehicle code moves to its own module, entered only via the route classifier of R68).


### R3. Copy-era LINEAR scanout machinery still compiled into and executed on the active per-present flush path

- **Category:** legacy-removal · **Reported by:** `umd-forward-a/legacy-linear-copy-machinery`
- **Merged duplicate reports (1):** `umd-forward-b/legacy-linear-copy-hook-in-om-bind` — Legacy LINEAR copy-scanout selection still rides inside every OMSetRenderTargets, excluded from the exact-primary path only by a runtime list lookup
- **Files:** `umd/src/forward.rs`, `umd/src/device_funcs.rs`, `umd/bridge/dxvk_bridge.cpp`
- **Symbols:** `ensure_kmd_scanout_target`, `track_dwm_composition_target`, `publish_dwm_composition`, `copy_to_scanout_target`, `remember_scanout_target`, `flush`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Frozen baseline: DWM renders directly into the exact OPTIMAL primary, no guest primary-to-scanout copy. Yet flush() (2868-2882) calls publish_dwm_composition on every Flush, which calls ensure_kmd_scanout_target (685-730): in DWM it imports the legacy KMD LINEAR target into DWM's Venus device (bridge dxvk_bridge.cpp:1258 via helios_venus_query_scanout) and overwrites dev.scanout_* Cells (716-723) that remember_scanout_target (639-644) had set from the direct primary — the same Cell cluster holds two meanings discriminated only by call order. If the query fails, the full bridge/ICD chain is retried on EVERY flush/present. set_render_targets (3940) still feeds track_dwm_composition_target, guarded by a per-call linear scan of direct_scanout_allocations (745-751). copy_to_scanout_target is reachable from dxgi_blt1/present1 fallbacks (8374, 9189). remember_scanout_target uses a largest-area-wins heuristic (633-638).

**Evidence.** forward.rs:2868-2878 `let published = unsafe { publish_dwm_composition(&context, h) }; context.Flush();`; :716-723 `dev.scanout_resource_raw.set(raw); ... dev.scanout_format.set(87);` overwriting fields set at :639-644; :633-638 `if current_area != 0 && area < current_area { return; }`; :751 `if resource_raw == 0 || direct_primary || !ensure_kmd_scanout_target(h)`; :803 `context.CopySubresourceRegion(target, ...)`; bridge dxvk_bridge.cpp:1272-1276 per-call `find_helios_icd_export("helios_venus_query_scanout")`; REFACTOR_HANDOFF.md:21-22 "no guest primary-to-scanout copy".

**Recommendation.** Behavior-preserving step: seal the fallback into scanout.rs behind an explicit one-shot ScanoutPath enum { DirectPrimary(ValidatedScanoutDesc), LegacyLinearCopy(LinearImport) } chosen at first primary creation, so exact-primary code cannot reference legacy state and the legacy import cannot clobber direct-primary fields. Phase-2 removal candidate (delete helpers + scanout_*/composition_source/scanout_import state) only after same-boot evidence that scanout_copy_count stays 0 and "DWM KMD scanout import ready" no longer fires on the 142 baseline, with owner sign-off.

**Risk.** Medium: dxgi_blt1/present1 non-primary paths still call copy_to_scanout_target; removing (vs sealing) needs confirmation those paths are dead on the baseline. Sealing alone is low risk.

**Dependencies.** R14 (split-forward-rs); owner confirmation + same-boot log evidence that the LINEAR fallback is unused on KMD 22.22.142.0

**Atomic commit boundary.** Commit 1: extract scanout.rs and introduce the sealed ScanoutPath enum (no behavior change). Commit 2 (after evidence): delete the LegacyLinearCopy variant and its device state.

**Validation.** Baseline boot log check: absence/presence of "DWM desktop->LINEAR scanout copy" and "DWM KMD scanout import ready"; then visible desktop, VpSA=1/ScSet=1, ScanoutDiag absent, same-boot QEMU evidence of the OPTIMAL DWM primary, cursor, 63 fps cadence, no DWM crash.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** "Fallback LINEAR-copy resources must never affect the exact-primary path" is enforced by a per-call direct_primary linear scan (745-751), an is_dwm_process() exe-name gate (669-679), and call-order between remember_scanout_target and ensure_kmd_scanout_target. Invalid sequence permitted: first flush overwrites the direct primary's dev.scanout_* fields with the LINEAR import, and any future reader of scanout_* on the primary path silently gets the fallback identity.
1. **Compile-time representation:** Sealed ScanoutPath enum stored once in HeliosDevice; DirectPrimary variant carries a ValidatedScanoutDesc, LegacyLinearCopy carries the imported target; exact-primary code paths take &DirectPrimary so legacy state is unnameable there.
1. **Smallest atomic migration:** scanout.rs module extraction + replacing the six scanout_* Cells and scanout_import/composition_source RefCells with the enum; callers (flush, set_render_targets, dxgi_present family) updated in the same commit.
1. **Remaining `unsafe` preconditions:** COM raw-pointer lifetimes of the imported ID3D11Resource stay unsafe (windows-crate from_raw); the compiler cannot see DXVK's refcount.
1. **Regression test proving preserved behavior:** Same-boot: scanout_copy_count==0, no "DWM desktop->LINEAR scanout copy" lines, VpSA=1/ScSet=1, QEMU shows the OPTIMAL primary, 63 fps DComp cadence unchanged.

**Lead-reviewer note.** CAUTION (from the verified R14 corrections): ensure_kmd_scanout_target is LIVE fallback code, called at forward.rs:751/:789 and bypassed only when direct_primary is set. Scope here is containment — move it into one fallback module entered through the R68 route type — NOT deletion. Deletion requires separately proving the fallback unreachable on all supported configurations, which nobody has done.


### R4. Proven-rejected bring-up bisect arms (BarSegMode 1/2/5/11, probe RAM, const levers) still compiled into the active StartDevice/segment-query flow

- **Category:** legacy-removal · **Reported by:** `kmd-core/barsegmode-legacy-arms`
- **Merged duplicate reports (1):** `xc-legacy/barseg-knob-bisect` — AddAdapter segment topology is still fully knob-driven bisect machinery (BarSegMode arms 1/2/5/11, BarSegFlags/BarSegBaseMB, probe_only RAM) for a problem solved 2026-07-05
- **Files:** `kmd_render/src/ddi/start_device.rs`, `kmd_render/src/ddi/query_adapter_info.rs`, `kmd_render/src/adapter.rs`
- **Symbols:** `setup_bar_segment`, `BarSegment::probe_only`, `bar_probe_ram`, `VENUS_ALLOC_ENABLED`, `REPORT_APERTURE_PAGING_SEGMENT`, `DECLARE_CROSS_ADAPTER_RESOURCE`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** The BarSegMode ladder documents every non-production arm as rejected: start_device.rs:37-38 mode 1 "REJECTED by dxgmms", :38 mode 2 "historic size-bisect arm; rejected", :39 mode 5 "historic backing-bisect arm; rejected", :47-48 mode 11 "rejected — confirms the must-be-last rule". Yet mode 5 still allocates 16 MiB probe RAM (:58-72) with its `probe_only` field (adapter.rs:73-78) and `bar_probe_ram` slot + Drop arm (adapter.rs:134-136, 1058-1061), and query_segments still carries the topo-11/default table arms (query_adapter_info.rs:717-726). Two const levers are permanently decided with large dead branches compiled in: VENUS_ALLOC_ENABLED=true (start_device.rs:17, dead else at :180-185) and REPORT_APERTURE_PAGING_SEGMENT=true (query_adapter_info.rs:388, dead SAFE-shape arm :761-775). DECLARE_CROSS_ADAPTER_RESOURCE=false is OR-ed with the runtime knob (:210) so the const arm is dead too.

**Evidence.** start_device.rs:37-39 "1 = ... REJECTED by dxgmms... 2 = ... (historic size-bisect arm; rejected) 5 = ... (historic backing-bisect arm; rejected)"; :47-48 "11 = ... (rejected — confirms the must-be-last rule...)"; :58-72 mode-5 probe allocation; adapter.rs:73-78 "AddAdapter-acceptance probe only (`BarSegMode` 5...)"; start_device.rs:17 "const VENUS_ALLOC_ENABLED: bool = true;"; query_adapter_info.rs:388 "const REPORT_APERTURE_PAGING_SEGMENT: bool = true;" with the 60-line dead false-arm at :761-775; :412 "pub(crate) const DECLARE_CROSS_ADAPTER_RESOURCE: bool = false;" OR-ed at :210.

**Recommendation.** Delete the arms proven unable to bind (1/2/5/11) and their supporting state (probe_only, bar_probe_ram, mode-5 allocation); parse BarSegMode once into an exhaustive enum { Off = recovery, Production } with unknown values mapped to Production + a named counter. Inline the decided consts (VENUS_ALLOC_ENABLED, REPORT_APERTURE_PAGING_SEGMENT) into straight-line code, keeping their doc history in the commit message. Keep BarSegMode=0 and DisplayHalf=0 recovery shapes — they are the documented recovery levers, not bisect archaeology.

**Risk.** Removes A/B levers; needs owner sign-off that the rejected shapes will never be re-bisected (external prerequisite). Pure deletion otherwise — the surviving paths are byte-identical.

**Dependencies.** owner sign-off that rejected bisect arms are retired

**Atomic commit boundary.** One commit per lever family: (1) BarSegMode arms + probe RAM state; (2) VENUS_ALLOC_ENABLED / REPORT_APERTURE_PAGING_SEGMENT const inlining.

**Validation.** Boot with default registry (BarSegMode absent → Production) and with BarSegMode=0 (recovery render-only); AddAdapter binds; desktop visible; VpSA=1/ScSet=1; grep proves probe_only/bar_probe_ram gone.

**Lead-reviewer note.** Keep the production mode (BarSegMode 10) and one documented kill-switch shape; delete only arms the memory record marks disproven (bisect modes 1/2/5/11, probe RAM, const levers). KMD change: three-site version bump + guest reboot to verify AddAdapter still binds Code 0.


### R5. wait_gpu/refresh_scanout DMA-retire refresh path is dead code threaded through the active fence pipeline

- **Category:** legacy-removal · **Reported by:** `kmd-submit/dead-wait-gpu-refresh-path`
- **Files:** `kmd_render/src/ddi/submit_command.rs`, `kmd_render/src/virtio/gpu.rs`, `kmd_render/src/ddi/interrupt.rs`
- **Symbols:** `note_and_maybe_signal`, `note_wddm_submission`, `WddmPending`, `WddmReady`, `drain_used_and_complete`
- **Verification:** **CONFIRMED** (severity low) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** Both submit DDIs call note_and_maybe_signal(.., false); wait_gpu is never true anywhere, yet it threads through note_wddm_submission into WddmPending.wait_gpu and refresh_scanout:wait_gpu, and the DPC has a dead `if ready.refresh_scanout { request_scanout_refresh() }` branch. This is the pre-v142 copy-era "refresh scanout at DMA retire" model, superseded by the frozen watermark path (note_scanout_refresh/take_ready_scanout_refresh, which is live and must not be touched). The dead boolean also feeds async_retired_up_to's wait_gpu arg from the pending FIFO, making the ring>=1 gating rule harder to read than it is.

**Evidence.** submit_command.rs:335 and :364 `note_and_maybe_signal(adapter, fence, is_paging, false)` (only callers); :277 `if wait_gpu { adapter.request_scanout_refresh(); }`; gpu.rs:1774-1775 `wait_gpu, refresh_scanout: wait_gpu,`; gpu.rs:670-671,677 WddmPending/WddmReady fields; interrupt.rs:60-62 `if ready.refresh_scanout { adapter.request_scanout_refresh(); }` — unreachable since wait_gpu is always false.

**Recommendation.** Delete the wait_gpu parameter from note_and_maybe_signal and note_wddm_submission, the wait_gpu/refresh_scanout fields from WddmPending and refresh_scanout from WddmReady, and the interrupt.rs:60 dead branch. async_retired_up_to keeps its bool only for the two literal-true watermark callers (or split into two named fns). All removed values are statically false, so behavior is identical.

**Risk.** Low: every deleted path is provably unreachable (grep shows no true-passing caller). Risk is only merge friction with other submit_command work.

**Atomic commit boundary.** One commit removing the dead parameter/fields end-to-end (submit_command.rs + gpu.rs + interrupt.rs).

**Validation.** KMD builds; boot to visible desktop; VpSA=1/ScSet=1; DComp cadence ~63 fps; idle-to-active dirty edge still wakes (proves the live take_ready_scanout_refresh path was untouched); no new gate timeouts; WDDM_FENCE_FROM_DPC still advances.

**Lead-reviewer note.** Verified CONFIRMED with full caller/git-history proof; all deleted values are statically false. Safe first KMD commit of the tranche.


### R6. reply_shmem_roundtrip and the virtqueue-seqno warm-up machinery are dead code kept compiled in the client

- **Category:** legacy-removal · **Reported by:** `kmd-venus/dead-roundtrip-removal`
- **Files:** `kmd_render/src/virtio/venus.rs`
- **Symbols:** `VenusClient::reply_shmem_roundtrip`, `roundtrip_seqno`, `CMD_SUBMIT_VIRTQUEUE_SEQNO_MESA`, `CMD_WAIT_VIRTQUEUE_SEQNO_MESA`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** reply_shmem_roundtrip (701-715) is #[allow(dead_code)] and has zero callers; the bring-up explicitly documents its removal: 'The previous warm-up used a DIRECT vkWaitVirtqueueSeqnoMESA, which the host rejects ("must be called on ring dispatch") — removed' (2548-2551). Its support state (roundtrip_seqno field, 507; initialized 2507) and the two command ids CMD_SUBMIT/WAIT_VIRTQUEUE_SEQNO_MESA (76-77, used nowhere else) survive in the production client. A stale #[allow(dead_code)] also sits on ring_command_noreply (661) which IS used (1335, 1460, 1503, 1533, 1634, 1708, 2441), hiding future real dead code from the compiler.

**Evidence.** venus.rs:701-702 '#[allow(dead_code)] fn reply_shmem_roundtrip' — no callers (grep). venus.rs:2548-2551 'The previous warm-up used a DIRECT vkWaitVirtqueueSeqnoMESA, which the host rejects ("must be called on ring dispatch") — removed.' venus.rs:76-77 'const CMD_SUBMIT_VIRTQUEUE_SEQNO_MESA: u32 = 251; const CMD_WAIT_VIRTQUEUE_SEQNO_MESA: u32 = 252;' only referenced at 706/712 inside the dead fn. Stale allow: :661-662 '#[allow(dead_code)] fn ring_command_noreply' despite live uses at 1335, 1460, 1503, 1533, 1634, 1708, 2441.

**Recommendation.** Delete reply_shmem_roundtrip, the roundtrip_seqno field, and the two orphaned command constants (bring-up comment at 2547-2551 already preserves the why); drop the stale allow on ring_command_noreply so dead-code lint works again. Pure deletion, no behavior change.

**Risk.** None — provably uncalled; the host rejects the command anyway.

**Atomic commit boundary.** One deletion commit.

**Validation.** Build with dead-code lint clean; grep confirms no references; standard regression gate.


### R7. ScanoutDiag: 16 registry-gated diagnostic scanout modes (~950 lines) with disproven arms, a per-SetVidPnSourceAddress registry read, and diag resources that can reach the production publish words

- **Category:** legacy-removal · **Reported by:** `xc-legacy/scanout-diag-legacy`
- **Merged duplicate reports (2):** `kmd-transport-ctrl/diag-scanout-extraction` — ~400 lines of ScanoutDiag-only diagnostic scanout paths (with duplicated fill loops, duplicated virgl constants, and intentional leaks) live inside the active control module; extract behind a sealed diagnostic module; `xc-duplication/kmd-diagnostic-scanout-legacy` — Four diagnostic scanout builders in ctrl.rs (plus venus.rs diagnostic allocators) triplicate color-bar fills and VIRGL constant blocks and are compiled into the production driver
- **Files:** `kmd_render/src/ddi/scanout_diag.rs`, `kmd_render/src/virtio/ctrl.rs`, `kmd_render/src/adapter.rs`, `kmd_render/src/ddi/display.rs`
- **Symbols:** `maybe_run`, `rebind_if_forced`, `diag_mode`, `diagnostic_2d_scanout`, `diagnostic_guest_blob_scanout`, `diagnostic_virgl_host3d_blob`, `diagnostic_virgl_host3d_guest_scanout`, `set_scanout_2d`, `AdapterContext::diag_scanout_*`, `remember_scanout_blob`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 3 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** scanout_diag.rs (533 lines) implements modes 1-16; the 39th session root-caused the problem these modes bisected (IMAGE_TILING_LINEAR const), and the frozen baseline requires ScanoutDiag absent. ctrl.rs carries ~400 more lines (diagnostic_* verbs at 618-1020 + set_scanout_2d at 564) whose ONLY callers are scanout_diag.rs. Liveness: default absent ⇒ maybe_run returns after writing ~24 registry zeros each StartDevice (l.112-139), and rebind_if_forced returns false — but it calls diag_mode() (an RtlQueryRegistryValues round-trip) on EVERY PASSIVE SetVidPnSourceAddress (display.rs:747), and up to 3 more times per forced rebind (l.479,503,514). Mode>=2 rebinds route diag blobs through remember_scanout_blob, which also sets the production host_bound_scanout_resource (adapter.rs:510-511).

**Evidence.** scanout_diag.rs:18-20 'fn diag_mode() { crate::diag::read_config_dword(b"ScanoutDiag", 0) }' called at 479, 503, 514; display.rs:747 'if crate::ddi::scanout_diag::rebind_if_forced(adapter, 11)' inside dxgkddi_set_vidpn_source_address; ctrl.rs outline: diagnostic_2d_scanout:618, diagnostic_guest_blob_scanout:700, diagnostic_virgl_host3d_blob:787, diagnostic_virgl_host3d_guest_scanout:866 — grep shows callers only in scanout_diag.rs; adapter.rs:508-511 remember_scanout_blob stores host_bound_scanout_resource; REFACTOR_HANDOFF.md:34 'ScanoutDiag is absent and must remain absent'.

**Recommendation.** Delete modes 1-15 + their ctrl.rs verbs + diag_scanout_* adapter fields + rebind_if_forced (and its display.rs call site). If a scanout oracle retains operational value, keep only mode 16 (the LINEAR venus oracle) behind a StartDevice-cached bool so the per-flip registry read disappears; route it through a sealed Diag arm (see scanout-identity-static) so it cannot write production publish words. Behavior-preserving under default (knob absent).

**Risk.** Losing a boot-time diagnosis tool; mitigated by the proven host-side oracle (/tmp/vk-dmabuf-scanout.c per 39th-session memory) and by keeping mode 16 if desired. Non-default knob value has some kill-switch-adjacent value only as a diagnostic, never in production.

**Atomic commit boundary.** Commit 1: delete rebind_if_forced + display.rs call + per-flip registry read. Commit 2: delete modes/verbs/fields.

**Validation.** Boot with ScanoutDiag absent is byte-identical behavior (VpSA=1/ScSet=1, visible desktop); Sdg* value names disappear from the service key; SetVidPnSourceAddress no longer performs a registry query per call (measure present-to-scanout before/after).

**Lead-reviewer note.** ScanoutDiag stays a supported, off-by-default diagnostic tool — trim the disproven arms and extract the four color-bar/VIRGL builder copies out of ctrl.rs/venus.rs into one sealed diagnostic module. The per-flip registry read is R13; the type-level seal against the primary path is R45. Frozen-baseline requirement: ScanoutDiag absent during all primary tests.



---

## Part II, Tranche 2 — Hot-path telemetry containment

The evidence-counter discipline stays — counters are how this project debugs — but registry I/O and formatting move off per-present/per-submit/per-paging paths onto atomics mirrored out at PASSIVE cadence, following the rate-limit patterns the codebase already uses (display.rs `n < 8 || n & 0x3FF == 0`, gdi_blit deferred flush). Failure counters must stay loud per the diag contract. D1 (registry writes above PASSIVE in SetVidPnSourceAddress) is the defect-priority member of this family.

**Regression-gate emphasis:** counters still move this-boot (CLAUDE.md rule 7 check before/after), DComp cadence ≥ baseline 63 fps, no counter consumers (AzureTriage recipes, probe scripts) broken by renamed values — keep names stable.

### R8. dxgkddi_present performs ~15 unconditional synchronous registry writes plus two virtio-lock lookups per call, duplicating data already mirrored in atomics

- **Category:** telemetry · **Reported by:** `kmd-display/present-hotpath-registry-telemetry`
- **Merged duplicate reports (4):** `xc-errors/present-ddi-unthrottled-registry-telemetry` — DxgkDdiPresent and DxgkDdiPresentToHwQueue perform ~8-15 unthrottled synchronous registry writes per call on the BLT-present hot path; `xc-concurrency/present-ddi-hotpath-registry` — DxgkDdiPresent writes ~5-13 registry values and takes the device spinlock twice for diagnostics on every present; `xc-legacy/kmd-present-pb-trace` — DxgkDdiPresent performs ~8+ unconditional synchronous registry writes and two device-spinlock blob lookups per present — IddCx-era feasibility tracing on the active hot path; `xc-unsafe/present-ddi-per-present-registry-writes` — DxgkDdiPresent performs ~5-14 synchronous registry writes and two device-spinlock acquisitions on every present
- **Files:** `kmd_render/src/ddi/display.rs`, `kmd_render/src/diag.rs`, `kmd_render/src/device.rs`
- **Symbols:** `dxgkddi_present`, `rec_named`, `diag_dump_present_atomics`, `PRESENT_LAST_SRC_COUNT`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 5 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** Every DxgkDdiPresent call unconditionally writes PBcall/PBflag/PBcnt/PBalst (display.rs:277-290), PBpdsz (299), and on the allocation-list path PBsrcH/PBdstH/PBsrc/PBsw/PBsh/PBstrk/PBdst/PBdw/PBdh/PBdtrk (321-371) via rec_named → RtlWriteRegistryValue, plus two adapter.with_virtio(blob_lookup) lock round-trips (346, 362) purely for the PBstrk/PBdtrk codes. The same function already maintains PRESENT_* atomics (266-271, 304-308) with an existing PASSIVE dump (diag_dump_present_atomics, 238-248, invoked from device.rs:99) — the registry writes are a second copy of much of that data. The comment (273-276) shows this is bring-up archaeology ('is DxgkDdiPresent even the hook for the IddCx composition present') from the retired IddCx era. Every other tracer in the file is rate-limited (issue_present_scanout at 214-222: 'n < 8 || n & 0x3FF == 0'; VpSA at 645: '% 600'); the PB* block is not. Present runs at PASSIVE so this is a cost/duplication issue, not an IRQL bug.

**Evidence.** display.rs:277-282 'rec_named(b"PBcall", PRESENT_COUNT.load(Ordering::Relaxed)); rec_named(b"PBflag", present_flags); rec_named(b"PBcnt", ...)' unconditional; 346 'let lk = adapter.with_virtio(|v| v.blob_lookup(s.resource_id));' per call; contrast 214-215 'let n = PRESENT_SCANOUT_SUCCESS_COUNT.fetch_add(1, ...); if n < 8 || n & 0x3FF == 0'. Duplication: 266-267 stores PRESENT_LAST_SRC_COUNT/DST atomics, 279-282 writes the same counts as PBcnt; dump already exists at 238-248, called from device.rs:99.

**Recommendation.** Convert the PB* block to the established pattern: keep/extend the atomics, extend diag_dump_present_atomics (or a DiagLevel>=1 gate) for the extra fields, and drop the per-call blob_lookup probes or rate-limit them like PScSet. Also fold display.rs:21-31 rec_named into diag::record_named_bytes (verbatim duplicate). First measure: read PBcall delta over 10 s this boot to record the actual call rate in the commit message (Operating Rule 7).

**Risk.** Low; diagnostic-only change. PB* live-registry readability degrades to sampled — acceptable since the IddCx question it answered is closed.

**Atomic commit boundary.** One commit gating/removing the PB* registry writes + rec_named dedup; no present-path logic change.

**Validation.** Before/after PBcall-rate measurement; reboot; desktop visible; DComp cadence >= baseline 63 fps; PRESENT_* atomics still dump on device create.

**Lead-reviewer note.** Five independent reports. Measure first per Operating Rule 7: capture per-present cost (ETW or a cycle counter) before/after to land with numbers.


### R9. dxgkddi_present_to_hw_queue performs up to 8 unconditional synchronous registry writes per call

- **Category:** telemetry · **Reported by:** `kmd-submit/phq-present-registry-telemetry`
- **Files:** `kmd_render/src/ddi/scheduler.rs`
- **Symbols:** `dxgkddi_present_to_hw_queue`, `record_named_bytes`
- **Verification:** **MODIFIED** (severity low) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** Every invocation writes PHQcall, PHQflag, PHQcnt, PHQalst, PHQsrcH, PHQdstH, PHQsrc, PHQst via record_named_bytes — which is an ungated RtlWriteRegistryValue (diag.rs: 'Named counters (record_named*) are NOT gated here'). This violates the file's own established patterns: display.rs rate-limits success-path traces (`n < 8 || n & 0x3FF == 0`) and gdi_blit defers flushes to every 64th batch precisely because 'per-batch dump is 20 synchronous kernel registry writes on the hottest GDI path'. If dxgkrnl ever routes presents through the HW queue at frame rate, this is ~500 registry writes/s on the present path. Caveat per evidence discipline: SubmitCommandToHwQueue returns NOT_SUPPORTED, so this DDI is likely cold today — read PHQcall's this-boot delta before treating it as hot.

**Evidence.** scheduler.rs:226-227 `PRESENT_HWQ_COUNT.fetch_add(...); crate::diag::record_named_bytes(b"PHQcall", ...)`; :243-250 PHQflag/PHQcnt/PHQalst unconditional; :259-279 PHQsrcH/PHQdstH/PHQsrc; :327 PHQst on success path. diag.rs:35-37 'their steady-state writers ... each cost a synchronous kernel registry write' and :34-36 'Named counters (record_named*) are NOT gated here'; contrast display.rs:214-215 `if n < 8 || n & 0x3FF == 0` and gdi_blit.rs:84-87 FLUSH_EVERY=64.

**Recommendation.** Apply the display.rs pattern: first-8 + every-1024th success trace; keep failure statuses always-loud (PHQst on error paths). Alternatively gate the descriptive fields (PHQflag/PHQcnt/PHQalst/srcH/dstH) behind DiagLevel>=1 and keep PHQcall/PHQsrc as atomics flushed on DestroyDevice like the engine atomics.

**Risk.** Low; purely trace cadence. Keep the counters' names so existing triage recipes still find them.

**Atomic commit boundary.** One commit in scheduler.rs.

**Validation.** Measure first (rule 7): record PHQcall across a DComp run pre/post; verify identical present behavior, 63 fps cadence, and that a forced failure still lands a PHQst value.

**Verifier corrections (authoritative).** (1) current_state overbroad: "Every invocation writes [all 8]" is false — only PHQcall (+PHQst on early-fail) is unconditional (2 writes on null-arg calls); a plain successful call writes 5 (PHQcall/PHQflag/PHQcnt/PHQalst/PHQst); PHQsrcH/PHQdstH/PHQsrc additionally require `(present_flags & (1<<2)) == 0 && !allocation_list.is_null()` (scheduler.rs:252). The title's "up to 8" is the correct formulation. (2) Liveness tightened: the path is cold BY CONSTRUCTION, not merely "likely cold" — query_adapter_info.rs:224-225 advertises only MULTI_ENGINE_AWARE|PREEMPTION_AWARE (no HW-scheduling/HwQueue caps), so dxgkrnl cannot route frame-rate presents through HW queues on this configuration; the "~500 registry writes/s" scenario is hypothetical until HwSch caps are ever advertised. This makes the fix hygiene/future-proofing, severity low, not a live perf defect. (3) Recommendation stands as written and is safe: rate-limit success traces (display.rs `n < 8 || n & 0x3FF == 0` pattern), keep PHQst loud on all failure paths per the diag.rs:32-34 contract ("failure counters must stay loud"), preserve value names for existing triage recipes. Validation step (read PHQcall this-boot delta first, per CLAUDE.md rule 7) should be kept mandatory before and after.

**Lead-reviewer note.** Verified MODIFIED: the HwQueue path is cold BY CONSTRUCTION (no HW-scheduling caps advertised in query_adapter_info.rs:224-225), so this is hygiene/future-proofing at low severity — do it alongside R8 with the same pattern, not as its own campaign.


### R10. 16 registry writes per BAR paging op and per aperture map/unmap — unthrottled diag I/O on warm kernel paths

- **Category:** telemetry · **Reported by:** `kmd-alloc/paging-telemetry-flood`
- **Files:** `kmd_render/src/ddi/build_paging_buffer.rs`, `kmd_render/src/ddi/cpu_host_aperture.rs`, `kmd_render/src/ddi/create_allocation.rs`
- **Symbols:** `dump_bar_counters`, `dump_bar_ap_counters`, `create_one`, `record_alloc_event`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** dump_bar_counters (build_paging_buffer.rs:119-136, 16 record_named_bytes = 16 RtlWriteRegistryValue round-trips) runs at the tail of EVERY BAR content op (:519), so an eviction/commit burst multiplies registry I/O 16x per TRANSFER/FILL. dump_bar_ap_counters (cpu_host_aperture.rs:68-85, 16 writes) fires on every aperture map, unmap and every refusal (:177,:184,:192,:202,:209,:218,:224,:289). create_one performs ~20 diag::record registry writes per allocation (:708-711 four in a row, :1064-1082 seven more) plus record_alloc_event's three (:489-498), repeated per open (:1268). All are correctly PASSIVE-gated (no IRQL violation) — the cost is pure I/O latency on paging/allocation paths, versus the file's own throttle precedent: PRIMARY_COPY_SUBMIT_COUNT gates its breadcrumbs to `n == 1 || n % 600 == 0` (:414).

**Evidence.** build_paging_buffer.rs:519 `dump_bar_counters();` unconditional at the end of every content op; :119-136 sixteen `crate::diag::record_named_bytes(...)` calls; cpu_host_aperture.rs:68-85 sixteen writes, invoked at :177, :184-185, :192-193, :202-203, :209-210, :218, :224, :289; create_allocation.rs:708-711 four consecutive `crate::diag::record(0x0C11..0x0C32...)` per allocation and :1064-1082 seven more; throttle precedent :413-418 `if n == 1 || n % 600 == 0 { ...record... }` with the comment at :485-487 'writing the registry per frame would itself throttle the display path'.

**Recommendation.** Keep the atomics as the source of truth (already readable by symbol and via QUERY_STATS) and throttle the registry mirror: dump on first op, on any error-counter transition, and every Nth op (reuse the 1/600 pattern); collapse create_one's create-path breadcrumbs behind a single throttled sequence or a debug knob. Pure telemetry-frequency change; counters and their meanings untouched.

**Risk.** Diagnostic granularity for live bring-up drops between throttle ticks; mitigate with an existing-style registry knob (e.g. DiagVerbose) restoring per-op dumps, and always-dump-on-error-transition so any nonzero ChE*/PgE* still surfaces immediately.

**Atomic commit boundary.** One commit per file (build_paging_buffer, cpu_host_aperture, create_allocation) — independently landable.

**Validation.** Before/after timing per the measure-first rule: count registry writes per boot (S-ring volume) and time a Lock/eviction-heavy scenario; ensure error counters still appear in the registry when forced; standard visible-desktop gate.


### R11. Unconditional formatted file-I/O logging on per-frame DDI paths (sync tokens, allocation-backed copies)

- **Category:** telemetry · **Reported by:** `umd-forward-a/hot-path-unconditional-logging`
- **Merged duplicate reports (1):** `umd-forward-b/hot-path-log-io-uncapped-predicates` — Per-frame file I/O: log predicates like `rt0.0 != 0` / `alloc != 0` never saturate, and diagnostic COM GetDesc runs before the gate
- **Files:** `umd/src/forward.rs`, `umd/src/lib.rs`
- **Symbols:** `sync_token_cb`, `resource_copy`, `log_line`, `resource_map`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** log_line is an unbuffered file write under a process mutex (lib.rs:980-998; its own comment says per-line cost was "measurable on per-frame paths (PSC WS2)"). Yet sync_token_cb logs EVERY acquire/release with format! (2824-2828) — these fire per shared-surface access per frame in DWM; resource_copy logs every copy where either side has an allocation, forever (`n < 256 || dst_alloc != 0 || src_alloc != 0`, 2599-2604) — allocation-backed copies are exactly the per-frame shared-surface copies. Most other paths correctly use capped counters (n<128) or trace_line! (registry-gated, args unevaluated when off), so these two are outliers. create_resource's tex2d arm also emits up to 4 uncapped log blocks per large-texture create behind the thrice-repeated `>=1024 || >=576 || misc != 0` condition (1811-1857).

**Evidence.** umd/src/lib.rs:982-984 "the old open/append/close-per-line pattern cost a full CreateFile round trip on every logged DDI call — measurable on per-frame paths (PSC WS2)"; forward.rs:2824-2828 `let hr = cb(...); log_line(&format!("DDI sync_token: release={} ..."))` (unconditional); :2599-2604 `if n < 256 || dst_alloc != 0 || src_alloc != 0 { log_line(&format!("DDI resource_copy ..." )) }`; :1811, :1831, :1839, :1853 repeated `mip0.TexelWidth >= 1024 || mip0.TexelHeight >= 576 || misc != 0` log gates.

**Recommendation.** Convert sync_token_cb and the allocation-backed resource_copy arm to trace_line! plus a monotonic counter (retaining the first-N capped log for bring-up visibility); collapse the create_resource tex2d log blocks into one capped helper. Also unify the 22 hand-rolled `static *_LOG_COUNT + if n < cap` sites behind a capped_log! macro during the split. Land with before/after numbers per Operating Rule 7 (a one-boot count of suppressed lines suffices).

**Risk.** Low: purely diagnostic output volume; keep counters so no signal disappears silently. Confirm no current triage recipe greps the sync_token per-frame lines (ROADMAP tooling check).

**Atomic commit boundary.** One commit converting the two hot sites; the capped_log! consolidation rides the split commits.

**Validation.** Before/after count of log lines per minute at idle desktop and under DOOM/dxvk-tests; DComp cadence and present-gate wake latency unchanged or improved; desktop gate items.


### R12. Hot present path performs 3-4 std::env lookups per frame for diagnostics and embeds CPU-readback diag helpers in the DXGI module

- **Category:** telemetry · **Reported by:** `umd-forward-c/present-env-scan-telemetry`
- **Files:** `umd/src/forward.rs`
- **Symbols:** `maybe_log_present_readback`, `maybe_force_present_alpha_opaque`, `env_flag`, `dxgi_present`, `write_bgra32_bmp`
- **Verification:** **MODIFIED** (severity low) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** Every dxgi_present calls maybe_force_present_alpha_opaque + maybe_log_present_readback unconditionally (8412-8413); each begins with a fresh std::env::var_os scan (8103, 7914), and env_flag("HELIOS_PRESENT_OPTIMIZE_COMPOSITION") re-scans the environment at 8520 and again in the log at 8593 (also 9241/9264 in present1). env_flag (112-114) is an uncached var_os per call — on Windows that is an env-block walk + allocation, 3-4x per present at 63 fps, in the exact frame path the stage is measuring. The two helpers themselves (full staging copy + per-pixel CPU pass when enabled, BMP writer) are bring-up diagnostics living inside the present module.

**Evidence.** forward.rs:112-114 'fn env_flag(name: &str) -> bool { std::env::var_os(name).is_some() }'; 7914 'std::env::var_os("HELIOS_PRESENT_READBACK")' per present; 8103 'std::env::var_os("HELIOS_PRESENT_FORCE_OPAQUE")' per present; 8412-8413 both called unconditionally in dxgi_present; 8520 + 8593 double env_flag("HELIOS_PRESENT_OPTIMIZE_COMPOSITION") in one present.

**Recommendation.** Cache the three env flags in OnceLock at first use (matching the existing trace_enabled/present_gate_us read-once precedent, lib.rs:1006-1040 — process env for these debug knobs is fixed at spawn; state the semantic change in the commit). Move maybe_log_present_readback, maybe_force_present_alpha_opaque, write_bgra32_bmp into forward/diag.rs with a single early-out entry so the disabled cost is one branch, and the diag module is sealed out of the exact-primary code.

**Risk.** Caching changes behavior only for someone setting the env var mid-process (not a supported workflow; all registry knobs are already read-once). No other risk.

**Atomic commit boundary.** One commit: OnceLock caching + move of the two helpers and BMP writer.

**Validation.** Present-path perf counters before/after (present-gate telemetry, DComp fps unchanged or better); HELIOS_PRESENT_READBACK=1 launch still produces the 8 readback lines and BMP dumps.

**Verifier corrections (authoritative).** 1) Steady-state cost is 3 env scans per dxgi_present, not 4: the second OPTIMIZE_COMPOSITION lookup at forward.rs:8593 runs only when the throttled forensics log fires (`ordinal < 64 || (ordinal+1) % 512 == 0`, line 8583), and the present1 lookup at 9264 is inside trace_line! (lib.rs:1302-1308), which skips argument evaluation unless UmdTrace=1 and is capped at the first 64 calls — so "3-4x per present" should read "3 per present, 4th only on log/trace frames". 2) Perf framing must be downgraded to hygiene: std::env::var_os on Windows is a UTF-16 name allocation + GetEnvironmentVariableW env-block scan, sub-microsecond each; 3×63/s is noise against the ~0.48 ms frame-gate average, so validation should expect NO counter/fps delta ("unchanged", not "or better") — the real value is sealing CPU-readback bring-up diagnostics (full staging copy + per-pixel pass + BMP writer) out of the exact-primary present module and matching the codebase's read-once convention. 3) Safety of caching is now proven, not assumed: no set_var/SetEnvironmentVariable anywhere in umd/src; the three knobs appear only in forward.rs and ROADMAP.md (documented as launch-time diagnostics); all four existing hot-path knobs (trace_enabled lib.rs:1006, present_gate_us 1115, vehicle_flip_gate_us 1165, present_sync_publish_enabled 1264) are already OnceLock read-once. 4) Scope: cache exactly the three hot flags (READBACK, FORCE_OPAQUE, OPTIMIZE_COMPOSITION); leave HELIOS_PRESENT_DUMP_DIR (8006/8022) uncached — it is cold (reached only with READBACK set, first 8 presents). 5) Move is safe pure code motion: maybe_* helpers are called only from dxgi_present (8412-8413), write_bgra32_bmp only from the readback helper; no frozen-baseline component or kernel invariant is touched and the 10 ms condvar frame gate is untouched (timeout doctrine N/A — no timeout modified).

**Lead-reviewer note.** Verified MODIFIED: steady-state is 3 env scans per present (not 4) and the cost is sub-microsecond hygiene, not measurable perf — expect NO fps delta in validation. The real value is sealing the CPU-readback diagnostics out of the present module and matching the OnceLock read-once convention. Cache exactly READBACK/FORCE_OPAQUE/OPTIMIZE_COMPOSITION; leave HELIOS_PRESENT_DUMP_DIR uncached (cold).


### R13. Every SetVidPnSourceAddress performs an uncached ScanoutDiag registry read (RtlQueryRegistryValues) via rebind_if_forced — a diagnostic hook doing kernel registry I/O on the flip path

- **Category:** telemetry · **Reported by:** `xc-duplication/scanoutdiag-per-flip-registry`
- **Merged duplicate reports (2):** `xc-concurrency/scanout-diag-in-primary-path` — ScanoutDiag rebind hook does a synchronous registry read inside SetVidPnSourceAddress on every flip; diagnostic path enters the exact-primary DDI; `xc-unsafe/scanout-diag-per-flip-registry-read` — rebind_if_forced does per-flip registry reads inside the exact-primary path; diag scanout not type-sealed from production
- **Files:** `kmd_render/src/ddi/display.rs`, `kmd_render/src/ddi/scanout_diag.rs`, `kmd_render/src/diag.rs`, `kmd_render/src/adapter.rs`
- **Symbols:** `rebind_if_forced`, `diag_mode`, `read_config_dword`, `dxgkddi_set_vidpn_source_address`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 3 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** display.rs:747 calls scanout_diag::rebind_if_forced(adapter, 11) on every SetVidPnSourceAddress (per flip, ~60 Hz); rebind_if_forced's first statement is diag_mode() (scanout_diag.rs:479) which is diag::read_config_dword(b"ScanoutDiag", 0) (scanout_diag.rs:19) — an uncached RtlQueryRegistryValues round-trip per call (diag.rs:129-161, unlike DiagLevel which IS cached via diag::level(), diag.rs:35-47). So the production exact-primary flip path pays a synchronous kernel registry read solely to confirm diagnostics are off, and the diagnostic module sits inline in the frozen-baseline path.

**Evidence.** display.rs:747 'if crate::ddi::scanout_diag::rebind_if_forced(adapter, 11) {' inside dxgkddi_set_vidpn_source_address; scanout_diag.rs:479 'if diag_mode() < 2 || !adapter.display_half {'; scanout_diag.rs:19 'crate::diag::read_config_dword(b"ScanoutDiag", 0)'; diag.rs:129-131 'Read a REG_DWORD config value from the service key... PASSIVE_LEVEL only' with no caching (contrast diag.rs:39-47 level() which caches DiagLevel in an AtomicU32).

**Recommendation.** Cache ScanoutDiag once at StartDevice into AdapterContext (exactly like gdi_accel_mode/alloc_cached/display_half, adapter.rs:154-180); rebind_if_forced tests the cached field and early-outs with zero I/O. The established diag workflow (reg add + pnputil /restart-device re-runs StartDevice) is preserved. Optionally make the hook a sealed one-liner `adapter.scanout_diag_forced()` so the diagnostic module is not referenced from display.rs at all when the knob is 0.

**Risk.** Anyone relying on flipping ScanoutDiag live without an adapter restart loses that (undocumented) ability; note it in ROADMAP tooling. Behavior at knob=0 is bit-identical.

**Atomic commit boundary.** One commit: cached field + rebind_if_forced early-out.

**Validation.** Boot with ScanoutDiag absent: identical behavior, VpSA=1/ScSet=1, ScanoutDiag stays absent per the regression gate; with ScanoutDiag=2 + adapter restart the color-bar rebind still works (SdgReb/SdgRSet move).

**Lead-reviewer note.** Cache the ScanoutDiag knob at device start (or re-arm via an explicit escape) so SetVidPnSourceAddress performs zero registry I/O. The behavioral seal (diag resources unable to reach production publish state) is R45.



---

## Part II, Tranche 3 — File splits (pure moves)

Splits land as **pure code motion**: `git diff --color-moved` must show moves only, no logic edits — the handoff explicitly forbids semantic rewrites disguised as file moves. Any visibility widening (pub(super)/pub(crate)) and required `pub use` re-exports are named in the commit message. Splits come after tranches 1-2 so deleted code is never moved and telemetry edits don't conflict with moves; they come before dedup/typestate tranches so those diffs are small and reviewable.

**Regression-gate emphasis:** builds + format checks; adapter restart for UMD-only tranches; KMD splits need the version bump + reboot; visible desktop unchanged.

### R14. forward.rs (9533 lines) mixes 8+ responsibilities; split along the real boundaries already visible in lines 1-3300

- **Category:** split · **Reported by:** `umd-forward-a/split-forward-rs`
- **Merged duplicate reports (7):** `umd-forward-b/split-forward-scope-modules` — forward.rs 3200-6500 contains seven cohesive DDI subsystems that should become submodules; `umd-forward-c/forward-split-modules` — forward.rs 6400-9533 packs six unrelated responsibilities; split along selftest / input-layout / vehicle / DXGI-present / diag / install boundaries; `xc-errors/forward-rs-split` — umd/src/forward.rs is a 9,533-line module mixing ~14 unrelated responsibilities; split along real DDI-domain boundaries; `xc-duplication/split-umd-forward-rs` — umd/src/forward.rs is a 9533-line monolith (format tables, handle store, resources, views, shaders, tiled, queries, DXBC layout parser, selftests, vehicle present, DXGI DDIs, install tables) — split along its existing section boundaries; `xc-concurrency/forward-rs-split` — umd/src/forward.rs is 9533 lines spanning at least eight unrelated responsibilities, including selftests and an HLSL compiler in the production DLL; `xc-legacy/forward-rs-split` — umd/src/forward.rs is a 9,533-line module mixing ~12 unrelated responsibilities; `xc-unsafe/forward-rs-split` — umd/src/forward.rs is a 9533-line module with ~12 unrelated responsibilities
- **Files:** `umd/src/forward.rs`
- **Symbols:** `dxgi_bytes_per_pixel`, `store_resource`, `ensure_kmd_scanout_target`, `allocate_wddm_resource`, `create_resource`, `rtv_desc`, `resource_copy`, `flatten_stage_io_signatures`
- **Verification:** **MODIFIED** (severity medium) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** One module contains: (a) DXGI format tables (153-294); (b) DDI-handle/COM payload helpers ResourceState/RtvState/store/load/release (79-102, 366-582, 972-1132); (c) scanout/composition tracking incl. the legacy LINEAR copy path (584-874); (d) WDDM allocation private-data build/parse (104-151, 296-364, 1244-1621); (e) resource create/open/destroy/resolve (1623-2167); (f) RTV/DSV/clear views (2171-2571); (g) copy/map/sync-token/flush/discard (2575-3021); (h) shader create/flatten (3025-3900); beyond scope: pipeline state, draws, present/DXGI (7549+). 22 file-local *_LOG_COUNT statics (49-71) and per-function const redefinitions show the sections already act as separate modules.

**Evidence.** umd/src/forward.rs:1 "d3d10umddi device-funcs → D3D11 COM forwarders"; :49-71 (22 log-count statics); :153 `fn dxgi_to_d3dddi_format`; :366 "--- handle <-> COM helpers ---"; :685 `ensure_kmd_scanout_target`; :1244 `allocate_wddm_resource`; :1623 `create_resource`; :2573 "--- Copy / Map / Flush ---"; :3023 "--- Shaders ---"; wc -l = 9533.

**Recommendation.** Mechanically extract modules with pub(crate) visibility, no logic edits: format.rs (tables), handle.rs (payload store/load/release + ResourceState/RtvState), scanout.rs (584-874 + PresentSource block at 7549+), alloc.rs (meta/trailer + allocate_wddm_resource + finish_wddm_tex2d), resource.rs, view.rs, copy_sync.rs, shader.rs, and later state.rs/draw.rs/dxgi_present.rs. Keep the device_funcs table wiring in a thin forward.rs. This is the enabler for the typed refactors below.

**Risk.** Low if moves are pure (git diff --color-moved verifiable). Main hazard is accidentally changing visibility of a helper the cxx bridge or device_funcs table references.

**Atomic commit boundary.** One commit per extracted module (pure code motion), starting with format.rs and handle.rs which have no callers outside forward.rs.

**Validation.** Release UMD build + rustfmt diff check; git diff --color-moved=dimmed-zebra shows only moves; adapter restart; visible desktop, VpSA=1/ScSet=1, ScanoutDiag absent, cursor OK, DComp cadence ~63 fps, no new present-gate timeouts.

**Verifier corrections (authoritative).** 1) Static count: lines 49-71 contain 23 (not 22) *_LOG_COUNT statics, and 7 more exist mid-file (WDDM13_MARKER_LOG_COUNT:5755; ROTATE/BLT1/RESIDENCY/MPO/PRESENT1/DXGI13_RESERVED:8755-8760) — each must move with its owning section or the split breaks compilation. 2) "Legacy LINEAR copy path (584-874)" is compiled-in LIVE fallback: ensure_kmd_scanout_target is called at forward.rs:751 and :789, skipped only when direct_primary is set; the split may move it but must not treat "legacy" as removable. 3) Risk section should name the exact external callers: besides device_funcs.rs calling forward::install/install_11_1/install_wddm1_3/install_wddm2_1/install_dxgi/install_dxgi_1_1/install_dxgi_1_3 (device_funcs.rs:499-682), lib.rs (cxx bridge surface) calls forward::set_present_source (:211), forward::wait_last_present (:230), forward::take_present_result (:249), and forward::selftest_offscreen_clear/selftest_triangle/selftest_cb_readback/selftest_triangle_cb (:346-354); the "later dxgi_present.rs" step must re-export these from forward or update lib.rs paths in the same commit. 4) The thread_local! block at :7562 (PresentSource pending state) must stay a single instance in whichever module receives the present region.

**Verifier corrections for merged report `umd-forward-c/forward-split-modules` (MODIFIED, severity medium).** (1) Title range wrong: the packed span is 6235-9533, not 6400-9533 — the selftest block the finding itself assigns to forward/selftest.rs starts at 6235 (fn at 6239). (2) "+ compile_hlsl/make_tex2d" is redundant: they are already inside the cited 6235-6823 range (6335, 6367); verified selftest-only (zero callers outside 6402-6792). (3) lib.rs selftest call sites are 346-355 (four calls), export at 257; TEMPORARY comment 252-255 is correct. (4) "Sealed: nothing in the DDI flow may call it" is convention-only, not compiler-enforced — the four selftest fns must stay pub (or pub(crate) + re-export) for the helios_umd_selftest export, so the seal is a review rule, not a static guarantee. (5) Visibility churn is smaller than implied: Rust child modules see ancestor privates, so forward/install.rs needs NO visibility changes for the hundreds of handlers remaining in the parent module (lines 1-6234); only sibling-module items (dxgi_present 8310, dxgi_present_mpo 8993, dxgi_present1 9123, moved blend handlers) need pub(super). (6) Missing requirement: pub-use re-exports in forward/mod.rs to keep external paths byte-stable — device_funcs.rs calls crate::forward::install/install_11_1/install_wddm1_3/install_wddm2_1/install_dxgi/install_dxgi_1_1/install_dxgi_1_3 at 9 sites, and lib.rs calls forward::set_present_source/wait_last_present/take_present_result (211/230/249) plus forward::selftest_* (346-355). (7) The diag statics PRESENT_READBACK_LOG_COUNT/PRESENT_FORCE_OPAQUE_LOG_COUNT live in the file-header static block (lines 70-71), not inside 7913-8223 — they must move to forward/diag.rs with their fns. (8) Timeout doctrine note the finding omits: no timeout semantics are touched; the bounded flip/kwait waits (flip_wait_setup 7618, wait_last_present) move verbatim and the 10 ms condvar frame gate contract must remain byte-identical.

**Lead-reviewer note.** Eight reports — the top structural item in the review. Execute as the UNION of the three range reviewers' boundary proposals (a: handle/format/resource/view/shader seams at the file's own banners; b: seven DDI subsystems in 3200-6500; c: selftest/input-layout/vehicle/DXGI-present/diag/install from 6235). Both verified correction sets below are mandatory reading: forward/mod.rs pub-use re-exports keep device_funcs.rs and lib.rs (cxx bridge) paths byte-stable; all 30 *_LOG_COUNT statics (23 in the header block + 7 mid-file) move with their owning sections; the thread_local PRESENT_SOURCE block stays a single instance in the present module; child modules see ancestor privates, so visibility churn is far smaller than it looks.


### R15. lib.rs mixes seven responsibilities (adapter DDI, device creation, caps, knobs, logging, vehicle exports, selftest) under a stale bring-up module doc

- **Category:** split · **Reported by:** `umd-core/split-lib-rs`
- **Files:** `umd/src/lib.rs`
- **Symbols:** `OpenAdapter10`, `OpenAdapter10_2`, `create_device`, `get_caps`, `log_line`, `trace_enabled`, `helios_umd_set_present_source`, `helios_umd_selftest`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** 1352-line lib.rs contains: DLL exports + adapter open/close/versions (374-643, 799-834), CreateDevice device-lifecycle logic (645-797), the full adapter caps policy including FL gating history (836-959), six registry knob readers (~300 lines, 1006-1298), logging infrastructure + trace macro + module-path forensics (961-999, 1302-1352), in-process vehicle exports (191-250), dead D3D12 scaffolding, and the selftest harness. The module doc (lib.rs:1-7) still describes the pre-DXVK bring-up world: 'This is not a D3D implementation yet... device creation still fails explicitly' — false for over a year of project history and actively misleading for the refactor phase.

**Evidence.** lib.rs:1-7 stale doc: '//! This is not a D3D implementation yet... Until the DXVK/VKD3D-backed adapter/device path exists, device creation still fails explicitly.' vs lib.rs:794 '  CreateDevice -> S_OK (DXVK device + D3D11 funcs table installed)'. Responsibility spans: vehicle exports 202-250; selftest 257-371; adapter DDI 374-643; create_device 645-797; caps 836-959; logging 961-999 + 1302-1352; knobs 1006-1298.

**Recommendation.** After the removals/dedups land, split along real boundaries: adapter.rs (OpenAdapter*/close/versions/caps), device_create.rs (create_device + HeliosDevice construction), knobs.rs (finding centralize-config-knobs), logging.rs (umd_log_path/log_line/trace_line/log_self_module_path), vehicle_api.rs (the three helios_umd_* exports, which are pure forward:: shims). lib.rs keeps only exports re-export + crate wiring. Rewrite the module doc to describe the actual PSC-stage architecture. Pure code motion; no signature changes.

**Risk.** Low if performed as motion-only commits after the deletions; the #[no_mangle] exports must stay in linkable positions (any module works, but verify the export table).

**Dependencies.** R1 (remove-dead-d3d12-scaffolding); R1 (remove-temporary-selftest); R27 (centralize-config-knobs); R65 (adopt-bindgen-adapter-structs)

**Atomic commit boundary.** One motion-only commit per extracted module (logging, adapter, device_create, vehicle_api), each independently buildable; doc rewrite in its own commit.

**Validation.** Release UMD build; export table identical (OpenAdapter10/10_2/12 + three helios_umd_* exports); git diff --color-moved confirms motion-only; visible desktop; VpSA=1/ScSet=1; DComp cadence ~63fps.

**Lead-reviewer note.** After R1 (selftest/D3D12 deletion) and coordinated with R14 — the two files trade symbols via the cxx bridge surface.


### R16. gpu.rs mixes five unrelated responsibilities (bring-up, queue machinery, fence tables, WDDM FIFO, blob/window tables) in one 2421-line file

- **Category:** split · **Reported by:** `kmd-transport-gpu/split-gpu-rs`
- **Merged duplicate reports (2):** `xc-duplication/split-virtio-gpu-rs` — kmd_render/src/virtio/gpu.rs mixes PCI bring-up, async queue machinery, resource/blob tables, window allocator, fence tables and WDDM FIFO in one 2421-line file; `xc-legacy/virtio-gpu-split` — virtio/gpu.rs is a 2,421-line god-module: PCI discovery, ring transport, DMA pool, four bounded tables, fence-event table, and WDDM pending FIFO in one VirtioGpu struct
- **Files:** `kmd_render/src/virtio/gpu.rs`
- **Symbols:** `VirtioGpu`, `InFlight`, `BlobSlot`, `WddmPending`, `FenceEventEntry`, `scan_host_visible_window`, `alloc_window_range`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 3 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** gpu.rs contains (a) ~150 lines of telemetry statics (83-212), (b) PCI capability scanning helpers (213-356), (c) device bring-up `init` (831-1024), (d) the async in-flight queue machinery: enqueue_sync/enqueue_async_control/enqueue_async_submit/drain_used/parked-reap/DMA pool (1026-1575), (e) fence-waiter + usermode fence-event tables (1577-1685), (f) the WDDM pending FIFO + scanout refresh watermark (1686-1817), and (g) blob/resource/context tracking tables plus the host-visible window offset allocator (358-468, 1819-2391). Sections (f) and (g) never touch `PciTransport`/`VirtQueue` at all — they are plain tables that merely share the device spinlock — yet they live as methods on the transport struct.

**Evidence.** gpu.rs:1 (file is 2421 lines). Section banners: gpu.rs:83 "── C3/M3.4 async-transport telemetry", gpu.rs:213 "── Host-visible window discovery (Gate 5a Stage 2)", gpu.rs:358 "── Host-visible blob mapping", gpu.rs:470 "── C3/M3.4 async submission machinery", gpu.rs:1577 "── Wire-fence table (WAIT_FENCE)", gpu.rs:1686 "── WDDM pending-fence FIFO", gpu.rs:1819 "── Table helpers". gpu.rs:2325-2368 `free_window_range` (pure offset arithmetic, no device access) is a method on the same struct that owns `PciTransport` (gpu.rs:736-738).

**Recommendation.** Mechanical split along the existing section markers into sibling modules composed inside `VirtioGpu`: `virtio/caps.rs` (cap walks + HostVisibleWindow), `virtio/queue.rs` (transport + InFlight machinery + DMA pool), `virtio/fence.rs` (FenceWaiter/FenceEventEntry tables + WddmPending FIFO + scanout watermark), `virtio/tables.rs` (BlobSlot/resource/context tables + WindowRange allocator, as structs `BlobTable`/`WindowAllocator` owned by VirtioGpu), `virtio/stats.rs` (the atomics + bump_high_water). Keep the public method surface on VirtioGpu delegating, so ctrl.rs/venus.rs/ddi callers are untouched. No semantic changes; pure moves plus struct composition.

**Risk.** Low — moves only. Main hazard is accidentally changing visibility (several consts are exported via accessor fns `max_blobs()`/`max_resources()` at 206-211) or breaking the `pub(crate)`/`pub` boundaries ctrl.rs and the escape handlers rely on.

**Atomic commit boundary.** One commit per extracted module, starting with the fully self-contained tables/allocator (no transport coupling), ending with the queue machinery.

**Validation.** KMD builds (win_build_kmd) with zero diff in behavior; boot to visible desktop; VpSA=1/ScSet=1, ScanoutDiag absent; DComp cadence ~63 fps; CTRL_TIMEOUT_COUNT/DRAIN_BAD_TOKEN stay 0; QUERY_STATS returns identical table stats for an identical workload.


### R17. 2861-line venus.rs mixes ring transport, wire codec, Vulkan client, bring-up sequence, copy orchestration, and diagnostic-only paths

- **Category:** split · **Reported by:** `kmd-venus/venus-split`
- **Merged duplicate reports (1):** `xc-concurrency/venus-rs-split` — kmd venus.rs (2861 lines) mixes the wire codec, ring transport, Vulkan bring-up, scanout-image construction and prepared-copy machinery in one module
- **Files:** `kmd_render/src/virtio/venus.rs`
- **Symbols:** `KernelMap`, `Writer`, `ReplyReader`, `VenusClient`, `PreparedImageCopy`, `allocate_host_visible_blob`, `gpu_clear_scanout_image`, `allocate_scanout_image_blob`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** One file holds six real responsibilities: (1) BAR mapping + vn_ring producer protocol with memory-ordering contracts (KernelMap + publish/notify/wait, 261-715); (2) the LE wire codec (Writer/ReplyReader, 359-483); (3) per-command Vulkan client calls (717-1710); (4) PreparedImageCopy scanout-copy orchestration incl. layout/ownership protocol (1711-2170); (5) diagnostic-only entry points compiled into the production client (gpu_clear_scanout_image 2172-2250, allocate_scanout_image_blob 2252-2324) alongside the production fallback allocator misdocumented as 'Diagnostic' (2326-2328); (6) the 410-line bring-up sequence with the device-extension tier ladder (2449-2861). The codec cannot be unit-tested because it is entangled with wdk-sys via KernelMap in the same module.

**Evidence.** wc -l = 2861. Boundaries: venus.rs:261 'struct KernelMap'; :364 'struct Writer'; :430 'struct ReplyReader'; :491 'pub struct VenusClient'; :234 'pub struct PreparedImageCopy'; :2172-2174 '/// Diagnostic-only GPU fill for scanout images. The image is KMD-owned and never enters the production present path'; :2252-2253 '/// Diagnostic-only scanout allocation'; :2326-2327 '/// Diagnostic scanout allocation matching the working Linux probe' — yet display.rs:76-77 calls it from production_linear_scanout; :2459 'pub fn allocate_host_visible_blob' runs to :2861.

**Recommendation.** Split along these boundaries into kmd_render/src/virtio/venus/: ring.rs (KernelMap, ring produce/wait, fatal latch), codec.rs (constants + Writer/ReplyReader — pure, no wdk deps, host-testable), client.rs (VenusClient command methods), bringup.rs (allocate_host_visible_blob + ext ladder), scanout_copy.rs (PreparedImageCopy machinery), diag.rs (gpu_clear_scanout_image, allocate_scanout_image_blob) exposed via a sealed diagnostic sub-API so production code cannot call them (see scanout-diag-mode-enum). Pure code motion: no signature or behavior changes beyond visibility, so the diff is verifiable as moves.

**Risk.** Moves disguising edits — keep each commit move-only and diff-review with --color-moved; visibility tightening may reveal cross-module uses to resolve explicitly.

**Dependencies.** R23 (ring-call-dedup)

**Atomic commit boundary.** One move-only commit per extracted module (ring, codec, scanout_copy, bringup, diag), in that order.

**Validation.** git diff --color-moved=zebra shows only moves; KMD builds; new host-side cfg(test) target for codec.rs compiles; full visual regression gate (visible desktop, cursor, 63 fps cadence, VpSA=1/ScSet=1, ScanoutDiag absent).

**Lead-reviewer note.** After R6/R7 delete the dead roundtrip + diagnostic builders so they are not moved.


### R18. adapter.rs is a 1063-line god-object: lock core, scanout publication, HPD thread lifecycle, vsync timer, RAM allocator, and a 90-line inline telemetry dump

- **Category:** split · **Reported by:** `kmd-core/adapter-split`
- **Files:** `kmd_render/src/adapter.rs`
- **Symbols:** `AdapterContext`, `queue_active_scanout_refresh`, `init_hpd`, `stop_hpd`, `init_vsync`, `cancel_vsync`, `alloc_contiguous_ram`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** One file/type mixes six responsibilities: (1) lock/guard core (with_virtio/with_venus_client/with_wddm_notify_lock, :980-1039); (2) ~25 scanout-identity atomics + publish/forget/queue methods (:196-813); (3) HPD system-thread create/signal/join (:390-483) incl. the PsThreadType extern (:22-28); (4) vsync KTIMER/KDPC arm/cancel (:815-871); (5) contiguous-RAM allocation (:932-962); (6) two modulo-gated telemetry blocks totaling ~90 lines inline in the refresh function (:722-811) that reach into five other modules' counters. The struct itself is 200 lines of fields (:80-284). Every future change collides in this file, and the frozen refresh pipeline's core logic (the ~40 lines of real gating in queue_active_scanout_refresh) is buried in telemetry.

**Evidence.** adapter.rs:80-284 = 200-line struct; :722-811 two telemetry blocks ("if n == 1 || (n % 600) == 0" / "if n == 1 || (n % 16) == 0") writing ~25 named registry values from inside queue_active_scanout_refresh, pulling counters from crate::ddi::interrupt, crate::virtio::gpu, crate::ddi (VIDPN_SOURCE_ADDRESS_COUNT, DMA_STALE_SKIP_COUNT); :390-483 HPD thread create/join incl. ObReferenceObjectByHandle; :815-871 vsync timer; :932-962 MmAllocateContiguousMemory helper; :22-28 PsThreadType extern block.

**Recommendation.** Split along the real boundaries into adapter/{mod.rs (context + locks + WddmNotifyGuard), scanout.rs (identity state + refresh queueing), hpd.rs (worker lifecycle: thread handle + stop flag + event), vsync.rs (timer/DPC arm/cancel)}; move alloc_contiguous_ram next to its PagingRam type; extract both telemetry blocks into a diag-side `fn snapshot_refresh_telemetry(n, adapter)` so the refresh function reads as its actual gate logic. Pure moves — no semantic edits, no reordering of atomics.

**Risk.** Low (mechanical), but a move disguising an edit is the classic failure mode — enforce move-only diffs per commit and keep pub(crate) surfaces identical.

**Dependencies.** D20 (adapter-lifecycle-aliasing)

**Atomic commit boundary.** One commit per extracted module (scanout, hpd, vsync, telemetry extraction), each independently buildable.

**Validation.** cargo build + fmt/diff check showing move-only hunks; boot to visible desktop; VpSA=1/ScSet=1; RfCnt/RfRid cadence identical to baseline boot; 63 fps DComp.


### R19. display.rs interleaves five unrelated responsibilities (present DDI + its diagnostics, scanout binding, LINEAR fallback allocation, VidPn thunks, cursor/system-display stubs)

- **Category:** split · **Reported by:** `kmd-display/display-file-split`
- **Files:** `kmd_render/src/ddi/display.rs`
- **Symbols:** `dxgkddi_present`, `dxgkddi_set_vidpn_source_address`, `production_linear_scanout`, `issue_present_scanout`, `dxgkddi_set_pointer_position`, `dxgkddi_system_display_enable`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** 966 lines spanning: the LINEAR-fallback allocator (production_linear_scanout, 45-124); present private-data decode + publication (126-224); present atomics/dump (33-43, 226-248); dxgkddi_present with its embedded feasibility tracer (250-448); pointer DDIs (450-484); the display_half gate helper (491-494); seven VidPn thunk DDIs (496-627, 842-897); the scanout bind path SVSA (629-840); scanline/system-display/stop-device stubs (899-954); and exchange_pre_start_info (956-966), which is not a display DDI at all. Real boundaries: present handling, exact-primary scanout binding (SVSA + issue_present_scanout + production_linear_scanout — the trio sharing the validated descriptor), VidPn thunk layer (belongs beside vidpn.rs), and no-op/stub DDIs.

**Evidence.** display.rs:1-6 module doc claims only 'Display/VidPn DDIs' scope; 45 'fn production_linear_scanout' (allocator), 250 'pub unsafe extern "C" fn dxgkddi_present', 629 'dxgkddi_set_vidpn_source_address' (211-line body), 956-966 'dxgkddi_exchange_pre_start_info' (adapter start concern, records 0x0E00_0001), 922-954 system-display/stop-device stubs — five concerns in one 966-line file.

**Recommendation.** Split along those seams: ddi/present.rs (present DDI, private-data decode, counters/dump), ddi/scanout_bind.rs (SVSA, issue_present_scanout, production_linear_scanout, rec_named removal per telemetry finding), VidPn thunks merged into or beside vidpn.rs (sharing the legalize boundary), ddi/display_stubs.rs (pointer/scanline/system-display/monitor-link), and move exchange_pre_start_info to start_device.rs. Pure file moves with re-exports through ddi/mod.rs — no semantic edits in the same commit (handoff: 'Avoid semantic rewrites disguised as file moves').

**Risk.** Low if strictly mechanical; the DDI function-table wiring in mod.rs/add_device must keep every extern C symbol.

**Dependencies.** D1 (svsa-raised-irql-registry-writes)

**Atomic commit boundary.** One move-only commit (defect fix lands first so it is not entangled with moves).

**Validation.** KMD builds + clean diff check (moves only); reboot; adapter starts Code 0; VpSA=1/ScSet=1; visible desktop.


### R20. submit_command.rs mixes the DIRQL notification core, submission DDIs, three render-record DDIs with present-marker sniffing, TDR DDIs, and the TDR debug report

- **Category:** split · **Reported by:** `kmd-submit/split-submit-command`
- **Files:** `kmd_render/src/ddi/submit_command.rs`
- **Symbols:** `notify_at_dirql`, `signal_dma_completed_locked`, `signal_crtc_vsync`, `dxgkddi_render`, `dxgkddi_render_gdi`, `dxgkddi_collect_dbg_info`, `diag_dump_engine_atomics`
- **Verification:** **MODIFIED** (severity low) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** 850 lines, five responsibilities. The DIRQL notification core (NotifyDmaCompletedCtx, notify_at_dirql, signal_dma_completed[_locked], signal_dma_preempted_locked, signal_crtc_vsync) is consumed from interrupt.rs (DPC fence retirement) and start_device.rs (vsync worker) — it is adapter-wide infrastructure, not a submission DDI. dxgkddi_render additionally embeds the present-marker sniffing (two struct probes + nested raw-pointer ladders) and the scanout publish; dxgkddi_collect_dbg_info's 35-entry counter report and diag_dump_engine_atomics are diagnostics. File-boundary blur is why the fence-ordering contract (guard + watermark) is spread across 4 files.

**Evidence.** submit_command.rs:97-253 notification core (notify_at_dirql, signal_* family); external consumers interrupt.rs:67 `super::submit_command::signal_dma_completed_locked(` and start_device.rs:306 `crate::ddi::submit_command::signal_crtc_vsync(`; :446-587 dxgkddi_render incl. :468-541 marker sniffing; :756-850 collect_dbg_info 35-u32 report; :53-93 diag_dump_engine_atomics.

**Recommendation.** Split along consumer boundaries: (1) `ddi/notify.rs` — the DIRQL notification core, keeping the WddmNotifyGuard-typed signal_*_locked functions together with the wrap-around forward-fence rule; (2) `ddi/render_record.rs` — dxgkddi_render/render_km/render_gdi + present-marker parsing; (3) submit/preempt/reset/query-fence stay in submit_command.rs; (4) collect_dbg_info + diag_dump_engine_atomics into the diagnostics module. Pure moves, no semantic edits (handoff: 'avoid semantic rewrites disguised as file moves').

**Risk.** Low if moves are mechanical; the danger is drive-by edits to the fence-forward/stale-skip logic during the move. pub(crate) surface changes only.

**Dependencies.** R5 (dead-wait-gpu-refresh-path); R24 (dedup-submit-and-record-tails); R43 (scanout-request-descriptor)

**Atomic commit boundary.** Two commits: (a) move notification core + diagnostics, (b) move render-record DDIs.

**Validation.** Builds; git diff --color-moved verifies move-only; boot to desktop; DMA_STALE_SKIP/WDDM_FENCE_FROM_DPC behave as baseline; preempt path exercised by a TDR-free gaming run; 63 fps cadence.

**Verifier corrections (authoritative).** 1) Drop/reword "file-boundary blur is why the fence-ordering contract (guard + watermark) is spread across 4 files": the split does NOT reduce that spread — WddmNotifyGuard stays in adapter.rs, the note_wddm_submission watermark in virtio/gpu.rs, the DPC consumer in interrupt.rs, and note_and_maybe_signal stays in submit_command.rs; the contract still spans 4-5 files after the split. The split only co-locates the guard-typed signal family. 2) Not a visibility-neutral pure move: signal_dma_preempted_locked (:238) is private and its only caller dxgkddi_preempt_command (:402) stays in submit_command.rs, so moving the signal_*_locked family to notify.rs requires widening it to pub(crate) (consistent with the finding's own risk note, but must be called out in the move commit). 3) start_device.rs:306's consumer is vsync_dpc_routine, a KTIMER DPC at DISPATCH_LEVEL (start_device.rs:277-309), not a "vsync worker" thread. 4) dxgkddi_patch (:722) and dxgkddi_query_current_fence (:734) placement: query-fence is named as staying; patch is unassigned — it should stay in submit_command.rs (it is the no-op partner of the submit path). 5) If diag_dump_engine_atomics and dxgkddi_collect_dbg_info land in one diagnostics module, preserve both IRQL-contract doc comments verbatim: diag_dump_engine_atomics is PASSIVE-only (diag::record) while collect_dbg_info is any-IRQL/no-locks/no-allocation; co-location is safe only because Rust kernel code here has no per-file section pageability (verified: no link_section attributes in the crate).

**Lead-reviewer note.** Verified MODIFIED — apply the corrections: the split does NOT reduce the 4-file fence-contract spread (drop that claim from the commit rationale); signal_dma_preempted_locked needs pub(crate); preserve both IRQL-contract doc comments verbatim if the diagnostics co-locate.


### R21. Prepared-scanout-copy engine (7-atomic hand-rolled publish protocol) lives inside create_allocation.rs with ordering enforced by comment

- **Category:** split · **Reported by:** `kmd-alloc/scanout-copy-extraction`
- **Files:** `kmd_render/src/ddi/create_allocation.rs`
- **Symbols:** `cached_prepared_copy`, `publish_prepared_copy`, `clear_prepared_copy`, `submit_primary_scanout_copy`, `AllocationContext`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** ~160 lines of display-fallback machinery (274-430) plus seven per-allocation atomics (scanout_copy_* at 61-67) sit in the allocation-lifecycle file. The cache is one logical struct smeared over 7 atomics with a comment-only protocol: 'command_buffer_id is the publish word. A reader that acquires a nonzero command id sees one coherent immutable snapshot' (300-301) — Relaxed stores fenced by one Release/Acquire pair. In practice every mutation happens under the PASSIVE venus mutex (with_venus_client, adapter.rs:918-930) except destroy_allocation_ctx's initial read (621) and clear (634), whose safety rests on the unwritten assumption that dxgkrnl never races DestroyAllocation against SetVidPnSourceAddress on the same allocation.

**Evidence.** create_allocation.rs:61-67 seven `scanout_copy_*` AtomicU64/U32 fields; :299-314 publish_prepared_copy with :300-301 `// command_buffer_id is the publish word...` and five Relaxed stores sealed by one Release (:312-313); :277-297 cached_prepared_copy reconstructs the struct from Relaxed loads after one Acquire; destroy read outside the venus mutex :621 `if let Some(copy) = cached_prepared_copy(&ctx)` then mutex-entered drain :623-627; submit path entirely inside `adapter.with_venus_client(|client| { ... })` :365-407.

**Recommendation.** Extract to ddi/scanout_copy.rs (or under display/): move the PreparedImageCopy cache + submit/destroy-drain logic out of create_allocation.rs, and replace the 7 atomics + last_fence with a single small-lock-guarded (or venus-mutex-adjacent) `Option<PreparedCopyState>` so coherence is structural, not comment-ordered. AllocationContext keeps only an opaque `scanout_copy: ScanoutCopySlot` field. No protocol/order changes: same prepare-once, submit-per-frame, drain-before-teardown sequence and the same CpCpy/CpFnc/CpDrn breadcrumbs.

**Risk.** The destroy drain ('leak rather than use-after-free' arm, 621-636) must keep its exact semantics; the fallback copy path is exercised pre-logon, so a reboot is required to prove it. Guarded-slot locking must not be taken at raised IRQL (both call sites are PASSIVE today).

**Atomic commit boundary.** Commit 1: file move + slot type with identical atomics. Commit 2: collapse atomics into the guarded Option. (Lands before alloc-backing-enum.)

**Validation.** Reboot; pre-logon display (fallback copy path) works; after logon direct path unchanged (ScCpy=2 zero-copy marker); CpCpy/CpDrn sequences identical; no new gate timeouts; visible desktop + cursor + 63fps gate.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Snapshot coherence depends on (a) the publish-word ordering comment and (b) the implicit venus-mutex serialization plus dxgkrnl's destroy-vs-flip exclusion; a future reader touching a scanout_copy_* atomic outside the mutex or before the Acquire silently reads a torn snapshot.
1. **Compile-time representation:** Single `Option<PreparedCopyState>` behind a guard type; readers get `&PreparedCopyState` only through the guard, making torn reads unrepresentable.
1. **Smallest atomic migration:** New module + AllocationContext field swap in one commit; call sites (display.rs:819, destroy path) mechanical.
1. **Remaining `unsafe` preconditions:** The dxgkrnl guarantee that SetVidPnSourceAddress never races DestroyAllocation for the same hAllocation stays an FFI contract; the guard makes the driver-internal accesses safe regardless.
1. **Regression test proving preserved behavior:** Reboot cycle exercising fallback (pre-logon) and direct (post-logon) paths with identical CpCpy/CpFnc/CpDrn/ScCpy counter sequences and no DEVICE_NOT_READY regressions.

**Lead-reviewer note.** Extraction only here; the publish-protocol typing it exposes is R44.


### R22. diag.rs mixes breadcrumb ring, production counters, and registry-knob reading; knob names/defaults are scattered byte literals with a silent 14-char truncation trap

- **Category:** split · **Reported by:** `kmd-core/config-knob-module`
- **Files:** `kmd_render/src/diag.rs`, `kmd_render/src/ddi/start_device.rs`, `kmd_render/src/ddi/query_adapter_info.rs`
- **Symbols:** `read_config_dword`, `record_named_bytes`, `level`, `setup_bar_segment`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** diag.rs still opens with "TEMPORARY post-start bring-up tracer (remove once Code 43 / AddAdapter clears)" (:1) — Code 43 cleared 2026-07-05; the module is now permanent infrastructure hosting three unrelated things: the DiagLevel-gated S-ring, the named-counter API (CLAUDE.md-mandated production telemetry), and `read_config_dword` (configuration, not diagnostics). Knob definitions are stringly byte literals with magic defaults at each call site: BarSegMode default 10 (start_device.rs:52), GdiAccelMode 1 / AllocCached 1 / DisplayHalf 0 (:126-128), DirectFlipCaps 0 (query_adapter_info.rs:451), CrossAdaptCaps 0 (:470), BarSegFlags 0x1C / BarSegBaseMB 0 (:596-597), ScanoutDiag in venus.rs, DiagLevel in diag.rs:44. Names >14 chars are silently truncated (diag.rs:105 ".min(14)"; :130) — the trap is documented as a per-knob footnote: "NB: `read_config_dword` truncates names to 14 chars — this name is exactly 14" (query_adapter_info.rs:468).

**Evidence.** diag.rs:1 "TEMPORARY post-start bring-up tracer (remove once Code 43 / AddAdapter clears)."; :104-105 "let n = name.len().min(14);" silent truncation; :129-137 read_config_dword living in the diagnostics module; query_adapter_info.rs:468 "NB: `read_config_dword` truncates names to 14 chars — this name is exactly 14." — invariant carried by a comment at one call site; scattered defaults: start_device.rs:52 (10), :126-128 (1,1,0), query_adapter_info.rs:596 (0x1C).

**Recommendation.** Create `config.rs`: `pub struct Knob { name: &'static [u8], default: u32 }` with a const constructor that compile-time asserts name length <= 14, and one table declaring every knob (name, default, doc) — the single inventory ROADMAP.md points at. Call sites become `config::BAR_SEG_MODE.read()`. diag.rs keeps only the ring + named counters and drops the stale TEMPORARY header. No default values change.

**Risk.** Minimal; a transcription error in a default would silently change a knob — validation diff must show each (name, default) pair moved verbatim.

**Atomic commit boundary.** One commit: introduce config.rs + migrate all read_config_dword callers; a follow-up trims diag.rs's header/doc.

**Validation.** Builds; boot with empty service key → BarM=10/GdiM=1/AlcC=1/DspH read-back records unchanged; with DisplayHalf=1 the production activation still occurs (VpSA=1/ScSet=1).

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Knob-name length <= 14 and name/default agreement across call sites are unchecked; a 15-char name silently reads a different value (wrong knob), and two call sites can disagree on a default.
1. **Compile-time representation:** `Knob` consts with a const-assert on name length; one declaration per knob makes duplicate/divergent defaults unrepresentable.
1. **Smallest atomic migration:** Single commit moving all knobs; compiler finds every caller.
1. **Remaining `unsafe` preconditions:** Registry value type/content remain runtime data (see read-config-dword-typecheck); the table cannot prove the guest registry contains sane values.
1. **Regression test proving preserved behavior:** Boot with empty service key: all knob read-back diag records (BarM/GdiM/AlcC/DspH/BarF/BarB) match the baseline boot.

**Lead-reviewer note.** Creates the natural home for the R51 PASSIVE proof token and fixes the 14-char knob-name truncation trap as a named, tested constraint.



---

## Part II, Tranche 4 — Dedup and consolidation

Consolidations inside the post-split structure. Each entry names all duplicate sites and one consolidation point. Where duplicates have *divergent* behavior, the divergence is documented in the entry (several verifier corrections exist precisely because a naive unification would change behavior — read them before writing code).

**Regression-gate emphasis:** byte-identical outputs where claimed (wire bytes, NTSTATUS/HRESULT per path); the affected DDI's counters unchanged in steady state.

### R23. ~22 hand-rolled encode/reply-validate sequences and duplicated command encoders; reply/no-reply contract enforced only by comments

- **Category:** dedup · **Reported by:** `kmd-venus/ring-call-dedup`
- **Merged duplicate reports (1):** `xc-duplication/venus-client-dedup` — venus.rs repeats the reply-header validation block ~19 times and has three near-identical VkImageCreateInfo encoders — extract expect_reply() and a parameterized image-create builder
- **Files:** `kmd_render/src/virtio/venus.rs`
- **Symbols:** `VenusClient::ring_command_reply`, `ReplyReader`, `create_scanout_image`, `create_linear_scanout_image`, `create_optimal_bgra_source_alias`, `allocate_memory_blob`, `allocate_dedicated_image_memory`, `allocate_export_image_memory`, `allocate_imported_resource_memory`, `queue_submit_command_buffer`, `submit_prepared_image_copy`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Every command with a reply repeats the same 5-step boilerplate by hand: build Writer, ring_command_reply, construct ReplyReader over reply_map, check echoed cmd id, check VkResult — ~22 copies, each with hand-assigned diag codes (which already collided, see diag-code-collisions). Three near-identical vkCreateImage encoders (lines 839-897, 899-958, 964-1042) differ only in pNext chain/tiling/usage/layout; four vkAllocateMemory encoders (722-787, 1072-1112, 1114-1153, 1157-1195) differ only in the pNext arm. vkQueueSubmit's VkSubmitInfo body is encoded twice: ring path lines 1666-1682 vs direct fire-and-forget path lines 2108-2123, where the essential invariant 'Keeping VK_COMMAND_GENERATE_REPLY_BIT_EXT clear is essential' (line 2106) is enforced only by a comment and a literal 0.

**Evidence.** venus.rs:752-762 'let mut r = ReplyReader::new(&self.reply_map); let cmd = r.read_i32()?; if cmd as u32 != CMD_ALLOCATE_MEMORY { diag(0x00F6)...' — pattern repeats at 881-895, 937-957, 1025-1041, 1056-1069, 1101-1111, 1137-1152, 1182-1194, 1211-1221, 1241-1256, 1277-1289, 1307-1322, 1357-1376, 1407-1417, 1546-1556, 1573-1592, 1610-1621, 1652-1657, 1684-1695. Dup VkSubmitInfo: 1666-1682 vs 2108-2123. Comment-only contract at 2106-2107: 'Keeping VK_COMMAND_GENERATE_REPLY_BIT_EXT clear is essential: there is no reply-shmem transaction on this direct, fire-and-forget path.'

**Recommendation.** Add one `ring_call(adapter, cmd_type, encode: impl FnOnce(&mut Writer)) -> Result<ReplyReader, VirtioError>` that sends, waits, validates the echoed cmd header and VkResult once (diag parameterized by cmd id), and returns the positioned reader — so a ReplyReader can only exist for a validated reply. Consolidate image-create and memory-allocate encoders into one parametrized encoder each (pNext arm as an enum: None/ExportDmaBuf/ExportDedicated{image}/ImportResource{res,size}/ModifierList). Give the direct path a distinct `DirectStream` type whose constructor cannot set CMD_FLAG_GENERATE_REPLY, and make ctrl::submit_venus_async_scanout accept only it, turning the comment-only contract into a type. Encoded bytes must remain identical.

**Risk.** Encoder consolidation could subtly change wire bytes (venus decoder is unforgiving; NVIDIA host poison history). Mitigate with golden-byte tests comparing new encoders against captured current output before the swap.

**Atomic commit boundary.** Three commits: (1) ring_call helper + mechanical migration of reply validation; (2) image-create/memory-alloc encoder consolidation with golden-byte tests; (3) DirectStream/no-reply type for the async scanout submit path.

**Validation.** Host-side cfg(test) golden-byte tests for every consolidated encoder vs pre-refactor byte captures; KMD build; reboot; device healthy; ScanoutDiag absent; VpSA=1/ScSet=1; visible desktop, cursor, ~63 fps DComp cadence; no new control timeouts or ring FATAL latches.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** A ReplyReader may be constructed at any time over reply_map regardless of whether the last command generated a reply, and a GENERATE_REPLY stream can be handed to the fire-and-forget async path (which has no reply transaction) — nothing stops decoding a stale/absent reply or violating the no-reply contract of submit_venus_async_scanout.
1. **Compile-time representation:** ring_call is the only producer of ReplyReader (validated header consumed inside it); a sealed DirectStream newtype whose constructor forces flags=0 is the only input type accepted by ctrl::submit_venus_async_scanout.
1. **Smallest atomic migration:** One commit per mechanical step; ring_call migration is call-site-local within venus.rs, DirectStream touches venus.rs + ctrl.rs signature.
1. **Remaining `unsafe` preconditions:** The compiler cannot prove the host actually wrote the reply bytes before head advanced — that stays a vn_ring protocol trust; volatile MMIO reads inside ReplyReader remain a small unsafe core.
1. **Regression test proving preserved behavior:** Golden-byte equality of all encoded streams pre/post; boot-time bring-up diag sequence 0x0001..0x000C unchanged; full visual regression gate.


### R24. SubmitCommand vs SubmitCommandVirtual are duplicate bodies; the DMA record tail is triplicated across the three render DDIs

- **Category:** dedup · **Reported by:** `kmd-submit/dedup-submit-and-record-tails`
- **Files:** `kmd_render/src/ddi/submit_command.rs`
- **Symbols:** `dxgkddi_submit_command`, `dxgkddi_submit_command_virtual`, `dxgkddi_render`, `dxgkddi_render_km`, `dxgkddi_render_gdi`
- **Verification:** **MODIFIED** (severity low) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** The two submit DDIs differ only in arg struct name: both do null checks, fence extraction, SUBMIT_COUNT/SUBMIT_LAST_FENCE, `Flags.__bindgen_anon_1.Value & 1` paging-bit peek (the magic bit-0 read is itself duplicated with its SAFETY comment), then note_and_maybe_signal. The record tail (cmd_len vs dma_cap check, copy_nonoverlapping, pDmaBuffer advance, MultipassOffset=0) appears three times in render/render_km/render_gdi with three near-identical SAFETY comments. Divergence risk: a future fix (e.g. paging-flag width) must be applied 2-3 times.

**Evidence.** submit_command.rs:316-336 vs :342-365 — identical bodies (`SUBMIT_COUNT.fetch_add`, `SUBMIT_LAST_FENCE.store`, `(unsafe { submit.Flags.__bindgen_anon_1.Value } & 1) != 0`, `note_and_maybe_signal(adapter, fence, is_paging, false)`); record tails :572-586, :630-647, :694-713 each with `copy_nonoverlapping(args.pCommand as *const u8, args.pDmaBuffer as *mut u8, cmd_len)` + `args.pDmaBuffer = ... .add(cmd_len)` + `args.MultipassOffset = 0`.

**Recommendation.** Extract `fn handle_submit(adapter, fence: u32, flags_value: u32) -> NTSTATUS` (single site for the paging-bit decode — consider a `SubmitFlags` newtype with `is_paging()`) and `fn record_dma_passthrough(cmd, cmd_len, dma, dma_cap) -> Result<advance>` used by all three render DDIs. Pure consolidation, byte-identical behavior.

**Risk.** Low; both paths are exercised every frame (paging + render).

**Atomic commit boundary.** One commit in submit_command.rs.

**Validation.** Builds; desktop up; SUBMIT_COUNT/SUBMIT_PAGING_COUNT ratios match baseline; no 0x119 bugcheck through a preempt-heavy run.

**Verifier corrections (authoritative).** 1) The record tail is NOT uniform across the three DDIs; the shared helper must cover only copy-if-nonnull + pDmaBuffer advance + MultipassOffset=0, leaving per-DDI validation in place. Specifically: (a) dxgkddi_render rejects cmd_len>0 && pCommand==null with STATUS_INVALID_PARAMETER (lines 460-462) while render_km (630) and render_gdi (699) silently skip the copy but still advance pDmaBuffer by cmd_len — do not unify these policies. (b) The cmd_len>dma_cap check is at the HEAD of dxgkddi_render (463-466), before the HeliosPresentRefresh/Render marker peeks and issue_present_scanout/arm_scanout_refresh side effects — it must stay there, or a BUFFER_TOO_SMALL call would arm a scanout refresh and then be re-called after buffer growth (double-arm, touches the frozen refresh-marker path). (c) In render_gdi the check (696) runs AFTER gdi_blit::execute (690); hoisting it into a shared preamble changes the grow-retry behavior (blit currently executes even when BUFFER_TOO_SMALL is returned). 2) handle_submit should take DXGK_SUBMITCOMMANDFLAGS by value (both arg structs use this exact type; it is Copy) and perform the single unsafe `Value` read inside — the recommended `flags_value: u32` parameter leaves the unsafe union read duplicated at both call sites, making the "single site for the paging-bit decode" claim only partially achieved as written. 3) State explicitly that the extracted handle_submit must remain DISPATCH-safe (atomics + note_and_maybe_signal only; no diag::record), per the file's IRQL discipline. 4) Deployment cost missing from risk: any KMD change requires the three-site version bump and a reboot to take effect (dxgkrnl caches the driver), against the frozen 22.22.142.0 baseline.

**Lead-reviewer note.** Verified MODIFIED — the record tail is NOT uniform: keep per-DDI null-pCommand policy, keep dxgkddi_render's cap check at the head (before marker peeks/side effects) and render_gdi's after execute; pass DXGK_SUBMITCOMMANDFLAGS by value so the unsafe union read is truly single-site; helper stays DISPATCH-safe.


### R25. dxgi_present1 multi-surface arm duplicates dxgi_present's publish/gate/PresentCb tail with divergent error semantics

- **Category:** dedup · **Reported by:** `umd-forward-c/present-tail-dedup`
- **Files:** `umd/src/forward.rs`
- **Symbols:** `dxgi_present`, `dxgi_present1`, `submit_runtime_present`
- **Verification:** **MODIFIED** (severity medium) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** dxgi_present1's multi-surface arm re-implements the whole present tail: scanout-route classification (9188-9193 vs 8355-8402), present_sync_publish (9197-9205 vs 8383-8394), frame gate (9207-9212 vs 8481-8498), pfnPresentCb + private-data + submit_runtime_present (9214-9254 vs 8500-8540). Divergences hide in the copies: present1 ignores the gate result entirely (9210, no timeout accounting) and returns DXGI_ERROR_UNSUPPORTED when callbacks are missing (9216-9222), while dxgi_present logs 'skip PresentCb' and returns 0 = success with nothing presented (8532-8539) — an error path reporting success. present1 also skips maybe_force_present_alpha_opaque/readback diagnostics.

**Evidence.** forward.rs:8500-8540 vs 9214-9254 (identical DXGIDDICB_PRESENT fill + 'present_hr = submit_runtime_present(dev, present_private)'); 9210 'dev.dxvk.present_frame_gate(gate_us);' result dropped; 8532-8538 'log_line(... "DXGI Present: skip PresentCb ...")' then falls through to return present_hr==0; 9216-9222 same condition returns DXGI_ERROR_UNSUPPORTED; duplicated presented_primary_private lookups at 8357+8504 and 9188+9226.

**Recommendation.** Extract one present_tail(h, src_h, dst_h, dxgi_ctx, route, gate_kind) helper covering publish, gate, pfnPresentCb private-data, and submit_runtime_present, used by both entry points; parameterize the two intentional differences (vehicle handling, diagnostics) explicitly. First commit is behavior-identical extraction (preserve each path's current returns); a second, separate commit reconciles the missing-callback fake-success in dxgi_present to a documented legal error after verifying the DXGI DDI return contract (that part is an error-path change, gate it on owner sign-off).

**Risk.** The tail touches the frozen present contract (marker submission via submit_runtime_present); any drift in ordering (publish -> gate -> PresentCb -> RenderCb) regresses the direct-primary refresh. Extraction must keep the exact call order and the exact conditions (src_alloc != 0, h_context non-null).

**Dependencies.** R14 (forward-split-modules)

**Atomic commit boundary.** Commit 1: pure extraction with per-path behavior flags. Commit 2 (separate, reviewed): unify missing-callback error return.

**Validation.** win_cargo release; reboot-free adapter restart; visible desktop; VpSA=1/ScSet=1; no new present-gate timeouts; DComp 63 fps; exercise Present1 multi path (flip-model app) and confirm PRESENT1_LOG lines unchanged in shape.

**Verifier corrections (authoritative).** 1) Gate claim overstated: "present1 ignores the gate result entirely (9210, no timeout accounting)" is literally true but is NOT a divergence — dxgi_present line 8488 (`if !dev.dxvk.present_frame_gate(gate_us) && is_vehicle_present`) also drops the gate result for non-vehicle presents (no EXT_FLIP_GATE_TIMEOUTS, no log), and present1-multi is never a vehicle present. Observable gate-timeout behavior is identical in both functions for the comparable path; strike this from the divergence list (or reword as "both non-vehicle paths silently drop the gate result"). 2) Missing divergence: dxgi_present's non-vehicle arm has the CopySubresourceRegion src→dst copy-pair path (8361-8371) plus the `copied` flag; present1-multi has neither (only copy_to_scanout_target when dst_alloc==0). The helper must start after route classification (as the proposed signature taking `route` already implies) or parameterize this third difference explicitly. 3) Additional divergence for commit-2 scope: helios_device(h)==None returns 0 (success) from dxgi_present (present_hr init at 8324, return at 8605) but E_INVALIDARG from present1 (init at 9214) — include it in the reconciliation review. 4) Tighten the fake-success claim's scope: the skip branch (8532-8538) also covers src_alloc==0, where returning 0 may be deliberate tolerance for allocation-less sources; present1 instead rejects src_alloc==0 early with E_INVALIDARG (9179-9185). The recommendation's gating of that unification on DDI-contract verification + owner sign-off is correct and must stay — commit 2 must not land as part of the extraction.

**Lead-reviewer note.** Verified MODIFIED — commit 1 (behavior-identical extraction parameterized by route, preserving per-path returns and the publish→gate→PresentCb→submit ordering) lands now; commit 2 (reconciling the divergent error semantics incl. the helios_device-None 0-vs-E_INVALIDARG and the fake-success skip branches) is owner-gated and must NOT ride along.


### R26. Hull/domain shader creates are four ~40-line clones; nine creates share a copy-pasted prologue; the two tess-signature flatteners duplicate the walk

- **Category:** dedup · **Reported by:** `umd-forward-b/tess-shader-create-quadruplication`
- **Merged duplicate reports (1):** `umd-forward-a/signature-flatten-shader-dedup` — Three near-identical signature flatteners and per-stage shader-create boilerplate
- **Files:** `umd/src/forward.rs`
- **Symbols:** `create_hull_shader`, `create_hull_shader_11_1`, `create_domain_shader`, `create_domain_shader_11_1`, `create_geometry_shader`, `create_geometry_shader_so`, `create_compute_shader`, `flatten_tess_io_signatures`, `flatten_tess_io_signatures_11_1`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** create_hull_shader (3529-3568), create_hull_shader_11_1 (3570-3609), create_domain_shader (3611-3650), create_domain_shader_11_1 (3652-3693) are byte-for-byte identical except the kind literal (0/1), the flatten function, and the name string — including the identical sig-path-then-raw-bytecode fallback. The prologue (clear_handle, helios_device, shader_code_len, log_shader_code, len==0 bail, from_raw_parts, dxvk.as_ref) is additionally cloned in create_geometry_shader (3447-3460), create_geometry_shader_so (3484-3504), create_compute_shader (3701-3714) and create_shader_11_1_common (3348-3361). flatten_tess_io_signatures (3195-3237) and _11_1 (3243-3303) duplicate the three-array walk, differing only in entry field width (3 padded vs 5 real words) — the wire layout invariant '[n_in,n_out,n_patch, 5 words/entry]' is maintained by parallel edits.

**Evidence.** umd/src/forward.rs:3552-3562 and :3593-3603 identical "create_tess_shader_sig(0, ...)" + "falling back to raw bytecode" blocks; :3634-3644 and :3675-3687 the same with kind 1; :3226 pads "[e.SystemValue as u32, e.Register, e.Mask as u32, 0, 0]" vs :3274-3280 real 5 fields — same [n_in,n_out,n_patch]+5-word wire layout maintained in two places (doc comment at 3239-3242).

**Recommendation.** One create_tess_shader_common(h, kind: TessKind, code, h_shader, sig_words, name, fallback_fn) collapsing the four bodies; one prologue helper returning (dev, dxvk, bytes) or None used by all creates; one generic flatten over an entry-view closure yielding the 5-word tuple (the 10.x path passes zeros for comptype/stream) so the wire layout is written once next to a doc comment and a unit test on the word layout.

**Risk.** Low: mechanical; the only behavior to preserve exactly is the log strings (grep-targets in triage recipes) and the zero padding of 10.x entries.

**Atomic commit boundary.** One commit for the four tess creates + prologue helper; one for the flatten unification with its layout unit test.

**Validation.** Release build; byte-diff of umd log lines for one hull+domain create pair before/after (same strings); selftests PASS; a tessellation-using sample (dxvk-tests) renders identically.


### R27. Six copy-pasted RegGetValueA knob readers (~300 lines) with three different absent-value polarities, plus scattered uncached env knobs

- **Category:** dedup · **Reported by:** `umd-core/centralize-config-knobs`
- **Merged duplicate reports (4):** `xc-errors/umd-registry-knob-dedup` — Six near-identical RegGetValueA knob readers in umd/src/lib.rs duplicate the extern block, constants and read logic; `xc-duplication/umd-reg-knob-dedup` — Six copy-pasted RegGetValueA read-once knob readers in umd/src/lib.rs — collapse to one helper; `xc-concurrency/umd-knob-reader-dedup` — Six copy-pasted RegGetValueA/OnceLock registry-knob readers in umd/src/lib.rs; `xc-legacy/umd-dedup-boilerplate` — Six copy-pasted registry-knob readers in lib.rs and four near-identical device-funcs table fills in device_funcs.rs (~420 duplicated lines)
- **Files:** `umd/src/lib.rs`
- **Symbols:** `trace_enabled`, `feature_level_mode`, `present_gate_us`, `vehicle_flip_gate_us`, `vehicle_kernel_flip_wait`, `present_sync_publish_enabled`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 5 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** Each of the six knob functions (lib.rs:1006-1040, 1068-1106, 1115-1154, 1165-1204, 1223-1258, 1264-1298) re-declares the same `RegGetValueA` extern block, HKEY_LOCAL_MACHINE/RRF_RT_REG_DWORD constants, SAFETY comment, and OnceLock scaffold — six near-identical ~35-line bodies differing only in value name and default. The absent-value semantics are encoded in three subtly different boolean/if shapes: `rc == 0 && value != 0` (absent=OFF, 1039/1296), `rc != 0 || value != 0` (absent=ON, 1256), `if rc == 0 { value } else { DEFAULT }` (1148-1152, 1198-1202). Env knobs are separate and inconsistent: lib.rs:790 reads HELIOS_DXGI_NO_REDIRECTION with an uncached std::env::var_os on every CreateDevice, while forward.rs has its own env_flag helper (forward.rs:112-114) and ad-hoc HELIOS_PRESENT_* reads.

**Evidence.** lib.rs:1010-1021, 1073-1083, 1120-1130, 1170-1180, 1228-1238, 1268-1278 — six identical 'unsafe extern "system" { fn RegGetValueA(...) }' blocks. Polarity variants: lib.rs:1038 'rc == 0 && value != 0'; lib.rs:1255-1256 '// Absent/unreadable = ON... rc != 0 || value != 0'; lib.rs:1148-1152 'if rc == 0 { value } else { DEFAULT_US }'. Uncached env knob: lib.rs:790 'if std::env::var_os("HELIOS_DXGI_NO_REDIRECTION").is_some()'.

**Recommendation.** Introduce umd/src/knobs.rs: one `fn hklm_helios_dword(value_name: &CStr) -> Option<u32>` containing the single RegGetValueA extern + SAFETY comment, plus typed accessors declaring their default explicitly (e.g. `dword_or(c"PresentGateUs", 10_000)`, `bool_default_on(c"VehicleKernelFlipWait")`). Move the per-knob doc comments (they carry session history — keep verbatim). Route env knobs through one cached helper in the same module. Behavior-preserving: same values, same read-once semantics.

**Risk.** Low; the subtle part is preserving each knob's exact absent/unreadable polarity — a mis-transcribed default flips a shipping behavior (e.g. kernel flip wait default-ON). Diff review must check each accessor against the original expression.

**Atomic commit boundary.** One commit: add knobs.rs, port all six registry readers + the CreateDevice env read; forward.rs env callers can migrate in a follow-up.

**Validation.** Release UMD build; A/B with UmdTrace=1 shows trace lines; VehicleKernelFlipWait absent still logs 'flip-kwait READY'; PresentGateUs absent still gates at 10ms (no new steady-state gate timeouts); FL default stays FL10 profile; DComp cadence ~63fps.

**Lead-reviewer note.** Five reports. Preserve each knob's absent-value polarity exactly — three different polarities exist today and at least one (VehicleKernelFlipWait: absent = ON) is load-bearing.


### R28. Six duplicated null+magic handle resolvers, and DescribeAllocation dereferences hAllocation with no magic check at all

- **Category:** static-guarantee · **Reported by:** `kmd-alloc/handle-resolver-dedup`
- **Merged duplicate reports (2):** `xc-concurrency/alloc-handle-resolver` — Raw HANDLE→AllocationContext/OpenAllocationContext casts with magic checks are re-implemented at 6+ sites in create_allocation.rs; `xc-errors/allocation-handle-trusted-boundary` — Opaque WDDM handle -> AllocationContext casts are scattered with per-site magic checks, and DescribeAllocation dereferences with no check at all
- **Files:** `kmd_render/src/ddi/create_allocation.rs`
- **Symbols:** `present_alloc_info`, `present_scanout_alloc_info`, `paging_alloc_info`, `scanout_alloc_info`, `set_bar_placement`, `submit_primary_scanout_copy`, `dxgkddi_describe_allocation`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 3 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** The `is_null() -> cast -> magic != MAGIC` ritual is hand-repeated in present_alloc_info (161-171), present_scanout_alloc_info (186-193), paging_alloc_info (214-221), scanout_alloc_info (253-259), set_bar_placement (434-441) and submit_primary_scanout_copy (343-354), each an unsafe fn with its own SAFETY prose. The duplication has already diverged: dxgkddi_describe_allocation derefs `args.hAllocation as *const AllocationContext` at :1361 after only a null check — no magic validation — even though the magic exists precisely because 'a garbage dereference ... is a bugcheck' (:36).

**Evidence.** Pattern: create_allocation.rs:162-167 `if h.is_null() { return None; } let open = unsafe { &*(h as *const OpenAllocationContext) }; if open.magic != OPEN_ALLOCATION_CTX_MAGIC { return None; }` repeated at :190-193, :218-221, :257-259, :438-441, :346-354. Divergence: :1356-1361 `if args.hAllocation.is_null() { return STATUS_INVALID_PARAMETER; } ... let ctx = unsafe { &*(args.hAllocation as *const AllocationContext) };` — no magic check; :35-37 doc: 'validates hAllocation casts in paging DDIs (a garbage dereference in BuildPagingBuffer is a bugcheck)'.

**Recommendation.** One trusted boundary per context type: `fn resolve_alloc(h: HANDLE) -> Option<AllocationRef<'_>>` and `fn resolve_open(h: HANDLE) -> Option<OpenAllocRef<'_>>` (non-null, magic-checked, lifetime-scoped to the DDI call). All projections (PagingAllocInfo, ScanoutInfo, PresentAllocInfo, describe fill, bar placement) take the ref type; the six unsafe casts collapse to two, and describe_allocation gains the missing check for free (behavior-preserving: dxgkrnl only ever passes back our own handle).

**Risk.** Very low: pure consolidation; the only observable delta is DescribeAllocation now refusing a magic-mismatched handle, which per contract cannot occur.

**Atomic commit boundary.** One commit inside create_allocation.rs (call sites in display.rs/scheduler.rs/build_paging_buffer.rs/cpu_host_aperture.rs updated mechanically).

**Validation.** Builds; boot with visible desktop; describe/open/present flows exercised (window drag, app launch); 0x0C20/0x0C22 describe breadcrumbs and Present/paging counters unchanged.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Handle validity checked per call site by convention; permits a resolver that forgets the magic (already happened in DescribeAllocation) or forgets the null check; also, the magic can never catch a freed-then-still-magic-intact context.
1. **Compile-time representation:** Non-null lifetime-bearing AllocationRef/OpenAllocRef produced only by one checking constructor; projections take the ref type so an unchecked path cannot exist.
1. **Smallest atomic migration:** create_allocation.rs in one commit; external callers keep the same Option-returning signatures.
1. **Remaining `unsafe` preconditions:** Handle liveness (not yet Destroy/Close-freed) is a dxgkrnl round-trip contract that cannot be encoded; it remains the single documented precondition of the resolve fns.
1. **Regression test proving preserved behavior:** Full boot + present + paging + describe exercise with unchanged counter streams; a debug-only assert build confirms no resolver rejects during a normal session.

**Lead-reviewer note.** Also closes the DescribeAllocation unchecked-deref gap as part of the same boundary. Apply the R60 verifier's UB caution here too: the resolver validates raw u32/pointer fields BEFORE exposing any typed view; no enum/NonNull inside the struct read from an untrusted handle.


### R29. Two near-identical 40-line hand-rolled PCI capability walks with magic byte offsets; parse-once typed capability reader wanted

- **Category:** dedup · **Reported by:** `kmd-transport-gpu/cap-walk-dedup-typed-caps`
- **Files:** `kmd_render/src/virtio/gpu.rs`
- **Symbols:** `scan_host_visible_window`, `map_isr_status_register`, `bar_base`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** scan_host_visible_window (gpu.rs:276-312) and map_isr_status_register (gpu.rs:326-356) duplicate the entire walk skeleton: identical cap-list presence check (gpu.rs:277-279 vs 327-329), identical pointer mask/bounded loop (`for _ in 0..48`, `& 0xFC`), identical d0 field extraction, then diverge only in which cfg_type they match and which `virtio_pci_cap` fields they decode via raw `cap + 4/8/12/16/20` offset arithmetic with the struct layout described only in comments (gpu.rs:292, 297, 341). Each new capability need (e.g. a future notify-cap or config-cap consumer) would clone the walk a third time.

**Evidence.** gpu.rs:277-281 and gpu.rs:327-331 are byte-for-byte the same presence-check + cap-pointer mask. gpu.rs:283 and gpu.rs:331 identical `for _ in 0..48` walk. Field decode by comment: gpu.rs:292 "`virtio_pci_cap`: bar at +4 byte0, id (shmid) at +4 byte1.", gpu.rs:297 "`virtio_pci_cap64`: offset lo/hi at +8/+16, length lo/hi at +12/+20.", gpu.rs:340-342 "bar at +4 byte0; offset (u32) at +8".

**Recommendation.** Extract one iterator `virtio_vendor_caps(access) -> impl Iterator<Item = VirtioPciCapHeader { cfg_type, bar, offset_in_cfg }>` plus a validated `read_cap64(access, cap) -> VirtioPciCap64 { offset: u64, length: u64 }` built on the wide accessor from cfg-offset-u8-truncation. Both consumers become ~8-line matches over typed values; the byte-offset magic lives once next to the spec-layout comment. `bar_base`'s 64-bit-BAR decode stays shared as-is.

**Risk.** Low — init-time-only code with easily-compared outputs (window base/len, ISR VA).

**Dependencies.** D6 (cfg-offset-u8-truncation)

**Atomic commit boundary.** One commit after the wide-accessor defect fix lands.

**Validation.** Boot diag records unchanged: 0x0B00_0005 (window found) and 0x0B00_0006 (ISR mapped); INTx acknowledged (INT_ROUTINE_COUNT advancing, no interrupt-storm Code 43); QUERY_STATS window_len identical.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Cap-structure field offsets (+4/+8/+12/+16/+20) and the 48-step/dword-align walk bounds are encoded twice by hand; the invalid state permitted is a divergent edit (one walk fixed, the other not) or a third hand-rolled copy re-introducing the truncation bug.
1. **Compile-time representation:** Single typed iterator + validated VirtioPciCap64 reader; offset arithmetic exists in exactly one function; consumers pattern-match typed fields.
1. **Smallest atomic migration:** gpu.rs (or the new virtio/caps.rs) only; no external signature changes.
1. **Remaining `unsafe` preconditions:** None — pure config-space reads; the device-supplied cap chain remains untrusted input handled by the bounded walk.
1. **Regression test proving preserved behavior:** Same-boot comparison of window base/len + ISR VA diag records against the pre-refactor boot; INTx storm detector never trips.


### R30. Duplicated command construction (sync vs async SET_SCANOUT_BLOB / RESOURCE_FLUSH), duplicated enqueue-backpressure loops, and a triplicated blob-teardown sequence

- **Category:** dedup · **Reported by:** `kmd-transport-ctrl/ctrl-dedup`
- **Files:** `kmd_render/src/virtio/ctrl.rs`
- **Symbols:** `set_scanout_blob`, `set_scanout_blob_async`, `resource_flush`, `resource_flush_async`, `ctrl_roundtrip`, `submit_venus_async`, `release_blob_for_owner`, `release_blobs_for_owner`, `forget_allocation_blob`, `submit_venus_sync`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Three families of copy-paste: (1) command builders — VirtioGpuSetScanoutBlob is filled field-by-field identically at ctrl.rs:414-429 and 522-536; VirtioGpuResourceFlush at 441-450 and 468-476; a drift in one copy would silently desynchronize the sync (diagnostic) and async (production) scanout binds. (2) the enqueue-with-backpressure retry loop (take-Option, drain_used+enqueue, QueueFull -> reap+sleep_ms(1), ENQUEUE_RETRY_MAX) appears at ctrl.rs:256-277 (one buffer) and 1412-1439 (two buffers) with identical 'expect("... returned on every retry path")' scaffolding. (3) blob teardown — 'if mapped { resource_unmap_blob; free_window_range_pub } ; if take_live_resource { ctx_detach_resource; resource_unref }' is triplicated at 1287-1302, 1314-1327, and (unmap half) 1336-1349. Plus submit_venus_sync (1376-1382) is a pure alias of submit_3d_sync with one caller (venus.rs:561), and the resp_is_ok tail of ctrl_roundtrip_ok is re-inlined in resource_assign_uuid (606-611) and resource_map_blob_roundtrip (1137-1139).

**Evidence.** ctrl.rs:414-429 vs 522-536: identical 'cmd.hdr.type_ = VIRTIO_GPU_CMD_SET_SCANOUT_BLOB; ... cmd.strides[0] = stride; cmd.offsets[0] = offset;' blocks; ctrl.rs:441-450 vs 468-476 identical RESOURCE_FLUSH fill; ctrl.rs:257 and 1416 twin 'let m = meta_slot.take().expect("meta returned on every retry path")' loops; ctrl.rs:1290-1299, 1317-1326, 1343-1346 triplicated 'resource_unmap_blob ... free_window_range_pub ... take_live_resource ... ctx_detach_resource ... resource_unref' sequence; ctrl.rs:1376-1382 'pub fn submit_venus_sync(...) { submit_3d_sync(adapter, ctx_id, stream) }'.

**Recommendation.** Pure-function builders (build_set_scanout_blob(...) -> VirtioGpuSetScanoutBlob, build_resource_flush(...)) used by both sync and async paths; one generic backpressure helper parameterized over the buffer bundle (or two thin wrappers over a shared retry core); one teardown_blob(adapter, ctx_id, res, mapped, off, len) helper used by all three reclamation paths; collapse submit_venus_sync into submit_3d_sync (rename at the one caller); share the check-RESP_OK tail.

**Risk.** Low: mechanical consolidation of identical bodies. The teardown helper must preserve the exact call order (unmap before free_window_range before take_live_resource gate) — the host-subregion-overlap invariant depends on it.

**Dependencies.** R7 (diag-scanout-extraction)

**Atomic commit boundary.** Three small commits (builders; backpressure helper; teardown helper + alias collapse), each independently revertible.

**Validation.** KMD build + side-by-side diff showing identical emitted command bytes; regression gate; blob-churn workload (game launch/exit) with QUERY_STATS showing blobs_live returning to baseline and no qemu 'resource does not exist' host-log lines.


### R31. MapCpuHostAperture DISPATCH-ack path duplicates the whole-allocation shape check and diverges: it skips the consecutive-pages validation

- **Category:** dedup · **Reported by:** `kmd-alloc/aperture-shape-validate-once`
- **Files:** `kmd_render/src/ddi/cpu_host_aperture.rs`
- **Symbols:** `dxgkddi_map_cpu_host_aperture`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** The whole-allocation page-count formula is written twice: raised-IRQL arm :155 `args.NumberOfPages as u64 == (a.size.saturating_add(4095) >> 12).max(1)` vs PASSIVE arm :181 `let blob_pages = (alloc.size.saturating_add(4095) >> 12).max(1);` + :189 `n != blob_pages`. The copies have already diverged: the PASSIVE path validates every aperture page is consecutive (:197-205, BAR_AP_ERR_SPARSE) before trusting page0, but the DISPATCH idempotent-ack path reads ONLY page0 (:159-166) — a sparse range whose first page happens to match the existing blob mapping would be acknowledged, and CPU VAs over pages 1..N-1 would alias other window content. dxgkrnl allocates consecutive ranges in practice (ChEs=0 post-boot), so this is latent, but it is exactly the silent-content-loss class the module doc says the loud refusals exist to kill (:127-129).

**Evidence.** cpu_host_aperture.rs:149-167 DISPATCH ack: `.filter(|a| ... && args.NumberOfPages as u64 == (a.size.saturating_add(4095) >> 12).max(1)) .map(|a| { let page0 = read_unaligned(args.pCpuHostAperturePages) as u64; ... blob_resid_at_offset(offset) == Some(a.resource_id) })` — no loop over pages; PASSIVE path :181 `let blob_pages = (alloc.size.saturating_add(4095) >> 12).max(1);` and :197-204 `for i in 1..n { ... if p != page0 + i { BAR_AP_ERR_SPARSE...; return STATUS_NO_MEMORY; } }`; module doc :127-129 'a null success lets the CPU read/write UNBACKED window offsets ... the exact silent-content-loss class this fix exists to kill'.

**Recommendation.** Validate-once constructor: `ApertureRequest::validate(args, &alloc, bar) -> Result<ApertureRequest, Refusal>` performing null/count/whole-allocation/consecutive/bounds checks in one place, returning a typed {resource_id, offset, pages}; both the PASSIVE map path and the DISPATCH already-mapped ack consume it. Counter mapping stays 1:1 (ChEp/ChEs/ChEb) so refusal telemetry is unchanged; the only behavioral delta is the DISPATCH ack now also refusing sparse ranges — which today would be silently mis-acked.

**Risk.** The DISPATCH ack is on the display-activation path (v71 lesson): the consecutive check there is pure reads of dxgkrnl's page array (DISPATCH-safe, as page0 read already is). If a sparse DISPATCH request ever occurs it now defers via STATUS_NO_MEMORY (retry at PASSIVE) instead of falsely acking — strictly safer, same legal status.

**Atomic commit boundary.** One commit in cpu_host_aperture.rs.

**Validation.** Reboot: display activates (VpSA=1/ScSet=1), ChIa/ChId counts comparable to baseline, ChEs stays 0, ChEp/ChEb unchanged, no 0-path VidPn commits (ETW AzureTriage clean).

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** The 'whole-allocation, consecutive, in-bounds' aperture-request shape is re-derived per arm; the two arms already disagree (sparse check missing at DISPATCH), permitting a false idempotent ack over a sparse range.
1. **Compile-time representation:** ApertureRequest constructible only via the one validating constructor; both arms consume the typed request, so a shape check cannot be skipped per-arm.
1. **Smallest atomic migration:** cpu_host_aperture.rs single commit; unmap path can share the page0/offset extraction.
1. **Remaining `unsafe` preconditions:** pCpuHostAperturePages length (NumberOfPages entries) remains a dxgkrnl contract that cannot be encoded — reads stay unsafe with that one documented precondition.
1. **Regression test proving preserved behavior:** Boot-time activation sequence with ChE* zero and identical ChMn/ChIa/ChId trajectories; VidPn commit succeeds (no v71-style 0-path).

**Lead-reviewer note.** Before unifying, determine whether the DISPATCH-ack path's skipped consecutive-pages validation is deliberate (IRQL constraint). If it is, encode the difference explicitly (two validation levels in the type), don't silently unify to either behavior.


### R32. Wire/layout contracts duplicated across UMD and KMD by parallel constants: HELIOS_ALLOC_MISC_* bits, cross-adapter 256-byte pitch math, and DXGI 87/88 scanout-format literals — move to protocol/

- **Category:** dedup · **Reported by:** `xc-duplication/protocol-owns-scanout-wire-contract`
- **Files:** `umd/src/forward.rs`, `kmd_render/src/ddi/create_allocation.rs`, `kmd_render/src/ddi/display.rs`, `protocol/src/wddm.rs`
- **Symbols:** `HELIOS_ALLOC_MISC_PRIMARY`, `HELIOS_ALLOC_MISC_DIRECT_SCANOUT`, `cross_adapter_pitch`, `CROSS_ADAPTER_PITCH_ALIGN`, `HeliosWddmAllocMeta`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Three cross-tree contracts are kept in sync only by comments: (1) the misc_flags bits travelling inside HeliosWddmAllocMeta are defined twice — umd/src/forward.rs:76-77 and kmd_render/src/ddi/create_allocation.rs:462-465 — with forward.rs:73 explicitly saying 'matching HELIOS_ALLOC_MISC_PRIMARY in kmd_render/...'; (2) the 256-aligned cross-adapter pitch that SET_SCANOUT_BLOB strides depend on is computed independently in umd forward.rs:1289-1294 and kmd create_allocation.rs:453-475 (a divergence shears scan-out, the exact class of the 7584-vs-7680 bug); (3) the DXGI_FORMAT_B8G8R8A8/X8 87|88 acceptance set is a bare literal at display.rs:90/166/710/738-739, create_allocation.rs:355-356, submit alloc meta 1513, and umd forward.rs:157/1769.

**Evidence.** umd/src/forward.rs:73-77 '/// KMD-private meta bit matching `HELIOS_ALLOC_MISC_PRIMARY` in /// `kmd_render/src/ddi/create_allocation.rs`... const HELIOS_ALLOC_MISC_PRIMARY: u32 = 0x8000_0000;' duplicating create_allocation.rs:462-465; pitch math duplicated: forward.rs:1289-1294 'const CROSS_ADAPTER_PITCH_ALIGN: u32 = 256; ... & !(CROSS_ADAPTER_PITCH_ALIGN - 1)' vs create_allocation.rs:472-475 'pub(crate) fn cross_adapter_pitch(width: u32) -> u32 { ... & !(CROSS_ADAPTER_PITCH_ALIGN - 1) }'; format literals: display.rs:710 'matches!(source.dxgi_format, 87 | 88)', forward.rs:1769 'matches!(a.Format as u32, 87 | 88)'.

**Recommendation.** protocol/ already owns HeliosWddmAllocMeta; move the misc-bit constants next to it (pub const on the struct), add `pub fn cross_adapter_pitch(width: u32) -> u32` and a `ScanoutFormat` mini-enum/const pair (B8G8R8A8=87, B8G8R8X8=88 with an is_scanout_format() predicate) to protocol/src/wddm.rs, and delete both per-crate copies. protocol builds on both platforms by design (CLAUDE.md repository notes).

**Risk.** Near zero: values are identical today; protocol is no_std-compatible and already a dependency of both crates. Keep numeric values bit-identical (wire ABI).

**Atomic commit boundary.** One commit adding the protocol items + switching both crates; no INF/version bump needed (no wire change).

**Validation.** Both trees build (Linux check + win_cargo); byte-identical HeliosWddmAllocMeta layout (existing const asserts); boot with visible desktop and correct stride (no shear); ScFmt/PScFmt counters stay silent.

**Lead-reviewer note.** protocol/ builds on both platforms — moving the constants there is the mechanism that makes UMD/KMD drift a compile error rather than a wire bug.



---

## Part II, Tranche 5 — Static guarantees: constants and newtypes (mechanical)

Mechanical, low-risk migrations that make transposition and drift errors unrepresentable: protocol constants pinned to ground truth, bare-integer identities newtyped. These land before the structural typestate tranche because the typestate work builds on the newtypes. Wire-ABI byte layouts must be provably unchanged (const asserts, byte-equality tests).

**Regression-gate emphasis:** builds on both platforms for protocol/; byte-identical wire encodings (assert-based); full boot-to-desktop once per batch.

### R33. 110+ hand-transcribed Vulkan/venus wire constants verified only by eyeball — the exact bug class that cost a full session (IMAGE_TILING_LINEAR=0)

- **Category:** static-guarantee · **Reported by:** `kmd-venus/vk-consts-ground-truth`
- **Merged duplicate reports (1):** `xc-unsafe/venus-protocol-typed-constants` — Hand-transcribed Vulkan/Venus protocol constants as bare u32 — the proven IMAGE_TILING_LINEAR=0 defect class
- **Files:** `kmd_render/src/virtio/venus.rs`
- **Symbols:** `CMD_*`, `ST_*`, `IMAGE_TILING_LINEAR`, `FORMAT_B8G8R8A8_UNORM`, `QUEUE_FAMILY_EXTERNAL`, `VK_MAX_MEMORY_TYPES`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Lines 47-158 hand-transcribe ~28 VkCommandTypeEXT ids, ~13 VkStructureType ids, and dozens of flag/enum values. The file's own comment records the historical failure: 'This was 0 (OPTIMAL), so create_linear_scanout_image built a TILED image ... (ScanoutDiag=16 SdgErr=2 / SdgLStg=3)' (115-119). Verification is a comment claim ('Verified against vn_protocol_driver_defines.h', line 48). The authoritative sources are in-repo: icd/mesa/src/virtio/venus-protocol/vn_protocol_driver_defines.h (command ids; spot-checked: vkAllocateMemory=21, vkCreateFence=35, vkCreateRingMESA=188 all match today) and vendored Vulkan headers (dxvk-helios/include/vulkan, qemu-helios/third_party/Vulkan-Headers/registry/vk.xml).

**Evidence.** venus.rs:115-120 '// VK_IMAGE_TILING_OPTIMAL = 0, VK_IMAGE_TILING_LINEAR = 1. This was 0 (OPTIMAL), so create_linear_scanout_image built a TILED image → device-local-only memoryTypeBits ... const IMAGE_TILING_LINEAR: u32 = 1;'. venus.rs:47-48 '// ── venus command type ids (VkCommandTypeEXT) ... // Verified against vn_protocol_driver_defines.h.' Ground truth in-repo: icd/mesa/src/virtio/venus-protocol/vn_protocol_driver_defines.h:52 'VK_COMMAND_TYPE_vkAllocateMemory_EXT = 21', :66 '...vkCreateFence_EXT = 35', :261 '...vkCreateRingMESA_EXT = 188'.

**Recommendation.** Make the transcription machine-checked at build time: extend kmd_render/build.rs to parse vn_protocol_driver_defines.h (and the vendored vulkan_core.h for VkStructureType/VkFormat/VkImageTiling/flags) and either (a) generate the constants module, or (b) emit compile_error! if any checked-in constant disagrees with the parsed value. Option (b) is the smallest trusted boundary: values stay greppable in-source, but a mistranscription becomes a build failure instead of a black screen plus a diagnostic session. Keep the constants' names and values identical — zero wire change.

**Risk.** build.rs parsing must tolerate header formatting drift when the mesa fork is bumped; a fragile parser that silently matches nothing would give false confidence — fail the build if fewer constants than expected are found.

**Atomic commit boundary.** One commit: build.rs verifier + expected-count guard; no source-of-truth value changes.

**Validation.** Build passes with verifier active and fails when a constant is deliberately perturbed (test the verifier both ways once); binary diff of the driver .sys ideally identical; standard regression gate (visible desktop, VpSA=1/ScSet=1, ScanoutDiag absent).

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Constant correctness rests on a comment; a wrong value encodes a valid-looking but semantically different Vulkan command (the LINEAR/OPTIMAL bug produced no error — just an unusable memoryTypeBits and a dead scanout path diagnosed over a full session).
1. **Compile-time representation:** Build-time cross-check of every constant against the in-repo protocol header / vendored registry; mismatch = compile error naming the constant.
1. **Smallest atomic migration:** build.rs only; no driver-source semantic change.
1. **Remaining `unsafe` preconditions:** Constants absent from the parsed headers (e.g. venus MESA experimental sTypes if the fork lags) need an explicit exempt-list; the checker cannot validate wire *layout* (field order/padding), only values.
1. **Regression test proving preserved behavior:** Deliberate perturbation test proves the checker fires; unchanged binary + standard visual gate proves no behavior change.

**Lead-reviewer note.** This is the exact bug class that cost a full session (IMAGE_TILING_LINEAR transcribed as 0 = OPTIMAL). Prefer generating or asserting the table against venus-protocol/vk.xml (ground truth per Operating Rule 1) over another hand audit.


### R34. All Venus object handles are bare u64 and virtio resources bare u32 — transposed arguments compile silently into wrong wire commands

- **Category:** static-guarantee · **Reported by:** `kmd-venus/handle-id-newtypes`
- **Files:** `kmd_render/src/virtio/venus.rs`
- **Symbols:** `VenusClient::bind_image_memory`, `VenusClient::cleanup_imported_source_alias`, `VenusClient::alloc_handle`, `HostVisibleBlob`, `ScanoutImageBlob`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** image_id, memory_id, fence_id, pool_id, command_buffer_id, device_id, queue_id, ring_id, blob_id are all u64 from one alloc_handle() counter; res_id is u32. Multi-handle signatures like bind_image_memory(adapter, image_id, memory_id) (1197-1202) and cleanup_imported_source_alias(adapter, resource_id, image_id, memory_id) (1711-1717) accept transposed same-type arguments without complaint; the encoded command is well-formed on the wire and fails only at host decode (or worse, destroys the wrong object if ids alias across types — all ids share one number space, so a stale image id can equal a live memory id). This is the same bug shape as the WDDM handle-reinterpretation class the handoff calls out.

**Evidence.** venus.rs:549-553 'fn alloc_handle(&mut self) -> u64 { let h = self.next_handle; self.next_handle += 1; h }' — one number space for every object type. venus.rs:1197-1202 'fn bind_image_memory(&mut self, adapter: &AdapterContext, image_id: u64, memory_id: u64)'; :1711-1717 'fn cleanup_imported_source_alias(&mut self, adapter: &AdapterContext, resource_id: u32, image_id: u64, memory_id: u64)'; :2277-2279 allocate_dedicated_image_memory(adapter, image_id, alloc_size, memory_type_index) mixes u64 handle and u64 size adjacently.

**Recommendation.** Introduce #[repr(transparent)] newtypes: ImageId(u64), MemoryId(u64), FenceId(u64), PoolId(u64), CmdBufId(u64), DeviceId(u64), QueueId(u64), ResId(u32); alloc_handle becomes generic (`fn alloc<T: FreshHandle>`) so each call site names the type it mints. Writer gains typed put methods (w.image(id)) that unwrap at the single encode boundary. HostVisibleBlob/ScanoutImageBlob fields adopt the newtypes; the create_allocation.rs atomic cache stores raw u64 but converts through the typestate constructor (prepared-copy-typestate).

**Risk.** Mechanical but wide; the newtypes must stay repr(transparent) zero-cost and must not leak into the helios_protocol shared ABI structs. Do after the split so churn is per-module.

**Dependencies.** R17 (venus-split); R23 (ring-call-dedup)

**Atomic commit boundary.** One commit per handle family (image/memory first — they are the ones passed together), or a single mechanical commit if review bandwidth allows.

**Validation.** Pure type refactor: identical wire bytes (golden-byte tests from ring-call-dedup re-run unchanged); KMD build; standard visual regression gate.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** A swapped image/memory/fence/pool argument (or a size passed where a handle goes) encodes a syntactically valid venus command; the failure surfaces only as a host decode error or wrong-object operation, invisible until a black screen or poisoned decoder.
1. **Compile-time representation:** Per-type transparent newtypes; typed Writer put methods; generic typed alloc_handle.
1. **Smallest atomic migration:** Per handle family within venus/ modules; ScanoutImageBlob/HostVisibleBlob consumers (display.rs, create_allocation.rs, scanout_diag.rs) in the same commit as their field changes.
1. **Remaining `unsafe` preconditions:** Cannot prove an id is *live* on the host or that it belongs to this device/context — liveness stays a runtime/host contract; raw u64 persists at the atomic-cache and wire boundaries.
1. **Regression test proving preserved behavior:** Golden-byte encoder equality plus the standard visual gate; bring-up diag sequence unchanged on reboot.


### R35. Wire fence ids, WDDM submission fences, ctx/resource/blob ids, and virtio ring indices are interchangeable bare integers with sentinel zeros and a magic ring_idx==1 branch

- **Category:** static-guarantee · **Reported by:** `kmd-transport-ctrl/wire-fence-id-newtypes`
- **Files:** `kmd_render/src/virtio/gpu.rs`, `kmd_render/src/virtio/ctrl.rs`
- **Symbols:** `InFlightKind::AsyncVenus`, `WddmPending`, `WddmReady`, `VirtioGpu::enqueue_async_submit`, `VirtioGpu::fence_wait_prepare`, `submit_venus_async`, `submit_venus_async_scanout`, `wait_fence`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** The transport's core identities are raw integers: wire fence u64 (0 = 'never a valid wire fence', gpu.rs:807-809), dxgkrnl SubmissionFenceId u32, ring index u32 clamped to u8, ctx/resource u32 (0 reserved), blob id u64. WddmPending packs fence:u32 and watermark:u64 side by side (gpu.rs:667-672) — two differently-scaled 'fences' one field apart. The GPU-completion domain is the magic literal 1: drain_used publishes scanout pixels on 'response_ok && ring_idx == 1' (gpu.rs:1413), submit_venus_async_scanout hardcodes 1 (ctrl.rs:1473), async_retired_up_to special-cases ring 0 (gpu.rs:1733-1740), and enqueue_async_submit silently clamps 'ring_idx.min(u8::MAX as u32) as u8' (gpu.rs:1229). Nothing stops passing a blob id to wait_fence or comparing a WDDM fence with a wire fence.

**Evidence.** gpu.rs:807-809 'Next wire fence id to assign (globally monotonic, starts at 1; 0 is never a valid wire fence)'; gpu.rs:667-672 'struct WddmPending { fence: u32, watermark: u64, ... }'; gpu.rs:1229 'cmd.hdr.ring_idx = ring_idx.min(u8::MAX as u32) as u8'; gpu.rs:1413 'if response_ok && ring_idx == 1'; gpu.rs:1593 'if fence_id == 0 || fence_id >= self.next_wire_fence { return FenceWaitPrep::Invalid; }'; ctrl.rs:1473 'v.enqueue_async_submit(ctx_id, 1, meta, venus, venus_len, Some(notify))'; ctrl.rs:1356-1357 '`fence_id` stays 0 (parity with the proven System-class `submit_direct` shape)'.

**Recommendation.** #[repr(transparent)] newtypes minted at single sites: WireFenceId(NonZeroU64) produced only by enqueue_async_submit; WddmFenceId(u32); CtxId(u32); ResourceId(NonZeroU32); BlobId(u64); and enum RingDomain { Decode, GpuCompletion } replacing raw ring_idx (only 0 and 1 are ever produced in-kernel), giving exhaustive matches where drain_used/async_retired_up_to compare integers today. The escape/venus ABI boundary keeps raw integers and converts via validate-once constructors — the single range-check site for guest-supplied ids.

**Risk.** Medium-low: wide but mechanical signature churn across gpu.rs/ctrl.rs/escape.rs/venus.rs/submit_command.rs; no runtime change if constructors are transparent. Watch the escape structs (ABI frozen) — conversion only at the boundary.

**Atomic commit boundary.** One commit for the type definitions + gpu.rs/ctrl.rs plumbing; a follow-up commit converting escape.rs/venus.rs call sites if reviewers prefer smaller diffs (both must land before the next KMD image).

**Validation.** KMD build; wire traffic byte-identical (no protocol change); ASYNC_SUBMIT_COUNT/ASYNC_COMPLETE_COUNT and RING_SUBMIT_COUNT==RING_COMPLETE_COUNT convergence this boot; regression gate incl. 63 fps cadence and no new WAIT_FENCE timeouts.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Wire fences are nonzero, monotonic, transport-scoped; ring 0 retires at host decode and ring>=1 at GPU completion; WDDM fences are a separate 32-bit monotonic namespace. All enforced by runtime comparisons and comments. Invalid states permitted: fence 0 construction, cross-namespace comparison/passing (blob id into wait_fence type-checks), ring 7 with undefined completion semantics silently clamped to u8.
1. **Compile-time representation:** WireFenceId(NonZeroU64) minted only by enqueue_async_submit; WddmFenceId(u32); ResourceId(NonZeroU32); CtxId(u32); BlobId(u64); enum RingDomain { Decode, GpuCompletion } with exhaustive matches replacing ring_idx integer tests.
1. **Smallest atomic migration:** gpu.rs + ctrl.rs in one commit (types + internal plumbing); escape/venus boundary conversion in the same KMD image.
1. **Remaining `unsafe` preconditions:** Guest/usermode-supplied ids arriving through escapes cannot be typed away — the newtype constructors become the single validate-once site; no new unsafe.
1. **Regression test proving preserved behavior:** Byte-identical wire traffic (no protocol change is possible if constructors are transparent); RING_SUBMIT==RING_COMPLETE convergence and standard visible-desktop gate.


### R36. dxgkrnl-reserve blob-window invariant is one silent runtime guard over bare u64 offsets shared by both window halves

- **Category:** static-guarantee · **Reported by:** `kmd-alloc/window-offset-newtypes`
- **Files:** `kmd_render/src/virtio/gpu.rs`, `kmd_render/src/ddi/cpu_host_aperture.rs`, `kmd_render/src/virtio/ctrl.rs`
- **Symbols:** `free_window_range`, `reserve_window_prefix`, `blob_remap_begin`, `blob_resid_at_offset`, `alloc_window_range`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** The Key Invariant 'offsets below the VidMm/CpuHostAperture reserve belong to dxgkrnl — never recycle them' is enforced at exactly one place: free_window_range's silent early-return `if offset < self.vidmm_reserved { return; }` (gpu.rs:2334-2336), relied on by comments at ctrl.rs:1250-1252 and blob_note_unmapped (gpu.rs:2224-2225 'VidMm-partition offsets never enter the free list'). All offsets are bare u64: alloc_window_range-produced KMD offsets and dxgkrnl aperture-page offsets flow through the same parameters, and blob_resid_at_offset (2216-2221) resolves EITHER half — so dxgkddi_unmap_cpu_host_aperture (cpu_host_aperture.rs:265-281) would happily unmap a KMD-half escape mapping if dxgkrnl ever named pages outside the declared span. The silent-ignore also masks bugs (a misrecorded low offset leaks with no counter, contra the loud-failure rule).

**Evidence.** gpu.rs:2329-2336 `// VidMm-partition offsets are owned by VidMm's segment allocator ... if offset < self.vidmm_reserved { return; }` (silent); :2124-2128 `reserve_window_prefix`; :2216-2221 `blob_resid_at_offset` matches any `s.mapped && s.map_offset == offset` with no reserve bound; cpu_host_aperture.rs:265-266 `let page0 = read_unaligned(args.pCpuHostAperturePages) as u64; let offset = page0 << 12;` then :274-280 unmaps whatever blob resolves; ctrl.rs:1250-1252 '(for KMD-partition offsets only — the free guard ignores VidMm-partition ones)'.

**Recommendation.** Introduce `KmdWindowOffset` (only constructor: alloc_window_range) and `VidmmOffset` (only constructor: a checked `VidmmOffset::new(off, reserve)` at the aperture/UPDATE_PAGE_TABLE boundaries). free_window_range/free_window_range_pub take KmdWindowOffset — the reserve guard becomes structural (keep a debug counter where the runtime check was). Blob slots replace mapped/map_pending/map_offset with `enum Placement { None, Pending(..), Kmd(KmdWindowOffset,len), Vidmm(VidmmOffset,len) }`, and blob_resid_at_offset becomes Vidmm-only.

**Risk.** Mechanical but wide inside gpu.rs/ctrl.rs; the two-phase begin/finish protocol must keep identical state transitions. Land as pure type-threading with no logic change; the Placement enum can be a follow-up commit.

**Atomic commit boundary.** Commit 1: the two offset newtypes threaded through alloc/free/map paths (no logic change). Commit 2 (optional): Placement enum replacing the mapped/map_pending bools.

**Validation.** Reboot; QUERY_STATS window_used/window_len steady-state matches baseline; ChMn/ChUn advance, ChE* stay 0, WINDOW_RANGE_DROPS/WINDOW_ALLOC_REJECTS unchanged; visible desktop + full regression gate; MAP_BLOB-heavy workload (game launch) reaches the same mappings high-water.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Reserve ownership enforced by one silent numeric check; permits (a) feeding VidMm offsets into KMD free paths (silently ignored — masks the bug), (b) resolving/unmapping KMD-half blobs from aperture-named offsets, (c) any future free path that forgets to funnel through free_window_range.
1. **Compile-time representation:** KmdWindowOffset / VidmmOffset newtypes with single trusted constructors; free list and blob placement typed per half.
1. **Smallest atomic migration:** virtio/gpu.rs + virtio/ctrl.rs in one commit; cpu_host_aperture.rs and build_paging_buffer.rs conversions are call-site mechanical.
1. **Remaining `unsafe` preconditions:** The reserve boundary VALUE is runtime configuration (StartDevice-time segment sizing), so VidmmOffset::new still performs one runtime range check — the type encodes provenance, not the numeric bound.
1. **Regression test proving preserved behavior:** Steady-state window accounting (QUERY_STATS) and Ch*/Pg* counters identical across a boot + app churn session; no new leaks (window_used returns to baseline after app exit).

**Lead-reviewer note.** Encodes the 'blob window offsets below the VidMm/CpuHostAperture reserve belong to dxgkrnl' key invariant in the offset type itself.


### R37. Six hand-maintained raw-u32 DXGI format tables plus scattered 87/88/21 literals; no format newtype

- **Category:** static-guarantee · **Reported by:** `umd-forward-a/dxgi-format-magic-tables`
- **Files:** `umd/src/forward.rs`
- **Symbols:** `dxgi_bytes_per_pixel`, `dxgi_bits_per_sample`, `dxgi_output_family_bits`, `dxgi_msaa_bits_per_sample`, `dxgi_resolve_required`, `dxgi_color_typeless_parent`, `dxgi_integer_typed_format`, `dxgi_to_d3dddi_format`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Lines 153-294 hold seven partial classification functions over bare u32 DXGI values with numeric range patterns (`60..=66 => 1`, `87..=93 => Some(32)`) whose correctness depends on memorized enum values; the sets drifted independently (bytes_per_pixel treats 48..=59 as 4 bytes while bits_per_sample says 16 bits — safe only because over-reporting pads pitch, per the comment). Magic literals recur elsewhere: `dev.scanout_format.set(87)` (722), `matches!(a.Format as u32, 87 | 88)` for scanout eligibility (1769), `format: 21` D3DDDIFMT in the open fallback (2012), and check_format_support (6060+) maintains yet another family of tables. A wrong entry here is exactly the class of bug that produced the 4x-oversized A8 surfaces (comment 178-180).

**Evidence.** forward.rs:181-190 `60..=66 => 1, ... _ => 4` with comment "under-reporting an A8 mask as 4bpp is what made openers size these surfaces 4x too large"; :195-209, 211-224, 237-245, 247-261, 263-268, 270-294 (six more numeric tables); :722 `dev.scanout_format.set(87);`; :1769 `let is_scanout = !a.pPrimaryDesc.is_null() && matches!(a.Format as u32, 87 | 88);`; :156-158 local consts for 28/87/21.

**Recommendation.** Introduce a DxgiFormat newtype (wrapping the windows-crate DXGI_FORMAT) and one const FormatInfo table (bytes/bits per sample, family bits, resolvable, typeless-parent, integer, scanout-eligible) from which all seven functions become lookups; add a unit test asserting the new table reproduces the old functions' outputs for 0..=132 so the migration is provably behavior-preserving. Named consts replace 87/88/21 literals.

**Risk.** Low: pure functions with an exhaustive equivalence test; only risk is transcription, which the test eliminates.

**Dependencies.** R14 (split-forward-rs)

**Atomic commit boundary.** One commit adding format.rs with table + equivalence test; a follow-up replacing the literals at 722/1769/2012 with named consts.

**Validation.** cargo test (host-side) equivalence over 0..=132 for all seven predicates; release build; CheckFormatSupport-driven workloads (dxvk-tests, 3DMark FL10 run) unchanged; desktop gate items.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Format properties are encoded as unchecked numeric ranges in seven places; the invalid state is a new/edited entry disagreeing between tables (e.g. a format MSAA-advertised but sized wrong), detectable only as rendering corruption.
1. **Compile-time representation:** DxgiFormat newtype + single const FORMAT_INFO table; predicates become field reads so a format has exactly one description; scanout-eligibility becomes a named const set.
1. **Smallest atomic migration:** format.rs + mechanical call-site substitution; the equivalence unit test lands in the same commit.
1. **Remaining `unsafe` preconditions:** None — this area is safe code; what cannot be encoded is agreement with Windows' actual DXGI semantics, covered by the equivalence test freezing today's behavior.
1. **Regression test proving preserved behavior:** Table-equivalence test over 0..=132 for all seven predicates plus the desktop/cadence gate.


### R38. Adopted venus resource id still smuggled through HeliosWddmAllocPrivate::_pad on the create path

- **Category:** static-guarantee · **Reported by:** `umd-forward-a/pad-field-smuggling`
- **Files:** `umd/src/forward.rs`, `protocol/src/wddm.rs`
- **Symbols:** `allocate_wddm_resource`, `HeliosWddmAllocPrivate`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** The open path was fixed with the versioned HeliosWddmOpenIdentity (read_open_identity, 351-364, whose doc says it "replaces the _pad-smuggling heuristics"), but the create path still writes `private.alloc._pad = backing_resource_id;` (1370-1372) and every log prints `res_id={}` from `_pad` (1381, 1440). The protocol field itself is a misnomer with the truth in a trailing comment: `pub _pad: u32, // in: optional existing virtio resource id to adopt` (protocol/src/wddm.rs:60); protocol/src/wddm.rs:184 documents the smuggling. Nothing types the id: 0-as-none sentinel, and any u32 (a width, a format) can be assigned to it.

**Evidence.** forward.rs:1370-1372 `if backing_blob_id != 0 && backing_resource_id != 0 { private.alloc._pad = backing_resource_id; }`; :1381 logs `private.alloc._pad` as res_id; protocol/src/wddm.rs:60 `pub _pad: u32,    // in:  optional existing virtio resource id to adopt`; protocol/src/wddm.rs:184 "smuggling the venus resource id through `HeliosWddmAllocPrivate::_pad`"; forward.rs:348-350 doc: "This replaces the `_pad`-smuggling heuristics" (open side only).

**Recommendation.** Rename the field to adopt_resource_id (Rust field rename, wire layout byte-identical — same offset/size, shared protocol crate used by both UMD and KMD so one commit updates all readers/writers), and type it as Option<VirtioResourceId(NonZeroU32)> at the API surface with a #[repr(transparent)] u32 on the wire. The conditional at 1370 becomes construction of the typed value where backing is chosen.

**Risk.** Low-medium: touches the shared protocol crate → KMD rebuild required, and a KMD image change requires a guest reboot per the deployment contract. No wire bytes change.

**Dependencies.** guest reboot window (KMD rebuild); KMD version bump touches all three sites per CLAUDE.md

**Atomic commit boundary.** One commit across protocol + umd + kmd_render renaming the field and adding the layout test; no logic change.

**Validation.** Layout assertions (size_of/offset_of tests in protocol crate) proving byte-identical struct; KMD + UMD release builds; reboot; VpSA=1/ScSet=1, visible desktop, allocate_wddm_resource logs show identical res_id values as pre-change boot.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** "_pad carries the adoption resource id iff kind is DEVICE_MEMORY with a blob" lives in comments and one if; the invalid states permitted are assigning any unrelated u32 to _pad, or a reader treating real padding as an id (the pre-identity open bug class).
1. **Compile-time representation:** Field rename + VirtioResourceId(NonZeroU32) newtype with Option at the API layer, #[repr(transparent)] u32 on the wire; the KMD-side reader consumes the same typed field from the shared crate.
1. **Smallest atomic migration:** protocol crate + both consumers in one commit (they share the crate, so partial migration cannot compile).
1. **Remaining `unsafe` preconditions:** Wire trust: the KMD must still validate the id refers to a LIVE venus resource (it already does at open); a guest-supplied u32 cannot be proven live at compile time.
1. **Regression test proving preserved behavior:** protocol layout test (size/offset equality); same-boot create/open of a shared surface renders identically; WDDM_ALLOC log res_id values match the previous boot's pattern.

**Lead-reviewer note.** Wire-ABI-visible: the KMD reads this struct. Keep byte layout identical (rename _pad to a real field in protocol/ with a const assert), and coordinate with R32.


### R39. Escape dispatch on raw u32 constants plus 12 hand-copied size-check/read/write-back prologues

- **Category:** static-guarantee · **Reported by:** `kmd-submit/escape-verb-enum-prologue`
- **Files:** `kmd_render/src/ddi/escape.rs`, `protocol/src/escape.rs`
- **Symbols:** `dxgkddi_escape`, `HeliosEscapeHeader`, `HELIOS_ESCAPE_SUBMIT_VENUS`
- **Verification:** **MODIFIED** (severity medium) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** dxgkddi_escape matches hdr.cmd_type against 12 loose u32 consts with `_ => STATUS_NOT_IMPLEMENTED`; nothing forces a new protocol verb to be handled (HELIOS_ESCAPE_PRESENT_BLOB 0x0007 already exists in the protocol and silently falls into the wildcard). Every handler repeats the identical prologue: `if buf.len() < size_of::<T>() { return STATUS_BUFFER_TOO_SMALL; } let req: T = pod_read_unaligned(..)` and the write-back `buf[..sz].copy_from_slice(bytes_of(&out))` — 12 copies of the trust-boundary bounds logic that must never drift per-verb.

**Evidence.** escape.rs:77-92 `match hdr.cmd_type { HELIOS_ESCAPE_CTX_CREATE => ... _ => STATUS_NOT_IMPLEMENTED }`; protocol/src/escape.rs:29-57 loose consts incl. :38 `HELIOS_ESCAPE_PRESENT_BLOB: u32 = 0x0007` (unhandled, legacy Gate-7 op); prologue copies at escape.rs:101-105, 203-207, 274-278, 325-329, 385-387, 401-405, 421-425, 491-495, 525-529, 585-588, 608-612.

**Recommendation.** Add `#[repr(u32)] enum HeliosEscapeVerb` with `TryFrom<u32>` in protocol/src/escape.rs (wire ABI unchanged — ICD keeps sending the same integers); dispatcher matches the enum exhaustively so adding a verb is a compile error until handled, TryFrom Err -> STATUS_NOT_IMPLEMENTED. Add two tiny helpers `read_req::<T: Pod>(&[u8]) -> Result<T, NTSTATUS>` / `write_resp` so the bounds check exists once at the trust boundary.

**Risk.** Low: pure re-expression; the ICD-visible behavior (status codes per verb, unknown-verb rejection) is unchanged. Protocol crate builds on both platforms — keep the enum no_std-clean.

**Atomic commit boundary.** One commit: protocol enum + dispatcher + prologue helpers (handlers converted mechanically).

**Validation.** KMD + UMD/ICD builds on both toolchains; fence-event PROBE_ACK round-trip; QUERY_STATS/QUERY_SCANOUT read back sane values; desktop up (SUBMIT_VENUS is every frame); no new ring failures.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Verb completeness enforced only by convention; a new protocol const compiles while silently routing to STATUS_NOT_IMPLEMENTED. Per-verb bounds checks are 12 independent copies that can diverge (the exact class the historical per-arm drop bug came from).
1. **Compile-time representation:** Exhaustive enum + TryFrom at the single untrusted-integer decode point; Pod-bounded read/write helpers as the one trusted boundary for buffer slicing.
1. **Smallest atomic migration:** Protocol enum + KMD dispatcher in one commit; ICD callers untouched (u32 on the wire).
1. **Remaining `unsafe` preconditions:** The initial `from_raw_parts_mut(buf_ptr, buf_len)` trust of dxgkrnl's length cannot be encoded; guest payload contents stay untrusted and per-field validated.
1. **Regression test proving preserved behavior:** Escape smoke: probe ack, ctx create/destroy, alloc/map/release blob, submit+wait fence, stats v1 and v2 sizes — same NTSTATUS per case as baseline.

**Verifier corrections (authoritative).** 1) Prologue count: not "12 identical copies / every handler" — 10 handlers share the byte-identical prologue; escape_wait_fence (kmd_render/src/ddi/escape.rs:457-468, LEGACY_SIZE=32 dual-shape with raw offset reads at buf[16..24]/buf[24..32]) and escape_query_stats (escape.rs:325-330, dual 88-byte v1 / 152-byte v2) have deliberately divergent size gates that are documented deploy-order/version compat contracts (protocol/src/escape.rs:200-206, 355-359). The atomicity note "handlers converted mechanically" must explicitly exclude these two top-level gates — force-fitting read_req::<T> there returns STATUS_BUFFER_TOO_SMALL to legacy 32-byte WAIT_FENCE and v1 QUERY_STATS callers, an ABI break. The evidence's citation of 325-329 as an "identical prologue copy" is wrong. 2) read_req covers only the fixed prefix: escape_submit_venus's variable-length payload check (escape.rs:427-436, checked_add + <= buf.len()) stays handler-local; the helper does not subsume per-arm variable-length validation. 3) "silently falls into the wildcard" overstates: STATUS_NOT_IMPLEMENTED is a loud rejection AND a contractual capability-probe signal (protocol/src/escape.rs:278-280 — old-KMD detection depends on it); the TryFrom-Err mapping must preserve it (the recommendation does). 4) HELIOS_ESCAPE_PRESENT_BLOB has zero senders anywhere (Mesa ICD carries its own C #defines and never defines 0x0007; archived kmd/src, icd/src, umd/src reference no HELIOS_ESCAPE_* const) — exclude it from the enum so it stays a TryFrom Err, rather than adding a handler arm; do not delete the const/struct in the same commit (byte-ABI record, size assert at protocol/src/escape.rs:407). 5) Regression list must add: legacy 32-byte WAIT_FENCE shape and 88-byte v1 QUERY_STATS both still accepted with unchanged NTSTATUS/writeback.

**Lead-reviewer note.** Verified MODIFIED — mandatory exclusions: escape_wait_fence (legacy 32-byte dual shape) and escape_query_stats (v1/v2 dual size) keep their hand-written top-level size gates (documented ABI compat contracts); read_req covers only fixed prefixes (submit_venus variable-length check stays handler-local); STATUS_NOT_IMPLEMENTED for unknown verbs is contractual (capability probe) and must be preserved; HELIOS_ESCAPE_PRESENT_BLOB has zero senders — leave it out of the enum, do not delete the const/struct in the same commit.


### R40. escape_wait_fence decodes the legacy 32-byte shape via hardcoded byte offsets that silently depend on HeliosEscapeWaitFence's layout

- **Category:** static-guarantee · **Reported by:** `kmd-submit/wait-fence-legacy-layout`
- **Files:** `kmd_render/src/ddi/escape.rs`, `protocol/src/escape.rs`
- **Symbols:** `escape_wait_fence`, `HeliosEscapeWaitFence`
- **Verification:** **MODIFIED** (severity low) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** `LEGACY_SIZE: usize = 32`, `fence_id: u64 = pod_read_unaligned(&buf[16..24])`, `timeout_ns = pod_read_unaligned(&buf[24..32])` — three magic numbers re-deriving the protocol struct layout (16-byte header + two u64s), with the 'legacy struct is a strict prefix of the v2 struct' invariant living only in a comment. Any field reorder or header growth in protocol/src/escape.rs compiles cleanly and makes the KMD wait on garbage fence ids for legacy callers. Note the wait itself is a bounded PASSIVE KEVENT wait — a safety contract per the timeout doctrine, KEEP.

**Evidence.** escape.rs:461 `const LEGACY_SIZE: usize = 32;`; :466-468 `// The legacy struct is a strict prefix of the v2 struct ... let fence_id: u64 = pod_read_unaligned(&buf[16..24]); let timeout_ns: u64 = pod_read_unaligned(&buf[24..32]);`; protocol/src/escape.rs HeliosEscapeWaitFence = hdr + fence_id + timeout_ns + out_completed + _pad (40-byte const assert exists, no prefix assert).

**Recommendation.** Define `#[repr(C)] HeliosEscapeWaitFenceLegacy { hdr, fence_id, timeout_ns }` (Pod) in protocol/src/escape.rs next to the v2 struct, with const asserts: size == 32, and offset_of equality for fence_id/timeout_ns against HeliosEscapeWaitFence (prefix proof). escape_wait_fence reads the legacy struct via pod_read_unaligned of the 32-byte prefix; the three magic numbers disappear.

**Risk.** Minimal; wire bytes and semantics identical, compile-time proof only.

**Atomic commit boundary.** One commit: protocol struct + asserts + escape.rs read.

**Validation.** Builds both platforms (protocol crate); WAIT_FENCE round-trip via current ICD; forced-timeout case still reports out_completed=0 / STATUS_IO_TIMEOUT for legacy size.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Prefix-layout equivalence between the legacy and v2 wait structs asserted only by comment; a protocol edit permits decoding fence_id from the wrong offset with no diagnostic.
1. **Compile-time representation:** Explicit legacy Pod struct + const offset/size asserts proving strict-prefix at build time.
1. **Smallest atomic migration:** One commit across protocol + escape.rs (protocol builds on both platforms, no ABI change).
1. **Remaining `unsafe` preconditions:** None new; the buffer contents remain untrusted and bounds-checked as today.
1. **Regression test proving preserved behavior:** ICD wait-fence smoke (normal completion + forced timeout) with both v2 and a synthetic 32-byte legacy call, statuses matching baseline.

**Verifier corrections (authoritative).** Exact lines: LEGACY_SIZE is escape.rs:458 (not 461); the hardcoded reads are :467-468 (466 is the comment); the prefix-comment is :464-466. Overbroad/incorrect failure scenario, three corrections: (1) "header growth ... compiles cleanly" is FALSE — protocol/src/escape.rs:397-411 is a `const _: () = { assert!(...) }` block with `size_of::<HeliosEscapeHeader>() == 16` (:398) and `size_of::<HeliosEscapeWaitFence>() == 40` (:406); any header growth fails both asserts at compile time. Only a size-preserving field REORDER inside the struct (e.g. swapping the two u64s, or moving out_completed+_pad before fence_id) compiles cleanly. (2) The hazard is NOT limited to "legacy callers": escape.rs:467-468 are the ONLY reads of fence_id/timeout_ns — the v2 path uses them too; the struct read at :478 is used solely to write back out_completed. (3) The claimed concrete failure ("KMD waits on garbage fence ids" after a Rust-side reorder) is wrong for the actual wire: the producer is a C mirror struct in icd/mesa/src/virtio/vulkan/vn_renderer_helios.c:199-205 (with its own _Static_assert == 40 at :260, always sending the 40-byte v2 shape at :1533-1534), so a Rust-only reorder leaves the hardcoded 16/24 offsets still matching the C wire — the real Rust-reorder breakage is the v2 out_completed write-back at :478/:484 landing at the wrong wire offset; conversely a C-side reorder (which no Rust assert can catch) is what would produce garbage fence ids. Tightened claim: the recommendation is a real but Rust-internal-only static guarantee — a legacy Pod prefix struct plus core::mem::offset_of const asserts (fence_id == 16, timeout_ns == 24; also assert out_completed == 32, which the finding omits and which is the field a reorder actually breaks today) eliminates the three magic numbers and pins escape.rs's reads to the struct, matching the crate's existing const-assert house style; it CANNOT protect the true ABI boundary, which is the hand-mirrored C struct in the mesa ICD. Behavior: bit-identical reads/statuses, no ABI change, no invariant touched; the bounded PASSIVE KEVENT wait is correctly classified KEEP (safety contract per timeout doctrine) and is untouched. Severity low: no current bug, size asserts already catch the most likely drift (header/size changes), and the cross-language gap the asserts cannot close is the dominant residual risk.

**Lead-reviewer note.** Verified MODIFIED — the guarantee is Rust-internal only: offset_of const asserts (fence_id==16, timeout_ns==24, AND out_completed==32 — the field a reorder actually breaks) pin escape.rs to the struct; the true ABI boundary is the hand-mirrored C struct in the mesa ICD, which no Rust assert can check — record that as the residual risk.


### R41. Descriptor-span layout is reconstructed by hand in four places (three enqueues + drain_used) and must match by discipline; buffer-shape combinations are open, not exhaustive

- **Category:** static-guarantee · **Reported by:** `kmd-transport-gpu/inflight-span-single-source`
- **Files:** `kmd_render/src/virtio/gpu.rs`, `kmd_render/src/virtio/hal.rs`
- **Symbols:** `VirtioGpu::enqueue_sync`, `VirtioGpu::enqueue_async_control`, `VirtioGpu::enqueue_async_submit`, `VirtioGpu::drain_used`, `InFlight`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Each enqueue builds `[in0|in1?|resp]` (+ optional venus) spans via raw `from_raw_parts` over a DmaBuffer, calls `control.add`, then separately stores the same lengths into `InFlight`. `drain_used` re-derives the spans from the stored lengths and must pass `pop_used` EXACTLY what `add` got — enforced only by the comment at gpu.rs:1317-1318. The shape space is open: an entry with both `in1_len>0` and `venus_len>0` is representable but drain's if/else-if (1325-1333) would pop wrong spans → `failed` latch. The three enqueues also duplicate the failed-check/length-validation/capacity-gate/QueueFull-mapping block, and are inconsistent about doorbell ordering: enqueue_sync notifies BEFORE pushing to `inflight` (1094-1097) while the async paths push first with a comment claiming the order matters (1183-1187) — in fact the device spinlock serializes against the drain, so the comments justify an ordering the lock already makes irrelevant. Finally, hal.rs:270-274 documents (comment-only) that every queued buffer must be dma_alloc'd contiguous or `share`'s single-base physical translation is wrong.

**Evidence.** gpu.rs:1317-1318 "// SAFETY: exactly the spans `add` was called with; the entry still owns both buffers." gpu.rs:1325-1333 `if venus_len > 0 {..} else if in1_len > 0 {..}` (both-set entry mishandled). gpu.rs:1094-1097 notify-then-push in enqueue_sync vs gpu.rs:1183-1184 "Publish token ownership before ringing the device doorbell" (push-then-notify) in enqueue_async_control. Triplicated gate: gpu.rs:1064-1067, 1142-1145, 1217-1220. hal.rs:271-273 "Buffers handed to the queue are always `dma_alloc`'d (contiguous), so a single physical base is valid for the whole buffer." (comment-only contract).

**Recommendation.** Introduce an exhaustive `enum InFlightBuffers { Ctrl { meta: DmaBuffer, in0: usize, resp: usize }, CtrlWithInline { .., in1: usize }, Submit { meta: DmaBuffer, venus: DmaBuffer, venus_len: usize, .. } }` with one method `fn with_spans<R>(&self, f: impl FnOnce(&[&[u8]], &mut [&mut [u8]]) -> R) -> R` used by BOTH add and pop_used, so the add/pop span identity is definitional, invalid shape combinations are unrepresentable, and the only-DmaBuffer-backed contract required by hal.rs `share` is carried by the type. Factor the shared validate/gate/push/notify epilogue into one private `queue_entry` helper with a single (documented, lock-based) doorbell ordering.

**Risk.** Medium — this is the hot path for every Venus submission; a mistake latches `failed` and kills the transport. Mitigate by keeping the span math byte-identical and landing as one reviewed commit.

**Atomic commit boundary.** One commit: introduce InFlightBuffers + with_spans, convert the three enqueues and drain_used together (they cannot be converted independently).

**Validation.** Boot to desktop; DRAIN_BAD_TOKEN=0 and `failed` never latches across a full desktop session + DOOM run; ASYNC_SUBMIT_COUNT/ASYNC_COMPLETE_COUNT advance in lockstep; no new present-gate timeouts; 63 fps DComp cadence.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** pop_used must receive byte-identical spans to add, and only DmaBuffer-backed contiguous memory may be queued; both enforced by comments. Permitted invalid states: an InFlight with in1_len>0 AND venus_len>0 (drain pops wrong spans → failed latch, transport dead), or a future caller queuing a non-contiguous slice (share() returns wrong phys base → device DMA to wrong pages).
1. **Compile-time representation:** Exhaustive `InFlightBuffers` enum owning the DmaBuffers, with one `with_spans` accessor used by both add and pop; enqueue APIs accept the enum, making shape and backing definitional.
1. **Smallest atomic migration:** gpu.rs only: three enqueue methods + drain_used + InFlight struct in one commit; ctrl.rs/venus.rs call signatures can stay (constructors build the enum internally).
1. **Remaining `unsafe` preconditions:** The `from_raw_parts` reconstruction itself stays unsafe (virtio-drivers' add/pop take slices, and the borrows must not be held across the entry's Vec move) — the enum shrinks the trusted boundary to one function but cannot remove it.
1. **Regression test proving preserved behavior:** Full desktop session + game workload with DRAIN_BAD_TOKEN=0, transport never latching failed, identical ASYNC_* counter progression; same-boot QEMU evidence of the OPTIMAL DWM primary.



---

## Part II, Tranche 6 — Static guarantees: typestate, RAII, sealed interfaces

The structural core of the static-guarantee axis: validate-once descriptors, typed publish protocols, consume-on-queue ownership, proof tokens, and sealing diagnostic/fallback resources out of the exact-primary path. Order within the tranche: the two scanout-identity foundations (R42, R44) first — half the tranche depends on their types — then KMD transport/display/alloc items, then the UMD family. Every entry here must honor the handoff's anti-cosmetic rule: a wrapper that merely relocates an unchecked cast does not land.

**Regression-gate emphasis:** full gate after each sub-batch; for anything touching scanout identity or the present path, same-boot QEMU evidence of the actual OPTIMAL DWM primary (not a diagnostic fill) is mandatory.

### R42. Primary scan-out validation is triplicated and the direct-vs-fallback identity is an unchecked bool; introduce a validate-once ValidatedScanout constructor and a sealed DirectPrimary/FallbackLinear enum

- **Category:** static-guarantee · **Reported by:** `xc-duplication/validated-scanout-descriptor`
- **Merged duplicate reports (5):** `kmd-display/validated-scanout-descriptor` — Scanout descriptor (format/extent/pitch/offset/size/exportability) is validated ad hoc at three divergent sites instead of once by a constructor; `xc-errors/validated-scanout-descriptor` — Scanout primary validation (pitch/offset/size/format 87|88) duplicated at three sites as loose scalars; introduce a validate-once ValidatedScanout descriptor; `xc-concurrency/validated-scanout-descriptor` — Scanout-candidate validation (pitch/format/extent/min-size/stride-fallback) is duplicated across four arms; no validate-once descriptor type; `xc-unsafe/validated-scanout-descriptor` — Scanout validation (format 87/88, pitch, offset, size) duplicated across 3 KMD sites with re-declared magic constants; `kmd-alloc/scanout-target-sealed-enum` — direct-vs-fallback scanout identity is a bool on ScanoutInfo with ad-hoc downstream validation instead of a validate-once sealed descriptor
- **Files:** `kmd_render/src/ddi/display.rs`, `kmd_render/src/ddi/create_allocation.rs`, `kmd_render/src/ddi/submit_command.rs`, `protocol/src/wddm.rs`
- **Symbols:** `issue_present_scanout`, `dxgkddi_set_vidpn_source_address`, `dxgkddi_render`, `ScanoutInfo`, `HeliosPresentPrivateData`, `set_scanout_blob`, `publish_scanout_candidate`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 6 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** The geometry/format contract for the exact DWM primary (stride >= width*4, stride%4==0, plane_offset <= u32::MAX, alloc_size >= offset+stride*height, dxgi_format in {87,88}, w/h == mode) is re-implemented ad hoc at three sites: display.rs:186-201 (issue_present_scanout), display.rs:703-714 (SetVidPnSourceAddress direct arm), and submit_command.rs:511-532 (DxgkDdiRender HeliosPresentRenderCmd arm, which recomputes required_size itself). ScanoutInfo carries `direct_scanout: bool` (create_allocation.rs:246) and HeliosPresentPrivateData a reserved bit, so downstream code distinguishes the Windows-designated primary from the LINEAR fallback only by boolean tests, and any struct can reach ctrl::set_scanout_blob / publish_scanout_candidate partially validated if one site's checks drift (they already differ in which fields they check).

**Evidence.** display.rs:186 'let min_size = plane_offset.saturating_add((stride as u64).saturating_mul(height as u64));' vs display.rs:704-710 'let valid = source.pitch >= width.saturating_mul(4) && source.pitch & 3 == 0 && source.plane_offset <= u32::MAX as u64 && source.venus_alloc_size >= min_size && matches!(source.dxgi_format, 87 | 88);' vs submit_command.rs:514-516 'let required_size = private.plane_offset.saturating_add((private.pitch as u64).saturating_mul(private.height as u64));'. Bool identity: create_allocation.rs:246 'pub direct_scanout: bool'; display.rs:180 'if !direct_scanout { ... return false; }'.

**Recommendation.** Add a KMD `ValidatedScanout` type with private fields and one `try_new(info, mode_wh) -> Result<...>` constructor performing the full check-set once, plus `enum ScanoutSource { DirectPrimary(ValidatedScanout), FallbackLinear(ValidatedScanout) }` replacing the bool. Make `ctrl::set_scanout_blob{,_async}`, `publish_scanout_candidate`, and `submit_primary_scanout_copy` accept only the validated type. All three call sites become constructor calls + match; failure arms keep their exact counters (ScSet=0xE3/0xD, PScSet).

**Risk.** Behavior drift if the consolidated check-set is the union rather than the exact per-site set — the three sites intentionally differ (Present arm tolerates width==0 -> mode fill-in; SetVidPn rejects w/h != mode). Encode mode fill-in inside the constructor and keep per-site strictness via a parameter so each caller's accept/reject behavior is bit-identical.

**Atomic commit boundary.** One commit: introduce ValidatedScanout + ScanoutSource and convert the two display.rs arms; a second commit converts the submit_command.rs render arm.

**Validation.** KMD build; boot with visible desktop; VpSA=1, ScSet=1, ScRid follows primary rotation; ScSet=0xE3/0xD and PScSet counters stay 0 in steady state; forced fallback (non-direct primary) still lights up via ScCpy=1; DComp cadence ~63 fps; no new gate timeouts; same-boot QEMU evidence of the exact OPTIMAL primary (no diagnostic fill).

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Only a fully-validated, Windows-designated primary may be bound via SET_SCANOUT_BLOB or published as a scanout candidate; enforced today by three hand-copied if-chains plus a `direct_scanout` bool. Permitted invalid sequence: a caller passes an unvalidated or fallback ScanoutInfo (or a drifted check-set omits one field) straight into publish_scanout_candidate/set_scanout_blob, letting a fallback/diagnostic or malformed descriptor enter the exact-primary path.
1. **Compile-time representation:** ValidatedScanout with private fields, sole constructor try_new(); sealed enum ScanoutSource{DirectPrimary,FallbackLinear}; scanout-binding APIs take only these types, so an unvalidated or wrong-kind descriptor is unrepresentable at the binding boundary.
1. **Smallest atomic migration:** Commit 1: type + display.rs conversion (both arms). Commit 2: submit_command.rs render arm.
1. **Remaining `unsafe` preconditions:** scanout_alloc_info's raw hAllocation cast + magic check stays unsafe (dxgkrnl round-trips an opaque pointer; cannot be typed). Field values originate from ICD-supplied meta, so validation remains runtime — the guarantee is single-point, non-bypassable validation, not proof of host acceptance.
1. **Regression test proving preserved behavior:** Same-boot QEMU screenshot of the real OPTIMAL primary; ScSet=1/ScRid rotation; ScSet error counters zero; fallback-copy path still functional (ScCpy=1 when forced).

**Lead-reviewer note.** Six reports — the highest-value static guarantee in the review and an explicitly sanctioned handoff pattern (validate-once scanout descriptor + sealed DirectPrimary/FallbackLinear identity). Design it together with R43's verified corrections (four issue_present_scanout call sites; two synthesize size from UMD-supplied bytes) and R44 (publication).


### R43. issue_present_scanout takes 9 positional primitives with divergent size semantics per call site; the alloc-size check is vacuous on the Render path

- **Category:** static-guarantee · **Reported by:** `kmd-submit/scanout-request-descriptor`
- **Files:** `kmd_render/src/ddi/submit_command.rs`, `kmd_render/src/ddi/scheduler.rs`, `kmd_render/src/ddi/display.rs`
- **Symbols:** `issue_present_scanout`, `dxgkddi_render`, `dxgkddi_present_to_hw_queue`, `ScanoutInfo`
- **Verification:** **MODIFIED** (severity medium) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** Three call sites assemble (resource_id,width,height,pitch,dxgi_format,plane_offset,venus_alloc_size,direct_scanout,via) positionally. scheduler.rs passes the authoritative sc.venus_alloc_size; submit_command.rs synthesizes required_size = plane_offset + pitch*height from UMD-supplied command bytes and passes it as venus_alloc_size — display.rs then checks `venus_alloc_size < min_size` where min_size is the identical formula, so the size check is tautologically satisfied on the Render path (host SET_SCANOUT_BLOB is the only backstop). `via` is a bare magic tag (3, 4). Adjacent u32s invite silent transposition; nothing distinguishes declared vs authoritative size.

**Evidence.** display.rs:142-153 nine-arg signature ending `direct_scanout: bool, via: u32`; :186 `min_size = plane_offset.saturating_add((stride as u64).saturating_mul(height as u64))` with :192 `venus_alloc_size < min_size`; submit_command.rs:514-516 `required_size = private.plane_offset.saturating_add((private.pitch as u64).saturating_mul(private.height as u64))` fed as the venus_alloc_size arg at :521-532 with trailing magic `4`; scheduler.rs:266-277 passes `sc.venus_alloc_size` with trailing magic `3`.

**Recommendation.** Introduce a validated ScanoutRequest constructor (per REFACTOR_HANDOFF 'validated scanout descriptor'): newtypes for ResourceId/PitchBytes/PlaneOffset, `via` as an exhaustive enum (RenderMarker, HwQueuePresent, Present, SetVidPnSourceAddress), and size provenance as enum { Authoritative(u64), DerivedFromGeometry } so the vacuous check is visible in the type. Constructor performs the existing format/extent/stride/offset checks once; issue_present_scanout takes the descriptor. Keep acceptance behavior bit-identical (DerivedFromGeometry arm keeps today's pass-through); tightening via the KMD blob table is a separate, explicitly gated follow-up.

**Risk.** Medium-touch across the exact-primary path. Must not alter which presents publish: any check reordering that newly rejects a DWM primary blanks the desktop. Keep validation predicate byte-for-byte equivalent.

**Dependencies.** R5 (dead-wait-gpu-refresh-path)

**Atomic commit boundary.** One commit adding the descriptor type + converting the three call sites; no predicate changes.

**Validation.** Same-boot QEMU evidence of the OPTIMAL DWM primary; VpSA=1/ScSet=1; PScSet counters unchanged in kind (no new 0xE3/0xD rejects at steady state); 63 fps cadence; cursor without trails.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Argument order/meaning enforced only by position and comments; 'venus_alloc_size is the real blob size' holds at two of three call sites and silently does not hold at the Render marker site, permitting an undersized-geometry publish to pass the KMD check.
1. **Compile-time representation:** ScanoutRequest built by a validate-once constructor; PitchBytes/PlaneOffset/ResourceId newtypes; SizeProvenance enum; PublishVia exhaustive enum replacing the via integer.
1. **Smallest atomic migration:** One commit: type in display.rs (or a new scanout_desc.rs), three call-site conversions, zero predicate changes.
1. **Remaining `unsafe` preconditions:** The UMD-supplied HeliosPresentRenderCmd fields remain untrusted guest data; the type can mark them Derived but cannot prove them true — authoritative cross-check against the KMD blob table stays a runtime (follow-up) check.
1. **Regression test proving preserved behavior:** Boot with counters: PScVia distribution identical per path; publish count parity vs baseline over a fixed DComp run; visible desktop + exact-primary QEMU evidence.

**Verifier corrections (authoritative).** 1) FOUR call sites, not three: the finding missed display.rs:329-340 (via=1, DxgkDdiPresent alloc-list path, authoritative sc.venus_alloc_size) and display.rs:373-393 (via=2, DxgkDdiPresent private-driver-data path) — and via=2 ALSO synthesizes required_size = plane_offset + pitch*height from UMD-supplied HeliosPresentPrivateData (display.rs:379-381), so the vacuous size check exists at TWO of four sites (via=2 and via=4), not only the Render marker; authoritative size holds at via=1 and via=3. Migration is four call-site conversions, not three. 2) PublishVia enum variants wrong: there is no SetVidPnSourceAddress caller of issue_present_scanout — set_vidpn_source_address has its own inline near-duplicate predicate at display.rs:702-714 (venus_alloc_size >= min_size at :709, authoritative source); the Present variant must split into PresentAllocList (via=1) and PresentPrivateData (via=2); actual codes are 1/2/3/4. Unifying the :703-710 duplicate predicate into the same constructor is a legitimate extension but must stay byte-identical and is out of the stated one-commit scope. 3) 'Tautologically satisfied' holds only in the nominal case (pitch != 0 && height != 0); with degenerate zeros display.rs substitutes cross_adapter_pitch(width)/mode_h so min_size exceeds the synthesized size and the check rejects — the precise claim is that the check carries no independent information about the real allocation size on the derived paths (host SET_SCANOUT_BLOB is the only backstop). 4) Missing risk: PScVia diag records the raw via integer (display.rs:172/181/194/216); the enum must map to the same numeric codes 1-4 or cross-boot diag-ring analysis breaks. Supporting observation strengthening the design: direct_scanout provenance diverges identically (KMD-owned AllocationContext at via=1/3 vs UMD-controlled private.reserved flag at via=2/4).

**Lead-reviewer note.** Verified MODIFIED — corrections are load-bearing: FOUR call sites (via=1 alloc-list and via=3 authoritative; via=2 private-data and via=4 render-marker synthesize required_size from UMD-supplied bytes, so the size check is vacuous at TWO sites); set_vidpn_source_address has its own inline near-duplicate predicate (display.rs:702-714) that may be unified only byte-identically and outside the first commit; the PublishVia enum must map to the existing numeric diag codes 1-4 or cross-boot PScVia analysis breaks.


### R44. Hand-rolled multi-atomic publish protocols for scanout/prepared-copy identity (comment-enforced 'publish word' + packed u64s) should become one typed seqlock-style PublishedCell

- **Category:** static-guarantee · **Reported by:** `xc-duplication/scanout-publish-cell`
- **Merged duplicate reports (5):** `kmd-core/scanout-identity-atomic-sprawl` — Scanout identity published across 5-8 independent atomics with a comment-enforced "publish word" protocol and a heuristic tear re-check; `xc-errors/scanout-identity-publish-word` — Four hand-rolled multi-word atomic 'publish word' families in AdapterContext enforce snapshot coherence only by comment-documented load ordering; `xc-concurrency/scanout-binding-atomics` — Four parallel scanout-identity families are hand-packed atomics (wh=(w<<32)|h, layout=(pitch<<32)|offset) kept coherent only by publish-word comments; `xc-unsafe/scanout-publication-snapshot-type` — Multi-atomic scanout publication protocols with per-site ad hoc coherence conventions; `xc-legacy/scanout-identity-static` — Scanout identity/validity is 20+ parallel atomics, a direct_scanout bool, and validation duplicated in three places — encode a validated descriptor and a sealed scanout-source enum
- **Files:** `kmd_render/src/adapter.rs`, `kmd_render/src/ddi/create_allocation.rs`, `kmd_render/src/ddi/display.rs`
- **Symbols:** `remember_primary_scanout`, `publish_scanout_candidate`, `remember_scanout_blob`, `remember_diag_scanout_blob`, `queue_active_scanout_refresh`, `publish_prepared_copy`, `cached_prepared_copy`, `production_linear_scanout`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 6 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** Four independent identity clusters (active_scanout_* 4 atomics, primary_scanout_* 7, dedicated_scanout_* 3, diag_scanout_* 3 in adapter.rs:204-243; scanout_copy_* 7 in create_allocation.rs:61-67) each re-implement 'store companions Relaxed, store the id last Release, readers re-check the publish word'. Width/height and pitch/offset are hand-packed into AtomicU64s and decoded at every reader (adapter.rs:503-533, 621-633; display.rs:54-70). Only queue_active_scanout_refresh (adapter.rs:621-629) actually re-checks the publish word after sampling; production_linear_scanout (display.rs:52-74) reads companions with Relaxed and no re-check, so a concurrent republish can produce a torn identity — nothing but comments (adapter.rs:536-537 'resource_id is stored last...', create_allocation.rs:300 'command_buffer_id is the publish word') stops the next reader from doing it wrong.

**Evidence.** adapter.rs:536-537 '`resource_id` is stored last so an acquire reader never combines a new id with stale geometry'; adapter.rs:625-628 'A newer present may publish while we sample the companion fields. Retry from the worker rather than combine two primary identities.'; create_allocation.rs:300-301 'command_buffer_id is the publish word. A reader that acquires a nonzero command id sees one coherent immutable PreparedImageCopy snapshot.'; display.rs:62-70 reads primary_scanout_layout/alloc_size/memory_type with Relaxed and no post-sample re-check; packing convention 'wh = ((width as u64) << 32) | height' repeated at adapter.rs:503, 526, 550, 602.

**Recommendation.** Introduce one small generic `PublishedCell<T: Copy>` (seqlock: version counter + payload behind a spinlock-free protocol, or a tiny KSPIN_LOCK-guarded struct for PASSIVE/DISPATCH writers) with `publish(T)` / `read() -> Option<T>` as the only API, and typed payloads (ScanoutIdentity{res_id,w,h,pitch,offset,format}, PrimaryScanoutIdentity{..alloc_size,mem_type,dxgi,generation}, PreparedCopyIds). Delete the packed-u64 conventions and per-site decode.

**Risk.** The cell is read from the DPC-adjacent worker and published from PASSIVE DDIs; a naive lock would violate the no-wait rule in DISPATCH readers. Seqlock read must be bounded-retry returning Option (retry at caller), matching today's Busy semantics. Migrate one cluster per commit to keep A/B bisectable.

**Atomic commit boundary.** Commit 1: PublishedCell + active_scanout_* cluster (publisher publish_scanout_candidate/remember_scanout_blob + reader queue_active_scanout_refresh). Commits 2-4: primary_/dedicated_/diag_ clusters, then scanout_copy_* prepared-copy cluster.

**Validation.** Visible desktop, cursor without trails, idle-to-active responsiveness; ScRid/RfRid follow flips; no new RbFail/RfFail increments; DComp ~63 fps; dwm restart and adapter restart clean.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** A reader must never combine fields from two different published identities; enforced by a store-order + re-check convention that each of ~6 read sites must re-implement. Invalid sequence permitted: read companions, concurrent republish, use id from the new publish with old geometry (production_linear_scanout has no re-check today).
1. **Compile-time representation:** PublishedCell<T> whose read() returns a whole T snapshot or None; individual fields are not independently addressable, so a torn combination is unrepresentable in safe code.
1. **Smallest atomic migration:** One cluster (active_scanout_*) with both its writers and its single reader in one commit.
1. **Remaining `unsafe` preconditions:** None new; the cell uses the same atomics. The DISPATCH-context bounded-retry semantics (return None on contention) remain a runtime behavior the caller must handle, as today.
1. **Regression test proving preserved behavior:** Rapid cursor motion + window drag with no stale-frame flicker; RbFail/RfFail stay flat; primary rotation under dwm restart shows ScRid tracking each flip.

**Lead-reviewer note.** Six reports. One typed seqlock-style PublishedCell replaces the four hand-rolled multi-atomic publish families. This is the riskiest single change in the review (it touches the frozen refresh-marker/scanout publication contract): land per-family, each behind the full regression gate with same-boot QEMU primary evidence.


### R45. ScanoutDiag reaches into the production scanout publication state and hooks the exact-primary bind path with per-flip registry reads

- **Category:** static-guarantee · **Reported by:** `kmd-display/seal-scanout-diag-out-of-primary-path`
- **Merged duplicate reports (2):** `xc-errors/scanout-diag-primary-path-sealing` — ScanoutDiag hook does an uncached registry read on every SetVidPnSourceAddress and can silently substitute the diagnostic blob for the real primary; seal it out of the exact-primary path; `kmd-venus/scanout-diag-mode-enum` — ScanoutDiag raw-integer mode comparisons and registry reads scattered through production venus code; diagnostic allocators not sealed from the production path
- **Files:** `kmd_render/src/ddi/scanout_diag.rs`, `kmd_render/src/ddi/display.rs`, `kmd_render/src/adapter.rs`, `kmd_render/src/ddi/start_device.rs`
- **Symbols:** `rebind_if_forced`, `maybe_run`, `AdapterContext::remember_scanout_blob`, `active_scanout_resource`, `host_bound_scanout_resource`, `diag_mode`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 3 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** The handoff explicitly wants a sealed interface keeping diagnostic/fallback resources out of the exact-primary path; today the coupling is only a runtime knob. (1) dxgkddi_set_vidpn_source_address calls scanout_diag::rebind_if_forced on every bind (display.rs:747); rebind_if_forced calls diag_mode() — an uncached RtlQueryRegistryValues read — up to 3 times per call (scanout_diag.rs:480, 503, 514), i.e. a synchronous registry read on the flip path even in production (mode 0). adapter.rs:620 elsewhere states the rule this breaks: 'never query the registry on every frame'. (2) Diag blobs write the production publish words: scanout_diag calls adapter.remember_scanout_blob (scanout_diag.rs:156, 199, 294, 353, 473, 509, 529) which stores into active_scanout_resource AND host_bound_scanout_resource (adapter.rs:502-511). (3) All scanout publication atomics are pub fields on AdapterContext (adapter.rs:204-243), so any module can bypass the publish/bind discipline; queue_active_scanout_refresh even needs a re-read retry to defend torn multi-atomic snapshots (adapter.rs:625-628). (4) When armed (mode>=2) rebind_if_forced hijacks the primary bind and SVSA returns STATUS_SUCCESS without ever binding the Windows primary — intended, but unrepresented in types.

**Evidence.** display.rs:747 'if crate::ddi::scanout_diag::rebind_if_forced(adapter, 11) { return STATUS_SUCCESS; }' with scanout_diag.rs:18-20 'fn diag_mode() -> u32 { crate::diag::read_config_dword(b"ScanoutDiag", 0) }' called at 480, 503, 514. scanout_diag.rs:473 'adapter.remember_scanout_blob(blob.res_id, width, height);' -> adapter.rs:506-511 stores active_scanout_resource and host_bound_scanout_resource. adapter.rs:620 '// ...never query the registry on every frame.' adapter.rs:625-628 'A newer present may publish while we sample the companion fields. Retry from the worker rather than combine two primary identities.'

**Recommendation.** Seal the publication surface: move active_scanout_*/host_bound_*/primary_scanout_*/diag_scanout_* into a private ScanoutPublishState owned by AdapterContext, exposing only publish_candidate(ScanoutSource::ExactPrimary...), remember_bound(fallback), and a separate DiagScanout type whose methods can only touch diag_* state plus an explicit, loudly-counted override entry point. Cache the ScanoutDiag knob once at StartDevice into an enum (read where maybe_run already runs, start_device.rs:255) so the production SVSA hook is a single cached atomic load, zero registry I/O; diag_mode() is then read exactly once per boot, preserving today's per-boot semantics (the knob already only takes effect after adapter restart for maybe_run). Keep behavior identical for every mode value.

**Risk.** Low-medium: field privatization touches display.rs, hpd.rs, interrupt/submit paths that read the publish words; must be a mechanical accessor migration with no ordering changes (Release publish-word stores preserved).

**Dependencies.** R42 (validated-scanout-descriptor)

**Atomic commit boundary.** Two commits: (a) cache the ScanoutDiag knob at StartDevice + single-read rebind_if_forced; (b) privatize publish state behind ScanoutPublishState and convert scanout_diag to the DiagScanout type.

**Validation.** Production boot: ScanoutDiag absent, VpSA=1/ScSet=1, desktop visible, cadence intact; per-flip cost drop measurable (registry read removed); diag boot with ScanoutDiag=2 and =16 still shows color bars and SdgRSet=1.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** 'Diagnostic resources never enter the exact-primary publication state in production' is enforced only by a registry knob checked at runtime; invalid sequences permitted: any module storing to the pub atomics directly, diag writing active/host_bound words, and torn companion-field reads defended by a retry heuristic.
1. **Compile-time representation:** Private ScanoutPublishState with sealed publish/bind/flush methods taking ValidatedScanoutDescriptor; distinct DiagScanout type with no path to the production publish words except one named override method; knob as a once-cached enum.
1. **Smallest atomic migration:** adapter.rs field privatization + call-site conversion in display.rs/hpd.rs/scanout_diag.rs in one commit; knob caching is separable and can land first.
1. **Remaining `unsafe` preconditions:** None new; the multi-atomic snapshot consistency remains a runtime protocol (publish-word Release ordering) unless later replaced by a generation/seqlock — the type seals writers but cannot prove reader snapshot atomicity.
1. **Regression test proving preserved behavior:** Frozen-baseline gate: 'ScanoutDiag absent, VpSA=1, ScSet=1' on production boot plus a diag-mode boot showing bars — both same-boot registry evidence.

**Lead-reviewer note.** The handoff's sealed-interface pattern verbatim: after R7/R13, make diagnostic resources UNABLE to reach the production publish state by type (diag handles are a different type than ValidatedScanout; no conversion exists outside the sealed module). Guarantees the 'ScanoutDiag absent during primary tests' baseline statically.


### R46. BlobSlot lifecycle is interdependent booleans (mapped/map_pending) plus sentinel owner=0/ctx_id=0 and zeroed offset/len/cache; multi-phase map protocol and tuple-soup returns are comment-enforced

- **Category:** static-guarantee · **Reported by:** `kmd-transport-gpu/blob-slot-state-enums`
- **Merged duplicate reports (1):** `xc-duplication/blob-map-typestate` — BlobSlot lifecycle is two booleans + zero sentinels, and multi-phase map/create flows rely on caller-discipline finish/cancel calls — encode as a MapState enum and RAII reservation guards
- **Files:** `kmd_render/src/virtio/gpu.rs`
- **Symbols:** `BlobSlot`, `VirtioGpu::blob_map_begin`, `VirtioGpu::blob_map_finish`, `VirtioGpu::blob_remap_begin`, `VirtioGpu::take_blob_for_owner`, `VirtioGpu::note_blob_size`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** BlobSlot (gpu.rs:414-439) carries `mapped: bool`, `map_pending: bool`, `map_cache/map_offset/map_len` that are only meaningful in certain boolean states ("valid once mapped"), and `owner: usize` where 0 is a sentinel meaning KMD-internal (gpu.rs:1995-1998 "Record with ctx_id 0 / owner 0"). mapped&&map_pending is representable; readers of map_offset/map_cache on an unmapped slot silently get 0. The begin→host-roundtrip→finish protocol is correlated across gpu.rs and ctrl.rs by matching `(resource_id, map_pending, map_offset)` (gpu.rs:2078-2091), and finish-without-begin is expressible. Accessors return positional tuples callers must decode: `take_blob_for_owner -> Option<(u32, u32, bool, u64, u64)>` (gpu.rs:1937), `forget_allocation_blob -> Option<(bool, u64, u64)>` (gpu.rs:2270). Adjacent same-typed params (`blob_map_finish(resource_id, offset, len, ..)` — two u64s) invite transposition.

**Evidence.** gpu.rs:428-437 "mapped: bool, /// A RESOURCE_MAP_BLOB round-trip is in flight... map_pending: bool, /// Host caching nibble from RESP_OK_MAP_INFO (valid once `mapped`). map_cache: u32". gpu.rs:1995-1998 "Record with ctx_id 0 / owner 0: these blobs are not driven by an escape device handle". gpu.rs:2081 finish matches `s.map_pending && s.map_offset == offset` (protocol by field correlation). gpu.rs:1937-1947 five-element tuple return. gpu.rs:2174-2178 remap manually flips `map_pending=true; mapped=false` (hand-maintained exclusivity).

**Recommendation.** Model the state machine: `enum MapState { Unmapped, Pending { offset: WindowOffset, len: u64 }, Mapped { offset: WindowOffset, len: u64, cache: u32 } }` and `enum BlobOwner { Kmd, Escape(NonZeroUsize) }` with exhaustive matches. Have `blob_map_begin` return a move-only `PendingMap` token that `blob_map_finish` consumes (finish-without-begin becomes unrepresentable; the existing SlotGone arm still handles the teardown race). Replace tuple returns with named structs (`ReclaimedBlob { ctx_id, resource_id, mapping: Option<Mapping> }`). Introduce thin newtypes `ResourceId(u32)`, `CtxId(u32)`, `WindowOffset(u64)`, `WireFence(u64)` at the same boundary — the blob API is where u32/u64 identifier confusion is densest.

**Risk.** Medium — touches the MAP_BLOB path every venus process depends on and the VidMm remap path (blob_remap_begin). Keep the wire behavior and the SlotGone/HostRejected semantics bit-identical; the enum refactor must not change which ranges are freed on which arm (gpu.rs:2086-2110 frees on cache=None even when the slot is gone — preserve exactly).

**Dependencies.** R16 (split-gpu-rs)

**Atomic commit boundary.** Two commits: (1) MapState/BlobOwner enums + named return structs inside gpu.rs (tables module), callers mechanically updated; (2) the PendingMap consume-token across gpu.rs+ctrl.rs map flows.

**Validation.** Boot to desktop; run a blob-heavy workload (DOOM / blob_map_size_probe); BLOB_FULL_REJECTS/WINDOW_ALLOC_REJECTS/ADOPT_DEAD_REJECTS stay 0; QUERY_STATS window_used returns to baseline after process exit (no leaked ranges); MAP_PAGES_FAILS 0; device restart + reboot cycles clean.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** mapped/map_pending are mutually exclusive and map_offset/map_len/map_cache are valid only in specific states; owner==0 means KMD-internal. Permitted invalid sequences: blob_map_finish without a begin (matches nothing or a stale pending slot), reading zeroed geometry from an unmapped slot as if real, passing len where offset is expected.
1. **Compile-time representation:** MapState enum + BlobOwner enum + move-only PendingMap token consumed by finish + WindowOffset/ResourceId newtypes.
1. **Smallest atomic migration:** gpu.rs BlobSlot + its ~12 accessor methods in one commit; the PendingMap token adds ctrl.rs map_blob_prepare/map_blob_at in a second.
1. **Remaining `unsafe` preconditions:** None added; the teardown race (owner sweep removing the slot mid-roundtrip) is inherently dynamic and stays a runtime result arm (SlotGone) — a token cannot pin a slot another path may legitimately reclaim.
1. **Regression test proving preserved behavior:** Blob churn workload with QUERY_STATS deltas identical pre/post (blobs_live, window_used), plus dwm restart + full reboot with visible desktop and zero new *_REJECTS counters.


### R47. SyncWaitBlock register/wait/abandon lifecycle is a comment-enforced pinned-pointer protocol; encode it as a pinned RAII registration guard with a Completed proof for copy_resp

- **Category:** static-guarantee · **Reported by:** `kmd-transport-ctrl/sync-wait-typestate`
- **Merged duplicate reports (1):** `kmd-transport-gpu/syncwaitblock-raii-registration` — SyncWaitBlock stack registration into device-lock tables is a comment-enforced protocol (init-at-final-address, always-deregister-before-return) with kernel-stack-corruption downside
- **Files:** `kmd_render/src/virtio/ctrl.rs`, `kmd_render/src/virtio/gpu.rs`
- **Symbols:** `SyncWaitBlock`, `ctrl_roundtrip`, `wait_fence`, `wait_block`, `VirtioGpu::enqueue_sync`, `VirtioGpu::abandon_sync`, `VirtioGpu::fence_wait_prepare`, `VirtioGpu::fence_wait_cancel`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** A stack SyncWaitBlock is zero-constructed, unsafely init'ed in place, registered into the transport by raw NonNull (enqueue_sync waiter / fence_waiters), waited on, and must be deregistered under the device spinlock before the frame dies. Every step of that protocol is prose: 'Lives on the waiter's stack; the registered pointer stays valid because the waiter ALWAYS deregisters' (gpu.rs:514-516), 'MUST be init'ed... must not move until deregistered' (gpu.rs:538-539), 'Copy the response bytes out (only valid once is_done)' (gpu.rs:561). ctrl_roundtrip itself violates the last clause on the transport-gone race (see evidence): it calls block.copy_resp and returns Ok(()) with a never-written zeroed response. The drain writes and KeSetEvents these blocks from another CPU at DISPATCH; a forgotten deregistration on any early-exit path is a kernel stack UAF.

**Evidence.** gpu.rs:514-516 'Lives on the waiter's stack; the registered pointer stays valid because the waiter ALWAYS deregisters (or observes completion) under the device spinlock'; gpu.rs:550 'pub unsafe fn init(&mut self)' with contract 'self must be at its final (pinned) address'; gpu.rs:561 'Copy the response bytes out (only valid once [`Self::is_done`])'. ctrl.rs:249-252 'let mut block = SyncWaitBlock::new_zeroed(); unsafe { block.init() }; let block_ptr = NonNull::from(&mut block)'; ctrl.rs:281-293: on timeout, abandon_sync '.unwrap_or(true)' then unconditionally 'block.copy_resp(resp_out); Ok(())' — copy_resp without any is_done proof. gpu.rs:1348-1354 drain SAFETY comment restates the same cross-thread contract.

**Recommendation.** Trusted-boundary typestate in gpu.rs: an in-place constructor yielding Pin<&mut SyncWaitBlock> (init folded in, no separate unsafe init call); registration APIs consume it and return a #[must_use] RegisteredWait guard whose only exits are (a) wait -> Completed proof token, the sole type from which copy_resp is reachable, or (b) cancel/Drop, which performs abandon_sync/fence_wait_cancel under the lock. ctrl_roundtrip and wait_fence become guard users; drain-side signaling stays a small unsafe core in drain_used. This also mechanically fixes the copy_resp-without-done hole because no Completed token exists on that path.

**Risk.** Medium: touches the hottest wait path (every synchronous ctrl round-trip and WAIT_FENCE escape). Pure control-flow re-plumbing, no wire-format or timing change; the danger is a subtle change to the timeout/abandon race, which is exactly what the guard must reproduce bit-for-bit (abandon under the lock, treat already-signaled as success).

**Atomic commit boundary.** One commit: guard types + the four gpu.rs registration/cancel signatures + the two ctrl.rs users (ctrl_roundtrip, wait_fence). No DDI or escape ABI change.

**Validation.** KMD build; same-boot counters: CTRL_TIMEOUT_COUNT stays 0, FENCE_WAIT_TIMEOUTS no growth, DRAIN_BAD_TOKEN=0, ASYNC_COMPLETE advances; full regression gate (visible desktop, VpSA=1/ScSet=1, cursor, ~63 fps DComp cadence, no new gate/control timeouts).

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Invariant: a registered SyncWaitBlock is init'ed, address-pinned, and always deregistered (or observed complete) under the device spinlock before its frame returns; resp is read only after done. Permitted invalid sequences today: registering a never-init'ed block (uninitialized KEVENT dispatcher header -> bugcheck); an early return between registration and abandon (drain later writes/signals a dead stack frame -> kernel UAF); calling copy_resp without done — which ctrl.rs:292 actually does on the with_virtio-Err race.
1. **Compile-time representation:** Pin<&mut SyncWaitBlock> constructor (init in place); registration consumes it, returns #[must_use] RegisteredWait whose Drop/cancel runs abandon_sync/fence_wait_cancel under the lock; wait success returns Completed<'_>, the only type exposing copy_resp.
1. **Smallest atomic migration:** gpu.rs guard + 4 signatures, ctrl.rs 2 call sites, single commit.
1. **Remaining `unsafe` preconditions:** drain_used still dereferences the registered NonNull cross-thread under the spinlock — a lifetime the borrow checker cannot witness across a KSPIN_LOCK; stays a // SAFETY: in the trusted core. mem::forget on the guard would defeat Drop-based deregistration; not encodable, mitigated by keeping construction macro/function-scoped.
1. **Regression test proving preserved behavior:** Same-boot CTRL_TIMEOUT_COUNT=0, FENCE_WAIT_TIMEOUTS flat, DRAIN_BAD_TOKEN=0, plus the standard visible-desktop/cadence gate; a WAIT_FENCE escape storm (existing dxvk workload) exercises the timeout/cancel race.

**Lead-reviewer note.** Pinned RAII registration guard for the stack-registered wait block; the failure mode being prevented is kernel-stack corruption, which justifies the effort even though no current bug is proven.


### R48. resource_flush_async / set_scanout_blob_async accept 3-7 raw NonNull pointers whose 'must remain live until transport teardown' contract is a doc comment; the only legal targets are AdapterContext fields — derive them inside ctrl.rs

- **Category:** unsafe-contract · **Reported by:** `kmd-transport-ctrl/async-ctrl-pointer-bag`
- **Merged duplicate reports (4):** `kmd-transport-gpu/async-ctrl-notify-pointer-bundle` — InFlightKind::AsyncControl carries five raw NonNull pointers (plus AsyncScanoutNotify's two) whose required adapter-lifetime is enforced only by comments; `xc-errors/async-notify-sink-contract` — Async scanout ctrl completions carry 5 raw NonNull pointers into AdapterContext whose lifetime is guaranteed only by StopDevice-before-RemoveDevice call order; `xc-unsafe/async-completion-token` — Async ctrl completion crosses the transport as 5-6 raw NonNull pointers with prose lifetime contracts; `xc-duplication/inflight-pointer-raii` — In-flight table callback targets are bare NonNull with comment-only ownership: fence-event Ob references need a Drop-based EventRef; async-control adapter-field pointers need a constructor-scoped wrapper
- **Files:** `kmd_render/src/virtio/ctrl.rs`, `kmd_render/src/virtio/gpu.rs`, `kmd_render/src/adapter.rs`
- **Symbols:** `resource_flush_async`, `set_scanout_blob_async`, `submit_venus_async_scanout`, `InFlightKind::AsyncControl`, `AsyncScanoutNotify`, `AdapterContext::queue_active_scanout_refresh`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 5 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** The fire-and-forget scanout commands thread raw NonNull<AtomicU32>/NonNull<KEVENT> through ctrl.rs into InFlightKind::AsyncControl / AsyncScanoutNotify, dereferenced by the drain at DISPATCH arbitrarily later (gpu.rs:1380-1394, 1418-1421). The lifetime contract is prose: 'The pointed-to objects must remain live until transport teardown; the scanout caller uses fields embedded in AdapterContext' (gpu.rs:1116-1118). The sole call site (adapter.rs:654-667, 692-701) does pass adapter-embedded fields plus an unsafe NonNull::new_unchecked(self.hpd_event.get()) — but any future caller passing a stack or per-frame atomic compiles cleanly and becomes a use-after-free in the interrupt DPC.

**Evidence.** gpu.rs:1116-1118 'The pointed-to objects must remain live until transport teardown; the scanout caller uses fields embedded in `AdapterContext`, whose lifetime encloses the virtio transport.'; ctrl.rs:508-521 set_scanout_blob_async takes completion, completion_errors, host_bound, refresh_pending, wake_event all as NonNull; adapter.rs:662-667 'NonNull::from(&self.scanout_bind_inflight), ... unsafe { NonNull::new_unchecked(self.hpd_event.get()) }'; ctrl.rs:1464-1468 AsyncScanoutNotify built from 'NonNull::from(&adapter.scanout_refresh_pending)' + unchecked hpd_event; gpu.rs:1389-1394 drain deref: 'pending.as_ref().store(1, Ordering::Release); ... KeSetEvent(wake_event.as_ptr(), ...)'.

**Recommendation.** Remove the pointer parameters from the public API. Both functions already take &AdapterContext; since the targets are fixed adapter fields (scanout_bind_inflight / scanout_flush_inflight / scanout_bind_fail / scanout_refresh_fail / host_bound_scanout_resource / scanout_refresh_pending / hpd_event), derive the NonNulls inside ctrl.rs (or accept a sealed ScanoutChannel type with a private constructor only AdapterContext can build). gpu.rs's enqueue_async_control keeps the pointer form as the module-private trusted boundary with one SAFETY comment. Same for submit_venus_async_scanout's AsyncScanoutNotify.

**Risk.** Low: single production call path; pointers resolve to the identical addresses. Slightly couples ctrl.rs to adapter field names — acceptable since the coupling already exists semantically (drain writes those exact fields).

**Atomic commit boundary.** One commit: ctrl.rs two signatures + submit_venus_async_scanout notify construction + adapter.rs call sites; gpu.rs unchanged except visibility.

**Validation.** KMD build; same-boot scanout pipeline all-green: VpSA=1/ScSet=1, RbFail/RfFail counters stay 0, ASYNC_CTRL_COUNT advances with flushes, visible desktop with responsive idle-to-active dirty edge and no cursor trails.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Every pointer parked in InFlightKind::AsyncControl/AsyncScanoutNotify must outlive the transport (drain dereferences at DISPATCH after the caller returned). Enforced only by a doc comment and one disciplined call site; a stack-backed atomic passed by a future caller is a compiling kernel UAF.
1. **Compile-time representation:** Public API takes only &AdapterContext (or a sealed ScanoutChannel constructible solely by AdapterContext); NonNull derivation happens once inside ctrl.rs from adapter-embedded fields, so AsyncControl targets are adapter-owned by construction.
1. **Smallest atomic migration:** ctrl.rs signatures + adapter.rs call sites in one commit; gpu.rs enqueue_async_control becomes pub(crate)-internal trusted boundary.
1. **Remaining `unsafe` preconditions:** 'Adapter outlives every in-flight entry' (teardown drains/leaks entries before AdapterContext frees) is a transport-teardown ordering fact the types cannot see — one SAFETY comment remains at the single internal construction site; hpd_event UnsafeCell access stays unsafe.
1. **Regression test proving preserved behavior:** Scanout bind+flush cycle same-boot: ScSet/ScFlu fire, host_bound follows ScRid across primary rotation, RbFail/RfFail stay 0, visible desktop refresh on dirty edge.

**Lead-reviewer note.** Five reports. The strongest design suggestion among them: the only legal pointer targets are AdapterContext fields, so derive the sink inside ctrl.rs from a single &AdapterContext (one lifetime to prove) instead of accepting 3-7 raw NonNull params. inflight-pointer-raii's fence-event half belongs to R49.


### R49. ObReferenceObjectByHandle reference ownership for fence events is tracked by comments across register/unregister/drain

- **Category:** static-guarantee · **Reported by:** `kmd-submit/fence-event-obref-raii`
- **Files:** `kmd_render/src/ddi/escape.rs`
- **Symbols:** `reference_user_event`, `dereference_user_event`, `escape_register_fence_event`, `escape_unregister_fence_event`
- **Verification:** **MODIFIED** (severity medium) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** A raw NonNull<KEVENT> carries an Ob reference whose ownership transfers are documented only in prose: register's Registered arm comments 'The table now owns the reference'; every other arm must remember dereference_user_event; unregister does a subtle conditional double-deref ('The table's reference transfers back to us: drop it plus our lookup reference'). A future edit that misses one deref leaks a KEVENT (object never freed) or over-derefs (UAF in the retirement drain).

**Evidence.** escape.rs:231-232 `// The table now owns the reference; the drain signals + derefs.`; :239-241 AlreadyComplete arm manually KeSetEvent+deref; :246,:251,:256,:260 four separate manual `dereference_user_event(event)` failure arms; :287-295 `if removed { // The table's reference transfers back to us... dereference_user_event(event); } dereference_user_event(event);`; :189 doc 'the DISPATCH drain path uses ObDereferenceObjectDeferDelete'.

**Recommendation.** Introduce `UserEventRef` (NonNull<KEVENT> + PASSIVE-only Drop calling ObfDereferenceObject, with an explicit `into_table_raw()` escape hatch at the trusted boundary). `fence_event_register` consumes it and returns it back in the non-Registered arms (e.g. `AlreadyComplete(UserEventRef)`), so transfer-to-table is visible in the signature; `fence_event_unregister` returns `Option<UserEventRef>` for the reclaimed table reference, making unregister's two derefs two owned drops. Behavior (ref counts per path) is unchanged.

**Risk.** Low-medium: must not let Drop run at DISPATCH — the DPC drain keeps its explicit ObDereferenceObjectDeferDelete on raw pointers inside the trusted module. Verify no arm changes its deref count.

**Dependencies.** R39 (escape-verb-enum-prologue)

**Atomic commit boundary.** One commit inside escape.rs + the two gpu.rs signatures (fence_event_register/unregister).

**Validation.** Steady desktop run with fence events active: QUERY_STATS v2 fence_event registers/signals/cancels/teardown_drops advance as baseline; !object leak check over a long run (registers == signals+cancels+teardown_drops); no DWM crash.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Exactly-one-deref-per-reference enforced by call-site discipline; permits a leaked or double-dropped Ob reference on any new/edited arm (double drop = executive object UAF).
1. **Compile-time representation:** Move-only UserEventRef with Drop; ownership transfer expressed by consuming/returning the value in fence_event_register/unregister signatures.
1. **Smallest atomic migration:** escape.rs + the two VirtioGpu fence-event method signatures; drain path untouched (stays raw + DeferDelete).
1. **Remaining `unsafe` preconditions:** Drop is PASSIVE-only and cannot encode IRQL in the type; the table<->raw boundary (into_table_raw / from parked entry) remains a small trusted region, as does the DISPATCH DeferDelete drain.
1. **Regression test proving preserved behavior:** Fence-event register/wait/cancel loop (existing ICD path) with QUERY_STATS v2 counter parity vs baseline; overnight desktop soak without pool leak (poolmon tag / driver verifier pass).

**Verifier corrections (authoritative).** 1) Drop mechanism: UserEventRef::Drop must call ObDereferenceObjectDeferDelete, NOT plain ObfDereferenceObject with a prose PASSIVE-only rule. fence_event_register/unregister execute at DISPATCH inside with_virtio (adapter.rs:1017-1027 raises via KeAcquireSpinLockRaiseToDpc); a future early-return would run Drop at DISPATCH, and plain deref of the last reference runs PASSIVE-only deletion (hazard documented at gpu.rs:1444-1447). As written the refactor inverts the future-mistake failure mode from leak (fail-safe) to potential bugcheck (fail-unsafe). DeferDelete decrements immediately and only defers zero-count deletion — ref counts per path unchanged; precedent at gpu.rs:2400-2401. 2) Scope: migration touches FenceEventReg (gpu.rs:648, variants carry UserEventRef, enum becomes move-only) and FenceEventEntry's raw boundary, not just "the two gpu.rs signatures". 3) Closure design: keep the owned UserEventRef OUTSIDE the with_virtio closure and pass only the raw pointer in for the identity compare (unregister needs the pointer anyway); with_virtio's DeviceNotFound arm never calls the closure, so a closure-captured ref would drop implicitly in the epilogue after KeReleaseSpinLock — accounting is still one deref but the path must be explicit, not accidental. 4) Evidence line nits: the failure-arm derefs are at escape.rs:245, 252, 256, 260 (finding cited 246/251). 5) Tighten claim: no current bug — all six arms verified balanced; value is future-edit hardening only, appropriate to defer or land as a single behavior-identical commit with QUERY_STATS v2 counter parity (registers == signals + cancels + teardown_drops) as validation.

**Lead-reviewer note.** Verified MODIFIED — critical correction: UserEventRef::Drop must call ObDereferenceObjectDeferDelete, NOT plain ObfDereferenceObject, because register/unregister run at DISPATCH inside with_virtio; a plain deref would invert the failure mode from fail-safe leak to potential bugcheck. Keep the owned ref outside the with_virtio closure; validation via QUERY_STATS v2 counter parity (registers == signals + cancels + teardown_drops).


### R50. WddmNotifyGuard proves the notify lock is held but not that it was acquired BEFORE the virtio lock — the AB-BA inversion is still expressible in safe code

- **Category:** concurrency · **Reported by:** `kmd-transport-gpu/notify-guard-witnesses-possession-not-order`
- **Merged duplicate reports (1):** `xc-unsafe/notify-virtio-lock-order-api` — wddm_notify → virtio lock order enforced by comment across six hand-rolled nesting sites
- **Files:** `kmd_render/src/virtio/gpu.rs`, `kmd_render/src/adapter.rs`
- **Symbols:** `WddmNotifyGuard`, `VirtioGpu::note_scanout_refresh`, `VirtioGpu::take_ready_wddm`, `AdapterContext::with_virtio`, `AdapterContext::with_wddm_notify_lock`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** The frozen baseline calls this the "WddmNotifyGuard lock-order proof", and all seven current call sites correctly nest with_wddm_notify_lock → with_virtio (interrupt.rs:40-55, submit_command.rs:271-273, 292-294). But the guard only witnesses possession: safe code can write `adapter.with_virtio(|v| adapter.with_wddm_notify_lock(|g| v.note_scanout_refresh(g)))` — the borrow checker accepts the capture, and this acquires notify INSIDE virtio, an AB-BA inversion against the DPC's notify→virtio order: two CPUs deadlock spinning at DISPATCH (silent machine hang, the exact class the kernel invariants forbid). The guard-taking methods (note_scanout_refresh, take_ready_scanout_refresh, note_wddm_submission, take_ready_wddm, preempt_flush, gpu.rs:1693-1817) are exactly the ones a future call site would compose wrongly.

**Evidence.** adapter.rs:294-300 "Proof that this adapter's WDDM notification spinlock is currently held" (possession only). gpu.rs:1691-1696 `note_scanout_refresh(&mut self, _notify_guard: &WddmNotifyGuard)` requires &mut VirtioGpu — obtainable only inside with_virtio, while a guard can be created inside that same closure via `adapter.with_wddm_notify_lock` (adapter.rs:980-986 takes &self, no held-lock precondition). Correct-order example that the type system does not force: interrupt.rs:40-43 `adapter.with_wddm_notify_lock(|guard| { ... adapter.with_virtio(|v| v.take_ready_scanout_refresh(guard)) ... })`.

**Recommendation.** Add a combined-acquisition helper on AdapterContext, e.g. `with_notify_then_virtio<R>(&self, f: impl FnOnce(&WddmNotifyGuard, &mut VirtioGpu) -> R) -> Result<R, DriverError>`, which acquires the two locks in the proven order, and make it the ONLY way to reach the guard-taking VirtioGpu methods: move them behind a second token type (`NotifyThenVirtio<'_>` handed out only by the helper) or reduce their visibility so the standalone `with_virtio` + free-floating guard composition cannot name them. Convert interrupt.rs/submit_command.rs mechanically (same lock sequence emitted).

**Risk.** Low-medium — pure reshuffling of acquisition plumbing on the hottest DPC path; must not add a lock acquisition or change the section the closures run in. Preserves the frozen-baseline mechanism exactly (same watermark capture under the same order).

**Atomic commit boundary.** One commit: helper + token in adapter.rs, method-visibility change in gpu.rs, seven call sites converted.

**Validation.** Boot + desktop; refresh markers still consumed by the used-ring DPC (WDDM_FENCE_FROM_DPC advancing); no new gate timeouts; stress with concurrent submits + DPC (game workload) with zero hangs; 63 fps DComp.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** wddm_notify_lock must be acquired strictly before virtio_lock. Permitted invalid sequence: with_wddm_notify_lock nested inside a with_virtio closure → AB-BA spinlock deadlock at DISPATCH against the DPC.
1. **Compile-time representation:** Combined with_notify_then_virtio helper issuing a pairing token that is the only gateway to the guard-taking VirtioGpu methods; the wrong nesting can no longer name those methods.
1. **Smallest atomic migration:** adapter.rs + gpu.rs method signatures + interrupt.rs/scheduler.rs/submit_command.rs call sites, one commit.
1. **Remaining `unsafe` preconditions:** A global lock-order discipline (any other pair of adapter locks, e.g. the mapping-table spinlock) cannot be encoded without a full level-token system; this finding closes only the notify/virtio pair, which is the pair the baseline contract names.
1. **Regression test proving preserved behavior:** Same-boot desktop + game stress with DPC-completed fences (WDDM_FENCE_FROM_DPC) advancing and zero watchdog/DPC-timeout events; refresh ordering behavior byte-identical.

**Lead-reviewer note.** WddmNotifyGuard proves possession, not acquisition order — the AB-BA inversion is still expressible in safe code across six hand-rolled nesting sites. Make the virtio lock acquirable (in the nested direction) only through a method on the notify guard.


### R51. diag::record / record_named / read_config_dword PASSIVE-only rule enforced by repeated comments at every call layer, not by types

- **Category:** static-guarantee · **Reported by:** `kmd-core/diag-passive-proof-token`
- **Merged duplicate reports (1):** `kmd-transport-ctrl/passive-irql-token` — 'Every function in this module MUST be called at PASSIVE_LEVEL' is enforced only by a module doc comment; a DISPATCH caller of sleep_ms/wait/reap compiles and deadlocks or bugchecks
- **Files:** `kmd_render/src/diag.rs`, `kmd_render/src/ddi/interrupt.rs`, `kmd_render/src/adapter.rs`
- **Symbols:** `record`, `record_named_bytes`, `read_config_dword`, `queue_active_scanout_refresh`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** The kernel invariant "no diag::record (registry writes) above PASSIVE" is restated as prose in at least three places (diag.rs:12-13 "only call [record] from PASSIVE DDIs (never the DPC/ISR...)"; interrupt.rs:24-26; gpu.rs:1972/2253 "Atomic, not diag::record — callers hold the device spinlock") and honored purely by call-site audit. Nothing stops a future DPC-path edit (e.g. inside with_wddm_notify_lock or the vsync DPC) from calling record_named_bytes — it compiles and produces IRQL bugchecks or silent deadlock. The 90-line telemetry block in queue_active_scanout_refresh (adapter.rs:722-811) is legal only because its sole caller is the PASSIVE HPD worker (hpd.rs:140) — an invariant the signature does not carry.

**Evidence.** diag.rs:12-13 "IRQL: `RtlWriteRegistryValue` requires PASSIVE_LEVEL — only call [`record`] from PASSIVE DDIs (never the DPC/ISR or DISPATCH paging paths)."; interrupt.rs:24-26 "`diag::record` is PASSIVE-only... so the ISR/DPC cannot touch the registry ring. These atomics are incremented here at DIRQL/DISPATCH"; gpu.rs:1972 "Atomic, not diag::record — callers hold the device spinlock"; adapter.rs:720-722 "Registry writes are synchronous and must not become a once-per-second frame-path tax" — all discipline, no type.

**Recommendation.** Add a zero-sized `Passive` proof token: `unsafe fn Passive::assert()` minted once at the ~dozen entry points that are contractually PASSIVE (lifecycle DDIs, QueryAdapterInfo, Escape, DestroyDevice, the HPD thread routine), and require `Passive` by value/ref in `record`, `record_named*`, and `read_config_dword`. Dual-IRQL DDIs (SetVidPnSourceAddress) simply cannot mint it, statically excluding registry writes there. This is compile-time PASSIVE/DISPATCH separation with a small trusted boundary — not a cosmetic wrapper, since it deletes the need for per-call comments.

**Risk.** Mechanical signature threading; zero codegen change (ZST). The only judgment calls are which DDI entries may mint the token — each mint carries a // SAFETY citing the MSDN IRQL contract.

**Dependencies.** R22 (config-knob-module)

**Atomic commit boundary.** One commit adding the token + threading it through diag/config and all callers (compiler enumerates them).

**Validation.** Builds; boot; named counters (GdiM/DspH/RfCnt/BarM...) still appear and move this boot; no behavior change expected — diff review + counter presence is the gate.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Any code path can call the registry-writing diag/config functions at any IRQL; a DISPATCH/DIRQL caller bugchecks (IRQL_NOT_LESS_OR_EQUAL) or deadlocks the graphics stack.
1. **Compile-time representation:** `Passive` ZST proof token required by every registry-touching function; minted only at contractually-PASSIVE entry points via one audited unsafe constructor per entry.
1. **Smallest atomic migration:** Single commit; the compiler finds every call site.
1. **Remaining `unsafe` preconditions:** IRQL is dynamic — the mint at each DDI entry trusts dxgkrnl's documented calling IRQL and cannot be checked by the type system; that trust shrinks from every call site to ~12 entry points.
1. **Regression test proving preserved behavior:** Boot with DiagLevel=1: S-ring and named counters populate exactly as the baseline boot; no new bugchecks under load.

**Lead-reviewer note.** The compile-time PASSIVE/DISPATCH separation pattern from the handoff. Zero-sized PassiveLevel token minted at known-PASSIVE entry points; diag::record and ctrl waits take it by value. Makes D1's bug class unrepresentable after D1's behavioral fix lands.


### R52. Hand-rolled acquire/release choreography with integer fail-point sentinels in the VidPn iteration should become RAII guards and a typed fail enum

- **Category:** static-guarantee · **Reported by:** `kmd-display/vidpn-raii-modeset-guards`
- **Merged duplicate reports (1):** `xc-legacy/vidpn-raii-guards` — enum_cofunc_modality / commit_vidpn manage OS VidPn set/path handles with manual release pairing and numeric fail-points; several error exits leak acquired sets
- **Files:** `kmd_render/src/ddi/vidpn.rs`
- **Symbols:** `enum_cofunc_modality`, `commit_vidpn`, `recommend_monitor_modes`, `add_single_source_mode`, `add_single_target_mode`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** enum_cofunc_modality is a 320-line function (vidpn.rs:396-714) doing manual goto-style cleanup: every OS object (source/target mode set, pinned mode info, path info) is acquired raw and released by remembering to call the right pfn on every exit edge, with bare integer fail points ('fp = 10..31', e.g. 503, 515, 524, 668, 678) recorded to VpECf as undocumented sentinel codes. Path-advance does pointer juggling: vidpn.rs:673-681 'let prev = path; status = acquire_next(...); if !ok(status) { fp = 31; path = prev; break; }'. The assign-failure leak (separate defect) is a direct product of this shape. add_single_source_mode/add_single_target_mode are near-identical twins (224-272 vs 278-321) differing only in the mode struct and its fill.

**Evidence.** vidpn.rs:469-479 manual loop entry; 501-549 the source-set acquire/release/create/add/assign ladder with fp=10/12/13/14/15/16; 673-683 'let prev = path; ... fp = 31; path = prev; break; } let _ = unsafe { release_path(h_topo, prev) };'; 704 'rec(b"VpECf", fp); // fail-point id (see fp = N sites)'; twin fns at 224-272 and 278-321.

**Recommendation.** After the leak defect lands: introduce Drop-based guards — SourceModeSetGuard/TargetModeSetGuard { h_vidpn, h_set, iface } releasing on drop with an into_assigned(self) consuming method that forgets on successful assign (ownership-transfer typestate), a PinnedModeGuard, and a path iterator yielding path-info guards so acquire_next/release_path pairing is structural. Replace fp integers with a #[repr(u32)] CofuncFailPoint enum (same numeric values recorded to VpECf, preserving diag decoding). Factor the twin add_single_*_mode bodies over a small trait or closure for the mode-fill. no_std Drop is fine at PASSIVE.

**Risk.** Medium: this rewrites cleanup control flow in a proven-fragile DDI (36th-session 0-paths history). Mitigate by keeping the recorded fp values and legalization identical and landing as a pure-structure commit with no status-value changes.

**Dependencies.** D8 (vidpn-assign-failure-modeset-leak)

**Atomic commit boundary.** One commit for the guards + enum inside vidpn.rs only; the add_single dedup can be a follow-up commit.

**Validation.** Reboot; negotiation identical: VpISp>=1, VpECp>=1, VpECr=0, VpCN=1, VpCP=1, VpCW = mode; monitor present; desktop visible; no new VpECe records.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Pairing of pfnAcquire*/pfnCreate* with release-or-assign is enforced by call order and break discipline; permitted invalid sequences: missed release on a new error edge (already happened at fp=16/26), double release, and use of a set after assign transferred ownership.
1. **Compile-time representation:** RAII guards with Drop releasing; into_assigned(self) consuming ownership on successful assign; a path-info iterator making release structural; CofuncFailPoint enum replacing sentinel integers.
1. **Smallest atomic migration:** vidpn.rs only, one commit, no NTSTATUS or diag-value changes.
1. **Remaining `unsafe` preconditions:** The pfn calls themselves remain unsafe (raw OS function pointers with opaque handles); guard validity is still tied to the DDI argument lifetime, expressed as a borrow but not provable against dxgkrnl.
1. **Regression test proving preserved behavior:** A/B boot comparing the full Vp* record set (VpISp/VpECp/VpECr/VpECe/VpECf/VpCN/VpCP/VpCW) — identical values prove preserved negotiation behavior.

**Lead-reviewer note.** Subsumes defect D8 structurally.


### R53. The 'only legal NTSTATUS may escape a VidPn DDI' contract is enforced by remembering to call legalize_vidpn at inconsistent layers

- **Category:** static-guarantee · **Reported by:** `kmd-display/legal-vidpn-status-newtype`
- **Files:** `kmd_render/src/ddi/vidpn.rs`, `kmd_render/src/ddi/display.rs`
- **Symbols:** `legalize_vidpn`, `commit_vidpn`, `recommend_monitor_modes`, `enum_cofunc_modality`, `dxgkddi_commit_vidpn`, `dxgkddi_recommend_monitor_modes`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** An out-of-contract NTSTATUS from a VidPn DDI makes dxgkrnl discard every candidate VidPn (ETW-proven 36th-session 0-paths root cause, documented at vidpn.rs:139-148). The clamp exists (legalize_vidpn, vidpn.rs:149-160) but is applied inconsistently: enum_cofunc_modality legalizes internally (425, 446, 475, 710); recommend_monitor_modes returns the raw pfnAddMode status (vidpn.rs:384 'return st;') relying on the display.rs thunk wrapping it (display.rs:854-856); commit_vidpn returns only SUCCESS variants yet is still wrapped (display.rs:604-606). Nothing stops a future helper or new error edge from returning a raw status through an unwrapped thunk — the exact proven failure class.

**Evidence.** vidpn.rs:149-160 'pub(crate) fn legalize_vidpn(s: NTSTATUS) -> NTSTATUS { ... STATUS_GRAPHICS_INVALID_VIDPN }' with 139-147 explaining dxgkrnl 'discards EVERY candidate VidPn'. vidpn.rs:380-384 raw escape: 'rec(b"VpMMe", st as u32); return st;' — only legal because display.rs:854-856 wraps: 'crate::ddi::vidpn::legalize_vidpn(unsafe { crate::ddi::vidpn::recommend_monitor_modes(adapter, _recommend) })'.

**Recommendation.** Make the boundary type-enforced: a VidPnDdiStatus newtype whose only constructors are legalize_vidpn(raw) and success/known-graphics-status constants; the extern-C VidPn thunks return `VidPnDdiStatus::into_ntstatus()`, and the inner bodies (recommend_monitor_modes, commit_vidpn, enum_cofunc_modality) return VidPnDdiStatus (or Result<(), RawCallbackStatus> that must pass through legalize). This is not a cosmetic wrapper: the compiler then proves no raw callback NTSTATUS reaches dxgkrnl from these DDIs, converting a whole proven-fatal bug class into a type error.

**Risk.** Low: pure type plumbing; numeric values returned to dxgkrnl are unchanged.

**Dependencies.** R52 (vidpn-raii-modeset-guards)

**Atomic commit boundary.** One commit across vidpn.rs + the display.rs VidPn thunks.

**Validation.** Reboot; SetDisplayConfig works, VpECr legal, no AzureTriage 'Driver returned an invalid NTSTATUS code' events in an ETW capture, desktop visible.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Legal-return-set membership is enforced by caller discipline at whichever layer remembered to wrap; invalid sequence: a new error path returning a raw NTSTATUS through an unwrapped thunk — dxgkrnl then flags the driver buggy and drops all VidPns (proven).
1. **Compile-time representation:** VidPnDdiStatus newtype with private inner value; constructors only via legalize_vidpn and named legal constants; thunk signatures convert at exactly one boundary.
1. **Smallest atomic migration:** vidpn.rs + VidPn thunks in display.rs, one commit; no numeric change.
1. **Remaining `unsafe` preconditions:** None; the DDI's legal set itself is a WDDM documentation fact encoded in legalize_vidpn's whitelist and can drift only with the WDK, not the compiler.
1. **Regression test proving preserved behavior:** ETW Microsoft-Windows-DxgKrnl AzureTriage grep shows zero invalid-NTSTATUS events across a modeset (mode change + adapter restart) with the desktop visible after.


### R54. Every display DDI re-reinterprets the miniport handle as *const AdapterContext with duplicated, inconsistent null/display_half gating

- **Category:** unsafe-contract · **Reported by:** `kmd-display/adapter-handle-boundary`
- **Files:** `kmd_render/src/ddi/display.rs`
- **Symbols:** `display_half_on`, `dxgkddi_present`, `dxgkddi_is_supported_vidpn`, `dxgkddi_commit_vidpn`, `dxgkddi_set_vidpn_source_address`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** The pattern `let p = _adapter as *const AdapterContext; if p.is_null() || !unsafe { (*p).display_half }` is hand-repeated in at least 6 DDIs (display.rs:506-508, 555-557, 588-590, 637-639, 847-849) alongside the helper display_half_on (491-494) used by 5 more; two sites in dxgkddi_present skip the null check entirely (display.rs:315 and 374: 'let adapter = unsafe { &*(_adapter as *const AdapterContext) };'). Each duplication is an independent unsafe reinterpretation of the same dxgkrnl round-trip contract, and the not-display-half status differs by DDI (STATUS_NOT_SUPPORTED vs STATUS_GRAPHICS_INVALID_VIDPN at 508) as a per-contract requirement that is currently invisible in the shape.

**Evidence.** display.rs:491-494 'unsafe fn display_half_on(h: IN_CONST_HANDLE) -> bool { let p = h as *const AdapterContext; !p.is_null() && unsafe { (*p).display_half } }' vs display.rs:315 'let adapter = unsafe { &*(_adapter as *const AdapterContext) };' (no null check) vs display.rs:506-508 'let p = _adapter as *const AdapterContext; if p.is_null() || !unsafe { (*p).display_half } { return STATUS_GRAPHICS_INVALID_VIDPN; }' — three shapes for one contract.

**Recommendation.** Create one trusted boundary: `unsafe fn adapter_from_handle<'a>(h: IN_CONST_HANDLE) -> Option<&'a AdapterContext>` (single SAFETY comment stating the dxgkrnl contract), plus `fn display_adapter(h) -> Option<&DisplayAdapter>` where DisplayAdapter is a zero-cost proof wrapper (Deref to AdapterContext) obtainable only when display_half is true — so 'this code runs only with the display half up' becomes a parameter type instead of a re-checked bool. Each DDI keeps its own legal not-supported status at the call site. This is the handoff's 'non-null wrappers instead of repeatedly reinterpreting raw WDDM handles' item; it shrinks the unsafe surface to one function rather than relocating casts.

**Risk.** Low; mechanical. Care only that the render-only (knob-off) fallback statuses stay byte-identical per DDI.

**Dependencies.** R19 (display-file-split)

**Atomic commit boundary.** One commit inside the display DDI module(s); no behavior change.

**Validation.** Builds; reboot; all display DDIs answer as before (S-ring codes 0x1300_00xx unchanged at DiagLevel 1); render-only knob-off boot still starts Code 0.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** 'The handle is our non-null AdapterContext and display_half gates this DDI' is re-proven ad hoc per DDI; permitted invalid states: a DDI that forgets the null check (two already exist) or the display_half check, or returns an off-contract fallback status.
1. **Compile-time representation:** Single unsafe conversion fn + DisplayAdapter proof newtype; display-only helpers take &DisplayAdapter so they cannot be called from render-only paths.
1. **Smallest atomic migration:** display.rs (post-split modules), one commit.
1. **Remaining `unsafe` preconditions:** The one reinterpretation of dxgkrnl's opaque handle — inherent to the DDI ABI, cannot be encoded; trusted-boundary SAFETY comment stays.
1. **Regression test proving preserved behavior:** DiagLevel=1 boot: identical 0x1300_xxxx breadcrumb sequence for a modeset; knob-off boot returns the same NOT_SUPPORTED set.


### R55. MappingTable hand-rolls five acquire/release spinlock pairs; no-realloc-under-lock and duplicate-map guards are comment/TOCTOU-enforced

- **Category:** static-guarantee · **Reported by:** `kmd-alloc/mapping-table-raii`
- **Files:** `kmd_render/src/mapping.rs`, `kmd_render/src/ddi/escape.rs`
- **Symbols:** `MappingTable`, `contains`, `insert`, `take_one_for`, `take_for_resource`, `escape_map_blob`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Every MappingTable method manually pairs KeAcquireSpinLockRaiseToDpc/KeReleaseSpinLock around an UnsafeCell (contains :102-107, insert :117-136, live :142-146, take_one_for :156-163, take_for_resource :170-180) — five unsafe blocks each re-stating the same SAFETY prose, with no guard ensuring release on early return. The push-never-reallocates invariant is doc-comment only (:24-29 'insert only pushes within that reserved capacity'): a Vec allows any future method to push past capacity at DISPATCH. The duplicate-map guard is split across two lock acquisitions at the call site: escape.rs:535 `contains(...)` then :565 `insert(...)` — racing MAP_BLOBs on one device both pass contains, defeating the documented guard (benign today: both entries drain at file cleanup).

**Evidence.** mapping.rs:102-107 `let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.lock.get()) }; ... unsafe { KeReleaseSpinLock(self.lock.get(), irql) };` (pattern repeated :117/:136, :142/:146, :156/:163, :170/:180); :24-29 '`insert` only `push`es within that reserved capacity, so it never reallocates and is safe to call under the spinlock' (comment-only); escape.rs:535 `if adapter.mappings.contains(owner, req.resource_id) { return STATUS_INVALID_DEVICE_REQUEST; }` then :565-567 `adapter.mappings.insert(owner, req.resource_id, user_va, mdl as usize)` — two separate lock holds.

**Recommendation.** Add a private `fn locked<R>(&self, f: impl FnOnce(&mut Entries) -> R) -> R` (RAII release), collapsing the five unsafe pairs to one trusted boundary; replace Vec with a fixed-capacity array type (heapless/ArrayVec-style, const MAX_MAPPINGS) so no-alloc-under-lock is a type property; add `insert_unique(owner, resid, va, mdl) -> Inserted|Duplicate|Full` and use it from escape_map_blob (single lock hold). Behavior identical except the racy duplicate case, which currently violates the documented guard.

**Risk.** Low; the drain contract (pop one, unmap at PASSIVE outside the lock, :149-154) must keep its shape — the closure API naturally preserves it. Fixed-capacity type changes the allocation-at-construction footprint only.

**Atomic commit boundary.** One commit: mapping.rs internal rework + the escape.rs call-site swap to insert_unique.

**Validation.** Boot + game map burst (the Doom BU_STATIC scenario): MAPPINGS_HIGH_WATER reaches comparable values, MAPPING_FULL_REJECTS stays 0, process exit clean (no 0x76 PROCESS_HAS_LOCKED_PAGES), QUERY_STATS v2 mapping fields unchanged.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** (a) Lock release by convention on every path; (b) no-realloc-under-DISPATCH by comment over a growable Vec; (c) at-most-one-mapping-per-(owner,resid) enforced across two lock acquisitions — a race admits duplicates.
1. **Compile-time representation:** Closure/RAII lock boundary (release structural); fixed-capacity entry array (push cannot allocate); insert_unique making the uniqueness check and insertion one critical section.
1. **Smallest atomic migration:** mapping.rs + one escape.rs call site, one commit.
1. **Remaining `unsafe` preconditions:** KSPIN_LOCK FFI and the Send/Sync impls remain; caller IRQL (PASSIVE for the unmap-outside-lock drain) stays a documented precondition — Rust cannot carry the IRQL proof here without a broader IRQL-token scheme.
1. **Regression test proving preserved behavior:** MAP_BLOB/RELEASE_BLOB round-trips + multi-process teardown (dwm restart, game exit) with clean 0x76-free shutdown and identical QUERY_STATS mapping counters.


### R56. AllocationContext encodes backing class as interdependent nullable ids/booleans; destroy re-derives class by nonzero-ness

- **Category:** static-guarantee · **Reported by:** `kmd-alloc/alloc-backing-enum`
- **Files:** `kmd_render/src/ddi/create_allocation.rs`
- **Symbols:** `AllocationContext`, `create_one`, `destroy_allocation_ctx`, `read_alloc_identity`, `read_standard_meta`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** AllocationContext (lines 44-115) carries owns_resource: bool, blob_id, venus_memory_id (nonzero => KMD-backed), venus_image_id (nonzero => KMD scanout image), direct_scanout: bool, bar_eligible: bool, plus dead Stage-2b fields: mapped/map_offset/map_len are written once (false/0 at 958-960) and never set again, so the destroy branch at 652-654 (`if ctx.mapped && !unmapped_here { resource_unmap_blob }`) is unreachable; blob_id (947) is never read. create_one builds one of FOUR backing classes via an if/else ladder (754 adopt, 783 primary scanout image, 806 KMD memory blob, 844 raw blob) that mutates `ap` in place and smuggles the resid through `_pad`; owns_resource is then re-derived at 872 and bar_eligible at 940. destroy_allocation_ctx (615-681) reconstructs the class from field nonzero-ness (647 owns_resource, 666 venus_image_id!=0, 669 venus_memory_id!=0) — invalid combinations (e.g. image without memory, adopted with image) are representable and would take a wrong teardown arm.

**Evidence.** create_allocation.rs:49 `owns_resource: bool`; :74 `mapped: bool` set only at :960 `mapped: false`; :652 `if ctx.mapped && !unmapped_here {` (unreachable); :947 `blob_id: ap.blob_id` never read (grep confirms only 50/947); :754-869 four-arm ladder `let resource_id = if adopt_supplied_resource {...} else if ap.kind == HELIOS_WDDM_ALLOC_KIND_STANDARD && is_primary {...}`; :872 `let owns_resource = !adopt_supplied_resource || ap.kind == HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY;`; :940 `let bar_eligible = venus_memory_id != 0 && bar_seg_id.is_some();`; destroy re-derivation :647 `if ctx.resource_id != 0 && ctx.owns_resource && !adapter_owned_scanout`, :666 `if ctx.venus_image_id != 0`, :669 `if ctx.venus_memory_id != 0`.

**Recommendation.** Two commits. (1) Legacy removal: delete mapped/map_offset/map_len/blob_id and the unreachable 652-654 unmap branch. (2) Replace the field soup with `enum AllocationBacking { Adopted { resource_id, owns: bool }, KmdScanoutImage { memory_id, image_id, resource_id }, KmdMemoryBlob { memory_id, resource_id }, RawBlob { resource_id } }`, constructed by a validate-once parse of the private data (fold read_alloc_identity/read_standard_meta dual-layout heuristics into one `ParsedAllocRequest` constructor) and matched EXHAUSTIVELY in destroy_allocation_ctx. bar_eligible becomes a method of the variant + bar segment presence.

**Risk.** Destroy ordering is subtle (drain prepared copy -> forget blob -> take_live_resource-guarded detach/unref -> image -> memory; the res-45 and QEMU double-unref lessons live here). Mitigate by keeping the match arms byte-identical in op order and landing after scanout-copy-extraction to avoid double-churning the struct.

**Dependencies.** R21 (scanout-copy-extraction)

**Atomic commit boundary.** Commit 1: dead-field + dead-branch deletion (no behavior change). Commit 2: enum migration inside create_allocation.rs only (projections PagingAllocInfo/ScanoutInfo unchanged).

**Validation.** KMD builds; guest reboot; visible desktop, VpSA=1/ScSet=1, ScanoutDiag absent, cursor clean, ~63fps DComp; dwm restart + pnputil /restart-device cycles with ZERO new QEMU 'resource does not exist' unref lines in /tmp/helios-qemu-stderr.log; CpDrn/CpKeep counters behave as before; alloc/open event ring shows same create/open sequence.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Backing class = correlation among owns_resource/venus_memory_id/venus_image_id/adopt-kind, enforced only by create_one's ladder writing consistent combinations. Permits: an AllocationContext with venus_image_id!=0 && venus_memory_id==0 (destroy would destroy an image and skip the memory free), or adopted+owns with KMD ids (double teardown), if any future edit drifts one arm.
1. **Compile-time representation:** Exhaustive `enum AllocationBacking` with per-variant ids; destroy_allocation_ctx matches all variants; invalid combinations unrepresentable; owns/adopted distinction is a variant, not a derived bool.
1. **Smallest atomic migration:** create_allocation.rs only; the HANDLE/FFI surface and PagingAllocInfo/ScanoutInfo snapshots keep their shapes.
1. **Remaining `unsafe` preconditions:** hAllocation is still a raw pointer round-tripped through dxgkrnl (magic-checked); bar_placed stays an atomic sentinel because paging DDIs run concurrently with allocation DDIs — liveness across the FFI boundary cannot be encoded.
1. **Regression test proving preserved behavior:** Boot + dwm restart + adapter restart with QEMU stderr clean of duplicate-unref, plus the standard visible-desktop gate; destroy-path counters (CpDrn, CpKeep) and QUERY_STATS blobs_live return to pre-refactor steady-state values.


### R57. PreparedImageCopy is Copy with public fields, a bool+zero-sentinel source discriminant, and a comment-only 'alive until destroyed' contract

- **Category:** static-guarantee · **Reported by:** `kmd-venus/prepared-copy-typestate`
- **Files:** `kmd_render/src/virtio/venus.rs`, `kmd_render/src/ddi/create_allocation.rs`
- **Symbols:** `PreparedImageCopy`, `VenusClient::destroy_prepared_image_copy`, `VenusClient::submit_prepared_image_copy`, `cached_prepared_copy`, `publish_prepared_copy`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** PreparedImageCopy (venus.rs:234-255) derives Clone+Copy with all-pub fields. Its source identity is `owns_source_alias: bool` plus zero-sentinel `source_resource_id`/`source_memory_id` — interdependent fields whose coupling ('Zero for a borrowed KMD-created source') lives in doc comments. destroy_prepared_image_copy consumes `copy: PreparedImageCopy` by value (2133), but Copy makes consumption meaningless: a caller can retain a bitwise copy and resubmit after the pool is destroyed (host use-after-free of the command pool). The lifetime contract is a comment: 'The object must remain alive until destroy_prepared_image_copy has drained the queue' (232-233). Similarly VenusClient's copy-target lifecycle is three zero-sentinel u64 fields (534-536) checked ad hoc (2098, 1736-1744).

**Evidence.** venus.rs:234-238 '#[derive(Clone, Copy)] pub struct PreparedImageCopy { /// True when preparation created/attached/imported the source objects below. /// False for a borrowed KMD-created LINEAR source image. pub owns_source_alias: bool'; :241-246 'Zero for a borrowed KMD-created source ... or zero for a borrowed source'; :232-233 'The object must remain alive until [`VenusClient::destroy_prepared_image_copy`] has drained the queue'; :2130-2135 consuming signature defeated by Copy; :534-536 'copy_target_image_id: u64, copy_target_init_pool_id: u64, copy_target_init_command_buffer_id: u64'; create_allocation.rs:282-296 rebuilds the struct from seven Relaxed atomic loads.

**Recommendation.** Make PreparedImageCopy non-Clone/non-Copy with private fields; replace the bool+sentinels with `enum CopySource { ImportedAlias { resource: u32, memory: u64 }, BorrowedLinear }` matched exhaustively in destroy; keep the by-value consuming destroy (now actually consuming). Replace the copy_target_* trio with `enum CopyTargetState { Unset, Ready { image, pool, cmd_buf } }`. In create_allocation.rs, keep the atomic-snapshot cache but funnel reconstruction through one `PreparedImageCopy::from_snapshot` constructor that revalidates the enum invariants, so an incoherent snapshot cannot yield a submittable object.

**Risk.** The create_allocation.rs publish-word protocol (Release store of command_buffer_id, 299-314) must not be weakened while converting; the snapshot constructor must preserve exact accept/reject behavior of today's ad-hoc checks.

**Dependencies.** R17 (venus-split)

**Atomic commit boundary.** One commit covering venus.rs type change + create_allocation.rs cache constructor (they are ABI-coupled and cannot split).

**Validation.** Compile-time: resubmit-after-destroy no longer compiles (doc-test or trybuild-style check optional). Runtime gate: desktop visible, no ScanoutDiag, VpSA=1/ScSet=1, CpDrn=1 on allocation teardown, no new 'CpCpy' error breadcrumbs, no QEMU 'resource does not exist' unref errors across a DWM restart.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Nothing prevents (a) submitting a bitwise copy of a destroyed PreparedImageCopy — enqueueing a freed host command buffer; (b) constructing owns_source_alias=true with zero resource/memory ids (destroy would then skip detach or free id 0); (c) an incoherent atomic snapshot masquerading as a valid copy object.
1. **Compile-time representation:** Non-Copy opaque token + exhaustive CopySource enum + CopyTargetState enum; destroy consumes the token; submit borrows it and requires the client's Ready target state.
1. **Smallest atomic migration:** venus.rs + create_allocation.rs in one commit (type is shared).
1. **Remaining `unsafe` preconditions:** The cross-thread atomic snapshot in AllocationContext cannot be type-proven coherent — the publish-word Release/Acquire protocol remains a runtime contract, shrunk to one constructor; host-side object liveness is inherently unverifiable from the guest.
1. **Regression test proving preserved behavior:** Same-boot QEMU evidence of the OPTIMAL primary; allocation create/destroy cycling (DWM restart) with CpDrn=1 and zero QEMU unref errors; fallback (non-direct) primary path exercised once via a windowed BLT scenario.


### R58. Aperture-first / CpuHostAperture-LAST segment invariant enforced only by match-arm ordering and registry knob defaults; BarSegment reported without proof the blob-window prefix was reserved

- **Category:** static-guarantee · **Reported by:** `kmd-core/segment-table-order-static`
- **Files:** `kmd_render/src/ddi/query_adapter_info.rs`, `kmd_render/src/ddi/start_device.rs`, `kmd_render/src/adapter.rs`
- **Symbols:** `query_segments`, `write_bar_knob_descriptor`, `write_cpu_host_memory_descriptor`, `setup_bar_segment`, `BarSegment`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** The AddAdapter-fatal rule (CLAUDE.md: "A SupportsCpuHostAperture segment must be the LAST reported segment") is realized by (a) positional array construction with a comment ("The aperture is ALWAYS first", query_adapter_info.rs:703-710) whose non-production arms (topo 11, default topo) place a SupportsCpuHostAperture RAM segment before the BAR segment — i.e. the code itself can still emit the ETW-proven-rejected orders; and (b) the production BAR flags being a runtime registry value (`BarSegFlags` default 0x1C, :596) so a stray reg write silently produces an invalid descriptor and Code 43. Separately, setup_bar_segment reports a BarSegment even if the reserve of the window head is discarded: start_device.rs:85 "let _ = adapter.with_virtio(|v| v.reserve_window_prefix(size));" — the "offsets below the reserve belong to dxgkrnl" invariant then holds only by happy-path call order.

**Evidence.** query_adapter_info.rs:703-710 "ids are positional (index 0 = id 1). The aperture is ALWAYS first (InitDmaPools validates segdesc[0]...)"; :713-731 match arms where topo-11/default place `Seg::RamCpuHost` (which sets SupportsCpuHostAperture at :556-561) before `Seg::Bar`; :596 "let flags = crate::diag::read_config_dword(b\"BarSegFlags\", 0x1C);" — production flags are a runtime reg value. start_device.rs:84-85 "// The KMD blob-window allocator must never hand out offsets inside the aperture region... let _ = adapter.with_virtio(|v| v.reserve_window_prefix(size));" — result discarded, BarSegment still returned at :88-94.

**Recommendation.** Build the table through a consuming builder: `SegmentTableBuilder::new(ApertureSeg)` (aperture structurally index 0) → `.push_memory(seg)` (rejects SupportsCpuHostAperture flags) → `.finish_cpu_host(seg) -> SegmentTable` (the only way to emit a cpu-host segment, structurally last, at most one). query_segments serializes a prebuilt table. Make `BarSegment` constructible only from a `ReservedPrefix` proof value returned by `reserve_window_prefix` (change it to return a token; the `let _ =` becomes impossible). Restrict `BarSegFlags` to a validated enum of the two shapes that can bind, mapping unknown values to the default with a loud named counter.

**Risk.** Must not alter emitted bytes for the production topology (mode 10, flags 0x1C): golden-compare the descriptor structs before/after. Knob-validation change must keep `reg add` A/B for the surviving shapes.

**Dependencies.** R4 (barsegmode-legacy-arms)

**Atomic commit boundary.** Two commits: (1) reserve-proof token threaded into BarSegment::new; (2) SegmentTable builder replacing the positional array.

**Validation.** AddAdapter binds (no Code 43); ETW AzureTriage clean; segment diag records 0x0900_0002 NbSegment unchanged; desktop + VpSA=1/ScSet=1; blob allocator never hands out an offset below the reserve (existing 0x0B00_0008 records + MapCpuHostAperture counters unchanged).

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Cpu-host-aperture-last and aperture-first hold only for the default knob value + the one surviving match arm; the invalid orders (rejected at AddAdapter with Code 43) and an unreserved-prefix BarSegment are all constructible today.
1. **Compile-time representation:** Consuming SegmentTableBuilder whose finish() is the sole cpu-host-segment emitter; BarSegment::new(ReservedPrefix, ...) proof token from reserve_window_prefix.
1. **Smallest atomic migration:** Reserve-proof token first (tiny), then the builder swap in query_segments.
1. **Remaining `unsafe` preconditions:** Raw descriptor writes into dxgkrnl's caller buffer (pSegmentDescriptor, two-call protocol) stay unsafe — the buffer contract is dxgkrnl's; the builder only guarantees our side's ordering/flags.
1. **Regression test proving preserved behavior:** Byte-compare emitted DXGK_SEGMENTDESCRIPTOR4 array for BarSegMode 10 default-flags against the frozen baseline; boot to desktop.

**Lead-reviewer note.** Encodes the 'SupportsCpuHostAperture segment must be LAST' key invariant (the ETW-proven Code 43 lesson) as a construction-order guarantee instead of match-arm ordering.


### R59. WDDM interface/caps version derived from two interdependent bools re-evaluated at four sites; the invalid combination is expressible and the tri-state is duplicated

- **Category:** static-guarantee · **Reported by:** `kmd-core/wddm-surface-enum`
- **Files:** `kmd_render/src/lib.rs`, `kmd_render/src/ddi/query_adapter_info.rs`
- **Symbols:** `build_ddi_table`, `query_driver_caps`, `query_wddm_device_caps`, `dxgkddi_get_node_metadata`, `RAISE_WDDM_3_2_GPUMMU`, `USE_WDDM_2_1_DISPLAY_SURFACE`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** The same nested if over RAISE_WDDM_3_2_GPUMMU / USE_WDDM_2_1_DISPLAY_SURFACE appears at lib.rs:96-104 (DXGKDDI_INTERFACE_VERSION), query_adapter_info.rs:122-130 (DRIVERCAPS.WDDMVersion) and :285-293 (WDDMDEVICECAPS.WDDMVersion), with a fourth dependent site at :887 (GetNodeMetadata.GpuMmuSupported) and a fifth in the MemoryManagementCaps bits (:213-216). dxgkrnl rejects an internally inconsistent surface at AddAdapter (comment :119-121 "Keep this in sync with..."), yet consistency is by copy-paste. The bools also permit the meaningless state USE_WDDM_2_1_DISPLAY_SURFACE=true with RAISE=false, which silently degrades to 1.3 — surprising coupling documented nowhere.

**Evidence.** lib.rs:96-104 "data.Version = if crate::ddi::query_adapter_info::RAISE_WDDM_3_2_GPUMMU { if ...USE_WDDM_2_1_DISPLAY_SURFACE { DXGKDDI_INTERFACE_VERSION_WDDM2_1 } else { ...WDDM3_2 } } else { ...WDDM1_3 };" — repeated verbatim at query_adapter_info.rs:122-130 and :285-293; :887 "node.GpuMmuSupported = if RAISE_WDDM_3_2_GPUMMU { 1 } else { 0 };"; :119-121 "Keep this in sync with DXGKQAITYPE_WDDMDEVICECAPS and DriverEntry's DRIVER_INITIALIZATION_DATA.Version." — sync by comment.

**Recommendation.** Replace both consts with `enum WddmSurface { Wddm1_3, Wddm2_1GpuMmu, Wddm3_2GpuMmu }` and a single `const ACTIVE: WddmSurface` (currently Wddm2_1GpuMmu) exposing `interface_version()`, `caps_version()`, `gpummu()`; all five sites call these accessors. The invalid bool combination becomes unrepresentable and a future version bump is a one-line change with exhaustive-match coverage.

**Risk.** Trivial; must emit the identical three constants (WDDM2_1 interface version, DXGKDDI_WDDMv2_1, GpuMmuSupported=1) — diff of the 0x01D0 diag record value this boot confirms.

**Atomic commit boundary.** One commit replacing the two consts + five sites.

**Validation.** Boot: 0x01D0 record shows the same WDDMVersion; adapter binds; monitored fences still active (no new gate timeouts); desktop visible.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Version/caps coherence across five sites is maintained by copy-paste; the bools admit an unintended combination (2_1 display surface without the GpuMmu raise) that silently produces a different adapter surface.
1. **Compile-time representation:** Exhaustive `WddmSurface` enum with accessor methods; one source of truth, invalid combinations unrepresentable.
1. **Smallest atomic migration:** Single commit.
1. **Remaining `unsafe` preconditions:** None.
1. **Regression test proving preserved behavior:** 0x01D0/0x01D4 diag records and DxgkInitialize acceptance identical to baseline boot.


### R60. hw_queue_adapter heuristically reinterprets one handle as two struct types by magic sniffing while the destroy paths Box::from_raw without any tag check

- **Category:** static-guarantee · **Reported by:** `kmd-submit/hwqueue-handle-tag`
- **Files:** `kmd_render/src/ddi/scheduler.rs`
- **Symbols:** `hw_queue_adapter`, `HwContext`, `HwQueue`, `dxgkddi_destroy_hw_context`, `dxgkddi_destroy_hw_queue`
- **Verification:** **MODIFIED** (severity low) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** HwContext and HwQueue are two structurally identical {magic:u32, adapter:*mut} structs. hw_queue_adapter reads the pointer as HwQueue, and on magic mismatch re-reads the same memory as HwContext, justified by 'Some WDDM documentation names this first parameter generically' — an accepted-on-a-guess heuristic. Meanwhile dxgkddi_destroy_hw_context/queue call Box::from_raw with the nominal type and no magic verification: correctness of the free currently rests on the two layouts happening to be identical. A future field added to one struct makes the unchecked destroy a heap-corrupting mismatched free.

**Evidence.** scheduler.rs:19-29 two identical structs; :40-54 double interpretation incl. comment :45-47 'Some WDDM documentation names this first parameter generically as a context handle. Accept our HW context too'; :154 `drop(unsafe { Box::from_raw(h_hw_context as *mut HwContext) })` and :187 same for HwQueue — neither checks magic before freeing.

**Recommendation.** Replace both with one `#[repr(C)] struct EngineObj { kind: EngineObjKind /* #[repr(u32)] enum with the two existing magic values */, adapter: NonNull<AdapterContext> }`. One `resolve(handle) -> Option<(&EngineObj, EngineObjKind)>` at the trusted boundary keeps today's accept-either behavior explicitly; destroys verify the tag before from_raw (mismatch -> counted, leak instead of corrupt — matches 'loud failure over fake success'). Wire values unchanged.

**Risk.** Low; HW queues are currently created but submissions return NOT_SUPPORTED, so the path is warm at create/destroy only.

**Atomic commit boundary.** One commit in scheduler.rs.

**Validation.** Boot + adapter restart (create/destroy hw context+queue cycle) clean; diag 0x0700_0001..4 breadcrumbs at DiagLevel>=1 as baseline; no pool corruption under verifier.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Handle-type identity checked by magic in the query path but unchecked in the destroy path; permits a mismatched-type Box::from_raw that is benign only while the two layouts coincide.
1. **Compile-time representation:** Single EngineObj type + exhaustive EngineObjKind enum: one layout by construction, tag matched exhaustively; NonNull removes the separate null-adapter runtime check.
1. **Smallest atomic migration:** scheduler.rs only; handle values dxgkrnl round-trips are unchanged.
1. **Remaining `unsafe` preconditions:** The initial HANDLE->pointer cast from dxgkrnl cannot be proven; the magic/kind check remains a runtime tag at that boundary (FFI round-trip is inherently untyped).
1. **Regression test proving preserved behavior:** Adapter restart loop (pnputil /restart-device) N times: create/destroy counters balanced, no bugcheck, device healthy.

**Verifier corrections (authoritative).** 1) Reframe severity of the current code: destroy-path correctness rests on dxgkrnl's handle round-trip contract, not on the two layouts coinciding; drop "heap-corrupting mismatched free" as a present-tense defect — it is a future-divergence hazard only, contingent on an OS contract violation. 2) Risk/validation: the path is NOT warm at create/destroy only — dxgkddi_present_to_hw_queue (scheduler.rs:221-329) calls hw_queue_adapter on the live direct-primary present path and triggers issue_present_scanout (line 266, commit 6e31f02 "display: scan out the exact DWM primary"); validation must include a visible-desktop present-scanout check (helios_paintcap / VNC) in addition to the adapter-restart create/destroy loop, since a regression here breaks the frozen-baseline direct scanout. 3) Implementation must NOT store the tag as a #[repr(u32)] enum nor the pointer as NonNull in the repr(C) struct that resolve() reads from the untrusted HANDLE: keep the stored fields u32 + *mut (well-defined for any bit pattern), validate with match-u32 -> Option<EngineObjKind> and a null check BEFORE exposing any typed/NonNull view; the static guarantee applies on the create/destroy sides while the resolve boundary stays raw. 4) Mismatch-on-destroy handling (count + leak instead of free) is correct per the loud-failure doctrine; use diag::record (PASSIVE-only, which holds for these DDIs) for the counter.

**Lead-reviewer note.** Verified MODIFIED — implementation constraint: do NOT store a #[repr(u32)] enum or NonNull in the repr(C) struct that resolve() reads from an untrusted HANDLE (forming &EngineObj over garbage would be UB — strictly worse than today). Keep stored fields u32 + *mut; validate then expose a typed view. Path is live per-present via dxgkddi_present_to_hw_queue → issue_present_scanout, so validation includes visible-desktop evidence.


### R61. RenderGdi executor: batch surface mappings freed by a manual loop and command-stream walking split between a hardcoded header check and read_arm

- **Category:** static-guarantee · **Reported by:** `kmd-submit/gdi-batch-raii-cursor`
- **Files:** `kmd_render/src/ddi/gdi_blit.rs`
- **Symbols:** `execute`, `surface`, `SurfMap`, `SurfView`, `read_arm`, `dispatch`
- **Verification:** **MODIFIED** (severity low) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** execute() maps up to 8 blobs via MmMapIoSpace and unmaps them in a trailing manual loop; nothing prevents a future early return between map and unmap from leaking kernel VA mappings every batch. SurfView is Copy with a raw `va` untied to the mapping's life. The batch walker — the exact site of the historical ~48% per-arm drop bug — splits its invariant across a hardcoded `off + 8 <= total` header check, `csize < 8`, and read_arm's offset_of-based per-arm check, i.e. the 'validate per-arm before reading' kernel invariant is enforced by three separated fragments.

**Evidence.** gdi_blit.rs:139-142 manual unmap loop `for m in maps.iter().flatten() { unsafe { MmUnmapIoSpace(...) } }`; :101-109 `#[derive(Clone, Copy)] struct SurfView { va: *mut u8, len: usize, ... }`; :123 `while off + 8 <= total` (hardcoded 8) vs :179 `let payload_off = core::mem::offset_of!(DXGK_RENDERKM_COMMAND, Command)`; :128 `if csize < 8 || off + csize > total { break; }`; :53-57 comment documenting the ~48% drop bug this walker previously caused.

**Recommendation.** (1) `BatchMappings` owning the 8 slots with Drop doing MmUnmapIoSpace (RenderGdi is PASSIVE per its DDI annotation, so Drop is legal), and give SurfView a `'batch` lifetime borrowed from it so a view cannot outlive its mapping — compiler-enforced. (2) A `CmdCursor` iterator over (opcode, remaining-bytes) that owns the header/size arithmetic once, with read_arm as its only payload accessor; the hardcoded 8 becomes the same offset_of constant. No pixel-path changes.

**Risk.** Medium diff size in the hottest GDI path; any accidental change to clipping/pitch selection shows as visual corruption. Keep op_* bodies untouched.

**Atomic commit boundary.** Two commits: (a) BatchMappings RAII + lifetime on SurfView, (b) CmdCursor.

**Validation.** helios_paintcap screenshot parity (ground truth); GdiE/GdiS/GdTc/GdDs deltas match baseline over an identical desktop interaction script; no GdFm/GdFs growth; text renders (CLEARTYPEBLEND path).

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Mapping lifetime = 'until the trailing loop runs' by control-flow convention (an early return leaks 8 kernel mappings per batch); per-arm bounds validity relies on three separate size fragments agreeing.
1. **Compile-time representation:** Drop-owned BatchMappings + SurfView<'batch> (leak/expiry impossible in safe code); CmdCursor that yields only header-validated commands so read_arm is unreachable with inconsistent avail.
1. **Smallest atomic migration:** gdi_blit.rs only; op_* signatures gain a lifetime parameter mechanically.
1. **Remaining `unsafe` preconditions:** MmMapIoSpace/MmUnmapIoSpace and the dxgkrnl-supplied pCommand/pAllocationList extents stay trusted-by-contract; pixel writes remain unsafe bounded by row_ptr.
1. **Regression test proving preserved behavior:** Identical desktop script (open/move/scroll windows, type text) → paintcap image compare + counter parity for GdiE/GdiS/GdTc/GdDs/GdFa..GdFm.

**Verifier corrections (authoritative).** (1) Migration is not "mechanical lifetime parameters on op_*": SurfView is Copy with no borrow of the map table BY DESIGN (gdi_blit.rs:100) because ops hold a view across a second surface(&mut maps, ...) call (e.g. op_bitblt :459-464); SurfView<'batch> borrowing the table will not borrow-check — the design must switch the 8-slot table to shared-borrow + interior mutability (once-set Cell slots) or index-based views. (2) State explicitly that CmdCursor must keep avail = total - off (remaining batch bytes, what dispatch gets at :135) and never clamp to CommandSize, or it re-introduces the 48%-drop bug class. (3) Scope the leak claim: there is NO current leak — all returns in execute() are before any mapping (:115-117) or after the unmap loop (:147-150), and walker breaks fall through; the RAII guarantee is purely prophylactic, and Drop does not cover panics (kernel panic=abort; DDI panics forbidden regardless). (4) Missing value context that downgrades severity: ROADMAP.md:429-440 and :1220 — GdiAccelMode=0 A/B passed and the executor is slated for retirement ("retire the gdi_blit executor + flip the compiled default"); compiled default is still 1 (start_device.rs:126) so the module is live on default installs, but a medium-diff refactor with visual-corruption risk in a module scheduled for deletion should be deferred until the retirement decision lands; do it only if retirement is abandoned.

**Lead-reviewer note.** Verified MODIFIED — DEFER: ROADMAP marks the gdi_blit executor for retirement after the passed GdiAccelMode=0 A/B. Implement only if the retirement decision is abandoned; if implemented, CmdCursor must keep avail = total - off (the 48%-drop lesson) and SurfView needs the interior-mutability redesign described in the corrections.


### R62. The ISR's 'nonzero isr_status implies a valid dxgkrnl callback table' contract rests on StartDevice side-effect ordering documented in a comment; violation silently drops the DPC after claiming the interrupt

- **Category:** static-guarantee · **Reported by:** `kmd-transport-ctrl/isr-publication-pair`
- **Files:** `kmd_render/src/ddi/interrupt.rs`, `kmd_render/src/ddi/start_device.rs`
- **Symbols:** `dxgkddi_interrupt_routine`, `AdapterContext::isr_status`, `AdapterContext::dxgkrnl`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** interrupt.rs:86-88 encodes a two-field publication order in prose: 'The register VA is published lock-free by StartDevice (isr_status, Release) BEFORE the transport goes live, and adapter.dxgkrnl is written before that — so a nonzero isr_status implies a valid callback table.' The ISR acts on that implication: after the read-to-clear (which has already deasserted the line and consumed the ISR-status bits), it reaches 'if let Some(dxgkrnl) = adapter.dxgkrnl.as_ref() { if let Some(queue_dpc) = ... }' (interrupt.rs:122-123). If a refactor ever reorders StartDevice or a teardown path clears the fields asymmetrically, the None arms silently skip queueing the DPC after the interrupt was claimed and acknowledged — a lost used-ring drain with no counter, surfacing as mysterious wait-slice-latency completions.

**Evidence.** interrupt.rs:86-88 '// The register VA is published lock-free by StartDevice (`isr_status`, Release) BEFORE the transport goes live, and `adapter.dxgkrnl` is written before that — so a nonzero `isr_status` implies a valid callback table.'; interrupt.rs:99-102 'let isr_va = adapter.isr_status.load(Ordering::Acquire); if isr_va == 0 { return 0; }'; interrupt.rs:106 read_volatile is read-to-clear ('the read clears + deasserts'); interrupt.rs:121-128: after clearing, 'if status & 0x3 != 0 { if let Some(dxgkrnl) = adapter.dxgkrnl.as_ref() { if let Some(queue_dpc) = dxgkrnl.DxgkCbQueueDpc {' — two silent-skip Option arms guarding the DPC that the comment declares unreachable.

**Recommendation.** Publish one immutable pairing: a single AtomicPtr (or AtomicUsize) to a StartDevice-allocated IsrLive { isr_status: NonNull<u8>, dxgkrnl: &'static-lifetime interface ref } — one Acquire load in the ISR yields either null (not ours) or both proofs together, deleting the unreachable-by-contract None arms. Alternatively (smaller): make the ISR count the None case loudly (new DIRQL-safe atomic) so a future ordering regression is at least visible — but the pairing type is the real fix.

**Risk.** Low: ISR body stays the same length (one load + derefs); the IsrLive allocation must outlive IoDisconnectInterrupt-equivalent teardown — same lifetime the two fields already require.

**Atomic commit boundary.** One commit: IsrLive struct + StartDevice publication + ISR/DPC consumption; teardown clears the single pointer.

**Validation.** Reboot with new KMD image; INT_ROUTINE_COUNT and DPC_ROUTINE_COUNT advance together this boot; no interrupt-storm Code 43; standard regression gate.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** nonzero isr_status ⇒ dxgkrnl callback table valid, guaranteed only by StartDevice's write order. Invalid sequence permitted: publish isr_status before dxgkrnl (or clear asymmetrically at stop) → ISR claims + acknowledges the interrupt (line deasserted, status consumed) but silently never queues the DPC → lost completions until the next interrupt.
1. **Compile-time representation:** Single AtomicPtr<IsrLive> where IsrLive owns NonNull<u8> (ISR register VA) and the callback-table reference; one Acquire load proves both; the Option arms and their silent-skip behavior become unrepresentable.
1. **Smallest atomic migration:** interrupt.rs + start_device.rs publication/teardown in one commit.
1. **Remaining `unsafe` preconditions:** The MMIO volatile read, the miniport-context cast, and 'IsrLive is not freed while the interrupt is connected' (interrupt-disconnect ordering) remain unsafe — hardware/dxgkrnl ABI facts outside the type system.
1. **Regression test proving preserved behavior:** Same-boot INT_ROUTINE_COUNT/DPC_ROUTINE_COUNT both advancing, CONTROL_INT_COUNT sane, no Code 43, visible desktop with normal present cadence.


### R63. Three incompatible DDI-handle payload layouts behind *mut c_void, discriminated only by caller convention

- **Category:** static-guarantee · **Reported by:** `umd-forward-a/handle-payload-type-confusion`
- **Merged duplicate reports (2):** `xc-unsafe/umd-handle-typed-wrappers` — UMD handle layer: two incompatible pDrvPrivate layouts behind bare *mut c_void; 267 unsafe fns with 3 SAFETY comments; `umd-forward-b/drv-private-slot-contract` — The 8-byte pDrvPrivate slot contract (calc_size literal 8 ↔ one pointer word ↔ which loader may read it) is convention only
- **Files:** `umd/src/forward.rs`
- **Symbols:** `store_com`, `load_com`, `release_com`, `store_resource`, `load_resource`, `release_resource`, `store_rtv`, `load_rtv`, `release_rtv`, `discard_11_1`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 3 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** A DDI pDrvPrivate slot may hold: a raw owned COM pointer (store_com 451-453, shaders/DSVs/samplers), a Box<ResourceState> pointer (store_resource 474-495), or a Box<RtvState> pointer (store_rtv 972-993). All accessors take *mut c_void; the bindgen handle types (D3D10DDI_HRESOURCE vs D3D10DDI_HRENDERTARGETVIEW vs D3D10DDI_HSHADER) are distinct but the helpers erase them. load_com::<T> on a resource handle would reinterpret a Box pointer as a COM object (UB, vtable call into a Rust struct); nothing but discipline prevents it. discard_11_1/clear_view_11_1 receive (handle_type, *mut c_void) and re-dispatch at runtime (2917-2948). resolve_shared_resource additionally reinterprets a bare ddi::HANDLE as both the device (2134-2136) and the resource private pointer (2120-2127).

**Evidence.** forward.rs:451-453 `unsafe fn store_com<T: Interface>(handle_priv: *mut c_void, obj: T) { *(handle_priv as *mut *mut c_void) = obj.into_raw(); }`; :537 `let state = *(handle_priv as *const *mut ResourceState);`; :999 same cast to *mut RtvState; :1110-1119 `load_com<T>` returns ManuallyDrop<T> from the same untyped slot; :2917-2948 runtime dispatch on D3D11DDI_HANDLETYPE; :2123 `(*arg).hResource as *mut c_void` and :2134-2136 `d3d11_context(Hdevice { pDrvPrivate: h as *mut c_void })`.

**Recommendation.** Introduce a sealed HandlePayload trait mapping each bindgen handle newtype to its payload type (D3D10DDI_HRESOURCE→ResourceState, HRENDERTARGETVIEW→RtvState, HSHADER/HDEPTHSTENCILVIEW/…→RawCom<T>), with load/store/release generic over the handle type instead of *mut c_void. Keep discard_11_1/clear_view_11_1 and resolve_shared_resource as the small trusted boundary that converts (runtime type tag, raw ptr) → typed handle exactly once. Pure refactor: identical layouts, identical codegen.

**Risk.** Low: mechanical; the danger is a mis-mapped handle type, which review of the device_funcs table assignment catches (each DDI slot names its handle type).

**Dependencies.** R14 (split-forward-rs)

**Atomic commit boundary.** One commit in handle.rs converting the accessors + all in-file callers; no ABI or layout change.

**Validation.** Release build; full regression gate (desktop, VpSA/ScSet, cadence); dxvk-tests / samples exercising every view type; HANDLE_MISS/noop counters unchanged.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** "This pDrvPrivate slot holds layout X" exists only in each call site's head; the invalid sequence load_com::<ID3D11Resource>(resource_handle) or load_resource(rtv_handle) compiles today and is instant UB.
1. **Compile-time representation:** Sealed trait HandlePayload { type Payload; } implemented for each bindgen handle newtype; fn load<H: HandlePayload>(h: H) -> Option<&H::Payload>; RawCom<T> wrapper for the plain-COM classes.
1. **Smallest atomic migration:** handle.rs + call-site type substitutions in one commit; the (handle_type, *mut c_void) DDIs stay as the only unchecked conversion, in one function.
1. **Remaining `unsafe` preconditions:** The runtime's promise that pDrvPrivate points at the CalcPrivate*Size-sized slot, and COM pointer liveness, cannot be encoded — that is the trusted boundary.
1. **Regression test proving preserved behavior:** Full desktop + dxvk-tests pass; grep proves no remaining `as *const *mut ResourceState`-style casts outside handle.rs.

**Lead-reviewer note.** Three reports on the same trust boundary: three incompatible payload layouts behind *mut c_void, 267 unsafe fns with 3 SAFETY comments, and the 8-byte pDrvPrivate slot convention. One tagged-payload design closes all three.


### R64. cxx bridge passes owned and borrowed COM/resource pointers as indistinguishable bare usize — ownership lives in comments, adoption in hand-rolled Drop impls

- **Category:** unsafe-contract · **Reported by:** `umd-core/cxx-com-ownership-newtypes`
- **Files:** `umd/src/bridge.rs`, `umd/src/device_funcs.rs`, `umd/src/lib.rs`
- **Symbols:** `ffi::HeliosDxvkDevice`, `d3d11_device_ptr`, `open_ddi_texture2d`, `create_ddi_scanout_texture2d`, `open_kmd_scanout_target`, `PresentSrcEntry`, `HeliosDevice::scanout_resource_raw`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Every bridge return of a COM pointer is a bare usize whose ownership polarity is comment-only: bridge.rs:20-23 'Raw ID3D11Device*... Borrowed — the bridge keeps the owning ref'; bridge.rs:77-78 'Returns an owned ID3D11Resource* (as usize)'; open_ddi_texture2d (50-63) is owned per the doc on PresentSrcEntry (device_funcs.rs:33-34). Rust-side handling is bespoke per site: PresentSrcEntry hand-implements Drop with from_raw adoption (device_funcs.rs:37-52); scanout_resource_raw is a non-owning usize Cell (77-81) while scanout_import holds a genuinely owned Option<ID3D11Resource> (89), sometimes both referencing the same object (forward.rs:716+724); the selftest does a ManuallyDrop::new(from_raw(..)) borrow dance (lib.rs:275-277). Nothing prevents adopting an owned pointer twice (double Release), never adopting it (leak), or using a borrowed device pointer after the UniquePtr drops.

**Evidence.** bridge.rs:20-23 '/// Raw `ID3D11Device*` / `ID3D11DeviceContext*` (as usize)... Borrowed — the bridge keeps the owning ref; wrap on the Rust side without taking ownership.'; bridge.rs:77-78 '/// Returns an owned `ID3D11Resource*` (as usize), or 0 on failure.'; device_funcs.rs:37-52 manual Drop: 'SAFETY: resource_raw is the owned COM ref returned by open_ddi_texture2d; from_raw adopts it so drop releases it'; device_funcs.rs:77-81 'Non-owning pointer...' vs :89 owned scanout_import; lib.rs:275-277 'ManuallyDrop::new(unsafe { ...from_raw(dev_ptr as *mut _) })'.

**Recommendation.** Add a small trusted module (e.g. umd/src/com.rs): `#[repr(transparent)] struct OwnedCom(NonNull<c_void>)` whose Drop calls Release exactly once, with `fn as_raw(&self)` and a consuming `into_raw`; and `struct BorrowedCom<'a>(NonNull<c_void>, PhantomData<&'a ffi::HeliosDxvkDevice>)` returned by safe wrapper fns over d3d11_device_ptr/d3d11_context_ptr so the borrow cannot outlive the bridge device. The cxx signatures stay usize (cxx cannot express COM ownership); only this module converts, so every other file handles typed ownership. Convert PresentSrcEntry (deleting its manual Drop) and the scanout fields first.

**Risk.** Medium-low: refcount behavior must be bit-identical (one adopt per owned return, zero for borrowed). Wrappers must not 'merely relocate unchecked casts' — keep raw conversion private to the module and make each bridge wrapper's polarity match its C++ implementation, verified against bridge/dxvk_bridge.cpp.

**Dependencies.** verify each wrapper's polarity against bridge/dxvk_bridge.cpp before landing

**Atomic commit boundary.** Two commits: (1) introduce com.rs + typed wrapper fns over the bridge, converting PresentSrcEntry; (2) convert scanout/composition_source fields and the remaining forward.rs call sites.

**Validation.** Release UMD build; repeated dwm restart + app open/close cycles show no new leaks (working-set/log deltas) and no premature-release crashes; device teardown logs unchanged; visible desktop; VpSA=1/ScSet=1.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Which usize returns carry a +1 COM ref is documented per-function in comments; call sites must remember to adopt exactly once. Permitted invalid sequences: double from_raw adoption (double Release → UAF in dwm), missed adoption (leak per frame source), borrowed device/context pointer used after the owning UniquePtr<HeliosDxvkDevice> drops.
1. **Compile-time representation:** OwnedCom (Drop = single Release, NonNull) vs BorrowedCom<'a> tied to &'a HeliosDxvkDevice; bridge returns converted in exactly one private module, everything downstream safe and polarity-typed.
1. **Smallest atomic migration:** Wrapper module + PresentSrcEntry conversion is independently shippable; scanout field conversion follows.
1. **Remaining `unsafe` preconditions:** The C++ side's actual AddRef behavior per function is unverifiable from Rust — polarity of each wrapper is a trusted, per-function audit against dxvk_bridge.cpp. Raw usize transport through cxx cannot carry NonNull or lifetime; the single conversion module is the trusted boundary.
1. **Regression test proving preserved behavior:** Leak/UAF soak: N dwm restarts + M app swapchain create/destroy cycles with before/after handle- and memory-counters, plus unchanged teardown log sequences.


### R65. Hand-rolled adapter/CreateDevice ABI mirrors duplicate structs bindgen already generates, with layout guaranteed only by an offsets comment

- **Category:** static-guarantee · **Reported by:** `umd-core/adopt-bindgen-adapter-structs`
- **Files:** `umd/src/lib.rs`, `umd/src/ddi.rs`
- **Symbols:** `D3d10DdiArgOpenAdapter`, `D3d10DdiAdapterFuncs`, `D3d10_2DdiAdapterFuncs`, `D3d10_2DdiArgGetCaps`, `DxgiDdiBaseArgs`, `D3d10DdiArgCreateDevice`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** lib.rs:46-120 hand-declares repr(C) mirrors of the adapter-open ABI. Their correctness rests on a comment: lib.rs:104-106 'Offsets (x64): hRTDevice@0, interface@8, version@12, ... ppfnRetrieveSubObject@80'. Meanwhile ddi.rs:16 includes the bindgen output which already contains the authoritative D3D10DDIARG_OPENADAPTER, D3D10DDIARG_CREATEDEVICE, D3D10DDI_ADAPTERFUNCS, D3D10_2DDI_ADAPTERFUNCS, D3D10_2DDIARG_GETCAPS, and DXGI_DDI_BASE_ARGS (verified in generated d3d10umddi.rs lines 15071/24668/24750/24779/24977/25053). The project already distrusts the mirror enough to raw-dump the first 12 quadwords of CreateDevice args at runtime (lib.rs:673-681), and lib.rs:667-672 documents the real historical failure mode of a layout misread (11.1 handlers wired into 11.0 slots, VUID-Input-08733).

**Evidence.** lib.rs:98-106 '/// `D3D10DDIARG_CREATEDEVICE` (d3d10umddi.h, WDK 10.0.26100, x64), laid out field-for-field... Offsets (x64): hRTDevice@0, interface@8, version@12...'; lib.rs:673-681 runtime layout distrust: 'let q = args as *const u64; ... "CreateDevice raw args:" ... read_unaligned()'. ddi.rs:16 'include!(concat!(env!("OUT_DIR"), "/d3d10umddi.rs"))' — generated file defines D3D10DDIARG_CREATEDEVICE (line 24668) and D3D10DDIARG_OPENADAPTER (line 25053).

**Recommendation.** Replace the hand mirrors with the ddi:: bindgen types in OpenAdapter10/10_2, open_adapter_common, create_device, get_caps, and get_supported_versions. If a hand type must remain (e.g. to keep the crate's public export signature stable), add const assertions: size_of equality plus core::mem::offset_of! pins for every field against the bindgen twin. The raw-dump instrumentation can then be retired or demoted behind trace_enabled().

**Risk.** Medium-low: bindgen's unions/anon fields make some accesses more verbose; a wrong field mapping during the port is exactly the bug class this prevents, so port with offset_of! assertions in place first, then swap types.

**Atomic commit boundary.** Two commits: (1) add offset_of!/size_of const assertions binding each mirror to its bindgen twin (pure additive, proves current layout); (2) swap usages to ddi:: types and delete mirrors.

**Validation.** Release UMD build with const asserts (compile fails on any drift); umd log shows identical 'CreateDevice interface=0x... version=0x...' values before/after on the same boot; dwm device create S_OK; visible desktop; VpSA=1/ScSet=1.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** The hand mirror's field offsets must equal the WDK header layout; nothing checks this. A WDK/bindgen update or a field edit in the mirror silently shifts interface/version/pDeviceFuncs reads, selecting the wrong funcs-table layout — the exact historical bug at lib.rs:667-672 (11.1 handlers in 11.0 slots).
1. **Compile-time representation:** Use the bindgen-generated structs directly (single source of truth), or const `offset_of!`/`size_of` assertions binding every mirror field to its bindgen twin so any drift is a compile error.
1. **Smallest atomic migration:** Commit 1: const assertions only (no behavior change, proves today's layout). Commit 2: swap the six types at their ~10 usage sites and delete mirrors.
1. **Remaining `unsafe` preconditions:** The runtime still hands an untyped *mut c_void through the DDI; trusting that pointer to be a D3D10DDIARG_CREATEDEVICE at all remains an unverifiable FFI precondition. Bindgen fidelity to the installed WDK header is trusted, not proven, at runtime.
1. **Regression test proving preserved behavior:** Same-boot before/after comparison of the logged interface/version/pointer fields in 'CreateDevice interface=...' for a dwm device; desktop visible; no noop-counter regression.


### R66. Four near-identical device-funcs fill functions plus a magic-hex, ordering-load-bearing interface dispatch; prefix-compat and slot-shape contracts enforced only by comments

- **Category:** static-guarantee · **Reported by:** `umd-core/unify-table-fill-and-version-dispatch`
- **Files:** `umd/src/device_funcs.rs`, `umd/src/lib.rs`
- **Symbols:** `fill_d3d11_device_funcs`, `fill_d3d11_1_device_funcs`, `fill_wddm1_3_device_funcs`, `fill_wddm2_1_device_funcs`, `fill_dxgi_base_funcs`, `fill_dxgi_1_1_base_funcs`, `fill_dxgi_1_3_base_funcs`, `create_device`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** device_funcs.rs repeats the same pattern four times (457-500, 505-545, 547-589, 591-633): a bulk loop writing Some(ddi_noop_device) over the whole struct viewed as Option<fn> slots, an 18/19-entry calc! macro list duplicated verbatim, then layered forward::install* calls. Two comment-only contracts underwrite it: the ABI note (device_funcs.rs:11-15, uniform `extern "C" fn(usize)->usize` stub transmuted into every slot) and prefix-compat ('The D3D11.1 layout is an extension of the D3D11.0 prefix', 504-505) justifying casts like `&mut *(funcs as *mut ddi::D3D11DDI_DEVICEFUNCS)` at 512/554/598. The selector in lib.rs:756-787 is a descending `if create.interface >= 0x000b_0022 ... >= 0x000b_0010 ... >= 0x000b_000f` chain of raw hex whose ordering is load-bearing and whose device-table/DXGI-table pairing (WDDM2.1+WDDM1.3→DXGI1_3, 11.1→DXGI1_1, 11.0→base) is repeated by hand per branch; lib.rs:667-672 records a real shipped bug from exactly this kind of layout/interface misread.

**Evidence.** device_funcs.rs:459-463 'let n = size_of::<D3D11DDI_DEVICEFUNCS>() / size_of::<usize>(); let slots = funcs as *mut Option<UniformFn>; for i in 0..n { *slots.add(i) = Some(ddi_noop_device); }' repeated at 506-510, 548-552, 592-596; calc! lists at 473-492, 519-539, 561-581, 605-625; prefix casts 512/554/598 justified only by 504-505 comment. lib.rs:756/764/772 'if create.interface >= 0x000b_0022 ... else if ... >= 0x000b_0010 ... else if ... >= 0x000b_000f'. lib.rs:667-672: past bug — 'a misread here silently wires typed 11.1 handlers into slots an 11.0-negotiated device never calls'.

**Recommendation.** Introduce `enum DdiLevel { D3d11_0, D3d11_1, Wddm1_3, Wddm2_1 }` with `from_interface(u32)` using named version constants shared with SUPPORTED_DDI_VERSIONS, and one exhaustive match that yields the (device fill, dxgi fill) pair — making the pairing and the descending order structural. Factor a generic `unsafe fn stub_fill<T>(p: *mut T, stub: UniformFn)` carrying `const { assert!(size_of::<T>() % size_of::<Option<UniformFn>>() == 0) }`, and one shared calc-list application on the common D3D11 prefix (per-version delta for pfnCheckDeferredContextHandleSizes). Pin prefix-compat with const offset_of! assertions (e.g. pfnDestroyDevice, pfnRelocateDeviceFuncs, last-shared-slot offsets equal across the four table types).

**Risk.** Medium: the fill order (stub fill → calc overrides → destroy/relocate → layered installs) is behaviorally significant; keep it identical. Transmute-based calc! stays inside the one trusted helper.

**Dependencies.** R65 (adopt-bindgen-adapter-structs)

**Atomic commit boundary.** Two commits: (1) DdiLevel enum + named constants replacing the hex chain in create_device (no table changes); (2) generic stub_fill + shared calc list + prefix const-assertions in device_funcs.rs.

**Validation.** Release UMD build; selftest (if still present) reports 0 null slots; audit_wddm1_3/audit_dxgi_1_3 logs show identical slot values before/after on the same boot; dwm + an app device (11.0 and WDDM1.3 negotiations) both create S_OK; visible desktop; noop hit counters do not regress.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** (a) Every slot of each funcs table is a pointer-sized Option<fn> compatible with the uniform stub ABI; (b) the 11.1/WDDM1.3/WDDM2.1 tables begin with a bit-identical D3D11.0 prefix; (c) the >= chain must be checked in descending order and each branch must pair the right DXGI table. Violations compile clean today and manifest as silent slot corruption or wrong-layout handlers.
1. **Compile-time representation:** Exhaustive `match DdiLevel` for dispatch+pairing; generic stub_fill with const size assertions; const offset_of! equality assertions across the four table types for landmark fields, so a bindgen/WDK layout change breaks the build instead of the desktop.
1. **Smallest atomic migration:** Enum-dispatch commit is independently shippable; fill-dedup commit touches only device_funcs.rs internals with unchanged public fill_* signatures.
1. **Remaining `unsafe` preconditions:** Cannot prove every bindgen field of the tables is a function-pointer slot (no field reflection), nor that the x64 'uniform stub reads only arg0, returns RAX' ABI claim holds for every slot's true signature — both stay documented trusted assumptions inside one small helper.
1. **Regression test proving preserved behavior:** Same-boot slot-dump parity via the existing audit_* logs (all 4 table variants), zero-null-slot check, dwm + app devices at 11.0/11.1/WDDM1.3/WDDM2.1 negotiations, visible desktop.


### R67. Vehicle per-thread contract lives in three independently-mutated TLS cells and a u8 sentinel device state; encode as single typed state machines

- **Category:** static-guarantee · **Reported by:** `umd-forward-c/vehicle-state-machine-types`
- **Merged duplicate reports (1):** `umd-core/flip-wait-typestate` — Kernel flip-wait state machine is a sentinel u8 (0/1/2) with a separately-stored fence handle valid only in state 1
- **Files:** `umd/src/forward.rs`, `umd/src/device_funcs.rs`
- **Symbols:** `PRESENT_SOURCE`, `LAST_VEHICLE_DEVICE`, `PRESENT_RESULT`, `flip_wait_setup`, `wait_last_present`, `HeliosDevice::flip_wait_state`
- **Verification:** **MODIFIED** (severity low) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** The same-thread vehicle contract spans three separate TLS cells (forward.rs:7562-7580) that must be updated in lockstep: failure resets at 8345-8347 and 8405-8409, success writes at 8543-8550 — nothing prevents a future edit from updating one and not the others (e.g. PRESENT_RESULT set while LAST_VEHICLE_DEVICE==0). LAST_VEHICLE_DEVICE is a raw usize deref'd in wait_last_present (7742) under a comment-only lifetime ('Valid ONLY inside the ICD's present-call window'). Separately, HeliosDevice::flip_wait_state is a Cell<u8> with documented sentinels 0/1/2 (device_funcs.rs:108-112) plus two sibling cells (flip_wait_fence, flip_wait_next_value) whose validity is coupled to state==1 by convention only — state==1 with fence==0 is representable.

**Evidence.** forward.rs:7565-7570 '/// Valid ONLY inside the ICD's present-call window ... 0 after a failed vehicle present' (comment-enforced lifetime); 7742 'let dev = unsafe { &*(dev_ptr as *const HeliosDevice) };'; lockstep writes 8345-8347 vs 8543-8550; device_funcs.rs:108-112 '0 = unprobed, 1 = ready, 2 = disabled'; 7619-7623 'match dev.flip_wait_state.get() { 1 => return true, 2 => return false, _ => {} }'.

**Recommendation.** TLS: one Cell<VehicleThreadState> enum { Idle, SourcePending(PresentSource), Presented { dev: NonNull<HeliosDevice>, result: Option<(FenceId, SyncValue)> } } with transition methods (arm/fail/mint/consume) so desynchronized combinations are unrepresentable and every counter increment lives inside the transition. Device: replace the tri-state u8 + loose cells with Cell<FlipWaitState> where Ready carries the NonZero fence handle and next value, Disabled carries nothing — flip_wait_setup becomes the validate-once constructor of Ready.

**Risk.** Must preserve exact counter semantics (EXT_OVERWRITES on replace, EXT_RESULT_MISSES/OVERWRITES on take/replace) and the exact reset points; enum-in-Cell needs Copy or swap-based transitions.

**Dependencies.** R14 (forward-split-modules)

**Atomic commit boundary.** Commit 1: FlipWaitState enum (device side, 3 read sites). Commit 2: VehicleThreadState TLS consolidation.

**Validation.** Vehicle probe run (vehicle_flipwait_probe / vkcube): EXT_* counter progression identical to baseline; wait_last_present returns unchanged; no new gate timeouts; DComp 63 fps.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Three TLS cells + a u8 sentinel must transition in lockstep; permitted invalid states: result pending with no device recorded, flip_wait_state==1 with fence==0, stale device pointer consumed after present failure path missed a reset.
1. **Compile-time representation:** Single Cell<enum> per concern with transition methods; Ready variant carries NonZero fence + value so 'ready without fence' cannot exist; result only exists inside Presented.
1. **Smallest atomic migration:** forward/vehicle.rs (post-split) + 3 device-field read sites; C exports keep their i32 ABI.
1. **Remaining `unsafe` preconditions:** The Presented device pointer's liveness still rests on the ICD holding the vehicle D3D11 device across Present->wait (same-thread C contract); cannot be encoded — keep the SAFETY comment on the single deref site.
1. **Regression test proving preserved behavior:** Vehicle present loop with induced failure (unplug publish) shows identical counter/log sequence; get_present_result miss/consume behavior byte-identical.

**Verifier corrections (authoritative).** 1) The proposed VehicleThreadState { Idle, SourcePending(PresentSource), Presented{dev, result} } is behavior-altering: arming a new source must NOT drop the previous present's device. In the documented 'Present succeeded at DXGI level but DDI never invoked' flow (forward.rs:7713-7716; ICD success-status diag), the ICD's fallback wait (icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:2228) currently performs a real bounded gate on the prior device (wait_present returns 0); the proposed enum makes it return -1 and inflates the ICD-side helios_vehicle_wait_timeouts counter. Fix the shape: SourcePending must carry the last Presented state (e.g. Armed { src, last: Option<Presented> }) or the device pointer stays a separate cell. 2) 8543-8550 is not a 'success write': it also runs when pfnPresentCb fails (present_hr < 0) — the mint transition must key off vehicle_present_prepare success, not present success. Validation must therefore also cover ICD-side counters (helios_vehicle_wait_timeouts / gate_arms / gate_fallbacks), not only the UMD EXT_* set. 3) Scope tightened: commit 1 (FlipWaitState enum, sites forward.rs:7619/7625/7676-7677/8439-8442 + init lib.rs:739-741) is safe as written — no 1->2 transition exists so next_value can live in Ready; only commit 2 needs the redesign. 4) Framing: 'state==1 with fence==0 is representable' is type-level only — runtime-unreachable today (h_fence==0 check at 7664-7666 precedes state.set(1)); the finding proposes edit-proofing, not a live-bug fix, and severity should be read accordingly.

**Lead-reviewer note.** Verified MODIFIED — commit 1 (FlipWaitState enum over the sentinel u8 + fence handle) is safe as written and subsumes the merged flip-wait-typestate report. Commit 2 (TLS vehicle state) needs the corrected shape: arming a new source must NOT drop the previous present's device (Armed { src, last: Option<Presented> }), and the mint transition keys off vehicle_present_prepare success, not present success; validate with the ICD-side vehicle counters.


### R68. Present routing is comment-enforced boolean mutation (published_to_scanout/copied) instead of an exhaustive route type sealing the exact-primary path

- **Category:** static-guarantee · **Reported by:** `umd-forward-c/present-route-enum`
- **Files:** `umd/src/forward.rs`
- **Symbols:** `dxgi_present`, `presented_primary_private`, `copy_to_scanout_target`, `publish_dwm_composition`
- **Verification:** **MODIFIED** (severity low) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** dxgi_present classifies each present by mutating two bools through side-effectful stages: direct primary (presented_primary_private 8357), legacy src->dst copy (8358-8371), legacy LINEAR scanout-copy fallback (8372-8378), then a publish_dwm_composition fallback guarded only by '!published_to_scanout' with the invariant in a comment: 'otherwise this records the same full-frame copy twice' (8396-8398). The authoritative private data is then RE-derived at 8504 for pfnPresentCb; nothing ties the two lookups together. The sealed-primary rule (a Windows-designated OPTIMAL primary must never take the LINEAR copy/diagnostic route) exists only as runtime ordering of these branches.

**Evidence.** forward.rs:8356-8357 'let mut published_to_scanout = presented_primary_private(h, src_h.pDrvPrivate).is_some();'; 8372-8378 fallback mutates both flags; 8396-8398 '// Use the RTV-tracking fallback only when no present source could be copied, otherwise this records the same full-frame copy twice.'; 8504 'let present_private = presented_primary_private(h, src_h.pDrvPrivate);' re-lookup; sealing precedent at 744-751 (track_dwm_composition_target refuses direct primaries at runtime only).

**Recommendation.** Classify once: enum PresentRoute { DirectPrimary(HeliosPresentPrivateData), BltCopy, ScanoutFallback, NoRoute } computed by a single pure function from (resource state, allocations, device); match exhaustively for the copy phase, the publish-fallback decision, and the pfnPresentCb private-data (taken from the DirectPrimary variant, eliminating the second lookup). Only the DirectPrimary arm can produce PrivateDriverData; fallback arms structurally cannot, which is the sealed-interface property the frozen baseline wants. Reuse the same classifier in dxgi_present1.

**Risk.** Route classification must be bit-identical to today's short-circuit order (direct-primary wins; scanout fallback only when dst_alloc==0); the double lookup being collapsed to one is safe (same thread, no rotation between) but must be stated in the commit.

**Dependencies.** R25 (present-tail-dedup)

**Atomic commit boundary.** One commit: introduce PresentRoute + classifier, convert dxgi_present; second commit converts dxgi_present1.

**Validation.** Visible desktop after adapter restart, VpSA=1/ScSet=1, ScanoutDiag absent; same-boot QEMU evidence of the real OPTIMAL DWM primary; scanout_copy_count ('DWM desktop->LINEAR scanout copy' log, forward.rs:806-813) must NOT start moving during direct-primary operation — that is the double-copy regression detector.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Mutually exclusive present routes tracked as two mutable bools plus branch order; permits publishing the LINEAR fallback copy AND private scanout data for the same frame, or double-recording the full-frame copy, if a future edit reorders branches.
1. **Compile-time representation:** Exhaustive PresentRoute enum from a single classifier; PrivateDriverData constructible only from the DirectPrimary variant (private constructor), so fallback/diagnostic routes cannot enter the exact-primary path.
1. **Smallest atomic migration:** dxgi_present body only (then present1); no ABI, KMD, or bridge change.
1. **Remaining `unsafe` preconditions:** resource state derefs behind pDrvPrivate remain unsafe (runtime-owned memory); classifier still trusts DXGI to pass tracked handles.
1. **Regression test proving preserved behavior:** Direct-primary boot: scanout-copy counter static, VpSA/ScSet=1, desktop visible; legacy blt-model probe (d3d11_triangle BLT) still renders.

**Verifier corrections (authoritative).** 1) VEHICLE ROUTE OMITTED: dxgi_present has a fifth route (forward.rs:8332-8351) selected by side-effectful TLS PRESENT_SOURCE.take() — it cannot come from "a single pure function from (resource state, allocations, device)". The 8504 private-data lookup also executes for vehicle presents, so "PrivateDriverData taken from the DirectPrimary variant" would change the vehicle-src-is-direct-primary edge case. Fix: add a Vehicle variant fed by the TLS take (classifier pure over the remaining inputs), or scope the classifier to the non-vehicle else-branch and keep the pfnPresentCb private-data derivation route-independent. 2) PRESENT1 REUSE OVERBROAD: dxgi_present1 multi-surface (9187-9194) has NO BltCopy stage — direct-primary || scanout-fallback then publish only; a shared classifier returning BltCopy there would introduce a src->dst CopySubresourceRegion that never happens today. Reuse must map BltCopy=>no-copy for present1-multi (single-surface already delegates to dxgi_present at 9133-9146). 3) SCOPE TIGHTENED: `copied` is telemetry-only (sole read at 8591 forensics log); only published_to_scanout carries behavior. The enum must still reproduce copied's per-route log value (vehicle/blt/fallback => true) to keep cross-boot forensic lines comparable. 4) The double-lookup collapse safety claim should cite the concrete writers proven quiescent mid-present: remember_direct_scanout_allocation (1618, resource creation) and dxgi_rotate_resource_identities (8736/8741), both DXGI-runtime-serialized against Present.

**Lead-reviewer note.** Verified MODIFIED — the route classifier must include the Vehicle variant fed by the side-effectful TLS take (or scope the classifier to the non-vehicle branch); present1-multi has NO BltCopy stage, so classifier reuse there must map BltCopy=>no-copy; `copied` is telemetry-only but its per-route log value must be reproduced for cross-boot forensic comparability. This entry is the sealing mechanism tranche 1 items route through.


### R69. Frame-gate selection (KEEP contracts) is expressed as scattered sentinel integers and bools; encode gate kind and armed-wait proof in types, and count direct-primary gate expiries

- **Category:** static-guarantee · **Reported by:** `umd-forward-c/gate-kind-static-classification`
- **Files:** `umd/src/forward.rs`, `umd/src/lib.rs`
- **Symbols:** `dxgi_present`, `present_gate_us`, `vehicle_flip_gate_us`, `EXT_FLIP_GATE_TIMEOUTS`
- **Verification:** **MODIFIED** (severity low) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** Timeout-doctrine classification: both gates are bounded waits around a real condvar/fence completion — PresentGateUs 10 ms direct-primary producer gate (lib.rs:1110-1114, ~0.48 ms steady) and VehicleFlipGateUs 32 ms flip-order fallback — both KEEP contracts, and this review proposes no weakening. Structurally, though, the selection is 'let gate_us = if is_vehicle_present {..} else {..}; if !kernel_wait_armed && gate_us != 0' (8481-8486): a u32 where 0 is a disable sentinel, a bool proxying for 'a kernel wait was queued', and a timeout counter that only exists for the vehicle kind (8488-8489) — a direct-primary (active-path) gate expiry increments nothing in the UMD even though the regression gate says 'no new present-gate steady-state timeouts'. present1 drops the gate result entirely (9210).

**Evidence.** forward.rs:8481-8486 'let gate_us = if is_vehicle_present { crate::vehicle_flip_gate_us() } else { present_gate_us() }; if !kernel_wait_armed && gate_us != 0'; 8488-8489 'if !dev.dxvk.present_frame_gate(gate_us) && is_vehicle_present { EXT_FLIP_GATE_TIMEOUTS...' (non-vehicle expiry uncounted); 9210 gate result ignored; lib.rs:1113-1114 'bounded, condition-variable-backed gate closes that producer race'; 8433-8434 'ARM BEFORE QUEUE: an armed-but-unqueued signal is harmless ... a queued-but-unarmed wait would park the context forever' (call-order contract in a comment -> ArmProof candidate).

**Recommendation.** Represent the decision once: enum PresentOrdering { KernelArmed(ArmProof), BoundedGate { budget: NonZeroU32, kind: GateKind }, Disabled }, where ArmProof is only constructible from a successful pfnWaitForSynchronizationObjectFromGpuCb (the arm block), making 'skipped CPU gate without an armed kernel wait' unrepresentable; knob loaders return Option<NonZeroU32> instead of 0-sentinels. Add PRESENT_GATE_TIMEOUTS (direct-primary kind) alongside EXT_FLIP_GATE_TIMEOUTS — telemetry addition, behavior-preserving.

**Risk.** The 10 ms bounded condvar gate is a frozen-baseline safety contract: the refactor must not alter budgets, wake conditions, or the timeout-proceeds-loudly semantics; only the representation and counting change.

**Dependencies.** R25 (present-tail-dedup)

**Atomic commit boundary.** One commit: PresentOrdering enum + NonZero knob types + new counter; no budget/semantic change.

**Validation.** Steady-state: PRESENT_GATE_TIMEOUTS==0 and ~0.48 ms average from present-gate telemetry; idle-to-active wake unchanged; DComp 63 fps; kwait A/B (PresentSyncPublish=1) still arms and skips the CPU gate.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** kernel_wait_armed bool + gate_us==0 sentinel + arm-before-queue call order enforced by comments; permits queuing a GPU wait without an armed signal (context parked forever) or silently skipping both orderings.
1. **Compile-time representation:** PresentOrdering enum with ArmProof token constructible only after successful arm+queue; Option<NonZeroU32> gate budgets; exhaustive match forces every present to pick exactly one ordering.
1. **Smallest atomic migration:** dxgi_present ordering block + two knob functions (after present-tail extraction).
1. **Remaining `unsafe` preconditions:** The dxgkrnl callback results remain trusted i32 HRESULTs; the proof token attests our call order, not kernel behavior.
1. **Regression test proving preserved behavior:** 24h-equivalent present soak (existing probes): zero new gate timeouts, kwait A/B parity, 63 fps cadence.

**Verifier corrections (authoritative).** 1) current_state: replace 'a direct-primary (active-path) gate expiry increments nothing in the UMD' with 'direct-primary expiries are counted only in the bridge-side aggregate (dxvk_bridge.cpp:1802-1805 s_gateTimeouts, reported in the present-gate: telemetry line every 128 presents) which pools dxgi_present non-vehicle, present1 (9210), and wait_last_present (7743) callers; the Rust-side named-counter registry attributes only the vehicle kind (EXT_FLIP_GATE_TIMEOUTS, 7598/8489)'. The regression gate is already measurable from present-gate telemetry; PRESENT_GATE_TIMEOUTS adds per-kind attribution, not first-ever visibility. 2) static_guarantee.runtime_invariant: drop 'permits queuing a GPU wait without an armed signal (context parked forever)' — the queue call at forward.rs:8444-8454 is lexically nested inside the successful present_flip_wait_arm branch (8441), so arm-before-queue is structurally enforced today; ArmProof is a drift guard, not the closure of a live hole. 3) migration_boundary: add present1's gate block (forward.rs:9207-9212) and wait_last_present (forward.rs:7733-7748, caller-supplied timeout_us from the ICD, not a knob) to the boundary; three present_frame_gate call sites, not one. 4) Note gate_us==0 skipping both orderings is a documented operator A/B lever on both knobs ('0 disables'), correctly preserved by the proposed Disabled variant — it is not a silent-skip defect. 5) severity: low — telemetry refinement plus representational cleanup; no measurement gap of the claimed magnitude exists.

**Lead-reviewer note.** Verified MODIFIED — downgraded: gate expiries ARE already counted in the bridge-side aggregate (s_gateTimeouts), so this adds per-caller attribution, not first visibility; arm-before-queue is already structurally enforced; three present_frame_gate call sites (incl. wait_last_present with ICD-supplied timeout) are the migration boundary. Both gates are KEEP safety contracts — nothing about their bounds changes.


### R70. Shader-stage dispatch uses "VS"/"PS" string keys with a silent `_ => {}` fallthrough; six-fold per-stage wrapper sextuplets

- **Category:** static-guarantee · **Reported by:** `umd-forward-b/stage-string-dispatch-enum`
- **Files:** `umd/src/forward.rs`
- **Symbols:** `set_constant_buffers1_common`, `set_shader_resources_common`, `ps_set_constant_buffers`, `cs_set_samplers`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** set_constant_buffers1_common (5145-5199) and set_shader_resources_common (5276-5304) select the DXVK context method by matching a &str stage tag: `match stage { "VS" => ..., ..., _ => {} }` (5190-5197, 5295-5302). A misspelled tag compiles and silently drops the bind — invisible wrong rendering, violating loud-failure. Around them sit three hand-rolled sextuplets of near-identical extern wrappers: constant buffers (5084-5143), constant buffers1 (5201-5265), SRVs (5267-5344), samplers (5345-5404), plus the six *_set_shader and six *_set_shader_with_ifaces (3727-3895) which differ only in stage field + COM type.

**Evidence.** umd/src/forward.rs:5190-5197 "match stage { \"VS\" => c.VSSetConstantBuffers1(...), ... _ => {} }"; :5295-5302 same shape for SetShaderResources with terminal "_ => {}"; :5084-5143 six ~10-line clones ps/vs/gs/hs/ds/cs_set_constant_buffers; :5345-5404 six sampler clones.

**Recommendation.** Introduce `enum ShaderStage { Vs, Ps, Gs, Hs, Ds, Cs }` and match exhaustively (no `_` arm) in both commons; derive the log tag from the enum. Generate the extern wrapper sextuplets with a small macro parameterized over (stage variant, context method, COM type) so the stage/method pairing is stated once. Keep extern signatures identical (they are table entries).

**Risk.** Low: mechanical; the macro must not change extern "C" ABI signatures. Behavior identical by construction.

**Atomic commit boundary.** One commit: enum + two commons; one commit per wrapper family converted to the macro.

**Validation.** Release build; desktop + selftest_triangle_cb (exercises VS/PS constant-buffer binds through the real wrappers) PASS; dxvk-tests unchanged; grep confirms identical log strings.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** "Every stage tag passed to a common maps to exactly one context method" holds only because current call sites pass correct literals; the `_ => {}` arm means a wrong tag (typo in a future call site) silently discards the entire bind — state divergence with no error, no counter.
1. **Compile-time representation:** enum ShaderStage with exhaustive match (removing `_`), so an unmapped stage is a compile error; wrapper macro ties each extern entry to its enum variant and windows-rs method in one declaration.
1. **Smallest atomic migration:** One commit converting both commons + call sites; wrapper macro conversion can follow independently.
1. **Remaining `unsafe` preconditions:** None added; the extern "C" table entries remain unsafe by nature (runtime-provided pointers), unchanged.
1. **Regression test proving preserved behavior:** selftest_triangle_cb PASS (binds VS+PS constant buffers through these paths) plus visible desktop; log-string byte diff empty.


### R71. check_format_support: 170 lines of interleaved caps policy keyed by a sentinel u32 mode and hardcoded WARP caps words

- **Category:** static-guarantee · **Reported by:** `umd-forward-b/feature-profile-enum-caps-table`
- **Files:** `umd/src/forward.rs`, `umd/src/lib.rs`
- **Symbols:** `check_format_support`, `helios_multisample_quality_levels`, `feature_level_mode`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** check_format_support (6060-6230) mixes five concerns: DXVK query, FL10 scrub, FL11 augmentation (6098-6139), family normalization with hardcoded WARP-parity caps words (`19 | 44 => caps = 0x0012_10b0`, 6186-6189), and the XR-bias NOT_SUPPORTED sentinel (6216-6220). The profile is `crate::feature_level_mode()` — a raw u32 with documented sentinel meanings 0/1/2 (lib.rs:1063-1067) — compared at five scattered sites (5891, 6095, 6180, 6193, 6221) via `!= 1`/`== 1`, so undocumented values (3+) silently behave as FL10 and the diagnostic mode 2 exists only in a comment. helios_multisample_quality_levels (5886-5917) implements the paired half of the same runtime-validated contract in a separate function with its own mode check; the lib.rs doc (1049-1055) warns the three caps must move together, but nothing ties them.

**Evidence.** umd/src/forward.rs:6186-6189 "19 | 44 => caps = 0x0012_10b0, 20 | 40 | 45 | 55 => caps = 0x0033_10b0, 21 | 46 => caps = 0x04d2_17b0, 22 | 47 => caps = 0x0052_11b0"; :6095 "if crate::feature_level_mode() != 1 {" and :6180/:6193/:6221 repeated mode checks; :5891 same in helios_multisample_quality_levels. umd/src/lib.rs:1063-1067 sentinel doc "absent = FL10_0 ... 1 = full FL11_0 ... 2 = DIAGNOSTIC"; lib.rs:1049-1052 "This gate MUST cover the three caps together" — a comment-only contract.

**Recommendation.** Behavior-preserving: introduce `enum FeatureProfile { Fl10, Fl11, DiagPipelineOnly }` parsed once from the registry value (unknown → Fl10 plus a one-shot log, preserving today's behavior loudly); pass it explicitly through both caps fns and match exhaustively. Extract the per-format overrides (WARP words, XR sentinel, SO_BUFFER family list, R32G32B32 fixups) into one declarative const table with named caps-bit constants, so format policy is data reviewed in one place and MSAA quality answers are derived from the same table entries the format-support path uses.

**Risk.** Medium-low: caps bits are runtime-validated as a coherent contract (DXGI_ERROR_UNSUPPORTED on mismatch, 30th/31st sessions); any transcription slip breaks FL11 device creation. Mitigate with the A/B dump below before landing.

**Dependencies.** capture the A/B caps dump before hot-path-log-io-uncapped-predicates gates the FormatSupport log

**Atomic commit boundary.** Commit 1: FeatureProfile enum + threading (no caps change). Commit 2: table extraction with the empty-diff proof attached.

**Validation.** Capture "FormatSupport fmt=... final=0x..." for all formats with FeatureLevel11 absent/0/1 before and after; diff must be empty. D3D11CreateDevice succeeds at both profiles; dwm stable; Fire Strike FL11 progression unchanged.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** Profile is a sentinel u32: `!= 1` silently coerces unknown/diagnostic values into FL10 behavior at some sites while doc-only mode 2 semantics live in comments; the three-caps coherence rule (pipeline level, format bits, MSAA quality) is enforced by nothing — one site can drift and the runtime rejects device creation at a distance.
1. **Compile-time representation:** FeatureProfile enum matched exhaustively at every policy site (no catch-all), parsed once; caps policy as one const table consumed by both check_format_support and helios_multisample_quality_levels so they cannot disagree on a format's MSAA capability.
1. **Smallest atomic migration:** Enum + threading first (pure refactor, empty caps diff); table extraction second.
1. **Remaining `unsafe` preconditions:** None; the residual risk is semantic (correct transcription of hex words), covered by the empty-diff validation, not by types.
1. **Regression test proving preserved behavior:** Byte-identical FormatSupport dump across all formats and all three knob values; successful FL10 and FL11 D3D11CreateDevice.


### R72. allocate_wddm_resource takes 8 positional scalars + bool; scanout validity re-derived downstream instead of a validated descriptor

- **Category:** static-guarantee · **Reported by:** `umd-forward-a/alloc-request-validate-once`
- **Files:** `umd/src/forward.rs`
- **Symbols:** `allocate_wddm_resource`, `finish_wddm_tex2d`, `HeliosPresentPrivateData`
- **Verification:** **MODIFIED** (severity medium) — adversarially verified against the code; corrections below are authoritative over the original claim.

**Current state.** allocate_wddm_resource(h, a, mip0, h_rt, backing_blob_id, backing_blob_size, backing_resource_id, venus_alloc_size, memory_type_index, direct_scanout_primary, scanout_pitch, scanout_offset) — call sites pass literal soups like `(h, a, &mip0, h_rt, 0, 0, 0, 0, 0, false, 0, 0)` (1691, 1897). Interdependent invariants live in scattered ifs: blob_id!=0 implies DEVICE_MEMORY kind and SHAREABLE flags (1327-1344); `_pad` set only when blob and resource id nonzero (1370-1372); the primary's PresentPrivateData validity is re-derived in finish_wddm_tex2d as `direct_scanout_primary && backing_resource_id != 0 && scanout_pitch != 0` (1584); the needs-allocation predicate plus its three DDI bit consts is duplicated verbatim (1265-1267 vs 1516-1518). The handoff explicitly asks for validate-once scanout constructors (format/extent/pitch/offset/exportability).

**Evidence.** forward.rs:1691 `allocate_wddm_resource(h, a, &mip0, h_rt, 0, 0, 0, 0, 0, false, 0, 0)`; :1265-1267 vs :1516-1518 duplicated `!a.pPrimaryDesc.is_null() || (a.BindFlags & DDI_BIND_PRESENT) != 0 || (a.MiscFlags & (DDI_MISC_SHARED | DDI_MISC_SHARED_KEYEDMUTEX)) != 0` with locally redefined consts; :1584 `if direct_scanout_primary && backing_resource_id != 0 && scanout_pitch != 0`; :1327-1344 kind/flags derived from blob_id twice; REFACTOR_HANDOFF.md:88-90 validate-once constructor pattern.

**Recommendation.** Model the request as data: enum AllocBacking { KmdStandard, VenusAdopt { blob: BlobId, blob_size, resource: VirtioResourceId, alloc_size, mem_type } } plus Option<ValidatedScanoutDesc> built by a constructor that checks pitch!=0/format/extent once and is the only way to mint HeliosPresentPrivateData with the DIRECT_SCANOUT flag. allocate_wddm_resource takes the enum and returns Result<Option<WddmAllocation>, hr> (also fixing the (0,0) conflation used by finding create-ddi-error-suppression). One shared needs_wddm_allocation(a) predicate.

**Risk.** Low-medium: the function builds the exact wire RuntimeAllocPrivate; keep byte-identical output (assert layout in a unit test) to stay behavior-preserving.

**Dependencies.** R14 (split-forward-rs)

**Atomic commit boundary.** One commit in alloc.rs introducing the types and converting the three call sites (create buffer/tex2d/tex3d) + finish_wddm_tex2d.

**Validation.** Unit test asserting the serialized RuntimeAllocPrivate bytes match the old code for representative inputs (standard, venus-adopt, direct primary); then boot: VpSA=1/ScSet=1, visible desktop, ScanoutDiag absent, same-boot OPTIMAL-primary QEMU evidence.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** "direct_scanout_primary implies a nonzero venus resource id and pitch, and DEVICE_MEMORY kind implies a blob id" are re-checked (or silently assumed) at each site; the invalid state permitted is a direct-scanout primary whose PresentPrivateData is quietly downgraded to empty because one of three scalars was 0.
1. **Compile-time representation:** AllocBacking enum + ValidatedScanoutDesc newtype (NonZeroU32 pitch, NonZeroU32 resource id) whose constructor is the sole minting point for the DIRECT_SCANOUT HeliosPresentPrivateData.
1. **Smallest atomic migration:** alloc.rs: allocate_wddm_resource + finish_wddm_tex2d + three call sites, one commit.
1. **Remaining `unsafe` preconditions:** The pfnAllocateCb FFI call and the wire-struct write stay unsafe; the KMD's interpretation of the bytes cannot be proven from the UMD.
1. **Regression test proving preserved behavior:** Byte-equality unit test of RuntimeAllocPrivate serialization pre/post; boot with direct primary live (VpSA=1/ScSet=1) and allocate_wddm_resource log fields identical.

**Verifier corrections (authoritative).** (1) Arity in title: allocate_wddm_resource takes 7 trailing scalars + 1 bool (8 trailing positional args), not "8 scalars + bool"; tex3d call site is forward.rs:1896-1898. (2) Tighten NonZero scope: NonZeroU32 pitch/resource-id belong ONLY in ValidatedScanoutDesc. The VenusAdopt enum variant must keep raw u32/u64 fields because two wire-visible corner states are representable today and must survive the migration byte-identically: blob_id!=0 with resource_id==0 → kind=DEVICE_MEMORY, _pad=0 (:1327-1331 vs :1370-1372), and blob_id!=0 with blob_size==0 → kind=DEVICE_MEMORY but size=linear_size (:1306-1310). The proposed byte-equality test over "representative inputs" would miss these; they must be explicit test vectors, and NonZero modeling of them would silently change wire bytes before any test ran. (3) The Result<_, hr> return may distinguish error from not-needed, but callers must keep mapping Err → proceed with zero handles in this commit (today a failed pfnAllocateCb still yields a usable non-shared resource, :1692-1717 and :1899-1917); changing caller error behavior belongs to the separate create-ddi-error-suppression finding. (4) Precision on silence: the :1584 downgrade is fully silent only on the scanout_pitch==0 leg; the resource_id==0/suballocated leg already logs "SHARED RESOURCE WITHOUT IMPORTABLE BACKING" at :1520 — the constructor should make BOTH legs loud (named counter/log), matching loud-failure doctrine, without altering the resulting allocation bytes.

**Lead-reviewer note.** Verified MODIFIED — corrections are load-bearing: NonZero types belong ONLY in ValidatedScanoutDesc; the VenusAdopt variant keeps raw u32/u64 fields because two wire-visible corner states (blob_id!=0/resource_id==0; blob_id!=0/blob_size==0) must survive byte-identically and must be explicit test vectors; Err→zero-handles caller mapping stays in this commit (error-behavior change belongs to D12); make BOTH silent-downgrade legs loud without altering allocation bytes.


### R73. DWM scanout target is seven independent Cells set piecemeal by two writers — no validated descriptor, torn/partial states representable

- **Category:** static-guarantee · **Reported by:** `umd-core/scanout-descriptor-typestate`
- **Files:** `umd/src/device_funcs.rs`
- **Symbols:** `HeliosDevice::scanout_resource_raw`, `HeliosDevice::scanout_resource_id`, `HeliosDevice::scanout_allocation`, `HeliosDevice::scanout_width`, `HeliosDevice::scanout_height`, `HeliosDevice::scanout_format`, `HeliosDevice::scanout_import`, `HeliosDevice::scanout_generation`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** device_funcs.rs:77-96 spreads the scanout identity over seven Cells plus a RefCell<Option<ID3D11Resource>> import. Two distinct writers populate them field-by-field: the largest-primary heuristic path (forward.rs:639-644, guarded by an area comparison at 633-638) and the KMD-import path (forward.rs:716-724, which hardcodes `dev.scanout_format.set(87)` and sets allocation to 0 as an implicit variant discriminator); clearing is also field-by-field (forward.rs:658-664). Which 'variant' the device is in (direct primary vs KMD import) is recoverable only from boolean combinations (allocation==0, import.is_some()). Any early return or future edit between the sets leaves a mixed descriptor that downstream present/copy code will consume; nothing validates width/height/format/pitch coherently at one point.

**Evidence.** device_funcs.rs:77-85 '/// Non-owning pointer to the largest scanout primary resource... pub scanout_resource_raw: Cell<usize>, scanout_resource_id, scanout_allocation, scanout_width, scanout_height, scanout_format'. forward.rs:639-644 six sequential .set() calls; forward.rs:716-724 second writer including '722: dev.scanout_format.set(87);' (bare DXGI_FORMAT_B8G8R8A8_UNORM) and '718: dev.scanout_allocation.set(0);' as variant marker; forward.rs:658-664 field-by-field clear; forward.rs:757-760 variant logic by field comparison 'width != dev.scanout_width.get() || ... || resource_raw == dev.scanout_resource_raw.get()'.

**Recommendation.** Replace the field cluster with `RefCell<Option<ScanoutTarget>>` where `ScanoutTarget` is produced by one validate-once constructor from HeliosPresentPrivateData (nonzero resource, valid extent/format), and is an enum: `DirectPrimary { raw, alloc, id, extent, format }` vs `KmdImport { resource: ID3D11Resource, id, extent, format, generation }`. Swap/clear become single `replace` operations (atomic under the single-threaded DDI contract); the magic 87 becomes a named DXGI format constant inside the constructor. This is the handoff's 'validated scanout descriptor' pattern applied to the exact-primary path.

**Risk.** Medium: this touches the direct-primary path protected by the frozen baseline. Behavior (largest-area preference, generation tracking, allocation-keyed lookup via the separate direct_scanout_allocations table) must be preserved exactly; the refactor only changes representation.

**Dependencies.** R64 (cxx-com-ownership-newtypes)

**Atomic commit boundary.** One commit swapping the struct fields + the three forward.rs writer/clearer sites and readers (approx. forward.rs:633-830); no protocol or KMD change.

**Validation.** Release UMD build; reboot-free UMD deploy + adapter restart; visible desktop rendering the exact OPTIMAL primary (same-boot QEMU evidence, not a diagnostic fill); ScanoutDiag absent; VpSA=1/ScSet=1; ScRid follows flips; resize + dwm restart cycles keep the descriptor coherent; DComp cadence ~63fps.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** A scanout target is 'valid' only when all seven fields were written by the same event; the direct-vs-import variant is encoded as allocation==0 plus import.is_some(). Permitted invalid states: torn descriptor after a partial write/clear, nonzero extent with resource_raw==0, import Some while raw identifies a different resource, format 0 consumed as real.
1. **Compile-time representation:** RefCell<Option<ScanoutTarget>> enum with a validate-once constructor; consumers must match a variant, so a partially-initialized or mixed-variant descriptor is unrepresentable.
1. **Smallest atomic migration:** Single commit over device_funcs.rs struct + forward.rs scanout writer/reader block; present private-data wire format untouched.
1. **Remaining `unsafe` preconditions:** resource_raw's COM liveness (creator may release the underlying resource) is a cross-object lifetime the type cannot carry — it stays a documented non-owning reference invalidated via the existing unset path; single-threaded DDI access remains a runtime contract.
1. **Regression test proving preserved behavior:** Same-boot desktop visibility on the exact DWM primary + VpSA/ScSet counters + resize/dwm-restart cycling; compare 'DDI scanout target:' log lines before/after for identical values.

**Lead-reviewer note.** UMD-side counterpart of R42: the seven independent Cells in device_funcs.rs become one validated descriptor published atomically. Coordinate the two so the UMD descriptor is the thing the KMD descriptor validates against.



---

## Part II, Tranche 7 — Concurrency and wait-structure

Wait-structure changes last: they touch the parts of the driver where the frozen baseline lives (fence retirement, refresh markers, HPD/StartDevice ordering) and benefit from every earlier tranche's structure and types. Timeout doctrine applies to every entry: bounded waits on real events are KEEP contracts; only arbitrary delays and polls with existing wake edges change shape.

**Regression-gate emphasis:** the full gate, plus idle-to-active wake latency and steady-state cadence measured separately (per the stage charter), plus no new control timeouts or ring failures over an extended soak.

### R74. Timeout classification: the bounded KEVENT waits are safety contracts (KEEP); the three 1 ms sleep-poll loops (queue-full, map-busy, waiter-table-full) poll conditions whose clearing edges already occur under the device spinlock and are replaceable by events

- **Category:** concurrency · **Reported by:** `kmd-transport-ctrl/backpressure-sleep-polls`
- **Merged duplicate reports (1):** `xc-concurrency/ctrl-sleep-poll-backpressure` — Five bounded 1 ms sleep-poll retry loops in virtio::ctrl poll conditions (queue slot free, map-busy, waiter-table slot) that the interrupt DPC already observes — replaceable by an event wake
- **Files:** `kmd_render/src/virtio/ctrl.rs`, `kmd_render/src/virtio/gpu.rs`
- **Symbols:** `ctrl_roundtrip`, `submit_venus_async`, `map_blob_prepare`, `map_blob_at`, `wait_fence`, `ENQUEUE_RETRY_MAX`, `MAP_BUSY_RETRY_MAX`, `wait_block`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** Per the timeout doctrine: KEEP — wait_block's adaptive-slice KEVENT wait with per-slice used-ring re-drain (ctrl.rs:157-191) is a bounded wait around a real event with documented interrupt-loss tolerance; SYNC_ROUNDTRIP_TIMEOUT_MS=30s (ctrl.rs:59) and WAIT_FENCE_MAX_MS=120s (ctrl.rs:65) are loud-failure bounds, not ordering hacks. FLAG — three loops are pure 1 ms sleep-polls: QueueFull backpressure (ctrl.rs:266-274 and 1427-1436, ENQUEUE_RETRY_MAX=5000 -> ~5 s budget), BlobMapBegin/RemapBegin::Busy (ctrl.rs:1171-1177 and 1242-1246, MAP_BUSY_RETRY_MAX=30000 -> ~30 s), and FenceWaitPrep::TableFull (ctrl.rs:1511-1518, 1000 retries). Each polls a state transition produced under the device spinlock — drain_used frees inflight/parked capacity, blob_map_finish clears the busy latch, drain removes fence_waiters — where KeSetEvent is already used for other waiters. The 5 s / 30 s budgets are magic and unrelated to SYNC_ROUNDTRIP_TIMEOUT_MS.

**Evidence.** ctrl.rs:61-62 'Backpressure retry budget when the control queue / in-flight tables are full (1 ms PASSIVE sleep per retry). const ENQUEUE_RETRY_MAX: u32 = 5_000;'; ctrl.rs:66-67 'const MAP_BUSY_RETRY_MAX: u32 = 30_000;'; ctrl.rs:271-274 'reap_parked(adapter); sleep_ms(1);'; ctrl.rs:1171-1177 'BlobMapBegin::Busy => { busy_retries += 1; ... sleep_ms(1); }'; ctrl.rs:1511-1518 'FenceWaitPrep::TableFull => { ... sleep_ms(1); }'. KEEP evidence: ctrl.rs:6-9 module doc 'waits use adaptive slices and re-drain the used ring... interrupt-driven when interrupts flow and degrade to ~ms-latency polling when they do not'; ctrl.rs:55-59 30 s budget rationale.

**Recommendation.** Add adapter-owned NotificationEvents ('ctrl space available', 'blob map settled') set at the exact clearing edges (drain_used when occupancy drops below the gate; blob_map_finish; fence_waiters removal), and convert the three loops to bounded KEVENT waits keeping their current total budgets as explicit safety timeouts (documented as such). This preserves behavior (same bounds, same failure classification) while eliminating steady 1 ms wake cadence under sustained backpressure. Derive the budgets from one doctrine constant instead of three magic numbers.

**Risk.** Medium: touches drain_used (DISPATCH, under spinlock) — the added KeSetEvent must be unconditional-cheap and must not signal while occupancy is still above the gate (spurious wakes are safe, missed wakes are not — set on every drain that frees capacity). These loops rarely spin in steady state, so the perf win is bounded; the value is mostly removing polling from a module whose header promises interrupt-driven waits.

**Atomic commit boundary.** One commit per loop family (queue-space event first; map-settled second), each keeping the old budget as the wait bound.

**Validation.** Before/after counters under a present storm: QUEUE_FULL_RETRIES trend, CTRL_TIMEOUT_COUNT stays 0, no new gate timeouts, 63 fps cadence unchanged; artificial queue-pressure test (small CTRL_QUEUE_SIZE debug build) proving forward progress and the bounded-timeout failure still fires.

**Lead-reviewer note.** Both reports agree the bounded KEVENT waits are KEEP safety contracts; only the three (xc report counts five — reconcile at implementation) 1 ms sleep-poll loops whose clearing edges already occur under the device spinlock become event waits. Measure wake latency before/after per Operating Rule 7.


### R75. Opportunistic PASSIVE drains (wait_block slices) retire Venus watermarks without running the WddmNotifyGuard consumption stage; under a lost interrupt a ready WDDM fence or scanout marker can sit unconsumed

- **Category:** concurrency · **Reported by:** `kmd-transport-ctrl/opportunistic-drain-vidsch-gap`
- **Files:** `kmd_render/src/virtio/ctrl.rs`, `kmd_render/src/ddi/interrupt.rs`
- **Symbols:** `wait_block`, `drain_used_and_complete`, `VirtioGpu::take_ready_wddm`, `VirtioGpu::take_ready_scanout_refresh`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** drain_used_and_complete (interrupt.rs:37-77) is the pairing that keeps VidSch honest: drain, then under with_wddm_notify_lock consume take_ready_scanout_refresh/take_ready_wddm and signal DMA_COMPLETED. But wait_block's interrupt-loss fallback calls bare 'adapter.with_virtio(|v| v.drain_used())' (ctrl.rs:189), which retires Venus fences and satisfies wddm_pending watermarks without the consumption stage. With working interrupts this is safe — the level-triggered ISR-status bit still forces a DPC that consumes. But in the exact lost-interrupt scenario the fallback exists for, a WDDM pending fence whose watermark was satisfied by the opportunistic drain waits for the next interrupt, or for the HPD worker's drain_used_and_complete, which is gated on scanout traffic being in flight (hpd.rs:125-130) — a quiet desktop provides neither. interrupt.rs:34-36 states the intent ('prevents an opportunistic drain from consuming a Venus completion without notifying VidSch') yet only the two drain_used_and_complete callers honor it; the enforcement is call-convention, not structure.

**Evidence.** ctrl.rs:188-189 '// Interrupt-loss tolerance: drain whatever completed. let _ = adapter.with_virtio(|v| v.drain_used());' — no take_ready_* follow-up; interrupt.rs:33-36 'the scanout worker may also call it at PASSIVE_LEVEL as a bounded fallback... Keeping fence retirement here prevents an opportunistic drain from consuming a Venus completion without notifying VidSch.'; hpd.rs:125-130 the only PASSIVE drain_used_and_complete call is gated: 'if (wait_status == STATUS_TIMEOUT && ctrl_inflight) || ((...flush_inflight... || ...bind_inflight...) && ...refresh_pending...)'; gpu.rs:1780-1782 take_ready_wddm doc: 'The DPC signals DMA_COMPLETED for each, in order, OUTSIDE the device spinlock.'

**Recommendation.** Behavior-preserving-in-contract hardening: make wait_block's slice fallback call ddi::interrupt::drain_used_and_complete(adapter) instead of bare drain_used — wait_block runs at PASSIVE holding no locks, so the wddm_notify -> virtio lock order is honored (the HPD worker already calls it from PASSIVE). In-lock opportunistic drains (enqueue paths, already inside with_virtio) must stay bare — document that asymmetry at the drain_used definition, or restrict bare drain_used visibility so future PASSIVE callers cannot pick the wrong one.

**Risk.** Low-medium: adds notify-lock acquisitions only on timed-out wait slices (not steady state — normal completion wakes via KEVENT before any slice expires). Verify no caller of wait_block already holds the notify lock (none do today: ctrl_roundtrip and wait_fence are escape/DDI PASSIVE paths).

**Atomic commit boundary.** One-line call change in wait_block plus a visibility/doc commit on drain_used; independently revertible.

**Validation.** Regression gate; targeted: mask the virtio INTx briefly in a debug scenario (or rely on the existing bring-up interrupt-loss path) and confirm WDDM fences still complete within one wait slice; WDDM_FENCE_FROM_DPC counter still advances; no new present-gate timeouts at steady state.

**Lead-reviewer note.** Investigate as a possible defect first: under a lost interrupt, a ready WDDM fence or scanout marker can sit unconsumed because opportunistic PASSIVE drains skip the WddmNotifyGuard consumption stage. If the gap is real, the fix is a defect fix (owner decision), not a refactor; if not, document why and encode the invariant.


### R76. HPD worker's first child indication is ordered against StartDevice by a 500 ms timeout, and an early event wake can defeat even that

- **Category:** concurrency · **Reported by:** `kmd-display/hpd-startdevice-ordering-by-delay`
- **Merged duplicate reports (2):** `xc-concurrency/hpd-startdevice-time-ordering` — HPD worker orders its first DxgkCbIndicateChildStatus against StartDevice by a 500 ms delay; an early hpd_event wake breaks the forbidden-during-StartDevice contract; `xc-legacy/hpd-startdevice-ordering` — HPD worker orders its first DxgkCbIndicateChildStatus after StartDevice with a 500 ms timed wait — an arbitrary delay standing in for an ordering event, defeatable by an early wake
- **Files:** `kmd_render/src/ddi/hpd.rs`, `kmd_render/src/ddi/start_device.rs`
- **Symbols:** `hpd_thread_routine`, `indicate_child_status`, `init_hpd`
- **Verification:** UNVERIFIED (verifier lost to session limit), but reported independently by 3 reviewers — high convergence. Re-verify cited lines before implementing.

**Current state.** DxgkCbIndicateChildStatus is forbidden during DxgkDdiStartDevice (hpd.rs:5-8). The worker is created inside StartDevice (start_device.rs:270) and 'ensures' StartDevice has returned by waiting 500 ms (hpd.rs:64-65 'wait briefly so StartDevice has certainly returned', QuadPart = -5_000_000). Timeout-doctrine classification: this initial 500 ms is an arbitrary delay used to make ordering appear correct — the flagged hack class. Worse, the wait is on hpd_event, and the comment admits 'A boot config-change wakes us early' (hpd.rs:66): a virtio config-change ISR→DPC signaling hpd_event in the window between init_hpd and StartDevice's return (start_device.rs:270→274) wakes the worker immediately and indicate_child_status runs during StartDevice. By contrast, the steady-state loop's timeouts are correct KEEP-class contracts: the 4 ms wait (hpd.rs:101) is a bounded lost-interrupt fallback around the real KEVENT completion path, and the 16 ms wait (104) is a bounded retry after a loud enqueue failure — neither should be removed.

**Evidence.** hpd.rs:61-65 '// First indication: wait briefly so StartDevice has certainly returned (DxgkCbIndicateChildStatus is forbidden during it) ... initial_timeout.QuadPart = -5_000_000; // 500 ms'; hpd.rs:66 '// A boot config-change wakes us early'; start_device.rs:265-270 worker started inside StartDevice, which returns at 274. KEEP-class: hpd.rs:101 'timeout.QuadPart = -40_000; // 4 ms: bounded lost-interrupt fallback.' and 104 '-160_000; // 16 ms retry after a loud enqueue failure.'

**Recommendation.** Replace ordering-by-delay with a real happens-after witness: a start_complete event/flag signaled from a context guaranteed to follow StartDevice's return — the first post-start DDI (QueryChildRelations entry is called by dxgkrnl only after StartDevice succeeds). The worker's first indication waits on that gate; retain a bounded timeout on the gate purely as a lost-signal backstop (KEEP class, loudly counted if it fires). Encode it as a proof token: indicate_child_status takes &PostStartProof, constructible only from the gate wait. Also consider renaming/splitting: the module is documented as the HPD worker but half the loop body is the scanout-refresh flush engine (hpd.rs:88-158); at minimum retitle, ideally host the refresh mux beside the publish state (see seal-scanout finding).

**Risk.** Low-medium: touching bring-up-critical ordering (Code 43 territory). The gate must be provably signaled on every start path including render-only (or the worker not started there — it already isn't); keep the backstop timeout so a missed signal degrades to today's behavior, not a hang.

**Atomic commit boundary.** One commit: add the post-start gate + token, keep the 4 ms/16 ms steady-state waits untouched.

**Validation.** Cold boot + pnputil restart-device: HpdI records success, HpdN>=1, monitor devnode appears, VpECp>=1, VpCN=1, desktop visible; no Code 43; backstop-timeout counter reads 0.

**Static guarantee (5-point):**
1. **Runtime-only invariant / invalid states permitted:** 'No child-status indication until StartDevice has returned' rests on a 500 ms delay; invalid sequence: config-change signal in the init_hpd→return window (or a >500 ms preempted StartDevice) → DxgkCbIndicateChildStatus during StartDevice.
1. **Compile-time representation:** PostStartProof token minted only by waiting on a start_complete event signaled from a guaranteed post-StartDevice context (first QueryChildRelations); indicate_child_status requires the token.
1. **Smallest atomic migration:** hpd.rs + one signal site in the child-relations DDI + the event in adapter.rs, single commit.
1. **Remaining `unsafe` preconditions:** The 'QueryChildRelations happens only after successful StartDevice' fact is a dxgkrnl sequencing guarantee the compiler cannot see; the token trusts that one documented ordering instead of a timer.
1. **Regression test proving preserved behavior:** Three consecutive cold boots + one adapter restart all reach a connected monitor and visible desktop with backstop counter 0 (vs. baseline HpdI/HpdN values).

**Lead-reviewer note.** Three reports; textbook ordering-hack per the timeout doctrine (a 500 ms delay standing in for an ordering event, defeatable by an early wake). Replace with an explicit StartDevice-completed event the HPD worker waits on; the early-wake path is the defect-shaped edge.


### R77. error.rs: dead status_of, vestigial fallible AdapterContext::new, and a 5-variant catch-all with no per-DDI NTSTATUS-legality mapping

- **Category:** error-path · **Reported by:** `kmd-core/driver-error-ntstatus-legality`
- **Files:** `kmd_render/src/error.rs`, `kmd_render/src/adapter.rs`, `kmd_render/src/ddi/add_device.rs`
- **Symbols:** `DriverError`, `status_of`, `AdapterContext::new`
- **Verification:** UNVERIFIED (verifier lost to session limit). Re-verify cited lines before implementing.

**Current state.** `status_of` (error.rs:33-38) has zero callers. `AdapterContext::new` (adapter.rs:325-388) is infallible — its body is a single `Ok(Self{...})` — yet returns `Result<Self, DriverError>`, forcing a dead error arm in add_device.rs:26-28. More substantively, DriverError's five variants map one-to-one to NTSTATUS (error.rs:15-23) with no notion of which DDI is returning: CLAUDE.md's code-style rule requires "documented NTSTATUS from the DDI's legal return set (an illegal NTSTATUS is itself logged by dxgkrnl as a driver bug)", but e.g. `DeviceNotFound → STATUS_DEVICE_DOES_NOT_EXIST` from `with_virtio`/`with_venus_client` (adapter.rs:921-926, 1017-1023) flows toward DDI boundaries where that status is not in the documented set; legality is re-judged (or not) ad hoc at each `.into()`.

**Evidence.** error.rs:33-38 `status_of` — grep shows no callers outside error.rs; adapter.rs:325-388 "pub fn new(pdo: PDEVICE_OBJECT) -> Result<Self, DriverError> { Ok(Self { ... }) }" — no Err path; add_device.rs:26-28 "let ctx = match AdapterContext::new(...) { Ok(c) => c, Err(e) => return e.into_ntstatus(), };" — dead arm; error.rs:19 "Self::DeviceNotFound => STATUS_DEVICE_DOES_NOT_EXIST" — blanket mapping with no DDI context.

**Recommendation.** Delete status_of; make AdapterContext::new return Self and drop the dead arm. Keep DriverError strictly internal and add small per-DDI boundary mappers (e.g. `fn escape_status(e: DriverError) -> NTSTATUS`, `fn start_status(...)`) that exhaustively match variants onto that DDI's legal set — the exhaustive match is the compile-time reminder when a variant is added. Do not build a generic `LegalStatus<D>` machinery; the trusted boundary is a handful of 5-line mappers.

**Risk.** Changing any actually-returned status is a behavior change — the first commit must be mapping-preserving (mappers reproduce today's statuses verbatim); tightening to documented sets is a separate, owner-reviewed follow-up per DDI.

**Atomic commit boundary.** Commit 1: delete status_of + infallible new (pure dead-code removal). Commit 2: introduce mapping-preserving per-DDI mappers.

**Validation.** Builds; boot; escape/allocation paths return the same statuses (UMD logs unchanged); no new dxgkrnl-logged driver-bug events in the ETW AzureTriage trace.

**Lead-reviewer note.** Per-DDI legal-NTSTATUS mapping complements R53 (VidPn variant) and closes the 'illegal NTSTATUS is itself a driver bug' contract loudly.



---

## Part III — Considered and rejected / verifier downgrades

No completed verdict was an outright REFUTED, but the adversarial pass materially
downgraded several claims. Per the handoff's requirement to call out cosmetic or
overstated guarantees, the notable downgrades (already folded into their entries):

- **Present-gate divergence was illusory** (R25): the claimed gate-result divergence
  between `dxgi_present` and `dxgi_present1` does not exist — both non-vehicle paths drop
  the gate result identically. The dedup's value is drift prevention, not a live bug.
- **Direct-primary gate expiries are already counted** (R69): bridge-side `s_gateTimeouts`
  aggregates all callers; the finding's "uncounted expiries" claim was wrong. What remains
  is per-caller attribution — worth low severity, not medium.
- **Arm-before-queue is already structurally enforced** (R69): the queue call is lexically
  nested inside the successful-arm branch; the proposed `ArmProof` guards future drift
  only, and must not be sold as closing a live hole.
- **`PresentToHwQueue` telemetry is cold by construction** (R9): no HW-scheduling caps are
  advertised, so dxgkrnl cannot route frame-rate presents there; hygiene, not a live perf
  defect.
- **UMD env-scan cost is noise** (R12): sub-microsecond against the 0.48 ms gate; the
  entry's value is sealing readback diagnostics out of the present module, and validation
  must expect *no* fps delta.
- **`gdi_blit` RAII deferred** (R61): the executor is slated for retirement per ROADMAP
  (GdiAccelMode=0 A/B passed); a risk-bearing refactor in a module scheduled for deletion
  is not worth it unless retirement is abandoned.
- **hwqueue tag as proposed would be UB** (R60): the literal recommendation (repr(u32)
  enum + NonNull read from an untrusted handle) is strictly worse than today's code; the
  corrected raw-fields-validate-then-view design is what may land.
- **A "sealed" selftest module is a review rule, not a static guarantee** (R14/c-range
  corrections): the selftest fns must stay reachable for the export, so the seal is
  convention — honesty required in the commit message (moot if R1 deletes them first).

## Part IV — Coverage gaps and process caveats

- **Verification debt:** 148/169 findings are UNVERIFIED (session limit). Mandatory
  process rule for Phase 2: before implementing any UNVERIFIED entry, re-read its cited
  lines and re-run its liveness argument; treat the entry as hypothesis, not fact. The
  21 completed verdicts averaged 3–5 material corrections each — assume similar latent
  correction density in the unverified set.
- **Dedicated telemetry sweep did not run** (xc-telemetry finder lost to the session
  limit). Telemetry findings exist from 5 other reviewers (R8–R13, D1), but a focused
  pass over *counter cost* (atomics contention, S-ring cap behavior at 3000) has not
  happened. Low residual risk; fold into tranche 2 implementation review.
- **Files with no finding coverage:** `kmd_render/src/ddi/mod.rs`, `kmd_render/src/dxgk.rs`,
  `kmd_render/src/virtio/mod.rs` (glue, 160 lines total — acceptable);
  `kmd_render/src/ddi/blob_map.rs` and `kmd_render/src/ddi/gpummu.rs` appear only in minor
  notes — give both a deliberate look during tranche 4 (kmd-alloc scope).
- **No automated merge/critic pass ran** — clustering and ordering in this document are
  the lead reviewer's; if two entries seem to demand incompatible shapes (most likely
  around R42/R43/R44 scanout types), the conflict is resolved at design time in favor of
  R42's descriptor with R43's corrections, which were adversarially verified.
- **dxvk-helios, icd/mesa, protocol/ were context, not targets.** Several entries touch
  `protocol/` (R32, R38, R39, R40) and one names `umd/bridge/dxvk_bridge.cpp` (R2, R67);
  treat those as boundary work items — the C++ engine itself was not reviewed.

## Appendix A — Reading an entry

Each entry: category and reporting scope(s); files/symbols; verification status;
current state with file:line evidence; recommendation; risk; dependencies; smallest
atomic commit; validation; the 5-point static-guarantee block where the finding concerns
a runtime-only invariant; verbatim verifier corrections where a verdict completed; and
lead-reviewer notes. Line numbers were captured at baseline `22.22.142.0` — re-anchor
before editing.


---

## Appendix B — Minor notes (one-liners, unverified)

Trivia not worth individual tracking; fold opportunistically into nearby tranche work. Grouped by reviewer scope.


**kmd-alloc** (12):

- kmd_render/src/ddi/gpummu.rs:145 stale doc: '1 = root for the two-level scheme' — the geometry has been 4-level since the rework (lines 51, 171-175); update the comment.
- kmd_render/src/ddi/gpummu.rs:186-192 root_page_table_size_bytes ignores num_pte magnitude (root covers only 1<<ROOT_PAGE_TABLE_INDEX_BIT_COUNT entries = 2); a larger request silently gets one page — add a loud counter/debug assert per the loud-failure rule.
- kmd_render/src/ddi/build_paging_buffer.rs:376-384 UPDATE_PAGE_TABLE contiguity check samples only the first and last PTE; a permuted middle passes undetected (diagnostic-only harvest, low impact).
- kmd_render/src/ddi/blob_map.rs:87-125 map_io_pages_to_user relies on whole-body unsafe-fn (no per-operation unsafe blocks), diverging from the crate's SAFETY-comment-per-block style; enabling unsafe_op_in_unsafe_fn would catch it.
- kmd_render/src/ddi/blob_map.rs:97 `size as ULONG` u64→u32 truncation is guarded only at a distance (escape.rs:547 size<=u32::MAX check + gpu.rs:393 MAX_BLOB_MAP_BYTES=256MiB) — a cross-module implicit contract; take u32 or assert locally.
- kmd_render/src/virtio/ctrl.rs:1171-1177 and 1241-1246 blob-map Busy arm sleeps 1 ms up to MAP_BUSY_RETRY_MAX polling a peer's in-flight map — per the timeout doctrine this is a bounded poll loop, not an event wait (mild hack, rare contention); replaceable by an event signaled at blob_map_finish; low priority.
- kmd_render/src/ddi/escape.rs:535→565 contains/insert TOCTOU lets racing same-device MAP_BLOBs bypass the documented duplicate-map guard (benign: both entries drain at cleanup); closed by mapping-table-raii's insert_unique.
- kmd_render/src/ddi/create_allocation.rs:546 'both candidate layouts are exactly 48 bytes' is comment-only — add const _: () = assert!(size_of::<HeliosWddmOpenIdentity>() == size_of::<HeliosWddmAllocPrivate>()) in the protocol crate.
- kmd_render/src/ddi/create_allocation.rs:462-465 KMD-internal misc_flags bits 31/30 (HELIOS_ALLOC_MISC_PRIMARY/DIRECT_SCANOUT) are overlaid on the UMD's D3D11 misc-flag word by magic constants duplicated across KMD and UMD — move the named bits into helios_protocol so both sides share one definition.
- kmd_render/src/ddi/cpu_host_aperture.rs:112 `(args.NumberOfPages & 0xFFFF_FFFF)` masks a 32-bit value with a 32-bit mask — no-op; drop or widen intentionally.
- kmd_render/src/ddi/create_allocation.rs:1334 `let _ = (open.allocation, open.private_size);` — OpenAllocationContext.private_size is dead (only this dummy read); remove the field.
- kmd_render/src/ddi/create_allocation.rs:483-487 record_alloc_event doc still explains correlating against 'the IDD's IddCx swapchain surface' — historical-path language in an active-flow comment; reword to the current direct-primary architecture.

**kmd-core** (12):

- lib.rs:185-187: stale contract comment — 'Display/VidPn paths... all return unsupported while StartDevice reports zero sources and children' is false in the production DisplayHalf=1 shape; comments describing the DDI table's behavior should reference the knob, not the recovery default.
- lib.rs:32-33: '// TEMPORARY: post-start bring-up tracer to locate the AddAdapter failure' on `mod diag` is stale (AddAdapter cleared 2026-07-05; diag is now production telemetry).
- lib.rs:38-40: blanket `#[allow(dead_code)] mod virtio` ('allow until M4 consumes them all') now hides genuinely dead transport code post-M4; remove the allow and delete what falls out.
- adapter.rs:616: doc typo 'Queue one non-blocking RESOURCE_FLUSH... The the exact-primary copy's ring-1 completion DPC'.
- adapter.rs:489: display_mode's usable-mode floor '>= 320 && >= 240' are unnamed magic values; name them (MIN_HOST_MODE_W/H).
- adapter.rs:395-399: init_hpd is 'Idempotent-ish' via a check-then-act on hpd_thread (load then store) — safe only because StartDevice is serialized; a compare_exchange or the lifecycle typestate from adapter-lifecycle-aliasing would make it actually idempotent.
- device.rs:143-147: CreateContext DMA constants (DmaBufferSegmentSet=1, 256 KiB, PrivateDataSize=40) are load-bearing magic numbers justified only by an adjacent comment; hoist to named consts shared with the paging/submit paths that assume them.
- device.rs:19-35: DeviceContext/ContextContext back-pointers are raw `*mut` with lifetime-by-comment and are null-checked ad hoc at each use (submit_command.rs:482,505,686); a NonNull-based `DdiHandle<T>` at one cast boundary would centralize the trust.
- query_adapter_info.rs:803,842: the QUERYSEGMENT/QUERYSEGMENT3 fallbacks report a 1-segment aperture-only topology inconsistent with the QUERYSEGMENT4 production table — if dxgkrnl ever exercised them the adapter would silently run a different memory topology instead of failing loudly; fold into the SegmentTable work or return NOT_SUPPORTED deliberately with a counter.
- Timeout doctrine classifications for in-scope/adjacent waits: hpd.rs:100-108 4 ms lost-interrupt fallback and 16 ms enqueue-retry are bounded timeouts around a real KEVENT wait = safety contract, KEEP; adapter.rs:846-849 16 ms vsync KTIMER is a display cadence generator (CRTC vsync emulation for flip-queue advancement, frozen baseline), not an ordering delay, KEEP.
- diag.rs:56-57: the S-ring STEP counter is a process-global static that never resets across StopDevice/StartDevice cycles and silently stops at MAX_STEPS=3000; if ring exhaustion ever matters, a named counter should record the drop (loud-failure rule).
- query_adapter_info.rs:246 + 451: `direct_flip_advertised()` performs a synchronous registry read on every DRIVERCAPS/segment query rather than caching like DiagLevel; harmless at current query rates but inconsistent with the read-once knob pattern used in StartDevice.

**kmd-display** (10):

- display.rs:21-31 rec_named is a verbatim duplicate of diag.rs:104-114 record_named_bytes (and vidpn.rs:132-134 rec is a third thin wrapper) — fold into one helper.
- display.rs:270 and 292 read args.Flags.__bindgen_anon_1.Value twice via separate unsafe union reads, and 292 tests the magic bit '(1 << 2)' instead of the generated Flip bitfield accessor — reuse the present_flags local and name the bit.
- display.rs:734-740: comment claims 'Preserve A-vs-X on the virtio scanout contract' but vformat is unconditionally VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM (same at 202) — misleading comment; make the collapse explicit.
- display.rs:741-746: the fallback path records ScFmt for unexpected dxgi formats but proceeds to bind anyway, while the direct path rejects (710) — asymmetric record-and-continue worth a named policy in the validated-descriptor constructor.
- scanout_diag.rs:112-139: maybe_run issues ~28 registry writes zeroing Sdg* before the mode==0 early-out at 140 — every production StartDevice stamps diag keys; move the zeroing below the mode gate.
- vidpn.rs:620-621: '// SAFETY: copy the 360-byte path...' annotates a safe struct copy (let mut local = *p;) — mislabeled SAFETY comment.
- hpd.rs:20: local 'const STATUS_TIMEOUT: NTSTATUS = 0x0000_0102' shadows the WDK constant — use the binding to avoid drift.
- display.rs:396-417: patch-location DriverId/Value magic numbers 1 and 2 are unnamed; give them named constants tied to the HeliosPresentRefreshCmd contract.
- display.rs:379-393: the present private-data path synthesizes venus_alloc_size as plane_offset + pitch*height, making issue_present_scanout's alloc-size check self-satisfying for via=2 — carry the UMD's real allocation size in HeliosPresentPrivateData or drop the vacuous check when the descriptor constructor lands.
- vidpn.rs:209-211: video_signal_info approximates HSyncFreq as h*REFRESH_HZ and PixelRate as w*h*60 ignoring blanking, while build_edid (66-73) models blanking — harmless for a virtual signal but the divergence deserves a comment.

**kmd-submit** (12):

- gdi_blit.rs:235-239 — unknown/DXGK_GDIOP_ESCAPE opcodes take `_ => true` and are counted into OPS_EXECUTED (GdiE); count them as an 'ignored' bucket so GdiE means executed raster work.
- gdi_blit.rs:753-770 — the CLEARTYPEBLEND alpha-row byte scan (up to width*4 reads per text op, CT_ALPHA_ZERO) is a resolved 15th-session black-desktop diagnostic still running on every text op; gate behind DiagLevel or drop.
- submit_command.rs:543-549 — patch-list validation (BUFFER_TOO_SMALL/INVALID_PARAMETER) runs after the present-marker side effects (:468-541) already armed a scanout refresh/publish; a grow-and-retry Render arms twice (benign because coalesced/idempotent) — validate before side effects.
- escape.rs:331 — QUERY_STATS v1/v2 negotiation by `buf.len() >= sz2` instead of an explicit version/verb; size-as-version is a heuristic worth an explicit marker when the verb enum lands.
- escape.rs:43-92 — the KMD never reads DXGKARG_ESCAPE.Flags.HardwareAccess; the 26th-session 'all escapes HardwareAccess=0' wedge invariant is enforced only in the mesa ICD — an always-on KMD counter of HardwareAccess=1 arrivals would witness regressions of the FlushAllDevice-vs-parked-queue class.
- protocol/src/escape.rs:38 + icd/win-build/helios_vk_present.c:527 — HELIOS_ESCAPE_PRESENT_BLOB (0x0007) is a Phase-7 throwaway verb the KMD rejects via the wildcard; only the Gate-7 oracle tool sends it — mark historical in the protocol doc when the verb enum lands (keep the numeric slot reserved).
- scheduler.rs:252 — `(present_flags & (1 << 2)) == 0` selects the allocation-list arm by an unnamed DXGK_PRESENTFLAGS bit; name the bit (Flip) via the bindgen bitfield or a documented const.
- scheduler.rs:283-305 — present patch entries hardcode Value=1/2, DriverId=1/2 with no named meaning; document or const-name alongside the 'HEPQ' no-op record.
- device.rs:18-27 + submit_command.rs:482,505,686-689 — DeviceContext.adapter/ContextContext.device are nullable raw back-pointers null-laddered at every use despite 'valid for the lifetime' comments; NonNull set at create time would delete the ladders (touches device.rs, out of this scope's files).
- submit_command.rs:220-233 — signal_crtc_vsync takes bare i64 physical_address / u32 target_id; PhysAddr/TargetId newtypes would prevent transposition when a second target ever appears.
- ctrl.rs:1173-1178 (context for gdi_blit callers) — map_blob_prepare's Busy → sleep_ms(1) retry is bounded by MAP_BUSY_RETRY_MAX around a real two-phase map race: per the timeout doctrine this is a safety contract (KEEP), not a polling hack.
- escape.rs:546-548 — MAP_BLOB validates `prep.size == 0 || prep.size > u32::MAX` only after the RESOURCE_MAP_BLOB round-trip; harmless because prepare is idempotent and RELEASE_BLOB reclaims, but the check could run on the begin-phase size before the host round-trip.

**kmd-transport-ctrl** (10):

- ctrl.rs:396 orphan doc line 'Drop the host's reference to a resource.' is fused into set_scanout_blob's doc block (397-404); resource_unref (ctrl.rs:583) is left undocumented — reattach when touching the file.
- ctrl.rs:1007-1010 diagnostic_virgl_host3d_guest_scanout's TRANSFER failure path unrefs the resource but never ctx_destroys the diag context nor removes the live-resource table entry (diagnostic-only leak; fix during diag extraction).
- ctrl.rs:778-781 diagnostic_guest_blob_scanout's set_scanout_blob failure path calls resource_unref without take_live_resource, leaving a stale live-table entry that attach_resource_checked would trust (diagnostic-only).
- ctrl.rs:68 VIRGL_DIAG_BLOB_ID (diagnostic-only state) sits among the wait-budget constants; move with the diagnostic module.
- ctrl.rs:196-217 reap_parked's early returns on with_virtio Err leave reap_in_progress latched true forever (permanent QueueFull if the transport ever returned); the begin/finish two-phase bool (gpu.rs:1489-1510) is a natural consumed-ReapTicket typestate — transport-gone-only today, so minor.
- ctrl.rs:50-53 hand-rolled KERNEL_MODE: i8 = 0 / EXECUTIVE: i32 = 0 duplicate WDK enum values also needed by gpu.rs (NOTIFICATION_EVENT/IO_NO_INCREMENT at gpu.rs:508-512) — one shared constants module.
- interrupt.rs:145-147 the DPC re-signals HPD on every DPC until the worker's swap at hpd.rs:135 consumes the bit — intended level-latch coalescing, but worth a one-line comment so it is not 'fixed' later.
- ctrl.rs:183-186 wait_block treats every non-STATUS_SUCCESS KeWaitForSingleObject result as an elapsed slice; a KernelMode non-alertable wait can only return SUCCESS/TIMEOUT — match the two statuses explicitly to keep the invariant loud.
- gpu.rs:1229 'ring_idx.min(u8::MAX as u32) as u8' silently clamps out-of-range ring indices (subsumed by the RingDomain enum in wire-fence-id-newtypes).
- ctrl.rs:62/67 the ~5 s (ENQUEUE_RETRY_MAX) and ~30 s (MAP_BUSY_RETRY_MAX) poll budgets are magic and not derived from SYNC_ROUNDTRIP_TIMEOUT_MS — unify under one documented doctrine constant when backpressure-sleep-polls lands.

**kmd-transport-gpu** (12):

- hal.rs:16-21,190-194,227-232 — dma_alloc/mmio_phys_to_virt return (0, NonNull::dangling()) on failure (Hal trait has no error channel); a BAR-map failure faults inside PciTransport::new instead of failing StartDevice with a status. Documented/accepted, but a cheap preflight (map BARs / probe one contiguous page before PciTransport::new) would convert the init-time BSOD into a clean STATUS_INSUFFICIENT_RESOURCES.
- hal.rs:261-265 — 'virtio MMIO cache full' is kmsg-only; the loud-failure rule elsewhere uses named counters (init-time only, low value).
- gpu.rs:197-202 — bump_high_water is a racy load/store (documented as approximate telemetry; fine, but a fetch_max would cost nothing).
- gpu.rs:475-488 — the PARKED_ENQUEUE_GATE safety argument ('this bound is never exceeded') is provable arithmetic; add a const assertion `PARKED_ENQUEUE_GATE + MAX_INFLIGHT <= MAX_PARKED` so the forget()-leak backstop at gpu.rs:1471-1476 is compiler-checked unreachable.
- gpu.rs:1488-1510 + adapter callers — begin_parked_reap/finish_parked_reap rely on the caller returning the swapped vectors; a caller that drops them leaves reap_in_progress=true forever, permanently gating enqueues at PARKED_ENQUEUE_GATE (silent transport starvation). Sole caller (ctrl.rs:196-218 reap_parked) is correct today; an RAII reap guard returning the vectors on Drop would make the wedge unrepresentable.
- gpu.rs:70-74 — CTRL_POLL_SPINS bounds init's polled round-trip by iteration count calibrated in a comment ('~10 ns → ≈1 s'); convert to a time-based budget alongside the init-reset-timeout fix (poll itself is a legitimate pre-interrupt bring-up wait — KEEP per timeout doctrine).
- gpu.rs:1762-1770 — note_wddm_submission overflow degrades to immediate DMA_COMPLETED before venus retirement (counted via WDDM_PENDING_OVERFLOWS, documented practically unreachable); worth a comment cross-reference to the never-signal-early invariant so no one 'simplifies' the counter away.
- config.rs:39-53,59-71 — read_word returns 0 / write_word no-ops silently when the dxgkrnl callback Option is None; a debug_assert or one-time counter would distinguish 'device reports zeros' from 'callback table absent'.
- ctrl.rs:1223 `blob_size.saturating_add(4095) & !4095` and ctrl.rs:800,879 `(size + 0xFFF) & !0xFFF` duplicate gpu.rs:396-398 round_up_page/BLOB_PAGE — export and reuse the one helper.
- gpu.rs:943-949 — display_mode sanity bounds (320/240/16384) are inline magic; a validated DisplayMode::try_from(pmode) constructor would name them (validate-once pattern, low value).
- gpu.rs:2028-2035 blob_map_begin `owner: Option<usize>` overloads 'any-owner kernel resolve' onto None — an enum {AnyOwnerKernel, Escape(owner)} would document the two trust domains at call sites (folds naturally into blob-slot-state-enums if accepted).
- ctrl.rs:786-1014 diagnostic_* scanout probes (2D scanout, guest-blob, virgl host3d) remain compiled into the active build and callable from the same ctrl surface as the exact-primary path; per the sealed-interface axis, consider gating them behind a diagnostics feature/module so they cannot enter primary flows (ScanoutDiag must remain absent during primary tests). Primary subject is ctrl.rs, hence a note not a finding.

**kmd-venus** (10):

- venus.rs:526-528 — `fatal` field doc claims 'the allocation path reaches these waits at DISPATCH_LEVEL under the device spinlock' and cites removed RING_POLL_SPINS; both stale — waits sleep via ctrl::sleep_ms and the client is PASSIVE-only under the venus mutex (adapter.rs:914-917). Fix the comment; a PASSIVE proof-token parameter is the stronger follow-up under the split.
- venus.rs:598-626 timeout-doctrine classification: ring_wait_until's spin-burst + 1 ms sleep-poll bounded by 30 s is a KEEP — the vn ring has no doorbell/interrupt (host writes head into ring shmem only; Mesa's vn_ring polls identically), so no event/fence can replace it without protocol change.
- venus.rs:1607 and :2142 — magic 5_000_000_000 ns fence timeouts, both bounded waits on real fences (KEEP per doctrine); name one shared FENCE_DRAIN_TIMEOUT_NS constant.
- venus.rs:772-779 — allocate_memory_blob leaks the freshly allocated VkDeviceMemory if ctrl::resource_create_blob fails (no vkFreeMemory rollback); reclaimed only at context teardown. Add the rollback arm when touching the encoder consolidation.
- venus.rs:2155-2158 — destroy_prepared_image_copy leaks the marker fence if wait_for_fence fails (destroy_fence skipped); acceptable best-effort but worth a breadcrumb.
- venus.rs:1588-1591 — create_fence uniquely requires the echoed handle == fence_id while create_command_pool (1321-1322), allocate_command_buffer (1371-1376) and get_device_queue (1287-1288) accept 'returned if nonzero else ours'; unify the echo contract inside the ring_call helper.
- venus.rs:137-139 — QUEUE_FAMILY_FOREIGN_EXT is a misleading alias of QUEUE_FAMILY_EXTERNAL kept 'for the older diagnostic call sites' (used only at 2223); rename at the diagnostic call site and delete.
- venus.rs:205-215 + 2853-2858 — HostVisibleBlob.gpa uses a 0 sentinel for 'not yet window-mapped' (allocate_memory_blob returns gpa:0; bring-up rebuilds the struct with the real gpa); a Mapped/Unmapped typestate or NonZero gpa in a second type would remove the partially-initialized descriptor.
- venus.rs:2445 round_up_page duplicates create_allocation.rs:467's round_up_page; hoist one shared page-align helper.
- venus.rs:2500 — ring_id 0x4845_4C49_4F53_0001 doubles as a magic literal with an inline comment; fold into a named const beside the ring layout constants when splitting.

**umd-core** (9):

- DEFECT (low impact, report separately per rules): umd/src/lib.rs:23 names 0x887a_0020 'DXGI_ERROR_UNSUPPORTED' but that value is DXGI_ERROR_DRIVER_INTERNAL_ERROR; the real DXGI_ERROR_UNSUPPORTED (0x887A0004) is correctly defined under the same name in umd/src/forward.rs:393. OpenAdapter12 (lib.rs:390) therefore returns DRIVER_INTERNAL_ERROR while logging 'DXGI_ERROR_UNSUPPORTED'. Same-name/different-value constants across sibling modules are a refactor trap; fix the value and dedup HRESULT constants into one module.
- umd/src/lib.rs:191: `static mut ADAPTER_COOKIE` is never read or written as data — only its address is taken as the adapter cookie (lib.rs:624-626). A plain non-mut `static` (address cast to *mut for the handle, never dereferenced) removes the last static-mut in the crate.
- umd/src/device_funcs.rs:319,377: the table-audit 'suspicious slot' heuristic `value < 0x0000_0001_0000_0000` is a magic address-range guess; the audits (282-390) emit ~30-45 log lines per device creation unconditionally (capped at 32 audits). Tables are stable now — gate behind trace_enabled().
- umd/src/device_funcs.rs:209-233: the noop-DDI stubs count hits globally but cannot attribute WHICH slot was called (only the first hit gets a backtrace). The PSC conformance charter ('drive noop-DDI hit counters to zero') would benefit from macro-generated per-index stubs that log the slot number — behavior-preserving telemetry precision.
- umd/src/device_funcs.rs:61 and 107: the single-threaded-DDI RefCell contract (rooted in GetCaps THREADING caps = 0 at lib.rs:862-865) is comment-only, and a RefCell double-borrow panic inside a DDI is the forbidden silent-graphics-deadlock class; consider try_borrow with a counted refusal on the few multi-borrow-shaped paths, or document the caps linkage at each RefCell.
- umd/src/lib.rs:243-250: helios_umd_get_present_result returns -1 both for null out-params (caller bug) and for the documented 'none pending' case; a distinct code (or debug counter) would keep caller bugs from hiding in the normal fallback path.
- umd/src/lib.rs:1264-1298: PresentSyncPublish is documented as a 'Legacy IddCx producer switch' (default off, no cross-process IDD consumer exists), but its named-present-fence machinery is shared with the active kwait path (forward.rs:7888-7893 creates the fence kwait consumes) — do not remove without first splitting fence creation from slot publication in dxvk-helios.
- Untracked, git-ignored /home/rupansh/helios-vgpu/umd_clean/ contains a full stale copy of the UMD sources (11,490 lines); it pollutes repo-wide greps during the refactor — consider deleting the working copy before Phase 2.
- umd/src/bridge.rs:11-220 is a single flat #[cxx::bridge] block mixing device lifecycle, shader creates, scanout, present-sync, and flip-wait FFI; cxx requires one module, but grouping with section comments (or splitting the C++ header) would help the Phase-2 file splits stay aligned across the language boundary.

**umd-forward-a** (12):

- forward.rs:2116-2167 — resolve_shared_resource and dxgi_resolve_shared_resource duplicate log+Flush logic and reinterpret bare HANDLEs as Hdevice/resource-private pointers (2123, 2134-2136, 2148-2150); fold into one helper behind the typed-handle boundary of finding handle-payload-type-confusion.
- forward.rs:1144-1149, 1166-1179, 1261-1263, 1513-1515 — DDI_BIND_PRESENT/DDI_MISC_SHARED/DDI_MISC_SHARED_KEYEDMUTEX redefined locally in four functions; hoist to module consts (one commit, rides the split).
- forward.rs:391-393 — E_FAIL/E_INVALIDARG/DXGI_ERROR_UNSUPPORTED redefined as local consts; the windows crate exports these HRESULTs.
- forward.rs:132-151, 310-343 — StandardAllocMetaV1/V2 parse-only legacy arms are unreachable with version-locked KMD+UMD (both writers always emit the full 40-byte meta, kmd_render/src/ddi/create_allocation.rs:1410); removal candidate with owner confirmation, pairs with finding open-meta-1x1-fallback.
- forward.rs:408-410 — set_runtime_error depends on 'pfnSetErrorCb is the first member of every D3D11DDI_CORELAYER_DEVICECALLBACKS revision' enforced only by comment; a static_assert-style offset check against the bindgen layouts would make it compile-time.
- forward.rs:669-679 — is_dwm_process gates driver behavior on the exe name 'dwm.exe' (heuristic identity); acceptable bring-up trick but should live in the sealed scanout module so it cannot spread.
- forward.rs:633-638 — remember_scanout_target 'largest area wins' heuristic selects THE scanout target among rotating primaries; only feeds the legacy copy path (falls away with legacy-linear-copy-machinery).
- umd/src/device_funcs.rs:81-97 + forward.rs:616 — HeliosDevice uses Cell/RefCell from DDI and DXGI callback threads; a double borrow panics, and a panic in a DDI is the silent-graphics-deadlock class (CLAUDE.md invariant). Current borrows are short/non-reentrant; consider try_borrow + counter in the split.
- forward.rs:166-173 — d3dddi_to_dxgi_format maps every unknown format to B8G8R8A8_UNORM silently (both match arms identical); make the fallback explicit or log once.
- forward.rs:3031-3033 — shader_code_len trusts the DXBC total-size dword at offset 24 unbounded (the SHDR arm caps at 1<<20 but the DXBC arm does not) before from_raw_parts at 3086; runtime-supplied length should get the same per-arm sanity cap.
- forward.rs:2534-2553 — clear_rtv re-derefs RtvState inline instead of using rtv_info(), duplicating the unsafe cast for a log line.
- forward.rs:1220-1231 — calc_size_resource/calc_size_rtv return magic 8 documented as 'one COM pointer' (line 1218) while the slots actually hold Box<ResourceState>/Box<RtvState> pointers; comment is stale, and a size_of::<*mut c_void>() const would self-document.

**umd-forward-b** (10):

- umd/src/forward.rs:5017 collect_buffers takes and discards `start` (`let _ = start;`) — drop the parameter.
- umd/src/forward.rs:5905-5909 the `(8, Some(_))` match arm duplicates the general arm and silently drops the documented '8x only below 128 bits/sample' rule from the doc comment at 5883-5885 — code/doc divergence worth reconciling during the caps-table work.
- umd/src/forward.rs:5464-5475 resource_update_subresource_11_1 silently discards `_copy_flags` (D3D11.1 NO_OVERWRITE/DISCARD) with no counter, contra the named-counter rule for skipped semantics.
- umd/src/forward.rs:5562,5600,5752 update_tile_mappings/copy_tile_mappings/resize_tile_pool discard HRESULTs (`let _ =`) with no counter or log.
- umd/src/forward.rs:6240,6291,6310,6493 selftests hard-code 87/3/1/4 (BGRA/STAGING/MAP_READ/TRIANGLELIST) as bare literals; name them alongside the production format helpers when selftest.rs is extracted.
- umd/src/forward.rs:5758,5773 set_marker and set_marker_mode share one WDDM13_MARKER_LOG_COUNT, so one entry point consumes the other's log budget.
- umd/src/forward.rs:3513-3527 calc_size_tess_shader and calc_size_tess_shader_11_1 are duplicate bodies — fold when the shared DRV_PRIVATE size const lands.
- umd/src/forward.rs:4324 so_set_targets passes `Some(offsets)` without a null check while `buffers` is null-checked; a null offsets pointer with num>0 would reach DXVK unvalidated.
- umd/src/forward.rs:3363-3380 the Input-08733 'Evidence line' sig-entry dump runs unconditionally on every 11.1 shader create — investigation residue that belongs behind the trace gate now the bug is closed.
- umd/src/forward.rs:5287 srv_bind_summary walks all slots on every bind even when no log will fire; move the walk inside the will-log branch together with the telemetry finding.

**umd-forward-c** (8):

- forward.rs:7047-7049 — comment claims Vec reserve prevents CString pointer invalidation, but desc pointers target CString heap buffers which never move on Vec realloc; the SAFETY rationale is wrong (harmless) and should be corrected during the input_layout move.
- forward.rs:7356-7362 — ia_set_vertex_buffers passes Some(strides)/Some(offsets) even when the pointers are null (its own bookkeeping at 7330-7331 admits null); contract-guarded for num>0 but worth an explicit null->None mapping.
- forward.rs:7860-7877 — vehicle_present_prepare rc==1 geometry mismatch is counted but the present still publishes and mints with an uncopied backbuffer (one stale frame during resize); deliberate transient, deserves an explicit comment.
- forward.rs:8542-8559 — PRESENT_RESULT is set even when pfnPresentCb failed (present_hr<0), contradicting the doc at 7576-7578 ('None after a failed present'); benign today because the publish fence still retires, but the contract text and code disagree.
- forward.rs:8761,8939-8990 — DXGI_MPO_MAX_PLANES=16 with RGB|BILINEAR|SHARED|IMMEDIATE caps advertised while dxgi_present_mpo (8993-9113) merely forwards plane allocations; verify MPO_LOG_COUNT stays 0 in production or document why the caps are safe to advertise.
- forward.rs:6958-6963/7395-7397/7439-7443 — calc_size_* handlers return bare magic 8 ('8-byte COM-pointer slot'); a shared named const would document the handle-slot convention once.
- lib.rs:252-255 + 346-354 — helios_umd_selftest export is marked 'TEMPORARY (Gate 5b bring-up)... Remove once the DDI path is validated end-to-end'; candidate for removal with owner approval when the selftests move to forward/selftest.rs.
- forward.rs:6221-6226 — check_format_support formats a log string per call whenever FeatureLevel11=1; cheap but belongs behind trace_enabled with the other per-op chatter.

**xc-concurrency** (15):

- KEEP (doctrine): venus.rs:598-626 ring_wait_until — bounded PASSIVE sleep-poll on real ring-head progress with fatal latch; the vn ring has no host doorbell (venus.rs:27-31), so no event exists to wait on; mirror of Mesa vn_ring. Structure fine; RING_SPIN_BURST=50_000 (venus.rs:188) is a tuning magic worth a measured comment.
- KEEP (doctrine): ctrl.rs:157-191 wait_block — KEVENT wait in adaptive 1ms→1s slices with used-ring re-drain as lost-interrupt tolerance; bounded by caller budget; this is the correct pattern the backpressure loops should adopt.
- KEEP (doctrine, frozen baseline): UMD 10 ms condvar frame gate — bridge.rs:132-136 present_frame_gate + lib.rs:1108-1114 PresentGateUs ("bounded, condition-variable-backed gate ... not a polling Sleep loop"); timeout proceeds loudly (forward.rs:8486-8497).
- KEEP (doctrine): hpd.rs:100-107 steady-state 4 ms in-flight / 16 ms enqueue-failure timed waits — bounded lost-interrupt/retry fallbacks around a real KEVENT, with RfFail loud counter (hpd.rs:153-157).
- KEEP (doctrine): gpu.rs:70-74 CTRL_POLL_SPINS=100_000_000 — documented as the only remaining polled wait (single-threaded pre-interrupt bring-up GET_DISPLAY_INFO), bounded ~1 s.
- KEEP: ctrl.rs:59 SYNC_ROUNDTRIP_TIMEOUT_MS=30_000 and :65 WAIT_FENCE_MAX_MS=120_000 — bounded waits on real completions with slot abandonment (no transport poison), counted (CTRL_TIMEOUT_COUNT/FENCE_WAIT_TIMEOUTS).
- display.rs:688-692: a DISPATCH-level SetVidPnSourceAddress silently skips the whole scanout bind and returns STATUS_SUCCESS (only ScIrq counts) — success-after-skip is deliberate (PASSIVE calls redo the bind) but deserves an explicit contract comment naming the re-bind guarantee.
- device_funcs.rs:108-112 flip_wait_state: Cell<u8> with sentinel states 0/1/2 documented in a comment — replace with a 3-variant enum when present.rs is extracted.
- forward.rs:7914 maybe_log_present_readback calls std::env::var_os on every present (and again at :8006/:8022); cache the env probe in a OnceLock; same for maybe_force_present_alpha_opaque (:8102).
- forward.rs:7562-7580 PRESENT_SOURCE/LAST_VEHICLE_DEVICE/PRESENT_RESULT thread-locals carry a same-thread call-window contract enforced only by comments — fold into the sealed vehicle module (present-path-sealed-enum).
- adapter.rs:846-849 vsync heartbeat hardcodes 16 ms KeSetTimerEx period regardless of the reported mode's refresh rate — fine for the 60 Hz contract, but the magic constant should be derived from one named source shared with the EDID/mode code (vidpn.rs).
- display.rs:63-73 vs :113-123 production_linear_scanout builds ScanoutInfo from two divergent sources (cached atomics vs fresh allocation) — collapses naturally under the ValidatedScanoutDescriptor work.
- escape.rs:457-477 escape_wait_fence still accepts the legacy 32-byte pre-v2 shape ("old ICD") — retire once the ICD floor guarantees the v2 struct, with a counter first to prove zero legacy callers.
- diag.rs:1 module doc still says "TEMPORARY post-start bring-up tracer (remove once Code 43 / AddAdapter clears)" — stale; the module is now the permanent PSC counter surface and the doc should say so (or the S-ring should be split from the named-counter API).
- ctrl.rs:1224-1231 map_blob_at stale-overlap eviction uses a fixed [0u32; 8] scratch for overlapping blobs — silently handles at most 8 stale overlaps per call (unwrap_or(0) on error); add a counter if n==stale.len() to keep the truncation loud.

**xc-duplication** (15):

- umd/src/lib.rs:388-422 — OpenAdapter12 has an unconditional `return DXGI_ERROR_UNSUPPORTED` followed by a 30-line #[allow(unreachable_code)] dead block: delete the dead arm.
- kmd_render/src/virtio/ctrl.rs:1225-1231 + gpu.rs:2188-2211 — map_blob_at's stale-overlap eviction truncates at `[0u32; 8]` with no counter; >8 stale overlapping blobs silently keep the host window subregion overlap alive (violates the loud-failure rule; add a counter or loop until empty).
- kmd_render/src/virtio/venus.rs:188 — RING_SPIN_BURST = 50_000 PASSIVE spin iterations before the 1 ms sleep fallback is an unmeasured magic heuristic (the vn ring has no interrupt, so polling itself is inherent/KEEP; the burst size should be measured or shrunk).
- kmd_render/src/virtio/ctrl.rs:67,1171-1177 — MAP_BUSY_RETRY_MAX gives up to 30 s of 1 ms sleep-polls waiting out another mapper's in-flight RESOURCE_MAP_BLOB; a KEVENT signaled from blob_map_finish would make it event-driven (rare contention path; the bounded poll is a safety contract today).
- umd/src/forward.rs:8520,8593 (env_flag) and :7914,:8103 — std::env::var_os is evaluated on every dxgi_present (twice) plus in maybe_log_present_readback/maybe_force_present_alpha_opaque; cache in OnceLock like trace_enabled.
- kmd_render/src/ddi/escape.rs:382-407 (and 10 more handlers) — every escape handler repeats the size-check + pod_read_unaligned + write-back prologue; a generic with_escape_req::<T> helper would collapse ~100 lines.
- kmd_render/src/virtio/venus.rs:137-139 — QUEUE_FAMILY_FOREIGN_EXT is misnamed (its value is VK_QUEUE_FAMILY_EXTERNAL, as the comment admits); rename to kill the confusion.
- protocol/src/wddm.rs:60 — HeliosWddmAllocPrivate::_pad is documented as 'optional existing virtio resource id to adopt' but still named `_pad` and load-bearing at forward.rs:1371 and create_allocation.rs:739-745; rename (layout-preserving) to adopt_resource_id.
- kmd_render/src/ddi/display.rs:21-31 — rec_named is a byte-for-byte copy of diag::record_named_bytes (diag.rs:104-114), and diag.rs itself repeats the ASCII→UTF-16 name-build loop again in read_config_dword (129-137); one helper, three call sites.
- kmd_render/src/ddi/submit_command.rs:316-336 vs 342-365 — dxgkddi_submit_command_virtual and dxgkddi_submit_command bodies are identical; share one inner fn (render_km/render_gdi's copy-advance tail is a third near-copy).
- kmd_render/src/ddi/display.rs:666-669 and :688-692 — SetVidPnSourceAddress returns STATUS_SUCCESS both when scanout_alloc_info(h) is unresolvable (ScRid=0 breadcrumb) and at IRQL>0 (ScIrq breadcrumb): counted skips that report success; verify the DDI contract requires success here and document, else return the legal failure status.
- kmd_render/src/ddi/display.rs:759-771 — the rebind arm inside SetVidPnSourceAddress uses the synchronous set_scanout_blob (30 s ctrl budget) while the worker owns set_scanout_blob_async; routing rebinds through publish_scanout_candidate would keep multi-second host stalls out of a flip DDI (rare: only on !already_bound).
- kmd_render/src/ddi/create_allocation.rs:685-1084 — create_one is ~400 lines mixing private-data parsing, four backing-policy arms, identity write-back and VidMm segment policy; extract per-kind backing constructors when the file is next touched.
- umd/src/forward.rs:669-679 — is_dwm_process() gates the scanout import path by executable-name comparison ('dwm.exe'): a process-identity heuristic; consider a capability signal (e.g. the runtime's primary creation) instead.
- kmd_render/src/adapter.rs:616-812 — queue_active_scanout_refresh embeds ~100 lines of rate-limited registry telemetry (n%16/n%600 blocks) inline in the refresh worker; extract a diag::snapshot_scanout_telemetry() so the control flow is readable (writes are already rate-limited — keep the rates).

**xc-errors** (13):

- RESOLVED PRE-PHASE-2 (KMD 22.22.143.0): `kmd_render/src/ddi/present_packet.rs`
  now shares typed source/destination decoding and all-or-nothing patch emission between
  `DxgkDdiPresent` and `DxgkDdiPresentToHwQueue`. Only `NonNull` allocation handles can
  become patch references; absent fixed slots emit no entry, and insufficient capacity
  returns `STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER` before any write. The per-DDI no-op DMA
  record construction remains intentionally distinct because the command payloads differ.
- umd/src/forward.rs:9187-9249 (dxgi_present1 multi-surface tail) duplicates dxgi_present's publish/gate/PresentCb/submit_runtime_present sequence (8352-8528) — dedup after the forward.rs split.
- kmd_render/src/ddi/submit_command.rs:468-541 dxgkddi_render sniffs two different magic structs (HeliosPresentRefreshCmd, HeliosPresentRenderCmd) over the same DMA bytes — consider a tagged command envelope in helios_protocol instead of magic probing.
- umd/src/forward.rs:8520,8593,9241,9264 (+7914,8103) call std::env::var_os per present — cache HELIOS_PRESENT_* flags in OnceLock like the registry knobs.
- kmd_render/src/adapter.rs:464-482 stop_hpd: if ObReferenceObjectByHandle fails, the join is silently skipped (no counter) before ZwClose — worker could theoretically outlive teardown; add a loud counter.
- kmd_render/src/ddi/escape.rs:547-549 escape_map_blob returns STATUS_INVALID_PARAMETER after map_blob_prepare succeeded — host mapping/window offset stay allocated until release/teardown; asymmetric but reclaimed; document or unmap.
- kmd_render/src/ddi/create_allocation.rs:1334 `let _ = (open.allocation, open.private_size);` is dead code — delete.
- umd/src/lib.rs:390-422 OpenAdapter12 dead body behind an unconditional return — delete (covered by legacy finding, safe as its own first commit).
- Timeout classifications: ctrl.rs:157-191 wait_block (bounded KEVENT wait + used-ring re-drain per slice) and the UMD 10 ms present gate are safety contracts — KEEP; ctrl.rs:1171-1177/1242-1246 map-busy 1 ms sleep loops (bounded 30 s) are backpressure waiting out another mapper — acceptable but replaceable by an event if ever hot; venus.rs:596-623 ring-head spin-burst + 1 ms sleep-poll (bounded, poison-latched) is structural — the vn ring seqno has no guest interrupt — KEEP with the latch.
- kmd_render/src/diag.rs:126-129 read_config_dword uses RTL_QUERY_REGISTRY_DIRECT without TYPECHECK (documented trust in own knobs) — adding RTL_QUERY_REGISTRY_TYPECHECK would harden against a string-typed knob.
- kmd_render/src/ddi/display.rs:76-124 production_linear_scanout and create_allocation.rs:783-805 (create_one primary arm) duplicate the allocate_linear_scanout_image_blob + remember bookkeeping sequence — consolidate when the ValidatedScanout lands.
- kmd_render/src/ddi/scanout_diag.rs:112-139 maybe_run writes ~26 zeroing registry values even when mode==0 — early-return after SdgM/SdgErr to keep clean boots quieter (StartDevice-only, cosmetic).
- umd/src/lib.rs:669-681 create_device dumps 12 raw quadwords of the args struct unconditionally per device create — gate behind trace_enabled once the CreateDevice layout is considered stable.

**xc-legacy** (15):

- kmd_render/src/ddi/display.rs:21-31 — rec_named is a byte-for-byte duplicate of diag::record_named_bytes (diag.rs:104-114); delete and call the diag helper.
- DXGI format magic 87/88 redefined at display.rs:90,166-167,738-739, create_allocation.rs:355-356, and matched raw in display.rs:710 'matches!(source.dxgi_format, 87 | 88)'; forward.rs:722 hard-codes 87 — hoist shared constants into helios_protocol (folds into scanout-identity-static).
- kmd_render/src/diag.rs:1 — module doc still says 'TEMPORARY post-start bring-up tracer (remove once Code 43 / AddAdapter clears)'; Code 43 cleared 2026-07-05 and the S-ring is now a DiagLevel-gated production facility — retitle the doc.
- kmd_render/src/mapping.rs:3-22 — doc header describes the archived System-class path (IOCTL_HELIOS_MAP_BLOB, EvtFileCleanup, WDFFILEOBJECT) while the live owner is the D3DKMT device handle via DxgkDdiEscape/DestroyDevice (escape.rs:71-75); rewrite to the active contract.
- kmd_render/src/ddi/query_adapter_info.rs:122-130, 285-293, 434, 441 — RAISE_WDDM_3_2_GPUMMU=true + USE_WDDM_2_1_DISPLAY_SURFACE=true leave the DXGKDDI_WDDMv1_3 and v3_2 branches dead; collapse to one expressed WDDM version constant with the history in a comment.
- kmd_render/src/virtio/ctrl.rs:257-276 — enqueue backpressure is a bounded 1 ms sleep-poll (ENQUEUE_RETRY_MAX=5000) for queue space; counted (QfRet) and loud on exhaustion, so per the timeout doctrine it is an acceptable bounded contract, but a queue-space KEVENT signaled from drain_used would remove the poll entirely; same for MAP_BUSY_RETRY_MAX at 1173/1243.
- kmd_render/src/virtio/venus.rs:596-620 ring_wait_until spin-burst + 1 ms sleep-poll bounded by RING_WAIT_TIMEOUT_MS=30s with a fatal latch — KEEP (the venus shmem ring has no guest-visible doorbell; cold/boot/fallback path only), per the timeout doctrine a bounded safety contract.
- kmd_render/src/ddi/scanout_diag.rs:112-139 — maybe_run unconditionally writes ~24 Sdg* registry zeros on every StartDevice even with ScanoutDiag absent; dies with the scanout-diag-legacy tranche.
- kmd_render/src/ddi/gdi_blit.rs:64-79 — BIG_FILL_*/BIG_BLT_*/CT_ALPHA_* 'black-desktop hunt (15th session)' counters are solved-investigation archaeology on the hottest GDI path (atomics only, flushed every 64 batches at gdi_blit.rs:147-170); delete with the diag tranche.
- umd/src/lib.rs:191 'static mut ADAPTER_COOKIE' — replace with a plain static (only its address is used); removes a static-mut footgun.
- kmd_render/src/ddi/escape.rs:457-486 — escape_wait_fence still accepts the legacy 32-byte pre-v2 shape 'The legacy 32-byte shape (old ICD) is still accepted'; remove once ICD deploy skew is impossible (single-repo lockstep deploys).
- umd/src/forward.rs:7734-7742 — LAST_VEHICLE_DEVICE stores a raw HeliosDevice pointer in TLS with a comment-only same-thread/liveness contract ('SAFETY: same-thread contract'); at minimum tie it to the device generation or clear it in DestroyDevice.
- umd/src/device_funcs.rs:11-15,470 — uniform-stub fn-pointer transmute across signatures is formally UB (works on x64 caller-clean ABI); document as the single trusted ABI boundary or generate typed stubs (noted in umd-dedup-boilerplate).
- kmd_render/src/adapter.rs:508-511 — remember_scanout_blob doc claims 'Existing callers invoke this only after a synchronous successful SET_SCANOUT_BLOB (diagnostic/bootstrap paths)' yet it is also the production host_bound publish for the direct path via set_vidpn_source_address:789; tighten when the sealed ScanoutSource enum lands.
- kmd_render/src/ddi/display.rs:491-494 display_half_on plus ~12 sibling 'h as *const AdapterContext' casts — a single checked AdapterRef::from_ddi(handle) constructor would centralize the null-check + cast (cosmetic unless combined with a broader handle-wrapper pass; rejecting a full wrapper type here as it would merely relocate the cast).

**xc-unsafe** (12):

- umd/src/lib.rs:1012,1074,1121,1171,1229,1270 — the RegGetValueA extern block + query boilerplate is duplicated six times verbatim; extract one read_helios_dword(name, default) helper (dedup, one commit).
- umd/src/device_funcs.rs:457-633 — the four fill_*_device_funcs repeat the same stub-fill loop and calc! list; extract a shared filler parameterized by table size (dedup).
- umd/src/forward.rs:6239-6828 — selftest_offscreen_clear/triangle/cb_readback/triangle_cb are a dev harness compiled into the production DLL; quarantine behind a feature flag or dedicated module during the split.
- umd/src/forward.rs:7733-7748 — wait_last_present dereferences a TLS-stashed raw HeliosDevice pointer under a same-thread comment contract ('the ICD holds the vehicle D3D11 device reference'); if the vehicle path survives the legacy excision, encode as a handle validated against the device registry rather than a bare usize.
- Timeout doctrine classifications (all KEEP, no change requested): the UMD 10 ms present_frame_gate (forward.rs:8486-8498) and 32 ms VehicleFlipGateUs are bounded condvar/fence waits = safety contracts (frozen baseline); kmd ctrl.rs:157-191 wait_block adaptive slices re-drain = bounded KEVENT wait with lost-interrupt tolerance; venus.rs:598-626 ring_wait_until 1 ms poll = bounded poll where the vn-ring protocol offers no guest-visible doorbell for head progress, with FATAL latch; hpd.rs:100-107 4 ms in-flight fallback = bounded lost-interrupt tolerance.
- kmd_render/src/ddi/hpd.rs — the 'HPD' worker also owns all scanout bind/flush pumping (hpd_thread_routine:93-158); rename/split responsibilities (same thread is fine) so display-refresh logic isn't hidden under hot-plug naming.
- kmd_render/src/ddi/submit_command.rs:468-541 — dxgkddi_render parses the same command bytes twice against two magics (HeliosPresentRefreshCmd then HeliosPresentRenderCmd); a single parse-to-enum constructor removes the order-dependence and the double read_unaligned.
- kmd_render/src/ddi/display.rs — every display DDI re-derives &AdapterContext via '_adapter as *const AdapterContext' (e.g. :315, :374, :506, :555, :588, :637); one checked helper returning Option<&AdapterContext> (generalizing display_half_on at :491-494) shrinks ~15 repeated unsafe casts to one audited site.
- kmd_render/src/virtio/gpu.rs (2421 lines) mixes PCI capability scanning, blob/window-offset tables, the async in-flight queue, fence/fence-event tables, and the WDDM pending FIFO — split along those section comments when convenient (lower priority than venus.rs).
- kmd_render/src/adapter.rs:892-912 — the hand-rolled KEVENT venus mutex pairs acquire/release manually in with_venus_client and set_venus_client; a RAII guard would prevent a future early-return leak (currently correct).
- kmd_render/src/ddi/create_allocation.rs:44-142 — three handle-context types (AllocationContext/OpenAllocationContext/ResourceContext) discriminated by first-field magic and reinterpreted at ~8 sites; a generic TaggedHandle<T> with a single checked decode fn would centralize the casts (pairs with the UMD handle finding).
- umd/src/forward.rs:8581-8604 — the per-present forensics log_line (first 64 + every 512) does file I/O on the present path; acceptable, but consider moving under trace_enabled() now that the 0x80070057 investigation it served is closed.
